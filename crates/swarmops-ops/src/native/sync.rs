//! A host's native sync and push (the native sync design §2.4, S1–S4): the changes of `main`
//! landed by the upstream, each once under its key, and `main` moved to what the upstream holds;
//! a branch or a tag moved there by compare-and-swap with its ids kept.
//!
//! ```text
//!   sync of the followed branch b (main)
//!     attempt 1..max_attempts:
//!       fetch (F1–F6)                     T = the tracking ref of b; M = main
//!       no T      → put M's history the upstream lacks, then b: none → M     pushed
//!       no M      → main := T                                                fast_forwarded
//!       S1 plan: base B, changes U1..Un; drop each whose key a first parent of T above B holds
//!          none left → main := T (nothing when M = T)          up_to_date | fast_forwarded
//!          mode local → landed onto T here, main := that                     landed_locally
//!       S2 has, then put what the upstream lacks: blobs first, then changesets, parents first
//!       S3 per change: land b, base = its parent, head = it (the upstream keys it, ss1-)
//!          E_CONFLICT → stop: fetch, main := the upstream's head, the refused ones named
//!       S4 fetch again; main: M → T' by compare-and-swap (lost: again)       pushed
//!
//!   push of a branch or a tag
//!     refs; put what the upstream lacks of its history; move it: old → head
//! ```
//!
//! `main` on the host moves only to what the upstream's `b` names (the tracking ref, which a fetch
//! moves forward only): the floor of the M16 forward rule. A change refused by a conflict, and
//! every later one, stays in the store under its id; rebased onto `main` it is a new change with
//! a new key, which the next sync lands.

use std::collections::{BTreeMap, HashSet};

use serde_json::Value;
use swarmops_api::wire::{WIRE_HAS, WIRE_LAND, WIRE_MOVE, WIRE_PUT};
use swarmops_api::{
    DropReason, DroppedChange, ErrorCode, HeadMove, Identity, LandResp, MAX_CHUNK_BYTES,
    MAX_WIRE_IDS, OpError, PublishedChange, PushResp, PushStatus, RemoteInfo, SyncMode, SyncResp,
    SyncStatusKind, WireHas, WireHasReq, WireLandReq, WireMove, WireMoveMode, WireMoveReq, WirePut,
    WirePutReq,
};
use swarmops_core::{ChangesetId, ObjectId, ObjectKind, Signature, bytes};
use swarmops_git::sync::{self as engine, SyncError};
use swarmops_repo::{CasResult, LocalRepo};
use swarmops_store::{Recovered, RefKind, ref_kind};

use super::chunk;
use super::fetch::{self, Fetched, ask, check_format, not_native, outcome, puts, refs, request};
use super::link::Link;
use super::refused;
use super::ssh::SshLink;
use super::url::NativeUrl;
use crate::error;
use crate::local::Recording;

/// The store ref a sync moves.
const MAIN: &str = "main";

/// The prefix of a tracking ref: `remote/<remote>/<ref>`.
const TRACKING_PREFIX: &str = "remote/";

/// The prefix of the actor a push's tracking-ref move is recorded under: `remote/<remote>`.
const ACTOR_PREFIX: &str = "remote/";

/// The metadata key of a changeset's agent, and of a landed changeset's key.
const AGENT_KEY: &str = "agent";
const LAND_KEY: &str = "land_key";

/// A record's header in a chunk: kind, id, payload length.
const RECORD_HEADER_LEN: u64 = 37;

/// The reason of a native push of `main` or `release/*`: they are landed, by `sync`.
const LAND_WITH_SYNC: &str = "land_with_sync";

/// What a sync is asked to do.
pub(crate) struct SyncArgs<'a> {
    /// The remote, by its name.
    pub(crate) remote: &'a str,
    /// The followed branch: what the changes land on.
    pub(crate) branch: &'a str,
    /// Who moves `main`.
    pub(crate) actor: &'a str,
    /// The committer of each change landed here (mode local).
    pub(crate) lander: &'a Signature,
    /// Whether the upstream lands the changes.
    pub(crate) mode: SyncMode,
    /// How many times a sync starts again when `main` moved under it.
    pub(crate) max_attempts: u32,
}

/// What a sync did.
#[derive(Debug)]
pub(crate) struct Synced {
    /// How it ended.
    pub(crate) status: SyncStatusKind,
    /// The attempt it ended in.
    pub(crate) attempts: u32,
    /// `main` before, and now.
    pub(crate) main_old: Option<ChangesetId>,
    pub(crate) main_new: Option<ChangesetId>,
    /// The upstream's branch as the first fetch found it, and now.
    pub(crate) remote_old: Option<ChangesetId>,
    pub(crate) remote_new: Option<ChangesetId>,
    /// Each change, and the changeset it landed as.
    pub(crate) published: Vec<(ChangesetId, ChangesetId)>,
    /// The changes the upstream held already, by their keys.
    pub(crate) dropped: Vec<ChangesetId>,
    /// What the store repaired before a move of `main`.
    pub(crate) recovered: Vec<Recovered>,
    /// What the sync has to say that did not stop it.
    pub(crate) warnings: Vec<String>,
}

/// What one attempt came to.
enum Step {
    /// The sync is done.
    Done(SyncStatusKind),
    /// `main` moved under it: again.
    MainMoved,
}

/// What a push did.
#[derive(Debug)]
pub(crate) struct Pushed {
    /// How it ended.
    pub(crate) status: PushStatus,
    /// What the upstream's ref named before, and names now.
    pub(crate) old: Option<ChangesetId>,
    pub(crate) new: ChangesetId,
    /// How many objects it sent.
    pub(crate) sent: u64,
}

