//! The land itself: [`Repo::land`], its request and outcome, and the merge and publication
//! steps the detached land and the rebase share.

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
use swarmops_store::{ObjectStore, Recovered};

use crate::commit::{Draft, WrittenChangeset};
use crate::error::RepoError;
use crate::merge::MergeOutcome;
use crate::read::{RepoRead, StoredChangeset};
use crate::repo::{Repo, ref_error};

use super::key::{KeyFound, TargetWalk};
use super::resolve::Resolving;
use super::{Publication, Published, Resolutions};

/// The metadata key of a landed changeset's idempotency key.
pub(super) const LAND_KEY: &str = "land_key";

/// The metadata key that names who landed a changeset: every changeset a land makes carries it.
const LANDER: &str = "lander";

/// The metadata key of the chain a landed changeset squashed.
pub(super) const SQUASHED_FROM: &str = "squashed_from";

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
    /// 1. The operations layer takes 1 to 32. A land with `expect_target` makes one.
    pub max_attempts: u32,
    /// Publish only while the target names this changeset (M5): one attempt, merged onto it and
    /// published by compare-and-swap; a target that names another, when the attempt reads it or
    /// when it publishes, is [`RepoError::CasMismatch`], and nothing is recorded. `None`: the
    /// land merges onto whatever the target names, and retries.
    pub expect_target: Option<ChangesetId>,
    /// What to make of some paths of the merge, decided against a head of the target, which
    /// each attempt applies to its merge before its conflict check ([`Resolutions`]); `None`:
    /// none.
    pub resolutions: Option<Resolutions>,
}

impl LandRequest {
    /// A land of `head` onto `target` by `lander`: its base the merge base, no key, the head's
    /// message, no metadata of the lander's, 8 attempts at most, no expected target, and no
    /// resolutions.
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
            expect_target: None,
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
    /// as one changeset in one write transaction, by the steps [the module documentation](super)
    /// gives, with their errors. Nothing is recorded unless it lands; a replay writes nothing.
    /// Its resolutions are read before the first attempt: [`RepoError::NotFound`] for an
    /// `against` the store does not hold, [`RepoError::MissingBlob`] for a blob one names by id
    /// that it does not hold.
    pub fn land(&self, req: LandRequest) -> Result<LandOutcome, RepoError> {
        // 1. The request, checked in the snapshot the first attempt reads in.
        let Started {
            mut landing,
            read,
            theirs,
        } = self.start(req)?;
        let seed = match &landing.req.key {
            Some(key) => key.as_bytes().to_vec(),
            None => landing.req.head.0.0.to_vec(),
        };
        let expected = landing.req.expect_target;
        let max_attempts = match expected {
            Some(_) => 1,
            None => landing.req.max_attempts.max(1),
        };

        // 2. The attempts, each from what the target names.
        let (mut read, mut theirs) = (Some(read), theirs);
        let mut lost = Recovered::default();
        for attempt in 1..=max_attempts {
            // The first attempt reads in the snapshot the request was checked in.
            let read = if let Some(read) = read.take() {
                read
            } else {
                thread::sleep(backoff(attempt, &seed));
                let read = self.snapshot()?;
                theirs = target_head(&read, &landing.req.target)?;
                read
            };
            match landing.attempt(read, theirs, attempt)? {
                Attempt::Done(outcome) => return Ok(outcome),
                Attempt::Moved { actual, recovered } => (theirs, lost) = (actual, recovered),
            }
        }

        // 3. Every attempt lost: an exact land's one, or all of them.
        if let Some(expected) = expected {
            return Err(RepoError::CasMismatch {
                target: landing.req.target,
                actual: theirs,
                expected,
                recovered: Box::new(lost),
            });
        }
        Err(RepoError::RetryExhausted {
            attempts: max_attempts,
            actual: theirs,
        })
    }

