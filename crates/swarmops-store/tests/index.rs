//! Columnar changeset index: the column files and their byte layout, the sorted key views,
//! lookups that combine a view with the unsorted tail, compaction, and recovery from a crash
//! in the middle of an append or of a compaction.
//!
//! Every test owns a fresh store root (`tempfile::tempdir()`), so tests are independent and
//! may run in any order or in parallel.

#[cfg(feature = "test-hooks")]
use std::ffi::OsString;
use std::fmt::Debug;
use std::fs;
use std::io;
use std::num::NonZero;
#[cfg(feature = "test-hooks")]
use std::os::unix::process::ExitStatusExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(feature = "test-hooks")]
use camino::Utf8PathBuf;
use proptest::prelude::*;
use swarmops_core::bytes::to_u64;
use swarmops_core::{ChangesetId, ObjectId};
#[cfg(feature = "test-hooks")]
use swarmops_store::{CRASH_AT_ENV, CrashAtFs};
use swarmops_store::{
    ChangesetMeta, FsOps, Index, Layout, LimitKind, MetaStore, RealFs, StoreError,
};
#[cfg(feature = "test-hooks")]
use swarmops_testkit::child::{SIGABRT, run, this_test};
#[cfg(feature = "test-hooks")]
use swarmops_testkit::faults::checked;
use swarmops_testkit::fs::{NoSyncFs, RecordingFs};
use swarmops_testkit::measure::{mix, percentile};
use swarmops_testkit::store::new_root as bare_root;
use swarmops_testkit::store::{append_bytes, file_len, real_fs, set_len};
#[cfg(feature = "test-hooks")]
use swarmops_testkit::test_name;
use tempfile::TempDir;

/// Set only in the children that `crash_at_any_index_write_recovers_to_whole_records`
/// spawns: the store root.
#[cfg(feature = "test-hooks")]
const CRASH_CHILD_ROOT: &str = "SWARMOPS_TEST_INDEX_CRASH_ROOT";

/// The FsOps calls of [`crash_child`]: none for its `repair_tail` (the files are clean),
/// 3 appends of 5 calls each (1-15), the sync (16-20), then the compaction: 5 sync_data of
/// the columns (21-25), per view 2 temp removes and 2 temp appends (26-33), 4 temp syncs
/// (34-37) and 4 renames (38-41).
#[cfg(feature = "test-hooks")]
const CRASH_CHILD_CALLS: u64 = 41;

/// The files `append` writes, in the order it writes them (`ids.col` last), with the size of
/// one entry; `parents.log` holds 32 bytes per parent, not per record.
const COLUMNS: [(&str, u64); 5] = [
    ("parents.log", 32),
    ("idx/par.col", 8),
    ("idx/gen.col", 8),
    ("idx/dh.col", 32),
    ("idx/ids.col", 32),
];

/// The sorted view files, in the order `compact` renames them into place.
const VIEWS: [(&str, u64); 4] = [
    ("sorted/ids.key", 32),
    ("sorted/ids.rec", 8),
    ("sorted/dh.key", 32),
    ("sorted/dh.rec", 8),
];

/// A fresh store root and its layout, with an empty index laid out as `MetaStore::create`
/// lays it out.
fn new_root() -> (TempDir, Layout) {
    let (dir, layout) = bare_root();
    Index::create(&layout, real_fs()).expect("create the index files");
    (dir, layout)
}

fn open(layout: &Layout) -> Index {
    Index::open(layout, real_fs()).unwrap()
}

/// What opening `layout`'s index gives, the handle shown by its length.
fn open_len(layout: &Layout) -> Result<u64, StoreError> {
    Index::open(layout, real_fs()).map(|index| index.len())
}

fn cs(bytes: [u8; 32]) -> ChangesetId {
    ChangesetId::new(ObjectId::new(bytes))
}

fn id_of(byte: u8) -> ChangesetId {
    cs([byte; 32])
}

/// A changeset to append. `seq` is left 0: the index numbers its records itself.
fn meta(
    id: ChangesetId,
    generation: u64,
    delta_hash: [u8; 32],
    parents: Vec<ChangesetId>,
) -> ChangesetMeta {
    ChangesetMeta::new(id, 0, generation, delta_hash, parents)
}

/// `meta` as the index returns it for record `seq`.
fn at_seq(meta: &ChangesetMeta, seq: u64) -> ChangesetMeta {
    let mut stored = meta.clone();
    stored.seq = seq;
    stored
}

/// An id that looks like a hash, a different one for each `n` (its first 8 bytes are
/// `mix(4n)`, and `mix` is a bijection).
fn spread_id(n: u64) -> ChangesetId {
    let mut bytes = [0; 32];
    for (word, chunk) in (n.wrapping_mul(4)..).zip(bytes.as_chunks_mut::<8>().0) {
        *chunk = mix(word).to_le_bytes();
    }
    cs(bytes)
}

/// A root changeset with id `spread_id(n)`, its delta hash one of three.
fn spread_meta(n: u64) -> ChangesetMeta {
    let group = u8::try_from(n % 3).unwrap();
    meta(spread_id(n), 1, [group; 32], vec![])
}

fn hex(bytes: &[u8; 32]) -> String {
    ObjectId::new(*bytes).to_hex()
}

/// The lengths of the [`COLUMNS`] files, in that order.
fn column_lens(layout: &Layout) -> [u64; 5] {
    COLUMNS.map(|(rel, _)| file_len(layout.meta_path(rel)))
}

/// The 32-byte entries of `meta/<rel>`.
fn keys_on_disk(layout: &Layout, rel: &str) -> Vec<[u8; 32]> {
    let bytes = fs::read(layout.meta_path(rel)).unwrap();
    let (keys, partial) = bytes.as_chunks::<32>();
    assert!(partial.is_empty(), "{rel} ends in a partial entry");
    keys.to_vec()
}

/// The `u64le` entries of `meta/<rel>`.
fn recs_on_disk(layout: &Layout, rel: &str) -> Vec<u64> {
    let bytes = fs::read(layout.meta_path(rel)).unwrap();
    let (recs, partial) = bytes.as_chunks::<8>();
    assert!(partial.is_empty(), "{rel} ends in a partial entry");
    recs.iter().map(|rec| u64::from_le_bytes(*rec)).collect()
}

/// Appends `metas` as the next records, checking the recs they get, then syncs.
fn append_all(index: &mut Index, metas: &[ChangesetMeta]) {
    let first = index.len();
    for (rec, meta) in (first..).zip(metas) {
        assert_eq!(index.append(meta).unwrap(), rec, "{meta:?}");
    }
    index.sync().unwrap();
}

#[track_caller]
fn assert_index_corrupt<T: Debug>(got: Result<T, StoreError>, what: &str) {
    match got {
        Err(StoreError::IndexCorrupt { .. }) => {}
        other => panic!("{what}: expected IndexCorrupt, got {other:?}"),
    }
}

/// Asserts that `got` is the refusal of a call the store does not take.
#[track_caller]
fn assert_bad_call<T: Debug>(got: Result<T, StoreError>, what: &str) {
    match got {
        Err(StoreError::BadCall(_)) => {}
        other => panic!("{what}: expected BadCall, got {other:?}"),
    }
}

/// Real files, every FsOps call recorded as `"<method> <file name of each path>"`, and a
/// function that returns the calls made since it last ran.
fn recording_fs() -> (Arc<dyn FsOps>, impl Fn() -> Vec<String>) {
    let (fs, calls) = RecordingFs::new();
    (Arc::new(fs.naming_all_paths()), calls)
}

#[test]
fn append_get_roundtrip_with_parents() {
    let (_root, layout) = new_root();
    let mut index = open(&layout);
    assert!(index.is_empty());
    let metas = [
        meta(id_of(1), 1, [0xd1; 32], vec![]),
        meta(id_of(2), 2, [0xd2; 32], vec![id_of(1)]),
        // A merge: parents.log keeps the order, mainline parent first.
        meta(id_of(3), 3, [0xd3; 32], vec![id_of(2), id_of(1)]),
        // As many parents as par.col's count byte holds.
        meta(
            id_of(4),
            u64::MAX,
            [0xd4; 32],
            (0..255).map(spread_id).collect(),
        ),
    ];
    append_all(&mut index, &metas);
    assert_eq!(index.len(), 4);
    let reopened = open(&layout);
    assert_eq!(reopened.len(), 4);
    for handle in [&index, &reopened] {
        for (rec, meta) in (0..).zip(&metas) {
            assert_eq!(handle.get(rec).unwrap(), at_seq(meta, rec));
        }
    }

    // The index numbers its records itself; a seq the caller sets is ignored.
    let mut late = meta(id_of(5), 2, [0xd5; 32], vec![id_of(1)]);
    late.seq = 77;
    assert_eq!(index.append(&late).unwrap(), 4);
    assert_eq!(index.get(4).unwrap(), at_seq(&late, 4));
    assert_bad_call(index.get(5), "a rec past the end");
    assert_bad_call(index.get(u64::MAX), "u64::MAX");
}

#[test]
fn find_hits_sorted_and_tail() {
    assert_eq!(Index::COMPACT_THRESHOLD, 1024);
    let (_root, layout) = new_root();
    let mut index = open(&layout);
    let metas: Vec<ChangesetMeta> = (0..8).map(spread_meta).collect();
    append_all(&mut index, &metas[..5]);
    assert_eq!(index.compact().unwrap(), 5);
    assert_eq!((index.sorted_count(), index.tail_len()), (5, 0));
    append_all(&mut index, &metas[5..]);
    assert_eq!(
        (index.len(), index.sorted_count(), index.tail_len()),
        (8, 5, 3)
    );

    let reopened = open(&layout);
    assert_eq!((reopened.sorted_count(), reopened.tail_len()), (5, 3));
    for handle in [&index, &reopened] {
        for (rec, meta) in (0..).zip(&metas) {
            assert_eq!(handle.find(meta.id).unwrap(), Some(rec), "{}", meta.id);
        }
        for absent in [spread_id(8), id_of(0), id_of(0xff)] {
            assert_eq!(handle.find(absent).unwrap(), None, "{absent}");
        }
    }
}

/// An id that starts with `prefix`, then `next`, then bytes of `spread_id(n)`.
fn id_with_prefix(prefix: &[u8], next: u8, n: u64) -> ChangesetId {
    let mut bytes = spread_id(n).0.0;
    bytes[..prefix.len()].copy_from_slice(prefix);
    bytes[prefix.len()] = next;
    cs(bytes)
}

#[test]
fn find_prefix_returns_all_matches_sorted_and_tail() {
    // Ids sharing the 12-hex prefix abcdef012345, two compacted into the sorted view and
    // two left in the tail, each after an id sharing only its first 11 hex.
    let shared = [0xab, 0xcd, 0xef, 0x01, 0x23, 0x45];
    let near = [0xab, 0xcd, 0xef, 0x01, 0x23, 0x46];
    let ids = [
        id_with_prefix(&shared, 0x10, 0),
        id_with_prefix(&near, 0x10, 1),
        id_with_prefix(&shared, 0x2f, 2),
        spread_id(3),
        id_with_prefix(&shared, 0x11, 4),
        id_with_prefix(&near, 0x55, 5),
        id_with_prefix(&shared, 0xf0, 6),
    ];
    let metas: Vec<ChangesetMeta> = ids.iter().map(|&id| meta(id, 1, [0; 32], vec![])).collect();
    let (_root, layout) = new_root();
    let mut index = open(&layout);
    append_all(&mut index, &metas[..4]);
    index.compact().unwrap();
    append_all(&mut index, &metas[4..]);
    assert_eq!(index.tail_len(), 3);

    let reopened = open(&layout);
    let full = hex(&ids[6].0.0);
    for handle in [&index, &reopened] {
        let find = |prefix: &str| handle.find_prefix(prefix).unwrap();
        assert_eq!(find("abcdef012345"), [0, 2, 4, 6]);
        assert_eq!(find("ABCDEF012345"), [0, 2, 4, 6], "either case");
        assert_eq!(
            find("abcdef0123451"),
            [0, 4],
            "13 hex: byte 6's high nibble is 1"
        );
        assert_eq!(find("abcdef0123452"), [2]);
        assert_eq!(find("abcdef01234"), [0, 1, 2, 4, 5, 6], "11 hex");
        assert_eq!(find("abcdef012346"), [1, 5]);
        assert_eq!(find("abcdef012347"), Vec::<u64>::new());
        assert_eq!(find(&full), [6], "all 64 hex");
        assert_eq!(find(&full[..63]), [6], "63 hex");
    }
}

