//! `push` (M13; operations spec §1's `push` row, §6): a store ref sent to a remote branch by
//! compare-and-swap, never by force. The ref is exported into a staging branch of the mirror
//! seeded at what the last fetch saw, so the export only appends, then git pushes it with a
//! lease on that value: the remote moves only from it, to a descendant of it.
//!
//! A client-side operation, as `git export` is, and an MCP tool, which takes a remote by its
//! name only. Its public form takes the mirror's lock; the locked form receives it from `sync`,
//! which holds it already.
//!
//! ```text
//!   push <remote> [<ref>[:<branch>]]
//!     the ref, the remote, the branch     checked first; no git runs for a refusal
//!     git/lock                            held to the end (E_BUSY at once)
//!     seen = refs/remotes/<remote>/<branch>, imported objects-only if the store lacks it
//!     refs/heads/push/<remote>/<branch>   seeded at seen, then the export appends to it: E
//!     E == seen                           up_to_date, and no git ran
//!     GitHub's limits                     when the remote is GitHub's
//!     git push --force-with-lease=<branch>:<seen> E:<branch>
//!     refs/remotes/<remote>/<branch>      seen → E, by compare-and-swap; git gc --auto
//! ```

use swarmops_api::{
    ErrorCode, LossyClass, OpError, PushReq, PushResp, PushStatus, RemoteInfo, RemoteListReq,
};
use swarmops_core::ChangesetId;
use swarmops_git::mirror::{Mirror, MirrorError, MirrorLock, Rebuilt};
use swarmops_git::transport::{
    Credential, GitCli, PushOutcome, PushUpdate, RemoteUrl, Transport as _,
};
use swarmops_git::{
    DivergedReason, ExportError, ExportReport, ExportRequest, GitOid, GixSink, GixSource,
    ImportError, ImportRequest, LossySet, SinkError, limits,
};
use swarmops_store::{Recovered, SYSTEM_GIT, TailRepair};

use crate::error::{self, Phase};
use crate::local::{LocalOps, Recording, Resolved};

/// The mirror as errors name it: relative to the store's root, never a host path.
const MIRROR_GIT: &str = "git/mirror.git";

/// The store ref a remote's followed branch is the default branch of.
const MAIN: &str = "main";

impl LocalOps {
    /// `push` as a request of its own ([`Recording::push`]).
    pub fn push(&self, req: PushReq) -> Result<PushResp, OpError> {
        self.recording(&Resolved::default()).push(req)
    }
}

