//! `ws status` (§4.3 of the M5 design): the workspace's changes against its head, found by stat
//! and the index, and how far its target has moved since its base.
//!
//! The M5 plan's task W1 named the entry point; task W6 writes its body, which until then fails
//! with `E_INVALID_ARG` naming it.

use swarmops_api::{ErrorCode, OpError, WsStatus, WsStatusReq};

use crate::Ctx;

/// `ws_status` in the workspace `req` names.
pub fn status(_ctx: &Ctx<'_>, _req: WsStatusReq) -> Result<WsStatus, OpError> {
    Err(OpError::new(
        ErrorCode::InvalidArg,
        "ws_status is not built yet (the M5 plan's task W6)",
    ))
}
