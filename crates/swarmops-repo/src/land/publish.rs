//! A land's publish (operations spec §3 step (f)): the changeset it made, `L`, inserted and the
//! target moved to it in one write transaction, one journal frame, by the landing model the M4
//! measurement chose (umbrella decision 10: metalog merge).
//!
//! Under the lock the publish reads the target again (`MetaWrite::get_ref`). At the head `L` was
//! made on, `onto`, it commits `L` by compare-and-swap. Moved, to `actual`, the lock holder makes
//! the land anew there when the rule allows it ([`Relocate::Allow`]):
//!
//! ```text
//!   onto ── g1 ── g2 ── actual        what the target gained: the deltas' paths of g1 .. actual
//!     \                    \
//!      L                    L'        L's delta on actual's tree; one parent, actual
//! ```
//!
//! 1. It walks first parents from `actual` back to `onto` with `MetaWrite::changeset`, at most
//!    [`MAX_WALK`] steps; past them, or with `onto` off that line (a move back, or to another
//!    line), the land goes back to its lander ([`Published::Moved`]).
//! 2. It gathers the paths the walked changesets' deltas touch. A path of `L`'s delta overlaps
//!    one of them when the two are equal or one is a directory prefix of the other ([`overlaps`]).
//! 3. Each overlapping path is merged three-way (diff3) from `onto`'s tree, `L`'s and `actual`'s
//!    when it is a regular file on all three, of at most [`MAX_MERGE_BYTES`] each; otherwise, or
//!    when the merge conflicts, the land goes back to its lander.
//! 4. `L'`, the paths that did not overlap as `L` has them and the merged ones, on `actual`'s
//!    tree, is written and flushed under the lock, before the frame that names it (I1); then its
//!    insert and the target's move from `actual` to it are committed as one frame. A change
//!    `actual` holds already goes back to the lander, whose re-merge finds nothing to land.
//!
//! `L'` keeps `L`'s author, committer, message and metadata (`land_key`, `squashed_from`,
//! `lander`). With no overlapping path its delta is `L`'s, and so is its delta hash. A land with
//! an expected target ([`Relocate::Never`], M5) is never made anew: a moved target goes back as
//! [`Published::Moved`], so that what lands is the tree its lander checked.

use std::collections::{BTreeMap, BTreeSet};

use swarmops_core::{
    Changeset, ChangesetId, DeltaEntry, DeltaOp, EntryTarget, Hashed, LeafKind, ObjectKind,
    RepoPath, Tree, TreeId,
};
use swarmops_merge::diff3;
use swarmops_merge::tree_diff::TreeRead;
use swarmops_merge::tree_merge;
use swarmops_store::{
    CasResult, ChangesetMeta, MetaWrite, ObjectStat, ObjectStore, Recovered, StoreError,
};

use crate::commit::{Draft, WrittenChangeset};
use crate::error::RepoError;
use crate::repo::{self, Repo};

/// The most first-parent steps a publish walks from the target's head back to a land's `onto`:
/// past them, the land goes back to its lander.
const MAX_WALK: usize = 256;

/// The most bytes the lock holder merges, on each side of a file: past them, the land goes back
/// to its lander.
const MAX_MERGE_BYTES: u64 = 1 << 20;

/// Whether a publish may make the land anew on a target that moved since the land read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Relocate {
    /// By the landing model's rule ([`crate::land`], step 6).
    Allow,
    /// Never: a moved target goes back to the lander as [`Published::Moved`]. A land with an
    /// expected target publishes so.
    Never,
}

/// A land's changeset, written and flushed, to publish.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Publication {
    /// The ref it lands on.
    pub target: String,
    /// The target's head it was made on, its one parent: what the publish expects the target to
    /// name.
    pub onto: ChangesetId,
    /// Its id.
    pub id: ChangesetId,
    /// The changeset, whose objects are durable.
    pub changeset: Changeset,
    /// Who moves the target, as the reflog names them: at most 255 bytes.
    pub actor: String,
    /// Whether the lock holder may make it anew on a target that moved.
    pub relocate: Relocate,
}

