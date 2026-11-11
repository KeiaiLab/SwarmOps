//! Snapshots (the M19 design §4.1, §4.4): a workspace's edits on disk recorded as one keyed
//! changeset on its head, which moves no ref, and the per-path carry of an update's edits onto
//! the rebased head.
//!
//! ```text
//!  take    edits E ─▶ D(E) = BLAKE3 over sorted (path, kind, blob)
//!            none                        → Clean
//!            snapshot.json {head, D(E)}  → Same(S), no store call
//!            else commit{parent: head, E, key} (commit::record) → New(S), snapshot.json
//!
//!  carry   per path p of E, from three trees (head, S, H') and diff{head, H'}'s names:
//!            H' left p as head had it            → S's entry
//!            H' gave p S's entry                 → nothing to change
//!            both changed a text file, merged clean (head, S, H') → the merged bytes
//!            otherwise                           → dropped: H''s entry stays
//! ```
//!
//! `.swarmws/snapshot.json` `{format: 1, head, snapshot, edits, at}` names the latest snapshot,
//! replaced whole; one this build cannot read is no snapshot, and the next one is new.
//!
//! `ws snapshot` (the M19 design §5) takes one on demand, under the workspace's lock:
//!
//! ```text
//!  lock → not rebasing, files of the head → scan → Clean | Same: nothing written (a cut
//!         snapshot's record dropped)
//!       → journal/pending.json {op: snapshot, key, edits: D(E)} (WS1; a cut record of the same
//!         edits on this head gives its key, so that the store replays its commit)
//!       → take → the record removed
//! ```

use std::collections::BTreeSet;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use swarmops_api::{
    ChangeInput, ChangeOp, ContentInput, DiffFormat, DiffReq, EntryKindOut, ErrorCode, ModeInput,
    OpError, SnapshotWrite, TreeEntryOut, WsSnapshotReq, WsSnapshotResp,
};
use swarmops_core::ObjectId;
use swarmops_merge::conflict::ContentMerge;
use swarmops_merge::diff3;

use crate::hold::{self, Done, Op};
use crate::materialize::{self, Tree};
use crate::pending::{self, Pending, PendingOp};
use crate::rebase;
use crate::scan::{self, Change, Keep, Want};
use crate::state::State;
use crate::workspace::{self, Locate, Workspace};
use crate::{Ctx, after, commit};

/// The latest snapshot's record, in `.swarmws/`.
pub(crate) const SNAPSHOT_JSON: &str = "snapshot.json";

/// The layout of [`SNAPSHOT_JSON`] this build reads and writes.
const FORMAT: u32 = 1;

/// The message of a snapshot's changeset.
const MESSAGE: &str = "snapshot of the workspace's edits";

/// The entries one `diff` page asks for.
const PAGE: usize = 1000;

/// The state of `ws snapshot`'s pending record.
const SNAPSHOTTING: &str = "snapshotting";

/// What [`take`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Taken {
    /// A new changeset holds the edits.
    New(String),
    /// The latest snapshot holds these edits on this head already: nothing written.
    Same(String),
    /// No edits: nothing written.
    Clean,
}

/// A workspace's edits, hashed, with their digest and the key a new snapshot of them takes.
pub(crate) struct Found {
    /// The edits, by path, each naming its blob.
    pub(crate) changes: Vec<Change>,
    /// Their digest, [`digest`].
    pub(crate) digest: String,
    /// The key of the commit that records them: made and recorded durably by the caller.
    pub(crate) key: String,
}

/// What [`carry`] decided: the change list of `S'` on `H'`, and the paths whose edits did not
/// apply.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Carried {
    /// The changes `S'` makes on `H'`; none when every edit was dropped or is `H'`'s already.
    pub(crate) changes: Vec<ChangeInput>,
    /// The paths dropped, sorted: the files take `H'`'s bytes there.
    pub(crate) dropped: Vec<String>,
}

/// The record of the latest snapshot.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
struct Latest {
    /// [`FORMAT`].
    format: u32,
    /// The head it was taken on.
    head: String,
    /// The changeset.
    snapshot: String,
    /// The digest of the edits it holds.
    edits: String,
    /// When it was taken, in Unix milliseconds.
    at: u64,
}

