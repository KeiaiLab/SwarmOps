//! `ws resolve` (§5.3 of the M5 design): conflicting paths marked resolved, refused while a
//! marker line remains unless forced.
//!
//! ```text
//!  .swarmws/conflicts.json  the report; its `files`, the paths still to resolve
//!  .swarmws/resolved.json   {"files": {path: take?}}: the paths resolved, by the file or a side
//! ```
//!
//! A path resolved by its file is sent by `ws rebase --continue` as the file's content; one
//! resolved with `--take` is written from that side's changeset now and sent as the side.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use swarmops_api::{ConflictReport, ErrorCode, OpError, Side, WsResolveReq, WsResolveResp};

use crate::hostfs::HostKind;
use crate::materialize;
use crate::rebase;
use crate::state::State;
use crate::status::CONFLICTS_JSON;
use crate::workspace::{self, Locate, Workspace};
use crate::{Ctx, after, discard, io_error, markers};

/// The paths of a rebase in progress resolved so far, in `.swarmws/`.
pub const RESOLVED_JSON: &str = "resolved.json";

/// The paths resolved so far: by the file the workspace holds (none), or by a side.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Resolved {
    /// Each resolved path, and the side it takes, if any.
    pub(crate) files: BTreeMap<String, Option<Side>>,
}

/// `ws_resolve` in the workspace `req` names.
pub fn resolve(ctx: &Ctx<'_>, req: WsResolveReq) -> Result<WsResolveResp, OpError> {
    // 1. The workspace under its lock, rebasing, and its conflicts.
    let WsResolveReq {
        workspace,
        paths,
        all,
        take,
        force,
        status_after,
        ..
    } = req;
    let (ws, _held) = workspace::open_locked(ctx, &Locate::Name(workspace))?;
    let State::Rebasing { against, .. } = &ws.file.state else {
        return Err(OpError::new(
            ErrorCode::InvalidArg,
            "the workspace is not rebasing: there is nothing to resolve",
        ));
    };
    let against = against.clone();
    let mut report = conflicts(ctx, &ws)?;
    let mut resolved = read(ctx, &ws)?;

    // 2. The paths: each one that conflicts, or all of them.
    let asked = if all {
        report.files.iter().map(|file| file.path.clone()).collect()
    } else {
        materialize::normalize(paths)?
    };
    if asked.is_empty() {
        return Err(OpError::new(
            ErrorCode::InvalidArg,
            "ws resolve needs a path, or --all",
        ));
    }
    let open: Vec<&str> = report.files.iter().map(|file| file.path.as_str()).collect();
    if let Some(stray) = (asked.iter()).find(|path| !open.contains(&path.as_str())) {
        return Err(OpError::new(
            ErrorCode::InvalidArg,
            format!("{stray:?} has no conflict left to resolve"),
        )
        .with_details(Map::from_iter([(
            "path".to_owned(),
            Value::from(stray.as_str()),
        )])));
    }

    // 3. Resolved by a side: that side's file written; by the file: no marker left in it.
    match take {
        Some(side) => {
            let rev = match side {
                Side::Ours => ws.file.head.clone(),
                Side::Theirs => against,
                _ => return Err(io_error::unknown_variant("Side")),
            };
            discard::restore(ctx, &ws, &rev, &asked)?;
        }
        None => refuse_markers(ctx, &ws, &asked, force)?,
    }

    // 4. Recorded: resolved, and no longer in the report's files.
    for path in &asked {
        resolved.files.insert(path.clone(), take);
    }
    write(ctx, &ws, &resolved)?;
    report.files.retain(|file| !asked.contains(&file.path));
    rebase::write_record(ctx, &ws, CONFLICTS_JSON, &report)?;
    let mut out = WsResolveResp::new(
        asked,
        report.files.into_iter().map(|file| file.path).collect(),
    );
    out.status_after = after::attach(ctx, &ws, status_after);
    Ok(out)
}

/// `E_MARKERS_PRESENT` naming each of `paths` whose file still holds a marker line, unless
/// `force`; a path outside the workspace's prefixes, which it has no file at, is resolved by a
/// side only (`E_INVALID_ARG`).
fn refuse_markers(
    ctx: &Ctx<'_>,
    ws: &Workspace,
    paths: &[String],
    force: bool,
) -> Result<(), OpError> {
    let mut marked = Vec::new();
    for path in paths {
        // 1. A file the workspace holds.
        if !ws.file.all && !materialize::covered(path, &ws.file.prefixes) {
            return Err(OpError::new(
                ErrorCode::InvalidArg,
                format!("{path:?} is outside the workspace's prefixes: resolve it with --take"),
            )
            .with_hint(format!("swarmops ws materialize {path}")));
        }

        // 2. Its lines, looked at unless forced; a removed file holds none.
        let abs = ws.dir.join(path);
        let meta =
            (ctx.host.symlink_metadata(&abs)).map_err(|e| materialize::host_failed(path, &e))?;
        if force || !meta.is_some_and(|meta| meta.kind == HostKind::File) {
            continue;
        }
        let bytes = (ctx.host.read_file(&abs)).map_err(|e| materialize::host_failed(path, &e))?;
        if markers::marked(&bytes) {
            marked.push(Value::from(path.as_str()));
        }
    }
    if marked.is_empty() {
        return Ok(());
    }
    Err(OpError::new(
        ErrorCode::MarkersPresent,
        format!(
            "conflict markers are still in {}",
            Value::from(marked.clone())
        ),
    )
    .with_details(Map::from_iter([("paths".to_owned(), Value::from(marked))]))
    .with_hint("edit them out, or swarmops ws resolve --force <path>"))
}

/// The conflict report of workspace `ws`'s rebase: `E_INVALID_ARG` when it has none.
pub(crate) fn conflicts(ctx: &Ctx<'_>, ws: &Workspace) -> Result<ConflictReport, OpError> {
    rebase::read_record(ctx, ws, CONFLICTS_JSON)?.ok_or_else(|| {
        OpError::new(
            ErrorCode::InvalidArg,
            format!("the workspace is rebasing without its {CONFLICTS_JSON}: abort the rebase"),
        )
        .with_hint("swarmops ws rebase --abort")
    })
}

/// The paths of workspace `ws`'s rebase resolved so far.
pub(crate) fn read(ctx: &Ctx<'_>, ws: &Workspace) -> Result<Resolved, OpError> {
    Ok(rebase::read_record(ctx, ws, RESOLVED_JSON)?.unwrap_or_default())
}

/// Records `resolved` as the paths of workspace `ws`'s rebase resolved so far.
pub(crate) fn write(ctx: &Ctx<'_>, ws: &Workspace, resolved: &Resolved) -> Result<(), OpError> {
    rebase::write_record(ctx, ws, RESOLVED_JSON, resolved)
}
