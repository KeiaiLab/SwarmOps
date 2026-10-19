//! The diff of two changesets (storage spec §4): the leaf-level changes between their root
//! trees, read in one snapshot ([`RepoRead::diff`]). A snapshot is the [`TreeRead`] that
//! [`diff_trees`] reads the trees through, each verified against its id as every object read
//! is, so that a subtree both changesets share is never read.

use swarmops_core::{ChangesetId, Tree, TreeId};
use swarmops_merge::tree_diff::{PathFilter, TreeChange, TreeRead, diff_trees};
use swarmops_store::ObjectStore;

use crate::error::RepoError;
use crate::read::RepoRead;

/// A snapshot gives a diff its trees: each read by its id and verified ([`RepoRead::tree`]),
/// [`RepoError::MissingObject`] for a tree the store lacks.
impl<O: ObjectStore> TreeRead for RepoRead<'_, O> {
    type Error = RepoError;

    fn tree(&self, id: TreeId) -> Result<Tree, RepoError> {
        // The inherent method of the same name.
        RepoRead::tree(self, id)
    }
}

impl<O: ObjectStore> RepoRead<'_, O> {
    /// The leaves that differ between changeset `from`'s tree and changeset `to`'s, within
    /// `filter`: [`diff_trees`] of their roots, each path once, sorted by path bytes. A subtree
    /// both trees share is never read, nor is a directory `filter` does not reach.
    /// [`RepoError::NotFound`] if the snapshot does not hold `from` or `to`.
    pub fn diff(
        &self,
        from: ChangesetId,
        to: ChangesetId,
        filter: &PathFilter,
    ) -> Result<Vec<TreeChange>, RepoError> {
        let (from, to) = (self.root(from)?, self.root(to)?);
        diff_trees(self, Some(from), Some(to), filter)
    }

    /// The root tree of changeset `cs`; [`RepoError::NotFound`] if the snapshot does not hold
    /// it.
    fn root(&self, cs: ChangesetId) -> Result<TreeId, RepoError> {
        match self.changeset(cs)? {
            Some(stored) => Ok(stored.changeset.root),
            None => Err(RepoError::NotFound(cs.to_string())),
        }
    }
}
