//! `meta/lock`: the exclusive advisory lock (the standard library's file lock, `flock` on
//! Linux) that serializes metadata writers across threads and processes (storage spec §3).
//!
//! # The lock file
//!
//! `meta/lock` is a regular file, which [`MetaLock::acquire`] makes if it is missing and never
//! writes. One that is anything else, as a crafted or restored store can hold, is refused before
//! it is opened, as every store file is: a FIFO, whose open for writing would wait for a reader
//! (and every writer with it, before its timeout even starts), a symlink, whatever it points at
//! (the open would follow it, and make the file it names, outside the store), a socket, a device
//! or a directory. The open itself follows no symlink (`O_NOFOLLOW`), does not wait for a FIFO's
//! reader (`O_NONBLOCK`, which leaves the lock as it is: only `LOCK_NB` keeps `flock` from
//! waiting), and checks what it opened (`fstat`), so that one put there between the check and the
//! open is refused at once too. [`MetaLock::refuses`] is the check alone, which `verify` reports.
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

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::mpsc::{self, RecvTimeoutError, SendError};
use std::thread;
use std::time::Duration;

use crate::error::{FileOp, IoFailure, StoreError};
use crate::fs_ops;
use crate::layout::Layout;

/// The exclusive lock on `meta/lock`, held until dropped.
///
/// The lock belongs to this value's own open file, so two acquisitions conflict even within
/// one process. Dropping the value unlocks the file and closes it; the kernel releases the
/// lock too when the process dies, so a crashed writer leaves no stale lock behind.
#[derive(Debug)]
#[non_exhaustive]
pub struct MetaLock {
    file: File,
}

impl MetaLock {
    /// Takes the lock on `layout`'s `meta/lock`, creating the file if it is missing (`meta/`
    /// must exist). If another handle holds it, waits in the kernel's queue (see the module
    /// documentation) until `timeout` has passed, then gives up with [`StoreError::Busy`]; a
    /// zero `timeout` gives up at once. A failure names `meta/lock`, relative to the store's
    /// root, and what failed: its open, or the lock. A `meta/lock` that is not a regular file
    /// fails its open at once, as damage (kind `InvalidData`; see the module documentation).
    pub fn acquire(layout: &Layout, timeout: Duration) -> Result<MetaLock, StoreError> {
        let lock = layout.meta_lock();
        let failed = |op, e| {
            IoFailure::new(
                op,
                fs_ops::store_relative(layout.root(), lock.as_std_path()),
                e,
            )
        };
        let file = open_lock_file(lock.as_std_path()).map_err(|e| failed(FileOp::Open, e))?;
        match file.try_lock() {
            Ok(()) => return Ok(Self { file }),
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(e)) => return Err(failed(FileOp::Lock, e).into()),
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
            })
            .map_err(|e| failed(FileOp::Lock, e))?;
        match waiting.recv_timeout(timeout) {
            Ok(locked) => Ok(locked.map_err(|e| failed(FileOp::Lock, e))?),
            Err(RecvTimeoutError::Timeout) => Err(StoreError::Busy),
            Err(RecvTimeoutError::Disconnected) => Err(failed(
                FileOp::Lock,
                io::Error::other("the thread waiting for it ended without it"),
            )
            .into()),
        }
    }

    /// Whether [`MetaLock::acquire`] refuses `layout`'s `meta/lock` before it opens it: the file
    /// is there, and is not a regular file (see the module documentation). It only looks
    /// (`lstat`): it opens nothing, follows no symlink and writes nothing, so that a check that
    /// takes no lock, `verify`, can ask. A lock file that is missing is not refused (an acquire
    /// makes it), and one that cannot be looked at is not counted here (an acquire's open fails
    /// on it as it would).
    pub fn refuses(layout: &Layout) -> bool {
        refuses(layout.meta_lock().as_std_path())
    }
}

