//! `ws commit` (§4.5 of the M5 design): the changed files hashed, the blobs the store lacks
//! uploaded, one keyed commit on the workspace's head, then `workspace.json`.
//!
//! ```text
//!  lock → scan → sizes (E_TOO_LARGE) → hashes, bounded → journal/pending.json {key} (WS1)
//!       → blob_has, blob_put what is missing → commit{parent: head, {blob: id}…, key}
//!       → workspace.json head (WS2) → index → the pending record removed
//! ```
//!
//! A crash between the store's commit and `workspace.json` leaves the pending record: the next
//! commit takes its key, and the store replays the same changeset (a keyed commit takes its
//! key's time).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map, Value};
use swarmops_api::{
    BlobHasReq, BlobPutReq, ChangeInput, ChangeOp, CommitReq, ContentInput, ErrorCode, FileKind,
    ModeInput, OpError, WsCommitReq, WsCommitResp,
};
use swarmops_core::ObjectId;
use swarmops_store::MAX_OBJECT_LEN;

use crate::hostfs::HostKind;
use crate::index::{Index, Row, UNCONFIRMED};
use crate::materialize::{covered, host_failed, normalize};
use crate::pending::{self, Pending, PendingOp};
use crate::scan::{self, Blob, Change, HASH_BYTES_IN_FLIGHT, HostFile, Keep, Want};
use crate::state::{self, State};
use crate::workspace::{Locate, Workspace, open_locked};
use crate::{Ctx, after};

/// The ids one `blob_has` asks about.
const HAS_BATCH: usize = 1000;

/// One path the commit writes or removes.
struct Entry {
    /// The repository path.
    path: String,
    /// What it holds now: its kind and blob; none for a removal.
    put: Option<(FileKind, Blob)>,
    /// The host's file there, when the commit reads it.
    host: Option<HostFile>,
}

/// `ws_commit` in the workspace `req` names.
pub fn commit(ctx: &Ctx<'_>, req: WsCommitReq) -> Result<WsCommitResp, OpError> {
    // 1. The request's paths and modes, then the workspace under its lock.
    let WsCommitReq {
        workspace,
        message,
        paths,
        modes,
        status_after,
    } = req;
    let named = normalize(paths)?;
    let modes = modes_of(modes)?;
    let (mut ws, _held) = open_locked(ctx, &Locate::Name(workspace))?;

    // 2. The commit, then the status after it.
    let mut done = commit_in(ctx, &mut ws, message, &named, &modes)?;
    done.status_after = after::attach(ctx, &ws, status_after);
    Ok(done)
}

/// Commits the edits of workspace `ws`, whose lock the caller holds, under the `named` paths
/// (every path when empty) with `modes`: what `ws commit` does once it holds the lock, and what
/// `ws rebase -m` and `ws land -m` do first. The response carries no status.
pub(crate) fn commit_in(
    ctx: &Ctx<'_>,
    ws: &mut Workspace,
    message: String,
    named: &[String],
    modes: &BTreeMap<String, FileKind>,
) -> Result<WsCommitResp, OpError> {
    // 1. Not rebasing; a mode only where the volume drops the bit.
    if let State::Rebasing { .. } = ws.file.state {
        return Err(OpError::new(
            ErrorCode::UnresolvedConflicts,
            "the workspace is rebasing: finish the rebase before committing",
        )
        .with_hint("swarmops ws rebase --continue, or swarmops ws rebase --abort"));
    }
    if !modes.is_empty() && ws.file.volume.exec_bit {
        return Err(OpError::new(
            ErrorCode::InvalidArg,
            "this workspace's volume keeps the executable bit: set it on the file instead",
        ));
    }

    // 2. What changed, under the named paths (and those of the modes); a named path outside the
    //    prefixes joins them.
    let extra = outside(ctx, ws, named)?;
    let mut filter = named.to_vec();
    if !named.is_empty() {
        filter.extend(modes.keys().cloned());
    }
    let want = Want {
        paths: filter,
        full: false,
        extra: extra.clone(),
    };
    let scanned = scan::scan(ctx, ws, &want)?;

    // 3. The entries: each change, and each mode asked for; nothing, an empty commit.
    let mut entries = entries_of(&scanned.changes, modes);
    entries.extend(mode_only(&scanned.index, &scanned.changes, modes));
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    if entries.is_empty() {
        return Err(OpError::new(
            ErrorCode::EmptyCommit,
            "the workspace has no change to commit",
        ));
    }

    // 4. No file past what an object holds, before anything is read or sent.
    if let Some(entry) =
        (entries.iter()).find(|e| e.host.is_some_and(|h| h.meta.size > MAX_OBJECT_LEN))
    {
        let size = entry.host.map_or(0, |host| host.meta.size);
        return Err(too_large(&entry.path, size));
    }

    // 5. The bytes the scan did not hash, hashed under the budget.
    hash_missing(ctx, ws, &mut entries)?;

    // 6. The pending record, durable before the store hears of the commit (WS1).
    let key = pending_key(ctx, ws)?;

    // 7. The blobs the store lacks, uploaded.
    let blobs_written = upload(ctx, ws, &entries)?;

    // 8. The commit, by blob id, on the head's full id.
    let changes = entries.iter().map(change_input).collect();
    let req = CommitReq {
        parent: Some(ws.file.head.as_str().into()),
        changes,
        message,
        advance: None,
        idempotency_key: Some(key),
        allow_empty: false,
    };
    let done = ctx.store.commit(&ws.who, req)?;

    // 9. The workspace stands on it (WS2); the index learns it; the record goes.
    let parent = ws.file.head.clone();
    let mut file = ws.file.clone();
    file.head.clone_from(&done.changeset);
    file.materialized.clone_from(&done.changeset);
    file.onto = None;
    if !extra.is_empty() {
        file.prefixes = normalize(file.prefixes.iter().chain(&extra).cloned().collect())?;
    }
    state::replace(ctx.fs.as_ref(), &ws.dir, &file)?;
    ws.file = file;
    let mut scanned = scanned;
    learn(&mut scanned.index, &entries);
    scanned.keep(&ws.dir, &scan::binding_of(ws), Keep::Always);
    pending::remove_pending(ctx.fs.as_ref(), &ws.dir)?;

    Ok(WsCommitResp {
        changeset: done.changeset,
        parent,
        files_changed: done.files_changed,
        blobs_written,
        replayed: done.replayed,
        status_after: None,
    })
}

