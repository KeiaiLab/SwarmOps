//! The native metadata store through its facade, [`MetaStore`]: changesets and refs written
//! in write transactions, lock-free snapshots that overlay the frames past the checkpoint,
//! replay after a crash, the batched checkpoint and compaction, and writers in several
//! processes at once.
//!
//! Every test owns a fresh store root (`tempfile::tempdir()`), so tests are independent and
//! may run in any order or in parallel. The multi-process tests run this test binary again
//! as their children (selected by [`CHILD_ROOT`]), all on the real filesystem.

mod rendezvous;

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_core::{ChangesetId, ObjectId, ObjectKind, hash_object};
use swarmops_store::{
    CRASH_AT_ENV, CasResult, ChangesetMeta, CommitInfo, CrashAtFs, Frame, FsOps, Index,
    InsertOutcome, Journal, JournalEntry, Layout, LinkOutcome, MetaLock, MetaStore, RealFs, Record,
    RefMoveEntry, RefState, StoreError,
};
use tempfile::TempDir;

use crate::rendezvous::Rendezvous;

/// Set only in the children the multi-process tests spawn: the store root.
const CHILD_ROOT: &str = "SWARMOPS_TEST_META_CHILD_ROOT";

/// Set with [`CHILD_ROOT`]: the child's number, from 0.
const CHILD_NUMBER: &str = "SWARMOPS_TEST_META_CHILD_NUMBER";

/// How many children each multi-process test spawns.
const CHILDREN: u64 = 8;

/// How many changesets each child of `multi_process_inserts_all_persist` inserts.
const PER_CHILD: u64 = 50;

/// How a child's report to its parent starts, among the test harness's own output.
const REPORT: &str = "meta-child: ";

const ACTOR: &str = "test/1";

const SIGABRT: i32 = 6;

/// Set only in the child of `a_batched_sync_with_no_thread_to_spare_syncs_on_the_committer`,
/// which runs with no thread to spare: the store root.
#[cfg(target_os = "linux")]
const NO_THREADS_ROOT: &str = "SWARMOPS_TEST_META_NO_THREADS_ROOT";

/// Set only in the children of `crash_at_any_create_call_leaves_a_store_open_or_create_takes`:
/// the store root.
const CREATE_CRASH_ROOT: &str = "SWARMOPS_TEST_META_CREATE_CRASH_ROOT";

/// The FsOps calls of `MetaStore::create`, in order: the journal, `journal.ckpt` and the index
/// (its columns, `parents.log` and its four view files) staged in `meta.tmp/meta/`, then
/// `refs/` and the lock file, the staged directory synced, and the rename to `meta/` (which
/// syncs the store root).
const CREATE_CALLS: [&str; 24] = [
    "create_dir_all meta",
    "append_durable journal.log",
    "append_durable journal.off",
    "fsync_dir meta",
    "fsync_dir meta.tmp",
    "atomic_replace journal.ckpt",
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
    "create_dir_all refs",
    "append lock",
    "fsync_dir meta",
    "rename_durable meta",
];

fn new_root() -> (TempDir, Utf8PathBuf) {
    let dir = tempfile::tempdir().expect("create store root");
    let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("temp dir is UTF-8");
    (dir, root)
}

/// A fresh store; its directory lives as long as the `TempDir`.
fn new_store() -> (TempDir, Utf8PathBuf, MetaStore) {
    let (dir, root) = new_root();
    let store = MetaStore::create(&root).unwrap();
    (dir, root, store)
}

/// The id of changeset `n`: a real hash, so the ids look like a store's.
fn id(n: u64) -> ChangesetId {
    ChangesetId(hash_object(ObjectKind::Changeset, &n.to_le_bytes()))
}

/// The delta hash of changeset `n`.
fn delta(n: u64) -> [u8; 32] {
    hash_object(ObjectKind::Blob, &n.to_le_bytes()).0
}

/// Changeset `n` with parents `parents`, as a caller hands it to `insert_changeset`: seq and
/// generation 0, since the store assigns both.
fn cs(n: u64, parents: &[u64]) -> ChangesetMeta {
    ChangesetMeta {
        id: id(n),
        seq: 0,
        generation: 0,
        delta_hash: delta(n),
        parents: parents.iter().map(|&parent| id(parent)).collect(),
    }
}

/// `cs(n, parents)` as the store holds it: at `seq`, with `generation`.
fn stored(n: u64, parents: &[u64], seq: u64, generation: u64) -> ChangesetMeta {
    ChangesetMeta {
        seq,
        generation,
        ..cs(n, parents)
    }
}

/// Inserts `changesets`, each new, in one write transaction, and commits it.
fn insert(store: &MetaStore, changesets: &[ChangesetMeta]) -> CommitInfo {
    let mut write = store.write().unwrap();
    for meta in changesets {
        let outcome = write.insert_changeset(meta.clone()).unwrap();
        assert!(
            matches!(outcome, InsertOutcome::Inserted { .. }),
            "{outcome:?}"
        );
    }
    write.commit().unwrap()
}

/// Moves ref `name` from changeset `expected` to `new` in a write transaction of its own.
fn cas(store: &MetaStore, name: &str, expected: Option<u64>, new: Option<u64>) -> CasResult {
    let mut write = store.write().unwrap();
    let result = write
        .cas_ref(name, expected.map(id), new.map(id), ACTOR)
        .unwrap();
    write.commit().unwrap();
    result
}

fn ref_state(target: u64, version: u64) -> RefState {
    RefState {
        target: id(target),
        version,
    }
}

/// A journal handle of its own on the store at `root`, as another process would open it.
fn journal(root: &Utf8Path) -> Journal {
    Journal::open(&Layout::new(root), Arc::new(RealFs)).unwrap()
}

/// An index handle of its own on the store at `root`.
fn index(root: &Utf8Path) -> Index {
    Index::open(&Layout::new(root), Arc::new(RealFs)).unwrap()
}

/// Every frame of the journal at `root`.
fn frames(root: &Utf8Path) -> Vec<Frame> {
    journal(root).tail_frames_from(0).unwrap()
}

fn ckpt(root: &Utf8Path) -> u64 {
    journal(root).ckpt().unwrap()
}

fn log_len(root: &Utf8Path) -> u64 {
    file_len(Layout::new(root).journal_log())
}

fn file_len(path: impl AsRef<Path>) -> u64 {
    fs::metadata(path).unwrap().len()
}

fn append_bytes(path: impl AsRef<Path>, bytes: &[u8]) {
    let mut file = OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(bytes).unwrap();
}

/// The content of the file of ref `name`, if there is one.
fn ref_file(root: &Utf8Path, name: &str) -> Option<String> {
    fs::read_to_string(Layout::new(root).meta_path(&format!("refs/{name}"))).ok()
}

/// What a ref file holds: the target's 64 hex digits and the version, a line each.
fn ref_content(target: u64, version: u64) -> String {
    format!("{}\n{version}\n", id(target))
}

fn unix_now() -> i64 {
    let since = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    i64::try_from(since.as_secs()).unwrap()
}

/// The journal record of `meta`, at `generation`.
fn changeset_record(meta: &ChangesetMeta, generation: u64) -> Record {
    Record::Changeset {
        id: meta.id,
        generation,
        delta_hash: meta.delta_hash,
        parents: meta.parents.clone(),
    }
}

fn ref_move(name: &str, old: Option<u64>, new: Option<u64>, actor: &str, ts: i64) -> Record {
    Record::RefMove {
        name: name.into(),
        old: old.map(id),
        new: new.map(id),
        actor: actor.into(),
        ts,
    }
}

/// Appends `records` as one frame through a journal handle of its own, under the lock, and
/// stops there: what a writer that died right after its commit point leaves behind (no
/// index records, no ref files, the checkpoint where it was).
fn append_unmaterialized(root: &Utf8Path, records: &[Record]) -> Frame {
    let layout = Layout::new(root);
    let _lock = MetaLock::acquire(&layout, Duration::from_secs(10)).unwrap();
    let journal = Journal::open(&layout, Arc::new(RealFs)).unwrap();
    journal.truncate_torn_tail().unwrap();
    journal.append_frame(records).unwrap()
}

/// The seqs of the journal's `Compacted` records, as a snapshot of `store` lists them.
fn compacted_frames(store: &MetaStore) -> Vec<u64> {
    let entries = store
        .read()
        .unwrap()
        .journal_after(None, usize::MAX)
        .unwrap();
    entries
        .iter()
        .filter(|entry| entry.kind == "compacted")
        .map(|entry| entry.seq)
        .collect()
}

#[track_caller]
fn assert_missing_parent(got: Result<InsertOutcome, StoreError>, parent: u64) {
    match got {
        Err(StoreError::MissingParent(missing)) => assert_eq!(missing, id(parent)),
        other => panic!("expected MissingParent({}), got {other:?}", id(parent)),
    }
}

#[track_caller]
fn assert_ref_name<T: std::fmt::Debug>(got: Result<T, StoreError>, name: &str) {
    match got {
        Err(StoreError::RefName(refused)) => assert_eq!(refused, name),
        other => panic!("expected RefName({name:?}), got {other:?}"),
    }
}

#[track_caller]
fn assert_format(got: Result<MetaStore, StoreError>, what: &str) {
    match got {
        Err(StoreError::Format(_)) => {}
        Err(other) => panic!("{what}: expected Format, got {other:?}"),
        Ok(_) => panic!("{what}: expected Format, got a store"),
    }
}

