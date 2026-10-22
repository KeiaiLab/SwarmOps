//! Landing (operations spec §3, the store-only block): [`Repo::land`] merges a change, the
//! first-parent chain from a base to a head, three-way onto a ref's head, and publishes the result
//! as one new changeset, `L`, the change squashed, in one write transaction with the ref's move to
//! it.
//!
//! A land makes attempts, at most [`LandRequest::max_attempts`], each reading in a snapshot of
//! its own ([`Repo::snapshot`]):
//!
//! 1. Before the first, the request: the target is `main` or `release/<x>`
//!    ([`RepoError::RefName`]) and exists ([`RepoError::NotFound`]); a base it gives is a
//!    first-parent ancestor of the head ([`RepoError::BaseNotAncestor`]), on the target or not (a
//!    base off the target is M11's form); without one, the base is the merge base of the head and
//!    the target's head, along first parents ([`RepoError::NoMergeBase`] when they share none).
//! 2. An attempt reads the target's head, `theirs`. A head on the target's first-parent chain has
//!    nothing to land ([`NothingToLandReason::OnTarget`]). The chain from the base (exclusive) to
//!    the head, which `L` squashes, holds no changeset of two parents or more
//!    ([`RepoError::MergeChangeset`]).
//! 3. A keyed land walks the target's first parents from `theirs` down to the base, or, for a
//!    base off the target, down to the first changeset at or below the base's generation (a land of
//!    the change lands above its base), for a changeset whose `land_key` metadata is the key: one
//!    whose `squashed_from` ends at the head is the land, made before, and the land is a replay
//!    ([`LandStatus::Replayed`]); any other means the key landed another change
//!    ([`RepoError::KeyReused`]). The walk reads each changeset once per land, so a later attempt
//!    reads only what the target gained.
//! 4. It merges three-way (storage spec §4): the first attempt what the chain changed onto
//!    `theirs` ([`Repo::merge`] of the base, the head and `theirs`, over the paths the chain
//!    touches); a later one what it merged before, `L`, onto what the target gained since, over
//!    `L`'s delta paths alone ([`Repo::merge_trees`] of the head `L` was made on, `L` and the head
//!    now: the incremental re-merge), and, if that conflicts, the whole merge again, whose result
//!    stands. The request's resolutions apply to the merge before its conflicts are counted
//!    ([`Resolutions`]): each, while `theirs` holds its path as the resolutions' `against` does,
//!    replaces what the merge made at and below its path, conflicts included; a path `theirs`
//!    holds otherwise is stale. Conflicts left, or a stale path, end the land, recording nothing
//!    ([`RepoError::Conflict`], with who last touched each conflicting path on the target since
//!    the base, found on the same walk, and the stale paths). A merge that leaves `theirs`' tree
//!    as it is has nothing to land ([`NothingToLandReason::NoChanges`]).
//! 5. It writes `L` outside the lock: its one parent `theirs`, its delta the merge's, its message
//!    the request's or the head's, its author the head's, its committer the lander; its metadata
//!    the head's `agent` and `run_id`, the key as `land_key`, the lander's name as `lander`, the
//!    request's `lander_metadata`, and `squashed_from`, the chain's ids, oldest first, in hex, a
//!    space between each two. Its blobs, trees and object are written and flushed before anything
//!    names them (I1).
//! 6. It publishes `L`: its insert and the target's move from `theirs` to it, by
//!    compare-and-swap, in one write transaction, the frame a commit with `advance` writes. A
//!    target that moved since drops the write, and the next attempt starts from what it names,
//!    after a wait from the fourth attempt on ([`backoff`]). A failure after the commit point is a
//!    warning; a journal append that fails (`NotCommitted`), a lock wait that times out (`Busy`)
//!    and any other error end the land as they are.
//!
//! A land that finds its change on the target, landed by a land with its key or there in any
//! other way, decides so under the store's lock, as a keyed commit's replay is decided: it takes a
//! write transaction, whose tail check makes durable the frames its snapshot saw (or takes back
//! those a failed sync left), reads the target there, and drops the transaction without a frame.
//! A target that names something else then makes another attempt. So a land never answers that it
//! landed, or had nothing to land, on the strength of a frame a failed sync could still take back.
//!
//! When the last attempt lost its race, the land fails with [`RepoError::RetryExhausted`]. A lost
//! attempt leaves nothing in the journal: the objects it flushed stay, unreferenced, for GC.
//!
//! The detached land ([`Repo::land_detached`], operations spec §3, the detached form) is the
//! first attempt without the key walk and the publication: the same chain walk (step 2), merge
//! (step 4), conflict report and writer of `L` (step 5), then `L`'s insert alone, moving no ref.

