//! `ws rebase` (§5.3 of the M5 design): the store's `rebase`, then the files moved to its
//! result; on a conflict, the workspace rebasing with markers written.
//!
//! ```text
//!  start     lock → edits committed (-m) or E_WS_DIRTY → journal/pending.json {key} (WS1)
//!            → rebase{head, base, onto, key}
//!              ├─ H' → files from `materialized` to H' with workspace.json {head: H', base: T,
//!              │       onto: T} (WS2) → the pending record removed
//!              └─ E_CONFLICT → conflicts.json, workspace.json rebasing{onto: T, against},
//!                              markers in the conflicting text files; nothing else moves
//!  continue  every conflict resolved (ws resolve) → rebase{…, resolutions{against, files}}
//!            → as start: H', or a new report (the target moved), still rebasing
//!  abort     the conflicting paths restored from the head → clean
//! ```
//!
//! A rebase cut short after the store's frame leaves its pending record: the same call again
//! takes its key, and the store replays the same `H'`.

use std::collections::{BTreeMap, BTreeSet};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use swarmops_api::{
    BlobPutReq, ConflictReport, ContentInput, DEFAULT_MAX_CONFLICT_BYTES, ErrorCode, ModeInput,
    OpError, RebaseReq, RebaseResp, ResolutionInput, ResolutionsInput, Side, WsRebaseAction,
    WsRebaseReq, WsRebaseResp,
};
use swarmops_store::FsMethod;

use crate::hostfs::HostKind;
use crate::materialize::{self, covered, dirty, host_failed};
use crate::pending::{self, Pending, PendingOp};
use crate::resolve::{self, RESOLVED_JSON, Resolved};
use crate::scan::Want;
use crate::state::{self, SWARMWS_DIR, State};
use crate::status::{self, CONFLICTS_JSON};
use crate::volume::EXEC_BITS;
use crate::workspace::{Locate, Workspace, open_locked};
use crate::{Ctx, after, commit, discard, io_failed, markers};

/// The pending record's state of a rebase that starts.
const STARTING: &str = "starting";

/// The pending record's state of a rebase that continues with resolutions.
const CONTINUING: &str = "continuing";

/// `ws_rebase` in the workspace `req` names.
pub fn rebase(ctx: &Ctx<'_>, req: WsRebaseReq) -> Result<WsRebaseResp, OpError> {
    // 1. The workspace under its lock.
    let WsRebaseReq {
        workspace,
        onto,
        action,
        message,
        status_after,
    } = req;
    let (mut ws, _held) = open_locked(ctx, &Locate::Name(workspace))?;

    // 2. The action; then the status after it.
    let mut done = match action {
        WsRebaseAction::Start => start(ctx, &mut ws, &onto, message)?,
        WsRebaseAction::Continue => resume(ctx, &mut ws)?,
        WsRebaseAction::Abort => abort(ctx, &mut ws)?,
    };
    done.status_after = after::attach(ctx, &ws, status_after);
    Ok(done)
}

/// Starts a rebase of workspace `ws` onto the head of ref `onto`, its edits committed first with
/// `message`, else refused.
fn start(
    ctx: &Ctx<'_>,
    ws: &mut Workspace,
    onto: &str,
    message: Option<String>,
) -> Result<WsRebaseResp, OpError> {
    // 1. Not in a rebase already.
    if let State::Rebasing { .. } = ws.file.state {
        return Err(
            OpError::new(ErrorCode::InvalidArg, "the workspace is rebasing already")
                .with_hint("swarmops ws rebase --continue, or swarmops ws rebase --abort"),
        );
    }

    // 2. A rebase cut short: its key; else the edits settled first.
    let cut = cut_short(ctx, ws, PendingOp::Rebase, STARTING)?;
    if cut.is_none() {
        settle_edits(ctx, ws, message, &BTreeSet::new())?;
    }

    // 3. The pending record, durable before the store call (WS1); then the call.
    let key = cut.map_or_else(new_key, |cut| cut.key);
    let record = record(PendingOp::Rebase, &key, None, ws, onto, STARTING);
    pending::write_pending(ctx.fs.as_ref(), &ws.dir, &record)?;
    let done = rebase_in(ctx, ws, onto, &key, None);
    let done = finish_call(ctx, ws, done)?;
    pending::remove_pending(ctx.fs.as_ref(), &ws.dir)?;
    Ok(response(&done))
}

