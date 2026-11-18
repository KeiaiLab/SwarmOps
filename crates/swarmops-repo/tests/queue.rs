//! The landing queue's facts (the M22 core design §3-§6): an enqueue is idempotent by its key
//! and returns once its frame is committed; the fold derives every entry's state from the
//! target's Queue facts alone, in enqueue order; every Queue write is a version compare-and-swap
//! and one frame; a round builds the open entries into a chain on the target's head, passing
//! over a conflicting entry and landing one the target holds.
//!
//! Every test owns a fresh FORMAT 3 store (`tempfile::tempdir()`), the feature `queue` enabled
//! with the test hooks' `supported_for_tests`.

use std::sync::Barrier;
use std::thread;

use camino::Utf8PathBuf;
use swarmops_core::{ChangesetId, Signature};
use swarmops_repo::queue::{
    EnqueueRequest, Entry, EntryId, EntryState, QueueFold, RemovalReason, RemoveRequest,
    RoundRequest, VerdictRequest, VerdictState, Written,
};
use swarmops_repo::{LocalRepo, RepoError, StoreFormat};
use swarmops_store::{FactKind, Guard, seed_nonces, supported_for_tests};
use swarmops_testkit::repo::{commit, path, put};
use tempfile::TempDir;

/// Who writes the queue's facts.
const ACTOR: &str = "test/queue";

/// The target every test queues on.
const MAIN: &str = "main";

/// The concurrent writers of the version CAS test, and the verdicts each records.
const WRITERS: usize = 4;
const VERDICTS: usize = 10;

/// The entries the version CAS test races a withdraw on.
const RACES: usize = 8;

/// A store set up for the queue: its directory, its root, the repo, and the seams it holds.
struct Store {
    _dir: TempDir,
    root: Utf8PathBuf,
    repo: LocalRepo,
    _seams: [Guard; 2],
}

/// A FORMAT 3 store whose `main` names a root changeset holding `f.txt`: one of FORMAT 2,
/// upgraded (no `init` makes FORMAT 3 yet), the feature `queue` supported and the nonces seeded.
fn v3_store() -> (Store, ChangesetId) {
    let seams = [supported_for_tests(&["queue"]), seed_nonces(22)];
    let dir = tempfile::tempdir().expect("a temp dir");
    let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).expect("UTF-8");
    let repo = LocalRepo::create_as(&root, "queue", StoreFormat::V2).expect("create");
    let base = commit(&repo, &[], vec![put("f.txt", b"base\n")]);
    repo.cas_ref(MAIN, None, Some(base), ACTOR).expect("main");
    assert!(repo.upgrade().expect("upgrade").upgraded);
    drop(repo);
    let repo = LocalRepo::open(&root).expect("open");
    assert_eq!(repo.format(), StoreFormat::V3);
    let store = Store {
        _dir: dir,
        root,
        repo,
        _seams: seams,
    };
    (store, base)
}

/// The lander of the chains: fixed, so that a round builds the same chain again.
fn lander() -> Signature {
    Signature::new(
        b"test/lander".to_vec(),
        b"lander@test.swarmops.local".to_vec(),
        1_760_000_000,
        0,
        false,
    )
    .expect("a signature")
}

/// An enqueue of `head` onto `main` by [`ACTOR`], with `key`.
fn enqueue_req(head: ChangesetId, key: Option<&str>) -> EnqueueRequest {
    let mut req = EnqueueRequest::new(MAIN, head, ACTOR);
    req.key = key.map(str::to_owned);
    req
}

/// The fold of `main`.
fn fold(repo: &LocalRepo) -> QueueFold {
    repo.queue_fold(MAIN).expect("fold")
}

/// The entry `id` of `main`'s fold.
fn entry(repo: &LocalRepo, id: EntryId) -> Entry {
    fold(repo).entry(id).expect("the entry").clone()
}

/// How many frames the journal holds: the operations so far.
fn frames(repo: &LocalRepo) -> u64 {
    repo.snapshot().expect("snapshot").next_seq()
}