#[test]
fn insert_then_get_roundtrips_meta_and_generation() {
    let (_dir, root, store) = new_store();
    let mut write = store.write().unwrap();
    // The seq and generation a caller passes are ignored: the store assigns both.
    let mut first = cs(1, &[]);
    (first.seq, first.generation) = (7, 99);
    let inserted = |seq| InsertOutcome::Inserted { seq };
    assert_eq!(write.insert_changeset(first).unwrap(), inserted(0));
    assert_eq!(write.insert_changeset(cs(2, &[1])).unwrap(), inserted(1));
    let info = write.commit().unwrap();
    assert_eq!((info.frame_seq, info.inserted, info.moved_refs), (0, 2, 0));
    // A merge: 1 + its largest parent generation; the parents keep their order.
    let mut write = store.write().unwrap();
    assert_eq!(write.insert_changeset(cs(3, &[2, 1])).unwrap(), inserted(2));
    write.commit().unwrap();

    let want = [
        stored(1, &[], 0, 1),
        stored(2, &[1], 1, 2),
        stored(3, &[2, 1], 2, 3),
    ];
    let reopened = MetaStore::open(&root).unwrap();
    for store in [&store, &reopened] {
        let read = store.read().unwrap();
        for meta in &want {
            assert_eq!(read.changeset(meta.id).unwrap().as_ref(), Some(meta));
        }
        assert_eq!(read.changeset(id(4)).unwrap(), None);
        assert_eq!(read.count_changesets().unwrap(), 3);
    }
}

#[test]
fn insert_changeset_rejects_missing_parent() {
    let (_dir, root, store) = new_store();
    let mut write = store.write().unwrap();
    assert_missing_parent(write.insert_changeset(cs(2, &[1])), 1);
    // A parent queued earlier in the same write counts.
    write.insert_changeset(cs(1, &[])).unwrap();
    write.insert_changeset(cs(2, &[1])).unwrap();
    write.commit().unwrap();

    // A second parent that is nowhere: refused, and nothing is queued.
    let mut write = store.write().unwrap();
    assert_missing_parent(write.insert_changeset(cs(3, &[2, 42])), 42);
    assert_eq!(write.commit().unwrap().inserted, 0);
    assert_eq!(frames(&root).len(), 1, "the refused insert wrote nothing");
    let read = store.read().unwrap();
    assert_eq!(read.count_changesets().unwrap(), 2);
    assert_eq!(read.changeset(id(3)).unwrap(), None);
}

#[test]
fn insert_changeset_rejects_duplicate_parents() {
    let (_dir, root, store) = new_store();
    insert(&store, &[cs(1, &[]), cs(2, &[1])]);
    let mut write = store.write().unwrap();
    for (parents, twice) in [(&[1, 1][..], 1), (&[2, 1, 2], 2)] {
        match write.insert_changeset(cs(3, parents)) {
            Err(StoreError::DuplicateParent(parent)) => assert_eq!(parent, id(twice)),
            other => panic!("{parents:?}: expected DuplicateParent, got {other:?}"),
        }
    }
    assert_eq!(write.commit().unwrap().inserted, 0);
    assert_eq!(frames(&root).len(), 1);
    assert_eq!(store.read().unwrap().changeset(id(3)).unwrap(), None);
}

#[test]
fn insert_is_idempotent() {
    let (_dir, root, store) = new_store();
    insert(&store, &[cs(1, &[])]);
    let mut write = store.write().unwrap();
    let again = write.insert_changeset(cs(1, &[])).unwrap();
    assert_eq!(again, InsertOutcome::AlreadyPresent);
    let info = write.commit().unwrap();
    let nothing = CommitInfo {
        frame_seq: 1,
        inserted: 0,
        moved_refs: 0,
        checkpointed: false,
    };
    assert_eq!(
        info, nothing,
        "a write with nothing queued appends no frame"
    );
    assert_eq!(frames(&root).len(), 1);

    // A changeset queued earlier in the same write is present too.
    let mut write = store.write().unwrap();
    let first = write.insert_changeset(cs(2, &[1])).unwrap();
    assert_eq!(first, InsertOutcome::Inserted { seq: 1 });
    let second = write.insert_changeset(cs(2, &[1])).unwrap();
    assert_eq!(second, InsertOutcome::AlreadyPresent);
    write.commit().unwrap();
    let frames = frames(&root);
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[1].records, [changeset_record(&cs(2, &[1]), 2)]);
    assert_eq!(store.read().unwrap().count_changesets().unwrap(), 2);
}

#[test]
fn cas_ref_create_update_delete_and_mismatch() {
    let (_dir, root, store) = new_store();
    insert(&store, &[cs(1, &[]), cs(2, &[1])]);
    let before = unix_now();
    assert_eq!(
        cas(&store, "main", None, Some(1)),
        CasResult::Applied { old: None }
    );
    let read = store.read().unwrap();
    assert_eq!(read.get_ref("main").unwrap(), Some(ref_state(1, 1)));
    assert_eq!(ref_file(&root, "main"), Some(ref_content(1, 1)));

    // A wrong expectation: the actual target comes back, and nothing is written.
    let frames_before = frames(&root).len();
    let actual_1 = CasResult::Mismatch {
        actual: Some(id(1)),
    };
    assert_eq!(cas(&store, "main", Some(2), Some(2)), actual_1);
    assert_eq!(cas(&store, "main", None, Some(2)), actual_1);
    assert_eq!(
        frames(&root).len(),
        frames_before,
        "no frame for a mismatch"
    );

    let applied = CasResult::Applied { old: Some(id(1)) };
    assert_eq!(cas(&store, "main", Some(1), Some(2)), applied);
    let read = store.read().unwrap();
    assert_eq!(read.get_ref("main").unwrap(), Some(ref_state(2, 2)));
    assert_eq!(ref_file(&root, "main"), Some(ref_content(2, 2)));

    let applied = CasResult::Applied { old: Some(id(2)) };
    assert_eq!(cas(&store, "main", Some(2), None), applied);
    assert_eq!(store.read().unwrap().get_ref("main").unwrap(), None);
    assert_eq!(ref_file(&root, "main"), None, "a deleted ref has no file");
    let actual_none = CasResult::Mismatch { actual: None };
    assert_eq!(cas(&store, "main", Some(2), Some(1)), actual_none);

    let after = unix_now();
    let log = store.read().unwrap().reflog("main", 10).unwrap();
    let moves: Vec<_> = log
        .iter()
        .map(|entry| (entry.seq, entry.old, entry.new, entry.actor.as_str()))
        .collect();
    let want = [
        (3, Some(id(2)), None, ACTOR),
        (2, Some(id(1)), Some(id(2)), ACTOR),
        (1, None, Some(id(1)), ACTOR),
    ];
    assert_eq!(moves, want, "newest first");
    assert!(log.iter().all(|entry| (before..=after).contains(&entry.ts)));
    assert_eq!(store.read().unwrap().reflog("main", 2).unwrap(), log[..2]);
    assert_eq!(store.read().unwrap().reflog("main", 0).unwrap(), []);
}

#[test]
fn cas_ref_rejects_unknown_target_and_bad_name() {
    let (_dir, _root, store) = new_store();
    insert(&store, &[cs(1, &[])]);
    let mut write = store.write().unwrap();
    match write.cas_ref("main", None, Some(id(9)), ACTOR) {
        Err(StoreError::UnknownRefTarget(target)) => assert_eq!(target, id(9)),
        other => panic!("expected UnknownRefTarget, got {other:?}"),
    }
    let too_long = format!("release/{}", "x".repeat(248));
    let bad = [
        "",
        "master",
        "Main",
        "main/x",
        "release",
        "release/",
        "release/a/b",
        "release/.",
        "release/..",
        "release/.hidden",
        "release/a b",
        "release/é",
        "refs/heads/main",
        "../main",
        "release/../main",
        &too_long,
    ];
    let read = store.read().unwrap();
    for name in bad {
        assert_ref_name(write.cas_ref(name, None, Some(id(1)), ACTOR), name);
        assert_ref_name(read.get_ref(name), name);
        assert_ref_name(read.reflog(name, 1), name);
    }
    let longest = format!("release/{}", "x".repeat(247));
    let good = ["main", "release/v1.0", "release/2026-10_rc.1", &longest];
    for name in good {
        let got = write.cas_ref(name, None, Some(id(1)), ACTOR).unwrap();
        assert_eq!(got, CasResult::Applied { old: None }, "{name}");
    }
    // An actor longer than a ref move can record is refused before anything is queued.
    let actor = "a".repeat(256);
    match write.cas_ref("release/v2", None, Some(id(1)), &actor) {
        Err(StoreError::Io(e)) => assert_eq!(e.kind(), io::ErrorKind::InvalidInput),
        other => panic!("expected InvalidInput, got {other:?}"),
    }
    assert_eq!(write.commit().unwrap().moved_refs, 4);
    let listed = store.read().unwrap().list_refs().unwrap();
    let names: Vec<&str> = listed.iter().map(|(name, _)| name.as_str()).collect();
    let mut sorted = good;
    sorted.sort_unstable();
    assert_eq!(names, sorted);
    assert!(listed.iter().all(|(_, state)| *state == ref_state(1, 1)));
}

#[test]
fn ref_files_lists_each_ref_file_even_when_one_is_damaged() {
    let (_dir, root, store) = new_store();
    assert!(store.ref_files().unwrap().is_empty());
    insert(&store, &[cs(1, &[]), cs(2, &[1])]);
    cas(&store, "main", None, Some(1));
    cas(&store, "release/a", None, Some(2));
    cas(&store, "release/a", Some(2), Some(1));

    // A damaged ref file fails every snapshot; ref_files reports it and lists the rest. Temp
    // files and names that are no refs are passed over, as snapshots pass over them.
    let release = Layout::new(&root).meta_path("refs/release");
    fs::write(release.join("broken"), b"not a ref file\n").unwrap();
    fs::write(release.join(".tmpA1b2C3"), b"half a ref file").unwrap();
    fs::write(release.join("no spaces"), ref_content(1, 1)).unwrap();
    assert!(store.read().is_err());

    let files = store.ref_files().unwrap();
    let names: Vec<&str> = files.iter().map(|file| file.name.as_str()).collect();
    assert_eq!(names, ["main", "release/a", "release/broken"]);
    assert_eq!(files[0].state.as_ref().unwrap(), &ref_state(1, 1));
    assert_eq!(files[1].state.as_ref().unwrap(), &ref_state(1, 2));
    match &files[2].state {
        Err(StoreError::Io(e)) => assert_eq!(e.kind(), io::ErrorKind::InvalidData),
        other => panic!("expected InvalidData, got {other:?}"),
    }
}

