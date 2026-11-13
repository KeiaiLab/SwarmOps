//! Revert (the M18 design §4): [`Repo::revert`] takes back the first-parent range `(S, X]` of a
//! target `T` by landing its inverse, a land with changes. Nothing else is new: the merge, the
//! key's walk, the conflict report, the batch, the relocation and the publish are the land's.
//!
//! ```text
//!   T:  … ── S ── … ── X ── Y ── Z (head)      C = a commit on X: every path the range touched,
//!                       \                          set to its value at S (a leaf: put, by its
//!                        C                         blob id; nothing or a directory: deleted)
//!   land (base X, head X, commit C) onto T:  merge3(base X, ours C, theirs Z) → L, parent Z
//! ```
//!
//! `S` is `X`'s first parent by default, so the range is one changeset; a root `X` has the empty
//! tree there. A range whose changesets include merges is taken back against their first
//! parents only. The landed changeset records the range as `reverts` (hex ids, oldest first, a
//! space between each two) and the reason as `revert_reason`; neither is a git trailer. A keyed
//! revert run again replays, as `C`'s tree is a function of the range; the same key for another
//! range is [`RepoError::KeyReused`].

use std::collections::BTreeSet;

use swarmops_core::{ChangesetId, EntryTarget, RepoPath, Signature};
use swarmops_store::ObjectStore;

use crate::commit::{Change, ChangeOp, CommitRequest};
use crate::error::RepoError;
use crate::land::{LandOutcome, LandRequest, NothingToLandReason};
use crate::read::RepoRead;
use crate::repo::Repo;

/// The most changesets one revert takes back ([`RepoError::RevertTooLong`] past it).
pub const MAX_REVERT_CHANGESETS: usize = 256;

/// The most bytes of a revert's reason; the operations layer refuses a longer one, or none.
pub const MAX_REVERT_REASON_BYTES: usize = 4096;

/// The metadata key of the changesets a revert took back: hex ids, oldest first.
pub const REVERTS: &str = "reverts";

/// The metadata key of a revert's reason.
pub const REVERT_REASON: &str = "revert_reason";

/// How many hex digits of a changeset a revert's default message shows.
const SHORT_ID: usize = 12;

/// The attempts a revert's land makes by default, as a land's.
const MAX_ATTEMPTS: u32 = 8;

/// A revert to make ([`Repo::revert`]): the range `(since, changeset]` of `target`'s
/// first-parent chain taken back by a land.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RevertRequest {
    /// The range's last changeset, `X`: on the target's first-parent chain.
    pub changeset: ChangesetId,
    /// Where the range starts, `S`, exclusive: a first-parent ancestor of `changeset`. `None`:
    /// `changeset`'s first parent, or the empty tree for a root.
    pub since: Option<ChangesetId>,
    /// The ref to land the inverse on: `main` or `release/<x>`.
    pub target: String,
    /// Why: recorded as `revert_reason`, and the default message's body. The operations layer
    /// takes 1 to [`MAX_REVERT_REASON_BYTES`] bytes.
    pub reason: String,
    /// The landed changeset's message, and `C`'s; `None`: `Revert <X>: <X's title>` (a range:
    /// `Revert <n> changesets <S>..<X>`), a blank line, the reason.
    pub message: Option<Vec<u8>>,
    /// The land's idempotency key ([`LandRequest::key`]).
    pub key: Option<String>,
    /// Who reverts: `C`'s author and committer, and the land's lander.
    pub lander: Signature,
    /// The land's attempts at most ([`LandRequest::max_attempts`]).
    pub max_attempts: u32,
    /// Publish only while the target names this changeset ([`LandRequest::expect_target`]).
    pub expect_target: Option<ChangesetId>,
    /// `C`'s metadata: who made the inverse change (`agent`, `run_id`), which the land copies to
    /// the changeset it lands. Keys strictly ascending.
    pub metadata: Vec<(String, Vec<u8>)>,
    /// More metadata for the landed changeset ([`LandRequest::lander_metadata`]), beside
    /// [`REVERTS`] and [`REVERT_REASON`].
    pub lander_metadata: Vec<(String, Vec<u8>)>,
}

