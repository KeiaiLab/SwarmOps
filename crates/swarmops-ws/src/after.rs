//! The status after a mutation (§6.3 of the M5 design): every workspace mutation's response but
//! `ws_delete`'s carries the first page of the workspace's status after it, unless the request
//! says `none`, so that an agent sees the outcome and the next state in one response.
//!
//! The M5 plan's task W1 named the one function that builds it; task W6 writes its body, which
//! until then gives no status.

use swarmops_api::{StatusAfter, WsStatus};

use crate::Ctx;
use crate::workspace::Workspace;

/// The status of `ws` to put in a mutation's response, as `want` asks: the first page, up to 100
/// changes, or none.
pub fn attach(_ctx: &Ctx<'_>, _ws: &Workspace, _want: StatusAfter) -> Option<WsStatus> {
    None
}
