//! `ws create` (§3.1 of the M5 design): the registry entry, then the directory and its
//! `.swarmws/`, then the tree of `base` under the prefixes, written out.
//!
//! The M5 plan's task W1 named the entry point; task W5 writes its body, which until then fails
//! with `E_INVALID_ARG` naming it.

use swarmops_api::{ErrorCode, OpError, WsCreateReq, WsInfo};

use crate::Ctx;

/// `ws_create` in the workspace `req` names.
pub fn create(_ctx: &Ctx<'_>, _req: WsCreateReq) -> Result<WsInfo, OpError> {
    Err(OpError::new(
        ErrorCode::InvalidArg,
        "ws_create is not built yet (the M5 plan's task W5)",
    ))
}