/// A root changeset whose id starts with `prefix`, its other bytes `fill`.
fn with_prefix(prefix: &[u8], fill: u8) -> ChangesetMeta {
    let mut bytes = [fill; 32];
    bytes[..prefix.len()].copy_from_slice(prefix);
    ChangesetMeta {
        id: ChangesetId(ObjectId(bytes)),
        seq: 0,
        generation: 0,
        delta_hash: [fill; 32],
        parents: vec![],
    }
}

#[test]
fn resolve_prefix_unique_and_ambiguous() {
    let (_dir, root, store) = new_store();
    let shared = [0xab, 0xcd, 0xef, 0x01, 0x23, 0x45];
    let one = with_prefix(&shared, 0x10); // abcdef012345 1010…
    let two = with_prefix(&shared, 0x20); // abcdef012345 2020…
    let near = with_prefix(&[0xab, 0xcd, 0xef, 0x01, 0x23, 0x46], 0x10); // 11 digits shared
    insert(&store, &[one.clone(), two.clone(), near.clone()]);
    // One more in a frame past the checkpoint that the index does not hold yet.
    let past = with_prefix(&shared, 0x30);
    append_unmaterialized(&root, &[changeset_record(&past, 1)]);

    let read = store.read().unwrap();
    let resolve = |prefix: &str| read.resolve_prefix(prefix).unwrap();
    assert_eq!(
        resolve("abcdef012345"),
        [one.id, two.id, past.id],
        "ambiguous"
    );
    assert_eq!(resolve("abcdef0123451"), [one.id], "an odd digit count");
    assert_eq!(resolve("ABCDEF0123452"), [two.id], "either case");
    assert_eq!(resolve("abcdef0123453"), [past.id], "from the overlay");
    assert_eq!(resolve("abcdef01234"), [one.id, two.id, near.id, past.id]);
    assert_eq!(resolve("abcdef012347"), []);
    assert_eq!(resolve(&one.id.0.to_hex()), [one.id], "all 64 digits");
    for bad in ["", "abcdef01234g", &"a".repeat(65)] {
        match read.resolve_prefix(bad) {
            Err(StoreError::Io(error)) => assert_eq!(error.kind(), io::ErrorKind::InvalidInput),
            other => panic!("{bad:?}: expected InvalidInput, got {other:?}"),
        }
    }
    assert_eq!(read.by_delta_hash(&[0x30; 32]).unwrap(), [past.id]);
    assert_eq!(read.by_delta_hash(&[0x10; 32]).unwrap(), [one.id, near.id]);
}

#[test]
fn journal_after_returns_global_order() {
    let (_dir, root, first) = new_store();
    let second = MetaStore::open(&root).unwrap(); // another handle, as another process has
    let before = unix_now();
    insert(&first, &[cs(1, &[])]); // frame 0
    insert(&second, &[cs(2, &[1])]); // frame 1
    let mut write = first.write().unwrap(); // frame 2: a changeset and a ref move
    write.insert_changeset(cs(3, &[2])).unwrap();
    write.cas_ref("main", None, Some(id(3)), ACTOR).unwrap();
    write.commit().unwrap();
    let applied = CasResult::Applied { old: Some(id(3)) };
    assert_eq!(cas(&second, "main", Some(3), Some(2)), applied); // frame 3
    let after = unix_now();

    let changeset = |seq, n| (seq, "changeset", id(n).0.0.to_vec());
    let moved = |seq| (seq, "ref_move", b"main".to_vec());
    let want = [
        changeset(0, 1),
        changeset(1, 2),
        changeset(2, 3),
        moved(2),
        moved(3),
    ];
    for store in [&first, &second] {
        let read = store.read().unwrap();
        let all = read.journal_after(None, 100).unwrap();
        let shape: Vec<_> = all
            .iter()
            .map(|entry| (entry.seq, entry.kind, entry.key.clone()))
            .collect();
        assert_eq!(shape, want);
        for entry in &all {
            let JournalEntry { kind, ts, .. } = entry;
            match *kind {
                "ref_move" => assert!((before..=after).contains(ts), "{entry:?}"),
                _ => assert_eq!(*ts, 0, "{entry:?}"),
            }
        }
        assert_eq!(read.journal_after(Some(1), 100).unwrap(), all[2..]);
        assert_eq!(
            read.journal_after(Some(0), 2).unwrap(),
            all[1..4],
            "limit counts frames"
        );
        assert_eq!(read.journal_after(None, 1).unwrap(), all[..1]);
        assert_eq!(read.journal_after(Some(3), 10).unwrap(), []);
        assert_eq!(read.journal_after(Some(u64::MAX), 10).unwrap(), []);
        assert_eq!(read.journal_after(None, 0).unwrap(), []);
    }
}

#[test]
fn write_is_one_frame_for_insert_plus_cas() {
    let (_dir, root, store) = new_store();
    let mut write = store.write().unwrap();
    write.insert_changeset(cs(1, &[])).unwrap();
    let applied = write.cas_ref("main", None, Some(id(1)), ACTOR).unwrap();
    assert_eq!(applied, CasResult::Applied { old: None });
    let info = write.commit().unwrap();
    let want = CommitInfo {
        frame_seq: 0,
        inserted: 1,
        moved_refs: 1,
        checkpointed: false,
    };
    assert_eq!(info, want);

    let frames = frames(&root);
    assert_eq!(frames.len(), 1, "one frame for the whole transaction");
    match frames[0].records.as_slice() {
        [
            Record::Changeset { id: inserted, .. },
            Record::RefMove {
                name,
                old: None,
                new: Some(target),
                actor,
                ..
            },
        ] => {
            assert_eq!((*inserted, *target), (id(1), id(1)));
            assert_eq!((name.as_str(), actor.as_str()), ("main", ACTOR));
        }
        other => panic!("expected a changeset and a ref move, got {other:?}"),
    }
    let entries = store
        .read()
        .unwrap()
        .journal_after(None, usize::MAX)
        .unwrap();
    assert_eq!(entries.len(), 2);
    assert!(entries.iter().all(|entry| entry.seq == 0));
}

#[test]
fn drop_without_commit_discards() {
    let (_dir, root, store) = new_store();
    let mut write = store.write().unwrap();
    write.insert_changeset(cs(1, &[])).unwrap();
    write.cas_ref("main", None, Some(id(1)), ACTOR).unwrap();
    drop(write);

    let read = store.read().unwrap();
    assert_eq!(read.changeset(id(1)).unwrap(), None);
    assert_eq!(read.get_ref("main").unwrap(), None);
    assert_eq!(read.count_changesets().unwrap(), 0);
    assert_eq!(read.journal_after(None, 10).unwrap(), []);
    assert_eq!(log_len(&root), 0);
    assert_eq!(ref_file(&root, "main"), None);
    // The lock went with it: another writer gets it at once.
    let impatient = MetaStore::open(&root)
        .unwrap()
        .with_lock_timeout(Duration::ZERO);
    insert(&impatient, &[cs(1, &[])]);
    assert_eq!(store.read().unwrap().count_changesets().unwrap(), 1);
}

#[test]
fn reader_sees_frames_past_ckpt() {
    let (_dir, root, store) = new_store();
    insert(&store, &[cs(1, &[])]);
    let records = [
        changeset_record(&cs(2, &[1]), 2),
        ref_move("main", None, Some(2), "crashed/1", 7),
    ];
    assert_eq!(append_unmaterialized(&root, &records).seq, 1);
    assert_eq!(
        index(&root).find(id(2)).unwrap(),
        None,
        "the index lacks it"
    );
    assert_eq!(ref_file(&root, "main"), None);

    let read = store.read().unwrap();
    assert_eq!(read.changeset(id(2)).unwrap(), Some(stored(2, &[1], 1, 2)));
    assert_eq!(read.count_changesets().unwrap(), 2);
    assert_eq!(
        read.resolve_prefix(&id(2).to_string()[..16]).unwrap(),
        [id(2)]
    );
    assert_eq!(read.by_delta_hash(&delta(2)).unwrap(), [id(2)]);
    assert_eq!(read.get_ref("main").unwrap(), Some(ref_state(2, 1)));
    let main = ("main".to_owned(), ref_state(2, 1));
    assert_eq!(read.list_refs().unwrap(), [main]);
    let moved = RefMoveEntry {
        seq: 1,
        old: None,
        new: Some(id(2)),
        actor: "crashed/1".into(),
        ts: 7,
    };
    assert_eq!(read.reflog("main", 10).unwrap(), [moved]);
    drop(read);

    // The next write replays the frame into the index and the ref file; the checkpoint
    // moves at the next batched sync point, here an explicit flush.
    let write = store.write().unwrap();
    assert_eq!(index(&root).find(id(2)).unwrap(), Some(1));
    assert_eq!(ref_file(&root, "main"), Some(ref_content(2, 1)));
    assert!(!write.commit().unwrap().checkpointed);
    assert_eq!(ckpt(&root), 0);
    store.flush().unwrap();
    assert_eq!(ckpt(&root), log_len(&root));

    let reopened = MetaStore::open(&root).unwrap();
    let read = reopened.read().unwrap();
    assert_eq!(read.changeset(id(2)).unwrap(), Some(stored(2, &[1], 1, 2)));
    assert_eq!(read.get_ref("main").unwrap(), Some(ref_state(2, 1)));
    insert(&store, &[cs(3, &[2])]);
}

