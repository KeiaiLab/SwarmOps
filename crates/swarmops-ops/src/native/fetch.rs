//! A host's native fetch (the native sync design §2.4, F1–F6): the upstream's refs, the
//! changesets they reach that the host lacks and the blobs those name, adopted with their ids;
//! then the tracking refs `remote/<remote>/<ref>` moved, and the upstream's tags made host tags.
//!
//! ```text
//!   F1 hello; refs, every page
//!   F2 per ref: list want=its head, have=the host's tracking heads   (every page)
//!      per batch of what the host lacks, parents first:
//!   F3   changesets: read back if the store holds their object, else get ─▶ stored at once
//!   F4   blobs their deltas name that the store lacks ─▶ get ─▶ stored a chunk at a time
//!   F5   adopt (V1–V5), no ref moved
//!   F6 tracking refs checked (main and release/* forward only, V6), then each moved by
//!      compare-and-swap; tags: created when absent, kept and reported when they differ
//! ```
//!
//! A fetch cut anywhere and run again asks only for what the store lacks: an object stored
//! before the cut is read back, never asked for again; changesets adopted before it are held.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use swarmops_api::remote::MAX_FETCH_BRANCHES;
use swarmops_api::wire::{WIRE_GET, WIRE_HELLO, WIRE_LIST, WIRE_REFS};
use swarmops_api::{
    BranchUpdate, ErrorCode, FetchResp, Identity, MAX_WIRE_IDS, OpError, RemoteHead, RemoteInfo,
    WIRE_PROTOCOL, WireBlobWant, WireCursor, WireGetReq, WireHello, WireHelloReq, WireList,
    WireListReq, WireRefs, WireRefsReq, WireRequest,
};
use swarmops_core::{BlobId, Changeset, ChangesetId, DeltaOp, Hashed, ObjectKind};
use swarmops_git::ExportOid;
use swarmops_repo::{CasResult, LocalRepo, MoveMode, RepoError};
use swarmops_store::{RefKind, StoreError, StoreFormat, ref_kind};

use super::link::Link;
use super::refused;
use super::ssh::SshLink;
use super::url::NativeUrl;
use crate::error;
use crate::local::Recording;

/// The object kinds a host takes, by name.
const OBJECT_KINDS: [&str; 2] = ["blob", "changeset"];

/// The prefix of a tracking ref: `remote/<remote>/<ref>`.
const TRACKING_PREFIX: &str = "remote/";

/// The prefix of a store's tag.
const TAG_PREFIX: &str = "tags/";

/// The prefix of the actor a fetch's ref moves are recorded under: `remote/<remote>`.
const ACTOR_PREFIX: &str = "remote/";

/// The store ref a native clone makes.
const MAIN: &str = "main";

/// What a server that does not speak native sync is reached by instead.
const GIT_WIRE_HINT: &str = "this server takes the git wire: use its ssh:// address";

/// The warning of an upstream tag kept as the host's own: `tag_differs: tags/<path>`.
const TAG_DIFFERS: &str = "tag_differs";

/// What a host tag became of the upstream's tag of the same name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TagOutcome {
    /// It was absent, and now names the upstream's changeset.
    Created,
    /// It named that changeset already.
    Same,
    /// It names another changeset, and is kept: tags never move.
    Differs,
}

/// What a fetch did.
#[derive(Debug, Default)]
pub(crate) struct Fetched {
    /// The upstream's refs, as the fetch read them.
    pub(crate) heads: BTreeMap<String, ChangesetId>,
    /// What the tracking refs named before, by the upstream's ref name.
    pub(crate) before: BTreeMap<String, ChangesetId>,
    /// The tracking refs moved, by the upstream's ref name: old, new.
    pub(crate) moved: Vec<(String, Option<ChangesetId>, ChangesetId)>,
    /// The changesets adopted.
    pub(crate) adopted: u64,
    /// The host tags made.
    pub(crate) tags_created: u64,
    /// What the fetch has to say that did not stop it.
    pub(crate) warnings: Vec<String>,
}

