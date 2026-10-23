//! Materialisation (§3 of the M5 design): only what changed between two trees written, never
//! over an edit or through a symlink, resumable through an intent record; and `ws materialize`,
//! which adds prefixes.
//!
//! ```text
//!  intent {from?, to, prefixes}
//!     │
//!     ├─ from none: read_tree of `to` under the prefixes (depth 64, listed again below that)
//!     └─ from:      diff{from, to, names, paths: prefixes}, paged
//!     ▼
//!  prepare   the portability rules over what it adds, then each path on the host against
//!            the tree it stood for: an edit, or a symlink or file the agent put above a path,
//!            is E_WS_DIRTY, and nothing is written
//!     ▼
//!  apply     journal/materialize.json durable (the caller) → removals, emptied directories
//!            pruned → blob_get in batches of 64 MiB → writes in parallel (temp, rename) →
//!            workspace.json `materialized = to` → the intent removed
//! ```
//!
//! An edit is told from the tree the files stood for: a file whose bytes, kind or mode are not
//! the tree's is the agent's. A path that already holds what the new tree holds is not written
//! again, so a materialisation a crash cut finishes by running again (`finish`).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io;
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use camino::Utf8Path;
use rayon::prelude::*;
use serde_json::{Map, Value};
use swarmops_api::{
    BlobGetReq, ContentEncoding, DEFAULT_BLOB_GET_MAX_BYTES, DEFAULT_DIFF_CONTEXT,
    DEFAULT_DIFF_MAX_BYTES, DiffFormat, DiffReq, DiffStatus, EntryKindOut, ErrorCode, FileKind,
    MAX_TREE_DEPTH, OpError, ReadTreeReq, StoreOps, TreeEntryOut, WsMaterializeReq,
    WsMaterializeResp,
};
use swarmops_core::{ObjectKind, RepoPath};

use crate::hostfs::{HostFs, HostKind, HostMeta};
use crate::pending::{self, Intent};
use crate::portable::{self, HOST};
use crate::state::{self, Volume};
use crate::volume::{EXEC_BITS, EXEC_MODE};
use crate::workspace::{self, Locate, Workspace};
use crate::{Ctx, after, io_error};

/// The entries one `read_tree` or `diff` page asks for.
const PAGE: usize = 1000;

/// The most paths an `E_WS_DIRTY` names.
const MAX_DIRTY: usize = 20;

/// What a materialisation did on the host.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Written {
    /// The files written.
    pub files: u64,
    /// The files removed.
    pub removed: u64,
    /// The bytes written.
    pub bytes: u64,
}

/// One file of a tree: what a workspace holds at a repository path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Item {
    /// The repository path.
    pub(crate) path: String,
    /// A file, an executable or a symlink.
    pub(crate) kind: FileKind,
    /// Its blob's id.
    pub(crate) blob: String,
    /// Its blob's size.
    pub(crate) size: u64,
}

/// Whether a materialisation starts now or finishes one a crash cut.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Start {
    /// Now: a file of the old tree that is gone from the host is the agent's removal, an edit.
    Fresh,
    /// After a crash: a file gone from the host may be one the cut run removed.
    Resume,
}

/// One change a materialisation makes to a path.
#[derive(Debug, Clone)]
enum Change {
    /// Write `new`, over `old` where the old tree held a file there.
    Write { new: Item, old: Option<Item> },
    /// Remove the old tree's file.
    Delete { old: Item },
}

/// A file to write, and what the host held at its path when it was checked.
#[derive(Debug, Clone)]
pub(crate) struct Put {
    /// The file.
    pub(crate) item: Item,
    /// What the host held there: nothing, the tree's file, or a directory emptied first.
    pub(crate) there: Option<HostKind>,
}

/// What [`prepare`] found to do, every path checked.
#[derive(Debug, Default)]
pub(crate) struct Prepared {
    /// The files to remove, by repository path.
    removes: Vec<String>,
    /// The files to write.
    puts: Vec<Put>,
}

