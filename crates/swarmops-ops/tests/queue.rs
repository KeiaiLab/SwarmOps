//! `LocalOps`'s landing queue (the M22 core design §6): `queue_enqueue`, `queue_status` and
//! `queue_withdraw` over a local store, and the runner's `QueueDriver` that lands an entry.
//!
//! Every test owns a fresh store in a temp dir of its own, the feature `queue` enabled with the
//! test hooks' `supported_for_tests` (the store's build does not support it yet).

use camino::Utf8PathBuf;
use swarmops_api::{
    ChangeInput, ChangeOp, CommitReq, ContentInput, ErrorCode, Identity, ModeInput, OpError,
    OpLogReq, QueueEnqueueReq, QueueEntryState, QueuePublishReq, QueuePublishStatus, QueueRoundReq,
    QueueStatusReq, QueueVerdictReq, QueueWithdrawReq, RemovalReason, StoreOps, VerdictState,
};
use swarmops_ops::LocalOps;
use swarmops_store::{Guard, supported_for_tests};
use swarmops_ws::QueueDriver;
use tempfile::TempDir;

/// The target every test queues on.
const MAIN: &str = "main";

/// The check a verdict names.
const CHECK: &str = "true";

/// Who enqueues and withdraws.
fn who() -> Identity {
    Identity::parse("test/queue").expect("an identity")
}

/// A store of `format` whose `main` names a changeset holding `f.txt`: the store, its temp
/// dir, and that changeset.
fn store(format: u32) -> (TempDir, LocalOps, String) {
    let dir = tempfile::tempdir().expect("a temp dir");
    let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).expect("UTF-8");
    let (ops, _) = LocalOps::init_as(&root, "queue", Some(format)).expect("init");
    let base = commit(&ops, None, "f.txt", Some(MAIN));
    (dir, ops, base)
}

/// Commits `name` on `parent` (on `main` when none), moving `advance` if given: its changeset.
fn commit(ops: &LocalOps, parent: Option<&str>, name: &str, advance: Option<&str>) -> String {
    let mut change = ChangeInput::new(name.to_owned(), ChangeOp::Put);
    change.content = Some(ContentInput::Inline(format!("{name}\n")));
    change.mode = Some(ModeInput::File);
    let mut req = CommitReq::new(vec![change], format!("add {name}"));
    req.parent = parent.map(Into::into);
    req.advance = advance.map(str::to_owned);
    ops.commit(&who(), req).expect("commit").changeset
}

/// The enqueue of `head` onto `main` with `key`.
fn enqueue_req(head: &str, key: Option<&str>) -> QueueEnqueueReq {
    let mut req = QueueEnqueueReq::new(MAIN, head);
    req.key = key.map(str::to_owned);
    req
}

/// The newest operation's number.
fn newest_op(ops: &LocalOps) -> u64 {
    let page = ops.op_log(OpLogReq::default()).expect("op log");
    page.ops[0].op
}

/// `err`'s code and `details.reason`.
fn refusal(err: &OpError) -> (&str, Option<&str>) {
    let reason = err.details.get("reason").and_then(|reason| reason.as_str());
    (err.code.as_str(), reason)
}

/// Lands entry `id`, alone in `main`'s queue on `base`, through the runner's driver: a round
/// recording its candidate, a passed verdict, a round that finds it, and the publish. Its
/// candidate.
fn land(ops: &LocalOps, base: &str, id: &str) -> String {
    let mut probe = QueueRoundReq::new(MAIN);
    probe.probes = vec![1];
    let round = ops.plan_round(probe).expect("a round with a probe");
    assert_eq!(round.head.as_deref(), Some(base), "built on main");
    assert_eq!(round.items.len(), 1, "one link: {round:?}");
    let link = round.items[0].clone();
    let verdict = QueueVerdictReq::new(MAIN, id, &link.tree, VerdictState::Passed, CHECK);
    ops.record_verdict(verdict).expect("verdict");
    let decided = ops.plan_round(QueueRoundReq::new(MAIN)).expect("a round");
    assert_eq!(
        decided.items[0].candidate, link.candidate,
        "the same chain again"
    );
    assert_eq!(
        decided.items[0].verdict,
        Some(VerdictState::Passed),
        "its verdict"
    );
    let mut publish = QueuePublishReq::new(MAIN, base);
    publish.tip = Some(link.candidate.clone());
    publish.landed = vec![id.to_owned()];
    let published = ops.publish(publish).expect("publish");
    assert_eq!(
        published.status,
        QueuePublishStatus::Published,
        "{published:?}"
    );
    assert_eq!(
        published.head.as_deref(),
        Some(link.candidate.as_str()),
        "main moved"
    );
    link.candidate
}

