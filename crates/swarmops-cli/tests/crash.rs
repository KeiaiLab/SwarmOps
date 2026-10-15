//! `swarmops` processes that crash in the middle of a write (operations spec §0; storage spec
//! §2–§3): the M1 acceptance tests for crash consistency, through the real binary. They drive
//! the test hooks, so this test target requires the `test-hooks` feature, which the workspace's
//! gates turn on (`--all-features`).
//!
//! - A crash is `SWARMOPS_TEST_CRASH_AT=k`: the store's k-th filesystem call aborts the
//!   process, as a `kill -9` at that exact point would (no destructor runs; what reached the
//!   kernel stays). The object store and the metadata store share one call count, objects are
//!   written one at a time then, and a sweep's commits sign at a fixed time
//!   (`SWARMOPS_TEST_NOW`), so that they write the same objects: every run of a command makes
//!   the same calls in the same order, and call k is the same call in every run. A dry run with
//!   `SWARMOPS_TEST_FS_CALLS=<file>` counts a command's calls first (twice, in two seconds of
//!   the clock, for the commit), and the sweep crashes the command at every one of them.
//! - Every test ends with `verify --deep` through the binary, and reads its `problems`.
//!
//! Each test owns its temp dirs, and the tests share nothing.

mod support;

use std::fs;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use support::{Run, Store, agent, run};
use swarmops_ops::NOW_ENV;
use swarmops_store::{CRASH_AT_ENV, FS_CALLS_ENV};

/// How many runs of a crash sweep go at once, each with a store of its own.
const AT_ONCE: usize = 8;

/// How many frames lie past the checkpoint in the commit sweep's template: the frame of the
/// commit the sweep crashes is the 32nd, so its transaction also syncs the index and moves the
/// checkpoint, and the sweep crashes those calls too.
const TEMPLATE_FRAMES: u64 = 31;

/// The commit the sweep crashes: three files in new nested directories on main, so that it
/// reads main's tree and writes blobs, trees and its changeset.
const CRASH_PUTS: [(&str, &str); 3] = [
    ("src/lib.rs", "pub fn f() {}\n"),
    ("src/bin/tool.rs", "fn main() {}\n"),
    ("docs/notes.md", "# notes\n"),
];

/// The time the crash tests sign at: November 2023, in Unix seconds.
const NOW: u64 = 1_700_000_000;

/// `SWARMOPS_TEST_NOW=<unix seconds>`: a commit signs its changeset at that time, not the
/// clock's, so that the same commit made twice is the same changeset, whenever it runs; unset,
/// the time is the clock's.
#[test]
fn swarmops_test_now_fixes_the_signature_time() {
    let template = Store::init();
    let now = NOW.to_string();
    let ids: Vec<String> = (0..2)
        .map(|run| {
            let store = Store::copy_of(&template);
            let changes = store.changes("same", "same.txt", "the same change\n");
            let args = ["commit", "-m", "same", "--changes", &changes];
            let result = store.run_as(&agent(0), &args, &[(NOW_ENV, &now)]).ok();
            let id = result["changeset"].as_str().unwrap().to_owned();
            let shown = store.show(&id);
            assert_eq!(shown["created_at"], NOW * 1000, "run {run}");
            for signature in ["author", "committer"] {
                assert_eq!(shown["signatures"][signature]["secs"], NOW, "run {run}");
            }
            store.assert_clean(&format!("run {run}"));
            id
        })
        .collect();
    assert_eq!(ids[0], ids[1], "one commit, at one time: one changeset");

    let store = Store::copy_of(&template);
    let before = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let id = store.commit(&agent(0), "clock.txt", "the clock\n", None);
    let secs = store.show(&id)["signatures"]["committer"]["secs"]
        .as_u64()
        .unwrap();
    assert!(secs >= before, "unset, the clock's time: {secs} < {before}");
    store.assert_clean("signed at the clock's time");
}

/// Runs `run(k)` for every k of `ks`, [`AT_ONCE`] at a time, each on a thread of its own; the
/// outcomes, in the order of `ks`.
fn sweep<T: Send>(ks: RangeInclusive<u64>, run: impl Fn(u64) -> T + Sync) -> Vec<T> {
    let ks: Vec<u64> = ks.collect();
    let run = &run;
    let mut outcomes = Vec::with_capacity(ks.len());
    for batch in ks.chunks(AT_ONCE) {
        thread::scope(|scope| {
            let runs: Vec<_> = batch.iter().map(|&k| scope.spawn(move || run(k))).collect();
            for handle in runs {
                let outcome = handle
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
                outcomes.push(outcome);
            }
        });
    }
    outcomes
}

/// The number a dry run wrote to `file`: how many filesystem calls it made.
fn reported_calls(file: &Path) -> u64 {
    let text = fs::read_to_string(file).unwrap_or_else(|e| {
        panic!(
            "{}: the run reported no filesystem call count ({e})",
            file.display()
        )
    });
    text.trim()
        .parse()
        .unwrap_or_else(|e| panic!("{text:?} is not a call count: {e}"))
}

