//! The landing queue's facts (the M22 core design §3-§6): a target's queue is the Queue facts
//! (kind 7) whose `other` is the target's ref name key, each about one entry, its subject.
//!
//! ```text
//!   enqueue ──▶ Enqueue(target, head, base, key)            subject = entry id, version 0
//!   round   ──▶ chain on the target's head H (land planner)
//!               │ clean     L1 ─▶ L2 ─▶ …  (each on the one before; a conflict passed over)
//!               │ probes    Candidate(Lk, tree, H, k) on entry k, the chain's changesets
//!               │ conflict  Removed(conflict, report blob)
//!               └ on H      Landed(H)
//!   runner  ──▶ Verdict(tree, state, check, output blob)     on the prefix's last entry
//!   withdraw──▶ Removed(withdrawn)
//!   fold    ◀── facts_by_other(ref_name_key(target)): entries in Enqueue order, their states,
//!               the tree → verdict map, the cancelled counts
//! ```
//!
//! Every write is one frame, and every fact in it a version compare-and-swap
//! (`Expect::Version(n)`, `n` the facts about its entry so far), the version also leading its
//! body. A write that finds its version passed reads its entry again under the lock and writes
//! at the new one, or, for a round, builds again. An enqueue looks for its key and writes its
//! Enqueue in one write transaction, and answers once the frame is committed.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use swarmops_core::TreeId;
use swarmops_core::{BlobId, Changeset, ChangesetId, Hashed, ObjectId, ObjectKind, Signature};
use swarmops_store::{
    CasResult, ChangesetMeta, CommitInfo, Expect, Fact, FactKind, MetaWrite, ObjectStore,
    StoreError, StoreFormat, ref_name_key,
};

use crate::error::RepoError;
use crate::land::{
    self, CleanPlan, LandCheckRequest, LandGroupRequest, MAX_GROUP_CHANGES, MergeConflicts,
    NothingToLandReason, PlanItem, PlanOutcome, backoff,
};
use crate::oplog::OpId;
use crate::read::RepoRead;
use crate::repo::{Repo, post_commit_warning, ref_error};
use crate::staged::Staged;

/// The feature a Queue fact needs (the FORMAT 3 design §2.6).
const FEATURE: &str = "queue";

/// The longest idempotency key an enqueue takes, in bytes: the Enqueue body's `str(128)`.
pub const MAX_KEY_BYTES: usize = 128;

/// The longest string a fact body's `str` field holds, in bytes.
const MAX_STR_BYTES: usize = 255;

/// The rounds a round builds at most before [`RepoError::QueueContended`]: each one lost to a
/// write that moved the target or passed an entry's version.
const MAX_ROUNDS: u32 = 8;

/// The Queue fact types (the M22 core design §3 table).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Type {
    Enqueue,
    Candidate,
    Verdict,
    Landed,
    Removed,
}

impl Type {
    /// Its type number in the fact.
    fn code(self) -> u8 {
        match self {
            Self::Enqueue => 0,
            Self::Candidate => 1,
            Self::Verdict => 2,
            Self::Landed => 3,
            Self::Removed => 4,
        }
    }

    /// The type numbered `code`; `None` for none.
    fn from_code(code: u8) -> Option<Type> {
        [
            Self::Enqueue,
            Self::Candidate,
            Self::Verdict,
            Self::Landed,
            Self::Removed,
        ]
        .into_iter()
        .find(|ty| ty.code() == code)
    }
}

/// An entry's id: the entity id of its Enqueue fact (the FORMAT 3 design §2.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub struct EntryId(pub [u8; 32]);

impl EntryId {
    /// The entry id of `bytes`.
    pub fn new(bytes: [u8; 32]) -> EntryId {
        EntryId(bytes)
    }

    /// The id of 64 hex digits `hex`; `None` for anything else.
    pub fn from_hex(hex: &str) -> Option<EntryId> {
        ObjectId::from_hex(hex).ok().map(|id| EntryId(id.0))
    }
}

impl fmt::Display for EntryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&ObjectId::from(self.0).to_hex())
    }
}

/// Where an entry stands (the M22 core design §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum EntryState {
    /// Open, and no candidate of it is being checked on the target's head.
    Waiting,
    /// Open: its latest candidate is on the target's head, and its tree has no verdict.
    Checking,
    /// The target holds its change.
    Landed,
    /// Taken out of the queue, for a reason.
    Removed,
}

/// What checking a candidate tree came to (the Verdict body's state).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum VerdictState {
    /// The check exited 0.
    Passed,
    /// It exited otherwise.
    Failed,
    /// The runner's timeout killed it.
    TimedOut,
    /// Another signal killed it: blamed on no entry.
    Cancelled,
}

impl VerdictState {
    /// Its value in the body.
    fn code(self) -> u8 {
        match self {
            Self::Passed => 0,
            Self::Failed => 1,
            Self::TimedOut => 2,
            Self::Cancelled => 3,
        }
    }

    /// The state of value `code`; `None` for none.
    fn from_code(code: u8) -> Option<VerdictState> {
        [Self::Passed, Self::Failed, Self::TimedOut, Self::Cancelled]
            .into_iter()
            .find(|state| state.code() == code)
    }
}

/// Why an entry was removed (the Removed body's reason).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RemovalReason {
    /// The check of the prefix it ends failed, the one before passing.
    CheckFailed,
    /// It conflicts with the target's head and the entries before it.
    Conflict,
    /// The check of the prefix it ends timed out.
    TimedOut,
    /// Its enqueuer withdrew it.
    Withdrawn,
    /// The target ref is gone.
    TargetGone,
    /// Its checks were cancelled too often.
    Cancelled,
}

impl RemovalReason {
    /// Its value in the body.
    fn code(self) -> u8 {
        match self {
            Self::CheckFailed => 0,
            Self::Conflict => 1,
            Self::TimedOut => 2,
            Self::Withdrawn => 3,
            Self::TargetGone => 4,
            Self::Cancelled => 5,
        }
    }

    /// The reason of value `code`; `None` for none.
    fn from_code(code: u8) -> Option<RemovalReason> {
        [
            Self::CheckFailed,
            Self::Conflict,
            Self::TimedOut,
            Self::Withdrawn,
            Self::TargetGone,
            Self::Cancelled,
        ]
        .into_iter()
        .find(|reason| reason.code() == code)
    }
}

