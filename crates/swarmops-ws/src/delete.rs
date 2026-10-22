//! `ws delete` (§7.2 of the M5 design): the directory removed, then the registry entry; edits
//! and unlanded commits refused unless forced or rescued.
//!
//! The M5 plan's task W1 named the entry point; task W9 writes its body, which until then fails
//! with `E_INVALID_ARG` naming it.

use swarmops_api::{ErrorCode, OpError, WsDeleteReq, WsDeleteResp};

use crate::Ctx;

/// `ws_delete` in the workspace `req` names.
pub fn delete(_ctx: &Ctx<'_>, _req: WsDeleteReq) -> Result<WsDeleteResp, OpError> {
    Err(OpError::new(
        ErrorCode::InvalidArg,
        "ws_delete is not built yet (the M5 plan's task W9)",
    ))
}
