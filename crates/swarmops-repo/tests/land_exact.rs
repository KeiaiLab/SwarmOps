//! The landing queue's exact publish (the M22 core design §2 I-1, I-2 and I-8, §5 step 8): the
//! target moves from the head its chain was built on to the green prefix's tip, and each entry
//! of the prefix gets its Landed fact, in one frame; a moved target, an entry whose version
//! moved (withdrawn) or a tip tree without a passed verdict writes nothing at all. A publish
//! crashing at any filesystem call leaves both or neither.
//!
//! Every test owns a fresh FORMAT 3 store (`tempfile::tempdir()`), the feature `queue` enabled
//! with the test hooks' `supported_for_tests`.

use std::os::unix::process::ExitStatusExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_core::{ChangesetId, Signature};
use swarmops_repo::land::{ExactPublish, ExactPublished};
use swarmops_repo::queue::{
    EnqueueRequest, EntryId, EntryState, QueueFold, RoundItem, RoundRequest, VerdictRequest,
    VerdictState,
};
use swarmops_repo::verify::Depth;
use swarmops_repo::{LocalRepo, RepoError, StoreError, StoreFormat};
use swarmops_store::{CRASH_AT_ENV, Guard, ORDER_CHECK_ENV, seed_nonces, supported_for_tests};
use swarmops_testkit::child::{SIGABRT, run, this_test};
use swarmops_testkit::fs::copy_dir;
use swarmops_testkit::repo::{commit, put};
use swarmops_testkit::test_name;
use tempfile::TempDir;

/// Who writes the queue's facts and moves the target.
const ACTOR: &str = "test/land-exact";

/// The target every test queues on.
const MAIN: &str = "main";

/// The check every verdict names.
const CHECK: &str = "just check";

/// Set only in the children the crash sweep spawns: the store root.
const CHILD_ROOT: &str = "SWARMOPS_TEST_REPO_LAND_EXACT_ROOT";

/// How many runs of the crash sweep go at once: each has a child and a store of its own.
const AT_ONCE: u64 = 8;

/// A store set up for the queue: its directory, its root, the repo, and the seams it holds.
struct Store {
    _dir: TempDir,
    root: Utf8PathBuf,
    repo: LocalRepo,
    _seams: [Guard; 2],
}

/// A queue of two entries on `main`, built into a chain of two on its head and both probed.
struct Queued {
    store: Store,
    /// What `main` names: the head the chain is on.
    head: ChangesetId,
    /// The chain, in enqueue order.
    items: Vec<RoundItem>,
}

/// The seams a queue store needs: the feature `queue` supported, the nonces seeded.
fn seams() -> [Guard; 2] {
    [supported_for_tests(&["queue"]), seed_nonces(22)]
}

/// A FORMAT 3 store whose `main` names a root changeset holding `f.txt`: one of FORMAT 2,
/// upgraded (no `init` makes FORMAT 3 yet).
fn v3_store() -> (Store, ChangesetId) {
    let seams = seams();
    let dir = tempfile::tempdir().expect("a temp dir");
    let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).expect("UTF-8");
    let repo = LocalRepo::create_as(&root, "exact", StoreFormat::V2).expect("create");
    let base = commit(&repo, &[], vec![put("f.txt", b"base\n")]);
    repo.cas_ref(MAIN, None, Some(base), ACTOR).expect("main");
    assert!(repo.upgrade().expect("upgrade").upgraded);
    drop(repo);
    let repo = LocalRepo::open(&root).expect("open");
    let store = Store {
        _dir: dir,
        root,
        repo,
        _seams: seams,
    };
    (store, base)
}

/// The lander of the chains: fixed, so that every store builds the same chain.
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

