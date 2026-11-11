//! The scale cases (the M21 design §2.10, the M21 plan's task M21-2): A1's two stores, 1,000 and
//! 1,000,000 changesets and files ([`swarmops_testkit::bulk`]), read the same ways, so that what
//! grows with the history shows. Each case prints `scale-perf: ` lines and asserts only what it
//! ran (every sample found, the answers equal), never a time: a missed target is recorded in
//! `docs/perf.md`.
//!
//! The cases run only with `SWARMOPS_SCALE=1` ([`bulk::scale`]); without it each prints
//! `scale-perf: <case> not run` and returns, touching no fixture, so that a run of every ignored
//! test (`just check-slow`) is not hours long. They are ignored in `just check`; `just perf` runs
//! them, in a release build, once M21-P lists them. The large store is built once, a part a call
//! of `fixture_next_part` (`scale-perf: fixture part k of n`), under `SWARMOPS_BENCH_DIR`, and
//! reused; a case finds it with [`bulk::open`] and, while a part is missing, prints
//! `scale-perf: <case> not run (fixture k of n)` and returns.
//!
//! ```text
//!   fixture_next_part ──▶ bulk::next_part ──▶ large (1m), a part a call
//!   bulk::build_small() ──▶ small (1k) ─┐
//!   bulk::open(Large) ────▶ large (1m) ─┴─▶ id lookup · snapshot + lookup · file at a revision ·
//!                                           path history; verify · gc · export · upgrade: peak
//!                                           memory, 100k beside 1m
//! ```

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use swarmops_core::{ObjectKind, RepoPath, hash_object};
use swarmops_repo::LocalRepo;
use swarmops_testkit::accounting::PeakWatch;
use swarmops_testkit::bulk::{self, Fixture, Objects, Progress, Scale, Size, Spec};
use swarmops_testkit::child::Child;
use swarmops_testkit::measure::{base_dir, percentile};
use swarmops_testkit::store::layout_of;

use crate::support;

/// The cases, by name: the test of each ends on `scale-perf: <name> not run` without the gate.
const CASES: [&str; 5] = [
    "lookup_snapshot_read_at_1m",
    "path_history_first_page_1k_vs_1m",
    "peak_rss_100k_vs_1m",
    "text_fixture_bytes_on_disk",
    "fixture_next_part",
];

/// How long a command of the memory case may take.
const SLOW_RUN: Duration = Duration::from_hours(4);

/// The changesets of the memory case's smaller store.
const MID: usize = 100_000;

/// The changesets of the memory case's smaller store: [`MID`], or a tenth of the large store when
/// a trial sets its size.
fn mid() -> usize {
    MID.min(bulk::large() / 10)
}

/// The path of `hot.txt`.
fn hot() -> RepoPath {
    RepoPath::new(bulk::HOT).expect("a valid path")
}

/// Whether the gate is open for `case`; if not, it said so.
fn runs(case: &str) -> bool {
    bulk::gate(case) == Scale::Run
}

/// The large store, whole; else `None`, once the case's `not run (fixture k of n)` line is out.
fn large_or_not_run(case: &str) -> Option<Fixture> {
    let fixture = bulk::open(Size::Large);
    if fixture.is_none() {
        let (k, n) = bulk::parts(Size::Large);
        println!("scale-perf: {case} not run (fixture {k} of {n})");
    }
    fixture
}

/// `p50` and `p99` of `samples`, as a figure line's text.
fn stat(mut samples: Vec<Duration>) -> String {
    samples.sort();
    let p50 = percentile(&samples, 50);
    let p99 = percentile(&samples, 99);
    format!("n={} p50={p50:?} p99={p99:?}", samples.len())
}

/// `big` over `small` to two decimals, in integers: `1234` over `1000` is `1.23`.
fn hundredths(big: u64, small: u64) -> String {
    let scaled = u128::from(big) * 100 / u128::from(small.max(1));
    format!("{}.{:02}", scaled / 100, scaled % 100)
}

/// Runs `step` on every sample of `fixture` and times each.
fn timed<T>(
    fixture: &Fixture,
    mut step: impl FnMut(usize, swarmops_core::ChangesetId) -> T,
) -> Vec<Duration> {
    let mut times = Vec::new();
    for (k, id) in &fixture.samples {
        let started = Instant::now();
        let got = step(*k, *id);
        times.push(started.elapsed());
        drop(got);
    }
    times
}

