//! `ws discard` (§3 of the M5 design): paths restored from the workspace's head.
//!
//! The M5 plan's task W5 wrote the form with paths: each named path is made what the head
//! holds there, a file or every file below a directory, whatever the agent did to it (an edit,
//! a removal, a mode, a file or a symlink put where a directory was); a file the head lacks is
//! removed. Untracked files below a named directory are left. Task W6 wrote the form without
//! paths: every change status finds, an added file removed (an ignored one is no change).

use std::collections::BTreeSet;

use swarmops_api::{ErrorCode, OpError, WsDiscardReq, WsDiscardResp};

use crate::hostfs::{HostFs, HostKind};
use crate::materialize::{self, Item, Put, Tree};
use crate::portable::{self, HOST};
use crate::scan::Want;
use crate::workspace::{self, Locate, Workspace};
use crate::{Ctx, after, status};

/// `ws_discard` in the workspace `req` names.
pub fn discard(ctx: &Ctx<'_>, req: WsDiscardReq) -> Result<WsDiscardResp, OpError> {
    // 1. The named paths, else every changed one, within the workspace's prefixes.
    let WsDiscardReq {
        workspace,
        paths,
        status_after,
        ..
    } = req;
    let asked = if paths.is_empty() {
        None
    } else {
        Some(materialize::normalize(paths)?)
    };
    let (ws, _held) = workspace::open_locked(ctx, &Locate::Name(workspace))?;
    let named = match asked {
        Some(named) => named,
        None => changed(ctx, &ws)?,
    };
    if let Some(outside) =
        (named.iter()).find(|path| !ws.file.all && !materialize::covered(path, &ws.file.prefixes))
    {
        return Err(OpError::new(
            ErrorCode::InvalidArg,
            format!("{outside:?} is outside the workspace's prefixes: ws materialize it first"),
        ));
    }

    // 2. The head's files written back.
    let discarded = restore(ctx, &ws, &ws.file.head, &named)?;
    let mut out = WsDiscardResp::new(discarded.into_iter().collect());
    out.status_after = after::attach(ctx, &ws, status_after);
    Ok(out)
}

/// Makes each of the `named` paths of workspace `ws` (whose lock the caller holds) what
/// changeset `rev` holds there, whatever the agent did to it; a file `rev` lacks is removed. The
/// paths it changed. What `ws discard` does from the head, and what a rebase's `--abort` and
/// `ws resolve --take` do from one side.
pub(crate) fn restore(
    ctx: &Ctx<'_>,
    ws: &Workspace,
    rev: &str,
    named: &[String],
) -> Result<BTreeSet<String>, OpError> {
    // 1. What `rev` holds under each name, and the portability rules over it, before anything
    //    is touched.
    let tree = Tree::new(ctx.store, rev);
    let mut found = Vec::new();
    for path in named {
        let items = materialize::items_under(ctx.store, &tree, Some(path))?.unwrap_or_default();
        found.push((path, items));
    }
    let held: Vec<String> = (found.iter())
        .flat_map(|(_, items)| items.iter().map(|item| item.path.clone()))
        .collect();
    portable::check(&held, None, HOST)?;

    // 2. What the agent put in the way, path by path.
    let mut restore = Vec::new();
    let mut discarded = BTreeSet::new();
    for (path, items) in found {
        if items.is_empty() {
            discard_added(ctx.host.as_ref(), ws, path, &mut discarded)?;
        }
        for item in items {
            clear_way(ctx.host.as_ref(), ws, path, &item.path, &mut discarded)?;
            if let Some(put) = unrestored(ctx.host.as_ref(), ws, item)? {
                discarded.insert(put.item.path.clone());
                restore.push(put);
            }
        }
    }

    // 3. Its files written back.
    restore.sort_by(|a, b| a.item.path.cmp(&b.item.path));
    materialize::put_all(ctx, &ws.dir, ws.file.volume, &restore)?;
    Ok(discarded)
}

/// Every path workspace `ws`'s status finds changed, as discard takes paths.
fn changed(ctx: &Ctx<'_>, ws: &Workspace) -> Result<Vec<String>, OpError> {
    let changes = status::changes_of(ctx, ws, &Want::default())?;
    materialize::normalize(changes.into_iter().map(|change| change.path).collect())
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
            .map_err(|e| materialize::host_failed(&ancestor, &e))?;
        match there.map(|meta| meta.kind) {
            None => return Ok(()),
            Some(HostKind::Dir) => {}
            Some(_) => return Err(materialize::dirty(vec![ancestor])),
        }
    }

    // 2. The file or symlink itself.
    let abs = ws.dir.join(path);
    let there = host
        .symlink_metadata(&abs)
        .map_err(|e| materialize::host_failed(path, &e))?;
    if there.is_some_and(|meta| meta.kind != HostKind::Dir) {
        (host.remove_file(&abs)).map_err(|e| materialize::host_failed(path, &e))?;
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
            .map_err(|e| materialize::host_failed(&ancestor, &e))?;

        // 2. Missing or a directory: fine. Else removed, if it is the caller's to discard.
        match there.map(|meta| meta.kind) {
            None => return Ok(()),
            Some(HostKind::Dir) => {}
            Some(_) if materialize::covered(&ancestor, &[named.to_owned()]) => {
                (host.remove_file(&abs)).map_err(|e| materialize::host_failed(&ancestor, &e))?;
                discarded.insert(ancestor);
                return Ok(());
            }
            Some(_) => return Err(materialize::dirty(vec![ancestor])),
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
        .map_err(|e| materialize::host_failed(&item.path, &e))?;
    let Some(meta) = there else {
        return Ok(Some(Put { item, there: None }));
    };

    // 2. Something else: written over it.
    let held = materialize::holds(host, &abs, meta, &item, ws.file.volume)
        .map_err(|e| materialize::host_failed(&item.path, &e))?;
    let there = Some(meta.kind);
    Ok((!held).then_some(Put { item, there }))
}