#[test]
fn torn_tail_recovered_on_next_write() {
    let (_dir, root, store) = new_store();
    insert(&store, &[cs(1, &[])]);
    let clean = log_len(&root);
    // A writer that died in the middle of its append: a header announcing 100 bytes, and
    // 10 of them.
    let mut torn = 100_u32.to_le_bytes().to_vec();
    torn.extend([0; 4]);
    torn.extend([0xee; 10]);
    append_bytes(Layout::new(&root).journal_log(), &torn);

    let read = store.read().unwrap();
    assert_eq!(
        read.count_changesets().unwrap(),
        1,
        "readers skip a torn tail"
    );
    assert_eq!(read.journal_after(None, 10).unwrap().len(), 1);
    drop(read);

    let mut write = store.write().unwrap();
    assert_eq!(log_len(&root), clean, "the next writer cuts the torn tail");
    write.insert_changeset(cs(2, &[1])).unwrap();
    assert_eq!(write.commit().unwrap().frame_seq, 1);
    let frames = frames(&root);
    assert_eq!(frames.len(), 2);
    assert_eq!(
        frames[1].offset, clean,
        "the new frame starts where the torn one did"
    );
    let reopened = MetaStore::open(&root).unwrap();
    let read = reopened.read().unwrap();
    assert_eq!(read.changeset(id(2)).unwrap(), Some(stored(2, &[1], 1, 2)));
    assert_eq!(read.count_changesets().unwrap(), 2);
}

#[test]
fn compaction_triggered_past_threshold() {
    let (_dir, root, store) = new_store();
    let batch =
        |b: u64| -> Vec<ChangesetMeta> { (b * 100..(b + 1) * 100).map(|n| cs(n, &[])).collect() };
    for b in 0..10 {
        assert!(!insert(&store, &batch(b)).checkpointed, "batch {b}");
    }
    assert_eq!(index(&root).tail_len(), 1000);
    assert_eq!(compacted_frames(&store), []);

    // The write that takes the tail past the threshold compacts, then checkpoints.
    let info = insert(&store, &batch(10));
    assert_eq!(info.frame_seq, 10);
    assert!(info.checkpointed, "a compaction is a sync point");
    let index = index(&root);
    assert!(index.tail_len() <= Index::COMPACT_THRESHOLD);
    let counts = (index.len(), index.sorted_count(), index.tail_len());
    assert_eq!(counts, (1100, 1100, 0));
    assert_eq!(
        compacted_frames(&store),
        [11],
        "a frame of its own, after the write's"
    );
    let frames = frames(&root);
    assert_eq!(
        frames[11].records,
        [Record::Compacted { sorted_count: 1100 }]
    );
    assert_eq!(
        ckpt(&root),
        log_len(&root),
        "the checkpoint is past the Compacted frame"
    );

    let read = store.read().unwrap();
    assert_eq!(read.count_changesets().unwrap(), 1100);
    for n in [0, 549, 1099] {
        assert_eq!(read.changeset(id(n)).unwrap(), Some(stored(n, &[], n, 1)));
    }
}

/// The store root and number of this process, if it is a child a multi-process test spawned.
fn child_env() -> Option<(Utf8PathBuf, u64)> {
    let root = std::env::var(CHILD_ROOT).ok()?;
    let number = std::env::var(CHILD_NUMBER).unwrap().parse().unwrap();
    Some((root.into(), number))
}

/// Waits until the parent says go, so that the children start their writes together.
fn wait_for_go(root: &Utf8Path) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !root.join("go").exists() {
        assert!(Instant::now() < deadline, "the parent never said go");
        thread::sleep(Duration::from_millis(1));
    }
}

