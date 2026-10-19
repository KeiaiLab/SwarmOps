//! M2 Task 3 (issue 11, design-issue-11 §2.1): what first-parent walks and lookups by id cost
//! on long histories, and two costs of the commit path that docs/known-limitations.md lists
//! ("Commit-path costs"). The end-of-milestone measurement runs it (`just perf`), docs/perf.md
//! records it, and the M2 plan's Decision 18 decides from it.
//!
//! For linear chains of 200,000 and 1,000,000 changesets, a store is built outside the timed
//! loops, in a fresh temporary directory under `SWARMOPS_BENCH_DIR` (default
//! `~/.local/state/swarmops-bench`), through a filesystem that skips every sync ([`NoSyncFs`]):
//! real ids (BLAKE3 hashes), each changeset the child of the one before, in write transactions
//! of 50,000 changesets that each move `main` to their last and compact what they added into
//! the sorted views. A handle on the real filesystem then times, the files in the page cache as
//! a hot store's are, each variant cycling through 1,024 targets in a scattered order:
//!
//! - `walk_changeset_100` and `walk_first_parent_100`: 100 first-parent steps on one snapshot,
//!   by [`MetaRead::changeset`] (a whole record per step, as `Repo::log_first_parent` walks) and
//!   by [`MetaRead::first_parent`] (the record's `par.col` entry and 32 bytes of `parents.log`);
//! - `find_hit` and `find_miss`: [`Index::find`] of an id the index holds and of one it does
//!   not, every record in the sorted views;
//! - `bisect_hit` and `bisect_miss`: the same lookups by a copy, in this file, of a plain
//!   bisection that keeps the key at its bound (what the fix wave puts back if the
//!   interpolation goes), on the same [`SortedView`] reads. Decision 18 keeps the interpolation
//!   if `find_hit`'s p50 at 1M is at least 20 % below `bisect_hit`'s;
//! - `insert_tail0` and `insert_tail1024`: [`MetaWrite::insert_changeset`] of a new child of
//!   `main`'s head in an open write transaction, never committed, with no record in the index's
//!   unsorted tail and then with 1,024, the most it holds before a writer compacts: every insert
//!   of a new changeset scans that tail for its id. Only the insert is timed;
//! - `reflog100_tail31`: [`MetaRead::reflog`] of `main`'s last 100 moves on one snapshot that
//!   holds 31 frames past the checkpoint, the most the batched checkpoint leaves, every frame a
//!   move of `main`.
//!
//! Besides criterion's estimate, each variant prints the nearest-rank percentiles of every call
//! it timed, warm-up included (`percentiles history/<size>/<variant>: …`, a line `just perf`
//! shows). `SWARMOPS_BENCH_HISTORY_CHANGESETS=<n>[,<n>…]` measures other chain lengths (each of
//! at least 1,000 changesets), for a quick check of the bench.

use std::hint::black_box;
use std::sync::Arc;
use std::time::{Duration, Instant};

use camino::{Utf8Path, Utf8PathBuf};
use criterion::measurement::WallTime;
use criterion::{BenchmarkGroup, Criterion, criterion_group, criterion_main};
use swarmops_core::{ChangesetId, ObjectKind, hash_object};
use swarmops_store::{
    CasResult, ChangesetMeta, Index, InsertOutcome, Journal, Layout, MetaRead, MetaStore,
    MetaWrite, RealFs, SortedView,
};
use swarmops_testkit::fs::NoSyncFs;
use swarmops_testkit::measure::{base_dir, mix};

/// The chain lengths measured, in changesets, unless [`CHANGESETS`] says otherwise.
const SIZES: [u64; 2] = [200_000, 1_000_000];

/// Set to measure other chain lengths: changeset counts, comma-separated, each at least
/// [`MIN_CHANGESETS`].
const CHANGESETS: &str = "SWARMOPS_BENCH_HISTORY_CHANGESETS";

/// The shortest chain [`CHANGESETS`] may ask for: room for the walks and the targets.
const MIN_CHANGESETS: u64 = 1_000;

/// How many changesets one write transaction of the build inserts (a frame holds at most
/// 65,535 records).
const PER_TRANSACTION: u64 = 50_000;

/// The first-parent steps of a walk.
const STEPS: u64 = 100;

/// How many targets each variant cycles through.
const TARGETS: usize = 1_024;

/// The records the index's unsorted tail holds for `insert_tail1024`: the most it holds before
/// a writer compacts (it compacts once the tail holds more).
const TAIL: u64 = Index::COMPACT_THRESHOLD;

