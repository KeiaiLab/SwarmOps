//! The hold record (the M21 design §10.4): who last changed a workspace, when, and what it
//! stood on then, in a file beside the registry.
//!
//! ```text
//! <store>/workspaces/hold/<name>.json
//!   {"format":1,"name","holder","since","last_op","last_op_at","lease_until",
//!    "target","base","base_op"?,"head","unlanded"?,"snapshot_at"?}
//!
//! a mutation that succeeded, its lock still held ─▶ renew: read the last record (FsOps),
//!   count, write a temp beside it, 0600, and rename it over (HostFs, no sync)
//! ws create ─▶ renew_locking: the workspace's lock taken, then renew; a repeated create, which
//!   changed nothing, writes only a record lost
//! ws delete ─▶ remove
//! ```
//!
//! A diagnostic, as an `OpRecord` is: nothing decides on it, so it is not synced, and a write
//! that fails is one `warning:` line on stderr, never the mutation's failure. A record lost or
//! torn reads as unknown, and the next mutation writes it again. It records and refuses nothing:
//! a second holder is not turned away and an expired one is not reclaimed (decision D21). It
//! holds no path, content or environment value. An older build's registry listing passes over
//! `hold/`, as it does over `tmp/`: neither ends in `.json`.

use std::fmt;
use std::io::{self, Write as _};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::Ctx;
use crate::lock;
use crate::registry::REGISTRY_DIR;
use crate::state::WorkspaceFile;
use crate::status;
use crate::workspace::Workspace;

/// The format of a hold record this build reads and writes.
pub const HOLD_FORMAT: u32 = 1;

/// How long a hold lasts past its last operation, in seconds (decision D22, proposed).
pub const WS_LEASE_S: u64 = 3_600;

/// The records' directory, in [`REGISTRY_DIR`].
const HOLD_DIR: &str = "hold";

/// Milliseconds in a second.
const MS_PER_S: u64 = 1_000;

/// A mutation that renews a hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Op {
    /// `ws create`.
    Create,
    /// `ws commit`.
    Commit,
    /// `ws rebase`, each of its actions.
    Rebase,
    /// `ws resolve`.
    Resolve,
    /// `ws land`.
    Land,
    /// `ws materialize`.
    Materialize,
    /// `ws discard`.
    Discard,
    /// `ws snapshot`.
    Snapshot,
}

impl Op {
    /// The name a record gives it.
    fn name(self) -> &'static str {
        match self {
            Op::Create => "create",
            Op::Commit => "commit",
            Op::Rebase => "rebase",
            Op::Resolve => "resolve",
            Op::Land => "land",
            Op::Materialize => "materialize",
            Op::Discard => "discard",
            Op::Snapshot => "snapshot",
        }
    }
}

/// A mutation that succeeded, as its record counts it.
#[derive(Debug, Clone)]
pub(crate) struct Done<'a> {
    /// Which.
    op: Op,
    /// The head the workspace had when the mutation took its lock.
    from: &'a str,
    /// The commits it added to the chain: a commit's one, a `ws rebase -m`'s.
    made: u64,
    /// Whether it recorded a snapshot of the edits.
    snapshot: Snapshot,
}

/// Whether a mutation recorded a snapshot of the edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Snapshot {
    /// It did.
    Taken,
    /// It did not.
    None,
}

impl<'a> Done<'a> {
    /// Mutation `op`, which took the lock with the workspace on head `from`; it made no commit
    /// and no snapshot.
    pub(crate) fn new(op: Op, from: &'a str) -> Done<'a> {
        Done {
            op,
            from,
            made: 0,
            snapshot: Snapshot::None,
        }
    }

    /// This mutation, which added `made` commits to the chain.
    pub(crate) fn made(mut self, made: u64) -> Done<'a> {
        self.made = made;
        self
    }

    /// This mutation, which recorded a snapshot of the edits.
    pub(crate) fn snapshot(mut self) -> Done<'a> {
        self.snapshot = Snapshot::Taken;
        self
    }
}

/// Whether a renew replaces a record already there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Replace {
    /// It does: the mutation changed the workspace.
    Always,
    /// It does not: a repeated `ws create`, which changed nothing, writes only a record lost.
    IfMissing,
}

/// One workspace's hold record. Times are Unix milliseconds.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
struct Hold {
    /// [`HOLD_FORMAT`].
    format: u32,
    /// The workspace's name, its file's.
    name: String,
    /// The identity of the last mutation: the caller's, else the workspace's creator.
    holder: String,
    /// When `holder` became the holder.
    since: u64,
    /// The last mutation's name.
    last_op: String,
    /// When it ended.
    last_op_at: u64,
    /// `last_op_at` plus [`WS_LEASE_S`].
    lease_until: u64,
    /// The ref the workspace lands on.
    target: String,
    /// The target's changeset the chain starts from.
    base: String,
    /// The operation that put the target on `base`, as `workspace.json` records it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    base_op: Option<u64>,
    /// The chain's last changeset.
    head: String,
    /// The commits of the chain not landed; omitted when not known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    unlanded: Option<u64>,
    /// When the edits were last snapshotted; omitted when never.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    snapshot_at: Option<u64>,
}