/// Continues workspace `ws`'s rebase with its resolutions: refused while a conflict is
/// unresolved.
fn resume(ctx: &Ctx<'_>, ws: &mut Workspace) -> Result<WsRebaseResp, OpError> {
    // 1. Rebasing, with every conflict resolved.
    let State::Rebasing { against, .. } = &ws.file.state else {
        return Err(not_rebasing());
    };
    let against = against.clone();
    let report = resolve::conflicts(ctx, ws)?;
    if !report.files.is_empty() {
        let paths: Vec<Value> = (report.files.iter())
            .map(|file| Value::from(file.path.as_str()))
            .collect();
        return Err(OpError::new(
            ErrorCode::UnresolvedConflicts,
            format!("unresolved conflicts: {}", Value::from(paths.clone())),
        )
        .with_details(Map::from_iter([("paths".to_owned(), Value::from(paths))]))
        .with_hint("edit them, then swarmops ws resolve <path>…"));
    }

    // 2. A continue cut short: its key; else no edit beside the resolved paths.
    let resolved = resolve::read(ctx, ws)?;
    let cut = cut_short(ctx, ws, PendingOp::Rebase, CONTINUING)?;
    if cut.is_none() {
        let conflicted: BTreeSet<String> = resolved.files.keys().cloned().collect();
        settle_edits(ctx, ws, None, &conflicted)?;
    }

    // 3. The resolutions, the pending record (WS1), the call.
    let resolutions = resolutions(ctx, ws, &resolved, &against)?;
    let key = cut.map_or_else(new_key, |cut| cut.key);
    let target = ws.file.target.clone();
    let record = record(PendingOp::Rebase, &key, None, ws, &target, CONTINUING);
    pending::write_pending(ctx.fs.as_ref(), &ws.dir, &record)?;
    let done = rebase_in(ctx, ws, &target, &key, Some(resolutions));
    let done = finish_call(ctx, ws, done)?;
    pending::remove_pending(ctx.fs.as_ref(), &ws.dir)?;
    Ok(response(&done))
}

/// Leaves workspace `ws`'s rebase: each conflicting path restored from the head, the workspace
/// clean on it.
fn abort(ctx: &Ctx<'_>, ws: &mut Workspace) -> Result<WsRebaseResp, OpError> {
    // 1. Rebasing; its conflicting paths, resolved or not, that the workspace holds.
    let State::Rebasing { onto, .. } = &ws.file.state else {
        return Err(not_rebasing());
    };
    let onto = onto.clone();
    let paths = conflicted(ctx, ws)?;

    // 2. The head's files back, then the state clean, then the rebase's records gone.
    let head = ws.file.head.clone();
    discard::restore(ctx, ws, &head, &paths)?;
    ws.file.state = State::Clean;
    state::replace(ctx.fs.as_ref(), &ws.dir, &ws.file)?;
    remove_record(ctx, ws, CONFLICTS_JSON)?;
    remove_record(ctx, ws, RESOLVED_JSON)?;
    if (pending::read_pending(ctx.fs.as_ref(), &ws.dir)?).is_some_and(|p| p.op == PendingOp::Rebase)
    {
        pending::remove_pending(ctx.fs.as_ref(), &ws.dir)?;
    }

    Ok(WsRebaseResp {
        status: None,
        changeset: head,
        onto,
        status_after: None,
    })
}

