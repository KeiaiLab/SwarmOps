//! Columnar changeset index: the column files and their byte layout, the sorted key views,
//! lookups that combine a view with the unsorted tail, compaction, and recovery from a crash
//! in the middle of an append or of a compaction.
//!
//! Every test owns a fresh store root (`tempfile::tempdir()`), so tests are independent and
//! may run in any order or in parallel.

use std::ffi::OsString;
use std::fmt::Debug;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::num::NonZero;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use camino::Utf8PathBuf;
use proptest::prelude::*;
use swarmops_core::{ChangesetId, ObjectId};
use swarmops_store::{
    CRASH_AT_ENV, ChangesetMeta, CrashAtFs, FsOps, Index, Layout, LinkOutcome, RealFs, StoreError,
};
use tempfile::TempDir;

const SIGABRT: i32 = 6;

/// Set only in the children that `crash_at_any_index_write_recovers_to_whole_records`
/// spawns: the store root.
const CRASH_CHILD_ROOT: &str = "SWARMOPS_TEST_INDEX_CRASH_ROOT";

/// The FsOps calls of [`crash_child`]: none for its `repair_tail` (the files are clean),
/// 3 appends of 5 calls each (1-15), the sync (16-20), then the compaction: 5 sync_data of
/// the columns (21-25), per view 2 temp removes and 2 temp appends (26-33), 4 temp syncs
/// (34-37) and 4 renames (38-41).
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

/// A fresh, empty store root and its layout; the directory lives as long as the `TempDir`.
fn new_root() -> (TempDir, Layout) {
    let dir = tempfile::tempdir().expect("create store root");
    let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("temp dir is UTF-8");
    (dir, Layout::new(root))
}

fn real_fs() -> Arc<dyn FsOps> {
    Arc::new(RealFs)
}

fn open(layout: &Layout) -> Index {
    Index::open(layout, real_fs()).unwrap()
}

/// What opening `layout`'s index gives, the handle shown by its length.
fn open_len(layout: &Layout) -> Result<u64, StoreError> {
    Index::open(layout, real_fs()).map(|index| index.len())
}

fn cs(bytes: [u8; 32]) -> ChangesetId {
    ChangesetId(ObjectId(bytes))
}

fn id_of(byte: u8) -> ChangesetId {
    cs([byte; 32])
}

fn to_u64(n: usize) -> u64 {
    u64::try_from(n).unwrap()
}

/// A changeset to append. `seq` is left 0: the index numbers its records itself.
fn meta(
    id: ChangesetId,
    generation: u64,
    delta_hash: [u8; 32],
    parents: Vec<ChangesetId>,
) -> ChangesetMeta {
    ChangesetMeta {
        id,
        seq: 0,
        generation,
        delta_hash,
        parents,
    }
}

/// `meta` as the index returns it for record `seq`.
fn at_seq(meta: &ChangesetMeta, seq: u64) -> ChangesetMeta {
    ChangesetMeta {
        seq,
        ..meta.clone()
    }
}

/// SplitMix64's output function: a bijection on `u64` that scatters its input.
fn mix(n: u64) -> u64 {
    let mut z = n.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
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
    ObjectId(*bytes).to_hex()
}

fn file_len(path: impl AsRef<Path>) -> u64 {
    fs::metadata(path).unwrap().len()
}

fn append_bytes(path: impl AsRef<Path>, bytes: &[u8]) {
    let mut file = OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(bytes).unwrap();
}