/// Writes workspace `ws`'s hold record after mutation `done` succeeded, the caller still holding
/// the lock. Unsynced; a failure is a `warning:` line on stderr, never an error.
pub(crate) fn renew(ctx: &Ctx<'_>, ws: &Workspace, done: &Done<'_>) {
    // 1. The last record, if this build reads it; the time now.
    let file = &ws.file;
    let before = read(ctx, &file.name);
    let now = now_ms();

    // 2. The holder, held since it last changed.
    let holder = ws.who.agent().to_owned();
    let since = match &before {
        Some(before) if before.holder == holder => before.since,
        _ => now,
    };

    // 3. The counts the mutation knows; a new workspace starts with none.
    let unlanded = unlanded(ctx, file, done, before.as_ref());
    let snapshot_at = match (done.snapshot, &before) {
        (Snapshot::Taken, _) => Some(now),
        (Snapshot::None, _) if done.op == Op::Create => None,
        (Snapshot::None, before) => before.as_ref().and_then(|before| before.snapshot_at),
    };

    // 4. The record, a temp renamed over the last one.
    let hold = Hold {
        format: HOLD_FORMAT,
        name: file.name.clone(),
        holder,
        since,
        last_op: done.op.name().to_owned(),
        last_op_at: now,
        lease_until: now.saturating_add(WS_LEASE_S * MS_PER_S),
        target: file.target.clone(),
        base: file.base.clone(),
        base_op: file.base_op,
        head: file.head.clone(),
        unlanded,
        snapshot_at,
    };
    let Some(rel) = rel(&file.name) else {
        return;
    };
    let bytes = serde_json::to_vec(&hold).expect("a hold record serializes");
    let dir = ctx.store_root.join(REGISTRY_DIR);
    if let Err(e) = ctx.host.write_private(&dir, &rel, &bytes) {
        warn(&file.name, "written", &e.kind());
    }
}

/// Renews workspace `ws`'s hold record as [`renew`] does, taking the workspace's lock for it, as
/// every other mutation holds it when it renews: `ws create`, which takes none. `replace` says
/// whether a record there is replaced. A lock not taken is a `warning:` line, never an error.
pub(crate) fn renew_locking(ctx: &Ctx<'_>, ws: &Workspace, done: &Done<'_>, replace: Replace) {
    // 1. The lock, so that no other mutation's renew runs beside this one.
    let name = &ws.file.name;
    let _held = match lock::acquire(&ws.dir) {
        Ok(held) => held,
        Err(e) => {
            warn(name, "written", &e.code);
            return;
        }
    };

    // 2. A record there kept when asked; else renewed.
    if replace == Replace::IfMissing && read(ctx, name).is_some() {
        return;
    }
    renew(ctx, ws, done);
}

/// Removes workspace `name`'s hold record, if there is one; a failure is a `warning:` line.
pub(crate) fn remove(ctx: &Ctx<'_>, name: &str) {
    let Some(rel) = rel(name) else {
        return;
    };
    let dir = ctx.store_root.join(REGISTRY_DIR);
    match ctx.host.remove_file(&dir, &rel) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => warn(name, "removed", &e.kind()),
    }
}

/// The commits of `file`'s chain not landed, after mutation `done`: none after a land; the last
/// record's count plus what `done` made, when that record names the head `done` started from;
/// else counted on the chain (`None` past the count's cap or on a store error).
fn unlanded(
    ctx: &Ctx<'_>,
    file: &WorkspaceFile,
    done: &Done<'_>,
    before: Option<&Hold>,
) -> Option<u64> {
    // 1. A land leaves none; a create counts its chain, which is empty unless it found one.
    match done.op {
        Op::Land => return Some(0),
        Op::Create => return count(ctx, file),
        _ => {}
    }

    // 2. The last count, when it speaks of the head this mutation started from.
    let kept = before
        .filter(|before| before.head == done.from)
        .and_then(|before| before.unlanded);
    match kept {
        Some(kept) => Some(kept.saturating_add(done.made)),
        None => count(ctx, file),
    }
}

/// The changesets of `file`'s chain, counted on the store.
fn count(ctx: &Ctx<'_>, file: &WorkspaceFile) -> Option<u64> {
    status::chain_len(ctx.store, &file.head, &file.base)
        .ok()
        .flatten()
}

/// Workspace `name`'s record, if there is one this build reads: in [`HOLD_FORMAT`], of that name.
fn read(ctx: &Ctx<'_>, name: &str) -> Option<Hold> {
    let path = ctx.store_root.join(REGISTRY_DIR).join(rel(name)?);
    let bytes = ctx.fs.read(path.as_std_path()).ok().flatten()?;
    let hold: Hold = serde_json::from_slice(&bytes).ok()?;
    (hold.format == HOLD_FORMAT && hold.name == name).then_some(hold)
}

/// Workspace `name`'s record, below [`REGISTRY_DIR`]; none for a name that is not one file name
/// in [`HOLD_DIR`] (empty, hidden, or holding a separator), which the registry's grammar refuses
/// but a `workspace.json` edited by hand might name.
fn rel(name: &str) -> Option<String> {
    let one = !name.is_empty() && !name.starts_with('.') && !name.contains(['/', '\\']);
    one.then(|| format!("{HOLD_DIR}/{name}.json"))
}

/// Says on stderr that workspace `name`'s record was not `what`, and `why`: an error's kind or
/// code, never its text, which may name a host path.
fn warn(name: &str, what: &str, why: &dyn fmt::Display) {
    let _ = writeln!(
        io::stderr().lock(),
        "warning: the hold record of workspace {name:?} was not {what}: {why}"
    );
}

/// The time now, in Unix milliseconds.
fn now_ms() -> u64 {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
}