/// S1–S4 of [the module documentation](self): `args.branch` of the upstream behind `link`
/// synced with `main` of `repo`.
pub(crate) fn sync(
    repo: &LocalRepo,
    link: &mut dyn Link,
    args: &SyncArgs<'_>,
) -> Result<Synced, OpError> {
    let mut out = Synced {
        status: SyncStatusKind::UpToDate,
        attempts: 0,
        main_old: main_now(repo)?,
        main_new: None,
        remote_old: None,
        remote_new: None,
        published: Vec::new(),
        dropped: Vec::new(),
        recovered: Vec::new(),
        warnings: Vec::new(),
    };
    for attempt in 1..=args.max_attempts {
        // 1. The upstream's refs, then main: a move of main under the attempt fails its swap.
        let fetched = fetch::fetch(repo, link, args.remote)?;
        if attempt == 1 {
            out.remote_old = fetched.before.get(args.branch).copied();
        }
        out.warnings.extend(fetched.warnings.iter().cloned());
        let main = main_now(repo)?;

        // 2. The attempt.
        out.attempts = attempt;
        match attempt_once(repo, link, args, main, &fetched, &mut out)? {
            Step::Done(status) => {
                out.status = status;
                return Ok(out);
            }
            Step::MainMoved => out.published.clear(),
        }
    }
    Err(exhausted(args))
}

/// One attempt from `main` and what `fetched` found.
fn attempt_once(
    repo: &LocalRepo,
    link: &mut dyn Link,
    args: &SyncArgs<'_>,
    main: Option<ChangesetId>,
    fetched: &Fetched,
    out: &mut Synced,
) -> Result<Step, OpError> {
    // 1. An upstream without the branch: main's history sent, the branch made there.
    let Some(&theirs) = fetched.heads.get(args.branch) else {
        return onto_nothing(repo, link, args, main, fetched, out);
    };
    out.remote_new = Some(theirs);

    // 2. No main: main is the upstream's head.
    let Some(main) = main else {
        return moved(repo, args, None, theirs, SyncStatusKind::FastForwarded, out);
    };

    // 3. S1: the changes, less those the upstream landed already (N5).
    let plan = engine::plan(repo, main, theirs).map_err(|e| engine_error(e, args))?;
    let keys = landed_keys(repo, theirs, plan.base)?;
    let (dropped, to_land): (Vec<ChangesetId>, Vec<ChangesetId>) = (plan.unpublished.iter())
        .partition(|&&id| keys.contains(&super::key::sync_key(args.branch, id)));
    out.dropped = dropped;
    if to_land.is_empty() {
        if main == theirs {
            out.main_new = Some(main);
            return Ok(Step::Done(SyncStatusKind::UpToDate));
        }
        return moved(
            repo,
            args,
            Some(main),
            theirs,
            SyncStatusKind::FastForwarded,
            out,
        );
    }

    // 4. No push asked for: the changes landed onto the upstream's head here.
    if args.mode == SyncMode::Local {
        let relocated = engine::relocate(repo, &to_land, theirs, args.lander)
            .map_err(|e| engine_error(e, args))?;
        out.published = relocated.landed;
        let head = relocated.head;
        return moved(
            repo,
            args,
            Some(main),
            head,
            SyncStatusKind::LandedLocally,
            out,
        );
    }

    // 5. S2, S3: what the upstream lacks, then each change landed in order.
    let haves: Vec<ChangesetId> = fetched.heads.values().copied().collect();
    upload(repo, link, main, &haves)?;
    for (n, &change) in to_land.iter().enumerate() {
        match land(repo, link, args, change) {
            Ok(landed) => out.published.push((change, landed)),
            Err(e) if e.code == ErrorCode::Conflict.as_str() => {
                return Err(refused_from(repo, link, args, main, &to_land[n..], e, out));
            }
            Err(e) => return Err(e),
        }
    }

    // 6. S4: what landed fetched, main moved there.
    let fetched = fetch::fetch(repo, link, args.remote)?;
    let head = upstream_head(&fetched, args)?;
    out.remote_new = Some(head);
    moved(repo, args, Some(main), head, SyncStatusKind::Pushed, out)
}

/// An attempt onto an upstream with no branch `args.branch`: `main`'s history sent, and the
/// branch made at `main`, ids kept.
fn onto_nothing(
    repo: &LocalRepo,
    link: &mut dyn Link,
    args: &SyncArgs<'_>,
    main: Option<ChangesetId>,
    fetched: &Fetched,
    out: &mut Synced,
) -> Result<Step, OpError> {
    let branch = args.branch;
    let remote = error::shell_word(args.remote);
    let Some(main) = main else {
        return Err(error::error(
            ErrorCode::RevNotFound,
            format!("neither main nor {branch} on the remote exists: nothing to sync"),
        )
        .with_details(error::details([("rev", MAIN.into())])));
    };
    if args.mode == SyncMode::Local {
        return Err(error::error(
            ErrorCode::RevNotFound,
            format!("the remote has no branch {branch} to land main onto"),
        )
        .with_details(error::details([("rev", branch.into())]))
        .with_hint(error::hint(format!("swarmops sync {remote} creates it"))));
    }

    // 1. The history, then the branch, made where main is.
    let haves: Vec<ChangesetId> = fetched.heads.values().copied().collect();
    upload(repo, link, main, &haves)?;
    move_ref(link, branch, None, main)?;

    // 2. The branch fetched: its tracking ref at main.
    let fetched = fetch::fetch(repo, link, args.remote)?;
    out.remote_new = Some(upstream_head(&fetched, args)?);
    out.main_new = Some(main);
    Ok(Step::Done(SyncStatusKind::Pushed))
}

/// `main` moved from `old` to `new`, the attempt done as `status`; or again when `main` moved
/// meanwhile.
fn moved(
    repo: &LocalRepo,
    args: &SyncArgs<'_>,
    old: Option<ChangesetId>,
    new: ChangesetId,
    status: SyncStatusKind,
    out: &mut Synced,
) -> Result<Step, OpError> {
    if !move_main(repo, args, old, new, out)? {
        return Ok(Step::MainMoved);
    }
    Ok(Step::Done(status))
}