/// The seq of the op log's newest operation.
fn newest_op(repo: &LocalRepo) -> u64 {
    let (ops, _) = repo.op_log(None, 1).expect("op log");
    ops[0].seq
}

/// The Queue facts about entry `id`: each one's type and the version its body leads with.
fn facts_of(repo: &LocalRepo, id: EntryId) -> Vec<(u8, u32)> {
    let read = repo.snapshot().expect("snapshot");
    let facts = read.queue_facts(id).expect("facts");
    (facts.iter())
        .filter(|fact| fact.kind() == FactKind::Queue)
        .map(|fact| {
            let version = u32::from_le_bytes(fact.body()[..4].try_into().expect("4 bytes"));
            (fact.ty(), version)
        })
        .collect()
}

/// A change on `base` that puts `name` holding `content`.
fn change(repo: &LocalRepo, base: ChangesetId, name: &str, content: &[u8]) -> ChangesetId {
    commit(repo, &[base], vec![put(name, content)])
}

/// K4: the enqueue's frame is committed when it returns (a store opened again folds the
/// entry); the same key with the same target, head and base answers the same entry and writes
/// nothing; the same key with another head or base is `QueueKeyReused`.
#[test]
fn enqueue_is_idempotent_by_key() {
    let (store, base) = v3_store();
    let repo = &store.repo;
    let a = change(repo, base, "a.txt", b"a\n");
    let b = change(repo, base, "b.txt", b"b\n");

    // 1. The first enqueue: a new entry, one frame, durable once it returns.
    let before = frames(repo);
    let first = repo
        .queue_enqueue(&enqueue_req(a, Some("k")))
        .expect("enqueue");
    assert!(!first.replayed);
    assert_eq!(frames(repo), before + 1, "one frame");
    assert_eq!(first.op, Some(before), "the frame's seq is the operation");
    assert_eq!(newest_op(repo), before, "one op in the op log");
    let reopened = LocalRepo::open(&store.root).expect("open again");
    let found = fold(&reopened);
    let again = found
        .entry(first.entry.id)
        .expect("committed before the answer");
    assert_eq!(again.state, EntryState::Waiting);
    assert_eq!((again.head, again.base), (a, base));
    assert_eq!(again.key.as_deref(), Some("k"));
    drop(reopened);

    // 2. The same key, target and head, base given or not: the same entry, nothing written.
    let replay = repo
        .queue_enqueue(&enqueue_req(a, Some("k")))
        .expect("replay");
    assert!(replay.replayed);
    assert_eq!(replay.entry.id, first.entry.id);
    assert_eq!(replay.op, None);
    let mut with_base = enqueue_req(a, Some("k"));
    with_base.base = Some(base);
    let replay = repo
        .queue_enqueue(&with_base)
        .expect("replay with its base");
    assert!(replay.replayed);
    assert_eq!(replay.entry.id, first.entry.id);
    assert_eq!(frames(repo), before + 1, "no second frame");
    assert_eq!(facts_of(repo, first.entry.id), [(0, 0)], "no second fact");

    // 3. The same key with another head, or another base: reused, nothing written.
    let reused = repo.queue_enqueue(&enqueue_req(b, Some("k")));
    assert!(
        matches!(&reused, Err(RepoError::QueueKeyReused { key, entry })
            if key == "k" && *entry == first.entry.id),
        "{reused:?}"
    );
    let mut other_base = enqueue_req(a, Some("k"));
    other_base.base = Some(a);
    let reused = repo.queue_enqueue(&other_base);
    assert!(
        matches!(reused, Err(RepoError::QueueKeyReused { .. })),
        "{reused:?}"
    );
    assert_eq!(frames(repo), before + 1, "a reused key writes nothing");

    // 4. Without a key each enqueue is an entry of its own.
    let one = repo.queue_enqueue(&enqueue_req(b, None)).expect("one");
    let two = repo.queue_enqueue(&enqueue_req(b, None)).expect("two");
    assert_ne!(one.entry.id, two.entry.id);
    assert_eq!(frames(repo), before + 3);
}