/// What a publish did.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Published {
    /// The target names `id` now, on `onto`: the changeset and the target's move to it are
    /// committed in one frame.
    Landed {
        /// The changeset the target names: the publication's, or one the lock holder made from it
        /// on the head the target had moved to.
        id: ChangesetId,
        /// Its parent: the head it landed on.
        onto: ChangesetId,
        /// Whether the lock holder made it anew on a head the target had moved to.
        relocated: bool,
        /// The paths the lock holder merged under the lock into new content, sorted.
        merged_in_lock: Vec<RepoPath>,
        /// How many paths its delta changes.
        files_changed: usize,
        /// Steps after the commit point that failed, one line each: the frame is committed all
        /// the same.
        warnings: Vec<String>,
    },
    /// The target names `actual`, and the publication was not made anew there: nothing was
    /// recorded.
    Moved {
        /// What the target names.
        actual: ChangesetId,
        /// What the write transaction's recovery repaired before the move was refused, of what
        /// earlier writers left: those repairs stay made.
        recovered: Recovered,
    },
}

/// The changeset a publish records, and how it was made.
struct Frame<'a> {
    /// Its id.
    id: ChangesetId,
    /// The changeset.
    changeset: &'a Changeset,
    /// The head it is made on, which the target must name.
    onto: ChangesetId,
    /// Whether the lock holder made it.
    relocated: bool,
    /// The paths the lock holder merged into new content.
    merged_in_lock: Vec<RepoPath>,
}

/// `L` made anew by the lock holder on the target's head ([`Repo::relocated`]).
struct Relocated {
    /// The new changeset, written and flushed.
    written: WrittenChangeset,
    /// The paths merged into new content.
    merged: Vec<RepoPath>,
}

impl<O: ObjectStore> Repo<O> {
    /// Publishes `p` in one write transaction, by [the module documentation](self)'s rule:
    /// queues the changeset it lands (found there already, it queues nothing) and the move of
    /// its target to it, by compare-and-swap, and commits both as one frame. A target that moved
    /// where the rule does not follow drops the write, which appends nothing:
    /// [`Published::Moved`]. A target that does not exist is [`RepoError::NotFound`]; a parent
    /// the store no longer holds (a frame a failed sync took back), [`RepoError::UnknownParent`].
    /// A failure after the commit point is a warning; a journal append that fails
    /// ([`StoreError::NotCommitted`]) is returned as it is.
    pub(crate) fn publish(&self, p: &Publication) -> Result<Published, RepoError> {
        let write = self.meta.write()?;

        // 1. The target, read under the lock: at `onto`, or moved and made anew there.
        if p.relocate == Relocate::Allow {
            let actual = write
                .get_ref(&p.target)
                .map_err(repo::ref_error)?
                .ok_or_else(|| RepoError::NotFound(p.target.clone()))?;
            if actual != p.onto {
                let Some(made) = self.relocated(&write, &p.changeset, p.onto, actual)? else {
                    let recovered = write.recovered();
                    return Ok(Published::Moved { actual, recovered });
                };
                let frame = Frame {
                    id: made.written.id,
                    changeset: &made.written.changeset,
                    onto: actual,
                    relocated: true,
                    merged_in_lock: made.merged,
                };
                return record(write, p, frame);
            }
        }

        // 2. `L` itself, by compare-and-swap from `onto`.
        let frame = Frame {
            id: p.id,
            changeset: &p.changeset,
            onto: p.onto,
            relocated: false,
            merged_in_lock: Vec::new(),
        };
        record(write, p, frame)
    }

