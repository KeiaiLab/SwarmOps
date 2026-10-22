//! `ws create` (§3.1 of the M5 design): the registry entry, then the directory and its
//! `.swarmws/`, then the tree of `base` under the prefixes, written out.
//!
//! ```text
//! registry entry linked (WS4) → .swarmws/journal/ → volume probed → tree listed and checked
//!   → journal/materialize.json (WS3) → workspace.json → files → the intent removed
//! ```
//!
//! The same request again is the same workspace (`created: false`). A create killed before its
//! `workspace.json` exists is finished by the same create again (the directory holds only its
//! `.swarmws/` then); one killed after, by the next open.
//!
//! The M5 plan's task W9 adds the bootstrap (`from`'s `.worktreeinclude`, the repository's own
//! command) after the files.

use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use camino::Utf8Path;
use serde_json::{Map, Value};
use swarmops_api::{ErrorCode, InfoReq, OpError, ResolveRevReq, RevKind, WsCreateReq, WsInfo};
use swarmops_store::FsMethod;

use crate::hostfs::HostFs;
use crate::list::described;
use crate::materialize::{self, Start, host_failed, normalize};
use crate::pending::{self, Intent};
use crate::registry::{ENTRY_FORMAT, Entry, Linked, Registry};
use crate::state::{self, SWARMWS_DIR, State, WORKSPACE_FORMAT, WorkspaceFile};
use crate::workspace::{Locate, Workspace, host_path};
use crate::{Ctx, after, io_failed, volume};

/// The ref a workspace lands on when its base names no ref.
const DEFAULT_TARGET: &str = "main";

/// `ws_create` in the workspace `req` names.
pub fn create(ctx: &Ctx<'_>, req: WsCreateReq) -> Result<WsInfo, OpError> {
    // 1. Who makes it, where, holding what.
    let Some(who) = ctx.who.clone() else {
        return Err(OpError::new(
            ErrorCode::IdentityRequired,
            "ws create needs an identity: it records who made the workspace",
        )
        .with_hint("pass --as <family>/<instance>, or set SWARMOPS_AGENT"));
    };
    let Some(path) = req.path.as_deref() else {
        return Err(OpError::new(
            ErrorCode::InvalidArg,
            "ws create needs the workspace's directory (`path`)",
        ));
    };
    let dir = host_path(Path::new(path))?;
    if req.all && !req.prefixes.is_empty() {
        return Err(OpError::new(
            ErrorCode::InvalidArg,
            "give prefixes or `all`, not both",
        ));
    }
    let prefixes = normalize(req.prefixes)?;

    // 2. The store's side: the base's changeset, the ref it lands on, the store's id.
    let resolved = ctx.store.resolve_rev(ResolveRevReq {
        rev: req.base.clone(),
    })?;
    let target = match resolved.kind {
        RevKind::Ref => req.base.to_string(),
        RevKind::Changeset => DEFAULT_TARGET.to_owned(),
    };
    let repo_id = ctx.store.repo_info(InfoReq {})?.repo_id;

    // 3. A workspace of this name there already: the same one, its entry made sure of.
    let entry = Entry {
        format: ENTRY_FORMAT,
        name: req.name.clone(),
        path: dir.to_string(),
        created_by: who.agent().to_owned(),
        created_at: now_ms(),
        session: req.session,
    };
    let registry = Registry::open(&ctx.store_root, Arc::clone(&ctx.fs));
    if let Some(there) = existing(ctx, &dir)? {
        if there.name != req.name || there.repo_id != repo_id {
            return Err(not_empty(&dir));
        }
        registry.create(&entry)?;
        let ws = Workspace::open(ctx, Locate::Path(dir.clone()))?;
        return Ok(answer(ctx, &ws, false, req.status_after));
    }

    // 4. Else the directory holds nothing but the `.swarmws/` a killed create left.
    let host = ctx.host.as_ref();
    let made_dir = empty(host, &dir)?;
    let linked = registry.create(&entry)?;

    // 5. Its private directory, and the volume it is on.
    let swarmws = dir.join(SWARMWS_DIR);
    let journal = pending::journal_in(&dir);
    (ctx.fs.create_dir_all(journal.as_std_path()))
        .map_err(|e| io_failed(FsMethod::CreateDirAll, SWARMWS_DIR, &e))?;
    (ctx.fs.fsync_dir(dir.as_std_path())).map_err(|e| io_failed(FsMethod::FsyncDir, ".", &e))?;
    let volume = volume::probe(host, &swarmws).map_err(|e| host_failed(SWARMWS_DIR, &e))?;

    // 6. The tree checked before anything of it is written; a refusal leaves nothing.
    let file = WorkspaceFile {
        format: WORKSPACE_FORMAT,
        name: req.name.clone(),
        repo: ctx.store_root.to_string(),
        repo_id,
        base: resolved.id.clone(),
        head: resolved.id.clone(),
        prefixes: prefixes.clone(),
        all: req.all,
        created_by: entry.created_by.clone(),
        created_at: entry.created_at,
        target,
        state: State::Clean,
        onto: None,
        volume,
        materialized: resolved.id.clone(),
    };
    let mut ws = Workspace { dir, file, who };
    let intent = Intent {
        from: None,
        to: resolved.id,
        prefixes,
    };
    let prepared = match materialize::prepare(ctx, &ws, &intent, Start::Fresh) {
        Ok(prepared) => prepared,
        Err(refused) => {
            undo(ctx, &ws.dir, made_dir, &linked, &req.name)?;
            return Err(refused);
        }
    };

    // 7. The intent, then the state, then the files (WS3).
    pending::write_intent(ctx.fs.as_ref(), &ws.dir, &intent)?;
    state::replace(ctx.fs.as_ref(), &ws.dir, &ws.file)?;
    materialize::apply(ctx, &mut ws, &intent, prepared)?;
    Ok(answer(ctx, &ws, true, req.status_after))
}

