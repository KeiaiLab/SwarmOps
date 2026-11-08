//! `ws land` (§5.4 of the M5 design): without a check, a land of the workspace's chain; with
//! one, rebase, check and an exact land, up to `max_attempts`, under one land key.
//!
//! ```text
//!  refusals before the lock: an edit off a sparse workspace's prefixes
//!    (E_WS_OUTSIDE_PREFIXES, before any store call but the workspace's open), no command for
//!    --check (E_INVALID_ARG missing "check")
//!  lock → a base the workspace's own target left, whatever target it lands on (E_WS_STALE,
//!         nothing written)
//!       → names the target gained that the files cannot hold (E_WS_UNPORTABLE, nothing
//!         written), unless the store only replays a land the target holds
//!       → edits carried (-m without --check), committed (-m with it) or E_WS_DIRTY → the
//!         land key K
//!
//!  without --check (mode merge by default):
//!    pending {K, landing} → land{head, base, key: K[, expect_target: onto][, changes: the
//!    edits]} → the workspace on C, the edits' changeset, when it carried them → files to L
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
//! goes straight to that land, which the store answers `replayed`. A land that carried the edits
//! (the M16 design §4) and that its target holds is landed again from the changeset `C` it made
//! of them, with no changes: the store replays it, and edits made since stay on disk, edits
//! against what landed (I8). One its target does not hold carries the edits as they are now.

use std::collections::BTreeSet;
use std::time::Duration;

use serde_json::{Map, Value};
use swarmops_api::{
    DEFAULT_MAX_CONFLICT_BYTES, ErrorCode, LandMode, LandReq, LandResp, MAX_LAND_ATTEMPTS, OpError,
    OpLogReq, ResolveRevReq, ShowReq, StatusAfter, WsCheckInput, WsLandReq, WsLandResp,
};

use crate::check::{self, Subject};
use crate::commit::{self, Edits};
use crate::io_error::unknown_variant;
use crate::materialize;
use crate::pending::{self, Pending, PendingOp};
use crate::rebase::{self, Rerun};
use crate::scan;
use crate::state::{self, State};
use crate::status::{self, Landed, OP_CAP};
use crate::workspace::{self, Locate, Workspace};
use crate::{Ctx, after};

/// The pending record's state before an attempt's rebase.
pub(crate) const REBASING: &str = "rebasing";

/// The pending record's state before the store's land.
const LANDING: &str = "landing";

/// The attempt from which a land waits before trying again (operations spec §3 step 7).
const FIRST_WAIT: u32 = 4;

/// The wait's step: 25 ms, doubled each attempt.
const WAIT_STEP: Duration = Duration::from_millis(25);

/// The longest wait's step: 2 s.
const WAIT_MAX: Duration = Duration::from_secs(2);

/// The operations one `op_log` page of the search for a held land's changesets asks for.
const OP_PAGE: usize = 100;

