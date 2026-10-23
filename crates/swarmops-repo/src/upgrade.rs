//! `upgrade` (the M6 design §3): moves a FORMAT 1 store to FORMAT 2, under its maintenance
//! lease ([`swarmops_store::meta::upgrade`]). Irreversible, so an operation of its own that no
//! other operation runs as a side effect.

use swarmops_store::{ObjectStore, StoreError, StoreFormat};

use crate::error::RepoError;
use crate::repo::Repo;

/// What [`Repo::upgrade`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct UpgradeReport {
    /// The format the store had.
    pub from: StoreFormat,
    /// The format it has now.
    pub to: StoreFormat,
    /// Whether it changed: false for a store of the latest format already.
    pub upgraded: bool,
}

impl UpgradeReport {
    /// Takes every field.
    pub fn new(from: StoreFormat, to: StoreFormat, upgraded: bool) -> UpgradeReport {
        UpgradeReport { from, to, upgraded }
    }
}

impl<O: ObjectStore> Repo<O> {
    /// Upgrades the store to FORMAT 2; nothing for a FORMAT 2 store.
    ///
    /// Not built yet (the M6 plan's U1): [`StoreError::Format`].
    pub fn upgrade(&self) -> Result<UpgradeReport, RepoError> {
        Err(StoreError::Format("upgrade not built".to_owned()).into())
    }
}