/// Whether the lock file at `path` is there and is not a regular file, looked at without opening
/// it or following a symlink (`lstat`): the check before [`open_lock_file`]'s open, and
/// [`MetaLock::refuses`]'s. A missing one is not refused (the open makes it), and one that cannot
/// be looked at is left to the open, which fails on it as it would.
fn refuses(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|found| !found.is_file())
}

/// Opens the lock file at `path` for writing, making it if it is missing: one that is not a
/// regular file is refused before it is opened, then [`open_unfollowed`] opens it.
fn open_lock_file(path: &Path) -> io::Result<File> {
    if refuses(path) {
        return Err(fs_ops::not_a_regular_file(path));
    }
    open_unfollowed(path)
}

/// [`open_lock_file`]'s open, without the check before it: it meets what can be put at `path`
/// after that check. It follows no symlink (`ELOOP`), does not wait for a FIFO's reader (`ENXIO`
/// when there is none), and refuses what it opened if that is not a regular file (a FIFO with a
/// reader, a device). `O_NONBLOCK` stays on the file and changes nothing for a regular file's
/// lock: `flock` waits unless `LOCK_NB` is given.
fn open_unfollowed(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if file.metadata()?.is_file() {
        Ok(file)
    } else {
        Err(fs_ops::not_a_regular_file(path))
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

#[cfg(test)]
mod tests {
    use std::fs::{self, File, OpenOptions};
    use std::io;
    use std::os::unix::fs::{OpenOptionsExt, symlink};
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    /// What [`super::open_unfollowed`] returned for `path` within 5 s, run on a thread of its
    /// own so that an open that blocks fails the test instead of hanging it: `None` if it
    /// blocked.
    fn opened_within(path: &Path) -> Option<io::Result<File>> {
        let (tx, rx) = mpsc::channel();
        let path = PathBuf::from(path);
        thread::spawn(move || {
            let _ = tx.send(super::open_unfollowed(&path));
        });
        rx.recv_timeout(Duration::from_secs(5)).ok()
    }

    /// The open alone, without the check before it, meets what can be put at the lock file's
    /// path after that check: a FIFO with no reader fails at once (`ENXIO`; without
    /// `O_NONBLOCK` the open would wait for a reader), one with a reader is opened and then
    /// refused as no regular file, and a symlink is not followed (`ELOOP`), whatever it points
    /// at, so nothing is made where it points. A missing file is made, and an existing regular
    /// one is opened as it is, its bytes kept.
    #[test]
    fn the_open_follows_no_symlink_waits_for_no_reader_and_keeps_only_a_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let at = |name: &str| dir.path().join(name);

        let fifo = at("fifo");
        let made = Command::new("mkfifo").arg(&fifo).status();
        assert!(made.expect("run mkfifo").success(), "mkfifo");
        let alone = opened_within(&fifo).expect("the open waited for a reader");
        assert_eq!(alone.unwrap_err().raw_os_error(), Some(libc::ENXIO));
        let reader = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&fifo)
            .expect("open the FIFO's reading end");
        let e = opened_within(&fifo).expect("the open blocked").unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");
        assert_eq!(e.to_string(), "fifo is not a regular file");
        drop(reader);

        let regular = at("regular");
        fs::write(&regular, b"kept").unwrap();
        let nowhere = at("nowhere");
        for (link, target) in [("to-nowhere", &nowhere), ("to-regular", &regular)] {
            symlink(target, at(link)).unwrap();
            let e = opened_within(&at(link))
                .expect("the open blocked")
                .unwrap_err();
            assert_eq!(e.raw_os_error(), Some(libc::ELOOP), "{link}: {e}");
        }
        assert!(!fs::exists(&nowhere).unwrap(), "a symlink was followed");

        let missing = at("missing");
        opened_within(&missing).expect("the open blocked").unwrap();
        assert!(fs::symlink_metadata(&missing).unwrap().is_file());
        opened_within(&regular).expect("the open blocked").unwrap();
        assert_eq!(fs::read(&regular).unwrap(), b"kept");
    }
}
