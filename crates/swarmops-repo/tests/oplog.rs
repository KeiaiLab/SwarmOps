//! The operation log and `undo` (storage spec §3): the journal's frames read back as the store's
//! operations, newest first, and an operation's ref moves restored by compare-and-swap in one
//! transaction, all of them or none, after a crash at any filesystem call too.
//!
//! Every test owns a fresh store (`tempfile::tempdir()`), so tests are independent and may run
//! in any order or in parallel. The crash sweep runs this test binary again as its children
//! (selected by [`CHILD_ROOT`]), on the real filesystem.

use std::collections::HashSet;
#[cfg(feature = "test-hooks")]
use std::os::unix::process::ExitStatusExt;
#[cfg(feature = "test-hooks")]
use std::process::Output;
use std::sync::Arc;
#[cfg(feature = "test-hooks")]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_core::{ChangesetId, ObjectKind, hash_object};
use swarmops_repo::{
    Advance, CasResult, Depth, LocalRepo, OpId, OpNotFoundReason, OpRecord, Operation, RepoError,
    UndoOutcome,
};
#[cfg(feature = "test-hooks")]
use swarmops_store::{CRASH_AT_ENV, ORDER_CHECK_ENV};
use swarmops_store::{
    ChangesetMeta, Journal, MetaStore, RealFs, Recovered, StoreError, StoreFormat,
};
#[cfg(feature = "test-hooks")]
use swarmops_testkit::child::{SIGABRT, run, this_test};
use swarmops_testkit::faults::checked;
#[cfg(feature = "test-hooks")]
use swarmops_testkit::fs::copy_dir;
use swarmops_testkit::fs::{FailingFs, HoldThenFail};
use swarmops_testkit::repo::{commit, new_repo, new_repo_as, put, request};
use swarmops_testkit::store::layout_of;
#[cfg(feature = "test-hooks")]
use swarmops_testkit::test_name;

/// Set only in the children the crash sweep spawns: the store root.
#[cfg(feature = "test-hooks")]
const CHILD_ROOT: &str = "SWARMOPS_TEST_REPO_OPLOG_CHILD_ROOT";

/// Set with [`CHILD_ROOT`]: the id of the operation the child undoes, read by the parent so the
/// child's filesystem calls are the undo's alone.
#[cfg(feature = "test-hooks")]
const CHILD_ID: &str = "SWARMOPS_TEST_REPO_OPLOG_CHILD_ID";

/// Who moves the refs the tests set up.
const ACTOR: &str = "test/agent";

/// Who undoes.
const UNDOER: &str = "test/undoer";

/// Three changesets in a first-parent chain, each committed in an operation of its own (ops 0,
/// 1 and 2), no ref moved.
fn chain(repo: &LocalRepo) -> [ChangesetId; 3] {
    let c1 = commit(repo, &[], vec![put("f", b"1")]);
    let c2 = commit(repo, &[c1], vec![put("f", b"2")]);
    let c3 = commit(repo, &[c2], vec![put("f", b"3")]);
    [c1, c2, c3]
}

/// Moves ref `name` from `expected` to `new` (`None`: creates it, deletes it) as [`ACTOR`], in
/// an operation of its own.
#[track_caller]
fn cas(repo: &LocalRepo, name: &str, expected: Option<ChangesetId>, new: Option<ChangesetId>) {
    let moved = repo.cas_ref(name, expected, new, ACTOR).unwrap();
    assert_eq!(moved.result, CasResult::Applied { old: expected });
    assert_eq!(moved.warnings, Vec::<String>::new());
}

/// What ref `name` names; `None` if it does not exist.
fn target(repo: &LocalRepo, name: &str) -> Option<ChangesetId> {
    repo.get_ref(name).unwrap().map(|state| state.target)
}

/// How many frames the journal of the store at `root` lists.
fn frame_count(root: &Utf8Path) -> u64 {
    Journal::open(&layout_of(root), Arc::new(RealFs::new()))
        .unwrap()
        .frame_count()
        .unwrap()
}

/// The moves `undone` reports, without their times, which [`undo`] checks.
fn moves_of(undone: &UndoOutcome) -> Vec<(String, Option<ChangesetId>, Option<ChangesetId>)> {
    undone
        .moves
        .iter()
        .map(|(name, old, new, _)| (name.clone(), *old, *new))
        .collect()
}

/// Now, in Unix seconds.
fn unix_now() -> i64 {
    let since = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    i64::try_from(since.as_secs()).unwrap()
}

/// The id of operation `op` as the operation log lists it.
#[track_caller]
fn id_of(repo: &LocalRepo, op: u64) -> OpId {
    let (ops, _) = repo.op_log(op.checked_add(1), 1).unwrap();
    assert_eq!(seqs(&ops), [op], "operation {op} is listed");
    OpId::new(op, ops[0].crc)
}

/// The id of the frame numbered `op` in the store at `root`, read without the store's lock: a
/// frame not listed, or not synced yet, too.
#[track_caller]
fn id_at(root: &Utf8Path, op: u64) -> OpId {
    let meta = MetaStore::open_with_fs(root, checked(RealFs::new())).unwrap();
    let frame = meta.read().unwrap().frame(op).unwrap();
    OpId::new(op, frame.expect("the frame is in journal.log").crc)
}