#[test]
fn find_prefix_rejects_what_is_not_a_hex_prefix() {
    let (_root, layout) = new_root();
    let mut index = open(&layout);
    append_all(&mut index, &[spread_meta(0)]);
    let too_long = "0".repeat(65);
    for bad in ["", "g", "0x12", "12 ", "é", too_long.as_str()] {
        match index.find_prefix(bad) {
            Err(StoreError::BadPrefix(prefix)) => assert_eq!(prefix, bad),
            other => panic!("{bad:?}: expected BadPrefix, got {other:?}"),
        }
    }
}

#[test]
fn find_delta_hash_groups_rebases() {
    // A rebase onto an upstream that left the files the change touches alone keeps the delta
    // hash, so the original changeset and the rebased one share it: one in the sorted view, one
    // in the tail, then both sorted.
    let change = [0xc1; 32];
    let other = [0xc2; 32];
    let original = meta(spread_id(1), 1, change, vec![]);
    let unrelated = meta(spread_id(2), 1, other, vec![]);
    let rebased = meta(spread_id(3), 2, change, vec![spread_id(2)]);
    let again = meta(spread_id(4), 3, change, vec![spread_id(3)]);
    let (_root, layout) = new_root();
    let mut index = open(&layout);
    append_all(&mut index, &[original, unrelated]);
    index.compact().unwrap();
    append_all(&mut index, &[rebased, again]);

    for compacted in [false, true] {
        if compacted {
            assert_eq!(index.compact().unwrap(), 4);
        }
        let reopened = open(&layout);
        for handle in [&index, &reopened] {
            assert_eq!(handle.find_delta_hash(&change).unwrap(), [0, 2, 3]);
            assert_eq!(handle.find_delta_hash(&other).unwrap(), [1]);
            assert_eq!(
                handle.find_delta_hash(&[0xc3; 32]).unwrap(),
                Vec::<u64>::new()
            );
            assert_eq!(handle.find_delta_hash(&[0; 32]).unwrap(), Vec::<u64>::new());
        }
    }
}

#[test]
fn compact_is_idempotent_and_keeps_order() {
    let (_root, layout) = new_root();
    let mut index = open(&layout);
    assert_eq!(index.compact().unwrap(), 0, "an empty index");

    // Enough records that compaction reads and writes more than one 4,096-entry block.
    let metas: Vec<ChangesetMeta> = (0..8_000).map(spread_meta).collect();
    append_all(&mut index, &metas[..5_000]);
    assert_eq!(index.compact().unwrap(), 5_000);
    append_all(&mut index, &metas[5_000..]);
    assert_eq!(index.compact().unwrap(), 8_000);
    let views: Vec<Vec<u8>> = VIEWS
        .iter()
        .map(|(rel, _)| fs::read(layout.meta_path(rel)).unwrap())
        .collect();
    assert_eq!(index.compact().unwrap(), 8_000, "nothing left to sort");
    assert_eq!(open(&layout).compact().unwrap(), 8_000, "another handle");
    for ((rel, _), before) in VIEWS.iter().zip(&views) {
        assert_eq!(&fs::read(layout.meta_path(rel)).unwrap(), before, "{rel}");
    }
    assert_eq!(index.tail_len(), 0);

    // ids.key is strictly ascending, and ids.rec maps each key to the record holding it.
    let ids = keys_on_disk(&layout, "idx/ids.col");
    let keys = keys_on_disk(&layout, "sorted/ids.key");
    let recs = recs_on_disk(&layout, "sorted/ids.rec");
    assert!(keys.windows(2).all(|pair| pair[0] < pair[1]), "ascending");
    assert_eq!(recs.len(), 8_000);
    for (key, &rec) in keys.iter().zip(&recs) {
        assert_eq!(ids[usize::try_from(rec).unwrap()], *key);
    }
    let mut every = recs.clone();
    every.sort_unstable();
    assert!(every.iter().copied().eq(0..8_000), "each rec once");

    // dh.key is sorted by delta hash, records with the same one by rec.
    let dhs = keys_on_disk(&layout, "idx/dh.col");
    let dh_keys = keys_on_disk(&layout, "sorted/dh.key");
    let dh_recs = recs_on_disk(&layout, "sorted/dh.rec");
    let pairs: Vec<([u8; 32], u64)> = dh_keys.into_iter().zip(dh_recs).collect();
    assert!(pairs.windows(2).all(|pair| pair[0] < pair[1]));
    for (key, rec) in &pairs {
        assert_eq!(dhs[usize::try_from(*rec).unwrap()], *key);
    }

    // Every record is reachable from its id.
    for (rec, meta) in (0..).zip(&metas) {
        assert_eq!(index.find(meta.id).unwrap(), Some(rec));
    }
}

#[derive(Debug, Clone)]
enum Op {
    /// Append a changeset with this id; its delta hash is `[n; 32]` for this `n`.
    Append([u8; 32], u8),
    Compact,
    /// Drop the handle and open the index from its files again.
    Reopen,
}

/// Ids whose first two bytes come from small sets, so that short prefixes are shared.
fn arb_key() -> impl Strategy<Value = [u8; 32]> {
    (
        prop::sample::select(&[0x00_u8, 0x0f, 0xa0, 0xab][..]),
        prop::sample::select(&[0x00_u8, 0x01, 0xff][..]),
        any::<[u8; 30]>(),
    )
        .prop_map(|(first, second, rest)| {
            let mut key = [0; 32];
            key[0] = first;
            key[1] = second;
            key[2..].copy_from_slice(&rest);
            key
        })
}

fn arb_op() -> impl Strategy<Value = Op> {
    prop_oneof![
        8 => (arb_key(), 0..4_u8).prop_map(|(key, dh)| Op::Append(key, dh)),
        1 => Just(Op::Compact),
        1 => Just(Op::Reopen),
    ]
}

/// Checks every lookup of `index` against a naive scan of `model`, record `rec` being
/// `model[rec]`.
fn check_lookups(
    index: &Index,
    model: &[ChangesetMeta],
    probes: &[([u8; 32], usize)],
) -> Result<(), TestCaseError> {
    prop_assert_eq!(index.len(), to_u64(model.len()));
    for (rec, meta) in (0..).zip(model) {
        prop_assert_eq!(index.find(meta.id).unwrap(), Some(rec));
        prop_assert_eq!(index.get(rec).unwrap(), at_seq(meta, rec));
    }
    for (key, _) in probes {
        if !model.iter().any(|meta| meta.id.0.0 == *key) {
            prop_assert_eq!(index.find(cs(*key)).unwrap(), None);
        }
    }
    let hexes: Vec<String> = model.iter().map(|meta| hex(&meta.id.0.0)).collect();
    let prefixes = probes
        .iter()
        .map(|(key, nibbles)| hex(key)[..*nibbles].to_owned())
        .chain(
            hexes
                .iter()
                .enumerate()
                .map(|(rec, hex)| hex[..=(rec % 16)].to_owned()),
        );
    for prefix in prefixes {
        let naive: Vec<u64> = (0..)
            .zip(&hexes)
            .filter(|(_, hex)| hex.starts_with(&prefix))
            .map(|(rec, _)| rec)
            .collect();
        prop_assert_eq!(index.find_prefix(&prefix).unwrap(), naive, "{}", prefix);
    }
    for n in 0..5 {
        let naive: Vec<u64> = (0..)
            .zip(model)
            .filter(|(_, meta)| meta.delta_hash == [n; 32])
            .map(|(rec, _)| rec)
            .collect();
        prop_assert_eq!(index.find_delta_hash(&[n; 32]).unwrap(), naive);
    }
    Ok(())
}

proptest! {
    #[test]
    fn compaction_preserves_lookups_proptest(
        ops in prop::collection::vec(arb_op(), 0..60),
        probes in prop::collection::vec((arb_key(), 1..=16_usize), 0..16),
    ) {
        let (_root, layout) = new_root();
        let fast = || -> Arc<dyn FsOps> { Arc::new(NoSyncFs) };
        let mut index = Index::open(&layout, fast()).unwrap();
        let mut model: Vec<ChangesetMeta> = Vec::new();
        for op in ops {
            match op {
                Op::Append(key, n) => {
                    if model.iter().any(|meta| meta.id.0.0 == key) {
                        continue; // the index holds each changeset once
                    }
                    let parents = model.last().map(|last| vec![last.id]).unwrap_or_default();
                    let generation = to_u64(model.len()) + 1;
                    let meta = meta(cs(key), generation, [n; 32], parents);
                    prop_assert_eq!(index.append(&meta).unwrap(), to_u64(model.len()));
                    model.push(meta);
                }
                Op::Compact => {
                    prop_assert_eq!(index.compact().unwrap(), to_u64(model.len()));
                    prop_assert_eq!(index.tail_len(), 0);
                }
                Op::Reopen => index = Index::open(&layout, fast()).unwrap(),
            }
        }
        check_lookups(&index, &model, &probes)?;
        check_lookups(&Index::open(&layout, fast()).unwrap(), &model, &probes)?;
    }
}

/// A read of a sorted view's entries past its end, which no lookup asks for, is refused as a
/// call the store does not take ([`StoreError::BadCall`]), by key and by rec alike, and so is
/// any read of a view that holds nothing yet.
#[test]
fn a_sorted_view_read_past_its_end_is_a_bad_call() {
    let (_root, layout) = new_root();
    let mut index = open(&layout);
    assert_bad_call(index.ids_view().key_at(0), "a key of an empty view");
    let metas: Vec<ChangesetMeta> = (0..3).map(spread_meta).collect();
    append_all(&mut index, &metas);
    assert_eq!(index.compact().unwrap(), 3);
    let view = index.ids_view();
    assert_eq!(view.len(), 3);
    view.key_at(2).unwrap();
    view.rec_at(2).unwrap();
    assert_bad_call(view.key_at(3), "the key past the end");
    assert_bad_call(view.rec_at(3), "the rec past the end");
    assert_bad_call(view.key_at(u64::MAX), "a key at u64::MAX");
    match view.rec_at(3) {
        Err(StoreError::BadCall(what)) => assert_eq!(
            what,
            "entries 3..+1 are not within a sorted view of 3 entries"
        ),
        other => panic!("expected BadCall, got {other:?}"),
    }
}

#[test]
fn read_at_binary_search_touches_only_key_column() {
    let (_root, layout) = new_root();
    let mut index = open(&layout);
    let metas: Vec<ChangesetMeta> = (0..10_000).map(spread_meta).collect();
    append_all(&mut index, &metas);
    index.compact().unwrap();
    let view = index.ids_view();
    assert_eq!(view.len(), 10_000);
    let mut keys: Vec<[u8; 32]> = metas.iter().map(|meta| meta.id.0.0).collect();
    keys.sort_unstable();

    // A search reads keys only, never the rec column. ⌈log2 10,000⌉ = 14: no more keys than a
    // bisection reads, on keys spread as these are (storage §3 allows 2(⌊log2 m⌋ + 1) = 28 on
    // any keys), and at most 6 on the mean.
    let mut probes = vec![[0; 32], [0xff; 32], keys[0], keys[9_999]];
    probes.extend(keys.iter().step_by(101).copied());
    probes.extend((10_000..10_100).map(|n| spread_id(n).0.0));
    let mut all_reads = 0;
    for probe in &probes {
        let (keys_read, recs_read) = (view.probes(), view.rec_reads());
        let at = view.lower_bound(probe).unwrap();
        let reads = view.probes() - keys_read;
        assert!(reads <= 14, "{reads} probes");
        all_reads += reads;
        assert_eq!(view.rec_reads(), recs_read, "lower_bound reads keys only");
        let want = keys.partition_point(|key| key < probe);
        assert_eq!(at, to_u64(want), "{}", hex(probe));
        if want < keys.len() {
            assert_eq!(view.key_at(at).unwrap(), keys[want]);
        }
    }
    assert!(
        all_reads <= 6 * to_u64(probes.len()),
        "{all_reads} key reads for {} searches",
        probes.len()
    );

    // A whole lookup reads the keys its search reads and no other: the key at the bound it
    // returns is one the search read, kept, not read again to compare. Then one rec for a hit,
    // none for a miss.
    let search_reads = |key: &[u8; 32]| {
        let before = view.probes();
        view.lower_bound(key).unwrap();
        view.probes() - before
    };
    for (rec, meta) in (0..).zip(&metas).step_by(97) {
        let searched = search_reads(&meta.id.0.0);
        let (keys_read, recs_read) = (view.probes(), view.rec_reads());
        assert_eq!(index.find(meta.id).unwrap(), Some(rec));
        assert_eq!(
            view.probes() - keys_read,
            searched,
            "a hit reads no key twice"
        );
        assert_eq!(view.rec_reads() - recs_read, 1, "one rec_at per hit");
    }
    for n in 10_000..10_100 {
        let searched = search_reads(&spread_id(n).0.0);
        let (keys_read, recs_read) = (view.probes(), view.rec_reads());
        assert_eq!(index.find(spread_id(n)).unwrap(), None);
        assert_eq!(
            view.probes() - keys_read,
            searched,
            "a miss reads no key twice"
        );
        assert_eq!(view.rec_reads(), recs_read, "no rec_at for a miss");
    }
}