/// The modes asked for, by repository path: `exec` or `file` only.
fn modes_of(modes: BTreeMap<String, ModeInput>) -> Result<BTreeMap<String, FileKind>, OpError> {
    let mut out = BTreeMap::new();
    for (path, mode) in modes {
        let kind = match mode {
            ModeInput::File => FileKind::File,
            ModeInput::Exec => FileKind::Exec,
            ModeInput::Symlink => {
                return Err(OpError::new(
                    ErrorCode::InvalidArg,
                    format!("{path:?}: a mode is exec or file; a symlink is made on the host"),
                ));
            }
        };
        let [path] = normalize(vec![path])?
            .try_into()
            .expect("one path in, one out");
        out.insert(path, kind);
    }
    Ok(out)
}

/// The named paths outside workspace `ws`'s prefixes, each a file or a symlink on the host;
/// `E_INVALID_ARG` for one that is not (a directory outside is materialised first).
fn outside(ctx: &Ctx<'_>, ws: &Workspace, named: &[String]) -> Result<Vec<String>, OpError> {
    let mut extra = Vec::new();
    for path in named {
        if ws.file.all || covered(path, &ws.file.prefixes) {
            continue;
        }
        let meta = ctx
            .host
            .symlink_metadata(&ws.dir.join(path))
            .map_err(|e| host_failed(path, &e))?;
        if !meta.is_some_and(|meta| matches!(meta.kind, HostKind::File | HostKind::Symlink)) {
            return Err(OpError::new(
                ErrorCode::InvalidArg,
                format!("{path:?} is outside the workspace's prefixes and no file there"),
            )
            .with_hint(format!("swarmops ws materialize {path}")));
        }
        extra.push(path.clone());
    }
    Ok(extra)
}

/// The entries of `changes`, each put of the kind `modes` names for it, if any.
fn entries_of(changes: &[Change], modes: &BTreeMap<String, FileKind>) -> Vec<Entry> {
    (changes.iter())
        .map(|change| {
            let Some(host) = change.new else {
                return Entry {
                    path: change.path.clone(),
                    put: None,
                    host: None,
                };
            };
            let kind = match modes.get(&change.path) {
                Some(kind) if host.kind != FileKind::Symlink => *kind,
                _ => host.kind,
            };
            Entry {
                path: change.path.clone(),
                put: host.blob.map(|blob| (kind, blob)),
                host: Some(HostFile { kind, ..host }),
            }
        })
        .collect()
}

/// The entries of the modes asked for at paths the scan found unchanged: the head's blob, the
/// mode asked for, where it is not the head's already.
fn mode_only(index: &Index, changes: &[Change], modes: &BTreeMap<String, FileKind>) -> Vec<Entry> {
    let found: BTreeSet<&str> = changes.iter().map(|change| change.path.as_str()).collect();
    (modes.iter())
        .filter(|(path, _)| !found.contains(path.as_str()))
        .filter_map(|(path, kind)| {
            let at = index.rows.binary_search_by(|row| row.path.cmp(path)).ok()?;
            let row = &index.rows[at];
            (row.mode != FileKind::Symlink && row.mode != *kind).then(|| Entry {
                path: path.clone(),
                put: Some((*kind, row.blob)),
                host: None,
            })
        })
        .collect()
}

