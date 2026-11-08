//! `ws status` (§4.3 of the M5 design): the workspace's changes against its head, found by stat
//! and the index, and how far its target has moved since its base.
//!
//! ```text
//!  resolve_rev{target} ──▶ main      equal to base: behind_by 0, no history read
//!  log{main, 100, cursor}…           until base, at most 10,000 counted
//!  log{base, 100, cursor}…           base not met: until a changeset main's log held
//!  scan (the index, the walker) ──▶ changes, a page after `cursor`, `limit` long
//!                                   and the edits off the prefixes, the first 1,000
//!
//!  stale (each command that reads or writes against the base, the M19 design §3):
//!  resolve_rev{target}, ancestry{base, head, limit 0}   the head reaches the base: current
//!  op_log{before…}                   until the move to the head, and the base's operation
//!                                    when `workspace.json` records none
//!
//!  landed (for `ws rebase`): the operations of a land key, then the target's history
//!  op_log{before…}                   to the journal's end: every changeset the key landed
//!  log{target, 100, cursor}…         until one of them: held; ended: absent
//! ```

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value};
use swarmops_api::{
    AncestryReq, ConflictReport, ErrorCode, LogReq, OpError, OpLogReq, OperationOut, ResolveRevReq,
    StoreOps, WsChange, WsRebasing, WsState, WsStatus, WsStatusReq,
};

use crate::Ctx;
use crate::materialize;
use crate::scan::{self, Change, Keep, Want};
use crate::state::{SWARMWS_DIR, State, WorkspaceFile};
use crate::workspace::{Locate, Workspace};

/// The most changesets `behind_by` counts: past it, the count stops there.
pub const BEHIND_CAP: u64 = 10_000;

/// The most entries off a sparse workspace's prefixes a status lists: past it,
/// `outside_truncated`.
pub const MAX_STATUS_OUTSIDE: usize = 1_000;

/// The changesets one `log` page of `behind_by` asks for.
const LOG_PAGE: usize = 100;

/// The most operations a search of the journal reads.
pub(crate) const OP_CAP: u64 = 10_000;

/// The operations one `op_log` page of that search asks for.
const OP_PAGE: usize = 100;

/// The changesets of an operation the searches ask to list: `stale` reads ref moves only, and
/// `landed` the land keys of those each operation lists: a land's one, or the two of a land that
/// carried changes, the changeset it committed first (the M16 design §4).
const OP_CHANGESETS: usize = 2;

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
    // 1. A base the target left: refused before anything is read further. Then where the
    //    target stands.
    refuse_stale(ctx.store, &ws.file)?;
    let (main, behind_by) = behind(ctx.store, &ws.file.target, &ws.file.base)?;

    // 2. The changes, the index kept for the next scan; the edits off the prefixes a commit
    //    refuses, the first of them.
    let mut scanned = scan::scan(ctx, ws, want)?;
    let (outside, outside_truncated) = capped(std::mem::take(&mut scanned.outside));
    let changes = scanned.keep(&ws.dir, &scan::binding_of(ws), Keep::IfChanged);

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
    out.outside = outside;
    out.outside_truncated = outside_truncated;
    Ok(out)
}