#[test]
fn find_takes_the_key_at_the_bound_not_the_last_probe() {
    // Three ids, in key order k40 < k80 < kc0, all in the sorted view, appended out of that
    // order so that no rec equals its key's place in the view. A search for k80 reads k80 (its
    // upper bound moves there), then k40 (its lower bound moves past it), by bisection and by
    // interpolation alike, since k80's first bytes lie in the middle of the key space: its last
    // probe is a key below the one it found. `find` compares the key read when the upper bound
    // last moved, never the last probe, which would report k80 missing.
    let (k40, k80, kc0) = ([0x40; 32], [0x80; 32], [0xc0; 32]);
    let (_root, layout) = new_root();
    let mut index = open(&layout);
    let metas = [k80, kc0, k40].map(|key| meta(cs(key), 1, key, vec![]));
    append_all(&mut index, &metas);
    assert_eq!(index.compact().unwrap(), 3);
    let view = index.ids_view();
    for (rec, key) in [(2, k40), (0, k80), (1, kc0)] {
        let recs_read = view.rec_reads();
        assert_eq!(index.find(cs(key)).unwrap(), Some(rec), "{}", hex(&key));
        assert_eq!(view.rec_reads() - recs_read, 1, "one rec_at per hit");
    }
    // Between k80 and kc0, above kc0 and below k40: no key there is the one asked for, so no
    // rec is read, and the tail is empty.
    for absent in [[0xa0; 32], [0xd0; 32], [0x10; 32]] {
        let recs_read = view.rec_reads();
        assert_eq!(index.find(cs(absent)).unwrap(), None, "{}", hex(&absent));
        assert_eq!(view.rec_reads(), recs_read, "no rec_at for a miss");
    }
}

/// In a fresh store root, an index of root records with ids `ids` and delta hashes `dhs`, in
/// rec order, written straight to its files as `Index::append` and `Index::compact` lay them out,
/// the sorted views covering every record, all of it sync-free: many records at the cost of a few
/// writes. The ids must differ.
fn compacted_index(ids: &[[u8; 32]], dhs: &[[u8; 32]]) -> (TempDir, Index) {
    assert_eq!(ids.len(), dhs.len());
    let (root, layout) = bare_root();
    Index::create(&layout, Arc::new(NoSyncFs)).unwrap();
    let write = |rel: &str, bytes: Vec<u8>| fs::write(layout.meta_path(rel), bytes).unwrap();
    write("idx/ids.col", ids.concat());
    write("idx/gen.col", 1_u64.to_le_bytes().repeat(ids.len()));
    write("idx/dh.col", dhs.concat());
    // No parents: every par.col entry is offset 0, count 0, and parents.log stays empty.
    write("idx/par.col", vec![0; 8 * ids.len()]);
    for (keys, view) in [(ids, "ids"), (dhs, "dh")] {
        let mut sorted: Vec<([u8; 32], u64)> = keys.iter().copied().zip(0..).collect();
        sorted.sort_unstable();
        let key_file: Vec<u8> = sorted.iter().flat_map(|(key, _)| *key).collect();
        let rec_file: Vec<u8> = sorted
            .iter()
            .flat_map(|(_, rec)| rec.to_le_bytes())
            .collect();
        write(&format!("sorted/{view}.key"), key_file);
        write(&format!("sorted/{view}.rec"), rec_file);
    }
    (root, Index::open(&layout, Arc::new(NoSyncFs)).unwrap())
}

/// The most keys storage §3 lets a search of a view of `len` keys read: 2(⌊log2 len⌋ + 1).
fn worst_case_probes(len: u64) -> u64 {
    2 * u64::from(len.bit_width())
}

/// At most `max` keys of one of three spreads: over the whole key space, as BLAKE3 ids and
/// delta hashes are; most sharing their first 4 to 7 bytes, the rest spread, so that the first
/// 8 bytes say little about where a key lies; or a few keys, each many times, as the delta hashes
/// of rebased changesets (or the tests' `[n; 32]`) are, whose first bytes say nothing.
fn arb_spread_keys(max: usize) -> impl Strategy<Value = Vec<[u8; 32]>> {
    prop_oneof![
        prop::collection::vec(any::<[u8; 32]>(), 0..max),
        (
            4..=7_usize,
            any::<[u8; 7]>(),
            prop::collection::vec((0..8_u8, any::<[u8; 32]>()), 0..max),
        )
            .prop_map(|(shared, prefix, keys)| {
                (keys.into_iter())
                    .map(|(spread, mut key)| {
                        if spread > 0 {
                            key[..shared].copy_from_slice(&prefix[..shared]);
                        }
                        key
                    })
                    .collect()
            }),
        (
            prop::collection::vec(any::<[u8; 32]>(), 1..4),
            prop::collection::vec(any::<prop::sample::Index>(), 0..max),
        )
            .prop_map(|(few, picks)| {
                (picks.iter())
                    .map(|pick| few[pick.index(few.len())])
                    .collect()
            }),
    ]
}

proptest! {
    #[test]
    fn lower_bound_agrees_with_partition_point(
        ids in arb_spread_keys(300),
        dhs in arb_spread_keys(300),
        random in prop::collection::vec(any::<[u8; 32]>(), 0..16),
        near in prop::collection::vec(any::<prop::sample::Index>(), 0..16),
    ) {
        // Each id once, in the order drawn; as many delta hashes as ids.
        let mut seen = std::collections::HashSet::new();
        let ids: Vec<[u8; 32]> = ids.into_iter().filter(|id| seen.insert(*id)).collect();
        let records = ids.len().min(dhs.len());
        let (ids, dhs) = (&ids[..records], &dhs[..records]);
        let (_root, index) = compacted_index(ids, dhs);
        // `find` takes the key at the bound: the drawn ids, found at their recs.
        for pick in near.iter().filter(|_| !ids.is_empty()) {
            let rec = pick.index(ids.len());
            prop_assert_eq!(index.find(cs(ids[rec])).unwrap(), Some(to_u64(rec)));
        }
        for (name, view, keys) in [("ids", index.ids_view(), ids), ("dh", index.dh_view(), dhs)] {
            let mut sorted = keys.to_vec();
            sorted.sort_unstable();
            // Random keys, the ends of the key space, and keys at and next to drawn keys of
            // the view: one apart in their last byte, and in their eighth.
            let mut probes = random.clone();
            probes.extend([[0; 32], [0xff; 32]]);
            for pick in near.iter().filter(|_| !sorted.is_empty()) {
                let key = sorted[pick.index(sorted.len())];
                for (byte, step) in [(31, 0), (31, 1), (31, u8::MAX), (7, 1), (7, u8::MAX)] {
                    let mut probe = key;
                    probe[byte] = probe[byte].wrapping_add(step);
                    probes.push(probe);
                }
            }
            for probe in &probes {
                let before = view.probes();
                let at = view.lower_bound(probe).unwrap();
                let reads = view.probes() - before;
                let want = to_u64(sorted.partition_point(|key| key < probe));
                prop_assert_eq!(at, want, "{} view, {}", name, hex(probe));
                prop_assert!(
                    reads <= worst_case_probes(view.len()),
                    "{} view of {}: {} reads for {}", name, view.len(), reads, hex(probe)
                );
            }
        }
    }
}

#[test]
fn uniform_ids_take_few_probes() {
    // 100,000 ids spread as BLAKE3 ids are, all in the sorted view. A bisection reads 16 or 17
    // keys per search (⌊log2 100,000⌋ + 1 = 17); interpolating on the first 8 bytes of the keys
    // reads about log2 log2 100,000 + 1: at most 6 on the mean, hits and misses alike.
    const RECORDS: u64 = 100_000;
    let ids: Vec<[u8; 32]> = (0..RECORDS).map(|n| spread_id(n).0.0).collect();
    let dhs: Vec<[u8; 32]> = (RECORDS..2 * RECORDS).map(|n| spread_id(n).0.0).collect();
    let (_root, index) = compacted_index(&ids, &dhs);
    let view = index.ids_view();
    let hits = (0..RECORDS).step_by(20);
    let misses = RECORDS..RECORDS + 5_000;
    let (mut searches, mut reads) = (0, 0);
    for n in hits.chain(misses) {
        let before = view.probes();
        let at = view.lower_bound(&spread_id(n).0.0).unwrap();
        reads += view.probes() - before;
        searches += 1;
        if n < RECORDS {
            assert_eq!(view.key_at(at).unwrap(), spread_id(n).0.0, "a hit's bound");
        }
    }
    assert_eq!(searches, 10_000);
    assert!(
        reads <= 6 * searches,
        "{reads} key reads for {searches} searches"
    );
}

#[test]
fn keys_that_share_their_first_8_bytes_take_about_a_bisection() {
    // 4,096 ids that share their first 8 bytes, the bytes a search interpolates on: they give
    // it nothing to go on. A bisection reads ⌊log2 4,096⌋ + 1 = 13 keys per search, and storage
    // §3 allows 26; the search reads at most 16 on the mean, not crawling through them, since it
    // bisects once both ends of its range share their first 8 bytes, and after two
    // interpolations in a row that each left more than half of the range.
    const RECORDS: u64 = 4_096;
    let clustered = |n: u64| {
        let mut key = spread_id(n).0.0;
        key[..8].copy_from_slice(&[0xab; 8]);
        key
    };
    let ids: Vec<[u8; 32]> = (0..RECORDS).map(clustered).collect();
    let (_root, index) = compacted_index(&ids, &ids);
    let view = index.ids_view();
    let (mut searches, mut reads) = (0, 0);
    for n in (0..RECORDS).step_by(4).chain(RECORDS..RECORDS + 1_024) {
        let before = view.probes();
        let at = view.lower_bound(&clustered(n)).unwrap();
        let read = view.probes() - before;
        assert!(read <= worst_case_probes(RECORDS), "{read} key reads");
        (searches, reads) = (searches + 1, reads + read);
        if n < RECORDS {
            assert_eq!(view.key_at(at).unwrap(), clustered(n), "a hit's bound");
        }
    }
    assert!(
        reads <= 16 * searches,
        "{reads} key reads for {searches} searches"
    );
}

#[test]
fn a_prefix_lookup_reads_only_the_id_column() {
    use swarmops_store::MetaLock;
    use swarmops_testkit::store::temp_dir;

    // Three changesets whose ids share the prefix abcdef012345, two compacted into the sorted
    // view and one in the tail, among others; three of them share a delta hash.
    let shared = [0xab, 0xcd, 0xef, 0x01, 0x23, 0x45];
    let change = [0xc1; 32];
    let first = [
        meta(id_with_prefix(&shared, 0x10, 0), 1, change, vec![]),
        meta(spread_id(1), 1, [0xc2; 32], vec![]),
        meta(id_with_prefix(&shared, 0x2f, 2), 1, change, vec![]),
        meta(spread_id(3), 1, [0xc3; 32], vec![]),
    ];
    let second = [
        meta(spread_id(4), 1, change, vec![]),
        meta(id_with_prefix(&shared, 0x11, 5), 1, [0xc4; 32], vec![]),
        meta(spread_id(6), 1, [0xc5; 32], vec![]),
    ];
    let (_dir, root) = temp_dir();
    let layout = Layout::new(&root);
    let store = MetaStore::create_with_fs(&root, Arc::new(NoSyncFs)).unwrap();
    let insert = |metas: &[ChangesetMeta]| {
        let mut write = store.write().unwrap();
        for meta in metas {
            write.insert_changeset(meta.clone()).unwrap();
        }
        write.commit().unwrap();
    };
    insert(&first);
    {
        // A compaction, as a writer makes one: under the lock.
        let _lock = MetaLock::acquire(&layout, Duration::from_secs(10)).unwrap();
        assert_eq!(
            Index::open(&layout, real_fs()).unwrap().compact().unwrap(),
            4
        );
    }
    insert(&second);
    let read = store.read().unwrap();
    let matches = [first[0].id, first[2].id, second[1].id];
    let rebases = [first[0].id, first[2].id, second[0].id];
    assert_eq!(read.resolve_prefix("abcdef012345").unwrap(), matches);
    assert_eq!(read.by_delta_hash(&change).unwrap(), rebases);

    // Every index file that holds neither the keys searched nor the ids, cut to nothing under
    // the snapshot: a lookup that read a match's whole record would fail on the first of them.
    for rel in ["idx/gen.col", "idx/par.col", "parents.log"] {
        set_len(layout.meta_path(rel), 0);
    }
    assert_eq!(read.resolve_prefix("abcdef012345").unwrap(), matches);
    assert_eq!(read.by_delta_hash(&change).unwrap(), rebases);
    // A prefix lookup searches the id view and scans ids.col: dh.col goes too.
    set_len(layout.meta_path("idx/dh.col"), 0);
    assert_eq!(read.resolve_prefix("abcdef012345").unwrap(), matches);
}

