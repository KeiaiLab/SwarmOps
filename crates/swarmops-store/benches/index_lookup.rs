//! M1 experiment: changeset index lookups on a wide history (storage spec §3, §3.1).
//!
//! For 10,000, 100,000 and 200,000 changesets, one index is built, outside every timed loop,
//! in a fresh temporary directory under `SWARMOPS_BENCH_DIR` (default
//! `~/.local/state/swarmops-bench`, on the native filesystem): a chain of real ids (BLAKE3
//! hashes), each changeset with its own delta hash. Its first records are compacted and its
//! last 1,000 left in the unsorted tail (`*_tail1000`); after those lookups it is compacted
//! whole (`*_compacted`) and they run again. The files stay in the page cache, as a hot
//! store's do.
//!
//! - `find_*`: [`Index::find`] of an id the index holds: any of its ids when compacted, one of
//!   the tail's with the tail (a binary search that misses, then the tail scan).
//! - `find_miss_*`: [`Index::find`] of an id it does not hold, as every insert of a new
//!   changeset asks.
//! - `find_prefix_*`: [`Index::find_prefix`] of the 12-hex prefix (the shortest a rev may
//!   give) of an id it holds, picked as for `find`.
//! - `find_delta_hash_*`: [`Index::find_delta_hash`] of a delta hash it holds, picked as for
//!   `find`.
//!
//! Each lookup cycles through 1,024 targets in a scattered order. The parameter is the
//! index's size; docs/perf.md cites criterion's median as the p50 per lookup.

use std::hint::black_box;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use camino::Utf8PathBuf;
use criterion::measurement::WallTime;
use criterion::{BenchmarkGroup, BenchmarkId, Criterion, criterion_group, criterion_main};
use swarmops_core::{ChangesetId, ObjectKind, hash_object};
use swarmops_store::{ChangesetMeta, Index, Layout, RealFs};
use swarmops_testkit::measure::{base_dir, mix};
use tempfile::TempDir;

/// The index sizes measured, in changesets.
const SIZES: [u64; 3] = [10_000, 100_000, 200_000];

/// The records left in the unsorted tail for the `*_tail1000` lookups.
const TAIL: u64 = 1_000;

/// How many targets each lookup cycles through.
const TARGETS: u64 = 1_024;

/// The id of changeset `n`.
fn id(n: u64) -> ChangesetId {
    ChangesetId::new(hash_object(ObjectKind::Changeset, &n.to_le_bytes()))
}

/// The delta hash of changeset `n`.
fn delta(n: u64) -> [u8; 32] {
    hash_object(ObjectKind::Blob, &n.to_le_bytes()).0
}

/// Changeset `n` of the chain: the child of changeset `n - 1`.
fn changeset(n: u64) -> ChangesetMeta {
    ChangesetMeta::new(
        id(n),
        0,
        n + 1,
        delta(n),
        n.checked_sub(1).map(id).into_iter().collect(),
    )
}

/// An index of changesets `0..size` in a fresh directory under `base`: the first
/// `size - TAIL` compacted into the sorted views, the last [`TAIL`] in the tail, all synced.
/// [`Index::create`] lays its files out, as a store's create does ([`Index::open`] creates
/// nothing).
fn build(base: &Path, size: u64) -> (TempDir, Index) {
    let dir = tempfile::tempdir_in(base).expect("create store dir");
    let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("UTF-8 store dir");
    let mut index =
        Index::create(&Layout::new(root), Arc::new(RealFs::new())).expect("create index");
    for n in 0..size - TAIL {
        index.append(&changeset(n)).expect("append");
    }
    assert_eq!(index.compact().expect("compact"), size - TAIL);
    for n in size - TAIL..size {
        index.append(&changeset(n)).expect("append");
    }
    index.sync().expect("sync");
    assert_eq!(index.tail_len(), TAIL);
    (dir, index)
}

/// [`TARGETS`] changesets of `from..to` in a scattered order (with repeats).
fn targets(from: u64, to: u64) -> Vec<u64> {
    (0..TARGETS).map(|i| from + mix(i) % (to - from)).collect()
}

/// Times the four lookups on `index`, which holds changesets `0..size`; `hits` are the
/// changesets the hit lookups ask for, and `layout` names the index's shape.
fn lookups(
    group: &mut BenchmarkGroup<'_, WallTime>,
    index: &Index,
    size: u64,
    layout: &str,
    hits: &[u64],
) {
    let ids: Vec<ChangesetId> = hits.iter().map(|&n| id(n)).collect();
    let misses: Vec<ChangesetId> = (size..size + TARGETS).map(id).collect();
    let prefixes: Vec<String> = ids
        .iter()
        .map(|id| id.0.to_hex()[..12].to_owned())
        .collect();
    let deltas: Vec<[u8; 32]> = hits.iter().map(|&n| delta(n)).collect();
    // Every target answers as it should before any of them is timed.
    for (&n, (id, (prefix, dh))) in hits
        .iter()
        .zip(ids.iter().zip(prefixes.iter().zip(&deltas)))
    {
        assert_eq!(index.find(*id).expect("find"), Some(n));
        assert!(index.find_prefix(prefix).expect("find_prefix").contains(&n));
        assert_eq!(index.find_delta_hash(dh).expect("find_delta_hash"), [n]);
    }
    for miss in &misses {
        assert_eq!(index.find(*miss).expect("find"), None);
    }

    let mut at = 0;
    let mut next = move || {
        at = (at + 1) % hits.len();
        at
    };
    group.bench_function(BenchmarkId::new(format!("find_{layout}"), size), |b| {
        b.iter(|| index.find(black_box(ids[next()])).expect("find"));
    });
    group.bench_function(BenchmarkId::new(format!("find_miss_{layout}"), size), |b| {
        b.iter(|| index.find(black_box(misses[next()])).expect("find"));
    });
    group.bench_function(
        BenchmarkId::new(format!("find_prefix_{layout}"), size),
        |b| {
            b.iter(|| {
                index
                    .find_prefix(black_box(&prefixes[next()]))
                    .expect("find_prefix")
            });
        },
    );
    group.bench_function(
        BenchmarkId::new(format!("find_delta_hash_{layout}"), size),
        |b| {
            b.iter(|| {
                index
                    .find_delta_hash(black_box(&deltas[next()]))
                    .expect("find_delta_hash")
            });
        },
    );
}

fn index_lookup(c: &mut Criterion) {
    let base = base_dir();
    let mut group = c.benchmark_group("index_lookup");
    group
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(3));
    for size in SIZES {
        let (dir, mut index) = build(&base, size);
        lookups(
            &mut group,
            &index,
            size,
            "tail1000",
            &targets(size - TAIL, size),
        );
        assert_eq!(index.compact().expect("compact"), size);
        assert_eq!(index.tail_len(), 0);
        lookups(&mut group, &index, size, "compacted", &targets(0, size));
        drop(index);
        drop(dir);
    }
    group.finish();
}

criterion_group!(benches, index_lookup);
criterion_main!(benches);
