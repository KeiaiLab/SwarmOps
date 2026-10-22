//! `ws discard` (§3 of the M5 design): paths restored from the workspace's head.
//!
//! The M5 plan's task W5 wrote the form with paths: each named path is made what the head
//! holds there, a file or every file below a directory, whatever the agent did to it (an edit,
//! a removal, a mode, a file or a symlink put where a directory was); a file the head lacks is
//! removed. Untracked files below a named directory are left. Task W6 writes the form without
//! paths (every change status finds); until then it fails with `E_INVALID_ARG` naming it.

use std::collections::BTreeSet;

use swarmops_api::{ErrorCode, OpError, WsDiscardReq, WsDiscardResp};

use crate::hostfs::{HostFs, HostKind};
use crate::materialize::{
    self, Item, Put, Tree, covered, dirty, holds, host_failed, items_under, normalize,
};
use crate::workspace::{Locate, Workspace, open_locked};
use crate::{Ctx, after};

/// `ws_discard` in the workspace `req` names.
pub fn discard(ctx: &Ctx<'_>, req: WsDiscardReq) -> Result<WsDiscardResp, OpError> {
    // 1. The named paths, within the workspace's prefixes.
    let WsDiscardReq {
        workspace,
        paths,
        status_after,
    } = req;
    if paths.is_empty() {
        return Err(OpError::new(
            ErrorCode::InvalidArg,
            "ws discard without paths is not built yet (the M5 plan's task W6)",
        ));
    }
    let named = normalize(paths)?;
    let (ws, _held) = open_locked(ctx, &Locate::Name(workspace))?;
    if let Some(outside) =
        (named.iter()).find(|path| !ws.file.all && !covered(path, &ws.file.prefixes))
    {
        return Err(OpError::new(
            ErrorCode::InvalidArg,
            format!("{outside:?} is outside the workspace's prefixes: ws materialize it first"),
        ));
    }

    // 2. What to restore, and what the agent put in its way, path by path.
    let head = Tree::new(ctx.store, &ws.file.head);
    let mut restore = Vec::new();
    let mut discarded = BTreeSet::new();
    for path in &named {
        let items = items_under(ctx.store, &head, Some(path))?.unwrap_or_default();
        if items.is_empty() {
            discard_added(ctx.host.as_ref(), &ws, path, &mut discarded)?;
        }
        for item in items {
            clear_way(ctx.host.as_ref(), &ws, path, &item.path, &mut discarded)?;
            if let Some(put) = unrestored(ctx.host.as_ref(), &ws, item)? {
                discarded.insert(put.item.path.clone());
                restore.push(put);
            }
        }
    }

    // 3. The head's files written back.
    restore.sort_by(|a, b| a.item.path.cmp(&b.item.path));
    materialize::put_all(ctx, &ws.dir, ws.file.volume, &restore)?;
    Ok(WsDiscardResp {
        discarded: discarded.into_iter().collect(),
        status_after: after::attach(ctx, &ws, status_after),
    })
}

/// Removes the file or symlink the agent added at `path`, which the head lacks; a directory is
/// left (it may hold a build's output). A symlink or file above `path` is never gone through:
/// `E_WS_DIRTY` naming it.
fn discard_added(
    host: &dyn HostFs,
    ws: &Workspace,
    path: &str,
    discarded: &mut BTreeSet<String>,
) -> Result<(), OpError> {
    // 1. Each directory above it: missing, nothing to remove; not a directory, refused.
    let parts: Vec<&str> = path.split('/').collect();
    for n in 1..parts.len() {
        let ancestor = parts[..n].join("/");
        let there = host
            .symlink_metadata(&ws.dir.join(&ancestor))
            .map_err(|e| host_failed(&ancestor, &e))?;
        match there.map(|meta| meta.kind) {
            None => return Ok(()),
            Some(HostKind::Dir) => {}
            Some(_) => return Err(dirty(vec![ancestor])),
        }
    }

    // 2. The file or symlink itself.
    let abs = ws.dir.join(path);
    let there = host
        .symlink_metadata(&abs)
        .map_err(|e| host_failed(path, &e))?;
    if there.is_some_and(|meta| meta.kind != HostKind::Dir) {
        (host.remove_file(&abs)).map_err(|e| host_failed(path, &e))?;
        discarded.insert(path.to_owned());
    }
    Ok(())
}

/// Removes what the agent put above `item` (below the workspace) where the head has a
/// directory: a file or a symlink at or below the named path `named`. One above `named` is not
/// the caller's to discard: `E_WS_DIRTY`.
fn clear_way(
    host: &dyn HostFs,
    ws: &Workspace,
    named: &str,
    item: &str,
    discarded: &mut BTreeSet<String>,
) -> Result<(), OpError> {
    let parts: Vec<&str> = item.split('/').collect();
    for n in 1..parts.len() {
        // 1. The next directory down.
        let ancestor = parts[..n].join("/");
        let abs = ws.dir.join(&ancestor);
        let there = host
            .symlink_metadata(&abs)
            .map_err(|e| host_failed(&ancestor, &e))?;

        // 2. Missing or a directory: fine. Else removed, if it is the caller's to discard.
        match there.map(|meta| meta.kind) {
            None => return Ok(()),
            Some(HostKind::Dir) => {}
            Some(_) if covered(&ancestor, &[named.to_owned()]) => {
                (host.remove_file(&abs)).map_err(|e| host_failed(&ancestor, &e))?;
                discarded.insert(ancestor);
                return Ok(());
            }
            Some(_) => return Err(dirty(vec![ancestor])),
        }
    }
    Ok(())
}

/// The write that restores `item` when the host does not hold it as the head does (with what
/// the host holds there); none when it does.
fn unrestored(host: &dyn HostFs, ws: &Workspace, item: Item) -> Result<Option<Put>, OpError> {
    // 1. Nothing there: written.
    let abs = ws.dir.join(&item.path);
    let there = host
        .symlink_metadata(&abs)
        .map_err(|e| host_failed(&item.path, &e))?;
    let Some(meta) = there else {
        return Ok(Some(Put { item, there: None }));
    };

    // 2. Something else: written over it.
    let held =
        holds(host, &abs, meta, &item, ws.file.volume).map_err(|e| host_failed(&item.path, &e))?;
    let there = Some(meta.kind);
    Ok((!held).then_some(Put { item, there }))
}
