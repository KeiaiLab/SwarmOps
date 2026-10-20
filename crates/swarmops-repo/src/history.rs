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
//! the next, and reads no derived value. A longer one lifts along the first-parent skip list
//! ([`FirstParentSkipList`]): from a changeset's list it jumps the `2^i` steps of the highest
//! level that does not pass what it walks to, then goes on one level lower from where it landed, a
//! jump per bit of the distance at most. A list the store lacks is derived on the way, with every
//! list below it on its chain that the store lacks too; the store keeps them.
//!
//! A list read back is a cache file anyone who can write the store's files can have placed, so a
//! walk checks what it uses against the index and against the derivation's own rule, one level
//! down: entry 0 of each list it reads is the changeset's first parent (a root's list is empty),
//! and the list has no more entries than the changeset's generation leaves room for; each jump of
//! `2^i` steps lands on a changeset the snapshot holds, at least `2^i` generations lower, which
//! is entry `i - 1` of the list of entry `i - 1` (the one more list a jump reads); a lift ends
//! where no first-parent step further keeps to its target, and a list that ends before a jump the
//! walk needs has a last entry whose own list is shorter. A list that fails is
//! [`RepoError::DerivedCorrupt`], never a walk to a changeset off the chain. Lists that someone
//! made to agree with each other can pass these checks; `verify --deep` checks every list against
//! the objects.
//!
//! Every step takes the first parent from the changeset's object, hash-checked against its id,
//! never from its index record: the parents the index records are what a damaged or crafted index
//! says, and a walk that followed them could pass over committed changesets, or go round a cycle
//! the index makes up, while the objects can do neither, each naming its parents by their hashes.
//! The skip lists are derived from the objects too. The walks read the index records of the
//! changesets they start from and lift to, for their generations; [`RepoRead::merge_base`] reads
//! the record of each changeset it steps to as well, since it steps the side of the higher
//! generation.

use std::cmp::Ordering;
use std::fmt;
use std::iter::FusedIterator;

use swarmops_core::ChangesetId;
use swarmops_store::{ChangesetMeta, ObjectStore, StoreError};

use crate::derive::{Derivation, FirstParentSkipList, SkipList};
use crate::error::RepoError;
use crate::read::RepoRead;

/// The widest gap, in generations, that a walk crosses by narrow lookups, one first parent at a
/// time; a wider one lifts along the first-parent skip list ([`FirstParentSkipList`]), whose
/// first derivation writes a value file for each changeset of the chain below, so that a short
/// walk never derives a chain (M2 Decision 7). [`RepoRead::ancestor`] counts first-parent steps
/// instead of generations.
pub const SKIP_AFTER: u64 = 1024;