mod detached;
mod key;
mod publish;
mod resolve;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::thread;
use std::time::Duration;

use swarmops_core::{
    BlobId, Changeset, ChangesetId, DeltaEntry, Hashed, ObjectId, ObjectKind, RepoPath, Signature,
    TreeId,
};
use swarmops_merge::diff3::merge_content;
use swarmops_merge::tree_merge::FileConflict;
use swarmops_store::ObjectStore;

use crate::commit::{Draft, WrittenChangeset};
use crate::error::RepoError;
use crate::merge::MergeOutcome;
use crate::read::{RepoRead, StoredChangeset};
use crate::repo::{Repo, ref_error};

pub use detached::{DetachedLand, DetachedOutcome};
use key::{KeyFound, TargetWalk};
pub use publish::{Publication, Published};
use resolve::Resolving;
pub use resolve::{Resolution, Resolutions, Side};

/// The metadata key of a landed changeset's idempotency key.
const LAND_KEY: &str = "land_key";

/// The metadata key that names who landed a changeset: every changeset a land makes carries it.
const LANDER: &str = "lander";

/// The metadata key of the chain a landed changeset squashed.
const SQUASHED_FROM: &str = "squashed_from";

/// The keys of a head's metadata that a land copies to the changeset it makes: who made the change.
const COPIED: [&str; 2] = ["agent", "run_id"];

/// The attempts a land makes by default ([`LandRequest::new`]).
const MAX_ATTEMPTS: u32 = 8;

/// A land to make ([`Repo::land`]): the change, the first-parent chain from `base` to `head`, onto
/// the ref `target`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LandRequest {
    /// The change's head: the last changeset of the chain the land squashes.
    pub head: ChangesetId,
    /// Where the change starts: a first-parent ancestor of `head`, whose own changes are not part
    /// of it, on the target or not. `None`: the merge base of `head` and the target's head, along
    /// first parents.
    pub base: Option<ChangesetId>,
    /// The ref to land on: `main` or `release/<x>`, which must exist.
    pub target: String,
    /// The land's idempotency key, which the landed changeset records as `land_key`: a land with
    /// one looks for it on the target first, and replays the land it finds. The land takes it as
    /// it is; the operations layer checks its form (1 to 64 letters, digits and `-`).
    pub key: Option<String>,
    /// The landed changeset's message; `None`: the head's.
    pub message: Option<Vec<u8>>,
    /// Who lands: the landed changeset's committer, whose name the land records as `lander`, and
    /// who moves the target, as the reflog names them (at most 255 bytes).
    pub lander: Signature,
    /// More metadata for the landed changeset, beside what the land records itself (`agent`,
    /// `run_id`, `land_key`, `lander` and `squashed_from`): the lander's run as `lander_run_id`,
    /// say, or a change request's id. Any order: the land sorts it. A key the land records too
    /// makes a changeset it cannot encode: [`RepoError::Codec`], before anything is written.
    pub lander_metadata: Vec<(String, Vec<u8>)>,
    /// How many attempts the land makes at most before [`RepoError::RetryExhausted`]; 0 counts as
    /// 1. The operations layer takes 1 to 32.
    pub max_attempts: u32,
    /// What to make of some paths of the merge, decided against a head of the target, which
    /// each attempt applies to its merge before its conflict check ([`Resolutions`]); `None`:
    /// none.
    pub resolutions: Option<Resolutions>,
}

