//! The landing queue over a local store (the M22 core design §5, §6): `queue_enqueue`,
//! `queue_status` and `queue_withdraw`, and the runner's [`QueueDriver`], each a call of the
//! repository's queue ([`swarmops_repo::queue`]) with its ids in full.
//!
//! ```text
//!   enqueue / status / withdraw ──▶ Repo::queue_enqueue / queue_fold / queue_withdraw
//!   runner (swarmops-ws) ──▶ QueueDriver for LocalOps
//!       try_lane, lane_free ──▶ LaneLock (meta/lanes/)
//!       plan_round          ──▶ Repo::queue_round, signed at the head's time
//!       record_verdict      ──▶ Repo::queue_verdict
//!       publish             ──▶ Repo::publish_exact, then Repo::queue_remove
//! ```
//!
//! The missing-check refusal of an enqueue is the workspace layer's (`ws::queue::enqueue`): the
//! check's configuration is read there.

use swarmops_api::ErrorCode;
use swarmops_api::{
    Identity, OpError, QueueEnqueueReq, QueueEnqueueResp, QueueEntryOut, QueueEntryState,
    QueuePublishReq, QueuePublishResp, QueuePublishStatus, QueueRemovalIn, QueueRoundItem,
    QueueRoundReq, QueueRoundResp, QueueStatusReq, QueueStatusResp, QueueVerdictReq,
    QueueWithdrawReq, RemovalReason, VerdictState,
};
use swarmops_core::{ChangesetId, ObjectId, TreeId};
use swarmops_repo::RepoError;
use swarmops_repo::land::{ExactPublish, ExactPublished};
use swarmops_repo::queue::{
    self as repo_queue, EnqueueRequest, Entry, EntryId, EntryState, QueueFold, RemoveRequest,
    RoundRequest, VerdictRequest,
};
use swarmops_store::{LaneLock, Layout};
use swarmops_ws::{LaneGuard, QueueDriver};

use crate::error;
use crate::local::{LocalOps, Recording, RefUse, check_ref_use};

/// The identity a queue runner writes its facts and signs its chains as.
const RUNNER: &str = "swarmops/queue-runner";

/// The rounds [`QueueDriver::plan_round`] builds at most while the target moves under it.
const ROUND_TRIES: u32 = 8;

/// `queue_enqueue` as `who`: `req.head` and `req.base` resolved in one snapshot, then the
/// repository's enqueue. A key of more than 128 bytes is `E_INVALID_ARG`.
pub(crate) fn enqueue(
    ops: &Recording<'_>,
    who: &Identity,
    req: QueueEnqueueReq,
) -> Result<QueueEnqueueResp, OpError> {
    // 1. The request's own rules, before anything is read.
    check_ref_use(&req.target, RefUse::Publish)?;
    if let Some(key) = &req.key
        && key.len() > repo_queue::MAX_KEY_BYTES
    {
        return Err(error::error(
            ErrorCode::InvalidArg,
            format!(
                "a queue key of {} bytes: at most {}",
                key.len(),
                repo_queue::MAX_KEY_BYTES
            ),
        )
        .with_details(error::details([("reason", "key_too_long".into())])));
    }

    // 2. The revisions, in one snapshot.
    let repo = &ops.ops.repo;
    let read = repo.snapshot().map_err(error::op)?;
    let head = read.resolve(req.head.as_str()).map_err(error::op)?;
    ops.resolved.record(req.head.as_str(), head);
    let base = match &req.base {
        Some(rev) => {
            let id = read.resolve(rev.as_str()).map_err(error::op)?;
            ops.resolved.record(rev.as_str(), id);
            Some(id)
        }
        None => None,
    };
    drop(read);

    // 3. The entry, durable once it returns; its place from the queue after.
    let mut enqueue = EnqueueRequest::new(&req.target, head, who.agent());
    enqueue.base = base;
    enqueue.key = req.key;
    let done = repo.queue_enqueue(&enqueue).map_err(error::op)?;
    let fold = repo.queue_fold(&req.target).map_err(error::op)?;
    let entry = fold.entry(done.entry.id).unwrap_or(&done.entry);
    Ok(QueueEnqueueResp::new(
        entry_out(&fold, entry)?,
        done.replayed,
    ))
}

