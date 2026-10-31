//! A push to `refs/land/<target>` (M14 design §7): its tip imported, then landed onto `target`
//! with the push's land key, and a conflict reported on the side band.
//!
//! ```text
//! 1 a commit to land, and access: land ─▶ 2 import the tip from serve.git, objects only
//! ─▶ 3 LocalOps::land_gated, keyed by who pushed which tip where, access: land read again
//!      before each attempt
//!      landed | replayed ─▶ 4 target exported ─▶ ok, and the landed commit on stderr
//!      E_CONFLICT        ─▶ 4 target exported ─▶ ng conflict, the report on stderr
//!      any other error   ─▶ ng <CODE>: <message>
//! ```
//!
//! Each attempt of step 3 reads `access.toml` again first (the M15 design §6): a `land` grant taken
//! away while an attempt loses its race stops the land before the next, with step 1's refusal.
//! An attempt already judged lands even if the grant goes before its publish.
//!
//! The same push again finds its key on the target's first parents and replays the land, writing
//! nothing. receive-pack moves no ref itself, so nothing is left under `refs/land/`. The hook's
//! stderr is git's side band 2: the client prints each line after `remote:`.
//!
//! The `ok` carries no `option refname refs/heads/<target>`: the landed commit is a squash made
//! here, which the client does not have, and git refuses to point its tracking branch at an
//! object it lacks (`update_ref failed … nonexistent object`, git 2.43.0). The stderr line names
//! the commit instead; a fetch brings it in.

use std::io::{self, Write as _};
use std::path::Path;
use std::time::Duration;

use swarmops_api::{
    ConflictKindOut, ConflictReport, ErrorCode, FileConflictOut, Identity, LandReq, LandResp,
    LandStatus, LinesOut, OpError, Rev,
};
use swarmops_core::{ChangesetId, ObjectId};
use swarmops_git::serve::land_key::land_key;
use swarmops_git::serve::proc_receive::{Command, Report};
use swarmops_git::serve::repo::ServeError;
use swarmops_git::{
    DivergedReason, ExportError, ExportRequest, GitOid, GitSource as _, GixSink, GixSource,
    ImportError, ImportRequest, RawCommit, export, import,
};

use super::access::{Access, Action};
use super::guard::Redactor;
use super::hook::{HookCtx, ng};
use super::refresh::{SERVE_REFRESH_ATTEMPTS, serve_error};
use crate::{LocalOps, error};

/// The most conflicting paths a land's report lists, then `… and <k> more`.
pub const MAX_CONFLICT_LINES: usize = 50;

/// How long a land waits for `git/serve.lock` to export its target: the store's writer lock
/// wait, never more.
const SERVE_LOCK_WAIT: Duration = Duration::from_secs(10);

/// The name the texts give the serving repository: never its host path.
const SERVE_GIT: &str = "serve.git";

/// The reason of the `ng` of a land that conflicts: the report follows on stderr.
const CONFLICT: &str = "conflict";

/// How many hex digits of a commit the conflict report shows.
const SHORT_OID: usize = 7;

/// A store ref's head and the commit it exports to.
type Head = (ChangesetId, GitOid);

/// What the hook reports on `command`, a push to `refs/land/<target>`.
pub fn run(ctx: &HookCtx<'_>, command: &Command, target: &str) -> Report {
    match land(ctx, command, target) {
        Ok(report) => report,
        Err(e) => ng(&command.refname, &e),
    }
}

