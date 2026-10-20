//! First-parent history (storage spec §3): from a changeset back through its first parents to a
//! root, each changeset's record from the changeset index and its parents from its object, which
//! must name the same ones. The mainline is first-parent: a merge's other parents are not walked.

use swarmops_core::ChangesetId;
use swarmops_store::{ObjectStore, StoreError};

use crate::error::RepoError;
use crate::read::{RepoRead, StoredChangeset};
use crate::repo::Repo;

impl<O: ObjectStore> Repo<O> {
    /// Up to `limit` changesets from `from` back through first parents, in a snapshot of its
    /// own ([`RepoRead::log_first_parent`]).
    pub fn log_first_parent(
        &self,
        from: ChangesetId,
        limit: usize,
    ) -> Result<Vec<StoredChangeset>, RepoError> {
        self.snapshot()?.log_first_parent(from, limit)
    }
}

impl<O: ObjectStore> RepoRead<'_, O> {
    /// Up to `limit` changesets from `from` back through first parents, newest first, `from`
    /// included; fewer when a root comes first. [`RepoError::NotFound`] if the snapshot does not
    /// hold `from`. Each step reads the changeset's index record and its object, and goes on to
    /// the first parent they both name: a record whose parents are not its object's is a damaged
    /// index, [`StoreError::IndexCorrupt`], so that the log neither leaves out a committed
    /// changeset nor goes round a cycle a damaged index makes up.
    pub fn log_first_parent(
        &self,
        from: ChangesetId,
        limit: usize,
    ) -> Result<Vec<StoredChangeset>, RepoError> {
        let mut meta = self
            .meta
            .changeset(from)?
            .ok_or_else(|| RepoError::NotFound(from.to_string()))?;
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut log = Vec::with_capacity(limit.min(1024));
        loop {
            let stored = self.repo.stored(&meta)?;
            let parent = stored.changeset.parents.first().copied();
            log.push(stored);
            match parent {
                // A changeset's parents are recorded before it: one missing is a damaged index.
                Some(parent) if log.len() < limit => {
                    meta = self
                        .meta
                        .changeset(parent)?
                        .ok_or(StoreError::MissingParent(parent))?;
                }
                _ => return Ok(log),
            }
        }
    }

    /// The first parent of changeset `id`, `None` for a root, from its object, as every step of
    /// a first-parent walk takes it ([`RepoRead::walk_step`]). [`RepoError::NotFound`] if the
    /// snapshot does not hold `id`, which a narrow index lookup tells
    /// ([`swarmops_store::MetaRead::first_parent`]).
    pub(crate) fn first_parent(&self, id: ChangesetId) -> Result<Option<ChangesetId>, RepoError> {
        if self.meta.first_parent(id)?.is_none() {
            return Err(RepoError::NotFound(id.to_string()));
        }
        self.walk_step(id)
    }
}
