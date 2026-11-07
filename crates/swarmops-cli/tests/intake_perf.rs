//! What an import costs the others (M17 design §1, R-2 and A39), for the `perf` recipe.
//!
//! A 12,000-commit history is imported into `main` of a store while 8 committers each run
//! `commit` (a commit that moves no ref) in a loop and a reader samples `main`. The longest wait
//! of a committer, its `E_BUSY` count, whether a reader saw the new head before the import ended,
//! the import's peak resident memory on 1,000 and on 12,000 commits, and the longest hold of the
//! writer lock during the import of 1,000 commits alone, which is its longest frame commit or
//! flush (the M17 design §3.1), print as `intake-perf: ` lines. The case asserts only what it ran (every command exits 0 or retries an `E_BUSY`, the
//! readers see the old head until the import's end, the store verifies), never the time: a
//! missed target is recorded in `docs/perf.md`. It is ignored in `just check`; `just perf` runs it
//! in a release build.
//!
//! ```text
//!   GitServer: fast-import ──▶ repo.git ──▶ swarmops git import ──▶ store (main moves once)
//!                                               8 × swarmops commit (loop) ──┘ lock waits
//!                                               1 × swarmops ref list (loop) ── old head?
//! ```

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use swarmops_store::{MetaLock, StoreError};
use swarmops_testkit::accounting::PeakWatch;
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

/// How often the lock probe tries the writer lock: the resolution of the longest hold it sees.
const PROBE_EVERY: Duration = Duration::from_millis(1);

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

/// `git import` of `bare`'s `main` into the store at `store`: how long it took and the largest
/// `VmHWM` of its process, in KiB.
fn import(store: &str, bare: &Path) -> (Duration, u64) {
    let bare = bare.to_str().expect("UTF-8 paths");
    let line = [
        "--repo", store, "--as", IMPORTER, "git", "import", bare, "--ref", "main", "--into", "main",
    ];
    let mut command = support::swarmops(&line, &[]);
    let started = Instant::now();
    let child = Child::spawn(&mut command);
    let watch = PeakWatch::start(child.id());
    let output = child.wait(started + SLOW_RUN);
    let peak = watch.finish();
    let elapsed = started.elapsed();
    assert!(
        output.status.success(),
        "git import: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    (elapsed, peak)
}

/// The longest the writer lock of the store at `store` stayed held, and how many holds, as a
/// probe that tries it every [`PROBE_EVERY`] without waiting sees them until `done`: holds back
/// to back count as one, and each is seen to about a period. The probe holds the lock for no
/// longer than a try.
fn lock_holds(store: &str, done: &AtomicBool) -> (Duration, usize) {
    let layout = swarmops_testkit::store::layout_of(store);
    let mut longest = Duration::ZERO;
    let mut holds = 0;
    let mut since: Option<Instant> = None;
    while !done.load(Ordering::Relaxed) {
        let now = Instant::now();
        match MetaLock::acquire(&layout, Duration::ZERO) {
            // 1. Held: a hold starts, unless one is under way.
            Err(StoreError::Busy) => {
                if since.is_none() {
                    since = Some(now);
                    holds += 1;
                }
            }
            // 2. Free: the hold under way, if any, ended.
            Ok(lock) => {
                drop(lock);
                if let Some(start) = since.take() {
                    longest = longest.max(now - start);
                }
            }
            Err(error) => panic!("the lock probe: {error}"),
        }
        thread::sleep(PROBE_EVERY);
    }
    (longest, holds)
}

/// What one committer saw.
#[derive(Default)]
struct Committed {
    /// The wall time of each commit that succeeded, from spawn to exit.
    waits: Vec<Duration>,
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
        let wait = started.elapsed();
        if run.busy() {
            seen.busy.push(wait);
            continue;
        }
        assert_eq!(run.exit(), 0, "commit: {run}");
        seen.waits.push(wait);
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

/// 8 committers and a reader beside the import of 12,000 commits, and the import's memory on
/// 1,000 and on 12,000.
#[test]
#[ignore = "slow, ~12 min (estimate): imports 13,000 commits with 8 committers beside, run by `just perf`"]
fn commits_beside_an_import() {
    let work = TempDir::new().expect("a temp dir");

    // 1. The import's memory and its longest hold of the writer lock on the short history,
    //    alone.
    let (_short_server, short) = bare(SHORT);
    let alone = store(work.path(), "short");
    let probed = AtomicBool::new(false);
    let ((elapsed, peak), (longest_hold, holds)) = thread::scope(|scope| {
        let probe = scope.spawn(|| lock_holds(&alone, &probed));
        let imported = import(&alone, &short);
        probed.store(true, Ordering::Relaxed);
        (imported, probe.join().expect("the lock probe ran"))
    });
    println!(
        "intake-perf: import of {SHORT} commits: {:.3} s, peak memory {:.1} MiB; longest \
         writer-lock hold {:.3} s of {holds}",
        elapsed.as_secs_f64(),
        mib(peak),
        longest_hold.as_secs_f64(),
    );

    // 2. The long history, with the committers and the reader running beside it.
    let (_server, long) = bare(LONG);
    let shared = store(work.path(), "long");
    let done = AtomicBool::new(false);
    let (committed, reads, imported, peak, ended) = thread::scope(|scope| {
        let committers: Vec<_> = (0..COMMITTERS)
            .map(|n| {
                let (shared, done, dir) = (shared.as_str(), &done, work.path());
                scope.spawn(move || commit_loop(shared, dir, n, done))
            })
            .collect();
        let reader = scope.spawn(|| {
            let mut reads = Vec::new();
            while !done.load(Ordering::Relaxed) {
                let target = main_target(&shared);
                reads.push((Instant::now(), target));
                thread::sleep(READ_EVERY);
            }
            reads
        });
        let (imported, peak) = import(&shared, &long);
        let ended = Instant::now();
        done.store(true, Ordering::Relaxed);
        let committed: Vec<_> = committers
            .into_iter()
            .map(|one| one.join().expect("a committer ran"))
            .collect();
        (
            committed,
            reader.join().expect("the reader ran"),
            imported,
            peak,
            ended,
        )
    });

    // 3. The readers saw the old head (no main) until the import's end.
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

    // 4. The figures.
    let waits: Vec<_> = committed.iter().flat_map(|one| one.waits.clone()).collect();
    let busy: Vec<_> = committed.iter().flat_map(|one| one.busy.clone()).collect();
    let (longest, median) = longest_median(&waits);
    let (longest_busy, _) = longest_median(&busy);
    println!(
        "intake-perf: import of {LONG} commits beside {COMMITTERS} committers: {:.3} s, peak \
         memory {:.1} MiB; {} commits, longest {:.3} s, median {:.3} s; {} E_BUSY, longest \
         {:.3} s; {} reads of main, none saw the new head before the end",
        imported.as_secs_f64(),
        mib(peak),
        waits.len(),
        longest.as_secs_f64(),
        median.as_secs_f64(),
        busy.len(),
        longest_busy.as_secs_f64(),
        reads.len(),
    );

    // 5. The store is whole.
    support::run_within(&["--repo", &shared, "verify", "--deep"], &[], SLOW_RUN).ok();
}
