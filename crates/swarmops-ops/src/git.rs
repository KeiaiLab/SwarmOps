//! `git import` and `git export` (operations spec §1's `git_import` and `git_export` rows and §6's
//! import and export bullets; storage spec §7): a git repository's history brought into the
//! store, by [`Recording::git_import`] and [`LocalOps::git_import`], and the store's history
//! written back into a git repository, by [`Recording::git_export`] and [`LocalOps::git_export`].
//!
//! Both are client-side operations, as `init` is (the M3 plan's decision 7, the M8 plan's
//! decision 1): each reads or writes a git repository on the caller's host, at the path the
//! request gives (`GitImportReq.git_path`, `GitExportReq.git_path`), so neither is a
//! [`StoreOps`](swarmops_api::StoreOps) operation or an MCP tool, and no server ever receives
//! their requests (operations spec §0 and §7). They run the import and the export of
//! `swarmops-git` ([`swarmops_git::import()`], [`swarmops_git::export()`]) on the store's
//! repository, through gix ([`GixSource`], [`GixSink`]), and map their failures to the operations
//! spec's codes (`crate::error`'s `git_import_error` and `git_export_error`, the tables in
//! [`LocalOps`]'s documentation).
//!
//! The request names the classes of content it allows lossily with the wire's enum,
//! [`swarmops_api::LossyClass`]; the conversion has its own, [`swarmops_git::LossyClass`]. The
//! two convert by name (`as_str`), and a test holds their lists equal.

use std::path::Path;

use swarmops_api::{
    ErrorCode, GitExportReq, GitExportResp, GitImportReq, GitImportResp, Identity, LossyClass,
    LossyCommitOut, OpError,
};
use swarmops_git::mirror;
use swarmops_git::{
    ExportError, ExportReport, ExportRequest, GixSink, GixSource, ImportError, ImportReport,
    ImportRequest,
};
use swarmops_repo::RepoError;
use swarmops_store::{Recovered, TailRepair};

use crate::error;
use crate::local::{LocalOps, Recording, RefUse, Resolved, check_ref_use};

impl LocalOps {
    /// `git import` as a request of its own ([`Recording::git_import`]), whose repairs go
    /// unrecorded: to report them, call it through [`LocalOps::recording`].
    pub fn git_import(&self, who: &Identity, req: GitImportReq) -> Result<GitImportResp, OpError> {
        self.recording(&Resolved::default()).git_import(who, req)
    }

    /// `git export` as a request of its own ([`Recording::git_export`]), whose source ref and
    /// repairs go unrecorded: to report them, call it through [`LocalOps::recording`].
    pub fn git_export(&self, req: GitExportReq) -> Result<GitExportResp, OpError> {
        self.recording(&Resolved::default()).git_export(req)
    }
}

