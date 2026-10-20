//! `git import` (operations spec §1's `git_import` row and §6's import bullet; storage spec §7):
//! a git repository's history brought into the store, by [`Recording::git_import`] and
//! [`LocalOps::git_import`].
//!
//! It is a client-side operation, as `init` is (the M3 plan's decision 7): it reads its source
//! repository on the caller's host, at the path the request gives (`GitImportReq.git_path`), so
//! it is no [`StoreOps`](swarmops_api::StoreOps) operation and no MCP tool, and no server ever
//! receives its request (operations spec §0 and §7). It runs the import of `swarmops-git`
//! ([`swarmops_git::import()`]) on the store's repository, through gix ([`GixSource`]), and maps
//! its failures to the operations spec's codes (`crate::error`'s `git_import_error`, the table
//! in [`LocalOps`]'s documentation).
//!
//! The request names the classes of content it allows lossily with the wire's enum,
//! [`swarmops_api::LossyClass`]; the conversion has its own, [`swarmops_git::LossyClass`]. The
//! two convert by name (`as_str`), and a test holds their lists equal.

use std::path::Path;

use swarmops_api::{GitImportReq, GitImportResp, Identity, LossyClass, LossyCommitOut, OpError};
use swarmops_git::{GixSource, ImportError, ImportReport, ImportRequest};
use swarmops_store::check_ref_name;

use crate::error::git_import_error;
use crate::local::{LocalOps, Recording, Resolved};

impl LocalOps {
    /// `git import` as a request of its own ([`Recording::git_import`]), whose repairs go
    /// unrecorded: to report them, call it through [`LocalOps::recording`].
    pub fn git_import(&self, who: &Identity, req: GitImportReq) -> Result<GitImportResp, OpError> {
        self.recording(&Resolved::default()).git_import(who, req)
    }
}

impl Recording<'_> {
    /// `git import`: every commit that `req.git_ref` reaches in the git repository at
    /// `req.git_path` and the store does not hold becomes a changeset, parents first, and
    /// `req.target_ref` moves to the changeset of `req.git_ref`'s commit, the ref move recorded
    /// for `who`'s agent. A client-side operation, not a [`StoreOps`](swarmops_api::StoreOps)
    /// one: `git_path` is opened on this host, relative to the working directory. The target is
    /// checked first, then the repository opened, so a target that is no ref name is
    /// `E_REF_NAME_INVALID` whatever the path holds. What the import's writes repaired before
    /// their own work, of what earlier writers left, is recorded in the request's [`Resolved`]
    /// (`meta.recovered`), on success. [`LocalOps`]' documentation gives the errors.
    pub fn git_import(&self, who: &Identity, req: GitImportReq) -> Result<GitImportResp, OpError> {
        let GitImportReq {
            git_path,
            git_ref,
            target_ref,
            lossy,
        } = req;
        let failed = |e: ImportError| git_import_error(e, &git_path, &git_ref);
        if check_ref_name(&target_ref).is_err() {
            return Err(failed(ImportError::RefName(target_ref)));
        }
        let source = GixSource::open(Path::new(&git_path)).map_err(|e| failed(e.into()))?;
        let mut request = ImportRequest::new(git_ref.as_str(), target_ref, who.agent());
        request.lossy = lossy.into_iter().map(import_class).collect();
        let report = swarmops_git::import(&self.ops.repo, &source, &request).map_err(failed)?;
        self.resolved.record_recovered(report.recovered);
        Ok(response(report))
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
    use swarmops_store::Recovered;

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
}
