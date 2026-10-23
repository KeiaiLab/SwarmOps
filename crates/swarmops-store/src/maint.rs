//! The maintenance lease (the M6 design §8.1): `meta/maint.lock`, which a gc, a repair and an
//! upgrade hold while they run, so that no two of them run at once. It is taken before
//! `meta/lock`, by each of them, so that they never deadlock; it is never waited for.
//!
//! ```text
//! MaintLease::try_acquire   meta/maint.lock   try_lock, or MaintenanceBusy at once
//!   lease.meta_lock(t)      meta/lock         MetaLock::acquire, waiting up to t
//! ```
//!
//! The order is kept by construction: `meta/lock` is taken through the lease
//! ([`MaintLease::meta_lock`]), and a thread that holds a `meta/lock` taken so and asks for the
//! lease again breaks the order, which [`MaintLease::try_acquire`] refuses with a panic (a bug,
//! never a state of the store).

use std::cell::Cell;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::Duration;

use crate::error::{FileOp, IoFailure, StoreError};
use crate::fs_ops;
use crate::layout::Layout;
use crate::meta::MetaLock;

/// The lease file's name in `meta/`.
const LEASE_FILE: &str = "maint.lock";

thread_local! {
    /// How many `meta/lock`s taken through a lease ([`MaintLease::meta_lock`]) this thread
    /// holds: a lease asked for while one is held is asked for in the wrong order.
    static META_HELD: Cell<u32> = const { Cell::new(0) };
}

/// A held maintenance lease: released when dropped.
#[derive(Debug)]
#[non_exhaustive]
pub struct MaintLease {
    file: File,
}

/// `meta/lock`, taken after the maintenance lease ([`MaintLease::meta_lock`]): released when
/// dropped.
#[derive(Debug)]
#[non_exhaustive]
pub struct LeasedMetaLock {
    _lock: MetaLock,
}

impl MaintLease {
    /// Takes the lease of the store of `layout`, without waiting:
    /// [`StoreError::MaintenanceBusy`] if another gc, repair or upgrade holds it. The lease
    /// file is made if it is missing (`meta/` must exist) and opened as `meta/lock` is: one that
    /// is not a regular file is refused (kind `InvalidData`), and no symlink is followed.
    ///
    /// # Panics
    ///
    /// If this thread holds a `meta/lock` taken through a lease: the lease comes first.
    pub fn try_acquire(layout: &Layout) -> Result<MaintLease, StoreError> {
        assert_eq!(
            META_HELD.get(),
            0,
            "lock order: meta/maint.lock is taken before meta/lock, never while it is held"
        );
        let path = layout.meta_path(LEASE_FILE);
        let failed = |op, e| {
            IoFailure::new(
                op,
                fs_ops::store_relative(layout.root(), path.as_std_path()),
                e,
            )
        };

        // 1. Open the file as meta/lock is opened, then try its lock once.
        let file = open_lease_file(path.as_std_path()).map_err(|e| failed(FileOp::Open, e))?;
        match file.try_lock() {
            Ok(()) => Ok(MaintLease { file }),
            Err(TryLockError::WouldBlock) => Err(StoreError::MaintenanceBusy),
            Err(TryLockError::Error(e)) => Err(failed(FileOp::Lock, e).into()),
        }
    }

    /// Takes `meta/lock` of the store of `layout`, under this lease, waiting up to `timeout`
    /// ([`MetaLock::acquire`]): the second lock of the order.
    pub fn meta_lock(
        &self,
        layout: &Layout,
        timeout: Duration,
    ) -> Result<LeasedMetaLock, StoreError> {
        let lock = MetaLock::acquire(layout, timeout)?;
        META_HELD.set(META_HELD.get() + 1);
        Ok(LeasedMetaLock { _lock: lock })
    }
}

impl Drop for MaintLease {
    fn drop(&mut self) {
        // Closing the file would release it too, once no forked child shares it.
        let _ = self.file.unlock();
    }
}

impl Drop for LeasedMetaLock {
    fn drop(&mut self) {
        META_HELD.set(META_HELD.get().saturating_sub(1));
    }
}

/// Opens the lease file at `path` for writing, making it if it is missing, by `meta/lock`'s
/// rules: refused unopened if it is there and no regular file (`lstat`), opened without
/// following a symlink or waiting for a FIFO's reader, and refused if what was opened is no
/// regular file.
fn open_lease_file(path: &Path) -> io::Result<File> {
    if fs::symlink_metadata(path).is_ok_and(|found| !found.is_file()) {
        return Err(fs_ops::not_a_regular_file(path));
    }
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(fs_ops::not_a_regular_file(path));
    }
    Ok(file)
}
