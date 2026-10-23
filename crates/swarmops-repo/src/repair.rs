//! `repair` (the M6 design §7): rebuilds the projections of the journal, quarantines damaged
//! objects and drops damaged cache entries, under the store's maintenance lease. It never cuts
//! the journal; damage past its torn tail is reported and refused.

use swarmops_store::{ObjectStore, StoreError};

use crate::error::RepoError;
use crate::repo::Repo;
use crate::verify::VerifyReport;

/// What [`Repo::repair`] is asked to do.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct RepairRequest {
    /// List what it would do, with `done` false, and write nothing.
    pub dry_run: bool,
}

impl RepairRequest {
    /// A repair that mends, or with `dry_run` only lists.
    pub fn new(dry_run: bool) -> RepairRequest {
        RepairRequest { dry_run }
    }
}

/// What a [`RepairAction`] does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RepairKind {
    /// Writes `journal.off` afresh from the journal.
    RebuildOffsets,
    /// Replays the journal into the index and its sorted views again.
    RebuildIndex,
    /// Writes a ref's file again from the journal.
    RebuildRef,
    /// Moves an object file that does not hash to its id, or does not decode, to
    /// `quarantine/objects/`.
    QuarantineObject,
    /// Removes a derived value's file that fails its checks.
    DropDerived,
    /// Removes an external id entry that is damaged or names no changeset of the store.
    DropExternalId,
}

impl RepairKind {
    /// Its name in the API (`RepairAction.kind`): `rebuild_offsets`, `rebuild_index`,
    /// `rebuild_ref`, `quarantine_object`, `drop_derived` or `drop_external_id`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RebuildOffsets => "rebuild_offsets",
            Self::RebuildIndex => "rebuild_index",
            Self::RebuildRef => "rebuild_ref",
            Self::QuarantineObject => "quarantine_object",
            Self::DropDerived => "drop_derived",
            Self::DropExternalId => "drop_external_id",
        }
    }
}

/// One thing [`Repo::repair`] did, or would do.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RepairAction {
    /// What.
    pub kind: RepairKind,
    /// On what: an object id, a ref name or a file class (never a host path).
    pub target: String,
    /// Whether it was done: false for a dry run.
    pub done: bool,
}

impl RepairAction {
    /// Takes every field.
    pub fn new(kind: RepairKind, target: impl Into<String>, done: bool) -> RepairAction {
        RepairAction {
            kind,
            target: target.into(),
            done,
        }
    }
}

/// What [`Repo::repair`] did, and what a verify after it found.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RepairReport {
    /// What it did, in order.
    pub actions: Vec<RepairAction>,
    /// The verify after it.
    pub after: VerifyReport,
}

impl<O: ObjectStore> Repo<O> {
    /// Mends what verify finds that the journal or a quarantine can mend.
    ///
    /// Not built yet (the M6 plan's R2): [`StoreError::Format`].
    pub fn repair(&self, req: RepairRequest) -> Result<RepairReport, RepoError> {
        let _ = req;
        Err(StoreError::Format("repair not built".to_owned()).into())
    }
}