impl RevertRequest {
    /// A revert of `changeset` alone on `target` by `lander`, for `reason`: the default message,
    /// no key, 8 attempts at most, no expected target, and no metadata.
    pub fn new(
        changeset: ChangesetId,
        target: &str,
        reason: impl Into<String>,
        lander: Signature,
    ) -> RevertRequest {
        RevertRequest {
            changeset,
            since: None,
            target: target.to_owned(),
            reason: reason.into(),
            message: None,
            key: None,
            lander,
            max_attempts: MAX_ATTEMPTS,
            expect_target: None,
            metadata: Vec::new(),
            lander_metadata: Vec::new(),
        }
    }
}

impl<O: ObjectStore> Repo<O> {
    /// Takes back the range `req` names by landing its inverse (the [module
    /// documentation](self)). Checked in one snapshot first, writing nothing on a refusal: the
    /// target's name ([`RepoError::RefName`]) and existence, `X` and `S`
    /// ([`RepoError::NotFound`]), `S` a first-parent ancestor of `X`
    /// ([`RepoError::BaseNotAncestor`]), `X` on the target's first-parent chain
    /// ([`RepoError::RevertOffTarget`]), at most [`MAX_REVERT_CHANGESETS`] changesets
    /// ([`RepoError::RevertTooLong`]), and an inverse that changes something
    /// ([`RepoError::NothingToLand`], [`NothingToLandReason::NoChanges`]). Then one
    /// [`Repo::land`], with its outcome and its errors: a conflict with what the target gained
    /// since `X` is [`RepoError::Conflict`], and a range taken back already is
    /// [`RepoError::NothingToLand`].
    pub fn revert(&self, req: RevertRequest) -> Result<LandOutcome, RepoError> {
        // 1. The range and its inverse, from one snapshot.
        self.check_ref_name(&req.target)?;
        let read = self.snapshot()?;
        let range = range_of(&read, &req)?;
        let changes = inverse(&read, &req, &range)?;
        if changes.is_empty() {
            return Err(RepoError::NothingToLand(NothingToLandReason::NoChanges));
        }
        let message = match &req.message {
            Some(message) => message.clone(),
            None => default_message(&read, &req, &range)?,
        };
        drop(read);

        // 2. The land of `C`, the inverse committed on `X`, from `X`.
        let reverted: Vec<String> = range.iter().map(ToString::to_string).collect();
        let mut lander_metadata = req.lander_metadata;
        lander_metadata.push((REVERTS.to_owned(), reverted.join(" ").into_bytes()));
        lander_metadata.push((REVERT_REASON.to_owned(), req.reason.into_bytes()));
        let commit = CommitRequest::new(
            vec![req.changeset],
            changes,
            req.lander.clone(),
            req.lander.clone(),
            message.clone(),
            req.metadata,
            false,
            None,
        );
        let mut land = LandRequest::new(req.changeset, &req.target, req.lander);
        land.base = Some(req.changeset);
        land.key = req.key;
        land.message = Some(message);
        land.lander_metadata = lander_metadata;
        land.max_attempts = req.max_attempts;
        land.expect_target = req.expect_target;
        land.commit = Some(commit);
        self.land(land)
    }
}

