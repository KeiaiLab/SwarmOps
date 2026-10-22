//! `ws rebase` (§5.3 of the M5 design): the store's `rebase`, then the files moved to its
//! result; on a conflict, the workspace rebasing with markers written.
//!
//! The M5 plan's task W1 named the entry point; task W7 writes its body, which until then fails
//! with `E_INVALID_ARG` naming it.

use swarmops_api::{ErrorCode, OpError, WsRebaseReq, WsRebaseResp};

use crate::Ctx;

/// `ws_rebase` in the workspace `req` names.
pub fn rebase(_ctx: &Ctx<'_>, _req: WsRebaseReq) -> Result<WsRebaseResp, OpError> {
    Err(OpError::new(
        ErrorCode::InvalidArg,
        "ws_rebase is not built yet (the M5 plan's task W7)",
    ))
}
