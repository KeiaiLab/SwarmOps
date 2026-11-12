//! All-parent ancestry (the M16 design §5; storage spec §4's merge base, all-parent form): for
//! two changesets `from` and `to`, whether `from` is an ancestor of `to`, their merge bases (the
//! common ancestors no other common ancestor reaches), and the changesets `to` reaches and
//! `from` does not (`from..to`), a page at a time. Every parent is followed, a merge's other
//! parents too; first-parent reads (log, history, land, rebase, the forward-only check) are
//! [`history`](crate::history)'s and do not change. The same walk from several `from`s gives the
//! changesets a store lacks of another's history ([`RepoRead::missing`], the native sync design
//! §2.4).
//!
//! One walk gives the three answers. It pops changesets from a queue ordered by generation,
//! highest first, ties by id, so that every child of a changeset is popped before it and each
//! changeset carries its final marks when popped:
//!
//! ```text
//!   to ──▶ TO          from ──▶ FROM
//!          │  parents inherit the marks
//!          ▼
//!   TO only            → an entry of from..to
//!   FROM and TO, live  → a merge base; it marks its parents STALE
//!   STALE              → below a merge base: nothing more to find there
//! ```
//!
//! The walk ends when no queued changeset carries TO without STALE: nothing left can be an
//! entry or a merge base. Answers come in the queue's order, which is the order the operation
//! documents. It reads each changeset's parents from its object, hash-checked against its id, and
//! their index records for their generations: a generation that is not 1 + the largest of its
//! parents' is a damaged index ([`StoreError::IndexCorrupt`]), so that no damaged `gen.col`
//! orders the walk wrong. It reads only: no frame, no lock.
//!
//! [`StoreError::IndexCorrupt`]: swarmops_store::StoreError::IndexCorrupt

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};

use swarmops_core::ChangesetId;
use swarmops_store::{ChangesetMeta, ObjectStore};

use crate::error::RepoError;
use crate::history::wrong_generation;
use crate::read::{RepoRead, StoredChangeset};

/// The most merge bases an answer lists ([`Ancestry::merge_bases_truncated`] says when there
/// are more).
pub const MAX_MERGE_BASES: usize = 64;

/// What [`RepoRead::ancestry`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Ancestry {
    /// Whether `from` is an ancestor of `to` along any parents, `to` included.
    pub is_ancestor: bool,
    /// The first [`MAX_MERGE_BASES`] merge bases, by generation descending, then id ascending.
    pub merge_bases: Vec<ChangesetId>,
    /// Whether there are more merge bases than [`Ancestry::merge_bases`] lists.
    pub merge_bases_truncated: bool,
    /// A page of `from..to`, in the same order, after the walk's `after`.
    pub entries: Vec<StoredChangeset>,
    /// Whether more of `from..to` follows the page's last entry.
    pub more: bool,
}

impl Ancestry {
    /// Takes the two answers; the page is empty.
    pub fn new(is_ancestor: bool, merge_bases: Vec<ChangesetId>) -> Ancestry {
        Ancestry {
            is_ancestor,
            merge_bases,
            merge_bases_truncated: false,
            entries: Vec::new(),
            more: false,
        }
    }
}

/// A page of the changesets a `want` reaches and no `have` does ([`RepoRead::missing`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Missing {
    /// The page, generation descending, then id ascending.
    pub changesets: Vec<ChangesetId>,
    /// The page's last changeset, its generation and its id, when more follow: the next
    /// page's `after`.
    pub next: Option<(u64, ChangesetId)>,
}

impl Missing {
    /// Takes every field.
    pub fn new(changesets: Vec<ChangesetId>, next: Option<(u64, ChangesetId)>) -> Missing {
        Missing { changesets, next }
    }
}

/// The marks a changeset carries in the walk.
#[derive(Debug, Clone, Copy, Default)]
struct Marks(u8);

impl Marks {
    /// Reached from `from`.
    const FROM: u8 = 1;
    /// Reached from `to`.
    const TO: u8 = 2;
    /// Reached from a merge base.
    const STALE: u8 = 4;
    /// Popped: its marks are final.
    const POPPED: u8 = 8;

    fn has(self, mark: u8) -> bool {
        self.0 & mark == mark
    }

    /// Whether the changeset can still give an entry or a merge base.
    fn live(self) -> bool {
        self.has(Self::TO) && !self.has(Self::STALE) && !self.has(Self::POPPED)
    }
}

/// A queued changeset: generation highest first, then id lowest first.
type Queued = (u64, Reverse<ChangesetId>);

/// The walk's state: the queue, each seen changeset's marks, and how many queued ones are live.
struct Walk {
    queue: BinaryHeap<Queued>,
    marks: HashMap<ChangesetId, Marks>,
    live: usize,
}

impl Walk {
    /// Adds `mark` to changeset `id` of generation `generation`, queueing it when first seen.
    fn mark(&mut self, id: ChangesetId, generation: u64, mark: u8) {
        let marks = self.marks.entry(id).or_insert_with(|| {
            self.queue.push((generation, Reverse(id)));
            Marks::default()
        });
        let was = marks.live();
        marks.0 |= mark;
        match (was, marks.live()) {
            (false, true) => self.live += 1,
            (true, false) => self.live -= 1,
            _ => {}
        }
    }
}