impl LandRequest {
    /// A land of `head` onto `target` by `lander`: its base the merge base, no key, the head's
    /// message, no metadata of the lander's, 8 attempts at most, and no resolutions.
    pub fn new(head: ChangesetId, target: &str, lander: Signature) -> LandRequest {
        LandRequest {
            head,
            base: None,
            target: target.to_owned(),
            key: None,
            message: None,
            lander,
            lander_metadata: Vec::new(),
            max_attempts: MAX_ATTEMPTS,
            resolutions: None,
        }
    }
}

/// What a land did ([`Repo::land`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LandOutcome {
    /// Whether it landed the change now, or found it landed by a land with its key.
    pub status: LandStatus,
    /// The landed changeset, `L`: the change squashed, on the target.
    pub landed: ChangesetId,
    /// The target's head `L` landed on: its parent.
    pub rebased_onto: ChangesetId,
    /// The base the change was taken from.
    pub base: ChangesetId,
    /// How many attempts it made: each read the target in a snapshot of its own, and published,
    /// or found the land made before.
    pub attempts: u32,
    /// Whether the publish made `L` anew on a head the target moved to after the land read it;
    /// the compare-and-swap publish never does.
    pub relocated: bool,
    /// The changesets `L` squashed, oldest first: the head's first-parent chain after the base,
    /// ending at the head; for a replay, those the replayed changeset records.
    pub squashed_from: Vec<ChangesetId>,
    /// How many paths `L` changes: its delta's length.
    pub files_changed: usize,
    /// The paths both sides changed whose contents merged into content neither had, sorted;
    /// empty for a replay.
    pub auto_merged: Vec<RepoPath>,
    /// Steps after the commit point that failed, one line each: the land is committed all the
    /// same, and the next write to the store repairs what is missing.
    pub warnings: Vec<String>,
}

/// Whether a land landed its change ([`LandOutcome::status`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LandStatus {
    /// It landed the change now.
    Landed,
    /// A land with the same key had landed the change before: nothing was written.
    Replayed,
}

/// Why a land has nothing to land ([`RepoError::NothingToLand`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NothingToLandReason {
    /// The head is on the target's first-parent chain.
    OnTarget,
    /// The target holds the change already: merged onto it, the change leaves its tree as it is.
    NoChanges,
}

impl NothingToLandReason {
    /// The reason's name, as the operations layer reports it: `on_target` or `no_changes`.
    pub fn as_str(self) -> &'static str {
        match self {
            NothingToLandReason::OnTarget => "on_target",
            NothingToLandReason::NoChanges => "no_changes",
        }
    }
}

/// The reason as a clause: "the head is on the target", "the target has the change already".
impl fmt::Display for NothingToLandReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            NothingToLandReason::OnTarget => "the head is on the target",
            NothingToLandReason::NoChanges => "the target has the change already",
        })
    }
}

/// Why a land conflicts ([`RepoError::Conflict`]): the three-way merge of the change onto the
/// target's head, and what a report of it needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeConflicts {
    /// The base the change was taken from.
    pub base: ChangesetId,
    /// The change's head.
    pub ours: ChangesetId,
    /// The target's head the change was merged onto.
    pub theirs: ChangesetId,
    /// The paths that conflict, sorted, each with its kind and the leaf each side holds there:
    /// every one, its hunks as a merge holds them ([`crate::merge`]: the first ones as diff3 made
    /// them, and each file's past those as one that spans them).
    pub conflicts: Vec<FileConflict>,
    /// The paths both sides changed whose contents merged cleanly, sorted.
    pub auto_merged: Vec<RepoPath>,
    /// For each conflicting path, the newest changeset on the target's first-parent chain since
    /// the base whose delta touches it (the path, a path below it, or one above it), and that
    /// changeset's `agent` metadata (empty when it names none). A path none of them touched has
    /// none: a base off the target can leave theirs' change below the walk.
    pub last_touched: BTreeMap<RepoPath, (ChangesetId, String)>,
    /// The paths whose resolutions did not apply, sorted: the target's head holds there what the
    /// resolutions' `against` does not ([`Resolutions`]). Empty for a land without resolutions.
    pub stale: Vec<RepoPath>,
}