/// The file a run of `store`'s writes its call count to.
fn calls_file(store: &Store) -> PathBuf {
    store.tmp().join("fs-calls")
}

/// A store for the commit sweep: main at a first changeset, then changesets on it, until
/// [`TEMPLATE_FRAMES`] frames lie past the checkpoint (which has passed none). Every command
/// on it, and on its copies, signs at [`NOW`], so the store and every commit on it are the same
/// in every run.
fn commit_template() -> Store {
    let store = Store::init().with_env(&[(NOW_ENV, &NOW.to_string())]);
    let first = store.commit(&agent(0), "README.md", "# crash test\n", None);
    let create = store.run_as(&agent(0), &["ref", "set", "main", &first, "--create"], &[]);
    create.ok();
    for n in 2..TEMPLATE_FRAMES {
        store.commit(
            &agent(0),
            &format!("history/{n}.txt"),
            &format!("{n}\n"),
            None,
        );
    }
    assert_eq!(store.checkpoint(), 0);
    store
}

/// Runs the commit the sweep crashes on `store`, with `env` set.
fn crash_commit(store: &Store, env: &[(&str, &str)]) -> Run {
    let changes = store.changes_of("crash", &CRASH_PUTS);
    store.run_as(
        &agent(1),
        &["commit", "-m", "crash", "--changes", &changes],
        env,
    )
}

/// What the crash commit does when nothing crashes it.
#[derive(Debug, PartialEq, Eq)]
struct DryRun {
    /// How many filesystem calls it makes.
    calls: u64,
    /// The changeset it commits.
    changeset: String,
}

/// Runs the crash commit on a copy of `template` without a crash.
fn dry_commit(template: &Store) -> DryRun {
    let store = Store::copy_of(template);
    let file = calls_file(&store);
    let result = crash_commit(&store, &[(FS_CALLS_ENV, file.to_str().unwrap())]).ok();
    assert_eq!(
        store.checkpoint(),
        store.journal_len(),
        "the 32nd frame past the checkpoint moves it to the journal's end"
    );
    DryRun {
        calls: reported_calls(&file),
        changeset: result["changeset"].as_str().unwrap().to_owned(),
    }
}

/// Waits until the clock's second changes: a run after it signs at another second, unless
/// something fixes the time.
fn wait_for_the_next_second() {
    let second = || {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    };
    let start = second();
    while second() == start {
        thread::sleep(Duration::from_millis(10));
    }
}

/// What one run of the commit sweep found.
struct Crashed {
    /// The commit ran to its end: k was past its last call.
    finished: bool,
    /// The store holds its changeset.
    committed: bool,
    /// The temp files the crash left in `objects/tmp/`.
    leftovers: usize,
}

/// One run of the commit sweep: the crash commit on a copy of `template`, its k-th filesystem
/// call aborting it. It makes the dry run's calls, in the same order, so it must abort when k is
/// one of them, and otherwise commit the dry run's changeset after the dry run's calls. The
/// store must then verify clean (the temp files a crash leaves in `objects/tmp/` are no
/// objects), hold the changeset or not, and take a fresh commit, which `show` finds.
fn crash_commit_at(template: &Store, k: u64, dry: &DryRun) -> Crashed {
    let store = Store::copy_of(template);
    let file = calls_file(&store);
    let crash_at = k.to_string();
    let env = [
        (CRASH_AT_ENV, crash_at.as_str()),
        (FS_CALLS_ENV, file.to_str().unwrap()),
    ];
    let crash = crash_commit(&store, &env);
    let finished = k > dry.calls;
    if finished {
        let result = crash.ok();
        assert_eq!(result["changeset"], dry.changeset.as_str(), "k={k}");
        let made = reported_calls(&file);
        assert_eq!(
            made, dry.calls,
            "k={k}: the dry run's calls, no more and no fewer"
        );
    } else {
        assert!(crash.aborted(), "k={k} of {} calls: {crash}", dry.calls);
        assert!(!file.exists(), "k={k}: a run that crashes reports nothing");
    }
    let leftovers = store.temp_files();
    store.assert_clean(&format!("k={k}: after the crash"));

    let before = TEMPLATE_FRAMES - 1; // one of the template's frames moved main
    let changesets = store.info()["changesets"].as_u64().unwrap();
    let new = changesets.checked_sub(before).unwrap_or_else(|| {
        panic!("k={k}: {changesets} changesets, fewer than the template's {before}")
    });
    let committed = match new {
        0 => false,
        1 => true,
        n => panic!("k={k}: {n} new changesets"),
    };
    assert!(
        committed || !finished,
        "k={k}: it returned, but is not stored"
    );
    // The changeset the store holds, if any, is the dry run's.
    let shown = store.read(&["show", &dry.changeset]);
    if committed {
        shown.ok();
    } else {
        shown.err("E_REV_NOT_FOUND");
    }

    let next = format!("after a crash at call {k}\n");
    let fresh = store.commit(&agent(2), "after-crash.txt", &next, None);
    let shown = store.show(&fresh);
    assert_eq!(shown["changes"][0]["path"], "after-crash.txt", "k={k}");
    assert_eq!(store.info()["changesets"], changesets + 1, "k={k}");
    store.assert_clean(&format!("k={k}: after the next commit"));
    Crashed {
        finished,
        committed,
        leftovers,
    }
}

