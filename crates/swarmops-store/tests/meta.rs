//! The native metadata store through its facade, [`MetaStore`]: changesets and refs written
//! in write transactions, lock-free snapshots that overlay the frames past the checkpoint,
//! replay after a crash, the batched checkpoint and compaction, and writers in several
//! processes at once.
//!
//! Every test owns a fresh store root (`tempfile::tempdir()`), so tests are independent and
//! may run in any order or in parallel. The multi-process tests run this test binary again
//! as their children (selected by [`CHILD_ROOT`]), all on the real filesystem.

use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_core::{ChangesetId, ObjectId, ObjectKind, hash_object};
#[cfg(feature = "test-hooks")]
use swarmops_store::{CRASH_AT_ENV, CrashAtFs, FailAt, FailAtFs, FailMode, InjectedError};
use swarmops_store::{
    CasResult, ChangesetMeta, CommitInfo, Frame, FsOps, Index, InsertOutcome, Journal,
    JournalEntry, Layout, LimitKind, LogCut, MetaLock, MetaRead, MetaStore, RealFs, Record,
    Recovered, RefMoveEntry, RefState, StoreError, StoreFormat,
};
use swarmops_testkit::child::{CHILD_TIMEOUT, Child, SIGABRT, run, test_args, this_test};
use swarmops_testkit::faults::checked;
use swarmops_testkit::fs::{Call, Failing, NoSyncFs, RecordingFs, Watch, Watched, When, copy_dir};
#[cfg(feature = "test-hooks")]
use swarmops_testkit::store::sweep::{
    SWEEP_PLAN, SWEEP_ROOT, Said, Template, assert_recovers, calls_of, changeset, copy_of,
    run_sweep_child, said, sweep_child, sweep_template, swept,
};
use swarmops_testkit::store::temp_dir as new_root;
use swarmops_testkit::store::{append_bytes, file_len, set_len};
use swarmops_testkit::{per_format, test_name};
use tempfile::TempDir;

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

/// Set only in the child of `a_batched_sync_with_no_thread_to_spare_syncs_on_the_committer`,
/// which runs with no thread to spare: the store root.
#[cfg(target_os = "linux")]
const NO_THREADS_ROOT: &str = "SWARMOPS_TEST_META_NO_THREADS_ROOT";

/// Set only in the children of `crash_at_any_create_call_leaves_a_store_open_or_create_takes`:
/// the store root.
#[cfg(feature = "test-hooks")]
const CREATE_CRASH_ROOT: &str = "SWARMOPS_TEST_META_CREATE_CRASH_ROOT";

/// Set only in the children of `the_test_hooks_build_the_filesystem_they_name`: a directory of
/// their own.
#[cfg(feature = "test-hooks")]
const HOOKS_ROOT: &str = "SWARMOPS_TEST_META_HOOKS_ROOT";

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

/// A fresh store; its directory lives as long as the `TempDir`.
fn new_store() -> (TempDir, Utf8PathBuf, MetaStore) {
    let (dir, root) = new_root();
    let store = create_checked(&root).unwrap();
    (dir, root, store)
}

/// [`MetaStore::create`], the store's writes checked against its ordering rules
/// ([`checked`]): every test here runs under the order checker, but where it says otherwise.
fn create_checked(root: &Utf8Path) -> Result<MetaStore, StoreError> {
    MetaStore::create_with_fs(root, checked(RealFs::new()))
}

/// [`MetaStore::open`], the store's writes checked against its ordering rules ([`checked`]).
fn open_checked(root: &Utf8Path) -> Result<MetaStore, StoreError> {
    MetaStore::open_with_fs(root, checked(RealFs::new()))
}

/// The id of changeset `n`: a real hash, so the ids look like a store's.
fn id(n: u64) -> ChangesetId {
    ChangesetId::new(hash_object(ObjectKind::Changeset, &n.to_le_bytes()))
}

/// The delta hash of changeset `n`.
fn delta(n: u64) -> [u8; 32] {
    hash_object(ObjectKind::Blob, &n.to_le_bytes()).0
}

/// Changeset `n` with parents `parents`, as a caller hands it to `insert_changeset`: seq and
/// generation 0, since the store assigns both.
fn cs(n: u64, parents: &[u64]) -> ChangesetMeta {
    ChangesetMeta::new(
        id(n),
        0,
        0,
        delta(n),
        parents.iter().map(|&parent| id(parent)).collect(),
    )
}

/// `cs(n, parents)` as the store holds it: at `seq`, with `generation`.
fn stored(n: u64, parents: &[u64], seq: u64, generation: u64) -> ChangesetMeta {
    let mut meta = cs(n, parents);
    meta.seq = seq;
    meta.generation = generation;
    meta
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
    RefState::new(id(target), version)
}

/// A journal handle of its own on the store at `root`, as another process would open it.
fn journal(root: &Utf8Path) -> Journal {
    Journal::open(&Layout::new(root), Arc::new(RealFs::new())).unwrap()
}

