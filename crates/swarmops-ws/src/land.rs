//! `ws land` (§5.4 of the M5 design): without a check, a land of the workspace's chain; with
//! one, rebase, check and an exact land, up to `max_attempts`, under one land key.
//!
//! ```text
//!  refusals before the lock: no command for --check (E_INVALID_ARG missing "check")
//!  lock → edits committed (-m) or E_WS_DIRTY → the land key K
//!
//!  without --check (mode merge by default):
//!    pending {K, landing} → land{head, base, key: K[, expect_target: onto]} → files to L
//!
//!  with --check (mode exact by default), attempt n = 1..max_attempts:
//!    pending {K, rebase_key R_n, rebasing} → rebase{key: R_n} → H' on T, files moved
//!      conflict → E_CONFLICT, the workspace rebasing
//!    check in the workspace (SWARMOPS_CHECK_TREE = H'.root) → failed: E_CHECK_FAILED, on H'
//!    pending {K, R_n, landing} → land{head: H', base: T, expect_target: T, key: K}
//!      landed or replayed → base = head = L (L.root == H'.root, no file moves)
//!      E_CAS_MISMATCH → the land's backoff, attempt n + 1
//!  exhausted → E_CAS_RETRY_EXHAUSTED
//! ```
//!
//! A land cut short keeps its pending record: a re-run takes its keys, so the store replays the
//! same `H'` (the attempt's rebase key) and the same land (`K`); one cut after its land's frame
//! goes straight to that land, which the store answers `replayed`.

use std::collections::BTreeSet;
use std::time::Duration;

use serde_json::{Map, Value};
use swarmops_api::{
    DEFAULT_MAX_CONFLICT_BYTES, ErrorCode, LandMode, LandReq, LandResp, MAX_LAND_ATTEMPTS, OpError,
    StatusAfter, WsCheckInput, WsLandReq, WsLandResp,
};

use crate::check::{self, Subject};
use crate::io_error::unknown_variant;
use crate::pending::{self, Pending, PendingOp};
use crate::rebase;
use crate::state::State;
use crate::workspace::{self, Locate, Workspace};
use crate::{Ctx, after};

/// The pending record's state before an attempt's rebase.
const REBASING: &str = "rebasing";

/// The pending record's state before the store's land.
const LANDING: &str = "landing";

/// The attempt from which a land waits before trying again (operations spec §3 step 7).
const FIRST_WAIT: u32 = 4;

/// The wait's step: 25 ms, doubled each attempt.
const WAIT_STEP: Duration = Duration::from_millis(25);

/// The longest wait's step: 2 s.
const WAIT_MAX: Duration = Duration::from_secs(2);

/// A checked land's command and how long it may run.
struct Check {
    /// The command; none: the repository's, read at each rebased changeset.
    argv: Option<Vec<String>>,
    /// Its timeout.
    timeout: Duration,
}

/// Whether a land's pending record is on disk before its store land.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Record {
    /// Not yet: the land writes it.
    Due,
    /// Already, by a run cut short.
    Written,
}

/// One `ws land`: the workspace under its lock and what the request fixed.
struct Landing<'c, 'a> {
    /// What the operation runs with.
    ctx: &'c Ctx<'a>,
    /// The workspace.
    ws: Workspace,
    /// The land key `K`, fixed for the whole command.
    key: String,
    /// The ref it lands on.
    target: String,
    /// The landed changeset's message.
    message: Option<String>,
    /// Merge, or exact.
    mode: LandMode,
    /// The attempts a checked land makes; the store's own for an unchecked merge.
    max_attempts: u32,
}