/// The range `req` names in `read`, oldest first, its rules checked ([`Repo::revert`]).
fn range_of<O: ObjectStore>(
    read: &RepoRead<'_, O>,
    req: &RevertRequest,
) -> Result<Vec<ChangesetId>, RepoError> {
    let x = req.changeset;

    // 1. The target, `X` and `S`; `S` below `X`, `X` on the target.
    let target = read.get_ref(&req.target)?;
    let target = target.ok_or_else(|| RepoError::NotFound(req.target.clone()))?;
    let stored = read.changeset(x)?;
    let stored = stored.ok_or_else(|| RepoError::NotFound(x.to_string()))?;
    let since = match req.since {
        Some(since) => Some(since),
        None => stored.changeset.parents.first().copied(),
    };
    if let Some(since) = since {
        if read.changeset(since)?.is_none() {
            return Err(RepoError::NotFound(since.to_string()));
        }
        if !read.is_ancestor(since, x)? {
            return Err(RepoError::BaseNotAncestor {
                base: since,
                head: x,
            });
        }
    }
    if !read.is_ancestor(x, target.target)? {
        return Err(RepoError::RevertOffTarget {
            changeset: x,
            target: req.target.clone(),
        });
    }

    // 2. The first parents from `X` down to `S`, exclusive, or to the root.
    let mut range = Vec::new();
    for id in read.first_parents(x) {
        let id = id?;
        if Some(id) == since {
            break;
        }
        if range.len() == MAX_REVERT_CHANGESETS {
            return Err(RepoError::RevertTooLong {
                changeset: x,
                max: MAX_REVERT_CHANGESETS,
            });
        }
        range.push(id);
    }
    range.reverse();
    Ok(range)
}

/// `C`'s changes: every path the deltas of `range` touch, as `S` holds it, against what `X`
/// holds there. A leaf at `S` is put by its blob id, unless `X` holds the same; nothing or a
/// directory at `S` is a delete of the file `X` holds there, if any (the leaves below a
/// directory are paths of the range too, and get their own changes).
fn inverse<O: ObjectStore>(
    read: &RepoRead<'_, O>,
    req: &RevertRequest,
    range: &[ChangesetId],
) -> Result<Vec<Change>, RepoError> {
    // 1. The union of the range's paths.
    let mut paths: BTreeSet<RepoPath> = BTreeSet::new();
    for &id in range {
        let stored = read.changeset(id)?;
        let stored = stored.ok_or_else(|| RepoError::NotFound(id.to_string()))?;
        paths.extend(stored.changeset.delta.into_iter().map(|entry| entry.path));
    }

    // 2. Each path set back to `S`'s value: `S` is the first range changeset's first parent.
    let first = range.first().copied();
    let since = match first {
        Some(first) => read.first_parent(first)?,
        None => None,
    };
    let mut changes = Vec::new();
    for path in paths {
        let before = match since {
            Some(since) => read.entry_at(since, &path)?,
            None => None,
        };
        let now = read.entry_at(req.changeset, &path)?;
        let op = match (before, now) {
            (Some(EntryTarget::Leaf(kind, id)), now) => {
                if now == Some(EntryTarget::Leaf(kind, id)) {
                    continue;
                }
                ChangeOp::PutBlob { kind, id }
            }
            (_, Some(EntryTarget::Leaf(..))) => ChangeOp::Delete,
            _ => continue,
        };
        changes.push(Change::new(path, op));
    }
    Ok(changes)
}

/// The default message of the revert of `range` ([`RevertRequest::message`]).
fn default_message<O: ObjectStore>(
    read: &RepoRead<'_, O>,
    req: &RevertRequest,
    range: &[ChangesetId],
) -> Result<Vec<u8>, RepoError> {
    let short = |id: ChangesetId| id.to_string()[..SHORT_ID].to_owned();
    let x = req.changeset;

    // 1. A range of more than one changeset: its ends, and the reason as the body.
    if range.len() > 1 {
        let first = range.first().copied();
        let since = match first {
            Some(first) => read.first_parent(first)?,
            None => None,
        };
        let since = since.map_or_default(short);
        let subject = format!("Revert {} changesets {since}..{}", range.len(), short(x));
        return Ok(format!("{subject}\n\n{}\n", req.reason).into_bytes());
    }

    // 2. One changeset: its title.
    let stored = read.changeset(x)?;
    let stored = stored.ok_or_else(|| RepoError::NotFound(x.to_string()))?;
    let message = String::from_utf8_lossy(&stored.changeset.message).into_owned();
    let title = message.lines().next().unwrap_or_default().trim().to_owned();
    let subject = format!("Revert {}: {title}", short(x));
    Ok(format!("{subject}\n\n{}\n", req.reason).into_bytes())
}
