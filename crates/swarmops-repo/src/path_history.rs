//! Path history (storage spec §5's `LastModified`; operations spec §1's `path_history` and
//! `log --path`), read in a snapshot ([`RepoRead`]): the changeset that last touched a path
//! ([`RepoRead::last_modified`]), a path's history, which chases that value from one touch to the
//! newest one below its first parent ([`RepoRead::path_history`]), and the changesets that touch
//! any of a filter's paths ([`RepoRead::log_paths`]).
//!
//! A changeset touches a path when its delta has an entry at the path or below it: a file whose
//! content or kind changed, was added or deleted, or anything below a directory. History is
//! first-parent, as the log is: a changeset's delta is relative to its first parent.
//!
//! A value of [`LastModified`] read back is a cache file anyone who can write the store's files
//! can have placed, so each one these reads use is checked before it is: the changeset it names
//! is the one asked about, or one the snapshot holds of a lower generation, and its delta touches
//! the path. One that fails is [`RepoError::DerivedCorrupt`]. Whether a changeset between the two
//! touched the path, and whether the one named is on the first-parent chain at all, a reader
//! cannot tell without the walk the value saves; `verify --deep` checks every value against the
//! objects.

use swarmops_core::{ChangesetId, EntryTarget, RepoPath};
use swarmops_store::{ObjectStore, StoreError};

use crate::derive::{self, Derivation, LastModified, PathKey};
use crate::error::RepoError;
use crate::merge::tree_diff::PathFilter;
use crate::read::{RepoRead, StoredChangeset};

/// One changeset of a path's history ([`RepoRead::path_history`]): one that touched the path, and
/// what the path holds after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PathEvent {
    /// The changeset.
    pub changeset: ChangesetId,
    /// What the path holds after it: a leaf, a directory, or nothing (`None`: it deleted the path,
    /// or everything below it).
    pub entry: Option<EntryTarget>,
}