/// Draws from a seeded sequence: [`mix`] of a counter.
struct Draws(u64);

impl Draws {
    /// A draw below `n`.
    fn below(&mut self, n: u64) -> u64 {
        self.0 += 1;
        mix(self.0) % n
    }
}

/// `count` changesets in random chains with merges, their ids `spread_id(ids_from + n)`: now and
/// then a root; otherwise a first parent that is mostly the changeset before, and up to two more
/// parents drawn from those before. Each has the generation a store gives it.
fn random_chains(draws: &mut Draws, count: u64, ids_from: u64) -> Vec<ChangesetMeta> {
    let mut model: Vec<ChangesetMeta> = Vec::new();
    for n in 0..count {
        let mut parents = Vec::new();
        let earlier = to_u64(model.len());
        if earlier > 0 && draws.below(8) > 0 {
            parents.push(match draws.below(4) {
                0 => draws.below(earlier),
                _ => earlier - 1,
            });
            for _ in 0..draws.below(3) {
                let other = draws.below(earlier);
                if !parents.contains(&other) {
                    parents.push(other);
                }
            }
        }
        let at = |rec: u64| &model[usize::try_from(rec).unwrap()];
        let generation = parents.iter().map(|&p| at(p).generation + 1).max();
        let parents = parents.iter().map(|&p| at(p).id).collect();
        let delta_hash = [u8::try_from(n % 7).unwrap(); 32];
        let id = spread_id(ids_from + n);
        model.push(meta(id, generation.unwrap_or(1), delta_hash, parents));
    }
    model
}

/// Appends `metas` to the journal of the store at `layout` as one frame, under the lock, and
/// stops there: what a writer that died right after its commit point leaves behind, which a
/// snapshot reads into its overlay and no index handle counts.
fn append_unmaterialized(layout: &Layout, metas: &[ChangesetMeta]) {
    use swarmops_store::{Journal, MetaLock, Record};

    let _lock = MetaLock::acquire(layout, Duration::from_secs(10)).unwrap();
    let journal = Journal::open(layout, real_fs()).unwrap();
    journal.truncate_torn_tail().unwrap();
    let records: Vec<Record> = (metas.iter())
        .map(|meta| Record::Changeset {
            id: meta.id,
            generation: meta.generation,
            delta_hash: meta.delta_hash,
            parents: meta.parents.clone(),
        })
        .collect();
    journal.append_frame(&records).unwrap();
}

#[test]
fn first_parent_and_generation_match_get() {
    use swarmops_store::MetaLock;
    use swarmops_testkit::store::temp_dir;

    // Random chains with merges, written in transactions of 1 to 12 changesets (no checkpoint:
    // fewer than 32 frames), half of the seeds with the first half compacted into the sorted
    // view, then 4 more changesets in a frame no writer has materialized: the snapshot's
    // overlay. Every changeset's first parent, read narrowly, is the first of those
    // `changeset` gives, wherever it is found; every record's narrow reads are `get`'s fields.
    for seed in 0..4_u64 {
        let mut draws = Draws(seed << 32);
        let model = random_chains(&mut draws, 124, seed * 1_000);
        let (stored, unmaterialized) = model.split_at(120);
        let (_dir, root) = temp_dir();
        let layout = Layout::new(&root);
        let store = MetaStore::create_with_fs(&root, Arc::new(NoSyncFs)).unwrap();
        let mut written = 0;
        while written < stored.len() {
            let take = usize::try_from(1 + draws.below(12)).unwrap();
            let batch = &stored[written..stored.len().min(written + take)];
            let mut write = store.write().unwrap();
            for meta in batch {
                write.insert_changeset(meta.clone()).unwrap();
            }
            write.commit().unwrap();
            written += batch.len();
            if seed % 2 == 1 && written >= 60 && written - batch.len() < 60 {
                // A compaction, as a writer makes one: under the lock.
                let _lock = MetaLock::acquire(&layout, Duration::from_secs(10)).unwrap();
                let mut index = Index::open(&layout, real_fs()).unwrap();
                assert_eq!(index.compact().unwrap(), to_u64(written));
            }
        }
        append_unmaterialized(&layout, unmaterialized);

        let read = store.read().unwrap();
        for (seq, meta) in (0..).zip(&model) {
            let what = format!("seed {seed}, changeset {seq}");
            let full = read.changeset(meta.id).unwrap().expect(&what);
            assert_eq!(full, at_seq(meta, seq), "{what}");
            assert_eq!(
                read.first_parent(meta.id).unwrap(),
                Some(full.parents.first().copied()),
                "{what}"
            );
        }
        for absent in [spread_id(999_999), id_of(0), id_of(0xff)] {
            assert_eq!(read.first_parent(absent).unwrap(), None, "{absent}");
        }

        let index = Index::open(&layout, real_fs()).unwrap();
        assert_eq!(index.len(), 120, "the last frame is not materialized");
        for rec in 0..index.len() {
            let full = index.get(rec).unwrap();
            assert_eq!(index.id(rec).unwrap(), full.id, "seed {seed}, rec {rec}");
            assert_eq!(index.generation(rec).unwrap(), full.generation);
            assert_eq!(
                index.first_parent(rec).unwrap(),
                full.parents.first().copied()
            );
        }
        for past in [index.len(), u64::MAX] {
            assert_bad_call(index.id(past), "id past the end");
            assert_bad_call(index.generation(past), "generation past the end");
            assert_bad_call(index.first_parent(past), "first_parent past the end");
        }
    }
}

#[test]
fn narrow_lookups_read_their_own_column_alone() {
    // A root, a child and a merge. Each narrow lookup reads its own file and no other: with every
    // other index file cut to nothing under the handle, it still answers for every record, where
    // `get`, which reads them all, fails. The first parent of the merge is read without its
    // second parent, which is cut off parents.log.
    let metas = [
        meta(id_of(1), 1, [0xd1; 32], vec![]),
        meta(id_of(2), 2, [0xd2; 32], vec![id_of(1)]),
        meta(id_of(3), 3, [0xd3; 32], vec![id_of(2), id_of(1)]),
    ];
    let lookups: [(&str, &[&str]); 3] = [
        (
            "id",
            &["idx/gen.col", "idx/dh.col", "idx/par.col", "parents.log"],
        ),
        (
            "generation",
            &["idx/ids.col", "idx/dh.col", "idx/par.col", "parents.log"],
        ),
        (
            "first_parent",
            &["idx/ids.col", "idx/gen.col", "idx/dh.col"],
        ),
    ];
    for (lookup, cut) in lookups {
        let (_root, layout) = new_root();
        let mut index = open(&layout);
        append_all(&mut index, &metas);
        for rel in cut {
            set_len(layout.meta_path(rel), 0);
        }
        if lookup == "first_parent" {
            // The child's parent and the merge's first parent: 64 of the 96 bytes.
            set_len(layout.meta_path("parents.log"), 64);
        }
        for (rec, meta) in (0..).zip(&metas) {
            match lookup {
                "id" => assert_eq!(index.id(rec).unwrap(), meta.id),
                "generation" => assert_eq!(index.generation(rec).unwrap(), meta.generation),
                _ => assert_eq!(
                    index.first_parent(rec).unwrap(),
                    meta.parents.first().copied()
                ),
            }
        }
        assert!(index.get(2).is_err(), "{lookup}: get reads what was cut");
    }
}

#[test]
#[ignore = "measurement, ~10 s: 200,000 appends; prints the find latency percentiles"]
fn index_on_wide_history_is_fast() {
    const RECORDS: u64 = 200_000;
    let (_root, layout) = new_root();
    let mut index = open(&layout);
    let started = Instant::now();
    let mut syncs = Duration::ZERO;
    for n in 0..RECORDS {
        let parents = n.checked_sub(1).map(spread_id).into_iter().collect();
        assert_eq!(
            index
                .append(&meta(spread_id(n), n + 1, [0; 32], parents))
                .unwrap(),
            n
        );
        if (n + 1) % 10_000 == 0 {
            let sync = Instant::now();
            index.sync().unwrap();
            syncs += sync.elapsed();
        }
    }
    let appends = started.elapsed().saturating_sub(syncs);
    let started = Instant::now();
    assert_eq!(index.compact().unwrap(), RECORDS);
    let compaction = started.elapsed();

    let mut samples = Vec::with_capacity(1_000);
    for i in 0..1_000 {
        let n = mix(i) % RECORDS;
        let started = Instant::now();
        let found = index.find(spread_id(n)).unwrap();
        samples.push(started.elapsed());
        assert_eq!(found, Some(n));
    }
    samples.sort_unstable();
    let (p50, p90, p99) = (
        percentile(&samples, 50),
        percentile(&samples, 90),
        percentile(&samples, 99),
    );
    let max = samples[samples.len() - 1];
    let each = appends.as_secs_f64() * 1e6 / 200_000.0;
    println!(
        "{RECORDS} records: appends {appends:?} ({each:.1} us each), 20 syncs {syncs:?}, \
         compaction {compaction:?}"
    );
    println!("1,000 finds: p50 {p50:?} p90 {p90:?} p99 {p99:?} max {max:?}");
    assert!(p99 < Duration::from_millis(1), "find p99 {p99:?}");
}