/// Rebases workspace `ws`'s chain onto the head of ref `onto` with `key` and `resolutions`, and
/// moves the workspace with it: to `H'` (files, then `workspace.json`), or, on a conflict,
/// into `rebasing` with the report and markers written, the conflict returned. The caller holds
/// the lock and has made its pending record durable.
pub(crate) fn rebase_in(
    ctx: &Ctx<'_>,
    ws: &mut Workspace,
    onto: &str,
    key: &str,
    resolutions: Option<ResolutionsInput>,
) -> Result<RebaseResp, OpError> {
    // 1. The store's rebase of the chain from the base.
    let req = RebaseReq {
        head: ws.file.head.as_str().into(),
        base: Some(ws.file.base.as_str().into()),
        onto: onto.to_owned(),
        message: None,
        idempotency_key: Some(key.to_owned()),
        max_conflict_bytes: DEFAULT_MAX_CONFLICT_BYTES,
        resolutions,
    };
    let done = match ctx.store.rebase(&ws.who, req) {
        Ok(done) => done,
        Err(e) if e.code == ErrorCode::Conflict.as_str() => {
            enter(ctx, ws, onto, &report_of(&e)?)?;
            return Err(e);
        }
        Err(e) => return Err(e),
    };

    // 2. The workspace moved to it: files and state in one replace of workspace.json (WS2).
    let rebasing = matches!(ws.file.state, State::Rebasing { .. });
    ws.file.head.clone_from(&done.changeset);
    ws.file.base.clone_from(&done.onto);
    ws.file.onto = Some(done.onto.clone());
    onto.clone_into(&mut ws.file.target);
    ws.file.state = State::Clean;
    move_to(ctx, ws, &done.changeset)?;

    // 3. A rebase in progress is over: its records go.
    if rebasing {
        remove_record(ctx, ws, CONFLICTS_JSON)?;
        remove_record(ctx, ws, RESOLVED_JSON)?;
    }
    Ok(done)
}

/// Moves the files of workspace `ws`, whose state the caller has set in memory, to changeset
/// `to`, and replaces `workspace.json` with that state: by the materialisation when the files
/// move, else directly.
pub(crate) fn move_to(ctx: &Ctx<'_>, ws: &mut Workspace, to: &str) -> Result<(), OpError> {
    let moves = ws.file.materialized != to;
    materialize::update(ctx, ws, to)?;
    if !moves {
        state::replace(ctx.fs.as_ref(), &ws.dir, &ws.file)?;
    }
    Ok(())
}

/// What a rebase's store call `done` leaves: a conflict, which moved the workspace into
/// `rebasing`, takes the pending record with it; any other failure keeps it for a re-run.
pub(crate) fn finish_call<T>(
    ctx: &Ctx<'_>,
    ws: &Workspace,
    done: Result<T, OpError>,
) -> Result<T, OpError> {
    match done {
        Err(e) if refused(&e) => {
            pending::remove_pending(ctx.fs.as_ref(), &ws.dir)?;
            Err(e)
        }
        done => done,
    }
}

/// Whether the store refused a call outright, nothing written: its pending record has nothing
/// to replay. A failure that may have come after the store's frame (an I/O error, a kill) is
/// not.
pub(crate) fn refused(e: &OpError) -> bool {
    [
        ErrorCode::Conflict,
        ErrorCode::CasMismatch,
        ErrorCode::CasRetryExhausted,
        ErrorCode::InvalidArg,
        ErrorCode::RevNotFound,
        ErrorCode::NothingToLand,
        ErrorCode::CheckFailed,
    ]
    .iter()
    .any(|code| e.code == code.as_str())
}

