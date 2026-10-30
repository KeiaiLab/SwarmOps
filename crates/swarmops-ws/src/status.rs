//! `ws status` (§4.3 of the M5 design): the workspace's changes against its head, found by stat
//! and the index, and how far its target has moved since its base.
//!
//! ```text
//!  resolve_rev{target} ──▶ main      equal to base: behind_by 0, no history read
//!  log{main, 100, cursor}…           until base, at most 10,000 counted
//!  log{base, 100, cursor}…           base not met: until a changeset main's log held
//!  scan (the index, the walker) ──▶ changes, a page after `cursor`, `limit` long
//! ```

use std::collections::HashMap;

use serde_json::{Map, Value};
use swarmops_api::{
    ConflictReport, ErrorCode, LogReq, OpError, ResolveRevReq, StoreOps, WsChange, WsRebasing,
    WsState, WsStatus, WsStatusReq,
};

use crate::Ctx;
use crate::materialize;
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
        ..
    } = req;
    if limit == 0 {
        return Err(
            OpError::new(ErrorCode::InvalidArg, "limit must be at least 1")
                .with_details(Map::from_iter([("limit".to_owned(), Value::from(0))])),
        );
    }
    let paths = materialize::normalize(paths)?;

    // 2. The workspace, without its lock: a read.
    let ws = Workspace::open(ctx, Locate::Name(workspace))?;
    let want = Want { paths, full };
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
        .map(|change| {
            let mut out = WsChange::new(change.path.clone(), change.kind);
            out.size = change.new.map(|file| file.meta.size);
            out
        })
        .collect();
    let cursor = (shown.last())
        .filter(|_| truncated)
        .map(|last| last.path.clone());

    // 4. The state, and the rebase in progress.
    let (state, rebasing) = match &ws.file.state {
        State::Clean => (WsState::Clean, None),
        State::Rebasing { onto, .. } => {
            let rebasing = WsRebasing::new(onto.clone(), conflicts(ws));
            (WsState::Rebasing, Some(rebasing))
        }
    };
    let mut out = WsStatus::new(
        ws.file.name.clone(),
        ws.file.base.clone(),
        ws.file.head.clone(),
        main,
        behind_by,
        state,
        shown,
        truncated,
    );
    out.rebasing = rebasing;
    out.cursor = cursor;
    Ok(out)
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

/// The head of ref `target`, and how many changesets it holds that `base`'s first-parent
/// history lacks, at most [`BEHIND_CAP`]: one ref read, then `log` a page at a time until `base`;
/// when `base` is not on the target's history, `base`'s own history too, until it meets it.
fn behind(store: &dyn StoreOps, target: &str, base: &str) -> Result<(String, u64), OpError> {
    // 1. The head, a ref read; at the base, nothing to count.
    let main = store.resolve_rev(ResolveRevReq::new(target))?.id;
    if main == base {
        return Ok((main, 0));
    }

    // 2. The target's history until the base: each changeset's place on it, the base's the count.
    let mut count = 0;
    let mut seen = HashMap::new();
    let mut met = false;
    walk(store, &main, |id| {
        met = id == base;
        if !met {
            seen.insert(id.to_owned(), count);
            count += 1;
        }
        met
    })?;
    if met {
        return Ok((main, count));
    }

    // 3. The base off it (a sync dropped it, or the target moved back): the place of the first
    //    changeset of the base's history the target's holds; none, unrelated, the count as read.
    let mut parted = None;
    walk(store, base, |id| {
        parted = seen.get(id).copied();
        parted.is_some()
    })?;
    Ok((main, parted.unwrap_or(count)))
}

/// Visits `rev`'s first-parent history, newest first, a `log` page at a time, until `stop` says
/// so, the history ends or [`BEHIND_CAP`] changesets were visited.
fn walk(
    store: &dyn StoreOps,
    rev: &str,
    mut stop: impl FnMut(&str) -> bool,
) -> Result<(), OpError> {
    let mut visited = 0;
    let mut cursor = None;
    loop {
        // 1. The next page.
        let mut req = LogReq::default();
        req.rev = Some(rev.into());
        req.limit = LOG_PAGE;
        req.cursor = cursor.take();
        let page = store.log(req)?;

        // 2. Its changesets, until the stop or the cap.
        for entry in &page.entries {
            if visited == BEHIND_CAP || stop(&entry.id) {
                return Ok(());
            }
            visited += 1;
        }

        // 3. The next page, while there is one under the cap.
        if !page.truncated || visited == BEHIND_CAP {
            return Ok(());
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
