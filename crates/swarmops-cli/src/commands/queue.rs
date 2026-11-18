//! The `queue` commands (the M22 core design §6): a target's landing queue from the command line
//! and from `swarmops mcp`'s `queue_*` tools, which share the functions here.
//!
//! ```text
//!  enqueue ─▶ ws::queue::enqueue (no check: refused) ─▶ entry durable ─┐
//!  status, withdraw, wait ─▶ the store's queue ────────────────────────┤
//!                                                                      ▼
//!                     nudge: an open entry and a free lane? ─▶ detached `queue run REF`
//!  run ─▶ ws::queue::run on the store's LocalOps as its QueueDriver, checks in <store>/queue/
//! ```
//!
//! A runner starts only after the entry it is for is durable, and any queue command that finds
//! work with no runner starts one (I-6): a runner killed mid-round leaves its queue to the next
//! command, with no daemon.

use std::os::unix::process::CommandExt as _;
use std::process::{Command as Process, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use camino::Utf8Path;
use serde_json::{Map, Value};
use swarmops_api::{
    ErrorCode, Identity, OpError, QueueEnqueueReq, QueueEnqueueResp, QueueEntryOut,
    QueueEntryState, QueueStatusReq, QueueStatusResp, QueueWaitReq, QueueWithdrawReq,
    RemovalReason, StoreOps,
};
use swarmops_ops::LocalOps;
use swarmops_ops::identity::require_identity;
use swarmops_ws::QueueDriver;
use swarmops_ws::queue::RunReq;

use super::{Ctx, ws};
use crate::cli::QueueCommand;
use crate::context;
use crate::output::{self, Printed};

/// How long `queue wait` sleeps between two looks at its entry (the prototype's poll).
const WAIT_POLL: Duration = Duration::from_secs(2);

/// The directory, in the store's root, that a runner makes its checks' workspaces in.
const RUNNER_DIR: &str = "queue";

/// The identity a runner makes its checks' workspaces as: the store's own queue runner, which
/// signs its chains and facts too.
const RUNNER: &str = "swarmops/queue-runner";

/// Runs a `queue` subcommand, which names operation `op`.
pub(super) fn run(ctx: &Ctx<'_>, op: &'static str, command: QueueCommand) -> Printed {
    let agent = ctx.global.agent.as_deref();
    match command {
        QueueCommand::Enqueue {
            target,
            head,
            base,
            key,
        } => output::render(
            &ctx.envelope_at_root(
                op,
                || {
                    let who = context::mutator(agent, ctx.env)?;
                    let mut req = QueueEnqueueReq::new(target, head);
                    req.base = base.map(Into::into);
                    req.key = key;
                    Ok((who, req))
                },
                |ops, root, (who, req)| enqueue(&ops, root, Some(&who), req),
            ),
            ctx.format,
        ),
        QueueCommand::Status { target, entry } => output::render(
            &ctx.envelope_at_root(
                op,
                || {
                    let mut req = QueueStatusReq::new();
                    req.target = target;
                    req.entry = entry;
                    Ok(req)
                },
                |ops, root, req| status(&ops, root, req),
            ),
            ctx.format,
        ),
        QueueCommand::Withdraw { entry } => output::render(
            &ctx.envelope_at_root(
                op,
                || {
                    Ok((
                        context::mutator(agent, ctx.env)?,
                        QueueWithdrawReq::new(entry),
                    ))
                },
                |ops, root, (who, req)| withdraw(&ops, root, Some(&who), req),
            ),
            ctx.format,
        ),
        QueueCommand::Wait { entry, timeout } => output::render(
            &ctx.envelope_at_root(
                op,
                || {
                    let mut req = QueueWaitReq::new(entry);
                    req.timeout_s = timeout;
                    Ok(req)
                },
                |ops, root, req| wait(&ops, root, req),
            ),
            ctx.format,
        ),
        QueueCommand::Run {
            target,
            inflight,
            check_timeout,
        } => output::render(
            &ctx.envelope_at_root(
                op,
                || Ok(()),
                |ops, root, ()| {
                    let mut req = RunReq::new(target, RUNNER_DIR);
                    req.inflight = inflight;
                    req.check_timeout = Duration::from_secs(check_timeout);
                    drain(&ops, root, req)
                },
            ),
            ctx.format,
        ),
    }
}

/// `queue_enqueue` as `who`: refused by the workspace layer when the target's head names no
/// check; once the entry is durable, a runner started for its target when none runs.
pub(crate) fn enqueue(
    ops: &dyn StoreOps,
    root: &Utf8Path,
    who: Option<&Identity>,
    req: QueueEnqueueReq,
) -> Result<QueueEnqueueResp, OpError> {
    // 1. The entry, durable when this returns.
    let wctx = ws::context(ops, root, who);
    let done = swarmops_ws::queue::enqueue(&wctx, req)?;

    // 2. Its runner, after the entry (I-6).
    nudge(ops, root, &done.entry.target);
    Ok(done)
}

/// `queue_status`, and a runner started for each target listed with open entries and no
/// runner.
pub(crate) fn status(
    ops: &dyn StoreOps,
    root: &Utf8Path,
    req: QueueStatusReq,
) -> Result<QueueStatusResp, OpError> {
    // 1. The entries.
    let listed = ops.queue_status(req)?;

    // 2. Each target with work, once.
    let mut targets: Vec<&str> = (listed.entries.iter())
        .filter(|entry| is_open(entry))
        .map(|entry| entry.target.as_str())
        .collect();
    targets.sort_unstable();
    targets.dedup();
    for target in targets {
        nudge(ops, root, target);
    }
    Ok(listed)
}

/// `queue_withdraw` as `who`, which needs an identity; then the target's runner started when
/// its queue still has work and none runs.
pub(crate) fn withdraw(
    ops: &dyn StoreOps,
    root: &Utf8Path,
    who: Option<&Identity>,
    req: QueueWithdrawReq,
) -> Result<QueueEntryOut, OpError> {
    let who = require_identity(who.cloned())?;
    let entry = ops.queue_withdraw(&who, req)?;
    nudge(ops, root, &entry.target);
    Ok(entry)
}

/// `queue_wait`: looks at the entry every [`WAIT_POLL`], starting its runner when none runs,
/// until it lands (the answer), is removed (its removal's error) or `req.timeout_s` passes
/// (`E_QUEUE_WAIT_TIMEOUT`).
pub(crate) fn wait(
    ops: &dyn StoreOps,
    root: &Utf8Path,
    req: QueueWaitReq,
) -> Result<QueueEntryOut, OpError> {
    let deadline = req
        .timeout_s
        .map(|secs| Instant::now() + Duration::from_secs(secs));
    loop {
        // 1. Where it stands: landed and removed end the wait.
        let entry = one(ops, &req.entry)?;
        match entry.state {
            QueueEntryState::Landed => return Ok(entry),
            QueueEntryState::Removed => return Err(removed(&entry)),
            _ => {}
        }

        // 2. Open: a runner for it, unless one runs.
        nudge(ops, root, &entry.target);

        // 3. Its time, or the next look.
        let left = deadline.map(|at| at.saturating_duration_since(Instant::now()));
        if left == Some(Duration::ZERO) {
            return Err(timed_out(&entry));
        }
        thread::sleep(left.map_or(WAIT_POLL, |left| left.min(WAIT_POLL)));
    }
}

/// `queue run`: drains `req.target`'s queue with the store's [`LocalOps`] as its driver, then
/// lists that queue as it left it. Returns at once when another runner holds the lane. It needs
/// no identity: its runner acts as the store's own, [`RUNNER`].
fn drain(ops: &dyn StoreOps, root: &Utf8Path, mut req: RunReq) -> Result<QueueStatusResp, OpError> {
    // 1. The lane's side of the store, beside the recorded operations.
    let local = LocalOps::open(root)?;
    let runner = Identity::parse(RUNNER)?;
    let wctx = ws::context(ops, root, Some(&runner)).with_queue(&local);
    req.dir = wctx.store_root.join(RUNNER_DIR);

    // 2. The rounds, then the queue.
    swarmops_ws::queue::run(&wctx, &req)?;
    let mut list = QueueStatusReq::new();
    list.target = Some(req.target);
    ops.queue_status(list)
}

/// Starts `queue run <target>` on the store at `root`, detached, when `target`'s queue has an
/// open entry and no runner holds its lane. Best effort: the next queue command looks again.
fn nudge(ops: &dyn StoreOps, root: &Utf8Path, target: &str) {
    // 1. Work, and a free lane.
    let mut req = QueueStatusReq::new();
    req.target = Some(target.to_owned());
    let has_work = ops
        .queue_status(req)
        .is_ok_and(|listed| listed.entries.iter().any(is_open));
    if !has_work {
        return;
    }
    let free = LocalOps::open(root).and_then(|local| local.lane_free(target));
    if !matches!(free, Ok(true)) {
        return;
    }

    // 2. This program as the runner: a process group of its own, no stdio to wait on.
    let Ok(program) = std::env::current_exe() else {
        return;
    };
    let spawned = Process::new(program)
        .arg(format!("--repo={root}"))
        .args(["queue", "run", target])
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();

    // 3. Reaped when it ends, so a long-lived caller (`mcp`) keeps no zombie.
    if let Ok(mut child) = spawned {
        thread::spawn(move || child.wait());
    }
}

/// Whether `entry` is still in its queue.
fn is_open(entry: &QueueEntryOut) -> bool {
    matches!(
        entry.state,
        QueueEntryState::Waiting | QueueEntryState::Checking
    )
}

/// The entry whose id is `id`: `E_INVALID_ARG` when no queue holds it.
fn one(ops: &dyn StoreOps, id: &str) -> Result<QueueEntryOut, OpError> {
    let mut req = QueueStatusReq::new();
    req.entry = Some(id.to_owned());
    let listed = ops.queue_status(req)?;
    listed.entries.into_iter().next().ok_or_else(|| {
        OpError::new(ErrorCode::InvalidArg, format!("no queue holds entry {id}")).with_details(
            Map::from_iter([
                ("reason".to_owned(), Value::from("unknown_entry")),
                ("entry".to_owned(), Value::from(id)),
            ]),
        )
    })
}

/// `E_QUEUE_WAIT_TIMEOUT` for `entry`, still open.
fn timed_out(entry: &QueueEntryOut) -> OpError {
    OpError::new(
        ErrorCode::QueueWaitTimeout,
        format!("entry {} is still queued", entry.id),
    )
    .with_details(Map::from_iter([
        ("entry".to_owned(), Value::from(entry.id.as_str())),
        ("state".to_owned(), to_value(entry.state)),
    ]))
    .with_hint(format!("swarmops queue wait {}", entry.id))
}

/// The error of `entry`, removed: its code by its reason, and the removal in `details`.
///
/// ```text
///  check_failed, timed_out, cancelled  E_CHECK_FAILED
///  conflict                            E_CONFLICT
///  target_gone                         E_REF_NOT_FOUND
///  withdrawn                           E_NOTHING_TO_LAND
/// ```
fn removed(entry: &QueueEntryOut) -> OpError {
    // 1. The code.
    let code = match entry.removal {
        Some(RemovalReason::Conflict) => ErrorCode::Conflict,
        Some(RemovalReason::TargetGone) => ErrorCode::RefNotFound,
        Some(RemovalReason::Withdrawn) => ErrorCode::NothingToLand,
        _ => ErrorCode::CheckFailed,
    };

    // 2. What the removal names.
    let mut details = Map::from_iter([
        ("entry".to_owned(), Value::from(entry.id.as_str())),
        ("target".to_owned(), Value::from(entry.target.as_str())),
        ("reason".to_owned(), to_value(entry.removal)),
    ]);
    for (key, value) in [
        ("tree", &entry.tree),
        ("check", &entry.check),
        ("report", &entry.report),
    ] {
        if let Some(value) = value {
            details.insert(key.to_owned(), Value::from(value.as_str()));
        }
    }
    OpError::new(
        code,
        format!("entry {} was removed from its queue", entry.id),
    )
    .with_details(details)
}

/// `value` as the JSON the API writes it in.
fn to_value<T: serde::Serialize>(value: T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}