fn set_len(path: impl AsRef<Path>, len: u64) {
    OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_len(len)
        .unwrap();
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

#[track_caller]
fn assert_invalid_input<T: Debug>(got: Result<T, StoreError>, what: &str) {
    match got {
        Err(StoreError::Io(e)) if e.kind() == io::ErrorKind::InvalidInput => {}
        other => panic!("{what}: expected InvalidInput, got {other:?}"),
    }
}

/// [`RealFs`], recording each call as `"<method> <file name>…"` when `calls` is set, and
/// skipping every flush to stable storage when `durable` is false: for tests whose subject
/// is what the files hold, which a process crash or a skipped fsync does not change.
struct TestFs {
    calls: Option<Arc<Mutex<Vec<String>>>>,
    durable: bool,
}

impl TestFs {
    fn record(&self, method: &str, paths: &[&Path]) {
        if let Some(calls) = &self.calls {
            let mut call = method.to_owned();
            for path in paths {
                call.push(' ');
                call.push_str(&path.file_name().unwrap().to_string_lossy());
            }
            calls.lock().unwrap().push(call);
        }
    }
}

/// Real files, every FsOps call recorded, and a function that returns the calls made since
/// it last ran.
fn recording_fs() -> (Arc<dyn FsOps>, impl Fn() -> Vec<String>) {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let fs = TestFs {
        calls: Some(Arc::clone(&calls)),
        durable: true,
    };
    (Arc::new(fs), move || {
        std::mem::take(&mut *calls.lock().unwrap())
    })
}

/// Real files, no fsyncs.
fn fast_fs() -> TestFs {
    TestFs {
        calls: None,
        durable: false,
    }
}

impl FsOps for TestFs {
    fn create_dir_all(&self, p: &Path) -> io::Result<()> {
        self.record("create_dir_all", &[p]);
        RealFs.create_dir_all(p)
    }

    fn write_durable_temp(&self, dir: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
        self.record("write_durable_temp", &[dir]);
        RealFs.write_durable_temp(dir, bytes)
    }

    fn link_noclobber(&self, from: &Path, to: &Path) -> io::Result<LinkOutcome> {
        self.record("link_noclobber", &[from, to]);
        RealFs.link_noclobber(from, to)
    }

    fn fsync_dir(&self, p: &Path) -> io::Result<()> {
        self.record("fsync_dir", &[p]);
        if self.durable {
            RealFs.fsync_dir(p)
        } else {
            Ok(())
        }
    }

    fn read(&self, p: &Path) -> io::Result<Option<Vec<u8>>> {
        self.record("read", &[p]);
        RealFs.read(p)
    }

    fn exists(&self, p: &Path) -> io::Result<bool> {
        self.record("exists", &[p]);
        RealFs.exists(p)
    }

    fn remove(&self, p: &Path) -> io::Result<()> {
        self.record("remove", &[p]);
        RealFs.remove(p)
    }

    fn append_durable(&self, path: &Path, bytes: &[u8]) -> io::Result<u64> {
        self.record("append_durable", &[path]);
        if self.durable {
            RealFs.append_durable(path, bytes)
        } else {
            RealFs.append(path, bytes)
        }
    }

    fn file_len(&self, path: &Path) -> io::Result<u64> {
        self.record("file_len", &[path]);
        RealFs.file_len(path)
    }

    fn truncate(&self, path: &Path, len: u64) -> io::Result<()> {
        self.record("truncate", &[path]);
        if self.durable {
            RealFs.truncate(path, len)
        } else {
            OpenOptions::new().write(true).open(path)?.set_len(len)
        }
    }

    fn atomic_replace(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        self.record("atomic_replace", &[path]);
        RealFs.atomic_replace(path, bytes)
    }

    fn sync_data(&self, path: &Path) -> io::Result<()> {
        self.record("sync_data", &[path]);
        if self.durable {
            RealFs.sync_data(path)
        } else {
            Ok(())
        }
    }

    fn append(&self, path: &Path, bytes: &[u8]) -> io::Result<u64> {
        self.record("append", &[path]);
        RealFs.append(path, bytes)
    }

    fn rename_durable(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.record("rename_durable", &[from, to]);
        if self.durable {
            RealFs.rename_durable(from, to)
        } else {
            fs::rename(from, to)
        }
    }
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
    let late = ChangesetMeta {
        seq: 77,
        ..meta(id_of(5), 2, [0xd5; 32], vec![id_of(1)])
    };
    assert_eq!(index.append(&late).unwrap(), 4);
    assert_eq!(index.get(4).unwrap(), at_seq(&late, 4));
    assert_invalid_input(index.get(5), "a rec past the end");
    assert_invalid_input(index.get(u64::MAX), "u64::MAX");
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
        assert_invalid_input(index.find_prefix(bad), bad);
    }
}

#[test]
fn find_delta_hash_groups_rebases() {
    // A rebase that applies cleanly keeps the delta hash, so the original changeset and the
    // rebased one share it: one in the sorted view, one in the tail, then both sorted.
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
        let fast = || -> Arc<dyn FsOps> { Arc::new(fast_fs()) };
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

    // ⌈log2 10,000⌉ = 14: a binary search over the key column reads at most 14 keys, and
    // never the rec column.
    let mut probes = vec![[0; 32], [0xff; 32], keys[0], keys[9_999]];
    probes.extend(keys.iter().step_by(101).copied());
    probes.extend((10_000..10_100).map(|n| spread_id(n).0.0));
    for probe in &probes {
        let (keys_read, recs_read) = (view.probes(), view.rec_reads());
        let at = view.lower_bound(probe).unwrap();
        assert!(
            view.probes() - keys_read <= 14,
            "{} probes",
            view.probes() - keys_read
        );
        assert_eq!(view.rec_reads(), recs_read, "lower_bound reads keys only");
        let want = keys.partition_point(|key| key < probe);
        assert_eq!(at, to_u64(want), "{}", hex(probe));
        if want < keys.len() {
            assert_eq!(view.key_at(at).unwrap(), keys[want]);
        }
    }

    // A whole lookup: the search, one more key to compare, then one rec for a hit.
    for (rec, meta) in (0..).zip(&metas).step_by(97) {
        let (keys_read, recs_read) = (view.probes(), view.rec_reads());
        assert_eq!(index.find(meta.id).unwrap(), Some(rec));
        assert!(view.probes() - keys_read <= 15);
        assert_eq!(view.rec_reads() - recs_read, 1, "one rec_at per hit");
    }
    for n in 10_000..10_100 {
        let (keys_read, recs_read) = (view.probes(), view.rec_reads());
        assert_eq!(index.find(spread_id(n)).unwrap(), None);
        assert!(view.probes() - keys_read <= 15);
        assert_eq!(view.rec_reads(), recs_read, "no rec_at for a miss");
    }
}