/// `ws_materialize`: adds `req.paths` to the workspace's prefixes and writes their subtrees,
/// from the tree its files stand for. `E_PATH_NOT_FOUND` for a path that tree lacks.
pub fn materialize(ctx: &Ctx<'_>, req: WsMaterializeReq) -> Result<WsMaterializeResp, OpError> {
    // 1. The paths, then the workspace under its lock.
    let WsMaterializeReq {
        workspace,
        paths,
        status_after,
        ..
    } = req;
    if paths.is_empty() {
        return Err(OpError::new(
            ErrorCode::InvalidArg,
            "ws materialize needs at least one path",
        ));
    }
    let asked = normalize(paths)?;
    let (mut ws, _held) = workspace::open_locked(ctx, &Locate::Name(workspace))?;

    // 2. The ones it does not hold yet, each one the tree has.
    let added: Vec<String> = if ws.file.all {
        Vec::new()
    } else {
        (asked.into_iter())
            .filter(|path| !covered(path, &ws.file.prefixes))
            .collect()
    };
    let tree = Tree::new(ctx.store, &ws.file.materialized);
    for path in &added {
        if items_under(ctx.store, &tree, Some(path))?.is_none() {
            return Err(OpError::new(
                ErrorCode::PathNotFound,
                format!("{} has no {path:?}", short(&ws.file.materialized)),
            )
            .with_details(Map::from_iter([(
                "path".to_owned(),
                Value::from(path.as_str()),
            )])));
        }
    }

    // 3. Written as a materialisation of the whole subtrees, under its intent.
    let mut written = Written::default();
    if !added.is_empty() {
        let intent = Intent {
            from: None,
            to: ws.file.materialized.clone(),
            prefixes: added.clone(),
        };
        let prepared = prepare(ctx, &ws, &intent, Start::Fresh)?;
        pending::write_intent(ctx.fs.as_ref(), &ws.dir, &intent)?;
        written = apply(ctx, &mut ws, &intent, prepared)?;
    }

    let mut out = WsMaterializeResp::new(added, written.bytes);
    out.status_after = after::attach(ctx, &ws, status_after);
    Ok(out)
}

/// Moves the files of workspace `ws` to changeset `to`: only what differs between the tree they
/// stand for and `to`'s, under the prefixes, as a rebase or a land does after its store call.
/// The caller holds the workspace's lock. `E_WS_DIRTY` (nothing written) where the agent's edits
/// are in the way; `E_WS_UNPORTABLE` (nothing written) for paths the volume cannot hold.
pub fn update(ctx: &Ctx<'_>, ws: &mut Workspace, to: &str) -> Result<Written, OpError> {
    // 1. Nothing to do when the files stand for `to` already.
    if ws.file.materialized == to {
        return Ok(Written::default());
    }

    // 2. Checked, the intent made durable, then applied.
    let intent = Intent {
        from: Some(ws.file.materialized.clone()),
        to: to.to_owned(),
        prefixes: if ws.file.all {
            Vec::new()
        } else {
            ws.file.prefixes.clone()
        },
    };
    let prepared = prepare(ctx, ws, &intent, Start::Fresh)?;
    pending::write_intent(ctx.fs.as_ref(), &ws.dir, &intent)?;
    apply(ctx, ws, &intent, prepared)
}

/// Finishes the materialisation a crash left in workspace `ws`, if its intent is there. The
/// caller holds the workspace's lock.
pub(crate) fn finish(ctx: &Ctx<'_>, ws: &mut Workspace) -> Result<(), OpError> {
    let Some(intent) = pending::read_intent(ctx.fs.as_ref(), &ws.dir)? else {
        return Ok(());
    };
    let prepared = prepare(ctx, ws, &intent, Start::Resume)?;
    apply(ctx, ws, &intent, prepared).map(|_| ())
}

/// What `intent` changes in workspace `ws`, checked over the whole set before anything is
/// written: the portability rules over what it adds, then every path on the host against the
/// tree it stood for.
pub(crate) fn prepare(
    ctx: &Ctx<'_>,
    ws: &Workspace,
    intent: &Intent,
    start: Start,
) -> Result<Prepared, OpError> {
    // 1. The changes: a whole tree, or the difference of two.
    let new = Tree::new(ctx.store, &intent.to);
    let (changes, added) = match &intent.from {
        None => whole(ctx.store, &new, ws, intent)?,
        Some(from) => between(ctx.store, &new, from, intent)?,
    };

    // 2. What it adds, against the volume: the case set only where the volume folds case.
    let prefixes = after_intent(ws, intent);
    let case_set = if ws.file.volume.case_insensitive {
        let roots = match &intent.from {
            None => intent.prefixes.clone(),
            Some(_) => added.clone(),
        };
        let mut set = case_set(&new, &roots, ws.file.all, &prefixes)?;
        set.extend(added.iter().cloned());
        Some(set)
    } else {
        None
    };
    portable::check(&added, case_set.as_deref(), HOST)?;

    // 3. Each path on the host, against the trees.
    check_host(ctx.host.as_ref(), &ws.dir, ws.file.volume, changes, start)
}

