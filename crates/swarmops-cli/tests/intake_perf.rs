//! What an import costs the others (M17 design §1, R-2 and A39; the M17 closure design §3.1),
//! for the `perf` recipe.
//!
//! A 12,000-commit history is imported into `main` of a store while 8 committers each run
//! `commit` (a commit that moves no ref) in a loop and a reader samples `main`. A sampler of
//! `/proc/locks` ([`LockHolders`]) names the holder of the writer lock every millisecond without
//! taking it, so each commit's wait behind the import, `behind` (the import's hold time inside
//! the commit's life, an upper bound of its wait behind it), prints beside its wall time from
//! spawn to exit, with the import's longest hold. Two more cases: the same committers alone for
//! 120 s (the control lane: their own queue), and the import's peak resident memory on 1,000
//! and on 12,000 commits alone, with its longest hold of the writer lock on 1,000 (its longest
//! frame commit or flush, the M17 design §3.1). The figures print as `intake-perf: ` lines. The
//! cases assert only what they ran (every command exits 0 or retries an `E_BUSY`, the readers
//! see the old head until the import's end, the store verifies), never the time: a missed target
//! is recorded in `docs/perf.md`. They are ignored in `just check`; `just perf` runs them in a
//! release build.
//!
//! ```text
//!   GitServer: fast-import ──▶ repo.git ──▶ swarmops git import ──▶ store (main moves once)
//!                                               8 × swarmops commit (loop) ──┘ lock waits
//!                                               1 × swarmops ref list (loop) ── old head?
//!                                               /proc/locks every 1 ms ── who holds meta/lock
//! ```

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use swarmops_testkit::accounting::{LockHolders, PeakWatch};
use swarmops_testkit::child::Child;
use swarmops_testkit::remote::GitServer;
use tempfile::TempDir;

use crate::serve_perf::{fast_import, mib};
use crate::support;

/// The history the roadmap's targets name.
const LONG: usize = 12_000;

/// The smaller history, for the import's memory beside the long one's.
const SHORT: usize = 1_000;

/// How many agents commit beside the import.
const COMMITTERS: usize = 8;

/// How long the import of [`LONG`] commits may take: a loaded host takes minutes.
const SLOW_RUN: Duration = Duration::from_secs(3_600);

/// How long a single `commit` or `ref list` may take.
const STEP: Duration = Duration::from_secs(120);

/// How often the reader looks at `main`.
const READ_EVERY: Duration = Duration::from_millis(200);

/// A read that ended this long before the import did cannot have seen its move: the import exits
/// right after the move, and the test notes the end after the process is reaped.
const READ_TAIL: Duration = Duration::from_secs(2);

/// The agent that imports.
const IMPORTER: &str = "agent/importer";

/// How long the committers of the control lane run.
const ALONE_FOR: Duration = Duration::from_secs(120);

/// A bare repository of `commits` commits on `main`.
fn bare(commits: usize) -> (GitServer, std::path::PathBuf) {
    let server = GitServer::new();
    let bare = server.create("repo");
    fast_import(&server, &bare, commits);
    (server, bare)
}

/// A new store in `dir`: its path.
fn store(dir: &Path, name: &str) -> String {
    let path = dir.join(name);
    let path = path.to_str().expect("UTF-8 paths").to_owned();
    support::run_within(&["init", &path], &[], STEP).ok();
    path
}

/// What one `git import` cost.
struct Imported {
    /// From spawn to exit.
    elapsed: Duration,
    /// The largest `VmHWM` of its process, in KiB.
    peak: u64,
    /// Its process, the holder [`LockHolders`] names.
    pid: u32,
}