/// F1–F6 of [the module documentation](self): `remote`'s refs fetched over `link` into `repo`.
pub(crate) fn fetch(
    repo: &LocalRepo,
    link: &mut dyn Link,
    remote: &str,
) -> Result<Fetched, OpError> {
    // 1. The session, then every ref the upstream lists that a host fetches.
    hello(link)?;
    let heads = refs(link)?;
    let before = tracking(repo, remote)?;

    // 2. Per ref, what the host lacks, a batch at a time, parents first.
    let haves: Vec<String> = (before.values().collect::<HashSet<_>>().into_iter())
        .take(MAX_WIRE_IDS)
        .map(ToString::to_string)
        .collect();
    let mut adopted = 0;
    let mut done = HashSet::new();
    for (name, &head) in &heads {
        if held(repo, head)? {
            continue;
        }
        let mut lacking = Vec::new();
        for id in list(link, head, &haves)? {
            if !done.contains(&id) && !held(repo, id)? {
                lacking.push(id);
            }
        }
        lacking.reverse();
        for batch in lacking.chunks(MAX_WIRE_IDS) {
            adopted += transfer(repo, link, name, batch)?;
            done.extend(batch.iter().copied());
        }
    }

    // 3. The refs.
    let mut fetched = Fetched {
        heads,
        before,
        adopted,
        ..Fetched::default()
    };
    move_refs(repo, remote, &mut fetched)?;
    Ok(fetched)
}

/// Host tag `name` for the upstream's `target`: created when absent, kept when it names
/// `target`, kept and reported when it names another changeset; never moved.
pub(crate) fn keep_tag(
    repo: &LocalRepo,
    name: &str,
    target: ChangesetId,
    actor: &str,
) -> Result<TagOutcome, OpError> {
    let compared = |at: ChangesetId| {
        if at == target {
            TagOutcome::Same
        } else {
            TagOutcome::Differs
        }
    };

    // 1. A tag there is compared, never moved.
    if let Some(state) = repo.get_ref(name).map_err(error::op)? {
        return Ok(compared(state.target));
    }

    // 2. Created; one made meanwhile is compared.
    let made = repo.cas_ref(name, None, Some(target), actor);
    match made.map_err(error::op)?.result {
        CasResult::Applied { .. } => Ok(TagOutcome::Created),
        CasResult::Mismatch {
            actual: Some(actual),
        } => Ok(compared(actual)),
        _ => Err(moved_meanwhile(name)),
    }
}

/// `wire_hello`: the upstream takes protocol [`WIRE_PROTOCOL`].
pub(super) fn hello(link: &mut dyn Link) -> Result<WireHello, OpError> {
    let kinds = OBJECT_KINDS.map(str::to_owned).to_vec();
    let req = WireHelloReq::new(WIRE_PROTOCOL, WIRE_PROTOCOL, kinds, Vec::new());
    let hello: WireHello = ask(link, WIRE_HELLO, req)?;
    if hello.protocol != WIRE_PROTOCOL {
        let message = format!("the upstream chose protocol {}", hello.protocol);
        return Err(refused("protocol", message));
    }
    Ok(hello)
}

/// Every page of `wire_refs`: the upstream's refs a host fetches (`main`, `release/*`,
/// branches, tags), by name.
pub(super) fn refs(link: &mut dyn Link) -> Result<BTreeMap<String, ChangesetId>, OpError> {
    let mut heads = BTreeMap::new();
    let mut after = None;
    loop {
        // 1. One page.
        let mut req = WireRefsReq::new();
        req.after.clone_from(&after);
        let page: WireRefs = ask(link, WIRE_REFS, req)?;
        for listed in page.refs {
            let fetched = matches!(
                ref_kind(&listed.name),
                Some(RefKind::Main | RefKind::Release | RefKind::Branch | RefKind::Tag)
            );
            if fetched {
                heads.insert(listed.name, changeset_id(&listed.changeset)?);
            }
        }

        // 2. The next, which must come after this one.
        match page.next {
            Some(next) if after.as_ref().is_none_or(|after| next > *after) => after = Some(next),
            Some(_) => return Err(refused("answer_line", "a refs page that does not go on")),
            None => return Ok(heads),
        }
    }
}

/// The host's tracking refs of `remote`, by the upstream's ref name.
fn tracking(repo: &LocalRepo, remote: &str) -> Result<BTreeMap<String, ChangesetId>, OpError> {
    let prefix = format!("{TRACKING_PREFIX}{remote}/");
    let refs = repo.list_refs().map_err(error::op)?;
    let tracked = (refs.into_iter())
        .filter_map(|(name, state)| Some((name.strip_prefix(&prefix)?.to_owned(), state.target)));
    Ok(tracked.collect())
}