/// Steps 1 to 4 of [the module documentation](self): the report of a land, or why it failed.
fn land(ctx: &HookCtx<'_>, command: &Command, target: &str) -> Result<Report, OpError> {
    // 1. A commit to land, onto a target the caller may land onto.
    let Some(tip) = command.new else {
        return Err(super::refused(
            ErrorCode::InvalidArg,
            "land_deletion",
            format!(
                "{} lands a commit: there is nothing to delete",
                command.refname
            ),
        ));
    };
    ctx.access.check(ctx.who, Action::Land, target)?;

    // 2. The tip's history into the store, no ref moved.
    let head = import_tip(ctx, tip)?;

    // 3. The land, keyed by who pushed which tip where: the same push again replays it.
    let mut req = LandReq::new(Rev::from(head.to_string()));
    target.clone_into(&mut req.target);
    req.idempotency_key = Some(land_key(ctx.who.agent(), target, tip));
    let landed = match land_onto(ctx.ops, ctx.who, req, target) {
        Ok(landed) => landed,
        Err(e) if e.code == ErrorCode::Conflict.as_str() => {
            return conflict(ctx, command, target, e);
        }
        Err(e) => return Err(e),
    };

    // 4. The target exported, and the client told which commit landed (no option refname: the
    //    module documentation).
    let id = changeset_id(&landed.landed)?;
    if let Some(oid) = served_oid(ctx, target, id)? {
        let status = match landed.status {
            LandStatus::Replayed => "landed already",
            _ => "landed",
        };
        say(
            ctx,
            [format!(
                "{status} onto {target} as {oid}: git fetch brings it in"
            )],
        );
    }
    Ok(Report::ok(command.refname.clone()))
}

/// Step 3's land of `req` onto `target` for `who`, `access.toml` read again before each attempt
/// (the M15 design §6): a grant taken away between attempts stops the land before the next, with
/// the refusal step 1 gives at connection.
fn land_onto(
    ops: &LocalOps,
    who: &Identity,
    req: LandReq,
    target: &str,
) -> Result<LandResp, OpError> {
    let granted = |_attempt: u32| Access::load(ops)?.check(who, Action::Land, target);
    ops.land_gated(who, req, granted)
}

/// The `ng` of a land that conflicts, `e`: its report written on stderr for the client, with the
/// target's head as the client fetches it. A conflict without a report is `ng` with `e`.
fn conflict(
    ctx: &HookCtx<'_>,
    command: &Command,
    target: &str,
    e: OpError,
) -> Result<Report, OpError> {
    // 1. The report the land carries in details.report.
    let report = e.details.get("report").cloned();
    let Some(report) = report.and_then(|r| serde_json::from_value::<ConflictReport>(r).ok()) else {
        return Err(e);
    };

    // 2. The target's head the change met, as the commit the client fetches.
    let theirs = changeset_id(&report.theirs)?;
    let at = match served_oid(ctx, target, theirs)? {
        Some(oid) => oid.to_string(),
        None => report.theirs.clone(),
    };
    let at = &at[..SHORT_OID];

    // 3. The report on stderr, for the client.
    say(ctx, conflict_lines(&report, target, at));
    Ok(Report::ng(command.refname.clone(), CONFLICT))
}

/// Writes `lines` on stderr, git's side band 2, which the client prints after `remote:`: each
/// one line, the store's path masked.
fn say(ctx: &HookCtx<'_>, lines: impl IntoIterator<Item = String>) {
    let store = ctx.repo.git_dir().as_std_path().ancestors().nth(2);
    let redactor = Redactor::new(&store.into_iter().collect::<Vec<&Path>>());
    let mut stderr = io::stderr().lock();
    for line in lines {
        let line = redactor.redact(&error::one_line(line));
        // Advice: a client gone already reads nothing, and the report stands all the same.
        let _ = writeln!(stderr, "{line}");
    }
}

/// The lines of a conflict `report` of a land onto `target`, whose head the client fetches as
/// `at`: the count, a line per path up to [`MAX_CONFLICT_LINES`], what to do.
///
/// ```text
/// E_CONFLICT: 1 path conflicts with main at 1a2b3c4
///   src/a.rs: both changed lines 3-4; main's change by agent/bob
/// resolve it on top of main, then push again
/// ```
fn conflict_lines(report: &ConflictReport, target: &str, at: &str) -> Vec<String> {
    let n = report.files.len();
    let count = match n {
        1 => "1 path conflicts".to_owned(),
        n => format!("{n} paths conflict"),
    };
    let mut lines = vec![format!("E_CONFLICT: {count} with {target} at {at}")];
    let listed = report.files.iter().take(MAX_CONFLICT_LINES);
    lines.extend(listed.map(|file| path_line(file, target)));
    if n > MAX_CONFLICT_LINES {
        lines.push(format!("  … and {} more", n - MAX_CONFLICT_LINES));
    }
    lines.push(format!("resolve it on top of {target}, then push again"));
    lines
}