/// `git import` of `bare`'s `main` into the store at `store`.
fn import(store: &str, bare: &Path) -> Imported {
    let bare = bare.to_str().expect("UTF-8 paths");
    let line = [
        "--repo", store, "--as", IMPORTER, "git", "import", bare, "--ref", "main", "--into", "main",
    ];
    let mut command = support::swarmops(&line, &[]);
    let started = Instant::now();
    let child = Child::spawn(&mut command);
    let pid = child.id();
    let watch = PeakWatch::start(pid);
    let output = child.wait(started + SLOW_RUN);
    let peak = watch.finish();
    let elapsed = started.elapsed();
    assert!(
        output.status.success(),
        "git import: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Imported { elapsed, peak, pid }
}

/// The writer lock of the store at `store`.
fn meta_lock(store: &str) -> PathBuf {
    let layout = swarmops_testkit::store::layout_of(store);
    layout.meta_lock().into_std_path_buf()
}

/// What one committer saw.
#[derive(Default)]
struct Committed {
    /// The life of each commit that succeeded: its spawn and its exit.
    lives: Vec<(Instant, Instant)>,
    /// The wall time of each run that met `E_BUSY` (exit 4), which the committer ran again.
    busy: Vec<Duration>,
}

/// Commits in a loop as agent `n` until `done`: a file of its own each time, no ref moved.
fn commit_loop(store: &str, dir: &Path, n: usize, done: &AtomicBool) -> Committed {
    let agent = format!("agent/committer-{n}");
    let changes = dir.join(format!("changes-{n}.json"));
    let changes = changes.to_str().expect("UTF-8 paths").to_owned();
    let mut seen = Committed::default();
    let mut round = 0;
    while !done.load(Ordering::Relaxed) {
        let list = format!(
            r#"[{{"path":"c{n}/f{round}.txt","op":"put","content":{{"inline":"{n} {round}\n"}}}}]"#
        );
        fs::write(&changes, list).expect("write a change list");
        let message = format!("c{n} {round}");
        let line = [
            "--repo",
            store,
            "--as",
            &agent,
            "commit",
            "-m",
            &message,
            "--changes",
            &changes,
        ];
        let started = Instant::now();
        let run = support::run_within(&line, &[], STEP);
        let ended = Instant::now();
        if run.busy() {
            seen.busy.push(ended - started);
            continue;
        }
        assert_eq!(run.exit(), 0, "commit: {run}");
        seen.lives.push((started, ended));
        round += 1;
    }
    seen
}

/// The target `main` names, from one `ref list`; `None` where there is no `main` (or the read
/// failed).
fn main_target(store: &str) -> Option<String> {
    let run = support::run_within(
        &["--repo", store, "ref", "list", "--prefix", "main"],
        &[],
        STEP,
    );
    if run.exit() != 0 {
        return None;
    }
    let envelope: Value = serde_json::from_str(&run.stdout).ok()?;
    let refs = envelope["result"]["refs"].as_array()?;
    let main = refs.iter().find(|entry| entry["name"] == "main")?;
    Some(main["target"].as_str()?.to_owned())
}

/// The longest and the median of `times`.
fn longest_median(times: &[Duration]) -> (Duration, Duration) {
    let mut sorted = times.to_vec();
    sorted.sort_unstable();
    let longest = sorted.last().copied().unwrap_or_default();
    (
        longest,
        sorted.get(sorted.len() / 2).copied().unwrap_or_default(),
    )
}

/// The median, the 99th percentile and the largest of `times`.
fn percentiles(times: &[Duration]) -> (Duration, Duration, Duration) {
    let mut sorted = times.to_vec();
    sorted.sort_unstable();
    let at = |per_cent: usize| {
        let index = (sorted.len() * per_cent / 100).min(sorted.len().saturating_sub(1));
        sorted.get(index).copied().unwrap_or_default()
    };
    (at(50), at(99), sorted.last().copied().unwrap_or_default())
}

/// `times` as `p50 0.048 s, p99 0.112 s, max 0.210 s`.
fn spread(times: &[Duration]) -> String {
    let (p50, p99, max) = percentiles(times);
    format!(
        "p50 {:.3} s, p99 {:.3} s, max {:.3} s",
        p50.as_secs_f64(),
        p99.as_secs_f64(),
        max.as_secs_f64()
    )
}

/// [`COMMITTERS`] committers on the store at `store` until `done`: what each saw.
fn committers(store: &str, dir: &Path, done: &AtomicBool) -> Vec<Committed> {
    thread::scope(|scope| {
        let runs: Vec<_> = (0..COMMITTERS)
            .map(|n| scope.spawn(move || commit_loop(store, dir, n, done)))
            .collect();
        runs.into_iter()
            .map(|one| one.join().expect("a committer ran"))
            .collect()
    })
}

/// The import's peak resident memory on 1,000 and on 12,000 commits, each alone, and its longest
/// hold of the writer lock on 1,000.
#[test]
#[ignore = "slow, ~470 s (469.8 s optimized at load 28 to 5, idle not measured): imports 13,000 commits alone, run by `just perf`"]
fn import_memory() {
    let work = TempDir::new().expect("a temp dir");

    // 1. The short history, its holds of the writer lock named by the sampler.
    let (_short_server, short) = bare(SHORT);
    let alone = store(work.path(), "short");
    let mut holders = LockHolders::start(&meta_lock(&alone));
    let imported = import(&alone, &short);
    holders.stop();
    let (longest_hold, holds) = holders.longest(imported.pid);
    println!(
        "intake-perf: import of {SHORT} commits: {:.3} s, peak memory {:.1} MiB; longest \
         writer-lock hold {:.3} s of {holds}",
        imported.elapsed.as_secs_f64(),
        mib(imported.peak),
        longest_hold.as_secs_f64(),
    );

    // 2. The long history, alone.
    let (_long_server, long) = bare(LONG);
    let shared = store(work.path(), "long");
    let imported = import(&shared, &long);
    println!(
        "intake-perf: import of {LONG} commits alone: {:.3} s, peak memory {:.1} MiB",
        imported.elapsed.as_secs_f64(),
        mib(imported.peak),
    );
    assert!(
        main_target(&shared).is_some(),
        "the import did not move main"
    );
    support::run_within(&["--repo", &shared, "verify", "--deep"], &[], SLOW_RUN).ok();
}

/// The control lane: the same committers for [`ALONE_FOR`] with no import, their wall time from
/// spawn to exit (their own queue, read beside the import's lane).
#[test]
#[ignore = "slow, ~128 s (127.5 s optimized at load 28, idle not measured): 8 committers for 120 s, run by `just perf`"]
fn committers_alone() {
    let work = TempDir::new().expect("a temp dir");
    let shared = store(work.path(), "alone");
    let done = AtomicBool::new(false);
    let committed = thread::scope(|scope| {
        let run = scope.spawn(|| committers(&shared, work.path(), &done));
        thread::sleep(ALONE_FOR);
        done.store(true, Ordering::Relaxed);
        run.join().expect("the committers ran")
    });

    // The figures.
    let waits: Vec<_> = (committed.iter())
        .flat_map(|one| one.lives.iter().map(|&(from, to)| to - from))
        .collect();
    let busy: usize = committed.iter().map(|one| one.busy.len()).sum();
    assert!(!waits.is_empty(), "no commit ran");
    println!(
        "intake-perf: {COMMITTERS} committers alone for {} s: {} commits, spawn to exit {}; \
         {busy} E_BUSY",
        ALONE_FOR.as_secs(),
        waits.len(),
        spread(&waits),
    );
    support::run_within(&["--repo", &shared, "verify", "--deep"], &[], SLOW_RUN).ok();
}

/// 8 committers and a reader beside the import of 12,000 commits: each commit's wait behind the
/// import (`behind`) and its wall time, and the import's longest hold of the writer lock.
#[test]
#[ignore = "slow, ~990 s (989.9 s optimized at load 65 to 28, idle not measured): imports 12,000 commits with 8 committers beside, run by `just perf`"]
fn commits_beside_an_import() {
    let work = TempDir::new().expect("a temp dir");

    // 1. The long history, with the committers, the reader and the lock sampler beside it.
    let (_server, long) = bare(LONG);
    let shared = store(work.path(), "long");
    let mut holders = LockHolders::start(&meta_lock(&shared));
    let done = AtomicBool::new(false);
    let (committed, reads, imported, ended) = thread::scope(|scope| {
        let committing = scope.spawn(|| committers(&shared, work.path(), &done));
        let reader = scope.spawn(|| {
            let mut reads = Vec::new();
            while !done.load(Ordering::Relaxed) {
                let target = main_target(&shared);
                reads.push((Instant::now(), target));
                thread::sleep(READ_EVERY);
            }
            reads
        });
        let imported = import(&shared, &long);
        let ended = Instant::now();
        done.store(true, Ordering::Relaxed);
        (
            committing.join().expect("the committers ran"),
            reader.join().expect("the reader ran"),
            imported,
            ended,
        )
    });
    holders.stop();

    // 2. The readers saw the old head (no main) until the import's end.
    let early: Vec<_> = reads
        .iter()
        .filter(|(at, target)| target.is_some() && *at + READ_TAIL < ended)
        .collect();
    assert!(
        early.is_empty(),
        "{} reads saw the new head before the import ended",
        early.len()
    );
    assert!(
        main_target(&shared).is_some(),
        "the import did not move main"
    );

    // 3. The figures: each commit's wall time, and the import's hold time inside its life.
    let lives: Vec<_> = committed.iter().flat_map(|one| one.lives.clone()).collect();
    let waits: Vec<_> = lives.iter().map(|&(from, to)| to - from).collect();
    let behind: Vec<_> = (lives.iter())
        .map(|&(from, to)| holders.held_by(imported.pid, from, to))
        .collect();
    let busy: Vec<_> = committed.iter().flat_map(|one| one.busy.clone()).collect();
    let (longest, median) = longest_median(&waits);
    let (longest_busy, _) = longest_median(&busy);
    let (longest_hold, holds) = holders.longest(imported.pid);
    println!(
        "intake-perf: import of {LONG} commits beside {COMMITTERS} committers: {:.3} s; {} \
         commits, longest {:.3} s, median {:.3} s; {} E_BUSY, longest {:.3} s; {} reads of \
         main, none saw the new head before the end",
        imported.elapsed.as_secs_f64(),
        waits.len(),
        longest.as_secs_f64(),
        median.as_secs_f64(),
        busy.len(),
        longest_busy.as_secs_f64(),
        reads.len(),
    );
    println!(
        "intake-perf: beside the import of {LONG} commits, per commit: behind the import {} \
         (target at most 0.100 s); spawn to exit {}; the import's longest writer-lock hold \
         {:.3} s of {holds}",
        spread(&behind),
        spread(&waits),
        longest_hold.as_secs_f64(),
    );

    // 4. The store is whole.
    support::run_within(&["--repo", &shared, "verify", "--deep"], &[], SLOW_RUN).ok();
}
