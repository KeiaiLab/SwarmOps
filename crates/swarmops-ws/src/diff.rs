//! `ws diff` (§4.4 of the M5 design): the workspace's edits against its head, as the store's
//! `Diff` type, each file's hunks made by `swarmops-merge`'s line diff over the head's blob and
//! the host's file, with the store `diff`'s rules: its formats, its binary probe, its bound on
//! the line diff (`hunks_truncated`) and its `max_bytes` page.

use std::io;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map, Value};
use swarmops_api::{
    BlobGetReq, ContentEncoding, DEFAULT_BLOB_GET_MAX_BYTES, Diff, DiffFormat, DiffHunk,
    DiffLineOut, DiffStatus, ErrorCode, FileDiff, LineOp, OpError, StoreOps, WsChangeKind,
    WsDiffReq,
};
use swarmops_core::ObjectId;
use swarmops_merge::line_diff::{self, DiffLine, UnifiedHunk};

use crate::Ctx;
use crate::io_error::unknown_variant;
use crate::materialize;
use crate::scan::{self, Change, Want};
use crate::status;
use crate::workspace::{Locate, Workspace};

/// `ws_diff` in the workspace `req` names.
pub fn diff(ctx: &Ctx<'_>, req: WsDiffReq) -> Result<Diff, OpError> {
    // 1. The request's limits and paths.
    let WsDiffReq {
        workspace,
        paths,
        format,
        context,
        limit,
        cursor,
        max_bytes,
        ..
    } = req;
    at_least_one("limit", u64::try_from(limit).unwrap_or(u64::MAX))?;
    at_least_one("max_bytes", max_bytes)?;
    let paths = materialize::normalize(paths)?;

    // 2. The changes after the cursor, by a scan without the lock: a read.
    let ws = Workspace::open(ctx, Locate::Name(workspace))?;
    let want = Want {
        paths,
        full: false,
        extra: Vec::new(),
    };
    let changes = status::changes_of(ctx, &ws, &want)?;
    let rest: Vec<&Change> = (changes.iter())
        .filter(|change| {
            cursor
                .as_deref()
                .is_none_or(|after| change.path.as_str() > after)
        })
        .collect();

    // 3. The page: at most `limit` files, a `unified` one within `max_bytes` of JSON.
    let mut files: Vec<FileDiff> = Vec::new();
    let mut used = 2;
    for change in rest.iter().take(limit) {
        let mut file = file_diff(ctx, &ws, change, format, context)?;
        if format == DiffFormat::Unified {
            let comma = u64::from(!files.is_empty());
            let room = max_bytes.saturating_sub(used + comma);
            if json_len(&file) > room {
                if !files.is_empty() {
                    break;
                }
                cut_hunks(&mut file, room);
            }
            used += comma + json_len(&file);
        }
        files.push(file);
    }

    // 4. Where the next page starts.
    let truncated = files.len() < rest.len();
    let cursor = (files.last())
        .filter(|_| truncated)
        .map(|last| last.path.clone());
    let mut out = Diff::new(
        ws.file.head.clone(),
        format!("ws:{}", ws.file.name),
        files,
        truncated,
    );
    out.cursor = cursor;
    Ok(out)
}

/// What a page says of `change` in `format`, as the store's `diff` says it of a file: its path
/// and status; for `stat` and `unified`, whether it is binary and its line counts; for
/// `unified`, its kinds, its blobs and its hunks.
fn file_diff(
    ctx: &Ctx<'_>,
    ws: &Workspace,
    change: &Change,
    format: DiffFormat,
    context: u32,
) -> Result<FileDiff, OpError> {
    // 1. The path and the status.
    let mut file = FileDiff::new(change.path.clone(), status_of(change.kind)?);
    if format == DiffFormat::Names {
        return Ok(file);
    }

    // 2. Each side's bytes: the head's blob, the host's file.
    let old = match change.old {
        Some((_, blob)) => Some(blob_bytes(ctx.store, &hex(blob))?),
        None => None,
    };
    let new = match change.new {
        Some(host) => {
            let abs = ws.dir.join(&change.path);
            let bytes = scan::content(ctx.host.as_ref(), &abs, host.meta.kind)
                .map_err(|e| materialize::host_failed(&change.path, &e))?;
            Some((host.kind, bytes))
        }
        None => None,
    };

    // 3. The kinds and blobs, for `unified`.
    let new_blob = (new.as_ref()).map(|(_, bytes)| scan::blob_id(bytes));
    if format == DiffFormat::Unified {
        file.old_kind = change.old.map(|(kind, _)| kind);
        file.new_kind = new.as_ref().map(|(kind, _)| *kind);
        file.old_blob = change.old.map(|(_, blob)| hex(blob));
        file.new_blob = new_blob.map(hex);
    }

    // 4. Text or binary; text past the line diff's bound gets no counts and no hunks.
    let shared = change.old.map(|(_, blob)| blob) == new_blob && new_blob.is_some();
    let old = old.as_deref().unwrap_or_default();
    let new = new.as_ref().map_or(&[][..], |(_, bytes)| bytes.as_slice());
    let text = line_diff::is_text(old) && line_diff::is_text(new);
    file.binary = Some(!text);
    if !text {
        return Ok(file);
    }
    let hunks = if shared {
        Vec::new()
    } else if let Ok(hunks) = line_diff::line_hunks(old, new) {
        hunks
    } else {
        file.hunks_truncated = Some(true);
        file.hunks = (format == DiffFormat::Unified).then(Vec::new);
        return Ok(file);
    };

    // 5. The counts, and for `unified` the hunks.
    let stats = line_diff::line_stats(&hunks);
    file.insertions = Some(stats.insertions);
    file.deletions = Some(stats.deletions);
    if format == DiffFormat::Unified {
        let shown = line_diff::unified(old, new, &hunks, context);
        file.hunks = Some(shown.iter().map(diff_hunk).collect::<Result<_, _>>()?);
    }
    Ok(file)
}

