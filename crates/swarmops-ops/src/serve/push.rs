//! One command of a push that moves a ref (M14 design §6): mapped, its access checked, its
//! objects imported, then the store ref moved by compare-and-swap.
//!
//! ```text
//! 1 refmap::from_git ─▶ 2 access: push ─▶ 3 import the tip from serve.git, objects only
//!   ── serve.lock ──────────────────────────────────────────────────────────────────
//!   4 the ref's head exported ─▶ 5 old = that export? ─▶ 6 fast-forward? ─▶ 7 store CAS
//!   ─▶ 8 the moved ref exported ─▶ ok
//! ```
//!
//! git has checked the pack (fsck, connectivity) and moved it into `serve.git` before the hook
//! runs, so the import reads the tip there. `main` moves only forward along first parents: the
//! land replay and the conflict report walk them, so a merge whose second parent is the old
//! `main` would hide earlier lands from them. Another branch moves forward along any parent.
//! Steps 4 to 8 hold `serve.lock`, so that no refresh moves the ref between the compare and the
//! export; the store's own compare-and-swap is the one that decides.

use std::time::Duration;

use swarmops_api::{ErrorCode, OpError, RefSetReq, Rev, StoreOps as _};
use swarmops_core::ChangesetId;
use swarmops_git::serve::proc_receive::{Command, Report};
use swarmops_git::serve::refmap::{Mapped, Refusal, from_git};
use swarmops_git::transport::{GitCli, Transport as _};
use swarmops_git::{
    DivergedReason, ExportError, ExportRequest, GitOid, GixSink, GixSource, ImportError,
    ImportRequest, export, import,
};
use swarmops_store::RefKind;

use super::access::Action;
use super::hook::{HookCtx, ng};
use super::not_built;
use super::refresh::{SERVE_REFRESH_ATTEMPTS, serve_error};
use crate::error;

/// How long a push waits for `git/serve.lock`: the store's writer lock wait, never more.
const SERVE_LOCK_WAIT: Duration = Duration::from_secs(10);

/// Where a branch is in git.
const HEADS: &str = "refs/heads/";

/// The name the texts give the serving repository: never its host path.
const SERVE_GIT: &str = "serve.git";

/// The branch that moves only forward along first parents.
const MAIN: &str = "main";

/// Why a move of `main` that is not forward along first parents is refused.
const MAIN_NOT_FORWARD: &str =
    "non-fast-forward: main moves along first parents; push to refs/land/main";

/// The option of an `ok` report that names the commit the ref moved to, when it is not the one
/// pushed.
const NEW_OID: &str = "new-oid";

/// A store ref's head and the commit it exports to.
type Head = (ChangesetId, GitOid);

/// What the hook reports on `command`.
pub fn decide(ctx: &HookCtx<'_>, command: &Command) -> Report {
    match push(ctx, command) {
        Ok(report) => report,
        Err(e) => ng(&command.refname, &e),
    }
}

/// Steps 1 to 8 of [the module documentation](self): the report of a move, or why it was
/// refused.
fn push(ctx: &HookCtx<'_>, command: &Command) -> Result<Report, OpError> {
    // 1. The store ref; a tag and a deletion are the push rules' to come.
    let (store, kind) = match from_git(ctx.ops.repo.format(), &command.refname) {
        Mapped::Ref { store, kind } => (store, kind),
        Mapped::Refused(refusal) => return Err(refused(&command.refname, refusal)),
        _ => return Err(error::unknown_variant("Mapped")),
    };
    if kind == RefKind::Tag {
        return Err(not_built("pushing a tag"));
    }
    let Some(new) = command.new else {
        return Err(not_built("deleting a ref"));
    };

    // 2. The caller may push it.
    ctx.access.check(ctx.who, Action::Push, &store)?;

    // 3. The tip's history into the store, no ref moved.
    let tip = import_tip(ctx, new)?;

    // 4. Under serve.lock: the head now, as its export.
    let _lock = ctx.repo.lock(SERVE_LOCK_WAIT).map_err(serve_error)?;
    let head = export_ref(ctx, &command.refname, &store)?;

    // 5. The client's old value is that export, or the ref is new on both sides.
    if head.map(|(_, oid)| oid) != command.old {
        return Ok(stale(command));
    }

    // 6. Forward only: main along first parents; another branch's other moves need `force`,
    //    which the push rules move.
    if let Some((id, oid)) = head
        && !forward(ctx, &store, (id, oid), (tip, new))?
    {
        if store == MAIN {
            return Ok(Report::ng(command.refname.clone(), MAIN_NOT_FORWARD));
        }
        ctx.access.check(ctx.who, Action::Force, &store)?;
        return Err(not_built("a forced move"));
    }

    // 7. The store ref moved from the head read, by compare-and-swap.
    let mut req = RefSetReq::new(store.clone(), Rev::from(tip.to_string()));
    match head {
        Some((id, _)) => req.expected_old = Some(Rev::from(id.to_string())),
        None => req.create = true,
    }
    match ctx.ops.ref_set(ctx.who, req) {
        Err(e) if e.code == ErrorCode::CasMismatch.as_str() => return Ok(stale(command)),
        moved => moved?,
    };

    // 8. serve.git's ref at the move's export: the pushed commit, unless its import was lossy.
    let moved = export_ref(ctx, &command.refname, &store)?;
    let mut report = Report::ok(command.refname.clone());
    if let (Some((_, oid)), Report::Ok { options, .. }) = (moved, &mut report)
        && oid != new
    {
        options.push((NEW_OID.to_owned(), oid.to_string()));
    }
    Ok(report)
}

