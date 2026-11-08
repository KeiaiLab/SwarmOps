//! The all-parent ancestry read (the M16 design §5): what one `RepoRead::ancestry` costs on a
//! merge-rich history of 20,000 and of 200,000 changesets.
//!
//! The history is a mainline in which every 16th changeset merges a short branch: one side
//! changeset on the mainline eight changesets back. A store is built per size in a fresh
//! temporary directory under `SWARMOPS_BENCH_DIR` (default `~/.local/state/swarmops-bench`),
//! through the test kit's `NoSyncFs` (the reads sync nothing), as raw batches:
//!
//! ```text
//!   m0 ─ m1 ─ … ─ m8 ─ … ─ m15 ─ m16 ─ …      every 16th mainline changeset a merge
//!                  └──── s16 ─────┘           of the one before and a side changeset
//! ```
//!
//! Three reads per size:
//!
//! - `is_ancestor_far`: `m1` against the tip with a page of 0: the walk crosses the whole
//!   history;
//! - `merge_base_100`: the tip against a branch off the mainline 100 changesets below it;
//! - `between_page_50`: a page of 50 of the 1,000 mainline changesets below the tip.
//!
//! And one per size of the `merge_base` operation's store reads (the M18 design §8),
//! `merge_base/<size>`: `RepoRead::merge_base` of a change on the branch (one file changed)
//! against the tip, along first parents, then the tree diff from that base to the change; the
//! operation's hunks and JSON are left out.
//!
//! Besides criterion's estimate, each read prints the nearest-rank percentiles of every call it
//! timed, warm-up included (`percentiles ancestry/<size>/<read>: …`, `percentiles
//! merge_base/<size>: …`). `bench/thresholds.toml` gates the medians from two quiet runs on (it
//! skips them until then); `docs/perf.md` records the p99s.

use std::hint::black_box;
use std::sync::Arc;
use std::time::{Duration, Instant};

use camino::Utf8PathBuf;
use criterion::{Criterion, criterion_group, criterion_main};
use swarmops_core::{ChangesetId, LeafKind, ObjectKind, Tree, TreeEntry, TreeId};
use swarmops_repo::merge::tree_diff::PathFilter;
use swarmops_repo::{LocalRepo, RawChangeset};
use swarmops_testkit::fs::NoSyncFs;
use swarmops_testkit::measure::base_dir;
use swarmops_testkit::repo::{blob, signature};
use tempfile::TempDir;

/// The history sizes measured, in changesets, with their names.
const SIZES: [(usize, &str); 2] = [(20_000, "20k"), (200_000, "200k")];

/// Every this many mainline changesets, one merges a side changeset.
const MERGE_EVERY: usize = 16;

/// How far back on the mainline a side changeset starts.
const SIDE_BACK: usize = 8;

/// How far below the tip `merge_base_100`'s branch starts.
const BASE_DEPTH: usize = 100;

/// How far below the tip `between_page_50`'s `from` is, and its page.
const BETWEEN_DEPTH: usize = 1000;
const PAGE: usize = 50;

/// The changesets of one raw batch, a merge's side changeset aside: within a journal frame's
/// 65,535 records.
const BATCH: usize = 50_000;

/// Fewer timed calls than this, as criterion's test mode makes, print no percentiles.
const MIN_SAMPLES: usize = 10;

/// A store of one merge-rich history, and the changesets the reads start from.
struct Fixture {
    _dir: TempDir,
    repo: LocalRepo,
    mainline: Vec<ChangesetId>,
    branch: ChangesetId,
    /// A changeset on `branch` that changes its one file.
    change: ChangesetId,
}

impl Fixture {
    /// The history of `size` changesets, mainline and side ones together, then the branch.
    fn build(size: usize) -> Fixture {
        let dir = tempfile::tempdir_in(base_dir()).expect("a temp dir");
        let at = Utf8PathBuf::from_path_buf(dir.path().join("store")).expect("a UTF-8 path");
        let repo = LocalRepo::create_with_fs(&at, "bench", Arc::new(NoSyncFs)).expect("create");
        let root = one_file_tree(&repo, b"f");

        // 1. The mainline, a merge of a side changeset every MERGE_EVERY, in raw batches.
        let mut mainline: Vec<ChangesetId> = Vec::new();
        let mut made = 0;
        while made < size {
            let mut batch = repo.raw_batch();
            while made < size && batch.len() < BATCH {
                let place = mainline.len();
                let mut parents: Vec<ChangesetId> = mainline.last().copied().into_iter().collect();
                if place >= MERGE_EVERY && place.is_multiple_of(MERGE_EVERY) {
                    let back = mainline[place - SIDE_BACK];
                    let merged = raw(vec![back], root, &format!("s{place}"));
                    parents.push(batch.push(merged).expect("a side changeset"));
                    made += 1;
                }
                let next = raw(parents, root, &format!("m{place}"));
                mainline.push(batch.push(next).expect("a mainline changeset"));
                made += 1;
            }
            batch.commit().expect("commit a batch");
        }

        // 2. The branch off the mainline below the tip.
        let below = mainline[mainline.len() - 1 - BASE_DEPTH];
        let branch = raw(vec![below], root, "branch");
        let branch = repo.commit_raw(branch).expect("the branch").outcomes[0].id;

        // 3. A change on the branch, for `merge_base`'s diff.
        let changed = one_file_tree(&repo, b"g");
        let change = raw(vec![branch], changed, "change");
        let change = repo.commit_raw(change).expect("the change").outcomes[0].id;
        Fixture {
            _dir: dir,
            repo,
            mainline,
            branch,
            change,
        }
    }