/// The edits of workspace `ws` (tracked changes, and untracked additions the ignore rules let
/// in), hashed, with their digest and a new key; its index kept.
pub(crate) fn scan(ctx: &Ctx<'_>, ws: &Workspace) -> Result<Found, OpError> {
    // 1. The scan, its index kept as a status keeps it.
    let scanned = scan::scan(ctx, ws, &Want::default())?;
    let mut changes = scanned.keep(&ws.dir, &scan::binding_of(ws), Keep::IfChanged);

    // 2. Every file's blob, then the digest.
    commit::hash_changes(ctx, ws, &mut changes)?;
    let digest = digest(&changes);
    Ok(Found {
        changes,
        digest,
        key: rebase::new_key(),
    })
}

/// Records the edits `scanned` of workspace `ws` as a snapshot on its head, unless there are
/// none or the latest snapshot holds them on this head already. The caller holds the lock and
/// has made `scanned.key` durable.
pub(crate) fn take(ctx: &Ctx<'_>, ws: &Workspace, scanned: &Found) -> Result<Taken, OpError> {
    // 1. None, or those of the latest snapshot on this head: nothing written.
    if let Some(taken) = unwritten(ctx, ws, scanned) {
        return Ok(taken);
    }

    // 2. The keyed commit on the head; then the record names it.
    let id = commit::record(ctx, ws, &scanned.changes, MESSAGE, &scanned.key)?;
    write_latest(ctx, ws, &ws.file.head, &id, &scanned.digest)?;
    Ok(Taken::New(id))
}

/// What [`take`] makes of the edits `scanned` of workspace `ws` without writing: none is
/// `Clean`, those the latest snapshot holds on this head are `Same`; else none.
fn unwritten(ctx: &Ctx<'_>, ws: &Workspace, scanned: &Found) -> Option<Taken> {
    if scanned.changes.is_empty() {
        return Some(Taken::Clean);
    }
    let latest = latest(ctx, ws)?;
    (latest.head == ws.file.head && latest.edits == scanned.digest)
        .then_some(Taken::Same(latest.snapshot))
}

/// `ws_snapshot`: the edits of the workspace `req` names, recorded as a changeset on its head
/// that moves no ref, unless there are none or its latest snapshot holds them on this head.
/// The workspace's head, files and refs stay as they are.
pub fn snapshot(ctx: &Ctx<'_>, req: WsSnapshotReq) -> Result<WsSnapshotResp, OpError> {
    // 1. The workspace under its lock: not rebasing, its files those of its head.
    let WsSnapshotReq {
        workspace,
        status_after,
        ..
    } = req;
    let (ws, _held) = workspace::open_locked(ctx, &Locate::Name(workspace))?;
    refuse_unsettled(&ws)?;

    // 2. The edits; nothing written for none, or for those of the latest snapshot, but the
    //    record of a snapshot cut short after it was recorded, which would hold every other
    //    command.
    let found = scan(ctx, &ws)?;
    let taken = match unwritten(ctx, &ws, &found) {
        Some(taken) => {
            drop_cut(ctx, &ws)?;
            taken
        }
        None => recorded(ctx, &ws, found)?,
    };

    // 3. The answer, then the status after it.
    let mut renewed = Done::new(Op::Snapshot, &ws.file.head);
    let mut out = match taken {
        Taken::New(id) => {
            renewed = renewed.snapshot();
            WsSnapshotResp::new(Some(id), SnapshotWrite::New)
        }
        Taken::Same(id) => WsSnapshotResp::new(Some(id), SnapshotWrite::Same),
        Taken::Clean => WsSnapshotResp::new(None, SnapshotWrite::Clean),
    };
    out.status_after = after::attach(ctx, &ws, status_after);

    // 4. The hold record, the lock still held: the time of a snapshot written.
    hold::renew(ctx, &ws, &renewed);
    Ok(out)
}

/// The latest snapshot of workspace `ws`, on whichever head it was taken: what `ws info`
/// reports.
pub(crate) fn latest_id(ctx: &Ctx<'_>, ws: &Workspace) -> Option<String> {
    latest(ctx, ws).map(|latest| latest.snapshot)
}

