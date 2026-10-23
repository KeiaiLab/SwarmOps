//! `gc` (the M6 design §8): the store's leftovers removed, counted by kind.

use swarmops_api::{ErrorCode, GcReq, GcResp, OpError};

use crate::error;
use crate::local::Recording;

/// `gc` on the store `ops` reads.
/// It needs no identity: it records nothing, no journal frame and no agent.
///
/// Not built yet (the M6 plan's G1): `E_INVALID_ARG`.
pub(crate) fn run(ops: &Recording<'_>, req: GcReq) -> Result<GcResp, OpError> {
    let _ = (ops, req);
    Err(error::error(ErrorCode::InvalidArg, "gc is not built yet"))
}
