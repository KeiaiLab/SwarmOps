//! M1 read path after the journal hardening: what one lock-free snapshot costs now that a
//! reader measures `journal.off` before `journal.log` and finds the checkpoint's frame by a
//! binary search of `journal.off` (`Journal::tail_frames_from`, twice per
//! `MetaStore::read`). docs/perf.md "M1 (CLI level)" records the numbers.
//!
//! For 10,000 and 100,000 changesets, one per write transaction (a frame each, as commits make
//! them), a store is built outside the timed loops through a filesystem that skips fsyncs (a
//! read does not depend on them), in a fresh temporary directory under `SWARMOPS_BENCH_DIR`
//! (default `~/.local/state/swarmops-bench`). It is then timed through a handle on the real
//! filesystem, its files in the page cache as a hot store's are, twice:
//!
//! - `ckpt_end`: after `MetaStore::flush`, the checkpoint at the end of the journal;
//! - `tail31`: after 31 more transactions, 31 frames past the checkpoint, the most that the
//!   batched checkpoint (every 32 frames) leaves.
//!
//! The variants, each cycling through 1,024 changesets in a scattered order:
//!
//! - `read_changeset`: `MetaStore::read()`, then `changeset(id)` on the snapshot: what a CLI
//!   read takes before it reads an object, and a commit twice;
//! - `read`: `MetaStore::read()` alone;
//! - `tail_frames`: `Journal::tail_frames_from(journal.ckpt)` alone, which `read` calls twice:
//!   the binary search of `journal.off`, then the frames past the checkpoint;
//! - `off_search`: that binary search alone, one 8-byte `read_at` of `journal.off` per probe
//!   as `Journal` does it, copied here since `Journal`'s is private: what caching the
//!   checkpoint's frame would save.
//!
//! A third store, of 10,000 changesets with `main` naming the newest, holds 0, then 16, then
//! 256 `release/*` refs besides `main`, every ref move checkpointed (`10k_refs0`, `10k_refs16`,
//! `10k_refs256`). A snapshot reads every ref file (storage spec §3), so these give what each
//! ref adds to a read: the number that known-limitations' "A snapshot reads every ref file"
//! weighs once refs grow past dozens. The store's handle is opened once and kept, as a
//! long-lived server keeps its own (a warm server), and its variants are:
//!
//! - `read_changeset` and `read`, as above;
//! - `show_two_snapshots`: what `show main` read of the metadata before each request read in
//!   one snapshot (storage spec §8): `main` resolved in one snapshot (its ref, and its changeset
//!   checked to be there), then its changeset read in a second;
//! - `show_one_snapshot`: the same reads in one snapshot, as `show` makes them now.
//!
//! Besides criterion's estimate, each variant prints the nearest-rank percentiles of every
//! operation it timed, warm-up included (`percentiles read_snapshot/<store>/<variant>: …`),
//! for comparison with Task 2b's per-lookup `find` percentiles (`tests/index.rs`,
//! `index_on_wide_history_is_fast`).

use std::fs::{self, File};
use std::hint::black_box;
use std::os::unix::fs::FileExt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use camino::{Utf8Path, Utf8PathBuf};
use criterion::measurement::WallTime;
use criterion::{BenchmarkGroup, Criterion, criterion_group, criterion_main};
use swarmops_core::{ChangesetId, ObjectKind, hash_object};
use swarmops_store::{
    CasResult, ChangesetMeta, Journal, Layout, MetaRead, MetaStore, RealFs, StoreError,
};
use swarmops_testkit::fs::NoSyncFs;
use swarmops_testkit::measure::{base_dir, mix};

/// How many changesets the stores hold before the 31 more of `tail31`.
const SIZES: [u64; 2] = [10_000, 100_000];

/// How many frames `tail31` leaves past the checkpoint.
const TAIL: u64 = 31;

/// How many changesets each variant cycles through.
const TARGETS: usize = 1_024;

/// The size of one `journal.off` entry.
const ENTRY_LEN: u64 = 8;

/// How many changesets the store of the ref variants holds.
const REF_STORE: u64 = 10_000;

/// How many `release/*` refs the ref variants' store holds besides `main`, in turn.
const REF_COUNTS: [u64; 3] = [0, 16, 256];

/// The id of changeset `n`: a real hash, so the ids are spread as a store's are.
fn id(n: u64) -> ChangesetId {
    ChangesetId(hash_object(ObjectKind::Changeset, &n.to_le_bytes()))
}