/// K11: enqueue, status and withdraw round-trip on a FORMAT 3 store; on a FORMAT 2 store an
/// enqueue is `E_FORMAT_REQUIRED`; a head the target names is `E_NOTHING_TO_LAND`; a withdraw
/// of a landed entry is `E_INVALID_ARG` `already_landed` and writes nothing.
#[test]
fn local_queue_ops_round_trip() {
    let _queue: Guard = supported_for_tests(&["queue"]);
    let (_dir, ops, base) = store(3);
    let a = commit(&ops, Some(&base), "a.txt", None);

    // 1. An enqueue: a new waiting entry, first in line, from the merge base.
    let first = (ops.queue_enqueue(&who(), enqueue_req(&a, Some("k")))).expect("enqueue");
    assert!(!first.replayed, "a new key makes a new entry");
    let entry = first.entry;
    assert_eq!(
        (
            entry.state,
            entry.position,
            entry.head.as_str(),
            entry.base.as_str()
        ),
        (QueueEntryState::Waiting, Some(1), a.as_str(), base.as_str()),
        "the entry as enqueued"
    );
    assert_eq!(entry.key.as_deref(), Some("k"), "the entry keeps its key");

    // 2. The status lists it, by target and by id; the same key answers it again.
    let mut by_target = QueueStatusReq::new();
    by_target.target = Some(MAIN.to_owned());
    let listed = ops.queue_status(by_target.clone()).expect("status");
    assert_eq!(listed.entries, vec![entry.clone()], "status of main");
    let mut by_id = QueueStatusReq::new();
    by_id.entry = Some(entry.id.clone());
    let one = ops.queue_status(by_id).expect("status of the entry");
    assert_eq!(one.entries, vec![entry.clone()], "status of the entry");
    let again = (ops.queue_enqueue(&who(), enqueue_req(&a, Some("k")))).expect("replay");
    assert!(again.replayed, "the same key replays");
    assert_eq!(again.entry.id, entry.id, "the same key answers its entry");

    // 3. A head the target names already has nothing to land.
    let err = (ops.queue_enqueue(&who(), enqueue_req(&base, None))).unwrap_err();
    assert_eq!(err.code, ErrorCode::NothingToLand.as_str(), "{err:?}");

    // 4. A withdraw removes it, reason withdrawn.
    let withdrawn =
        (ops.queue_withdraw(&who(), QueueWithdrawReq::new(&entry.id))).expect("withdraw");
    assert_eq!(
        (withdrawn.state, withdrawn.removal, withdrawn.position),
        (
            QueueEntryState::Removed,
            Some(RemovalReason::Withdrawn),
            None
        ),
        "the withdrawn entry"
    );
    let listed = ops.queue_status(by_target.clone()).expect("status");
    assert_eq!(listed.entries, vec![withdrawn], "status after the withdraw");

    // 5. Another entry landed through the runner's driver: a candidate, its verdict, a publish.
    let b = commit(&ops, Some(&base), "b.txt", None);
    let landing = (ops.queue_enqueue(&who(), enqueue_req(&b, None))).expect("enqueue b");
    let id = landing.entry.id;
    let candidate = land(&ops, &base, &id);
    let mut by_id = QueueStatusReq::new();
    by_id.entry = Some(id.clone());
    let landed = ops.queue_status(by_id).expect("status of b").entries;
    assert_eq!(landed.len(), 1, "{landed:?}");
    assert_eq!(
        (landed[0].state, landed[0].landed.as_deref()),
        (QueueEntryState::Landed, Some(candidate.as_str())),
        "b landed as its candidate"
    );

    // 6. A withdraw of the landed entry is refused, and records nothing.
    let before = newest_op(&ops);
    let err = (ops.queue_withdraw(&who(), QueueWithdrawReq::new(&id))).unwrap_err();
    assert_eq!(
        refusal(&err),
        (ErrorCode::InvalidArg.as_str(), Some("already_landed")),
        "{err:?}"
    );
    assert_eq!(
        newest_op(&ops),
        before,
        "the refused withdraw wrote nothing"
    );

    // 7. A FORMAT 2 store has no queue.
    let (_old_dir, old, old_base) = store(2);
    let c = commit(&old, Some(&old_base), "c.txt", None);
    let err = (old.queue_enqueue(&who(), enqueue_req(&c, None))).unwrap_err();
    assert_eq!(err.code, ErrorCode::FormatRequired.as_str(), "{err:?}");
}