/// The store's status of a workspace change: a mode change is a modification. `E_INTERNAL` for
/// a kind this build does not know.
fn status_of(kind: WsChangeKind) -> Result<DiffStatus, OpError> {
    match kind {
        WsChangeKind::Added => Ok(DiffStatus::Added),
        WsChangeKind::Deleted => Ok(DiffStatus::Deleted),
        WsChangeKind::Modified | WsChangeKind::ModeChanged => Ok(DiffStatus::Modified),
        WsChangeKind::TypeChanged => Ok(DiffStatus::TypeChanged),
        _ => Err(unknown_variant("WsChangeKind")),
    }
}

/// `hunk` as a `unified` page gives it; `E_INTERNAL` for a line this build does not know.
fn diff_hunk(hunk: &UnifiedHunk<'_>) -> Result<DiffHunk, OpError> {
    let lines = (hunk.lines.iter())
        .map(|line| {
            let (op, text) = match *line {
                DiffLine::Context(text) => (LineOp::Context, text),
                DiffLine::Removed(text) => (LineOp::Remove, text),
                DiffLine::Added(text) => (LineOp::Add, text),
                _ => return Err(unknown_variant("DiffLine")),
            };
            Ok(DiffLineOut::new(
                op,
                String::from_utf8_lossy(text).into_owned(),
            ))
        })
        .collect::<Result<_, _>>()?;
    Ok(DiffHunk::new(
        u64::from(hunk.before_start),
        u64::from(hunk.before_lines),
        u64::from(hunk.after_start),
        u64::from(hunk.after_lines),
        lines,
    ))
}

/// Cuts `file`'s hunks to the first ones that fit with it in `room` bytes of JSON, as the
/// store's page does, and marks it cut; a file without hunks stays as it is.
fn cut_hunks(file: &mut FileDiff, room: u64) {
    // 1. Only a file with hunks is cut.
    let hunks = match file.hunks.take() {
        Some(hunks) if !hunks.is_empty() => hunks,
        none => {
            file.hunks = none;
            return;
        }
    };

    // 2. The hunks that fit, in order.
    file.hunks = Some(Vec::new());
    file.hunks_truncated = Some(true);
    let mut size = json_len(&*file);
    let mut kept = Vec::new();
    for hunk in hunks {
        let more = json_len(&hunk) + u64::from(!kept.is_empty());
        if size + more > room {
            break;
        }
        size += more;
        kept.push(hunk);
    }
    file.hunks = Some(kept);
}

/// The bytes of blob `id`, by `blob_get`.
fn blob_bytes(store: &dyn StoreOps, id: &str) -> Result<Vec<u8>, OpError> {
    // 1. Asked for alone, with room for the largest object.
    let mut req = BlobGetReq::new(vec![id.to_owned()]);
    req.max_bytes = DEFAULT_BLOB_GET_MAX_BYTES.max(swarmops_store::MAX_OBJECT_LEN);
    let got = store.blob_get(req)?;
    let Some(blob) = got.blobs.into_iter().next() else {
        return Err(OpError::new(
            ErrorCode::StoreCorrupt,
            format!("the head names blob {id}, which the store does not hold"),
        ));
    };

    // 2. Decoded.
    match blob.encoding {
        ContentEncoding::Utf8 => Ok(blob.content.into_bytes()),
        ContentEncoding::Base64 => STANDARD.decode(&blob.content).map_err(|e| {
            OpError::new(
                ErrorCode::Internal,
                format!("blob_get gave blob {id} in bad base64: {e}"),
            )
        }),
        _ => Err(unknown_variant("ContentEncoding")),
    }
}

/// Blob id `blob` in hex.
fn hex(blob: scan::Blob) -> String {
    ObjectId::new(blob).to_hex()
}

/// `E_INVALID_ARG` when `value`, the request's `name`, is 0.
fn at_least_one(name: &str, value: u64) -> Result<(), OpError> {
    if value > 0 {
        return Ok(());
    }
    Err(
        OpError::new(ErrorCode::InvalidArg, format!("{name} must be at least 1"))
            .with_details(Map::from_iter([(name.to_owned(), Value::from(0))])),
    )
}

/// The bytes `value` takes as JSON.
fn json_len<T: serde::Serialize>(value: &T) -> u64 {
    let mut count = ByteCount(0);
    serde_json::to_writer(&mut count, value).expect("an API value serializes");
    count.0
}

/// A writer that keeps nothing but the number of bytes written to it.
struct ByteCount(u64);

impl io::Write for ByteCount {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 += u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