/// Runs the test named `test` again in [`CHILDREN`] child processes on the store at `root`,
/// lets them go together, and returns what each one printed and how it ended.
fn run_children(test: &str, root: &Utf8Path) -> Vec<Output> {
    let exe = std::env::current_exe().unwrap();
    let children: Vec<_> = (0..CHILDREN)
        .map(|number| {
            Command::new(&exe)
                .args(["--exact", test, "--nocapture"])
                .env(CHILD_ROOT, root)
                .env(CHILD_NUMBER, number.to_string())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    fs::write(root.join("go"), b"").unwrap();
    children
        .into_iter()
        .map(|child| child.wait_with_output().unwrap())
        .collect()
}

/// The report line a child printed, after asserting that it exited cleanly.
#[track_caller]
fn report(number: usize, output: &Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "child {number}: {:?}\n--- stdout ---\n{stdout}\n--- stderr ---\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );
    stdout
        .lines()
        .find_map(|line| line.strip_prefix(REPORT))
        .unwrap_or_default()
        .to_owned()
}

/// The number of changeset `j` of child `child`.
fn child_changeset(child: u64, j: u64) -> u64 {
    1_000 * (child + 1) + j
}

/// A child of `multi_process_inserts_all_persist`: [`PER_CHILD`] write transactions, each
/// inserting the next changeset of its own chain off changeset 0.
fn insert_child(root: &Utf8Path, child: u64) {
    let store = MetaStore::open(root).unwrap();
    wait_for_go(root);
    let mut parent = 0;
    for j in 0..PER_CHILD {
        let n = child_changeset(child, j);
        insert(&store, &[cs(n, &[parent])]);
        parent = n;
        // A pause between transactions, as an agent has, so the writers interleave.
        thread::sleep(Duration::from_millis(2));
    }
    println!("{REPORT}done");
}

#[test]
fn multi_process_inserts_all_persist() {
    if let Some((root, child)) = child_env() {
        insert_child(&root, child);
        return;
    }
    let (_dir, root, store) = new_store();
    insert(&store, &[cs(0, &[])]);
    let outputs = run_children("multi_process_inserts_all_persist", &root);
    for (number, output) in outputs.iter().enumerate() {
        assert_eq!(report(number, output), "done");
    }

    let reopened = MetaStore::open(&root).unwrap();
    let read = reopened.read().unwrap();
    let total = 1 + CHILDREN * PER_CHILD;
    assert_eq!(read.count_changesets().unwrap(), total);
    let mut seqs = vec![0];
    for child in 0..CHILDREN {
        for j in 0..PER_CHILD {
            let parent = j.checked_sub(1).map_or(0, |i| child_changeset(child, i));
            let got = read
                .changeset(id(child_changeset(child, j)))
                .unwrap()
                .unwrap();
            assert_eq!((got.generation, got.parents), (j + 2, vec![id(parent)]));
            seqs.push(got.seq);
        }
    }
    seqs.sort_unstable();
    assert_eq!(seqs, (0..total).collect::<Vec<_>>(), "seqs are contiguous");
    let frames = frames(&root);
    assert_eq!(
        frames.len(),
        usize::try_from(total).unwrap(),
        "a frame per write"
    );
    let ckpt = ckpt(&root);
    let past_ckpt = frames.iter().filter(|frame| frame.offset >= ckpt).count();
    assert!(past_ckpt < 32, "{past_ckpt} frames past the checkpoint");
}

/// A child of `multi_process_cas_exactly_one_wins`: one try at moving main from changeset 0
/// to its own changeset.
fn cas_child(root: &Utf8Path, child: u64) {
    let store = MetaStore::open(root).unwrap();
    wait_for_go(root);
    let mut write = store.write().unwrap();
    let actor = format!("agent/{child}");
    let result = write
        .cas_ref("main", Some(id(0)), Some(id(child + 1)), &actor)
        .unwrap();
    write.commit().unwrap();
    match result {
        CasResult::Applied { old } => {
            assert_eq!(old, Some(id(0)));
            println!("{REPORT}applied");
        }
        CasResult::Mismatch { actual } => {
            println!("{REPORT}mismatch {}", actual.expect("main exists"));
        }
    }
}

#[test]
fn multi_process_cas_exactly_one_wins() {
    if let Some((root, child)) = child_env() {
        cas_child(&root, child);
        return;
    }
    let (_dir, root, store) = new_store();
    let mut write = store.write().unwrap();
    write.insert_changeset(cs(0, &[])).unwrap();
    for child in 0..CHILDREN {
        write.insert_changeset(cs(child + 1, &[0])).unwrap();
    }
    write.cas_ref("main", None, Some(id(0)), ACTOR).unwrap();
    write.commit().unwrap();

    let outputs = run_children("multi_process_cas_exactly_one_wins", &root);
    let reports: Vec<String> = outputs
        .iter()
        .enumerate()
        .map(|(number, output)| report(number, output))
        .collect();
    let winners: Vec<u64> = (0..CHILDREN)
        .filter(|&child| reports[usize::try_from(child).unwrap()] == "applied")
        .collect();
    let [winner] = winners[..] else {
        panic!("expected exactly one winner: {reports:?}");
    };
    let target = id(winner + 1);
    for (child, report) in (0..).zip(&reports) {
        if child != winner {
            assert_eq!(*report, format!("mismatch {target}"), "child {child}");
        }
    }

    let reopened = MetaStore::open(&root).unwrap();
    let read = reopened.read().unwrap();
    let main = RefState { target, version: 2 };
    assert_eq!(read.get_ref("main").unwrap(), Some(main));
    let log = read.reflog("main", 10).unwrap();
    let moves: Vec<_> = log.iter().map(|entry| (entry.old, entry.new)).collect();
    assert_eq!(moves, [(Some(id(0)), Some(target)), (None, Some(id(0)))]);
    assert_eq!(log[0].actor, format!("agent/{winner}"));
}

#[test]
fn write_finishes_an_interrupted_compaction() {
    let (_dir, root, store) = new_store();
    let all: Vec<ChangesetMeta> = (0..1100).map(|n| cs(n, &[])).collect();
    assert!(insert(&store, &all).checkpointed, "the write compacts");
    // A crash in a first compaction after it renamed the id view into place and before the
    // delta-hash view: dh.key and dh.rec are still the empty files the store was created with.
    let sorted = Layout::new(&root).meta_path("sorted");
    fs::write(sorted.join("dh.key"), []).unwrap();
    fs::write(sorted.join("dh.rec"), []).unwrap();
    let torn = index(&root);
    assert_eq!((torn.ids_view().len(), torn.dh_view().len()), (1100, 0));
    let found = store.read().unwrap().by_delta_hash(&delta(7)).unwrap();
    assert_eq!(
        found,
        [id(7)],
        "the tail scan covers the missing view meanwhile"
    );

    // The next write's repair of the index tail finishes the compaction, before anything
    // is queued.
    let write = store.write().unwrap();
    let repaired = index(&root);
    let views = (repaired.ids_view().len(), repaired.dh_view().len());
    assert_eq!(views, (1100, 1100));
    drop(write);
}

#[test]
fn a_snapshot_lists_no_frame_committed_after_it() {
    let (_dir, _root, store) = new_store();
    insert(&store, &[cs(1, &[]), cs(2, &[1])]); // frame 0
    cas(&store, "main", None, Some(1)); // frame 1
    store.flush().unwrap();
    let checkpointed = store.read().unwrap(); // no frame past the checkpoint
    insert(&store, &[cs(3, &[2])]); // frame 2, past the checkpoint
    let overlaid = store.read().unwrap();
    cas(&store, "main", Some(1), Some(2)); // frame 3, after both snapshots

    for (read, last) in [(&checkpointed, 1), (&overlaid, 2)] {
        let entries = read.journal_after(None, 10).unwrap();
        assert_eq!(entries.last().map(|entry| entry.seq), Some(last));
        assert_eq!(read.get_ref("main").unwrap(), Some(ref_state(1, 1)));
        let log = read.reflog("main", 10).unwrap();
        assert_eq!(log.iter().map(|entry| entry.seq).collect::<Vec<_>>(), [1]);
    }
    let now = store.read().unwrap();
    assert_eq!(now.journal_after(Some(2), 10).unwrap()[0].seq, 3);
    assert_eq!(now.reflog("main", 10).unwrap().len(), 2);
}

#[test]
fn replay_appends_the_records_a_power_loss_took() {
    let (_dir, root, store) = new_store();
    for n in 0..8 {
        insert(&store, &[cs(n, &[])]); // frames 0..=7, all past the checkpoint
    }
    // A power loss took the last three index records, never synced (the roots have no
    // parents, so parents.log is empty).
    let meta = Layout::new(&root).meta_dir();
    for (file, width) in [
        ("idx/ids.col", 32),
        ("idx/dh.col", 32),
        ("idx/gen.col", 8),
        ("idx/par.col", 8),
    ] {
        let column = OpenOptions::new()
            .write(true)
            .open(meta.join(file))
            .unwrap();
        column.set_len(5 * width).unwrap();
    }
    assert_eq!(index(&root).len(), 5);

    let read = store.read().unwrap();
    assert_eq!(read.count_changesets().unwrap(), 8);
    for n in 0..8 {
        assert_eq!(
            read.changeset(id(n)).unwrap(),
            Some(stored(n, &[], n, 1)),
            "{n}"
        );
    }
    drop(read);
    store.write().unwrap().commit().unwrap();
    let index = index(&root);
    assert_eq!(
        index.len(),
        8,
        "replay appended exactly the three it lacked"
    );
    for n in 0..8 {
        assert_eq!(index.get(n).unwrap(), stored(n, &[], n, 1), "{n}");
    }
}

#[track_caller]
fn assert_journal_corrupt(got: Result<(), StoreError>, offset: u64) {
    match got {
        Err(StoreError::JournalCorrupt { offset: at }) => assert_eq!(at, offset),
        other => panic!("expected JournalCorrupt at {offset}, got {other:?}"),
    }
}

#[test]
fn a_ref_move_that_names_no_ref_is_journal_corrupt() {
    let (_dir, root, store) = new_store();
    insert(&store, &[cs(1, &[])]);
    // No writer of this version records such a move (a newer one might, or damage): applied,
    // its name would lead out of meta/refs/ and onto the journal itself.
    let name = "../journal.log";
    let frame = append_unmaterialized(&root, &[ref_move(name, None, Some(1), "x/1", 7)]);
    assert_journal_corrupt(store.read().map(drop), frame.offset);
    assert_journal_corrupt(store.write().map(drop), frame.offset);
    assert_eq!(frames(&root).len(), 2, "journal.log is untouched");
}

#[test]
fn errors_after_the_commit_point_say_the_transaction_committed() {
    for failing in ["sync_data ids.col", "atomic_replace journal.ckpt"] {
        let (_dir, root) = new_root();
        let (fs, _calls) = RecordingFs::new();
        let fail = fs.failing();
        let store = MetaStore::create_with_fs(&root, Arc::new(fs)).unwrap();
        for n in 0..31 {
            insert(&store, &[cs(n, &[])]); // frames 0..=30
        }
        // The 32nd frame past the checkpoint: its batched index sync, or the checkpoint
        // after it, fails.
        fail.set(failing);
        let mut write = store.write().unwrap();
        write.insert_changeset(cs(31, &[])).unwrap();
        write.cas_ref("main", None, Some(id(31)), ACTOR).unwrap();
        match write.commit() {
            Err(StoreError::PostCommit {
                frame_seq: 31,
                source,
            }) => assert!(
                matches!(*source, StoreError::Io(_)),
                "{failing}: {source:?}"
            ),
            other => panic!("{failing}: expected PostCommit at frame 31, got {other:?}"),
        }
        let read = store.read().unwrap();
        let committed = read.changeset(id(31)).unwrap();
        assert_eq!(committed, Some(stored(31, &[], 31, 1)), "{failing}");
        assert_eq!(
            read.get_ref("main").unwrap(),
            Some(ref_state(31, 1)),
            "{failing}"
        );
        drop(read);
        assert_eq!(ckpt(&root), 0, "{failing}: the checkpoint did not move");

        // Retrying the transaction finds it done. With a healthy fs the next write syncs and
        // checkpoints (32 frames lie past the checkpoint), and a flush has nothing left.
        fail.clear();
        let mut write = store.write().unwrap();
        let again = write.insert_changeset(cs(31, &[])).unwrap();
        assert_eq!(again, InsertOutcome::AlreadyPresent);
        let moved = write.cas_ref("main", None, Some(id(31)), ACTOR).unwrap();
        assert_eq!(
            moved,
            CasResult::Mismatch {
                actual: Some(id(31))
            }
        );
        assert!(write.commit().unwrap().checkpointed, "{failing}");
        store.flush().unwrap();
        assert_eq!(ckpt(&root), log_len(&root), "{failing}");
    }
}

#[test]
fn an_empty_write_reports_its_housekeeping_errors_plainly() {
    let (_dir, root) = new_root();
    let (fs, _calls) = RecordingFs::new();
    let fail = fs.failing();
    let store = MetaStore::create_with_fs(&root, Arc::new(fs)).unwrap();
    // The compaction cannot write the sorted views.
    fail.set("append ids.key.tmp");
    let mut write = store.write().unwrap();
    for n in 0..1100 {
        write.insert_changeset(cs(n, &[])).unwrap();
    }
    match write.commit() {
        Err(StoreError::PostCommit { frame_seq: 0, .. }) => {}
        other => panic!("the transaction commits before its compaction fails: {other:?}"),
    }
    // A flush, or a commit with nothing queued, commits nothing of its own: the same failure
    // of the compaction it retries is a plain error.
    match store.flush() {
        Err(StoreError::Io(_)) => {}
        other => panic!("flush: expected a plain Io error, got {other:?}"),
    }
    match store.write().unwrap().commit() {
        Err(StoreError::Io(_)) => {}
        other => panic!("an empty commit: expected a plain Io error, got {other:?}"),
    }

    fail.clear();
    store.flush().unwrap();
    let index = index(&root);
    assert_eq!((index.sorted_count(), index.tail_len()), (1100, 0));
    assert_eq!(compacted_frames(&store), [1]);
    assert_eq!(ckpt(&root), log_len(&root));
}

#[track_caller]
fn assert_invalid_input<T: std::fmt::Debug>(got: Result<T, StoreError>) {
    match got {
        Err(StoreError::Io(error)) => assert_eq!(error.kind(), io::ErrorKind::InvalidInput),
        other => panic!("expected InvalidInput, got {other:?}"),
    }
}

#[test]
fn a_transaction_refuses_more_records_than_a_frame_holds() {
    let (_dir, root, store) = new_store();
    insert(&store, &[cs(1, &[]), cs(2, &[])]);
    // 65,535 moves of main, back and forth between changesets 1 and 2, fill one frame.
    let mut write = store.write().unwrap();
    let mut main = None;
    for i in 0..65_535 {
        let target = Some(id(1 + i % 2));
        let moved = write.cas_ref("main", main, target, ACTOR).unwrap();
        assert_eq!(moved, CasResult::Applied { old: main });
        main = target;
    }
    // The next record is refused, and nothing is queued; what queues nothing still answers.
    assert_invalid_input(write.cas_ref("main", main, Some(id(2)), ACTOR));
    assert_invalid_input(write.insert_changeset(cs(3, &[])));
    let present = write.insert_changeset(cs(1, &[])).unwrap();
    assert_eq!(present, InsertOutcome::AlreadyPresent);
    let stale = write.cas_ref("main", None, Some(id(2)), ACTOR).unwrap();
    assert_eq!(stale, CasResult::Mismatch { actual: main });
    assert_eq!(write.commit().unwrap().moved_refs, 65_535);
    assert_eq!(frames(&root)[1].records.len(), 65_535);
    assert_eq!(
        store.read().unwrap().get_ref("main").unwrap(),
        Some(ref_state(1, 1))
    );
}

#[test]
fn replay_checks_ref_names_before_appending_to_the_index() {
    let (_dir, root, store) = new_store();
    insert(&store, &[cs(1, &[])]);
    let records = [
        changeset_record(&cs(2, &[1]), 2),
        ref_move("../journal.log", None, Some(2), "x/1", 7),
    ];
    let frame = append_unmaterialized(&root, &records);
    assert_journal_corrupt(store.write().map(drop), frame.offset);
    assert_eq!(
        index(&root).len(),
        1,
        "the frame's changeset was not appended"
    );
}

/// Asserts that the store at `root` holds a fresh `meta/`, every file there, and nothing
/// left of the staging; and that a reader of it writes nothing.
#[track_caller]
fn assert_fresh_meta(root: &Utf8Path) {
    let meta = Layout::new(root).meta_dir();
    let files = [
        ("journal.log", 0),
        ("journal.off", 0),
        ("journal.ckpt", 8),
        ("lock", 0),
        ("idx/ids.col", 0),
        ("idx/gen.col", 0),
        ("idx/dh.col", 0),
        ("idx/par.col", 0),
        ("parents.log", 0),
        ("sorted/ids.key", 0),
        ("sorted/ids.rec", 0),
        ("sorted/dh.key", 0),
        ("sorted/dh.rec", 0),
    ];
    for (file, len) in files {
        assert_eq!(file_len(meta.join(file)), len, "{file}");
    }
    assert_eq!(
        fs::read(meta.join("journal.ckpt")).unwrap(),
        0_u64.to_le_bytes()
    );
    for (dir, holds) in [("refs", 0), ("sorted", 4)] {
        assert_eq!(
            fs::read_dir(meta.join(dir)).unwrap().count(),
            holds,
            "{dir} holds {holds} files"
        );
    }
    assert!(!root.join("meta.tmp").exists(), "the staging is gone");
    let (fs, calls) = RecordingFs::new();
    let reader = MetaStore::open_with_fs(root, Arc::new(fs)).unwrap();
    assert_eq!(reader.read().unwrap().count_changesets().unwrap(), 0);
    // It measures the journal past the checkpoint (journal.off first) before and after
    // reading the ref files.
    let snapshot = ["file_len journal.off", "file_len journal.log"];
    assert_eq!(
        calls(),
        [&snapshot[..], &["read main"], &snapshot].concat(),
        "a reader writes nothing"
    );
}

#[test]
fn crash_at_any_create_call_leaves_a_store_open_or_create_takes() {
    if let Ok(root) = std::env::var(CREATE_CRASH_ROOT) {
        let k = std::env::var(CRASH_AT_ENV).unwrap().parse().unwrap();
        let fs: Arc<dyn FsOps> = Arc::new(CrashAtFs::new(RealFs, k));
        MetaStore::create_with_fs(Utf8Path::new(&root), fs).unwrap();
        return;
    }
    let calls = u64::try_from(CREATE_CALLS.len()).unwrap();
    for k in 1..=calls + 1 {
        let (_dir, root) = new_root();
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "crash_at_any_create_call_leaves_a_store_open_or_create_takes",
                "--nocapture",
            ])
            .env(CREATE_CRASH_ROOT, &root)
            .env(CRASH_AT_ENV, k.to_string())
            .output()
            .unwrap();
        let ended_as_expected = if k <= calls {
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
        // The rename into place is the last call: a crash at any call leaves no meta/, so
        // open refuses and the next create cleans up and succeeds.
        if k <= calls {
            assert_format(
                MetaStore::open(&root),
                &format!("open after a crash at {k}"),
            );
            MetaStore::create(&root).unwrap();
        }
        assert_fresh_meta(&root);
    }
}

#[test]
fn create_starts_over_from_what_a_crashed_create_left() {
    let (_dir, root) = new_root();
    // A real crash in the middle of a replace leaves its temp file behind, and whatever else
    // lies in meta.tmp/ is no part of a store either.
    let staged = root.join("meta.tmp/meta");
    fs::create_dir_all(&staged).unwrap();
    fs::write(staged.join(".tmpA1b2C3"), b"half a checkpoint").unwrap();
    fs::write(staged.join("journal.log"), b"not a frame").unwrap();
    assert_format(MetaStore::open(&root), "a staged meta/ is no store");
    MetaStore::create(&root).unwrap();
    assert_fresh_meta(&root);
    assert!(!Layout::new(&root).meta_path(".tmpA1b2C3").exists());
}

#[test]
fn create_lays_out_meta_and_open_requires_it() {
    let (_dir, root) = new_root();
    assert_format(MetaStore::open(&root), "open before create");
    let store = MetaStore::create(&root).unwrap();
    assert_fresh_meta(&root);
    assert_format(MetaStore::create(&root), "a second create");

    let reopened = MetaStore::open(&root).unwrap();
    for store in [&store, &reopened] {
        let read = store.read().unwrap();
        assert_eq!(read.count_changesets().unwrap(), 0);
        assert_eq!(read.list_refs().unwrap(), []);
        assert_eq!(read.journal_after(None, 10).unwrap(), []);
    }
}

#[test]
fn checkpoint_is_batched_every_32_frames() {
    let (_dir, root, store) = new_store();
    for n in 0..31 {
        assert!(!insert(&store, &[cs(n, &[])]).checkpointed, "frame {n}");
        assert_eq!(ckpt(&root), 0, "frame {n}");
    }
    assert!(
        insert(&store, &[cs(31, &[])]).checkpointed,
        "the 32nd frame past it"
    );
    let first = ckpt(&root);
    assert_eq!(first, log_len(&root));

    assert!(!insert(&store, &[cs(32, &[])]).checkpointed);
    assert_eq!(ckpt(&root), first);
    store.flush().unwrap();
    let flushed = ckpt(&root);
    assert_eq!(
        flushed,
        log_len(&root),
        "flush checkpoints the frame past it"
    );
    store.flush().unwrap();
    assert_eq!(ckpt(&root), flushed, "nothing left to flush");
    assert_eq!(store.read().unwrap().count_changesets().unwrap(), 33);
}

#[test]
fn ref_versions_count_target_changes_and_replay_never_bumps_twice() {
    let (_dir, root, store) = new_store();
    insert(&store, &[cs(1, &[]), cs(2, &[1]), cs(3, &[2])]);
    cas(&store, "main", None, Some(1));
    // Two moves in one write: compared against the queued one, written once.
    let mut write = store.write().unwrap();
    write
        .cas_ref("main", Some(id(1)), Some(id(2)), ACTOR)
        .unwrap();
    let stale = write
        .cas_ref("main", Some(id(1)), Some(id(3)), ACTOR)
        .unwrap();
    assert_eq!(
        stale,
        CasResult::Mismatch {
            actual: Some(id(2))
        }
    );
    write
        .cas_ref("main", Some(id(2)), Some(id(3)), ACTOR)
        .unwrap();
    assert_eq!(write.commit().unwrap().moved_refs, 2);
    assert_eq!(ref_file(&root, "main"), Some(ref_content(3, 2)));
    // Moves that end where they began leave the file alone.
    let mut write = store.write().unwrap();
    write
        .cas_ref("main", Some(id(3)), Some(id(1)), ACTOR)
        .unwrap();
    write
        .cas_ref("main", Some(id(1)), Some(id(3)), ACTOR)
        .unwrap();
    write.commit().unwrap();
    assert_eq!(ref_file(&root, "main"), Some(ref_content(3, 2)));

    // A dead writer's move past the checkpoint: readers count it once, and replay writes
    // it once, however many writes replay it.
    append_unmaterialized(&root, &[ref_move("main", Some(3), Some(1), "crashed/1", 7)]);
    assert_eq!(
        store.read().unwrap().get_ref("main").unwrap(),
        Some(ref_state(1, 3))
    );
    for _ in 0..2 {
        store.write().unwrap().commit().unwrap();
        assert_eq!(ref_file(&root, "main"), Some(ref_content(1, 3)));
        assert_eq!(
            store.read().unwrap().get_ref("main").unwrap(),
            Some(ref_state(1, 3))
        );
    }
    let log = store.read().unwrap().reflog("main", 10).unwrap();
    let moves: Vec<_> = log.iter().map(|entry| (entry.old, entry.new)).collect();
    let want = [
        (Some(id(3)), Some(id(1))),
        (Some(id(1)), Some(id(3))),
        (Some(id(3)), Some(id(1))),
        (Some(id(2)), Some(id(3))),
        (Some(id(1)), Some(id(2))),
        (None, Some(id(1))),
    ];
    assert_eq!(moves, want);
    // Deleted, then created again: the version starts over.
    cas(&store, "main", Some(1), None);
    cas(&store, "main", None, Some(2));
    assert_eq!(
        store.read().unwrap().get_ref("main").unwrap(),
        Some(ref_state(2, 1))
    );
}

#[test]
fn crash_mid_materialization_is_replayed() {
    let (_dir, root, store) = new_store();
    insert(&store, &[cs(1, &[])]);
    cas(&store, "main", None, Some(1));
    // A writer that died after its frame and the first two files of its index record:
    // parents.log and par.col hold record 1, the other columns do not, and the ref file
    // still names changeset 1.
    let records = [
        changeset_record(&cs(2, &[1]), 2),
        ref_move("main", Some(1), Some(2), "crashed/1", 7),
    ];
    append_unmaterialized(&root, &records);
    let meta = Layout::new(&root).meta_dir();
    append_bytes(meta.join("parents.log"), &id(1).0.0);
    append_bytes(meta.join("idx/par.col"), &1_u64.to_le_bytes()); // offset 0, one parent

    let read = store.read().unwrap();
    assert_eq!(read.changeset(id(2)).unwrap(), Some(stored(2, &[1], 1, 2)));
    assert_eq!(read.get_ref("main").unwrap(), Some(ref_state(2, 2)));
    drop(read);

    // The next writer cuts the torn record and replays the frame.
    store.write().unwrap().commit().unwrap();
    let index = index(&root);
    assert_eq!(index.len(), 2);
    assert_eq!(index.get(1).unwrap(), stored(2, &[1], 1, 2));
    assert_eq!(file_len(meta.join("parents.log")), 32);
    assert_eq!(file_len(meta.join("idx/par.col")), 16);
    assert_eq!(ref_file(&root, "main"), Some(ref_content(2, 2)));
}

/// Sets its flag when dropped, also while a panic unwinds.
struct SetOnDrop<'a>(&'a AtomicBool);

