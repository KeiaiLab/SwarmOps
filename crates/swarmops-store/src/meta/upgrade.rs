//! The upgrade of a FORMAT 1 store to FORMAT 2 (the M6 design §3): the FORMAT 2 files written
//! beside the FORMAT 1 ones, FORMAT 1's checkpoint poisoned ([`crate::format::UPGRADE_POISON`]),
//! then `FORMAT` replaced, the commit point. An open that finds the poison under a FORMAT 1
//! `FORMAT` finishes the upgrade ([`finish_if_poisoned`]).

use std::sync::Arc;

use crate::error::StoreError;
use crate::format::{self, StoreFormat};
use crate::fs_ops::FsOps;
use crate::layout::Layout;

/// What [`upgrade`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Upgraded {
    /// The format the store had.
    pub from: StoreFormat,
    /// The format it has now.
    pub to: StoreFormat,
    /// Whether it changed: false for a store that had the latest format already.
    pub upgraded: bool,
}

impl Upgraded {
    /// Takes every field.
    pub fn new(from: StoreFormat, to: StoreFormat, upgraded: bool) -> Upgraded {
        Upgraded { from, to, upgraded }
    }
}

/// Upgrades the store of `layout` to FORMAT 2; nothing for a FORMAT 2 store.
///
/// Not built yet (the M6 plan's U1): [`StoreError::Format`].
pub fn upgrade(layout: &Layout, fs: Arc<dyn FsOps>) -> Result<Upgraded, StoreError> {
    let _ = (layout, fs);
    Err(format::not_built("upgrade"))
}

/// Finishes an upgrade of the FORMAT 1 store of `layout` that stopped between its poison and
/// its commit point: replaces `FORMAT` if FORMAT 1's checkpoint is the poison. Whether it did.
///
/// Not built yet (the M6 plan's U1): finds nothing to finish.
pub fn finish_if_poisoned(layout: &Layout, fs: Arc<dyn FsOps>) -> Result<bool, StoreError> {
    let _ = (layout, fs);
    Ok(false)
}