/// Applies `prepared` to workspace `ws`, whose intent `intent` is durable already: removals, then
/// writes, then `workspace.json` names `intent.to` (and the prefixes it added), then the intent
/// goes.
pub(crate) fn apply(
    ctx: &Ctx<'_>,
    ws: &mut Workspace,
    intent: &Intent,
    prepared: Prepared,
) -> Result<Written, OpError> {
    // 1. The removals, each emptied directory pruned.
    let host = ctx.host.as_ref();
    let Prepared { removes, puts } = prepared;
    for path in &removes {
        let abs = ws.dir.join(path);
        match host.remove_file(&abs) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(host_failed(path, &e)),
            _ => {}
        }
        prune(host, &ws.dir, path)?;
    }

    // 2. The writes.
    let mut written = put_all(ctx, &ws.dir, ws.file.volume, &puts)?;
    written.removed = to_u64(removes.len());

    // 3. The state names the new tree and the prefixes it added; then the intent goes.
    let mut file = ws.file.clone();
    file.materialized.clone_from(&intent.to);
    file.prefixes = after_intent(ws, intent);
    if file != ws.file {
        state::replace(ctx.fs.as_ref(), &ws.dir, &file)?;
        ws.file = file;
    }
    pending::remove_intent(ctx.fs.as_ref(), &ws.dir)?;
    Ok(written)
}

/// Writes `puts` into the workspace at `dir`, on a volume that holds `volume`: their contents
/// fetched by `blob_get` in batches of up to 64 MiB, each batch's directories made, then its files
/// written in parallel.
pub(crate) fn put_all(
    ctx: &Ctx<'_>,
    dir: &Utf8Path,
    volume: Volume,
    puts: &[Put],
) -> Result<Written, OpError> {
    let mut written = Written::default();
    for batch in batches(puts) {
        // 1. The batch's contents, each blob once.
        let ids: BTreeSet<&str> = batch.iter().map(|put| put.item.blob.as_str()).collect();
        let contents = fetch(ctx.store, ids.into_iter().collect())?;

        // 2. Its directories, once each, in order.
        let parents: BTreeSet<&str> = (batch.iter())
            .filter_map(|put| put.item.path.rsplit_once('/').map(|(parent, _)| parent))
            .collect();
        for parent in parents {
            (ctx.host.create_dir_all(&dir.join(parent))).map_err(|e| host_failed(parent, &e))?;
        }

        // 3. Its files, in parallel.
        let host: &dyn HostFs = ctx.host.as_ref();
        batch.par_iter().try_for_each(|put| {
            let bytes = &contents[&put.item.blob];
            put_one(host, dir, volume, put, bytes)
        })?;
        written.files += to_u64(batch.len());
        written.bytes += batch.iter().map(|put| put.item.size).sum::<u64>();
    }
    Ok(written)
}

/// Writes `put`'s file, holding `bytes`, at its path below `dir`: a symlink where the volume
/// makes them, else a file (holding a symlink's target), executable where the volume keeps the
/// bit. What was there (checked to be the tree's, or an emptied directory) is replaced.
fn put_one(
    host: &dyn HostFs,
    dir: &Utf8Path,
    volume: Volume,
    put: &Put,
    bytes: &[u8],
) -> Result<(), OpError> {
    // 1. An emptied directory where a file goes, removed; one still holding files is an edit.
    let Put { item, there } = put;
    let abs = dir.join(&item.path);
    if *there == Some(HostKind::Dir) {
        let removed = host
            .remove_dir_if_empty(&abs)
            .map_err(|e| host_failed(&item.path, &e))?;
        if !removed {
            return Err(dirty(vec![item.path.clone()]));
        }
    }

    // 2. A symlink, where the volume makes them: what was there removed first.
    if item.kind == FileKind::Symlink && volume.symlinks {
        let target = std::str::from_utf8(bytes).map_err(|_| {
            OpError::new(
                ErrorCode::WsUnportable,
                format!("the symlink {} holds a target that is not UTF-8", item.path),
            )
        })?;
        if there.is_some_and(|kind| kind != HostKind::Dir) {
            (host.remove_file(&abs)).map_err(|e| host_failed(&item.path, &e))?;
        }
        return (host.symlink(target, &abs)).map_err(|e| host_failed(&item.path, &e));
    }

    // 3. A file, the bit set where the volume keeps it.
    (host.write_file(&abs, bytes)).map_err(|e| host_failed(&item.path, &e))?;
    if item.kind == FileKind::Exec && volume.exec_bit {
        (host.set_mode(&abs, EXEC_MODE)).map_err(|e| host_failed(&item.path, &e))?;
    }
    Ok(())
}