/// The line of a conflicting `file`: its path, how both sides changed it, and who changed it on
/// `target`.
fn path_line(file: &FileConflictOut, target: &str) -> String {
    let what = match file.kind {
        ConflictKindOut::Content | ConflictKindOut::AddAdd if !file.hunks.is_empty() => {
            let ranges: Vec<String> = (file.hunks.iter())
                .map(|hunk| lines(&hunk.theirs))
                .collect();
            format!("both changed lines {}", ranges.join(", "))
        }
        ConflictKindOut::Content => "both changed it".to_owned(),
        ConflictKindOut::Binary => "both changed it, not as text".to_owned(),
        ConflictKindOut::AddAdd => "both added it".to_owned(),
        ConflictKindOut::DeleteModify => format!("deleted by the push, changed on {target}"),
        ConflictKindOut::ModifyDelete => format!("changed by the push, deleted on {target}"),
        ConflictKindOut::Mode => "both changed its kind".to_owned(),
        ConflictKindOut::TypeChange => "a file on one side, a directory on the other".to_owned(),
        _ => "the sides conflict".to_owned(),
    };
    match &file.theirs_agent {
        Some(agent) => format!("  {}: {what}; {target}'s change by {agent}", file.path),
        None => format!("  {}: {what}", file.path),
    }
}