/// The changesets per operation that search reads: a land that carried edits records two.
const OP_CHANGESETS: usize = 2;

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
    /// The edits the land carries as its changes (`-m` without `--check`); none when there are
    /// none, or the land commits them first.
    edits: Option<Edits>,
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
    commit::refuse_outside(scan::outside(ctx, &seen)?)?;
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

    // 3. A base the workspace's own target left: refused before anything is settled or landed,
    //    onto any target, which the land would bring the work taken back to.
    status::refuse_stale(ctx.store, &ws.file)?;

    // 4. Names the target gained that the files cannot hold, and those of the edits `-m`
    //    commits or carries: refused before the store lands, unless the store only replays a
    //    land cut short that the target holds.
    let rerun = match (&check, &message) {
        (None, Some(_)) => Rerun::Carries,
        _ => Rerun::Waits,
    };
    if !replays(ctx, &ws, &target)? {
        let head = ctx.store.resolve_rev(ResolveRevReq::new(target.as_str()))?;
        let edits = rebase::unsettled(ctx, &ws, message.as_deref(), PendingOp::Land, rerun)?;
        materialize::preflight(ctx, &ws, &head.id, &edits)?;
    }

    // 5. A land cut short: its keys. `-m` without `--check`: the edits carried, if any, the
    //    land's run again too (the M16 design §4), unless the target holds it: then the
    //    workspace stands on the `C` it made, and later edits stay on disk. Else the edits
    //    settled and something to land.
    let cut = cut_land(ctx, &ws)?;
    let held = match (&cut, rerun) {
        (Some(cut), Rerun::Carries) if ws.file.materialized == ws.file.head => {
            stand_on_held(ctx, &mut ws, cut, &target)?
        }
        _ => false,
    };
    let edits = match rerun {
        Rerun::Carries if !held && ws.file.materialized == ws.file.head => commit::edits(ctx, &ws)?,
        _ => None,
    };
    if cut.is_none() && edits.is_none() {
        rebase::settle_edits(ctx, &mut ws, message.clone(), &BTreeSet::new())?;
        if ws.file.head == ws.file.base {
            return Err(OpError::new(
                ErrorCode::NothingToLand,
                "the workspace's head is its base: there is nothing to land",
            ));
        }
    }

    // 6. The land, checked or not; then the status after it.
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
        edits,
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
        // 1. Exact: onto the target's head the last rebase merged onto, which a commit clears,
        //    as edits carried would.
        let exact = self.mode == LandMode::Exact
            && self.ws.file.target == self.target
            && self.edits.is_none();
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
        // 1. The names the target holds that the files cannot: refused before the key is recorded.
        let ctx = self.ctx;
        let target = self.target.clone();
        rebase::check_onto(ctx, &self.ws, &target, &[])?;

        // 2. The rebase, its key durable first (WS1).
        self.write(Some(rebase_key), attempt, REBASING)?;
        let done = rebase::rebase_in(ctx, &mut self.ws, &target, rebase_key, None);
        let done = rebase::finish_call(ctx, &self.ws, done)?;

        // 3. The check on it: the request's command, else the repository's there.
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

        // 4. The land next, from the rebased head.
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
        let edits = self.edits.take();
        if let Some(edits) = &edits {
            req.changes = Some(edits.changes(ctx, &self.ws)?);
        }
        let done = ctx.store.land(&self.ws.who, req);
        let done = rebase::finish_call(ctx, &self.ws, done)?;

        // 2. Edits carried: the workspace on C, which holds them, as their commit leaves it; the
        //    record follows it there, so that a re-run cut short after this replays L from C.
        if let (Some(edits), Some(made)) = (edits, done.squashed_from.last()) {
            edits.settle(ctx, &mut self.ws, made)?;
            self.write(None, 1, LANDING)?;
        }

        // 3. The workspace on L, with the operation that moved the target there: files, then
        //    workspace.json (WS2); the record goes.
        let base_op = status::base_op(ctx.store, &self.target, &done.landed)?;
        let ws = &mut self.ws;
        ws.file.head.clone_from(&done.landed);
        ws.file.base.clone_from(&done.landed);
        ws.file.base_op = base_op;
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

/// Whether the record of a `ws land` cut short on workspace `ws`'s head names a land that
/// `target` holds, which the store then answers as a replay and writes nothing for.
fn replays(ctx: &Ctx<'_>, ws: &Workspace, target: &str) -> Result<bool, OpError> {
    let Some(cut) = pending::read_pending(ctx.fs.as_ref(), &ws.dir)? else {
        return Ok(false);
    };
    if cut.op != PendingOp::Land || cut.head != ws.file.head {
        return Ok(false);
    }
    Ok(status::landed(ctx.store, target, &cut.key, &cut.head)? == Landed::Held)
}

/// The record of a `ws land` cut short on workspace `ws`'s head, in either state; one a land
/// that carried edits left on the head before them is taken onto `ws`'s head ([`trailing`]).
fn cut_land(ctx: &Ctx<'_>, ws: &Workspace) -> Result<Option<Pending>, OpError> {
    let found = pending::read_pending(ctx.fs.as_ref(), &ws.dir)?;
    if let Some(cut) = &found
        && trailing(ctx, ws, cut)?
    {
        let mut cut = cut.clone();
        cut.head.clone_from(&ws.file.head);
        pending::write_pending(ctx.fs.as_ref(), &ws.dir, &cut)?;
        return Ok(Some(cut));
    }
    let state = match &found {
        Some(cut) if cut.op == PendingOp::Land => cut.state.clone(),
        _ => LANDING.to_owned(),
    };
    rebase::cut_short(ctx, ws, PendingOp::Land, &state)
}

