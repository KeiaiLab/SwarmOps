//! G2 (docs/goals.md): what one commit costs with its syncs left out, against the target of
//! 5 ms plus 0.3 ms per changed file, and what it costs with full fsync, the only mode.
//!
//! A store of 1,000 files in 100 directories is built in a fresh temporary directory under
//! `SWARMOPS_BENCH_DIR` (default `~/.local/state/swarmops-bench`), and `main` points at it.
//! Each timed iteration is one `LocalRepo::commit` of `k` changed files (1, 10 and 100, each
//! rewritten with a new version) on `main`'s head, moving `main` (`advance`), as `swarmops
//! commit --advance` does inside the binary. Three filesystems, each a store of its own:
//!
//! - `nosync`: every sync skipped (the test kit's `NoSyncFs`), the store's work alone;
//! - `fsync`: the real filesystem, every sync made (`RealFs`, the product's default);
//! - `fsync_less_syncs`: the real filesystem, each commit's time less the time it spent in the
//!   calls that sync (`write_durable_temp`, `fsync_dir`, `append_durable`, `atomic_replace`,
//!   `sync_data`, `rename_durable`), counted and timed around each call. The timed calls hold
//!   their writes too, so this is a lower bound of the work without syncs, where `nosync` is
//!   an upper one (its writes still reach the page cache).
//!
//! Besides criterion's estimate, each variant prints the nearest-rank percentiles of every
//! commit it timed, warm-up included, and for `fsync_less_syncs` the syncing calls per commit
//! (`percentiles commit/<fs>/<k>: …`, a line `just perf` shows). The product's default stays
//! full fsync: the no-sync filesystem exists only in this bench. `docs/perf.md` records it;
//! the gate only builds it.

use std::hint::black_box;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use camino::Utf8PathBuf;
use criterion::{Criterion, criterion_group, criterion_main};
use swarmops_core::ChangesetId;
use swarmops_repo::{Advance, Change, CommitRequest, LocalRepo};
use swarmops_store::{FsMethod, FsOps, RealFs};
use swarmops_testkit::fs::{Call, NoSyncFs, Watch, Watched};
use swarmops_testkit::measure::base_dir;
use swarmops_testkit::repo::{put, request};

/// The changed files per commit measured.
const FILES: [usize; 3] = [1, 10, 100];

/// The base's directories, and the files in each.
const DIRS: usize = 100;
const PER_DIR: usize = 10;

/// Who commits.
const ACTOR: &str = "bench/commit";

/// Fewer timed commits than this, as criterion's test mode makes, print no percentiles.
const MIN_SAMPLES: usize = 50;

/// The calls that sync, for `fsync_less_syncs`.
const SYNCING: [FsMethod; 6] = [
    FsMethod::WriteDurableTemp,
    FsMethod::FsyncDir,
    FsMethod::AppendDurable,
    FsMethod::AtomicReplace,
    FsMethod::SyncData,
    FsMethod::RenameDurable,
];

/// How the store under a variant syncs.
#[derive(Clone, Copy)]
enum Syncs {
    /// None at all.
    Skipped,
    /// Every one, as the product does.
    Made,
    /// Every one, timed so that its time can be taken off.
    Subtracted,
}

impl Syncs {
    /// The variant's name.
    fn name(self) -> &'static str {
        match self {
            Syncs::Skipped => "nosync",
            Syncs::Made => "fsync",
            Syncs::Subtracted => "fsync_less_syncs",
        }
    }
}

/// Counts and times the calls that sync ([`SYNCING`]); one commit runs at a time.
#[derive(Default)]
struct SyncClock {
    /// The syncing calls so far.
    calls: AtomicU64,
    /// Nanoseconds in them so far.
    ns: AtomicU64,
    /// When the call in flight began, in nanoseconds since `epoch`.
    began: AtomicU64,
    /// The clock's origin.
    epoch: std::sync::OnceLock<Instant>,
}

impl SyncClock {
    /// Nanoseconds since the clock's origin.
    fn now(&self) -> u64 {
        let epoch = self.epoch.get_or_init(Instant::now);
        u64::try_from(epoch.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }

    /// The calls and the time counted so far.
    fn read(&self) -> (u64, Duration) {
        let calls = self.calls.load(Ordering::Relaxed);
        (calls, Duration::from_nanos(self.ns.load(Ordering::Relaxed)))
    }
}

impl Watch for SyncClock {
    fn before(&self, call: &Call<'_>) -> io::Result<()> {
        if SYNCING.contains(&call.method) {
            self.began.store(self.now(), Ordering::Relaxed);
        }
        Ok(())
    }