/// How long a land waits before its attempt `attempt`, counted from 1 (operations spec §3 step
/// 7): nothing before the fourth; from it, `min(2 s, 25 ms·2^attempt)·U(0.5, 1.5)`, `U` drawn from
/// the BLAKE3 hash of `seed` and the attempt's number, so that the same land waits the same each
/// time, and landers of other changes spread out. A land's seed is its key, or its head's id.
pub fn backoff(attempt: u32, seed: &[u8]) -> Duration {
    if attempt < 4 {
        return Duration::ZERO;
    }
    let step = Duration::from_millis(25)
        .saturating_mul(1_u32 << attempt.min(16))
        .min(Duration::from_secs(2));
    let mut hasher = blake3::Hasher::new();
    hasher.update(seed);
    hasher.update(&attempt.to_le_bytes());
    let [a, b, c, d, ..] = *hasher.finalize().as_bytes();
    let unit = f64::from(u32::from_le_bytes([a, b, c, d])) / 4_294_967_296.0;
    step.mul_f64(0.5 + unit)
}

impl<O: ObjectStore> Repo<O> {
    /// Lands `req`: merges its change three-way onto the target's head and publishes the result
    /// as one changeset in one write transaction, by the steps [the module documentation](self)
    /// gives, with their errors. Nothing is recorded unless it lands; a replay writes nothing.
    /// Its resolutions are read before the first attempt: [`RepoError::NotFound`] for an
    /// `against` the store does not hold, [`RepoError::MissingBlob`] for a blob one names by id
    /// that it does not hold.
    pub fn land(&self, mut req: LandRequest) -> Result<LandOutcome, RepoError> {
        swarmops_store::check_ref_name(&req.target).map_err(ref_error)?;
        let read = self.snapshot()?;
        let theirs = target_head(&read, &req.target)?;
        let head = stored(&read, req.head)?;
        let base = match req.base {
            Some(base) if read.is_ancestor(base, req.head)? => base,
            Some(base) => {
                return Err(RepoError::BaseNotAncestor {
                    base,
                    head: req.head,
                });
            }
            None => read
                .merge_base(req.head, theirs)?
                .ok_or(RepoError::NoMergeBase {
                    head: req.head,
                    theirs,
                })?,
        };
        let base = stored(&read, base)?;
        let resolving = (req.resolutions.take())
            .map(|resolutions| Resolving::new(self, &read, resolutions, head.changeset.root))
            .transpose()?;
        let seed = match &req.key {
            Some(key) => key.as_bytes().to_vec(),
            None => req.head.0.0.to_vec(),
        };
        let max_attempts = req.max_attempts.max(1);
        let mut land = Landing {
            repo: self,
            walk: TargetWalk::new(base.generation),
            req,
            head,
            base,
            squashed: None,
            prior: None,
            auto_merged: BTreeSet::new(),
            resolving,
        };
        let (mut read, mut theirs) = (Some(read), theirs);
        for attempt in 1..=max_attempts {
            // The first attempt reads in the snapshot the request was checked in.
            let read = if let Some(read) = read.take() {
                read
            } else {
                thread::sleep(backoff(attempt, &seed));
                let read = self.snapshot()?;
                theirs = target_head(&read, &land.req.target)?;
                read
            };
            match land.attempt(read, theirs, attempt)? {
                Attempt::Done(outcome) => return Ok(outcome),
                Attempt::Moved(actual) => theirs = actual,
            }
        }
        Err(RepoError::RetryExhausted {
            attempts: max_attempts,
            actual: theirs,
        })
    }