impl<O: ObjectStore> RepoRead<'_, O> {
    /// The ancestry of changesets `from` and `to` (the module documentation): the two answers,
    /// and up to `limit` changesets of `from..to` after `after` (a generation and an id, the
    /// last entry of the page before; `None` from the start), with whether more follow. A
    /// `limit` of 0 gives the two answers alone. [`RepoError::NotFound`] if the snapshot does not
    /// hold either; `StoreError::IndexCorrupt` for a generation its parents' do not vouch for,
    /// `StoreError::MissingParent` for a parent the index lacks.
    pub fn ancestry(
        &self,
        from: ChangesetId,
        to: ChangesetId,
        limit: usize,
        after: Option<(u64, ChangesetId)>,
    ) -> Result<Ancestry, RepoError> {
        let start = self.walk_start(from)?;
        let end = self.walk_start(to)?;
        self.walk(&[start], &end, limit, after)
    }

    /// The changesets `want` reaches and no changeset of `have` reaches: the entries of the
    /// [`RepoRead::ancestry`] walk with every `have` marked FROM, up to `limit` of them after
    /// `after` (as there). A `have` the snapshot does not hold is passed over; a `want` it does
    /// not hold is [`RepoError::NotFound`]. The errors of a damaged index are `ancestry`'s.
    pub fn missing(
        &self,
        have: &[ChangesetId],
        want: ChangesetId,
        limit: usize,
        after: Option<(u64, ChangesetId)>,
    ) -> Result<Missing, RepoError> {
        let end = self.walk_start(want)?;
        let mut starts = Vec::with_capacity(have.len());
        for &id in have {
            if let Some(record) = self.meta.changeset(id)? {
                starts.push(record);
            }
        }
        let found = self.walk(&starts, &end, limit, after)?;
        let last = found
            .entries
            .last()
            .map(|entry| (entry.generation, entry.id));
        let changesets = found.entries.into_iter().map(|entry| entry.id).collect();
        Ok(Missing::new(changesets, last.filter(|_| found.more)))
    }

    /// The walk of the module documentation from the changesets `froms` and `to`; `ancestry`'s
    /// answers, `is_ancestor` for any of `froms`.
    fn walk(
        &self,
        froms: &[ChangesetMeta],
        end: &ChangesetMeta,
        limit: usize,
        after: Option<(u64, ChangesetId)>,
    ) -> Result<Ancestry, RepoError> {
        // 1. Every end queued with its marks.
        let mut walk = Walk {
            queue: BinaryHeap::new(),
            marks: HashMap::new(),
            live: 0,
        };
        for start in froms {
            walk.mark(start.id, start.generation, Marks::FROM);
        }
        walk.mark(end.id, end.generation, Marks::TO);
        let from: HashSet<ChangesetId> = froms.iter().map(|start| start.id).collect();

        let mut found = Ancestry::new(false, Vec::new());
        let mut bases = 0_usize;

        // 2. Pop until nothing queued can be an entry or a merge base.
        while walk.live > 0 {
            let Some((generation, Reverse(id))) = walk.queue.pop() else {
                break;
            };
            let marks = walk.marks[&id];
            walk.mark(id, generation, Marks::POPPED);
            let record = self.walk_record(id)?;
            let changeset = self.repo.changeset_object(id)?;
            let parents = self.parent_records(&record, &changeset.parents)?;

            // 3. What the changeset is: a merge base, an entry, or neither.
            let mut down = marks.0 & (Marks::FROM | Marks::TO | Marks::STALE);
            let common = marks.has(Marks::FROM | Marks::TO) && !marks.has(Marks::STALE);
            if from.contains(&id) {
                found.is_ancestor |= marks.has(Marks::TO);
            }
            if common {
                bases += 1;
                if found.merge_bases.len() < MAX_MERGE_BASES {
                    found.merge_bases.push(id);
                }
                down |= Marks::STALE;
            }
            let entry = marks.has(Marks::TO) && !marks.has(Marks::FROM);
            if entry && past(after, generation, id) && limit > 0 {
                if found.entries.len() == limit {
                    found.more = true;
                } else {
                    found.entries.push(StoredChangeset {
                        id,
                        seq: record.seq,
                        generation,
                        changeset,
                    });
                }
            }

            // 4. The parents inherit the marks.
            for parent in &parents {
                walk.mark(parent.id, parent.generation, down);
            }
        }
        found.merge_bases_truncated = bases > MAX_MERGE_BASES;
        Ok(found)
    }

    /// The index records of `parents`, the parents the object of the changeset `record` is of
    /// names; `record`'s generation checked against theirs: 1 + the largest, 1 for a root.
    fn parent_records(
        &self,
        record: &ChangesetMeta,
        parents: &[ChangesetId],
    ) -> Result<Vec<ChangesetMeta>, RepoError> {
        let records = parents
            .iter()
            .map(|&parent| self.walk_record(parent))
            .collect::<Result<Vec<_>, _>>()?;
        let top = records.iter().map(|r| r.generation).max().unwrap_or(0);
        if record.generation != top.saturating_add(1) {
            return Err(wrong_generation());
        }
        Ok(records)
    }
}

/// Whether changeset `id` of generation `generation` comes after `after` in the answers' order
/// (generation descending, then id ascending); every one does after `None`.
fn past(after: Option<(u64, ChangesetId)>, generation: u64, id: ChangesetId) -> bool {
    let Some((at, last)) = after else {
        return true;
    };
    generation < at || (generation == at && id > last)
}
