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
use swarmops_store::{Recovered, TailRepair};

use crate::error;
use crate::local::{LocalOps, Recording, Resolved};

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
        } = req;
        let failed = |e: ImportError| error::git_import_error(e, &git_path, &git_ref);
        if swarmops_store::check_ref_name(&target_ref).is_err() {
            return Err(failed(ImportError::RefName(target_ref)));
        }
        refuse_mirror(&git_path)?;
        let source = GixSource::open(Path::new(&git_path)).map_err(|e| failed(e.into()))?;
        let mut request = ImportRequest::new(git_ref.as_str(), target_ref, who.agent());
        request.lossy = lossy.into_iter().map(import_class).collect();
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
        } = req;
        let failed = |e: ExportError| error::git_export_error(e, &git_path, &source_ref, &branch);
        if swarmops_store::check_ref_name(&source_ref).is_err() {
            return Err(failed(ExportError::RefName(source_ref.clone())));
        }
        refuse_mirror(&git_path)?;
        let mut sink = GixSink::open(Path::new(&git_path)).map_err(|e| failed(e.into()))?;
        let request = ExportRequest {
            source_ref: source_ref.clone(),
            branch: branch.clone(),
            force,
        };
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
    } = repair;
    Recovered {
        log_cut: cut,
        frames_listed,
        frames_rewritten,
        off_bytes_dropped,
        ..Recovered::default()
    }
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
    } = report;
    GitExportResp {
        exported: commits_written,
        objects_written,
        head_changeset: head.to_string(),
        head_sha: head_oid.to_string(),
        branch,
        old_sha: old.map(|oid| oid.to_string()),
        branch_moved,
        incremental: base.is_some(),
        rewritten,
        warnings,
    }
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
        recovered: _,
    } = report;
    let lossy = lossy
        .iter()
        .map(|commit| LossyCommitOut {
            commit: commit.commit.to_string(),
            changeset: commit.changeset.to_string(),
            classes: commit.classes.iter().map(wire_class).collect(),
        })
        .collect();
    GitImportResp {
        imported,
        skipped,
        head_changeset: head.to_string(),
        git_head_sha: git_head.to_string(),
        target_ref,
        ref_moved,
        old_head: old_head.map(|id| id.to_string()),
        lossy,
        lossy_total,
        warnings,
    }
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
            ChangesetId(ObjectId([1; 32])),
            ChangesetId(ObjectId([2; 32])),
        );
        let (commit, tip) = (GitOid([0xab; 20]), GitOid([0xcd; 20]));
        let classes: LossySet = [
            swarmops_git::LossyClass::TreeOrder,
            swarmops_git::LossyClass::Gitlinks,
        ]
        .into_iter()
        .collect();
        let report = ImportReport {
            imported: 3,
            skipped: 1,
            head,
            git_head: tip,
            target_ref: "release/git".to_owned(),
            old_head: Some(old),
            ref_moved: true,
            lossy: vec![LossyCommit {
                commit,
                changeset: head,
                classes,
            }],
            lossy_total: 7,
            warnings: vec!["a warning".to_owned()],
            recovered: Recovered::default(),
        };
        let expected = GitImportResp {
            imported: 3,
            skipped: 1,
            head_changeset: head.to_string(),
            git_head_sha: "cd".repeat(20),
            target_ref: "release/git".to_owned(),
            ref_moved: true,
            old_head: Some(old.to_string()),
            lossy: vec![LossyCommitOut {
                commit: "ab".repeat(20),
                changeset: head.to_string(),
                classes: vec![LossyClass::Gitlinks, LossyClass::TreeOrder],
            }],
            lossy_total: 7,
            warnings: vec!["a warning".to_owned()],
        };
        assert_eq!(response(report), expected);
    }

    /// An export's report becomes the response field by field: ids in hex, `exported` the commits
    /// written, `old_sha` the commit the branch named, and `incremental` whether the export went on
    /// from an export the branch held; a branch the export created has neither.
    #[test]
    fn an_export_report_becomes_its_response() {
        let (head, base) = (
            ChangesetId(ObjectId([1; 32])),
            ChangesetId(ObjectId([2; 32])),
        );
        let report = ExportReport {
            head,
            head_oid: GitOid([0xcd; 20]),
            branch: "stable".to_owned(),
            old: Some(GitOid([0xab; 20])),
            base: Some(base),
            branch_moved: true,
            commits_written: 3,
            objects_written: 9,
            objects_checked: 4,
            rewritten: 1,
            recovered: TailRepair::default(),
            warnings: vec!["a warning".to_owned()],
        };
        let expected = GitExportResp {
            exported: 3,
            objects_written: 9,
            head_changeset: head.to_string(),
            head_sha: "cd".repeat(20),
            branch: "stable".to_owned(),
            old_sha: Some("ab".repeat(20)),
            branch_moved: true,
            incremental: true,
            rewritten: 1,
            warnings: vec!["a warning".to_owned()],
        };
        assert_eq!(export_response(report.clone()), expected);
        let created = export_response(ExportReport {
            old: None,
            base: None,
            ..report
        });
        assert_eq!((created.old_sha, created.incremental), (None, false));
    }

    /// What a committed snapshot's tail check repaired becomes the record `meta.recovered` gives:
    /// its journal fields as they are, its index fields zero; a clean tail, an empty record.
    #[test]
    fn a_tail_repair_becomes_its_recovered_record() {
        use swarmops_store::LogCut;

        let cut = Some(LogCut { at: 96, bytes: 7 });
        let repair = TailRepair {
            cut,
            frames_listed: 2,
            frames_rewritten: 1,
            off_bytes_dropped: 16,
        };
        let expected = Recovered {
            log_cut: cut,
            frames_listed: 2,
            frames_rewritten: 1,
            off_bytes_dropped: 16,
            ..Recovered::default()
        };
        assert_eq!(recovered(repair), expected);
        assert!(recovered(TailRepair::default()).is_empty());
    }
}