    /// The tip of the mainline.
    fn tip(&self) -> ChangesetId {
        *self.mainline.last().expect("a mainline")
    }

    /// The mainline changeset `depth` below the tip.
    fn below(&self, depth: usize) -> ChangesetId {
        self.mainline[self.mainline.len() - 1 - depth]
    }
}

/// The tree of one file `f` holding `content`, stored: every changeset of the bench has it, with
/// `f`, but `merge_base`'s change.
fn one_file_tree(repo: &LocalRepo, content: &[u8]) -> TreeId {
    let entry = TreeEntry::leaf("f", LeafKind::File, blob(content));
    let tree = Tree::new(vec![entry]).expect("a valid tree");
    let objects = vec![
        (ObjectKind::Blob, content.to_vec()),
        (ObjectKind::Tree, tree.encode()),
    ];
    repo.put_objects(objects).expect("put the tree");
    tree.id()
}

/// A raw changeset of `root` on `parents`, named by `message`.
fn raw(parents: Vec<ChangesetId>, root: TreeId, message: &str) -> RawChangeset {
    RawChangeset::new(
        parents,
        root,
        signature(),
        signature(),
        message.as_bytes().to_vec(),
        Vec::new(),
        Vec::new(),
    )
}

/// One read: its name, its `from` and `to`, and its page.
struct Read {
    name: &'static str,
    from: ChangesetId,
    to: ChangesetId,
    limit: usize,
}

/// The nearest-rank percentile `per_mille` of `sorted`.
fn at(sorted: &[Duration], per_mille: usize) -> Duration {
    let rank = (sorted.len() * per_mille).div_ceil(1000).max(1);
    sorted[rank - 1]
}

/// Milliseconds, for the printed lines.
fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1e3
}

/// Every read, a store per size: the ancestry reads, then `merge_base`'s.
fn ancestry(c: &mut Criterion) {
    for (size, size_name) in SIZES {
        let fixture = Fixture::build(size);
        let reads = [
            Read {
                name: "is_ancestor_far",
                from: fixture.mainline[1],
                to: fixture.tip(),
                limit: 0,
            },
            Read {
                name: "merge_base_100",
                from: fixture.tip(),
                to: fixture.branch,
                limit: 0,
            },
            Read {
                name: "between_page_50",
                from: fixture.below(BETWEEN_DEPTH),
                to: fixture.tip(),
                limit: PAGE,
            },
        ];

        // 1. The ancestry reads, each call in a snapshot of its own.
        let mut group = configured(c, "ancestry");
        for read in reads {
            let name = format!("{size_name}/{}", read.name);
            timed(&mut group, "ancestry", &name, || {
                let snapshot = fixture.repo.snapshot().expect("a snapshot");
                let found = snapshot
                    .ancestry(read.from, read.to, read.limit, None)
                    .expect("an ancestry");
                black_box(found);
            });
        }
        group.finish();

        // 2. `merge_base`'s reads: the base of the change and the tip, then the diff to it.
        let mut group = configured(c, "merge_base");
        timed(&mut group, "merge_base", size_name, || {
            let snapshot = fixture.repo.snapshot().expect("a snapshot");
            let base = snapshot
                .merge_base(fixture.change, fixture.tip())
                .expect("a merge base")
                .expect("a shared root");
            let changes = snapshot
                .diff(base, fixture.change, &PathFilter::all())
                .expect("a diff");
            assert_eq!(changes.len(), 1, "the change changes one file");
            black_box(changes);
        });
        group.finish();
    }
}

/// Criterion's group `name`, as every read of this bench is measured.
fn configured<'a>(
    c: &'a mut Criterion,
    name: &str,
) -> criterion::BenchmarkGroup<'a, criterion::measurement::WallTime> {
    let mut group = c.benchmark_group(name);
    group
        .sample_size(10)
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(5));
    group
}

/// Times `call` as bench `name` of `group` (named `group_name`), then prints the percentiles of
/// every call timed, once criterion has measured (its test mode runs once).
fn timed(
    group: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>,
    group_name: &str,
    name: &str,
    mut call: impl FnMut(),
) {
    // 1. Time each call.
    let mut samples: Vec<Duration> = Vec::new();
    group.bench_function(name, |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let start = Instant::now();
                call();
                let took = start.elapsed();
                total += took;
                samples.push(took);
            }
            total
        });
    });

    // 2. The percentiles.
    if samples.len() < MIN_SAMPLES {
        return;
    }
    samples.sort_unstable();
    let n = samples.len();
    println!(
        "percentiles {group_name}/{name}: n {n} p50 {:.3} ms p90 {:.3} ms p99 {:.3} ms max \
         {:.3} ms",
        millis(at(&samples, 500)),
        millis(at(&samples, 900)),
        millis(at(&samples, 990)),
        millis(samples[n - 1]),
    );
}

criterion_group!(benches, ancestry);
criterion_main!(benches);