/// Nearest-rank percentile over sorted samples.
fn percentile(sorted: &[Duration], percent: usize) -> Duration {
    let rank = (sorted.len() * percent).div_ceil(100).max(1);
    sorted[rank - 1]
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
fn open_creates_the_index_files_and_then_reads_sizes_only() {
    let (_root, layout) = new_root();
    fs::create_dir(layout.meta_dir()).unwrap(); // as Journal::create leaves it
    let (fs, calls) = recording_fs();
    let index = Index::open(&layout, Arc::clone(&fs)).unwrap();
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
            "fsync_dir idx",
            "fsync_dir meta",
        ]
    );
    for (rel, _) in COLUMNS {
        assert_eq!(file_len(layout.meta_path(rel)), 0, "{rel}");
    }
    assert!(layout.meta_path("sorted").is_dir());

    let again = Index::open(&layout, fs).unwrap();
    assert_eq!(again.len(), 0);
    assert_eq!(calls(), Vec::<String>::new(), "no FsOps call");
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
    let mut index = Index::open(&layout, Arc::new(fast_fs())).unwrap();
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
                    let index = Index::open(layout, Arc::new(fast_fs())).unwrap();
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
    assert_invalid_input(index.append(&too_many), "256 parents");
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

#[test]
fn real_fs_appends_without_syncing_and_renames_durably() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("f");
    assert_eq!(RealFs.append(&file, b"abc").unwrap(), 0, "creates the file");
    assert_eq!(
        RealFs.append(&file, b"de").unwrap(),
        3,
        "where the bytes start"
    );
    assert_eq!(RealFs.append(&file, b"").unwrap(), 5);
    assert_eq!(fs::read(&file).unwrap(), b"abcde");

    let target = dir.path().join("g");
    fs::write(&target, b"old").unwrap();
    RealFs.rename_durable(&file, &target).unwrap();
    assert_eq!(fs::read(&target).unwrap(), b"abcde", "replaces the target");
    assert!(!file.exists());
    let missing = RealFs.rename_durable(&file, &target);
    assert!(
        matches!(&missing, Err(e) if e.kind() == io::ErrorKind::NotFound),
        "{missing:?}"
    );

    // CrashAtFs (k = 0 never aborts) and Arc forward both.
    let fs: Arc<dyn FsOps> = Arc::new(CrashAtFs::new(RealFs, 0));
    assert_eq!(fs.append(&file, b"xy").unwrap(), 0);
    fs.rename_durable(&file, &target).unwrap();
    assert_eq!(fs::read(&target).unwrap(), b"xy");
}

/// The records of the crash sweep: five compacted before the child runs, three it appends.
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
/// FsOps call: repair, append three records, sync, compact.
fn crash_child(root: OsString) {
    let k = std::env::var(CRASH_AT_ENV).unwrap().parse().unwrap();
    let layout = Layout::new(Utf8PathBuf::from_path_buf(root.into()).unwrap());
    let fs: Arc<dyn FsOps> = Arc::new(CrashAtFs::new(fast_fs(), k));
    let mut index = Index::open(&layout, fs).unwrap();
    index.repair_tail().unwrap();
    for meta in &crash_metas()[5..] {
        index.append(meta).unwrap();
    }
    index.sync().unwrap();
    index.compact().unwrap();
}

#[test]
fn crash_at_any_index_write_recovers_to_whole_records() {
    if let Some(root) = std::env::var_os(CRASH_CHILD_ROOT) {
        crash_child(root);
        return;
    }
    // k = CRASH_CHILD_CALLS + 1 is past the child's last call: it finishes.
    let metas = crash_metas();
    for k in 1..=CRASH_CHILD_CALLS + 1 {
        let (_root, layout) = new_root();
        let mut index = Index::open(&layout, Arc::new(fast_fs())).unwrap();
        append_all(&mut index, &metas[..5]);
        index.compact().unwrap();
        drop(index);

        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "crash_at_any_index_write_recovers_to_whole_records",
            ])
            .arg("--nocapture")
            .env(CRASH_CHILD_ROOT, layout.root())
            .env(CRASH_AT_ENV, k.to_string())
            .output()
            .unwrap();
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
        // The next writer repairs, appends what is missing (as replay would) and compacts.
        let mut writer = open(&layout);
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
