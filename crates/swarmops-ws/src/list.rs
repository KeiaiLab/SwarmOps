//! `ws list` (§2.3 of the M5 design): the store's workspaces, from the registry alone; with
//! `check`, each `workspace.json` too, and a missing directory reported as an orphan.

use std::sync::Arc;

use camino::Utf8Path;
use swarmops_api::{ErrorCode, OpError, WsInfo, WsList, WsListReq};

use crate::Ctx;
use crate::registry::{Entry, Registry};
use crate::state::{self, WorkspaceFile};

/// `ws_list`: every workspace of the store, by name. Without `check` only the registry is read
/// (no directory is touched), so `base` and `head` are empty; with `check` each workspace's
/// `workspace.json` is read too, and one that is gone (its directory, or its state in it) is
/// an orphan, its entry kept.
#[expect(
    clippy::needless_pass_by_value,
    reason = "every ws operation takes its request by value: the M5 design's frozen signature"
)]
pub fn list(ctx: &Ctx<'_>, req: WsListReq) -> Result<WsList, OpError> {
    // 1. The entries, by name.
    let registry = Registry::open(&ctx.store_root, Arc::clone(&ctx.fs));
    let entries = registry.list()?;

    // 2. The registry alone, unless asked to check.
    if !req.check {
        let workspaces = entries.into_iter().map(listed).collect();
        return Ok(WsList::new(workspaces));
    }

    // 3. Each workspace's state, or an orphan.
    let mut workspaces = Vec::with_capacity(entries.len());
    for entry in entries {
        let dir = Utf8Path::new(&entry.path);
        match state::read(ctx.fs.as_ref(), dir) {
            Ok(file) => workspaces.push(described(&entry.name, dir, &file)),
            Err(e) if e.code == ErrorCode::WsNotFound.as_str() => {
                let mut out = listed(entry);
                out.orphan = true;
                workspaces.push(out);
            }
            Err(e) => return Err(e),
        }
    }
    Ok(WsList::new(workspaces))
}

/// Workspace `name` at `dir`, as its `workspace.json` describes it: what `ws info` answers and
/// `ws list --check` lists.
pub(crate) fn described(name: &str, dir: &Utf8Path, file: &WorkspaceFile) -> WsInfo {
    let mut out = WsInfo::new(
        name.to_owned(),
        dir.to_string(),
        file.base.clone(),
        file.head.clone(),
    );
    out.prefixes.clone_from(&file.prefixes);
    out.all = file.all;
    out
}

/// A workspace as its registry entry alone tells: its name and directory.
fn listed(entry: Entry) -> WsInfo {
    WsInfo::new(entry.name, entry.path, String::new(), String::new())
}