/// An entry's candidate: the prefix of the chain that ends with it, on a head of the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Candidate {
    /// The prefix's last changeset.
    pub candidate: ChangesetId,
    /// Its root tree.
    pub tree: TreeId,
    /// The target's head the chain was built on.
    pub on_head: ChangesetId,
    /// How many entries the prefix holds: the entry's place in the chain, from 1.
    pub ahead: u16,
}

/// A verdict on a candidate tree, from any round.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Verdict {
    /// The tree checked.
    pub tree: TreeId,
    /// What the check came to.
    pub state: VerdictState,
    /// The check, as its command line.
    pub check: String,
    /// The blob that holds the tail of its output; `None` for none.
    pub output: Option<BlobId>,
    /// The entry it is recorded on: the last of the prefix.
    pub entry: EntryId,
}

/// Why and how an entry was removed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Removal {
    /// Why.
    pub reason: RemovalReason,
    /// The check whose verdict removed it; `None` for none.
    pub check: Option<String>,
    /// The candidate tree that verdict is of; `None` for none.
    pub tree: Option<TreeId>,
    /// The blob that holds the report: a conflict's paths, or a check's output.
    pub report: Option<BlobId>,
}

/// One entry of a target's queue, as its facts make it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Entry {
    /// Its id.
    pub id: EntryId,
    /// The ref it lands on.
    pub target: String,
    /// The change's head.
    pub head: ChangesetId,
    /// Where the change starts.
    pub base: ChangesetId,
    /// The key it was enqueued with.
    pub key: Option<String>,
    /// Where it stands.
    pub state: EntryState,
    /// Its latest candidate.
    pub candidate: Option<Candidate>,
    /// The target's changeset that holds its change, once landed.
    pub landed: Option<ChangesetId>,
    /// Why it was removed, once removed.
    pub removal: Option<Removal>,
    /// How many of its verdicts were cancelled.
    pub cancelled: u32,
    /// How many facts are about it: the version its next fact is written at.
    pub version: u32,
}

impl Entry {
    /// Whether it is still in the queue: waiting or checking.
    pub fn is_open(&self) -> bool {
        matches!(self.state, EntryState::Waiting | EntryState::Checking)
    }
}

/// A target's queue, folded from its facts ([`RepoRead::queue`]).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct QueueFold {
    /// The target.
    pub target: String,
    /// What the target names; `None` when it is gone.
    pub head: Option<ChangesetId>,
    /// Every entry, in the order of their Enqueue frames.
    pub entries: Vec<Entry>,
    /// The verdict on each tree checked: the latest, but a cancelled one never hides a decisive
    /// one.
    pub verdicts: BTreeMap<TreeId, Verdict>,
}

impl QueueFold {
    /// The open entries, in enqueue order.
    pub fn open(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter().filter(|entry| entry.is_open())
    }

    /// Entry `id`; `None` if the target's queue never held it.
    pub fn entry(&self, id: EntryId) -> Option<&Entry> {
        self.entries.iter().find(|entry| entry.id == id)
    }

    /// The verdict on `tree`; `None` for none.
    pub fn verdict(&self, tree: TreeId) -> Option<&Verdict> {
        self.verdicts.get(&tree)
    }
}

/// An enqueue ([`Repo::queue_enqueue`]): `head`'s change onto `target`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct EnqueueRequest {
    /// The ref to land on, which must exist.
    pub target: String,
    /// The change's head.
    pub head: ChangesetId,
    /// Where the change starts, a first-parent ancestor of the head; `None`, the merge base of
    /// the head and the target's head.
    pub base: Option<ChangesetId>,
    /// Makes a retry safe: at most [`MAX_KEY_BYTES`]. Scoped to the target.
    pub key: Option<String>,
    /// Who enqueues.
    pub actor: String,
}

impl EnqueueRequest {
    /// An enqueue of `head` onto `target` by `actor`, from the merge base, without a key.
    pub fn new(target: &str, head: ChangesetId, actor: &str) -> EnqueueRequest {
        EnqueueRequest {
            target: target.to_owned(),
            head,
            base: None,
            key: None,
            actor: actor.to_owned(),
        }
    }
}

/// What an enqueue did.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Enqueued {
    /// The entry.
    pub entry: Entry,
    /// Whether its key named the entry already, and nothing was written.
    pub replayed: bool,
    /// The operation that recorded it: its frame's seq; `None` when replayed.
    pub op: Option<u64>,
    /// Steps after the commit point that failed, one line each.
    pub warnings: Vec<String>,
}

/// A round ([`Repo::queue_round`]): the target's open entries built into a chain.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RoundRequest {
    /// The target.
    pub target: String,
    /// Build only while the target names this; nothing is recorded when it names another.
    pub expect_head: Option<ChangesetId>,
    /// The prefixes to record as candidates, each by the place of its last entry in the chain,
    /// from 1.
    pub probes: Vec<usize>,
    /// The chain's committer, as a land's.
    pub lander: Signature,
    /// Who writes the facts.
    pub actor: String,
}

impl RoundRequest {
    /// A round of `target` by `lander` and `actor`, recording no candidate.
    pub fn new(target: &str, lander: Signature, actor: &str) -> RoundRequest {
        RoundRequest {
            target: target.to_owned(),
            expect_head: None,
            probes: Vec::new(),
            lander,
            actor: actor.to_owned(),
        }
    }
}

/// One link of a round's chain.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RoundItem {
    /// The entry.
    pub entry: EntryId,
    /// The changeset of the prefix ending with it.
    pub candidate: ChangesetId,
    /// Its root tree.
    pub tree: TreeId,
    /// The verdict on the tree, from any round.
    pub verdict: Option<VerdictState>,
    /// How many of the entry's verdicts were cancelled.
    pub cancelled: u32,
}

/// What a round did.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Round {
    /// The head the chain is built on; `None` when the target is gone.
    pub head: Option<ChangesetId>,
    /// The chain, in enqueue order.
    pub items: Vec<RoundItem>,
    /// The entries it recorded landed: the target holds their change.
    pub landed: Vec<EntryId>,
    /// The entries it removed: a conflict, or the target gone.
    pub removed: Vec<EntryId>,
    /// The operation that recorded it; `None` when it recorded nothing.
    pub op: Option<u64>,
    /// Steps after the commit point that failed, one line each.
    pub warnings: Vec<String>,
}