/// Whether the host's `meta` at `abs` holds `item`, on a volume that holds `volume`: its kind,
/// its mode where the volume keeps the bit, and its bytes (a symlink's target).
pub(crate) fn holds(
    host: &dyn HostFs,
    abs: &Utf8Path,
    meta: HostMeta,
    item: &Item,
    volume: Volume,
) -> io::Result<bool> {
    // 1. A symlink where the volume makes them: its target's bytes.
    if item.kind == FileKind::Symlink && volume.symlinks {
        if meta.kind != HostKind::Symlink {
            return Ok(false);
        }
        let target = host.read_link(abs)?;
        return Ok(blob_id(target.as_bytes()) == item.blob);
    }

    // 2. Else a file, of the kind's mode where the volume keeps the bit.
    if meta.kind != HostKind::File || meta.size != item.size {
        return Ok(false);
    }
    if volume.exec_bit && item.kind != FileKind::Symlink {
        let exec = meta.mode & EXEC_BITS != 0;
        if exec != (item.kind == FileKind::Exec) {
            return Ok(false);
        }
    }

    // 3. Its bytes.
    Ok(blob_id(&host.read_file(abs)?) == item.blob)
}

/// Whether the host's `meta` at `abs` is a file holding executable `item`'s bytes without its
/// executable bit: what a write killed between the file and its bit leaves, and nothing else
/// (a plain item with the bit is an agent's edit).
fn bytes_held(
    host: &dyn HostFs,
    abs: &Utf8Path,
    meta: HostMeta,
    item: &Item,
) -> Result<bool, OpError> {
    if item.kind != FileKind::Exec || meta.mode & EXEC_BITS != 0 {
        return Ok(false);
    }
    if meta.kind != HostKind::File || meta.size != item.size {
        return Ok(false);
    }
    let bytes = host
        .read_file(abs)
        .map_err(|e| host_failed(&item.path, &e))?;
    Ok(blob_id(&bytes) == item.blob)
}

/// The id of the blob holding `bytes`.
fn blob_id(bytes: &[u8]) -> String {
    swarmops_core::hash_object(ObjectKind::Blob, bytes).to_string()
}

/// The changes `prepare` checks, the host's side of each.
fn check_host(
    host: &dyn HostFs,
    dir: &Utf8Path,
    volume: Volume,
    changes: Vec<Change>,
    start: Start,
) -> Result<Prepared, OpError> {
    // 1. What the tree removes: a file or symlink there may stand above a path it writes. What
    //    it writes: a file or symlink there may stand above a path it removed already.
    let deletes: HashSet<String> = (changes.iter())
        .filter_map(|change| match change {
            Change::Delete { old } => Some(old.path.clone()),
            Change::Write { .. } => None,
        })
        .collect();
    let writes: HashSet<String> = (changes.iter())
        .filter_map(|change| match change {
            Change::Write { new, .. } => Some(new.path.clone()),
            Change::Delete { .. } => None,
        })
        .collect();

    // 2. Each change against what the host holds.
    let mut prepared = Prepared::default();
    let mut dirt = Vec::new();
    let mut above: HashMap<String, Option<HostKind>> = HashMap::new();
    for change in changes {
        let path = match &change {
            Change::Write { new, .. } => new.path.clone(),
            Change::Delete { old } => old.path.clone(),
        };
        // A removal never goes through a symlink or file above it: one the agent made is an
        // edit; one the new tree writes there means the removal ran before the crash.
        if matches!(change, Change::Delete { .. })
            && let Some(blocked) = blocker(host, dir, &path, &HashSet::new(), &mut above)?
        {
            if !writes.contains(&blocked) {
                dirt.push(blocked);
            }
            continue;
        }

        let abs = dir.join(&path);
        let there = host
            .symlink_metadata(&abs)
            .map_err(|e| host_failed(&path, &e))?;
        let held = |item: &Item, meta: HostMeta| {
            holds(host, &abs, meta, item, volume).map_err(|e| host_failed(&item.path, &e))
        };
        match (change, there) {
            // A removal: of the tree's file, or one gone already.
            (Change::Delete { .. }, None) => {}
            (Change::Delete { old }, Some(meta)) => {
                if held(&old, meta)? {
                    prepared.removes.push(path);
                } else {
                    dirt.push(path);
                }
            }

            // A write: over nothing, the old tree's file, an emptied directory; never an edit.
            (Change::Write { new, old }, there) => {
                if let Some(blocked) = blocker(host, dir, &path, &deletes, &mut above)? {
                    dirt.push(blocked);
                    continue;
                }
                let fine = match (there, &old) {
                    (None, None) => true,
                    (None, Some(_)) => start == Start::Resume,
                    (Some(meta), _) if held(&new, meta)? => continue,
                    (Some(meta), Some(old)) if held(old, meta)? => true,
                    // A file written before the crash, its mode not yet set: finished, not an edit.
                    (Some(meta), _)
                        if start == Start::Resume && bytes_held(host, &abs, meta, &new)? =>
                    {
                        true
                    }
                    (Some(meta), _) if meta.kind == HostKind::Dir => {
                        only_removed(host, dir, &path, &deletes)?
                    }
                    (Some(_), _) => false,
                };
                if fine {
                    let there = there.map(|meta| meta.kind);
                    prepared.puts.push(Put { item: new, there });
                } else {
                    dirt.push(path);
                }
            }
        }
    }

    // 3. Any edit in the way refuses the whole materialisation.
    if !dirt.is_empty() {
        return Err(dirty(dirt));
    }
    prepared.puts.sort_by(|a, b| a.item.path.cmp(&b.item.path));
    Ok(prepared)
}