#[test]
fn append_writes_the_fixed_column_layout() {
    let (_root, layout) = new_root();
    let mut index = open(&layout);
    append_all(
        &mut index,
        &[
            meta(id_of(0x11), 1, [0xd1; 32], vec![]),
            meta(
                id_of(0x22),
                0x0102_0304_0506_0708,
                [0xd2; 32],
                vec![id_of(0x11), id_of(0xaa)],
            ),
            meta(id_of(0x33), 3, [0xd3; 32], vec![id_of(0x22)]),
        ],
    );
    let read = |rel| fs::read(layout.meta_path(rel)).unwrap();
    assert_eq!(
        read("idx/ids.col"),
        [[0x11; 32], [0x22; 32], [0x33; 32]].concat()
    );
    let mut generations = vec![1, 0, 0, 0, 0, 0, 0, 0];
    generations.extend([8, 7, 6, 5, 4, 3, 2, 1]);
    generations.extend([3, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(read("idx/gen.col"), generations);
    assert_eq!(
        read("idx/dh.col"),
        [[0xd1; 32], [0xd2; 32], [0xd3; 32]].concat()
    );
    // par.col = parents.log offset << 8 | parent count: no parents at 0, two at 0, one at 64.
    let par: Vec<u8> = [0_u64, 2, (64 << 8) | 1]
        .iter()
        .flat_map(|entry| entry.to_le_bytes())
        .collect();
    assert_eq!(read("idx/par.col"), par);
    assert_eq!(
        read("parents.log"),
        [[0x11; 32], [0xaa; 32], [0x22; 32]].concat()
    );
}

#[test]
fn create_lays_out_the_index_and_open_reads_sizes_only() {
    let (_root, layout) = bare_root();
    fs::create_dir(layout.meta_dir()).unwrap(); // as the journal leaves meta/
    let (fs, calls) = recording_fs();
    let index = Index::create(&layout, Arc::clone(&fs)).unwrap();
    assert_eq!(
        (index.len(), index.sorted_count(), index.tail_len()),
        (0, 0, 0)
    );
    assert_eq!(
        calls(),
        [
            "create_dir_all idx",
            "create_dir_all sorted",
            "append ids.col",
            "append gen.col",
            "append dh.col",
            "append par.col",
            "append parents.log",
            "append ids.key",
            "append ids.rec",
            "append dh.key",
            "append dh.rec",
            "fsync_dir idx",
            "fsync_dir sorted",
            "fsync_dir meta",
        ]
    );
    for (rel, _) in COLUMNS.iter().chain(&VIEWS) {
        assert_eq!(file_len(layout.meta_path(rel)), 0, "{rel}");
    }

    let again = Index::open(&layout, fs).unwrap();
    assert_eq!(again.len(), 0);
    assert_eq!(calls(), Vec::<String>::new(), "no FsOps call");
}

#[test]
fn open_does_not_create_files() {
    // Lock-free readers open the index, so opening creates nothing, even where every index
    // file is missing: a store gets its index files from `MetaStore::create` alone.
    let (_root, layout) = bare_root();
    fs::create_dir(layout.meta_dir()).unwrap(); // as the journal leaves meta/
    let (fs, calls) = recording_fs();
    assert_index_corrupt(
        Index::open(&layout, Arc::clone(&fs)).map(|index| index.len()),
        "no index files",
    );
    assert_eq!(calls(), Vec::<String>::new(), "no FsOps call");
    assert_eq!(
        fs::read_dir(layout.meta_dir()).unwrap().count(),
        0,
        "nothing was created in meta/"
    );
}

#[test]
fn a_missing_column_beside_a_non_empty_one_is_index_corrupt() {
    // A complete store has every index file, and nothing removes one, so a missing column,
    // parents.log or view file is damage, beside full columns or empty ones; opening creates
    // nothing in its place.
    for records in [8, 0] {
        for rel in COLUMNS.iter().chain(&VIEWS).map(|(rel, _)| *rel) {
            let what = format!("{rel} missing beside {records} records");
            let (_root, layout) = new_root();
            let mut index = open(&layout);
            let metas: Vec<ChangesetMeta> = (0..records).map(spread_meta).collect();
            append_all(&mut index, &metas);
            if records > 0 {
                index.compact().unwrap();
            }
            drop(index);
            fs::remove_file(layout.meta_path(rel)).unwrap();
            assert_index_corrupt(open_len(&layout), &what);
            assert!(!layout.meta_path(rel).exists(), "{what}: open created it");
        }
    }
}

#[test]
fn append_sync_and_compact_make_the_documented_fs_calls() {
    let (_root, layout) = new_root();
    drop(open(&layout));
    let (fs, calls) = recording_fs();
    let mut index = Index::open(&layout, fs).unwrap();
    assert_eq!(calls(), Vec::<String>::new());

    index
        .append(&meta(id_of(1), 1, [0xd1; 32], vec![]))
        .unwrap();
    let appends = [
        "append parents.log",
        "append par.col",
        "append gen.col",
        "append dh.col",
        "append ids.col",
    ];
    assert_eq!(calls(), appends);
    index.sync().unwrap();
    let syncs = [
        "sync_data parents.log",
        "sync_data par.col",
        "sync_data gen.col",
        "sync_data dh.col",
        "sync_data ids.col",
    ];
    assert_eq!(calls(), syncs);

    assert_eq!(index.compact().unwrap(), 1);
    let mut want: Vec<&str> = syncs.to_vec();
    want.extend([
        "remove ids.key.tmp",
        "remove ids.rec.tmp",
        "append ids.key.tmp",
        "append ids.rec.tmp",
        "remove dh.key.tmp",
        "remove dh.rec.tmp",
        "append dh.key.tmp",
        "append dh.rec.tmp",
        "sync_data ids.key.tmp",
        "sync_data ids.rec.tmp",
        "sync_data dh.key.tmp",
        "sync_data dh.rec.tmp",
        "rename_durable ids.key.tmp ids.key",
        "rename_durable ids.rec.tmp ids.rec",
        "rename_durable dh.key.tmp dh.key",
        "rename_durable dh.rec.tmp dh.rec",
    ]);
    assert_eq!(calls(), want);
    assert_eq!(index.compact().unwrap(), 1);
    assert_eq!(calls(), Vec::<String>::new(), "nothing to compact");
    assert_eq!(index.repair_tail().unwrap(), 0);
    assert_eq!(calls(), Vec::<String>::new(), "nothing to repair");
}

/// A crash in the middle of an append: what it is, the bytes it left in which files, and how
/// many records `repair_tail` drops for it.
type Crash<'a> = (&'a str, Vec<(&'a str, &'a [u8])>, u64);

#[test]
fn repair_tail_cuts_a_partially_appended_record() {
    let metas = [
        meta(id_of(1), 1, [0xd1; 32], vec![]),
        meta(id_of(2), 2, [0xd2; 32], vec![id_of(1)]),
        meta(id_of(3), 3, [0xd3; 32], vec![id_of(2)]),
    ];
    // What a crash inside the third append leaves: the files written before it hold their
    // part of record 2 (the last one half-written), the rest none; parents.log comes first
    // and ids.col last. With power lost before a sync, any file can lack its part: in the
    // last two cases ids.col holds record 2 while parents.log, or gen.col, lacks it. With
    // each, how many records repair_tail drops: record slots cut from the columns, so bytes
    // in parents.log alone drop none.
    let par = ((32_u64 << 8) | 1).to_le_bytes();
    let generation = 3_u64.to_le_bytes();
    let crashes: [Crash<'_>; 4] = [
        ("inside parents.log", vec![("parents.log", &[2; 20])], 0),
        (
            "inside dh.col",
            vec![
                ("parents.log", &[2; 32]),
                ("idx/par.col", &par),
                ("idx/gen.col", &generation),
                ("idx/dh.col", &[0xd3; 16]),
            ],
            1,
        ),
        (
            "parents.log not synced",
            vec![
                ("idx/par.col", &par),
                ("idx/gen.col", &generation),
                ("idx/dh.col", &[0xd3; 32]),
                ("idx/ids.col", &[3; 32]),
            ],
            1,
        ),
        (
            "gen.col not synced",
            vec![
                ("parents.log", &[2; 32]),
                ("idx/par.col", &par),
                ("idx/dh.col", &[0xd3; 32]),
                ("idx/ids.col", &[3; 32]),
            ],
            1,
        ),
    ];
    for (what, leftovers, dropped) in crashes {
        for repair_first in [true, false] {
            let (_root, layout) = new_root();
            let mut index = open(&layout);
            append_all(&mut index, &metas[..2]);
            let clean = column_lens(&layout);
            for (rel, bytes) in &leftovers {
                append_bytes(layout.meta_path(rel), bytes);
            }

            let reader = open(&layout);
            assert_eq!(reader.len(), 2, "{what}: readers count whole records only");
            assert_eq!(reader.get(1).unwrap(), at_seq(&metas[1], 1), "{what}");
            assert_eq!(reader.find(id_of(3)).unwrap(), None, "{what}");

            let mut writer = open(&layout);
            if repair_first {
                assert_eq!(writer.repair_tail().unwrap(), dropped, "{what}: dropped");
                assert_eq!(column_lens(&layout), clean, "{what}");
                assert_eq!(writer.repair_tail().unwrap(), 0, "{what}: repaired already");
            }
            // append repairs first itself when the files are not where it left them.
            assert_eq!(writer.append(&metas[2]).unwrap(), 2, "{what}");
            writer.sync().unwrap();
            let fresh = open(&layout);
            for (rec, meta) in (0..).zip(&metas) {
                assert_eq!(fresh.get(rec).unwrap(), at_seq(meta, rec), "{what}");
                assert_eq!(fresh.find(meta.id).unwrap(), Some(rec), "{what}");
            }
            assert_eq!(column_lens(&layout), [64, 24, 24, 96, 96], "{what}");
        }
    }
}

/// `truncate_to` cuts whole records off the end, `ids.col` first, as a writer whose sync
/// failed does (see `MetaWrite`), never below what the sorted views cover; appends then carry
/// on where it cut. A handle opened before the cut fails, with an I/O error rather than wrong
/// data, on every lookup that reads past the cut, as `truncate_to`'s documentation lists.
#[test]
fn truncate_to_cuts_whole_records_back_to_a_count() {
    let metas = [
        meta(id_of(1), 1, [0xd1; 32], vec![]),
        meta(id_of(2), 2, [0xd2; 32], vec![id_of(1)]),
        meta(id_of(3), 3, [0xd3; 32], vec![id_of(2), id_of(1)]),
        meta(id_of(4), 1, [0xd4; 32], vec![]),
        meta(id_of(5), 2, [0xd5; 32], vec![id_of(4)]),
        meta(id_of(6), 3, [0xd6; 32], vec![id_of(5)]),
    ];
    let (_root, layout) = new_root();
    let (fs, calls) = recording_fs();
    let mut index = Index::open(&layout, fs).unwrap();
    append_all(&mut index, &metas[..3]);
    index.compact().unwrap();
    append_all(&mut index, &metas[3..]);
    let stale = open(&layout);
    calls();

    // Records 0..=3 stay: record 3 has no parents, so parents.log keeps the 3 parents of
    // records 1 and 2.
    assert_eq!(index.truncate_to(4).unwrap(), 4);
    assert_eq!(index.len(), 4);
    assert_eq!(column_lens(&layout), [96, 32, 32, 128, 128]);
    let cuts = [
        "truncate ids.col",
        "truncate dh.col",
        "truncate gen.col",
        "truncate par.col",
        "truncate parents.log",
    ];
    assert_eq!(calls(), cuts);
    let fresh = open(&layout);
    assert_eq!(fresh.len(), 4);
    for (rec, meta) in (0..).zip(&metas[..4]) {
        assert_eq!(fresh.get(rec).unwrap(), at_seq(meta, rec));
        assert_eq!(fresh.find(meta.id).unwrap(), Some(rec));
    }
    assert_eq!(fresh.find(id_of(5)).unwrap(), None);
    // The stale handle answers what it reads below the cut: records, and ids the view holds. A
    // lookup that reads past the cut fails: a record cut, and any scan of the tail, which runs
    // to the end the handle counts, whether the id it looks for is cut or not.
    assert_eq!(stale.get(1).unwrap(), at_seq(&metas[1], 1));
    assert_eq!(stale.find(id_of(1)).unwrap(), Some(0), "the view holds it");
    let past_the_cut = [
        ("get of a record cut", stale.get(5).map(drop)),
        ("find of a record kept", stale.find(id_of(4)).map(drop)),
        ("find_prefix", stale.find_prefix("01").map(drop)),
        (
            "find_delta_hash",
            stale.find_delta_hash(&[0xd1; 32]).map(drop),
        ),
    ];
    for (what, got) in past_the_cut {
        match got {
            Err(StoreError::Io(e)) => assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof, "{what}"),
            other => panic!("{what} on a stale handle: expected an I/O error, got {other:?}"),
        }
    }

    // Never below the 3 records the views cover; parents.log ends there already.
    assert_eq!(index.truncate_to(1).unwrap(), 3);
    assert_eq!(column_lens(&layout), [96, 24, 24, 96, 96]);
    assert_eq!(calls(), cuts[..4]);
    let fresh = open(&layout);
    assert_eq!((fresh.len(), fresh.sorted_count()), (3, 3));
    // Nothing to cut: a count it does not reach.
    assert_eq!(index.truncate_to(10).unwrap(), 3);
    assert_eq!(calls(), Vec::<String>::new());

    // Appends carry on at the cut.
    append_all(&mut index, &metas[3..]);
    assert_eq!(index.compact().unwrap(), 6);
    let fresh = open(&layout);
    for (rec, meta) in (0..).zip(&metas) {
        assert_eq!(fresh.get(rec).unwrap(), at_seq(meta, rec));
        assert_eq!(fresh.find(meta.id).unwrap(), Some(rec));
    }
}

#[test]
fn readers_tolerate_an_interrupted_compaction_and_repair_tail_finishes_it() {
    // compact() renames ids.key, ids.rec, dh.key and dh.rec over the old view in that order;
    // a crash after `renamed` of them leaves those new and the rest old.
    for renamed in 1..4 {
        let what = format!("{renamed} of 4 renamed");
        let (_root, layout) = new_root();
        let mut index = open(&layout);
        let metas: Vec<ChangesetMeta> = (0..8).map(spread_meta).collect();
        append_all(&mut index, &metas[..5]);
        index.compact().unwrap();
        let old: Vec<Vec<u8>> = VIEWS
            .iter()
            .map(|(rel, _)| fs::read(layout.meta_path(rel)).unwrap())
            .collect();
        append_all(&mut index, &metas[5..]);
        index.compact().unwrap();
        for ((rel, _), bytes) in VIEWS.iter().zip(&old).skip(renamed) {
            fs::write(layout.meta_path(rel), bytes).unwrap();
        }

        let reader = open(&layout);
        assert_eq!(reader.len(), 8, "{what}");
        assert!(reader.tail_len() >= 3, "{what}: the views disagree");
        for (rec, meta) in (0..).zip(&metas) {
            assert_eq!(reader.find(meta.id).unwrap(), Some(rec), "{what}");
            let prefix = &hex(&meta.id.0.0)[..12];
            assert_eq!(reader.find_prefix(prefix).unwrap(), [rec], "{what}");
        }
        assert_eq!(
            reader.find_delta_hash(&[0; 32]).unwrap(),
            [0, 3, 6],
            "{what}"
        );
        assert_eq!(reader.find_delta_hash(&[2; 32]).unwrap(), [2, 5], "{what}");

        let mut writer = open(&layout);
        assert_eq!(
            writer.repair_tail().unwrap(),
            0,
            "{what}: no record dropped"
        );
        assert_eq!((writer.sorted_count(), writer.tail_len()), (8, 0), "{what}");
        for (rel, width) in VIEWS {
            assert_eq!(file_len(layout.meta_path(rel)), 8 * width, "{what}: {rel}");
        }
        let repaired = open(&layout);
        assert_eq!(repaired.tail_len(), 0, "{what}");
        for (rec, meta) in (0..).zip(&metas) {
            assert_eq!(repaired.find(meta.id).unwrap(), Some(rec), "{what}");
        }
        assert_eq!(repaired.find_delta_hash(&[1; 32]).unwrap(), [1, 4, 7]);
    }
}