/// A verdict to record ([`Repo::queue_verdict`]).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct VerdictRequest {
    /// The target.
    pub target: String,
    /// The entry that ends the prefix.
    pub entry: EntryId,
    /// The candidate tree.
    pub tree: TreeId,
    /// What the check came to.
    pub state: VerdictState,
    /// The check, as its command line: its first 255 bytes are kept.
    pub check: String,
    /// The tail of its output, kept as a blob; empty for none.
    pub output: Vec<u8>,
    /// Who records it.
    pub actor: String,
}

impl VerdictRequest {
    /// A verdict of `state` on `tree`, recorded on `entry` of `target`, without output.
    pub fn new(
        target: &str,
        entry: EntryId,
        tree: TreeId,
        state: VerdictState,
        check: &str,
        actor: &str,
    ) -> VerdictRequest {
        VerdictRequest {
            target: target.to_owned(),
            entry,
            tree,
            state,
            check: check.to_owned(),
            output: Vec::new(),
            actor: actor.to_owned(),
        }
    }
}

/// A removal to record at a version ([`Repo::queue_remove`]).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RemoveRequest {
    /// The target.
    pub target: String,
    /// The entry.
    pub entry: EntryId,
    /// The version its fold read: how many facts are about it.
    pub version: u32,
    /// Why.
    pub reason: RemovalReason,
    /// The check whose verdict removes it.
    pub check: Option<String>,
    /// The candidate tree that verdict is of.
    pub tree: Option<TreeId>,
    /// The report, kept as a blob; empty for none.
    pub report: Vec<u8>,
    /// Who removes it.
    pub actor: String,
}

impl RemoveRequest {
    /// A removal of `entry` of `target` at `version` for `reason`, without check or report.
    pub fn new(
        target: &str,
        entry: EntryId,
        version: u32,
        reason: RemovalReason,
        actor: &str,
    ) -> RemoveRequest {
        RemoveRequest {
            target: target.to_owned(),
            entry,
            version,
            reason,
            check: None,
            tree: None,
            report: Vec::new(),
            actor: actor.to_owned(),
        }
    }
}

/// What a write at a version did.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Written {
    /// Recorded as operation `op`.
    Recorded {
        /// Its frame's seq.
        op: u64,
        /// Steps after the commit point that failed, one line each.
        warnings: Vec<String>,
    },
    /// The entry's version is past the one given: nothing was written.
    Mismatch,
}

/// A Queue fact's body, before its version.
#[derive(Debug, Clone)]
enum Body {
    Enqueue {
        target: String,
        head: ChangesetId,
        base: ChangesetId,
        key: Option<String>,
    },
    Candidate(Candidate),
    Verdict {
        tree: TreeId,
        state: VerdictState,
        check: String,
        output: Option<BlobId>,
    },
    Landed(ChangesetId),
    Removed(Removal),
}

impl Body {
    /// Its type.
    fn ty(&self) -> Type {
        match self {
            Self::Enqueue { .. } => Type::Enqueue,
            Self::Candidate(_) => Type::Candidate,
            Self::Verdict { .. } => Type::Verdict,
            Self::Landed(_) => Type::Landed,
            Self::Removed(_) => Type::Removed,
        }
    }

    /// Its bytes, led by `version` (an Enqueue's is 0).
    fn encode(&self, version: u32) -> Vec<u8> {
        let mut out = version.to_le_bytes().to_vec();
        match self {
            Self::Enqueue {
                target,
                head,
                base,
                key,
            } => {
                put_str(&mut out, target);
                out.extend_from_slice(&head.0.0);
                out.extend_from_slice(&base.0.0);
                put_str(&mut out, key.as_deref().unwrap_or(""));
            }
            Self::Candidate(c) => {
                out.extend_from_slice(&c.candidate.0.0);
                out.extend_from_slice(&c.tree.0.0);
                out.extend_from_slice(&c.on_head.0.0);
                out.extend_from_slice(&c.ahead.to_le_bytes());
            }
            Self::Verdict {
                tree,
                state,
                check,
                output,
            } => {
                out.extend_from_slice(&tree.0.0);
                out.push(state.code());
                put_str(&mut out, check);
                out.extend_from_slice(&blob_bytes(*output));
            }
            Self::Landed(landed) => out.extend_from_slice(&landed.0.0),
            Self::Removed(removal) => {
                out.push(removal.reason.code());
                put_str(&mut out, removal.check.as_deref().unwrap_or(""));
                out.extend_from_slice(&removal.tree.map_or([0; 32], |tree| tree.0.0));
                out.extend_from_slice(&blob_bytes(removal.report));
            }
        }
        out
    }

    /// The body of `fact`, a Queue fact the store checked against its type's layout.
    fn decode(fact: &Fact) -> Result<Body, RepoError> {
        let ty = Type::from_code(fact.ty()).ok_or_else(|| bad(fact))?;
        let mut r = Cursor::new(fact.body());
        r.u32()?;
        let body = match ty {
            Type::Enqueue => Body::Enqueue {
                target: r.string()?,
                head: ChangesetId::new(r.id()?),
                base: ChangesetId::new(r.id()?),
                key: Some(r.string()?).filter(|key| !key.is_empty()),
            },
            Type::Candidate => Body::Candidate(Candidate {
                candidate: ChangesetId::new(r.id()?),
                tree: TreeId::new(r.id()?),
                on_head: ChangesetId::new(r.id()?),
                ahead: r.u16()?,
            }),
            Type::Verdict => Body::Verdict {
                tree: TreeId::new(r.id()?),
                state: VerdictState::from_code(r.u8()?).ok_or_else(|| bad(fact))?,
                check: r.string()?,
                output: r.blob()?,
            },
            Type::Landed => Body::Landed(ChangesetId::new(r.id()?)),
            Type::Removed => Body::Removed(Removal {
                reason: RemovalReason::from_code(r.u8()?).ok_or_else(|| bad(fact))?,
                check: Some(r.string()?).filter(|check| !check.is_empty()),
                tree: r.blob()?.map(|id| TreeId::new(id.0)),
                report: r.blob()?,
            }),
        };
        Ok(body)
    }
}

