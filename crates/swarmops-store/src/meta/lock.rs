//! `meta/lock`: the exclusive advisory lock (the standard library's file lock, `flock` on
//! Linux) that serializes metadata writers, and committed readers, across threads and processes
//! (storage spec §3).
//!
//! # The lock file
//!
//! `meta/lock` is a regular file, which [`MetaLock::acquire`] makes if it is missing (mode 0600,
//! whatever the umask) and never writes. One that is anything else, as a crafted or restored store can hold, is refused before
//! it is opened, as every store file is: a FIFO, whose open for writing would wait for a reader
//! (and every writer with it, before its timeout even starts), a symlink, whatever it points at
//! (the open would follow it, and make the file it names, outside the store), a socket, a device
//! or a directory. The open itself follows no symlink (`O_NOFOLLOW`), does not wait for a FIFO's
//! reader (`O_NONBLOCK`, which leaves the lock as it is: only `LOCK_NB` keeps `flock` from
//! waiting), and checks what it opened (`fstat`), so that one put there between the check and the
//! open is refused at once too. [`MetaLock::refuses`] is the check alone, which `verify` reports.
//!
//! # Waiting: the kernel flock queue (not strictly FIFO), timeout via the lock file's helper
//!
//! [`MetaLock::acquire`] opens `meta/lock` afresh and first tries the lock without blocking,
//! so an uncontended writer takes it at once, on its own thread. If another handle holds it,
//! the wait goes to the kernel, through [`crate::flock_wait`]: the one helper thread this
//! process keeps for that lock file blocks in `flock(LOCK_EX)` (std's `File::lock`), and the
//! caller, a claim in the helper's queue, waits up to its timeout for the helper to hand the
//! locked file over. The kernel wakes the blocked waiters the moment the holder unlocks, so the
//! lock passes on at once instead of at a waiter's next poll. It is not strictly FIFO: the
//! waiters of the processes race for the lock, and a writer that asks again right after it
//! unlocked races with them; only the waits of one process are served in their order. With 32
//! writers that loop, the median wait is one rotation (31 transactions), or next to nothing
//! when the writer that has just committed asks again first; the longest is a few hundred
//! transactions (docs/perf.md, "M1 store fixes (7c)").
//!
//! A caller that times out returns [`StoreError::Busy`] and drops its claim, which takes it out
//! of the queue and nothing else: the helper stays in the kernel's queue (a blocked `flock`
//! cannot be called off) for the waits that are left and those to come. When it gets the lock
//! and no claim is left, it unlocks the file at once and ends; a lock handed over in the
//! instant the caller gives up is dropped with the claim. Either way a wait that gave up holds
//! the lock only for that instant. A holder that never lets go (a hung process) keeps that one
//! helper blocked in each waiting process, however many waits gave up.

use std::cell::{Cell, RefCell};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::error::{FileOp, IoFailure, StoreError};
use crate::flock_wait::{self, Claim, LockKey, Locked};
use crate::fs_ops;
use crate::fs_ops::counters::Counters;
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
    /// When it was given to its caller: its hold is counted from then ([`Counters`]). A lock
    /// the helper holds and no caller took is no `MetaLock`, and no hold.
    given: Instant,
}

impl MetaLock {
    /// Takes the lock on `layout`'s `meta/lock`, creating the file if it is missing (`meta/`
    /// must exist). If another handle holds it, waits in the kernel's queue (see the module
    /// documentation) until `timeout` has passed, then gives up with [`StoreError::Busy`]; a
    /// zero `timeout` gives up at once. A failure names `meta/lock`, relative to the store's
    /// root, and what failed: its open, or the lock. A `meta/lock` that is not a regular file
    /// fails its open at once, as damage (kind `InvalidData`; see the module documentation).
    pub fn acquire(layout: &Layout, timeout: Duration) -> Result<MetaLock, StoreError> {
        let wait = match Self::start(layout)? {
            Started::Held(lock) => return Ok(lock),
            Started::Waiting(wait) => wait,
        };

        // A wait that gives up is a timeout of the counters; its time ends with the wait.
        wait.wait(timeout).unwrap_or_else(|| {
            Counters::process().lock_timeout();
            Err(StoreError::Busy)
        })
    }