#[test]
fn readers_opening_during_compactions_see_whole_views() {
    // Readers open the index without a lock while a writer appends and compacts over and
    // over: a view in the middle of its renames must never look damaged to them, nor cover
    // more records than they count, and they must find each record they count. Twice as
    // many readers as cores, so that some are preempted in the middle of an open.
    let readers = 2 * thread::available_parallelism().map_or(4, NonZero::get);
    let (_root, layout) = new_root();
    let metas: Vec<ChangesetMeta> = (0..600).map(spread_meta).collect();
    let mut index = Index::open(&layout, Arc::new(NoSyncFs)).unwrap();
    append_all(&mut index, &metas[..4]);
    index.compact().unwrap();
    let writing = AtomicBool::new(true);
    thread::scope(|scope| {
        scope.spawn(|| {
            for chunk in metas[4..].chunks(4) {
                append_all(&mut index, chunk);
                index.compact().unwrap();
            }
            writing.store(false, Ordering::SeqCst);
        });
        for reader in 0..to_u64(readers) {
            let (layout, metas, writing) = (&layout, &metas, &writing);
            scope.spawn(move || {
                let mut opens = 0_u64;
                while writing.load(Ordering::SeqCst) {
                    let index = Index::open(layout, Arc::new(NoSyncFs)).unwrap();
                    let rec = mix((reader << 32) | opens) % index.len();
                    let meta = &metas[usize::try_from(rec).unwrap()];
                    assert_eq!(index.find(meta.id).unwrap(), Some(rec));
                    opens += 1;
                }
            });
        }
    });
}

#[test]
fn compaction_refuses_damaged_views_and_ids_held_twice() {
    // A view out of order, or an id the index holds twice (the writer's checks failed),
    // would make a view that answers wrongly: compaction refuses to write it.
    let setup = || {
        let (root, layout) = new_root();
        let mut index = open(&layout);
        let metas: Vec<ChangesetMeta> = (0..8).map(spread_meta).collect();
        append_all(&mut index, &metas[..5]);
        index.compact().unwrap();
        append_all(&mut index, &metas[5..]);
        (root, layout, index)
    };

    let (_root, layout, mut index) = setup();
    let path = layout.meta_path("sorted/ids.key");
    let mut keys = fs::read(&path).unwrap();
    let (first, rest) = keys.split_at_mut(32);
    first.swap_with_slice(&mut rest[..32]);
    fs::write(&path, &keys).unwrap();
    assert_index_corrupt(index.compact(), "two keys swapped");
    assert_eq!(fs::read(&path).unwrap(), keys, "the view stays as it was");

    let (_root, layout, mut index) = setup();
    let path = layout.meta_path("sorted/ids.rec");
    let mut recs = fs::read(&path).unwrap();
    recs[..8].copy_from_slice(&5_u64.to_le_bytes());
    fs::write(&path, &recs).unwrap();
    assert_index_corrupt(index.compact(), "a rec past the view's records");

    let (_root, layout, mut index) = setup();
    assert_eq!(index.append(&spread_meta(1)).unwrap(), 8);
    assert_index_corrupt(index.compact(), "an id twice");
    assert_eq!(index.find(spread_id(1)).unwrap(), Some(1));
    assert_eq!(
        open(&layout).sorted_count(),
        5,
        "the views stay as they were"
    );
}

#[test]
fn a_sorted_view_rec_past_what_it_covers_is_index_corrupt() {
    // A view of m entries holds the recs [0, m), as compaction checks when it merges one, so a
    // lookup checks each rec it reads from a view against the view's own length: one at or
    // past it is damage (a `.rec` entry gone bad), never a record to read, whether it points
    // into the unsorted tail (another changeset's record) or past the index's end.
    let (_root, layout) = new_root();
    let mut index = open(&layout);
    let metas: Vec<ChangesetMeta> = (0..5).map(spread_meta).collect();
    append_all(&mut index, &metas[..3]);
    assert_eq!(index.compact().unwrap(), 3);
    append_all(&mut index, &metas[3..]);
    assert_eq!((index.sorted_count(), index.tail_len()), (3, 2));
    let id = keys_on_disk(&layout, "sorted/ids.key")[1];
    let its_rec = recs_on_disk(&layout, "sorted/ids.rec")[1];
    let dh = keys_on_disk(&layout, "sorted/dh.key")[1];
    // The first record of the tail, then one past the index's end.
    for rec in [3, 7] {
        let ids = set_rec(&layout, "sorted/ids.rec", 1, rec);
        let index = open(&layout);
        assert_eq!(index.len(), 5);
        assert_view_points_past(index.find(cs(id)), format!("find, rec {rec}"));
        let prefix = &hex(&id)[..12];
        assert_view_points_past(index.find_prefix(prefix), format!("find_prefix, rec {rec}"));
        fs::write(layout.meta_path("sorted/ids.rec"), ids).unwrap();
        let dhs = set_rec(&layout, "sorted/dh.rec", 1, rec);
        let found = open(&layout).find_delta_hash(&dh);
        assert_view_points_past(found, format!("find_delta_hash, rec {rec}"));
        fs::write(layout.meta_path("sorted/dh.rec"), dhs).unwrap();
    }
    assert_eq!(
        open(&layout).find(cs(id)).unwrap(),
        Some(its_rec),
        "the views restored"
    );

    // As a reader meets it, through a snapshot: one write of 1,025 changesets, whose commit
    // compacts them (the tail passes 1,024), so that the views cover every record.
    let (_dir, store_layout) = bare_root();
    let store = MetaStore::create(store_layout.root()).unwrap();
    let mut write = store.write().unwrap();
    for n in 0..=Index::COMPACT_THRESHOLD {
        write.insert_changeset(spread_meta(n)).unwrap();
    }
    write.commit().unwrap();
    assert_eq!(
        open(&store_layout).sorted_count(),
        Index::COMPACT_THRESHOLD + 1
    );
    let first = keys_on_disk(&store_layout, "sorted/ids.key")[0];
    set_rec(
        &store_layout,
        "sorted/ids.rec",
        0,
        Index::COMPACT_THRESHOLD + 1,
    );
    let read = store.read().unwrap();
    assert_view_points_past(read.changeset(cs(first)), "a snapshot's changeset");
    let prefix = &hex(&first)[..12];
    assert_view_points_past(read.resolve_prefix(prefix), "a snapshot's resolve_prefix");
}

/// Sets entry `at` of the rec file `meta/<rel>` to `rec`; returns what the file held before.
fn set_rec(layout: &Layout, rel: &str, at: usize, rec: u64) -> Vec<u8> {
    let path = layout.meta_path(rel);
    let before = fs::read(&path).unwrap();
    let mut recs = before.clone();
    recs[at * 8..at * 8 + 8].copy_from_slice(&rec.to_le_bytes());
    fs::write(&path, recs).unwrap();
    before
}

/// Asserts that `got` is the error of a lookup that read, from a sorted view, a rec the view
/// does not cover.
#[track_caller]
fn assert_view_points_past<T: Debug>(got: Result<T, StoreError>, what: impl AsRef<str>) {
    let what = what.as_ref();
    match got {
        Err(StoreError::IndexCorrupt {
            what: "a sorted view points at a record it does not cover",
        }) => {}
        other => panic!("{what}: expected IndexCorrupt, got {other:?}"),
    }
}

#[test]
fn damage_under_an_open_handle_is_index_corrupt_and_cuts_nothing() {
    // gen.col cut back to 4 of 8 records under an open handle. No crash and no writer cuts
    // a whole record, so this is damage: whichever write meets it must refuse, without
    // cutting the other files to match, and leave the handle as it was. The handle counts
    // all 8 records, with views over them or none, or it counts 4 and another handle
    // appended and compacted the other 4 since.
    for compacted_by in ["this handle", "nobody", "another handle"] {
        for op in ["append", "compact", "repair_tail"] {
            let what = format!("{op}, compacted by {compacted_by}");
            let (_root, layout) = new_root();
            let mut index = open(&layout);
            let metas: Vec<ChangesetMeta> = (0..9).map(spread_meta).collect();
            append_all(&mut index, &metas[..4]);
            let (counted, sorted) = match compacted_by {
                "this handle" => {
                    append_all(&mut index, &metas[4..8]);
                    index.compact().unwrap();
                    (8, 8)
                }
                "nobody" => {
                    append_all(&mut index, &metas[4..8]);
                    (8, 0)
                }
                _ => {
                    let mut other = open(&layout);
                    append_all(&mut other, &metas[4..8]);
                    other.compact().unwrap();
                    (4, 0)
                }
            };
            set_len(layout.meta_path("idx/gen.col"), 4 * 8);
            let read_all = || -> Vec<Vec<u8>> {
                COLUMNS
                    .iter()
                    .chain(&VIEWS)
                    .map(|(rel, _)| fs::read(layout.meta_path(rel)).unwrap_or_default())
                    .collect()
            };
            let before = read_all();

            let got = match op {
                "append" => index.append(&metas[8]),
                "compact" => index.compact(),
                _ => index.repair_tail(),
            };
            assert_index_corrupt(got, &what);
            assert!(read_all() == before, "{what}: a file was changed");
            assert_eq!(index.len(), counted, "{what}");
            assert_eq!(index.sorted_count(), sorted, "{what}");
            assert_eq!(index.tail_len(), counted - sorted, "{what}");
            assert_eq!(index.find(spread_id(100)).unwrap(), None, "{what}");
            let last = usize::try_from(counted - 1).unwrap();
            assert_eq!(
                index.find(metas[last].id).unwrap(),
                Some(counted - 1),
                "{what}"
            );
        }
    }
}

#[test]
fn open_rejects_index_files_no_crash_leaves() {
    // Five records compacted, then three more; the views of the five are kept aside.
    let setup = || {
        let (root, layout) = new_root();
        let mut index = open(&layout);
        let metas: Vec<ChangesetMeta> = (0..8).map(spread_meta).collect();
        append_all(&mut index, &metas[..5]);
        index.compact().unwrap();
        let five: Vec<Vec<u8>> = VIEWS
            .iter()
            .map(|(rel, _)| fs::read(layout.meta_path(rel)).unwrap())
            .collect();
        append_all(&mut index, &metas[5..]);
        index.compact().unwrap();
        (root, layout, five)
    };

    let (_root, layout, _) = setup();
    append_bytes(layout.meta_path("sorted/ids.key"), &[0; 5]);
    assert_index_corrupt(open_len(&layout), "a partial key");

    // A compaction renames the ids view first, so a dh view ahead of it is damage.
    let (_root, layout, five) = setup();
    for ((rel, _), bytes) in VIEWS.iter().zip(&five).take(2) {
        fs::write(layout.meta_path(rel), bytes).unwrap();
    }
    assert_index_corrupt(open_len(&layout), "dh view ahead of ids");

    // Views over records that the columns no longer hold.
    let (_root, layout, _) = setup();
    for (rel, width) in COLUMNS.into_iter().skip(1) {
        set_len(layout.meta_path(rel), 4 * width);
    }
    assert_index_corrupt(open_len(&layout), "views past the columns");

    // A par.col entry pointing past the end of parents.log, or off a parent boundary.
    for (entry, what) in [((1_u64 << 20) | 1, "past the end"), (1 << 8, "misaligned")] {
        let (_root, layout, _) = setup();
        let mut par = fs::read(layout.meta_path("idx/par.col")).unwrap();
        par[..8].copy_from_slice(&entry.to_le_bytes());
        fs::write(layout.meta_path("idx/par.col"), par).unwrap();
        let index = open(&layout);
        assert_eq!(
            index.len(),
            8,
            "{what}: only the last record is checked on open"
        );
        assert_index_corrupt(index.get(0), what);
        assert_eq!(index.get(1).unwrap(), at_seq(&spread_meta(1), 1), "{what}");
    }
}

