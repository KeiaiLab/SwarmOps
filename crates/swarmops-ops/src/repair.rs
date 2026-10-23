//! `repair` (the M6 design §7): the journal's projections rebuilt, damaged objects
//! quarantined, damaged cache entries dropped, and a verify after it.

use swarmops_api::{ErrorCode, OpError, RepairReq, RepairResp};

use crate::error;
use crate::local::Recording;

/// `repair` on the store `ops` reads.
/// It needs no identity: it records nothing, no journal frame and no agent.
///
/// Not built yet (the M6 plan's R2): `E_INVALID_ARG`.
pub(crate) fn run(ops: &Recording<'_>, req: RepairReq) -> Result<RepairResp, OpError> {
    let _ = (ops, req);
    Err(error::error(
        ErrorCode::InvalidArg,
        "repair is not built yet",
    ))
}