/// Refuses a snapshot of workspace `ws` while it rebases, as `ws commit` does, and while its
/// files stand for another changeset than its head, whose edits are not the head's.
fn refuse_unsettled(ws: &Workspace) -> Result<(), OpError> {
    if let State::Rebasing { .. } = ws.file.state {
        return Err(OpError::new(
            ErrorCode::UnresolvedConflicts,
            "the workspace is rebasing: finish the rebase before taking a snapshot",
        )
        .with_hint("swarmops ws rebase --continue, or swarmops ws rebase --abort"));
    }
    if ws.file.materialized != ws.file.head {
        return Err(OpError::new(
            ErrorCode::InvalidArg,
            "the workspace's files stand for another changeset than its head: nothing to snapshot",
        ));
    }
    Ok(())
}

/// Takes the snapshot of the edits `found` of workspace `ws` behind a pending record of its own,
/// durable before the store call (WS1): a cut record of the same edits on this head gives its
/// key, so that the store replays its commit. The record goes once the snapshot is taken, or
/// when the store refused it outright.
fn recorded(ctx: &Ctx<'_>, ws: &Workspace, mut found: Found) -> Result<Taken, OpError> {
    // 1. The key: a cut snapshot's of the same edits on this head, else the new one.
    let fs = ctx.fs.as_ref();
    let cut = rebase::cut_short(ctx, ws, PendingOp::Snapshot, SNAPSHOTTING)?;
    if let Some(cut) = cut.filter(|cut| cut.edits.as_deref() == Some(found.digest.as_str())) {
        found.key = cut.key;
    }

    // 2. The record, durable before the store hears of the commit.
    let record = Pending {
        op: PendingOp::Snapshot,
        key: found.key.clone(),
        rebase_key: None,
        head: ws.file.head.clone(),
        target: None,
        attempt: 1,
        state: SNAPSHOTTING.to_owned(),
        snapshot_key: None,
        edits: Some(found.digest.clone()),
        snapshot: None,
        edits_key: None,
    };
    pending::write_pending(fs, &ws.dir, &record)?;

    // 3. The snapshot; a refusal wrote nothing, so its record has nothing to replay.
    let taken = match take(ctx, ws, &found) {
        Err(e) if refused(&e) => {
            pending::remove_pending(fs, &ws.dir)?;
            return Err(e);
        }
        taken => taken?,
    };
    pending::remove_pending(fs, &ws.dir)?;
    Ok(taken)
}

/// Removes the pending record of a snapshot cut short in workspace `ws`: when the edits are
/// none or the latest snapshot's, a re-run has nothing of it to replay.
fn drop_cut(ctx: &Ctx<'_>, ws: &Workspace) -> Result<(), OpError> {
    let fs = ctx.fs.as_ref();
    match pending::read_pending(fs, &ws.dir)? {
        Some(cut) if cut.op == PendingOp::Snapshot => pending::remove_pending(fs, &ws.dir),
        _ => Ok(()),
    }
}

/// Whether the store, or the size check before it, refused a snapshot's commit outright.
fn refused(e: &OpError) -> bool {
    rebase::refused(e) || e.code == ErrorCode::TooLarge.as_str()
}

/// The digest of `changes`: BLAKE3 over each one's path, kind and blob (none for a removal), in
/// path order. Two scans of the same edits give the same digest.
///
/// For example, `src/a.txt` modified to a file of blob `ab…` adds `src/a.txt\0file\0ab…\n`.
pub(crate) fn digest(changes: &[Change]) -> String {
    let mut sorted: Vec<&Change> = changes.iter().collect();
    sorted.sort_by(|a, b| a.path.cmp(&b.path));
    let mut hasher = blake3::Hasher::new();
    for change in sorted {
        let (kind, blob) = match change.new {
            Some(host) => (
                kind_name(host.kind),
                host.blob.map(|b| ObjectId::new(b).to_hex()),
            ),
            None => ("gone", None),
        };
        hasher.update(change.path.as_bytes());
        hasher.update(b"\0");
        hasher.update(kind.as_bytes());
        hasher.update(b"\0");
        hasher.update(blob.unwrap_or_default().as_bytes());
        hasher.update(b"\n");
    }
    hasher.finalize().to_hex().to_string()
}