#[test]
fn append_refuses_more_than_255_parents() {
    let (_root, layout) = new_root();
    let mut index = open(&layout);
    append_all(&mut index, &[spread_meta(0)]);
    let before = column_lens(&layout);
    let too_many = meta(id_of(1), 2, [0; 32], vec![spread_id(0); 256]);
    match index.append(&too_many) {
        Err(StoreError::Limit { what, max, got }) => {
            assert_eq!((what, max, got), (LimitKind::Parents, 255, 256));
        }
        other => panic!("256 parents: expected Limit, got {other:?}"),
    }
    assert_eq!(column_lens(&layout), before, "nothing written");
    assert_eq!(index.len(), 1);
    let most = meta(id_of(1), 2, [0; 32], vec![spread_id(0); 255]);
    assert_eq!(index.append(&most).unwrap(), 1);
    assert_eq!(index.get(1).unwrap(), at_seq(&most, 1));
}

#[test]
fn a_stale_handle_appends_at_the_real_end() {
    // Writers take turns under meta/lock, each with its own handle (often in its own
    // process), so a handle may be behind the files when its turn comes.
    let (_root, layout) = new_root();
    let mut first = open(&layout);
    let mut second = open(&layout);
    let metas: Vec<ChangesetMeta> = (0..4).map(spread_meta).collect();
    assert_eq!(first.append(&metas[0]).unwrap(), 0);
    assert_eq!(second.append(&metas[1]).unwrap(), 1);
    assert_eq!(second.append(&metas[2]).unwrap(), 2);
    assert_eq!(first.append(&metas[3]).unwrap(), 3);
    assert_eq!(first.len(), 4);
    first.sync().unwrap();
    let reader = open(&layout);
    for (rec, meta) in (0..).zip(&metas) {
        assert_eq!(reader.get(rec).unwrap(), at_seq(meta, rec));
        assert_eq!(reader.find(meta.id).unwrap(), Some(rec));
    }
    // Compaction through the stale handle covers the records it has not seen as well.
    assert_eq!(second.compact().unwrap(), 4);
    assert_eq!(open(&layout).sorted_count(), 4);
}

/// A record, then three a batch appends after it, each with parents (one, a merge's two, one),
/// so that the batch starts past rec 0 and past the first record's parents in `parents.log`.
fn batch_metas() -> (ChangesetMeta, [ChangesetMeta; 3]) {
    let before = meta(id_of(9), 2, [0xd9; 32], vec![id_of(8)]);
    let batch = [
        meta(id_of(1), 3, [0xd1; 32], vec![id_of(9)]),
        meta(id_of(2), 4, [0xd2; 32], vec![id_of(1), id_of(9)]),
        meta(id_of(3), 5, [0xd3; 32], vec![id_of(2)]),
    ];
    (before, batch)
}

/// Where the parents of [`batch_metas`]' records end in `parents.log`: the first record's,
/// then each of the batch's.
const BATCH_PARENTS_END: [u64; 4] = [32, 64, 128, 160];

/// A fresh store root with an empty index, every handle on it from [`no_sync`]: the tests that
/// lay out many roots pay for no flush.
fn new_no_sync_root() -> (TempDir, Layout) {
    let (dir, layout) = bare_root();
    Index::create(&layout, Arc::new(NoSyncFs)).expect("create the index files");
    (dir, layout)
}

/// An index handle on `layout` whose changes flush nothing.
fn no_sync(layout: &Layout) -> Index {
    Index::open(layout, Arc::new(NoSyncFs)).unwrap()
}

#[test]
fn append_batch_appends_each_index_file_once() {
    let (before, batch) = batch_metas();
    let (_root, layout) = new_root();
    append_all(&mut open(&layout), std::slice::from_ref(&before));
    let (recording, calls) = recording_fs();
    let mut index = Index::open(&layout, recording).unwrap();
    assert_eq!(index.append_batch(&batch).unwrap(), 1, "the first new rec");
    assert_eq!(
        calls(),
        [
            "append parents.log",
            "append par.col",
            "append gen.col",
            "append dh.col",
            "append ids.col",
        ],
        "one append to each file, in write order, for the three records"
    );
    assert_eq!(index.len(), 4);

    // The bytes three appends write, file by file, on another store.
    let (_singles_root, singles) = new_root();
    append_all(&mut open(&singles), std::slice::from_ref(&before));
    append_all(&mut open(&singles), &batch);
    for (rel, _) in COLUMNS {
        let read = |layout: &Layout| fs::read(layout.meta_path(rel)).unwrap();
        assert_eq!(read(&layout), read(&singles), "{rel}");
    }
    // Each record reads back, from the handle that appended it and from a fresh one.
    let reopened = open(&layout);
    for handle in [&index, &reopened] {
        assert_eq!(handle.get(0).unwrap(), at_seq(&before, 0));
        for (rec, meta) in (1..).zip(&batch) {
            assert_eq!(handle.get(rec).unwrap(), at_seq(meta, rec));
            assert_eq!(handle.find(meta.id).unwrap(), Some(rec));
        }
    }
}

/// A batch is refused whole, before any call: a record of more than 255 parents anywhere in it
/// is refused as `append` refuses one. An empty batch makes no call either: it returns the rec
/// the next record takes.
#[test]
fn append_batch_writes_nothing_for_a_batch_it_refuses() {
    let (before, batch) = batch_metas();
    let (_root, layout) = new_root();
    append_all(&mut open(&layout), std::slice::from_ref(&before));
    let lens = column_lens(&layout);
    let (recording, calls) = recording_fs();
    let mut index = Index::open(&layout, recording).unwrap();
    let mut refused = batch.to_vec();
    refused.push(meta(id_of(4), 6, [0xd4; 32], vec![spread_id(0); 256]));
    match index.append_batch(&refused) {
        Err(StoreError::Limit { what, max, got }) => {
            assert_eq!((what, max, got), (LimitKind::Parents, 255, 256));
        }
        other => panic!("256 parents: expected Limit, got {other:?}"),
    }
    assert_eq!(calls(), Vec::<String>::new(), "no call");
    assert_eq!(column_lens(&layout), lens, "nothing written");
    assert_eq!(index.len(), 1);
    assert_eq!(
        index.append_batch(&[]).unwrap(),
        1,
        "the rec the next record takes"
    );
    assert_eq!(
        calls(),
        Vec::<String>::new(),
        "an empty batch makes no call"
    );
    assert_eq!(index.append_batch(&batch).unwrap(), 1);
    assert_eq!(index.len(), 4);
}

/// What a crash in the middle of a batch's appends leaves, or a power loss before their sync:
/// any one of the five files short, cut at a record's boundary or inside a record. Readers
/// count the records that all five hold whole; `repair_tail` cuts the rest off (it returns the
/// record slots it cut from the longest column), as after a single append; and the rest of the
/// batch, appended by a handle opened before it, lands where the whole records end: that handle
/// catches up first, cutting what is not whole if no repair did.
#[test]
fn an_append_batch_cut_anywhere_counts_its_whole_prefix() {
    let (before, batch) = batch_metas();
    for (rel, width) in COLUMNS {
        // The file's length with the first record and k of the batch's, k = 0..=3.
        let bounds: Vec<u64> = (0..4)
            .map(|k| match rel {
                "parents.log" => BATCH_PARENTS_END[k],
                _ => width * (1 + to_u64(k)),
            })
            .collect();
        // Each boundary, and between two of them: k records whole, and part of the next.
        let mut cuts: Vec<(u64, usize)> = bounds.iter().copied().zip(0..).collect();
        cuts.extend((0..3).map(|k| (bounds[k].midpoint(bounds[k + 1]), k)));
        for (len, k) in cuts {
            for repair_first in [true, false] {
                let what = format!("{rel} cut to {len} bytes, repair first: {repair_first}");
                let (_root, layout) = new_no_sync_root();
                let mut index = no_sync(&layout);
                index.append_batch(std::slice::from_ref(&before)).unwrap();
                let mut stale = no_sync(&layout);
                index.append_batch(&batch).unwrap();
                let whole = column_lens(&layout);
                set_len(layout.meta_path(rel), len);
                let records = 1 + to_u64(k);
                assert_eq!(open_len(&layout).unwrap(), records, "{what}: a reader");
                if repair_first {
                    let dropped = no_sync(&layout).repair_tail().unwrap();
                    assert_eq!(dropped, 3 - to_u64(k), "{what}: dropped");
                    let kept = [
                        BATCH_PARENTS_END[k],
                        8 * records,
                        8 * records,
                        32 * records,
                        32 * records,
                    ];
                    assert_eq!(column_lens(&layout), kept, "{what}: cut");
                    assert_eq!(open_len(&layout).unwrap(), records, "{what}: reopened");
                }
                let rest = stale.append_batch(&batch[k..]).unwrap();
                assert_eq!(rest, records, "{what}: where the rest lands");
                assert_eq!(column_lens(&layout), whole, "{what}: whole again");
                let after = open(&layout);
                assert_eq!(after.len(), 4, "{what}");
                for (rec, meta) in (1..).zip(&batch) {
                    assert_eq!(after.get(rec).unwrap(), at_seq(meta, rec), "{what}");
                }
            }
        }
    }
}

#[test]
fn real_fs_appends_without_syncing_and_renames_durably() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("f");
    assert_eq!(
        RealFs::new().append(&file, b"abc").unwrap(),
        0,
        "creates the file"
    );
    assert_eq!(
        RealFs::new().append(&file, b"de").unwrap(),
        3,
        "where the bytes start"
    );
    assert_eq!(RealFs::new().append(&file, b"").unwrap(), 5);
    assert_eq!(fs::read(&file).unwrap(), b"abcde");

    let target = dir.path().join("g");
    fs::write(&target, b"old").unwrap();
    RealFs::new().rename_durable(&file, &target).unwrap();
    assert_eq!(fs::read(&target).unwrap(), b"abcde", "replaces the target");
    assert!(!file.exists());
    let missing = RealFs::new().rename_durable(&file, &target);
    assert!(
        matches!(&missing, Err(e) if e.kind() == io::ErrorKind::NotFound),
        "{missing:?}"
    );

    // CrashAtFs (k = 0 never aborts) and Arc forward both.
    #[cfg(feature = "test-hooks")]
    {
        let fs: Arc<dyn FsOps> = Arc::new(CrashAtFs::new(RealFs::new(), 0));
        assert_eq!(fs.append(&file, b"xy").unwrap(), 0);
        fs.rename_durable(&file, &target).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"xy");
    }
}

/// The records of the crash sweep: five compacted before the child runs, three it appends.
#[cfg(feature = "test-hooks")]
fn crash_metas() -> Vec<ChangesetMeta> {
    (0..8_u64)
        .map(|n| {
            let parents = n.checked_sub(1).map(spread_id).into_iter().collect();
            meta(
                spread_id(n),
                n + 1,
                [u8::try_from(n % 3).unwrap(); 32],
                parents,
            )
        })
        .collect()
}

/// One writer's work in a child process, through a `CrashAtFs` that aborts on its k-th
/// FsOps call, under the order checker: repair, append three records, sync, compact.
#[cfg(feature = "test-hooks")]
fn crash_child(root: OsString) {
    let k = std::env::var(CRASH_AT_ENV).unwrap().parse().unwrap();
    let layout = Layout::new(Utf8PathBuf::from_path_buf(root.into()).unwrap());
    let fs = checked(CrashAtFs::new(NoSyncFs, k));
    let mut index = Index::open(&layout, fs).unwrap();
    index.repair_tail().unwrap();
    for meta in &crash_metas()[5..] {
        index.append(meta).unwrap();
    }
    index.sync().unwrap();
    index.compact().unwrap();
}