#[test]
fn a_scale_case_without_the_variable_says_not_run() {
    let bench = tempfile::tempdir().unwrap();
    for case in CASES {
        let test = swarmops_testkit::test_name!(case);
        let mut command = swarmops_testkit::child::this_test(&test);
        command
            .env_remove(bulk::SCALE_ENV)
            .env("SWARMOPS_BENCH_DIR", bench.path());
        let output = swarmops_testkit::child::run(&mut command);
        let out = String::from_utf8_lossy(&output.stdout);
        assert!(output.status.success(), "{case}: {output:?}");

        // Exactly its line, and nothing else of the family.
        let lines: Vec<&str> = out
            .lines()
            .filter(|line| line.starts_with("scale-perf: "))
            .collect();
        assert_eq!(lines, [format!("scale-perf: {case} not run")], "{case}");
    }

    // No fixture was made or touched.
    let left: Vec<_> = std::fs::read_dir(bench.path()).unwrap().collect();
    assert!(left.is_empty(), "a case without the variable made {left:?}");
}

/// The cases that read the large store: on an incomplete one each says how far it is.
const LARGE_CASES: [&str; 3] = [
    "lookup_snapshot_read_at_1m",
    "path_history_first_page_1k_vs_1m",
    "peak_rss_100k_vs_1m",
];

/// The `scale-perf: ` lines of `case`, run alone with the gate open on a large store of
/// `changesets` under `bench`.
fn scale_lines(case: &str, bench: &Path, changesets: usize) -> Vec<String> {
    let test = swarmops_testkit::test_name!(case);
    let mut command = swarmops_testkit::child::this_test(&test);
    command
        .env(bulk::SCALE_ENV, "1")
        .env(bulk::LARGE_ENV, changesets.to_string())
        .env("SWARMOPS_BENCH_DIR", bench);
    let output = swarmops_testkit::child::run(&mut command);
    assert!(output.status.success(), "{case}: {output:?}");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.starts_with("scale-perf: "))
        .map(str::to_owned)
        .collect()
}

/// What `bench` holds: every entry's name, and each fixture's `parts.txt`.
fn bench_state(bench: &Path) -> Vec<(String, Option<String>)> {
    let mut state: Vec<_> = std::fs::read_dir(bench)
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            let parts = std::fs::read_to_string(path.join("parts.txt")).ok();
            (path.display().to_string(), parts)
        })
        .collect();
    state.sort();
    state
}

#[test]
fn a_case_on_an_incomplete_fixture_says_which_part() {
    let bench = tempfile::tempdir().unwrap();
    let two_parts = bulk::PART + 1;

    // No part yet: each case says so, and makes nothing.
    for case in LARGE_CASES {
        let lines = scale_lines(case, bench.path(), two_parts);
        assert_eq!(
            lines,
            [format!("scale-perf: {case} not run (fixture 0 of 2)")],
            "{case}"
        );
    }
    assert!(bench_state(bench.path()).is_empty(), "a case built nothing");
}

#[test]
#[ignore = "slow, ~25 s: builds two parts of a 2,049-changeset store in a debug build"]
fn the_builder_and_the_cases_count_the_same_parts() {
    let bench = tempfile::tempdir().unwrap();
    let two_parts = bulk::PART + 1;

    // 1. The builder records one part of two.
    let lines = scale_lines("fixture_next_part", bench.path(), two_parts);
    assert_eq!(lines[0], "scale-perf: fixture part 1 of 2");

    // 2. Each case on the large store says so, and builds nothing.
    let before = bench_state(bench.path());
    for case in LARGE_CASES {
        let lines = scale_lines(case, bench.path(), two_parts);
        assert_eq!(
            lines,
            [format!("scale-perf: {case} not run (fixture 1 of 2)")],
            "{case}"
        );
    }
    assert_eq!(bench_state(bench.path()), before, "a case built nothing");

    // 3. The second part makes it whole; one more call writes nothing and says whole again.
    let lines = scale_lines("fixture_next_part", bench.path(), two_parts);
    assert_eq!(lines[0], "scale-perf: fixture part 2 of 2, whole");
    let whole = bench_state(bench.path());
    let lines = scale_lines("fixture_next_part", bench.path(), two_parts);
    assert_eq!(lines[0], "scale-perf: fixture part 2 of 2, whole");
    assert_eq!(
        bench_state(bench.path()),
        whole,
        "a whole fixture is left as it is"
    );
}