/// `queue_status`: one target's entries, or every branch's; only `req.entry`'s when given.
pub(crate) fn status(ops: &Recording<'_>, req: QueueStatusReq) -> Result<QueueStatusResp, OpError> {
    // 1. The targets: the one named, or every ref's queue.
    let repo = &ops.ops.repo;
    let wanted = req.entry.as_deref().map(entry_id).transpose()?;
    let targets = match req.target {
        Some(target) => vec![target],
        None => (repo.list_refs().map_err(error::op)?)
            .into_iter()
            .map(|(name, _)| name)
            .collect(),
    };

    // 2. Each target's entries, open ones first in enqueue order.
    let mut entries = Vec::new();
    for target in targets {
        let fold = repo.queue_fold(&target).map_err(error::op)?;
        let (open, closed): (Vec<&Entry>, Vec<&Entry>) =
            fold.entries.iter().partition(|entry| entry.is_open());
        for entry in open.into_iter().chain(closed) {
            if wanted.is_some_and(|id| id != entry.id) {
                continue;
            }
            entries.push(entry_out(&fold, entry)?);
        }
    }
    Ok(QueueStatusResp::new(entries))
}

/// `queue_withdraw` as `who`: the entry removed, reason withdrawn; one that landed is
/// `E_INVALID_ARG` (`details.reason` `already_landed`), and nothing is written.
pub(crate) fn withdraw(
    ops: &Recording<'_>,
    who: &Identity,
    req: &QueueWithdrawReq,
) -> Result<QueueEntryOut, OpError> {
    let repo = &ops.ops.repo;
    let id = entry_id(&req.entry)?;
    let entry = repo.queue_withdraw(id, who.agent()).map_err(error::op)?;
    let fold = repo.queue_fold(&entry.target).map_err(error::op)?;
    entry_out(&fold, fold.entry(id).unwrap_or(&entry))
}

/// A target's lane, held by a [`LocalOps`] runner until dropped.
struct Lane {
    _lock: LaneLock,
}

impl LaneGuard for Lane {}

impl QueueDriver for LocalOps {
    fn try_lane(&self, target: &str) -> Result<Option<Box<dyn LaneGuard>>, OpError> {
        self.repo.check_ref_name(target).map_err(error::op)?;
        let layout = Layout::new(self.store_path());
        let lane = LaneLock::try_acquire(&layout, target).map_err(store_error)?;
        Ok(lane.map(|lane| Box::new(Lane { _lock: lane }) as Box<dyn LaneGuard>))
    }

    fn lane_free(&self, target: &str) -> Result<bool, OpError> {
        Ok(self.try_lane(target)?.is_some())
    }

    fn plan_round(&self, req: QueueRoundReq) -> Result<QueueRoundResp, OpError> {
        // 1. The request: its head and probes in the repository's terms.
        let expect = req.expect_head.as_deref().map(changeset_id).transpose()?;
        let probes = (req.probes.iter())
            .map(|&probe| usize::try_from(probe).map_err(|_| bad_arg("a probe past usize")))
            .collect::<Result<Vec<_>, _>>()?;

        // 2. Rounds on the head read, signed at its time, so that a round on the same head
        //    builds the same chain again; again while the target moves under it.
        let mut round = None;
        for _ in 0..ROUND_TRIES {
            let head = self.repo.get_ref(&req.target).map_err(error::op)?;
            let head = head.map(|state| state.target);
            let mut ask = RoundRequest::new(&req.target, self.lander(head)?, RUNNER);
            ask.expect_head = expect.or(head);
            ask.probes.clone_from(&probes);
            let done = self.repo.queue_round(&ask).map_err(error::op)?;
            let settled = expect.is_some() || done.head == head;
            round = Some(done);
            if settled {
                break;
            }
        }
        let round = round.ok_or_else(|| bad_arg("a queue round of no try"))?;

        // 3. The chain, ids in full.
        let mut out = QueueRoundResp::new(&req.target, Vec::new());
        out.head = round.head.map(|head| head.to_string());
        for link in round.items {
            let mut item = QueueRoundItem::new(
                link.entry.to_string(),
                link.candidate.to_string(),
                link.tree.to_string(),
            );
            item.verdict = link.verdict.map(verdict_out).transpose()?;
            item.cancelled = link.cancelled;
            out.items.push(item);
        }
        out.landed = round.landed.iter().map(ToString::to_string).collect();
        out.removed = round.removed.iter().map(ToString::to_string).collect();
        Ok(out)
    }

    fn record_verdict(&self, req: QueueVerdictReq) -> Result<(), OpError> {
        let entry = entry_id(&req.entry)?;
        let tree = TreeId::new(object_id(&req.tree)?);
        let state = verdict_in(req.state)?;
        let mut verdict = VerdictRequest::new(&req.target, entry, tree, state, &req.check, RUNNER);
        verdict.output = req.output.into_bytes();
        self.repo.queue_verdict(&verdict).map_err(error::op)?;
        Ok(())
    }