/// A Queue fact's body that does not read as its type's: a bug, since the store checked it.
fn bad(fact: &Fact) -> RepoError {
    RepoError::Internal(format!(
        "a Queue fact of type {} whose body does not read",
        fact.ty()
    ))
}

/// Appends `text` as a body's `str`: its length byte, then its bytes.
fn put_str(out: &mut Vec<u8>, text: &str) {
    out.push(u8::try_from(text.len()).unwrap_or(u8::MAX));
    out.extend_from_slice(text.as_bytes());
}

/// A blob field's bytes: all zero for none.
fn blob_bytes(blob: Option<BlobId>) -> [u8; 32] {
    blob.map_or([0; 32], |blob| blob.0.0)
}

/// `text`'s first [`MAX_STR_BYTES`] bytes, cut at a character's start.
fn clip(text: &str) -> &str {
    let mut end = text.len().min(MAX_STR_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// A reader of a fact body.
struct Cursor<'a> {
    bytes: &'a [u8],
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Cursor<'a> {
        Cursor { bytes }
    }

    /// The next `n` bytes.
    fn take(&mut self, n: usize) -> Result<&'a [u8], RepoError> {
        if self.bytes.len() < n {
            return Err(RepoError::Internal(
                "a Queue fact body cut short".to_owned(),
            ));
        }
        let (head, rest) = self.bytes.split_at(n);
        self.bytes = rest;
        Ok(head)
    }

    fn u8(&mut self) -> Result<u8, RepoError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, RepoError> {
        let bytes = self.take(2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self) -> Result<u32, RepoError> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn id(&mut self) -> Result<ObjectId, RepoError> {
        let mut id = [0; 32];
        id.copy_from_slice(self.take(32)?);
        Ok(ObjectId::from(id))
    }

    /// A blob field: `None` when all zero.
    fn blob(&mut self) -> Result<Option<BlobId>, RepoError> {
        let id = self.id()?;
        Ok((id.0 != [0; 32]).then_some(BlobId::new(id)))
    }

    fn string(&mut self) -> Result<String, RepoError> {
        let len = usize::from(self.u8()?);
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec())
            .map_err(|_| RepoError::Internal("a Queue fact string not UTF-8".to_owned()))
    }
}

/// The Queue fact of `body` about `entry` of `target`, at `version`, by `actor`.
fn fact(
    entry: EntryId,
    target: &str,
    version: u32,
    body: &Body,
    actor: &str,
) -> Result<Fact, RepoError> {
    let fact = Fact::new(
        FactKind::Queue,
        body.ty().code(),
        entry.0,
        ref_name_key(target),
        unix_now(),
        actor.to_owned(),
        body.encode(version),
    )?;
    Ok(fact)
}

/// The entry `id`'s facts, folded against `head` (the target's) and `verdicts` (the target's
/// map); `None` when the first is no Enqueue.
fn entry_of(
    id: EntryId,
    facts: &[(Fact, Body)],
    head: Option<ChangesetId>,
    verdicts: &BTreeMap<TreeId, Verdict>,
) -> Option<Entry> {
    // 1. Its Enqueue, the first fact about it.
    let Some((
        _,
        Body::Enqueue {
            target,
            head: h,
            base,
            key,
        },
    )) = facts.first()
    else {
        return None;
    };
    let mut entry = Entry {
        id,
        target: target.clone(),
        head: *h,
        base: *base,
        key: key.clone(),
        state: EntryState::Waiting,
        candidate: None,
        landed: None,
        removal: None,
        cancelled: 0,
        version: u32::try_from(facts.len()).unwrap_or(u32::MAX),
    };

    // 2. The others, in journal order: the first Landed or Removed ends it.
    for (_, body) in &facts[1..] {
        match body {
            Body::Candidate(candidate) => entry.candidate = Some(*candidate),
            Body::Verdict {
                state: VerdictState::Cancelled,
                ..
            } => entry.cancelled += 1,
            Body::Landed(landed) if entry.is_open() => {
                entry.state = EntryState::Landed;
                entry.landed = Some(*landed);
            }
            Body::Removed(removal) if entry.is_open() => {
                entry.state = EntryState::Removed;
                entry.removal = Some(removal.clone());
            }
            _ => {}
        }
    }

    // 3. Open: checking while its latest candidate is on the head and has no verdict.
    let checking = entry
        .candidate
        .is_some_and(|c| Some(c.on_head) == head && !verdicts.contains_key(&c.tree));
    if entry.is_open() && checking {
        entry.state = EntryState::Checking;
    }
    Some(entry)
}

/// The Queue facts of `facts`, each with its body, in their order.
fn queue_bodies(facts: Vec<Fact>) -> Result<Vec<(Fact, Body)>, RepoError> {
    (facts.into_iter())
        .filter(|fact| fact.kind() == FactKind::Queue)
        .map(|fact| {
            let body = Body::decode(&fact)?;
            Ok((fact, body))
        })
        .collect()
}