impl Recording<'_> {
    /// `git import`: every commit that `req.git_ref` reaches in the git repository at
    /// `req.git_path` and the store does not hold becomes a changeset, parents first, and
    /// `req.target_ref` moves to the changeset of `req.git_ref`'s commit, the ref move recorded
    /// for `who`'s agent. A client-side operation, not a [`StoreOps`](swarmops_api::StoreOps)
    /// one: `git_path` is opened on this host, relative to the working directory. The target is
    /// checked first, then the repository opened, so a target that is no ref name is
    /// `E_REF_NAME_INVALID` whatever the path holds; a path inside a store's `git/`, the mirror,
    /// is then `E_INVALID_ARG` `store_mirror`. What the import's writes repaired before
    /// their own work, of what earlier writers left, is recorded in the request's [`Resolved`]
    /// (`meta.recovered`), on success. [`LocalOps`]' documentation gives the errors.
    pub fn git_import(&self, who: &Identity, req: GitImportReq) -> Result<GitImportResp, OpError> {
        let GitImportReq {
            git_path,
            git_ref,
            target_ref,
            lossy,
            ..
        } = req;
        let failed = |e: ImportError| error::git_import_error(e, &git_path, &git_ref);
        check_git_ref(self.ops, &target_ref, |name| {
            failed(ImportError::RefName(name))
        })?;
        refuse_mirror(&git_path)?;
        let source = GixSource::open(Path::new(&git_path)).map_err(|e| failed(e.into()))?;
        let mut request = ImportRequest::new(git_ref.as_str(), target_ref, who.agent());
        request.lossy = lossy.into_iter().map(import_class).collect();
        request.memo_key.clone_from(&self.ops.memo_key);
        let report = swarmops_git::import(&self.ops.repo, &source, &request).map_err(failed)?;
        self.resolved.record_recovered(report.recovered);
        Ok(response(report))
    }

    /// `git export`: every commit of `req.source_ref`'s history that the branch `req.branch` of
    /// the git repository at `req.git_path` does not reach is written there, parents first, made
    /// durable, and the branch moved to the export of the ref's head by compare-and-swap (storage
    /// spec §7). A client-side operation, not a [`StoreOps`](swarmops_api::StoreOps) one:
    /// `git_path` is opened on this host, relative to the working directory. It needs no identity
    /// (the M8 plan's decision 8): it writes the git repository and two caches of the store, the
    /// derived values and the external id entries, and no journal frame. The source ref is
    /// checked as a ref name first, then the repository opened, so a source that is no ref name is
    /// `E_REF_NAME_INVALID` whatever the path holds; a path inside a store's `git/`, the mirror,
    /// is then `E_INVALID_ARG` `store_mirror`. The head the source ref named, read in a
    /// committed snapshot, is recorded in the request's [`Resolved`] under the ref's name (on
    /// success, and on a divergence, which names it), and what the snapshot's tail check repaired
    /// as its `meta.recovered`, on success. [`LocalOps`]' documentation gives the errors.
    pub fn git_export(&self, req: GitExportReq) -> Result<GitExportResp, OpError> {
        let GitExportReq {
            git_path,
            source_ref,
            branch,
            force,
            ..
        } = req;
        let failed = |e: ExportError| error::git_export_error(e, &git_path, &source_ref, &branch);
        check_git_ref(self.ops, &source_ref, |name| {
            failed(ExportError::RefName(name))
        })?;
        refuse_mirror(&git_path)?;
        let mut sink = GixSink::open(Path::new(&git_path)).map_err(|e| failed(e.into()))?;
        let mut request = ExportRequest::new(&source_ref, &branch);
        request.force = force;
        request.memo_key.clone_from(&self.ops.memo_key);
        let report = swarmops_git::export(&self.ops.repo, &mut sink, &request).map_err(|e| {
            if let ExportError::Diverged { head, .. } = &e {
                self.resolved.record(&source_ref, *head);
            }
            failed(e)
        })?;
        self.resolved.record(&source_ref, report.head);
        self.resolved.record_recovered(recovered(report.recovered));
        Ok(export_response(report))
    }
}

/// Refuses `name` as the store ref a git import moves or a git export or a push reads, before any
/// repository is opened: a remote-tracking ref is `E_REF_NAME_INVALID` `reserved` (only a fetch
/// moves one, and it is no history of the store's own); a name no format takes is `invalid(name)`;
/// a FORMAT 2 name (`feature/x`, `tags/v1`) in a FORMAT 1 store is `E_FORMAT_REQUIRED`.
pub(crate) fn check_git_ref(
    ops: &LocalOps,
    name: &str,
    invalid: impl FnOnce(String) -> OpError,
) -> Result<(), OpError> {
    check_ref_use(name, RefUse::Move)?;
    match ops.repo.check_ref_name(name) {
        Ok(()) => Ok(()),
        Err(RepoError::RefName(name)) => Err(invalid(name)),
        Err(e) => Err(error::op(e)),
    }
}