    fn publish(&self, req: QueuePublishReq) -> Result<QueuePublishResp, OpError> {
        // 1. The queue now: the head and every entry named as the round saw them, or nothing.
        let head = changeset_id(&req.head)?;
        let fold = self.repo.queue_fold(&req.target).map_err(error::op)?;
        let named = (req.landed.iter()).chain(req.removed.iter().map(|removal| &removal.entry));
        let mut open = Vec::new();
        for id in named {
            let id = entry_id(id)?;
            match fold.entry(id).filter(|entry| entry.is_open()) {
                Some(entry) => open.push((id, entry.version)),
                None => return self.moved(&req.target),
            }
        }
        if fold.head != Some(head) {
            return self.moved(&req.target);
        }

        // 2. The green prefix: the target's move and its Landed facts, in one frame.
        let mut op = None;
        if let Some(tip) = &req.tip {
            let mut exact = ExactPublish::new(&req.target, head, changeset_id(tip)?, RUNNER);
            exact.landed = open[..req.landed.len()].to_vec();
            match self.repo.publish_exact(&exact).map_err(error::op)? {
                ExactPublished::Published { op: seq, .. } => op = Some(seq),
                ExactPublished::Moved { .. } | ExactPublished::Mismatch { .. } => {
                    return self.moved(&req.target);
                }
                _ => return Err(error::unknown_variant("ExactPublished")),
            }
        }

        // 3. The removals, each at the version the queue read; one passed since is left as is.
        for (removal, &(id, version)) in req.removed.iter().zip(&open[req.landed.len()..]) {
            let written = self.remove(&req.target, removal, id, version)?;
            op = written.or(op);
        }
        let mut out = QueuePublishResp::new(QueuePublishStatus::Published);
        out.head = self.target_head(&req.target)?;
        out.op = op;
        Ok(out)
    }
}

impl LocalOps {
    /// The signature of a runner's chain on `head`: the runner's, at `head`'s committer time
    /// (0 when the target is gone), so that two rounds on one head build the same changesets.
    fn lander(&self, head: Option<ChangesetId>) -> Result<swarmops_core::Signature, OpError> {
        let mut secs = 0;
        if let Some(head) = head {
            let read = self.repo.snapshot().map_err(error::op)?;
            let stored = read.changeset(head).map_err(error::op)?;
            secs = stored.map_or(0, |stored| stored.changeset.committer.secs);
        }
        let runner = Identity::parse(RUNNER)?;
        self.native_signature(&runner, Some(secs))
    }

    /// Removes entry `id` of `target` at `version` as `removal` says: the removal's op, or
    /// `None` when the entry's version was passed and nothing was written.
    fn remove(
        &self,
        target: &str,
        removal: &QueueRemovalIn,
        id: EntryId,
        version: u32,
    ) -> Result<Option<u64>, OpError> {
        let reason = removal_in(removal.reason)?;
        let mut req = RemoveRequest::new(target, id, version, reason, RUNNER);
        req.check.clone_from(&removal.check);
        req.tree = removal
            .tree
            .as_deref()
            .map(object_id)
            .transpose()?
            .map(TreeId::new);
        match self.repo.queue_remove(&req).map_err(error::op)? {
            repo_queue::Written::Recorded { op, .. } => Ok(Some(op)),
            repo_queue::Written::Mismatch => Ok(None),
            _ => Err(error::unknown_variant("Written")),
        }
    }

    /// A publish that recorded nothing: the round's head or entries moved.
    fn moved(&self, target: &str) -> Result<QueuePublishResp, OpError> {
        let mut out = QueuePublishResp::new(QueuePublishStatus::Moved);
        out.head = self.target_head(target)?;
        Ok(out)
    }

    /// What `target` names now, in full; `None` when it is gone.
    fn target_head(&self, target: &str) -> Result<Option<String>, OpError> {
        let state = self.repo.get_ref(target).map_err(error::op)?;
        Ok(state.map(|state| state.target.to_string()))
    }
}

/// Entry `entry` of `fold` as `queue_status` lists it: its place among the open entries, and
/// the verdict on its candidate's tree (or the tree its removal names).
fn entry_out(fold: &QueueFold, entry: &Entry) -> Result<QueueEntryOut, OpError> {
    // 1. The entry's own fields.
    let state = match entry.state {
        EntryState::Waiting => QueueEntryState::Waiting,
        EntryState::Checking => QueueEntryState::Checking,
        EntryState::Landed => QueueEntryState::Landed,
        EntryState::Removed => QueueEntryState::Removed,
        _ => return Err(error::unknown_variant("EntryState")),
    };
    let mut out = QueueEntryOut::new(
        entry.id.to_string(),
        &entry.target,
        entry.head.to_string(),
        entry.base.to_string(),
        state,
    );
    out.key.clone_from(&entry.key);
    out.landed = entry.landed.map(|landed| landed.to_string());

    // 2. Its place in line, while open.
    if entry.is_open() {
        let place = fold.open().position(|open| open.id == entry.id);
        out.position = place.map(|at| u64::try_from(at).unwrap_or(u64::MAX).saturating_add(1));
    }

    // 3. Its candidate, the verdict on it, and its removal.
    out.candidate = entry.candidate.map(|c| c.candidate.to_string());
    let removal = entry.removal.as_ref();
    let tree = removal
        .and_then(|removal| removal.tree)
        .or(entry.candidate.map(|c| c.tree));
    let verdict = tree.and_then(|tree| fold.verdict(tree));
    out.tree = tree.map(|tree| tree.to_string());
    out.verdict = verdict.map(|v| verdict_out(v.state)).transpose()?;
    out.check = (removal.and_then(|r| r.check.clone())).or(verdict.map(|v| v.check.clone()));
    if let Some(removal) = removal {
        out.removal = Some(removal_out(removal.reason)?);
        let output = verdict.and_then(|v| v.output);
        out.report = removal.report.or(output).map(|blob| blob.to_string());
    }
    Ok(out)
}

