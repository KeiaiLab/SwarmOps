//! The maintenance lease (the M6 design §8.1): `meta/maint.lock`, which a gc, a repair and an
//! upgrade hold while they run, so that no two of them run at once. It is taken before
//! `meta/lock`, by each of them, so that they never deadlock; it is never waited for.

use crate::error::StoreError;
use crate::format;
use crate::layout::Layout;

/// A held maintenance lease: released when dropped.
#[derive(Debug)]
#[non_exhaustive]
pub struct MaintLease {}

impl MaintLease {
    /// Takes the lease of the store of `layout`, without waiting:
    /// [`StoreError::MaintenanceBusy`] if another gc, repair or upgrade holds it.
    ///
    /// Not built yet (the M6 plan's G1): [`StoreError::Format`].
    pub fn try_acquire(layout: &Layout) -> Result<MaintLease, StoreError> {
        let _ = layout;
        Err(format::not_built("the maintenance lease"))
    }
}