/// The frames past the checkpoint for `reflog100_tail31`: the most the batched checkpoint
/// (every 32 frames) leaves.
const PAST_CHECKPOINT: u64 = 31;

/// The moves `reflog100_tail31` asks for.
const REFLOG: usize = 100;

/// How many changesets `insert_*` queues in one write transaction before it starts another,
/// untimed: a transaction holds at most 65,535 records.
const INSERTS_PER_TRANSACTION: u64 = 8_192;

/// Where the ids of the changesets `insert_*` queues start: far past every chain's.
const NEW_IDS: u64 = 1 << 40;

/// Who moves `main`.
const ACTOR: &str = "bench/history";

/// The id of changeset `n`: a real hash, so the ids are spread as a store's are.
fn id(n: u64) -> ChangesetId {
    ChangesetId(hash_object(ObjectKind::Changeset, &n.to_le_bytes()))
}

/// Changeset `n`, the child of changeset `n - 1` (`parent` for the ones `insert_*` queues).
fn changeset(n: u64, parent: Option<ChangesetId>) -> ChangesetMeta {
    ChangesetMeta {
        id: id(n),
        seq: 0,
        generation: 0,
        delta_hash: hash_object(ObjectKind::Blob, &n.to_le_bytes()).0,
        parents: parent.into_iter().collect(),
    }
}

/// Appends changesets `from..to` to the chain in one write transaction that moves `main` from
/// changeset `from - 1` to changeset `to - 1`.
fn extend(store: &MetaStore, from: u64, to: u64) {
    let mut write = store.write().expect("a write transaction");
    for n in from..to {
        let inserted = write.insert_changeset(changeset(n, n.checked_sub(1).map(id)));
        assert!(matches!(inserted, Ok(InsertOutcome::Inserted { .. })));
    }
    let moved = write.cas_ref("main", from.checked_sub(1).map(id), Some(id(to - 1)), ACTOR);
    assert!(matches!(moved, Ok(CasResult::Applied { .. })), "{moved:?}");
    write.commit().expect("commit");
}

/// The chain lengths to measure: [`CHANGESETS`]'s, or [`SIZES`].
fn sizes() -> Vec<u64> {
    let Ok(list) = std::env::var(CHANGESETS) else {
        return SIZES.to_vec();
    };
    list.split(',')
        .map(|n| {
            let changesets: u64 = n
                .trim()
                .replace('_', "")
                .parse()
                .unwrap_or_else(|_| panic!("{CHANGESETS}: {n} is not a number of changesets"));
            assert!(
                changesets >= MIN_CHANGESETS,
                "{CHANGESETS}: a chain of {changesets} is shorter than {MIN_CHANGESETS}"
            );
            changesets
        })
        .collect()
}

/// `changesets` as the bench names a size: `200k`, `1M`.
fn size_name(changesets: u64) -> String {
    if changesets.is_multiple_of(1_000_000) {
        format!("{}M", changesets / 1_000_000)
    } else if changesets.is_multiple_of(1_000) {
        format!("{}k", changesets / 1_000)
    } else {
        changesets.to_string()
    }
}

/// [`TARGETS`] changesets of `from..to` in a scattered order, the same every run.
fn scattered(from: u64, to: u64) -> Vec<u64> {
    (0..TARGETS)
        .map(|i| from + mix(u64::try_from(i).expect("fits")) % (to - from))
        .collect()
}

/// Where `steps` first-parent steps from `from` end, each step a [`MetaRead::changeset`].
fn walk_changeset(read: &MetaRead<'_>, from: ChangesetId, steps: u64) -> ChangesetId {
    let mut at = from;
    for _ in 0..steps {
        let meta = read.changeset(at).expect("a lookup").expect("in the chain");
        at = meta.parents[0];
    }
    at
}

/// Where `steps` first-parent steps from `from` end, each step a [`MetaRead::first_parent`].
fn walk_first_parent(read: &MetaRead<'_>, from: ChangesetId, steps: u64) -> ChangesetId {
    let mut at = from;
    for _ in 0..steps {
        let parent = read
            .first_parent(at)
            .expect("a lookup")
            .expect("in the chain");
        at = parent.expect("not the root");
    }
    at
}

