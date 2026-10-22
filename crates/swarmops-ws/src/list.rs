//! `ws list` (§2.3 of the M5 design): the store's workspaces, from the registry alone; with
//! `check`, each `workspace.json` too, and a missing directory reported as an orphan.
//!
//! The M5 plan's task W1 named the entry point; task W4 writes its body, which until then fails
//! with `E_INVALID_ARG` naming it.

use swarmops_api::{ErrorCode, OpError, WsList, WsListReq};

use crate::Ctx;

/// `ws_list` in the workspace `req` names.
pub fn list(_ctx: &Ctx<'_>, _req: WsListReq) -> Result<WsList, OpError> {
    Err(OpError::new(
        ErrorCode::InvalidArg,
        "ws_list is not built yet (the M5 plan's task W4)",
    ))
}