/// A51: an id lookup, a snapshot and a lookup, and a file read at a revision, p50 and p99, in
/// the small store and in the large one.
#[test]
#[ignore = "slow, ~2 s on a built store (20k trial; 1M not run): 3 reads of 1,000 samples on two stores; building the 1M store is hours"]
fn lookup_snapshot_read_at_1m() {
    const CASE: &str = "lookup_snapshot_read_at_1m";
    if !runs(CASE) {
        return;
    }
    let Some(large) = large_or_not_run(CASE) else {
        return;
    };
    let small = bulk::build_small();
    for (name, fixture) in [("1k", &small), ("1m", &large)] {
        let name = format!("{name} ({} changesets)", fixture.spec.changesets);
        let repo = fixture.open();

        // 1. An id lookup: the index record of a changeset, in a snapshot of its own.
        let times = timed(fixture, |_, id| {
            repo.changeset_meta(id).unwrap().expect("a sample is held")
        });
        bulk::say(CASE, &format!("{name} id lookup {}", stat(times)));

        // 2. A snapshot, then the changeset in it: the object read included.
        let times = timed(fixture, |_, id| {
            let read = repo.snapshot().unwrap();
            read.changeset(id).unwrap().expect("a sample is held")
        });
        bulk::say(CASE, &format!("{name} snapshot + lookup {}", stat(times)));

        // 3. The file the changeset put, at that revision: the trees on its path and the blob.
        let times = timed(fixture, |k, id| {
            let (_, _, content) = repo
                .read_blob_at(id, &bulk::repo_path(k))
                .unwrap()
                .expect("the file is there");
            assert_eq!(content, bulk::file_text(k), "file {k}");
        });
        bulk::say(CASE, &format!("{name} file at a revision {}", stat(times)));
    }
}

/// The changesets on `head`'s first-parent chain whose delta names `hot.txt`, newest first, the
/// first `limit` of them: the answer by walking every changeset.
fn walked(
    repo: &LocalRepo,
    head: swarmops_core::ChangesetId,
    limit: usize,
) -> Vec<swarmops_core::ChangesetId> {
    let read = repo.snapshot().unwrap();
    let mut found = Vec::new();
    for item in read.first_parents(head) {
        let id = item.unwrap();
        let stored = read.changeset(id).unwrap().expect("a chain member is held");
        if stored
            .changeset
            .delta
            .iter()
            .any(|d| d.path.as_str() == bulk::HOT)
        {
            found.push(id);
        }
        if found.len() == limit {
            break;
        }
    }
    found
}

/// A9: the first page of the history of a path that 10 changesets touched, 1,000 changesets
/// against 1,000,000, and the answer equal to the walk of every changeset.
#[test]
#[ignore = "slow, ~1 s on a built store (20k trial; 1M not run): the first page, then the walk of every changeset; building the 1M store is hours"]
fn path_history_first_page_1k_vs_1m() {
    const CASE: &str = "path_history_first_page_1k_vs_1m";
    if !runs(CASE) {
        return;
    }
    let Some(large) = large_or_not_run(CASE) else {
        return;
    };
    let small = bulk::build_small();
    let mut took = Vec::new();
    for (name, fixture) in [("1k", &small), ("1m", &large)] {
        let name = format!("{name} ({} changesets)", fixture.spec.changesets);
        let repo = fixture.open();
        let read = repo.snapshot().unwrap();

        let started = Instant::now();
        let events = read
            .path_history(fixture.head, &hot(), bulk::HOT_TOUCHES)
            .unwrap();
        let elapsed = started.elapsed();
        took.push(elapsed);

        let page: Vec<_> = events.iter().map(|event| event.changeset).collect();
        assert_eq!(page.len(), bulk::HOT_TOUCHES, "{name}: a full page");
        assert_eq!(
            page,
            walked(&repo, fixture.head, bulk::HOT_TOUCHES),
            "{name}: the walk's answer"
        );
        bulk::say(CASE, &format!("{name} first page {elapsed:?}"));
    }
    let ratio = took[1].as_secs_f64() / took[0].as_secs_f64().max(f64::MIN_POSITIVE);
    bulk::say(CASE, &format!("1m over 1k {ratio:.2} (target <= 1.5)"));
}

/// The peak resident memory of `swarmops --repo <store> <args>`, in MiB; the command must exit 0.
fn peak(store: &Path, args: &[&str]) -> u64 {
    let store = store.to_str().expect("UTF-8 paths");
    let mut line = vec!["--repo", store];
    line.extend_from_slice(args);
    let mut command = support::swarmops(&line, &[]);
    let started = Instant::now();
    let child = Child::spawn(&mut command);
    let watch = PeakWatch::start(child.id());
    let output = child.wait(started + SLOW_RUN);
    let kib = watch.finish();
    assert!(
        output.status.success(),
        "{args:?}: {:?} {}{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    kib / 1024
}

/// A copy of the store at `from` that `upgrade` may change: everything but `objects/`, which is
/// a link to the original's (an upgrade writes none).
fn upgradable(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        let target = to.join(&name);
        if name == "objects" {
            std::os::unix::fs::symlink(entry.path(), target).unwrap();
            continue;
        }
        let copied = Command::new("cp")
            .arg("-a")
            .arg(entry.path())
            .arg(&target)
            .status()
            .unwrap();
        assert!(copied.success(), "cp {}", name.display());
    }
}