/// The directory above `path` (below `dir`) that the host holds as something else, a symlink
/// or a file the agent put there, which a write would go through or fail on; none when each is
/// a directory, is missing, or is a file the materialisation removes (`deletes`). The answers
/// are kept in `above` for the next path.
fn blocker(
    host: &dyn HostFs,
    dir: &Utf8Path,
    path: &str,
    deletes: &HashSet<String>,
    above: &mut HashMap<String, Option<HostKind>>,
) -> Result<Option<String>, OpError> {
    let mut end = 0;
    let parts: Vec<&str> = path.split('/').collect();
    for part in &parts[..parts.len() - 1] {
        // 1. The next directory down, looked at once.
        end += part.len();
        let ancestor = &path[..end];
        end += 1;
        let kind = if let Some(kind) = above.get(ancestor) {
            *kind
        } else {
            let meta = host
                .symlink_metadata(&dir.join(ancestor))
                .map_err(|e| host_failed(ancestor, &e))?;
            let kind = meta.map(|meta| meta.kind);
            above.insert(ancestor.to_owned(), kind);
            kind
        };

        // 2. Missing: nothing deeper is there. A directory: go on. Else a blocker, unless it
        //    goes first.
        match kind {
            None => return Ok(None),
            Some(HostKind::Dir) => {}
            Some(_) if deletes.contains(ancestor) => return Ok(None),
            Some(_) => return Ok(Some(ancestor.to_owned())),
        }
    }
    Ok(None)
}

