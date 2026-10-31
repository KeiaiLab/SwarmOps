//! `ws status` (§4.3 of the M5 design): the workspace's changes against its head, found by stat
//! and the index, and how far its target has moved since its base.
//!
//! ```text
//!  resolve_rev{target} ──▶ main      equal to base: behind_by 0, no history read
//!  log{main, 100, cursor}…           until base, at most 10,000 counted
//!  log{base, 100, cursor}…           base not met: until a changeset main's log held
//!  scan (the index, the walker) ──▶ changes, a page after `cursor`, `limit` long
//!
//!  stale (for `ws land`): base off main's history, history ended short of the cap
//!  op_log{before…}                   until the operation that put the target on the base
//!
//!  landed (for `ws rebase`): the operations of a land key, then the target's history
//!  op_log{before…}                   to the journal's end: every changeset the key landed
//!  log{target, 100, cursor}…         until one of them: held; ended: absent
//! ```

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value};
use swarmops_api::{
    ConflictReport, ErrorCode, LogReq, OpError, OpLogReq, OperationOut, ResolveRevReq, StoreOps,
    WsChange, WsRebasing, WsState, WsStatus, WsStatusReq,
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

/// The most operations the search for a stale base reads.
pub(crate) const OP_CAP: u64 = 10_000;

/// The operations one `op_log` page of that search asks for.
const OP_PAGE: usize = 100;

/// The changesets of an operation the searches ask to list: `stale` reads ref moves only, and
/// `landed` the land key of the one changeset each operation lists.
const OP_CHANGESETS: usize = 1;

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

/// Where a base stands on its target's first-parent history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Standing {
    /// On it.
    On,
    /// Off it: the history was read to its end without meeting the base.
    Off,
    /// Not met in the first [`BEHIND_CAP`] changesets: unknown.
    PastCap,
}

/// What reading a target's first-parent history for a base found.
struct Placed {
    /// The target's head.
    head: String,
    /// The changesets read before the base, or all of them.
    count: u64,
    /// Where each of them stands on the history, newest first from 0.
    seen: HashMap<String, u64>,
    /// Whether the base was met.
    standing: Standing,
}

/// The head of ref `target`, and how many changesets it holds that `base`'s first-parent
/// history lacks, at most [`BEHIND_CAP`]: one ref read, then `log` a page at a time until `base`;
/// when `base` is not on the target's history, `base`'s own history too, until it meets it.
fn behind(store: &dyn StoreOps, target: &str, base: &str) -> Result<(String, u64), OpError> {
    // 1. The target's history until the base: on it, the count is the base's place.
    let placed = place(store, target, base)?;
    if placed.standing == Standing::On {
        return Ok((placed.head, placed.count));
    }

    // 2. The base off it (a sync dropped it, or the target moved back): the place of the first
    //    changeset of the base's history the target's holds; none, unrelated, the count as read.
    let mut parted = None;
    walk(store, base, |id| {
        parted = placed.seen.get(id).copied();
        parted.is_some()
    })?;
    Ok((placed.head, parted.unwrap_or(placed.count)))
}

/// Reads the first-parent history of ref `target`, newest first, until `base`, the end of the
/// history or [`BEHIND_CAP`] changesets: the head, each changeset's place, and what became of the
/// base. At the base, no history is read.
fn place(store: &dyn StoreOps, target: &str, base: &str) -> Result<Placed, OpError> {
    // 1. The head, a ref read; at the base, nothing to count.
    let head = store.resolve_rev(ResolveRevReq::new(target))?.id;
    let mut placed = Placed {
        head,
        count: 0,
        seen: HashMap::new(),
        standing: Standing::On,
    };
    if placed.head == base {
        return Ok(placed);
    }

    // 2. The history until the base: each changeset's place on it, the base's the count.
    let mut met = false;
    let (count, seen) = (&mut placed.count, &mut placed.seen);
    let ended = walk(store, &placed.head, |id| {
        met = id == base;
        if !met {
            seen.insert(id.to_owned(), *count);
            *count += 1;
        }
        met
    })?;

    // 3. Met, or the history ended without it, or the cap came first.
    placed.standing = match (met, ended) {
        (true, _) => Standing::On,
        (false, Walked::Ended) => Standing::Off,
        (false, _) => Standing::PastCap,
    };
    Ok(placed)
}

/// How a walk of a history stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Walked {
    /// The caller's `stop` said so.
    Stopped,
    /// The history ended.
    Ended,
    /// [`BEHIND_CAP`] changesets were visited.
    Capped,
}