/// Hashes the entries whose bytes the scan did not hash, at most [`HASH_BYTES_IN_FLIGHT`] bytes
/// at once; one gone since the scan is `E_IO_TRANSIENT`, to run again.
fn hash_missing(ctx: &Ctx<'_>, ws: &Workspace, entries: &mut [Entry]) -> Result<(), OpError> {
    // 1. Those to hash.
    let todo: Vec<(usize, (String, HostFile))> = (entries.iter().enumerate())
        .filter(|(_, entry)| entry.put.is_none())
        .filter_map(|(at, entry)| Some((at, (entry.path.clone(), entry.host?))))
        .collect();
    let files: Vec<(String, HostFile)> = todo.iter().map(|(_, file)| file.clone()).collect();

    // 2. Hashed in parallel, under the budget.
    let blobs = scan::hash_all(ctx.host.as_ref(), &ws.dir, &files, HASH_BYTES_IN_FLIGHT)?;
    for ((at, (path, host)), blob) in todo.into_iter().zip(blobs) {
        let Some(blob) = blob else {
            return Err(changed_meanwhile(&path));
        };
        entries[at].put = Some((host.kind, blob));
    }
    Ok(())
}

/// The key of this commit: the pending record's, when a commit on this head was cut short;
/// else a new uuid v7, recorded durably first.
fn pending_key(ctx: &Ctx<'_>, ws: &Workspace) -> Result<String, OpError> {
    // 1. A commit cut short on this head: its key, to replay it.
    let fs = ctx.fs.as_ref();
    match pending::read_pending(fs, &ws.dir)? {
        Some(cut) if cut.op == PendingOp::Commit && cut.head == ws.file.head => return Ok(cut.key),
        Some(cut) if cut.op != PendingOp::Commit => {
            return Err(OpError::new(
                ErrorCode::InvalidArg,
                format!(
                    "a ws {} of this workspace was cut short: run it again first",
                    serde_json::to_value(cut.op).map_or_else(|_| "?".to_owned(), |v| v.to_string())
                ),
            ));
        }
        _ => {}
    }

    // 2. A new key, durable before the store call.
    let key = uuid::Uuid::now_v7().to_string();
    let record = Pending {
        op: PendingOp::Commit,
        key: key.clone(),
        rebase_key: None,
        head: ws.file.head.clone(),
        target: None,
        attempt: 1,
        state: "committing".to_owned(),
    };
    pending::write_pending(fs, &ws.dir, &record)?;
    Ok(key)
}

/// Uploads the blobs of `entries` the store lacks, asked in batches; how many it got.
fn upload(ctx: &Ctx<'_>, ws: &Workspace, entries: &[Entry]) -> Result<u64, OpError> {
    // 1. Each blob once, with a file that holds it.
    let mut wanted: HashMap<String, &Entry> = HashMap::new();
    for entry in entries {
        if let (Some((_, blob)), Some(_)) = (entry.put, entry.host) {
            wanted.entry(ObjectId(blob).to_hex()).or_insert(entry);
        }
    }
    let ids: Vec<String> = wanted.keys().cloned().collect();

    // 2. Those the store lacks, asked in batches.
    let mut missing = Vec::new();
    for batch in ids.chunks(HAS_BATCH) {
        let present = ctx.store.blob_has(BlobHasReq {
            ids: batch.to_vec(),
        })?;
        let present: BTreeSet<String> = present.present.into_iter().collect();
        missing.extend(batch.iter().filter(|id| !present.contains(*id)).cloned());
    }

    // 3. Each uploaded from its file, which must still hold it.
    let mut written = 0;
    for id in missing {
        let entry = wanted[&id];
        let host = entry
            .host
            .expect("only entries read from the host are uploaded");
        let abs = ws.dir.join(&entry.path);
        let bytes = scan::content(ctx.host.as_ref(), &abs, host.meta.kind)
            .map_err(|e| host_failed(&entry.path, &e))?;
        if ObjectId(scan::blob_id(&bytes)).to_hex() != id {
            return Err(changed_meanwhile(&entry.path));
        }
        let content = match String::from_utf8(bytes) {
            Ok(text) => ContentInput::Inline(text),
            Err(e) => ContentInput::Base64(STANDARD.encode(e.into_bytes())),
        };
        let put = ctx.store.blob_put(&ws.who, BlobPutReq { content })?;
        written += u64::from(put.written);
    }
    Ok(written)
}