    /// What ref `target` names under the store's lock, after the write's tail check, which makes
    /// the frames a snapshot showed durable, or takes back those a failed sync left: a write
    /// transaction taken for that alone, and dropped without a frame. [`RepoError::NotFound`] if
    /// the ref does not exist.
    fn target_under_lock(&self, target: &str) -> Result<ChangesetId, RepoError> {
        let write = self.meta.write()?;
        let named = write.get_ref(target).map_err(ref_error)?;
        named.ok_or_else(|| RepoError::NotFound(target.to_owned()))
    }
}

/// The changeset ref `target` names in `read`; [`RepoError::NotFound`] if it does not exist.
fn target_head<O: ObjectStore>(
    read: &RepoRead<'_, O>,
    target: &str,
) -> Result<ChangesetId, RepoError> {
    let state = read.get_ref(target)?;
    state
        .map(|state| state.target)
        .ok_or_else(|| RepoError::NotFound(target.to_owned()))
}

/// Changeset `id` as `read` holds it; [`RepoError::NotFound`] if it does not.
fn stored<O: ObjectStore>(
    read: &RepoRead<'_, O>,
    id: ChangesetId,
) -> Result<StoredChangeset, RepoError> {
    read.changeset(id)?
        .ok_or_else(|| RepoError::NotFound(id.to_string()))
}

/// What one attempt of a land came to.
enum Attempt {
    /// The land is done: landed now, or found landed before.
    Done(LandOutcome),
    /// The target names this changeset, not the head the attempt read: try again from it.
    Moved(ChangesetId),
}

/// What a lost attempt made, for the next to merge again: the incremental re-merge.
struct Prior {
    /// The root of the target's head the attempt merged onto.
    onto: TreeId,
    /// The root of the changeset it made.
    root: TreeId,
    /// The paths that changeset's delta changes.
    paths: Vec<RepoPath>,
}

/// One land, across its attempts.
struct Landing<'r, O: ObjectStore> {
    repo: &'r Repo<O>,
    req: LandRequest,
    /// The change's head.
    head: StoredChangeset,
    /// The base the change was taken from.
    base: StoredChangeset,
    /// The chain the land squashes, oldest first, once an attempt has walked it.
    squashed: Option<Vec<ChangesetId>>,
    /// The walk down the target's first parents, which each attempt continues.
    walk: TargetWalk,
    /// What the last attempt made, when it lost its race.
    prior: Option<Prior>,
    /// The paths the attempts merged into new content.
    auto_merged: BTreeSet<RepoPath>,
    /// The request's resolutions, read before the first attempt; `None` without any.
    resolving: Option<Resolving>,
}

