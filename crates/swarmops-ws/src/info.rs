//! `ws info` (§6.1 of the M5 design): one workspace, from its registry entry and its
//! `workspace.json`.
//!
//! The M5 plan's task W1 named the entry point; task W4 writes its body, which until then fails
//! with `E_INVALID_ARG` naming it.

use swarmops_api::{ErrorCode, OpError, WsInfo, WsInfoReq};

use crate::Ctx;

/// `ws_info` in the workspace `req` names.
pub fn info(_ctx: &Ctx<'_>, _req: WsInfoReq) -> Result<WsInfo, OpError> {
    Err(OpError::new(
        ErrorCode::InvalidArg,
        "ws_info is not built yet (the M5 plan's task W4)",
    ))
}