/// Workspace `ws` entering `rebasing` on `report`, the conflict of a rebase onto ref `onto`:
/// the report kept, the state replaced, then the files: what an earlier round of resolutions
/// left where a path conflicts again or went stale, back to the head, and the markers.
fn enter(
    ctx: &Ctx<'_>,
    ws: &mut Workspace,
    onto: &str,
    report: &ConflictReport,
) -> Result<(), OpError> {
    // 1. A rebase continued: the resolutions that still hold, and the paths of the last round.
    let again: BTreeSet<String> = (report.files.iter())
        .map(|file| file.path.clone())
        .chain(report.stale.iter().cloned())
        .collect();
    let continued = matches!(ws.file.state, State::Rebasing { .. });
    let (mut kept, earlier) = if continued {
        (resolve::read(ctx, ws)?, conflicted(ctx, ws)?)
    } else {
        (Resolved::default(), Vec::new())
    };
    kept.files.retain(|path, _| !again.contains(path));

    // 2. The report and the resolutions kept; the state rebasing onto the report's theirs.
    write_record(ctx, ws, CONFLICTS_JSON, report)?;
    resolve::write(ctx, ws, &kept)?;
    ws.file.state = State::Rebasing {
        onto: report.theirs.clone(),
        against: report.theirs.clone(),
    };
    onto.clone_into(&mut ws.file.target);
    state::replace(ctx.fs.as_ref(), &ws.dir, &ws.file)?;

    // 3. The files: after an earlier round, its paths to decide again back from the head (a
    //    first round finds the head's files there); then the markers.
    if continued {
        let back: BTreeSet<String> = (earlier.into_iter().chain(again))
            .filter(|path| !kept.files.contains_key(path))
            .filter(|path| ws.file.all || covered(path, &ws.file.prefixes))
            .collect();
        let back: Vec<String> = back.into_iter().collect();
        let head = ws.file.head.clone();
        discard::restore(ctx, ws, &head, &back)?;
    }
    markers::write(ctx, ws, report)?;
    Ok(())
}

/// Every conflicting path of workspace `ws`'s rebase, resolved or not, that the workspace holds.
fn conflicted(ctx: &Ctx<'_>, ws: &Workspace) -> Result<Vec<String>, OpError> {
    let report: Option<ConflictReport> = read_record(ctx, ws, CONFLICTS_JSON)?;
    let resolved = resolve::read(ctx, ws)?;
    let paths: BTreeSet<String> = (report.iter())
        .flat_map(|report| report.files.iter().map(|file| file.path.clone()))
        .chain(resolved.files.into_keys())
        .filter(|path| ws.file.all || covered(path, &ws.file.prefixes))
        .collect();
    Ok(paths.into_iter().collect())
}

/// The resolutions of `resolved`, decided against `against`: a side as it is, a path resolved
/// by its file as that file's content (uploaded, sent by blob id) or its removal.
fn resolutions(
    ctx: &Ctx<'_>,
    ws: &Workspace,
    resolved: &Resolved,
    against: &str,
) -> Result<ResolutionsInput, OpError> {
    let mut files = BTreeMap::new();
    for (path, take) in &resolved.files {
        let resolution = match take {
            Some(side) => taken(*side),
            None => content_of(ctx, ws, path)?,
        };
        files.insert(path.clone(), resolution);
    }
    Ok(ResolutionsInput {
        against: against.into(),
        files,
    })
}

/// The resolution that takes `side`.
fn taken(side: Side) -> ResolutionInput {
    ResolutionInput {
        take: Some(side),
        content: None,
        mode: None,
        delete: false,
    }
}