impl<O: ObjectStore> RepoRead<'_, O> {
    /// Target `target`'s queue, folded from its Queue facts in this snapshot (the M22 core
    /// design §4): the entries in the order of their Enqueue frames, each one's state, latest
    /// candidate, landing or removal and cancelled count, and the tree → verdict map.
    pub fn queue(&self, target: &str) -> Result<QueueFold, RepoError> {
        // 1. The target's facts, in journal order, grouped by entry in Enqueue order.
        let head = self.get_ref(target)?.map(|state| state.target);
        let facts = queue_bodies(self.meta.facts_by_other(ref_name_key(target))?)?;
        let mut order: Vec<EntryId> = Vec::new();
        let mut by_entry: BTreeMap<EntryId, Vec<(Fact, Body)>> = BTreeMap::new();
        let mut verdicts: BTreeMap<TreeId, Verdict> = BTreeMap::new();
        for (fact, body) in facts {
            let id = EntryId(fact.subject());

            // 2. A verdict joins the map, a cancelled one never over a decisive one.
            if let Body::Verdict {
                tree,
                state,
                check,
                output,
            } = &body
            {
                let decisive =
                    (verdicts.get(tree)).is_some_and(|held| held.state != VerdictState::Cancelled);
                if !(decisive && *state == VerdictState::Cancelled) {
                    let verdict = Verdict {
                        tree: *tree,
                        state: *state,
                        check: check.clone(),
                        output: *output,
                        entry: id,
                    };
                    verdicts.insert(*tree, verdict);
                }
            }
            if !by_entry.contains_key(&id) {
                order.push(id);
            }
            by_entry.entry(id).or_default().push((fact, body));
        }

        // 3. Each entry folded, in Enqueue order.
        let mut entries = Vec::with_capacity(order.len());
        for id in order {
            let facts = by_entry.get(&id).map_or(&[][..], Vec::as_slice);
            let entry = entry_of(id, facts, head, &verdicts).ok_or_else(|| {
                RepoError::Internal(format!("Queue entry {id} has a fact before its Enqueue"))
            })?;
            entries.push(entry);
        }
        Ok(QueueFold {
            target: target.to_owned(),
            head,
            entries,
            verdicts,
        })
    }

    /// The Queue facts about entry `id`, in journal order: its Enqueue first.
    pub fn queue_facts(&self, id: EntryId) -> Result<Vec<Fact>, RepoError> {
        let facts = self.meta.facts_by_subject(id.0)?;
        Ok((facts.into_iter())
            .filter(|fact| fact.kind() == FactKind::Queue)
            .collect())
    }

    /// Entry `id` from its own facts alone, its target's head and verdicts not read: never
    /// checking. [`RepoError::NotFound`] for an id no entry has.
    fn entry_alone(&self, id: EntryId) -> Result<Entry, RepoError> {
        let facts = queue_bodies(self.queue_facts(id)?)?;
        entry_of(id, &facts, None, &BTreeMap::new())
            .ok_or_else(|| RepoError::NotFound(format!("queue entry {id}")))
    }
}

/// What one round attempt came to.
enum Attempt {
    /// Done, recorded or not.
    Done(Round),
    /// A write passed it: build again.
    Lost,
}

/// One clean link of a round's chain: its entry, and the changeset of the prefix ending there.
struct Link<'f> {
    entry: &'f Entry,
    id: ChangesetId,
    changeset: Changeset,
}

/// What a round's frame came to: its seq (`None` for no frame), and its warnings.
type Committed = (Option<u64>, Vec<String>);

/// A round's chain, built outside the lock.
struct Built<'f> {
    /// The target's head it is built on.
    head: ChangesetId,
    /// The clean entries' links, in enqueue order.
    links: Vec<Link<'f>>,
    /// The entries whose change the head holds.
    landed: Vec<&'f Entry>,
    /// The conflicting entries, each with its report.
    removed: Vec<(&'f Entry, Vec<u8>)>,
    /// The chain's trees and merged contents, not yet written.
    objects: Vec<Hashed>,
}

impl<O: ObjectStore> Repo<O> {
    /// Target `target`'s queue in a snapshot of its own ([`RepoRead::queue`]).
    pub fn queue_fold(&self, target: &str) -> Result<QueueFold, RepoError> {
        self.snapshot()?.queue(target)
    }

    /// Puts `req`'s change in its target's queue (the M22 core design §6), answering once the
    /// Enqueue's frame is committed. The change is planned on the target's head first, as a
    /// land check plans it ([`RepoError::NotFound`], [`RepoError::BaseNotAncestor`],
    /// [`RepoError::NoMergeBase`], [`RepoError::MergeChangeset`] as a land's). Then, under the
    /// lock: a key the target's queue holds with the same head and base (any base, for a
    /// request without one) answers its entry, replayed, writing nothing; with another,
    /// [`RepoError::QueueKeyReused`]. A change the target holds is
    /// [`RepoError::NothingToLand`]. A store before FORMAT 3 is [`StoreError::FormatRequired`].
    pub fn queue_enqueue(&self, req: &EnqueueRequest) -> Result<Enqueued, RepoError> {
        // 1. The request: a FORMAT 3 store, a target name, a key that fits.
        self.require_queue()?;
        self.check_ref_name(&req.target)?;
        if let Some(key) = &req.key
            && key.len() > MAX_KEY_BYTES
        {
            return Err(StoreError::BadCall(format!(
                "a queue key of {} bytes: at most {MAX_KEY_BYTES}",
                key.len()
            ))
            .into());
        }

        // 2. The change planned on the target's head, lock-free: its base, or nothing to land.
        let mut item = PlanItem::new(req.head);
        item.base = req.base;
        let check = self.land_check(LandCheckRequest::new(&req.target, vec![item]))?;
        let planned = check.items.into_iter().next();
        let (base, nothing) = match planned.map(|planned| planned.outcome) {
            Some(PlanOutcome::Clean(clean)) => (clean.base, None),
            Some(PlanOutcome::Conflict(conflicts)) => (conflicts.base, None),
            Some(PlanOutcome::NothingToLand(reason)) => {
                (req.base.unwrap_or(req.head), Some(reason))
            }
            Some(PlanOutcome::Replayed(_)) => (req.head, Some(NothingToLandReason::OnTarget)),
            None => {
                return Err(RepoError::Internal(
                    "a check of one change planned none".into(),
                ));
            }
        };

        // 3. Under the lock: the key's entry, answered or refused.
        let mut write = self.meta.write()?;
        let read = self.snapshot()?;
        if let Some(key) = &req.key {
            let fold = read.queue(&req.target)?;
            let held = (fold.entries.iter()).find(|entry| entry.key.as_deref() == Some(key));
            if let Some(entry) = held {
                let same = entry.head == req.head && req.base.is_none_or(|b| b == entry.base);
                if !same {
                    return Err(RepoError::QueueKeyReused {
                        key: key.clone(),
                        entry: entry.id,
                    });
                }
                return Ok(Enqueued {
                    entry: entry.clone(),
                    replayed: true,
                    op: None,
                    warnings: Vec::new(),
                });
            }
        }
        if let Some(reason) = nothing {
            return Err(RepoError::NothingToLand(reason));
        }

        // 4. The Enqueue, the frame's first record: its entity id is the entry's.
        let nonce = write.nonce().ok_or_else(|| self.queue_format())?;
        let id = EntryId(FactKind::Queue.entity_id(read.next_seq(), nonce, 0));
        drop(read);
        let body = Body::Enqueue {
            target: req.target.clone(),
            head: req.head,
            base,
            key: req.key.clone(),
        };
        let enqueue = fact(id, &req.target, 0, &body, &req.actor)?;
        expect_applied(write.put_fact(enqueue, Expect::Version(0))?)?;

        // 5. The commit: the answer only once the frame is durable.
        let (op, warnings) = committed(write, "the enqueue")?;
        let entry = Entry {
            id,
            target: req.target.clone(),
            head: req.head,
            base,
            key: req.key.clone(),
            state: EntryState::Waiting,
            candidate: None,
            landed: None,
            removal: None,
            cancelled: 0,
            version: 1,
        };
        Ok(Enqueued {
            entry,
            replayed: false,
            op: Some(op),
            warnings,
        })
    }

