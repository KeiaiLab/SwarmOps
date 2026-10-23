//! Rebuilding the projections of the journal (the M6 design §7): `journal.off`, the index and
//! its sorted views, the ref files. Each function takes `meta/lock` itself and needs no open
//! [`crate::MetaStore`]: an open fails on the damage they mend. None of them cuts the journal.

use std::sync::Arc;

use crate::error::StoreError;
use crate::format;
use crate::fs_ops::FsOps;
use crate::layout::Layout;

/// What a rebuild wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct Rebuilt {
    /// How many entries, records or files it wrote.
    pub written: u64,
}

/// Writes `journal.off` afresh from the frames of `journal.log`, walked one at a time.
///
/// Not built yet (the M6 plan's R1): [`StoreError::Format`].
pub fn rebuild_offsets(layout: &Layout, fs: Arc<dyn FsOps>) -> Result<Rebuilt, StoreError> {
    let _ = (layout, fs);
    Err(format::not_built("rebuild of journal.off"))
}

/// Replays every frame into the index and its sorted views again.
///
/// Not built yet (the M6 plan's R1): [`StoreError::Format`].
pub fn rebuild_index(layout: &Layout, fs: Arc<dyn FsOps>) -> Result<Rebuilt, StoreError> {
    let _ = (layout, fs);
    Err(format::not_built("rebuild of the index"))
}

/// Writes every ref file again from the journal's `RefMove` records.
///
/// Not built yet (the M6 plan's R1): [`StoreError::Format`].
pub fn rebuild_refs(layout: &Layout, fs: Arc<dyn FsOps>) -> Result<Rebuilt, StoreError> {
    let _ = (layout, fs);
    Err(format::not_built("rebuild of the ref files"))
}
