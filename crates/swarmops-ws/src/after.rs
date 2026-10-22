//! The status after a mutation (§6.3 of the M5 design): every workspace mutation's response but
//! `ws_delete`'s carries the first page of the workspace's status after it, unless the request
//! says `none`, so that an agent sees the outcome and the next state in one response.

use swarmops_api::{StatusAfter, WsStatus};

use crate::Ctx;
use crate::scan::Want;
use crate::status;
use crate::workspace::Workspace;

/// The changes the first page after a mutation holds.
pub const AFTER_LIMIT: usize = 100;

/// The status of `ws` to put in a mutation's response, as `want` asks: the first page, up to 100
/// changes, or none. The mutation is done: a status that fails is left out, not reported.
pub fn attach(ctx: &Ctx<'_>, ws: &Workspace, want: StatusAfter) -> Option<WsStatus> {
    if want == StatusAfter::None {
        return None;
    }
    status::page(ctx, ws, &Want::default(), AFTER_LIMIT, None).ok()
}
