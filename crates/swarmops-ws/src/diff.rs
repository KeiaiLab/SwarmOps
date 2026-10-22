//! `ws diff` (§4.4 of the M5 design): the workspace's edits against its head, as the store's
//! diff type, rendered by the store's line diff.
//!
//! The M5 plan's task W1 named the entry point; task W6 writes its body, which until then fails
//! with `E_INVALID_ARG` naming it.

use swarmops_api::{Diff, ErrorCode, OpError, WsDiffReq};

use crate::Ctx;

/// `ws_diff` in the workspace `req` names.
pub fn diff(_ctx: &Ctx<'_>, _req: WsDiffReq) -> Result<Diff, OpError> {
    Err(OpError::new(
        ErrorCode::InvalidArg,
        "ws_diff is not built yet (the M5 plan's task W6)",
    ))
}
