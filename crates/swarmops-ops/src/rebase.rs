//! `rebase` (M5; operations spec §1's `rebase` row, §3): a change merged three-way onto a ref's
//! head and recorded as one changeset whose parent is that head, moving no ref. It is a land's
//! preparation (request checks, the merge, the resolutions) with a commit as its last step, so
//! that the tree an agent checks is the tree `land --expect-target` publishes.
//!
//! The M5 plan's task W1 froze the entry point; task W2 writes its body, which until then fails
//! with `E_INVALID_ARG` naming it.

use swarmops_api::{ErrorCode, Identity, OpError, RebaseReq, RebaseResp};

use crate::local::Recording;

/// `rebase` as `who`, on the store `ops` reads.
pub(crate) fn run(
    _ops: &Recording<'_>,
    _who: &Identity,
    _req: RebaseReq,
) -> Result<RebaseResp, OpError> {
    Err(OpError::new(
        ErrorCode::InvalidArg,
        "rebase is not built yet (the M5 plan's task W2)",
    ))
}
