//! The metadata checks of verify, in the store (the M6 design §6): the sorted views and the
//! index read in blocks, the journal's frames and offsets, and FORMAT 2's seq, previous CRC
//! and entry checksums. Damage is a finding of the report, not an error.

use super::store::MetaStore;
use crate::error::StoreError;
use crate::format;

/// What [`MetaStore::check`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct MetaCheck {
    /// One line per problem found; empty for sound metadata.
    pub problems: Vec<String>,
}

impl MetaStore {
    /// Checks the metadata files against the journal.
    ///
    /// Not built yet (the M6 plan's D2): [`StoreError::Format`].
    pub fn check(&self) -> Result<MetaCheck, StoreError> {
        Err(format::not_built("the metadata check"))
    }
}
