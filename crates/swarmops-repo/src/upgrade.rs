//! `upgrade` (the M6 design §3, the FORMAT 3 design §3): moves a FORMAT 1 or 2 store to FORMAT 3,
//! under its maintenance lease ([`upgrade`]). Irreversible, so an operation of its own that no
//! other operation runs as a side effect.

use std::sync::Arc;

use swarmops_store::meta::upgrade;
use swarmops_store::{ObjectStore, StoreFormat};

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
    /// Upgrades the store to FORMAT 3, a FORMAT 1 one through FORMAT 2 ([`upgrade::upgrade`]);
    /// nothing for a FORMAT 3 store. [`swarmops_store::StoreError::MaintenanceBusy`] at once if a
    /// gc, a repair or another upgrade holds the maintenance lease;
    /// [`swarmops_store::StoreError::Busy`] if a view step or a writer holds `meta/views.lock` or
    /// `meta/lock` for the whole wait.
    ///
    /// Every handle opened on the store in the format before, this one among them, reads its
    /// poisoned checkpoint from then on: its next read or write fails as damage, and a handle
    /// opened afresh reads FORMAT 3.
    pub fn upgrade(&self) -> Result<UpgradeReport, RepoError> {
        let done = upgrade::upgrade(&self.layout, Arc::clone(&self.fs))?;
        Ok(UpgradeReport::new(done.from, done.to, done.upgraded))
    }
}