    /// Takes entry `id` out of its queue, reason withdrawn: at the version a lock-free read
    /// found, or, when a write passed it, at the one read again under the lock. A removed entry
    /// is answered as it is; a landed one is [`RepoError::QueueAlreadyLanded`], writing
    /// nothing. The answer is the entry as its target's queue folds it after.
    pub fn queue_withdraw(&self, id: EntryId, actor: &str) -> Result<Entry, RepoError> {
        // 1. Lock-free: the entry, and whether there is anything to do.
        self.require_queue()?;
        let entry = self.snapshot()?.entry_alone(id)?;
        if let Some(done) = settled(&entry)? {
            return Ok(done);
        }
        let removal = Removal {
            reason: RemovalReason::Withdrawn,
            check: None,
            tree: None,
            report: None,
        };
        let body = Body::Removed(removal);

        // 2. The Removed at that version; past it, the entry read again under the lock.
        let mut write = self.meta.write()?;
        let first = fact(id, &entry.target, entry.version, &body, actor)?;
        if let CasResult::Mismatch { .. } = write.put_fact(first, Expect::Version(entry.version))? {
            let again = self.snapshot()?.entry_alone(id)?;
            if let Some(done) = settled(&again)? {
                return Ok(done);
            }
            let retry = fact(id, &again.target, again.version, &body, actor)?;
            expect_applied(write.put_fact(retry, Expect::Version(again.version))?)?;
        }

        // 3. The commit, then the entry as the queue folds it.
        committed(write, "the withdraw")?;
        let fold = self.queue_fold(&entry.target)?;
        fold.entry(id)
            .cloned()
            .ok_or_else(|| RepoError::NotFound(format!("queue entry {id}")))
    }

    /// Records `req`'s removal at `req.version` only: [`Written::Mismatch`], writing nothing,
    /// when the entry's facts are past it. Its report blob is written and flushed first (W9).
    pub fn queue_remove(&self, req: &RemoveRequest) -> Result<Written, RepoError> {
        // 1. The report, durable before the frame names it.
        self.require_queue()?;
        let report = self.queue_blob(&req.report)?;
        let removal = Removal {
            reason: req.reason,
            check: req.check.as_deref().map(clip).map(str::to_owned),
            tree: req.tree,
            report,
        };

        // 2. The Removed at the version given, or nothing.
        let mut write = self.meta.write()?;
        let body = Body::Removed(removal);
        let removed = fact(req.entry, &req.target, req.version, &body, &req.actor)?;
        if let CasResult::Mismatch { .. } = write.put_fact(removed, Expect::Version(req.version))? {
            return Ok(Written::Mismatch);
        }
        let (op, warnings) = committed(write, "the removal")?;
        Ok(Written::Recorded { op, warnings })
    }

    /// Records `req`'s verdict on the prefix's last entry: at the version a lock-free read
    /// found, or, when a write passed it, at the one read again under the lock. Its output blob
    /// is written and flushed first (W9). [`RepoError::NotFound`] for an entry of no queue, or
    /// of another target's.
    pub fn queue_verdict(&self, req: &VerdictRequest) -> Result<Written, RepoError> {
        // 1. Lock-free: the entry's version; the output, durable before the frame names it.
        self.require_queue()?;
        let entry = self.snapshot()?.entry_alone(req.entry)?;
        if entry.target != req.target {
            return Err(RepoError::NotFound(format!("queue entry {}", req.entry)));
        }
        let body = Body::Verdict {
            tree: req.tree,
            state: req.state,
            check: clip(&req.check).to_owned(),
            output: self.queue_blob(&req.output)?,
        };

        // 2. The Verdict at that version; past it, at the one read again under the lock.
        let mut write = self.meta.write()?;
        let first = fact(req.entry, &req.target, entry.version, &body, &req.actor)?;
        if let CasResult::Mismatch { .. } = write.put_fact(first, Expect::Version(entry.version))? {
            let again = self.snapshot()?.entry_alone(req.entry)?;
            let retry = fact(req.entry, &req.target, again.version, &body, &req.actor)?;
            expect_applied(write.put_fact(retry, Expect::Version(again.version))?)?;
        }
        let (op, warnings) = committed(write, "the verdict")?;
        Ok(Written::Recorded { op, warnings })
    }

    /// One round of `req.target`'s queue (the M22 core design §5, steps 1, 2 and 4): its open
    /// entries, at most [`MAX_GROUP_CHANGES`], built as the land planner plans them onto the
    /// target's head H, each clean one a changeset on the one before (the first on H). In one
    /// frame, each fact at its entry's version: a conflicting entry removed with reason
    /// conflict and a report of its paths, the later ones built on the tip before it; one whose
    /// change H holds landed as H; and for each probe, the chain's changesets up to it and a
    /// Candidate on its last entry. A target gone removes every open entry. A write that moved
    /// the target or passed a version makes it build again, at most 8 times
    /// ([`RepoError::QueueContended`]).
    pub fn queue_round(&self, req: &RoundRequest) -> Result<Round, RepoError> {
        self.require_queue()?;
        self.check_ref_name(&req.target)?;
        for attempt in 1..=MAX_ROUNDS {
            thread::sleep(backoff(attempt, req.target.as_bytes()));
            if let Attempt::Done(round) = self.round_attempt(req)? {
                return Ok(round);
            }
        }
        Err(RepoError::QueueContended {
            target: req.target.clone(),
            attempts: MAX_ROUNDS,
        })
    }