/// Two entries, `a.txt` and `b.txt` each on `main`'s head, built into a chain whose two
/// prefixes are probed; the second prefix's tree has a passed verdict, the first's none.
fn queued() -> Queued {
    // 1. The store and two changes of its head.
    let (store, head) = v3_store();
    let repo = &store.repo;
    for name in ["a.txt", "b.txt"] {
        let change = commit(repo, &[head], vec![put(name, b"x\n")]);
        let req = EnqueueRequest::new(MAIN, change, ACTOR);
        repo.queue_enqueue(&req).expect("enqueue");
    }

    // 2. The round: the chain L1, L2 on the head, both prefixes candidates.
    let mut round = RoundRequest::new(MAIN, lander(), ACTOR);
    round.probes = vec![1, 2];
    let items = repo.queue_round(&round).expect("round").items;
    assert_eq!(items.len(), 2);

    // 3. A passed verdict on L2's tree, recorded on its last entry.
    let tip = &items[1];
    let verdict = VerdictRequest::new(
        MAIN,
        tip.entry,
        tip.tree,
        VerdictState::Passed,
        CHECK,
        ACTOR,
    );
    repo.queue_verdict(&verdict).expect("verdict");
    Queued { store, head, items }
}

/// The fold of `main`.
fn fold(repo: &LocalRepo) -> QueueFold {
    repo.queue_fold(MAIN).expect("fold")
}

/// What `main` names.
fn main_head(repo: &LocalRepo) -> Option<ChangesetId> {
    repo.get_ref(MAIN).expect("main").map(|state| state.target)
}

/// How many frames the journal holds: the operations so far.
fn frames(repo: &LocalRepo) -> u64 {
    repo.snapshot().expect("snapshot").next_seq()
}

/// The publish of `items[..upto]` on `head`: the target to the last one's changeset, each
/// entry at the version `main`'s fold gives it now.
fn publish_req(repo: &LocalRepo, head: ChangesetId, items: &[RoundItem]) -> ExactPublish {
    let fold = fold(repo);
    let tip = items.last().expect("an item").candidate;
    let mut req = ExactPublish::new(MAIN, head, tip, ACTOR);
    req.landed = (items.iter())
        .map(|item| {
            let entry = fold.entry(item.entry).expect("the entry");
            (item.entry, entry.version)
        })
        .collect();
    req
}

/// That `publish` wrote nothing: `main`, the journal and the fold are as `before` holds them.
fn wrote_nothing(repo: &LocalRepo, before: &(Option<ChangesetId>, u64, QueueFold), what: &str) {
    assert_eq!(main_head(repo), before.0, "{what}: main moved");
    assert_eq!(frames(repo), before.1, "{what}: a frame was written");
    assert_eq!(fold(repo), before.2, "{what}: the queue changed");
}

/// `main`, the journal's length and the fold, now.
fn state(repo: &LocalRepo) -> (Option<ChangesetId>, u64, QueueFold) {
    (main_head(repo), frames(repo), fold(repo))
}