    /// Starts a land of `req` (step 1 of [the module documentation](super)): checks the request in
    /// a snapshot of its own, in which it reads the target's head, the change's head and base, and
    /// the resolutions. A rebase starts so too ([`crate::rebase`]).
    pub(crate) fn start(&self, mut req: LandRequest) -> Result<Started<'_, O>, RepoError> {
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
        let landing = Landing {
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
        Ok(Started {
            landing,
            read,
            theirs,
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
pub(super) fn stored<O: ObjectStore>(
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
    /// The target names `actual`, not the head the attempt read: try again from it.
    Moved {
        /// What the target names.
        actual: ChangesetId,
        /// What the publish's write transaction repaired first, if the publish found it moved.
        recovered: Recovered,
    },
}

/// A land started ([`Repo::start`]): its request checked, the snapshot it was checked in, and
/// the target's head there.
pub(crate) struct Started<'r, O: ObjectStore> {
    /// The land, before its first attempt.
    landing: Landing<'r, O>,
    /// The snapshot the request was checked in.
    read: RepoRead<'r, O>,
    /// What the target names there.
    theirs: ChangesetId,
}

impl<'r, O: ObjectStore> Started<'r, O> {
    /// The base the change is taken from.
    pub(crate) fn base(&self) -> ChangesetId {
        self.landing.base.id
    }

    /// What the target names in the snapshot.
    pub(crate) fn theirs(&self) -> ChangesetId {
        self.theirs
    }

    /// The snapshot the request was checked in.
    pub(crate) fn read(&self) -> &RepoRead<'r, O> {
        &self.read
    }

    /// One attempt's preparation onto the target's head in the snapshot (steps 2 to 4 of [the
    /// module documentation](super)), with the snapshot, for a caller that records the merge.
    pub(crate) fn prepare(mut self) -> Result<(RepoRead<'r, O>, Prep), RepoError> {
        let prep = self.landing.prepare(&self.read, self.theirs)?;
        Ok((self.read, prep))
    }
}

/// What an attempt's preparation came to (steps 2 to 4 of [the module documentation](super)).
pub(crate) enum Prep {
    /// The key's walk found the land made before, on the target's head `.0`: a replay, its
    /// `attempts` not yet counted.
    Replay(ChangesetId, LandOutcome),
    /// Nothing to land onto the target's head `.0`, for this reason.
    Nothing(ChangesetId, NothingToLandReason),
    /// The change merged onto the target's head. Boxed, as the merge would otherwise make every
    /// preparation this large.
    Merged(Box<Prepared>),
}

/// A change merged three-way onto the target's head, without conflicts: what a land writes and
/// publishes as `L`, and a rebase commits as `H'`.
pub(crate) struct Prepared {
    /// The merge: what turns the target's head's tree into the merged one (never empty), the
    /// contents it merged, and the paths it merged into new content.
    pub(crate) draft: MergeOutcome,
    /// The target's head it was merged onto.
    pub(crate) theirs: StoredChangeset,
    /// The base the change was taken from.
    pub(crate) base: ChangesetId,
    /// The chain the change squashes, oldest first: after the base, ending at the head.
    pub(crate) chain: Vec<ChangesetId>,
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
    /// One attempt (steps 2 to 6 of [the module documentation](super)), in `read`, whose target
    /// names `theirs`: its preparation, then the write and the publish of what it merged.
    fn attempt(
        &mut self,
        read: RepoRead<'_, O>,
        theirs: ChangesetId,
        attempt: u32,
    ) -> Result<Attempt, RepoError> {
        // 1. Steps 2 to 4; a finding without a merge is settled under the lock.
        let prepared = match self.prepare(&read, theirs)? {
            Prep::Replay(seen, replay) => {
                let replay = LandOutcome {
                    attempts: attempt,
                    ..replay
                };
                return self.settled(seen, Ok(replay));
            }
            Prep::Nothing(seen, reason) => {
                return self.settled(seen, Err(RepoError::NothingToLand(reason)));
            }
            Prep::Merged(prepared) => *prepared,
        };

        // 2. Steps 5 and 6. The snapshot is not read past the land's own write.
        drop(read);
        self.publish(prepared, attempt)
    }

    /// Steps 2 to 4 of [the module documentation](super), in `read`, whose target names
    /// `theirs`: what the land has to do there, or the merge to record.
    fn prepare(&mut self, read: &RepoRead<'_, O>, theirs: ChangesetId) -> Result<Prep, RepoError> {
        // 1. A head on the target's chain.
        if read.is_ancestor(self.req.head, theirs)? {
            return Ok(Prep::Nothing(theirs, NothingToLandReason::OnTarget));
        }
        let chain = self.squashed()?;
        let theirs = stored(read, theirs)?;

        // 2. The key's walk: a replay, or a key reused.
        if let Some(key) = &self.req.key {
            match self.walk.find_key(read, &theirs, key, self.req.head)? {
                Some(KeyFound::Replay(found)) => {
                    let replay = LandOutcome {
                        status: LandStatus::Replayed,
                        landed: found.id,
                        rebased_onto: found.parent,
                        base: self.base.id,
                        attempts: 0,
                        relocated: false,
                        squashed_from: found.squashed_from,
                        files_changed: found.files_changed,
                        auto_merged: Vec::new(),
                        warnings: Vec::new(),
                    };
                    return Ok(Prep::Replay(theirs.id, replay));
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

        // 3. An exact land onto another head than the one it expects: nothing to merge.
        if let Some(expected) = self.req.expect_target
            && theirs.id != expected
        {
            return Err(RepoError::CasMismatch {
                target: self.req.target.clone(),
                actual: theirs.id,
                expected,
                recovered: Box::default(),
            });
        }

        // 4. The merge, its resolutions, and its conflicts.
        let mut merged = self.merge(&theirs)?;
        let stale = match &self.resolving {
            Some(resolving) => resolving.apply(read, theirs.changeset.root, &mut merged)?,
            None => Vec::new(),
        };
        if !merged.conflicts.is_empty() || !stale.is_empty() {
            return Err(self.conflict(read, &theirs, merged, stale)?);
        }
        if merged.delta.is_empty() {
            return Ok(Prep::Nothing(theirs.id, NothingToLandReason::NoChanges));
        }
        Ok(Prep::Merged(Box::new(Prepared {
            draft: merged,
            theirs,
            base: self.base.id,
            chain,
        })))
    }

    /// Steps 5 and 6 of [the module documentation](super): writes `L` from `prepared`, then
    /// publishes it.
    fn publish(&mut self, prepared: Prepared, attempt: u32) -> Result<Attempt, RepoError> {
        let Prepared {
            draft,
            theirs,
            chain,
            ..
        } = prepared;
        self.auto_merged.extend(draft.auto_merged);
        let publication = self.write(&theirs, draft.delta, draft.new_blobs, &chain)?;
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
                    squashed_from: chain,
                    files_changed: publication.changeset.delta.len(),
                    auto_merged: std::mem::take(&mut self.auto_merged).into_iter().collect(),
                    warnings,
                }))
            }
            Published::Moved { actual, recovered } => {
                self.prior = Some(Prior {
                    onto: theirs.changeset.root,
                    root: publication.changeset.root,
                    paths: (publication.changeset.delta.iter())
                        .map(|entry| entry.path.clone())
                        .collect(),
                });
                Ok(Attempt::Moved { actual, recovered })
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
            return Ok(Attempt::Moved {
                actual: now,
                recovered: Recovered::default(),
            });
        }
        found.map(Attempt::Done)
    }
}

/// What a land records of itself in the changeset it makes, beside what it copies from the head.
#[derive(Clone, Copy)]
pub(super) struct Recorded<'a> {
    /// The land's key, recorded as `land_key`.
    pub(super) key: Option<&'a str>,
    /// Who lands, whose name is recorded as `lander`.
    pub(super) lander: &'a Signature,
    /// The lander's own metadata.
    pub(super) extra: &'a [(String, Vec<u8>)],
    /// The chain recorded as `squashed_from`, oldest first.
    pub(super) squashed: &'a [ChangesetId],
}