/// The index of the first key of `view` not less than `key`, and the key there if the search
/// read it: a plain bisection, one [`SortedView::key_at`] per probe, which keeps the key it read
/// when it last moved its upper bound. The copy Decision 18 compares the interpolation with.
fn bisect(view: &SortedView<32>, key: &[u8; 32]) -> (u64, Option<[u8; 32]>) {
    let (mut low, mut high, mut at_high) = (0, view.len(), None);
    while low < high {
        let mid = low + (high - low) / 2;
        let probe = view.key_at(mid).expect("a key");
        if probe < *key {
            low = mid + 1;
        } else {
            (high, at_high) = (mid, Some(probe));
        }
    }
    (low, at_high)
}

/// [`Index::find`] by [`bisect`]: the search, then the rec of a key that is there. Every record
/// is in the sorted views here, so there is no tail to scan.
fn bisect_find(view: &SortedView<32>, id: ChangesetId) -> Option<u64> {
    match bisect(view, &id.0.0) {
        (at, Some(found)) if found == id.0.0 => Some(view.rec_at(at).expect("a rec")),
        _ => None,
    }
}

/// How long `work` takes, with what it returns.
fn clock<T>(work: impl FnOnce() -> T) -> (T, Duration) {
    let started = Instant::now();
    let out = work();
    (out, started.elapsed())
}

/// Benches `name` in `group`: each iteration calls `op` with the next target, and `op` times
/// its own work and returns how long it took, so that what it sets up first goes untimed. Then
/// prints the percentiles of every call it timed.
fn timed(
    group: &mut BenchmarkGroup<'_, WallTime>,
    name: &str,
    mut op: impl FnMut(usize) -> Duration,
) {
    let mut samples: Vec<Duration> = Vec::new();
    let mut next = 0;
    group.bench_function(name, |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let took = op(next);
                total += took;
                samples.push(took);
                next = (next + 1) % TARGETS;
            }
            total
        });
    });
    // Criterion's test mode runs each variant once: no percentiles to speak of.
    if samples.len() < 1_000 {
        return;
    }
    samples.sort_unstable();
    let at = |per_mille: usize| {
        let rank = (samples.len() * per_mille).div_ceil(1000).max(1);
        samples[rank - 1]
    };
    println!(
        "percentiles history/{name}: n {} p50 {:.2} us p90 {:.2} us p99 {:.2} us max {:.2} us",
        samples.len(),
        micros(at(500)),
        micros(at(900)),
        micros(at(990)),
        micros(samples[samples.len() - 1])
    );
}

fn micros(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1e6
}

/// One chain's store under test: built on a filesystem without syncs, timed through a handle
/// on the real one.
struct Chain<'a> {
    /// The size's name, the first part of each variant's.
    name: String,
    root: &'a Utf8Path,
    /// Builds the store, without a sync.
    builder: MetaStore,
    /// Times it, on the real filesystem.
    store: MetaStore,
    /// How many changesets the chain holds.
    len: u64,
}

