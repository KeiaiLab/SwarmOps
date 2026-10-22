//! `ws info` (§6.1 of the M5 design): one workspace, from its registry entry and its
//! `workspace.json`.

use std::sync::Arc;

use swarmops_api::{Identity, OpError, StatusAfter, WsInfo, WsInfoReq};

use crate::list::described;
use crate::registry::Registry;
use crate::workspace::{Locate, Workspace, find};
use crate::{Ctx, after};

/// `ws_info`: the workspace `req` names, with its status. `E_WS_NOT_FOUND` for a name the
/// registry does not hold, or a workspace whose state is gone.
pub fn info(ctx: &Ctx<'_>, req: WsInfoReq) -> Result<WsInfo, OpError> {
    // 1. The entry's directory, and the state there.
    let WsInfoReq { workspace: name } = req;
    let registry = Registry::open(&ctx.store_root, Arc::clone(&ctx.fs));
    let at = Locate::Name(name.clone());
    let found = find(&at, Some(&registry), ctx.fs.as_ref())?;

    // 2. Who would act in it: the caller, else its creator.
    let who = match &ctx.who {
        Some(who) => who.clone(),
        None => Identity::parse(&found.file.created_by)?,
    };

    // 3. What it holds, and its status through the one function that builds it.
    let mut info = described(&name, &found.dir, &found.file);
    let ws = Workspace {
        dir: found.dir,
        file: found.file,
        who,
    };
    info.status_after = after::attach(ctx, &ws, StatusAfter::Page);
    Ok(info)
}