impl Drop for SetOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[test]
fn snapshots_stay_consistent_while_a_writer_commits() {
    // Each commit inserts changeset n and moves main to it, so main at version n must name
    // changeset n, and the snapshot must hold it.
    const MOVES: u64 = 100;
    let (_dir, _root, store) = new_store();
    let done = AtomicBool::new(false);
    thread::scope(|scope| {
        let readers: Vec<_> = (0..4)
            .map(|_| {
                scope.spawn(|| {
                    let mut seen = 0;
                    while !done.load(Ordering::Acquire) {
                        let read = store.read().unwrap();
                        let Some(main) = read.get_ref("main").unwrap() else {
                            continue;
                        };
                        assert_eq!(main.target, id(main.version), "{main:?}");
                        let target = read.changeset(main.target).unwrap();
                        assert!(target.is_some(), "main's target is in the snapshot");
                        assert!(read.count_changesets().unwrap() >= main.version);
                        seen = seen.max(main.version);
                    }
                    seen
                })
            })
            .collect();
        {
            // Stops the readers however the writes end: a panic here must not leave them
            // spinning, or the scope would never join them.
            let _stop = SetOnDrop(&done);
            for n in 1..=MOVES {
                let expected = if n == 1 { None } else { Some(id(n - 1)) };
                let mut write = store.write().unwrap();
                write.insert_changeset(cs(n, &[])).unwrap();
                write.cas_ref("main", expected, Some(id(n)), ACTOR).unwrap();
                write.commit().unwrap();
            }
        }
        for reader in readers {
            assert!(reader.join().unwrap() > 0, "every reader saw main move");
        }
    });
}