/// Whether the host's directory at `path` (below `dir`) holds only files the materialisation
/// removes (`deletes`), so that it is empty once they are gone.
fn only_removed(
    host: &dyn HostFs,
    dir: &Utf8Path,
    path: &str,
    deletes: &HashSet<String>,
) -> Result<bool, OpError> {
    let names = host
        .read_dir(&dir.join(path))
        .map_err(|e| host_failed(path, &e))?;
    for name in names {
        let below = format!("{path}/{name}");
        let meta = host
            .symlink_metadata(&dir.join(&below))
            .map_err(|e| host_failed(&below, &e))?;
        let fine = match meta.map(|meta| meta.kind) {
            None => true,
            Some(HostKind::Dir) => only_removed(host, dir, &below, deletes)?,
            Some(_) => deletes.contains(&below),
        };
        if !fine {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Removes the directories above `path` (below `dir`) that its removal emptied, upwards, never
/// one that still holds something (a build's `target/`) and never `dir` itself.
fn prune(host: &dyn HostFs, dir: &Utf8Path, path: &str) -> Result<(), OpError> {
    let mut at = path;
    while let Some((parent, _)) = at.rsplit_once('/') {
        match host.remove_dir_if_empty(&dir.join(parent)) {
            Ok(true) => {}
            Ok(false) => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(host_failed(parent, &e)),
        }
        at = parent;
    }
    Ok(())
}

/// The changes of a whole-tree intent: every file of `to` under its prefixes (the whole tree
/// when it has none), but those the workspace held before it (its prefixes the intent does not
/// add); each one added.
fn whole(
    store: &dyn StoreOps,
    tree: &Tree<'_>,
    ws: &Workspace,
    intent: &Intent,
) -> Result<(Vec<Change>, Vec<String>), OpError> {
    // 1. Each prefix's files; the whole tree without prefixes.
    let mut items = Vec::new();
    if intent.prefixes.is_empty() {
        items.extend(items_under(store, tree, None)?.unwrap_or_default());
    }
    for prefix in &intent.prefixes {
        items.extend(items_under(store, tree, Some(prefix))?.unwrap_or_default());
    }

    // 2. Those the workspace held already are left as they are.
    let before: Vec<String> = (ws.file.prefixes.iter())
        .filter(|prefix| !intent.prefixes.contains(prefix))
        .cloned()
        .collect();
    items.retain(|item| !covered(&item.path, &before));
    let added = items.iter().map(|item| item.path.clone()).collect();
    let changes = (items.into_iter())
        .map(|new| Change::Write { new, old: None })
        .collect();
    Ok((changes, added))
}

/// The changes between changeset `from` and `new`'s under the intent's prefixes, a `diff` page
/// at a time, and the paths it adds (that `from` lacks).
fn between(
    store: &dyn StoreOps,
    new: &Tree<'_>,
    from: &str,
    intent: &Intent,
) -> Result<(Vec<Change>, Vec<String>), OpError> {
    let old = Tree::new(store, from);
    let mut changes = Vec::new();
    let mut added = Vec::new();
    let mut cursor = None;
    loop {
        // 1. A page of the files that differ.
        let mut req = DiffReq::new(from, intent.to.as_str());
        req.paths.clone_from(&intent.prefixes);
        req.format = DiffFormat::Names;
        req.context = DEFAULT_DIFF_CONTEXT;
        req.limit = PAGE;
        req.cursor = cursor.take();
        req.max_bytes = DEFAULT_DIFF_MAX_BYTES;
        let page = store.diff(req)?;

        // 2. Each one's old and new file.
        for file in page.files {
            let path = file.path;
            let change = match file.status {
                DiffStatus::Deleted => Change::Delete {
                    old: old.file(&path)?,
                },
                DiffStatus::Added => {
                    added.push(path.clone());
                    Change::Write {
                        new: new.file(&path)?,
                        old: None,
                    }
                }
                DiffStatus::Modified | DiffStatus::TypeChanged => Change::Write {
                    new: new.file(&path)?,
                    old: Some(old.file(&path)?),
                },
                _ => return Err(io_error::unknown_variant("DiffStatus")),
            };
            changes.push(change);
        }
        if !page.truncated {
            return Ok((changes, added));
        }
        cursor = page.cursor;
    }
}

/// Every path the workspace will hold that can meet one of `roots` on a case-insensitive
/// volume: the entries of each directory above each root, in `tree`, that the workspace holds
/// (`all`, or at, below or above one of `prefixes`).
fn case_set(
    tree: &Tree<'_>,
    roots: &[String],
    all: bool,
    prefixes: &[String],
) -> Result<Vec<String>, OpError> {
    let mut set = BTreeSet::new();
    let mut listed = HashSet::new();
    for root in roots {
        // 1. Each directory above the root, listed once.
        let parts: Vec<&str> = root.split('/').collect();
        let dirs = (0..parts.len()).map(|n| parts[..n].join("/"));
        for dir in dirs {
            if !listed.insert(dir.clone()) {
                continue;
            }

            // 2. Its entries the workspace holds.
            let listing = tree.listing(&dir)?;
            for entry in listing.iter().flat_map(|entries| entries.iter()) {
                let path = join(&dir, &entry.name);
                if all || held_by(&path, prefixes) {
                    set.insert(path);
                }
            }
        }
    }
    Ok(set.into_iter().collect())
}

/// The workspace's prefixes once `intent` is done: those it adds joined in.
fn after_intent(ws: &Workspace, intent: &Intent) -> Vec<String> {
    if ws.file.all || intent.from.is_some() {
        return ws.file.prefixes.clone();
    }
    let joined = (ws.file.prefixes.iter())
        .chain(&intent.prefixes)
        .cloned()
        .collect();
    tidy(joined)
}

/// The files of `tree` at or below `prefix` (the root for none), from `read_tree` at depth 64,
/// a directory 64 levels below listed again from its own path; a file's prefix is that file.
/// None when the tree holds nothing there.
pub(crate) fn items_under(
    store: &dyn StoreOps,
    tree: &Tree<'_>,
    prefix: Option<&str>,
) -> Result<Option<Vec<Item>>, OpError> {
    // 1. A prefix that is a file, or nothing.
    if let Some(prefix) = prefix {
        match tree.entry(prefix)? {
            None => return Ok(None),
            Some(entry) if entry.kind != EntryKindOut::Dir => {
                return Ok(Some(vec![item(prefix, &entry)?]));
            }
            Some(_) => {}
        }
    }

    // 2. Each directory to list, at depth 64, a page at a time.
    let mut items = Vec::new();
    let mut todo = vec![prefix.map(str::to_owned)];
    while let Some(dir) = todo.pop() {
        let mut cursor = None;
        loop {
            let mut req = ReadTreeReq::new(tree.rev.as_str());
            req.path.clone_from(&dir);
            req.depth = MAX_TREE_DEPTH;
            req.limit = PAGE;
            req.cursor = cursor.take();
            let page = store.read_tree(req)?;

            // 3. Its files; a directory as deep as the listing reaches is listed again.
            for entry in &page.entries {
                let below = entry.path.as_deref().unwrap_or(&entry.name);
                let path = join(dir.as_deref().unwrap_or(""), below);
                if entry.kind != EntryKindOut::Dir {
                    items.push(item(&path, entry)?);
                } else if below.split('/').count() >= usize::try_from(MAX_TREE_DEPTH).unwrap_or(64)
                {
                    todo.push(Some(path));
                }
            }
            if !page.truncated {
                break;
            }
            cursor = page.cursor;
        }
    }
    Ok(Some(items))
}

/// The [`Item`] of `entry`, a file of a listing, at `path`; `E_INTERNAL` for a kind this build
/// does not know.
fn item(path: &str, entry: &TreeEntryOut) -> Result<Item, OpError> {
    let kind = match entry.kind {
        EntryKindOut::Exec => FileKind::Exec,
        EntryKindOut::Symlink => FileKind::Symlink,
        EntryKindOut::File | EntryKindOut::Dir => FileKind::File,
        _ => return Err(io_error::unknown_variant("EntryKindOut")),
    };
    Ok(Item {
        path: path.to_owned(),
        kind,
        blob: entry.id.clone(),
        size: entry.size.unwrap_or(0),
    })
}

/// A changeset's tree, read a directory at a time and kept: what a materialisation asks of one
/// path (its entry) or one directory (its entries).
pub(crate) struct Tree<'a> {
    /// The store it is read from.
    store: &'a dyn StoreOps,
    /// The changeset, a full id.
    rev: String,
    /// The directories read so far, by path ("" the root); none for one the tree lacks.
    dirs: std::sync::Mutex<HashMap<String, Option<Arc<[TreeEntryOut]>>>>,
}

impl<'a> Tree<'a> {
    /// The tree of changeset `rev` in `store`.
    pub(crate) fn new(store: &'a dyn StoreOps, rev: &str) -> Tree<'a> {
        Tree {
            store,
            rev: rev.to_owned(),
            dirs: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// The entries of directory `dir` ("" the root), every page; none when the tree has no
    /// directory there.
    fn listing(&self, dir: &str) -> Result<Option<Arc<[TreeEntryOut]>>, OpError> {
        // 1. Read before.
        if let Some(known) = self.dirs.lock().expect("no reader panicked").get(dir) {
            return Ok(known.clone());
        }

        // 2. Read now, a page at a time; a path that is no directory reads as none.
        let mut entries = Vec::new();
        let mut cursor = None;
        let listed = loop {
            let mut req = ReadTreeReq::new(self.rev.as_str());
            req.path = (!dir.is_empty()).then(|| dir.to_owned());
            req.depth = 1;
            req.limit = PAGE;
            req.cursor = cursor.take();
            let page = self.store.read_tree(req);
            let page = match page {
                Ok(page) => page,
                Err(e) if e.code == ErrorCode::PathNotFound.as_str() => break None,
                Err(e) => return Err(e),
            };
            entries.extend(page.entries);
            if !page.truncated {
                break Some(Arc::from(entries));
            }
            cursor = page.cursor;
        };

        // 3. Kept for the next question.
        (self.dirs.lock().expect("no reader panicked")).insert(dir.to_owned(), listed.clone());
        Ok(listed)
    }

    /// The entry at `path`, if the tree has one.
    pub(crate) fn entry(&self, path: &str) -> Result<Option<TreeEntryOut>, OpError> {
        let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
        let listing = self.listing(dir)?;
        Ok(listing.and_then(|entries| entries.iter().find(|e| e.name == name).cloned()))
    }

    /// The file at `path`, which a `diff` named: `E_INTERNAL` when the tree has none.
    fn file(&self, path: &str) -> Result<Item, OpError> {
        match self.entry(path)? {
            Some(entry) if entry.kind != EntryKindOut::Dir => item(path, &entry),
            _ => Err(OpError::new(
                ErrorCode::Internal,
                format!(
                    "the diff names {path:?}, which {} does not hold as a file",
                    short(&self.rev)
                ),
            )),
        }
    }
}

/// `puts` in batches whose contents hold at most `blob_get`'s 64 MiB each; a larger file is a
/// batch alone.
fn batches(puts: &[Put]) -> Vec<&[Put]> {
    let mut batches = Vec::new();
    let (mut start, mut used) = (0, 0u64);
    for (i, put) in puts.iter().enumerate() {
        if i > start && used + put.item.size > DEFAULT_BLOB_GET_MAX_BYTES {
            batches.push(&puts[start..i]);
            (start, used) = (i, 0);
        }
        used += put.item.size;
    }
    if start < puts.len() {
        batches.push(&puts[start..]);
    }
    batches
}

/// The contents of blobs `ids`, by `blob_get`, decoded; `E_STORE_CORRUPT` for one the store
/// lacks.
pub(crate) fn fetch(
    store: &dyn StoreOps,
    mut ids: Vec<&str>,
) -> Result<HashMap<String, Vec<u8>>, OpError> {
    let mut contents = HashMap::with_capacity(ids.len());
    while !ids.is_empty() {
        // 1. As many as fit in one answer.
        let mut req = BlobGetReq::new(ids.iter().map(|id| (*id).to_owned()).collect());
        req.max_bytes = DEFAULT_BLOB_GET_MAX_BYTES;
        let got = store.blob_get(req)?;
        if let Some(missing) = got.missing.first() {
            return Err(OpError::new(
                ErrorCode::StoreCorrupt,
                format!("the tree names blob {missing}, which the store does not hold"),
            ));
        }

        // 2. Each decoded; the rest asked again.
        for blob in got.blobs {
            let bytes = match blob.encoding {
                ContentEncoding::Utf8 => blob.content.into_bytes(),
                ContentEncoding::Base64 => STANDARD.decode(&blob.content).map_err(|e| {
                    OpError::new(
                        ErrorCode::Internal,
                        format!("blob_get gave blob {} in bad base64: {e}", blob.id),
                    )
                })?,
                _ => return Err(io_error::unknown_variant("ContentEncoding")),
            };
            contents.insert(blob.id, bytes);
        }
        ids.retain(|id| !contents.contains_key(*id));
    }
    Ok(contents)
}

/// The repository paths `paths`, each checked (one trailing `/` taken), sorted, those below
/// another dropped; `E_INVALID_ARG` for one that is not a repository path.
pub(crate) fn normalize(paths: Vec<String>) -> Result<Vec<String>, OpError> {
    let mut checked = Vec::with_capacity(paths.len());
    for path in paths {
        let trimmed = path.strip_suffix('/').unwrap_or(&path);
        let repo_path = RepoPath::new(trimmed).map_err(|e| {
            OpError::new(
                ErrorCode::InvalidArg,
                format!("{path:?} is not a repository path: {e}"),
            )
        })?;
        checked.push(repo_path.as_str().to_owned());
    }
    Ok(tidy(checked))
}

/// `paths` sorted, without repeats and without those at or below another.
fn tidy(mut paths: Vec<String>) -> Vec<String> {
    paths.sort();
    paths.dedup();
    let all = paths.clone();
    paths.retain(|path| {
        !all.iter()
            .any(|other| other != path && covered(path, std::slice::from_ref(other)))
    });
    paths
}

/// Whether `path` is at or below one of `prefixes`.
pub(crate) fn covered(path: &str, prefixes: &[String]) -> bool {
    prefixes.iter().any(|prefix| {
        path == prefix
            || path
                .strip_prefix(prefix.as_str())
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

/// Whether a workspace of `prefixes` holds `path`: at or below a prefix, or a directory above
/// one.
fn held_by(path: &str, prefixes: &[String]) -> bool {
    covered(path, prefixes)
        || (prefixes.iter()).any(|prefix| covered(prefix, std::slice::from_ref(&path.to_owned())))
}

/// `name` in directory `dir` ("" the root).
fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}

/// The first 12 digits of changeset id `id`, as messages name it.
fn short(id: &str) -> &str {
    id.get(..12).unwrap_or(id)
}

/// `E_WS_DIRTY` naming `paths` (the first 20, sorted): the agent's edits in the way.
pub(crate) fn dirty(mut paths: Vec<String>) -> OpError {
    paths.sort();
    paths.dedup();
    let named: Vec<Value> = (paths.iter())
        .take(MAX_DIRTY)
        .map(|path| Value::from(path.as_str()))
        .collect();
    OpError::new(
        ErrorCode::WsDirty,
        format!(
            "the workspace holds edits this would overwrite: {}",
            Value::from(named.clone())
        ),
    )
    .with_details(Map::from_iter([("paths".to_owned(), Value::from(named))]))
    .with_hint("swarmops ws commit -m <message>, or swarmops ws discard <path>…")
}

/// `E_IO` for a host call on the workspace's `path` that failed with `e`.
pub(crate) fn host_failed(path: &str, e: &io::Error) -> OpError {
    OpError::new(ErrorCode::Io, format!("{path}: {e}"))
        .with_details(Map::from_iter([("path".to_owned(), Value::from(path))]))
}

/// `n` as a u64.
fn to_u64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}