/// Changeset `n`, the child of changeset `n - 1`.
fn changeset(n: u64) -> ChangesetMeta {
    ChangesetMeta {
        id: id(n),
        seq: 0,
        generation: 0,
        delta_hash: hash_object(ObjectKind::Blob, &n.to_le_bytes()).0,
        parents: n.checked_sub(1).map(id).into_iter().collect(),
    }
}

/// Inserts changesets `from..to`, one write transaction each.
fn insert(store: &MetaStore, from: u64, to: u64) {
    for n in from..to {
        let mut write = store.write().expect("a write transaction");
        write.insert_changeset(changeset(n)).expect("insert");
        write.commit().expect("commit");
    }
}

/// Creates each ref of `refs`, naming the changeset given, in one write transaction, then
/// checkpoints it: a snapshot reads every ref from its file then, and no frame past the
/// checkpoint.
fn create_refs(store: &MetaStore, refs: &[(String, u64)]) {
    let mut write = store.write().expect("a write transaction");
    for (name, n) in refs {
        let created = write
            .cas_ref(name, None, Some(id(*n)), "bench/refs")
            .expect("a ref move");
        assert_eq!(created, CasResult::Applied { old: None }, "{name}");
    }
    write.commit().expect("commit");
    store.flush().expect("flush");
}

/// What `main` names in snapshot `read`, checked to be a changeset the snapshot holds, as
/// `swarmops_repo::RepoRead::resolve` resolves a ref.
fn resolve_main(read: &MetaRead<'_>) -> Result<ChangesetId, StoreError> {
    let head = read.get_ref("main")?.expect("main exists").target;
    let found = read.changeset(head)?;
    black_box(found.expect("main names a changeset of the store"));
    Ok(head)
}

/// [`TARGETS`] changesets of `0..count` in a scattered order, the same every run.
fn scattered(count: u64) -> Vec<u64> {
    (0..TARGETS)
        .map(|i| mix(u64::try_from(i).expect("fits")) % count)
        .collect()
}

/// The index of the first `journal.off` entry at `offset` or past it, by the binary search
/// `Journal` makes (one 8-byte `read_at` per probe).
fn off_search(off: &File, offset: u64, listed: u64) -> u64 {
    let (mut low, mut high) = (0, listed);
    while low < high {
        let mid = low + (high - low) / 2;
        let mut entry = [0; 8];
        off.read_exact_at(&mut entry, mid * ENTRY_LEN)
            .expect("read a journal.off entry");
        if u64::from_le_bytes(entry) < offset {
            low = mid + 1;
        } else {
            high = mid;
        }
    }
    low
}

/// A store under test, opened for reading on the real filesystem.
struct Snapshot<'a> {
    name: String,
    root: &'a Utf8Path,
    store: MetaStore,
    journal: Journal,
    off: File,
    /// The changesets the variants cycle through: their numbers.
    targets: Vec<u64>,
    /// The frames past the checkpoint.
    tail: u64,
}

