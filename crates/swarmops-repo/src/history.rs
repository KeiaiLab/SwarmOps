//! First-parent ancestry (storage spec §4, the merge base), in one snapshot of the metadata: a
//! walk from a changeset back through its first parents ([`RepoRead::first_parents`]), the
//! ancestor a number of first-parent steps back ([`RepoRead::ancestor`], which a revision's `~N`
//! takes), whether one changeset is a first-parent ancestor of another
//! ([`RepoRead::is_ancestor`]), and the newest changeset two first-parent chains share
//! ([`RepoRead::merge_base`]). History is first-parent, as `log` is: a merge's other parents are
//! not walked.
//!
//! A changeset's generation is 1 + the largest of its parents' (1 for a root), so generations
//! fall strictly along first parents, and a walk prunes by them: a changeset `g` generations below
//! another is at most `g` first-parent steps below it, if it is on its chain at all, and one of
//! generation `g` is at most `g - 1` first-parent steps above a root. A walk across at most
//! [`SKIP_AFTER`] generations (for [`RepoRead::ancestor`], steps) steps from one first parent to
//! the next by narrow index lookups ([`MetaRead::first_parent`]), and reads no derived value. A
//! longer one lifts along the first-parent skip list ([`FirstParentSkipList`]): from a changeset's
//! list it jumps the `2^i` steps of the highest level that does not pass what it walks to, then
//! goes on one level lower from where it landed, so that it reads one list per jump and at most
//! one per bit of the distance. A list the store lacks is derived on the way, with every list
//! below it on its chain that the store lacks too; the store keeps them.
//!
//! The walks read the index records of the changesets they start from and lift to, for their
//! generations; [`RepoRead::merge_base`] reads the record of each changeset it steps to as well,
//! since it steps the side of the higher generation.
//!
//! [`MetaRead::first_parent`]: swarmops_store::MetaRead::first_parent

use std::cmp::Ordering;
use std::fmt;
use std::iter::FusedIterator;

use swarmops_core::ChangesetId;
use swarmops_store::{ChangesetMeta, ObjectStore, StoreError};

use crate::derive::{FirstParentSkipList, SkipList};
use crate::error::RepoError;
use crate::read::RepoRead;

/// The widest gap, in generations, that a walk crosses by narrow lookups, one first parent at a
/// time; a wider one lifts along the first-parent skip list ([`FirstParentSkipList`]), whose
/// first derivation writes a value file for each changeset of the chain below, so that a short
/// walk never derives a chain (M2 Decision 7). [`RepoRead::ancestor`] counts first-parent steps
/// instead of generations.
pub const SKIP_AFTER: u64 = 1024;

/// A walk from a changeset back through its first parents to a root, the changeset first, in one
/// snapshot ([`RepoRead::first_parents`]). Each item is one lookup, which also finds the next
/// item: the start's reads its index record, which says whether the snapshot holds it, and every
/// first parent's is a narrow lookup. An error ends the walk.
pub struct FirstParents<'a, O: ObjectStore> {
    read: &'a RepoRead<'a, O>,
    /// The changeset to give next; `None` once the walk has ended.
    next: Option<ChangesetId>,
    /// Whether `next` is the changeset the walk started from, which the snapshot may lack
    /// ([`RepoError::NotFound`]), rather than a first parent, which it holds unless its index is
    /// damaged.
    start: bool,
}

impl<O: ObjectStore> fmt::Debug for FirstParents<'_, O> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FirstParents")
            .field("next", &self.next)
            .field("start", &self.start)
            .finish_non_exhaustive()
    }
}

/// The changesets of the walk, each once, `Ok` until an error, which is the last item:
/// [`RepoError::NotFound`] if the snapshot does not hold the changeset the walk started from,
/// [`StoreError::MissingParent`] for a first parent it lacks (a damaged index).
impl<O: ObjectStore> Iterator for FirstParents<'_, O> {
    type Item = Result<ChangesetId, RepoError>;

    fn next(&mut self) -> Option<Self::Item> {
        let id = self.next.take()?;
        let parent = if std::mem::take(&mut self.start) {
            self.read.first_parent(id)
        } else {
            self.read.walk_step(id)
        };
        Some(parent.map(|parent| {
            self.next = parent;
            id
        }))
    }
}

