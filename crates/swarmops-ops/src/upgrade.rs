//! `upgrade` (the M6 design §3): a FORMAT 1 store moved to FORMAT 2.

use swarmops_api::{ErrorCode, OpError, UpgradeReq, UpgradeResp};

use crate::error;
use crate::local::Recording;

/// `upgrade` of the store `ops` reads.
/// It needs no identity: it records nothing, no journal frame and no agent.
///
/// Not built yet (the M6 plan's U1): `E_INVALID_ARG`.
pub(crate) fn run(ops: &Recording<'_>, req: UpgradeReq) -> Result<UpgradeResp, OpError> {
    let _ = (ops, req);
    Err(error::error(
        ErrorCode::InvalidArg,
        "upgrade is not built yet",
    ))
}