/// Whether `cut` is the record of a land that carried edits, cut once the workspace stood on
/// `C` and before the record followed it there (the M16 design §4): a land record on the
/// workspace's head's parent, the files standing for the head, and the record's key landed on
/// its target. Its re-run lands the head with that key: the store's replay.
fn trailing(ctx: &Ctx<'_>, ws: &Workspace, cut: &Pending) -> Result<bool, OpError> {
    // 1. A land record one step behind a head the files stand for.
    let behind = cut.op == PendingOp::Land
        && cut.state == LANDING
        && cut.head != ws.file.head
        && ws.file.materialized == ws.file.head;
    if !behind {
        return Ok(false);
    }
    let mut req = ShowReq::new(ws.file.head.as_str());
    req.max_changes = 1;
    let head = ctx.store.show(req)?;
    if head.parents.first() != Some(&cut.head) {
        return Ok(false);
    }

    // 2. Its key landed on its target.
    let target = cut.target.as_deref().unwrap_or(&ws.file.target);
    Ok(status::landed(ctx.store, target, &cut.key, &ws.file.head)? == Landed::Held)
}

/// Whether `cut` is a land `target` holds; if so, workspace `ws` stands on the head it landed:
/// `C`, when the land carried edits, made on `ws`'s head (`workspace.json` first (WS2), then the
/// record follows). Its re-run then lands that head with no changes, which the store replays,
/// whatever was edited since (I8).
fn stand_on_held(
    ctx: &Ctx<'_>,
    ws: &mut Workspace,
    cut: &Pending,
    target: &str,
) -> Result<bool, OpError> {
    // 1. A land the target holds, and the head it landed.
    if cut.state != LANDING || !replays(ctx, ws, target)? {
        return Ok(false);
    }
    let Some(made) = carried_head(ctx, ws, &cut.key)? else {
        return Ok(true);
    };

    // 2. The workspace on `C`, whose files hold its edits; then the record on it too.
    let mut file = ws.file.clone();
    file.head.clone_from(&made);
    file.materialized.clone_from(&made);
    file.onto = None;
    state::replace(ctx.fs.as_ref(), &ws.dir, &file)?;
    ws.file = file;
    let mut record = cut.clone();
    record.head = made;
    pending::write_pending(ctx.fs.as_ref(), &ws.dir, &record).map(|()| true)
}

/// The changeset `C` a land with `key` made of the edits it carried on workspace `ws`'s head:
/// the last of the `squashed_from` of a changeset the journal names as a land of the key, read
/// newest first within [`OP_CAP`] operations; none when the land carried no edits.
fn carried_head(ctx: &Ctx<'_>, ws: &Workspace, key: &str) -> Result<Option<String>, OpError> {
    let mut visited = 0;
    let mut before = None;
    loop {
        // 1. The next page of operations.
        let mut req = OpLogReq::default();
        req.before = before;
        req.limit = OP_PAGE;
        req.max_changesets = OP_CHANGESETS;
        let page = ctx.store.op_log(req)?;

        // 2. Each land of the key: the head it squashed, if made on `ws`'s head.
        for op in &page.ops {
            if visited == OP_CAP {
                return Ok(None);
            }
            visited += 1;
            let lands = (op.changesets.iter()).filter(|c| c.land_key.as_deref() == Some(key));
            for land in lands {
                if let Some(made) = made_on(ctx, &land.id, &ws.file.head)? {
                    return Ok(Some(made));
                }
            }
        }

        // 3. The next page, while there is one.
        before = page.ops.last().map(|op| op.op);
        if !page.truncated || before.is_none() {
            return Ok(None);
        }
    }
}

/// The last changeset landed changeset `landed` squashed, when its first parent is `head`.
fn made_on(ctx: &Ctx<'_>, landed: &str, head: &str) -> Result<Option<String>, OpError> {
    // 1. The head the land squashed last.
    let mut req = ShowReq::new(landed);
    req.max_changes = 1;
    let shown = ctx.store.show(req)?;
    let Some(made) = shown.squashed_from.last() else {
        return Ok(None);
    };

    // 2. Made on `head`.
    let mut req = ShowReq::new(made.as_str());
    req.max_changes = 1;
    let parent = ctx.store.show(req)?;
    Ok((parent.parents.first().map(String::as_str) == Some(head)).then(|| made.clone()))
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