    /// The first half of [`MetaLock::acquire`]: opens `meta/lock` and tries the lock without
    /// blocking. [`Started::Held`] if it was free; else [`Started::Waiting`], whose
    /// [`LockWait::wait`] waits for it in the kernel's queue. Starts no thread.
    ///
    /// Every start is one wait of the process's [`Counters`], timed until the lock is given or
    /// the [`LockWait`] is dropped: a lone writer's, a queue lead's and a queue member's alike.
    pub(crate) fn start(layout: &Layout) -> Result<Started, StoreError> {
        let counters = Counters::process();
        let began = Instant::now();
        counters.lock_wait();

        // 1. The open: a failure ends the wait.
        let lock = layout.meta_lock();
        let file = match open_lock_file(lock.as_std_path()) {
            Ok(file) => file,
            Err(e) => {
                counters.lock_waited(began);
                return Err(lock_failure(layout, FileOp::Open, e).into());
            }
        };

        // 2. The try: only a taken lock leaves the wait open.
        match file.try_lock() {
            Ok(()) => {
                counters.lock_waited(began);
                Ok(Started::Held(Self::given(file)))
            }
            Err(TryLockError::WouldBlock) => Ok(Started::Waiting(LockWait {
                layout: layout.clone(),
                state: RefCell::new(WaitState::Idle(file)),
                began,
                over: Cell::new(false),
            })),
            Err(TryLockError::Error(e)) => {
                counters.lock_waited(began);
                Err(lock_failure(layout, FileOp::Lock, e).into())
            }
        }
    }