impl Recording<'_> {
    /// `push`: `req.source_ref` sent to the remote's branch by compare-and-swap, under the
    /// mirror's lock, which it takes. It needs no identity: it is an export, as git export is.
    pub fn push(&self, req: PushReq) -> Result<PushResp, OpError> {
        let remote = self.named_remote(&req.remote)?;
        let mirror = Mirror::open(self.ops.store_path()).map_err(error::mirror_error)?;
        let lock = mirror.lock().map_err(error::mirror_error)?;
        self.push_locked(&lock, &remote, req)
    }

    /// `push` to `remote` under the mirror's `lock`, which the caller holds.
    pub(crate) fn push_locked(
        &self,
        lock: &MirrorLock,
        remote: &RemoteInfo,
        req: PushReq,
    ) -> Result<PushResp, OpError> {
        let PushReq {
            source_ref, branch, ..
        } = req;
        let name = remote.name.as_str();

        // 1. The store ref (a ref name, and there), the URL and the branch; no git yet.
        self.source_head(&source_ref)?;
        let url = RemoteUrl::parse(&remote.url).map_err(error::url_refused)?;
        let branch = branch.unwrap_or_else(|| default_branch(remote, &source_ref));

        // 2. The mirror, made or rebuilt, and what the last fetch saw of the branch, which the
        // sink's ref-name rule checks.
        let (mirror, mut warnings) = ready(lock, self.ops.store_path())?;
        let seen = mirror
            .tracking(name, &branch)
            .map_err(|e| branch_error(e, &branch))?;

        // 3. The seen commit's changeset in the store, so that the export recognises it.
        if let Some(seen) = seen {
            self.know(&mirror, remote, &branch, seen)?;
        }

        // 4. The staging branch at `seen`, and the export appended to it: `E`.
        let staging = mirror
            .seed_staging(name, &branch, seen)
            .map_err(error::mirror_error)?;
        let report = self.export_into(&mirror, &source_ref, &staging, name, &branch)?;
        let new = report.head_oid;
        warnings.extend(report.warnings.iter().cloned());

        // 5. Nothing to send: no git runs.
        if Some(new) == seen {
            return Ok(response(
                remote,
                &branch,
                PushStatus::UpToDate,
                seen,
                &report,
                warnings,
            ));
        }

        // 6. A fast-forward of `seen`, by construction; anything else is a bug, never sent.
        let git = GitCli::find().map_err(|e| error::transport_error(e, name, Phase::Push))?;
        let failed = |e| error::transport_error(e, name, Phase::Push);
        if let Some(seen) = seen {
            let forward = git
                .is_ancestor(mirror.git_dir(), seen, new)
                .map_err(failed)?;
            if !forward {
                return Err(error::error(
                    ErrorCode::Internal,
                    "a push would not be a fast-forward of what the last fetch saw",
                ));
            }
        }

        // 7. GitHub's limits, on what the push adds, before anything is sent.
        if limits::applies_to(url.host()) {
            let objects = git
                .new_objects(mirror.git_dir(), new, seen)
                .map_err(failed)?;
            let warned = limits::check(&objects).map_err(|e| error::limit_error(e, name))?;
            warnings.extend(warned);
        }

        // 8. The compare-and-swap: the remote moves only from `seen`, to `new`.
        let mut cred = Credential::default();
        cred.token_env.clone_from(&remote.token_env);
        let update = PushUpdate::new(new, branch.clone(), seen);
        let outcome = git
            .push(mirror.git_dir(), &url, &cred, &update)
            .map_err(failed)?;

        // 9. The tracking ref follows the remote; git's housekeeping, whose failure costs disk.
        mirror
            .set_tracking(name, &branch, seen, new)
            .map_err(error::mirror_error)?;
        if let Err(e) = git.gc_auto(mirror.git_dir()) {
            warnings.push(format!("gc_auto_failed: {}", failed(e).message));
        }
        let status = match (outcome, seen) {
            (PushOutcome::AlreadyThere, _) => PushStatus::UpToDate,
            (PushOutcome::Sent, None) => PushStatus::Created,
            (PushOutcome::Sent, Some(_)) => PushStatus::Pushed,
            _ => return Err(error::unknown_variant("PushOutcome")),
        };
        Ok(response(remote, &branch, status, seen, &report, warnings))
    }

    /// The remote named `name`, as `remote list` reads it, or `E_REMOTE_NOT_FOUND`.
    fn named_remote(&self, name: &str) -> Result<RemoteInfo, OpError> {
        let listed = self.remote_list(RemoteListReq::default())?;
        let found = listed
            .remotes
            .into_iter()
            .find(|remote| remote.name == name);
        found.ok_or_else(|| error::remote_not_found(name))
    }

    /// Checks that `source_ref` is a ref name (`E_REF_NAME_INVALID`) and that the store has it
    /// (`E_REV_NOT_FOUND`), as `git export` names them.
    fn source_head(&self, source_ref: &str) -> Result<(), OpError> {
        let refused = |e| error::git_export_error(e, MIRROR_GIT, source_ref, source_ref);
        if swarmops_store::check_ref_name(source_ref).is_err() {
            return Err(refused(ExportError::RefName(source_ref.to_owned())));
        }
        let state = self.ops.repo.get_ref(source_ref).map_err(error::op)?;
        if state.is_none() {
            return Err(refused(ExportError::SourceRef(source_ref.to_owned())));
        }
        Ok(())
    }

    /// Imports `seen`, the mirror's tracking ref of `remote`'s `branch`, objects-only, unless
    /// the store holds its changeset already (a fetch imports the followed branch only).
    fn know(
        &self,
        mirror: &Mirror,
        remote: &RemoteInfo,
        branch: &str,
        seen: GitOid,
    ) -> Result<(), OpError> {
        let known = self
            .ops
            .repo
            .external_id(SYSTEM_GIT, &seen.0)
            .map_err(error::op)?;
        if known.is_some() {
            return Ok(());
        }

        // The tracking ref's history, with the classes the remote allows.
        let git_ref = format!("refs/remotes/{}/{branch}", remote.name);
        let failed = |e: ImportError| error::git_import_error(e, MIRROR_GIT, &git_ref);
        let source = GixSource::open(mirror.git_dir()).map_err(|e| failed(e.into()))?;
        let mut request = ImportRequest::objects_only(git_ref.as_str());
        request.lossy = lossy_set(&remote.lossy);
        let report = swarmops_git::import(&self.ops.repo, &source, &request).map_err(failed)?;
        self.resolved.record_recovered(report.recovered);
        Ok(())
    }

    /// Exports `source_ref` into the mirror's staging branch `staging`, without force; its
    /// divergences are the remote's (`E_REMOTE_DIVERGED`), and the head it read is recorded.
    fn export_into(
        &self,
        mirror: &Mirror,
        source_ref: &str,
        staging: &str,
        remote: &str,
        branch: &str,
    ) -> Result<ExportReport, OpError> {
        let failed = |e: ExportError| error::git_export_error(e, MIRROR_GIT, source_ref, staging);
        let mut sink = GixSink::open(mirror.git_dir()).map_err(|e| failed(e.into()))?;
        let request = ExportRequest::new(source_ref, staging);
        let exported = swarmops_git::export(&self.ops.repo, &mut sink, &request);

        // The outcome: a report, or why the staging branch is no base of the head.
        let report = match exported {
            Ok(report) => report,
            Err(ExportError::Diverged { head, reason, .. }) => {
                self.resolved.record(source_ref, head);
                return Err(self.diverged(reason, head, remote, branch)?);
            }
            Err(ExportError::Sink(SinkError::RefLocked { lock })) => {
                return Err(error::mirror_error(MirrorError::GitLockBusy { file: lock }));
            }
            Err(e) => return Err(failed(e)),
        };
        self.resolved.record(source_ref, report.head);
        self.resolved.record_recovered(recovered(report.recovered));
        Ok(report)
    }

    /// The error of an export into the staging branch that found `reason`, the source's head
    /// being `head`: the store ref is behind the remote, or the two diverged.
    fn diverged(
        &self,
        reason: DivergedReason,
        head: ChangesetId,
        remote: &str,
        branch: &str,
    ) -> Result<OpError, OpError> {
        let why = match reason {
            DivergedReason::SourceMovedBack { exported } => {
                let read = self.ops.repo.snapshot().map_err(error::op)?;
                let behind = read.is_ancestor(head, exported).map_err(error::op)?;
                if behind { "behind" } else { "diverged" }
            }
            DivergedReason::BranchAhead { .. }
            | DivergedReason::BothMoved { .. }
            | DivergedReason::BranchMoved => "diverged",
            DivergedReason::LossyHistory { .. } => "lossy_history",
            DivergedReason::MovedDuringExport { .. } => {
                return Ok(error::error(
                    ErrorCode::Internal,
                    "the staging branch moved during the export, under the mirror's lock",
                ));
            }
            _ => return Err(error::unknown_variant("DivergedReason")),
        };
        let message = format!(
            "{branch} on {remote:?} is no ancestor of what the push would send ({why}): nothing \
             was sent"
        );
        let next = error::hint(format!("swarmops sync {}", error::shell_word(remote)));
        Ok(error::error(ErrorCode::RemoteDiverged, message)
            .with_details(error::details([
                ("remote", remote.into()),
                ("branch", branch.into()),
                ("reason", why.into()),
            ]))
            .with_hint(next))
    }
}

