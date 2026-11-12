//! FORMAT 3's feature list (the FORMAT 3 design §2.6): a store that lists a feature this build
//! does not support is refused by its open, its reads and its writes, the write queue's leads
//! among them, with nothing written; a fact's feature is durable in the list before the frame
//! that holds it; decoding takes the build's kinds, never the list, so a reader that races an
//! enable reads right; a record of a kind the build does not know is damage unless the list,
//! read again, names a feature the build lacks.
//!
//! The stores are made with `MetaStore::create_in` of a FORMAT 3 layout: no `init` makes one yet.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use swarmops_core::{ChangesetId, ObjectKind, hash_object};
use swarmops_store::{
    ChangesetMeta, Layout, MAX_FEATURES, MetaStore, SUPPORTED_FEATURES, Serve, StoreError,
    StoreFormat,
};
use swarmops_testkit::store::{new_root, real_fs};
use tempfile::TempDir;
#[cfg(feature = "test-hooks")]
use {
    std::sync::Arc,
    std::sync::atomic::{AtomicBool, Ordering},
    std::time::Duration,
    swarmops_store::{Fact, FactKind, Journal, MetaLock, Record, seed_nonces, supported_for_tests},
    swarmops_testkit::fs::{RecordingFs, When},
};

const ACTOR: &str = "test/1";

/// Changeset `n`'s id.
fn id(n: u64) -> ChangesetId {
    ChangesetId::new(hash_object(ObjectKind::Changeset, &n.to_le_bytes()))
}

/// Changeset `n`, a root, as a caller hands it to `insert_changeset`.
fn changeset(n: u64) -> ChangesetMeta {
    let delta_hash = hash_object(ObjectKind::Blob, &n.to_le_bytes()).0;
    ChangesetMeta::new(id(n), 0, 0, delta_hash, Vec::new())
}

/// The header line of `meta/features`.
const HEADER: &str = "swarmops-features 1\n";

/// A fresh FORMAT 3 store, one changeset and `main` committed.
fn v3_store() -> (TempDir, Layout, MetaStore) {
    let (dir, layout) = new_root();
    let layout = Layout::with_format(layout.root(), StoreFormat::V3);
    let store = MetaStore::create_in(layout.clone(), real_fs()).unwrap();
    commit(&store, 1);
    (dir, layout, store)
}

/// Commits changeset `n`, and `main` at it, in a transaction of its own.
fn commit(store: &MetaStore, n: u64) {
    let mut write = store.write().unwrap();
    write.insert_changeset(changeset(n)).unwrap();
    let expected = write.get_ref("main").unwrap();
    write.cas_ref("main", expected, Some(id(n)), ACTOR).unwrap();
    write.commit().unwrap();
}

/// Writes `names` as the feature list of `layout`.
fn list(layout: &Layout, names: &[&str]) {
    let mut bytes = HEADER.to_owned();
    for name in names {
        bytes.push_str(name);
        bytes.push('\n');
    }
    fs::write(layout.features_file(), bytes).unwrap();
}

/// Every file under the root of `layout`, by path, with its bytes.
fn files(layout: &Layout) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut dirs = vec![layout.root().as_std_path().to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                dirs.push(path);
            } else {
                let bytes = fs::read(&path).unwrap();
                files.insert(path, bytes);
            }
        }
    }
    files
}

/// Whether `result` is the refusal of a store listing `features` this build lacks.
fn too_new<T>(result: &Result<T, StoreError>, features: &[&str]) -> bool {
    matches!(result, Err(StoreError::TooNew { format: 3, features: got }) if got == features)
}

/// A store listing a feature this build does not support is refused by its open, a handle's
/// read, committed read and write, with nothing written: every file's bytes are as they were.
#[test]
fn an_unknown_feature_refuses_open_read_and_write() {
    let (_dir, layout, store) = v3_store();
    list(&layout, &["newer"]);
    let before = files(&layout);

    let opened = MetaStore::open_in(layout.clone(), real_fs()).map(drop);
    assert!(too_new(&opened, &["newer"]), "open: {opened:?}");
    let read = store.read().map(drop);
    assert!(too_new(&read, &["newer"]), "read: {read:?}");
    let committed = store.read_committed().map(drop);
    assert!(too_new(&committed, &["newer"]), "committed: {committed:?}");
    let write = store.write().map(drop);
    assert!(too_new(&write, &["newer"]), "write: {write:?}");

    // The journal, the index, the refs, the checkpoint and the list: untouched. The lock file
    // is opened by the write, never written.
    assert_eq!(files(&layout), before, "nothing written");
}