/// A walk from a changeset back through its first parents to a root, the changeset first, in one
/// snapshot ([`RepoRead::first_parents`]). Each item reads its changeset's object, which names the
/// next item; the start's also looks its index record up, which says whether the snapshot holds
/// it. An error ends the walk.
pub struct FirstParents<'a, O: ObjectStore> {
    read: &'a RepoRead<'a, O>,
    /// The changeset to give next; `None` once the walk has ended.
    next: Option<ChangesetId>,
    /// Whether `next` is the changeset the walk started from, which the snapshot may lack
    /// ([`RepoError::NotFound`]), rather than a first parent, which it holds: a changeset is
    /// committed after its parents.
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
/// [`RepoError::MissingObject`] for a changeset whose object the store lacks.
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
    /// iterator of [`FirstParents`], each an object read. Its first item is
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
    /// hold `cs`; [`RepoError::DerivedCorrupt`] for a skip list that fails a walk's checks ([the
    /// module documentation](self)).
    pub fn ancestor(&self, cs: ChangesetId, n: u64) -> Result<Option<ChangesetId>, RepoError> {
        self.ancestor_within(cs, n, SKIP_AFTER)
    }

    /// Whether changeset `ancestor` is on the first-parent chain of changeset `of`, `of` included:
    /// a changeset is its own ancestor, and a merge's second parent is not one. Across at most
    /// [`SKIP_AFTER`] generations it walks from `of` by narrow lookups, at most one step per
    /// generation; across more, it lifts along the first-parent skip list to the furthest of
    /// `of`'s first-parent ancestors at `ancestor`'s generation or above, which is `ancestor` if
    /// anything is. [`RepoError::NotFound`] if the snapshot does not hold either;
    /// [`RepoError::DerivedCorrupt`] for a skip list that fails a lift's checks.
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
    /// snapshot does not hold either; [`RepoError::DerivedCorrupt`] for a skip list that fails a
    /// lift's checks.
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
            for _ in 0..n {
                let Some(parent) = self.walk_step(at)? else {
                    return Ok(None);
                };
                at = parent;
            }
            return Ok(Some(at));
        }
        // A jump of `2^level` steps for each bit of `n`, from the highest: entry `level` of the
        // list of where the walk is, which a chain shorter than the jump does not have.
        let mut at = start;
        let mut left = n;
        let mut list = self.checked_list(&at)?;
        for level in (0..64_usize).rev() {
            let jump = 1_u64 << level;
            if left & jump == 0 {
                continue;
            }
            let Some(&entry) = list.get(level) else {
                self.check_end(&at, &list)?;
                return Ok(None);
            };
            let next = self.listed_record(&at, level, entry)?;
            self.check_jump(&at, &list, level, &next)?;
            at = next;
            left -= jump;
            if left == 0 {
                break;
            }
            list = self.checked_list(&at)?;
        }
        Ok(Some(at.id))
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
        let mut at = of;
        for _ in 0..gap {
            match self.walk_step(at)? {
                Some(parent) if parent == ancestor => return Ok(true),
                Some(parent) => at = parent,
                None => break,
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
        let start = from.id;
        let mut at = from;
        let mut list = self.checked_list(&at)?;
        let mut level = list.len();
        while level > 0 {
            level -= 1;
            let Some(&entry) = list.get(level) else {
                continue;
            };
            let next = self.listed_record(&at, level, entry)?;
            if next.generation < target {
                continue;
            }
            self.check_jump(&at, &list, level, &next)?;
            at = next;
            if at.generation == target || level == 0 {
                break;
            }
            list = self.checked_list(&at)?;
        }
        // The furthest such ancestor: a first-parent step further falls below `target`.
        if let Some(parent) = self.walk_parent(&at)?
            && parent.generation >= target
        {
            return Err(wrong_list(
                start,
                format!(
                    "a lift from it toward generation {target} stopped at {}, of generation {}, \
                     whose first parent is of generation {}: a list on the way lacks an entry",
                    at.id, at.generation, parent.generation
                ),
            ));
        }
        Ok(at)
    }

    /// The first-parent skip list of changeset `cs`, read back from the derived store, or derived
    /// with the lists below it that the store lacks ([`crate::Repo::derived`]).
    fn skip_list_at(&self, cs: ChangesetId) -> Result<SkipList, RepoError> {
        self.repo.derived::<FirstParentSkipList>(cs, &())
    }

    /// The skip list of the changeset whose index record is `at`, checked against the record: its
    /// entry 0 is the changeset's first parent, none for a root, and it has no more entries than
    /// the generation leaves room for (an entry `2^i` steps back needs a generation over `2^i`).
    fn checked_list(&self, at: &ChangesetMeta) -> Result<SkipList, RepoError> {
        let list = self.skip_list_at(at.id)?;
        if list.first() != at.parents.first() {
            return Err(wrong_list(at.id, "its entry 0 is not its first parent"));
        }
        let room = list.len() <= 64
            && list
                .len()
                .checked_sub(1)
                .is_none_or(|top| 1_u64 << top < at.generation);
        if !room {
            return Err(wrong_list(
                at.id,
                format!(
                    "{} entries, more than generation {} leaves room for",
                    list.len(),
                    at.generation
                ),
            ));
        }
        Ok(list)
    }

    /// The index record of `entry`, entry `level` of the skip list of the changeset whose record
    /// is `at`: one the snapshot does not hold is a wrong list, not a damaged index.
    fn listed_record(
        &self,
        at: &ChangesetMeta,
        level: usize,
        entry: ChangesetId,
    ) -> Result<ChangesetMeta, RepoError> {
        self.meta.changeset(entry)?.ok_or_else(|| {
            wrong_list(
                at.id,
                format!("its entry {level} names {entry}, which the store does not hold"),
            )
        })
    }

    /// Checks a jump along entry `level` of `list`, the skip list of the changeset whose record is
    /// `at`, to the changeset whose record is `next`: every first-parent step lowers the
    /// generation by one at least, so `next` is `2^level` generations lower at least; and entry
    /// `level` is entry `level - 1` of the list of entry `level - 1`, as the derivation makes it
    /// (one more list read).
    fn check_jump(
        &self,
        at: &ChangesetMeta,
        list: &SkipList,
        level: usize,
        next: &ChangesetMeta,
    ) -> Result<(), RepoError> {
        let lower = (next.generation)
            .checked_add(1 << level)
            .is_some_and(|reach| reach <= at.generation);
        if !lower {
            return Err(wrong_list(
                at.id,
                format!(
                    "its entry {level} names {}, of generation {}, not {} or more below its own {}",
                    next.id,
                    next.generation,
                    1_u64 << level,
                    at.generation
                ),
            ));
        }
        if let Some(&half) = level.checked_sub(1).and_then(|below| list.get(below)) {
            let vouched = self.skip_list_at(half)?.get(level - 1).copied();
            if vouched != Some(next.id) {
                return Err(wrong_list(
                    at.id,
                    format!(
                        "its entry {level} names {}, where entry {} of the list of its entry {} \
                         names {}",
                        next.id,
                        level - 1,
                        level - 1,
                        vouched.map_or_else(|| "nothing".to_owned(), |id| id.to_string())
                    ),
                ));
            }
        }
        Ok(())
    }

    /// Checks `list`, the skip list of the changeset whose record is `at`, which ends before a
    /// jump the walk needs: the chain below ends there only if the list is whole, and then its
    /// last entry, at least half as far down as the chain goes, has a shorter list of its own (a
    /// root's list is empty, which [`RepoRead::checked_list`] checked).
    fn check_end(&self, at: &ChangesetMeta, list: &SkipList) -> Result<(), RepoError> {
        let Some(&furthest) = list.last() else {
            return Ok(());
        };
        let below = self.skip_list_at(furthest)?.len();
        if below >= list.len() {
            return Err(wrong_list(
                at.id,
                format!(
                    "it ends at {} entries, where the list of its last entry has {below}",
                    list.len()
                ),
            ));
        }
        Ok(())
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

    /// The index record of the first parent of the changeset `record` is of, `None` for a root:
    /// the parent its object names ([`RepoRead::walk_step`]), not the one the record does.
    fn walk_parent(&self, record: &ChangesetMeta) -> Result<Option<ChangesetMeta>, RepoError> {
        self.walk_step(record.id)?
            .map(|parent| self.walk_record(parent))
            .transpose()
    }

    /// The first parent of changeset `id`, `None` for a root, as its object names it: the object
    /// read and hash-checked against `id` ([`RepoError::MissingObject`] if the store lacks it). A
    /// step of every first-parent walk, which therefore never reads the parents an index record
    /// names: what a damaged or crafted index says there could make a walk pass over a committed
    /// changeset, or go round a cycle. An object names its parents by their hashes, so that no
    /// chain of objects has a cycle. The snapshot holds every first parent of a changeset it holds,
    /// committed before it.
    pub(crate) fn walk_step(&self, id: ChangesetId) -> Result<Option<ChangesetId>, RepoError> {
        Ok(self.repo.changeset_object(id)?.parents.first().copied())
    }
}

/// The error of a skip list read back for changeset `cs` that a walk finds wrong, as `why` says
/// ([`RepoError::DerivedCorrupt`]).
fn wrong_list(cs: ChangesetId, why: impl Into<String>) -> RepoError {
    RepoError::DerivedCorrupt {
        name: FirstParentSkipList::NAME.to_owned(),
        changeset: cs,
        why: why.into(),
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