/// Every page of `wire_list` for `head`: what it reaches that no `have` does, highest
/// generation first.
fn list(
    link: &mut dyn Link,
    head: ChangesetId,
    haves: &[String],
) -> Result<Vec<ChangesetId>, OpError> {
    let mut ids = Vec::new();
    let mut after: Option<WireCursor> = None;
    loop {
        let mut req = WireListReq::new(head.to_string());
        req.have = haves.to_vec();
        req.after.clone_from(&after);
        let page: WireList = ask(link, WIRE_LIST, req)?;
        for id in &page.changesets {
            ids.push(changeset_id(id)?);
        }
        match page.next {
            Some(next) if !page.changesets.is_empty() => after = Some(next),
            Some(_) => return Err(refused("answer_line", "a list page that does not go on")),
            None => return Ok(ids),
        }
    }
}

/// F3–F5 for `batch`, changesets under the upstream's ref `under`, parents first: their
/// objects and the blobs they name stored, then adopted. How many it adopted.
fn transfer(
    repo: &LocalRepo,
    link: &mut dyn Link,
    under: &str,
    batch: &[ChangesetId],
) -> Result<u64, OpError> {
    // 1. Each changeset: read back if stored before (a cut fetch's), else asked for and stored.
    let mut objects: HashMap<ChangesetId, Hashed> = HashMap::new();
    let mut asked = WireGetReq::new(under);
    for &id in batch {
        match stored_changeset(repo, id)? {
            Some(object) => {
                objects.insert(id, object);
            }
            None => asked.changesets.push(id.to_string()),
        }
    }
    if !asked.changesets.is_empty() {
        get(link, asked, |chunk| {
            store_changesets(repo, chunk, &mut objects)
        })?;
    }

    // 2. The blobs their deltas name that the store lacks, each asked for with its changeset.
    let mut wants = Vec::new();
    let mut seen = HashSet::new();
    for id in batch {
        let Some(object) = objects.get(id) else {
            return Err(not_sent(*id));
        };
        for blob in puts(object)? {
            if seen.insert(blob) && !repo.contains_object(blob.0).map_err(error::op)? {
                wants.push(WireBlobWant::new(id.to_string(), blob.0.to_string()));
            }
        }
    }
    for group in wants.chunks(MAX_WIRE_IDS) {
        let mut asked = WireGetReq::new(under);
        asked.blobs = group.to_vec();
        get(link, asked, |chunk| store_blobs(repo, chunk))?;
    }

    // 3. The adopt, parents first: the order the batch is in.
    let mut ordered = Vec::with_capacity(batch.len());
    for id in batch {
        ordered.push(objects.remove(id).ok_or_else(|| not_sent(*id))?);
    }
    let adopted = repo.adopt(ordered).map_err(error::op)?;
    Ok(swarmops_core::bytes::to_u64(adopted.adopted.len()))
}

/// F6: every tracking ref checked against its rule, then each moved; then the tags.
fn move_refs(repo: &LocalRepo, remote: &str, fetched: &mut Fetched) -> Result<(), OpError> {
    let actor = format!("{ACTOR_PREFIX}{remote}");

    // 1. The moves, each checked before any is made (V6): main and release/* forward only.
    let mut moves = Vec::new();
    {
        let read = repo.snapshot().map_err(error::op)?;
        for (name, &head) in &fetched.heads {
            if ref_kind(name) == Some(RefKind::Tag) {
                continue;
            }
            let old = fetched.before.get(name).copied();
            if old == Some(head) {
                continue;
            }
            let tracking = format!("{TRACKING_PREFIX}{remote}/{name}");
            let judged = read.check_ref_move(&tracking, old, Some(head), MoveMode::Advance);
            judged.map_err(error::op)?;
            moves.push((name.clone(), tracking, old, head));
        }
    }

    // 2. Each by compare-and-swap: one moved meanwhile stops the fetch.
    for (name, tracking, old, head) in moves {
        let cas = repo.cas_ref(&tracking, old, Some(head), &actor);
        if let CasResult::Mismatch { .. } = cas.map_err(error::op)?.result {
            return Err(moved_meanwhile(&tracking));
        }
        fetched.moved.push((name, old, head));
    }

    // 3. The tags, as host tags of the same name.
    let tags = (fetched.heads.iter()).filter(|(name, _)| name.starts_with(TAG_PREFIX));
    for (name, &head) in tags {
        match keep_tag(repo, name, head, &actor)? {
            TagOutcome::Created => fetched.tags_created += 1,
            TagOutcome::Same => {}
            TagOutcome::Differs => fetched.warnings.push(format!("{TAG_DIFFERS}: {name}")),
        }
    }
    Ok(())
}