/// The first [`MAX_STATUS_OUTSIDE`] of `outside`, and whether more followed.
fn capped(mut outside: Vec<String>) -> (Vec<String>, bool) {
    let truncated = outside.len() > MAX_STATUS_OUTSIDE;
    outside.truncate(MAX_STATUS_OUTSIDE);
    (outside, truncated)
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

/// Whether workspace `file`'s base has left its target: the target's head does not reach it
/// along any parent (`ancestry`), and the operation that put the target on it is known, the one
/// `base_op` records or, without one, the newest of the journal's first [`OP_CAP`] that moved
/// the target there. A base no operation put on the target (a workspace made of a side
/// changeset) is not stale. While a rebase is in progress, its `onto` is the base judged.
pub(crate) fn stale(store: &dyn StoreOps, file: &WorkspaceFile) -> Result<Option<Stale>, OpError> {
    // 1. The target's head reaches the base: not stale, and the journal is not read.
    let (base, recorded) = judged(file);
    let target = file.target.as_str();
    let head = store.resolve_rev(ResolveRevReq::new(target))?.id;
    if head == base {
        return Ok(None);
    }
    let mut req = AncestryReq::new(base, head.as_str());
    req.limit = 0;
    if store.ancestry(req)?.is_ancestor {
        return Ok(None);
    }

    // 2. The journal, newest first: the move to the head, down to the recorded base's
    //    operation; without one, until the move to the base.
    let mut moved_op = None;
    let mut base_op = recorded;
    scan_ops(store, |op| {
        if recorded.is_some_and(|at| op.op <= at) {
            return true;
        }
        if moved_op.is_none() && moves(op, target, &head) {
            moved_op = Some(op.op);
        }
        if recorded.is_none() && moves(op, target, base) {
            base_op = Some(op.op);
        }
        match recorded {
            Some(_) => moved_op.is_some(),
            None => base_op.is_some(),
        }
    })?;
    Ok(base_op.map(|base_op| Stale {
        head,
        base_op,
        moved_op,
    }))
}

/// The base [`stale`] judges in workspace `file`, and its recorded operation: while a rebase is
/// in progress, its `onto` (the target's head it merges onto, no operation recorded); else the
/// base.
fn judged(file: &WorkspaceFile) -> (&str, Option<u64>) {
    match &file.state {
        State::Rebasing { onto, .. } => (onto.as_str(), None),
        State::Clean => (file.base.as_str(), file.base_op),
    }
}

/// Whether operation `op` moved ref `target` to `to`.
fn moves(op: &OperationOut, target: &str, to: &str) -> bool {
    (op.ref_moves.iter()).any(|m| m.name == target && m.new.as_deref() == Some(to))
}

/// `E_WS_STALE` when workspace `file`'s base has left its target ([`stale`]): what each command
/// that reads or writes against the base answers first.
pub(crate) fn refuse_stale(store: &dyn StoreOps, file: &WorkspaceFile) -> Result<(), OpError> {
    let Some(left) = stale(store, file)? else {
        return Ok(());
    };
    Err(stale_error(&file.target, judged(file).0, &left))
}

/// The newest operation that moved ref `target` to `base`, among the journal's first
/// [`OP_CAP`]: what `workspace.json` records once a command set its base. None for a base no
/// operation put there.
pub(crate) fn base_op(
    store: &dyn StoreOps,
    target: &str,
    base: &str,
) -> Result<Option<u64>, OpError> {
    let mut found = None;
    scan_ops(store, |op| {
        let moved = moves(op, target, base);
        if moved {
            found = Some(op.op);
        }
        moved
    })?;
    Ok(found)
}

/// `E_WS_STALE`: the workspace's `base` is gone from the history of `target`, which `stale`
/// says moved off it. Nothing was changed; `ws rebase` brings the workspace onto the target.
pub(crate) fn stale_error(target: &str, base: &str, stale: &Stale) -> OpError {
    OpError::new(
        ErrorCode::WsStale,
        format!(
            "the workspace's base {base} is no longer on {target}'s history: \
             {target} moved to {head}",
            head = stale.head,
        ),
    )
    .with_details(Map::from_iter([
        ("target".to_owned(), Value::from(target)),
        ("base".to_owned(), Value::from(base)),
        ("head".to_owned(), Value::from(stale.head.as_str())),
        ("base_op".to_owned(), Value::from(stale.base_op)),
        ("moved_op".to_owned(), Value::from(stale.moved_op)),
    ]))
    .with_hint("swarmops ws rebase")
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
/// journal names as a land of that key, looked for on the history within [`BEHIND_CAP`]
/// changesets (the store's own search for a key also looks on other refs, which this does not).
/// The journal is read back from its newest operation to the one that made `head`, the
/// changeset the record stands on: no land of this record can be older (a reused caller key may
/// have an older one, which the store does not replay for it), and the read ends there however
/// long the journal is. When that operation is not among the newest [`OP_CAP`], a key no
/// changeset of the cap's operations carries is not known to be missing. A key no operation
/// made is [`Landed::Absent`] once the journal is read back to `head`; a `head` no operation
/// lists gives [`Landed::Unknown`] only past the cap, as a journal within it is read to its end.
pub(crate) fn landed(
    store: &dyn StoreOps,
    target: &str,
    key: &str,
    head: &str,
) -> Result<Landed, OpError> {
    find(store, target, key, head).map(|(landed, _)| landed)
}

/// The changeset of a land with `key` that ref `target`'s history holds, found as [`landed`]
/// finds it: of two lands of one key, the one on the target, not the newer.
pub(crate) fn held(
    store: &dyn StoreOps,
    target: &str,
    key: &str,
    head: &str,
) -> Result<Option<String>, OpError> {
    find(store, target, key, head).map(|(_, held)| held)
}

/// [`landed`]'s answer, and the changeset of the key the target's history met, if any.
fn find(
    store: &dyn StoreOps,
    target: &str,
    key: &str,
    head: &str,
) -> Result<(Landed, Option<String>), OpError> {
    // 1. The journal back to the head's operation: every changeset a land of the key made.
    let mut made = HashSet::new();
    let scanned = scan_ops(store, |op| {
        let lands = (op.changesets.iter()).filter(|c| c.land_key.as_deref() == Some(key));
        made.extend(lands.map(|c| c.id.clone()));
        op.changesets.iter().any(|c| c.id == head)
    })?;
    let capped = matches!(scanned, Scanned::Capped);
    if made.is_empty() {
        return Ok((
            if capped {
                Landed::Unknown
            } else {
                Landed::Absent
            },
            None,
        ));
    }

    // 2. The target's history: one of them met, or ended without (an older copy past the
    //    journal's cap may be on it), or cut at the cap.
    let mut met = None;
    let walked = walk(store, target, |id| {
        let found = made.contains(id);
        if found {
            met = Some(id.to_owned());
        }
        found
    })?;
    let landed = match walked {
        Walked::Stopped => Landed::Held,
        Walked::Ended if capped => Landed::Unknown,
        Walked::Ended => Landed::Absent,
        Walked::Capped => Landed::Unknown,
    };
    Ok((landed, met))
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