    /// One attempt of the round `req`.
    fn round_attempt(&self, req: &RoundRequest) -> Result<Attempt, RepoError> {
        // 1. The fold, lock-free; a target gone, or not the one expected.
        let read = self.snapshot()?;
        let fold = read.queue(&req.target)?;
        let Some(head) = fold.head else {
            drop(read);
            return self.remove_all(req, &fold);
        };
        if req.expect_head.is_some_and(|expected| expected != head) {
            return Ok(Attempt::Done(empty_round(Some(head))));
        }
        let open: Vec<&Entry> = fold.open().take(MAX_GROUP_CHANGES).collect();
        if open.is_empty() {
            return Ok(Attempt::Done(empty_round(Some(head))));
        }

        // 2. The chain, planned and linked outside the lock.
        let mut built = self.build_chain(&read, req, head, &open)?;
        drop(read);

        // 3. The probes, each a place in the chain; the links up to the last one.
        let probes: BTreeSet<usize> = req.probes.iter().copied().collect();
        let chain = built.links.len();
        if let Some(&probe) = probes.iter().find(|&&p| p == 0 || p > chain) {
            return Err(RepoError::QueueProbe { probe, chain });
        }
        let upto = probes.last().copied().unwrap_or(0);

        // 4. Every object the frame names, written and flushed before it (I1, W9).
        let chain_objects = std::mem::take(&mut built.objects);
        let mut objects = if upto > 0 { chain_objects } else { Vec::new() };
        let mut reports = Vec::with_capacity(built.removed.len());
        for (_, bytes) in &built.removed {
            let blob = Hashed::new(ObjectKind::Blob, bytes.clone());
            reports.push(BlobId::new(blob.id()));
            objects.push(blob);
        }
        if !objects.is_empty() {
            self.objects.put_hashed_batch(objects)?;
            self.objects.flush()?;
        }

        // 5. The frame, unless a write moved the target or passed a version.
        let Some((op, warnings)) = self.record_round(req, &built, &reports, &probes)? else {
            return Ok(Attempt::Lost);
        };

        // 6. The chain as the fold sees it.
        let items = (built.links.iter())
            .map(|link| RoundItem {
                entry: link.entry.id,
                candidate: link.id,
                tree: link.changeset.root,
                verdict: fold.verdict(link.changeset.root).map(|v| v.state),
                cancelled: link.entry.cancelled,
            })
            .collect();
        Ok(Attempt::Done(Round {
            head: Some(head),
            items,
            landed: built.landed.iter().map(|entry| entry.id).collect(),
            removed: built.removed.iter().map(|(entry, _)| entry.id).collect(),
            op,
            warnings,
        }))
    }

    /// The open entries `open` planned in `read` onto `head`, as the land planner plans them,
    /// each clean one linked on the one before; nothing written.
    fn build_chain<'f>(
        &self,
        read: &RepoRead<'_, O>,
        req: &RoundRequest,
        head: ChangesetId,
        open: &[&'f Entry],
    ) -> Result<Built<'f>, RepoError> {
        // 1. The plan of every open entry, from its own base.
        let onto = (read.changeset(head)?).ok_or_else(|| RepoError::NotFound(head.to_string()))?;
        let items: Vec<PlanItem> = (open.iter())
            .map(|entry| {
                let mut item = PlanItem::new(entry.head);
                item.base = Some(entry.base);
                item
            })
            .collect();
        let staged = Staged::new(&self.objects);
        let planned = land::plan_chain(self, read, &req.target, &onto, &items, &staged)?;