/// The API's verdict state for the repository's `state`.
fn verdict_out(state: repo_queue::VerdictState) -> Result<VerdictState, OpError> {
    Ok(match state {
        repo_queue::VerdictState::Passed => VerdictState::Passed,
        repo_queue::VerdictState::Failed => VerdictState::Failed,
        repo_queue::VerdictState::TimedOut => VerdictState::TimedOut,
        repo_queue::VerdictState::Cancelled => VerdictState::Cancelled,
        _ => return Err(error::unknown_variant("VerdictState")),
    })
}

/// The repository's verdict state for the API's `state`.
fn verdict_in(state: VerdictState) -> Result<repo_queue::VerdictState, OpError> {
    Ok(match state {
        VerdictState::Passed => repo_queue::VerdictState::Passed,
        VerdictState::Failed => repo_queue::VerdictState::Failed,
        VerdictState::TimedOut => repo_queue::VerdictState::TimedOut,
        VerdictState::Cancelled => repo_queue::VerdictState::Cancelled,
        _ => return Err(error::unknown_variant("VerdictState")),
    })
}

/// The API's removal reason for the repository's `reason`.
fn removal_out(reason: repo_queue::RemovalReason) -> Result<RemovalReason, OpError> {
    Ok(match reason {
        repo_queue::RemovalReason::CheckFailed => RemovalReason::CheckFailed,
        repo_queue::RemovalReason::Conflict => RemovalReason::Conflict,
        repo_queue::RemovalReason::TimedOut => RemovalReason::TimedOut,
        repo_queue::RemovalReason::Withdrawn => RemovalReason::Withdrawn,
        repo_queue::RemovalReason::TargetGone => RemovalReason::TargetGone,
        repo_queue::RemovalReason::Cancelled => RemovalReason::Cancelled,
        _ => return Err(error::unknown_variant("RemovalReason")),
    })
}

/// The repository's removal reason for the API's `reason`.
fn removal_in(reason: RemovalReason) -> Result<repo_queue::RemovalReason, OpError> {
    Ok(match reason {
        RemovalReason::CheckFailed => repo_queue::RemovalReason::CheckFailed,
        RemovalReason::Conflict => repo_queue::RemovalReason::Conflict,
        RemovalReason::TimedOut => repo_queue::RemovalReason::TimedOut,
        RemovalReason::Withdrawn => repo_queue::RemovalReason::Withdrawn,
        RemovalReason::TargetGone => repo_queue::RemovalReason::TargetGone,
        RemovalReason::Cancelled => repo_queue::RemovalReason::Cancelled,
        _ => return Err(error::unknown_variant("RemovalReason")),
    })
}

/// Entry id `hex`: 64 hex digits, else `E_INVALID_ARG`.
fn entry_id(hex: &str) -> Result<EntryId, OpError> {
    EntryId::from_hex(hex).ok_or_else(|| {
        bad_arg(format!("queue entry {hex:?}: expected 64 hex digits"))
            .with_details(error::details([("entry", hex.into())]))
    })
}

/// Changeset id `hex`, in full, else `E_INVALID_ARG`.
fn changeset_id(hex: &str) -> Result<ChangesetId, OpError> {
    object_id(hex).map(ChangesetId::new)
}

/// Object id `hex`, in full, else `E_INVALID_ARG`.
fn object_id(hex: &str) -> Result<ObjectId, OpError> {
    ObjectId::from_hex(hex).map_err(|_| bad_arg(format!("{hex:?}: expected 64 hex digits")))
}

/// `E_INVALID_ARG` saying `message`.
fn bad_arg(message: impl Into<String>) -> OpError {
    error::error(ErrorCode::InvalidArg, message)
}

/// A lane's store error, mapped as the repository's.
fn store_error(e: swarmops_store::StoreError) -> OpError {
    error::op(RepoError::Store(e))
}
