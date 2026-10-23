//! What gc removes (the M6 design §8.2): leftovers, and nothing else. Temp files past their
//! grace, the staging of a crashed create, derived values of superseded versions and what an
//! upgrade leaves of the format before. No object, changeset, journal, index, view or ref
//! file of the store's format, external id, mirror, workspace or quarantined file is ever one.

use std::time::{Duration, SystemTime};

use camino::Utf8PathBuf;

use crate::error::StoreError;
use crate::format;
use crate::fs_ops::StoreFs;
use crate::layout::Layout;

/// How old a temp file must be before gc removes it: one a write in progress still holds is
/// younger.
pub const GC_TEMP_GRACE: Duration = Duration::from_hours(24);

/// What kind of leftover a [`Leftover`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum LeftoverKind {
    /// A temp file in `objects/tmp/` older than [`GC_TEMP_GRACE`].
    ObjectTemp,
    /// A temp file in `meta/derived/tmp/` older than [`GC_TEMP_GRACE`].
    DerivedTemp,
    /// `meta.tmp/`, a crashed create's staging, where `meta/` exists.
    MetaStaging,
    /// `meta/derived/<name>/<version>/` of a derivation this build registers, below its
    /// version.
    DerivedVersion,
    /// A file of the format before an upgrade (`meta/journal.*`, `meta/refs/` in a FORMAT 2
    /// store), or of an upgrade that stopped before its commit point.
    UpgradeResidue,
}

impl LeftoverKind {
    /// Its name in the API (`GcCount.kind`): `object_temp`, `derived_temp`, `meta_staging`,
    /// `derived_version` or `upgrade_residue`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ObjectTemp => "object_temp",
            Self::DerivedTemp => "derived_temp",
            Self::MetaStaging => "meta_staging",
            Self::DerivedVersion => "derived_version",
            Self::UpgradeResidue => "upgrade_residue",
        }
    }
}

/// One leftover gc removes: its kind, where it is relative to the store's root, and how many
/// bytes it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Leftover {
    /// What it is.
    pub kind: LeftoverKind,
    /// Its path relative to the store's root (never a host path).
    pub rel: Utf8PathBuf,
    /// The bytes of its files.
    pub bytes: u64,
}

/// Every leftover of the store of `layout`, as of `now`, sorted by kind, then path.
///
/// Not built yet (the M6 plan's G1): [`StoreError::Format`].
pub fn list_leftovers(layout: &Layout, now: SystemTime) -> Result<Vec<Leftover>, StoreError> {
    let _ = (layout, now);
    Err(format::not_built("gc"))
}

/// Removes `leftover`, through `fs`, and makes its removal durable.
///
/// Not built yet (the M6 plan's G1): [`StoreError::Format`].
pub fn remove(fs: &StoreFs, leftover: &Leftover) -> Result<(), StoreError> {
    let _ = (fs, leftover);
    Err(format::not_built("gc"))
}
