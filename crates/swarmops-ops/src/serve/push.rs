//! One command of a push that moves a ref (M14 design §6, M16 design §7): judged (mapped, its
//! access checked, its objects imported, forward or forced), then the store ref moved by
//! compare-and-swap.
//!
//! ```text
//! judge: 1 refmap::from_git ─▶ 2 access: push ─▶ 3 import the tip, objects only
//!        ─▶ 3b the store's move rules ─▶ 3c forward from the head now? (else force)
//! apply: ── serve.lock ─────────────────────────────────────────────────────────────
//!   4 the ref's head exported ─▶ 5 old = that export? ─▶ 6 fast-forward? (else force)
//!   ─▶ 7 store CAS ─▶ 8 the moved ref exported ─▶ ok
//!
//! a deletion: 1 ─▶ main? E_REF_PROTECTED ─▶ 2 access: delete ─▶ 4 ─▶ 5 ─▶ 7 store CAS
//!   ─▶ 8 serve.git's ref deleted ─▶ ok
//! ```
//!
//! `pre-receive` judges inside git's quarantine, reading the tip there; git has checked the pack
//! (fsck, connectivity) first. The judge reads each head without exporting it: `main` as the
//! store ref, another branch as the commit `serve.git`'s ref names now. `proc-receive` judges
//! again once git has moved the pack into `serve.git`, then applies under the lock.
//!
//! `main` moves only forward along first parents: the land replay and the conflict report walk
//! them, so a merge whose second parent is the old `main` would hide earlier lands from them.
//! Another branch moves forward along any parent.
//! Steps 4 to 8 hold `serve.lock`, so that no refresh moves the ref between the compare and the
//! export; the store's own compare-and-swap is the one that decides. Its move rules (`main`
//! never deleted, a tag never moved) are the store's `check_move`, called, not restated: a tag
//! skips step 6 so that its move reaches the store's refusal (`E_TAG_IMMUTABLE`). An annotated
//! tag is no changeset, so its push is refused before the import (`annotated_tag`).
//!
//! Once step 7 has deleted a store ref, the deletion stands: `serve.git`'s ref naming another
//! commit than step 4 exported (moved by something that holds no `serve.lock`) is left as it
//! is, the report is `ok`, and the next refresh removes the ref the store no longer has.

use swarmops_api::{ErrorCode, OpError, RefDeleteReq, RefSetReq, Rev, StoreOps as _};
use swarmops_core::ChangesetId;
use swarmops_git::serve::proc_receive::{Command, Report};
use swarmops_git::serve::refmap::{Mapped, Refusal, from_git};
use swarmops_git::serve::repo::{ServeError, ServeLock};
use swarmops_git::transport::{GitCli, Transport as _};
use swarmops_git::{
    BranchUpdate, GitOid, GitSink as _, GitSource as _, GixSink, ImportError, ImportRequest,
    RawKind, import,
};
use swarmops_repo::RepoError;
use swarmops_store::{RefKind, check_move};

use super::access::Action;
use super::hook::{HookCtx, Judged, Objects, ng};
use super::refresh::{
    SERVE_LOCK_WAIT, StateUpdates, export_one, serve_error, short_name, write_state,
};
use crate::error;

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

/// What [`import_tip`] does with a tip that is an annotated tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TagTip {
    /// Refused (`annotated_tag`): a ref would name the tag, which is no changeset.
    Refused,
    /// Imported as the commit it names: a land squashes that commit's change.
    Peeled,
}

/// The phase log's line before what a push did under `serve.lock` (test hooks).
#[cfg(feature = "test-hooks")]
const HELD_MARK: &str = "serve.push.held 0 0\n";

/// The phase log's names of the export's counts ([`swarmops_git::export::work_counts`]).
#[cfg(feature = "test-hooks")]
const WORK_NAMES: [&str; 3] = [
    "serve.push.exports",
    "serve.push.export_of",
    "serve.push.nodes",
];

/// What a push does while it holds `serve.lock`, counted for the phase log (test hooks): the
/// git crate's totals before it are written when the lock is taken, then a mark; the held
/// span's totals and the export's counts when it ends. Nothing without the hooks.
///
/// ```text
///   <judge's totals>  serve.push.held 0 0  <held totals>  serve.push.{exports,export_of,nodes}
/// ```
struct Held {
    #[cfg(feature = "test-hooks")]
    start: [u64; 3],
}

impl Held {
    /// The held span, from now.
    fn start() -> Held {
        #[cfg(feature = "test-hooks")]
        {
            crate::phase::write_git();
            super::refresh::append(HELD_MARK);
            Held {
                start: swarmops_git::export::work_counts(),
            }
        }
        #[cfg(not(feature = "test-hooks"))]
        Held {}
    }
}

