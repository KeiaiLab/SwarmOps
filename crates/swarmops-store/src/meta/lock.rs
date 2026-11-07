//! `meta/lock`: the exclusive advisory lock (the standard library's file lock, `flock` on
//! Linux) that serializes metadata writers, and committed readers, across threads and processes
//! (storage spec §3).
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

use std::cell::RefCell;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SendError};
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
        match Self::start(layout)? {
            Started::Held(lock) => Ok(lock),
            Started::Waiting(wait) => wait.wait(timeout).unwrap_or(Err(StoreError::Busy)),
        }
    }

    /// The first half of [`MetaLock::acquire`]: opens `meta/lock` and tries the lock without
    /// blocking. [`Started::Held`] if it was free; else [`Started::Waiting`], whose
    /// [`LockWait::wait`] waits for it in the kernel's queue. Spawns no thread.
    pub(crate) fn start(layout: &Layout) -> Result<Started, StoreError> {
        let lock = layout.meta_lock();
        let file = open_lock_file(lock.as_std_path())
            .map_err(|e| lock_failure(layout, FileOp::Open, e))?;
        match file.try_lock() {
            Ok(()) => Ok(Started::Held(Self { file })),
            Err(TryLockError::WouldBlock) => Ok(Started::Waiting(LockWait {
                layout: layout.clone(),
                state: RefCell::new(WaitState::Idle(file)),
            })),
            Err(TryLockError::Error(e)) => Err(lock_failure(layout, FileOp::Lock, e).into()),
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
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(false);
    open_with(path, &options)
}

/// [`open_unfollowed`]'s open with `options`: no symlink followed, no FIFO's reader waited for,
/// and what it opened refused unless it is a regular file.
fn open_with(path: &Path, options: &OpenOptions) -> io::Result<File> {
    let file = options
        .clone()
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

/// What [`MetaLock::start`] found.
#[derive(Debug)]
pub(crate) enum Started {
    /// The lock was free: it is held now.
    Held(MetaLock),
    /// Another handle holds it: wait for it.
    Waiting(LockWait),
}

/// A wait for `meta/lock`, from [`MetaLock::start`]: its first [`LockWait::wait`] with a limit
/// above zero hands the open file to a helper thread blocked in `flock`, and each wait after it
/// waits on that helper's hand-off. Dropping it abandons the helper, as a timed-out
/// [`MetaLock::acquire`] does (see the module documentation).
#[derive(Debug)]
pub(crate) struct LockWait {
    layout: Layout,
    state: RefCell<WaitState>,
}

/// Where a [`LockWait`] is.
#[derive(Debug)]
enum WaitState {
    /// No helper yet: the open file, not locked.
    Idle(File),
    /// A helper blocked in `flock`, which hands the lock over on this channel.
    Helper(Receiver<io::Result<MetaLock>>),
    /// The lock was handed over, or the helper failed.
    Over,
}

impl LockWait {
    /// Waits up to `limit` for the lock: `None` if it is still taken then, else the lock or why
    /// waiting for it failed. A zero `limit` on a wait that has no helper yet spawns none.
    pub(crate) fn wait(&self, limit: Duration) -> Option<Result<MetaLock, StoreError>> {
        let mut state = self.state.borrow_mut();

        // 1. The first wait with a limit starts the helper.
        if let WaitState::Idle(_) = &*state {
            if limit.is_zero() {
                return None;
            }
            let WaitState::Idle(file) = std::mem::replace(&mut *state, WaitState::Over) else {
                unreachable!("the state was idle");
            };
            match spawn_helper(file) {
                Ok(waiting) => *state = WaitState::Helper(waiting),
                Err(e) => return Some(Err(lock_failure(&self.layout, FileOp::Lock, e).into())),
            }
        }

        // 2. The helper's hand-off, within the limit.
        let WaitState::Helper(waiting) = &*state else {
            let e = io::Error::other("the lock was handed over already");
            return Some(Err(lock_failure(&self.layout, FileOp::Lock, e).into()));
        };
        let failed = |e| lock_failure(&self.layout, FileOp::Lock, e);
        let handed = match waiting.recv_timeout(limit) {
            Ok(locked) => locked.map_err(|e| failed(e).into()),
            Err(RecvTimeoutError::Timeout) => return None,
            Err(RecvTimeoutError::Disconnected) => Err(failed(io::Error::other(
                "the thread waiting for it ended without it",
            ))
            .into()),
        };
        *state = WaitState::Over;
        Some(handed)
    }
}

/// Hands `file` to a helper thread that blocks until it holds the lock, then sends it over;
/// if the receiver is gone (an abandoned wait), the helper drops the guard, which unlocks it.
fn spawn_helper(file: File) -> io::Result<Receiver<io::Result<MetaLock>>> {
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
    Ok(waiting)
}

/// The failure of `op` on `layout`'s `meta/lock`, named relative to the store's root.
fn lock_failure(layout: &Layout, op: FileOp, e: io::Error) -> IoFailure {
    let lock = layout.meta_lock();
    IoFailure::new(
        op,
        fs_ops::store_relative(layout.root(), lock.as_std_path()),
        e,
    )
}

/// Whether [`open_flock_file`] makes a missing file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Create {
    /// Makes it.
    IfMissing,
    /// Fails with `NotFound`, making nothing.
    Never,
}

/// Opens the file at `path` for reading and writing as `meta/lock` is opened (a regular file
/// only, no symlink followed, no FIFO waited for, checked after the open), so that it can be
/// flocked: the queue files of `meta/queue/` (the M18 D2 design §3.4).
pub(crate) fn open_flock_file(path: &Path, create: Create) -> io::Result<File> {
    if refuses(path) {
        return Err(fs_ops::not_a_regular_file(path));
    }
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(create == Create::IfMissing)
        .truncate(false);
    open_with(path, &options)
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

    /// A store root with `meta/`, as [`super::MetaLock::start`] needs it.
    fn meta_root() -> (tempfile::TempDir, crate::layout::Layout) {
        let dir = tempfile::tempdir().unwrap();
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let layout = crate::layout::Layout::new(root);
        fs::create_dir(layout.meta_dir()).unwrap();
        (dir, layout)
    }

    /// How many helper threads (`meta-lock-wait`) this process runs.
    fn helpers() -> usize {
        fs::read_dir("/proc/self/task")
            .unwrap()
            .filter(|task| {
                let comm = task.as_ref().unwrap().path().join("comm");
                fs::read_to_string(comm).is_ok_and(|name| name.trim() == "meta-lock-wait")
            })
            .count()
    }

    /// Waits up to 5 s for `helpers() == n`.
    fn helpers_become(n: usize) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if helpers() == n {
                return true;
            }
            thread::sleep(Duration::from_millis(1));
        }
        false
    }

    /// A wait started while the lock is held gets it once the holder lets go, within its limit;
    /// a wait whose limit passes first gives `None` and can wait again.
    #[test]
    fn a_lock_wait_hands_the_lock_over() {
        let (_dir, layout) = meta_root();
        let held = super::MetaLock::acquire(&layout, Duration::ZERO).unwrap();
        let super::Started::Waiting(wait) = super::MetaLock::start(&layout).unwrap() else {
            panic!("the lock is held: start must wait");
        };
        assert!(wait.wait(Duration::from_millis(20)).is_none(), "still held");
        drop(held);
        let _lock = wait
            .wait(Duration::from_secs(5))
            .expect("handed over")
            .unwrap();
        assert!(
            matches!(
                super::MetaLock::acquire(&layout, Duration::ZERO),
                Err(crate::StoreError::Busy)
            ),
            "the handed lock is held"
        );
    }

    /// A dropped wait leaves its helper parked; when the holder lets go the helper takes the
    /// lock and drops it at once, so the next acquire gets it.
    #[test]
    fn a_dropped_lock_wait_abandons_its_helper() {
        let (_dir, layout) = meta_root();
        let held = super::MetaLock::acquire(&layout, Duration::ZERO).unwrap();
        let super::Started::Waiting(wait) = super::MetaLock::start(&layout).unwrap() else {
            panic!("the lock is held: start must wait");
        };
        assert!(wait.wait(Duration::from_millis(5)).is_none(), "still held");
        assert!(helpers_become(1), "one helper waits");
        drop(wait);
        drop(held);
        assert!(helpers_become(0), "the abandoned helper ended");
        super::MetaLock::acquire(&layout, Duration::from_secs(5)).unwrap();
    }

    /// A start, and a wait with a zero limit, spawn no helper: a zero-timeout acquire is the try
    /// alone, as before.
    #[test]
    fn a_zero_timeout_start_spawns_no_helper() {
        let (_dir, layout) = meta_root();
        let _held = super::MetaLock::acquire(&layout, Duration::ZERO).unwrap();
        let super::Started::Waiting(wait) = super::MetaLock::start(&layout).unwrap() else {
            panic!("the lock is held: start must wait");
        };
        assert!(wait.wait(Duration::ZERO).is_none(), "still held");
        assert!(matches!(
            super::MetaLock::acquire(&layout, Duration::ZERO),
            Err(crate::StoreError::Busy)
        ));
        thread::sleep(Duration::from_millis(20));
        assert_eq!(helpers(), 0, "no helper was spawned");
    }

    /// [`super::open_flock_file`] with [`super::Create::Never`] fails on a missing file and makes
    /// none; with [`super::Create::IfMissing`] it makes it, and the file can be flocked.
    #[test]
    fn a_never_open_makes_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.req");
        let e = super::open_flock_file(&path, super::Create::Never).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::NotFound, "{e}");
        assert!(!fs::exists(&path).unwrap(), "nothing made");
        let file = super::open_flock_file(&path, super::Create::IfMissing).unwrap();
        file.try_lock().unwrap();
        let other = super::open_flock_file(&path, super::Create::Never).unwrap();
        assert!(other.try_lock().is_err(), "the first open holds the flock");
    }
}