/// The remote branch a push of `source_ref` goes to by default: the remote's followed branch
/// for `main`, the same name for `release/<x>`.
fn default_branch(remote: &RemoteInfo, source_ref: &str) -> String {
    if source_ref == MAIN {
        return remote.branch.clone();
    }
    source_ref.to_owned()
}

/// The store's mirror made or rebuilt under `lock`, its stale git lock files removed; with the
/// warnings each step gives.
fn ready(lock: &MirrorLock, store: &camino::Utf8Path) -> Result<(Mirror, Vec<String>), OpError> {
    let mirror = Mirror::open(store).map_err(error::mirror_error)?;
    let mut warnings = Vec::new();

    // 1. Made, kept, or set aside and made again.
    let rebuilt = mirror.ensure(lock).map_err(error::mirror_error)?;
    if let Rebuilt::SetAside { name } = rebuilt {
        warnings.push(format!("mirror_set_aside: git/{name}"));
    }

    // 2. A dead process's git lock files.
    let removed = mirror.sweep_git_locks(lock).map_err(error::mirror_error)?;
    let removed = removed
        .into_iter()
        .map(|file| format!("stale_git_lock_removed: {file}"));
    warnings.extend(removed);
    Ok((mirror, warnings))
}

/// The mirror's failure `e` reading the tracking ref of `branch`: a name git takes for no ref
/// is `E_INVALID_ARG` `branch_name`.
fn branch_error(e: MirrorError, branch: &str) -> OpError {
    if !matches!(e, MirrorError::Sink(SinkError::BadBranch { .. })) {
        return error::mirror_error(e);
    }
    let why = error::hint(format!(
        "git check-ref-format --branch {} says why git takes no such branch",
        error::shell_word(branch)
    ));
    error::error(
        ErrorCode::InvalidArg,
        format!("{branch:?} is no branch name git takes"),
    )
    .with_details(error::details([
        ("reason", "branch_name".into()),
        ("branch", branch.into()),
    ]))
    .with_hint(why)
}

/// The response of a push of `report`'s head to `remote`'s `branch`, which named `old`.
fn response(
    remote: &RemoteInfo,
    branch: &str,
    status: PushStatus,
    old: Option<GitOid>,
    report: &ExportReport,
    warnings: Vec<String>,
) -> PushResp {
    let mut out = PushResp::new(
        remote.name.clone(),
        branch.to_owned(),
        status,
        report.head_oid.to_string(),
        report.head.to_string(),
        report.objects_written,
        report.rewritten,
    );
    out.old = old.map(|oid| oid.to_string());
    out.warnings = (!warnings.is_empty()).then_some(warnings);
    out
}

/// The import's set of the wire's `classes`, by name.
fn lossy_set(classes: &[LossyClass]) -> LossySet {
    let by_name = |class: &LossyClass| {
        swarmops_git::LossyClass::ALL
            .into_iter()
            .find(|import| import.as_str() == class.as_str())
    };
    classes.iter().filter_map(by_name).collect()
}

/// What a committed snapshot's tail check repaired, as `meta.recovered` gives it: its journal
/// fields.
fn recovered(repair: TailRepair) -> Recovered {
    let mut out = Recovered::default();
    out.log_cut = repair.cut;
    out.frames_listed = repair.frames_listed;
    out.frames_rewritten = repair.frames_rewritten;
    out.off_bytes_dropped = repair.off_bytes_dropped;
    out
}