#[cfg(feature = "test-hooks")]
impl Drop for Held {
    fn drop(&mut self) {
        use std::fmt::Write as _;

        crate::phase::write_git();
        let now = swarmops_git::export::work_counts();
        let mut lines = String::new();
        for (name, (now, start)) in WORK_NAMES.iter().zip(now.iter().zip(self.start)) {
            let _ = writeln!(lines, "{name} 0 {}", now - start);
        }
        super::refresh::append(&lines);
    }
}

/// The judge's steps 1 to 3c of [the module documentation](self) on `command`, its objects
/// read through `objects`: what to apply, or the report that refuses it.
pub(crate) fn judge(
    ctx: &HookCtx<'_>,
    command: &Command,
    objects: Objects<'_>,
) -> Result<Judged, Report> {
    judged(ctx, command, objects).map_err(|e| match e {
        Refused::Report(report) => report,
        Refused::Error(e) => ng(&command.refname, &e),
    })
}

/// Why the judge refused a command: a report of its own, or an error.
enum Refused {
    /// `ng` with this reason.
    Report(Report),
    /// `ng <CODE>: <message>`.
    Error(OpError),
}

impl From<OpError> for Refused {
    fn from(e: OpError) -> Refused {
        Refused::Error(e)
    }
}

/// [`judge`]'s steps.
fn judged(ctx: &HookCtx<'_>, command: &Command, objects: Objects<'_>) -> Result<Judged, Refused> {
    // 1. The store ref; a deletion has its own steps.
    let format = ctx.ops.repo.format();
    let (store, kind) = match from_git(format, &command.refname) {
        Mapped::Ref { store, kind } => (store, kind),
        Mapped::Refused(refusal) => return Err(refused(&command.refname, refusal).into()),
        _ => return Err(error::unknown_variant("Mapped").into()),
    };
    let now = ctx.ops.repo.get_ref(&store).map_err(error::op)?;
    let now = now.map(|state| state.target);
    let Some(new) = command.new else {
        // 1, 2. A ref no grant may delete (main) is refused as protected, before any grant.
        let protected = check_move(format, &store, now, None);
        protected.map_err(|e| error::op(RepoError::from(e)))?;
        ctx.access.check(ctx.who, Action::Delete, &store)?;
        return Ok(Judged::Delete { store, kind });
    };

    // 2. The caller may push it.
    ctx.access.check(ctx.who, Action::Push, &store)?;

    // 3. The tip's history into the store, no ref moved.
    let tip = import_tip(ctx, objects, new, TagTip::Refused)?;

    // 3b. The store's own move rules: a tag never moves.
    let rules = check_move(format, &store, now, Some(tip));
    rules.map_err(|e| error::op(RepoError::from(e)))?;

    // 3c. Forward from the head now, read without exporting: main along first parents in the
    //     store, another branch along any parent from the commit serve.git names.
    let Some(head) = now.filter(|_| kind != RefKind::Tag) else {
        return Ok(Judged::Move { store, kind, tip });
    };
    if store == MAIN {
        let read = ctx.ops.repo.snapshot().map_err(error::op)?;
        if !read.is_ancestor(head, tip).map_err(error::op)? {
            let report = Report::ng(command.refname.clone(), MAIN_NOT_FORWARD);
            return Err(Refused::Report(report));
        }
        return Ok(Judged::Move { store, kind, tip });
    }
    let short = short_name(&command.refname);
    let failed = |e| error::transport_error(e, SERVE_GIT, error::Phase::Push);
    let sink = GixSink::open(ctx.repo.git_dir().as_std_path())
        .map_err(|e| serve_error(ServeError::Sink(e)))?;
    let served = sink
        .branch(short)
        .map_err(|e| serve_error(ServeError::Sink(e)))?;
    if let Some(served) = served
        && !objects.is_ancestor(ctx.repo, served, new).map_err(failed)?
    {
        ctx.access.check(ctx.who, Action::Force, &store)?;
    }
    Ok(Judged::Move { store, kind, tip })
}

/// What `proc-receive` reports on `command`, which [`judge`] found to do `judged`.
pub(crate) fn apply(ctx: &HookCtx<'_>, command: &Command, judged: Judged) -> Report {
    let applied = match judged {
        Judged::Move { store, kind, tip } => push(ctx, command, &store, kind, tip),
        Judged::Delete { store, kind } => delete(ctx, command, &store, kind),
        Judged::Land { .. } => Err(error::unknown_variant("Judged")),
    };
    match applied {
        Ok(report) => report,
        Err(e) => ng(&command.refname, &e),
    }
}