/// A commit crashed at each of its filesystem calls in turn, from the first to past the last
/// (k = 1..=N + 1, N counted by a dry run): after each crash the store verifies clean, and the
/// next commit succeeds and is shown. The commit's frame is the 32nd past the checkpoint, so
/// the sweep crashes its index sync and checkpoint too. Every run signs at [`NOW`], so every
/// run makes the same calls, in the same order: two dry runs, in different seconds of the
/// clock, must count the same calls and commit the same changeset.
#[test]
#[ignore = "slow, ~20 s: ~90 crashed commits; just check-slow runs it"]
fn kill9_during_commit_store_verifies_and_next_commit_succeeds() {
    let template = commit_template();
    let dry = dry_commit(&template);
    wait_for_the_next_second();
    assert_eq!(
        dry_commit(&template),
        dry,
        "one call sequence for every run"
    );
    assert!(dry.calls > 30, "a commit makes dozens of calls: {dry:?}");

    let crashes = sweep(1..=dry.calls + 1, |k| crash_commit_at(&template, k, &dry));
    let after_commit_point = crashes
        .iter()
        .filter(|crash| crash.committed && !crash.finished)
        .count();
    assert!(
        after_commit_point > 0,
        "some crash came after the journal append: the metadata store's calls count too"
    );
    assert!(
        crashes.iter().any(|crash| !crash.committed),
        "some before it"
    );
    assert!(
        crashes.iter().any(|crash| crash.leftovers > 0),
        "some crash left a temp file in objects/tmp, which verify passed over"
    );
}

/// Runs `init` on a fresh path without a crash; how many filesystem calls it made.
fn dry_init() -> u64 {
    let store = Store::empty();
    let file = calls_file(&store);
    run(
        &["init", store.path()],
        &[(FS_CALLS_ENV, file.to_str().unwrap())],
    )
    .ok();
    reported_calls(&file)
}

/// One run of the init sweep: `init` on a fresh path, its k-th filesystem call (of the
/// `calls` a dry run counted) aborting it. Then either `info` says `E_NOT_A_REPO` and a second
/// `init` succeeds, or `info` succeeds (and a second `init` says `E_EXISTS`); either way the
/// store is fresh and verifies clean. Returns whether `info` found the store right after the
/// crash.
fn init_crashing_at(k: u64, calls: u64) -> bool {
    let store = Store::empty();
    let crash_at = k.to_string();
    let init = run(
        &["init", store.path()],
        &[(CRASH_AT_ENV, crash_at.as_str())],
    );
    if k <= calls {
        assert!(init.aborted(), "k={k}: {init}");
    } else {
        init.ok();
    }
    let info = store.read(&["info"]);
    let whole = info.exit() == 0;
    if whole {
        run(&["init", store.path()], &[]).err("E_EXISTS");
    } else {
        info.err("E_NOT_A_REPO");
        run(&["init", store.path()], &[]).ok();
    }
    let info = store.info();
    let fresh = (info["name"].as_str(), info["changesets"].as_u64());
    assert_eq!(fresh, (Some("store"), Some(0)), "k={k}");
    store.assert_clean(&format!("k={k}"));
    whole
}

/// `init` crashed at each of its filesystem calls in turn (`LocalRepo::create`, with
/// `MetaStore::create` inside it, then the first snapshot `init` reports from), from the first
/// to past the last: before `FORMAT` is linked there is no store and a second `init` makes it;
/// from then on the store is whole.
#[test]
#[ignore = "slow, ~25 s: ~300 crashed inits; just check-slow runs it"]
fn kill9_during_init_leaves_no_store_or_a_whole_one() {
    let calls = dry_init();
    assert!(
        calls > 256,
        "init makes 256 fan-out directories alone: {calls}"
    );
    let whole = sweep(1..=calls + 1, |k| init_crashing_at(k, calls));
    let first = whole
        .iter()
        .position(|&whole| whole)
        .expect("past the last call, init finishes");
    assert!(first > 256, "a store before its objects/ is made: {first}");
    assert!(
        whole[first..].iter().all(|&whole| whole),
        "once FORMAT is linked, the store is whole: {whole:?}"
    );
}