impl<'a> Snapshot<'a> {
    /// Opens the store at `root`, which holds `count` changesets, `tail` frames of them past
    /// the checkpoint, and checks that it does.
    fn open(name: String, root: &'a Utf8Path, count: u64, tail: u64) -> Snapshot<'a> {
        let layout = Layout::new(root);
        let store = MetaStore::open(root).expect("open the store");
        let journal = Journal::open(&layout, Arc::new(RealFs)).expect("open the journal");
        let ckpt = journal.ckpt().expect("journal.ckpt");
        let frames = journal.tail_frames_from(ckpt).expect("the frames past it");
        assert_eq!(frames.len(), usize::try_from(tail).expect("fits"), "{name}");
        let read = store.read().expect("a snapshot");
        assert_eq!(read.count_changesets().expect("count"), count, "{name}");
        let targets = scattered(count);
        for &n in &targets {
            let found = read.changeset(id(n)).expect("a lookup").expect("there");
            assert_eq!(found.seq, n, "{name}");
        }
        drop(read);
        let off = File::open(layout.journal_off()).expect("open journal.off");
        let snapshot = Snapshot {
            name,
            root,
            store,
            journal,
            off,
            targets,
            tail,
        };
        let listed = snapshot.listed();
        assert_eq!(
            off_search(&snapshot.off, ckpt, listed),
            listed - tail,
            "the search finds the first frame past the checkpoint"
        );
        snapshot
    }

    /// How many frames `journal.off` lists.
    fn listed(&self) -> u64 {
        let layout = Layout::new(self.root);
        fs::metadata(layout.journal_off())
            .expect("journal.off")
            .len()
            / ENTRY_LEN
    }

    fn bench(&self, group: &mut BenchmarkGroup<'_, WallTime>) {
        let ckpt = self.journal.ckpt().expect("journal.ckpt");
        let listed = self.listed();
        self.bench_reads(group);
        timed(group, &format!("{}/tail_frames", self.name), |_| {
            let frames = self.journal.tail_frames_from(ckpt)?;
            assert_eq!(frames.len(), usize::try_from(self.tail).expect("fits"));
            Ok(())
        });
        timed(group, &format!("{}/off_search", self.name), |_| {
            black_box(off_search(&self.off, ckpt, listed));
            Ok(())
        });
    }

    /// The ref variants, on a store that holds `refs` refs besides `main`.
    fn bench_refs(&self, group: &mut BenchmarkGroup<'_, WallTime>, refs: u64) {
        let read = self.store.read().expect("a snapshot");
        let shown = u64::try_from(read.list_refs().expect("the refs").len()).expect("fits");
        assert_eq!(shown, refs + 1, "{}: main and the release refs", self.name);
        drop(read);
        self.bench_reads(group);
        timed(group, &format!("{}/show_two_snapshots", self.name), |_| {
            let head = resolve_main(&self.store.read()?)?;
            black_box(self.store.read()?.changeset(head)?);
            Ok(())
        });
        timed(group, &format!("{}/show_one_snapshot", self.name), |_| {
            let read = self.store.read()?;
            let head = resolve_main(&read)?;
            black_box(read.changeset(head)?);
            Ok(())
        });
    }

    /// `read_changeset` and `read`, cycling through the store's targets.
    fn bench_reads(&self, group: &mut BenchmarkGroup<'_, WallTime>) {
        let ids: Vec<ChangesetId> = self.targets.iter().map(|&n| id(n)).collect();
        timed(group, &format!("{}/read_changeset", self.name), |i| {
            let read = self.store.read()?;
            black_box(read.changeset(ids[i])?);
            Ok(())
        });
        timed(group, &format!("{}/read", self.name), |_| {
            black_box(self.store.read()?);
            Ok(())
        });
    }
}

/// Benches `op` as `name` in `group`, and prints the percentiles of each call it timed.
fn timed(
    group: &mut BenchmarkGroup<'_, WallTime>,
    name: &str,
    mut op: impl FnMut(usize) -> Result<(), StoreError>,
) {
    let mut samples: Vec<Duration> = Vec::new();
    let mut next = 0;
    group.bench_function(name, |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let started = Instant::now();
                op(next).expect("the operation");
                let took = started.elapsed();
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
        "percentiles read_snapshot/{name}: n {} p50 {:.2} us p90 {:.2} us p99 {:.2} us max {:.2} us",
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

fn read_snapshot(c: &mut Criterion) {
    let base = base_dir();
    let mut group = c.benchmark_group("read_snapshot");
    group
        .sample_size(100)
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(3));
    for count in SIZES {
        let dir = tempfile::tempdir_in(&base).expect("a store dir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("UTF-8");
        let builder = MetaStore::create_with_fs(&root, Arc::new(NoSyncFs)).expect("create");
        insert(&builder, 0, count);
        builder.flush().expect("flush");
        let size = format!("{}k", count / 1_000);
        Snapshot::open(format!("{size}_ckpt_end"), &root, count, 0).bench(&mut group);
        insert(&builder, count, count + TAIL);
        Snapshot::open(format!("{size}_tail31"), &root, count + TAIL, TAIL).bench(&mut group);
    }
    let dir = tempfile::tempdir_in(&base).expect("a store dir");
    let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("UTF-8");
    let builder = MetaStore::create_with_fs(&root, Arc::new(NoSyncFs)).expect("create");
    insert(&builder, 0, REF_STORE);
    create_refs(&builder, &[("main".to_owned(), REF_STORE - 1)]);
    let size = format!("{}k", REF_STORE / 1_000);
    let mut made = 0;
    for count in REF_COUNTS {
        if count > made {
            let refs: Vec<(String, u64)> = (made..count)
                .map(|n| (format!("release/r{n:03}"), n))
                .collect();
            create_refs(&builder, &refs);
            made = count;
        }
        Snapshot::open(format!("{size}_refs{count}"), &root, REF_STORE, 0)
            .bench_refs(&mut group, count);
    }
    group.finish();
}

criterion_group!(benches, read_snapshot);
criterion_main!(benches);