    fn after(&self, call: &Call<'_>) {
        if !SYNCING.contains(&call.method) {
            return;
        }
        let took = self
            .now()
            .saturating_sub(self.began.load(Ordering::Relaxed));
        self.ns.fetch_add(took, Ordering::Relaxed);
        self.calls.fetch_add(1, Ordering::Relaxed);
    }
}

/// The `i`-th file of the base: 100 directories of 10 files.
fn file(i: usize) -> String {
    format!("src/d{:02}/f{}.rs", i % DIRS, i / DIRS)
}

/// The content of file `i` at `version`.
fn text(i: usize, version: u64) -> Vec<u8> {
    format!("// file {i}\nfn f{i}() {{}}\n// version {version}\n").into_bytes()
}

/// One store of a variant: its directory, the repository, `main`'s head and the clock.
struct Fixture {
    /// The store's directory, removed when the fixture is dropped.
    _dir: tempfile::TempDir,
    repo: LocalRepo,
    head: ChangesetId,
    clock: Arc<SyncClock>,
}

impl Fixture {
    /// A store whose `main` holds the 1,000-file base, syncing as `syncs` says.
    fn build(syncs: Syncs) -> Fixture {
        // 1. The store, on the filesystem the variant names.
        let dir = tempfile::tempdir_in(base_dir()).expect("a temp dir");
        let at = Utf8PathBuf::from_path_buf(dir.path().join("store")).expect("UTF-8");
        let clock = Arc::new(SyncClock::default());
        let fs: Arc<dyn FsOps> = match syncs {
            Syncs::Skipped => Arc::new(NoSyncFs),
            Syncs::Made => Arc::new(RealFs::new()),
            Syncs::Subtracted => Arc::new(Watched(Arc::clone(&clock), RealFs::new())),
        };
        let repo = LocalRepo::create_with_fs(&at, "bench", fs).expect("create");

        // 2. The base, on main.
        let all = (0..DIRS * PER_DIR)
            .map(|i| put(&file(i), &text(i, 0)))
            .collect();
        let mut req: CommitRequest = request(&[], all, "base");
        req.advance = Some(Advance::new("main", ACTOR));
        let head = repo.commit(req).expect("commit the base").id;
        Fixture {
            _dir: dir,
            repo,
            head,
            clock,
        }
    }

    /// Commits `files` changed files at `version` on the head and moves `main` there: how long
    /// it took, and the syncing calls it made with their time.
    fn commit(&mut self, files: usize, version: u64) -> (Duration, u64, Duration) {
        let changes: Vec<Change> = (0..files)
            .map(|i| put(&file(i * 7 % (DIRS * PER_DIR)), &text(i, version)))
            .collect();
        let mut req: CommitRequest = request(&[self.head], changes, "a bench commit");
        req.advance = Some(Advance::new("main", ACTOR));
        let (calls, synced) = self.clock.read();
        let started = Instant::now();
        let id = self.repo.commit(req).expect("commit").id;
        let took = started.elapsed();
        let (calls_after, synced_after) = self.clock.read();
        self.head = black_box(id);
        (
            took,
            calls_after - calls,
            synced_after.saturating_sub(synced),
        )
    }
}

/// The nearest-rank `per_mille` / 1000 percentile of `sorted`, which is not empty.
fn at(sorted: &[Duration], per_mille: usize) -> Duration {
    let rank = (sorted.len() * per_mille).div_ceil(1000).max(1);
    sorted[rank - 1]
}

/// A count as a float: every count here fits in 32 bits.
fn small(n: impl TryInto<u32>) -> f64 {
    f64::from(n.try_into().unwrap_or(u32::MAX))
}

/// Milliseconds, for the printed lines.
fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1e3
}

/// Every variant, a store per filesystem and change size.
fn commit_latency(c: &mut Criterion) {
    let mut group = c.benchmark_group("commit");
    group
        .sample_size(10)
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(5));
    for syncs in [Syncs::Skipped, Syncs::Made, Syncs::Subtracted] {
        for files in FILES {
            // 1. Time each commit, its syncs taken off for the subtracting variant.
            let mut fixture = Fixture::build(syncs);
            let mut samples: Vec<Duration> = Vec::new();
            let mut calls = 0;
            let mut version = 1;
            let name = format!("{}/{files}", syncs.name());
            group.bench_function(&name, |b| {
                b.iter_custom(|iters| {
                    let mut total = Duration::ZERO;
                    for _ in 0..iters {
                        let (took, made, synced) = fixture.commit(files, version);
                        let counted = match syncs {
                            Syncs::Subtracted => took.saturating_sub(synced),
                            Syncs::Skipped | Syncs::Made => took,
                        };
                        total += counted;
                        samples.push(counted);
                        calls += made;
                        version += 1;
                    }
                    total
                });
            });

            // 2. The percentiles, once criterion has measured (its test mode runs once).
            if samples.len() < MIN_SAMPLES {
                continue;
            }
            samples.sort_unstable();
            let n = samples.len();
            let per_commit = small(calls) / small(n);
            let target = 5.0 + 0.3 * small(files);
            println!(
                "percentiles commit/{name}: n {n} p50 {:.2} ms p90 {:.2} ms p99 {:.2} ms max \
                 {:.2} ms; syncing calls per commit {per_commit:.1}; target {target:.1} ms",
                millis(at(&samples, 500)),
                millis(at(&samples, 900)),
                millis(at(&samples, 990)),
                millis(samples[n - 1]),
            );
        }
    }
    group.finish();
}

criterion_group!(benches, commit_latency);
criterion_main!(benches);