/// Undoes operation `op` as [`UNDOER`], by the id the operation log lists: what the undo did,
/// each move it reports timed now and recorded in its frame as reported, by the undoer, and the
/// frame's crc reported.
#[track_caller]
fn undo(repo: &LocalRepo, op: u64) -> UndoOutcome {
    let id = id_of(repo, op);
    let before = unix_now();
    let undone = repo.undo(id, UNDOER).unwrap();
    let after = unix_now();
    assert_eq!(undone.undone, op);
    assert_eq!(undone.warnings, Vec::<String>::new());
    assert_eq!(undone.already_applied, undone.frame_seq.is_none());
    assert_eq!(undone.frame_seq.is_none(), undone.frame_crc.is_none());
    for (_, _, _, ts) in &undone.moves {
        assert!(
            (before..=after).contains(ts),
            "{ts} outside {before}..={after}"
        );
    }
    if let Some(seq) = undone.frame_seq {
        let (ops, _) = repo.op_log(Some(seq + 1), 1).unwrap();
        let recorded: Vec<OpRecord> = undone
            .moves
            .iter()
            .map(|(name, old, new, ts)| OpRecord::RefMove {
                name: name.clone(),
                old: *old,
                new: *new,
                actor: UNDOER.to_owned(),
                ts: *ts,
            })
            .collect();
        assert_eq!(ops[0].seq, seq);
        assert_eq!(Some(ops[0].crc), undone.frame_crc, "the frame's crc");
        assert_eq!(ops[0].records, recorded, "one frame, of the moves reported");
    } else {
        assert_eq!(undone.moves, Vec::new(), "nothing written, nothing moved");
    }
    undone
}

/// `op`'s records, each ref move's time checked to lie in `before..=after` and then set to 0.
fn untimed(op: &Operation, before: i64, after: i64) -> Vec<OpRecord> {
    op.records
        .iter()
        .map(|record| match record {
            OpRecord::RefMove {
                name,
                old,
                new,
                actor,
                ts,
            } => {
                assert!((before..=after).contains(ts), "{record:?}");
                OpRecord::RefMove {
                    name: name.clone(),
                    old: *old,
                    new: *new,
                    actor: actor.clone(),
                    ts: 0,
                }
            }
            other => other.clone(),
        })
        .collect()
}

/// A ref move of [`ACTOR`]'s at time 0, as [`untimed`] gives it.
fn moved(name: &str, old: Option<ChangesetId>, new: Option<ChangesetId>) -> OpRecord {
    OpRecord::RefMove {
        name: name.to_owned(),
        old,
        new,
        actor: ACTOR.to_owned(),
        ts: 0,
    }
}

/// The seqs of `ops`, in their order.
fn seqs(ops: &[Operation]) -> Vec<u64> {
    ops.iter().map(|op| op.seq).collect()
}

/// A commit on `parent` with `metadata` after the agent's, that moves `main` from `parent` to
/// itself in the same operation: the records of a publish, and of a land.
fn publish(repo: &LocalRepo, parent: ChangesetId, metadata: &[(&str, &[u8])]) -> ChangesetId {
    let mut all = vec![("agent".to_owned(), b"test/agent".to_vec())];
    all.extend(
        metadata
            .iter()
            .map(|(key, value)| ((*key).to_owned(), value.to_vec())),
    );
    let mut req = request(
        &[parent],
        vec![put("p", &[u8::try_from(metadata.len()).unwrap()])],
        "p",
    );
    req.metadata = all;
    req.advance = Some(Advance::new("main", ACTOR));
    repo.commit(req).unwrap().id
}

/// Moves `main` and `release/r` from `from` to `to` in one frame, written through a write
/// transaction of the metadata store's own (no operation of the repository's moves two refs
/// at once); the frame's seq.
fn two_ref_move(root: &Utf8Path, from: ChangesetId, to: ChangesetId) -> u64 {
    let meta = MetaStore::open_with_fs(root, checked(RealFs::new())).unwrap();
    let mut write = meta.write().unwrap();
    for name in ["main", "release/r"] {
        let moved = write.cas_ref(name, Some(from), Some(to), ACTOR).unwrap();
        assert_eq!(moved, CasResult::Applied { old: Some(from) }, "{name}");
    }
    write.commit().unwrap().frame_seq
}

/// The repository at `root`, its listings in `journal.off` failing: a write through it commits,
/// its frame whole and durable in `journal.log` but not listed, with a warning.
fn listing_fails(root: &Utf8Path) -> LocalRepo {
    let fs = FailingFs::new(|method, p| method == "append_durable" && p.ends_with("journal.off"));
    LocalRepo::open_with_fs(root, Arc::new(fs)).unwrap()
}