/// The resolution of `path` by what workspace `ws` holds there: its content, in its kind where
/// the volume tells it, or nothing.
fn content_of(ctx: &Ctx<'_>, ws: &Workspace, path: &str) -> Result<ResolutionInput, OpError> {
    // 1. What is there: nothing is a removal; a directory is no file.
    let abs = ws.dir.join(path);
    let host = ctx.host.as_ref();
    let meta = (host.symlink_metadata(&abs)).map_err(|e| host_failed(path, &e))?;
    let Some(meta) = meta else {
        return Ok(ResolutionInput {
            take: None,
            content: None,
            mode: None,
            delete: true,
        });
    };

    // 2. Its bytes and kind: a symlink's target, or a file's bytes and bit.
    let (bytes, mode) = match meta.kind {
        HostKind::Symlink => {
            let target = (host.read_link(&abs)).map_err(|e| host_failed(path, &e))?;
            (target.into_bytes(), Some(ModeInput::Symlink))
        }
        HostKind::File => {
            let bytes = (host.read_file(&abs)).map_err(|e| host_failed(path, &e))?;
            let kind = if meta.mode & EXEC_BITS == 0 {
                ModeInput::File
            } else {
                ModeInput::Exec
            };
            let mode = ws.file.volume.exec_bit.then_some(kind);
            (bytes, mode)
        }
        HostKind::Dir | HostKind::Other => {
            return Err(OpError::new(
                ErrorCode::InvalidArg,
                format!("{path:?} is resolved, but no file is there: resolve it with --take"),
            ));
        }
    };

    // 3. Uploaded, and named by its blob id.
    let content = match String::from_utf8(bytes) {
        Ok(text) => ContentInput::Inline(text),
        Err(e) => ContentInput::Base64(STANDARD.encode(e.into_bytes())),
    };
    let put = ctx.store.blob_put(&ws.who, BlobPutReq { content })?;
    Ok(ResolutionInput {
        take: None,
        content: Some(ContentInput::Blob(put.blob)),
        mode,
        delete: false,
    })
}

/// Settles the edits of workspace `ws` before a rebase or a land: none (but at `except`, the
/// paths a rebase resolves) is fine; with `message` they are committed; else `E_WS_DIRTY`. A
/// workspace whose files stand for another changeset than its head (a call cut short) is left
/// to the materialisation's own check.
pub(crate) fn settle_edits(
    ctx: &Ctx<'_>,
    ws: &mut Workspace,
    message: Option<String>,
    except: &BTreeSet<String>,
) -> Result<(), OpError> {
    // 1. The edits, but those at the excepted paths.
    if ws.file.materialized != ws.file.head {
        return Ok(());
    }
    let changes = status::changes_of(ctx, ws, &Want::default())?;
    let paths: Vec<String> = (changes.into_iter())
        .map(|change| change.path)
        .filter(|path| !except.contains(path))
        .collect();
    if paths.is_empty() {
        return Ok(());
    }

    // 2. Committed with the message, else refused.
    match message {
        Some(message) if except.is_empty() => {
            commit::commit_in(ctx, ws, message, &[], &BTreeMap::new()).map(|_| ())
        }
        _ => Err(dirty(paths)),
    }
}

/// The pending record of a call `op` in state `state` that was cut short on workspace `ws`'s
/// head, to re-run with its keys. A record of an older head is stale (`workspace.json` holds
/// its outcome) and goes, as does one of the same call in another state; one of another call on
/// this head is that call's to finish: `E_INVALID_ARG`.
pub(crate) fn cut_short(
    ctx: &Ctx<'_>,
    ws: &Workspace,
    op: PendingOp,
    state: &str,
) -> Result<Option<Pending>, OpError> {
    let fs = ctx.fs.as_ref();
    let Some(cut) = pending::read_pending(fs, &ws.dir)? else {
        return Ok(None);
    };
    if cut.head == ws.file.head && cut.op == op && cut.state == state {
        return Ok(Some(cut));
    }
    if cut.head == ws.file.head && cut.op != op {
        let name = serde_json::to_value(cut.op).map_or_else(|_| "?".to_owned(), |v| v.to_string());
        return Err(OpError::new(
            ErrorCode::InvalidArg,
            format!("a ws {name} of this workspace was cut short: run it again first"),
        ));
    }
    pending::remove_pending(fs, &ws.dir)?;
    Ok(None)
}