impl Chain<'_> {
    /// The walks, from scattered changesets at least [`STEPS`] from the root.
    fn bench_walks(&self, group: &mut BenchmarkGroup<'_, WallTime>) {
        let read = self.store.read().expect("a snapshot");
        let starts = scattered(STEPS, self.len);
        for &n in &starts {
            assert_eq!(walk_changeset(&read, id(n), STEPS), id(n - STEPS));
            assert_eq!(walk_first_parent(&read, id(n), STEPS), id(n - STEPS));
        }
        timed(group, &format!("{}/walk_changeset_100", self.name), |i| {
            clock(|| black_box(walk_changeset(&read, id(starts[i]), STEPS))).1
        });
        timed(
            group,
            &format!("{}/walk_first_parent_100", self.name),
            |i| clock(|| black_box(walk_first_parent(&read, id(starts[i]), STEPS))).1,
        );
    }

    /// The lookups by id, by the index's search and by the bisection copy.
    fn bench_finds(&self, group: &mut BenchmarkGroup<'_, WallTime>) {
        let index = Index::open(&Layout::new(self.root), Arc::new(RealFs)).expect("the index");
        assert_eq!(index.tail_len(), 0, "every record in the sorted views");
        let view = index.ids_view();
        let hits: Vec<ChangesetId> = scattered(0, self.len).into_iter().map(id).collect();
        let misses: Vec<ChangesetId> = (0..TARGETS)
            .map(|i| id(NEW_IDS - 1 - u64::try_from(i).expect("fits")))
            .collect();
        for (&hit, &miss) in hits.iter().zip(&misses) {
            let rec = index.find(hit).expect("a lookup");
            assert!(rec.is_some());
            assert_eq!(bisect_find(view, hit), rec);
            assert_eq!(index.find(miss).expect("a lookup"), None);
            assert_eq!(bisect_find(view, miss), None);
        }
        for (variant, ids) in [("hit", &hits), ("miss", &misses)] {
            timed(group, &format!("{}/find_{variant}", self.name), |i| {
                clock(|| black_box(index.find(ids[i]).expect("a lookup"))).1
            });
            timed(group, &format!("{}/bisect_{variant}", self.name), |i| {
                clock(|| black_box(bisect_find(view, ids[i]))).1
            });
        }
    }

    /// An insert of a new child of `main`'s head, the index's tail holding `tail` records.
    fn bench_insert(&self, group: &mut BenchmarkGroup<'_, WallTime>, tail: u64) {
        let head = id(self.len - 1);
        let mut write: Option<MetaWrite<'_>> = None;
        let mut next = NEW_IDS;
        timed(group, &format!("{}/insert_tail{tail}", self.name), |_| {
            if (next - NEW_IDS).is_multiple_of(INSERTS_PER_TRANSACTION) {
                // Untimed: the transaction before, dropped, unlocks; a new one locks.
                write = None;
                write = Some(self.store.write().expect("a write transaction"));
            }
            let open = write.as_mut().expect("a transaction");
            let meta = changeset(next, Some(head));
            next += 1;
            let (inserted, took) = clock(|| open.insert_changeset(meta));
            assert!(matches!(inserted, Ok(InsertOutcome::Inserted { .. })));
            took
        });
    }

    /// `main`'s last [`REFLOG`] moves, on a snapshot with [`PAST_CHECKPOINT`] frames past the
    /// checkpoint.
    fn bench_reflog(&self, group: &mut BenchmarkGroup<'_, WallTime>) {
        let read = self.store.read().expect("a snapshot");
        assert_eq!(
            read.reflog("main", REFLOG).expect("the reflog").len(),
            REFLOG
        );
        timed(group, &format!("{}/reflog100_tail31", self.name), |_| {
            clock(|| black_box(read.reflog("main", REFLOG).expect("the reflog"))).1
        });
    }

    /// Appends changesets until the chain holds `to`, one transaction each.
    fn grow_one_by_one(&mut self, to: u64) {
        for n in self.len..to {
            extend(&self.builder, n, n + 1);
        }
        self.len = to;
    }

    /// How many frames lie past the checkpoint.
    fn past_checkpoint(&self) -> u64 {
        let journal = Journal::open(&Layout::new(self.root), Arc::new(RealFs)).expect("a journal");
        let ckpt = journal.ckpt().expect("journal.ckpt");
        let frames = journal.tail_frames_from(ckpt).expect("the frames past it");
        u64::try_from(frames.len()).expect("fits")
    }
}

fn history(c: &mut Criterion) {
    let base = base_dir();
    let mut group = c.benchmark_group("history");
    group
        .sample_size(100)
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(3));
    for changesets in sizes() {
        let dir = tempfile::tempdir_in(&base).expect("a store dir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("a UTF-8 dir");
        let builder = MetaStore::create_with_fs(&root, Arc::new(NoSyncFs)).expect("create");
        for from in (0..changesets).step_by(usize::try_from(PER_TRANSACTION).expect("fits")) {
            extend(&builder, from, changesets.min(from + PER_TRANSACTION));
        }
        let mut chain = Chain {
            name: size_name(changesets),
            root: &root,
            builder,
            store: MetaStore::open(&root).expect("open the store"),
            len: changesets,
        };
        chain.bench_walks(&mut group);
        chain.bench_finds(&mut group);
        chain.bench_insert(&mut group, 0);

        // One transaction of TAIL changesets: no compaction, the tail holds them all.
        extend(&chain.builder, changesets, changesets + TAIL);
        chain.len += TAIL;
        chain.bench_insert(&mut group, TAIL);

        // Moves of main one transaction at a time, the first of them past the threshold (it
        // compacts and checkpoints), then a flush, then PAST_CHECKPOINT more.
        chain.grow_one_by_one(chain.len + u64::try_from(REFLOG).expect("fits"));
        chain.builder.flush().expect("flush");
        chain.grow_one_by_one(chain.len + PAST_CHECKPOINT);
        assert_eq!(chain.past_checkpoint(), PAST_CHECKPOINT);
        chain.bench_reflog(&mut group);
    }
    group.finish();
}

criterion_group!(benches, history);
criterion_main!(benches);