/// The operation log lists every transaction, newest first, each with the records its frame
/// holds: a commit's changeset, a ref's creation, a commit that advances a ref (and the land of
/// a later milestone, the same records), 1,025 changesets in one frame, and the compaction of
/// the index their commit set off, a frame of its own. A page holds at most `limit` operations
/// below `before`, and says whether older ones follow.
#[test]
fn op_log_lists_each_transaction_newest_first() {
    let (_dir, root, repo) = new_repo();
    let before = unix_now();
    let c1 = commit(&repo, &[], vec![put("a", b"1")]); // op 0
    cas(&repo, "main", None, Some(c1)); // op 1
    let c2 = publish(&repo, c1, &[]); // op 2
    let landed = publish(
        &repo,
        c2,
        &[("land_key", b"k-1"), ("lander", b"test/lander")],
    ); // op 3
    // Op 4 puts more changesets in the index's tail than it compacts at, so its commit compacts
    // the index, and appends op 5, the Compacted marker.
    let roots: Vec<ChangesetId> = (0..1025_u64)
        .map(|n| ChangesetId::new(hash_object(ObjectKind::Changeset, &n.to_le_bytes())))
        .collect();
    let meta = MetaStore::open_with_fs(&root, checked(RealFs::new())).unwrap();
    let mut write = meta.write().unwrap();
    for &id in &roots {
        let root_changeset = ChangesetMeta::new(id, 0, 0, [0; 32], Vec::new());
        write.insert_changeset(root_changeset).unwrap();
    }
    assert!(write.commit().unwrap().checkpointed, "it compacted");
    let after = unix_now();

    let (ops, older) = repo.op_log(None, 10).unwrap();
    assert!(!older, "op 0 is on the page");
    assert_eq!(seqs(&ops), [5, 4, 3, 2, 1, 0]);
    let records: Vec<Vec<OpRecord>> = ops.iter().map(|op| untimed(op, before, after)).collect();
    let inserted = |id: ChangesetId, parents: &[ChangesetId]| OpRecord::Changeset {
        id,
        parents: parents.to_vec(),
    };
    assert_eq!(records[0], [OpRecord::Compacted]);
    let many: Vec<OpRecord> = roots.iter().map(|&id| inserted(id, &[])).collect();
    assert_eq!(records[1], many);
    let land = [
        inserted(landed, &[c2]),
        moved("main", Some(c2), Some(landed)),
    ];
    assert_eq!(records[2], land);
    let advance = [inserted(c2, &[c1]), moved("main", Some(c1), Some(c2))];
    assert_eq!(records[3], advance);
    assert_eq!(records[4], [moved("main", None, Some(c1))]);
    assert_eq!(records[5], [inserted(c1, &[])]);

    // A page at a time, below `before`.
    let page = |before, limit| {
        let (ops, older) = repo.op_log(before, limit).unwrap();
        (seqs(&ops), older)
    };
    assert_eq!(page(None, 1), (vec![5], true));
    assert_eq!(page(Some(3), 2), (vec![2, 1], true));
    assert_eq!(page(Some(1), 5), (vec![0], false));
    assert_eq!(page(Some(0), 5), (vec![], false));
    assert_eq!(page(Some(u64::MAX), 6), (vec![5, 4, 3, 2, 1, 0], false));
}

/// `op log` never lists a frame its writer cuts (storage spec §2 rule 3 (a)): it waits for the
/// lock the writer holds between its frame's write and its failed sync, and then lists the
/// operations committed before. So the number the next write takes, and `op log` lists, names
/// that write only, and its `undo` undoes exactly its moves.
#[test]
fn op_log_never_lists_a_frame_its_writer_cuts() {
    let (_dir, root, repo) = new_repo();
    let c1 = commit(&repo, &[], vec![put("f", b"1")]); // op 0
    cas(&repo, "main", None, Some(c1)); // op 1
    let c2 = commit(&repo, &[c1], vec![put("f", b"2")]); // op 2
    let c3 = commit(&repo, &[c1], vec![put("f", b"3")]); // op 3
    let (fs, held) = HoldThenFail::new(layout_of(&root).journal_log(), RealFs::new());
    let writer = LocalRepo::open_with_fs(&root, checked(fs)).unwrap();
    let root = &root;
    thread::scope(|scope| {
        // 1. The writer's frame (op 4, main to c2) is whole in journal.log, not synced.
        let writing = scope.spawn(|| writer.cas_ref("main", Some(c1), Some(c2), ACTOR));
        held.wait();

        // 2. `op log` waits for the lock the held writer keeps.
        let (started, starting) = mpsc::channel();
        let listing = scope.spawn(move || {
            let reader = LocalRepo::open(root).unwrap();
            started.send(()).unwrap();
            reader.op_log(None, 10)
        });
        starting.recv_timeout(Duration::from_secs(60)).unwrap();
        thread::sleep(Duration::from_millis(300));
        assert!(
            !listing.is_finished(),
            "op log returned while the writer held the lock"
        );

        // 3. Let go: the sync fails, the writer cuts its frame, and op log lists op 3 newest.
        held.release();
        match writing.join().unwrap() {
            Err(RepoError::Store(StoreError::NotCommitted { source })) => {
                assert_eq!(source.raw_os_error(), Some(5), "{source}");
            }
            other => panic!("expected NotCommitted, got {other:?}"),
        }
        let (ops, older) = listing.join().unwrap().unwrap();
        assert!(!older);
        assert_eq!(seqs(&ops), [3, 2, 1, 0], "no trace of the cut frame");
    });

    // 4. The next write takes number 4; op log lists it with that write's move, and its undo
    // undoes exactly that move.
    let before = unix_now();
    cas(&repo, "main", Some(c1), Some(c3));
    let after = unix_now();
    let (ops, _) = repo.op_log(None, 1).unwrap();
    assert_eq!(seqs(&ops), [4]);
    assert_eq!(
        untimed(&ops[0], before, after),
        [moved("main", Some(c1), Some(c3))]
    );
    let undone = undo(&repo, 4);
    assert_eq!(moves_of(&undone), [("main".to_owned(), Some(c3), Some(c1))]);
    assert_eq!(target(&repo, "main"), Some(c1));
}

