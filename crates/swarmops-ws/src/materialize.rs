//! Materialisation (§3 of the M5 design): only what changed between two trees written, never
//! over an edit or through a symlink, resumable through an intent record; and `ws materialize`,
//! which adds prefixes.
//!
//! The M5 plan's task W1 named the entry point; task W5 writes its body, which until then fails
//! with `E_INVALID_ARG` naming it.

use swarmops_api::{ErrorCode, OpError, WsMaterializeReq, WsMaterializeResp};

use crate::Ctx;

/// `ws_materialize` in the workspace `req` names.
pub fn materialize(_ctx: &Ctx<'_>, _req: WsMaterializeReq) -> Result<WsMaterializeResp, OpError> {
    Err(OpError::new(
        ErrorCode::InvalidArg,
        "ws_materialize is not built yet (the M5 plan's task W5)",
    ))
}