/// Refuses a `git_path` inside a store's `git/`, the mirror only the remote commands write under
/// its lock (storage spec §2, M13): `E_INVALID_ARG` `reason: "store_mirror"`, so that a person's
/// command never races the mirror's lock.
fn refuse_mirror(git_path: &str) -> Result<(), OpError> {
    if !mirror::inside_store_git(Path::new(git_path)) {
        return Ok(());
    }
    let message = "the repository is a store's git mirror, which only the remote commands write";
    let refused = error::error(ErrorCode::InvalidArg, message)
        .with_details(error::details([("reason", "store_mirror".into())]))
        .with_hint(
            "so fetch, so push and so sync read and write the mirror; give another repository",
        );
    Err(refused)
}

/// What a committed snapshot's tail check repaired ([`TailRepair`]), as the record of repairs
/// that `meta.recovered` gives: its journal fields, and nothing of the index, which the tail
/// check neither repairs nor replays.
fn recovered(repair: TailRepair) -> Recovered {
    let TailRepair {
        cut,
        frames_listed,
        frames_rewritten,
        off_bytes_dropped,
        ..
    } = repair;
    let mut out = Recovered::default();
    out.log_cut = cut;
    out.frames_listed = frames_listed;
    out.frames_rewritten = frames_rewritten;
    out.off_bytes_dropped = off_bytes_dropped;
    out
}

/// The response that gives what `report` says: ids as hex, `exported` the commits written, and
/// `incremental` whether the export went on from an export the branch held (the base).
fn export_response(report: ExportReport) -> GitExportResp {
    let ExportReport {
        head,
        head_oid,
        branch,
        old,
        base,
        branch_moved,
        commits_written,
        objects_written,
        objects_checked: _,
        rewritten,
        recovered: _,
        warnings,
        ..
    } = report;
    let mut out = GitExportResp::new(
        commits_written,
        objects_written,
        head.to_string(),
        head_oid.to_string(),
        branch,
        branch_moved,
        base.is_some(),
        rewritten,
    );
    out.old_sha = old.map(|oid| oid.to_string());
    out.warnings = warnings;
    out
}

/// The conversion's class of the wire's `class`: the one of the same name.
fn import_class(class: LossyClass) -> swarmops_git::LossyClass {
    let same = swarmops_git::LossyClass::ALL
        .into_iter()
        .find(|import| import.as_str() == class.as_str());
    same.expect("both lists of lossy classes hold the same names: a test checks")
}

/// The wire's class of the conversion's `class`: the one of the same name.
fn wire_class(class: swarmops_git::LossyClass) -> LossyClass {
    let same = (LossyClass::ALL.into_iter()).find(|wire| wire.as_str() == class.as_str());
    same.expect("both lists of lossy classes hold the same names: a test checks")
}

/// The response that gives what `report` says: ids as hex, the lossy commits' classes in the
/// order of their names, as their changesets' `git.lossy` names them.
fn response(report: ImportReport) -> GitImportResp {
    let ImportReport {
        imported,
        skipped,
        head,
        git_head,
        target_ref,
        old_head,
        ref_moved,
        lossy,
        lossy_total,
        warnings,
        ..
    } = report;
    let lossy = lossy
        .iter()
        .map(|commit| {
            LossyCommitOut::new(
                commit.commit.to_string(),
                commit.changeset.to_string(),
                commit.classes.iter().map(wire_class).collect(),
            )
        })
        .collect();
    let mut out = GitImportResp::new(
        imported,
        skipped,
        head.to_string(),
        git_head.to_string(),
        target_ref,
        ref_moved,
        lossy,
        lossy_total,
    );
    out.old_head = old_head.map(|id| id.to_string());
    out.warnings = warnings;
    out
}

#[cfg(test)]
mod tests {
    use swarmops_core::{ChangesetId, ObjectId};
    use swarmops_git::{GitOid, LossyCommit, LossySet};

    use super::*;