/// Steps 4 to 8 of [the module documentation](self) for the store ref `store` of kind `kind`,
/// moved to the changeset `tip`: the report of a move, or why it was refused.
fn push(
    ctx: &HookCtx<'_>,
    command: &Command,
    store: &str,
    kind: RefKind,
    tip: ChangesetId,
) -> Result<Report, OpError> {
    let Some(new) = command.new else {
        return Err(error::unknown_variant("Command"));
    };

    // 4. Under serve.lock: the head now, as its export.
    let lock = ctx.repo.lock(SERVE_LOCK_WAIT).map_err(serve_error)?;
    let _held = Held::start();
    let head = export_ref(ctx, &command.refname, store, kind)?;
    record(ctx, &lock, &command.refname, head);

    // 5. The client's old value is that export, or the ref is new on both sides.
    if head.map(|(_, oid)| oid) != command.old {
        return Ok(stale(command));
    }

    // 6. Forward only: main along first parents; another branch's other moves need `force`.
    //    A tag never moves: step 7's store refuses it, whatever the grants.
    if let Some((id, oid)) = head
        && kind != RefKind::Tag
        && !forward(ctx, store, (id, oid), (tip, new))?
    {
        if store == MAIN {
            return Ok(Report::ng(command.refname.clone(), MAIN_NOT_FORWARD));
        }
        ctx.access.check(ctx.who, Action::Force, store)?;
    }

    // 7. The store ref moved from the head read, by compare-and-swap.
    let mut req = RefSetReq::new(store.to_owned(), Rev::from(tip.to_string()));
    match head {
        Some((id, _)) => req.expected_old = Some(Rev::from(id.to_string())),
        None => req.create = true,
    }
    match ctx.ops.ref_set(ctx.who, req) {
        Err(e) if e.code == ErrorCode::CasMismatch.as_str() => return Ok(stale(command)),
        moved => moved?,
    };

    // 8. serve.git's ref at the move's export: the pushed commit, unless its import was lossy.
    let moved = export_ref(ctx, &command.refname, store, kind)?;
    record(ctx, &lock, &command.refname, moved);
    let mut report = Report::ok(command.refname.clone());
    if let (Some((_, oid)), Report::Ok { options, .. }) = (moved, &mut report)
        && oid != new
    {
        options.push((NEW_OID.to_owned(), oid.to_string()));
    }
    Ok(report)
}

/// A deletion of the store ref `store` of kind `kind`, which [`judge`] found protected by no
/// rule and granted, from the export the client last saw: steps 4, 5 and 7 of [the module
/// documentation](self), then `serve.git`'s ref deleted.
fn delete(
    ctx: &HookCtx<'_>,
    command: &Command,
    store: &str,
    kind: RefKind,
) -> Result<Report, OpError> {
    // 4, 5. Under serve.lock: the head now, as its export, is the client's old value.
    let lock = ctx.repo.lock(SERVE_LOCK_WAIT).map_err(serve_error)?;
    let head = export_ref(ctx, &command.refname, store, kind)?;
    record(ctx, &lock, &command.refname, head);
    let Some((id, oid)) = head.filter(|(_, oid)| Some(*oid) == command.old) else {
        return Ok(stale(command));
    };

    // 7. The store ref deleted from the head read, by compare-and-swap.
    let req = RefDeleteReq::new(store, Rev::from(id.to_string()));
    match ctx.ops.ref_delete(ctx.who, req) {
        Err(e) if e.code == ErrorCode::CasMismatch.as_str() => return Ok(stale(command)),
        deleted => deleted?,
    };

    // 8. serve.git's ref, which step 4 wrote, deleted.
    unset(ctx, &lock, &command.refname, oid)?;
    record(ctx, &lock, &command.refname, None);
    Ok(Report::ok(command.refname.clone()))
}

/// Records in `git/serve.state` that `full` names the export `head` now (`None`: no ref), under
/// `lock`, after the export or the deletion that moved it. A write that fails is passed over:
/// the old state then names what `serve.git` no longer does, and the next connection refreshes.
fn record(ctx: &HookCtx<'_>, lock: &ServeLock, full: &str, head: Option<Head>) {
    let updates = StateUpdates::from([(full.to_owned(), head)]);
    let _ = write_state(ctx.repo, lock, &updates);
}

/// Deletes `full` from `serve.git`, where it names `oid`, under `lock`, after the store ref it
/// is served for was deleted. A ref that names another commit, or none, is left as it is: the
/// store's deletion stands, and the next refresh removes what the store no longer has.
fn unset(ctx: &HookCtx<'_>, lock: &ServeLock, full: &str, oid: GitOid) -> Result<(), OpError> {
    let update = ctx.repo.set_ref(lock, full, Some(oid), None);
    match update.map_err(serve_error)? {
        BranchUpdate::Applied | BranchUpdate::Mismatch { .. } => Ok(()),
        _ => Err(error::unknown_variant("BranchUpdate")),
    }
}

