//! The planner of changes landed in order onto one head of a target (the M18 design §6.1): what
//! [`Repo::land_check`] reports, and what a group land writes.
//!
//! ```text
//!   onto ──merge_chain(base₁, head₁)──▶ L₁'s tree ──merge_trees_in(base₂, head₂)──▶ L₂'s tree …
//!          (the land's whole merge)        │          (over the Staged overlay)
//!                                          ▼
//!                                  trees and merged blobs kept in the overlay, never written
//! ```
//!
//! Every item is checked first, as a land checks its request: its head and base, its chain, and
//! its key's walk on the target. A rev not found, a base that is no ancestor, a merge in a chain
//! or a key used for another change fails the whole plan before any merge. Then each item is
//! merged onto what the clean items before it made: the first by the land's whole merge, a later
//! one by the staged re-merge, which reads the trees no store holds. A conflicting item gets the
//! land's report and is passed over; an item on the target, or one whose change the target
//! holds, has nothing to land; a key the target holds a land of is a replay.

use std::collections::{BTreeMap, BTreeSet};

use swarmops_core::{BlobId, ChangesetId, DeltaEntry, Hashed, ObjectKind, RepoPath, Tree, TreeId};
use swarmops_merge::diff3;
use swarmops_merge::tree_diff::{self, ChangeKind, PathFilter, TreeRead};
use swarmops_store::ObjectStore;

use crate::error::RepoError;
use crate::merge::{self, MergeOutcome};
use crate::read::{RepoRead, StoredChangeset};
use crate::repo::{self, Repo};
use crate::staged::Staged;
use crate::tree_edit::{Encoded, TreeEdit};

use super::key::{KeyFound, Ours, TargetWalk};
use super::landing::{self, MergeConflicts, More, NothingToLandReason, Sides, stored};

/// The most changes one check or group takes; the operations layer refuses more.
pub const MAX_GROUP_CHANGES: usize = 32;

/// The most paths a clean item's summary lists ([`CleanPlan::paths`]).
pub const MAX_CHECK_PATHS: usize = 1000;

/// One change of a plan: the first-parent chain from `base` to `head`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PlanItem {
    /// The change's head.
    pub head: ChangesetId,
    /// Where the change starts, as [`super::LandRequest::base`]: `None`, the merge base of the
    /// head and the target's head.
    pub base: Option<ChangesetId>,
    /// The landed changeset's message; `None`: the head's. A check does not read it.
    pub message: Option<Vec<u8>>,
    /// The change's idempotency key, looked for on the target as a land looks for it.
    pub key: Option<String>,
}

impl PlanItem {
    /// The change whose head is `head`, from the merge base, without a message or a key.
    pub fn new(head: ChangesetId) -> PlanItem {
        PlanItem {
            head,
            base: None,
            message: None,
            key: None,
        }
    }
}

/// A dry run of lands ([`Repo::land_check`]): `items`, in order, onto `target`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct LandCheckRequest {
    /// The ref the changes would land on: `main` or `release/<x>`, which must exist.
    pub target: String,
    /// The changes, in the order they would land.
    pub items: Vec<PlanItem>,
}

impl LandCheckRequest {
    /// A check of `items` onto `target`.
    pub fn new(target: &str, items: Vec<PlanItem>) -> LandCheckRequest {
        LandCheckRequest {
            target: target.to_owned(),
            items,
        }
    }
}

/// What a dry run found ([`Repo::land_check`]).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct LandCheck {
    /// The target's head the items were planned on.
    pub onto: ChangesetId,
    /// Each item's plan, in the request's order.
    pub items: Vec<Planned>,
}

impl LandCheck {
    /// Makes one of the fields given.
    pub fn new(onto: ChangesetId, items: Vec<Planned>) -> LandCheck {
        LandCheck { onto, items }
    }
}

/// One item's plan.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Planned {
    /// The change's head.
    pub head: ChangesetId,
    /// What landing it after the clean items before it would come to.
    pub outcome: PlanOutcome,
}

impl Planned {
    /// Makes one of the fields given.
    pub fn new(head: ChangesetId, outcome: PlanOutcome) -> Planned {
        Planned { head, outcome }
    }
}

/// What landing one item would come to ([`Planned::outcome`]).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PlanOutcome {
    /// It merges cleanly onto what the clean items before it made.
    Clean(Box<CleanPlan>),
    /// A land with its key landed it before: the changeset that did.
    Replayed(ChangesetId),
    /// It has nothing to land, for this reason.
    NothingToLand(NothingToLandReason),
    /// It conflicts. The report's `theirs` and touches are the target's head and its chain:
    /// the hunks are the merge's with the clean items before it.
    Conflict(Box<MergeConflicts>),
}

/// A clean item's merge ([`PlanOutcome::Clean`]).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CleanPlan {
    /// The root of the tree it lands: the root its landed changeset would have.
    pub root: TreeId,
    /// The base the change was taken from.
    pub base: ChangesetId,
    /// The chain it squashes, oldest first: after the base, ending at the head.
    pub chain: Vec<ChangesetId>,
    /// What turns the tree it merged onto into `root`, sorted by path.
    pub delta: Vec<DeltaEntry>,
    /// The paths it changes, sorted, each with how: at most [`MAX_CHECK_PATHS`].
    pub paths: Vec<(RepoPath, PathChange)>,
    /// Whether it changes more paths than `paths` lists.
    pub paths_more: More,
}

