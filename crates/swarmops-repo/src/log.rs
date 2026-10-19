//! First-parent history (storage spec §3): from a changeset back through its first parents,
//! as the changeset index records them, to a root. The mainline is first-parent: a merge's
//! other parents are not walked.

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
    /// hold `from`.
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
            let parent = meta.parents.first().copied();
            log.push(self.repo.stored(&meta)?);
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

    /// The first parent of changeset `id` as the snapshot records it; `None` for a root.
    /// [`RepoError::NotFound`] if the snapshot does not hold `id`.
    pub(crate) fn first_parent(&self, id: ChangesetId) -> Result<Option<ChangesetId>, RepoError> {
        let meta = self
            .meta
            .changeset(id)?
            .ok_or_else(|| RepoError::NotFound(id.to_string()))?;
        Ok(meta.parents.first().copied())
    }
}