/// The changeset of the commit `new`, read through `objects`, its history imported into the
/// store objects-only: a push's tip and a land's. An annotated tag is no changeset, and the
/// import peels it to its commit: `tag` says whether that is refused (`annotated_tag`).
pub(super) fn import_tip(
    ctx: &HookCtx<'_>,
    objects: Objects<'_>,
    new: GitOid,
    tag: TagTip,
) -> Result<ChangesetId, OpError> {
    let hex = new.to_string();
    let failed = |e: ImportError| error::git_import_error(e, SERVE_GIT, &hex);
    let source = objects.source(ctx.repo).map_err(|e| failed(e.into()))?;
    let refused = tag == TagTip::Refused;
    if refused && source.header(new).map_err(|e| failed(e.into()))?.0 == RawKind::Tag {
        return Err(super::refused(
            ErrorCode::InvalidArg,
            "annotated_tag",
            format!(
                "{hex} is an annotated tag, which the store does not hold: push a lightweight tag"
            ),
        ));
    }
    let mut request = ImportRequest::objects_only(hex.as_str());
    request.memo_key.clone_from(&ctx.ops.memo_key);
    let report = import(&ctx.ops.repo, &source, &request).map_err(failed)?;
    Ok(report.head)
}

/// Exports the store ref `store` of kind `kind` into `full` of `serve.git`, forced, under
/// `serve.lock` (the refresh's own export of a ref, on a committed snapshot of its own): its
/// head and that head's export, or `None` when the store has no such ref.
fn export_ref(
    ctx: &HookCtx<'_>,
    full: &str,
    store: &str,
    kind: RefKind,
) -> Result<Option<Head>, OpError> {
    let exported = export_one(ctx.ops, ctx.repo, None, full, (store, kind))?;
    Ok(exported.map(|report| (report.head, report.head_oid)))
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

#[cfg(test)]
mod tests {
    use camino::Utf8PathBuf;
    use swarmops_api::{ChangeInput, ChangeOp, CommitReq, ContentInput, Identity, StoreOps as _};
    use swarmops_git::GitOid;
    use swarmops_git::serve::repo::ServeRepo;

    use super::unset;
    use crate::LocalOps;
    use crate::serve::access::Access;
    use crate::serve::hook::HookCtx;
    use crate::serve::refresh::{SERVE_LOCK_WAIT, Scope, refresh};

    /// Who pushes.
    const PUSHER: &str = "person/ada";

    /// The pusher's grants: the deletion is the store's by the time `unset` runs.
    const GRANTED: &str = "version = 1\n[[grant]]\nwho = [\"person/ada\"]\npush = [\"*\"]\n";

    /// The branch whose deletion the test ends, in git.
    const MAIN_REF: &str = "refs/heads/main";

    /// A commit `serve.git`'s branch does not name.
    const ELSEWHERE: &[u8] = b"00000000000000000000000000000000000000aa";

    /// A deletion the store has made is not undone by `serve.git`'s ref naming another commit
    /// than the one exported under `serve.lock` (something else moved it): the push's deletion
    /// stands, the ref is left to the next refresh, and no error makes the push an `ng`.
    #[test]
    fn a_ref_moved_under_the_lock_leaves_the_deletion_done() {
        // 1. A store with main, served.
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
        let (ops, _) = LocalOps::init(&root, "acme").unwrap();
        let who = Identity::parse(PUSHER).unwrap();
        let mut change = ChangeInput::new("a".to_owned(), ChangeOp::Put);
        change.content = Some(ContentInput::Inline("a".to_owned()));
        let mut req = CommitReq::new(vec![change], "a".to_owned());
        req.advance = Some("main".to_owned());
        ops.commit(&who, req).unwrap();
        std::fs::write(root.join("access.toml"), GRANTED).unwrap();
        let repo = ServeRepo::open(&root).unwrap();
        refresh(&ops, &repo, &Scope::All).unwrap();
        let served = repo.refs().unwrap()[MAIN_REF];

        // 2. The last step of a deletion that exported another commit than the ref names now.
        let access = Access::load(&ops).unwrap();
        let ctx = HookCtx::new(&ops, &repo, &who, &access);
        let lock = repo.lock(SERVE_LOCK_WAIT).unwrap();
        let elsewhere = GitOid::from_hex(ELSEWHERE).unwrap();
        let done = unset(&ctx, &lock, MAIN_REF, elsewhere);
        assert!(done.is_ok(), "{done:?}");

        // 3. The ref as it was: the next refresh's to remove.
        assert_eq!(repo.refs().unwrap()[MAIN_REF], served);
    }
}