/// `undo` reads the frame it undoes under the write's lock, after the tail check (storage spec
/// §3): an undo started while a writer holds its frame, whole and not synced, waits, and once
/// the failed sync cuts the frame back finds no such operation. Reading the frame before the
/// lock would see the cut frame, and act on it (here: a replay of a move never made).
#[test]
fn undo_never_acts_on_a_frame_its_writer_cuts() {
    let (_dir, root, repo) = new_repo();
    let c1 = commit(&repo, &[], vec![put("f", b"1")]); // op 0
    cas(&repo, "main", None, Some(c1)); // op 1
    let c2 = commit(&repo, &[c1], vec![put("f", b"2")]); // op 2
    let (fs, held) = HoldThenFail::new(layout_of(&root).journal_log(), RealFs::new());
    let writer = LocalRepo::open_with_fs(&root, checked(fs)).unwrap();
    let root = &root;
    thread::scope(|scope| {
        // 1. The writer's frame (op 3, main to c2) is whole in journal.log, not synced.
        let writing = scope.spawn(|| writer.cas_ref("main", Some(c1), Some(c2), ACTOR));
        held.wait();

        // 2. The undo of op 3, by the id a lock-free read shows, waits for the lock the held
        // writer keeps.
        let id = id_at(root, 3);
        let (started, starting) = mpsc::channel();
        let undoing = scope.spawn(move || {
            let undoer = LocalRepo::open(root).unwrap();
            started.send(()).unwrap();
            undoer.undo(id, UNDOER)
        });
        starting.recv_timeout(Duration::from_secs(60)).unwrap();
        thread::sleep(Duration::from_millis(300));
        assert!(
            !undoing.is_finished(),
            "undo returned while the writer held the lock"
        );

        // 3. Let go: the sync fails, the writer cuts its frame, and op 3 is not found.
        held.release();
        match writing.join().unwrap() {
            Err(RepoError::Store(StoreError::NotCommitted { source })) => {
                assert_eq!(source.raw_os_error(), Some(5), "{source}");
            }
            other => panic!("expected NotCommitted, got {other:?}"),
        }
        match undoing.join().unwrap() {
            Err(RepoError::OpNotFound {
                op: 3,
                last,
                reason,
                ..
            }) => assert_eq!((last, reason), (Some(2), OpNotFoundReason::PastEnd)),
            other => panic!("expected OpNotFound, got {other:?}"),
        }
    });
    assert_eq!(target(&repo, "main"), Some(c1));
    assert_eq!(frame_count(root), 3, "the undo wrote nothing");
}

/// Writer A's move of `release/r` from `from` to `cut` is whole in journal.log and then cut
/// back by its failed sync; returns the id a lock-free read gave its frame, numbered `op`, before
/// the cut.
fn taken_back(root: &Utf8Path, op: u64, from: ChangesetId, cut: ChangesetId) -> OpId {
    let (fs, held) = HoldThenFail::new(layout_of(root).journal_log(), RealFs::new());
    let writer = LocalRepo::open_with_fs(root, checked(fs)).unwrap();
    thread::scope(|scope| {
        // 1. A's frame is whole in journal.log, not synced: a read without the lock sees it.
        let writing = scope.spawn(|| writer.cas_ref("release/r", Some(from), Some(cut), ACTOR));
        held.wait();
        let id = id_at(root, op);

        // 2. Let go: the sync fails, and A cuts its frame.
        held.release();
        match writing.join().unwrap() {
            Err(RepoError::Store(StoreError::NotCommitted { source })) => {
                assert_eq!(source.raw_os_error(), Some(5), "{source}");
            }
            other => panic!("expected NotCommitted, got {other:?}"),
        }
        id
    })
}

/// An undo by the id of a frame its writer cut never acts on the operation that took the
/// frame's number (the M15 design §2): the frame at that number has another crc, so the undo is
/// not found and writes nothing, and the later operation's move stays.
#[test]
fn undo_of_a_taken_back_id_is_not_found() {
    let (_dir, root, repo) = new_repo();
    let [c1, c2, c3] = chain(&repo);
    cas(&repo, "release/r", None, Some(c1)); // op 3
    let cut = taken_back(&root, 4, c1, c2);
    cas(&repo, "release/r", Some(c1), Some(c3)); // op 4, writer B's
    let frames = frame_count(&root);
    let before = journal_bytes(&root);

    match repo.undo(cut, UNDOER) {
        Err(RepoError::OpNotFound {
            op: 4,
            crc,
            last,
            reason,
            ..
        }) => assert_eq!(
            (crc, last, reason),
            (cut.crc, Some(4), OpNotFoundReason::Checksum)
        ),
        other => panic!("expected OpNotFound, got {other:?}"),
    }
    assert_eq!(target(&repo, "release/r"), Some(c3), "B's move stays");
    assert_eq!(frame_count(&root), frames, "no frame");
    assert!(journal_bytes(&root) == before, "nothing written");
    assert_ne!(id_of(&repo, 4), cut, "B's frame has an id of its own");
}

/// The bytes of the journal of the store at `root`: `journal.log` and `journal.off`.
fn journal_bytes(root: &Utf8Path) -> (Vec<u8>, Vec<u8>) {
    let layout = layout_of(root);
    let log = std::fs::read(layout.journal_log()).unwrap();
    let off = std::fs::read(layout.journal_off()).unwrap();
    (log, off)
}

/// The crc check comes before the undo's other decisions (the M15 design §2): an undo by a
/// taken-back id whose number now holds a move of a ref that has moved again since is not found
/// (`checksum`), never blocked on that ref, and leaves the journal's bytes as they were.
#[test]
fn checksum_comes_before_cas() {
    let (_dir, root, repo) = new_repo();
    let [c1, c2, c3] = chain(&repo);
    cas(&repo, "release/r", None, Some(c1)); // op 3
    let cut = taken_back(&root, 4, c1, c2);
    cas(&repo, "release/r", Some(c1), Some(c3)); // op 4, writer B's
    cas(&repo, "release/r", Some(c3), Some(c2)); // op 5: B's ref moved since
    let before = journal_bytes(&root);

    match repo.undo(cut, UNDOER) {
        Err(RepoError::OpNotFound {
            op: 4,
            crc,
            last,
            reason,
            ..
        }) => assert_eq!(
            (crc, last, reason),
            (cut.crc, Some(5), OpNotFoundReason::Checksum)
        ),
        other => panic!("expected OpNotFound, got {other:?}"),
    }
    assert_eq!(target(&repo, "release/r"), Some(c2), "op 5's move stays");
    assert!(journal_bytes(&root) == before, "nothing written");
}

/// How many take-backs [`take_backs_never_reuse_an_id`] makes.
const TAKE_BACKS: u64 = 1_000;