/// `ws_land` in the workspace `req` names.
pub fn land(ctx: &Ctx<'_>, req: WsLandReq) -> Result<WsLandResp, OpError> {
    // 1. The request, and the refusals that need no lock.
    let WsLandReq {
        workspace,
        message,
        idempotency_key,
        target,
        check,
        mode,
        max_attempts,
        status_after,
        ..
    } = req;
    if !(1..=MAX_LAND_ATTEMPTS).contains(&max_attempts) {
        return Err(OpError::new(
            ErrorCode::InvalidArg,
            format!("max_attempts is 1 to {MAX_LAND_ATTEMPTS}, not {max_attempts}"),
        ));
    }
    let mode = mode.unwrap_or(match check {
        Some(_) => LandMode::Exact,
        None => LandMode::Merge,
    });
    let seen = Workspace::open(ctx, Locate::Name(workspace.clone()))?;
    let target = target.unwrap_or_else(|| seen.file.target.clone());
    let check = check
        .map(|check| command(ctx, &seen, &target, check))
        .transpose()?;

    // 2. The workspace under its lock, not rebasing.
    let (mut ws, _held) = workspace::open_locked(ctx, &Locate::Name(workspace))?;
    if let State::Rebasing { .. } = ws.file.state {
        return Err(OpError::new(
            ErrorCode::UnresolvedConflicts,
            "the workspace is rebasing: finish the rebase before landing",
        )
        .with_hint("swarmops ws rebase --continue, or swarmops ws rebase --abort"));
    }

    // 3. A land cut short: its keys; else the edits settled and something to land.
    let cut = cut_land(ctx, &ws)?;
    if cut.is_none() {
        rebase::settle_edits(ctx, &mut ws, message.clone(), &BTreeSet::new())?;
        if ws.file.head == ws.file.base {
            return Err(OpError::new(
                ErrorCode::NothingToLand,
                "the workspace's head is its base: there is nothing to land",
            ));
        }
    }

    // 4. The land, checked or not; then the status after it.
    let key = (cut.as_ref().map(|cut| cut.key.clone()))
        .or(idempotency_key)
        .unwrap_or_else(rebase::new_key);
    let mut landing = Landing {
        ctx,
        ws,
        key,
        target,
        message,
        mode,
        max_attempts,
    };
    let record = if cut.is_some() {
        Record::Written
    } else {
        Record::Due
    };
    let (land, attempts) = match check {
        Some(check) => landing.checked(&check, cut)?,
        None => (landing.once(record)?, 1),
    };
    let mut out = WsLandResp::new(land, attempts);
    out.status_after = attached(&landing, status_after);
    Ok(out)
}

impl Landing<'_, '_> {
    /// The land without a check: one store land, exact onto the recorded `onto` or merged.
    /// `record`: whether its pending record is there already.
    fn once(&mut self, record: Record) -> Result<LandResp, OpError> {
        // 1. Exact: onto the target's head the last rebase merged onto, which a commit clears.
        let exact = self.mode == LandMode::Exact && self.ws.file.target == self.target;
        let expect = self.ws.file.onto.clone().filter(|_| exact);
        if self.mode == LandMode::Exact && expect.is_none() {
            return Err(no_rebase());
        }

        // 2. The pending record (WS1), the land, the workspace on what landed.
        if record == Record::Due {
            self.write(None, 1, LANDING)?;
        }
        self.publish(expect)
    }