/// Moves `main` from `old` (none: makes it) to `new` by compare-and-swap; false when `main`
/// named something else. A replay (`main` at `new` already) is a move.
fn move_main(
    repo: &LocalRepo,
    args: &SyncArgs<'_>,
    old: Option<ChangesetId>,
    new: ChangesetId,
    out: &mut Synced,
) -> Result<bool, OpError> {
    let outcome = repo
        .cas_ref(MAIN, old, Some(new), args.actor)
        .map_err(error::op)?;
    out.recovered.push(outcome.recovered);
    out.warnings.extend(outcome.warnings);
    let moved = match outcome.result {
        CasResult::Applied { .. } => true,
        CasResult::Mismatch { actual } => actual == Some(new),
        _ => return Err(error::unknown_variant("CasResult")),
    };
    if moved {
        out.main_new = Some(new);
    }
    Ok(moved)
}

/// The keys of the changesets on `theirs`'s first parents above `base`: what the upstream
/// landed on `branch` since the host's changes left it.
fn landed_keys(
    repo: &LocalRepo,
    theirs: ChangesetId,
    base: ChangesetId,
) -> Result<HashSet<String>, OpError> {
    let mut keys = HashSet::new();
    let mut at = Some(theirs);
    let mut walked = 0;
    while let Some(id) = at {
        if id == base || walked >= engine::MAX_SYNC_WALK {
            break;
        }
        walked += 1;
        let stored = repo.changeset(id).map_err(error::op)?;
        let Some(stored) = stored else {
            break;
        };
        let key = metadata(&stored.changeset.metadata, LAND_KEY);
        if let Some(key) = key.filter(|key| key.starts_with(super::key::SYNC_KEY_PREFIX)) {
            keys.insert(key);
        }
        at = stored.changeset.parents.first().copied();
    }
    Ok(keys)
}

/// The text of metadata `key` among `metadata`, if it is there and UTF-8.
fn metadata(metadata: &[(String, Vec<u8>)], key: &str) -> Option<String> {
    let value = metadata.iter().find(|(k, _)| k == key)?;
    String::from_utf8(value.1.clone()).ok()
}

/// S3 for `change`: landed on `args.branch` from its parent; the changeset it landed as. A busy
/// upstream is asked again, up to `args.max_attempts` times.
fn land(
    repo: &LocalRepo,
    link: &mut dyn Link,
    args: &SyncArgs<'_>,
    change: ChangesetId,
) -> Result<ChangesetId, OpError> {
    let meta = repo.changeset_meta(change).map_err(error::op)?;
    let Some(base) = meta.and_then(|meta| meta.parents.first().copied()) else {
        return Err(error::error(
            ErrorCode::Internal,
            format!("{change} has no parent to land from"),
        ));
    };
    let mut req = WireLandReq::new(args.branch, base.to_string(), change.to_string());
    req.max_attempts = args.max_attempts;
    let mut tries = 0;
    loop {
        tries += 1;
        let landed: Result<LandResp, OpError> = ask(link, WIRE_LAND, req.clone());
        match landed {
            Ok(landed) => return fetch::changeset_id(&landed.landed),
            Err(e) if e.code == ErrorCode::Busy.as_str() && tries < args.max_attempts => {}
            Err(e) => return Err(e),
        }
    }
}

/// The conflict `e` of the first of `refused`, the changes left: the upstream's head fetched and
/// `main` moved there, before the refused ones; `e` naming them, the change's agent, and
/// whether `main` moved.
fn refused_from(
    repo: &LocalRepo,
    link: &mut dyn Link,
    args: &SyncArgs<'_>,
    main: ChangesetId,
    refused: &[ChangesetId],
    mut e: OpError,
    out: &mut Synced,
) -> OpError {
    // 1. Main at the upstream's head, which holds what landed before: never past the refused.
    let moved = fetch::fetch(repo, link, args.remote)
        .and_then(|fetched| upstream_head(&fetched, args))
        .and_then(|head| move_main(repo, args, Some(main), head, out));

    // 2. What the error names.
    let change = refused[0];
    let agent = repo
        .changeset(change)
        .ok()
        .flatten()
        .and_then(|stored| metadata(&stored.changeset.metadata, AGENT_KEY));
    let ids: Vec<Value> = refused.iter().map(|id| id.to_string().into()).collect();
    e.details
        .insert("change".to_owned(), change.to_string().into());
    if let Some(agent) = agent {
        e.details.insert("agent".to_owned(), agent.into());
    }
    e.details.insert("refused".to_owned(), Value::Array(ids));
    e.details
        .insert("main_moved".to_owned(), Value::Bool(moved.unwrap_or(false)));
    e.hint = Some(error::hint(format!(
        "rebase each refused change onto main, oldest first (swarmops rebase {change} --base \
         <its parent>, then ref set main), and sync again"
    )));
    e
}

/// The upstream's `args.branch` as `fetched` found it; `E_REMOTE_REJECTED` when it is gone.
fn upstream_head(fetched: &Fetched, args: &SyncArgs<'_>) -> Result<ChangesetId, OpError> {
    fetched.heads.get(args.branch).copied().ok_or_else(|| {
        error::error(
            ErrorCode::RemoteRejected,
            format!(
                "{} has no {} after the sync wrote it",
                args.remote, args.branch
            ),
        )
        .with_details(error::details([("reason", "branch_gone".into())]))
    })
}