/// [`undo_of_a_taken_back_id_is_not_found`] [`TAKE_BACKS`] times on one store: each round's
/// writer A moves `release/r` and cuts its frame, writer B takes its number with another move,
/// and the undo by A's id is not found (`checksum`) and leaves B's move. The op log then lists
/// every number once, and no id that was taken back.
#[test]
#[ignore = "slow, ~25 s: 1,000 take-backs"]
fn take_backs_never_reuse_an_id() {
    let (_dir, root, repo) = new_repo();
    let chain = chain(&repo);
    cas(&repo, "release/r", None, Some(chain[0])); // op 3
    let mut at = 0;
    let mut cut_ids = Vec::new();
    for round in 0..TAKE_BACKS {
        // 1. A moves release/r to the next changeset and cuts the frame; B moves it to the one
        // after, so their payloads differ.
        let op = round + 4;
        let (cut, kept) = ((at + 1) % 3, (at + 2) % 3);
        let id = taken_back(&root, op, chain[at], chain[cut]);
        cas(&repo, "release/r", Some(chain[at]), Some(chain[kept]));
        at = kept;

        // 2. The undo by A's id is not found, and B's move stays.
        match repo.undo(id, UNDOER) {
            Err(RepoError::OpNotFound {
                op: missing,
                reason: OpNotFoundReason::Checksum,
                ..
            }) => assert_eq!(missing, op, "round {round}"),
            other => panic!("round {round}: expected OpNotFound, got {other:?}"),
        }
        assert_eq!(
            target(&repo, "release/r"),
            Some(chain[kept]),
            "round {round}"
        );
        cut_ids.push(id);
    }

    // 3. Every number listed once, newest first; no taken-back id among them.
    let total = TAKE_BACKS + 4;
    let (ops, older) = repo.op_log(None, usize::try_from(total).unwrap()).unwrap();
    assert!(!older, "op 0 is on the page");
    let numbers: Vec<u64> = (0..total).rev().collect();
    assert!(seqs(&ops) == numbers, "every number once, none skipped");
    let ids: HashSet<OpId> = ops.iter().map(|op| OpId::new(op.seq, op.crc)).collect();
    let reused: Vec<&OpId> = cut_ids.iter().filter(|id| ids.contains(id)).collect();
    assert_eq!(reused, Vec::<&OpId>::new(), "no taken-back id listed");
}

/// `undo` of a move restores the ref to what it named before the move, in one new frame of one
/// ref move by the undoer; the changesets stay.
#[test]
fn undo_restores_a_moved_ref_in_one_frame() {
    let (_dir, root, repo) = new_repo();
    let [c1, c2, _] = chain(&repo);
    cas(&repo, "main", None, Some(c1)); // op 3
    cas(&repo, "main", Some(c1), Some(c2)); // op 4
    let changesets = repo.info().unwrap().changesets;

    let undone = undo(&repo, 4);
    assert_eq!(undone.frame_seq, Some(5));
    assert_eq!(moves_of(&undone), [("main".to_owned(), Some(c2), Some(c1))]);
    assert_eq!(undone.recovered, Recovered::default());
    assert_eq!(target(&repo, "main"), Some(c1));
    assert_eq!(frame_count(&root), 6, "one frame");
    assert_eq!(
        repo.info().unwrap().changesets,
        changesets,
        "the changesets stay"
    );
    assert_eq!(repo.verify(Depth::Deep).unwrap().problems, []);
}

/// `undo` of a ref's creation deletes the ref.
#[test]
fn undo_of_a_creation_deletes_the_ref() {
    let (_dir, _root, repo) = new_repo();
    let [c1, ..] = chain(&repo);
    cas(&repo, "release/r", None, Some(c1)); // op 3

    let undone = undo(&repo, 3);
    assert_eq!(
        moves_of(&undone),
        [("release/r".to_owned(), Some(c1), None)]
    );
    assert_eq!(target(&repo, "release/r"), None);
    assert_eq!(repo.list_refs().unwrap(), []);
}

/// `undo` of a ref's deletion makes the ref again, at the target it had.
#[test]
fn undo_of_a_deletion_recreates_the_ref() {
    let (_dir, _root, repo) = new_repo();
    let [c1, ..] = chain(&repo);
    cas(&repo, "release/r", None, Some(c1)); // op 3
    cas(&repo, "release/r", Some(c1), None); // op 4

    let undone = undo(&repo, 4);
    assert_eq!(
        moves_of(&undone),
        [("release/r".to_owned(), None, Some(c1))]
    );
    assert_eq!(target(&repo, "release/r"), Some(c1));
}

/// An undo run again finds every ref back where the operation found it: `already_applied`,
/// nothing written.
#[test]
fn undo_twice_is_a_replay() {
    let (_dir, root, repo) = new_repo();
    let [c1, c2, _] = chain(&repo);
    cas(&repo, "main", None, Some(c1)); // op 3
    cas(&repo, "main", Some(c1), Some(c2)); // op 4
    assert_eq!(undo(&repo, 4).frame_seq, Some(5));
    let frames = frame_count(&root);

    let again = undo(&repo, 4);
    assert!(again.already_applied);
    assert_eq!((again.frame_seq, again.moves.len()), (None, 0));
    assert_eq!(frame_count(&root), frames, "no frame");
    assert_eq!(target(&repo, "main"), Some(c1));
}