impl CleanPlan {
    /// Makes one of the fields given.
    pub fn new(
        root: TreeId,
        base: ChangesetId,
        chain: Vec<ChangesetId>,
        delta: Vec<DeltaEntry>,
        paths: Vec<(RepoPath, PathChange)>,
        paths_more: More,
    ) -> CleanPlan {
        CleanPlan {
            root,
            base,
            chain,
            delta,
            paths,
            paths_more,
        }
    }
}

/// How a clean item changes one path ([`CleanPlan::paths`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PathChange {
    /// A leaf where there was none.
    Added,
    /// The leaf there changed: the change's content, or its kind.
    Modified,
    /// The leaf there is gone.
    Deleted,
    /// Both sides changed the file, and their contents merged into content neither had.
    Merged,
}

/// An item, checked: what it is before any merge.
enum Checked {
    /// On the target, or a replay: nothing to merge.
    Settled(PlanOutcome),
    /// A change to merge.
    Merge(Box<Change>),
}

/// A checked change to merge.
struct Change {
    /// Its head.
    head: StoredChangeset,
    /// Its base.
    base: StoredChangeset,
    /// The chain it squashes, oldest first.
    chain: Vec<ChangesetId>,
    /// The walk down the target from its base, which its conflict report continues.
    walk: TargetWalk,
}

/// Plans `items`, in order, onto `onto`, the head of `target` in `read` (the
/// [module documentation](self)): each clean item's tree and merged blobs kept in `staged`,
/// which the caller drops (a check) or writes.
pub(super) fn plan<O: ObjectStore>(
    repo: &Repo<O>,
    read: &RepoRead<'_, O>,
    target: &str,
    onto: &StoredChangeset,
    items: &[PlanItem],
    staged: &Staged<'_, O>,
) -> Result<Vec<Planned>, RepoError> {
    // 1. Every item checked before any merge.
    let checked = (items.iter())
        .map(|item| check(repo, read, target, onto, item))
        .collect::<Result<Vec<Checked>, RepoError>>()?;

    // 2. Each change merged onto what the clean ones before it made.
    let mut made: Option<TreeId> = None;
    let mut planned = Vec::with_capacity(items.len());
    for (item, checked) in items.iter().zip(checked) {
        let outcome = match checked {
            Checked::Settled(outcome) => outcome,
            Checked::Merge(change) => {
                let (outcome, root) = merge_one(read, onto, made, *change, staged, repo)?;
                made = root.or(made);
                outcome
            }
        };
        planned.push(Planned::new(item.head, outcome));
    }
    Ok(planned)
}

/// `item` checked as a land checks its request and its first attempt, before the merge: its
/// head and base, the head on the target, its chain, and its key's walk.
fn check<O: ObjectStore>(
    repo: &Repo<O>,
    read: &RepoRead<'_, O>,
    target: &str,
    onto: &StoredChangeset,
    item: &PlanItem,
) -> Result<Checked, RepoError> {
    // 1. The head and the base.
    let head = stored(read, item.head)?;
    let base = match item.base {
        Some(base) if read.is_ancestor(base, item.head)? => base,
        Some(base) => {
            return Err(RepoError::BaseNotAncestor {
                base,
                head: item.head,
            });
        }
        None => (read.merge_base(item.head, onto.id)?).ok_or(RepoError::NoMergeBase {
            head: item.head,
            theirs: onto.id,
        })?,
    };
    let base = stored(read, base)?;

    // 2. A head on the target's chain, then the chain the change squashes.
    if read.is_ancestor(item.head, onto.id)? {
        let nothing = PlanOutcome::NothingToLand(NothingToLandReason::OnTarget);
        return Ok(Checked::Settled(nothing));
    }
    let chain = landing::chain(repo, base.id, head.id)?;

    // 3. The key's walk: a replay, or a key reused, on the target or on another ref.
    let mut walk = TargetWalk::new(base.generation);
    if let Some(key) = &item.key {
        let reused = |landed, target: &str| RepoError::KeyReused {
            key: key.clone(),
            landed,
            target: target.to_owned(),
        };
        match walk.find_key(read, onto, key, Ours::Chain(head.id))? {
            Some(KeyFound::Replay(found)) => {
                return Ok(Checked::Settled(PlanOutcome::Replayed(found.id)));
            }
            Some(KeyFound::Reused(landed)) => return Err(reused(landed, target)),
            None => {
                if let Some((name, landed)) = walk.find_elsewhere(read, target, key, onto)? {
                    return Err(reused(landed, &name));
                }
            }
        }
    }
    Ok(Checked::Merge(Box::new(Change {
        head,
        base,
        chain,
        walk,
    })))
}