/// The changeset of the commit `new`, which git moved into `serve.git`, its history imported
/// into the store objects-only.
fn import_tip(ctx: &HookCtx<'_>, new: GitOid) -> Result<ChangesetId, OpError> {
    let hex = new.to_string();
    let failed = |e: ImportError| error::git_import_error(e, SERVE_GIT, &hex);
    let source = GixSource::open(ctx.repo.git_dir().as_std_path()).map_err(|e| failed(e.into()))?;
    let mut request = ImportRequest::objects_only(hex.as_str());
    request.memo_key.clone_from(&ctx.ops.memo_key);
    let report = import(&ctx.ops.repo, &source, &request).map_err(failed)?;
    Ok(report.head)
}

/// Exports the store ref `store` into `full` of `serve.git`, forced, again while another writer
/// moves it, [`SERVE_REFRESH_ATTEMPTS`] times at most: its head and that head's export, or
/// `None` when the store has no such ref.
fn export_ref(ctx: &HookCtx<'_>, full: &str, store: &str) -> Result<Option<Head>, OpError> {
    let short = full.strip_prefix(HEADS).unwrap_or(full);
    let mut req = ExportRequest::new(store, short);
    req.force = true;
    req.memo_key.clone_from(&ctx.ops.memo_key);
    let failed = |e: ExportError| error::git_export_error(e, SERVE_GIT, store, short);

    // Each attempt opens serve.git afresh: what gix cached of its refs is not read again.
    let mut attempt = 0;
    loop {
        attempt += 1;
        let mut sink = GixSink::open(ctx.repo.git_dir().as_std_path())
            .map_err(|e| serve_error(swarmops_git::serve::repo::ServeError::Sink(e)))?;
        match export(&ctx.ops.repo, &mut sink, &req) {
            Ok(report) => return Ok(Some((report.head, report.head_oid))),
            Err(ExportError::SourceRef(_)) => return Ok(None),
            Err(ExportError::Diverged {
                reason: DivergedReason::MovedDuringExport { .. },
                ..
            }) if attempt < SERVE_REFRESH_ATTEMPTS => {}
            Err(e) => return Err(failed(e)),
        }
    }
}

/// Whether `to` moves `store` forward from `from` (each a changeset and its commit): along first
/// parents for `main`, along any parent in `serve.git` for another branch.
fn forward(ctx: &HookCtx<'_>, store: &str, from: Head, to: Head) -> Result<bool, OpError> {
    if store == MAIN {
        let read = ctx.ops.repo.snapshot().map_err(error::op)?;
        return read.is_ancestor(from.0, to.0).map_err(error::op);
    }
    let failed = |e| error::transport_error(e, SERVE_GIT, error::Phase::Push);
    let git = GitCli::find().map_err(failed)?;
    git.is_ancestor(ctx.repo.git_dir().as_std_path(), from.1, to.1)
        .map_err(failed)
}

/// The refusal of a git ref the map serves no store ref for.
fn refused(git_ref: &str, refusal: Refusal) -> OpError {
    let (code, reason, message) = match refusal {
        Refusal::Reserved => (
            ErrorCode::RefNameInvalid,
            "reserved",
            format!("{git_ref} names a ref the store reserves"),
        ),
        Refusal::FormatRequired { needs } => (
            ErrorCode::FormatRequired,
            "format_required",
            format!(
                "{git_ref} needs a store of format {}: swarmops upgrade",
                needs.version()
            ),
        ),
        _ => (
            ErrorCode::RefNameInvalid,
            "not_served",
            format!("{git_ref} is not served: push to refs/heads/ or refs/land/"),
        ),
    };
    super::refused(code, reason, message)
}

/// The refusal of `command`, whose old value is not its ref's export now (`E_CAS_MISMATCH`'s
/// case): nothing moved.
fn stale(command: &Command) -> Report {
    let reason = format!("stale: {} moved; fetch first", command.refname);
    Report::ng(command.refname.clone(), reason)
}
