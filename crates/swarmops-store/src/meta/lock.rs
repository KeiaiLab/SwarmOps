//! `meta/lock`: the exclusive advisory lock (the standard library's file lock, `flock` on
//! Linux) that serializes metadata writers across threads and processes (storage spec §3).

use std::fs::{File, OpenOptions, TryLockError};
use std::thread;
use std::time::{Duration, Instant};

use crate::error::StoreError;
use crate::layout::Layout;

/// How long [`MetaLock::acquire`] sleeps between attempts.
const RETRY_EVERY: Duration = Duration::from_millis(10);

/// The exclusive lock on `meta/lock`, held until dropped.
///
/// The lock belongs to this value's own open file, so two acquisitions conflict even within
/// one process. Dropping the value unlocks the file and closes it; the kernel releases the
/// lock too when the process dies, so a crashed writer leaves no stale lock behind.
#[derive(Debug)]
pub struct MetaLock {
    file: File,
}

impl MetaLock {
    /// Takes the lock on `layout`'s `meta/lock`, creating the file if it is missing (`meta/`
    /// must exist). Tries every 10 ms until `timeout` has passed, then gives up with
    /// [`StoreError::Busy`].
    pub fn acquire(layout: &Layout, timeout: Duration) -> Result<MetaLock, StoreError> {
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(layout.meta_lock())?;
        let deadline = Instant::now().checked_add(timeout);
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Self { file }),
                Err(TryLockError::WouldBlock) => {}
                Err(TryLockError::Error(e)) => return Err(e.into()),
            }
            let now = Instant::now();
            let nap = match deadline {
                Some(deadline) if now >= deadline => return Err(StoreError::Busy),
                Some(deadline) => RETRY_EVERY.min(deadline - now),
                None => RETRY_EVERY,
            };
            thread::sleep(nap);
        }
    }
}

impl Drop for MetaLock {
    fn drop(&mut self) {
        // Closing the file (right after this) would release the lock as well, but only once
        // no forked child still shares the descriptor; unlocking releases it at once. An
        // error leaves the close to do it.
        let _ = self.file.unlock();
    }
}