    /// The lock on `file`, given to its caller now: its hold is counted from here.
    fn given(file: File) -> MetaLock {
        MetaLock {
            file,
            given: Instant::now(),
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
    options
        .write(true)
        .create(true)
        .truncate(false)
        .mode(fs_ops::FILE_MODE);
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
        Counters::process().lock_held(self.given);
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
/// above zero claims the lock from the lock file's helper thread, with its open file, and each
/// wait after it waits on that claim. Dropping it drops the claim, as a timed-out
/// [`MetaLock::acquire`] does: no thread is the wait's own (see the module documentation). Its
/// time is counted until it gives the lock or is dropped ([`Counters`]).
#[derive(Debug)]
pub(crate) struct LockWait {
    layout: Layout,
    state: RefCell<WaitState>,
    /// When [`MetaLock::start`] began it.
    began: Instant,
    /// Whether its time is counted already: it gave the lock.
    over: Cell<bool>,
}

/// Where a [`LockWait`] is.
#[derive(Debug)]
enum WaitState {
    /// No claim yet: the open file, not locked.
    Idle(File),
    /// A claim in the queue of the lock file's helper, which hands the lock over to it.
    Claimed(Claim),
    /// The lock was handed over, or the wait failed.
    Over,
}

impl LockWait {
    /// Waits up to `limit` for the lock: `None` if it is still taken then, else the lock or why
    /// waiting for it failed. A zero `limit` on a wait that has no claim yet makes none, so it
    /// starts no helper.
    pub(crate) fn wait(&self, limit: Duration) -> Option<Result<MetaLock, StoreError>> {
        let mut state = self.state.borrow_mut();

        // 1. The first wait with a limit claims the lock from the lock file's helper.
        if let WaitState::Idle(_) = &*state {
            if limit.is_zero() {
                return None;
            }
            let WaitState::Idle(file) = std::mem::replace(&mut *state, WaitState::Over) else {
                unreachable!("the state was idle");
            };
            match LockKey::of(&file).and_then(|key| flock_wait::claim(key, file)) {
                Ok(claim) => *state = WaitState::Claimed(claim),
                Err(e) => return Some(Err(self.failed(e))),
            }
        }

        // 2. The helper's hand-off, within the limit.
        let WaitState::Claimed(claim) = &*state else {
            let e = io::Error::other("the lock was handed over already");
            return Some(Err(self.failed(e)));
        };
        let handed = match claim.wait(limit)? {
            Ok(locked) => Ok(self.give(locked)),
            Err(e) => Err(self.failed(e)),
        };
        *state = WaitState::Over;
        Some(handed)
    }

    /// The lock the helper handed over, given to the caller: the wait's time ends and the
    /// hold's begins.
    fn give(&self, locked: Locked) -> MetaLock {
        Counters::process().lock_waited(self.began);
        self.over.set(true);
        MetaLock::given(locked.into_file())
    }

    /// Why waiting for the lock failed, as a failure of the lock on `meta/lock`.
    fn failed(&self, e: io::Error) -> StoreError {
        lock_failure(&self.layout, FileOp::Lock, e).into()
    }
}

impl Drop for LockWait {
    fn drop(&mut self) {
        // A wait that gave no lock ends here: given up, answered, or failed.
        if !self.over.get() {
            Counters::process().lock_waited(self.began);
        }
    }
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
        .truncate(false)
        .mode(fs_ops::FILE_MODE);
    open_with(path, &options)
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

    use crate::flock_wait::{self, LockKey};
    use crate::layout::Layout;

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
    fn meta_root() -> (tempfile::TempDir, Layout) {
        let dir = tempfile::tempdir().unwrap();
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let layout = Layout::new(root);
        fs::create_dir(layout.meta_dir()).unwrap();
        (dir, layout)
    }

    /// The key of `layout`'s lock file, which exists: the one its waits claim under.
    fn key(layout: &Layout) -> LockKey {
        LockKey::of(&File::open(layout.meta_lock()).unwrap()).unwrap()
    }

    /// The helpers are counted by lock file: one that waits for another store's lock, as a test
    /// running beside this one in the process starts, is not this store's.
    #[test]
    fn helpers_are_counted_per_lock_file() {
        let (_dir, layout) = meta_root();
        let (_other_dir, other) = meta_root();
        let _held = super::MetaLock::acquire(&layout, Duration::ZERO).unwrap();
        let other_held = super::MetaLock::acquire(&other, Duration::ZERO).unwrap();
        let super::Started::Waiting(wait) = super::MetaLock::start(&other).unwrap() else {
            panic!("the lock is held: start must wait");
        };
        assert!(wait.wait(Duration::from_millis(1)).is_none(), "still held");
        let waiting = flock_wait::waiting(&key(&other));
        assert_eq!(waiting, Some(1), "the other store's helper, one claim");
        let waiting = flock_wait::waiting(&key(&layout));
        assert_eq!(waiting, None, "it is not this store's");
        drop(wait);
        drop(other_held);
        flock_wait::settled(&key(&other));
        assert_eq!(flock_wait::waiting(&key(&other)), None, "the helper ended");
    }

    /// A wait started while the lock is held gets it once the holder lets go, within its limit;
    /// a wait whose limit passes first gives `None` and can wait again. The helper ends with the
    /// hand-off: no claim is left.
    #[test]
    fn a_lock_wait_hands_the_lock_over() {
        let (_dir, layout) = meta_root();
        let held = super::MetaLock::acquire(&layout, Duration::ZERO).unwrap();
        let super::Started::Waiting(wait) = super::MetaLock::start(&layout).unwrap() else {
            panic!("the lock is held: start must wait");
        };
        assert!(wait.wait(Duration::from_millis(1)).is_none(), "still held");
        drop(held);
        let _lock = wait
            .wait(Duration::from_secs(60))
            .expect("handed over")
            .unwrap();
        assert!(
            matches!(
                super::MetaLock::acquire(&layout, Duration::ZERO),
                Err(crate::StoreError::Busy)
            ),
            "the handed lock is held"
        );
        flock_wait::settled(&key(&layout));
        assert_eq!(flock_wait::waiting(&key(&layout)), None, "no helper left");
    }

    /// A dropped wait leaves the lock file's helper, with no claim, for as long as the holder
    /// holds; when the holder lets go the helper takes the lock, lets it go at once and ends,
    /// so the next acquire gets it without a wait.
    #[test]
    fn a_dropped_lock_wait_leaves_one_helper_until_the_holder_lets_go() {
        let (_dir, layout) = meta_root();
        let held = super::MetaLock::acquire(&layout, Duration::ZERO).unwrap();
        let key = key(&layout);
        for _ in 0..3 {
            let super::Started::Waiting(wait) = super::MetaLock::start(&layout).unwrap() else {
                panic!("the lock is held: start must wait");
            };
            assert!(wait.wait(Duration::from_millis(1)).is_none(), "still held");
            assert_eq!(flock_wait::waiting(&key), Some(1), "one claim");
            drop(wait);
            assert_eq!(flock_wait::waiting(&key), Some(0), "the helper stays");
        }
        drop(held);
        flock_wait::settled(&key);
        assert_eq!(flock_wait::waiting(&key), None, "the helper ended");
        super::MetaLock::acquire(&layout, Duration::ZERO).unwrap();
    }

    /// A start, and a wait with a zero limit, start no helper: a zero-timeout acquire is the try
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
        let waiting = flock_wait::waiting(&key(&layout));
        assert_eq!(waiting, None, "no helper was started");
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