impl<O: ObjectStore> Landing<'_, O> {
    /// One attempt (steps 2 to 6 of [the module documentation](self)), in `read`, whose target
    /// names `theirs`.
    fn attempt(
        &mut self,
        read: RepoRead<'_, O>,
        theirs: ChangesetId,
        attempt: u32,
    ) -> Result<Attempt, RepoError> {
        if read.is_ancestor(self.req.head, theirs)? {
            let on_target = RepoError::NothingToLand(NothingToLandReason::OnTarget);
            return self.settled(theirs, Err(on_target));
        }
        let squashed = self.squashed()?;
        let theirs = stored(&read, theirs)?;
        if let Some(key) = &self.req.key {
            match self.walk.find_key(&read, &theirs, key, self.req.head)? {
                Some(KeyFound::Replay(found)) => {
                    let replay = LandOutcome {
                        status: LandStatus::Replayed,
                        landed: found.id,
                        rebased_onto: found.parent,
                        base: self.base.id,
                        attempts: attempt,
                        relocated: false,
                        squashed_from: found.squashed_from,
                        files_changed: found.files_changed,
                        auto_merged: Vec::new(),
                        warnings: Vec::new(),
                    };
                    return self.settled(theirs.id, Ok(replay));
                }
                Some(KeyFound::Reused(landed)) => {
                    return Err(RepoError::KeyReused {
                        key: key.clone(),
                        landed,
                    });
                }
                None => {}
            }
        }
        let mut merged = self.merge(&theirs)?;
        let stale = match &self.resolving {
            Some(resolving) => resolving.apply(&read, theirs.changeset.root, &mut merged)?,
            None => Vec::new(),
        };
        if !merged.conflicts.is_empty() || !stale.is_empty() {
            return Err(self.conflict(&read, &theirs, merged, stale)?);
        }
        if merged.delta.is_empty() {
            let no_changes = RepoError::NothingToLand(NothingToLandReason::NoChanges);
            return self.settled(theirs.id, Err(no_changes));
        }
        // The snapshot is not read past the land's own write.
        drop(read);
        self.auto_merged.extend(merged.auto_merged);
        let publication = self.write(&theirs, merged.delta, merged.new_blobs, &squashed)?;
        match self.repo.publish(&publication)? {
            Published::Landed {
                id,
                onto,
                relocated,
                merged_in_lock,
                warnings,
            } => {
                self.auto_merged.extend(merged_in_lock);
                Ok(Attempt::Done(LandOutcome {
                    status: LandStatus::Landed,
                    landed: id,
                    rebased_onto: onto,
                    base: self.base.id,
                    attempts: attempt,
                    relocated,
                    squashed_from: squashed,
                    files_changed: publication.changeset.delta.len(),
                    auto_merged: std::mem::take(&mut self.auto_merged).into_iter().collect(),
                    warnings,
                }))
            }
            Published::Moved { actual } => {
                self.prior = Some(Prior {
                    onto: theirs.changeset.root,
                    root: publication.changeset.root,
                    paths: (publication.changeset.delta.iter())
                        .map(|entry| entry.path.clone())
                        .collect(),
                });
                Ok(Attempt::Moved(actual))
            }
        }
    }

    /// The chain the land squashes ([`chain`]), walked the first time an attempt needs it.
    fn squashed(&mut self) -> Result<Vec<ChangesetId>, RepoError> {
        if let Some(squashed) = &self.squashed {
            return Ok(squashed.clone());
        }
        let chain = chain(self.repo, self.base.id, self.head.id)?;
        self.squashed = Some(chain.clone());
        Ok(chain)
    }

    /// The merge of the change onto `theirs`: the incremental re-merge of what the last attempt
    /// made, when one lost its race, unless it conflicts; else the whole merge, from the base.
    fn merge(&self, theirs: &StoredChangeset) -> Result<MergeOutcome, RepoError> {
        if let Some(prior) = &self.prior {
            let again = (self.repo).merge_trees(
                prior.onto,
                prior.root,
                theirs.changeset.root,
                &prior.paths,
            )?;
            if again.conflicts.is_empty() {
                return Ok(again);
            }
        }
        merge_chain(self.repo, &self.base, &self.head, theirs)
    }

    /// The error of a merge onto `theirs` that conflicts, or whose resolutions of `stale` did not
    /// apply: its conflicts, with who last touched each path on the target since the base, from
    /// the walk, and the stale paths.
    fn conflict(
        &mut self,
        read: &RepoRead<'_, O>,
        theirs: &StoredChangeset,
        merged: MergeOutcome,
        stale: Vec<RepoPath>,
    ) -> Result<RepoError, RepoError> {
        let sides = Sides {
            base: self.base.id,
            ours: self.head.id,
            theirs,
        };
        conflict_report(&mut self.walk, read, sides, merged, stale)
    }

    /// Writes the changeset the land publishes, `L`, and flushes it (step 5): `delta` applied to
    /// `theirs`' tree, the merged contents `new_blobs` among its blobs.
    fn write(
        &self,
        theirs: &StoredChangeset,
        delta: Vec<DeltaEntry>,
        new_blobs: BTreeMap<BlobId, Vec<u8>>,
        squashed: &[ChangesetId],
    ) -> Result<Publication, RepoError> {
        let head = &self.head.changeset;
        let recorded = Recorded {
            key: self.req.key.as_deref(),
            lander: &self.req.lander,
            extra: &self.req.lander_metadata,
            squashed,
        };
        let squash = Squash {
            theirs,
            head,
            delta,
            new_blobs,
            lander: self.req.lander.clone(),
            message: self.req.message.clone(),
            metadata: metadata(head, recorded),
        };
        let written = write_squash(self.repo, squash)?;
        Ok(Publication {
            target: self.req.target.clone(),
            onto: theirs.id,
            id: written.id,
            changeset: written.changeset,
            actor: String::from_utf8_lossy(&self.req.lander.name).into_owned(),
        })
    }

    /// `found`, an attempt's finding that the land is done, landed now or before, or has nothing to
    /// land, decided under the lock: so if the target still names `seen`, the head the attempt
    /// read; else another attempt from what it names.
    fn settled(
        &self,
        seen: ChangesetId,
        found: Result<LandOutcome, RepoError>,
    ) -> Result<Attempt, RepoError> {
        let now = self.repo.target_under_lock(&self.req.target)?;
        if now != seen {
            return Ok(Attempt::Moved(now));
        }
        found.map(Attempt::Done)
    }
}