/// K5: the fold lists the entries in the order of their Enqueue frames and derives each one's
/// state, candidate, landed changeset and removal, the tree → verdict map and the cancelled
/// count from the facts alone: a store opened again folds the same.
#[test]
fn the_fold_orders_entries_by_enqueue() {
    use EntryState::{Checking, Removed, Waiting};

    let (store, base) = v3_store();
    let repo = &store.repo;

    // 1. Six entries, each its own file, in this order; another target's entry is not listed.
    let names = ["a", "b", "c", "d", "e", "g"];
    let ids: Vec<EntryId> = (names.iter())
        .map(|name| {
            let head = change(repo, base, &format!("{name}.txt"), b"x\n");
            repo.queue_enqueue(&enqueue_req(head, None))
                .expect("enqueue")
                .entry
                .id
        })
        .collect();
    let release = change(repo, base, "r.txt", b"r\n");
    repo.cas_ref("release/1", None, Some(base), ACTOR)
        .expect("release");
    let other = EnqueueRequest::new("release/1", release, ACTOR);
    repo.queue_enqueue(&other).expect("another target");
    let folded = fold(repo);
    let order: Vec<EntryId> = folded.entries.iter().map(|e| e.id).collect();
    assert_eq!(order, ids, "enqueue order");
    assert!(folded.open().all(|e| e.state == EntryState::Waiting));
    assert_eq!(folded.head, Some(base));

    // 2. A round's candidates for prefixes 1 and 2: those two are checking.
    let mut round = RoundRequest::new(MAIN, lander(), ACTOR);
    round.probes = vec![1, 2];
    let built = repo.queue_round(&round).expect("round");
    assert_eq!(built.items.len(), 6);
    let (t1, t2) = (built.items[0].tree, built.items[1].tree);
    let folded = fold(repo);
    let states: Vec<EntryState> = folded.entries.iter().map(|e| e.state).collect();
    assert_eq!(
        states,
        [Checking, Checking, Waiting, Waiting, Waiting, Waiting]
    );
    let candidate = folded.entries[1].candidate.expect("a candidate");
    assert_eq!(
        (candidate.tree, candidate.on_head, candidate.ahead),
        (t2, base, 2)
    );
    assert_eq!(candidate.candidate, built.items[1].candidate);

    // 3. Verdicts: prefix 1 passed, prefix 2 cancelled twice; then the third withdrawn.
    let verdict = |entry: EntryId, tree, state| {
        let req = VerdictRequest::new(MAIN, entry, tree, state, "just check", ACTOR);
        repo.queue_verdict(&req).expect("verdict")
    };
    verdict(ids[0], t1, VerdictState::Passed);
    verdict(ids[1], t2, VerdictState::Cancelled);
    verdict(ids[1], t2, VerdictState::Cancelled);
    repo.queue_withdraw(ids[2], ACTOR).expect("withdraw");
    let folded = fold(repo);
    let states: Vec<EntryState> = folded.entries.iter().map(|e| e.state).collect();
    assert_eq!(
        states,
        [Waiting, Waiting, Removed, Waiting, Waiting, Waiting]
    );
    assert_eq!(
        folded.verdict(t1).map(|v| v.state),
        Some(VerdictState::Passed)
    );
    assert_eq!(
        folded.verdict(t2).map(|v| v.state),
        Some(VerdictState::Cancelled)
    );
    assert_eq!(folded.entries[1].cancelled, 2);
    assert_eq!(folded.entries[0].cancelled, 0);
    let removal = folded.entries[2].removal.clone().expect("removed");
    assert_eq!(removal.reason, RemovalReason::Withdrawn);
    let open: Vec<EntryId> = folded.open().map(|e| e.id).collect();
    assert_eq!(open, [ids[0], ids[1], ids[3], ids[4], ids[5]]);

    // 4. From the facts alone: the store opened again folds the same.
    let reopened = LocalRepo::open(&store.root).expect("open again");
    assert_eq!(fold(&reopened), folded);
}