/// The pending record of call `op` with `key` (and `rebase_key`) from workspace `ws`'s head,
/// onto or landing on `target`, in `state`.
pub(crate) fn record(
    op: PendingOp,
    key: &str,
    rebase_key: Option<&str>,
    ws: &Workspace,
    target: &str,
    state: &str,
) -> Pending {
    Pending {
        op,
        key: key.to_owned(),
        rebase_key: rebase_key.map(str::to_owned),
        head: ws.file.head.clone(),
        target: Some(target.to_owned()),
        attempt: 1,
        state: state.to_owned(),
    }
}

/// A new key for a store call: a uuid v7.
pub(crate) fn new_key() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// The conflict report `e`, an `E_CONFLICT`, carries.
fn report_of(e: &OpError) -> Result<ConflictReport, OpError> {
    let report = e.details.get("report").cloned().unwrap_or_default();
    serde_json::from_value(report).map_err(|parse| {
        OpError::new(
            ErrorCode::Internal,
            format!("the store's E_CONFLICT carries no report this build reads: {parse}"),
        )
    })
}

/// The response of a rebase that ended with `done`.
fn response(done: &RebaseResp) -> WsRebaseResp {
    WsRebaseResp {
        status: Some(done.status),
        changeset: done.changeset.clone(),
        onto: done.onto.clone(),
        status_after: None,
    }
}

/// `E_INVALID_ARG` for a continue or an abort with no rebase in progress.
fn not_rebasing() -> OpError {
    OpError::new(
        ErrorCode::InvalidArg,
        "the workspace is not rebasing: there is nothing to continue or abort",
    )
}

/// Record `name` of workspace `ws`, in `.swarmws/`, parsed, if it is there.
pub(crate) fn read_record<T: DeserializeOwned>(
    ctx: &Ctx<'_>,
    ws: &Workspace,
    name: &str,
) -> Result<Option<T>, OpError> {
    // 1. The bytes, if any.
    let file = format!("{SWARMWS_DIR}/{name}");
    let path = ws.dir.join(&file);
    let bytes =
        (ctx.fs.read(path.as_std_path())).map_err(|e| io_failed(FsMethod::Read, &file, &e))?;
    let Some(bytes) = bytes else {
        return Ok(None);
    };

    // 2. Parsed; a record this build cannot read is named.
    serde_json::from_slice(&bytes).map(Some).map_err(|e| {
        OpError::new(
            ErrorCode::InvalidArg,
            format!("{file} is not a record this build reads: {e}"),
        )
    })
}

/// Writes `value` as record `name` of workspace `ws`, in `.swarmws/`, whole and durably.
pub(crate) fn write_record<T: Serialize>(
    ctx: &Ctx<'_>,
    ws: &Workspace,
    name: &str,
    value: &T,
) -> Result<(), OpError> {
    let file = format!("{SWARMWS_DIR}/{name}");
    let bytes = serde_json::to_vec(value).expect("a workspace record serializes");
    (ctx.fs
        .atomic_replace(ws.dir.join(&file).as_std_path(), &bytes))
    .map_err(|e| io_failed(FsMethod::AtomicReplace, &file, &e))
}

/// Removes record `name` of workspace `ws`, in `.swarmws/`: an unlink, then a directory sync;
/// none there is removed already.
pub(crate) fn remove_record(ctx: &Ctx<'_>, ws: &Workspace, name: &str) -> Result<(), OpError> {
    // 1. The unlink.
    let file = format!("{SWARMWS_DIR}/{name}");
    match ctx.fs.remove(ws.dir.join(&file).as_std_path()) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(io_failed(FsMethod::Remove, &file, &e)),
    }

    // 2. The removal made durable.
    let dir = ws.dir.join(SWARMWS_DIR);
    (ctx.fs.fsync_dir(dir.as_std_path())).map_err(|e| io_failed(FsMethod::FsyncDir, &file, &e))
}