/// What a land records of itself in the changeset it makes, beside what it copies from the head.
#[derive(Clone, Copy)]
struct Recorded<'a> {
    /// The land's key, recorded as `land_key`.
    key: Option<&'a str>,
    /// Who lands, whose name is recorded as `lander`.
    lander: &'a Signature,
    /// The lander's own metadata.
    extra: &'a [(String, Vec<u8>)],
    /// The chain recorded as `squashed_from`, oldest first.
    squashed: &'a [ChangesetId],
}

/// The metadata of the changeset a land makes of the change whose head is `head`, sorted by key:
/// the head's `agent` and `run_id`, the land's key, the lander, the lander's own metadata and the
/// chain, as `recorded` gives them.
fn metadata(head: &Changeset, recorded: Recorded<'_>) -> Vec<(String, Vec<u8>)> {
    let mut metadata: Vec<(String, Vec<u8>)> = (head.metadata.iter())
        .filter(|(key, _)| COPIED.contains(&key.as_str()))
        .cloned()
        .collect();
    if let Some(key) = recorded.key {
        metadata.push((LAND_KEY.to_owned(), key.as_bytes().to_vec()));
    }
    metadata.push((LANDER.to_owned(), recorded.lander.name.clone()));
    metadata.extend(recorded.extra.iter().cloned());
    let chain: Vec<String> = (recorded.squashed.iter())
        .map(ToString::to_string)
        .collect();
    metadata.push((SQUASHED_FROM.to_owned(), chain.join(" ").into_bytes()));
    metadata.sort_by(|a, b| a.0.cmp(&b.0));
    metadata
}

/// The chain a land squashes, from `base` (exclusive) to `head`, oldest first, walked by the
/// changesets' objects, which name their parents by their hashes (an index record's parents are
/// what a damaged index says: a first-parent walk never takes them, [`RepoRead::walk_step`]):
/// [`RepoError::MergeChangeset`] for a changeset of it with two parents or more. The land's step
/// 2 and the detached land's.
fn chain<O: ObjectStore>(
    repo: &Repo<O>,
    base: ChangesetId,
    head: ChangesetId,
) -> Result<Vec<ChangesetId>, RepoError> {
    let mut chain = Vec::new();
    let mut at = head;
    while at != base {
        let parents = repo.changeset_object(at)?.parents;
        if parents.len() > 1 {
            return Err(RepoError::MergeChangeset(at));
        }
        chain.push(at);
        // The base is a first-parent ancestor of the head: a root comes after it, if at all.
        at = *parents
            .first()
            .ok_or(RepoError::BaseNotAncestor { base, head })?;
    }
    chain.reverse();
    Ok(chain)
}

