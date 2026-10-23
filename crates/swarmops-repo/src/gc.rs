//! `gc` (the M6 design §8): removes a store's leftovers, under its maintenance lease, and
//! counts the objects no changeset reaches. It removes no object and no changeset.

use swarmops_store::{LeftoverKind, ObjectStore, StoreError};

use crate::error::RepoError;
use crate::repo::Repo;

/// What [`Repo::gc`] is asked to do.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct GcRequest {
    /// List what it would remove, and remove nothing.
    pub dry_run: bool,
}

impl GcRequest {
    /// A gc that removes, or with `dry_run` only lists.
    pub fn new(dry_run: bool) -> GcRequest {
        GcRequest { dry_run }
    }
}

/// What [`Repo::gc`] removed, or would remove for a dry run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct GcReport {
    /// How many leftovers of each kind, kinds with none omitted.
    pub removed: Vec<(LeftoverKind, u64)>,
    /// The bytes of their files.
    pub bytes: u64,
    /// How many object files no indexed changeset reaches: counted, never removed.
    pub orphan_objects: u64,
}

impl<O: ObjectStore> Repo<O> {
    /// Removes the store's leftovers ([`swarmops_store::gc`]) under its maintenance lease.
    ///
    /// Not built yet (the M6 plan's G1): [`StoreError::Format`].
    pub fn gc(&self, req: GcRequest) -> Result<GcReport, RepoError> {
        let _ = req;
        Err(StoreError::Format("gc not built".to_owned()).into())
    }
}