/// Whether `result` is a failed read of a malformed file (`InvalidData`).
fn invalid_data<T>(result: &Result<T, StoreError>) -> bool {
    matches!(result, Err(StoreError::Io(e)) if e.kind() == std::io::ErrorKind::InvalidData)
}

/// A list whose header names a later list format (`swarmops-features 2`) is a newer build's,
/// refused with `TooNew` naming no feature, nothing written (the FORMAT 3 design's amendment of
/// 2026-11-11, G5); a header of another shape stays `InvalidData`.
#[test]
fn a_features_header_past_1_is_too_new() {
    let (_dir, layout, store) = v3_store();

    // 1. A later list format, whatever follows its header: TooNew, nothing written.
    for later in [
        "swarmops-features 2\n",
        "swarmops-features 10\nnewer\n",
        "swarmops-features 99999999999999999999999\n",
    ] {
        fs::write(layout.features_file(), later).unwrap();
        let before = files(&layout);
        let opened = MetaStore::open_in(layout.clone(), real_fs()).map(drop);
        assert!(too_new(&opened, &[]), "{later:?} open: {opened:?}");
        let read = store.read().map(drop);
        assert!(too_new(&read, &[]), "{later:?} read: {read:?}");
        let write = store.write().map(drop);
        assert!(too_new(&write, &[]), "{later:?} write: {write:?}");
        assert_eq!(files(&layout), before, "{later:?}: nothing written");
    }

    // 2. Any other header: InvalidData, as before.
    for other in [
        "swarmops-features x\n",
        "swarmops-features 0\n",
        "swarmops-features 2",
        "swarmops-features 02\n",
        "swarmops-features -2\n",
    ] {
        fs::write(layout.features_file(), other).unwrap();
        let read = store.read().map(drop);
        assert!(invalid_data(&read), "{other:?}: {read:?}");
    }
}

/// A write queue's lead checks the list too, where every write begins (the M20 plan's A3): its
/// turn is refused, nothing written, and the next turn is refused the same way.
#[test]
fn a_lead_checks_features_too() {
    let (_dir, layout, store) = v3_store();
    list(&layout, &["newer"]);
    let before = files(&layout);

    for serve in [Serve::Queue, Serve::OwnOnly] {
        let turn = store.take_turn(b"a member's request", serve).map(drop);
        assert!(too_new(&turn, &["newer"]), "{serve:?}: {turn:?}");
    }
    assert_eq!(files(&layout), before, "nothing written, no request left");
}

/// The release build supports `tag_object` alone, so it opens no store that lists another
/// feature: the seams that widen the set and fix the nonces exist only in a build with the test
/// hooks (a release build that named them would not compile; the CLI's release proof builds this
/// crate without them).
#[test]
fn release_build_has_no_feature_or_nonce_seam() {
    assert_eq!(SUPPORTED_FEATURES, ["tag_object"]);
    assert_eq!(MAX_FEATURES, 32);

    // With no seam held, every fact kind's feature but tag_object is unknown here.
    let (_dir, layout, store) = v3_store();
    for name in [
        "review",
        "annotation",
        "check",
        "queue",
        "withdrawal",
        "lineage",
    ] {
        let mut names = [name, "provenance"];
        names.sort_unstable();
        list(&layout, &names);
        let read = store.read().map(drop);
        assert!(too_new(&read, &names), "{name}: {read:?}");
    }
}

/// A lineage fact, which a frame of its own holds.
#[cfg(feature = "test-hooks")]
fn lineage() -> Record {
    let fact = Fact::new(
        FactKind::Lineage,
        0,
        [1; 32],
        [2; 32],
        1_700_000_000,
        ACTOR.to_owned(),
        vec![0; 32],
    );
    Record::Fact(fact.unwrap())
}

/// Appends a frame of a lineage fact to the journal of `layout` through `fs`, under the lock.
#[cfg(feature = "test-hooks")]
fn append_lineage(layout: &Layout, fs: Arc<dyn swarmops_store::FsOps>) -> Result<(), StoreError> {
    let _lock = MetaLock::acquire(layout, Duration::from_secs(10)).unwrap();
    let journal = Journal::open(layout, fs).unwrap();
    journal.repair_tail().unwrap();
    journal.append_frame(&[lineage()]).map(drop)
}