/// K7: the publish moves `main` from the head to L2 and records both entries landed (L1 and
/// L2) in one frame, one operation; a tip tree without a passed verdict, a target moved since
/// the round, an entry withdrawn or one whose version moved writes nothing at all.
#[test]
fn exact_publish_is_all_or_nothing() {
    // 1. A tip without a passed verdict: L1's tree has none.
    let Queued { store, head, items } = queued();
    let repo = &store.repo;
    let before = state(repo);
    let req = publish_req(repo, head, &items[..1]);
    match repo.publish_exact(&req) {
        Err(RepoError::Store(StoreError::BadCall(why))) => {
            assert!(why.contains("passed verdict"), "{why}");
        }
        other => panic!("a tip without a passed verdict: {other:?}"),
    }
    wrote_nothing(repo, &before, "no verdict");

    // 2. The target moved since the round: Moved, naming what it names.
    let Queued { store, head, items } = queued();
    let repo = &store.repo;
    let req = publish_req(repo, head, &items);
    let outside = commit(repo, &[head], vec![put("c.txt", b"c\n")]);
    repo.cas_ref(MAIN, Some(head), Some(outside), ACTOR)
        .expect("an outside move");
    let before = state(repo);
    let moved = repo.publish_exact(&req).expect("publish");
    assert_eq!(
        moved,
        ExactPublished::Moved {
            actual: Some(outside)
        }
    );
    wrote_nothing(repo, &before, "moved");

    // 3. An entry withdrawn since its version was read: its Landed CAS fails.
    let Queued { store, head, items } = queued();
    let repo = &store.repo;
    let req = publish_req(repo, head, &items);
    repo.queue_withdraw(items[1].entry, ACTOR)
        .expect("withdraw");
    let before = state(repo);
    let refused = repo.publish_exact(&req).expect("publish");
    assert_eq!(
        refused,
        ExactPublished::Mismatch {
            entry: items[1].entry
        }
    );
    wrote_nothing(repo, &before, "withdrawn");

    // 4. An open entry whose version moved (a cancelled verdict): its Landed CAS fails too.
    let Queued { store, head, items } = queued();
    let repo = &store.repo;
    let req = publish_req(repo, head, &items);
    let cancelled = VerdictRequest::new(
        MAIN,
        items[0].entry,
        items[0].tree,
        VerdictState::Cancelled,
        CHECK,
        ACTOR,
    );
    repo.queue_verdict(&cancelled).expect("verdict");
    let before = state(repo);
    let refused = repo.publish_exact(&req).expect("publish");
    assert_eq!(
        refused,
        ExactPublished::Mismatch {
            entry: items[0].entry
        }
    );
    wrote_nothing(repo, &before, "version moved");

    // 5. The publish: main to L2, both landed, in one frame that is the newest operation.
    let Queued { store, head, items } = queued();
    let repo = &store.repo;
    let req = publish_req(repo, head, &items);
    let before = frames(repo);
    let ExactPublished::Published { op, warnings } = repo.publish_exact(&req).expect("publish")
    else {
        panic!("the publish was refused");
    };
    assert_eq!(warnings, Vec::<String>::new());
    assert_eq!(frames(repo), before + 1, "one frame");
    let (ops, _) = repo.op_log(None, 1).expect("op log");
    assert_eq!(ops[0].seq, op, "the publish is the newest operation");
    assert_eq!(main_head(repo), Some(items[1].candidate));
    assert_landed(repo, &items);
}

/// That every entry of `items` is landed as its own prefix's changeset.
fn assert_landed(repo: &LocalRepo, items: &[RoundItem]) {
    let fold = fold(repo);
    for item in items {
        let entry = fold.entry(item.entry).expect("the entry");
        assert_eq!(entry.state, EntryState::Landed, "{}", item.entry);
        assert_eq!(entry.landed, Some(item.candidate), "{}", item.entry);
    }
}

/// The store root of this process, if it is a child the crash sweep spawned.
fn child_root() -> Option<Utf8PathBuf> {
    std::env::var(CHILD_ROOT).ok().map(Utf8PathBuf::from)
}

/// A child's publish: every open entry of `main`, in enqueue order, onto `main`'s head, the
/// target to the last one's candidate, each at the version the fold gives it.
fn child_publish(root: &Utf8Path) {
    let _seams = seams();
    let repo = LocalRepo::open(root).expect("open");
    let fold = fold(&repo);
    let head = fold.head.expect("main");
    let open: Vec<(EntryId, u32, ChangesetId)> = (fold.open())
        .map(|entry| {
            let tip = entry.candidate.expect("a candidate").candidate;
            (entry.id, entry.version, tip)
        })
        .collect();
    let tip = open.last().expect("an open entry").2;
    let mut req = ExactPublish::new(MAIN, head, tip, ACTOR);
    req.landed = open.iter().map(|&(id, version, _)| (id, version)).collect();
    let done = repo.publish_exact(&req).expect("publish");
    assert!(matches!(done, ExactPublished::Published { .. }), "{done:?}");
}