    /// The wire's classes and the conversion's are one list: the same names in the same order,
    /// each converting to the other and back, and the wire spells each class by that name.
    #[test]
    fn the_api_and_import_lossy_classes_agree() {
        let wire = LossyClass::ALL.map(LossyClass::as_str);
        let import = swarmops_git::LossyClass::ALL.map(swarmops_git::LossyClass::as_str);
        assert_eq!(wire, import);
        for class in LossyClass::ALL {
            assert_eq!(wire_class(import_class(class)), class);
            let spelled = serde_json::to_value(class).expect("a class serializes");
            assert_eq!(spelled, class.as_str());
        }
    }

    /// A report becomes the response field by field: ids in hex, the old head when there was
    /// one, and each lossy commit's classes in the order of their names, whatever order the
    /// request named them in.
    #[test]
    fn a_report_becomes_its_response() {
        let (head, old) = (
            ChangesetId::new(ObjectId::new([1; 32])),
            ChangesetId::new(ObjectId::new([2; 32])),
        );
        let (commit, tip) = (GitOid::new([0xab; 20]), GitOid::new([0xcd; 20]));
        let classes: LossySet = [
            swarmops_git::LossyClass::TreeOrder,
            swarmops_git::LossyClass::Gitlinks,
        ]
        .into_iter()
        .collect();
        let report = ImportReport::new(
            3,
            1,
            head,
            tip,
            "release/git".to_owned(),
            Some(old),
            true,
            vec![LossyCommit::new(commit, head, classes)],
            7,
            vec!["a warning".to_owned()],
            Recovered::default(),
        );
        let mut expected = GitImportResp::new(
            3,
            1,
            head.to_string(),
            "cd".repeat(20),
            "release/git".to_owned(),
            true,
            vec![LossyCommitOut::new(
                "ab".repeat(20),
                head.to_string(),
                vec![LossyClass::Gitlinks, LossyClass::TreeOrder],
            )],
            7,
        );
        expected.old_head = Some(old.to_string());
        expected.warnings = vec!["a warning".to_owned()];
        assert_eq!(response(report), expected);
    }

    /// An export's report becomes the response field by field: ids in hex, `exported` the commits
    /// written, `old_sha` the commit the branch named, and `incremental` whether the export went on
    /// from an export the branch held; a branch the export created has neither.
    #[test]
    fn an_export_report_becomes_its_response() {
        let (head, base) = (
            ChangesetId::new(ObjectId::new([1; 32])),
            ChangesetId::new(ObjectId::new([2; 32])),
        );
        let report = ExportReport::new(
            head,
            GitOid::new([0xcd; 20]),
            "stable".to_owned(),
            Some(GitOid::new([0xab; 20])),
            Some(base),
            true,
            3,
            9,
            4,
            1,
            TailRepair::default(),
            vec!["a warning".to_owned()],
        );
        let mut expected = GitExportResp::new(
            3,
            9,
            head.to_string(),
            "cd".repeat(20),
            "stable".to_owned(),
            true,
            true,
            1,
        );
        expected.old_sha = Some("ab".repeat(20));
        expected.warnings = vec!["a warning".to_owned()];
        assert_eq!(export_response(report.clone()), expected);
        let mut export_report = report;
        export_report.old = None;
        export_report.base = None;
        let created = export_response(export_report);
        assert_eq!((created.old_sha, created.incremental), (None, false));
    }

    /// What a committed snapshot's tail check repaired becomes the record `meta.recovered` gives:
    /// its journal fields as they are, its index fields zero; a clean tail, an empty record.
    #[test]
    fn a_tail_repair_becomes_its_recovered_record() {
        use swarmops_store::LogCut;

        let cut = Some(LogCut::new(96, 7));
        let mut repair = TailRepair::default();
        repair.cut = cut;
        repair.frames_listed = 2;
        repair.frames_rewritten = 1;
        repair.off_bytes_dropped = 16;
        let mut expected = Recovered::default();
        expected.log_cut = cut;
        expected.frames_listed = 2;
        expected.frames_rewritten = 1;
        expected.off_bytes_dropped = 16;
        assert_eq!(recovered(repair), expected);
        assert!(recovered(TailRepair::default()).is_empty());
    }
}