/// Visits `rev`'s first-parent history, newest first, a `log` page at a time, until `stop` says
/// so, the history ends or [`BEHIND_CAP`] changesets were visited.
fn walk(
    store: &dyn StoreOps,
    rev: &str,
    mut stop: impl FnMut(&str) -> bool,
) -> Result<Walked, OpError> {
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
            if visited == BEHIND_CAP {
                return Ok(Walked::Capped);
            }
            if stop(&entry.id) {
                return Ok(Walked::Stopped);
            }
            visited += 1;
        }

        // 3. The next page, while there is one under the cap.
        if !page.truncated {
            return Ok(Walked::Ended);
        }
        if visited == BEHIND_CAP {
            return Ok(Walked::Capped);
        }
        cursor = page.cursor;
    }
}

/// A base its target left: the operations that put the target on the base and on its head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Stale {
    /// The target's head now.
    pub(crate) head: String,
    /// The newest operation that moved the target to the base.
    pub(crate) base_op: u64,
    /// The newest operation that moved the target to `head`; none when not found.
    pub(crate) moved_op: Option<u64>,
}

/// How a read of the journal, newest operation first, stopped.
enum Scanned {
    /// The caller's `stop` said so.
    Stopped,
    /// The journal ended.
    Ended,
    /// [`OP_CAP`] operations were read.
    Capped,
}

/// Visits the journal's operations, newest first, a page at a time, until `stop` says so, the
/// journal ends or [`OP_CAP`] operations were visited.
fn scan_ops(
    store: &dyn StoreOps,
    mut stop: impl FnMut(&OperationOut) -> bool,
) -> Result<Scanned, OpError> {
    let mut visited = 0;
    let mut before = None;
    loop {
        // 1. The next page of operations.
        let mut req = OpLogReq::default();
        req.before = before;
        req.limit = OP_PAGE;
        req.max_changesets = OP_CHANGESETS;
        let page = store.op_log(req)?;

        // 2. Each operation, until the stop or the cap.
        for op in &page.ops {
            if visited == OP_CAP {
                return Ok(Scanned::Capped);
            }
            visited += 1;
            if stop(op) {
                return Ok(Scanned::Stopped);
            }
        }

        // 3. The next page, while there is one.
        before = page.ops.last().map(|op| op.op);
        if !page.truncated || before.is_none() {
            return Ok(Scanned::Ended);
        }
    }
}

/// Whether `base` has left ref `target`'s history: the first-parent history from the target's
/// head, read to its end within [`BEHIND_CAP`], lacks it, and the journal's newest [`OP_CAP`]
/// operations hold one that moved the target to it. A base no operation put on the target (a
/// workspace made of a side changeset) and a history longer than the cap are not stale.
pub(crate) fn stale(
    store: &dyn StoreOps,
    target: &str,
    base: &str,
) -> Result<Option<Stale>, OpError> {
    // 1. On the history, or not known to be off it: not stale, and the journal is not read.
    let placed = place(store, target, base)?;
    if placed.standing != Standing::Off {
        return Ok(None);
    }

    // 2. The journal, newest first: the move to the head, then the move to the base.
    let mut moved_op = None;
    let mut base_op = None;
    scan_ops(store, |op| {
        let moves = |to: &str| {
            (op.ref_moves.iter()).any(|m| m.name == target && m.new.as_deref() == Some(to))
        };
        if moved_op.is_none() && moves(&placed.head) {
            moved_op = Some(op.op);
        }
        if moves(base) {
            base_op = Some(op.op);
        }
        base_op.is_some()
    })?;
    Ok(base_op.map(|base_op| Stale {
        head: placed.head,
        base_op,
        moved_op,
    }))
}

/// Whether a land with a given key is on a ref's history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Landed {
    /// Its changeset is on the history: the store replays the land.
    Held,
    /// No changeset of the key is on it, or none was ever made: the store lands anew.
    Absent,
    /// The journal or the history is longer than its cap: not known.
    Unknown,
}

/// Whether a land with `key` is on the first-parent history of ref `target`: any changeset the
/// journal's newest [`OP_CAP`] operations name as a land of that key, looked for on the history
/// within [`BEHIND_CAP`] changesets, as the store's own search for a key does. A key no
/// operation made is [`Landed::Absent`].
pub(crate) fn landed(store: &dyn StoreOps, target: &str, key: &str) -> Result<Landed, OpError> {
    // 1. The journal: every changeset a land of the key made.
    let mut made = HashSet::new();
    let scanned = scan_ops(store, |op| {
        let lands = (op.changesets.iter()).filter(|c| c.land_key.as_deref() == Some(key));
        made.extend(lands.map(|c| c.id.clone()));
        false
    })?;
    let capped = matches!(scanned, Scanned::Capped);
    if made.is_empty() {
        return Ok(if capped {
            Landed::Unknown
        } else {
            Landed::Absent
        });
    }

    // 2. The target's history: one of them met, or ended without (an older copy past the
    //    journal's cap may be on it), or cut at the cap.
    let walked = walk(store, target, |id| made.contains(id))?;
    Ok(match walked {
        Walked::Stopped => Landed::Held,
        Walked::Ended if capped => Landed::Unknown,
        Walked::Ended => Landed::Absent,
        Walked::Capped => Landed::Unknown,
    })
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
