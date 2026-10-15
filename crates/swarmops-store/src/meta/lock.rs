//! `meta/lock`: the exclusive advisory lock (the standard library's file lock, `flock` on
//! Linux) that serializes metadata writers across threads and processes (storage spec §3).
//!
//! # Waiting: the kernel flock queue (not strictly FIFO), timeout via a helper thread
//!
//! [`MetaLock::acquire`] opens `meta/lock` afresh and first tries the lock without blocking,
//! so an uncontended writer takes it at once, on its own thread. If another handle holds it,
//! the wait goes to the kernel: a helper thread blocks in `flock(LOCK_EX)` (std's
//! `File::lock`) on that same open file, and the caller waits on a channel, up to its
//! timeout, for the helper to hand the locked file back. The kernel wakes the blocked waiters
//! the moment the holder unlocks, so the lock passes on at once instead of at a waiter's next
//! poll. It is not strictly FIFO: the woken waiters race for the lock, and a writer that asks
//! again right after it unlocked races with them. With 32 writers that loop, the median wait
//! is one rotation (31 transactions), or next to nothing when the writer that has just
//! committed asks again first; the longest is a few hundred transactions (docs/perf.md, "M1
//! store fixes (7c)").
//!
//! A caller that times out returns [`StoreError::Busy`] and drops its end of the channel,
//! which abandons its helper: the helper stays in the kernel's queue (a blocked `flock` cannot
//! be called off), and when it gets the lock its hand-off fails, so it drops the guard, which
//! unlocks the file and closes it at once. A guard handed off in the instant the caller gives
//! up is dropped with the channel. Either way an abandoned wait holds the lock only for that
//! instant. An abandoned helper parks until the holder lets go; a holder that never does (a
//! hung process) leaves one parked thread per abandoned wait in each waiting process.

use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::sync::mpsc::{self, RecvTimeoutError, SendError};
use std::thread;
use std::time::Duration;

use crate::error::StoreError;
use crate::layout::Layout;

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
    /// must exist). If another handle holds it, waits in the kernel's queue (see the module
    /// documentation) until `timeout` has passed, then gives up with [`StoreError::Busy`]; a
    /// zero `timeout` gives up at once.
    pub fn acquire(layout: &Layout, timeout: Duration) -> Result<MetaLock, StoreError> {
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(layout.meta_lock())?;
        match file.try_lock() {
            Ok(()) => return Ok(Self { file }),
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(e)) => return Err(e.into()),
        }
        if timeout.is_zero() {
            return Err(StoreError::Busy);
        }
        let (handoff, waiting) = mpsc::sync_channel(1);
        thread::Builder::new()
            .name("meta-lock-wait".to_owned())
            .spawn(move || {
                let locked = lock_blocking(&file).map(|()| MetaLock { file });
                if let Err(SendError(abandoned)) = handoff.send(locked) {
                    // The caller timed out and dropped its end: it has abandoned this wait.
                    // Dropping the guard unlocks the file.
                    drop(abandoned);
                }
            })?;
        match waiting.recv_timeout(timeout) {
            Ok(locked) => Ok(locked?),
            Err(RecvTimeoutError::Timeout) => Err(StoreError::Busy),
            Err(RecvTimeoutError::Disconnected) => {
                Err(io::Error::other("the thread waiting for meta/lock ended without it").into())
            }
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

/// Blocks until `file` holds the exclusive lock, asking again when a signal interrupts the
/// wait (std's `File::lock` reports `EINTR`).
fn lock_blocking(file: &File) -> io::Result<()> {
    loop {
        match file.lock() {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            locked => return locked,
        }
    }
}