/// S2: the changesets `head` reaches that no `haves` does and the upstream lacks, and the blobs
/// their deltas name that it lacks, put: blobs first, then changesets, parents first, a chunk
/// per request. How many objects it sent.
fn upload(
    repo: &LocalRepo,
    link: &mut dyn Link,
    head: ChangesetId,
    haves: &[ChangesetId],
) -> Result<u64, OpError> {
    // 1. What head reaches that the haves do not, highest generation first.
    let mut reached = Vec::new();
    {
        let read = repo.snapshot().map_err(error::op)?;
        let mut after = None;
        loop {
            let page = read.missing(haves, head, MAX_WIRE_IDS, after);
            let page = page.map_err(error::op)?;
            reached.extend(page.changesets);
            match page.next {
                Some(next) => after = Some(next),
                None => break,
            }
        }
    }

    // 2. Of those, what the upstream lacks, parents first; then the blobs they name it lacks.
    let ids: Vec<ObjectId> = reached.iter().map(|id| id.0).collect();
    let mut changesets = lacking(link, &ids)?;
    changesets.reverse();
    let mut blobs = Vec::new();
    let mut seen = HashSet::new();
    for &id in &changesets {
        let Some((ObjectKind::Changeset, payload)) = repo.object(id).map_err(error::op)? else {
            return Err(error::error(
                ErrorCode::Internal,
                format!("{id} is no changeset the store holds"),
            ));
        };
        let hashed = swarmops_core::Hashed::new(ObjectKind::Changeset, payload);
        for blob in puts(&hashed)? {
            if seen.insert(blob) {
                blobs.push(blob.0);
            }
        }
    }
    let blobs = lacking(link, &blobs)?;

    // 3. The puts: blobs, then changesets, each adopted once its parents are.
    let sent = put_all(repo, link, &blobs, Adopt::None)?;
    Ok(sent + put_all(repo, link, &changesets, Adopt::Each)?)
}

/// Whether a put adopts the changesets it carries.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Adopt {
    /// It carries blobs.
    None,
    /// Each changeset it carries, in order.
    Each,
}

/// `ids` of `wire_has`'s answer the upstream does not hold, in their order.
fn lacking(link: &mut dyn Link, ids: &[ObjectId]) -> Result<Vec<ObjectId>, OpError> {
    let mut out = Vec::new();
    for group in ids.chunks(MAX_WIRE_IDS) {
        let asked = group.iter().map(ToString::to_string).collect();
        let has: WireHas = ask(link, WIRE_HAS, WireHasReq::new(asked))?;
        if has.held.len() != group.len() {
            return Err(refused("answer_line", "a has answer of another length"));
        }
        let lacks = group.iter().zip(&has.held).filter(|(_, held)| !**held);
        out.extend(lacks.map(|(id, _)| *id));
    }
    Ok(out)
}

/// `ids` put, a chunk at a time: records within [`MAX_CHUNK_BYTES`], one past it alone, at most
/// [`MAX_WIRE_IDS`] each. How many it sent.
fn put_all(
    repo: &LocalRepo,
    link: &mut dyn Link,
    ids: &[ObjectId],
    adopt: Adopt,
) -> Result<u64, OpError> {
    let mut group: Vec<(ObjectKind, ObjectId, Vec<u8>)> = Vec::new();
    let mut used = 0;
    let mut sent = 0;
    for &id in ids {
        let Some((kind, payload)) = repo.object(id).map_err(error::op)? else {
            return Err(error::error(
                ErrorCode::Internal,
                format!("{id} is no object the store holds"),
            ));
        };
        let record = RECORD_HEADER_LEN + bytes::to_u64(payload.len());
        let full = used + record > MAX_CHUNK_BYTES || group.len() == MAX_WIRE_IDS;
        if !group.is_empty() && full {
            sent += put_one(link, &group, adopt)?;
            group.clear();
            used = 0;
        }
        used += record;
        group.push((kind, id, payload));
    }
    if !group.is_empty() {
        sent += put_one(link, &group, adopt)?;
    }
    Ok(sent)
}

/// One `wire_put` of `group` as one chunk.
fn put_one(
    link: &mut dyn Link,
    group: &[(ObjectKind, ObjectId, Vec<u8>)],
    adopt: Adopt,
) -> Result<u64, OpError> {
    // 1. The chunk.
    let records: Vec<(ObjectKind, ObjectId, &[u8])> = (group.iter())
        .map(|(kind, id, payload)| (*kind, *id, payload.as_slice()))
        .collect();
    let mut upload = Vec::new();
    chunk::write_chunk(&mut upload, &records)
        .map_err(|e| error::error(ErrorCode::Internal, e.to_string()))?;

    // 2. The request, then its answer.
    let mut req = WirePutReq::new(1, bytes::to_u64(upload.len()));
    if adopt == Adopt::Each {
        req.adopt = group.iter().map(|(_, id, _)| id.to_string()).collect();
    }
    let asked = request(WIRE_PUT, req)?;
    let answer = link.call(&asked, &upload)?;
    let _: WirePut = outcome(answer.line())?;
    Ok(bytes::to_u64(group.len()))
}

/// `wire_move` of `name`: from `old` (none: made) to `new`, forward.
fn move_ref(
    link: &mut dyn Link,
    name: &str,
    old: Option<ChangesetId>,
    new: ChangesetId,
) -> Result<WireMove, OpError> {
    let mut req = WireMoveReq::new(name);
    req.old = old.map(|old| old.to_string());
    req.new = Some(new.to_string());
    req.mode = WireMoveMode::Advance;
    ask(link, WIRE_MOVE, req)
}