/// K6: every Queue write is a version compare-and-swap: a removal at a version a Candidate
/// write passed writes nothing; a withdraw racing writers refolds and retries; no fact is lost
/// or written twice (each fact's version is its place among its entry's), and each queue change
/// is one frame, one operation.
#[test]
fn queue_writes_are_version_cas() {
    let (store, base) = v3_store();
    let repo = &store.repo;
    let head = change(repo, base, "a.txt", b"a\n");
    let id = (repo.queue_enqueue(&enqueue_req(head, None)))
        .expect("enqueue")
        .entry
        .id;

    // 1. The fold's version, then a Candidate write: one frame.
    let version = entry(repo, id).version;
    assert_eq!(version, 1);
    let before = frames(repo);
    let mut round = RoundRequest::new(MAIN, lander(), ACTOR);
    round.probes = vec![1];
    let built = repo.queue_round(&round).expect("round");
    assert_eq!(frames(repo), before + 1, "a round is one operation");
    assert_eq!(built.op, Some(before));
    assert_eq!(newest_op(repo), before, "one op in the op log");

    // 2. A removal at the stale version: a mismatch, nothing written.
    let stale = RemoveRequest::new(MAIN, id, version, RemovalReason::Withdrawn, ACTOR);
    assert_eq!(
        repo.queue_remove(&stale).expect("remove"),
        Written::Mismatch
    );
    assert_eq!(frames(repo), before + 1, "a mismatch writes nothing");
    assert_eq!(facts_of(repo, id), [(0, 0), (1, 1)]);

    // 3. Verdict writers and rounds racing a withdraw, all released at once, on RACES
    //    entries: every verdict and the withdraw land once each, every fact (Candidates too)
    //    at its own version, one frame each.
    let tree = built.items[0].tree;
    for n in 0..RACES {
        let id = if n == 0 {
            id
        } else {
            let head = change(repo, base, &format!("r{n}.txt"), b"r\n");
            (repo.queue_enqueue(&enqueue_req(head, None)))
                .expect("enqueue")
                .entry
                .id
        };
        let (frames_before, facts_before) = (frames(repo), facts_of(repo, id).len());
        race(repo, &round, id, tree);
        let facts = facts_of(repo, id);
        let total = facts.len();
        let versions: Vec<u32> = facts.iter().map(|&(_, version)| version).collect();
        let places: Vec<u32> = (0..u32::try_from(total).expect("few")).collect();
        assert_eq!(versions, places, "each fact at its own version");
        let verdicts = facts.iter().filter(|&&(ty, _)| ty == 2).count();
        assert_eq!(verdicts, WRITERS * VERDICTS, "none lost, none twice");
        assert_eq!(facts.iter().filter(|&&(ty, _)| ty == 4).count(), 1);
        let written = u64::try_from(total - facts_before).expect("few");
        assert_eq!(frames(repo), frames_before + written, "one frame each");
        let (ops, _) = repo.op_log(None, total - facts_before).expect("op log");
        let seqs: Vec<u64> = ops.iter().map(|op| op.seq).rev().collect();
        let after = frames_before + written;
        assert_eq!(
            seqs,
            (frames_before..after).collect::<Vec<_>>(),
            "each an op"
        );
    }

    // 4. A withdraw of a removed entry writes nothing more.
    let total = facts_of(repo, id).len();
    let again = repo.queue_withdraw(id, ACTOR).expect("withdraw again");
    assert_eq!(again.state, EntryState::Removed);
    assert_eq!(facts_of(repo, id).len(), total);
}