/// A side's lines of a hunk: `3-4`, `3`, or the line `3` a side without lines is before.
fn lines(side: &LinesOut) -> String {
    if side.last <= side.first {
        return side.first.to_string();
    }
    format!("{}-{}", side.first, side.last)
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

/// The commit `id`, a changeset on `target`'s first parents, exports to, with `target` exported
/// into `serve.git` first: `None` when `target` is gone, or no longer reaches `id` that way.
fn served_oid(ctx: &HookCtx<'_>, target: &str, id: ChangesetId) -> Result<Option<GitOid>, OpError> {
    // 1. Under serve.lock, the target exported.
    let Some((head, head_oid)) = export_target(ctx, target)? else {
        return Ok(None);
    };

    // 2. How far down the target's first parents id is.
    let read = ctx.ops.repo.snapshot().map_err(error::op)?;
    if !read.is_ancestor(id, head).map_err(error::op)? {
        return Ok(None);
    }
    let mut steps = 0;
    for step in read.first_parents(head) {
        if step.map_err(error::op)? == id {
            break;
        }
        steps += 1;
    }

    // 3. As far down the export's first parents: the export keeps each changeset's parents in
    //    order.
    let mut oid = head_oid;
    if steps == 0 {
        return Ok(Some(oid));
    }
    let source = serve_source(ctx)?;
    for _ in 0..steps {
        match first_parent(&source, oid)? {
            Some(parent) => oid = parent,
            None => return Ok(None),
        }
    }
    Ok(Some(oid))
}

/// Exports the store branch `target` into `serve.git`'s branch of the same name, forced, under
/// `serve.lock`, again while another writer moves it, [`SERVE_REFRESH_ATTEMPTS`] times at most:
/// its head and that head's export, or `None` when the store has no such branch.
fn export_target(ctx: &HookCtx<'_>, target: &str) -> Result<Option<Head>, OpError> {
    let _lock = ctx.repo.lock(SERVE_LOCK_WAIT).map_err(serve_error)?;
    let mut req = ExportRequest::new(target, target);
    req.force = true;
    req.memo_key.clone_from(&ctx.ops.memo_key);
    let failed = |e: ExportError| error::git_export_error(e, SERVE_GIT, target, target);

    // Each attempt opens serve.git afresh: what gix cached of its refs is not read again.
    let mut attempt = 0;
    loop {
        attempt += 1;
        let mut sink = GixSink::open(ctx.repo.git_dir().as_std_path())
            .map_err(|e| serve_error(ServeError::Sink(e)))?;
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

/// `serve.git`, to read commits from.
fn serve_source(ctx: &HookCtx<'_>) -> Result<GixSource, OpError> {
    GixSource::open(ctx.repo.git_dir().as_std_path())
        .map_err(|e| error::git_import_error(e.into(), SERVE_GIT, "HEAD"))
}

/// The first parent of the commit `oid` in `source`, `None` for a root.
fn first_parent(source: &GixSource, oid: GitOid) -> Result<Option<GitOid>, OpError> {
    let hex = oid.to_string();
    let failed = |e: ImportError| error::git_import_error(e, SERVE_GIT, &hex);
    let (_, bytes) = source.read(oid).map_err(|e| failed(e.into()))?;
    let commit = RawCommit::parse(&bytes).map_err(|e| {
        error::error(
            ErrorCode::StoreCorrupt,
            format!("{SERVE_GIT}: commit {hex}: {e}"),
        )
    })?;
    Ok(commit.parents.first().copied())
}

/// The changeset whose full id is `hex`, as the store gave it.
fn changeset_id(hex: &str) -> Result<ChangesetId, OpError> {
    let id = ObjectId::from_hex(hex)
        .map_err(|e| error::error(ErrorCode::Internal, format!("changeset id {hex:?}: {e}")))?;
    Ok(ChangesetId::new(id))
}

#[cfg(test)]
mod tests {
    use swarmops_api::{ConflictKindOut, ConflictReport, FileConflictOut, HunkOut, LinesOut};

    use super::{MAX_CONFLICT_LINES, conflict_lines};

    /// The changeset ids a report names, which its lines do not show.
    const ID: &str = "00";

    /// A report of `files`.
    fn report(files: Vec<FileConflictOut>) -> ConflictReport {
        ConflictReport::new(ID, ID, ID, files, vec![ConflictKindOut::Content], false)
    }

    /// A file both sides edited at the target's lines `first` to `last`, the target's side by
    /// `agent`.
    fn edited(path: &str, first: u64, last: u64, agent: &str) -> FileConflictOut {
        let mut file = FileConflictOut::new(path, ConflictKindOut::Content);
        let side = LinesOut::new(first, last);
        file.hunks = vec![HunkOut::new(0, side.clone(), side.clone(), side)];
        file.theirs_agent = Some(agent.to_owned());
        file
    }

    /// One path: the count, the path's lines and who changed them, what to do.
    #[test]
    fn one_path_report() {
        let report = report(vec![edited("src/a.rs", 3, 4, "agent/bob")]);
        assert_eq!(
            conflict_lines(&report, "main", "1a2b3c4"),
            [
                "E_CONFLICT: 1 path conflicts with main at 1a2b3c4",
                "  src/a.rs: both changed lines 3-4; main's change by agent/bob",
                "resolve it on top of main, then push again",
            ]
        );
    }

    /// Past MAX_CONFLICT_LINES paths, the rest is counted, not listed.
    #[test]
    fn long_report_is_cut() {
        let files = (0..=MAX_CONFLICT_LINES)
            .map(|n| edited(&format!("f{n:02}"), 1, 1, "agent/bob"))
            .collect();
        let lines = conflict_lines(&report(files), "release/1", "1a2b3c4");
        assert_eq!(lines.len(), MAX_CONFLICT_LINES + 3, "{lines:#?}");
        assert_eq!(
            lines[0],
            format!(
                "E_CONFLICT: {} paths conflict with release/1 at 1a2b3c4",
                MAX_CONFLICT_LINES + 1
            )
        );
        assert_eq!(
            lines[1],
            "  f00: both changed lines 1; release/1's change by agent/bob"
        );
        assert_eq!(lines[MAX_CONFLICT_LINES + 1], "  … and 1 more");
    }

    /// A conflict without hunks says how the sides changed the file.
    #[test]
    fn kinds_without_hunks() {
        let mut deleted = FileConflictOut::new("gone.txt", ConflictKindOut::ModifyDelete);
        deleted.theirs_agent = None;
        let lines = conflict_lines(&report(vec![deleted]), "main", "1a2b3c4");
        assert_eq!(lines[1], "  gone.txt: changed by the push, deleted on main");
    }

    /// The land's grant, judged again before each attempt (the M15 design §6).
    mod grant {
        use std::io;
        use std::sync::{Arc, Mutex};

        use camino::Utf8PathBuf;
        use serde_json::json;
        use swarmops_api::{
            ChangeInput, ChangeOp, CommitReq, ContentInput, ErrorCode, Identity, InfoReq, LandReq,
            LandStatus, OpLogReq, Rev, StoreOps as _,
        };
        use swarmops_store::RealFs;
        use swarmops_testkit::fs::{Call, Watch, Watched};
        use tempfile::TempDir;

        use super::super::land_onto;
        use crate::LocalOps;

        /// Who lands.
        const LANDER: &str = "person/ada";

        /// Who moves `main` while the land writes.
        const RACER: &str = "person/bob";

        /// The file both edit: 32,000 lines of 36 bytes, past the size the publish merges in
        /// the lock (1 MiB a side), so a race on it gives the land back to its lander.
        const REGISTRY: &str = "registry";

        /// The lines of the registry.
        const LINES: usize = 32_000;

        /// The lander may land onto `main`.
        const GRANTED: &str = "version = 1\n[[grant]]\nwho = [\"person/ada\"]\nland = [\"main\"]\n";

        /// The lander may read, no longer land.
        const REVOKED: &str = "version = 1\n[[grant]]\nwho = [\"person/ada\"]\nread = true\n";

        /// What the race does to `access.toml` beside its move of `main`.
        #[derive(Clone, Copy)]
        enum Meanwhile {
            /// It leaves the file as it is.
            Kept,
            /// It takes the lander's `land` away, keeping `read`.
            Revoked,
        }

        /// Moves `main` once, as another lander would, at the land's first flush of objects
        /// (its first `fsync_dir` under `objects/`, outside the lock), editing the registry a
        /// line apart from the land; then does to `access.toml` what `meanwhile` says.
        struct Race {
            root: Utf8PathBuf,
            meanwhile: Meanwhile,
            /// The changeset it moved `main` to, once it has.
            raced: Mutex<Option<String>>,
        }

        impl Watch for Race {
            fn before(&self, call: &Call<'_>) -> io::Result<()> {
                let at = call.path().strip_prefix(&self.root).unwrap_or(call.path());
                let mut raced = self.raced.lock().unwrap();
                if call.method != "fsync_dir" || !at.starts_with("objects") || raced.is_some() {
                    return Ok(());
                }

                // 1. Another lander's change on main, through a handle of its own.
                let other = LocalOps::open(&self.root).unwrap();
                *raced = Some(commit(&other, RACER, None, registry(1, "theirs")));

                // 2. The grant, as the operator leaves it.
                if let Meanwhile::Revoked = self.meanwhile {
                    std::fs::write(self.root.join("access.toml"), REVOKED)?;
                }
                Ok(())
            }
        }

        /// The registry, its line `at` replaced by `line`.
        fn registry(at: usize, line: &str) -> String {
            let lines = (0..LINES).map(|n| {
                if n == at {
                    format!("{line}\n")
                } else {
                    format!("entry {n:06} of the registry, as is\n")
                }
            });
            lines.collect()
        }

        /// Commits `content` at the registry by `agent` on `parent`, moving `main` when no
        /// parent is given: the changeset's id.
        fn commit(ops: &LocalOps, agent: &str, parent: Option<&str>, content: String) -> String {
            let who = Identity::parse(agent).unwrap();
            let mut change = ChangeInput::new(REGISTRY.to_owned(), ChangeOp::Put);
            change.content = Some(ContentInput::Inline(content));
            let mut req = CommitReq::new(vec![change], "registry".to_owned());
            req.parent = parent.map(Rev::from);
            req.advance = parent.is_none().then(|| "main".to_owned());
            ops.commit(&who, req).unwrap().changeset
        }

        /// The number of the store's last operation: its frame count.
        fn last_op(ops: &LocalOps) -> u64 {
            let mut req = OpLogReq::default();
            req.before = None;
            req.limit = 1;
            req.max_changesets = 1;
            ops.op_log(req).unwrap().ops[0].op
        }

        /// A land onto `main` that loses its first race, with the race doing `meanwhile`:
        /// the store's directory, its operations, the handle the race watches, the race, and the
        /// land's request.
        fn raced_land(meanwhile: Meanwhile) -> (TempDir, LocalOps, LocalOps, Arc<Race>, LandReq) {
            // 1. The store, main's registry, the lander's grant, and the change.
            let dir = tempfile::tempdir().unwrap();
            let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
            let (ops, _) = LocalOps::init(&root, "acme").unwrap();
            let base = registry(0, "entry 000000 of the registry, as is");
            let t0 = commit(&ops, LANDER, None, base);
            std::fs::write(root.join("access.toml"), GRANTED).unwrap();
            let head = commit(&ops, LANDER, Some(&t0), registry(4, "ours"));

            // 2. The race, and a handle whose writes it watches.
            let race = Arc::new(Race {
                root: root.clone(),
                meanwhile,
                raced: Mutex::new(None),
            });
            let racing = LocalOps::open_on(&root, Watched(Arc::clone(&race), RealFs::new()));
            let land: LandReq = serde_json::from_value(json!({"head": head})).unwrap();
            (dir, ops, racing, race, land)
        }

        /// A grant taken away while a land loses its first race stops the land before its
        /// second attempt: `E_POLICY_DENIED` as at connection, `main` the racer's changeset,
        /// and the race's frame alone.
        #[test]
        fn a_grant_revoked_after_a_lost_race_is_policy_denied() {
            let (_dir, ops, racing, race, land) = raced_land(Meanwhile::Revoked);
            let who = Identity::parse(LANDER).unwrap();
            let before = last_op(&ops);

            // 1. The refusal, as the connection's check gives it.
            let error = land_onto(&racing, &who, land, "main").unwrap_err();
            let raced = race.raced.lock().unwrap().clone().expect("the race ran");
            assert_eq!(error.code, ErrorCode::PolicyDenied.as_str(), "{error:?}");
            assert!(!error.retryable, "{error:?}");
            assert_eq!(error.details["reason"], "not_granted", "{error:?}");
            assert_eq!(error.details["identity"], LANDER, "{error:?}");
            assert_eq!(error.details["action"], "land", "{error:?}");
            assert_eq!(error.details["ref"], "main", "{error:?}");

            // 2. Nothing of the land recorded.
            let main = ops.repo_info(InfoReq::default()).unwrap().main;
            assert_eq!(main.as_deref(), Some(raced.as_str()));
            assert_eq!(last_op(&ops), before + 1, "the race's frame alone");
        }

        /// A grant kept through a lost race lets the land land on its second attempt.
        #[test]
        fn a_grant_kept_through_a_lost_race_lands() {
            let (_dir, ops, racing, race, land) = raced_land(Meanwhile::Kept);
            let who = Identity::parse(LANDER).unwrap();

            let landed = land_onto(&racing, &who, land, "main").unwrap();
            let raced = race.raced.lock().unwrap().clone().expect("the race ran");
            assert_eq!(
                (landed.status, landed.attempts, landed.rebased_onto.as_str()),
                (LandStatus::Landed, 2, raced.as_str())
            );
            let main = ops.repo_info(InfoReq::default()).unwrap().main;
            assert_eq!(main, Some(landed.landed));
        }
    }
}
