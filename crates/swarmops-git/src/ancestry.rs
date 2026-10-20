//! The ancestry of a store's changesets over every parent, as the import checks its base against
//! the tip it imports and the export checks the changeset its branch holds the export of against
//! the head it exports: a walk down the store's records, pruned by generation.

use std::collections::HashSet;

use swarmops_core::ChangesetId;
use swarmops_repo::{LocalRepo, RepoError};

/// Whether `base`, of generation `generation`, is one of `from` or an ancestor of one, over
/// every parent: a walk down the store's records ([`swarmops_repo::Repo::changeset_meta`]),
/// pruned where the generation is no higher than the base's, since generations grow from
/// parent to child.
pub(crate) fn reaches(
    repo: &LocalRepo,
    from: impl IntoIterator<Item = ChangesetId>,
    base: ChangesetId,
    generation: u64,
) -> Result<bool, RepoError> {
    let mut stack: Vec<ChangesetId> = from.into_iter().collect();
    let mut seen = HashSet::new();
    while let Some(at) = stack.pop() {
        if at == base {
            return Ok(true);
        }
        if !seen.insert(at) {
            continue;
        }
        if let Some(meta) = repo.changeset_meta(at)?
            && meta.generation > generation
        {
            stack.extend(meta.parents);
        }
    }
    Ok(false)
}