    /// The land with a check: rebase, check, exact land, again while the target moves. `cut`:
    /// the record of a run cut short, resumed at its attempt.
    fn checked(&mut self, check: &Check, cut: Option<Pending>) -> Result<(LandResp, u32), OpError> {
        let mut attempt = cut.as_ref().map_or(1, |cut| cut.attempt);
        let mut cut = cut;
        loop {
            // 1. Out of attempts.
            if attempt > self.max_attempts {
                pending::remove_pending(self.ctx.fs.as_ref(), &self.ws.dir)?;
                return Err(exhausted(attempt - 1));
            }

            // 2. Rebased and checked, unless a run cut short got past its check.
            let landing = (cut.as_ref()).is_some_and(|cut| cut.state == LANDING)
                && self.ws.file.onto.is_some();
            if !landing {
                let rebase_key = (cut.as_ref().and_then(|cut| cut.rebase_key.clone()))
                    .unwrap_or_else(rebase::new_key);
                self.rebase_and_check(check, &rebase_key, attempt)?;
            }
            cut = None;

            // 3. The land onto the rebased target, exact unless asked to merge.
            let expect = match self.mode {
                LandMode::Exact => self.ws.file.onto.clone(),
                LandMode::Merge => None,
                _ => return Err(unknown_variant("LandMode")),
            };
            match self.publish(expect) {
                Ok(done) => return Ok((done, attempt)),
                Err(e) if e.code == ErrorCode::CasMismatch.as_str() => {
                    attempt += 1;
                    std::thread::sleep(backoff(attempt, self.key.as_bytes()));
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Attempt `attempt`'s rebase, under `rebase_key`, and its check on the rebased changeset;
    /// then the pending record says the land is next.
    fn rebase_and_check(
        &mut self,
        check: &Check,
        rebase_key: &str,
        attempt: u32,
    ) -> Result<(), OpError> {
        // 1. The rebase, its key durable first (WS1).
        let ctx = self.ctx;
        self.write(Some(rebase_key), attempt, REBASING)?;
        let target = self.target.clone();
        let done = rebase::rebase_in(ctx, &mut self.ws, &target, rebase_key, None);
        let done = rebase::finish_call(ctx, &self.ws, done)?;

        // 2. The check on it: the request's command, else the repository's there.
        let argv = match &check.argv {
            Some(argv) => Ok(argv.clone()),
            None => check::configured(ctx.store, &done.changeset)?.ok_or_else(check::missing),
        };
        let subject = Subject {
            changeset: &done.changeset,
            tree: &done.root,
            attempt,
        };
        let ran = argv.and_then(|argv| check::run(&self.ws, &argv, &subject, check.timeout));
        rebase::finish_call(ctx, &self.ws, ran)?;

        // 3. The land next, from the rebased head.
        self.write(Some(rebase_key), attempt, LANDING)
    }

    /// The store's land of the workspace's chain with the key, onto `expect` if given; then the
    /// workspace on what landed and the pending record gone. A refusal takes the record too.
    fn publish(&mut self, expect: Option<String>) -> Result<LandResp, OpError> {
        // 1. The land.
        let ctx = self.ctx;
        let mut req = LandReq::new(self.ws.file.head.as_str());
        req.base = Some(self.ws.file.base.as_str().into());
        req.target.clone_from(&self.target);
        req.message.clone_from(&self.message);
        req.idempotency_key = Some(self.key.clone());
        req.max_attempts = self.max_attempts;
        req.max_conflict_bytes = DEFAULT_MAX_CONFLICT_BYTES;
        req.expect_target = expect.map(Into::into);
        let done = ctx.store.land(&self.ws.who, req);
        let done = rebase::finish_call(ctx, &self.ws, done)?;

        // 2. The workspace on it: files, then workspace.json (WS2); the record goes.
        let ws = &mut self.ws;
        ws.file.head.clone_from(&done.landed);
        ws.file.base.clone_from(&done.landed);
        ws.file.onto = None;
        ws.file.target.clone_from(&self.target);
        rebase::move_to(ctx, ws, &done.landed)?;
        pending::remove_pending(ctx.fs.as_ref(), &ws.dir)?;
        Ok(done)
    }

    /// Makes the pending record durable: the land key, the attempt's rebase key, in `state`.
    fn write(&self, rebase_key: Option<&str>, attempt: u32, state: &str) -> Result<(), OpError> {
        let mut record = rebase::record(
            PendingOp::Land,
            &self.key,
            rebase_key,
            &self.ws,
            &self.target,
            state,
        );
        record.attempt = attempt;
        pending::write_pending(self.ctx.fs.as_ref(), &self.ws.dir, &record)
    }
}

/// The status after the land, as `want` asks.
fn attached(landing: &Landing<'_, '_>, want: StatusAfter) -> Option<swarmops_api::WsStatus> {
    after::attach(landing.ctx, &landing.ws, want)
}

/// The record of a `ws land` cut short on workspace `ws`'s head, in either state.
fn cut_land(ctx: &Ctx<'_>, ws: &Workspace) -> Result<Option<Pending>, OpError> {
    let found = pending::read_pending(ctx.fs.as_ref(), &ws.dir)?;
    let state = match &found {
        Some(cut) if cut.op == PendingOp::Land => cut.state.clone(),
        _ => LANDING.to_owned(),
    };
    rebase::cut_short(ctx, ws, PendingOp::Land, &state)
}

/// The check `input` asks for, of workspace `seen` landing on `target`: its command, or the
/// repository's, which must exist (on the target or the workspace's head) before anything is
/// rebased or landed.
fn command(
    ctx: &Ctx<'_>,
    seen: &Workspace,
    target: &str,
    input: WsCheckInput,
) -> Result<Check, OpError> {
    // 1. The command given.
    let timeout = Duration::from_secs(input.timeout_s);
    if let Some(argv) = input.argv {
        if argv.is_empty() {
            return Err(check::missing());
        }
        return Ok(Check {
            argv: Some(argv),
            timeout,
        });
    }

    // 2. Else the repository's, there now.
    let known = check::configured(ctx.store, target)?.is_some()
        || check::configured(ctx.store, &seen.file.head)?.is_some();
    if !known {
        return Err(check::missing());
    }
    Ok(Check {
        argv: None,
        timeout,
    })
}

/// How long a land waits before its attempt `attempt`, counted from 1, as the store's land
/// waits (operations spec §3 step 7): nothing before the fourth; from it,
/// `min(2 s, 25 ms·2^attempt)·U(0.5, 1.5)`, `U` drawn from the BLAKE3 hash of `seed` (the land
/// key) and the attempt's number.
fn backoff(attempt: u32, seed: &[u8]) -> Duration {
    // 1. None for the first attempts.
    if attempt < FIRST_WAIT {
        return Duration::ZERO;
    }

    // 2. The step, then its draw.
    let step = WAIT_STEP
        .saturating_mul(1_u32 << attempt.min(16))
        .min(WAIT_MAX);
    let mut hasher = blake3::Hasher::new();
    hasher.update(seed);
    hasher.update(&attempt.to_le_bytes());
    let [a, b, c, d, ..] = *hasher.finalize().as_bytes();
    let unit = f64::from(u32::from_le_bytes([a, b, c, d])) / 4_294_967_296.0;
    step.mul_f64(0.5 + unit)
}

/// `E_INVALID_ARG{details.missing: "rebase"}`: an exact land of a workspace not rebased onto
/// its target since its last commit.
fn no_rebase() -> OpError {
    OpError::new(
        ErrorCode::InvalidArg,
        "an exact land needs the workspace rebased onto its target since its last commit",
    )
    .with_details(Map::from_iter([(
        "missing".to_owned(),
        Value::from("rebase"),
    )]))
    .with_hint("swarmops ws rebase, check the workspace, then swarmops ws land --mode exact")
}

/// `E_CAS_RETRY_EXHAUSTED` after `attempts` attempts, each lost to a moved target.
fn exhausted(attempts: u32) -> OpError {
    OpError::new(
        ErrorCode::CasRetryExhausted,
        format!("the target moved during each of {attempts} attempts: nothing landed"),
    )
    .with_details(Map::from_iter([(
        "attempts".to_owned(),
        Value::from(attempts),
    )]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No wait before the fourth attempt; from it, within half and one and a half of the step,
    /// and the same for the same seed.
    #[test]
    fn the_backoff_is_the_lands() {
        assert_eq!(backoff(3, b"k"), Duration::ZERO);
        let wait = backoff(4, b"k");
        assert!(wait >= Duration::from_millis(200) && wait <= Duration::from_millis(600));
        assert_eq!(wait, backoff(4, b"k"));
        assert!(backoff(20, b"k") <= Duration::from_secs(3));
    }
}