/// The change list's line of `entry`: a put by blob id in its kind, or a delete.
fn change_input(entry: &Entry) -> ChangeInput {
    let Some((kind, blob)) = entry.put else {
        return ChangeInput {
            path: entry.path.clone(),
            op: ChangeOp::Delete,
            content: None,
            mode: None,
        };
    };
    let mode = match kind {
        FileKind::File => ModeInput::File,
        FileKind::Exec => ModeInput::Exec,
        FileKind::Symlink => ModeInput::Symlink,
    };
    ChangeInput {
        path: entry.path.clone(),
        op: ChangeOp::Put,
        content: Some(ContentInput::Blob(ObjectId(blob).to_hex())),
        mode: Some(mode),
    }
}

/// The index's rows after the commit of `entries`: a removal's row gone, a put's row its kind
/// and blob, with the stat the scan took of its file (hashed again if it moved since).
fn learn(index: &mut Index, entries: &[Entry]) {
    for entry in entries {
        // 1. Where its row is, or goes.
        let found = (index.rows).binary_search_by(|row| row.path.as_str().cmp(&entry.path));

        // 2. Removed, or written.
        let Some((kind, blob)) = entry.put else {
            if let Ok(at) = found {
                index.rows.remove(at);
            }
            continue;
        };
        let old = found.ok().map(|at| &index.rows[at]);
        let (size, mtime_ns, inode) = match (entry.host, old) {
            (Some(host), _) => (host.meta.size, host.meta.mtime_ns, host.meta.inode),
            (None, Some(old)) => (old.size, old.mtime_ns, old.inode),
            (None, None) => (0, UNCONFIRMED, 0),
        };
        let row = Row {
            path: entry.path.clone(),
            size,
            mtime_ns,
            inode,
            mode: kind,
            blob,
        };
        match found {
            Ok(at) => index.rows[at] = row,
            Err(at) => index.rows.insert(at, row),
        }
    }
}

/// `E_TOO_LARGE` for the file at `path`, of `size` bytes.
fn too_large(path: &str, size: u64) -> OpError {
    OpError::new(
        ErrorCode::TooLarge,
        format!("{path} holds {size} bytes, past the {MAX_OBJECT_LEN} an object holds"),
    )
    .with_details(Map::from_iter([
        ("path".to_owned(), Value::from(path)),
        ("size".to_owned(), Value::from(size)),
        ("max".to_owned(), Value::from(MAX_OBJECT_LEN)),
    ]))
}

/// `E_IO_TRANSIENT` for a file that changed while the commit read it.
fn changed_meanwhile(path: &str) -> OpError {
    OpError::new(
        ErrorCode::IoTransient,
        format!("{path} changed while the commit read it: run the commit again"),
    )
    .with_details(Map::from_iter([("path".to_owned(), Value::from(path))]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A put's row takes the scan's stat of its file; a removal's row goes; a mode-only put
    /// keeps the row's stat.
    #[test]
    fn the_index_learns_what_a_commit_wrote() {
        // 1. Two rows.
        let row = |path: &str, fill| Row {
            path: path.to_owned(),
            size: 1,
            mtime_ns: 10,
            inode: 5,
            mode: FileKind::File,
            blob: [fill; 32],
        };
        let mut index = Index {
            written_ns: 20,
            rows: vec![row("a", 1), row("b", 2)],
        };

        // 2. `a` removed, `b` made executable in place, `c` added.
        let meta = crate::hostfs::HostMeta {
            kind: HostKind::File,
            size: 9,
            mtime_ns: 15,
            inode: 7,
            mode: 0o644,
        };
        let entries = [
            Entry {
                path: "a".to_owned(),
                put: None,
                host: None,
            },
            Entry {
                path: "b".to_owned(),
                put: Some((FileKind::Exec, [2; 32])),
                host: None,
            },
            Entry {
                path: "c".to_owned(),
                put: Some((FileKind::File, [3; 32])),
                host: Some(HostFile {
                    kind: FileKind::File,
                    meta,
                    blob: None,
                }),
            },
        ];
        learn(&mut index, &entries);

        // 3. Two rows: `b` with its stat, `c` with the scan's.
        let shown: Vec<(&str, u64, i64, FileKind)> = (index.rows.iter())
            .map(|r| (r.path.as_str(), r.size, r.mtime_ns, r.mode))
            .collect();
        assert_eq!(
            shown,
            [("b", 1, 10, FileKind::Exec), ("c", 9, 15, FileKind::File)]
        );
        assert_eq!(index.rows[1].blob, [3; 32]);
    }
}