/// A branch or a tag `source` of `repo` sent to the upstream's `target` behind `link`, and the
/// host's tracking ref of a branch moved there.
pub(crate) fn push(
    repo: &LocalRepo,
    link: &mut dyn Link,
    remote: &str,
    source: &str,
    target: &str,
) -> Result<Pushed, OpError> {
    // 1. What is sent, and where the upstream's ref is.
    let state = repo.get_ref(source).map_err(error::op)?;
    let head = state
        .map(|state| state.target)
        .ok_or_else(|| error::op(swarmops_repo::RepoError::NotFound(source.to_owned())))?;
    fetch::hello(link)?;
    let heads = refs(link)?;
    let old = heads.get(target).copied();
    let tracked = ref_kind(target) != Some(RefKind::Tag);

    // 2. Nothing to send, or what the upstream lacks, then the move.
    let mut pushed = Pushed {
        status: PushStatus::UpToDate,
        old,
        new: head,
        sent: 0,
    };
    if old != Some(head) {
        let haves = held_heads(repo, &heads)?;
        pushed.sent = upload(repo, link, head, &haves)?;
        move_ref(link, target, old, head)?;
        pushed.status = match old {
            Some(_) => PushStatus::Pushed,
            None => PushStatus::Created,
        };
    }

    // 3. The host's tracking ref of a branch follows.
    if tracked {
        track(repo, remote, target, head)?;
    }
    Ok(pushed)
}

/// The heads of `heads` the store holds.
fn held_heads(
    repo: &LocalRepo,
    heads: &BTreeMap<String, ChangesetId>,
) -> Result<Vec<ChangesetId>, OpError> {
    let mut held = Vec::new();
    for &id in heads.values() {
        if repo.changeset_meta(id).map_err(error::op)?.is_some() {
            held.push(id);
        }
    }
    Ok(held)
}

/// The tracking ref of `remote`'s `name` moved to `head` from what it names.
fn track(repo: &LocalRepo, remote: &str, name: &str, head: ChangesetId) -> Result<(), OpError> {
    let tracking = format!("{TRACKING_PREFIX}{remote}/{name}");
    let actor = format!("{ACTOR_PREFIX}{remote}");
    let old = repo.get_ref(&tracking).map_err(error::op)?;
    let old = old.map(|state| state.target);
    if old == Some(head) {
        return Ok(());
    }
    let cas = repo.cas_ref(&tracking, old, Some(head), &actor);
    match cas.map_err(error::op)?.result {
        CasResult::Applied { .. } => Ok(()),
        CasResult::Mismatch { actual } if actual == Some(head) => Ok(()),
        _ => Err(
            error::error(ErrorCode::Busy, format!("{tracking} moved during the push"))
                .with_details(error::details([("ref", tracking.as_str().into())]))
                .with_hint("run the push again"),
        ),
    }
}

/// `main` now, if it exists.
fn main_now(repo: &LocalRepo) -> Result<Option<ChangesetId>, OpError> {
    let state = repo.get_ref(MAIN).map_err(error::op)?;
    Ok(state.map(|state| state.target))
}

/// The sync engine's failure `e` as the sync's error.
fn engine_error(e: SyncError, args: &SyncArgs<'_>) -> OpError {
    error::sync_error(e, args.remote, args.branch)
}

/// `E_CAS_RETRY_EXHAUSTED`: `main` moved under every attempt.
fn exhausted(args: &SyncArgs<'_>) -> OpError {
    let attempts = args.max_attempts;
    error::error(
        ErrorCode::CasRetryExhausted,
        format!("main moved under the sync in each of its {attempts} attempts"),
    )
    .with_details(error::details([
        ("remote", args.remote.into()),
        ("branch", args.branch.into()),
        ("attempts", attempts.into()),
    ]))
    .with_hint(error::hint(format!(
        "swarmops sync {} again",
        error::shell_word(args.remote)
    )))
}

/// `E_INVALID_ARG` `land_with_sync`: a native push of `name`, `main` or `release/*`, which the
/// upstream lands rather than moves.
pub(crate) fn land_with_sync(name: &str, remote: &str) -> OpError {
    error::error(
        ErrorCode::InvalidArg,
        format!("{name} is landed on a native remote, not moved: sync it"),
    )
    .with_details(error::details([
        ("reason", LAND_WITH_SYNC.into()),
        ("ref", name.into()),
    ]))
    .with_hint(error::hint(format!(
        "swarmops sync {}",
        error::shell_word(remote)
    )))
}

