//! `ws delete` (§7.2 of the M5 design): the directory removed, then the registry entry; edits
//! and unlanded commits refused unless forced or rescued.
//!
//! ```text
//! rescue? commit the edits ─▶ lock ─▶ edits or commits not landed? refuse (unless forced)
//!   ─▶ the tree, then .swarmws/ but workspace.json, then workspace.json ─▶ the lock dropped
//!   ─▶ what is left of the directory ─▶ the registry entry (WS4)
//! ```
//!
//! `workspace.json` goes last but for the lock: a delete cut before it leaves a workspace that
//! the same delete finishes; one cut after leaves only empty directories and the entry, which
//! `ws delete --force` removes.

use std::io;
use std::sync::Arc;

use camino::Utf8Path;
use serde_json::{Map, Value};
use swarmops_api::{ErrorCode, OpError, StatusAfter, WsCommitReq, WsDeleteReq, WsDeleteResp};
use swarmops_store::FsMethod;

use crate::hostfs::{HostFs, HostKind};
use crate::lock::LOCK_FILE;
use crate::materialize::{dirty, host_failed};
use crate::registry::Registry;
use crate::scan::Want;
use crate::state::{self, SWARMWS_DIR, WORKSPACE_JSON};
use crate::status::changes_of;
use crate::workspace::{Locate, open_locked};
use crate::{Ctx, commit, io_error};

/// The message of a rescue changeset of workspace `name`.
pub(crate) fn rescue_message(name: &str) -> String {
    format!("rescue: workspace {name} removed with uncommitted edits")
}

/// `ws_delete` in the workspace `req` names.
pub fn delete(ctx: &Ctx<'_>, req: WsDeleteReq) -> Result<WsDeleteResp, OpError> {
    // 1. The edits kept as a rescue changeset first, when asked: a commit, under its own lock.
    let WsDeleteReq {
        workspace: name,
        force,
        rescue,
    } = req;
    let rescued = if rescue { rescued(ctx, &name)? } else { None };

    // 2. The workspace under its lock; one whose state is gone is an orphan of its entry.
    let at = Locate::Name(name.clone());
    let (ws, held) = match open_locked(ctx, &at) {
        Ok(opened) => opened,
        Err(e) if e.code == ErrorCode::WsNotFound.as_str() && force => {
            return orphan(ctx, &name, e);
        }
        Err(e) => return Err(e),
    };

    // 3. Nothing lost unless forced: no edits, and, unless rescued, no commit past the base.
    if !force {
        let want = Want {
            paths: Vec::new(),
            full: false,
            extra: Vec::new(),
        };
        let changed = changes_of(ctx, &ws, &want)?;
        if !changed.is_empty() {
            let paths = changed.into_iter().map(|change| change.path).collect();
            return Err(dirty(paths).with_hint(format!(
                "swarmops ws commit -m <message>, or swarmops ws delete {name} --rescue"
            )));
        }
        if !rescue && ws.file.head != ws.file.base {
            return Err(unlanded(&name, &ws.file.head, &ws.file.base));
        }
    }

    // 4. The tree, then the private state with `workspace.json` last.
    let host = ctx.host.as_ref();
    let swarmws = ws.dir.join(SWARMWS_DIR);
    let keep = [SWARMWS_DIR];
    remove_below(host, &ws.dir, &keep)?;
    remove_below(host, &swarmws, &[WORKSPACE_JSON, LOCK_FILE])?;
    let state_file = state::path_in(&ws.dir);
    (ctx.fs.remove(state_file.as_std_path()))
        .map_err(|e| io_error::io_failed(FsMethod::Remove, &shown(WORKSPACE_JSON), &e))?;
    (ctx.fs.fsync_dir(swarmws.as_std_path()))
        .map_err(|e| io_error::io_failed(FsMethod::FsyncDir, SWARMWS_DIR, &e))?;

    // 5. The lock dropped, then what is left of the directory, then the entry (WS4).
    drop(held);
    remove_below(host, &ws.dir, &[])?;
    remove_dir(host, &ws.dir)?;
    Registry::open(&ctx.store_root, Arc::clone(&ctx.fs)).delete(&name)?;
    Ok(WsDeleteResp {
        deleted: name,
        head: ws.file.head,
        rescued,
    })
}