/// The metadata of the changeset a land makes of the change whose head is `head`, sorted by key:
/// the head's `agent` and `run_id`, the land's key, the lander, the lander's own metadata and the
/// chain, as `recorded` gives them.
pub(super) fn metadata(head: &Changeset, recorded: Recorded<'_>) -> Vec<(String, Vec<u8>)> {
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
pub(super) fn chain<O: ObjectStore>(
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
pub(super) fn merge_chain<O: ObjectStore>(
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
pub(super) struct Sides<'a> {
    /// The base the change was taken from.
    pub(super) base: ChangesetId,
    /// The change's head.
    pub(super) ours: ChangesetId,
    /// The changeset the change was merged onto.
    pub(super) theirs: &'a StoredChangeset,
}

/// The error of a merge onto `sides.theirs` that conflicts, or whose resolutions of `stale` did
/// not apply: its conflicts, with who last touched each path on `theirs`' first-parent chain since
/// the base, from `walk`, and the stale paths.
pub(super) fn conflict_report<O: ObjectStore>(
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
pub(super) struct Squash<'a> {
    /// The changeset `L` is made on, its one parent.
    pub(super) theirs: &'a StoredChangeset,
    /// The change's head, whose author and message `L` takes.
    pub(super) head: &'a Changeset,
    /// The merge's delta, applied to `theirs`' tree.
    pub(super) delta: Vec<DeltaEntry>,
    /// The merged contents among `L`'s blobs.
    pub(super) new_blobs: BTreeMap<BlobId, Vec<u8>>,
    /// Who lands: `L`'s committer.
    pub(super) lander: Signature,
    /// `L`'s message; `None`: the head's.
    pub(super) message: Option<Vec<u8>>,
    /// `L`'s metadata, sorted ([`metadata`]).
    pub(super) metadata: Vec<(String, Vec<u8>)>,
}

/// Writes `L` and flushes it (step 5): its blobs, trees and object are durable on return, before
/// anything names them (I1). A delta that changes nothing is [`NothingToLandReason::NoChanges`].
/// The one writer of `L`, the land's and the detached land's.
pub(super) fn write_squash<O: ObjectStore>(
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
