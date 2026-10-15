//! `meta/lock`: the exclusive advisory lock (`flock`, through fd-lock) that serializes
//! metadata writers across threads and processes (storage spec §3).

use std::fs::{File, OpenOptions};
use std::io;
use std::mem;
use std::thread;
use std::time::{Duration, Instant};

use crate::error::StoreError;
use crate::layout::Layout;

/// How long [`MetaLock::acquire`] sleeps between attempts.
const RETRY_EVERY: Duration = Duration::from_millis(10);

/// The exclusive lock on `meta/lock`, held until dropped.
///
/// The lock belongs to this value's own open file, so two acquisitions conflict even within
/// one process. Dropping the value closes the file, which releases the lock; the kernel
/// releases it too when the process dies, so a crashed writer leaves no stale lock behind.
#[derive(Debug)]
pub struct MetaLock {
    _locked: fd_lock::RwLock<File>,
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
        let mut lock = fd_lock::RwLock::new(file);
        let deadline = Instant::now().checked_add(timeout);
        loop {
            let locked = match lock.try_write() {
                Ok(guard) => {
                    // The guard borrows `lock`, so it cannot be returned along with it.
                    // Forgetting it keeps the lock, which then lasts while the file is open.
                    mem::forget(guard);
                    true
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => false,
                Err(e) => return Err(e.into()),
            };
            if locked {
                return Ok(Self { _locked: lock });
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