/// An undo of an operation whose ref has moved since is blocked: it names the ref, what the
/// operation set it to and what it names now, and writes nothing. Once the later move is undone,
/// it goes through.
#[test]
fn undo_after_a_later_move_is_blocked_and_writes_nothing() {
    let (_dir, root, repo) = new_repo();
    let [c1, c2, c3] = chain(&repo);
    cas(&repo, "main", None, Some(c1)); // op 3
    cas(&repo, "main", Some(c1), Some(c2)); // op 4
    cas(&repo, "main", Some(c2), Some(c3)); // op 5
    let frames = frame_count(&root);

    match repo.undo(id_of(&repo, 4), UNDOER) {
        Err(RepoError::UndoBlocked {
            op: 4,
            name,
            expected,
            actual,
            recovered,
        }) => {
            assert_eq!(name, "main");
            assert_eq!((expected, actual), (Some(c2), Some(c3)));
            assert_eq!(*recovered, Recovered::default());
        }
        other => panic!("expected UndoBlocked, got {other:?}"),
    }
    assert_eq!(frame_count(&root), frames, "nothing written");
    assert_eq!(target(&repo, "main"), Some(c3));

    assert_eq!(
        moves_of(&undo(&repo, 5)),
        [("main".to_owned(), Some(c3), Some(c2))]
    );
    assert_eq!(
        moves_of(&undo(&repo, 4)),
        [("main".to_owned(), Some(c2), Some(c1))]
    );
    assert_eq!(target(&repo, "main"), Some(c1));
}

/// An undo of a frame that moved two refs restores both in one frame, or neither: with one of
/// them moved since, it is blocked on that one and leaves the other where it is.
#[test]
fn undo_of_a_frame_with_two_refs_is_all_or_nothing() {
    let (_dir, root, repo) = new_repo();
    let [c1, c2, c3] = chain(&repo);
    cas(&repo, "main", None, Some(c1)); // op 3
    cas(&repo, "release/r", None, Some(c1)); // op 4
    let op = two_ref_move(&root, c1, c2);
    assert_eq!(op, 5);
    cas(&repo, "release/r", Some(c2), Some(c3)); // op 6
    let frames = frame_count(&root);

    match repo.undo(id_of(&repo, op), UNDOER) {
        Err(RepoError::UndoBlocked {
            name,
            expected,
            actual,
            ..
        }) => {
            assert_eq!(name, "release/r");
            assert_eq!((expected, actual), (Some(c2), Some(c3)));
        }
        other => panic!("expected UndoBlocked, got {other:?}"),
    }
    assert_eq!(frame_count(&root), frames, "nothing written");
    let both = || (target(&repo, "main"), target(&repo, "release/r"));
    assert_eq!(both(), (Some(c2), Some(c3)), "main untouched");

    undo(&repo, 6); // release/r back to c2: frame 7
    let undone = undo(&repo, op);
    assert_eq!(undone.frame_seq, Some(8));
    let restored = [
        ("main".to_owned(), Some(c2), Some(c1)),
        ("release/r".to_owned(), Some(c2), Some(c1)),
    ];
    assert_eq!(moves_of(&undone), restored, "both, in one frame");
    assert_eq!(both(), (Some(c1), Some(c1)));
    assert_eq!(repo.verify(Depth::Deep).unwrap().problems, []);
}

/// A frame that moved a ref twice is undone by one move, from its last target back to its
/// first old one.
#[test]
fn undo_of_a_frame_that_moved_a_ref_twice_restores_its_first_old() {
    let (_dir, root, repo) = new_repo();
    let [c1, c2, c3] = chain(&repo);
    cas(&repo, "main", None, Some(c1)); // op 3
    let meta = MetaStore::open_with_fs(&root, checked(RealFs::new())).unwrap();
    let mut write = meta.write().unwrap();
    write.cas_ref("main", Some(c1), Some(c2), ACTOR).unwrap();
    write.cas_ref("main", Some(c2), Some(c3), ACTOR).unwrap();
    assert_eq!(write.commit().unwrap().frame_seq, 4);

    let undone = undo(&repo, 4);
    assert_eq!(moves_of(&undone), [("main".to_owned(), Some(c3), Some(c1))]);
    assert_eq!(target(&repo, "main"), Some(c1));
}

/// An operation that left every ref where it found it (a commit, or moves of a ref that cancel
/// out) has nothing to undo, and the undo writes nothing.
#[test]
fn undo_of_an_op_that_moved_no_ref_is_nothing_to_undo() {
    let (_dir, root, repo) = new_repo();
    let [c1, ..] = chain(&repo);
    let meta = MetaStore::open_with_fs(&root, checked(RealFs::new())).unwrap();
    let mut write = meta.write().unwrap();
    write.cas_ref("release/r", None, Some(c1), ACTOR).unwrap();
    write.cas_ref("release/r", Some(c1), None, ACTOR).unwrap();
    assert_eq!(write.commit().unwrap().frame_seq, 3);
    let frames = frame_count(&root);

    for op in [0, 2, 3] {
        match repo.undo(id_of(&repo, op), UNDOER) {
            Err(RepoError::NothingToUndo { op: nothing, .. }) => assert_eq!(nothing, op),
            other => panic!("op {op}: expected NothingToUndo, got {other:?}"),
        }
    }
    assert_eq!(frame_count(&root), frames, "nothing written");
}

/// An undo of an operation the journal does not hold is not found, and says which is the last.
#[test]
fn undo_of_an_op_past_the_journal_is_not_found() {
    let (_dir, _root, repo) = new_repo();
    let not_found = |op| match repo.undo(OpId::new(op, 0), UNDOER) {
        Err(RepoError::OpNotFound {
            op: missing,
            last,
            reason,
            ..
        }) => {
            assert_eq!((missing, reason), (op, OpNotFoundReason::PastEnd));
            last
        }
        other => panic!("op {op}: expected OpNotFound, got {other:?}"),
    };
    assert_eq!(not_found(0), None, "an empty journal");
    chain(&repo);
    assert_eq!(not_found(3), Some(2));
    assert_eq!(not_found(u64::MAX), Some(2));

    // A mistyped crc: the frame at the number has another, which the error never gives.
    let listed = id_of(&repo, 2);
    let mistyped = OpId::new(2, listed.crc ^ 1);
    match repo.undo(mistyped, UNDOER) {
        Err(e @ RepoError::OpNotFound { op: 2, .. }) => {
            let RepoError::OpNotFound { last, reason, .. } = &e else {
                unreachable!("matched above");
            };
            assert_eq!((*last, *reason), (Some(2), OpNotFoundReason::Checksum));
            let message = e.to_string();
            assert!(message.contains(&mistyped.to_string()), "{message}");
            assert!(!message.contains(&listed.to_string()), "{message}");
        }
        other => panic!("expected OpNotFound, got {other:?}"),
    }
}