impl Recording<'_> {
    /// `sync` of the native `remote` by `who`: its session over ssh, then [`sync`], answered as
    /// the git wire's sync answers (every git id the commit its changeset exports to).
    pub(crate) fn sync_native(
        &self,
        who: &Identity,
        remote: &RemoteInfo,
        mode: SyncMode,
        max_attempts: u32,
        lander: &Signature,
    ) -> Result<SyncResp, OpError> {
        let repo = &self.ops.repo;
        let name = remote.name.as_str();

        // 1. The host's format and the URL, before any byte is sent.
        check_format(repo, name)?;
        let url = NativeUrl::parse(&remote.url)?;

        // 2. The sync; a server that answers in git's pkt-lines does not speak it.
        let args = SyncArgs {
            remote: name,
            branch: &remote.branch,
            actor: who.agent(),
            lander,
            mode,
            max_attempts,
        };
        let mut link = SshLink::connect(&url)?;
        let synced = match sync(repo, &mut link, &args) {
            Ok(synced) => synced,
            Err(e) => return Err(link.git_refusal().map_or(e, |said| not_native(name, &said))),
        };
        drop(link);

        // 3. What the request moved, then the answer.
        let ended = || error::error(ErrorCode::Internal, "a sync ended with no head");
        let main_new = synced.main_new.ok_or_else(ended)?;
        let remote_new = synced.remote_new.ok_or_else(ended)?;
        for recovered in synced.recovered {
            self.resolved.record_recovered(recovered);
        }
        self.resolved.record(MAIN, main_new);
        let mut main = HeadMove::new(main_new.to_string());
        main.old = synced.main_old.map(|id| id.to_string());
        let mut remote_head = HeadMove::new(self.export_sha(remote_new)?);
        remote_head.old = synced
            .remote_old
            .map(|id| self.export_sha(id))
            .transpose()?;
        let mut published = Vec::with_capacity(synced.published.len());
        for (change, landed) in synced.published {
            let sha = self.export_sha(landed)?;
            published.push(PublishedChange::new(
                change.to_string(),
                landed.to_string(),
                sha,
            ));
        }
        let dropped = (synced.dropped.iter())
            .map(|id| DroppedChange::new(id.to_string(), DropReason::Trailer))
            .collect();
        let mut out = SyncResp::new(
            name.to_owned(),
            remote.branch.clone(),
            synced.status,
            synced.attempts,
            published,
            dropped,
            main,
            remote_head,
        );
        out.warnings = (!synced.warnings.is_empty()).then_some(synced.warnings);
        Ok(out)
    }

    /// `push` of `source` to the native `remote`'s `target`: a branch or a tag moved there by
    /// compare-and-swap; `main` and `release/*` are landed by `sync`.
    pub(crate) fn push_native(
        &self,
        remote: &RemoteInfo,
        source: &str,
        target: &str,
    ) -> Result<PushResp, OpError> {
        let repo = &self.ops.repo;
        let name = remote.name.as_str();

        // 1. What a push moves, the host's format and the URL, before any byte is sent.
        for checked in [source, target] {
            if matches!(ref_kind(checked), Some(RefKind::Main | RefKind::Release)) {
                return Err(land_with_sync(checked, name));
            }
        }
        check_format(repo, name)?;
        let url = NativeUrl::parse(&remote.url)?;

        // 2. The push; a server that answers in git's pkt-lines does not speak it.
        let mut link = SshLink::connect(&url)?;
        let pushed = match push(repo, &mut link, name, source, target) {
            Ok(pushed) => pushed,
            Err(e) => return Err(link.git_refusal().map_or(e, |said| not_native(name, &said))),
        };
        drop(link);

        // 3. The answer, its git ids the commits the changesets export to.
        let mut out = PushResp::new(
            name.to_owned(),
            target.to_owned(),
            pushed.status,
            self.export_sha(pushed.new)?,
            pushed.new.to_string(),
            pushed.sent,
            0,
        );
        out.old = pushed.old.map(|old| self.export_sha(old)).transpose()?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::io::{BufRead, BufReader, Write};
    use std::sync::Arc;
    use std::thread;

    use camino::{Utf8Path, Utf8PathBuf};
    use serde_json::json;
    use swarmops_api::wire::{WIRE_GET, WIRE_HELLO, WIRE_LIST, WIRE_REFS};
    use swarmops_api::{
        ChangeInput, ChangeOp, CommitReq, ContentInput, LandReq, RefSetReq, Rev, StoreOps as _,
        WIRE_PROTOCOL, WireChunks, WireGetReq, WireHello, WireList, WireListReq, WireRef, WireRefs,
    };
    use swarmops_core::Hashed;
    use swarmops_repo::Depth;
    use swarmops_store::{FsOps, RealFs, StoreFormat};
    use swarmops_testkit::power::{ImageRecordingFs, Seeded, Timeline};

    use super::*;
    use crate::LocalOps;
    use crate::native::chunk::Takes;
    use crate::native::key::sync_key;
    use crate::native::link::StreamLink;

    /// The agent that writes the upstream.
    const AGENT: &str = "agent/worker-1";

    /// The identity the upstream lands as: the connection's.
    const LANDER: &str = "person/ada";

    /// The agent that moves the host's `main`.
    const HOST_AGENT: &str = "agent/worker-17";

    /// The remote's name in the host.
    const HUB: &str = "hub";

    /// The power-loss sweep's seeds: 0 keeps nothing not durable, 1 everything.
    const SEEDS: [u64; 2] = [0, 1];

    /// An upstream store, written through its operations, answering the wire as `serve --stdio`
    /// does for a caller who may read, upload and land.
    struct Upstream {
        _dir: tempfile::TempDir,
        ops: LocalOps,
        who: Identity,
    }

    impl Upstream {
        /// An empty FORMAT 2 store.
        fn new() -> Upstream {
            let dir = tempfile::tempdir().unwrap();
            let root = Utf8PathBuf::from_path_buf(dir.path().join("up")).unwrap();
            let (ops, _) = LocalOps::init(&root, "up").unwrap();
            Upstream {
                _dir: dir,
                ops,
                who: Identity::parse(AGENT).unwrap(),
            }
        }

        /// A put of `path` on `branch`'s head (none: on `from`), `branch` moved or made there.
        fn advance(&self, branch: &str, path: &str, from: Option<ChangesetId>) -> ChangesetId {
            let old = self.ops.repo.get_ref(branch).unwrap();
            let old = old.map(|state| state.target);
            let mut change = ChangeInput::new(path.to_owned(), ChangeOp::Put);
            change.content = Some(ContentInput::Inline(format!("{path}\n")));
            let mut req = CommitReq::new(vec![change], path.to_owned());
            req.parent = old.or(from).map(|id| Rev::from(id.to_string()));
            let made = self.ops.commit(&self.who, req).unwrap().changeset;
            let mut set = RefSetReq::new(branch, Rev::from(made.clone()));
            match old {
                Some(old) => set.expected_old = Some(Rev::from(old.to_string())),
                None => set.create = true,
            }
            self.ops.ref_set(&self.who, set).unwrap();
            ChangesetId::new(made.parse().unwrap())
        }

        /// The upstream's `name`.
        fn head(&self, name: &str) -> ChangesetId {
            self.ops.repo.get_ref(name).unwrap().unwrap().target
        }

        /// Answers native sync's requests on pipes until the host closes them; `host` runs on
        /// the host's [`StreamLink`] meanwhile.
        fn serve<T>(&self, host: impl FnOnce(&mut StreamLink) -> T) -> T {
            let (request_in, request_out) = std::io::pipe().unwrap();
            let (answer_in, answer_out) = std::io::pipe().unwrap();
            thread::scope(|scope| {
                scope.spawn(|| self.answer(BufReader::new(request_in), answer_out));
                let mut link = StreamLink::new(answer_in, request_out);
                host(&mut link)
            })
        }

        /// Each request line of `input`, and a put's chunks, answered on `output`.
        fn answer(&self, mut input: impl BufRead, mut output: impl Write) {
            loop {
                let mut line = String::new();
                if input.read_line(&mut line).unwrap() == 0 {
                    return;
                }
                let request: Value = serde_json::from_str(&line).unwrap();
                let op = request["op"].as_str().unwrap().to_owned();
                let (result, chunk) = self.handle(&op, request["req"].clone(), &mut input);
                let envelope = json!({"ok": true, "op": op, "api": 1, "result": result});
                let mut line = serde_json::to_vec(&envelope).unwrap();
                line.push(b'\n');
                output.write_all(&line).unwrap();
                output.write_all(&chunk).unwrap();
                output.flush().unwrap();
            }
        }

        /// The result of `op` with `req`, and the chunk bytes after its line.
        fn handle(&self, op: &str, req: Value, input: &mut impl BufRead) -> (Value, Vec<u8>) {
            let repo = &self.ops.repo;
            let id = |text: &str| -> ObjectId { text.parse().unwrap() };
            let value = |v: Value| (v, Vec::new());
            match op {
                WIRE_HELLO => {
                    let kinds = vec!["blob".to_owned(), "changeset".to_owned()];
                    let hello = WireHello::new(WIRE_PROTOCOL, LANDER, 2, kinds, vec![], "test");
                    value(serde_json::to_value(hello).unwrap())
                }
                WIRE_REFS => {
                    let refs = repo.list_refs().unwrap().into_iter();
                    let refs =
                        refs.map(|(name, state)| WireRef::new(name, state.target.to_string()));
                    value(serde_json::to_value(WireRefs::new(refs.collect(), true)).unwrap())
                }
                WIRE_LIST => {
                    let req: WireListReq = serde_json::from_value(req).unwrap();
                    let have: Vec<ChangesetId> =
                        req.have.iter().map(|h| ChangesetId::new(id(h))).collect();
                    let want = ChangesetId::new(id(&req.want));
                    let read = repo.snapshot().unwrap();
                    let missing = read.missing(&have, want, req.limit, None).unwrap();
                    let ids = missing.changesets.iter().map(ToString::to_string).collect();
                    value(serde_json::to_value(WireList::new(ids)).unwrap())
                }
                WIRE_GET => {
                    let req: WireGetReq = serde_json::from_value(req).unwrap();
                    let blobs = req.blobs.iter().map(|want| want.id.clone());
                    let objects: Vec<(ObjectKind, ObjectId, Vec<u8>)> =
                        (req.changesets.iter().cloned().chain(blobs))
                            .map(|text| {
                                let (kind, payload) = repo.object(id(&text)).unwrap().unwrap();
                                (kind, id(&text), payload)
                            })
                            .collect();
                    let records: Vec<(ObjectKind, ObjectId, &[u8])> = (objects.iter())
                        .map(|(k, i, p)| (*k, *i, p.as_slice()))
                        .collect();
                    let mut bytes = Vec::new();
                    chunk::write_chunk(&mut bytes, &records).unwrap();
                    (json!({"chunks": 1, "bytes": bytes.len()}), bytes)
                }
                WIRE_HAS => {
                    let req: WireHasReq = serde_json::from_value(req).unwrap();
                    let held = (req.ids.iter())
                        .map(|text| {
                            let at = id(text);
                            let adopted = repo.changeset_meta(ChangesetId::new(at)).unwrap();
                            let blob =
                                matches!(repo.object(at).unwrap(), Some((ObjectKind::Blob, _)));
                            adopted.is_some() || blob
                        })
                        .collect();
                    value(serde_json::to_value(WireHas::new(held)).unwrap())
                }
                WIRE_PUT => {
                    let req: WirePutReq = serde_json::from_value(req).unwrap();
                    let mut left = WireChunks::new(req.chunks, req.bytes);
                    let mut changesets = std::collections::HashMap::new();
                    while left.chunks > 0 {
                        let records = chunk::read_chunk(input, left, Takes::BlobsAndChangesets);
                        let records = records.unwrap();
                        let len = chunk::chunk_len(&records);
                        left = WireChunks::new(left.chunks - 1, left.bytes - len);
                        let (blobs, others): (Vec<Hashed>, Vec<Hashed>) = records
                            .into_iter()
                            .partition(|r| r.kind() == ObjectKind::Blob);
                        repo.put_blobs(blobs).unwrap();
                        changesets.extend(others.into_iter().map(|c| (c.id(), c)));
                    }
                    let adopting = (req.adopt.iter())
                        .filter_map(|text| changesets.remove(&id(text)))
                        .collect();
                    let adopted = repo.adopt(adopting).unwrap();
                    let ids = |ids: &[ChangesetId]| ids.iter().map(ToString::to_string).collect();
                    let put = WirePut::new(ids(&adopted.adopted), ids(&adopted.present));
                    value(serde_json::to_value(put).unwrap())
                }
                WIRE_LAND => {
                    let req: WireLandReq = serde_json::from_value(req).unwrap();
                    let head = ChangesetId::new(id(&req.head));
                    let mut land = LandReq::new(Rev::from(req.head.clone()));
                    land.base = Some(Rev::from(req.base.clone()));
                    land.target.clone_from(&req.target);
                    land.idempotency_key = Some(sync_key(&req.target, head));
                    let ada = Identity::parse(LANDER).unwrap();
                    let resp = self.ops.land(&ada, land).unwrap();
                    value(serde_json::to_value(resp).unwrap())
                }
                _ => panic!("no answer to {op}"),
            }
        }
    }

    /// A sync's run: the host's store recorded on an [`ImageRecordingFs`] from its making, its
    /// `main` at two changes of its own the upstream does not have on `main`, the upstream's
    /// `main` moved on, then a sync between the marks `sync` and `synced`.
    struct Recorded {
        _dir: tempfile::TempDir,
        timeline: Timeline,
        /// The host's `main` before the sync, and the upstream's head it moved to.
        main: ChangesetId,
        head: ChangesetId,
    }

    /// Records [`Recorded`]'s run.
    fn record() -> Recorded {
        // 1. The upstream: main, and two changes on a branch off it.
        let up = Upstream::new();
        up.advance("main", "a.txt", None);
        let base = up.advance("main", "b.txt", None);
        up.advance("work/w", "c1.txt", Some(base));
        let main = up.advance("work/w", "c2.txt", None);

        // 2. The host, recorded from its making: fetched, main at the branch's head.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store = Utf8PathBuf::from_path_buf(root.join("host")).unwrap();
        let fs = Arc::new(ImageRecordingFs::new());
        let shared: Arc<dyn FsOps> = Arc::clone(&fs) as Arc<dyn FsOps>;
        let host = LocalRepo::create_with_fs_as(&store, "host", StoreFormat::V2, shared).unwrap();
        up.serve(|link| fetch::fetch(&host, link, HUB)).unwrap();
        host.cas_ref(MAIN, None, Some(main), HOST_AGENT).unwrap();

        // 3. The upstream's main moves on; the sync, marked.
        up.advance("main", "x.txt", None);
        let lander = Signature::new(b"host".to_vec(), b"h@x".to_vec(), 0, 0, false).unwrap();
        let args = SyncArgs {
            remote: HUB,
            branch: MAIN,
            actor: HOST_AGENT,
            lander: &lander,
            mode: SyncMode::Publish,
            max_attempts: 3,
        };
        fs.mark("sync");
        let synced = up.serve(|link| sync(&host, link, &args)).unwrap();
        fs.mark("synced");
        assert_eq!(synced.status, SyncStatusKind::Pushed, "{synced:?}");
        assert_eq!(synced.published.len(), 2, "{synced:?}");
        let head = up.head(MAIN);
        assert_eq!(synced.main_new, Some(head));
        drop(host);
        Recorded {
            _dir: dir,
            timeline: Timeline::new(&root, &fs.calls()),
            main,
            head,
        }
    }

    /// The image check of the host store at `root`, the sync having returned when `acked`: it
    /// opens and verifies deep clean, and `main` names the host's own head or the upstream's,
    /// the upstream's once the sync returned.
    fn check_host(root: &Utf8Path, rec: &Recorded, acked: bool) -> Result<(), String> {
        let host = LocalRepo::open_with_fs(root, Arc::new(RealFs::new()))
            .map_err(|e| format!("open: {e}"))?;
        let report = host
            .verify(Depth::Deep)
            .map_err(|e| format!("verify: {e}"))?;
        if !report.problems.is_empty() {
            return Err(format!("verify --deep: {:?}", report.problems));
        }
        let main = host.get_ref(MAIN).map_err(|e| format!("main: {e}"))?;
        let main = main.map(|state| state.target);
        let whole = main == Some(rec.head) || (!acked && main == Some(rec.main));
        if !whole {
            return Err(format!(
                "main names {main:?}, not {} or {}",
                rec.main, rec.head
            ));
        }
        Ok(())
    }

    /// A power loss at any step of a sync, the disk keeping nothing not durable (seed 0) or
    /// everything (seed 1), leaves a host that opens, verifies clean, and whose `main` names
    /// what it named before the sync or the upstream's head; the head once the sync returned.
    #[test]
    fn sync_power_loss_images_keep_main_at_m_or_the_upstream_head() {
        let rec = record();
        let synced_at = rec.timeline.mark("synced");
        let mut seen = HashSet::new();
        for cut in rec.timeline.mark("sync")..=rec.timeline.steps() {
            for seed in SEEDS {
                let image = rec.timeline.image(cut, &mut Seeded::new(seed, cut));
                if !seen.insert(image.digest()) {
                    continue;
                }
                let dir = tempfile::tempdir().unwrap();
                image.materialize(dir.path()).unwrap();
                let root = Utf8PathBuf::from_path_buf(dir.path().join("host")).unwrap();
                let checked = check_host(&root, &rec, cut >= synced_at);
                checked.unwrap_or_else(|why| {
                    panic!("cut {cut} seed {seed}: {why}\n{}", image.describe())
                });
            }
        }
        assert!(seen.len() > 10, "{} images", seen.len());
    }

    /// A change landed by the upstream under its key is recognised on the upstream's first
    /// parents and not landed again; one landed under another target's key is not.
    #[test]
    fn a_landed_key_is_recognised_on_the_target_only() {
        let up = Upstream::new();
        let base = up.advance("main", "a.txt", None);
        let change = up.advance("work/w", "w.txt", Some(base));
        let mut land = LandReq::new(Rev::from(change.to_string()));
        land.base = Some(Rev::from(base.to_string()));
        land.idempotency_key = Some(sync_key(MAIN, change));
        up.ops.land(&up.who, land).unwrap();

        let keys = landed_keys(&up.ops.repo, up.head(MAIN), base).unwrap();
        assert!(keys.contains(&sync_key(MAIN, change)), "{keys:?}");
        assert!(!keys.contains(&sync_key("release/1", change)), "{keys:?}");
    }
}