#[test]
fn write_times_out_as_busy() {
    let (_dir, root, store) = new_store();
    let held = store.write().unwrap();
    let impatient = MetaStore::open(&root)
        .unwrap()
        .with_lock_timeout(Duration::from_millis(100));
    let started = Instant::now();
    assert!(matches!(impatient.write(), Err(StoreError::Busy)));
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_millis(100),
        "gave up after {waited:?}"
    );
    drop(held);
    // A fresh handle with the default timeout: the abandoned wait lets the lock go as soon as
    // it gets it, but a 100 ms timeout need not cover a helper thread scheduled late.
    MetaStore::open(&root)
        .unwrap()
        .write()
        .unwrap()
        .commit()
        .unwrap();
}

#[test]
fn snapshot_reads_the_refs_again_after_a_commit_it_did_not_see() {
    let (_dir, root, store) = new_store();
    insert(&store, &[cs(1, &[])]);
    cas(&store, "main", None, Some(1));
    // A reader whose first read of main's file comes right after another process committed
    // changeset 2 and moved main to it: a frame that its look at the journal did not see.
    let writer_root = root.clone();
    let (fs, _calls) = RecordingFs::new();
    let fs = fs.with_hook("main", move || {
        let writer = MetaStore::open(&writer_root).unwrap();
        let mut write = writer.write().unwrap();
        write.insert_changeset(cs(2, &[1])).unwrap();
        write
            .cas_ref("main", Some(id(1)), Some(id(2)), ACTOR)
            .unwrap();
        write.commit().unwrap();
    });
    let reader = MetaStore::open_with_fs(&root, Arc::new(fs)).unwrap();

    let read = reader.read().unwrap();
    assert_eq!(read.get_ref("main").unwrap(), Some(ref_state(2, 2)));
    let target = read.changeset(id(2)).unwrap();
    assert_eq!(
        target,
        Some(stored(2, &[1], 1, 2)),
        "main's target is in the snapshot"
    );
    assert_eq!(read.count_changesets().unwrap(), 2);
    let log = read.reflog("main", 10).unwrap();
    let moves: Vec<_> = log.iter().map(|entry| (entry.old, entry.new)).collect();
    assert_eq!(moves, [(Some(id(1)), Some(id(2))), (None, Some(id(1)))]);
}