/// An undo is an operation like any other, and undoing it restores what it took back.
#[test]
fn undo_of_an_undo_restores_again() {
    let (_dir, _root, repo) = new_repo();
    let [c1, c2, _] = chain(&repo);
    cas(&repo, "main", None, Some(c1)); // op 3
    cas(&repo, "main", Some(c1), Some(c2)); // op 4
    assert_eq!(undo(&repo, 4).frame_seq, Some(5));

    let redone = undo(&repo, 5);
    assert_eq!(redone.frame_seq, Some(6));
    assert_eq!(moves_of(&redone), [("main".to_owned(), Some(c1), Some(c2))]);
    assert_eq!(target(&repo, "main"), Some(c2));
    assert!(
        undo(&repo, 5).already_applied,
        "op 5's refs are back where it found them"
    );
}

/// An undo whose frame is durable but whose step after the commit point fails (here its listing
/// in `journal.off`) is committed: it reports its moves with a warning, a read shows them, and
/// the next write lists the frame and says so.
#[test]
fn an_undo_that_fails_after_its_commit_point_warns() {
    let (_dir, root, repo) = new_repo_as(StoreFormat::V1);
    let [c1, c2, c3] = chain(&repo);
    cas(&repo, "main", None, Some(c1)); // op 3
    cas(&repo, "main", Some(c1), Some(c2)); // op 4

    let op4 = id_of(&repo, 4);
    let undone = listing_fails(&root).undo(op4, UNDOER).unwrap();
    assert_eq!(undone.frame_seq, Some(5));
    assert_eq!(moves_of(&undone), [("main".to_owned(), Some(c2), Some(c1))]);
    assert_eq!(undone.warnings.len(), 1, "{undone:?}");
    let op5 = id_at(&root, 5);
    assert_eq!(undone.frame_crc, Some(op5.crc), "the committed frame's crc");
    let said = format!("the undo of operation {op4} is committed (operation {op5})");
    assert!(
        undone.warnings[0].starts_with(&said),
        "{}",
        undone.warnings[0]
    );
    assert_eq!(target(&repo, "main"), Some(c1));
    let listed = repo
        .commit(request(&[c3], vec![put("g", b"g")], "next"))
        .unwrap();
    assert_eq!(listed.recovered.frames_listed, 1);
}

/// An undo reports what its write's recovery repaired first, of what an earlier writer left (a
/// frame whose listing failed): when it restores, and with the error that blocks it.
#[test]
fn undo_reports_what_its_write_repaired() {
    let mut listed_and_replayed = Recovered::default();
    listed_and_replayed.frames_listed = 1;
    listed_and_replayed.refs_replayed = 1;
    let (_dir, root, repo) = new_repo_as(StoreFormat::V1);
    let [c1, c2, c3] = chain(&repo);
    cas(&repo, "main", None, Some(c1)); // op 3
    let unlisted = listing_fails(&root)
        .cas_ref("main", Some(c1), Some(c2), ACTOR)
        .unwrap(); // op 4, committed and not listed
    assert_eq!(unlisted.warnings.len(), 1);

    let op4 = id_at(&root, 4);
    let undone = repo.undo(op4, UNDOER).unwrap();
    assert_eq!(undone.recovered, listed_and_replayed);
    assert_eq!(target(&repo, "main"), Some(c1));

    let unlisted = listing_fails(&root)
        .cas_ref("main", Some(c1), Some(c3), ACTOR)
        .unwrap(); // op 6, committed and not listed
    assert_eq!(unlisted.warnings.len(), 1);
    match repo.undo(op4, UNDOER) {
        Err(RepoError::UndoBlocked { recovered, .. }) => {
            assert_eq!(*recovered, listed_and_replayed);
        }
        other => panic!("expected UndoBlocked, got {other:?}"),
    }

    // The two refusals found under the lock, after the repair, report it too.
    let unlisted = listing_fails(&root)
        .cas_ref("main", Some(c3), Some(c2), ACTOR)
        .unwrap(); // op 7, committed and not listed
    assert_eq!(unlisted.warnings.len(), 1);
    match repo.undo(OpId::new(99, 0), UNDOER) {
        Err(RepoError::OpNotFound {
            op: 99,
            last: Some(7),
            recovered,
            ..
        }) => assert_eq!(*recovered, listed_and_replayed),
        other => panic!("expected OpNotFound, got {other:?}"),
    }
    let unlisted = listing_fails(&root)
        .cas_ref("main", Some(c2), Some(c1), ACTOR)
        .unwrap(); // op 8, committed and not listed
    assert_eq!(unlisted.warnings.len(), 1);
    match repo.undo(id_at(&root, 0), UNDOER) {
        Err(RepoError::NothingToUndo { op: 0, recovered }) => {
            assert_eq!(*recovered, listed_and_replayed);
        }
        other => panic!("expected NothingToUndo, got {other:?}"),
    }
}

/// The operation the crash sweep's child undoes: the template's frame that moved `main` and
/// `release/r` together.
#[cfg(feature = "test-hooks")]
const SWEPT_OP: u64 = 5;