/// An index handle of its own on the store at `root`.
fn index(root: &Utf8Path) -> Index {
    Index::open(&Layout::new(root), Arc::new(RealFs::new())).unwrap()
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
    let journal = Journal::open(&layout, Arc::new(RealFs::new())).unwrap();
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
fn assert_format_required<T: std::fmt::Debug>(got: Result<T, StoreError>, name: &str) {
    match got {
        Err(StoreError::FormatRequired {
            name: refused,
            needs: 2,
            store: 1,
        }) => assert_eq!(refused, name),
        other => panic!("expected FormatRequired({name:?}), got {other:?}"),
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
    let reopened = open_checked(&root).unwrap();
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
    let nothing = CommitInfo::new(1, 0, 0, false);
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

/// `count` root changesets, from changeset `first` on.
fn roots(first: u64, count: u64) -> Vec<ChangesetMeta> {
    (first..first + count).map(|n| cs(n, &[])).collect()
}

#[test]
fn insert_changesets_batch_is_one_frame() {
    let (_dir, root, store) = new_store();
    let inserted = |seq| InsertOutcome::Inserted { seq };
    // Each on the one before it; the seq and generation a caller passes are ignored.
    let mut first = cs(1, &[]);
    (first.seq, first.generation) = (7, 99);
    let mut write = store.write().unwrap();
    let outcomes = write
        .insert_changesets_batch(vec![first, cs(2, &[1]), cs(3, &[2])])
        .unwrap();
    assert_eq!(outcomes, [inserted(0), inserted(1), inserted(2)]);
    let want = CommitInfo::new(0, 3, 0, false);
    assert_eq!(write.commit().unwrap(), want);
    let records = [
        changeset_record(&cs(1, &[]), 1),
        changeset_record(&cs(2, &[1]), 2),
        changeset_record(&cs(3, &[2]), 3),
    ];
    let frames = frames(&root);
    assert_eq!(frames.len(), 1, "one frame for the batch");
    assert_eq!(frames[0].records, records, "generations 1 to 3");
    let entry = |n| JournalEntry::new(0, "changeset", id(n).0.0.to_vec(), 0);
    let entries = store.read().unwrap().journal_after(None, 10).unwrap();
    assert_eq!(entries, [entry(1), entry(2), entry(3)]);

    // A second batch builds on the store's changesets and on its own: a merge of both.
    let mut write = store.write().unwrap();
    let outcomes = write
        .insert_changesets_batch(vec![cs(4, &[1]), cs(5, &[3, 4])])
        .unwrap();
    assert_eq!(outcomes, [inserted(3), inserted(4)]);
    assert_eq!(write.commit().unwrap().inserted, 2);
    let want = [
        stored(1, &[], 0, 1),
        stored(2, &[1], 1, 2),
        stored(3, &[2], 2, 3),
        stored(4, &[1], 3, 2),
        stored(5, &[3, 4], 4, 4),
    ];
    let reopened = open_checked(&root).unwrap();
    for store in [&store, &reopened] {
        let read = store.read().unwrap();
        for meta in &want {
            assert_eq!(read.changeset(meta.id).unwrap().as_ref(), Some(meta));
        }
        assert_eq!(read.count_changesets().unwrap(), 5);
    }
}

#[test]
fn insert_changesets_batch_queues_nothing_on_an_error() {
    let (_dir, root, store) = new_store();
    insert(&store, &[cs(1, &[])]);
    let nothing = CommitInfo::new(1, 0, 0, false);
    // The third changeset's parent is nowhere: the two valid ones before it are not queued
    // either.
    let mut write = store.write().unwrap();
    match write.insert_changesets_batch(vec![cs(2, &[1]), cs(3, &[2]), cs(4, &[42])]) {
        Err(StoreError::MissingParent(parent)) => assert_eq!(parent, id(42)),
        other => panic!("expected MissingParent, got {other:?}"),
    }
    assert_eq!(write.commit().unwrap(), nothing, "no frame");
    // More changesets than one frame holds.
    let mut write = store.write().unwrap();
    let refused = write.insert_changesets_batch(roots(2, 65_536));
    assert_limit(refused, LimitKind::Records, 65_535, 65_536);
    assert_eq!(write.commit().unwrap(), nothing, "no frame");

    assert_eq!(frames(&root).len(), 1);
    let read = store.read().unwrap();
    assert_eq!(read.count_changesets().unwrap(), 1);
    for n in [2, 3, 65_537] {
        assert_eq!(read.changeset(id(n)).unwrap(), None, "{n}");
    }
}

/// What a transaction queued before a batch stays queued whatever the batch does, and counts
/// toward the records its frame holds; a changeset a batch names twice counts once. An error
/// leaves nothing of its own call queued: the same changesets, queued afterwards, take the seqs
/// that follow what was queued before.
#[test]
fn insert_changesets_batch_keeps_what_was_queued_before_it() {
    let (_dir, root, store) = new_store();
    insert(&store, &[cs(1, &[])]);
    let inserted = |seq| InsertOutcome::Inserted { seq };
    let mut write = store.write().unwrap();
    assert_eq!(write.insert_changeset(cs(2, &[1])).unwrap(), inserted(1));
    // Beside the one queued, 65,535 more are a record too many.
    let refused = write.insert_changesets_batch(roots(10, 65_535));
    assert_limit(refused, LimitKind::Records, 65_535, 65_536);
    // An error at a batch's third changeset, the first two valid.
    let batch = |third| vec![cs(3, &[2]), cs(4, &[3]), third];
    let mut too_many = cs(5, &[]);
    too_many.parents = vec![id(1); 256];
    let refused = write.insert_changesets_batch(batch(too_many));
    assert_limit(refused, LimitKind::Parents, 255, 256);
    match write.insert_changesets_batch(batch(cs(5, &[4, 3, 4]))) {
        Err(StoreError::DuplicateParent(parent)) => assert_eq!(parent, id(4)),
        other => panic!("expected DuplicateParent, got {other:?}"),
    }
    match write.insert_changesets_batch(batch(cs(5, &[4, 42]))) {
        Err(StoreError::MissingParent(parent)) => assert_eq!(parent, id(42)),
        other => panic!("expected MissingParent, got {other:?}"),
    }
    let outcomes = write.insert_changesets_batch(batch(cs(5, &[4]))).unwrap();
    assert_eq!(outcomes, [inserted(2), inserted(3), inserted(4)]);
    assert_eq!(write.commit().unwrap().inserted, 4);
    let chained: Vec<Record> = (2..=5)
        .map(|n| changeset_record(&cs(n, &[n - 1]), n))
        .collect();
    assert_eq!(frames(&root)[1].records, chained);

    // A frame's worth exactly fits: changeset 6 queued, 65,534 new ones and one named twice.
    let mut write = store.write().unwrap();
    assert_eq!(write.insert_changeset(cs(6, &[5])).unwrap(), inserted(5));
    let mut full = roots(10, 65_534);
    full.push(cs(10, &[]));
    let outcomes = write.insert_changesets_batch(full).unwrap();
    assert_eq!(outcomes.len(), 65_535);
    assert_eq!(outcomes[0], inserted(6));
    assert_eq!(outcomes[65_533], inserted(65_539));
    assert_eq!(outcomes[65_534], InsertOutcome::AlreadyPresent);
    let refused = write.insert_changeset(cs(7, &[6]));
    assert_limit(refused, LimitKind::Records, 65_535, 65_536);
}

#[test]
fn insert_changesets_batch_reports_what_is_present() {
    let (_dir, root, store) = new_store();
    insert(&store, &[cs(1, &[])]);
    let present = InsertOutcome::AlreadyPresent;
    let inserted = |seq| InsertOutcome::Inserted { seq };
    let mut write = store.write().unwrap();
    assert_eq!(write.insert_changeset(cs(2, &[1])).unwrap(), inserted(1));
    // In the store, queued before, new, named twice in the batch, new.
    let batch = vec![
        cs(1, &[]),
        cs(2, &[1]),
        cs(3, &[2]),
        cs(3, &[2]),
        cs(4, &[3, 1]),
    ];
    let outcomes = write.insert_changesets_batch(batch).unwrap();
    assert_eq!(
        outcomes,
        [present, present, inserted(2), present, inserted(3)]
    );
    // A batch of nothing new, or of nothing, queues nothing.
    let again = write.insert_changesets_batch(vec![cs(4, &[3, 1]), cs(1, &[])]);
    assert_eq!(again.unwrap(), [present, present]);
    let none = write.insert_changesets_batch(Vec::new()).unwrap();
    assert_eq!(none, Vec::<InsertOutcome>::new());
    assert_eq!(write.commit().unwrap().inserted, 3);
    let frames = frames(&root);
    assert_eq!(frames.len(), 2);
    let queued_once = [
        changeset_record(&cs(2, &[1]), 2),
        changeset_record(&cs(3, &[2]), 3),
        changeset_record(&cs(4, &[3, 1]), 4),
    ];
    assert_eq!(frames[1].records, queued_once, "each changeset queued once");
}

#[test]
fn a_frame_is_materialized_with_one_append_batch() {
    let (_dir, root) = new_root();
    let (recording, calls) = RecordingFs::new();
    let store = MetaStore::create_with_fs(&root, checked(recording)).unwrap();
    let mut write = store.write().unwrap();
    write
        .insert_changesets_batch(vec![cs(1, &[]), cs(2, &[1]), cs(3, &[2])])
        .unwrap();
    calls();
    write.commit().unwrap();
    let frame = ["append_durable journal.log", "append_durable journal.off"];
    let records = [
        "append parents.log",
        "append par.col",
        "append gen.col",
        "append dh.col",
        "append ids.col",
    ];
    assert_eq!(
        calls(),
        [&frame[..], &records].concat(),
        "five index appends for the frame's three changesets, not fifteen"
    );
    let index = index(&root);
    let want = [
        stored(1, &[], 0, 1),
        stored(2, &[1], 1, 2),
        stored(3, &[2], 2, 3),
    ];
    for (rec, meta) in (0..).zip(&want) {
        assert_eq!(index.get(rec).unwrap(), *meta);
    }
}

/// Replay brings the index up to the frames past the checkpoint a frame at a time, what each
/// frame holds that the index lacks with one `append_batch`: here the last two changesets of a
/// frame of three, whose first a crash left whole in the index, then both of the next frame's.
#[test]
fn replay_appends_what_each_frame_lacks_with_one_append_batch() {
    let (_dir, root, store) = new_store();
    insert(&store, &[cs(1, &[])]);
    drop(store);
    let chained = |n: u64| changeset_record(&cs(n, &[n - 1]), n);
    append_unmaterialized(&root, &[chained(2), chained(3), chained(4)]);
    append_unmaterialized(&root, &[chained(5), chained(6)]);
    let mut crashed = Index::open(&Layout::new(&root), Arc::new(RealFs::new())).unwrap();
    crashed.append(&stored(2, &[1], 1, 2)).unwrap();
    drop(crashed);

    let (recording, calls) = RecordingFs::new();
    let store = MetaStore::open_with_fs(&root, checked(recording)).unwrap();
    let write = store.write().unwrap();
    assert_eq!(write.recovered().records_replayed, 4);
    write.commit().unwrap();
    let appends: Vec<String> = (calls().into_iter())
        .filter(|call| call.starts_with("append "))
        .collect();
    let batch = [
        "append parents.log",
        "append par.col",
        "append gen.col",
        "append dh.col",
        "append ids.col",
    ];
    assert_eq!(appends, [batch, batch].concat(), "one batch for each frame");
    let index = index(&root);
    assert_eq!(index.len(), 6);
    for n in 1..=6 {
        let parents: Vec<u64> = (n - 1..n).filter(|&parent| parent > 0).collect();
        assert_eq!(
            index.get(n - 1).unwrap(),
            stored(n, &parents, n - 1, n),
            "{n}"
        );
    }
}

#[test]
fn cas_ref_create_update_delete_and_mismatch() {
    let (_dir, root, store) = new_store();
    insert(&store, &[cs(1, &[]), cs(2, &[1])]);
    let before = unix_now();
    assert_eq!(
        cas(&store, "release/v1", None, Some(1)),
        CasResult::Applied { old: None }
    );
    let read = store.read().unwrap();
    assert_eq!(read.get_ref("release/v1").unwrap(), Some(ref_state(1, 1)));
    assert_eq!(ref_file(&root, "release/v1"), Some(ref_content(1, 1)));

    // A wrong expectation: the actual target comes back, and nothing is written.
    let frames_before = frames(&root).len();
    let actual_1 = CasResult::Mismatch {
        actual: Some(id(1)),
    };
    assert_eq!(cas(&store, "release/v1", Some(2), Some(2)), actual_1);
    assert_eq!(cas(&store, "release/v1", None, Some(2)), actual_1);
    assert_eq!(
        frames(&root).len(),
        frames_before,
        "no frame for a mismatch"
    );

    let applied = CasResult::Applied { old: Some(id(1)) };
    assert_eq!(cas(&store, "release/v1", Some(1), Some(2)), applied);
    let read = store.read().unwrap();
    assert_eq!(read.get_ref("release/v1").unwrap(), Some(ref_state(2, 2)));
    assert_eq!(ref_file(&root, "release/v1"), Some(ref_content(2, 2)));

    let applied = CasResult::Applied { old: Some(id(2)) };
    assert_eq!(cas(&store, "release/v1", Some(2), None), applied);
    assert_eq!(store.read().unwrap().get_ref("release/v1").unwrap(), None);
    assert_eq!(
        ref_file(&root, "release/v1"),
        None,
        "a deleted ref has no file"
    );
    let actual_none = CasResult::Mismatch { actual: None };
    assert_eq!(cas(&store, "release/v1", Some(2), Some(1)), actual_none);

    let after = unix_now();
    let log = store.read().unwrap().reflog("release/v1", 10).unwrap();
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
    assert_eq!(
        store.read().unwrap().reflog("release/v1", 2).unwrap(),
        log[..2]
    );
    assert_eq!(store.read().unwrap().reflog("release/v1", 0).unwrap(), []);
}

/// A deletion of `main` is refused after the compare, before anything is queued: a stale
/// `expected` is still a mismatch, and `main` keeps its target (the v0.6.0 design D8).
#[test]
fn a_refused_protected_delete_queues_nothing() {
    let (_dir, root, store) = new_store();
    insert(&store, &[cs(1, &[]), cs(2, &[1])]);
    cas(&store, "main", None, Some(1));
    let frames_before = frames(&root).len();

    let mut write = store.write().unwrap();
    let stale = write.cas_ref("main", Some(id(2)), None, ACTOR).unwrap();
    assert_eq!(
        stale,
        CasResult::Mismatch {
            actual: Some(id(1))
        }
    );
    let refused = write.cas_ref("main", Some(id(1)), None, ACTOR);
    assert!(
        matches!(&refused, Err(StoreError::RefProtected(n)) if n == "main"),
        "{refused:?}"
    );
    assert!(write.queued().is_empty(), "{:?}", write.queued());
    write.commit().unwrap();

    assert_eq!(frames(&root).len(), frames_before, "no frame for a refusal");
    let read = store.read().unwrap();
    assert_eq!(read.get_ref("main").unwrap(), Some(ref_state(1, 1)));
    assert_eq!(ref_file(&root, "main"), Some(ref_content(1, 1)));
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
        "main/x",
        "release",
        "release/",
        "release/a/b",
        "release/.",
        "release/..",
        "release/.hidden",
        "release/a b",
        "release/é",
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
    // Names only FORMAT 2 takes: refused as needing it.
    for name in ["master", "Main", "refs/heads/main", "tags/v1"] {
        assert_format_required(write.cas_ref(name, None, Some(id(1)), ACTOR), name);
        assert_format_required(read.get_ref(name), name);
        assert_format_required(read.reflog(name, 1), name);
    }
    let longest = format!("release/{}", "x".repeat(247));
    let good = ["main", "release/v1.0", "release/2026-10_rc.1", &longest];
    for name in good {
        let got = write.cas_ref(name, None, Some(id(1)), ACTOR).unwrap();
        assert_eq!(got, CasResult::Applied { old: None }, "{name}");
    }
    // An actor longer than a ref move can record is refused before anything is queued.
    let actor = "a".repeat(256);
    let refused = write.cas_ref("release/v2", None, Some(id(1)), &actor);
    assert_limit(refused, LimitKind::ActorBytes, 255, 256);
    assert_eq!(write.commit().unwrap().moved_refs, 4);
    let listed = store.read().unwrap().list_refs().unwrap();
    let names: Vec<&str> = listed.iter().map(|(name, _)| name.as_str()).collect();
    let mut sorted = good;
    sorted.sort_unstable();
    assert_eq!(names, sorted);
    assert!(listed.iter().all(|(_, state)| *state == ref_state(1, 1)));
}

/// Inside a write transaction, `get_ref` reads a ref as the transaction's own recovery left it,
/// which is what `cas_ref` compares against: here a crashed writer's whole frame, never listed,
/// moved `main` from changeset 1 to 2, and the next write lists and replays it before it reads.
/// A compare-and-swap that expects what it read is applied.
#[test]
fn a_write_reads_a_ref_as_its_tail_check_left_it() {
    let (_dir, root, store) = new_store();
    insert(&store, &[cs(1, &[]), cs(2, &[1]), cs(3, &[2])]);
    cas(&store, "main", None, Some(1));
    // The crashed writer's frame: whole in journal.log, its entry never in journal.off.
    append_unmaterialized(
        &root,
        &[ref_move("main", Some(1), Some(2), ACTOR, unix_now())],
    );
    let off = Layout::new(&root).journal_off();
    set_len(&off, file_len(&off) - 8);
    assert_eq!(ref_file(&root, "main"), Some(ref_content(1, 1)));

    let mut write = store.write().unwrap();
    assert_eq!(write.recovered().frames_listed, 1, "the frame is listed");
    assert_eq!(write.get_ref("main").unwrap(), Some(id(2)));
    assert_eq!(write.get_ref("release/v1").unwrap(), None, "no such ref");
    let moved = write
        .cas_ref("main", Some(id(2)), Some(id(3)), ACTOR)
        .unwrap();
    assert_eq!(moved, CasResult::Applied { old: Some(id(2)) });
    write.commit().unwrap();
    let read = store.read().unwrap();
    assert_eq!(read.get_ref("main").unwrap(), Some(ref_state(3, 3)));
}

/// `get_ref` reads the target this transaction's last queued move of a ref leaves, before the
/// ref's file: a move, a delete, a ref the transaction creates, and a move to a changeset it
/// queued itself. Dropped uncommitted, the transaction moved nothing.
#[test]
fn a_write_reads_its_own_queued_move_first() {
    let (_dir, _root, store) = new_store();
    insert(&store, &[cs(1, &[]), cs(2, &[1])]);
    cas(&store, "main", None, Some(1));
    cas(&store, "release/v0", None, Some(1));

    let mut write = store.write().unwrap();
    assert_eq!(write.get_ref("main").unwrap(), Some(id(1)), "the file's");
    write
        .cas_ref("main", Some(id(1)), Some(id(2)), ACTOR)
        .unwrap();
    assert_eq!(write.get_ref("main").unwrap(), Some(id(2)), "the move");
    write
        .cas_ref("release/v0", Some(id(1)), None, ACTOR)
        .unwrap();
    assert_eq!(write.get_ref("release/v0").unwrap(), None, "the delete");
    write.insert_changeset(cs(3, &[2])).unwrap();
    write
        .cas_ref("release/v1", None, Some(id(3)), ACTOR)
        .unwrap();
    assert_eq!(write.get_ref("release/v1").unwrap(), Some(id(3)));
    drop(write);

    let read = store.read().unwrap();
    assert_eq!(read.get_ref("main").unwrap(), Some(ref_state(1, 1)));
    assert_eq!(read.get_ref("release/v0").unwrap(), Some(ref_state(1, 1)));
    assert_eq!(read.get_ref("release/v1").unwrap(), None);
}

/// `changeset` reads an indexed changeset's record, or one this transaction queued as the
/// index will hold it once committed: at the seq the insert returned, with its generation,
/// delta hash and parents, whatever else the queue holds between them; `None` for a changeset
/// in neither.
#[test]
fn a_write_reads_a_changeset_from_the_index_or_its_queue() {
    let (_dir, _root, store) = new_store();
    insert(&store, &[cs(1, &[]), cs(2, &[1])]);

    let mut write = store.write().unwrap();
    assert_eq!(write.changeset(id(2)).unwrap(), Some(stored(2, &[1], 1, 2)));
    let third = write.insert_changeset(cs(3, &[2])).unwrap();
    write.cas_ref("main", None, Some(id(3)), ACTOR).unwrap();
    let fourth = write.insert_changeset(cs(4, &[3, 1])).unwrap();
    assert_eq!(
        (third, fourth),
        (
            InsertOutcome::Inserted { seq: 2 },
            InsertOutcome::Inserted { seq: 3 }
        )
    );
    assert_eq!(write.changeset(id(3)).unwrap(), Some(stored(3, &[2], 2, 3)));
    let queued = Some(stored(4, &[3, 1], 3, 4));
    assert_eq!(write.changeset(id(4)).unwrap(), queued);
    assert_eq!(write.changeset(id(5)).unwrap(), None);
    write.commit().unwrap();
    assert_eq!(store.read().unwrap().changeset(id(4)).unwrap(), queued);
}

/// `get_ref` refuses a name that is not `main` or `release/<x>`, as `cas_ref` does: one only
/// FORMAT 2 takes as needing it.
#[test]
fn a_write_refuses_to_read_a_bad_ref_name() {
    let (_dir, _root, store) = new_store();
    let write = store.write().unwrap();
    for name in ["", "release/", "release/a/b", "release/.x", "../main"] {
        assert_ref_name(write.get_ref(name), name);
    }
    assert_format_required(write.get_ref("master"), "master");
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

#[test]
fn a_huge_ref_file_is_not_a_ref_file_and_is_not_read_whole() {
    // A ref file holds at most 86 bytes (64 hex digits, a line break, up to 20 decimal digits, a
    // line break): a crafted one of huge length (a sparse file, which costs no disk) is refused
    // by its length, before any of it is read, as the InvalidData every damaged ref file is.
    let (_dir, root, store) = new_store();
    insert(&store, &[cs(1, &[])]);
    cas(&store, "main", None, Some(1));
    let main = Layout::new(&root).meta_path("refs/main");
    OpenOptions::new()
        .write(true)
        .open(&main)
        .unwrap()
        .set_len(1 << 30)
        .unwrap();
    let started = Instant::now();
    let got = store.read().map(|_| ());
    let Err(StoreError::Io(e)) = &got else {
        panic!("a 1 GiB ref file: expected InvalidData, got {got:?}");
    };
    assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");
    assert_eq!(
        e.to_string(),
        "read meta/refs/main: is 1073741824 bytes long, not a ref file"
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "read whole: {:?}",
        started.elapsed()
    );
}

#[test]
fn a_malformed_checkpoint_or_ref_file_is_reported_without_the_store_path() {
    // The store's root is a host path, which no filesystem failure carries to an agent
    // (operations spec §0): a metadata file that does not read as one, or that is not a regular
    // file, is named by its place in the store, as the read that failed.
    let (_dir, root, store) = new_store();
    insert(&store, &[cs(1, &[])]);
    cas(&store, "main", None, Some(1));
    let read_fails_with = |want: &str| {
        let got = store.read().map(|_| ());
        let Err(StoreError::Io(e)) = &got else {
            panic!("{want}: expected InvalidData, got {got:?}");
        };
        assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");
        assert!(!e.to_string().contains(root.as_str()), "{e}");
        assert_eq!(e.to_string(), want);
    };
    let layout = Layout::new(&root);
    let ckpt = layout.journal_ckpt();
    let kept = fs::read(&ckpt).unwrap();
    fs::write(&ckpt, [0; 3]).unwrap();
    read_fails_with("read meta/journal.ckpt: holds 3 bytes, not a u64le");
    fs::remove_file(&ckpt).unwrap();
    fs::create_dir(&ckpt).unwrap();
    read_fails_with("read meta/journal.ckpt: journal.ckpt is not a regular file");
    fs::remove_dir(&ckpt).unwrap();
    fs::write(&ckpt, kept).unwrap();

    let main = layout.meta_path("refs/main");
    let kept = fs::read(&main).unwrap();
    fs::write(&main, b"not a ref file\n").unwrap();
    read_fails_with(r#"read meta/refs/main: not a ref file: "<64 hex digits>\n<version>\n""#);
    // One byte past what a ref file holds: refused by its length, before it is read.
    fs::write(&main, [b'0'; 87]).unwrap();
    read_fails_with("read meta/refs/main: is 87 bytes long, not a ref file");
    fs::write(&main, kept).unwrap();
    assert_eq!(
        store.read().unwrap().get_ref("main").unwrap(),
        Some(ref_state(1, 1))
    );
}

/// A root changeset whose id starts with `prefix`, its other bytes `fill`.
fn with_prefix(prefix: &[u8], fill: u8) -> ChangesetMeta {
    let mut bytes = [fill; 32];
    bytes[..prefix.len()].copy_from_slice(prefix);
    ChangesetMeta::new(
        ChangesetId::new(ObjectId::new(bytes)),
        0,
        0,
        [fill; 32],
        vec![],
    )
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
            Err(StoreError::BadPrefix(prefix)) => assert_eq!(prefix, bad),
            other => panic!("{bad:?}: expected BadPrefix, got {other:?}"),
        }
    }
    assert_eq!(read.by_delta_hash(&[0x30; 32]).unwrap(), [past.id]);
    assert_eq!(read.by_delta_hash(&[0x10; 32]).unwrap(), [one.id, near.id]);
}

/// A crafted or damaged changeset index below the checkpoint, where a handle counts its records
/// as they are (SECURITY.md, "Crafted or damaged store files"): a lookup by key never answers
/// with a record that holds another key, nor with parents that lie elsewhere than the index's own
/// layout puts them, but fails with `IndexCorrupt`, which `swarmops verify` explains.
mod damaged_index {
    use std::fmt::Debug;
    use std::fs;
    use std::sync::Arc;
    use std::time::Duration;

    use camino::Utf8Path;
    use swarmops_store::{Index, Layout, MetaLock, MetaStore, RealFs, StoreError};

    use super::{ACTOR, cs, delta, id, index, insert, new_store, stored};

    /// Commits c1 ← c2 ← c3 ← c4 to `store` at `root`, a frame each, then checkpoints them and
    /// compacts the index under the writer lock: the sorted views cover every record and no frame
    /// lies past the checkpoint, so that the index answers every lookup on its own.
    fn checkpointed_chain(store: &MetaStore, root: &Utf8Path) {
        for n in 1..=4 {
            let parents: Vec<u64> = (n > 1).then_some(n - 1).into_iter().collect();
            insert(store, &[cs(n, &parents)]);
        }
        store.flush().unwrap();
        let layout = Layout::new(root);
        let _lock = MetaLock::acquire(&layout, Duration::from_secs(10)).unwrap();
        let mut index = Index::open(&layout, Arc::new(RealFs::new())).unwrap();
        assert_eq!(index.compact().unwrap(), 4);
    }

    /// Points the entry of sorted view `view` (`ids` or `dh`) whose key is `key` at record `rec`,
    /// in its `.rec` file.
    fn repoint(root: &Utf8Path, view: &str, key: [u8; 32], rec: u64) {
        let layout = Layout::new(root);
        let keys = fs::read(layout.meta_path(&format!("sorted/{view}.key"))).unwrap();
        let at = (keys.as_chunks::<32>().0.iter())
            .position(|entry| *entry == key)
            .expect("the key is in the view");
        let file = layout.meta_path(&format!("sorted/{view}.rec"));
        let mut recs = fs::read(&file).unwrap();
        recs[at * 8..(at + 1) * 8].copy_from_slice(&rec.to_le_bytes());
        fs::write(&file, recs).unwrap();
    }

    #[track_caller]
    fn assert_index_corrupt<T: Debug>(got: Result<T, StoreError>, what: &str) {
        match got {
            Err(StoreError::IndexCorrupt { .. }) => {}
            other => panic!("{what}: expected IndexCorrupt, got {other:?}"),
        }
    }

    #[test]
    fn a_view_entry_naming_another_record_fails_every_lookup_of_its_key() {
        let (_dir, root, store) = new_store();
        checkpointed_chain(&store, &root);
        // The id view's entry for c1, and the delta-hash view's for c1's delta hash, name c3's
        // record (2): a bit flipped in each `.rec` file, or a crafted one.
        repoint(&root, "ids", id(1).0.0, 2);
        repoint(&root, "dh", delta(1), 2);
        let read = store.read().unwrap();
        let hex = id(1).0.to_hex();
        assert_index_corrupt(index(&root).find(id(1)), "find");
        assert_index_corrupt(read.changeset(id(1)), "changeset");
        assert_index_corrupt(read.first_parent(id(1)), "first_parent");
        assert_index_corrupt(read.resolve_prefix(&hex[..12]), "a prefix");
        assert_index_corrupt(read.resolve_prefix(&hex), "all 64 digits");
        assert_index_corrupt(read.by_delta_hash(&delta(1)), "delta hash");
        // Every other key reads as it did, c3's among them.
        for n in 2..=4 {
            let want = stored(n, &[n - 1], n - 1, n);
            assert_eq!(read.changeset(id(n)).unwrap(), Some(want), "c{n}");
            let prefix = &id(n).0.to_hex()[..12];
            assert_eq!(read.resolve_prefix(prefix).unwrap(), [id(n)], "c{n}");
            assert_eq!(read.by_delta_hash(&delta(n)).unwrap(), [id(n)], "c{n}");
        }
    }

    #[test]
    fn a_write_refuses_a_view_entry_naming_another_record() {
        let (_dir, root, store) = new_store();
        checkpointed_chain(&store, &root);
        repoint(&root, "ids", id(1).0.0, 2);
        let mut write = store.write().unwrap();
        // c3's record is not taken for c1's: not to call c1 present again, nor to give a
        // changeset on c1 its generation, nor to move a ref to c1.
        assert_index_corrupt(write.changeset(id(1)), "the write's lookup");
        assert_index_corrupt(write.insert_changeset(cs(1, &[])), "c1 again");
        assert_index_corrupt(write.insert_changeset(cs(5, &[1])), "a changeset on c1");
        assert_index_corrupt(
            write.cas_ref("main", None, Some(id(1)), ACTOR),
            "main to c1",
        );
        assert!(write.queued().is_empty(), "{:?}", write.queued());
    }

    #[test]
    fn a_par_col_entry_out_of_place_fails_the_lookups_of_its_record_and_the_next() {
        let (_dir, root, store) = new_store();
        checkpointed_chain(&store, &root);
        // c3's par.col entry (record 2) points at c2's parents, c1, instead of at its own, c2:
        // one bit, 0x2001 to 0x0001.
        let par = Layout::new(&root).meta_path("idx/par.col");
        let mut entries = fs::read(&par).unwrap();
        assert_eq!(entries[16..24], ((32_u64 << 8) | 1).to_le_bytes());
        entries[16..24].copy_from_slice(&1_u64.to_le_bytes());
        fs::write(&par, entries).unwrap();
        let read = store.read().unwrap();
        assert_index_corrupt(read.changeset(id(3)), "c3");
        assert_index_corrupt(read.first_parent(id(3)), "c3's first parent");
        // c4's entry is whole, but its parents no longer start where c3's end: which of the two
        // is wrong only the journal tells (verify), so neither is read.
        assert_index_corrupt(read.changeset(id(4)), "c4");
        assert_index_corrupt(read.first_parent(id(4)), "c4's first parent");
        assert_eq!(read.changeset(id(2)).unwrap(), Some(stored(2, &[1], 1, 2)));
        assert_eq!(read.first_parent(id(2)).unwrap(), Some(Some(id(1))));
        // `Index::get` reads a record as the files hold it: what verify compares with the
        // journal, to name the record that disagrees.
        assert_eq!(index(&root).get(2).unwrap().parents, [id(1)]);
    }
}

#[test]
fn journal_after_returns_global_order() {
    let (_dir, root, first) = new_store();
    let second = open_checked(&root).unwrap(); // another handle, as another process has
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

/// The seqs of `read.frames_before(before, limit)`, in the order it gives them.
fn seqs_before(read: &MetaRead<'_>, before: Option<u64>, limit: usize) -> Vec<u64> {
    let frames = read.frames_before(before, limit).unwrap();
    frames.iter().map(|frame| frame.seq).collect()
}

/// The seqs from `newest` down to `oldest`, both included.
fn down(newest: u64, oldest: u64) -> Vec<u64> {
    (oldest..=newest).rev().collect()
}

/// `MetaRead::frames_before` lists a snapshot's frames newest first, below `before` when it is
/// given: the frames past the checkpoint from the snapshot itself, then those before it from
/// `journal.off`, each the journal's frame with its seq, offset and records. A snapshot ends at
/// its own newest frame: one with no frame past its checkpoint skips the frames listed after it
/// was taken, and one with some lists none committed after it.
#[test]
fn frames_before_reads_newest_first_across_the_checkpoint() {
    let (_dir, root) = new_root();
    // Durability is not the subject: 41 commits on a filesystem that never syncs.
    let store = MetaStore::create_with_fs(&root, checked(NoSyncFs)).unwrap();
    for n in 0..32 {
        insert(&store, &[cs(n, &[])]);
    }
    // The 32nd frame's commit moved the checkpoint to the journal's end.
    assert_eq!(ckpt(&root), log_len(&root));
    let at_32 = store.read().unwrap();
    for n in 32..40 {
        insert(&store, &[cs(n, &[])]);
    }
    assert_eq!(seqs_before(&at_32, None, 3), down(31, 29));
    assert_eq!(seqs_before(&at_32, Some(35), 3), down(31, 29));
    assert_eq!(seqs_before(&at_32, Some(31), 2), down(30, 29));
    drop(at_32);

    let journal = frames(&root);
    assert_eq!(journal.len(), 40);
    assert_eq!(
        ckpt(&root),
        journal[32].offset,
        "frames 32 to 39 lie past the checkpoint"
    );
    let read = store.read().unwrap();
    assert_eq!(seqs_before(&read, None, 10), down(39, 30));
    assert_eq!(seqs_before(&read, Some(31), 5), down(30, 26));
    assert_eq!(
        seqs_before(&read, Some(34), 4),
        down(33, 30),
        "across the checkpoint"
    );
    assert_eq!(seqs_before(&read, Some(u64::MAX), 2), down(39, 38));
    assert_eq!(seqs_before(&read, Some(1), 5), [0]);
    assert_eq!(seqs_before(&read, Some(0), 5), Vec::<u64>::new());
    assert_eq!(seqs_before(&read, None, 0), Vec::<u64>::new());
    let newest_first: Vec<Frame> = journal.iter().rev().cloned().collect();
    assert_eq!(read.frames_before(None, usize::MAX).unwrap(), newest_first);
    // A frame committed after the snapshot is not in it.
    insert(&store, &[cs(40, &[])]);
    assert_eq!(seqs_before(&read, None, 1), [39]);
    assert_eq!(seqs_before(&store.read().unwrap(), None, 1), [40]);
}

/// `MetaRead::frame` reads one frame of a snapshot by its seq, past the checkpoint or before
/// it, and none past the snapshot's newest frame, also once later frames are committed.
#[test]
fn frame_reads_one_frame_and_none_past_the_end() {
    let (_dir, root, store) = new_store();
    assert_eq!(
        store.read().unwrap().frame(0).unwrap(),
        None,
        "an empty journal"
    );
    insert(&store, &[cs(1, &[])]); // frame 0
    let mut write = store.write().unwrap(); // frame 1: a changeset and two ref moves
    write.insert_changeset(cs(2, &[1])).unwrap();
    write.cas_ref("main", None, Some(id(2)), ACTOR).unwrap();
    write
        .cas_ref("release/r", None, Some(id(1)), ACTOR)
        .unwrap();
    write.commit().unwrap();
    let journal = frames(&root);
    assert_eq!(journal.len(), 2);

    // Past the checkpoint: the snapshot's own frames.
    let read = store.read().unwrap();
    for frame in &journal {
        assert_eq!(read.frame(frame.seq).unwrap().as_ref(), Some(frame));
    }
    for past in [2, 3, u64::MAX] {
        assert_eq!(read.frame(past).unwrap(), None, "frame {past}");
    }
    // Before the checkpoint: read from journal.off.
    store.flush().unwrap();
    assert_eq!(ckpt(&root), log_len(&root));
    let read = store.read().unwrap();
    for frame in &journal {
        assert_eq!(read.frame(frame.seq).unwrap().as_ref(), Some(frame));
    }
    // A frame committed after a snapshot is past its end.
    assert_eq!(
        cas(&store, "main", Some(2), Some(1)),
        CasResult::Applied { old: Some(id(2)) }
    );
    assert_eq!(read.frame(2).unwrap(), None);
    let now = store.read().unwrap().frame(2).unwrap().expect("frame 2");
    assert_eq!(now.records.len(), 1);
}

/// `MetaWrite::queued` lists what a write transaction has queued, in order, as the frame its
/// commit appends holds it, each ref move with its time; a refused move adds nothing.
#[test]
fn queued_is_the_frame_a_commit_appends() {
    let (_dir, root, store) = new_store();
    insert(&store, &[cs(1, &[])]);
    let mut write = store.write().unwrap();
    assert!(write.queued().is_empty(), "nothing is queued yet");
    write.insert_changeset(cs(2, &[1])).unwrap();
    let before = unix_now();
    write.cas_ref("main", None, Some(id(2)), ACTOR).unwrap();
    let refused = write.cas_ref("main", None, Some(id(1)), ACTOR).unwrap();
    assert_eq!(
        refused,
        CasResult::Mismatch {
            actual: Some(id(2))
        }
    );
    let queued = write.queued().to_vec();
    let after = unix_now();
    assert_eq!(queued.len(), 2, "{queued:?}");
    assert_eq!(queued[0], changeset_record(&cs(2, &[1]), 2));
    let Record::RefMove { ts, .. } = queued[1] else {
        panic!("a ref move: {queued:?}");
    };
    assert!((before..=after).contains(&ts), "{ts}");
    assert_eq!(queued[1], ref_move("main", None, Some(2), ACTOR, ts));
    write.commit().unwrap();
    assert_eq!(frames(&root)[1].records, queued);
}

#[test]
fn write_is_one_frame_for_insert_plus_cas() {
    let (_dir, root, store) = new_store();
    let mut write = store.write().unwrap();
    write.insert_changeset(cs(1, &[])).unwrap();
    let applied = write.cas_ref("main", None, Some(id(1)), ACTOR).unwrap();
    assert_eq!(applied, CasResult::Applied { old: None });
    let info = write.commit().unwrap();
    let want = CommitInfo::new(0, 1, 1, false);
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
    let impatient = open_checked(&root)
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
    let moved = RefMoveEntry::new(1, None, Some(id(2)), "crashed/1".into(), 7);
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

    let reopened = open_checked(&root).unwrap();
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
    let reopened = open_checked(&root).unwrap();
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
    assert_eq!(compacted_frames(&store), Vec::<u64>::new());

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
/// lets them go together, and returns what each one printed and how it ended; each must end
/// within [`CHILD_TIMEOUT`] of the go.
fn run_children(test: &str, root: &Utf8Path) -> Vec<Output> {
    let children: Vec<Child> = (0..CHILDREN)
        .map(|number| {
            Child::spawn(
                this_test(&test_name!(test))
                    .env(CHILD_ROOT, root)
                    .env(CHILD_NUMBER, number.to_string()),
            )
        })
        .collect();
    fs::write(root.join("go"), b"").unwrap();
    let deadline = Instant::now() + CHILD_TIMEOUT;
    children
        .into_iter()
        .map(|child| child.wait(deadline))
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
    let store = open_checked(root).unwrap();
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

/// Eight processes, each making 50 write transactions of its own chain at once: every insert
/// persists, seqs are contiguous, a frame per write. 400 fsync'd transactions: the slow suite
/// runs it; the default gate's acceptance test for concurrent writers is the CLI's
/// `eight_processes_commit_concurrently_all_persist`, through the binary.
#[test]
#[ignore = "slow, 8-20 s: 400 fsync'd transactions; just check-slow runs it"]
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

    let reopened = open_checked(&root).unwrap();
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
    let store = open_checked(root).unwrap();
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
        _ => panic!("a variant this test does not know"),
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

    let reopened = open_checked(&root).unwrap();
    let read = reopened.read().unwrap();
    let main = RefState::new(target, 2);
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

#[test]
fn index_records_a_power_loss_zero_filled_are_not_counted_and_replay_replaces_them() {
    // The index files are appended to without a sync until the checkpoint moves: a power loss
    // can keep a file's length and lose its last records' bytes, zeros in their place. The
    // journal does not vouch for such records: no handle counts them (a snapshot reads their
    // changesets from the frames past the checkpoint), and the next writer cuts them and
    // replays the journal's records in their place. The records the checkpoint passed, synced
    // before it moved, stay as they are; parents.log above all, which a zeroed par.col entry
    // must not cut. With the checkpoint at 0, and past the first 32 frames.
    let files = [
        ("parents.log", 32),
        ("idx/par.col", 8),
        ("idx/gen.col", 8),
        ("idx/dh.col", 32),
        ("idx/ids.col", 32),
    ];
    for checkpointed in [0, 32] {
        let (_template_dir, template) = new_root();
        let store = create_checked(&template).unwrap();
        // A chain: changeset n's parent is n - 1, its generation n + 1.
        let chain = |n: u64| {
            stored(
                n,
                &n.checked_sub(1).into_iter().collect::<Vec<_>>(),
                n,
                n + 1,
            )
        };
        for n in 0..checkpointed + 3 {
            let mut meta = chain(n);
            meta.seq = 0;
            meta.generation = 0;
            insert(&store, &[meta]);
        }
        assert_eq!(
            ckpt(&template) > 0,
            checkpointed > 0,
            "the checkpoint moved"
        );
        let all = checkpointed + 3;
        for lost in files.iter().map(|file| vec![*file]).chain([files.to_vec()]) {
            let what = format!("checkpoint past {checkpointed} records, zeros in {lost:?}");
            let (_dir, root) = new_root();
            copy_dir(template.as_std_path(), root.as_std_path());
            let meta = Layout::new(&root).meta_dir();
            for (file, width) in &lost {
                // The last three records' bytes (each has one parent, but a first root).
                let path = meta.join(file);
                let mut bytes = fs::read(&path).unwrap();
                let len = bytes.len();
                bytes[len.saturating_sub(3 * width)..].fill(0);
                fs::write(&path, bytes).unwrap();
            }
            // Record 0, a root, has no parents, and its par.col entry is 0: zeros in par.col
            // and parents.log leave it as it was.
            let parents_only = lost
                .iter()
                .all(|(file, _)| matches!(*file, "idx/par.col" | "parents.log"));
            let kept = checkpointed + u64::from(checkpointed == 0 && parents_only);
            let store = open_checked(&root).unwrap();
            assert_eq!(index(&root).len(), kept, "{what}: not counted");
            let read = store.read().unwrap();
            assert_eq!(read.count_changesets().unwrap(), all, "{what}");
            for n in 0..all {
                assert_eq!(
                    read.changeset(id(n)).unwrap(),
                    Some(chain(n)),
                    "{what}: {n}"
                );
            }
            drop(read);

            store.write().unwrap().commit().unwrap();
            let index = index(&root);
            assert_eq!(index.len(), all, "{what}: replayed");
            for n in 0..all {
                assert_eq!(index.get(n).unwrap(), chain(n), "{what}: record {n}");
            }
        }
    }
}

#[test]
fn the_index_finds_its_records_past_the_checkpoint_without_the_frames_before_it() {
    // The records past the checkpoint start after the last changeset before it. When the index
    // holds one of the changesets past the checkpoint, that record's place says where they
    // start, and no frame before the checkpoint is read: a long run of ref moves there costs a
    // snapshot or a write nothing. Shown with a ref-move frame well before the checkpoint
    // damaged, which a walk back to the last changeset would have to read.
    let (_dir, root, store) = new_store();
    insert(&store, &[cs(0, &[])]); // frame 0
    cas(&store, "main", None, Some(0)); // frame 1
    for _ in 0..40 {
        cas(&store, "main", Some(0), Some(0)); // frames 2..=41: ref moves, no changeset
    }
    store.flush().unwrap(); // the checkpoint past frame 41
    insert(&store, &[cs(1, &[0])]); // frames 42 and 43, records 1 and 2, past the checkpoint
    insert(&store, &[cs(2, &[1])]);
    let layout = Layout::new(&root);
    let frame_10 = u64::from_le_bytes(
        fs::read(layout.journal_off()).unwrap()[80..88]
            .try_into()
            .unwrap(),
    );
    let mut log = fs::read(layout.journal_log()).unwrap();
    log[usize::try_from(frame_10).unwrap() + 12] ^= 0x01; // a payload byte: its CRC fails
    fs::write(layout.journal_log(), log).unwrap();
    // And a power loss zeroed record 2's generation, which the journal must not vouch for.
    let generations = layout.meta_path("idx/gen.col");
    let mut column = fs::read(&generations).unwrap();
    column[16..24].fill(0);
    fs::write(&generations, column).unwrap();

    assert_eq!(index(&root).len(), 2, "record 2 is not counted");
    let read = store.read().unwrap();
    assert_eq!(read.count_changesets().unwrap(), 3);
    assert_eq!(read.changeset(id(2)).unwrap(), Some(stored(2, &[1], 2, 3)));
}

#[test]
fn snapshots_and_writes_vouch_for_the_index_with_the_frames_they_read() {
    // A snapshot reads the frames past the checkpoint for its overlay, and a write transaction
    // for its replay: the index counts its records past the checkpoint by those same frames,
    // without a read of the journal of its own, so that vouching costs a snapshot or a write
    // no more journal reads than it made before. Shown with journal.log replaced at its path,
    // once the store has opened it, by a copy whose frame past the checkpoint fails its CRC:
    // the store's handles still read the original, and only a read that opens the journal
    // again meets the damage (and, the journal damaged, counts every record as it stands).
    // A power loss zeroed the generation of record 1, which the journal does not vouch for.
    let (_dir, root, store) = new_store();
    insert(&store, &[cs(0, &[])]); // frame 0 (bytes 0..84), record 0
    insert(&store, &[cs(1, &[0])]); // frame 1 (bytes 84..200), record 1
    assert_eq!(ckpt(&root), 0, "both frames past the checkpoint");
    let layout = Layout::new(&root);
    let generations = layout.meta_path("idx/gen.col");
    let mut column = fs::read(&generations).unwrap();
    column[8..16].fill(0);
    fs::write(&generations, column).unwrap();

    let store = open_checked(&root).unwrap();
    let mut damaged = fs::read(layout.journal_log()).unwrap();
    damaged[84 + 12] ^= 0x01; // a payload byte of frame 1: its CRC fails
    let copy = layout.meta_path("journal.log.damaged");
    fs::write(&copy, damaged).unwrap();
    fs::rename(&copy, layout.journal_log()).unwrap();

    let read = store.read().unwrap();
    assert_eq!(read.count_changesets().unwrap(), 2);
    assert_eq!(read.changeset(id(1)).unwrap(), Some(stored(1, &[0], 1, 2)));
    drop(read);
    // The write cuts record 1 and replays the journal's in its place.
    drop(store.write().unwrap());
    let column = fs::read(&generations).unwrap();
    assert_eq!(column.len(), 16, "two records");
    assert_eq!(column[8..16], 2_u64.to_le_bytes(), "record 1 replayed");
}

#[test]
fn a_fifo_where_a_store_file_should_be_is_refused_not_opened() {
    // `tar`, `cp -R` and `rsync -a` recreate a FIFO, and opening one blocks until something
    // opens it for writing: a store file must be known to be a regular file before it is
    // opened. The open runs on a thread with a deadline, so that a regression fails instead of
    // hanging the test.
    let (_dir, root, store) = new_store();
    drop(store);
    let log = Layout::new(&root).journal_log();
    fs::remove_file(&log).unwrap();
    assert!(Command::new("mkfifo").arg(&log).status().unwrap().success());
    let (tx, rx) = std::sync::mpsc::channel();
    let opened_root = root.clone();
    thread::spawn(move || {
        let _ = tx.send(MetaStore::open(&opened_root).map(drop));
    });
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Err(StoreError::Io(e))) => {
            assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");
            assert!(e.to_string().ends_with("is not a regular file"), "{e}");
        }
        Ok(other) => panic!("a FIFO at journal.log: expected InvalidData, got {other:?}"),
        Err(timeout) => panic!("opening the store blocked on the FIFO at journal.log: {timeout}"),
    }
}

#[test]
fn a_fifo_at_meta_lock_fails_a_write_at_once_instead_of_blocking() {
    // A FIFO where the lock file should be, which a restored store can hold, would block every
    // writer's open of it until something opened it for reading: a write transaction, and a
    // committed snapshot (both take the lock), fail at once instead, as damage (`InvalidData`)
    // that names meta/lock. Reads take no lock, and go on. Each runs on a thread with a deadline,
    // so that a regression fails instead of hanging the test.
    let (_dir, root, store) = new_store();
    insert(&store, &[cs(0, &[])]);
    drop(store);
    let lock = Layout::new(&root).meta_lock();
    fs::remove_file(&lock).unwrap();
    let made = Command::new("mkfifo").arg(&lock).status();
    assert!(made.unwrap().success(), "mkfifo");
    for (what, committed) in [("write", false), ("read_committed", true)] {
        let (tx, rx) = mpsc::channel();
        let opened_root = root.clone();
        thread::spawn(move || {
            let store = MetaStore::open(&opened_root).unwrap();
            let _ = tx.send(if committed {
                store.read_committed().map(drop)
            } else {
                store.write().map(drop)
            });
        });
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Err(StoreError::Io(e))) => {
                let file = e.file.as_ref().map(|file| file.as_str());
                assert_eq!(
                    (e.kind(), file),
                    (io::ErrorKind::InvalidData, Some("meta/lock")),
                    "{what}: {e}"
                );
                assert_eq!(e.to_string(), "open meta/lock: lock is not a regular file");
            }
            Ok(other) => panic!("{what}: a FIFO at meta/lock: expected InvalidData, got {other:?}"),
            Err(timeout) => panic!("{what} blocked on the FIFO at meta/lock: {timeout}"),
        }
    }
    let store = MetaStore::open(&root).unwrap();
    assert_eq!(store.read().unwrap().count_changesets().unwrap(), 1);
}

#[test]
fn an_index_open_counts_by_length_past_a_malformed_checkpoint_and_fails_on_an_unreadable_one() {
    // Past the checkpoint the journal vouches for the index's records: a power loss zeroed
    // record 1's generation, so it is not counted. A checkpoint that does not hold a u64le is
    // damage, which verify reports: with nothing to vouch with, an index open counts the
    // records by their lengths. One that does not read at all is an I/O error, which the open
    // returns rather than count by length.
    let (_dir, root, store) = new_store();
    insert(&store, &[cs(0, &[])]);
    insert(&store, &[cs(1, &[0])]);
    assert_eq!(ckpt(&root), 0, "both records past the checkpoint");
    let layout = Layout::new(&root);
    let generations = layout.meta_path("idx/gen.col");
    let mut column = fs::read(&generations).unwrap();
    column[8..16].fill(0);
    fs::write(&generations, column).unwrap();
    assert_eq!(index(&root).len(), 1, "record 1 is not vouched for");

    fs::write(layout.journal_ckpt(), b"bad").unwrap();
    assert_eq!(
        index(&root).len(),
        2,
        "a malformed checkpoint: counted by length"
    );

    fs::remove_file(layout.journal_ckpt()).unwrap();
    // A symlink to itself: a read of it fails with ELOOP.
    std::os::unix::fs::symlink("journal.ckpt", layout.journal_ckpt()).unwrap();
    match Index::open(&layout, Arc::new(RealFs::new())) {
        Err(StoreError::Io(e)) => assert_eq!(e.raw_os_error(), Some(40), "ELOOP: {e}"),
        other => panic!("an unreadable checkpoint: expected its I/O error, got {other:?}"),
    }
}

#[test]
fn counting_by_length_never_lets_a_zeroed_par_col_entry_end_parents_log() {
    // A record's parents start in parents.log where the record before it leaves off (one with
    // no parents points at the next one's start). A power loss can zero the index's records
    // past the checkpoint, and where the journal cannot vouch for them (it is damaged, which
    // verify reports), the index counts its records by their lengths. A zeroed par.col entry
    // says "no parents, at offset 0": taken for the last record's, it would end parents.log
    // before the parents of the records before it, and a writer would cut them there and
    // replay over them. Records out of that layout are IndexCorrupt instead: snapshots and
    // writes fail, and nothing is cut.
    //
    // A chain 0 <- 1 <- 2 <- 3. The checkpoint past frames 0 to 2 (records 0 and 1, then a ref
    // move), records 2 and 3 past it, a power loss zeroing their ids and par.col entries: with
    // none of the changesets past the checkpoint among the index's last records, it walks the
    // journal back from the checkpoint to its last changeset, and meets frame 2 damaged.
    let (_dir, root, store) = new_store();
    insert(&store, &[cs(0, &[])]); // frame 0, record 0
    insert(&store, &[cs(1, &[0])]); // frame 1, record 1
    cas(&store, "main", None, Some(1)); // frame 2
    store.flush().unwrap();
    insert(&store, &[cs(2, &[1])]); // frame 3, record 2
    insert(&store, &[cs(3, &[2])]); // frame 4, record 3
    let layout = Layout::new(&root);
    let frame_2 = usize::try_from(frames(&root)[2].offset).unwrap();
    let mut log = fs::read(layout.journal_log()).unwrap();
    log[frame_2 + 12] ^= 0x01; // a payload byte: its CRC fails
    fs::write(layout.journal_log(), log).unwrap();
    let meta = layout.meta_dir();
    let zero_from = |file: &str, at: usize| {
        let mut bytes = fs::read(meta.join(file)).unwrap();
        bytes[at..].fill(0);
        fs::write(meta.join(file), bytes).unwrap();
    };
    zero_from("idx/ids.col", 2 * 32);
    zero_from("idx/par.col", 2 * 8);
    let files = [
        "parents.log",
        "idx/par.col",
        "idx/gen.col",
        "idx/dh.col",
        "idx/ids.col",
    ];
    let read_files = || files.map(|file| fs::read(meta.join(file)).unwrap());
    let before = read_files();

    let read = store.read().map(|read| read.count_changesets());
    let wrote = store.write().map(drop);
    assert!(read_files() == before, "the index files are as they were");
    assert!(
        matches!(read, Err(StoreError::IndexCorrupt { .. })),
        "a snapshot: {read:?}"
    );
    assert!(
        matches!(wrote, Err(StoreError::IndexCorrupt { .. })),
        "a write: {wrote:?}"
    );

    // The same where the checkpoint does not read (an index open, as verify makes one): a
    // chain of four past the checkpoint, the last two par.col entries zeroed.
    let (_dir, root, store) = new_store();
    for n in 0..4 {
        insert(
            &store,
            &[cs(n, &n.checked_sub(1).into_iter().collect::<Vec<_>>())],
        );
    }
    let layout = Layout::new(&root);
    let pars = layout.meta_path("idx/par.col");
    let mut column = fs::read(&pars).unwrap();
    column[2 * 8..].fill(0);
    fs::write(&pars, column).unwrap();
    fs::write(layout.journal_ckpt(), b"bad").unwrap();
    match Index::open(&layout, Arc::new(RealFs::new())) {
        Err(StoreError::IndexCorrupt { .. }) => {}
        other => panic!("a malformed checkpoint: expected IndexCorrupt, got {other:?}"),
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
        let store = MetaStore::create_with_fs(&root, checked(fs)).unwrap();
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
    let store = MetaStore::create_with_fs(&root, checked(fs)).unwrap();
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

/// A store on a [`RecordingFs`], under the order checker, and what makes its calls fail. Its
/// write transactions sync the index one file after another, in write order (parallelism 1), so
/// that a failure lands at the same point of the same call sequence in every run.
fn failing_store(root: &Utf8Path) -> (MetaStore, Failing) {
    let (fs, _calls) = RecordingFs::new();
    let fail = fs.failing();
    let mut store = MetaStore::create_with_fs(root, checked(fs)).unwrap();
    store.set_parallelism(1);
    (store, fail)
}

/// [`failing_store`] without the order checker: for the one test that pins a hazard the
/// checker rightly refuses (W3: a checkpoint past index records a failed sync may have lost,
/// once cutting them back failed too), which storage spec §3 documents as open until the next
/// successful sync.
fn unchecked_failing_store(root: &Utf8Path) -> (MetaStore, Failing) {
    let (fs, _calls) = RecordingFs::new();
    let fail = fs.failing();
    let mut store = MetaStore::create_with_fs(root, Arc::new(fs)).unwrap();
    store.set_parallelism(1);
    (store, fail)
}

/// Changeset `n` of a chain: a root for 0, else on `n - 1`.
fn chained(n: u64) -> ChangesetMeta {
    match n.checked_sub(1) {
        Some(parent) => cs(n, &[parent]),
        None => cs(n, &[]),
    }
}

/// [`chained`]`(n)` as the store holds it: at seq `n`, generation `n + 1`.
fn chained_stored(n: u64) -> ChangesetMeta {
    let mut meta = chained(n);
    meta.seq = n;
    meta.generation = n + 1;
    meta
}

/// The lengths of the index columns and `parents.log` at `root`.
fn chained_lens(root: &Utf8Path) -> [(&'static str, u64); 5] {
    let meta = Layout::new(root).meta_dir();
    [
        "idx/ids.col",
        "idx/dh.col",
        "idx/gen.col",
        "idx/par.col",
        "parents.log",
    ]
    .map(|file| (file, file_len(meta.join(file))))
}

/// What [`chained_lens`] gives for `records` records of [`chained`]: 32 bytes of
/// `parents.log` for each but the root.
fn chained_want(records: u64) -> [(&'static str, u64); 5] {
    [
        ("idx/ids.col", records * 32),
        ("idx/dh.col", records * 32),
        ("idx/gen.col", records * 8),
        ("idx/par.col", records * 8),
        ("parents.log", records.saturating_sub(1) * 32),
    ]
}

/// A failed index sync may have lost pages whose writeback failed, and a later sync can
/// succeed without saying so (Linux reports a writeback error once): so the writer cuts the
/// index back to the records the checkpoint has passed, and the next write's replay appends
/// the others again from the journal, to be synced afresh.
#[test]
fn a_failed_index_sync_cuts_the_index_back_to_the_checkpoint() {
    let (_dir, root) = new_root();
    let (store, fail) = failing_store(&root);
    // Frames 0..=31: the 32nd checkpoints the first 32 records.
    for n in 0..32 {
        insert(&store, &[chained(n)]);
    }
    let checkpoint = ckpt(&root);
    assert_eq!(checkpoint, log_len(&root), "the 32nd frame checkpointed");
    // Frames 32..=62 past the checkpoint, then the 32nd past it, whose sync fails at ids.col,
    // the last of the five files it syncs.
    for n in 32..63 {
        insert(&store, &[chained(n)]);
    }
    fail.set("sync_data ids.col");
    let mut write = store.write().unwrap();
    write.insert_changeset(chained(63)).unwrap();
    match write.commit() {
        Err(StoreError::PostCommit {
            frame_seq: 63,
            source,
        }) => assert!(matches!(*source, StoreError::Io(_)), "{source:?}"),
        other => panic!("expected PostCommit at frame 63, got {other:?}"),
    }
    fail.clear();

    // The index holds the 32 records the checkpoint passed, and nothing after them.
    assert_eq!(chained_lens(&root), chained_want(32));
    assert_eq!(index(&root).len(), 32);
    assert_eq!(ckpt(&root), checkpoint, "the checkpoint did not move");
    // Readers find every changeset all the same: those past the checkpoint in the journal.
    let read = store.read().unwrap();
    assert_eq!(read.count_changesets().unwrap(), 64);
    for n in [0, 31, 32, 63] {
        assert_eq!(
            read.changeset(id(n)).unwrap(),
            Some(chained_stored(n)),
            "{n}"
        );
    }
    drop(read);

    // The next write appends the 32 records again, syncs them and checkpoints.
    assert!(store.write().unwrap().commit().unwrap().checkpointed);
    assert_eq!(chained_lens(&root), chained_want(64));
    let index = index(&root);
    for n in 0..64 {
        assert_eq!(index.get(n).unwrap(), chained_stored(n), "{n}");
    }
    assert_eq!(ckpt(&root), log_len(&root));
}

/// The cut never goes below what the sorted views cover: they would cover records the
/// columns lack, and no handle would open the index. A compaction syncs the records it
/// covers before it writes the views, so those are durable even past the checkpoint (here
/// the compaction's own checkpoint failed).
#[test]
fn a_failed_index_sync_never_cuts_what_the_sorted_views_cover() {
    let (_dir, root) = new_root();
    let (store, fail) = failing_store(&root);
    fail.set("atomic_replace journal.ckpt");
    let mut write = store.write().unwrap();
    for n in 0..1100 {
        write.insert_changeset(cs(n, &[])).unwrap();
    }
    match write.commit() {
        Err(StoreError::PostCommit { frame_seq: 0, .. }) => {}
        other => panic!("expected PostCommit at frame 0, got {other:?}"),
    }
    fail.clear();
    assert_eq!((ckpt(&root), index(&root).sorted_count()), (0, 1100));

    // A changeset past the views, then a flush whose sync fails.
    insert(&store, &[cs(1100, &[])]);
    fail.set("sync_data ids.col");
    match store.flush() {
        Err(StoreError::Io(_)) => {}
        other => panic!("flush: expected a plain Io error, got {other:?}"),
    }
    fail.clear();
    let cut = index(&root);
    assert_eq!((cut.len(), cut.sorted_count()), (1100, 1100));

    store.flush().unwrap();
    let index = index(&root);
    assert_eq!(index.len(), 1101);
    assert_eq!(index.find(id(1100)).unwrap(), Some(1100));
    assert_eq!(ckpt(&root), log_len(&root));
}

/// A write that takes the tail past the compaction threshold syncs the index before it
/// compacts: a failed sync cuts the index back to the checkpoint, as at any sync point, and
/// nothing is compacted; the next write appends the records again and compacts them.
#[test]
fn a_failed_sync_before_a_compaction_cuts_the_index_and_compacts_nothing() {
    let (_dir, root) = new_root();
    let (store, fail) = failing_store(&root);
    for n in 0..32 {
        insert(&store, &[cs(n, &[])]); // frames 0..=31, checkpointed
    }
    fail.set("sync_data ids.col");
    let mut write = store.write().unwrap();
    for n in 32..1100 {
        write.insert_changeset(cs(n, &[])).unwrap();
    }
    match write.commit() {
        Err(StoreError::PostCommit {
            frame_seq: 32,
            source,
        }) => assert!(matches!(*source, StoreError::Io(_)), "{source:?}"),
        other => panic!("expected PostCommit at frame 32, got {other:?}"),
    }
    fail.clear();
    let cut = index(&root);
    assert_eq!((cut.len(), cut.sorted_count()), (32, 0));
    assert_eq!(
        compacted_frames(&store),
        Vec::<u64>::new(),
        "nothing was compacted"
    );

    assert!(store.write().unwrap().commit().unwrap().checkpointed);
    let index = index(&root);
    let counts = (index.len(), index.sorted_count(), index.tail_len());
    assert_eq!(counts, (1100, 1100, 0));
    assert_eq!(compacted_frames(&store), [33]);
    assert_eq!(ckpt(&root), log_len(&root));
}

/// When the cut fails too, the error says so beside the sync's: the sync's failure, naming the
/// index file it failed on, with the cut's failure as what else failed. The next writer finds
/// the records whole and keeps them, and syncs them again before it checkpoints.
#[test]
fn a_cut_that_fails_after_a_failed_sync_is_reported_with_it() {
    let (_dir, root) = new_root();
    let (store, fail) = unchecked_failing_store(&root);
    for n in 0..31 {
        insert(&store, &[chained(n)]);
    }
    fail.set("sync_data ids.col");
    fail.also("truncate ids.col");
    let mut write = store.write().unwrap();
    write.insert_changeset(chained(31)).unwrap();
    let message = match write.commit() {
        Err(StoreError::PostCommit {
            frame_seq: 31,
            source,
        }) => {
            let StoreError::Io(failure) = &*source else {
                panic!("expected the sync's I/O failure, got {source:?}");
            };
            assert_eq!(
                (failure.op, failure.file.as_deref()),
                (
                    swarmops_store::FileOp::Fs(swarmops_store::FsMethod::SyncData),
                    Some(Utf8Path::new("meta/idx/ids.col"))
                ),
                "{failure}"
            );
            let also = failure.also.as_deref().unwrap_or_default();
            assert!(also.contains("truncate meta/idx/ids.col"), "{failure}");
            source.to_string()
        }
        other => panic!("expected PostCommit at frame 31, got {other:?}"),
    };
    for injected in ["injected: sync_data ids.col", "injected: truncate ids.col"] {
        assert!(message.contains(injected), "{message}");
    }
    fail.clear();
    assert_eq!(chained_lens(&root), chained_want(32), "nothing was cut");
    assert!(store.write().unwrap().commit().unwrap().checkpointed);
    assert_eq!(ckpt(&root), log_len(&root));
    let index = index(&root);
    for n in 0..32 {
        assert_eq!(index.get(n).unwrap(), chained_stored(n), "{n}");
    }
}

/// A snapshot measures the index files, then reads the frames past the checkpoint, which vouch
/// for the records past it; a writer may cut those records in between: a flush whose index sync
/// fails ([`Index::truncate_to`]), or the first write after a power loss, which cuts the records
/// the journal does not vouch for and then fails to replay them. No cut takes a record the
/// checkpoint passed, and the snapshot counts every one of those, the changesets past the
/// checkpoint coming from the frames, numbered after them. (Looking for where the records past
/// the checkpoint start, the index used to meet the cut and count one record fewer: the last
/// changeset before the checkpoint went missing, and every seq past it was one short.)
#[test]
fn a_snapshot_whose_index_a_writer_cuts_as_it_opens_counts_every_record_the_checkpoint_passed() {
    for power_loss in [false, true] {
        let what = if power_loss {
            "the repair after a power loss"
        } else {
            "a failed index sync"
        };
        let (_dir, root) = new_root();
        let (writer, fail) = failing_store(&root);
        for n in 0..3 {
            insert(&writer, &[chained(n)]);
        }
        writer.flush().unwrap(); // the checkpoint past records 0..=2
        for n in 3..5 {
            insert(&writer, &[chained(n)]); // records 3 and 4, past the checkpoint
        }
        assert_eq!(chained_lens(&root), chained_want(5), "{what}");
        if power_loss {
            // Records 3 and 4 kept their length and lost their bytes, zeros in their place.
            let meta = Layout::new(&root).meta_dir();
            for ((file, len), (_, kept)) in chained_lens(&root).into_iter().zip(chained_want(3)) {
                let path = meta.join(file);
                let mut bytes = fs::read(&path).unwrap();
                bytes[usize::try_from(kept).unwrap()..usize::try_from(len).unwrap()].fill(0);
                fs::write(&path, bytes).unwrap();
            }
            fail.set("append parents.log"); // the replay's first append
        } else {
            fail.set("sync_data ids.col");
        }
        // The reader's first FsOps call measures journal.off, to read the frames past the
        // checkpoint, once it has measured the index files: right then, the writer cuts them.
        let (fs, _calls) = RecordingFs::new();
        let fs = fs.with_hook((
            "file_len journal.off".to_owned(),
            When::Before,
            Box::new(move || {
                let cut = if power_loss {
                    writer.write().map(drop)
                } else {
                    writer.flush()
                };
                assert!(matches!(cut, Err(StoreError::Io(_))), "{cut:?}");
            }),
        ));
        let reader = MetaStore::open_with_fs(&root, checked(fs)).unwrap();

        let read = reader.read().unwrap();
        assert_eq!(chained_lens(&root), chained_want(3), "{what}: cut");
        assert_eq!(read.count_changesets().unwrap(), 5, "{what}");
        for n in 0..5 {
            let got = read.changeset(id(n)).unwrap();
            assert_eq!(got, Some(chained_stored(n)), "{what}: changeset {n}");
        }
        drop(read);

        // The next write appends the records cut again, from the journal.
        open_checked(&root).unwrap().flush().unwrap();
        assert_eq!(chained_lens(&root), chained_want(5), "{what}: replayed");
        let index = index(&root);
        for n in 0..5 {
            assert_eq!(
                index.get(n).unwrap(),
                chained_stored(n),
                "{what}: record {n}"
            );
        }
    }
}

/// The sorted views can cover records past the checkpoint: a compaction syncs the records it
/// covers before it writes the views, and the checkpoint that follows it may fail (here), or a
/// snapshot may read the checkpoint just before a writer moves it. No cut takes those records
/// ([`Index::truncate_to`] and [`Index::repair_tail`] keep them), and a snapshot whose index a
/// writer cuts as it opens counts every one of them too. (The index used to count only the
/// records the checkpoint passed when it met the cut, fewer than the views cover, and the
/// snapshot failed as if the index were damaged: `IndexCorrupt`, over files that are whole.)
#[test]
fn a_snapshot_whose_index_a_writer_cuts_as_it_opens_counts_every_record_the_sorted_views_cover() {
    for power_loss in [false, true] {
        let what = if power_loss {
            "the repair after a power loss"
        } else {
            "a failed index sync"
        };
        let (_dir, root) = new_root();
        let (writer, fail) = failing_store(&root);
        for n in 0..3 {
            insert(&writer, &[cs(n, &[])]);
        }
        writer.flush().unwrap(); // the checkpoint past records 0..=2
        // Records 3..=1102 in one frame, past the compaction threshold: compacted, and the
        // checkpoint after the compaction fails.
        fail.set("atomic_replace journal.ckpt");
        let mut write = writer.write().unwrap();
        for n in 3..1103 {
            write.insert_changeset(cs(n, &[])).unwrap();
        }
        match write.commit() {
            Err(StoreError::PostCommit { frame_seq: 3, .. }) => {}
            other => panic!("{what}: expected PostCommit at frame 3, got {other:?}"),
        }
        fail.clear();
        let views = index(&root);
        assert_eq!((views.len(), views.sorted_count()), (1103, 1103), "{what}");
        let covered = chained_lens(&root);
        for n in 1103..1105 {
            insert(&writer, &[cs(n, &[])]); // records 1103 and 1104, past the views
        }
        if power_loss {
            // Records 1103 and 1104 kept their length and lost their bytes, zeros in their place.
            let meta = Layout::new(&root).meta_dir();
            for ((file, len), (_, kept)) in chained_lens(&root).into_iter().zip(covered) {
                let path = meta.join(file);
                let mut bytes = fs::read(&path).unwrap();
                bytes[usize::try_from(kept).unwrap()..usize::try_from(len).unwrap()].fill(0);
                fs::write(&path, bytes).unwrap();
            }
            fail.set("append parents.log"); // the replay's first append
        } else {
            fail.set("sync_data ids.col");
        }
        // The reader's first FsOps call measures journal.off, to read the frames past the
        // checkpoint, once it has measured the index files: right then, the writer cuts the
        // records past the views.
        let (fs, _calls) = RecordingFs::new();
        let fs = fs.with_hook((
            "file_len journal.off".to_owned(),
            When::Before,
            Box::new(move || {
                let cut = if power_loss {
                    writer.write().map(drop)
                } else {
                    writer.flush()
                };
                assert!(matches!(cut, Err(StoreError::Io(_))), "{cut:?}");
            }),
        ));
        let reader = MetaStore::open_with_fs(&root, checked(fs)).unwrap();

        let read = reader.read().unwrap();
        assert_eq!(chained_lens(&root), covered, "{what}: cut");
        assert_eq!(read.count_changesets().unwrap(), 1105, "{what}");
        for n in [0, 2, 3, 1102, 1103, 1104] {
            let got = read.changeset(id(n)).unwrap();
            assert_eq!(got, Some(stored(n, &[], n, 1)), "{what}: changeset {n}");
        }
        drop(read);

        // The next write appends the records cut again, from the journal.
        open_checked(&root).unwrap().flush().unwrap();
        let index = index(&root);
        let counts = (index.len(), index.sorted_count());
        assert_eq!(counts, (1105, 1103), "{what}: replayed");
        for n in [1103, 1104] {
            let got = index.get(n).unwrap();
            assert_eq!(got, stored(n, &[], n, 1), "{what}: record {n}");
        }
    }
}

/// A snapshot whose index opened before a failed index sync's cut, and counted records the cut
/// took, fails with an I/O error (`UnexpectedEof`) while it builds its overlay, never with
/// wrong data ([`Index::truncate_to`], storage spec §3); a snapshot taken after the cut reads
/// the index as cut, the journal's frames over it.
#[test]
fn a_snapshot_whose_index_a_writer_cuts_after_it_opened_fails_with_an_io_error() {
    let (_dir, root) = new_root();
    let (writer, fail) = failing_store(&root);
    for n in 0..3 {
        insert(&writer, &[chained(n)]);
    }
    writer.flush().unwrap(); // the checkpoint past records 0..=2
    for n in 3..5 {
        insert(&writer, &[chained(n)]); // records 3 and 4, past the checkpoint
    }
    assert_eq!(chained_lens(&root), chained_want(5));
    fail.set("sync_data ids.col");
    // The reader's first read of main's file comes once its index has opened and counted all
    // five records, right before it builds its overlay: the writer's flush cuts the index back
    // to the three records the checkpoint passed.
    let (fs, _calls) = RecordingFs::new();
    let fs = fs.with_hook((
        "read main".to_owned(),
        When::Before,
        Box::new(move || {
            let cut = writer.flush();
            assert!(matches!(cut, Err(StoreError::Io(_))), "{cut:?}");
        }),
    ));
    let reader = MetaStore::open_with_fs(&root, checked(fs)).unwrap();

    match reader.read().map(drop) {
        Err(StoreError::Io(e)) => assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof, "{e}"),
        other => panic!("expected an I/O error from the stale index, got {other:?}"),
    }
    assert_eq!(chained_lens(&root), chained_want(3), "cut");

    // A new snapshot reads the index as cut, and the journal's two frames over it.
    let read = reader.read().unwrap();
    assert_eq!(read.count_changesets().unwrap(), 5);
    for n in 0..5 {
        let got = read.changeset(id(n)).unwrap();
        assert_eq!(got, Some(chained_stored(n)), "changeset {n}");
    }
}

/// Records 0 to 2 of a chain through `writer` (frames 0 to 2), flushed, so that the checkpoint
/// passes them, then records 3 and 4 past it (frames 3 and 4); then a payload byte of frame 2
/// flipped, so that its CRC fails. A writer's tail check reads only the last listed frame whole,
/// and the headers of the two before it, so the writer still writes. A reader whose index holds
/// none of the changesets past the checkpoint among its last records walks the journal back
/// from the checkpoint to its last changeset, meets frame 2, and counts the records by their
/// lengths.
fn chain_past_a_damaged_frame(writer: &MetaStore, root: &Utf8Path) {
    for n in 0..3 {
        insert(writer, &[chained(n)]);
    }
    writer.flush().unwrap();
    for n in 3..5 {
        insert(writer, &[chained(n)]);
    }
    assert_eq!(chained_lens(root), chained_want(5));
    let layout = Layout::new(root);
    let frame_2 = usize::try_from(frames(root)[2].offset).unwrap();
    let mut log = fs::read(layout.journal_log()).unwrap();
    log[frame_2 + 12] ^= 0x01; // a payload byte: its CRC fails
    fs::write(layout.journal_log(), log).unwrap();
}

/// Where the journal cannot vouch for the index's records past the checkpoint, the index counts
/// them by their lengths ([`chain_past_a_damaged_frame`]); a writer may cut those records after
/// they were measured (a flush whose index sync fails, [`Index::truncate_to`]). The count reads
/// `par.col` as the cut leaves it, and counts the records the cut keeps, as a vouched count does:
/// every record the checkpoint passed, the changesets past it coming from the frames. (It read
/// `par.col` as it was measured, and failed with an I/O error, `UnexpectedEof`, on a single cut,
/// where storage spec §3 allows the error only if the index is cut again meanwhile.)
#[test]
fn counting_by_length_as_a_writer_cuts_counts_the_records_the_cut_keeps() {
    let (_dir, root) = new_root();
    let (writer, fail) = failing_store(&root);
    chain_past_a_damaged_frame(&writer, &root);
    fail.set("sync_data ids.col");
    // The reader's first FsOps call measures journal.off, to read the frames past the
    // checkpoint, once it has measured the index files: right then, the writer's flush fails its
    // index sync and cuts records 3 and 4. The reader's index then holds none of their changesets.
    let (fs, _calls) = RecordingFs::new();
    let fs = fs.with_hook((
        "file_len journal.off".to_owned(),
        When::Before,
        Box::new(move || {
            let cut = writer.flush();
            assert!(matches!(cut, Err(StoreError::Io(_))), "{cut:?}");
        }),
    ));
    let reader = MetaStore::open_with_fs(&root, checked(fs)).unwrap();

    let read = match reader.read() {
        Ok(read) => read,
        Err(e) => panic!("expected a snapshot, got {e:?}"),
    };
    assert_eq!(chained_lens(&root), chained_want(3), "cut");
    assert_eq!(read.count_changesets().unwrap(), 5);
    for n in 0..5 {
        let got = read.changeset(id(n)).unwrap();
        assert_eq!(got, Some(chained_stored(n)), "changeset {n}");
    }
}

/// The same, with the reader counting by length while the cut is made: after the writer has cut
/// `ids.col`, which it cuts first, and before it cuts `par.col`. What `ids.col` holds once
/// `par.col` is read bounds the count, so the snapshot counts the records the cut keeps, not
/// records whose ids are gone, which it would look for in `ids.col` and fail on with an I/O
/// error, the index cut only once.
#[test]
fn counting_by_length_in_the_middle_of_a_cut_counts_the_records_the_cut_keeps() {
    let (_dir, root) = new_root();
    let (paused_tx, paused_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel::<()>();
    let (fs, _calls) = RecordingFs::new();
    let fail = fs.failing();
    // The writer's cut waits before par.col until the reader has counted.
    let fs = fs.with_hook((
        "truncate par.col".to_owned(),
        When::Before,
        Box::new(move || {
            paused_tx.send(()).unwrap();
            let resumed = resume_rx.recv_timeout(PATIENCE);
            resumed.expect("the reader counts");
        }),
    ));
    let mut writer = MetaStore::create_with_fs(&root, checked(fs)).unwrap();
    writer.set_parallelism(1);
    chain_past_a_damaged_frame(&writer, &root);
    fail.set("sync_data ids.col");
    // The reader's first FsOps call measures journal.off, once it has measured the index files:
    // right then, the writer's flush starts, and the reader goes on once the cut has reached
    // par.col.
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let (fs, _calls) = RecordingFs::new();
    let fs = fs.with_hook((
        "file_len journal.off".to_owned(),
        When::Before,
        Box::new(move || {
            go_tx.send(()).unwrap();
            let paused = paused_rx.recv_timeout(PATIENCE);
            paused.expect("the writer cuts ids.col");
        }),
    ));
    let reader = MetaStore::open_with_fs(&root, checked(fs)).unwrap();

    let writer = &writer;
    thread::scope(|scope| {
        let flush = scope.spawn(move || {
            let go = go_rx.recv_timeout(PATIENCE);
            go.expect("the reader opens its index");
            writer.flush()
        });
        let read = reader.read();
        resume_tx.send(()).unwrap();
        let cut = flush.join().unwrap();
        assert!(matches!(cut, Err(StoreError::Io(_))), "{cut:?}");
        let read = match read {
            Ok(read) => read,
            Err(e) => panic!("expected a snapshot, got {e:?}"),
        };
        assert_eq!(chained_lens(&root), chained_want(3), "cut");
        assert_eq!(read.count_changesets().unwrap(), 5);
        for n in 0..5 {
            let got = read.changeset(id(n)).unwrap();
            assert_eq!(got, Some(chained_stored(n)), "changeset {n}");
        }
    });
}

/// How many frames `journal.off` at `root` lists.
fn listed(root: &Utf8Path) -> u64 {
    file_len(Layout::new(root).journal_off()) / 8
}

/// Where a [`Fails`] fails its call.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Strike {
    /// The call does nothing, and fails: a write refused.
    Before,
    /// The call runs, then fails: a sync that fails after its write, the bytes in the file (the
    /// page cache) and the caller told they are not durable.
    After,
}

/// When armed, fails the next call named `call` ([`Call::named`]) once, as `strike` says; and,
/// while set, every call named `cut` before it runs. Records every call it sees, by name.
struct Fails {
    call: &'static str,
    strike: Strike,
    cut: Option<&'static str>,
    armed: AtomicBool,
    calls: Mutex<Vec<String>>,
}

impl Fails {
    fn new(call: &'static str, strike: Strike) -> Fails {
        Fails {
            call,
            strike,
            cut: None,
            armed: AtomicBool::new(false),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn arm(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }

    /// The calls made since this last ran.
    fn calls(&self) -> Vec<String> {
        std::mem::take(&mut *self.calls.lock().unwrap())
    }

    /// Whether the armed failure strikes `call`, at `strike`; disarms it if so.
    fn strikes(&self, call: &str, strike: Strike) -> bool {
        call == self.call && self.strike == strike && self.armed.swap(false, Ordering::SeqCst)
    }
}

impl Watch for Fails {
    fn before(&self, call: &Call<'_>) -> io::Result<()> {
        let named = call.named();
        self.calls.lock().unwrap().push(named.clone());
        if self.cut == Some(named.as_str()) || self.strikes(&named, Strike::Before) {
            return Err(io::Error::other(format!("injected: {named}")));
        }
        Ok(())
    }

    fn after_success(&self, call: &Call<'_>) -> io::Result<()> {
        let named = call.named();
        if self.strikes(&named, Strike::After) {
            return Err(io::Error::other(format!(
                "injected: {named} wrote, then its sync failed"
            )));
        }
        Ok(())
    }
}

/// A store at `root` on `fails`, under the order checker, its index synced one file at a time.
fn store_on(root: &Utf8Path, fails: &Arc<Watched<Fails>>) -> MetaStore {
    let mut store = MetaStore::create_with_fs(root, checked(Arc::clone(fails))).unwrap();
    store.set_parallelism(1);
    store
}

/// Set only in the child of `a_failed_log_sync_cuts_the_frame_and_reports_not_committed` that
/// must abort: the store root.
const ABORT_ROOT: &str = "SWARMOPS_TEST_META_ABORT_ROOT";

/// The child of [`a_failed_log_sync_cuts_the_frame_and_reports_not_committed`] that aborts: a
/// commit whose frame's fdatasync fails, and so does the cut that would take the frame back.
fn cut_fails_child(root: &Utf8Path) {
    let mut fails = Fails::new("append_durable journal.log", Strike::After);
    fails.cut = Some("truncate journal.log");
    let fails = Arc::new(Watched(fails, RealFs::new()));
    let store = MetaStore::create_with_fs(root, Arc::clone(&fails) as Arc<dyn FsOps>).unwrap();
    insert(&store, &[cs(0, &[])]);
    fails.arm();
    let mut write = store.write().unwrap();
    write.insert_changeset(cs(1, &[0])).unwrap();
    let outcome = write.commit();
    println!("{REPORT}the commit returned: {outcome:?}");
}

/// A journal append whose fdatasync fails once its frame is written whole (Linux reports a
/// failed writeback once and may drop the pages it could not write, so a later sync can succeed
/// without them: the fsyncgate pattern) leaves nothing a later writer could list: the writer,
/// still holding meta/lock, cuts journal.log back to where the frame starts and syncs the cut,
/// and the transaction did not commit ([`StoreError::NotCommitted`]; the API layer reports it as
/// `E_IO_TRANSIENT`, `committed: false`). So does an append that fails before it writes
/// anything, which leaves nothing to cut. Retrying commits the transaction afresh, as frame 1
/// (the order checker would refuse a frame listed after the failed sync). If the cut fails too,
/// the process aborts, saying why, so that no later write in it can list a frame that may not
/// be on disk; the store then opens as after a crash.
#[test]
fn a_failed_log_sync_cuts_the_frame_and_reports_not_committed() {
    if let Ok(root) = std::env::var(ABORT_ROOT) {
        cut_fails_child(Utf8Path::new(&root));
        return;
    }
    for strike in [Strike::After, Strike::Before] {
        let (_dir, root) = new_root();
        let fails = Arc::new(Watched(
            Fails::new("append_durable journal.log", strike),
            RealFs::new(),
        ));
        let store = store_on(&root, &fails);
        insert(&store, &[cs(0, &[])]); // frame 0
        let (frame_start, entries) = (log_len(&root), listed(&root));
        fails.calls();
        fails.arm();
        let mut write = store.write().unwrap();
        write.insert_changeset(cs(1, &[0])).unwrap();
        write.cas_ref("main", None, Some(id(1)), ACTOR).unwrap();
        match write.commit() {
            Err(StoreError::NotCommitted { source }) => {
                assert!(source.to_string().contains("injected"), "{source}");
            }
            other => panic!("expected NotCommitted, got {other:?}"),
        }
        let made = fails.calls();
        let cut = made.iter().any(|call| call == "truncate journal.log");
        assert_eq!(
            cut,
            strike == Strike::After,
            "a cut only of what was written: {made:?}"
        );
        if strike == Strike::After {
            assert_eq!(
                made[made.len() - 2..],
                ["append_durable journal.log", "truncate journal.log"],
                "the cut comes right after the failed append, under the lock"
            );
        }
        assert_eq!(
            (log_len(&root), listed(&root)),
            (frame_start, entries),
            "no frame of it in journal.log, none listed"
        );
        let read = store.read().unwrap();
        assert_eq!(read.changeset(id(1)).unwrap(), None, "not committed");
        assert_eq!(read.get_ref("main").unwrap(), None);
        drop(read);

        // Retrying commits it, afresh.
        let mut write = store.write().unwrap();
        let again = write.insert_changeset(cs(1, &[0])).unwrap();
        assert_eq!(again, InsertOutcome::Inserted { seq: 1 });
        let moved = write.cas_ref("main", None, Some(id(1)), ACTOR).unwrap();
        assert_eq!(moved, CasResult::Applied { old: None });
        assert_eq!(write.commit().unwrap().frame_seq, 1);
        let read = store.read().unwrap();
        assert_eq!(read.get_ref("main").unwrap(), Some(ref_state(1, 1)));
        assert_eq!(read.count_changesets().unwrap(), 2);
    }

    // The cut fails too: the process aborts, and the store opens as a crash would leave it.
    let (_dir, root) = new_root();
    let child = run(this_test(&test_name!(
        "a_failed_log_sync_cuts_the_frame_and_reports_not_committed"
    ))
    .env(ABORT_ROOT, &root));
    let stderr = String::from_utf8_lossy(&child.stderr);
    assert_eq!(
        child.status.signal(),
        Some(SIGABRT),
        "{:?}\n{stderr}",
        child.status
    );
    assert!(
        stderr.contains("could not be cut back off it"),
        "the abort says why: {stderr}"
    );
    let store = open_checked(&root).unwrap();
    store.write().unwrap().commit().unwrap();
    assert!(store.read().unwrap().count_changesets().unwrap() >= 1);
}

/// A journal append whose frame is durable in journal.log (its fdatasync succeeded: the commit
/// point) but whose listing in journal.off fails has committed: commit says so,
/// [`StoreError::PostCommit`] at the frame's seq (a warning to the repository and the API
/// layer), readers see the frame, and the next write lists it and replays it, so that retrying
/// the transaction finds it done. When the entry reached journal.off and its sync failed, the
/// writer first cuts journal.off back to the entries before it: an entry a failed writeback may
/// have lost must not stay listed while later entries are made durable after it, a hole a power
/// loss would leave among the listed frames (the order checker refuses that: W1).
#[test]
fn a_failed_off_append_after_the_log_sync_is_post_commit() {
    for strike in [Strike::After, Strike::Before] {
        let (_dir, root) = new_root();
        let fails = Arc::new(Watched(
            Fails::new("append_durable journal.off", strike),
            RealFs::new(),
        ));
        let store = store_on(&root, &fails);
        insert(&store, &[cs(0, &[])]); // frame 0
        fails.calls();
        fails.arm();
        let mut write = store.write().unwrap();
        write.insert_changeset(cs(1, &[0])).unwrap();
        write.cas_ref("main", None, Some(id(1)), ACTOR).unwrap();
        match write.commit() {
            Err(StoreError::PostCommit {
                frame_seq: 1,
                source,
            }) => {
                let StoreError::Io(failure) = &*source else {
                    panic!("expected the listing's I/O failure, got {source:?}");
                };
                assert_eq!(
                    (failure.op, failure.file.as_deref()),
                    (
                        swarmops_store::FileOp::Fs(swarmops_store::FsMethod::AppendDurable),
                        Some(Utf8Path::new("meta/journal.off"))
                    ),
                    "{failure}"
                );
                assert!(failure.to_string().contains("injected"), "{failure}");
            }
            other => panic!("expected PostCommit at frame 1, got {other:?}"),
        }
        let made = fails.calls();
        let cut = made.iter().any(|call| call == "truncate journal.off");
        assert_eq!(
            cut,
            strike == Strike::After,
            "a cut only of what was written: {made:?}"
        );
        assert_eq!(
            (listed(&root), frames(&root).len()),
            (1, 2),
            "the frame is whole in journal.log, unlisted"
        );
        let read = store.read().unwrap();
        assert_eq!(read.changeset(id(1)).unwrap(), Some(stored(1, &[0], 1, 2)));
        assert_eq!(read.get_ref("main").unwrap(), Some(ref_state(1, 1)));
        drop(read);

        // Retrying finds the transaction done: the write lists the frame and replays it first.
        let mut write = store.write().unwrap();
        assert_eq!(listed(&root), 2, "the write listed the frame");
        let again = write.insert_changeset(cs(1, &[0])).unwrap();
        assert_eq!(again, InsertOutcome::AlreadyPresent);
        let moved = write.cas_ref("main", None, Some(id(1)), ACTOR).unwrap();
        assert_eq!(
            moved,
            CasResult::Mismatch {
                actual: Some(id(1))
            }
        );
        write.commit().unwrap();
        let read = store.read().unwrap();
        assert_eq!(read.get_ref("main").unwrap(), Some(ref_state(1, 1)));
        assert_eq!(read.count_changesets().unwrap(), 2);
        assert_eq!(ref_file(&root, "main"), Some(ref_content(1, 1)));
    }
}

/// When the cut that takes a failed listing back off `journal.off` fails too (the entry was
/// written and its sync failed, then the cut is refused), the error tells both: the listing's
/// failure, and the cut's as what else failed. The transaction committed all the same
/// ([`StoreError::PostCommit`]).
#[test]
fn a_failed_listing_whose_cut_fails_too_tells_both() {
    let (_dir, root) = new_root();
    let mut fails = Fails::new("append_durable journal.off", Strike::After);
    fails.cut = Some("truncate journal.off");
    let fails = Arc::new(Watched(fails, RealFs::new()));
    let store = MetaStore::create_with_fs(&root, Arc::clone(&fails) as Arc<dyn FsOps>).unwrap();
    insert(&store, &[cs(0, &[])]);
    fails.arm();
    let mut write = store.write().unwrap();
    write.insert_changeset(cs(1, &[0])).unwrap();
    match write.commit() {
        Err(StoreError::PostCommit {
            frame_seq: 1,
            source,
        }) => {
            let StoreError::Io(failure) = &*source else {
                panic!("expected the listing's I/O failure, got {source:?}");
            };
            let also = failure.also.as_deref().unwrap_or_default();
            assert!(
                also.starts_with(
                    "cutting back what reached it failed too: truncate meta/journal.off: injected"
                ),
                "{failure}"
            );
        }
        other => panic!("expected PostCommit at frame 1, got {other:?}"),
    }
}

/// When the append of the `Compacted` marker after a compaction fails (refused before it wrote
/// anything, so there is nothing to cut), the transaction, which committed before it, says so
/// ([`StoreError::PostCommit`]) around the plain I/O failure the append was, saying what it was:
/// not [`StoreError::NotCommitted`], which would say that the transaction did not commit.
#[test]
fn a_failed_compacted_marker_is_told_as_the_append_it_was() {
    use std::sync::atomic::AtomicUsize;

    use swarmops_testkit::fs::FailingFs;

    let (_dir, root) = new_root();
    let armed = Arc::new(AtomicBool::new(false));
    let appends = Arc::new(AtomicUsize::new(0));
    let fs = {
        let (armed, appends) = (Arc::clone(&armed), Arc::clone(&appends));
        // The second append to journal.log once armed: the frame's, then the marker's.
        FailingFs::new(move |method, path| {
            method == "append_durable"
                && path.ends_with("journal.log")
                && armed.load(Ordering::SeqCst)
                && appends.fetch_add(1, Ordering::SeqCst) == 1
        })
    };
    let store = MetaStore::create_with_fs(&root, Arc::new(fs)).unwrap();
    armed.store(true, Ordering::SeqCst);
    let mut write = store.write().unwrap();
    for n in 0..=Index::COMPACT_THRESHOLD {
        write.insert_changeset(cs(n, &[])).unwrap();
    }
    match write.commit() {
        Err(StoreError::PostCommit {
            frame_seq: 0,
            source,
        }) => {
            let StoreError::Io(failure) = &*source else {
                panic!("expected the marker's I/O failure, got {source:?}");
            };
            assert_eq!(
                failure.also.as_deref(),
                Some(
                    "it was appending the Compacted marker, which was cut back off the journal \
                     (the compaction itself is done)"
                ),
                "{failure}"
            );
        }
        other => panic!("expected PostCommit at frame 0, got {other:?}"),
    }
    assert_eq!(frames(&root).len(), 1, "no marker frame in the journal");
}

/// A write whose tail check cannot list a frame that a transaction before it left whole and
/// unlisted (its own listing failed: committed, [`StoreError::PostCommit`]) has committed nothing
/// of its own: its transaction never began, and running it again lists the frame. So the error is
/// [`StoreError::NotCommitted`], naming the append that failed (the API layer's retryable
/// `E_IO_TRANSIENT`), not a plain [`StoreError::Io`]; what reached `journal.off` of the entry is
/// cut back off it first, so nothing more is listed. The next write lists the frame, and the
/// retried transaction finds it done.
#[test]
fn a_failed_listing_in_the_tail_check_is_not_committed() {
    use swarmops_store::{FileOp, FsMethod};

    for strike in [Strike::After, Strike::Before] {
        let (_dir, root) = new_root();
        let fails = Arc::new(Watched(
            Fails::new("append_durable journal.off", strike),
            RealFs::new(),
        ));
        let store = store_on(&root, &fails);
        insert(&store, &[cs(0, &[])]); // frame 0
        fails.arm();
        let mut write = store.write().unwrap();
        write.insert_changeset(cs(1, &[0])).unwrap();
        let committed = write.commit();
        assert!(
            matches!(committed, Err(StoreError::PostCommit { frame_seq: 1, .. })),
            "{committed:?}"
        );
        assert_eq!(listed(&root), 1, "frame 1 is whole and unlisted");

        // The next write's tail check lists frame 1, and that listing fails.
        fails.arm();
        match store.write() {
            Err(StoreError::NotCommitted { source }) => assert_eq!(
                (source.op, source.file.as_deref()),
                (
                    FileOp::Fs(FsMethod::AppendDurable),
                    Some(Utf8Path::new("meta/journal.off"))
                ),
                "{source}"
            ),
            other => panic!("expected NotCommitted, got {other:?}"),
        }
        assert_eq!(listed(&root), 1, "nothing more is listed");

        // Run again, the write lists the frame, and the retried transaction finds it done.
        let mut write = store.write().unwrap();
        assert_eq!(listed(&root), 2, "the write listed the frame");
        let again = write.insert_changeset(cs(1, &[0])).unwrap();
        assert_eq!(again, InsertOutcome::AlreadyPresent);
        write.commit().unwrap();
        assert_eq!(store.read().unwrap().count_changesets().unwrap(), 2);
    }
}

/// [`a_failed_listing_in_the_tail_check_is_not_committed`] for a committed snapshot: its own tail
/// check lists the frame a transaction before it left whole and unlisted, and when that listing
/// fails the snapshot is [`StoreError::NotCommitted`], naming the append that failed. Nothing more
/// is listed, and the next committed snapshot lists the frame and returns its changeset.
#[test]
fn a_failed_listing_in_read_committeds_tail_check_is_not_committed() {
    use swarmops_store::{FileOp, FsMethod};

    for strike in [Strike::After, Strike::Before] {
        // 1. Frame 1 committed, its listing failed: whole and unlisted.
        let (_dir, root) = new_root();
        let fails = Arc::new(Watched(
            Fails::new("append_durable journal.off", strike),
            RealFs::new(),
        ));
        let store = store_on(&root, &fails);
        insert(&store, &[cs(0, &[])]); // frame 0
        fails.arm();
        let mut write = store.write().unwrap();
        write.insert_changeset(cs(1, &[0])).unwrap();
        let committed = write.commit();
        assert!(
            matches!(committed, Err(StoreError::PostCommit { frame_seq: 1, .. })),
            "{committed:?}"
        );
        assert_eq!(listed(&root), 1, "frame 1 is whole and unlisted");

        // 2. The committed snapshot's tail check lists frame 1, and that listing fails.
        fails.arm();
        match store.read_committed() {
            Err(StoreError::NotCommitted { source }) => assert_eq!(
                (source.op, source.file.as_deref()),
                (
                    FileOp::Fs(FsMethod::AppendDurable),
                    Some(Utf8Path::new("meta/journal.off"))
                ),
                "{source}"
            ),
            Ok((_, repair)) => panic!("expected NotCommitted, got a snapshot after {repair:?}"),
            Err(other) => panic!("expected NotCommitted, got {other:?}"),
        }
        assert_eq!(listed(&root), 1, "nothing more is listed");

        // 3. Taken again, the snapshot lists the frame and returns its changeset.
        let (read, repair) = store.read_committed().unwrap();
        assert_eq!(repair.frames_listed, 1, "{repair:?}");
        assert_eq!(read.count_changesets().unwrap(), 2);
        drop(read);
        assert_eq!(listed(&root), 2, "the snapshot listed the frame");
    }
}

/// What a write transaction on `store` repaired, as it reports it once open; the write is dropped
/// uncommitted (its repairs stay made). Then the next write repairs nothing.
#[track_caller]
/// The empty [`Recovered`] with `set` applied: `recovery(|r| r.views_rebuilt = true)`.
fn recovery(set: impl FnOnce(&mut Recovered)) -> Recovered {
    let mut r = Recovered::default();
    set(&mut r);
    r
}

fn recovered_by_a_write(store: &MetaStore) -> Recovered {
    let recovered = store.write().unwrap().recovered();
    assert_eq!(
        store.write().unwrap().recovered(),
        Recovered::default(),
        "the write after one that repaired {recovered:?}"
    );
    recovered
}

/// A [`failing_store`] holding changeset 0, whose last transaction committed and then failed at
/// the call `failing` names ([`StoreError::PostCommit`]): one that inserts changeset 1, or with
/// `moves`, one that moves main to changeset 0. The call fails no more.
fn committed_then_failed(
    failing: &'static str,
    moves: bool,
) -> (TempDir, Utf8PathBuf, MetaStore, Failing) {
    let (dir, root) = new_root();
    let (store, fail) = failing_store(&root);
    insert(&store, &[cs(0, &[])]);
    fail.set(failing);
    let mut write = store.write().unwrap();
    if moves {
        write.cas_ref("main", None, Some(id(0)), ACTOR).unwrap();
    } else {
        write.insert_changeset(cs(1, &[0])).unwrap();
    }
    let committed = write.commit();
    assert!(
        matches!(committed, Err(StoreError::PostCommit { .. })),
        "{failing}: {committed:?}"
    );
    fail.clear();
    (dir, root, store, fail)
}

/// A write transaction reports each repair its recovery makes (`MetaWrite::recovered`), what
/// the writers before it left when they crashed or failed after their commit point, and
/// nothing else: a torn tail cut off `journal.log`; a frame whose listing failed, listed and
/// replayed; index records a failed append left torn, cut and replayed; a ref file a failed
/// replace never wrote, replayed; a compaction a crash interrupted, finished; a frame whose sync
/// failed in the tail check, written afresh and listed; zeroed entries at the end of
/// `journal.off`, dropped, and their frames listed again. The write after each repairs nothing.
#[test]
fn a_write_reports_each_repair_it_makes() {
    let none = Recovered::default();

    // A hand-torn frame at the end of journal.log: a header announcing 100 bytes, and 10.
    let (_dir, root, store) = new_store();
    insert(&store, &[cs(0, &[])]);
    let clean = log_len(&root);
    let torn = [&100_u32.to_le_bytes()[..], &[0; 4], &[0xee; 10]].concat();
    append_bytes(Layout::new(&root).journal_log(), &torn);
    let cut = Some(LogCut::new(clean, 18));
    let want = recovery(|r| {
        r.log_cut = cut;
    });
    assert_eq!(recovered_by_a_write(&store), want, "a torn tail");

    // A step after the commit point that failed (PostCommit), in a transaction that inserts
    // changeset 1 or, with `moves`, moves main to changeset 0: the listing in journal.off
    // (nothing materialized), listed and its changeset replayed; the append of ids.col (the
    // record whole in the four files written before it), cut and replayed; the replace of
    // refs/main, replayed.
    let failed_after_the_commit_point = [
        (
            "append_durable journal.off",
            false,
            recovery(|r| {
                r.frames_listed = 1;
                r.records_replayed = 1;
            }),
        ),
        (
            "append ids.col",
            false,
            recovery(|r| {
                r.index_records_dropped = 1;
                r.records_replayed = 1;
            }),
        ),
        (
            "atomic_replace main",
            true,
            recovery(|r| {
                r.refs_replayed = 1;
            }),
        ),
    ];
    for (failing, moves, want) in failed_after_the_commit_point {
        let (_dir, root, store, _) = committed_then_failed(failing, moves);
        assert_eq!(recovered_by_a_write(&store), want, "{failing}");
        if moves {
            assert_eq!(ref_file(&root, "main"), Some(ref_content(0, 1)));
        }
    }

    // A crash between two view renames of a compaction: dh.key and dh.rec still the empty files
    // the store was created with. The compaction is finished.
    let (_dir, root, store) = new_store();
    let all: Vec<ChangesetMeta> = (0..1100).map(|n| cs(n, &[])).collect();
    assert!(insert(&store, &all).checkpointed, "the write compacts");
    let sorted = Layout::new(&root).meta_path("sorted");
    fs::write(sorted.join("dh.key"), []).unwrap();
    fs::write(sorted.join("dh.rec"), []).unwrap();
    let want = recovery(|r| {
        r.views_rebuilt = true;
    });
    assert_eq!(recovered_by_a_write(&store), want, "a compaction cut short");

    // The tail check's sync of a frame whose listing failed fails too (storage spec §2 rule 3
    // (c)): the frame is written afresh, listed, and replayed.
    let (_dir, _root, store, fail) = committed_then_failed("append_durable journal.off", false);
    fail.set("sync_data journal.log");
    let recovered = store.write().unwrap().recovered();
    fail.clear();
    let want = recovery(|r| {
        r.frames_listed = 1;
        r.frames_rewritten = 1;
        r.records_replayed = 1;
    });
    assert_eq!(recovered, want, "a failed sync in the tail check");
    assert_eq!(recovered_by_a_write(&store), none, "after the rewrite");

    // The entries of frames 1 and 2 zeroed at the end of journal.off, its length kept: what a
    // power loss leaves of entries appended and never synced. They are dropped, and the frames
    // listed again. A process that opens the store after the power loss repairs it, on the real
    // filesystem, under the order checker.
    let (_dir, root, store) = new_store();
    for n in 0..3 {
        insert(&store, &[cs(n, &[])]); // frames 0..=2
    }
    drop(store);
    let off = Layout::new(&root).journal_off();
    let mut entries = fs::read(&off).unwrap();
    entries[8..].fill(0);
    fs::write(&off, entries).unwrap();
    let after_power_loss = open_checked(&root).unwrap();
    let want = recovery(|r| {
        r.frames_listed = 2;
        r.off_bytes_dropped = 16;
    });
    assert_eq!(
        recovered_by_a_write(&after_power_loss),
        want,
        "zeroed entries"
    );
    assert_eq!(listed(&root), 3);
}

/// [`steady_writes_report_no_recovery`] on the filesystem `fs` makes for each handle.
fn steady_writes(fs: impl Fn() -> Arc<dyn FsOps>) {
    const COMMITS: u64 = 1100;
    let (_dir, root) = new_root();
    let first = MetaStore::create_with_fs(&root, fs()).unwrap();
    let second = MetaStore::open_with_fs(&root, fs()).unwrap();
    let mut checkpoints = 0;
    for n in 0..COMMITS {
        let store = if n % 2 == 0 { &first } else { &second };
        let mut write = store.write().unwrap();
        assert_eq!(write.recovered(), Recovered::default(), "commit {n}");
        write.insert_changeset(chained(n)).unwrap();
        let expected = n.checked_sub(1).map(id);
        let moved = write.cas_ref("main", expected, Some(id(n)), ACTOR).unwrap();
        assert_eq!(moved, CasResult::Applied { old: expected }, "commit {n}");
        checkpoints += u64::from(write.commit().unwrap().checkpointed);
    }
    for store in [&first, &second] {
        assert_eq!(store.write().unwrap().recovered(), Recovered::default());
    }
    // Every 32nd commit before the compaction (32), the compaction's at the 1,025th, and every
    // 32nd after it (2).
    assert_eq!(checkpoints, 35);
    assert_eq!(compacted_frames(&first).len(), 1, "one compaction");
    let read = first.read().unwrap();
    assert_eq!(read.count_changesets().unwrap(), COMMITS);
    assert_eq!(
        read.get_ref("main").unwrap(),
        Some(ref_state(COMMITS - 1, COMMITS))
    );
}

/// In steady state no write repairs anything: two handles on one store alternate 1,100 commits
/// (a changeset each, and main moved to it), across 35 checkpoints and one compaction, and
/// every write's recovery reports nothing. Replay reads the frames past the checkpoint again at
/// every write, a ref move among them each time, and none of that counts: a field that grew in
/// steady state would make the report worthless. The filesystem skips every flush, which the
/// report does not depend on; the next test runs the same on the real filesystem.
#[test]
fn steady_writes_report_no_recovery() {
    steady_writes(|| checked(NoSyncFs));
}

/// [`steady_writes_report_no_recovery`] on the real filesystem, every flush made.
#[test]
#[ignore = "slow, ~20 s: 1,100 fsync'd commits; just check-slow runs it"]
fn steady_writes_on_the_real_filesystem_report_no_recovery() {
    steady_writes(|| checked(RealFs::new()));
}

/// An injected failure is named by the call it struck, its file relative to the store's root:
/// the frame's append to `journal.log`, failing once it wrote ([`StoreError::NotCommitted`]);
/// the replacement of `meta/refs/main` after the commit point; and the first index file the
/// checkpoint syncs, `meta/parents.log` (each a [`StoreError::PostCommit`] at the frame). The
/// double's own error follows the call, and the store's path is nowhere in the text.
#[test]
fn an_injected_failure_names_the_call_it_struck() {
    use swarmops_store::{FileOp, FsMethod};

    let cases = [
        // The struck call, its method, its file, and the frames committed before.
        (
            "append_durable journal.log",
            FsMethod::AppendDurable,
            "meta/journal.log",
            1,
        ),
        (
            "atomic_replace main",
            FsMethod::AtomicReplace,
            "meta/refs/main",
            1,
        ),
        (
            "sync_data parents.log",
            FsMethod::SyncData,
            "meta/parents.log",
            31,
        ),
    ];
    for (call, method, file, before) in cases {
        let (_dir, root) = new_root();
        let fails = Arc::new(Watched(Fails::new(call, Strike::After), RealFs::new()));
        let store = store_on(&root, &fails);
        for n in 0..before {
            insert(&store, &[cs(n, &[])]);
        }
        fails.arm();
        let mut write = store.write().unwrap();
        write.insert_changeset(cs(before, &[])).unwrap();
        write
            .cas_ref("main", None, Some(id(before)), ACTOR)
            .unwrap();
        let outcome = write.commit();
        let reported = match &outcome {
            Err(StoreError::NotCommitted { .. }) => method == FsMethod::AppendDurable,
            Err(StoreError::PostCommit { frame_seq, .. }) => {
                method != FsMethod::AppendDurable && *frame_seq == before
            }
            _ => false,
        };
        assert!(reported, "{call}: {outcome:?}");
        let Some(failure) = outcome.as_ref().err().and_then(io_failure) else {
            panic!("{call}: no I/O failure in {outcome:?}");
        };
        assert_eq!(
            (failure.op, failure.file.as_deref()),
            (FileOp::Fs(method), Some(Utf8Path::new(file))),
            "{call}: {failure}"
        );
        let text = failure.to_string();
        assert!(
            text.starts_with(&format!("{method} {file}: injected: {call}")),
            "{text}"
        );
        assert!(!text.contains(root.as_str()), "no host path: {text}");
    }
}

/// One row of [`a_direct_read_that_fails_names_its_file`]: what it damages in `meta/`, what it
/// runs on the store, and the failure it expects (the operation, the file, the error's kind).
struct DirectRead {
    what: &'static str,
    damage: fn(&Utf8Path),
    run: fn(&Utf8Path) -> Result<(), StoreError>,
    op: swarmops_store::FileOp,
    file: &'static str,
    kind: io::ErrorKind,
}

/// Puts an empty directory where the file `meta/<file>` was.
fn a_directory_for(meta: &Utf8Path, file: &str) {
    fs::remove_file(meta.join(file)).unwrap();
    fs::create_dir(meta.join(file)).unwrap();
}

/// A failure of the metadata store's own reads, off the `FsOps` seam, names its file relative to
/// the store's root as well, and never the root: a `journal.off` that is gone fails the open of
/// it (`NotFound`); a `journal.ckpt` that is a directory, or three bytes long, and a ref file
/// that is a directory fail their reads as damage, and a `meta/lock` that is a directory its
/// open (`InvalidData`: a store file that is not a regular file is refused before it is opened);
/// and each of the nine index files, gone, is [`StoreError::IndexCorrupt`] naming it. Root-safe:
/// no file mode is involved.
#[test]
fn a_direct_read_that_fails_names_its_file() {
    use swarmops_store::{FileOp, FsMethod};

    let (_dir, template, store) = new_store();
    insert(&store, &[cs(0, &[])]);
    let moved = cas(&store, "main", None, Some(0));
    assert_eq!(moved, CasResult::Applied { old: None });
    drop(store);
    let damaged = |damage: &dyn Fn(&Utf8Path)| {
        let (dir, root) = new_root();
        copy_dir(template.as_std_path(), root.as_std_path());
        damage(&Layout::new(&root).meta_dir());
        (dir, root)
    };
    let read: fn(&Utf8Path) -> Result<(), StoreError> =
        |root| MetaStore::open(root).and_then(|store| store.read().map(drop));
    let cases = [
        DirectRead {
            what: "journal.off gone",
            damage: |meta| fs::remove_file(meta.join("journal.off")).unwrap(),
            run: |root| MetaStore::open(root).map(drop),
            op: FileOp::Open,
            file: "meta/journal.off",
            kind: io::ErrorKind::NotFound,
        },
        DirectRead {
            what: "journal.ckpt a directory",
            damage: |meta| a_directory_for(meta, "journal.ckpt"),
            run: read,
            op: FileOp::Fs(FsMethod::Read),
            file: "meta/journal.ckpt",
            kind: io::ErrorKind::InvalidData,
        },
        DirectRead {
            what: "journal.ckpt of three bytes",
            damage: |meta| fs::write(meta.join("journal.ckpt"), [0; 3]).unwrap(),
            run: read,
            op: FileOp::Fs(FsMethod::Read),
            file: "meta/journal.ckpt",
            kind: io::ErrorKind::InvalidData,
        },
        DirectRead {
            what: "refs/main a directory",
            damage: |meta| a_directory_for(meta, "refs/main"),
            run: read,
            op: FileOp::Fs(FsMethod::Read),
            file: "meta/refs/main",
            kind: io::ErrorKind::InvalidData,
        },
        DirectRead {
            what: "meta/lock a directory",
            damage: |meta| a_directory_for(meta, "lock"),
            run: |root| MetaStore::open(root).and_then(|store| store.write().map(drop)),
            op: FileOp::Open,
            file: "meta/lock",
            kind: io::ErrorKind::InvalidData,
        },
    ];
    for case in cases {
        let (_dir, root) = damaged(&case.damage);
        let failure = match (case.run)(&root) {
            Err(StoreError::Io(failure)) => failure,
            other => panic!("{}: expected an I/O failure, got {other:?}", case.what),
        };
        assert_eq!(
            (failure.op, failure.file.as_deref(), failure.kind()),
            (case.op, Some(Utf8Path::new(case.file)), case.kind),
            "{}: {failure}",
            case.what
        );
        let text = failure.to_string();
        assert!(!text.contains(root.as_str()), "{}: {text}", case.what);
        if case.what == "journal.ckpt of three bytes" {
            assert_eq!(text, "read meta/journal.ckpt: holds 3 bytes, not a u64le");
        }
    }

    for file in [
        "idx/ids.col",
        "idx/gen.col",
        "idx/dh.col",
        "idx/par.col",
        "parents.log",
        "sorted/ids.key",
        "sorted/ids.rec",
        "sorted/dh.key",
        "sorted/dh.rec",
    ] {
        let (_dir, root) = damaged(&|meta| fs::remove_file(meta.join(file)).unwrap());
        match Index::open(&Layout::new(&root), Arc::new(RealFs::new())) {
            Err(StoreError::IndexCorrupt { what }) => {
                assert_eq!(what, format!("meta/{file} is missing"));
            }
            other => panic!("{file} gone: expected IndexCorrupt, got {other:?}"),
        }
    }
}

/// More reads of the metadata store's own, off the `FsOps` seam, that name their file relative
/// to the store's root and never the root, as [`a_direct_read_that_fails_names_its_file`]'s do:
/// a `meta/refs/release` that is a regular file fails the listing of the release refs
/// (`read_dir`, `ENOTDIR`); and a `journal.log`, an index column or a sorted view file that is
/// a directory fails its open as damage (`InvalidData`: a store file that is not a regular file
/// is refused before it is opened), not as a missing file. Root-safe: no file mode is
/// involved.
#[test]
fn a_listing_or_an_open_that_fails_names_its_file() {
    use swarmops_store::FileOp;

    let (_dir, template, store) = new_store();
    insert(&store, &[cs(0, &[])]);
    drop(store);
    let index: fn(&Utf8Path) -> Result<(), StoreError> =
        |root| Index::open(&Layout::new(root), Arc::new(RealFs::new())).map(drop);
    let cases = [
        DirectRead {
            what: "refs/release a regular file",
            damage: |meta| {
                let release = meta.join("refs/release");
                if release.is_dir() {
                    fs::remove_dir(&release).unwrap();
                }
                fs::write(release, b"not a directory").unwrap();
            },
            run: |root| MetaStore::open(root).and_then(|store| store.read().map(drop)),
            op: FileOp::ReadDir,
            file: "meta/refs/release",
            kind: io::ErrorKind::NotADirectory,
        },
        DirectRead {
            what: "journal.log a directory",
            damage: |meta| a_directory_for(meta, "journal.log"),
            run: |root| MetaStore::open(root).map(drop),
            op: FileOp::Open,
            file: "meta/journal.log",
            kind: io::ErrorKind::InvalidData,
        },
        DirectRead {
            what: "idx/ids.col a directory",
            damage: |meta| a_directory_for(meta, "idx/ids.col"),
            run: index,
            op: FileOp::Open,
            file: "meta/idx/ids.col",
            kind: io::ErrorKind::InvalidData,
        },
        DirectRead {
            what: "sorted/dh.rec a directory",
            damage: |meta| a_directory_for(meta, "sorted/dh.rec"),
            run: index,
            op: FileOp::Open,
            file: "meta/sorted/dh.rec",
            kind: io::ErrorKind::InvalidData,
        },
    ];
    for case in cases {
        let (_dir, root) = new_root();
        copy_dir(template.as_std_path(), root.as_std_path());
        (case.damage)(&Layout::new(&root).meta_dir());
        let failure = match (case.run)(&root) {
            Err(StoreError::Io(failure)) => failure,
            other => panic!("{}: expected an I/O failure, got {other:?}", case.what),
        };
        assert_eq!(
            (failure.op, failure.file.as_deref(), failure.kind()),
            (case.op, Some(Utf8Path::new(case.file)), case.kind),
            "{}: {failure}",
            case.what
        );
        let text = failure.to_string();
        assert!(!text.contains(root.as_str()), "{}: {text}", case.what);
    }
}

/// The I/O failure `e` reports: its own, a [`StoreError::NotCommitted`]'s, or that of the step
/// a [`StoreError::PostCommit`] says failed; `None` for an error of another kind.
fn io_failure(e: &StoreError) -> Option<&swarmops_store::IoFailure> {
    match e {
        StoreError::Io(failure) | StoreError::NotCommitted { source: failure } => Some(failure),
        StoreError::PostCommit { source, .. } => io_failure(source),
        _ => None,
    }
}

#[cfg(feature = "test-hooks")]
per_format! {
/// An injected I/O error at any filesystem call of a write transaction, and of one that
/// compacts, is reported honestly: a transaction that returns, or says it committed
/// ([`StoreError::PostCommit`]), is in the store; one that fails otherwise is not; and the store
/// always opens and takes the next write. The transaction is swept call by call (k = 1..=N + 1,
/// N counted by a dry run), its k-th call failing once with `EIO` (a flushing call does its work
/// first, then fails) or `ENOSPC` (the call does nothing), under the order checker. Every error
/// names the call that failed, as `FailAtFs` recorded it: its method, and the path it acted on
/// relative to the store's root, with the injected errno. The same with every call from the
/// k-th on failing is [`persistent_failures_at_every_store_call_report_honestly`], in the slow
/// suite.
fn fail_at_every_store_call_reports_honestly(format) {
    for shape in [Template::Checkpoint, Template::Compaction] {
        let (_dir, template, n) = sweep_template(shape, format);
        let calls = calls_of(&template, |store| {
            swept(store, n).unwrap();
        });
        // FORMAT 2 keeps its ref files flat in refs2/: no ref directory to make and sync, two
        // calls fewer than FORMAT 1's.
        let floor = match format {
            StoreFormat::V1 => 20,
            StoreFormat::V2 => 18,
            other => panic!("no call floor for {other:?}"),
        };
        assert!(calls > floor, "{calls}");
        let kind = match shape {
            Template::Checkpoint => "a checkpoint",
            Template::Compaction => "a compaction",
        };
        let mut post_commit = 0;
        for error in [InjectedError::Eio, InjectedError::Enospc] {
            for k in 1..=calls + 1 {
                let what = format!("{kind}, call {k} of {calls} failing once with {error:?}");
                let (_dir, root) = copy_of(&template);
                let fail = FailAt::new(k, FailMode::Once, error);
                let failing = Arc::new(FailAtFs::new(NoSyncFs, fail));
                let mut store =
                    MetaStore::open_with_fs(&root, checked(Arc::clone(&failing))).unwrap();
                store.set_parallelism(1);
                let outcome = swept(&store, n);
                drop(store);
                assert!(k <= calls || outcome.is_ok(), "{what}: {outcome:?}");
                if let Err(e) = &outcome {
                    let failed = failing.failed_calls();
                    let [(method, path)] = failed.as_slice() else {
                        panic!("{what}: one call failed: {failed:?}");
                    };
                    let file = path.strip_prefix(&root).ok().and_then(Utf8Path::from_path);
                    let named = io_failure(e).map(|failure| {
                        (failure.op, failure.file.as_deref(), failure.raw_os_error())
                    });
                    let injected = error.os_error().raw_os_error();
                    assert_eq!(
                        named,
                        Some(((*method).into(), file, injected)),
                        "{what}: the error names the call that failed: {e}"
                    );
                }
                if let Err(e @ StoreError::PostCommit { source, .. }) = &outcome {
                    assert!(
                        !matches!(**source, StoreError::NotCommitted { .. }),
                        "{what}: a step after the commit point reports: {e}"
                    );
                }
                post_commit += usize::from(matches!(outcome, Err(StoreError::PostCommit { .. })));
                assert_recovers(&root, n, &[(n, said(&outcome))], &what);
            }
        }
        assert!(
            post_commit > 0,
            "{kind}: some failure came after the commit point"
        );
    }
}
}

#[cfg(feature = "test-hooks")]
per_format! {
/// [`fail_at_every_store_call_reports_honestly`] with every mutating call from the k-th on
/// failing (reads aside), as a device gone bad: each run in a child process, since a failed
/// journal append whose cut fails too aborts (the documented abort, which says why on stderr).
/// A run that aborts may have committed or not, but never half: the store opens, and holds the
/// transaction whole or not at all.
#[ignore = "slow, ~15 s: ~140 child processes; just check-slow runs it"]
fn persistent_failures_at_every_store_call_report_honestly(format) {
    let test = swarmops_testkit::formats::test_of(format);
    if let (Ok(root), Ok(plan)) = (std::env::var(SWEEP_ROOT), std::env::var(SWEEP_PLAN)) {
        sweep_child(Utf8Path::new(&root), &plan);
        return;
    }
    let mut aborted = 0;
    for shape in [Template::Checkpoint, Template::Compaction] {
        let (_dir, template, n) = sweep_template(shape, format);
        let calls = calls_of(&template, |store| {
            swept(store, n).unwrap();
        });
        let kind = match shape {
            Template::Checkpoint => "a checkpoint",
            Template::Compaction => "a compaction",
        };
        for errno in ["eio", "enospc"] {
            for k in 1..=calls {
                let what = format!("{kind}, calls {k}.. of {calls} failing with {errno}");
                let (_dir, root) = copy_of(&template);
                let said =
                    run_sweep_child(&test_name!(test), &root, &format!("fail:{k}:{errno}:{n}"));
                aborted += usize::from(said == Said::Aborted);
                assert_recovers(&root, n, &[(n, said)], &what);
            }
        }
    }
    assert!(
        aborted > 0,
        "a failed journal append whose cut fails too aborts"
    );
}
}

#[cfg(feature = "test-hooks")]
per_format! {
/// Errors while a writer recovers from a crash (SQLite's compound failure: an I/O error while
/// recovering from a prior crash) leave the store openable. The swept transaction is crashed at
/// each of its calls in turn (a child each); then, on each crashed store, the next transaction (a
/// changeset of its own) fails once with `EIO` at each call its write's recovery makes (the tail
/// check, the index repair, the replay: counted by a dry run on the crashed store). The crashed
/// transaction is in the store whole or not at all, the next one exactly when it said it
/// committed, and the store takes a write and a flush after. The next transaction's own calls,
/// past its recovery, are [`fail_at_every_store_call_reports_honestly`]'s.
fn errors_during_recovery_leave_the_store_openable(format) {
    const NEXT: u64 = 1000;
    let test = swarmops_testkit::formats::test_of(format);
    if let (Ok(root), Ok(plan)) = (std::env::var(SWEEP_ROOT), std::env::var(SWEEP_PLAN)) {
        sweep_child(Utf8Path::new(&root), &plan);
        return;
    }
    let next = |store: &MetaStore| -> Result<CommitInfo, StoreError> {
        let mut write = store.write()?;
        write.insert_changeset(changeset(NEXT))?;
        write.commit()
    };
    let (_dir, template, n) = sweep_template(Template::Checkpoint, format);
    let calls = calls_of(&template, |store| {
        swept(store, n).unwrap();
    });
    let mut most = 0;
    for k1 in 1..=calls {
        let (_crashed_dir, crashed) = copy_of(&template);
        let crashed_said = run_sweep_child(&test_name!(test), &crashed, &format!("crash:{k1}:{n}"));
        assert_eq!(crashed_said, Said::Aborted, "crash at {k1}");
        let recovery = calls_of(&crashed, |store| {
            drop(store.write().unwrap());
        });
        most = most.max(recovery);
        for k2 in 1..=recovery {
            let what = format!("crash at {k1}, then recovery call {k2} of {recovery} failing");
            let (_dir, root) = copy_of(&crashed);
            let fail = FailAt::new(k2, FailMode::Once, InjectedError::Eio);
            let mut store =
                MetaStore::open_with_fs(&root, checked(FailAtFs::new(NoSyncFs, fail))).unwrap();
            store.set_parallelism(1);
            let outcome = next(&store);
            drop(store);
            assert_recovers(
                &root,
                n,
                &[(n, Said::Aborted), (NEXT, said(&outcome))],
                &what,
            );
        }
    }
    assert!(
        most > 4,
        "some crash left recovery work: {most} calls at most"
    );
}
}

/// Asserts that `result` is the refusal of a request past the layout's limit `what`, which
/// holds `max`, asking for `got`.
#[track_caller]
fn assert_limit<T: std::fmt::Debug>(
    result: Result<T, StoreError>,
    what: LimitKind,
    max: u64,
    got: u64,
) {
    match result {
        Err(StoreError::Limit {
            what: limit,
            max: most,
            got: asked,
        }) => assert_eq!((limit, most, asked), (what, max, got)),
        other => panic!("expected Limit {{ {what:?}, {max}, {got} }}, got {other:?}"),
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
    let refused = write.cas_ref("main", main, Some(id(2)), ACTOR);
    assert_limit(refused, LimitKind::Records, 65_535, 65_536);
    let refused = write.insert_changeset(cs(3, &[]));
    assert_limit(refused, LimitKind::Records, 65_535, 65_536);
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

/// A changeset with more parents than a journal record and `par.col` count in one byte (255)
/// is refused as it is queued, [`StoreError::Limit`] with 256 (the limit is checked before the
/// parents are looked up, so none of them need be in the store), and nothing of it is queued:
/// the transaction commits what it queued besides. The repository refuses such a changeset
/// before it reaches the store, so only a direct call meets this.
#[test]
fn insert_changeset_refuses_more_than_255_parents() {
    let (_dir, root, store) = new_store();
    let parents: Vec<u64> = (0..256).collect();
    let mut write = store.write().unwrap();
    let refused = write.insert_changeset(cs(1000, &parents));
    assert_limit(refused, LimitKind::Parents, 255, 256);
    write.insert_changeset(cs(1, &[])).unwrap();
    write.commit().unwrap();
    assert_eq!(
        frames(&root)[0].records.len(),
        1,
        "the refused changeset was not queued"
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
    let reader = MetaStore::open_with_fs(root, checked(fs)).unwrap();
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

/// The child of [`the_test_hooks_build_the_filesystem_they_name`]: makes, through the
/// filesystem the hooks ask for, the calls the hook in its environment acts on, and reports
/// what each returned.
#[cfg(feature = "test-hooks")]
fn hooks_child(dir: &Utf8Path) {
    let hooked = swarmops_store::HookedFs::from_env().expect("a hook is set");
    println!("{REPORT}counts calls: {}", hooked.counts_calls());
    let fs = hooked.into_fs();
    let meta = Layout::new(dir).meta_dir().into_std_path_buf();
    std::fs::create_dir_all(&meta).unwrap();
    let (log, off) = (meta.join("journal.log"), meta.join("journal.off"));
    if std::env::var_os(swarmops_store::FAIL_AT_ENV).is_some() {
        // Calls 1 to 4: a read, an append, a read, a sync.
        let outcomes = [
            fs.file_len(&log).map(drop),
            fs.append(&log, b"x").map(drop),
            fs.file_len(&log).map(drop),
            fs.sync_data(&log),
        ];
        let errnos: Vec<String> = outcomes
            .iter()
            .map(|outcome| match outcome {
                Ok(()) => "ok".to_owned(),
                Err(e) => e
                    .raw_os_error()
                    .map_or("?".to_owned(), |errno| errno.to_string()),
            })
            .collect();
        println!("{REPORT}calls: {}", errnos.join(" "));
    } else {
        // Written, never synced, then listed: W1 refuses it.
        let at = fs.append(&log, b"\x01\0\0\0crc!x").unwrap();
        fs.append_durable(&off, &at.to_le_bytes()).unwrap();
        println!("{REPORT}listed an unsynced frame");
    }
}

/// The test hooks reach a whole process through its environment ([`HookedFs::from_env`],
/// which the stores open on): `SWARMOPS_TEST_ORDER_CHECK=1` checks the writes, and a call that
/// breaks a rule panics, naming it, while counting no calls (the stores keep their
/// parallelism); `SWARMOPS_TEST_FAIL_AT=2:persistent:enospc` fails call 2 with ENOSPC, and every
/// mutating call after it, reads aside.
#[test]
#[cfg(feature = "test-hooks")]
fn the_test_hooks_build_the_filesystem_they_name() {
    const TEST: &str = "the_test_hooks_build_the_filesystem_they_name";
    if let Some(dir) = std::env::var_os(HOOKS_ROOT) {
        hooks_child(&Utf8PathBuf::from_path_buf(dir.into()).unwrap());
        return;
    }
    let (_dir, root) = new_root();
    let checked = run(this_test(&test_name!(TEST))
        .env(HOOKS_ROOT, &root)
        .env(swarmops_store::ORDER_CHECK_ENV, "1"));
    let stderr = String::from_utf8_lossy(&checked.stderr);
    assert!(
        !checked.status.success() && stderr.contains("order check: W1 violated by append_durable"),
        "{:?}\n{stderr}",
        checked.status
    );
    let stdout = String::from_utf8_lossy(&checked.stdout);
    assert!(stdout.contains("counts calls: false"), "{stdout}");

    let (_dir, root) = new_root();
    let failing = run(this_test(&test_name!(TEST))
        .env(HOOKS_ROOT, &root)
        .env(swarmops_store::FAIL_AT_ENV, "2:persistent:enospc"));
    assert!(failing.status.success(), "{:?}", failing.status);
    let stdout = String::from_utf8_lossy(&failing.stdout);
    let reported: Vec<&str> = stdout
        .lines()
        .filter_map(|line| line.strip_prefix(REPORT))
        .collect();
    assert_eq!(
        reported,
        ["counts calls: true", "calls: ok 28 ok 28"],
        "{stdout}"
    );
}

#[test]
#[cfg(feature = "test-hooks")]
fn crash_at_any_create_call_leaves_a_store_open_or_create_takes() {
    if let Ok(root) = std::env::var(CREATE_CRASH_ROOT) {
        let k = std::env::var(CRASH_AT_ENV).unwrap().parse().unwrap();
        let fs = checked(CrashAtFs::new(RealFs::new(), k));
        MetaStore::create_with_fs(Utf8Path::new(&root), fs).unwrap();
        return;
    }
    let calls = u64::try_from(CREATE_CALLS.len()).unwrap();
    for k in 1..=calls + 1 {
        let (_dir, root) = new_root();
        let child = run(this_test(&test_name!(
            "crash_at_any_create_call_leaves_a_store_open_or_create_takes"
        ))
        .env(CREATE_CRASH_ROOT, &root)
        .env(CRASH_AT_ENV, k.to_string()));
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
            assert_format(open_checked(&root), &format!("open after a crash at {k}"));
            create_checked(&root).unwrap();
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
    assert_format(open_checked(&root), "a staged meta/ is no store");
    create_checked(&root).unwrap();
    assert_fresh_meta(&root);
    assert!(!Layout::new(&root).meta_path(".tmpA1b2C3").exists());
}

#[test]
fn create_lays_out_meta_and_open_requires_it() {
    let (_dir, root) = new_root();
    assert_format(open_checked(&root), "open before create");
    let store = create_checked(&root).unwrap();
    assert_fresh_meta(&root);
    assert_format(create_checked(&root), "a second create");

    let reopened = open_checked(&root).unwrap();
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
    // Deleted, then created again: the version starts over. `main` is never deleted, so a
    // release ref is.
    cas(&store, "release/r", None, Some(1));
    cas(&store, "release/r", Some(1), Some(3));
    cas(&store, "release/r", Some(3), None);
    cas(&store, "release/r", None, Some(2));
    assert_eq!(
        store.read().unwrap().get_ref("release/r").unwrap(),
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
    let impatient = open_checked(&root)
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
    open_checked(&root)
        .unwrap()
        .write()
        .unwrap()
        .commit()
        .unwrap();
}

/// A committed snapshot never returns a frame its writer cuts (design-L107, storage spec §3
/// "읽기"). The writer's `journal.log` append is held between its write and its fdatasync
/// ([`swarmops_testkit::fs::HoldThenFail`]), and then fails with `EIO`, so the writer cuts the
/// frame back off the journal, still holding the lock (storage spec §2 rule 3 (a)). While it is
/// held, a lock-free snapshot on a second thread overlays the frame: the local readers'
/// semantics, pinned. A committed snapshot taken on a third thread waits for the lock until the
/// writer lets it go, and then holds no trace of the frame; `journal.log` is back to its length
/// before.
#[test]
fn read_committed_never_returns_a_frame_its_writer_cuts() {
    use swarmops_store::TailRepair;
    use swarmops_testkit::fs::HoldThenFail;

    let (_dir, root, store) = new_store();
    insert(&store, &[cs(0, &[])]);
    cas(&store, "main", None, Some(0));
    let before = log_len(&root);
    let (fs, held) = HoldThenFail::new(Layout::new(&root).journal_log(), RealFs::new());
    let writer = MetaStore::open_with_fs(&root, checked(fs)).unwrap();
    let root = &root;
    thread::scope(|scope| {
        let writing = scope.spawn(|| {
            let mut write = writer.write()?;
            write.insert_changeset(cs(1, &[0]))?;
            write.cas_ref("main", Some(id(0)), Some(id(1)), ACTOR)?;
            write.commit()
        });
        held.wait();

        // The frame is whole in journal.log, not synced: a lock-free snapshot overlays it.
        let local = scope.spawn(|| {
            let store = open_checked(root).unwrap();
            let read = store.read().unwrap();
            (
                read.changeset(id(1)).unwrap(),
                read.get_ref("main").unwrap(),
            )
        });
        let seen = local.join().unwrap();
        assert_eq!(seen, (Some(stored(1, &[0], 1, 2)), Some(ref_state(1, 2))));

        // A committed snapshot waits for the lock, which the held writer keeps.
        let (started, starting) = mpsc::channel();
        let committed = scope.spawn(move || {
            let store = open_checked(root)
                .unwrap()
                .with_lock_timeout(Duration::from_secs(60));
            started.send(()).unwrap();
            let (read, repair) = store.read_committed().unwrap();
            let got = (
                read.changeset(id(1)).unwrap(),
                read.get_ref("main").unwrap(),
                read.count_changesets().unwrap(),
            );
            (got, repair)
        });
        starting.recv_timeout(Duration::from_secs(60)).unwrap();
        thread::sleep(Duration::from_millis(300));
        assert!(
            !committed.is_finished(),
            "a committed snapshot returned while the writer held the lock"
        );

        // Let go, the writer's sync fails, and it cuts the frame before it unlocks.
        held.release();
        match writing.join().unwrap() {
            Err(StoreError::NotCommitted { source }) => {
                assert_eq!(source.raw_os_error(), Some(5), "{source}");
            }
            other => panic!("expected NotCommitted, got {other:?}"),
        }
        let (got, repair) = committed.join().unwrap();
        assert_eq!(
            got,
            (None, Some(ref_state(0, 1)), 1),
            "no trace of the frame"
        );
        assert_eq!(
            repair,
            TailRepair::default(),
            "the writer cut its frame itself"
        );
    });
    assert_eq!(
        log_len(root),
        before,
        "journal.log is back to its length before"
    );
}

/// Set only in the child of `read_committed_lists_and_returns_a_frame_committed_but_not_listed`
/// that crashes between its frame's sync and its listing: the store root.
#[cfg(feature = "test-hooks")]
const COMMITTED_CRASH_ROOT: &str = "SWARMOPS_TEST_META_COMMITTED_CRASH_ROOT";

/// The transaction [`read_committed_lists_and_returns_a_frame_committed_but_not_listed`] commits
/// on a store holding changeset 0: changeset 1 on it, and main made to name changeset 1.
#[cfg(feature = "test-hooks")]
fn commit_one_on_zero(store: &MetaStore) -> Result<CommitInfo, StoreError> {
    let mut write = store.write()?;
    write.insert_changeset(cs(1, &[0]))?;
    write.cas_ref("main", None, Some(id(1)), ACTOR)?;
    write.commit()
}

/// How many [`FsOps`] calls a handle opened on the store at `root` makes, from its open through
/// [`commit_one_on_zero`]'s listing of its frame in `journal.off`, that call included: counted on
/// a copy of the store, through a [`RecordingFs`], under the order checker as the real runs are.
#[cfg(feature = "test-hooks")]
fn calls_to_the_listing(root: &Utf8Path) -> u64 {
    let (_dir, dry) = new_root();
    copy_dir(root.as_std_path(), dry.as_std_path());
    let (fs, calls) = RecordingFs::new();
    let store = MetaStore::open_with_fs(&dry, checked(fs)).unwrap();
    commit_one_on_zero(&store).unwrap();
    let calls = calls();
    let listing = (calls.iter())
        .position(|call| call == "append_durable journal.off")
        .unwrap_or_else(|| panic!("no listing among {calls:?}"));
    u64::try_from(listing + 1).unwrap()
}

/// A committed snapshot of `store`, the store at `root`, whose frame 2 (changeset 1, main moved
/// to it) is whole and synced and that `journal.off` does not list: it returns the frame's
/// changeset and move, `journal.off` lists the frame afterwards, and the repair says so.
#[cfg(feature = "test-hooks")]
#[track_caller]
fn assert_read_committed_lists_it(store: &MetaStore, root: &Utf8Path, what: &str) {
    use swarmops_store::TailRepair;

    let (read, repair) = store.read_committed().unwrap();
    assert_eq!(
        read.changeset(id(1)).unwrap(),
        Some(stored(1, &[0], 1, 2)),
        "{what}"
    );
    assert_eq!(
        read.get_ref("main").unwrap(),
        Some(ref_state(1, 1)),
        "{what}"
    );
    drop(read);
    assert_eq!(listed(root), 2, "{what}: journal.off lists the frame");
    let mut listed_one = TailRepair::default();
    listed_one.frames_listed = 1;
    assert_eq!(repair, listed_one, "{what}");
}

/// A committed snapshot lists, and returns, a frame committed and never listed: one whose
/// `journal.off` append failed (its commit acknowledged with a warning, `PostCommit`), and one a
/// crash left between its sync and its listing. Its tail check syncs the frame and lists it, as a
/// writer's does (storage spec §2 rule 3), and the `TailRepair` it returns says so.
#[test]
#[cfg(feature = "test-hooks")]
fn read_committed_lists_and_returns_a_frame_committed_but_not_listed() {
    use swarmops_store::FsMethod;

    if let Ok(root) = std::env::var(COMMITTED_CRASH_ROOT) {
        let k = std::env::var(CRASH_AT_ENV).unwrap().parse().unwrap();
        let fs = checked(CrashAtFs::new(RealFs::new(), k));
        let store = MetaStore::open_with_fs(Utf8Path::new(&root), fs).unwrap();
        // Call k is the listing: the process aborts there.
        let _ = commit_one_on_zero(&store);
        return;
    }

    // The listing fails once, after its write (EIO): the commit point is passed, and the writer
    // cuts the entry back off journal.off.
    let (_dir, root, store) = new_store();
    insert(&store, &[cs(0, &[])]);
    let fail = FailAt::new(
        calls_to_the_listing(&root),
        FailMode::Once,
        InjectedError::Eio,
    );
    let failing = Arc::new(FailAtFs::new(RealFs::new(), fail));
    let writer = MetaStore::open_with_fs(&root, checked(Arc::clone(&failing))).unwrap();
    let committed = commit_one_on_zero(&writer);
    assert!(
        matches!(committed, Err(StoreError::PostCommit { frame_seq: 1, .. })),
        "{committed:?}"
    );
    let listing = Layout::new(&root).journal_off().into_std_path_buf();
    assert_eq!(
        failing.failed_calls(),
        [(FsMethod::AppendDurable, listing)],
        "the failure struck the listing"
    );
    assert_eq!(
        (listed(&root), frames(&root).len()),
        (1, 2),
        "the frame is whole in journal.log, unlisted"
    );
    assert_read_committed_lists_it(&writer, &root, "a failed listing");

    // A crash at the listing: the child aborts right before it.
    let (_dir, root, store) = new_store();
    insert(&store, &[cs(0, &[])]);
    drop(store);
    let k = calls_to_the_listing(&root);
    let child = run(this_test(&test_name!(
        "read_committed_lists_and_returns_a_frame_committed_but_not_listed"
    ))
    .env(COMMITTED_CRASH_ROOT, &root)
    .env(CRASH_AT_ENV, k.to_string()));
    assert_eq!(
        child.status.signal(),
        Some(SIGABRT),
        "{:?}\n{}",
        child.status,
        String::from_utf8_lossy(&child.stderr)
    );
    assert_eq!(
        (listed(&root), frames(&root).len()),
        (1, 2),
        "the frame is whole in journal.log, unlisted"
    );
    let store = open_checked(&root).unwrap();
    assert_read_committed_lists_it(&store, &root, "a crash at the listing");
}

/// A committed snapshot waits for the lock as a writer does, up to the store's `lock_timeout`,
/// and then gives up with [`StoreError::Busy`] (the retryable `E_BUSY`); once the lock is free,
/// it takes it.
#[test]
fn read_committed_times_out_busy_behind_a_held_lock() {
    let (_dir, root, store) = new_store();
    insert(&store, &[cs(0, &[])]);
    let held = store.write().unwrap();
    let impatient = open_checked(&root)
        .unwrap()
        .with_lock_timeout(Duration::from_millis(200));
    let started = Instant::now();
    match impatient.read_committed() {
        Err(StoreError::Busy) => {}
        Err(e) => panic!("expected Busy, got {e:?}"),
        Ok((read, repair)) => panic!(
            "expected Busy, got a snapshot of {} changesets ({repair:?})",
            read.count_changesets().unwrap()
        ),
    }
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_millis(200),
        "gave up after {waited:?}"
    );
    drop(held);
    // A fresh handle with the default timeout, as in `write_times_out_as_busy`.
    let patient = open_checked(&root).unwrap();
    let (read, _) = patient.read_committed().unwrap();
    assert_eq!(read.count_changesets().unwrap(), 1);
}

/// A committed snapshot of a journal whose tail is clean writes nothing (design-L107 §9): of the
/// [`FsOps`] calls it makes, none mutates or flushes ([`swarmops_store::FsMethod::is_mutating`],
/// `flushes`).
#[test]
fn read_committed_on_a_clean_tail_writes_nothing() {
    use swarmops_store::{FsMethod, TailRepair};

    let (_dir, root) = new_root();
    let (fs, calls) = RecordingFs::new();
    let store = MetaStore::create_with_fs(&root, checked(fs)).unwrap();
    insert(&store, &[cs(0, &[])]);
    insert(&store, &[cs(1, &[0])]);
    cas(&store, "main", None, Some(1));
    calls();
    let (read, repair) = store.read_committed().unwrap();
    assert_eq!(repair, TailRepair::default());
    assert_eq!(read.count_changesets().unwrap(), 2);
    assert_eq!(read.get_ref("main").unwrap(), Some(ref_state(1, 1)));
    drop(read);
    let made = calls();
    let method = |call: &String| {
        let name = call.split(' ').next();
        (FsMethod::ALL.into_iter())
            .find(|method| Some(method.as_str()) == name)
            .unwrap_or_else(|| panic!("no method makes {call:?}"))
    };
    let writes: Vec<&String> = (made.iter())
        .filter(|call| method(call).is_mutating() || method(call).flushes())
        .collect();
    assert!(writes.is_empty(), "it wrote {writes:?}, of {made:?}");
}

/// A committed snapshot is taken under the lock, and not only its tail check: every [`FsOps`]
/// call it makes, the snapshot's read of main's ref file among them, meets `meta/lock` held, so
/// that no writer lands or cuts a frame while the snapshot reads (one round, as `read` takes it
/// with no writer running).
#[test]
fn read_committed_takes_its_snapshot_under_the_lock() {
    /// Records each call by name, with whether `meta/lock` was held when it was made: whether a
    /// lock of its own, taken without waiting, gives up.
    struct LockProbe {
        layout: Layout,
        calls: Mutex<Vec<(String, bool)>>,
    }

    impl Watch for LockProbe {
        fn before(&self, call: &Call<'_>) -> io::Result<()> {
            let attempt = MetaLock::acquire(&self.layout, Duration::ZERO);
            let held = matches!(attempt, Err(StoreError::Busy));
            drop(attempt);
            self.calls.lock().unwrap().push((call.named(), held));
            Ok(())
        }
    }

    let (_dir, root, store) = new_store();
    insert(&store, &[cs(0, &[])]);
    cas(&store, "main", None, Some(0));
    let probe = LockProbe {
        layout: Layout::new(&root),
        calls: Mutex::new(Vec::new()),
    };
    let probe = Arc::new(Watched(probe, RealFs::new()));
    let reader = MetaStore::open_with_fs(&root, checked(Arc::clone(&probe))).unwrap();
    probe.calls.lock().unwrap().clear();
    let (read, _) = reader.read_committed().unwrap();
    assert_eq!(read.get_ref("main").unwrap(), Some(ref_state(0, 1)));
    drop(read);
    let calls = std::mem::take(&mut *probe.calls.lock().unwrap());
    assert!(
        calls.iter().any(|(call, _)| call == "read main"),
        "the snapshot reads main's ref file: {calls:?}"
    );
    let unlocked: Vec<&(String, bool)> = calls.iter().filter(|(_, held)| !held).collect();
    assert!(unlocked.is_empty(), "made without the lock: {unlocked:?}");
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
    let fs = fs.with_hook((
        "read main".to_owned(),
        When::Before,
        Box::new(move || {
            let writer = open_checked(&writer_root).unwrap();
            let mut write = writer.write().unwrap();
            write.insert_changeset(cs(2, &[1])).unwrap();
            write
                .cas_ref("main", Some(id(1)), Some(id(2)), ACTOR)
                .unwrap();
            write.commit().unwrap();
        }),
    ));
    let reader = MetaStore::open_with_fs(&root, checked(fs)).unwrap();

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

/// A frame a lock-free reader holds can be cut back off the journal (a failed sync, storage
/// spec §2 rule 3 (a)) and the next writer's frame can land at its seq and offset: the count of
/// the frames past the checkpoint holds still, but the frames do not. The snapshot must read the
/// ref files and the frames again, or it lays the cut frame's records over ref files that
/// already show the replacement: a ref at a target that never committed, at a version that
/// never existed.
#[test]
fn a_snapshot_reads_again_when_a_frame_it_read_was_cut_and_replaced() {
    let (_dir, reader, calls) = snapshot_over_a_cut_and_replaced_frame(&[
        changeset_record(&cs(2, &[1]), 2),
        ref_move("main", Some(1), Some(2), ACTOR, 0),
    ]);
    let read = reader.read().unwrap();
    assert_eq!(read.get_ref("main").unwrap(), Some(ref_state(3, 2)));
    assert!(
        read.changeset(id(3)).unwrap().is_some(),
        "main's target is in the snapshot"
    );
    assert_eq!(
        read.changeset(id(2)).unwrap(),
        None,
        "the cut frame's changeset is not"
    );
    assert_eq!(read.count_changesets().unwrap(), 2);
    assert_eq!(
        calls().iter().filter(|call| *call == "read main").count(),
        2,
        "the frames changed, so the ref files were read again"
    );
}

/// The same with a cut frame that moved no ref: the snapshot would show the replacement's move
/// (the ref file has it) without the replacement's changeset, a dangling ref.
#[test]
fn a_snapshot_reads_again_when_a_frame_it_read_was_cut_and_replaced_with_a_move() {
    let (_dir, reader, _calls) =
        snapshot_over_a_cut_and_replaced_frame(&[changeset_record(&cs(2, &[1]), 2)]);
    let read = reader.read().unwrap();
    assert_eq!(read.get_ref("main").unwrap(), Some(ref_state(3, 2)));
    assert!(
        read.changeset(id(3)).unwrap().is_some(),
        "main's target is in the snapshot"
    );
    assert_eq!(read.changeset(id(2)).unwrap(), None);
    assert_eq!(read.count_changesets().unwrap(), 2);
}

/// A reader of a store holding changeset 1 and `main` at it, whose snapshot reads frame F
/// (`cut`, appended and then unlisted: the state on disk between a writer's append of the frame
/// and its sync) among the frames past the checkpoint. Right before the reader's first read of
/// main's file, that sync fails and F is cut back (storage spec §2 rule 3 (a)), and the next
/// writer commits changeset 3 on 1 and moves `main` 1 -> 3: a frame at F's seq and offset. The
/// function returned gives the reader's calls.
fn snapshot_over_a_cut_and_replaced_frame(
    cut: &[Record],
) -> (TempDir, MetaStore, impl Fn() -> Vec<String> + use<>) {
    let (dir, root, store) = new_store();
    insert(&store, &[cs(1, &[])]);
    cas(&store, "main", None, Some(1));
    drop(store);
    let layout = Layout::new(&root);
    let cut_at = append_unmaterialized(&root, cut).offset;
    set_len(layout.journal_off(), file_len(layout.journal_off()) - 8);
    let writer_root = root.clone();
    let log = layout.journal_log();
    let (fs, calls) = RecordingFs::new();
    let fs = fs.with_hook((
        "read main".to_owned(),
        When::Before,
        Box::new(move || {
            set_len(&log, cut_at);
            let writer = open_checked(&writer_root).unwrap();
            let mut write = writer.write().unwrap();
            write.insert_changeset(cs(3, &[1])).unwrap();
            write
                .cas_ref("main", Some(id(1)), Some(id(3)), ACTOR)
                .unwrap();
            write.commit().unwrap();
        }),
    ));
    let reader = MetaStore::open_with_fs(&root, checked(fs)).unwrap();
    (dir, reader, calls)
}

/// How long the threads of a test wait for one another before the test fails: far longer than
/// any hand-off takes, so that only a hang gets there.
const PATIENCE: Duration = Duration::from_secs(60);

/// Before each read of `main`'s file, while `left` is not 0, commits one more transaction through
/// `writer`, a store of its own on the same root: changeset `next` on `next - 1`, with `main`
/// moved from `next - 1` to it, `next` counting up.
struct CommitBeforeReadingMain {
    writer: MetaStore,
    left: AtomicU32,
    next: AtomicU64,
}

impl Watch for CommitBeforeReadingMain {
    fn before(&self, call: &Call<'_>) -> io::Result<()> {
        let commits = call.named() == "read main"
            && self
                .left
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                    left.checked_sub(1)
                })
                .is_ok();
        if commits {
            let n = self.next.fetch_add(1, Ordering::SeqCst);
            let mut write = self.writer.write().unwrap();
            write.insert_changeset(cs(n, &[n - 1])).unwrap();
            write
                .cas_ref("main", Some(id(n - 1)), Some(id(n)), ACTOR)
                .unwrap();
            write.commit().unwrap();
        }
        Ok(())
    }
}

/// A store holding changeset 1 with `main` at it (version 1), and a reader of it that, before
/// each of its reads of `main`'s file, `commits` times, commits changeset n on n - 1 and moves
/// `main` to it, n counting up from 2 ([`CommitBeforeReadingMain`]).
fn reader_racing_commits(commits: u32) -> (TempDir, Utf8PathBuf, MetaStore) {
    let (dir, root, store) = new_store();
    insert(&store, &[cs(1, &[])]); // frame 0
    cas(&store, "main", None, Some(1)); // frame 1
    drop(store);
    let racing = CommitBeforeReadingMain {
        writer: open_checked(&root).unwrap(),
        left: AtomicU32::new(commits),
        next: AtomicU64::new(2),
    };
    let reader = MetaStore::open_with_fs(&root, checked(Watched(racing, RealFs::new()))).unwrap();
    (dir, root, reader)
}

/// A snapshot reads the ref files and the frames past the checkpoint again when a commit lands
/// in between, but not without end: when one lands during every one of its
/// [`MetaStore::SNAPSHOT_ROUNDS`] rounds, 32 as the specs and the documents say, it gives up
/// with [`StoreError::SnapshotContended`] (the API's retryable `E_BUSY`) and leaves nothing
/// behind. (It used to read again for as long as commits kept landing.) The 30th commit here
/// moves the checkpoint while the reader holds the one it read first.
#[test]
fn a_snapshot_gives_up_as_contended_when_every_round_sees_a_new_frame() {
    let rounds = MetaStore::SNAPSHOT_ROUNDS;
    assert_eq!(
        rounds, 32,
        "the bound storage spec §3 and operations spec §4 state"
    );
    let (_dir, root, reader) = reader_racing_commits(rounds);
    match reader.read().map(|read| read.count_changesets()) {
        Err(StoreError::SnapshotContended { rounds: made }) => assert_eq!(made, rounds),
        other => panic!("expected SnapshotContended after {rounds} rounds, got {other:?}"),
    }
    assert!(
        ckpt(&root) > 0,
        "a commit moved the checkpoint under the reader"
    );
    // One commit landed in every round, and the read that gave up left nothing behind: the next
    // one, with no more commits landing, shows them all.
    let last = 1 + u64::from(rounds);
    let read = reader.read().unwrap();
    assert_eq!(read.get_ref("main").unwrap(), Some(ref_state(last, last)));
    assert_eq!(read.count_changesets().unwrap(), last);
}

/// The bound's guard: when the last of the rounds sees no new frame, the snapshot shows every
/// commit that landed during the others.
#[test]
fn a_snapshot_whose_last_round_sees_no_new_frame_shows_every_commit() {
    let rounds = MetaStore::SNAPSHOT_ROUNDS;
    let (_dir, _root, reader) = reader_racing_commits(rounds - 1);
    let read = reader.read().unwrap();
    let last = u64::from(rounds);
    assert_eq!(read.get_ref("main").unwrap(), Some(ref_state(last, last)));
    for n in 1..=last {
        let parents: Vec<u64> = (n - 1..n).filter(|&parent| parent > 0).collect();
        let got = read.changeset(id(n)).unwrap();
        assert_eq!(got, Some(stored(n, &parents, n - 1, n)), "changeset {n}");
    }
    assert_eq!(read.count_changesets().unwrap(), last);
}

/// When armed, holds the next `append_durable journal.off` once it has written and synced its
/// entry, says so on `listed`, waits until the reader says on `measured` that it has measured
/// `journal.off`, then fails the call as a sync that fails after its write: the writer cuts the
/// entry back off (storage spec §2 rule 3 (b)).
struct ListingFailsOnceMeasured {
    armed: AtomicBool,
    listed: mpsc::Sender<()>,
    measured: Mutex<mpsc::Receiver<()>>,
}

impl Watch for ListingFailsOnceMeasured {
    fn before(&self, _call: &Call<'_>) -> io::Result<()> {
        Ok(())
    }

    fn after_success(&self, call: &Call<'_>) -> io::Result<()> {
        if call.named() != "append_durable journal.off" || !self.armed.swap(false, Ordering::SeqCst)
        {
            return Ok(());
        }
        self.listed.send(()).unwrap();
        let measured = self.measured.lock().unwrap().recv_timeout(PATIENCE);
        measured.expect("the reader measures journal.off");
        Err(io::Error::from_raw_os_error(5)) // EIO
    }
}

/// A reader that measures `journal.off` while it lists a writer's frame, and reads its entries
/// once the writer has cut that entry back (its sync failed: storage spec §2 rule 3 (b)), finds
/// fewer entries than it measured. That is no damage: a writer cuts only entries that were never
/// genuine. The snapshot takes one more round and shows the writer's frame, whole in
/// `journal.log` and committed, as a frame not listed yet. (It used to report the journal
/// corrupt: the API's `E_STORE_CORRUPT`, which is not retryable.)
#[test]
fn a_snapshot_racing_a_journal_off_cut_reads_again_and_reports_no_damage() {
    let (_dir, root, store) = new_store();
    insert(&store, &[cs(1, &[])]); // frame 0
    cas(&store, "main", None, Some(1)); // frame 1
    drop(store);
    let (listed_tx, listed_rx) = mpsc::channel();
    let (measured_tx, measured_rx) = mpsc::channel();
    let (cut_tx, cut_rx) = mpsc::channel::<()>();
    let listing_fails = ListingFailsOnceMeasured {
        armed: AtomicBool::new(true),
        listed: listed_tx,
        measured: Mutex::new(measured_rx),
    };
    let writer =
        MetaStore::open_with_fs(&root, checked(Watched(listing_fails, RealFs::new()))).unwrap();
    // Right after the reader first measures journal.off, the writer's listing fails, and the
    // reader reads on once the writer has cut the entry back.
    let (fs, calls) = RecordingFs::new();
    let fs = fs.with_hook((
        "file_len journal.off".to_owned(),
        When::After,
        Box::new(move || {
            measured_tx.send(()).unwrap();
            let cut = cut_rx.recv_timeout(PATIENCE);
            cut.expect("the writer cuts its entry back");
        }),
    ));
    let reader = MetaStore::open_with_fs(&root, checked(fs)).unwrap();

    thread::scope(|scope| {
        let commit = scope.spawn(|| {
            let mut write = writer.write().unwrap();
            write.insert_changeset(cs(2, &[1])).unwrap();
            write
                .cas_ref("main", Some(id(1)), Some(id(2)), ACTOR)
                .unwrap();
            let committed = write.commit();
            cut_tx.send(()).unwrap();
            committed
        });
        let listed = listed_rx.recv_timeout(PATIENCE);
        listed.expect("the writer lists its frame");
        let read = match reader.read() {
            Ok(read) => read,
            Err(e) => panic!("expected a snapshot, got {e:?}"),
        };
        match commit.join().unwrap() {
            Err(StoreError::PostCommit { frame_seq: 2, .. }) => {}
            other => panic!("expected PostCommit at frame 2, got {other:?}"),
        }
        assert_eq!(read.get_ref("main").unwrap(), Some(ref_state(2, 2)));
        assert_eq!(read.changeset(id(2)).unwrap(), Some(stored(2, &[1], 1, 2)));
        assert_eq!(read.count_changesets().unwrap(), 2);
    });
    // The shrunk read took the first round, before journal.log was measured; the second opened
    // the index again.
    let round = ["file_len journal.off", "file_len journal.log"];
    assert_eq!(
        calls(),
        [
            &["file_len journal.off"][..],
            &round,
            &["read main"],
            &round
        ]
        .concat(),
        "one more round"
    );
    assert_eq!(
        (listed(&root), frames(&root).len()),
        (2, 3),
        "the frame is whole in journal.log, unlisted"
    );
}

/// Cuts the last entry off `journal.off` right after its `n`th measurement, once: a writer's cut
/// of an entry whose own listing failed (storage spec §2 rule 3 (b)), landing between a reader's
/// measurement and its read of the entries. Records every call, by name ([`Call::named`]).
struct CutAfterMeasurement {
    n: u32,
    seen: AtomicU32,
    calls: Arc<Mutex<Vec<String>>>,
}

impl Watch for CutAfterMeasurement {
    fn before(&self, call: &Call<'_>) -> io::Result<()> {
        self.calls.lock().unwrap().push(call.named());
        Ok(())
    }

    fn after(&self, call: &Call<'_>) {
        if call.named() == "file_len journal.off"
            && self.seen.fetch_add(1, Ordering::SeqCst) + 1 == self.n
        {
            set_len(call.path(), file_len(call.path()) - 8);
        }
    }
}

/// A cut that a round's second read of the frames meets, the one after the ref files are read,
/// takes that round as a cut its first read meets does: the snapshot reads the ref files and the
/// frames again, and shows the store as the cut leaves it, the frame whose entry was cut whole
/// in `journal.log` and read as one not listed yet. (Before Task 3 of M1.6, it reported the
/// journal corrupt.)
#[test]
fn a_snapshot_whose_second_read_of_the_frames_meets_a_cut_takes_another_round() {
    let (_dir, root, store) = new_store();
    insert(&store, &[cs(1, &[])]); // frame 0
    cas(&store, "main", None, Some(1)); // frame 1
    drop(store);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let cutting = CutAfterMeasurement {
        n: 2,
        seen: AtomicU32::new(0),
        calls: Arc::clone(&calls),
    };
    let reader = MetaStore::open_with_fs(&root, checked(Watched(cutting, RealFs::new()))).unwrap();
    let read = match reader.read() {
        Ok(read) => read,
        Err(e) => panic!("expected a snapshot, got {e:?}"),
    };
    assert_eq!(read.get_ref("main").unwrap(), Some(ref_state(1, 1)));
    assert_eq!(read.changeset(id(1)).unwrap(), Some(stored(1, &[], 0, 1)));
    assert_eq!(read.count_changesets().unwrap(), 1);
    // The first round: the frames read as the index opens, `main`, then the second read, which
    // meets the cut before it measures journal.log. The next round reads `main` and the frames.
    let (off, log) = ("file_len journal.off", "file_len journal.log");
    assert_eq!(
        *calls.lock().unwrap(),
        [off, log, "read main", off, "read main", off, log],
        "the second read met the cut, and one more round"
    );
    assert_eq!(
        (listed(&root), frames(&root).len()),
        (1, 2),
        "frame 1 is whole in journal.log, unlisted"
    );
}

/// How many changesets [`snapshots_stay_consistent_across_a_compaction`] seeds its store with,
/// compacted at once, so that the sorted views a snapshot holds are not empty.
const SEEDS: u64 = 1100;

/// The number of seed changeset `i`.
fn seed(i: u64) -> u64 {
    1_000_000 + i
}

/// The lookups of [`snapshots_stay_consistent_across_a_compaction`] on snapshot `read`, taken
/// with main at version `n`: main names changeset `n`, the snapshot holds exactly the seeds and
/// changesets 1..=`n`, and changesets 1, `n / 2` and `n` (in the tail) and every 64th seed (in
/// the sorted views) are found by id, by a 16-digit prefix and by delta hash. Returns `n`.
#[track_caller]
fn assert_whole_snapshot(read: &MetaRead<'_>) -> u64 {
    let main = read.get_ref("main").unwrap().expect("main exists");
    let n = main.version;
    assert_eq!(main.target, id(n), "{main:?}");
    assert_eq!(
        read.count_changesets().unwrap(),
        SEEDS + n,
        "one changeset per move"
    );
    let moved = [1, n.div_ceil(2), n].map(|k| (k, SEEDS + k - 1));
    let seeds = (0..SEEDS).step_by(64).map(|i| (seed(i), i));
    for (k, seq) in moved.into_iter().chain(seeds) {
        let found = read.changeset(id(k)).unwrap();
        assert_eq!(found, Some(stored(k, &[], seq, 1)), "{k} at {n}");
        let prefix = &id(k).to_string()[..16];
        assert_eq!(read.resolve_prefix(prefix).unwrap(), [id(k)], "{k} at {n}");
        assert_eq!(
            read.by_delta_hash(&delta(k)).unwrap(),
            [id(k)],
            "{k} at {n}"
        );
    }
    assert_eq!(
        read.changeset(id(n + 1)).unwrap(),
        None,
        "nothing after {n}"
    );
    n
}

/// Readers take snapshots throughout 1,100 commits (each inserts changeset n and moves main to
/// it) on a store seeded with 1,100 changesets compacted into the sorted views; the 1,025th
/// commit takes the tail past the threshold, and compacts it and the views into new views.
/// Every snapshot is whole ([`assert_whole_snapshot`]); some may land in the middle of the
/// compaction, though nothing here makes sure one does. And each reader holds the snapshot it
/// took right before that compaction (the writer waits until every reader holds one), and once
/// the compaction is done runs the same lookups on it, with the same answers: a snapshot keeps
/// its own handles on the view files the compaction renames new ones over.
#[test]
fn snapshots_stay_consistent_across_a_compaction() {
    const MOVES: u64 = 1100;
    // The commit whose changeset makes the tail one longer than the threshold.
    const COMPACTS: u64 = Index::COMPACT_THRESHOLD + 1;
    let (_dir, root) = new_root();
    let store = MetaStore::create_with_fs(&root, checked(NoSyncFs)).unwrap();
    let seeds: Vec<ChangesetMeta> = (0..SEEDS).map(|i| cs(seed(i), &[])).collect();
    assert!(insert(&store, &seeds).checkpointed, "the seeds compact");
    assert_eq!(index(&root).sorted_count(), SEEDS);
    let done = AtomicBool::new(false);
    let compacted = AtomicBool::new(false);
    // Whether each reader holds a snapshot from right before the compaction.
    let holding: [AtomicBool; 4] = Default::default();
    thread::scope(|scope| {
        let readers: Vec<_> = holding
            .iter()
            .map(|holds| {
                let (root, done, compacted) = (&root, &done, &compacted);
                scope.spawn(move || {
                    // A handle of its own, as a reader in another process has.
                    let store = open_checked(root).unwrap();
                    let mut before = None;
                    let mut read_again = false;
                    let mut last = 0;
                    loop {
                        let finished = done.load(Ordering::Acquire);
                        let read = store.read().unwrap();
                        if read.get_ref("main").unwrap().is_some() {
                            last = assert_whole_snapshot(&read);
                            if last == COMPACTS - 1 && before.is_none() {
                                before = Some(read);
                                holds.store(true, Ordering::Release);
                            }
                        }
                        if let Some(before) = &before
                            && !read_again
                            && compacted.load(Ordering::Acquire)
                        {
                            assert_eq!(assert_whole_snapshot(before), COMPACTS - 1);
                            read_again = true;
                        }
                        if finished {
                            return (read_again, last);
                        }
                    }
                })
            })
            .collect();
        {
            // Stops the readers however the writes end.
            let _stop = SetOnDrop(&done);
            for n in 1..=MOVES {
                if n == COMPACTS {
                    let deadline = Instant::now() + Duration::from_secs(60);
                    while holding.iter().any(|holds| !holds.load(Ordering::Acquire)) {
                        assert!(Instant::now() < deadline, "the readers fell behind");
                        thread::sleep(Duration::from_millis(1));
                    }
                }
                let expected = (n > 1).then(|| id(n - 1));
                let mut write = store.write().unwrap();
                write.insert_changeset(cs(n, &[])).unwrap();
                write.cas_ref("main", expected, Some(id(n)), ACTOR).unwrap();
                let info = write.commit().unwrap();
                if n == COMPACTS {
                    assert!(info.checkpointed, "the compacting commit checkpoints");
                    compacted.store(true, Ordering::Release);
                }
            }
        }
        for reader in readers {
            let (read_again, last) = reader.join().unwrap();
            assert!(
                read_again,
                "the snapshot from before the compaction, read after it"
            );
            assert_eq!(last, MOVES, "the last snapshot holds every commit");
        }
    });
    // The seeds' write is frame 0 and its Compacted frame 1; commit n is frame n + 1, and the
    // compaction's Compacted frame follows commit 1,025's.
    assert_eq!(compacted_frames(&store), [1, COMPACTS + 2]);
    assert!(index(&root).sorted_count() >= SEEDS + COMPACTS);
}

/// A ref deleted in a frame past the checkpoint that no writer materialized (a writer died
/// right after its commit point): its file is still there, but snapshots have it gone, and the
/// next write removes the file.
#[test]
fn a_snapshot_drops_a_ref_whose_delete_is_not_materialized() {
    let (_dir, root, store) = new_store();
    insert(&store, &[cs(1, &[])]);
    cas(&store, "main", None, Some(1));
    cas(&store, "release/v1", None, Some(1));
    let frame = append_unmaterialized(
        &root,
        &[ref_move("release/v1", Some(1), None, "crashed/1", 7)],
    );
    assert_eq!(ref_file(&root, "release/v1"), Some(ref_content(1, 1)));

    let read = store.read().unwrap();
    assert_eq!(read.get_ref("release/v1").unwrap(), None);
    let main = ("main".to_owned(), ref_state(1, 1));
    assert_eq!(read.list_refs().unwrap(), std::slice::from_ref(&main));
    let deleted = RefMoveEntry::new(frame.seq, Some(id(1)), None, "crashed/1".into(), 7);
    assert_eq!(read.reflog("release/v1", 1).unwrap(), [deleted]);
    drop(read);
    // The files alone, as verify reads them, still show it.
    let files: Vec<String> = store
        .ref_files()
        .unwrap()
        .into_iter()
        .map(|file| file.name)
        .collect();
    assert_eq!(files, ["main", "release/v1"]);

    store.write().unwrap().commit().unwrap();
    assert_eq!(ref_file(&root, "release/v1"), None);
    let read = store.read().unwrap();
    assert_eq!(read.list_refs().unwrap(), [main]);
    drop(read);
    // Created again, it starts over at version 1.
    cas(&store, "release/v1", None, Some(1));
    assert_eq!(
        store.read().unwrap().get_ref("release/v1").unwrap(),
        Some(ref_state(1, 1))
    );
}

/// The moves of ref `name` that `read`'s reflog lists, newest first, at most `limit`: (frame
/// seq, old, new).
fn reflog_moves(read: &MetaRead<'_>, name: &str, limit: usize) -> Vec<Move> {
    read.reflog(name, limit)
        .unwrap()
        .into_iter()
        .map(|entry| (entry.seq, entry.old, entry.new))
        .collect()
}

/// A ref move as [`reflog_moves`] lists it.
type Move = (u64, Option<ChangesetId>, Option<ChangesetId>);

/// `reflog` reads the frames before the checkpoint 256 at a time, newest first: over 600
/// frames, it finds every move of a ref, in order, across the batches and the frames past the
/// checkpoint; stops at its limit in the middle of a batch; and walks back to the first frame
/// for a ref moved only there.
#[test]
fn reflog_walks_back_past_256_frames() {
    let (_dir, root) = new_root();
    let store = MetaStore::create_with_fs(&root, checked(NoSyncFs)).unwrap();
    insert(&store, &[cs(1, &[]), cs(2, &[])]); // frame 0
    cas(&store, "release/old", None, Some(1)); // frame 1, its only move
    // Frames 2..=601: every third moves main between changesets 1 and 2, the others
    // release/busy.
    let mut main_moves = Vec::new();
    let (mut main, mut busy) = (None, None);
    for seq in 2..=601 {
        let (name, at) = if seq % 3 == 0 {
            ("main", &mut main)
        } else {
            ("release/busy", &mut busy)
        };
        let new = if *at == Some(1) { 2 } else { 1 };
        cas(&store, name, *at, Some(new));
        if name == "main" {
            main_moves.push((seq, at.map(id), Some(id(new))));
        }
        *at = Some(new);
    }
    main_moves.reverse();
    let checkpoint = ckpt(&root);
    let past = frames(&root)
        .iter()
        .filter(|frame| frame.offset >= checkpoint)
        .count();
    assert!((1..32).contains(&past), "{past} frames past the checkpoint");

    let read = store.read().unwrap();
    assert_eq!(reflog_moves(&read, "main", usize::MAX), main_moves);
    assert_eq!(reflog_moves(&read, "main", 150), main_moves[..150]);
    let old = [(1, None, Some(id(1)))];
    assert_eq!(reflog_moves(&read, "release/old", 10), old);
    assert_eq!(read.reflog("release/busy", usize::MAX).unwrap().len(), 400);
    drop(read);
    // With no frame past the checkpoint, the walk starts at the end of the journal.
    store.flush().unwrap();
    let read = store.read().unwrap();
    assert_eq!(reflog_moves(&read, "main", usize::MAX), main_moves);
    assert_eq!(reflog_moves(&read, "release/old", 10), old);
}

/// Set only in the children of `crash_at_any_replay_call_recovers_to_the_journal`: the store
/// root.
#[cfg(feature = "test-hooks")]
const REPLAY_CRASH_ROOT: &str = "SWARMOPS_TEST_META_REPLAY_CRASH_ROOT";

/// Lays out at `root` a store whose journal holds three frames past the checkpoint that no
/// writer materialized, as a power loss can leave the index short of several frames' records:
/// changesets 2..=4 on changeset 1, main moved from 1 to 4, release/new created and
/// release/old deleted. Everything before them is materialized and checkpointed.
#[cfg(feature = "test-hooks")]
fn unreplayed_store(root: &Utf8Path) {
    let store = MetaStore::create_with_fs(root, checked(NoSyncFs)).unwrap();
    insert(&store, &[cs(1, &[])]);
    cas(&store, "main", None, Some(1));
    cas(&store, "release/old", None, Some(1));
    store.flush().unwrap();
    let frames = [
        vec![
            changeset_record(&cs(2, &[1]), 2),
            changeset_record(&cs(3, &[2]), 3),
        ],
        vec![
            changeset_record(&cs(4, &[3]), 4),
            ref_move("main", Some(1), Some(4), "crashed/1", 7),
        ],
        vec![
            ref_move("release/new", None, Some(2), "crashed/1", 8),
            ref_move("release/old", Some(1), None, "crashed/1", 9),
        ],
    ];
    for records in &frames {
        append_unmaterialized(root, records);
    }
}

/// Asserts that `read` shows what [`unreplayed_store`]'s journal holds.
#[cfg(feature = "test-hooks")]
#[track_caller]
fn assert_replayed(read: &MetaRead<'_>, what: &str) {
    assert_eq!(read.count_changesets().unwrap(), 4, "{what}");
    for n in 1..=4 {
        let parents: Vec<u64> = (n - 1..n).filter(|&parent| parent > 0).collect();
        let want = stored(n, &parents, n - 1, n);
        assert_eq!(read.changeset(id(n)).unwrap(), Some(want), "{what}: {n}");
    }
    let refs = [
        ("main".to_owned(), ref_state(4, 2)),
        ("release/new".to_owned(), ref_state(2, 1)),
    ];
    assert_eq!(read.list_refs().unwrap(), refs, "{what}");
}

/// A crash at any FsOps call of a write's replay (the index appends of three frames, the ref
/// files they move) leaves a store whose snapshots show the journal's state, and the next
/// write finishes the replay: every record once, in journal order, and each ref file as its
/// last move leaves it.
#[test]
#[cfg(feature = "test-hooks")]
fn crash_at_any_replay_call_recovers_to_the_journal() {
    if let Ok(root) = std::env::var(REPLAY_CRASH_ROOT) {
        let k = std::env::var(CRASH_AT_ENV).unwrap().parse().unwrap();
        let fs = checked(CrashAtFs::new(RealFs::new(), k));
        let mut store = MetaStore::open_with_fs(Utf8Path::new(&root), fs).unwrap();
        // One file at a time, in write order, as the dry run: call k is the same call.
        store.set_parallelism(1);
        store.write().unwrap().commit().unwrap();
        return;
    }
    let (_template_dir, template) = new_root();
    unreplayed_store(&template);

    // A dry run counts the calls: the tail check and the snapshot of the journal, then the
    // replay's.
    let (_dry_dir, dry) = new_root();
    copy_dir(template.as_std_path(), dry.as_std_path());
    let (fs, calls) = RecordingFs::new();
    let mut store = MetaStore::open_with_fs(&dry, checked(fs)).unwrap();
    store.set_parallelism(1);
    store.write().unwrap().commit().unwrap();
    let dry_calls = calls();
    for call in ["append ids.col", "atomic_replace main", "remove old"] {
        assert!(dry_calls.iter().any(|c| c == call), "{call}: {dry_calls:?}");
    }
    let total = u64::try_from(dry_calls.len()).unwrap();

    for k in 1..=total + 1 {
        let (_dir, root) = new_root();
        copy_dir(template.as_std_path(), root.as_std_path());
        let child = run(this_test(&test_name!(
            "crash_at_any_replay_call_recovers_to_the_journal"
        ))
        .env(REPLAY_CRASH_ROOT, &root)
        .env(CRASH_AT_ENV, k.to_string()));
        let ended_as_expected = if k <= total {
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

        let store = open_checked(&root).unwrap();
        assert_replayed(&store.read().unwrap(), &format!("k={k}, after the crash"));
        store.write().unwrap().commit().unwrap();
        assert_replayed(&store.read().unwrap(), &format!("k={k}, replayed"));
        let index = index(&root);
        assert_eq!(index.len(), 4, "k={k}: each record once");
        for n in 1..=4 {
            assert_eq!(index.find(id(n)).unwrap(), Some(n - 1), "k={k}: {n}");
        }
        assert_eq!(ref_file(&root, "main"), Some(ref_content(4, 2)), "k={k}");
        let new = ref_file(&root, "release/new");
        assert_eq!(new, Some(ref_content(2, 1)), "k={k}");
        assert_eq!(ref_file(&root, "release/old"), None, "k={k}");
    }
}

#[test]
fn write_and_commit_make_the_documented_fs_calls() {
    let (_dir, root) = new_root();
    let (fs, calls) = RecordingFs::new();
    let mut store = MetaStore::create_with_fs(&root, checked(fs)).unwrap();
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
    let (fs, calls) = RecordingFs::new();
    let store = MetaStore::create_with_fs(root, checked(fs)).unwrap();
    for n in 0..31 {
        insert(&store, &[cs(n, &[])]);
    }
    calls();
    assert!(insert(&store, &[cs(31, &[])]).checkpointed);
    // No thread could take a share of the sync: each file was synced here, in write order,
    // all before the checkpoint moved.
    let made = calls();
    let synced_then_checkpointed = [
        "sync_data parents.log",
        "sync_data par.col",
        "sync_data gen.col",
        "sync_data dh.col",
        "sync_data ids.col",
        "atomic_replace journal.ckpt",
    ];
    assert_eq!(
        made[made.len().saturating_sub(6)..],
        synced_then_checkpointed
    );
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
    // RLIMIT_NPROC at 1 (`ulimit -u` in bash, `ulimit -p` in dash): this user's processes and
    // threads are past the limit, so the child can spawn no thread (the test harness then runs
    // the test on its main thread).
    let child = run(Command::new("sh")
        .arg("-c")
        .arg("ulimit -u 1 2>/dev/null || ulimit -p 1; exec \"$0\" \"$@\"")
        .arg(std::env::current_exe().unwrap())
        .args(test_args(&test_name!(TEST)))
        .env(NO_THREADS_ROOT, &root));
    let report = report(0, &child);
    if report == "threads can be spawned" {
        eprintln!("the thread limit does not bind here (as root, say): the fallback went untested");
    } else {
        assert_eq!(report, "checkpointed");
    }
}

/// Set only in the child of `a_temp_file_write_that_fails_keeps_the_os_error`: the directory it
/// writes its temp files in.
#[cfg(target_os = "linux")]
const FSIZE_DIR: &str = "SWARMOPS_TEST_META_FSIZE_DIR";

/// What the child of [`a_temp_file_write_that_fails_keeps_the_os_error`] reports of an object's
/// temp file and of a replacement that `RealFs` could not write past one block: each one's errno,
/// and whether its text names `dir`.
#[cfg(target_os = "linux")]
fn temp_write_failures(dir: &Utf8Path) {
    let bytes = vec![0; 64 * 1024];
    let reported = [
        RealFs::new()
            .write_durable_temp(dir.as_std_path(), &bytes)
            .map(drop),
        RealFs::new().atomic_replace(dir.join("f").as_std_path(), &bytes),
    ]
    .map(|result| match result {
        Ok(()) => "written".to_owned(),
        Err(e) => format!(
            "{:?} {}",
            e.raw_os_error(),
            e.to_string().contains(dir.as_str())
        ),
    });
    println!("{REPORT}{}", reported.join(", "));
}

/// A temp file that `RealFs` cannot write, for an object or a replacement, fails with the
/// operating system's own error, its errno kept and no path in its text: the store writes
/// through the file itself, not through tempfile's `NamedTempFile`, whose writes wrap a failure
/// with the temp file's absolute path and drop the errno. The child's files are limited to one
/// block (`ulimit -f 1`, `RLIMIT_FSIZE`, which binds root too) and it ignores `SIGXFSZ`, so a
/// write past the block fails with `EFBIG` (27).
#[cfg(target_os = "linux")]
#[test]
fn a_temp_file_write_that_fails_keeps_the_os_error() {
    const TEST: &str = "a_temp_file_write_that_fails_keeps_the_os_error";
    if let Some(dir) = std::env::var_os(FSIZE_DIR) {
        temp_write_failures(&Utf8PathBuf::from_path_buf(dir.into()).unwrap());
        return;
    }
    let (_dir, root) = new_root();
    let child = run(Command::new("sh")
        .arg("-c")
        .arg("trap '' XFSZ; ulimit -f 1; exec \"$0\" \"$@\"")
        .arg(std::env::current_exe().unwrap())
        .args(test_args(&test_name!(TEST)))
        .env(FSIZE_DIR, &root));
    assert_eq!(report(0, &child), "Some(27) false, Some(27) false");
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
    let store = MetaStore::create_with_fs(&root, checked(fs)).unwrap();
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

/// An open reads `<root>/FORMAT` for the store's format: none is FORMAT 1, as a metadata store
/// made on its own has always been, and so is FORMAT 1's marker; FORMAT 2's opens the FORMAT 2
/// journal (missing in this FORMAT 1 store); any other marker is no format.
#[test]
fn meta_open_reads_the_format_file() {
    let (_dir, root, store) = new_store();
    drop(store);
    let format_file = Layout::new(root.clone()).format_file();

    // 1. No FORMAT, and FORMAT 1's marker: FORMAT 1.
    assert_eq!(open_checked(&root).unwrap().format(), StoreFormat::V1);
    fs::write(&format_file, StoreFormat::V1.marker()).unwrap();
    assert_eq!(open_checked(&root).unwrap().format(), StoreFormat::V1);

    // 2. FORMAT 2's marker: the FORMAT 2 journal, which this FORMAT 1 store does not have.
    fs::write(&format_file, StoreFormat::V2.marker()).unwrap();
    let Err(StoreError::Format(what)) = open_checked(&root) else {
        panic!("FORMAT 2 opened");
    };
    assert!(what.contains("journal2.log"), "{what}");

    // 3. Any other marker: no format this build reads.
    fs::write(&format_file, b"swarmops-store 9\n").unwrap();
    let Err(StoreError::Format(what)) = open_checked(&root) else {
        panic!("an unknown format opened");
    };
    assert!(what.contains("does not read"), "{what}");

    // 4. `open_in` takes the caller's layout, whatever FORMAT says.
    let layout = Layout::with_format(root.clone(), StoreFormat::V1);
    let opened = MetaStore::open_in(layout, checked(RealFs::new())).unwrap();
    assert_eq!(opened.format(), StoreFormat::V1);
}
