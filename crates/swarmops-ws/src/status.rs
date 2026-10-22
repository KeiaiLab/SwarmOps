//! `ws status` (§4.3 of the M5 design): the workspace's changes against its head, found by stat
//! and the index, and how far its target has moved since its base.
//!
//! ```text
//!  resolve_rev{target} ──▶ main      equal to base: behind_by 0, no history read
//!  log{main, 100, cursor}…           until base, at most 10,000 counted
//!  scan (the index, the walker) ──▶ changes, a page after `cursor`, `limit` long
//! ```

use serde_json::{Map, Value};
use swarmops_api::{
    ConflictReport, ErrorCode, LogReq, OpError, ResolveRevReq, StoreOps, WsChange, WsRebasing,
    WsState, WsStatus, WsStatusReq,
};

use crate::Ctx;
use crate::materialize::normalize;
use crate::scan::{self, Change, Keep, Want};
use crate::state::{SWARMWS_DIR, State};
use crate::workspace::{Locate, Workspace};

/// The most changesets `behind_by` counts: past it, the count stops there.
pub const BEHIND_CAP: u64 = 10_000;

/// The changesets one `log` page of `behind_by` asks for.
const LOG_PAGE: usize = 100;

/// The conflict report of a rebase in progress, in [`SWARMWS_DIR`].
pub const CONFLICTS_JSON: &str = "conflicts.json";

/// `ws_status` in the workspace `req` names.
pub fn status(ctx: &Ctx<'_>, req: WsStatusReq) -> Result<WsStatus, OpError> {
    // 1. The request's limit and paths.
    let WsStatusReq {
        workspace,
        paths,
        full,
        limit,
        cursor,
    } = req;
    if limit == 0 {
        return Err(
            OpError::new(ErrorCode::InvalidArg, "limit must be at least 1")
                .with_details(Map::from_iter([("limit".to_owned(), Value::from(0))])),
        );
    }
    let paths = normalize(paths)?;

    // 2. The workspace, without its lock: a read.
    let ws = Workspace::open(ctx, Locate::Name(workspace))?;
    let want = Want {
        paths,
        full,
        extra: Vec::new(),
    };
    page(ctx, &ws, &want, limit, cursor.as_deref())
}

/// The page of workspace `ws`'s status `want` asks for: up to `limit` changes after `cursor`.
pub(crate) fn page(
    ctx: &Ctx<'_>,
    ws: &Workspace,
    want: &Want,
    limit: usize,
    cursor: Option<&str>,
) -> Result<WsStatus, OpError> {
    // 1. Where the target stands, read first.
    let (main, behind_by) = behind(ctx.store, &ws.file.target, &ws.file.base)?;

    // 2. The changes, the index kept for the next scan.
    let changes = changes_of(ctx, ws, want)?;

    // 3. The page after the cursor.
    let rest: Vec<&Change> = (changes.iter())
        .filter(|change| cursor.is_none_or(|after| change.path.as_str() > after))
        .collect();
    let truncated = rest.len() > limit;
    let shown: Vec<WsChange> = (rest.into_iter().take(limit))
        .map(|change| WsChange {
            path: change.path.clone(),
            kind: change.kind,
            size: change.new.map(|file| file.meta.size),
        })
        .collect();
    let cursor = (shown.last())
        .filter(|_| truncated)
        .map(|last| last.path.clone());

    // 4. The state, and the rebase in progress.
    let (state, rebasing) = match &ws.file.state {
        State::Clean => (WsState::Clean, None),
        State::Rebasing { onto, .. } => {
            let rebasing = WsRebasing {
                onto: onto.clone(),
                conflicts: conflicts(ws),
            };
            (WsState::Rebasing, Some(rebasing))
        }
    };
    Ok(WsStatus {
        name: ws.file.name.clone(),
        base: ws.file.base.clone(),
        head: ws.file.head.clone(),
        main,
        behind_by,
        state,
        changes: shown,
        rebasing,
        truncated,
        cursor,
    })
}

/// Every change of workspace `ws` that `want` asks for, by path; its index kept.
pub(crate) fn changes_of(
    ctx: &Ctx<'_>,
    ws: &Workspace,
    want: &Want,
) -> Result<Vec<Change>, OpError> {
    let scanned = scan::scan(ctx, ws, want)?;
    Ok(scanned.keep(&ws.dir, &scan::binding_of(ws), Keep::IfChanged))
}

/// The head of ref `target`, and how many changesets it holds past `base` along first parents,
/// at most [`BEHIND_CAP`]: one ref read, then `log` a page at a time until `base`.
fn behind(store: &dyn StoreOps, target: &str, base: &str) -> Result<(String, u64), OpError> {
    // 1. The head, a ref read; at the base, nothing to count.
    let main = store.resolve_rev(ResolveRevReq { rev: target.into() })?.id;
    if main == base {
        return Ok((main, 0));
    }

    // 2. The history, a page at a time, until the base or the cap.
    let mut count = 0;
    let mut cursor = None;
    loop {
        let page = store.log(LogReq {
            rev: Some(main.as_str().into()),
            limit: LOG_PAGE,
            cursor: cursor.take(),
            paths: Vec::new(),
        })?;
        for entry in &page.entries {
            if entry.id == base || count == BEHIND_CAP {
                return Ok((main, count));
            }
            count += 1;
        }
        if !page.truncated || count == BEHIND_CAP {
            return Ok((main, count));
        }
        cursor = page.cursor;
    }
}

/// The conflicting paths of workspace `ws`'s rebase, from its conflict report; none when it has
/// no readable one.
fn conflicts(ws: &Workspace) -> Vec<String> {
    let path = ws.dir.join(SWARMWS_DIR).join(CONFLICTS_JSON);
    let report = std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<ConflictReport>(&bytes).ok());
    report.map_or_else(Vec::new, |report| {
        report.files.into_iter().map(|file| file.path).collect()
    })
}