impl<O: ObjectStore> FusedIterator for FirstParents<'_, O> {}

impl<O: ObjectStore> RepoRead<'_, O> {
    /// A walk from changeset `from` back through its first parents to a root, `from` first: an
    /// iterator of [`FirstParents`], each a narrow lookup. Its first item is
    /// [`RepoError::NotFound`] if the snapshot does not hold `from`.
    pub fn first_parents(&self, from: ChangesetId) -> FirstParents<'_, O> {
        FirstParents {
            read: self,
            next: Some(from),
            start: true,
        }
    }

    /// The changeset `n` first-parent steps back from changeset `cs` (`cs` itself for 0), or
    /// `None` if a root comes first. Up to [`SKIP_AFTER`] steps it walks by narrow lookups;
    /// further, it lifts along the first-parent skip list, a jump per bit of `n`. A `cs` of
    /// generation `g` has at most `g - 1` first-parent steps above a root, so an `n` of `g` or
    /// more is `None` from `cs`'s record alone. [`RepoError::NotFound`] if the snapshot does not
    /// hold `cs`.
    pub fn ancestor(&self, cs: ChangesetId, n: u64) -> Result<Option<ChangesetId>, RepoError> {
        self.ancestor_within(cs, n, SKIP_AFTER)
    }

    /// Whether changeset `ancestor` is on the first-parent chain of changeset `of`, `of` included:
    /// a changeset is its own ancestor, and a merge's second parent is not one. Across at most
    /// [`SKIP_AFTER`] generations it walks from `of` by narrow lookups, at most one step per
    /// generation; across more, it lifts along the first-parent skip list to the furthest of
    /// `of`'s first-parent ancestors at `ancestor`'s generation or above, which is `ancestor` if
    /// anything is. [`RepoError::NotFound`] if the snapshot does not hold either.
    pub fn is_ancestor(&self, ancestor: ChangesetId, of: ChangesetId) -> Result<bool, RepoError> {
        self.is_ancestor_within(ancestor, of, SKIP_AFTER)
    }

    /// The newest changeset on the first-parent chains of both changesets `a` and `b` (either one
    /// when it is on the other's chain), or `None` when their chains reach different roots. It
    /// walks the side of the higher generation down, and both when they are level, by one first
    /// parent at a time while the two are at most [`SKIP_AFTER`] generations apart, and lifting
    /// along the first-parent skip list to the other's generation when they are further; no step
    /// passes the merge base, which is at or below the lower generation. Each step reads the index
    /// record of the changeset it reaches, for its generation. [`RepoError::NotFound`] if the
    /// snapshot does not hold either.
    pub fn merge_base(
        &self,
        a: ChangesetId,
        b: ChangesetId,
    ) -> Result<Option<ChangesetId>, RepoError> {
        self.merge_base_within(a, b, SKIP_AFTER)
    }

    /// [`RepoRead::ancestor`], lifting along the skip list past `skip_after` steps.
    fn ancestor_within(
        &self,
        cs: ChangesetId,
        n: u64,
        skip_after: u64,
    ) -> Result<Option<ChangesetId>, RepoError> {
        let start = self.walk_start(cs)?;
        if n == 0 {
            return Ok(Some(cs));
        }
        if n >= start.generation {
            return Ok(None);
        }
        if n <= skip_after {
            let mut at = cs;
            let mut parent = start.parents.first().copied();
            for step in 1..=n {
                let Some(next) = parent else {
                    return Ok(None);
                };
                at = next;
                if step < n {
                    parent = self.walk_step(at)?;
                }
            }
            return Ok(Some(at));
        }
        // A jump of `2^level` steps for each bit of `n`, from the highest: entry `level` of the
        // list of where the walk is, which a chain shorter than the jump does not have.
        let mut at = cs;
        let mut left = n;
        let mut list = self.skip_list_at(at)?;
        for level in (0..64_usize).rev() {
            let jump = 1_u64 << level;
            if left & jump == 0 {
                continue;
            }
            let Some(&next) = list.get(level) else {
                return Ok(None);
            };
            at = next;
            left -= jump;
            if left == 0 {
                break;
            }
            list = self.skip_list_at(at)?;
        }
        Ok(Some(at))
    }

    /// [`RepoRead::is_ancestor`], lifting along the skip list across more than `skip_after`
    /// generations.
    fn is_ancestor_within(
        &self,
        ancestor: ChangesetId,
        of: ChangesetId,
        skip_after: u64,
    ) -> Result<bool, RepoError> {
        let target = self.walk_start(ancestor)?.generation;
        let from = self.walk_start(of)?;
        if ancestor == of {
            return Ok(true);
        }
        if target >= from.generation {
            return Ok(false);
        }
        let gap = from.generation - target;
        if gap > skip_after {
            return Ok(self.lift_toward(from, target)?.id == ancestor);
        }
        // `ancestor`, if on `of`'s chain, is at most `gap` first-parent steps down it.
        let mut parent = from.parents.first().copied();
        for step in 1..=gap {
            match parent {
                Some(at) if at == ancestor => return Ok(true),
                Some(at) if step < gap => parent = self.walk_step(at)?,
                _ => break,
            }
        }
        Ok(false)
    }

    /// [`RepoRead::merge_base`], lifting along the skip list across more than `skip_after`
    /// generations.
    fn merge_base_within(
        &self,
        a: ChangesetId,
        b: ChangesetId,
        skip_after: u64,
    ) -> Result<Option<ChangesetId>, RepoError> {
        let mut x = self.walk_start(a)?;
        let mut y = self.walk_start(b)?;
        loop {
            if x.id == y.id {
                return Ok(Some(x.id));
            }
            // A changeset above the other side's generation is not the merge base, which is a
            // first-parent ancestor of both, so at or below both generations; and two different
            // changesets of one generation are not either, as neither is the other's ancestor.
            match x.generation.cmp(&y.generation) {
                Ordering::Greater => match self.descend_to(x, y.generation, skip_after)? {
                    Some(lower) => x = lower,
                    None => return Ok(None),
                },
                Ordering::Less => match self.descend_to(y, x.generation, skip_after)? {
                    Some(lower) => y = lower,
                    None => return Ok(None),
                },
                Ordering::Equal => match (self.walk_parent(&x)?, self.walk_parent(&y)?) {
                    (Some(px), Some(py)) => (x, y) = (px, py),
                    _ => return Ok(None),
                },
            }
        }
    }

    /// One move of [`RepoRead::merge_base`]'s walk for changeset `at`, whose generation is above
    /// `target`, the other side's: more than `skip_after` generations above it, a lift to the
    /// furthest first-parent ancestor at `target` or above; then, if still above `target`, one
    /// step to the first parent. `None` past a root.
    fn descend_to(
        &self,
        at: ChangesetMeta,
        target: u64,
        skip_after: u64,
    ) -> Result<Option<ChangesetMeta>, RepoError> {
        let at = if at.generation - target > skip_after {
            self.lift_toward(at, target)?
        } else {
            at
        };
        if at.generation > target {
            self.walk_parent(&at)
        } else {
            Ok(Some(at))
        }
    }

    /// The record of the furthest first-parent ancestor of `from` (`from` included) whose
    /// generation is `target` or above, `from`'s being so: lifted along the skip list from the
    /// highest level of `from`'s list down, a level at a time, each jump to the entry of that
    /// level of where the walk is if that changeset's generation is `target` or above. Generations
    /// fall strictly along the chain, so a jump that would pass the furthest such ancestor lands
    /// below `target`, and is not made; the jumps made add up to the distance, a jump per bit.
    fn lift_toward(&self, from: ChangesetMeta, target: u64) -> Result<ChangesetMeta, RepoError> {
        let mut at = from;
        let mut list = self.skip_list_at(at.id)?;
        let mut level = list.len();
        while level > 0 {
            level -= 1;
            let Some(&next) = list.get(level) else {
                continue;
            };
            let next = self.walk_record(next)?;
            if next.generation < target {
                continue;
            }
            at = next;
            if at.generation == target || level == 0 {
                break;
            }
            list = self.skip_list_at(at.id)?;
        }
        Ok(at)
    }

    /// The first-parent skip list of changeset `cs`, read back from the derived store, or derived
    /// with the lists below it that the store lacks ([`crate::Repo::derived`]).
    fn skip_list_at(&self, cs: ChangesetId) -> Result<SkipList, RepoError> {
        self.repo.derived::<FirstParentSkipList>(cs, &())
    }

    /// The index record of changeset `id`, where a walk starts: [`RepoError::NotFound`] if the
    /// snapshot does not hold it.
    fn walk_start(&self, id: ChangesetId) -> Result<ChangesetMeta, RepoError> {
        self.meta
            .changeset(id)?
            .ok_or_else(|| RepoError::NotFound(id.to_string()))
    }

    /// The index record of changeset `id`, a first-parent ancestor of a changeset the snapshot
    /// holds: one it lacks is a damaged index.
    fn walk_record(&self, id: ChangesetId) -> Result<ChangesetMeta, RepoError> {
        Ok(self
            .meta
            .changeset(id)?
            .ok_or(StoreError::MissingParent(id))?)
    }

    /// The index record of the first parent of the changeset `record` is of, `None` for a root.
    fn walk_parent(&self, record: &ChangesetMeta) -> Result<Option<ChangesetMeta>, RepoError> {
        record
            .parents
            .first()
            .map(|&parent| self.walk_record(parent))
            .transpose()
    }

    /// The first parent of changeset `id` (`None` for a root), a first-parent ancestor of a
    /// changeset the snapshot holds: a narrow lookup ([`swarmops_store::MetaRead::first_parent`]),
    /// and one the snapshot lacks is a damaged index.
    fn walk_step(&self, id: ChangesetId) -> Result<Option<ChangesetId>, RepoError> {
        Ok(self
            .meta
            .first_parent(id)?
            .ok_or(StoreError::MissingParent(id))?)
    }
}