        // 2. A clean one linked on the tip before it; a conflict passed over; one H holds landed.
        let group = LandGroupRequest::new(&req.target, Vec::new(), req.lander.clone());
        let mut built = Built {
            head,
            links: Vec::new(),
            landed: Vec::new(),
            removed: Vec::new(),
            objects: Vec::new(),
        };
        for ((&entry, item), planned) in open.iter().zip(&items).zip(&planned) {
            match &planned.outcome {
                PlanOutcome::Clean(clean) => {
                    let parent = (built.links.last())
                        .map_or((head, onto.changeset.root), |l| (l.id, l.changeset.root));
                    let (id, changeset) = link_one(read, &staged, &group, item, clean, parent)?;
                    built.links.push(Link {
                        entry,
                        id,
                        changeset,
                    });
                }
                PlanOutcome::Conflict(conflicts) => built.removed.push((entry, report(conflicts))),
                PlanOutcome::NothingToLand(_) | PlanOutcome::Replayed(_) => {
                    built.landed.push(entry);
                }
            }
        }
        built.objects = staged.into_objects();
        Ok(built)
    }

    /// Records `built` in one frame, under the lock, while the target still names its head:
    /// the links up to the last of `probes`, then each landed, removed (its report one of
    /// `reports`) and probed entry's fact at its version. `None`, recording nothing, when the
    /// target moved or a version was passed; else the frame's seq (`None` for no frame) and
    /// its warnings.
    fn record_round(
        &self,
        req: &RoundRequest,
        built: &Built<'_>,
        reports: &[BlobId],
        probes: &BTreeSet<usize>,
    ) -> Result<Option<Committed>, RepoError> {
        // 1. The target still at the head the chain is on.
        let mut write = self.meta.write()?;
        if write.get_ref(&req.target).map_err(ref_error)? != Some(built.head) {
            return Ok(None);
        }

        // 2. The chain's changesets up to the last probe, oldest first.
        let upto = probes.last().copied().unwrap_or(0);
        let metas = (built.links[..upto].iter())
            .map(|link| {
                let dh = link.changeset.delta_hash()?;
                let parents = link.changeset.parents.clone();
                Ok(ChangesetMeta::new(link.id, 0, 0, dh, parents))
            })
            .collect::<Result<Vec<_>, RepoError>>()?;
        write.insert_changesets_batch(metas).map_err(|e| match e {
            StoreError::MissingParent(parent) => RepoError::UnknownParent(parent),
            other => other.into(),
        })?;

        // 3. The facts: landed, removed for a conflict, a candidate for each probe.
        let mut facts: Vec<(&Entry, Body)> = Vec::new();
        for &entry in &built.landed {
            facts.push((entry, Body::Landed(built.head)));
        }
        for ((entry, _), blob) in built.removed.iter().zip(reports) {
            let removal = Removal {
                reason: RemovalReason::Conflict,
                check: None,
                tree: None,
                report: Some(*blob),
            };
            facts.push((entry, Body::Removed(removal)));
        }
        for &place in probes {
            let link = &built.links[place - 1];
            let candidate = Candidate {
                candidate: link.id,
                tree: link.changeset.root,
                on_head: built.head,
                ahead: u16::try_from(place).unwrap_or(u16::MAX),
            };
            facts.push((link.entry, Body::Candidate(candidate)));
        }
        for (entry, body) in &facts {
            let put = fact(entry.id, &req.target, entry.version, body, &req.actor)?;
            if let CasResult::Mismatch { .. } =
                write.put_fact(put, Expect::Version(entry.version))?
            {
                return Ok(None);
            }
        }

        // 4. The frame, if it holds a record.
        if write.queued().is_empty() {
            return Ok(Some((None, Vec::new())));
        }
        let (op, warnings) = committed(write, "the round")?;
        Ok(Some((Some(op), warnings)))
    }

    /// The round of a target gone: every open entry of `fold` removed with reason target_gone,
    /// in one frame, while the target is still gone under the lock.
    fn remove_all(&self, req: &RoundRequest, fold: &QueueFold) -> Result<Attempt, RepoError> {
        let open: Vec<&Entry> = fold.open().collect();
        if open.is_empty() {
            return Ok(Attempt::Done(empty_round(None)));
        }
        let mut write = self.meta.write()?;
        if write.get_ref(&req.target).map_err(ref_error)?.is_some() {
            return Ok(Attempt::Lost);
        }
        let removal = Removal {
            reason: RemovalReason::TargetGone,
            check: None,
            tree: None,
            report: None,
        };
        let body = Body::Removed(removal);
        for entry in &open {
            let put = fact(entry.id, &req.target, entry.version, &body, &req.actor)?;
            if let CasResult::Mismatch { .. } =
                write.put_fact(put, Expect::Version(entry.version))?
            {
                return Ok(Attempt::Lost);
            }
        }
        let (op, warnings) = committed(write, "the round")?;
        Ok(Attempt::Done(Round {
            head: None,
            items: Vec::new(),
            landed: Vec::new(),
            removed: open.iter().map(|entry| entry.id).collect(),
            op: Some(op),
            warnings,
        }))
    }

    /// Writes `bytes` as a blob and flushes it; `None`, writing nothing, for no bytes.
    fn queue_blob(&self, bytes: &[u8]) -> Result<Option<BlobId>, RepoError> {
        if bytes.is_empty() {
            return Ok(None);
        }
        let blob = Hashed::new(ObjectKind::Blob, bytes.to_vec());
        let id = BlobId::new(blob.id());
        self.objects.put_hashed_batch(vec![blob])?;
        self.objects.flush()?;
        Ok(Some(id))
    }

    /// Refuses a store before FORMAT 3, which holds no fact.
    fn require_queue(&self) -> Result<(), RepoError> {
        if self.format() == StoreFormat::V3 {
            return Ok(());
        }
        Err(self.queue_format())
    }

    /// [`StoreError::FormatRequired`] for the feature `queue` on this store.
    fn queue_format(&self) -> RepoError {
        StoreError::FormatRequired {
            name: FEATURE.to_owned(),
            needs: StoreFormat::V3.version(),
            store: self.format().version(),
        }
        .into()
    }
}

/// The changeset `item`, planned `clean`, makes on `parent` as a group land's link
/// ([`land::link`]).
fn link_one<O: ObjectStore>(
    read: &RepoRead<'_, O>,
    staged: &Staged<'_, O>,
    group: &LandGroupRequest,
    item: &PlanItem,
    clean: &CleanPlan,
    parent: (ChangesetId, TreeId),
) -> Result<(ChangesetId, Changeset), RepoError> {
    land::link(read, staged, group, (item, clean), parent)
}

/// A conflict's report: the change and the target's head, then each conflicting path, a line
/// each, e.g. `conflict f.txt`.
fn report(conflicts: &MergeConflicts) -> Vec<u8> {
    let mut out = format!(
        "base {} head {} onto {}\n",
        conflicts.base, conflicts.ours, conflicts.theirs
    );
    for conflict in &conflicts.conflicts {
        out.push_str("conflict ");
        out.push_str(conflict.path.as_str());
        out.push('\n');
    }
    for stale in &conflicts.stale {
        out.push_str("stale ");
        out.push_str(stale.as_str());
        out.push('\n');
    }
    out.into_bytes()
}

/// A round that recorded nothing, on `head`.
fn empty_round(head: Option<ChangesetId>) -> Round {
    Round {
        head,
        items: Vec::new(),
        landed: Vec::new(),
        removed: Vec::new(),
        op: None,
        warnings: Vec::new(),
    }
}

/// A withdraw's end before it writes: a removed entry as it is, a landed one refused.
fn settled(entry: &Entry) -> Result<Option<Entry>, RepoError> {
    match entry.state {
        EntryState::Landed => Err(RepoError::QueueAlreadyLanded { entry: entry.id }),
        EntryState::Removed => Ok(Some(entry.clone())),
        _ => Ok(None),
    }
}

/// A put that the lock holder's own read said would apply.
fn expect_applied(result: CasResult) -> Result<(), RepoError> {
    match result {
        CasResult::Applied { .. } => Ok(()),
        _ => Err(RepoError::Internal(
            "a Queue fact's version moved under the lock".to_owned(),
        )),
    }
}

/// Commits `write`: its frame's seq, and a warning when a step after the commit point failed
/// (`what` names the write).
fn committed(write: MetaWrite<'_>, what: &str) -> Result<(u64, Vec<String>), RepoError> {
    match write.commit() {
        Ok(CommitInfo { frame_seq, .. }) => Ok((frame_seq, Vec::new())),
        Err(StoreError::PostCommit {
            frame_seq,
            frame_crc,
            source,
        }) => {
            let warning = post_commit_warning(what, OpId::new(frame_seq, frame_crc), &source);
            Ok((frame_seq, vec![warning]))
        }
        Err(e) => Err(e.into()),
    }
}

/// The time now, in Unix seconds.
fn unix_now() -> i64 {
    let since = SystemTime::now().duration_since(UNIX_EPOCH);
    since.map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}
