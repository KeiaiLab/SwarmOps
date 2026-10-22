//! `ws commit` (§4.5 of the M5 design): the changed files hashed, the blobs the store lacks
//! uploaded, one keyed commit on the workspace's head, then `workspace.json`.
//!
//! The M5 plan's task W1 named the entry point; task W6 writes its body, which until then fails
//! with `E_INVALID_ARG` naming it.

use swarmops_api::{ErrorCode, OpError, WsCommitReq, WsCommitResp};

use crate::Ctx;

/// `ws_commit` in the workspace `req` names.
pub fn commit(_ctx: &Ctx<'_>, _req: WsCommitReq) -> Result<WsCommitResp, OpError> {
    Err(OpError::new(
        ErrorCode::InvalidArg,
        "ws_commit is not built yet (the M5 plan's task W6)",
    ))
}