/// `change` merged onto `made`, the tree of the clean items before it, or onto `onto` when
/// there are none: its outcome, and the root it makes when clean.
fn merge_one<O: ObjectStore>(
    read: &RepoRead<'_, O>,
    onto: &StoredChangeset,
    made: Option<TreeId>,
    change: Change,
    staged: &Staged<'_, O>,
    repo: &Repo<O>,
) -> Result<(PlanOutcome, Option<TreeId>), RepoError> {
    let Change {
        head,
        base,
        chain,
        mut walk,
    } = change;

    // 1. The land's whole merge onto the target's head, or the staged re-merge onto what the
    //    items before it made, over the paths the change touches.
    let merged = match made {
        None => landing::merge_chain(repo, &base, &head, onto)?,
        Some(root) => {
            let (base_root, head_root) = (base.changeset.root, head.changeset.root);
            let all = PathFilter::all();
            let touched = tree_diff::diff_trees(read, Some(base_root), Some(head_root), &all)?;
            let paths: Vec<RepoPath> = touched.into_iter().map(|c| c.path).collect();
            let roots = [base_root, head_root, root];
            merge::merge_trees_in(staged, roots, &paths, &diff3::merge_content)?
        }
    };

    // 2. A conflict gets the land's report; a merge that changes nothing has nothing to land.
    if !merged.conflicts.is_empty() {
        let sides = Sides {
            base: base.id,
            ours: head.id,
            theirs: onto,
        };
        return match landing::conflict_report(&mut walk, read, sides, merged, Vec::new())? {
            RepoError::Conflict(conflicts) => Ok((PlanOutcome::Conflict(conflicts), None)),
            other => Err(other),
        };
    }
    let from = made.unwrap_or(onto.changeset.root);
    let MergeOutcome {
        delta,
        new_blobs,
        auto_merged,
        ..
    } = merged;
    let Some(root) = stage(staged, from, &delta, new_blobs)? else {
        let nothing = PlanOutcome::NothingToLand(NothingToLandReason::NoChanges);
        return Ok((nothing, None));
    };

    // 3. The paths it changes, from the trees staged.
    let auto_merged: BTreeSet<RepoPath> = auto_merged.into_iter().collect();
    let (paths, paths_more) = summary(staged, from, root, &auto_merged)?;
    let clean = CleanPlan::new(root, base.id, chain, delta, paths, paths_more);
    Ok((PlanOutcome::Clean(Box::new(clean)), Some(root)))
}

/// `delta`, a merge's, applied to tree `from` in `staged`, with `new_blobs`, the contents the
/// merge made: the new root, its trees and blobs kept there; `None` when it changes nothing.
fn stage<O: ObjectStore>(
    staged: &Staged<'_, O>,
    from: TreeId,
    delta: &[DeltaEntry],
    new_blobs: BTreeMap<BlobId, Vec<u8>>,
) -> Result<Option<TreeId>, RepoError> {
    // 1. The edit; an edit that changes nothing is the change the tree holds already.
    let mut edit = TreeEdit::new(staged, from)?;
    if !edit.apply(delta)?.contains(&true) {
        return Ok(None);
    }
    let Encoded { root, changed } = edit.finish()?;

    // 2. The merged contents and the changed trees, kept in memory.
    let blobs = (new_blobs.into_values()).map(|bytes| Hashed::new(ObjectKind::Blob, bytes));
    staged.put_hashed_batch(blobs.chain(changed).collect())?;
    Ok(Some(root))
}

/// The paths from tree `from` to `to` in `staged`, each with how it changed, at most
/// [`MAX_CHECK_PATHS`]; `merged`, the paths whose contents merged.
fn summary<O: ObjectStore>(
    staged: &Staged<'_, O>,
    from: TreeId,
    to: TreeId,
    merged: &BTreeSet<RepoPath>,
) -> Result<(Vec<(RepoPath, PathChange)>, More), RepoError> {
    let changes = tree_diff::diff_trees(&Trees(staged), Some(from), Some(to), &PathFilter::all())?;
    let more = if changes.len() > MAX_CHECK_PATHS {
        More::Yes
    } else {
        More::No
    };
    let mut paths = Vec::with_capacity(changes.len().min(MAX_CHECK_PATHS));
    for change in changes.into_iter().take(MAX_CHECK_PATHS) {
        let how = match change.kind {
            ChangeKind::Added { .. } => PathChange::Added,
            ChangeKind::Deleted { .. } => PathChange::Deleted,
            _ if merged.contains(&change.path) => PathChange::Merged,
            ChangeKind::Modified { .. } | ChangeKind::TypeChanged { .. } => PathChange::Modified,
            _ => return Err(RepoError::unknown_variant("ChangeKind")),
        };
        paths.push((change.path, how));
    }
    Ok((paths, more))
}

/// The trees of an object store, for a diff of trees an overlay holds.
struct Trees<'o, S: ObjectStore>(&'o S);

impl<S: ObjectStore> TreeRead for Trees<'_, S> {
    type Error = RepoError;

    fn tree(&self, id: TreeId) -> Result<Tree, RepoError> {
        repo::tree(self.0, id)
    }
}
