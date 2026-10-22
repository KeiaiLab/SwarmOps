//! `ws land` (§5.4 of the M5 design): without a check, a land of the workspace's chain; with
//! one, rebase, check and an exact land, up to `max_attempts`, under one land key.
//!
//! The M5 plan's task W1 named the entry point; task W7 writes its body, which until then fails
//! with `E_INVALID_ARG` naming it.

use swarmops_api::{ErrorCode, OpError, WsLandReq, WsLandResp};

use crate::Ctx;

/// `ws_land` in the workspace `req` names.
pub fn land(_ctx: &Ctx<'_>, _req: WsLandReq) -> Result<WsLandResp, OpError> {
    Err(OpError::new(
        ErrorCode::InvalidArg,
        "ws_land is not built yet (the M5 plan's task W7)",
    ))
}