/// What `ws create` answers for `ws`.
fn answer(
    ctx: &Ctx<'_>,
    ws: &Workspace,
    created: bool,
    status_after: swarmops_api::StatusAfter,
) -> WsInfo {
    WsInfo {
        created,
        status_after: after::attach(ctx, ws, status_after),
        ..described(&ws.file.name, &ws.dir, &ws.file)
    }
}

/// The state of the workspace at `dir`, if one is there.
fn existing(ctx: &Ctx<'_>, dir: &Utf8Path) -> Result<Option<WorkspaceFile>, OpError> {
    match state::read(ctx.fs.as_ref(), dir) {
        Ok(file) => Ok(Some(file)),
        Err(e) if e.code == ErrorCode::WsNotFound.as_str() => Ok(None),
        Err(e) => Err(e),
    }
}

/// Whether the directory at `dir` must be made (it is missing); `E_PATH_NOT_EMPTY` when it
/// holds anything but a `.swarmws/`, or is not a directory.
fn empty(host: &dyn HostFs, dir: &Utf8Path) -> Result<bool, OpError> {
    match host.read_dir(dir) {
        Ok(names) if names.iter().all(|name| name == SWARMWS_DIR) => Ok(false),
        Ok(_) => Err(not_empty(dir)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotADirectory => Err(not_empty(dir)),
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => Err(not_empty(dir)),
        Err(e) => Err(host_failed(".", &e)),
    }
}

/// `E_PATH_NOT_EMPTY` for the directory at `dir`.
fn not_empty(dir: &Utf8Path) -> OpError {
    OpError::new(
        ErrorCode::PathNotEmpty,
        format!("{dir} holds something already: a workspace is made in an empty directory"),
    )
    .with_details(Map::from_iter([(
        "path".to_owned(),
        Value::from(dir.as_str()),
    )]))
}

/// Takes back a create the tree refused: its `.swarmws/` (empty but for its journal directory),
/// the directory if this create made it, then the entry if this create linked it (WS4: the
/// entry goes after the directory).
fn undo(
    ctx: &Ctx<'_>,
    dir: &Utf8Path,
    made_dir: bool,
    linked: &Linked,
    name: &str,
) -> Result<(), OpError> {
    // 1. The directories, if empty.
    let host = ctx.host.as_ref();
    let swarmws = dir.join(SWARMWS_DIR);
    for (path, shown) in [
        (pending::journal_in(dir), SWARMWS_DIR),
        (swarmws.clone(), SWARMWS_DIR),
    ] {
        (host.remove_dir_if_empty(&path)).map_err(|e| host_failed(shown, &e))?;
    }
    if made_dir {
        (host.remove_dir_if_empty(dir)).map_err(|e| host_failed(".", &e))?;
    }

    // 2. The entry, if this create made it.
    if *linked == Linked::New {
        Registry::open(&ctx.store_root, Arc::clone(&ctx.fs)).delete(name)?;
    }
    Ok(())
}

/// The time now, in Unix milliseconds.
fn now_ms() -> u64 {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
}
