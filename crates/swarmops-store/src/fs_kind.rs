//! Which filesystems a store may live on (the M6 design §9): on Linux, an allow-list of the
//! local filesystems whose fsync the store's durability rules rely on, read with `statfs`.
//! A filesystem not on the list (NFS, SMB, FUSE, 9p among them) is refused, fail-closed.

use camino::Utf8Path;

use crate::error::StoreError;

/// Refuses, with [`StoreError::UnsupportedFs`], a store at `path` on a filesystem the store
/// does not support. `path` is the store's root on open, and the nearest existing ancestor of
/// the directory a create makes.
///
/// Every filesystem passes for now: the allow-list is the M6 plan's B1.
pub fn check_supported_fs(path: &Utf8Path) -> Result<(), StoreError> {
    let _ = path;
    Ok(())
}