/// The rescue changeset of workspace `name`'s edits, committed on its head; none when it has
/// no edit.
fn rescued(ctx: &Ctx<'_>, name: &str) -> Result<Option<String>, OpError> {
    let req = WsCommitReq {
        workspace: name.to_owned(),
        message: rescue_message(name),
        paths: Vec::new(),
        modes: std::collections::BTreeMap::new(),
        status_after: StatusAfter::None,
    };
    match commit(ctx, req) {
        Ok(done) => Ok(Some(done.changeset)),
        Err(e) if e.code == ErrorCode::EmptyCommit.as_str() => Ok(None),
        Err(e) => Err(e),
    }
}

/// `ws delete --force` of workspace `name`, whose state is gone (`gone`, the error that said
/// so): its `.swarmws/` and its directory removed when nothing else is left there, then its
/// entry. A directory holding anything else is not the workspace's to remove: `gone` again.
fn orphan(ctx: &Ctx<'_>, name: &str, gone: OpError) -> Result<WsDeleteResp, OpError> {
    // 1. The entry: none, and there is nothing to delete.
    let registry = Registry::open(&ctx.store_root, Arc::clone(&ctx.fs));
    let Some(entry) = registry.read(name)? else {
        return Err(gone);
    };

    // 2. The directory, when only a `.swarmws/` (or nothing) is left in it.
    let host = ctx.host.as_ref();
    let dir = Utf8Path::new(&entry.path);
    match host.read_dir(dir) {
        Ok(names) if names.iter().all(|n| n == SWARMWS_DIR) => {
            remove_below(host, dir, &[])?;
            remove_dir(host, dir)?;
        }
        Ok(_) => return Err(gone),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(host_failed(".", &e)),
    }

    // 3. The entry, after the directory (WS4).
    registry.delete(name)?;
    Ok(WsDeleteResp {
        deleted: name.to_owned(),
        head: String::new(),
        rescued: None,
    })
}

/// Removes everything below directory `dir` but the names `keep` directly in it; a symlink is
/// removed, never followed.
fn remove_below(host: &dyn HostFs, dir: &Utf8Path, keep: &[&str]) -> Result<(), OpError> {
    let names = match host.read_dir(dir) {
        Ok(names) => names,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(host_failed(dir.as_str(), &e)),
    };
    for name in names {
        if keep.contains(&name.as_str()) {
            continue;
        }
        remove_all(host, &dir.join(name))?;
    }
    Ok(())
}

/// Removes `path` and, when it is a directory (not a symlink to one), everything below it.
fn remove_all(host: &dyn HostFs, path: &Utf8Path) -> Result<(), OpError> {
    let meta = (host.symlink_metadata(path)).map_err(|e| host_failed(path.as_str(), &e))?;
    let Some(meta) = meta else {
        return Ok(());
    };
    if meta.kind != HostKind::Dir {
        return (host.remove_file(path)).map_err(|e| host_failed(path.as_str(), &e));
    }
    remove_below(host, path, &[])?;
    remove_dir(host, path)
}

/// Removes the directory at `path`, emptied before: `E_IO` when something was put in it
/// meanwhile.
fn remove_dir(host: &dyn HostFs, path: &Utf8Path) -> Result<(), OpError> {
    match host.remove_dir_if_empty(path) {
        Ok(true) => Ok(()),
        Ok(false) => Err(OpError::new(
            ErrorCode::Io,
            format!("{path} was written to while it was deleted"),
        )),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(host_failed(path.as_str(), &e)),
    }
}

/// `E_WS_DIRTY` for workspace `name`, whose `head` holds commits past its `base` that were not
/// landed.
fn unlanded(name: &str, head: &str, base: &str) -> OpError {
    OpError::new(
        ErrorCode::WsDirty,
        format!("workspace {name:?} holds commits that were not landed (head {head}, base {base})"),
    )
    .with_details(Map::from_iter([
        ("paths".to_owned(), Value::Array(Vec::new())),
        ("head".to_owned(), Value::from(head)),
        ("base".to_owned(), Value::from(base)),
    ]))
    .with_hint(format!(
        "swarmops ws land, or swarmops ws delete {name} --rescue (the commits stay in the store)"
    ))
}

/// `name` in `.swarmws/`, as errors name it.
fn shown(name: &str) -> String {
    format!("{SWARMWS_DIR}/{name}")
}