/// Releases at once WRITERS verdict writers, a round runner and a withdraw of entry `id`.
fn race(repo: &LocalRepo, round: &RoundRequest, id: EntryId, tree: swarmops_core::TreeId) {
    let start = Barrier::new(WRITERS + 2);
    thread::scope(|scope| {
        for _ in 0..WRITERS {
            scope.spawn(|| {
                start.wait();
                for _ in 0..VERDICTS {
                    let state = VerdictState::Cancelled;
                    let req = VerdictRequest::new(MAIN, id, tree, state, "just check", ACTOR);
                    repo.queue_verdict(&req).expect("verdict");
                }
            });
        }
        scope.spawn(|| {
            start.wait();
            for _ in 0..VERDICTS {
                match repo.queue_round(round) {
                    Ok(_) | Err(RepoError::QueueContended { .. }) => {}
                    Err(e) => panic!("round: {e:?}"),
                }
            }
        });
        start.wait();
        let withdrawn = repo.queue_withdraw(id, ACTOR).expect("withdraw");
        assert_eq!(withdrawn.state, EntryState::Removed);
    });
}

/// K9: a round's chain passes over a conflicting entry, removing it with reason conflict and a
/// report naming the path, and builds the later entries on the previous tip; an entry whose
/// head the target holds lands with the target's changeset.
#[test]
fn the_chain_passes_over_a_conflict() {
    let (store, base) = v3_store();
    let repo = &store.repo;

    // 1. A changes f.txt, B changes it otherwise (a conflict with A), C adds c.txt, D is a
    //    change the target takes before the round.
    let a = change(repo, base, "f.txt", b"a\n");
    let b = change(repo, base, "f.txt", b"b\n");
    let c = change(repo, base, "c.txt", b"c\n");
    let d = change(repo, base, "d.txt", b"d\n");
    let ids: Vec<EntryId> = ([a, b, c, d].iter())
        .map(|&head| {
            (repo.queue_enqueue(&enqueue_req(head, None)))
                .expect("enqueue")
                .entry
                .id
        })
        .collect();
    repo.cas_ref(MAIN, Some(base), Some(d), ACTOR)
        .expect("main takes d");

    // 2. The round on d: A and C chained, B removed, D landed.
    let mut round = RoundRequest::new(MAIN, lander(), ACTOR);
    round.probes = vec![1, 2];
    let built = repo.queue_round(&round).expect("round");
    assert_eq!(built.head, Some(d));
    let chained: Vec<EntryId> = built.items.iter().map(|item| item.entry).collect();
    assert_eq!(chained, [ids[0], ids[2]]);
    assert_eq!(built.removed, [ids[1]]);
    assert_eq!(built.landed, [ids[3]]);

    // 3. C's candidate is on A's, A's on the head; C's tree holds A's f.txt and its own file.
    let read = repo.snapshot().expect("snapshot");
    let l1 = read.changeset(built.items[0].candidate).expect("read");
    let l2 = read.changeset(built.items[1].candidate).expect("read");
    let (l1, l2) = (l1.expect("L1 inserted"), l2.expect("L2 inserted"));
    assert_eq!(l1.changeset.parents, [d]);
    assert_eq!(l2.changeset.parents, [l1.id]);
    let file = |name: &str| {
        let found = read.read_blob_at(l2.id, &path(name)).expect("read");
        found.map(|(_, _, bytes)| bytes)
    };
    assert_eq!(file("f.txt").as_deref(), Some(&b"a\n"[..]));
    assert_eq!(file("c.txt").as_deref(), Some(&b"c\n"[..]));
    assert_eq!(file("d.txt").as_deref(), Some(&b"d\n"[..]));
    drop(read);

    // 4. The fold: B removed for its conflict, its report naming f.txt; D landed as d.
    let folded = fold(repo);
    let removal = folded.entries[1].removal.clone().expect("B removed");
    assert_eq!(removal.reason, RemovalReason::Conflict);
    let report = removal.report.expect("a report");
    let report = repo
        .snapshot()
        .expect("snapshot")
        .blob(report)
        .expect("blob");
    assert!(
        String::from_utf8_lossy(&report).contains("f.txt"),
        "the report names the path"
    );
    assert_eq!(folded.entries[3].state, EntryState::Landed);
    assert_eq!(folded.entries[3].landed, Some(d));
    let open: Vec<EntryId> = folded.open().map(|e| e.id).collect();
    assert_eq!(open, [ids[0], ids[2]]);
}
