//! `upgrade` (the M6 design §3): moves a FORMAT 1 store to FORMAT 2, under its maintenance
//! lease ([`upgrade`]). Irreversible, so an operation of its own that no other operation runs as
//! a side effect.

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
    /// Upgrades the store to FORMAT 2 ([`upgrade::upgrade`]); nothing for a FORMAT 2 store.
    /// [`swarmops_store::StoreError::MaintenanceBusy`] at once if a gc, a repair or another upgrade holds the
    /// maintenance lease; [`swarmops_store::StoreError::Busy`] if a writer holds `meta/lock` for the whole wait.
    ///
    /// Every handle opened on the store as FORMAT 1 before, this one among them, reads the
    /// poisoned FORMAT 1 checkpoint from then on: its next read or write fails as damage, and a
    /// handle opened afresh reads FORMAT 2.
    pub fn upgrade(&self) -> Result<UpgradeReport, RepoError> {
        let done = upgrade::upgrade(&self.layout, Arc::clone(&self.fs))?;
        Ok(UpgradeReport::new(done.from, done.to, done.upgraded))
    }
}