/// A hook for [`RecordingFs`]: the name of the file whose first `read` runs it, and what it
/// runs.
type Hook = (&'static str, Box<dyn FnOnce() + Send>);

/// Which call a [`RecordingFs`] fails, while set: the call as it records it,
/// `"<method> <file name>"`. Clones share the setting.
#[derive(Clone, Default)]
struct Failing(Arc<Mutex<Option<&'static str>>>);

impl Failing {
    fn set(&self, call: &'static str) {
        *self.0.lock().unwrap() = Some(call);
    }

    fn clear(&self) {
        *self.0.lock().unwrap() = None;
    }

    /// An injected error if `call` is the failing call; the call then does nothing.
    fn check(&self, call: &str) -> io::Result<()> {
        match *self.0.lock().unwrap() {
            Some(failing) if failing == call => Err(io::Error::other(format!("injected: {call}"))),
            _ => Ok(()),
        }
    }
}

/// [`RealFs`] that records each call as `"<method> <file name>"`, can run a hook once, at the
/// first `read` of a file of a given name, before that read, fails the call its [`Failing`]
/// names, and counts its `sync_data` calls in flight ([`Rendezvous`]).
struct RecordingFs {
    calls: Arc<Mutex<Vec<String>>>,
    hook: Mutex<Option<Hook>>,
    failing: Failing,
    syncs: Rendezvous,
}

impl RecordingFs {
    /// A recorder, and a function that returns the calls made since it last ran.
    fn new() -> (RecordingFs, impl Fn() -> Vec<String>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let fs = RecordingFs {
            calls: Arc::clone(&calls),
            hook: Mutex::new(None),
            failing: Failing::default(),
            syncs: Rendezvous::default(),
        };
        (fs, move || std::mem::take(&mut *calls.lock().unwrap()))
    }

    /// What counts this recorder's `sync_data` calls in flight.
    fn syncs(&self) -> Rendezvous {
        self.syncs.clone()
    }

    /// This recorder, running `hook` at the first `read` of a file named `file`.
    fn with_hook(self, file: &'static str, hook: impl FnOnce() + Send + 'static) -> Self {
        *self.hook.lock().unwrap() = Some((file, Box::new(hook)));
        self
    }

    /// What sets the call this recorder fails.
    fn failing(&self) -> Failing {
        self.failing.clone()
    }

    /// Records the call, then fails it if it is the failing one.
    fn record(&self, method: &str, path: &Path) -> io::Result<()> {
        let name = path.file_name().unwrap().to_string_lossy();
        let call = format!("{method} {name}");
        self.calls.lock().unwrap().push(call.clone());
        self.failing.check(&call)
    }
}

impl FsOps for RecordingFs {
    fn create_dir_all(&self, p: &Path) -> io::Result<()> {
        self.record("create_dir_all", p)?;
        RealFs.create_dir_all(p)
    }

    fn write_durable_temp(&self, dir: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
        self.record("write_durable_temp", dir)?;
        RealFs.write_durable_temp(dir, bytes)
    }

    fn link_noclobber(&self, from: &Path, to: &Path) -> io::Result<LinkOutcome> {
        self.record("link_noclobber", to)?;
        RealFs.link_noclobber(from, to)
    }

    fn fsync_dir(&self, p: &Path) -> io::Result<()> {
        self.record("fsync_dir", p)?;
        RealFs.fsync_dir(p)
    }

    fn read(&self, p: &Path) -> io::Result<Option<Vec<u8>>> {
        self.record("read", p)?;
        let hook = {
            let mut slot = self.hook.lock().unwrap();
            let due = slot
                .as_ref()
                .is_some_and(|(file, _)| p.file_name() == Some(file.as_ref()));
            if due { slot.take() } else { None }
        };
        if let Some((_, hook)) = hook {
            hook();
        }
        RealFs.read(p)
    }

    fn exists(&self, p: &Path) -> io::Result<bool> {
        self.record("exists", p)?;
        RealFs.exists(p)
    }

    fn remove(&self, p: &Path) -> io::Result<()> {
        self.record("remove", p)?;
        RealFs.remove(p)
    }

    fn append_durable(&self, path: &Path, bytes: &[u8]) -> io::Result<u64> {
        self.record("append_durable", path)?;
        RealFs.append_durable(path, bytes)
    }

    fn file_len(&self, path: &Path) -> io::Result<u64> {
        self.record("file_len", path)?;
        RealFs.file_len(path)
    }

    fn truncate(&self, path: &Path, len: u64) -> io::Result<()> {
        self.record("truncate", path)?;
        RealFs.truncate(path, len)
    }

    fn atomic_replace(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        self.record("atomic_replace", path)?;
        RealFs.atomic_replace(path, bytes)
    }

    fn sync_data(&self, path: &Path) -> io::Result<()> {
        self.record("sync_data", path)?;
        self.syncs.around(|| RealFs.sync_data(path))
    }

    fn append(&self, path: &Path, bytes: &[u8]) -> io::Result<u64> {
        self.record("append", path)?;
        RealFs.append(path, bytes)
    }

    fn rename_durable(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.record("rename_durable", to)?;
        RealFs.rename_durable(from, to)
    }
}

#[test]
fn write_and_commit_make_the_documented_fs_calls() {
    let (_dir, root) = new_root();
    let (fs, calls) = RecordingFs::new();
    let mut store = MetaStore::create_with_fs(&root, Arc::new(fs)).unwrap();
    // One thread: the index's five syncs go one after another, in write order, as crash
    // sweeps that count calls need them.
    store.set_parallelism(1);
    assert_eq!(calls(), CREATE_CALLS);
    let tail_check = ["file_len journal.log", "file_len journal.off"];
    // A reader's snapshot of the journal, journal.off measured first: the write's replay
    // reads the frames past the checkpoint so.
    let snapshot = ["file_len journal.off", "file_len journal.log"];
    let frame = ["append_durable journal.log", "append_durable journal.off"];
    let record = [
        "append parents.log",
        "append par.col",
        "append gen.col",
        "append dh.col",
        "append ids.col",
    ];
    let sync = [
        "sync_data parents.log",
        "sync_data par.col",
        "sync_data gen.col",
        "sync_data dh.col",
        "sync_data ids.col",
        "atomic_replace journal.ckpt",
    ];

    let mut write = store.write().unwrap();
    assert_eq!(calls(), [&tail_check[..], &snapshot].concat());
    write.insert_changeset(cs(1, &[])).unwrap();
    assert_eq!(calls(), Vec::<String>::new(), "queueing touches nothing");
    write.commit().unwrap();
    assert_eq!(calls(), [&frame[..], &record].concat());

    // A ref move: cas_ref reads the ref file; the commit reads it again and replaces it,
    // after the index record.
    let mut write = store.write().unwrap();
    write.insert_changeset(cs(2, &[1])).unwrap();
    write.cas_ref("main", None, Some(id(2)), ACTOR).unwrap();
    assert_eq!(
        calls(),
        [&tail_check[..], &snapshot, &["read main"]].concat()
    );
    write.commit().unwrap();
    let replace_main = ["read main", "atomic_replace main"];
    assert_eq!(calls(), [&frame[..], &record, &replace_main].concat());

    // From now on each write's replay reads the files of the refs moved past the checkpoint.
    // The first release ref creates meta/refs/release/; a delete removes the file.
    cas(&store, "release/v1", None, Some(1));
    let replay = ["read main", "read v1"];
    let create = [
        "read v1",
        "create_dir_all release",
        "fsync_dir refs",
        "atomic_replace v1",
    ];
    assert_eq!(
        calls(),
        [&tail_check[..], &snapshot, &replay, &frame, &create].concat()
    );
    cas(&store, "release/v1", Some(1), None);
    let delete = ["read v1", "remove v1", "fsync_dir release"];
    let replay_and_cas = ["read main", "read v1", "read v1"];
    assert_eq!(
        calls(),
        [&tail_check[..], &snapshot, &replay_and_cas, &frame, &delete].concat()
    );

    // Readers write nothing: they measure the journal (journal.off first) before and after
    // reading the ref files, and read the rest through their own handles. Here every frame is
    // past the checkpoint, so the reflog and the journal cursor read no frames from the
    // journal: the snapshot holds them all.
    let read = store.read().unwrap();
    read.changeset(id(2)).unwrap();
    read.list_refs().unwrap();
    read.reflog("main", 10).unwrap();
    read.journal_after(None, 10).unwrap();
    drop(read);
    assert_eq!(calls(), [&snapshot[..], &["read main"], &snapshot].concat());

    // Frames 4..=30, then the 32nd frame past the checkpoint: it syncs and checkpoints.
    for n in 3..30 {
        insert(&store, &[cs(n, &[])]);
    }
    calls();
    assert!(insert(&store, &[cs(30, &[])]).checkpointed);
    assert_eq!(
        calls(),
        [&tail_check[..], &snapshot, &replay, &frame, &record, &sync].concat()
    );

    // A flush with nothing past the checkpoint; then with one frame past it.
    store.flush().unwrap();
    assert_eq!(calls(), [&tail_check[..], &snapshot].concat());
    insert(&store, &[cs(31, &[])]);
    calls();
    store.flush().unwrap();
    assert_eq!(calls(), [&tail_check[..], &snapshot, &sync].concat());
}

/// The child of [`a_batched_sync_with_no_thread_to_spare_syncs_on_the_committer`]: 32
/// transactions at the default parallelism, the last of which syncs the index after its commit
/// point, while RLIMIT_NPROC leaves no thread to spawn.
#[cfg(target_os = "linux")]
fn commit_with_no_thread_to_spare(root: &Utf8Path) {
    if thread::Builder::new().spawn(|| ()).is_ok() {
        // The limit does not bind (root, say): nothing to check here.
        println!("{REPORT}threads can be spawned");
        return;
    }
    let store = MetaStore::create(root).unwrap();
    for n in 0..31 {
        insert(&store, &[cs(n, &[])]);
    }
    assert!(insert(&store, &[cs(31, &[])]).checkpointed);
    assert_eq!(ckpt(root), log_len(root));
    println!("{REPORT}checkpointed");
}

#[cfg(target_os = "linux")]
#[test]
fn a_batched_sync_with_no_thread_to_spare_syncs_on_the_committer() {
    const TEST: &str = "a_batched_sync_with_no_thread_to_spare_syncs_on_the_committer";
    if let Some(root) = std::env::var_os(NO_THREADS_ROOT) {
        let root = Utf8PathBuf::from_path_buf(root.into()).unwrap();
        commit_with_no_thread_to_spare(&root);
        return;
    }
    let (_dir, root) = new_root();
    // `ulimit -u 1`: this user's processes and threads are past the limit, so the child can
    // spawn no thread (the test harness then runs the test on its main thread).
    let child = Command::new("bash")
        .arg("-c")
        .arg("ulimit -u 1 && exec \"$0\" \"$@\"")
        .arg(std::env::current_exe().unwrap())
        .args(["--exact", TEST, "--nocapture"])
        .env(NO_THREADS_ROOT, &root)
        .output()
        .unwrap();
    let report = report(0, &child);
    assert!(
        report == "checkpointed" || report == "threads can be spawned",
        "{report}"
    );
}

/// The five index syncs, sorted.
const INDEX_SYNCS: [&str; 5] = [
    "sync_data dh.col",
    "sync_data gen.col",
    "sync_data ids.col",
    "sync_data par.col",
    "sync_data parents.log",
];

/// `calls`, split into what came before the last `count`, and those sorted.
fn sorted_tail(mut calls: Vec<String>, count: usize) -> (Vec<String>, Vec<String>) {
    let mut tail = calls.split_off(calls.len() - count);
    tail.sort();
    (calls, tail)
}

#[test]
fn the_index_syncs_its_five_files_at_once_and_the_checkpoint_waits_for_them() {
    let (_dir, root) = new_root();
    let (fs, calls) = RecordingFs::new();
    let (syncs, fail) = (fs.syncs(), fs.failing());
    // The default parallelism: 5.
    let store = MetaStore::create_with_fs(&root, Arc::new(fs)).unwrap();
    for n in 0..31 {
        insert(&store, &[cs(n, &[])]); // frames 0..=30
    }
    calls();
    let before = [
        "file_len journal.log",
        "file_len journal.off",
        "file_len journal.off",
        "file_len journal.log",
        "append_durable journal.log",
        "append_durable journal.off",
        "append parents.log",
        "append par.col",
        "append gen.col",
        "append dh.col",
        "append ids.col",
    ];

    // The 32nd frame past the checkpoint syncs the index: each of its five syncs waits until
    // all five are in flight, which only syncs made at once can be. The checkpoint moves once
    // all five have returned.
    syncs.arm(5);
    assert!(insert(&store, &[cs(31, &[])]).checkpointed);
    assert_eq!(syncs.most(), 5, "the five syncs were in flight together");
    let (made, last) = sorted_tail(calls(), 1);
    assert_eq!(last, ["atomic_replace journal.ckpt"]);
    let (made, index_syncs) = sorted_tail(made, 5);
    assert_eq!(index_syncs, INDEX_SYNCS);
    assert_eq!(made, before);
    assert_eq!(ckpt(&root), log_len(&root), "the checkpoint is at the end");

    // A sync that fails while the others run: all five are made, the error comes back after
    // the commit point, and the checkpoint stays where it was.
    syncs.arm(0);
    for n in 32..63 {
        insert(&store, &[cs(n, &[])]); // 31 frames past the checkpoint
    }
    let ckpt_before = ckpt(&root);
    calls();
    fail.set("sync_data gen.col");
    let mut write = store.write().unwrap();
    write.insert_changeset(cs(63, &[])).unwrap();
    match write.commit() {
        Err(StoreError::PostCommit {
            frame_seq: 63,
            source,
        }) => {
            assert!(matches!(*source, StoreError::Io(_)), "{source:?}");
        }
        other => panic!("expected PostCommit at frame 63, got {other:?}"),
    }
    fail.clear();
    let made = calls();
    let mut index_syncs: Vec<&str> = made
        .iter()
        .map(String::as_str)
        .filter(|call| call.starts_with("sync_data"))
        .collect();
    index_syncs.sort_unstable();
    assert_eq!(index_syncs, INDEX_SYNCS, "every file's sync was made");
    assert!(
        !made
            .iter()
            .any(|call| call == "atomic_replace journal.ckpt"),
        "no checkpoint after a failed sync: {made:?}"
    );
    assert_eq!(ckpt(&root), ckpt_before);
    store.flush().unwrap();
    assert_eq!(ckpt(&root), log_len(&root));
}
