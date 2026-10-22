//! `rebase` (M5; operations spec §1's `rebase` row, §3): a change merged three-way onto a ref's
//! head and recorded as one changeset whose parent is that head, moving no ref. It is a land's
//! preparation (request checks, the merge, the resolutions) with a commit as its last step, so
//! that the tree an agent checks is the tree `land --expect-target` publishes.
//!
//! Its request's own rules are checked before anything is read: `onto` is `main` or
//! `release/<x>` (`E_REF_NAME_INVALID`), a key is a uuid v7 as a commit's (`E_INVALID_ARG`), and
//! the resolutions are shaped as a land's. `swarmops mcp` also requires full ids for `head`,
//! `base` and `resolutions.against`.

use swarmops_api::{Identity, OpError, RebaseReq, RebaseResp, RebaseStatus};
use swarmops_core::bytes::to_u64;
use swarmops_repo::RepoError;

use crate::error::op;
use crate::local::Recording;

/// `rebase` as `who`, on the store `ops` reads.
pub(crate) fn run(
    ops: &Recording<'_>,
    who: &Identity,
    req: RebaseReq,
) -> Result<RebaseResp, OpError> {
    // 1. The ref it rebases onto, by the store's grammar, before anything is read.
    swarmops_store::check_ref_name(&req.onto).map_err(|e| op(RepoError::from(e)))?;

    // 2. The rebase.
    let outcome = ops.rebase_change(who, req)?;

    // 3. What it did, ids in full.
    let status = match outcome.status {
        swarmops_repo::RebaseStatus::Rebased => RebaseStatus::Rebased,
        swarmops_repo::RebaseStatus::Replayed => RebaseStatus::Replayed,
        swarmops_repo::RebaseStatus::UpToDate => RebaseStatus::UpToDate,
        swarmops_repo::RebaseStatus::Absorbed => RebaseStatus::Absorbed,
    };
    Ok(RebaseResp {
        status,
        changeset: outcome.changeset.to_string(),
        root: outcome.root.to_string(),
        onto: outcome.onto.to_string(),
        base: outcome.base.to_string(),
        files_changed: to_u64(outcome.files_changed),
        auto_merged: (outcome.auto_merged.iter())
            .map(ToString::to_string)
            .collect(),
    })
}