/// The walks with a threshold of the caller's in place of [`SKIP_AFTER`], in a build with the
/// `test-hooks` feature only: so that the tests lift along the skip list across the short gaps of
/// small histories.
#[cfg(feature = "test-hooks")]
impl<O: ObjectStore> RepoRead<'_, O> {
    /// [`RepoRead::ancestor`], lifting along the skip list past `skip_after` steps.
    pub fn ancestor_with_skip_after(
        &self,
        cs: ChangesetId,
        n: u64,
        skip_after: u64,
    ) -> Result<Option<ChangesetId>, RepoError> {
        self.ancestor_within(cs, n, skip_after)
    }

    /// [`RepoRead::is_ancestor`], lifting along the skip list across more than `skip_after`
    /// generations.
    pub fn is_ancestor_with_skip_after(
        &self,
        ancestor: ChangesetId,
        of: ChangesetId,
        skip_after: u64,
    ) -> Result<bool, RepoError> {
        self.is_ancestor_within(ancestor, of, skip_after)
    }

    /// [`RepoRead::merge_base`], lifting along the skip list across more than `skip_after`
    /// generations.
    pub fn merge_base_with_skip_after(
        &self,
        a: ChangesetId,
        b: ChangesetId,
        skip_after: u64,
    ) -> Result<Option<ChangesetId>, RepoError> {
        self.merge_base_within(a, b, skip_after)
    }
}