/// Asks `op` with `req` and reads its result; the upstream's refusal is returned as it is.
pub(super) fn ask<Q: Serialize, R: DeserializeOwned>(
    link: &mut dyn Link,
    op: &str,
    req: Q,
) -> Result<R, OpError> {
    let asked = request(op, req)?;
    let answer = link.call(&asked, &[])?;
    outcome(answer.line())
}

/// Asks `wire_get` with `req` and hands each chunk's objects to `each`, as they come.
fn get(
    link: &mut dyn Link,
    req: WireGetReq,
    mut each: impl FnMut(Vec<Hashed>) -> Result<(), OpError>,
) -> Result<(), OpError> {
    let asked = request(WIRE_GET, req)?;
    let mut answer = link.call(&asked, &[])?;
    let _: Value = outcome(answer.line())?;
    while let Some(chunk) = answer.next_chunk()? {
        each(chunk)?;
    }
    Ok(())
}

/// The request line of `op` with `req`.
pub(super) fn request<Q: Serialize>(op: &str, req: Q) -> Result<Vec<u8>, OpError> {
    let request = WireRequest::new(op, WIRE_PROTOCOL, req);
    serde_json::to_vec(&request).map_err(|e| {
        error::error(
            ErrorCode::Internal,
            format!("a {op} request cannot be written: {e}"),
        )
    })
}

/// The result an answer `line` carries, or the refusal it carries.
pub(super) fn outcome<R: DeserializeOwned>(line: &[u8]) -> Result<R, OpError> {
    let unreadable = || {
        refused(
            "answer_line",
            "an answer that is no envelope this host reads",
        )
    };
    let Ok(Value::Object(mut envelope)) = serde_json::from_slice::<Value>(line) else {
        return Err(unreadable());
    };
    if envelope.get("ok") == Some(&Value::Bool(true)) {
        let result = envelope.remove("result").unwrap_or(Value::Null);
        return serde_json::from_value(result).map_err(|_| unreadable());
    }
    let error = envelope.remove("error").unwrap_or(Value::Null);
    Err(serde_json::from_value::<OpError>(error).map_err(|_| unreadable())?)
}

/// Changeset `id`'s object as the store holds it unadopted (a cut fetch stored it), if it does.
fn stored_changeset(repo: &LocalRepo, id: ChangesetId) -> Result<Option<Hashed>, OpError> {
    if !repo.contains_object(id.0).map_err(error::op)? {
        return Ok(None);
    }
    match repo.object(id.0).map_err(error::op)? {
        Some((ObjectKind::Changeset, payload)) => {
            Ok(Some(Hashed::new(ObjectKind::Changeset, payload)))
        }
        _ => Ok(None),
    }
}

/// A chunk of changesets stored as objects at once, so that a cut fetch asks for none again,
/// and kept in `objects` for the adopt.
fn store_changesets(
    repo: &LocalRepo,
    chunk: Vec<Hashed>,
    objects: &mut HashMap<ChangesetId, Hashed>,
) -> Result<(), OpError> {
    let mut written = Vec::with_capacity(chunk.len());
    for object in &chunk {
        if object.kind() != ObjectKind::Changeset {
            return Err(refused(
                "object_kind",
                "a blob among the changesets asked for",
            ));
        }
        written.push((ObjectKind::Changeset, object.payload().to_vec()));
    }
    repo.put_objects(written).map_err(error::op)?;
    for object in chunk {
        objects.insert(ChangesetId::new(object.id()), object);
    }
    Ok(())
}

/// A chunk of blobs stored and flushed at once.
fn store_blobs(repo: &LocalRepo, chunk: Vec<Hashed>) -> Result<(), OpError> {
    if chunk.iter().any(|object| object.kind() != ObjectKind::Blob) {
        return Err(refused(
            "object_kind",
            "a changeset among the blobs asked for",
        ));
    }
    repo.put_blobs(chunk).map_err(error::op)?;
    Ok(())
}

/// The blobs changeset `object`'s delta puts.
pub(super) fn puts(object: &Hashed) -> Result<Vec<BlobId>, OpError> {
    let changeset = Changeset::decode(object.payload()).map_err(|_| {
        let corrupt = StoreError::Corrupt { id: object.id() };
        error::op(RepoError::from(corrupt))
    })?;
    let puts = (changeset.delta.iter()).filter_map(|entry| match entry.op {
        DeltaOp::Put { id, .. } => Some(id),
        _ => None,
    });
    Ok(puts.collect())
}

