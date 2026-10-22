//! `ws resolve` (§5.3 of the M5 design): conflicting paths marked resolved, refused while a
//! marker line remains unless forced.
//!
//! The M5 plan's task W1 named the entry point; task W7 writes its body, which until then fails
//! with `E_INVALID_ARG` naming it.

use swarmops_api::{ErrorCode, OpError, WsResolveReq, WsResolveResp};

use crate::Ctx;

/// `ws_resolve` in the workspace `req` names.
pub fn resolve(_ctx: &Ctx<'_>, _req: WsResolveReq) -> Result<WsResolveResp, OpError> {
    Err(OpError::new(
        ErrorCode::InvalidArg,
        "ws_resolve is not built yet (the M5 plan's task W7)",
    ))
}