#[test]
#[cfg(feature = "test-hooks")]
fn crash_at_any_index_write_recovers_to_whole_records() {
    if let Some(root) = std::env::var_os(CRASH_CHILD_ROOT) {
        crash_child(root);
        return;
    }
    // k = CRASH_CHILD_CALLS + 1 is past the child's last call: it finishes.
    let metas = crash_metas();
    for k in 1..=CRASH_CHILD_CALLS + 1 {
        let (_root, layout) = new_root();
        let mut index = Index::open(&layout, Arc::new(NoSyncFs)).unwrap();
        append_all(&mut index, &metas[..5]);
        index.compact().unwrap();
        drop(index);

        let child = run(this_test(&test_name!(
            "crash_at_any_index_write_recovers_to_whole_records"
        ))
        .env(CRASH_CHILD_ROOT, layout.root())
        .env(CRASH_AT_ENV, k.to_string()));
        let ended_as_expected = if k <= CRASH_CHILD_CALLS {
            child.status.signal() == Some(SIGABRT)
        } else {
            child.status.success()
        };
        assert!(
            ended_as_expected,
            "k={k}: {:?}\n{}",
            child.status,
            String::from_utf8_lossy(&child.stderr)
        );

        // Readers see a prefix of the records, each whole.
        let reader = open(&layout);
        let have = reader.len();
        assert!((5..=8).contains(&have), "k={k}: {have} records");
        for (rec, meta) in (0..).zip(&metas) {
            let want = (rec < have).then_some(rec);
            assert_eq!(reader.find(meta.id).unwrap(), want, "k={k}");
            if rec < have {
                assert_eq!(reader.get(rec).unwrap(), at_seq(meta, rec), "k={k}");
            }
        }
        // The next writer repairs, appends what is missing (as replay would) and compacts,
        // under the order checker as the crashed one was.
        let mut writer = Index::open(&layout, checked(RealFs::new())).unwrap();
        writer.repair_tail().unwrap();
        assert_eq!(writer.len(), have, "k={k}");
        append_all(&mut writer, &metas[usize::try_from(have).unwrap()..]);
        assert_eq!(writer.compact().unwrap(), 8, "k={k}");
        let after = open(&layout);
        for (rec, meta) in (0..).zip(&metas) {
            assert_eq!(after.find(meta.id).unwrap(), Some(rec), "k={k}");
            assert_eq!(after.get(rec).unwrap(), at_seq(meta, rec), "k={k}");
        }
        assert_eq!(after.find_delta_hash(&[2; 32]).unwrap(), [2, 5], "k={k}");
    }
}

/// The most entries one read of an index file may take: the store's `READ_WINDOW` (the M6
/// design §5).
const READ_WINDOW: u64 = 4_096;

/// The most recs a lookup by id prefix or delta hash returns: the store's `MAX_LOOKUP_HITS`.
const LOOKUP_HITS: usize = 4_096;

/// Asserts that no read of `index`'s files has taken more than [`READ_WINDOW`] entries.
#[track_caller]
fn assert_windowed(index: &Index, what: &str) {
    let widest = index.widest_read();
    assert!(
        widest <= READ_WINDOW,
        "{what}: a read of {widest} entries, past the window of {READ_WINDOW}"
    );
}

/// The process's peak resident memory, in KiB: `VmHWM` of `/proc/self/status` on Linux (nextest
/// runs each test in a process of its own); 0 elsewhere, where the check that uses it checks
/// nothing.
fn peak_resident_kib() -> u64 {
    let status = fs::read_to_string("/proc/self/status").unwrap_or_default();
    status
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))
        .and_then(|kib| kib.trim().trim_end_matches("kB").trim().parse().ok())
        .unwrap_or(0)
}

/// A column padded to 4 GiB: no read takes more than a window, and nothing the size of a
/// column is held, the peak resident memory growing by less than a quarter of par.col's 1 GiB,
/// where one read of it, or of the tail of ids.col, would hold all of it.
#[test]
fn a_column_padded_to_4_gib_opens_and_answers_in_windows() {
    // Three records compacted into the views, then every column padded with zeros, sparse, to
    // 2^27 records: ids.col and dh.col are 4 GiB. With no journal to vouch for them, an open
    // counts them by their lengths, and zeroed par.col entries keep the layout (no parents).
    // Record 3, the first past the views, gets an id of its own.
    const PADDED: u64 = 1 << 27;
    const MAX_GROWTH_KIB: u64 = 256 << 10;
    let (_root, layout) = new_root();
    let mut index = open(&layout);
    let metas = [1, 2, 3].map(|n| meta(id_of(n), 1, [n; 32], vec![]));
    append_all(&mut index, &metas);
    index.compact().unwrap();
    for (rel, width) in &COLUMNS[1..] {
        set_len(layout.meta_path(rel), PADDED * width);
    }
    assert_eq!(file_len(layout.meta_path("idx/ids.col")), 4 << 30);
    write_at(layout.meta_path("idx/ids.col"), 3 * 32, &id_of(7).0.0);
    let before = peak_resident_kib();

    // 1. The open checks every par.col entry, a window at a time.
    let padded = open(&layout);
    assert_eq!(padded.len(), PADDED);
    assert_windowed(&padded, "open");

    // 2. Lookups: from the views, and cut at the cap in the tail. (A scan of the whole tail,
    //    as a miss makes, takes as long as the open again: `lookups_cut_at_the_cap_and_say_so`
    //    windows one.)
    assert_eq!(padded.find(id_of(2)).unwrap(), Some(1), "from the view");
    assert_eq!(
        padded.find(id_of(7)).unwrap(),
        Some(3),
        "in the tail's first window"
    );
    let zeros: Vec<u64> = (4..).take(LOOKUP_HITS).collect();
    let by_prefix = padded.find_prefix_hits("00").unwrap();
    assert_eq!((by_prefix.recs, by_prefix.truncated), (zeros.clone(), true));
    let by_dh = padded.find_delta_hash_hits(&[0; 32]).unwrap();
    let zero_dh: Vec<u64> = (3..).take(LOOKUP_HITS).collect();
    assert_eq!((by_dh.recs, by_dh.truncated), (zero_dh, true));
    let last = meta(cs([0; 32]), 0, [0; 32], vec![]);
    assert_eq!(padded.get(PADDED - 1).unwrap(), at_seq(&last, PADDED - 1));
    assert_windowed(&padded, "lookups");
    let growth = peak_resident_kib().saturating_sub(before);
    assert!(
        growth < MAX_GROWTH_KIB,
        "the peak resident memory grew by {growth} KiB"
    );
}

/// Writes `bytes` into the existing file at `path` at byte `offset`.
fn write_at(path: impl AsRef<std::path::Path>, offset: u64, bytes: &[u8]) {
    use std::os::unix::fs::FileExt as _;

    let file = fs::OpenOptions::new().write(true).open(path).unwrap();
    file.write_all_at(bytes, offset).unwrap();
}

/// How many records [`lookups_cut_at_the_cap_and_say_so`] puts in the sorted views, and how
/// many after them in the tail; every one matches.
const SPLITS: [(u64, u64); 7] = [
    (5_000, 0),
    (0, 5_000),
    (3_000, 2_000),
    (4_096, 0),
    (0, 4_096),
    (2_000, 2_096),
    (2_000, 2_097),
];

#[test]
fn lookups_cut_at_the_cap_and_say_so() {
    // Ids sharing the prefix abcdef and one delta hash, among two that match neither.
    let shared = [0xab, 0xcd, 0xef];
    let change = [0xc1; 32];
    let matching = |n: u64| meta(id_with_prefix(&shared, 0x10, n), 1, change, vec![]);
    for (in_view, in_tail) in SPLITS {
        let what = format!("{in_view} in the views, {in_tail} in the tail");
        let total = in_view + in_tail;
        let (_root, layout) = new_root();
        let mut index = open(&layout);
        let view: Vec<ChangesetMeta> = (0..in_view).map(matching).collect();
        let tail: Vec<ChangesetMeta> = (in_view..total).map(matching).collect();
        let others = [spread_meta(total), spread_meta(total + 1)];
        index.append_batch(&view).unwrap();
        index.append_batch(&others).unwrap();
        index.compact().unwrap();
        index.append_batch(&tail).unwrap();
        index.sync().unwrap();
        assert_windowed(&index, &format!("{what}: the compaction"));

        // Every answer holds the cap's worth of distinct matches, ascending, and says whether
        // more matched; the lookups without the flag give the same recs.
        let want_len = usize::try_from(total).unwrap().min(LOOKUP_HITS);
        let cut = usize::try_from(total).unwrap() > LOOKUP_HITS;
        let reopened = open(&layout);
        for handle in [&index, &reopened] {
            let by_prefix = handle.find_prefix_hits("abcdef").unwrap();
            let by_dh = handle.find_delta_hash_hits(&change).unwrap();
            for (how, hits) in [("prefix", &by_prefix), ("delta hash", &by_dh)] {
                assert_eq!(
                    (hits.recs.len(), hits.truncated),
                    (want_len, cut),
                    "{what}: {how}"
                );
                assert!(hits.recs.is_sorted(), "{what}: {how}");
                assert!(hits.recs.windows(2).all(|pair| pair[0] < pair[1]), "{what}");
                for &rec in &hits.recs {
                    let got = handle.get(rec).unwrap();
                    assert_eq!(got.id.0.0[..3], shared, "{what}: {how}, rec {rec}");
                }
            }
            assert_eq!(
                handle.find_prefix("abcdef").unwrap(),
                by_prefix.recs,
                "{what}"
            );
            assert_eq!(
                handle.find_delta_hash(&change).unwrap(),
                by_dh.recs,
                "{what}"
            );

            // A run past the cap: its keys are read to one past the cap, 64 per read after the
            // search, and its recs in one read of a window.
            if in_view > to_u64(LOOKUP_HITS) {
                let view = handle.ids_view();
                let mut low = [0; 32];
                low[..3].copy_from_slice(&shared);
                let probes = view.probes();
                view.lower_bound(&low).unwrap();
                let search = view.probes() - probes;
                handle.find_prefix_hits("abcdef").unwrap();
                let run = (to_u64(LOOKUP_HITS) + 1).div_ceil(64);
                assert_eq!(view.probes() - probes, 2 * search + run, "{what}");
                assert_eq!(view.widest_read(), READ_WINDOW, "{what}");
            }

            // The view's run fills the cap first, then the tail's recs, in order.
            if in_view <= to_u64(LOOKUP_HITS) {
                let mut want: Vec<u64> = (0..in_view).collect();
                want.extend((in_view + 2..).take(want_len - want.len()));
                assert_eq!(by_prefix.recs, want, "{what}");
                assert_eq!(by_dh.recs, want, "{what}");
            }
            assert_eq!(handle.find(spread_id(total + 2)).unwrap(), None, "{what}");
            assert_windowed(handle, &what);
        }
    }
}

#[test]
fn vouching_for_a_long_tail_reads_in_windows() {
    use swarmops_store::{Journal, MetaLock, Record};
    use swarmops_testkit::store::temp_dir;

    // 5,000 changesets past the checkpoint in one journal frame, and as many records in the
    // index: an open vouches for them against the frame, reading the columns a window at a
    // time.
    let (_dir, root) = temp_dir();
    let layout = Layout::new(&root);
    MetaStore::create_with_fs(&root, Arc::new(NoSyncFs)).unwrap();
    let metas: Vec<ChangesetMeta> = (0..5_000).map(spread_meta).collect();
    {
        let _lock = MetaLock::acquire(&layout, Duration::from_secs(10)).unwrap();
        let records: Vec<Record> = (metas.iter())
            .map(|meta| Record::Changeset {
                id: meta.id,
                generation: meta.generation,
                delta_hash: meta.delta_hash,
                parents: meta.parents.clone(),
            })
            .collect();
        Journal::open(&layout, real_fs())
            .unwrap()
            .append_frame(&records)
            .unwrap();
        let mut index = open(&layout);
        index.append_batch(&metas).unwrap();
        index.sync().unwrap();
    }
    let vouched = open(&layout);
    assert_eq!(vouched.len(), 5_000, "every record vouched for");
    assert_eq!(
        vouched.widest_read(),
        READ_WINDOW,
        "the first window of each column"
    );
}

#[test]
fn resolve_prefix_past_the_cap_is_still_ambiguous() {
    use swarmops_testkit::store::temp_dir;

    // 5,000 changesets sharing a 12-hex prefix: the answer is cut at the cap, and still holds
    // more than one id, which is what makes a prefix ambiguous.
    let shared = [0xab, 0xcd, 0xef, 0x01, 0x23, 0x45];
    let (_dir, root) = temp_dir();
    let store = MetaStore::create_with_fs(&root, Arc::new(NoSyncFs)).unwrap();
    let mut write = store.write().unwrap();
    for n in 0..5_000 {
        let id = id_with_prefix(&shared, 0x10, n);
        write
            .insert_changeset(meta(id, 1, [0; 32], vec![]))
            .unwrap();
    }
    write.commit().unwrap();
    let found = store
        .read()
        .unwrap()
        .resolve_prefix("abcdef012345")
        .unwrap();
    assert_eq!(found.len(), LOOKUP_HITS);
    assert!(
        found
            .iter()
            .all(|id| id.0.to_hex().starts_with("abcdef012345"))
    );
}