impl<O: ObjectStore> RepoRead<'_, O> {
    /// The newest changeset on `cs`'s first-parent chain, `cs` included, that touched `path`;
    /// `None` if none did. The derived value [`LastModified`] ([`crate::Repo::derived`]), read back
    /// if the store holds it, or derived and stored: a walk of the changesets that do not touch the
    /// path, at most 64 steps where an earlier walk left its memos. [`RepoError::NotFound`] if the
    /// snapshot does not hold `cs`; [`RepoError::DerivedCorrupt`] for a value that fails its
    /// checks ([the module documentation](self)).
    pub fn last_modified(
        &self,
        cs: ChangesetId,
        path: &RepoPath,
    ) -> Result<Option<ChangesetId>, RepoError> {
        self.held(cs)?;
        self.last_touch(cs, path)
    }

    /// Up to `limit` changesets that touched `path`, newest first, from `from`'s first-parent
    /// chain, `from` included, each with what `path` holds after it: the last one to touch it
    /// ([`RepoRead::last_modified`]), then the last one to touch it from that one's first parent,
    /// and so on to a root. Empty when no changeset on the chain touched the path. A directory's
    /// history is that of everything below it. [`RepoError::NotFound`] if the snapshot does not
    /// hold `from`; [`RepoError::DerivedCorrupt`] for a value of [`LastModified`] that fails its
    /// checks.
    pub fn path_history(
        &self,
        from: ChangesetId,
        path: &RepoPath,
        limit: usize,
    ) -> Result<Vec<PathEvent>, RepoError> {
        self.held(from)?;
        let mut events = Vec::new();
        let mut at = Some(from);
        while events.len() < limit {
            let Some(cs) = at else { break };
            let Some(changeset) = self.last_touch(cs, path)? else {
                break;
            };
            events.push(PathEvent {
                changeset,
                entry: self.entry_at(changeset, path)?,
            });
            at = self.parent_of(changeset)?;
        }
        Ok(events)
    }

    /// Up to `limit` changesets from `from`'s first-parent chain, `from` included, newest first,
    /// whose delta has an entry that `filter` matches. With prefixes, a chase of each prefix's
    /// [`RepoRead::last_modified`], all on the one chain: the newest of them, then, for each prefix
    /// it touched, the last modifier from its first parent; so a changeset that touches several
    /// prefixes is listed once. For every path ([`PathFilter::all`]), the changesets whose delta is
    /// not empty, by a walk of the chain. [`RepoError::NotFound`] if the snapshot does not hold
    /// `from`; [`RepoError::DerivedCorrupt`] for a value of [`LastModified`] that fails its checks.
    pub fn log_paths(
        &self,
        from: ChangesetId,
        filter: &PathFilter,
        limit: usize,
    ) -> Result<Vec<StoredChangeset>, RepoError> {
        self.held(from)?;
        let Some(prefixes) = filter.prefixes() else {
            return self.log_changes(from, limit);
        };
        // Each prefix's next touch, as its generation and id: all are on `from`'s first-parent
        // chain, along which generations fall, so the highest is the newest.
        let mut next = Vec::with_capacity(prefixes.len());
        for prefix in prefixes {
            next.push(self.touch(Some(from), prefix)?);
        }
        let mut log = Vec::new();
        while log.len() < limit {
            let Some(newest) = next.iter().flatten().max().copied() else {
                break;
            };
            let (_, id) = newest;
            let stored = self.stored_at(id)?;
            let parent = stored.changeset.parents.first().copied();
            log.push(stored);
            for (prefix, touch) in prefixes.iter().zip(&mut next) {
                if *touch == Some(newest) {
                    *touch = self.touch(parent, prefix)?;
                }
            }
        }
        Ok(log)
    }

    /// The last changeset to touch `prefix` from `at` (none from no changeset), with its
    /// generation, which orders it on the chain.
    fn touch(
        &self,
        at: Option<ChangesetId>,
        prefix: &RepoPath,
    ) -> Result<Option<(u64, ChangesetId)>, RepoError> {
        let Some(at) = at else { return Ok(None) };
        let Some(id) = self.last_touch(at, prefix)? else {
            return Ok(None);
        };
        let record = self
            .meta
            .changeset(id)?
            .ok_or(StoreError::MissingParent(id))?;
        Ok(Some((record.generation, id)))
    }

    /// [`LastModified`]'s value of changeset `cs`, which the snapshot holds, under `path`,
    /// checked as [the module documentation](self) says.
    fn last_touch(
        &self,
        cs: ChangesetId,
        path: &RepoPath,
    ) -> Result<Option<ChangesetId>, RepoError> {
        let touch = self
            .repo
            .derived::<LastModified>(cs, &PathKey(path.clone()))?;
        if let Some(toucher) = touch.0 {
            self.check_touch(cs, toucher, path)?;
        }
        Ok(touch.0)
    }

    /// Checks that `toucher`, which [`LastModified`] says last touched `path` from changeset `cs`,
    /// can be that: `cs` itself, or a changeset the snapshot holds of a lower generation; and that
    /// its delta touches the path. Reads `toucher`'s object.
    fn check_touch(
        &self,
        cs: ChangesetId,
        toucher: ChangesetId,
        path: &RepoPath,
    ) -> Result<(), RepoError> {
        let wrong = |why: String| RepoError::DerivedCorrupt {
            name: LastModified::NAME.to_owned(),
            changeset: cs,
            why,
        };
        if toucher != cs {
            let from = self
                .meta
                .changeset(cs)?
                .ok_or_else(|| RepoError::NotFound(cs.to_string()))?;
            let Some(record) = self.meta.changeset(toucher)? else {
                return Err(wrong(format!(
                    "under path {path} it names {toucher}, which the store does not hold"
                )));
            };
            if record.generation >= from.generation {
                return Err(wrong(format!(
                    "under path {path} it names {toucher}, of generation {}, not below its own {}",
                    record.generation, from.generation
                )));
            }
        }
        if !derive::touches(&self.repo.changeset_object(toucher)?.delta, path) {
            return Err(wrong(format!(
                "under path {path} it names {toucher}, whose delta does not touch the path"
            )));
        }
        Ok(())
    }

    /// Up to `limit` changesets from `from` back through first parents, newest first, whose delta
    /// is not empty.
    fn log_changes(
        &self,
        from: ChangesetId,
        limit: usize,
    ) -> Result<Vec<StoredChangeset>, RepoError> {
        let mut log = Vec::new();
        let mut at = Some(from);
        while log.len() < limit {
            let Some(id) = at else { break };
            let stored = self.stored_at(id)?;
            at = stored.changeset.parents.first().copied();
            if !stored.changeset.delta.is_empty() {
                log.push(stored);
            }
        }
        Ok(log)
    }

    /// Changeset `id`, which the snapshot holds as an ancestor of a changeset it holds (a missing
    /// one is a damaged index).
    fn stored_at(&self, id: ChangesetId) -> Result<StoredChangeset, RepoError> {
        let record = self
            .meta
            .changeset(id)?
            .ok_or(StoreError::MissingParent(id))?;
        self.repo.stored(&record)
    }

    /// The first parent of changeset `cs` (`None` for a root), a narrow lookup that also says the
    /// snapshot holds it: [`RepoError::NotFound`] if it does not.
    fn held(&self, cs: ChangesetId) -> Result<Option<ChangesetId>, RepoError> {
        self.meta
            .first_parent(cs)?
            .ok_or_else(|| RepoError::NotFound(cs.to_string()))
    }

    /// The first parent of changeset `id` (`None` for a root), an ancestor of a changeset the
    /// snapshot holds, as its object names it ([`RepoRead::walk_step`]).
    fn parent_of(&self, id: ChangesetId) -> Result<Option<ChangesetId>, RepoError> {
        self.walk_step(id)
    }
}