/// The store root of this process, if it is a child the crash sweep spawned.
#[cfg(feature = "test-hooks")]
fn child_root() -> Option<Utf8PathBuf> {
    std::env::var(CHILD_ROOT).ok().map(Utf8PathBuf::from)
}

/// Runs the crash sweep's test again in a child process on the store at `root`, the child's
/// k-th filesystem call aborting it (`SWARMOPS_TEST_CRASH_AT=k`) and every call before it
/// checked against the store's write-ordering rules (`SWARMOPS_TEST_ORDER_CHECK=1`).
#[cfg(feature = "test-hooks")]
fn crash_child(root: &Utf8Path, id: OpId, k: u64) -> Output {
    run(this_test(&test_name!(
        "an_undo_crashing_at_any_call_is_whole_or_absent"
    ))
    .env(CHILD_ROOT, root)
    .env(CHILD_ID, id.to_string())
    .env(CRASH_AT_ENV, k.to_string())
    .env(ORDER_CHECK_ENV, "1"))
}

/// How many runs of the crash sweep go at once: each has a child process and a store of its
/// own.
#[cfg(feature = "test-hooks")]
const AT_ONCE: u64 = 8;

/// Runs `run(k)` for k = 1, 2, … [`AT_ONCE`] at a time, until a run reports that its child
/// finished; returns how many filesystem calls the child makes (the last k it crashed at).
/// Every run past that one must finish too.
#[cfg(feature = "test-hooks")]
fn sweep(run: impl Fn(u64) -> bool + Sync) -> u64 {
    let run = &run;
    let mut first = 1;
    loop {
        let finished: Vec<bool> = thread::scope(|scope| {
            let runs: Vec<_> = (first..first + AT_ONCE)
                .map(|k| scope.spawn(move || run(k)))
                .collect();
            runs.into_iter().map(|run| run.join().unwrap()).collect()
        });
        if let Some(at) = finished.iter().position(|&done| done) {
            assert!(
                finished[at..].iter().all(|&done| done),
                "a crash after the child's last call: {finished:?} from k={first}"
            );
            return first + u64::try_from(at).unwrap() - 1;
        }
        first += AT_ONCE;
        assert!(first < 1_000, "the child never finished");
    }
}

/// One run of the undo crash sweep: a child undoes [`SWEPT_OP`] on a copy of `template` until
/// its k-th filesystem call aborts it. Both refs must then name `c2` (nothing undone) or both
/// `c1` (the whole undo), the store verify clean, and the undo made again (its recovery under
/// the order checker, [`checked`], as the child ran) restore both, as a replay where the crash
/// left the undo whole. Returns whether the child finished (k was past its last call), and
/// whether the crash left the undo done.
#[cfg(feature = "test-hooks")]
fn undo_crashing_at(template: &Utf8Path, k: u64, c1: ChangesetId, c2: ChangesetId) -> (bool, bool) {
    let dir = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
    copy_dir(template.as_std_path(), root.as_std_path());
    let id = id_at(&root, SWEPT_OP);
    let child = crash_child(&root, id, k);
    let finished = child.status.success();
    assert!(
        finished || child.status.signal() == Some(SIGABRT),
        "k={k}: {:?}\n{}",
        child.status,
        String::from_utf8_lossy(&child.stderr)
    );

    let repo = LocalRepo::open_with_fs(&root, checked(RealFs::new())).unwrap();
    let both = || (target(&repo, "main"), target(&repo, "release/r"));
    let undone = match both() {
        (Some(main), Some(r)) if (main, r) == (c2, c2) => false,
        (Some(main), Some(r)) if (main, r) == (c1, c1) => true,
        other => panic!("k={k}: half an undo: {other:?}"),
    };
    assert!(
        undone || !finished,
        "k={k}: the undo returned but is not in the store"
    );
    assert_eq!(repo.verify(Depth::Deep).unwrap().problems, [], "k={k}");

    let again = repo.undo(id, UNDOER).unwrap();
    assert_eq!(again.already_applied, undone, "k={k}");
    assert_eq!(both(), (Some(c1), Some(c1)), "k={k}");
    assert_eq!(repo.verify(Depth::Deep).unwrap().problems, [], "k={k}");
    (finished, undone)
}

/// An undo of a frame that moved two refs, crashed at each of its filesystem calls in turn, from
/// the first to past the last: the store holds both refs restored or neither, verifies clean,
/// and takes the undo again. Some crash comes after the commit point.
#[test]
#[cfg(feature = "test-hooks")]
fn an_undo_crashing_at_any_call_is_whole_or_absent() {
    if let Some(root) = child_root() {
        // SWARMOPS_TEST_CRASH_AT is set: open shares one CrashAtFs between the object store
        // and the metadata store, so that call k is the same call in every run.
        let id: OpId = std::env::var(CHILD_ID).unwrap().parse().unwrap();
        let repo = LocalRepo::open(&root).unwrap();
        repo.undo(id, UNDOER).unwrap();
        return;
    }
    let (_template_dir, template, repo) = new_repo();
    let [c1, c2, _] = chain(&repo);
    cas(&repo, "main", None, Some(c1)); // op 3
    cas(&repo, "release/r", None, Some(c1)); // op 4
    assert_eq!(two_ref_move(&template, c1, c2), SWEPT_OP);
    drop(repo);
    let past_the_commit_point = AtomicBool::new(false);
    let calls = sweep(|k| {
        let (finished, undone) = undo_crashing_at(&template, k, c1, c2);
        if undone && !finished {
            past_the_commit_point.store(true, Ordering::Relaxed);
        }
        finished
    });
    assert!(calls > 10, "the child finished after {calls} calls");
    assert!(
        past_the_commit_point.load(Ordering::Relaxed),
        "no crash came after the commit point"
    );
}