/// The append of a frame holding a fact makes the fact's feature durable in the list first (W10):
/// the list is replaced before the frame's append, once; a fact whose feature the build does not
/// support, or a list that cannot be replaced, writes no frame.
#[cfg(feature = "test-hooks")]
#[test]
fn a_feature_is_durable_before_its_first_record() {
    let (_dir, layout, _store) = v3_store();
    let log = || fs::read(layout.journal_log()).unwrap();

    // 1. Not supported: refused, nothing written.
    let before = files(&layout);
    let refused = append_lineage(&layout, real_fs());
    assert!(
        matches!(refused, Err(StoreError::BadCall(_))),
        "{refused:?}"
    );
    assert_eq!(files(&layout), before);

    // 2. Supported, and the list cannot be replaced: no frame.
    let _lineage = supported_for_tests(&["lineage"]);
    let (fs, calls) = RecordingFs::new();
    let failing = fs.failing();
    let fs = Arc::new(fs);
    failing.set("atomic_replace features");
    let length = log().len();
    assert!(append_lineage(&layout, fs.clone()).is_err());
    assert_eq!(log().len(), length, "no frame");
    failing.clear();
    calls();

    // 3. Listed first, then the frame; the next frame lists nothing again.
    append_lineage(&layout, fs.clone()).unwrap();
    let made = calls();
    let listed = made
        .iter()
        .position(|call| call == "atomic_replace features");
    let framed = made
        .iter()
        .position(|call| call == "append_durable journal2.log");
    assert!(
        matches!((listed, framed), (Some(l), Some(f)) if l < f),
        "{made:?}"
    );
    assert_eq!(
        fs::read_to_string(layout.features_file()).unwrap(),
        format!("{HEADER}lineage\n")
    );
    append_lineage(&layout, fs).unwrap();
    assert!(!calls().iter().any(|call| call.ends_with(" features")));
}

/// Decoding takes the build's kinds, never the store's list (judgement H2): readers racing a
/// writer that enables a feature and appends its records read every snapshot right.
#[cfg(feature = "test-hooks")]
#[test]
fn a_reader_racing_a_feature_enable_reads_right() {
    let (_dir, layout, store) = v3_store();
    let _lineage = supported_for_tests(&["lineage"]);
    let done = AtomicBool::new(false);
    std::thread::scope(|scope| {
        for _ in 0..2 {
            scope.spawn(|| {
                while !done.load(Ordering::Relaxed) {
                    let read = store.read().map(drop);
                    assert!(read.is_ok(), "{read:?}");
                }
            });
        }
        for _ in 0..50 {
            append_lineage(&layout, real_fs()).unwrap();
        }
        done.store(true, Ordering::Relaxed);
    });
    assert!(store.read().is_ok());
}

/// A record of a kind this build does not know is damage, unless the list, read again when the
/// read meets it, names a feature the build lacks: then the store is too new. Here the list is
/// rewritten between the read's check and its read of the frames.
#[cfg(feature = "test-hooks")]
#[test]
fn an_unknown_kind_rereads_features_then_too_new() {
    let (_dir, layout, _store) = v3_store();
    {
        let _lineage = supported_for_tests(&["lineage"]);
        append_lineage(&layout, real_fs()).unwrap();
    }

    // 1. The list emptied: the record of a kind the build does not know is damage.
    list(&layout, &[]);
    let store = MetaStore::open_in(layout.clone(), real_fs()).unwrap();
    let read = store.read().map(drop);
    assert!(
        matches!(read, Err(StoreError::JournalCorrupt { .. })),
        "{read:?}"
    );

    // 2. Listed again once the read has checked the list: read again, it says too new.
    let features = layout.features_file();
    let (fs, _calls) = RecordingFs::new();
    let fs = fs.with_hook((
        "file_len journal2.off".to_owned(),
        When::Before,
        Box::new(move || fs::write(features, format!("{HEADER}lineage\n")).unwrap()),
    ));
    let store = MetaStore::open_in(layout.clone(), Arc::new(fs)).unwrap();
    let read = store.read().map(drop);
    assert!(too_new(&read, &["lineage"]), "{read:?}");
}

/// A seed fixes the nonces: two stores' first frames under the same seed end in the same one.
#[cfg(feature = "test-hooks")]
#[test]
fn a_seed_fixes_the_nonces() {
    let first_nonce = || {
        let _seed = seed_nonces(7);
        let (_dir, _layout, store) = v3_store();
        store.write().unwrap().nonce()
    };
    let (a, b) = (first_nonce(), first_nonce());
    assert!(a.is_some() && a == b, "{a:?} {b:?}");
    let (_dir, _layout, store) = v3_store();
    assert_ne!(store.write().unwrap().nonce(), a, "unseeded, another");
}