    /// `l`, made on `onto`, made anew on `actual`, the target's head, under the lock `write`
    /// holds (steps 1 to 4 of [the module documentation](self)): written and flushed, not
    /// queued. `None` when the rule gives the land back to its lander.
    fn relocated(
        &self,
        write: &MetaWrite<'_>,
        l: &Changeset,
        onto: ChangesetId,
        actual: ChangesetId,
    ) -> Result<Option<Relocated>, RepoError> {
        // 1. What the target gained since `onto`.
        let Some(gained) = gained_since(write, onto, actual)? else {
            return Ok(None);
        };
        let mut paths = BTreeSet::new();
        for id in gained {
            for entry in self.changeset_object(id)?.delta {
                paths.insert(entry.path.as_str().to_owned());
            }
        }

        // 2. `L`'s delta split: the paths that overlap what the target gained, and the rest.
        let (overlapping, mut changes): (Vec<DeltaEntry>, Vec<DeltaEntry>) = (l.delta.iter())
            .cloned()
            .partition(|entry| overlaps_any(&entry.path, &paths));

        // 3. The overlapping paths merged three-way, each a small file on every side.
        let actual_root = self.changeset_object(actual)?.root;
        let mut contents = BTreeMap::new();
        let mut merged = Vec::new();
        if !overlapping.is_empty() {
            let onto_root = self.changeset_object(onto)?.root;
            let trees = Trees(&self.objects);
            for entry in &overlapping {
                if !self.mergeable(&trees, [onto_root, actual_root], entry)? {
                    return Ok(None);
                }
            }
            let at: Vec<RepoPath> = overlapping.into_iter().map(|entry| entry.path).collect();
            let merge =
                self.merge_trees_with(onto_root, l.root, actual_root, &at, &diff3::merge_content)?;
            if !merge.conflicts.is_empty() {
                return Ok(None);
            }
            changes.extend(merge.delta);
            changes.sort_by(|a, b| a.path.cmp(&b.path));
            for bytes in merge.new_blobs.into_values() {
                let blob = Hashed::new(ObjectKind::Blob, bytes);
                contents.insert(blob.id(), blob);
            }
            merged = merge.auto_merged;
        }

        // 4. `L'` on `actual`, its objects flushed before any frame names them (I1).
        let draft = Draft {
            onto: Some(actual_root),
            changes,
            contents,
            parents: vec![actual],
            author: l.author.clone(),
            committer: l.committer.clone(),
            message: l.message.clone(),
            metadata: l.metadata.clone(),
            allow_empty: false,
        };
        match self.write_changeset(draft) {
            Ok(written) => Ok(Some(Relocated { written, merged })),
            Err(RepoError::EmptyChanges) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Whether the lock holder may merge `entry`, a path of `L`'s delta: `L` puts a regular file
    /// there, and the trees `[onto, actual]` hold one, each side at most [`MAX_MERGE_BYTES`].
    fn mergeable(
        &self,
        trees: &Trees<'_, O>,
        roots: [TreeId; 2],
        entry: &DeltaEntry,
    ) -> Result<bool, RepoError> {
        let DeltaOp::Put {
            kind: LeafKind::File,
            id,
        } = entry.op
        else {
            return Ok(false);
        };
        let mut blobs = vec![id];
        for root in roots {
            match tree_merge::entry_in(trees, root, &entry.path)? {
                Some(EntryTarget::Leaf(LeafKind::File, blob)) => blobs.push(blob),
                _ => return Ok(false),
            }
        }
        for blob in blobs {
            let small = matches!(
                self.objects.stat(blob.0)?,
                Some(ObjectStat { kind: ObjectKind::Blob, size, .. }) if size <= MAX_MERGE_BYTES
            );
            if !small {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

/// Queues `frame`'s changeset and the move of `p`'s target from `frame.onto` to it in `write`,
/// and commits both: [`Published::Landed`]; or, the target elsewhere, drops the write:
/// [`Published::Moved`].
fn record(
    mut write: MetaWrite<'_>,
    p: &Publication,
    frame: Frame<'_>,
) -> Result<Published, RepoError> {
    // 1. The insert, and the target's compare-and-swap.
    let changeset = frame.changeset;
    let meta = ChangesetMeta::new(
        frame.id,
        0,
        0,
        changeset.delta_hash()?,
        changeset.parents.clone(),
    );
    write.insert_changeset(meta).map_err(|e| match e {
        StoreError::MissingParent(parent) => RepoError::UnknownParent(parent),
        other => other.into(),
    })?;
    match write
        .cas_ref(&p.target, Some(frame.onto), Some(frame.id), &p.actor)
        .map_err(repo::ref_error)?
    {
        CasResult::Applied { .. } => {}
        // Returning drops the write: the queued insert with it, no frame, the lock released.
        CasResult::Mismatch {
            actual: Some(actual),
        } => {
            let recovered = write.recovered();
            return Ok(Published::Moved { actual, recovered });
        }
        CasResult::Mismatch { actual: None } => {
            return Err(RepoError::NotFound(p.target.clone()));
        }
        _ => return Err(RepoError::unknown_variant("CasResult")),
    }

    // 2. The frame; a failure past its commit point is a warning.
    let mut warnings = Vec::new();
    match write.commit() {
        Ok(_) => {}
        Err(StoreError::PostCommit {
            frame_seq, source, ..
        }) => {
            let what = format!(
                "the landed changeset {} and the move of ref {}",
                frame.id, p.target
            );
            warnings.push(repo::post_commit_warning(&what, frame_seq, &source));
        }
        Err(e) => return Err(e.into()),
    }
    Ok(Published::Landed {
        id: frame.id,
        onto: frame.onto,
        relocated: frame.relocated,
        merged_in_lock: frame.merged_in_lock,
        files_changed: changeset.delta.len(),
        warnings,
    })
}

/// The changesets on `actual`'s first-parent line down to `onto`, `actual` first and `onto`
/// left out, as `write` sees them; `None` past [`MAX_WALK`] of them, or when `onto` is not on
/// that line (a changeset at or below `onto`'s generation that is not `onto`, or a root), or
/// not in the index.
fn gained_since(
    write: &MetaWrite<'_>,
    onto: ChangesetId,
    actual: ChangesetId,
) -> Result<Option<Vec<ChangesetId>>, RepoError> {
    let Some(onto) = write.changeset(onto)? else {
        return Ok(None);
    };
    let mut gained = Vec::new();
    let mut at = actual;
    while at != onto.id {
        if gained.len() == MAX_WALK {
            return Ok(None);
        }
        let meta = write.changeset(at)?.ok_or(StoreError::MissingParent(at))?;

        // Generations fall by at least one per first-parent step.
        if meta.generation <= onto.generation {
            return Ok(None);
        }
        gained.push(at);
        match meta.parents.first() {
            Some(&parent) => at = parent,
            None => return Ok(None),
        }
    }
    Ok(Some(gained))
}

/// Whether `a` and `b` overlap: they are equal, or one is a directory prefix of the other
/// (`a` and `a/b`; not `a/b` and `a/bc`).
pub(crate) fn overlaps(a: &RepoPath, b: &RepoPath) -> bool {
    let below = |path: &str, dir: &str| {
        path.strip_prefix(dir)
            .is_some_and(|rest| rest.starts_with('/'))
    };
    let (a, b) = (a.as_str(), b.as_str());
    a == b || below(a, b) || below(b, a)
}

/// Whether `path` overlaps one of `gained` ([`overlaps`]): the candidates are `path` itself, the
/// directories above it, and the first path below it, found in the sorted set without a scan.
fn overlaps_any(path: &RepoPath, gained: &BTreeSet<String>) -> bool {
    // 1. `path`, or a directory above it.
    let mut at = Some(path.clone());
    while let Some(candidate) = at {
        if gained.contains(candidate.as_str()) {
            return true;
        }
        at = candidate.parent();
    }

    // 2. A path below it: all of them sort together, from `path/` on.
    let below = format!("{}/", path.as_str());
    (gained.range(below..).next())
        .and_then(|next| RepoPath::new(next.as_str()).ok())
        .is_some_and(|next| overlaps(path, &next))
}

/// The trees of a store, read for [`tree_merge::entry_in`].
struct Trees<'o, O: ObjectStore>(&'o O);

impl<O: ObjectStore> TreeRead for Trees<'_, O> {
    type Error = RepoError;

    fn tree(&self, id: TreeId) -> Result<Tree, RepoError> {
        repo::tree(self.0, id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `a` and `b` as repository paths.
    fn pair(a: &str, b: &str) -> (RepoPath, RepoPath) {
        (RepoPath::new(a).unwrap(), RepoPath::new(b).unwrap())
    }

    #[test]
    fn overlap_is_equality_or_a_directory_prefix() {
        for (a, b) in [("a/b", "a/b"), ("a", "a/b"), ("a/b", "a")] {
            let (a, b) = pair(a, b);
            assert!(overlaps(&a, &b), "{a:?} and {b:?} overlap");
        }
        for (a, b) in [
            ("a/b", "a/bc"),
            ("a", "ab"),
            ("a/bc", "a/b"),
            ("a/b", "a/c"),
        ] {
            let (a, b) = pair(a, b);
            assert!(!overlaps(&a, &b), "{a:?} and {b:?} do not overlap");
        }
    }

    #[test]
    fn a_path_overlaps_the_set_through_a_parent_or_a_child() {
        let gained: BTreeSet<String> = ["a/bc", "d/e/f", "g"].map(str::to_owned).into();
        let overlapping = |p: &str| overlaps_any(&RepoPath::new(p).unwrap(), &gained);
        assert!(overlapping("d"), "a child");
        assert!(overlapping("d/e"), "a grandchild");
        assert!(overlapping("g/h"), "a parent");
        assert!(overlapping("a/bc"), "equal");
        assert!(!overlapping("a/b"), "a sibling with a shared string prefix");
        assert!(!overlapping("a/b/c"), "below a sibling");
        assert!(!overlapping("d/e/fg"), "beside a gained path");
    }
}
