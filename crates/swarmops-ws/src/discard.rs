//! `ws discard` (§3 of the M5 design): paths restored from the workspace's head.
//!
//! The M5 plan's task W1 named the entry point; task W5 writes its body for named paths, and
//! task W6 for every change status finds; until then it fails with `E_INVALID_ARG` naming it.

use swarmops_api::{ErrorCode, OpError, WsDiscardReq, WsDiscardResp};

use crate::Ctx;

/// `ws_discard` in the workspace `req` names.
pub fn discard(_ctx: &Ctx<'_>, _req: WsDiscardReq) -> Result<WsDiscardResp, OpError> {
    Err(OpError::new(
        ErrorCode::InvalidArg,
        "ws_discard is not built yet (the M5 plan's task W5)",
    ))
}