/// A53: the peak memory of `verify --deep`, `gc`, `git export` and `upgrade`, on 100,000
/// changesets and on 1,000,000. Recorded; a ratio over 1.5 is unexpected (the M21 design D8).
#[test]
#[ignore = "slow, hours: git export of the 1M store dominates (20k: over 30 min at load 380, not finished)"]
fn peak_rss_100k_vs_1m() {
    const CASE: &str = "peak_rss_100k_vs_1m";
    if !runs(CASE) {
        return;
    }
    let Some(large) = large_or_not_run(CASE) else {
        return;
    };
    let mid = bulk::build(Spec::new(mid(), mid()));
    let scratch = tempfile::tempdir_in(base_dir()).unwrap();
    let (mid_n, large_n) = (mid.spec.changesets, large.spec.changesets);
    let mibs = |name: &str, run: &dyn Fn(&Fixture, &str) -> u64| {
        let (a, b) = (run(&mid, "100k"), run(&large, "1m"));
        let ratio = hundredths(b, a);
        bulk::say(
            CASE,
            &format!("{name} {mid_n}={a} MiB {large_n}={b} MiB ratio {ratio}"),
        );
    };
    mibs("verify --deep", &|f, _| {
        peak(f.store.as_std_path(), &["verify", "--deep"])
    });
    mibs("gc", &|f, _| peak(f.store.as_std_path(), &["gc"]));
    mibs("export", &|f, tag| {
        let bare = scratch.path().join(format!("export-{tag}.git"));
        let made = Command::new("git")
            .args(["init", "--bare", "-q"])
            .arg(&bare)
            .status()
            .unwrap();
        assert!(made.success(), "git init");
        let bare = bare.to_str().unwrap().to_owned();
        let mib = peak(
            f.store.as_std_path(),
            &["git", "export", &bare, "--ref", bulk::MAIN],
        );
        std::fs::remove_dir_all(&bare).unwrap();
        mib
    });
    mibs("upgrade", &|f, tag| {
        let copy = scratch.path().join(format!("upgrade-{tag}"));
        upgradable(f.store.as_std_path(), &copy);
        let mib = peak(&copy, &["upgrade"]);
        std::fs::remove_dir_all(&copy).unwrap();
        mib
    });
}

/// A4: what a text-heavy store's files hold against the text itself.
#[test]
#[ignore = "slow, ~10 s: builds a 1,000-changeset store with zstd object files"]
fn text_fixture_bytes_on_disk() {
    const CASE: &str = "text_fixture_bytes_on_disk";
    if !runs(CASE) {
        return;
    }
    let fixture = bulk::build(Spec::new(bulk::SMALL, bulk::SMALL).with_objects(Objects::Zstd));
    let layout = layout_of(fixture.store.as_str());

    // 1. The text of every file, and the size of the file that holds each as a blob.
    let (mut text, mut blobs) = (0_u64, 0_u64);
    for k in 0..bulk::SMALL {
        let payload = bulk::file_text(k);
        let id = hash_object(ObjectKind::Blob, &payload);
        text += payload.len() as u64;
        blobs += std::fs::metadata(layout.object_path(id)).unwrap().len();
    }

    // 2. Every object file: blobs, trees and changesets.
    let mut all = 0_u64;
    let mut files = 0_u64;
    for fanout in std::fs::read_dir(layout.objects_dir()).unwrap() {
        let fanout = fanout.unwrap().path();
        if fanout.file_name().is_some_and(|name| name == "tmp") {
            continue;
        }
        for file in std::fs::read_dir(fanout).unwrap() {
            all += file.unwrap().metadata().unwrap().len();
            files += 1;
        }
    }
    assert!(files >= bulk::SMALL as u64, "every blob has a file");
    bulk::say(
        CASE,
        &format!("text {text} B, its blob files {blobs} B, all {files} object files {all} B"),
    );
}

/// P4: records the next part of the large store, and says which, and what it cost. `perf` calls it
/// until its line says whole, at most n calls; the cases that read the store never build it.
#[test]
#[ignore = "slow, ~35 s: one part of the 1M store (20k trial: 33 s a part at load 380); 489 calls build it"]
fn fixture_next_part() {
    const CASE: &str = "fixture_next_part";
    if !runs(CASE) {
        return;
    }
    let started = Instant::now();
    let (part, of) = match bulk::next_part(Size::Large) {
        Progress::Built { part, of } => (part, of),
        Progress::Whole => bulk::parts(Size::Large),
    };
    let whole = if part == of { ", whole" } else { "" };
    println!("scale-perf: fixture part {part} of {of}{whole}");
    bulk::say(CASE, &format!("took {:?}", started.elapsed()));
}