/// One run of the crash sweep: on a copy of `template`, a child publishes the queue until its
/// k-th filesystem call aborts it. `main` then names the head with both entries open, or L2
/// with both landed, and the store verifies clean. Returns whether the child finished, and
/// whether the publish was committed.
fn publish_crashing_at(
    template: &Utf8Path,
    head: ChangesetId,
    items: &[RoundItem],
    k: u64,
) -> (bool, bool) {
    // 1. A child on a copy, crashing at its k-th call.
    let dir = tempfile::tempdir().expect("a temp dir");
    let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).expect("UTF-8");
    copy_dir(template.as_std_path(), root.as_std_path());
    let test = test_name!("an_exact_publish_crashing_at_any_call_lands_all_or_nothing");
    let child = run(this_test(&test)
        .env(CHILD_ROOT, &root)
        .env(CRASH_AT_ENV, k.to_string())
        .env(ORDER_CHECK_ENV, "1"));
    let finished = child.status.success();
    assert!(
        finished || child.status.signal() == Some(SIGABRT),
        "k={k}: {:?}\n{}",
        child.status,
        String::from_utf8_lossy(&child.stderr)
    );

    // 2. Both or neither, and a clean deep verify.
    let repo = LocalRepo::open(&root).expect("open");
    let fold = fold(&repo);
    let landed = (items.iter())
        .filter(|item| {
            fold.entry(item.entry)
                .is_some_and(|e| e.state == EntryState::Landed)
        })
        .count();
    let committed = main_head(&repo) == Some(items[1].candidate);
    if committed {
        assert_landed(&repo, items);
    } else {
        assert_eq!(main_head(&repo), Some(head), "k={k}: main moved elsewhere");
        assert_eq!(landed, 0, "k={k}: landed without main's move");
    }
    assert!(
        committed || !finished,
        "k={k}: the publish returned but is not stored"
    );
    assert_eq!(
        repo.verify(Depth::Deep).expect("verify").problems,
        [],
        "k={k}"
    );
    (finished, committed)
}

/// Runs `run(k)` for k = 1, 2, … [`AT_ONCE`] at a time, until a run reports that its child
/// finished; returns how many filesystem calls the child makes (the last k it crashed at).
/// Every run past that one must finish too.
fn sweep(run: impl Fn(u64) -> bool + Sync) -> u64 {
    let run = &run;
    let mut first = 1;
    loop {
        let finished: Vec<bool> = thread::scope(|scope| {
            let runs: Vec<_> = (first..first + AT_ONCE)
                .map(|k| scope.spawn(move || run(k)))
                .collect();
            runs.into_iter()
                .map(|run| run.join().expect("a run"))
                .collect()
        });
        if let Some(at) = finished.iter().position(|&done| done) {
            assert!(
                finished[at..].iter().all(|&done| done),
                "a crash after the child's last call: {finished:?} from k={first}"
            );
            return first + u64::try_from(at).expect("small") - 1;
        }
        first += AT_ONCE;
        assert!(first < 1_000, "the child never finished");
    }
}

/// K8: a publish crashing at any filesystem call (`CrashAtFs`) leaves `main` moved with every
/// Landed fact, or neither, and the store verifies clean; some crash comes after the commit
/// point.
#[test]
fn an_exact_publish_crashing_at_any_call_lands_all_or_nothing() {
    if let Some(root) = child_root() {
        child_publish(&root);
        return;
    }
    let Queued { store, head, items } = queued();
    let template = store.root.clone();
    drop(store.repo);
    let past_the_commit_point = AtomicBool::new(false);
    let calls = sweep(|k| {
        let (finished, committed) = publish_crashing_at(&template, head, &items, k);
        if committed && !finished {
            past_the_commit_point.store(true, Ordering::Relaxed);
        }
        finished
    });
    assert!(calls > 5, "the child finished after {calls} calls");
    assert!(
        past_the_commit_point.load(Ordering::Relaxed),
        "no crash came after the commit point"
    );
}