/// Whether the store holds changeset `id` adopted.
fn held(repo: &LocalRepo, id: ChangesetId) -> Result<bool, OpError> {
    Ok(repo.changeset_meta(id).map_err(error::op)?.is_some())
}

/// A changeset id as the wire writes it.
pub(super) fn changeset_id(text: &str) -> Result<ChangesetId, OpError> {
    let id = text
        .parse()
        .map_err(|_| refused("answer_line", format!("{text:?} is no changeset id")))?;
    Ok(ChangesetId::new(id))
}

/// `E_INVALID_ARG` `not_sent`: the upstream did not send changeset `id`, asked for.
fn not_sent(id: ChangesetId) -> OpError {
    refused("not_sent", format!("the upstream did not send {id}"))
}

/// `E_BUSY`: ref `name` moved while the fetch moved it.
fn moved_meanwhile(name: &str) -> OpError {
    error::error(ErrorCode::Busy, format!("{name} moved during the fetch"))
        .with_details(error::details([("ref", name.into())]))
        .with_hint("run the fetch again")
}

/// `E_FORMAT_REQUIRED`: a store of FORMAT 1 has no tracking refs, branches or tags.
pub(crate) fn check_format(repo: &LocalRepo, remote: &str) -> Result<(), OpError> {
    let format = repo.format();
    if format != StoreFormat::V1 {
        return Ok(());
    }
    let needs = StoreError::FormatRequired {
        name: format!("{TRACKING_PREFIX}{remote}"),
        needs: StoreFormat::V2.version(),
        store: format.version(),
    };
    Err(error::op(RepoError::from(needs)))
}

impl Recording<'_> {
    /// `fetch` of the native `remote`: its session over ssh, then [`fetch`], answered as the
    /// git wire's fetch answers (every git id the commit its changeset exports to).
    pub(crate) fn fetch_native(&self, remote: &RemoteInfo) -> Result<FetchResp, OpError> {
        let repo = &self.ops.repo;
        let name = remote.name.as_str();

        // 1. The host's format and the URL, before any byte is sent.
        check_format(repo, name)?;
        let url = NativeUrl::parse(&remote.url)?;

        // 2. The fetch; a server that answers in git's pkt-lines does not speak it.
        let mut link = SshLink::connect(&url)?;
        let fetched = match fetch(repo, &mut link, name) {
            Ok(fetched) => fetched,
            Err(e) => return Err(link.git_refusal().map_or(e, |said| not_native(name, &said))),
        };
        drop(link);

        // 3. The answer.
        let head = fetched.heads.get(&remote.branch).copied();
        let old = fetched.before.get(&remote.branch).copied();
        let moved = (fetched.moved.iter()).filter(|(name, ..)| !name.starts_with(TAG_PREFIX));
        let mut branches = Vec::new();
        for (branch, old, new) in moved {
            let mut update = BranchUpdate::new(branch.clone());
            update.old = old.map(|old| self.export_sha(old)).transpose()?;
            update.new = Some(self.export_sha(*new)?);
            branches.push(update);
        }
        let truncated = branches.len() > MAX_FETCH_BRANCHES;
        branches.truncate(MAX_FETCH_BRANCHES);
        let mut out = FetchResp::new(
            name.to_owned(),
            remote.branch.clone(),
            fetched.adopted,
            branches,
            truncated,
            fetched.tags_created,
        );
        out.head = head.map(|head| self.remote_head(head)).transpose()?;
        out.old_head = old.map(|old| self.remote_head(old)).transpose()?;
        out.warnings = (!fetched.warnings.is_empty()).then_some(fetched.warnings);
        Ok(out)
    }

    /// A native clone's `main`: made at `head`, the fetched branch's, by `who`; kept when a
    /// resumed clone made it there before.
    pub(crate) fn clone_main(&self, head: ChangesetId, who: &Identity) -> Result<(), OpError> {
        let made = self.ops.repo.cas_ref(MAIN, None, Some(head), who.agent());
        match made.map_err(error::op)?.result {
            CasResult::Applied { .. } => {}
            CasResult::Mismatch { actual } if actual == Some(head) => {}
            _ => return Err(moved_meanwhile(MAIN)),
        }
        self.resolved.record(MAIN, head);
        Ok(())
    }

    /// The commit changeset `id` exports to, as a git id.
    pub(super) fn export_sha(&self, id: ChangesetId) -> Result<String, OpError> {
        let exported = self.ops.repo.derived::<ExportOid>(id, &());
        Ok(exported.map_err(error::op)?.git_oid().to_string())
    }

    /// Changeset `id` as a fetch reports a head.
    fn remote_head(&self, id: ChangesetId) -> Result<RemoteHead, OpError> {
        Ok(RemoteHead::new(self.export_sha(id)?, id.to_string()))
    }
}