/// The whole merge of a change, `base` to `head`, onto `theirs` (step 4's first attempt): the
/// one merge the land and the detached land make.
fn merge_chain<O: ObjectStore>(
    repo: &Repo<O>,
    base: &StoredChangeset,
    head: &StoredChangeset,
    theirs: &StoredChangeset,
) -> Result<MergeOutcome, RepoError> {
    let roots = [
        base.changeset.root,
        head.changeset.root,
        theirs.changeset.root,
    ];
    repo.merge_roots(roots, &merge_content)
}

/// The three sides of a merge a conflict report names.
#[derive(Clone, Copy)]
struct Sides<'a> {
    /// The base the change was taken from.
    base: ChangesetId,
    /// The change's head.
    ours: ChangesetId,
    /// The changeset the change was merged onto.
    theirs: &'a StoredChangeset,
}

/// The error of a merge onto `sides.theirs` that conflicts, or whose resolutions of `stale` did
/// not apply: its conflicts, with who last touched each path on `theirs`' first-parent chain since
/// the base, from `walk`, and the stale paths.
fn conflict_report<O: ObjectStore>(
    walk: &mut TargetWalk,
    read: &RepoRead<'_, O>,
    sides: Sides<'_>,
    merged: MergeOutcome,
    stale: Vec<RepoPath>,
) -> Result<RepoError, RepoError> {
    let paths: Vec<RepoPath> = (merged.conflicts.iter())
        .map(|conflict| conflict.path.clone())
        .collect();
    let last_touched = walk.last_touched(read, sides.theirs, &paths)?;
    Ok(RepoError::Conflict(Box::new(MergeConflicts {
        base: sides.base,
        ours: sides.ours,
        theirs: sides.theirs.id,
        conflicts: merged.conflicts,
        auto_merged: merged.auto_merged,
        last_touched,
        stale,
    })))
}

/// The changeset a land makes, `L`, to write (step 5).
struct Squash<'a> {
    /// The changeset `L` is made on, its one parent.
    theirs: &'a StoredChangeset,
    /// The change's head, whose author and message `L` takes.
    head: &'a Changeset,
    /// The merge's delta, applied to `theirs`' tree.
    delta: Vec<DeltaEntry>,
    /// The merged contents among `L`'s blobs.
    new_blobs: BTreeMap<BlobId, Vec<u8>>,
    /// Who lands: `L`'s committer.
    lander: Signature,
    /// `L`'s message; `None`: the head's.
    message: Option<Vec<u8>>,
    /// `L`'s metadata, sorted ([`metadata`]).
    metadata: Vec<(String, Vec<u8>)>,
}

/// Writes `L` and flushes it (step 5): its blobs, trees and object are durable on return, before
/// anything names them (I1). A delta that changes nothing is [`NothingToLandReason::NoChanges`].
/// The one writer of `L`, the land's and the detached land's.
fn write_squash<O: ObjectStore>(
    repo: &Repo<O>,
    squash: Squash<'_>,
) -> Result<WrittenChangeset, RepoError> {
    let Squash {
        theirs,
        head,
        delta,
        new_blobs,
        lander,
        message,
        metadata,
    } = squash;

    // 1. The merged contents, as blob objects hashed once.
    let contents: BTreeMap<ObjectId, Hashed> = (new_blobs.into_values())
        .map(|bytes| {
            let blob = Hashed::new(ObjectKind::Blob, bytes);
            (blob.id(), blob)
        })
        .collect();

    // 2. L on theirs, the head's author, the lander its committer.
    let draft = Draft {
        onto: Some(theirs.changeset.root),
        changes: delta,
        contents,
        parents: vec![theirs.id],
        author: head.author.clone(),
        committer: lander,
        message: message.unwrap_or_else(|| head.message.clone()),
        metadata,
        allow_empty: false,
    };

    // 3. The merge puts only what theirs lacks, and deletes only what it holds: a delta that
    // changes nothing is the change theirs holds already.
    repo.write_changeset(draft).map_err(|e| match e {
        RepoError::EmptyChanges => RepoError::NothingToLand(NothingToLandReason::NoChanges),
        other => other,
    })
}