/// The digest [`digest`] gives the edits `changes` make on the changeset they apply to, as a
/// scan of files holding them finds them: the latest snapshot's after an update.
pub(crate) fn digest_of(changes: &[ChangeInput]) -> Result<String, OpError> {
    // 1. Each change's line: its kind and its blob, hashed where the bytes are carried.
    let mut lines: Vec<(String, &'static str, String)> = Vec::with_capacity(changes.len());
    for change in changes {
        if change.op == ChangeOp::Delete {
            lines.push((change.path.clone(), "gone", String::new()));
            continue;
        }
        let kind = match change.mode {
            Some(ModeInput::Exec) => "exec",
            Some(ModeInput::Symlink) => "symlink",
            _ => "file",
        };
        let blob = match &change.content {
            Some(ContentInput::Blob(id)) => id.clone(),
            Some(ContentInput::Inline(text)) => hex_of(text.as_bytes()),
            Some(ContentInput::Base64(text)) => {
                let bytes = STANDARD.decode(text).map_err(|e| {
                    OpError::new(
                        swarmops_api::ErrorCode::Internal,
                        format!("a carried change holds bad base64: {e}"),
                    )
                })?;
                hex_of(&bytes)
            }
            _ => return Err(crate::io_error::unknown_variant("ContentInput")),
        };
        lines.push((change.path.clone(), kind, blob));
    }

    // 2. In path order, as [`digest`] hashes them.
    lines.sort();
    let mut hasher = blake3::Hasher::new();
    for (path, kind, blob) in lines {
        for part in [path.as_str(), "\0", kind, "\0", blob.as_str(), "\n"] {
            hasher.update(part.as_bytes());
        }
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// The blob id of `bytes`, in hex.
fn hex_of(bytes: &[u8]) -> String {
    ObjectId::new(scan::blob_id(bytes)).to_hex()
}

/// The paths changeset `s` (a snapshot) changes on `head`: the edits it holds.
pub(crate) fn edited(ctx: &Ctx<'_>, head: &str, s: &str) -> Result<Vec<String>, OpError> {
    Ok(names(ctx, head, s, &[])?.into_iter().collect())
}

/// Decides, per path of `edits` (those snapshot `s` changes on `head`), what `S'` on `h2`
/// (`head` rebased) holds there: from the three trees only, so a re-run decides the same.
pub(crate) fn carry(
    ctx: &Ctx<'_>,
    head: &str,
    s: &str,
    h2: &str,
    edits: &[String],
) -> Result<Carried, OpError> {
    // 1. The edited paths `H'` changed; the three trees.
    let mut out = Carried::default();
    if edits.is_empty() {
        return Ok(out);
    }
    let moved = names(ctx, head, h2, edits)?;
    let (old, snap, new) = (
        Tree::new(ctx.store, head),
        Tree::new(ctx.store, s),
        Tree::new(ctx.store, h2),
    );

    // 2. Each path: S's entry where `H'` left it, nothing where `H'` has it, a clean merge, or
    //    dropped.
    for path in edits {
        let ours = file_at(&snap, path)?;
        if !moved.contains(path) {
            out.changes.push(change_of(path, ours.as_ref())?);
            continue;
        }
        let theirs = file_at(&new, path)?;
        if same(ours.as_ref(), theirs.as_ref()) {
            continue;
        }
        let base = file_at(&old, path)?;
        match merged(ctx, path, base.as_ref(), ours.as_ref(), theirs.as_ref())? {
            Some(change) => out.changes.push(change),
            None => out.dropped.push(path.clone()),
        }
    }
    out.dropped.sort();
    Ok(out)
}

/// The latest snapshot of workspace `ws`; none when there is none or its record is one this
/// build does not read.
fn latest(ctx: &Ctx<'_>, ws: &Workspace) -> Option<Latest> {
    let latest: Latest = rebase::read_record(ctx, ws, SNAPSHOT_JSON).ok()??;
    (latest.format == FORMAT).then_some(latest)
}

/// Replaces workspace `ws`'s `snapshot.json`: snapshot `id` of the edits of digest `edits` on
/// `head`.
pub(crate) fn write_latest(
    ctx: &Ctx<'_>,
    ws: &Workspace,
    head: &str,
    id: &str,
    edits: &str,
) -> Result<(), OpError> {
    let at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
    let latest = Latest {
        format: FORMAT,
        head: head.to_owned(),
        snapshot: id.to_owned(),
        edits: edits.to_owned(),
        at,
    };
    rebase::write_record(ctx, ws, SNAPSHOT_JSON, &latest)
}

/// The paths that differ between changesets `from` and `to`, under `paths` (every path when
/// empty), every page.
fn names(
    ctx: &Ctx<'_>,
    from: &str,
    to: &str,
    paths: &[String],
) -> Result<BTreeSet<String>, OpError> {
    let mut found = BTreeSet::new();
    let mut cursor = None;
    loop {
        let mut req = DiffReq::new(from, to);
        req.paths = paths.to_vec();
        req.format = DiffFormat::Names;
        req.limit = PAGE;
        req.cursor = cursor.take();
        let page = ctx.store.diff(req)?;
        found.extend(page.files.into_iter().map(|file| file.path));
        if !page.truncated {
            return Ok(found);
        }
        cursor = page.cursor;
    }
}

/// The file `tree` holds at `path`: none for nothing or a directory.
fn file_at(tree: &Tree<'_>, path: &str) -> Result<Option<TreeEntryOut>, OpError> {
    Ok(tree
        .entry(path)?
        .filter(|entry| entry.kind != EntryKindOut::Dir))
}

/// Whether two files are the same: kind and blob, or both none.
fn same(a: Option<&TreeEntryOut>, b: Option<&TreeEntryOut>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.kind == b.kind && a.id == b.id,
        (None, None) => true,
        _ => false,
    }
}

/// The change that makes `path` hold `entry`, by blob id; its removal for none.
fn change_of(path: &str, entry: Option<&TreeEntryOut>) -> Result<ChangeInput, OpError> {
    let Some(entry) = entry else {
        return Ok(ChangeInput::new(path.to_owned(), ChangeOp::Delete));
    };
    let mut out = ChangeInput::new(path.to_owned(), ChangeOp::Put);
    out.content = Some(ContentInput::Blob(entry.id.clone()));
    out.mode = Some(mode_of(entry.kind)?);
    Ok(out)
}

/// The change of `path` that merges `ours` (the snapshot's) and `theirs` (`H'`'s) from `base`
/// (the old head's): a text file on all three sides, of a kind at most one side changed, whose
/// lines merge clean. None otherwise.
fn merged(
    ctx: &Ctx<'_>,
    path: &str,
    base: Option<&TreeEntryOut>,
    ours: Option<&TreeEntryOut>,
    theirs: Option<&TreeEntryOut>,
) -> Result<Option<ChangeInput>, OpError> {
    // 1. A file on all three sides, not a symlink, of a kind both can take.
    let (Some(base), Some(ours), Some(theirs)) = (base, ours, theirs) else {
        return Ok(None);
    };
    let plain = |e: &TreeEntryOut| matches!(e.kind, EntryKindOut::File | EntryKindOut::Exec);
    if !(plain(base) && plain(ours) && plain(theirs)) {
        return Ok(None);
    }
    let kind = match (ours.kind == base.kind, theirs.kind == base.kind) {
        (true, _) => theirs.kind,
        (false, true) => ours.kind,
        (false, false) if ours.kind == theirs.kind => ours.kind,
        (false, false) => return Ok(None),
    };

    // 2. The three contents, merged by lines; a conflict or a binary is no merge.
    let ids = vec![base.id.as_str(), ours.id.as_str(), theirs.id.as_str()];
    let contents = materialize::fetch(ctx.store, ids)?;
    let merge = diff3::merge_content(
        &contents[&base.id],
        &contents[&ours.id],
        &contents[&theirs.id],
    );
    let ContentMerge::Clean(bytes) = merge else {
        return Ok(None);
    };

    // 3. The merged bytes, carried in the commit.
    let content = match String::from_utf8(bytes) {
        Ok(text) => ContentInput::Inline(text),
        Err(e) => ContentInput::Base64(STANDARD.encode(e.into_bytes())),
    };
    let mut out = ChangeInput::new(path.to_owned(), ChangeOp::Put);
    out.content = Some(content);
    out.mode = Some(mode_of(kind)?);
    Ok(Some(out))
}

/// The mode a change puts for an entry of `kind`.
fn mode_of(kind: EntryKindOut) -> Result<ModeInput, OpError> {
    match kind {
        EntryKindOut::File => Ok(ModeInput::File),
        EntryKindOut::Exec => Ok(ModeInput::Exec),
        EntryKindOut::Symlink => Ok(ModeInput::Symlink),
        _ => Err(crate::io_error::unknown_variant("EntryKindOut")),
    }
}

/// The name [`digest`] gives a file's kind.
fn kind_name(kind: swarmops_api::FileKind) -> &'static str {
    match kind {
        swarmops_api::FileKind::File => "file",
        swarmops_api::FileKind::Exec => "exec",
        swarmops_api::FileKind::Symlink => "symlink",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use camino::Utf8Path;
    use swarmops_api::{CommitReq, Identity, StoreOps};
    use swarmops_ops::LocalOps;

    use super::*;
    use crate::RealFs;
    use crate::hostfs::RealHost;

    /// A store of a head holding `a.txt` and `b.txt`, a snapshot `S` and a rebased head `H'` on
    /// it, each the head with `ours` and `theirs` put; what `carry` decides for `a.txt`.
    fn carry_of(ours: &str, theirs: &str) -> Carried {
        // 1. A store, its head.
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(tmp.path()).unwrap().join("store");
        let (ops, _) = LocalOps::init(&root, "acme").unwrap();
        let who = Identity::parse("agent/tester").unwrap();
        let commit = |parent: Option<&str>, puts: &[(&str, &str)]| {
            let changes = (puts.iter())
                .map(|(path, text)| {
                    let mut out = ChangeInput::new((*path).to_owned(), ChangeOp::Put);
                    out.content = Some(ContentInput::Inline((*text).to_owned()));
                    out
                })
                .collect();
            let mut req = CommitReq::new(changes, "test");
            req.parent = parent.map(Into::into);
            ops.commit(&who, req).unwrap().changeset
        };
        let head = commit(None, &[("a.txt", "1\n2\n3\n"), ("b.txt", "b\n")]);

        // 2. S edits `a.txt`; H' changes it, or only `b.txt` when `theirs` is empty.
        let s = commit(Some(&head), &[("a.txt", ours)]);
        let h2 = if theirs.is_empty() {
            commit(Some(&head), &[("b.txt", "b2\n")])
        } else {
            commit(Some(&head), &[("a.txt", theirs)])
        };

        // 3. The carry of S's one edit.
        let ctx = Ctx::new(
            &ops,
            root.clone(),
            None,
            Arc::new(RealFs::new()),
            Arc::new(RealHost::default()),
        );
        carry(&ctx, &head, &s, &h2, &["a.txt".to_owned()]).unwrap()
    }

    /// The content a carried change puts.
    fn put_of(carried: &Carried) -> Option<&ContentInput> {
        carried
            .changes
            .first()
            .and_then(|change| change.content.as_ref())
    }

    /// `H'` left the path as the head had it: S's entry, by blob id.
    #[test]
    fn a_path_the_new_head_left_carries_the_snapshots_entry() {
        let carried = carry_of("1\nmine\n3\n", "");
        assert_eq!(carried.dropped, Vec::<String>::new());
        assert!(matches!(put_of(&carried), Some(ContentInput::Blob(_))));
    }

    /// `H'` holds S's entry already: nothing to change, nothing dropped.
    #[test]
    fn a_path_the_new_head_holds_already_changes_nothing() {
        let carried = carry_of("1\nsame\n3\n", "1\nsame\n3\n");
        assert_eq!(carried, Carried::default());
    }

    /// Both changed different lines of a text file: the merged bytes.
    #[test]
    fn a_clean_merge_carries_the_merged_bytes() {
        let carried = carry_of("one\n2\n3\n", "1\n2\nthree\n");
        assert_eq!(carried.dropped, Vec::<String>::new());
        assert_eq!(
            put_of(&carried),
            Some(&ContentInput::Inline("one\n2\nthree\n".to_owned()))
        );
    }

    /// Both changed the same line: dropped, no change.
    #[test]
    fn a_conflicting_edit_is_dropped() {
        let carried = carry_of("1\nmine\n3\n", "1\ntheirs\n3\n");
        assert_eq!(carried.changes, []);
        assert_eq!(carried.dropped, ["a.txt"]);
    }
}