/// `E_REMOTE_REJECTED` `not_native`: `remote`'s server answered in git's pkt-lines, `said`.
pub(super) fn not_native(remote: &str, said: &str) -> OpError {
    let message = format!("{remote} does not speak native sync: it said {said:?}");
    error::error(ErrorCode::RemoteRejected, message)
        .with_details(error::details([
            ("reason", "not_native".into()),
            ("remote", remote.into()),
        ]))
        .with_hint(GIT_WIRE_HINT)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::io::BufReader;
    use std::sync::Arc;
    use std::thread;

    use camino::{Utf8Path, Utf8PathBuf};
    use serde_json::json;
    use swarmops_api::{
        ChangeInput, ChangeOp, CommitReq, ContentInput, RefSetReq, Rev, StoreOps as _,
        WireBlobWant, WireGetReq, WireListReq, WireRef,
    };
    use swarmops_core::ObjectId;
    use swarmops_repo::{Depth, LocalRepo};
    use swarmops_store::{FsOps, RealFs, StoreFormat};
    use swarmops_testkit::power::{ImageRecordingFs, Seeded, Timeline};

    use super::*;
    use crate::LocalOps;
    use crate::native::chunk;
    use crate::native::link::StreamLink;

    /// The agent that writes the upstream.
    const AGENT: &str = "agent/worker-1";

    /// The remote's name in the host.
    const HUB: &str = "hub";

    /// The power-loss sweep's seeds: 0 keeps nothing not durable, 1 everything.
    const SEEDS: [u64; 2] = [0, 1];

    /// An upstream store, written through its operations.
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
            let who = Identity::parse(AGENT).unwrap();
            Upstream {
                _dir: dir,
                ops,
                who,
            }
        }

        /// A put of `path` on `branch`'s head (none: a root), `branch` moved or made there.
        fn advance(&self, branch: &str, path: &str) -> ChangesetId {
            let old = self
                .ops
                .repo
                .get_ref(branch)
                .unwrap()
                .map(|state| state.target);
            let mut change = ChangeInput::new(path.to_owned(), ChangeOp::Put);
            change.content = Some(ContentInput::Inline(format!("{path}\n")));
            let mut req = CommitReq::new(vec![change], path.to_owned());
            req.parent = old.map(|old| Rev::from(old.to_string()));
            let made = self.ops.commit(&self.who, req).unwrap().changeset;
            self.set(branch, &made, old);
            ChangesetId::new(made.parse().unwrap())
        }

        /// Ref `name` moved from `old` (none: made) to `new`.
        fn set(&self, name: &str, new: &str, old: Option<ChangesetId>) {
            let mut set = RefSetReq::new(name, Rev::from(new.to_owned()));
            match old {
                Some(old) => set.expected_old = Some(Rev::from(old.to_string())),
                None => set.create = true,
            }
            self.ops.ref_set(&self.who, set).unwrap();
        }

        /// The upstream's refs.
        fn refs(&self) -> BTreeMap<String, ChangesetId> {
            let refs = self.ops.repo.list_refs().unwrap().into_iter();
            refs.map(|(name, state)| (name, state.target)).collect()
        }

        /// Answers native sync's requests on pipes until the host closes them, as `serve
        /// --stdio` answers a fetch; `fetch` runs on the host's [`StreamLink`] meanwhile.
        fn serve<T>(&self, fetch: impl FnOnce(&mut StreamLink) -> T) -> T {
            let (request_in, request_out) = std::io::pipe().unwrap();
            let (answer_in, answer_out) = std::io::pipe().unwrap();
            thread::scope(|scope| {
                scope.spawn(|| self.answer(BufReader::new(request_in), answer_out));
                let mut link = StreamLink::new(answer_in, request_out);
                fetch(&mut link)
            })
        }

        /// Each request line of `input` answered on `output`.
        fn answer(&self, input: impl std::io::BufRead, mut output: impl std::io::Write) {
            for line in input.lines() {
                let request: Value = serde_json::from_str(&line.unwrap()).unwrap();
                let op = request["op"].as_str().unwrap().to_owned();
                let req = request["req"].clone();
                let (result, chunk) = self.handle(&op, req);
                let envelope = json!({"ok": true, "op": op, "api": 1, "result": result});
                let mut line = serde_json::to_vec(&envelope).unwrap();
                line.push(b'\n');
                output.write_all(&line).unwrap();
                output.write_all(&chunk).unwrap();
                output.flush().unwrap();
            }
        }

        /// The result of `op` with `req`, and the chunk bytes after its line.
        fn handle(&self, op: &str, req: Value) -> (Value, Vec<u8>) {
            let read = self.ops.repo.snapshot().unwrap();
            match op {
                WIRE_HELLO => {
                    let kinds = OBJECT_KINDS.map(str::to_owned).to_vec();
                    let hello = WireHello::new(WIRE_PROTOCOL, AGENT, 2, kinds, vec![], "test");
                    (serde_json::to_value(hello).unwrap(), Vec::new())
                }
                WIRE_REFS => {
                    let refs = self.refs().into_iter();
                    let refs = refs.map(|(name, id)| WireRef::new(name, id.to_string()));
                    let page = WireRefs::new(refs.collect(), true);
                    (serde_json::to_value(page).unwrap(), Vec::new())
                }
                WIRE_LIST => {
                    let req: WireListReq = serde_json::from_value(req).unwrap();
                    let have: Vec<ChangesetId> = req
                        .have
                        .iter()
                        .map(|id| changeset_id(id).unwrap())
                        .collect();
                    let want = changeset_id(&req.want).unwrap();
                    let missing = read.missing(&have, want, req.limit, None).unwrap();
                    let ids = missing.changesets.iter().map(ToString::to_string).collect();
                    (
                        serde_json::to_value(WireList::new(ids)).unwrap(),
                        Vec::new(),
                    )
                }
                WIRE_GET => {
                    let req: WireGetReq = serde_json::from_value(req).unwrap();
                    let blobs = req.blobs.iter().map(|want: &WireBlobWant| want.id.clone());
                    let ids: Vec<ObjectId> = (req.changesets.iter().cloned().chain(blobs))
                        .map(|id| id.parse().unwrap())
                        .collect();
                    let objects: Vec<(ObjectKind, ObjectId, Vec<u8>)> = (ids.into_iter())
                        .map(|id| {
                            let (kind, payload) = self.ops.repo.object(id).unwrap().unwrap();
                            (kind, id, payload)
                        })
                        .collect();
                    let records: Vec<(ObjectKind, ObjectId, &[u8])> = objects
                        .iter()
                        .map(|(k, id, p)| (*k, *id, p.as_slice()))
                        .collect();
                    let mut bytes = Vec::new();
                    chunk::write_chunk(&mut bytes, &records).unwrap();
                    let announced = json!({"chunks": 1, "bytes": bytes.len()});
                    (announced, bytes)
                }
                _ => panic!("no answer to {op}"),
            }
        }
    }

    /// A fetch's run: the host's store recorded on an [`ImageRecordingFs`] from its making, a
    /// first fetch, then the upstream moved on and a second fetch between the marks `fetch` and
    /// `fetched`.
    struct Recorded {
        _dir: tempfile::TempDir,
        timeline: Timeline,
        /// What the second fetch moved each ref of the host from and to.
        moves: BTreeMap<String, (Option<ChangesetId>, ChangesetId)>,
    }

    /// Records [`Recorded`]'s run.
    fn record() -> Recorded {
        // 1. The upstream: main, a branch, a tag.
        let up = Upstream::new();
        up.advance("main", "a.txt");
        let main = up.advance("main", "b.txt");
        up.set("feature/x", &main.to_string(), None);
        up.advance("feature/x", "x.txt");

        // 2. The host, recorded from its making, and its first fetch.
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let root = Utf8PathBuf::from_path_buf(base.join("host")).unwrap();
        let fs = Arc::new(ImageRecordingFs::new());
        let shared: Arc<dyn FsOps> = Arc::clone(&fs) as Arc<dyn FsOps>;
        let host = LocalRepo::create_with_fs_as(&root, "host", StoreFormat::V2, shared).unwrap();
        up.serve(|link| fetch(&host, link, HUB)).unwrap();
        let before = tracked(&host);

        // 3. The upstream moves on; the second fetch, marked.
        up.advance("main", "c.txt");
        up.advance("feature/x", "y.txt");
        let tag = up.advance("feature/x", "z.txt");
        up.set("tags/v1", &tag.to_string(), None);
        fs.mark("fetch");
        let fetched = up.serve(|link| fetch(&host, link, HUB)).unwrap();
        fs.mark("fetched");
        assert_eq!(fetched.moved.len(), 2, "{fetched:?}");
        assert_eq!(fetched.tags_created, 1, "{fetched:?}");
        let after = tracked(&host);
        drop(host);

        let names: HashSet<&String> = before.keys().chain(after.keys()).collect();
        let moves = (names.into_iter())
            .map(|name| (name.clone(), (before.get(name).copied(), after[name])))
            .collect();
        Recorded {
            _dir: dir,
            timeline: Timeline::new(&base, &fs.calls()),
            moves,
        }
    }

    /// The host's tracking refs of [`HUB`] and its tags, by their names.
    fn tracked(host: &LocalRepo) -> BTreeMap<String, ChangesetId> {
        let refs = host.list_refs().unwrap().into_iter();
        let ours = refs
            .filter(|(name, _)| name.starts_with("remote/hub/") || name.starts_with(TAG_PREFIX));
        ours.map(|(name, state)| (name, state.target)).collect()
    }

    /// The image check of the host store at `root`, the second fetch having returned when
    /// `acked`: it opens and verifies deep clean, and each ref the fetch moves names its old
    /// changeset or its new one, the new one once the fetch returned.
    fn check_host(root: &Utf8Path, rec: &Recorded, acked: bool) -> Result<(), String> {
        let host = LocalRepo::open_with_fs(root, Arc::new(RealFs::new()))
            .map_err(|e| format!("open: {e}"))?;
        let report = host
            .verify(Depth::Deep)
            .map_err(|e| format!("verify: {e}"))?;
        if !report.problems.is_empty() {
            return Err(format!("verify --deep: {:?}", report.problems));
        }
        let now = tracked(&host);
        for (name, (old, new)) in &rec.moves {
            let at = now.get(name).copied();
            let whole = at == Some(*new) || (!acked && at == *old);
            if !whole {
                return Err(format!("{name} names {at:?}, not {old:?} or {new}"));
            }
        }
        Ok(())
    }

    /// A power loss at any step of a fetch, the disk keeping nothing not durable (seed 0) or
    /// everything (seed 1), leaves a host that opens, verifies clean, and whose tracking refs
    /// and tags each name what they named before the fetch or what it moved them to; every one
    /// moved once the fetch returned.
    #[test]
    fn fetch_power_loss_images_keep_tracking_refs_whole_or_old() {
        let rec = record();
        let fetched_at = rec.timeline.mark("fetched");
        let mut seen = HashSet::new();
        for cut in rec.timeline.mark("fetch")..=rec.timeline.steps() {
            for seed in SEEDS {
                let image = rec.timeline.image(cut, &mut Seeded::new(seed, cut));
                if !seen.insert(image.digest()) {
                    continue;
                }
                let dir = tempfile::tempdir().unwrap();
                image.materialize(dir.path()).unwrap();
                let root = Utf8PathBuf::from_path_buf(dir.path().join("host")).unwrap();
                let checked = check_host(&root, &rec, cut >= fetched_at);
                checked.unwrap_or_else(|why| {
                    panic!("cut {cut} seed {seed}: {why}\n{}", image.describe())
                });
            }
        }
        assert!(seen.len() > 10, "{} images", seen.len());
    }

    /// A tag the host lacks is made; one that names the same changeset stays; one that names
    /// another is kept, never moved.
    #[test]
    fn a_tag_is_made_kept_or_reported() {
        let up = Upstream::new();
        let one = up.advance("main", "a.txt");
        let two = up.advance("main", "b.txt");
        let actor = "remote/hub";
        let repo = &up.ops.repo;
        assert_eq!(
            keep_tag(repo, "tags/t", one, actor).unwrap(),
            TagOutcome::Created
        );
        assert_eq!(
            keep_tag(repo, "tags/t", one, actor).unwrap(),
            TagOutcome::Same
        );
        assert_eq!(
            keep_tag(repo, "tags/t", two, actor).unwrap(),
            TagOutcome::Differs
        );
        assert_eq!(repo.get_ref("tags/t").unwrap().unwrap().target, one);
    }
}
