//! A bounded wait for a file lock (`flock` on Linux) that costs its process one helper thread for
//! each lock file, however many waits there are and however many of them give up.
//!
//! A blocked `flock` cannot be called off without `unsafe`, so a wait with a timeout cannot block
//! on its own thread: a helper thread blocks in the kernel's queue for it, and the caller waits
//! for the helper's hand-off, up to its limit. Here every wait of the process for one lock file
//! is a [`Claim`] on the one helper that file has:
//!
//! ```text
//!   claim(key, file) ──▶ table[key]: claims [c1, c2, …], spare files, one helper thread
//!                                                                         │ File::lock (blocks)
//!   Claim::wait(limit) ◀── the lock, to the oldest claim still there ◀────┘
//!   Claim dropped      ──▶ out of the queue; the helper stays while the lock is held
//!   lock free, no claim ─▶ the helper unlocks, leaves the table and ends
//! ```
//!
//! What follows from it:
//!
//! - A wait that gives up takes only itself away: it leaves the queue and owns no thread.
//! - A process has at most one helper for each lock file it waits on, and only while that lock
//!   is held by another handle or claimed. A holder that never lets go (a hung process) keeps
//!   that one helper blocked, whatever the number of waits that gave up meanwhile.
//! - The waits of one process for one file are served oldest first, and the process has one
//!   waiter in the kernel's queue for that file. Between processes the kernel's queue decides,
//!   as it did.
//! - A lock the helper gets when nobody claims it any more is let go at once.
//!
//! The process's [`Counters`] count the helpers started and ended (`flock_helpers_started`,
//! `flock_helpers_ended`); those alive are the difference.
//!
//! The module opens no path: the caller opens the lock file, tries the lock on its own thread
//! first, and names the file with a [`LockKey`] of its making.

use std::collections::{BTreeMap, VecDeque};
use std::fs::File;
use std::io;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::fs_ops::counters::Counters;

/// The name of a helper thread, whatever lock file it waits for.
const HELPER_NAME: &str = "flock-wait";

/// Which lock file a wait is for, as the kernel tells files apart: two opens of one file have
/// the same key, by whatever path each was opened.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[non_exhaustive]
pub struct LockKey(u64, u64);

impl LockKey {
    /// The key of the file `file` is open on: its device and inode.
    #[cfg(unix)]
    pub fn of(file: &File) -> io::Result<LockKey> {
        use std::os::unix::fs::MetadataExt as _;

        let meta = file.metadata()?;
        Ok(LockKey(meta.dev(), meta.ino()))
    }
}

/// A lock file that holds its exclusive lock: dropping it unlocks the file and closes it.
#[derive(Debug)]
#[non_exhaustive]
pub struct Locked(Option<File>);

impl Locked {
    /// The open file, still locked: unlocking it is the caller's from here.
    pub fn into_file(mut self) -> File {
        match self.0.take() {
            Some(file) => file,
            None => unreachable!("the file is taken only here and in the drop"),
        }
    }
}

impl Drop for Locked {
    fn drop(&mut self) {
        // Closing the file would release the lock too, but only once no forked child still
        // shares the descriptor; unlocking releases it at once. An error leaves it to the close.
        if let Some(file) = self.0.take() {
            let _ = file.unlock();
        }
    }
}

/// One wait for a lock file, in the queue of that file's helper from [`claim`] until it is
/// dropped or handed the lock. Dropping it takes it out of the queue; a lock already handed to
/// it and not yet taken with [`Claim::wait`] is let go with it.
#[derive(Debug)]
#[non_exhaustive]
pub struct Claim {
    waits: Waits,
    key: LockKey,
    /// Its place in the table: no two claims of a process share one.
    id: u64,
    handoff: Receiver<io::Result<Locked>>,
}

impl Claim {
    /// Waits up to `limit` for the lock: `None` if it is still taken then (the claim keeps its
    /// place, and can wait again), else the lock, or why waiting for it failed. A claim is handed
    /// the lock once; a wait after that is an error.
    pub fn wait(&self, limit: Duration) -> Option<io::Result<Locked>> {
        match self.handoff.recv_timeout(limit) {
            Ok(handed) => Some(handed),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => Some(Err(io::Error::other(
                "the thread waiting for it ended without it",
            ))),
        }
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        let mut table = self.waits.table();
        let surplus = table.waits.get_mut(&self.key).and_then(|entry| {
            entry.claims.retain(|(id, _)| *id != self.id);
            // The helper blocks on one file and needs one more for each claim after the first.
            if entry.spares.len() > entry.claims.len() {
                entry.spares.pop()
            } else {
                None
            }
        });
        drop(table);
        drop(surplus);
    }
}

/// Joins the queue for the lock file `key` names with `file`, an open of that file that holds no
/// lock: the file's helper is started if it has none, and blocks in the kernel's queue on `file`
/// or on an earlier claim's. The caller has tried the lock on its own thread already; this is
/// the wait. Fails only if the helper thread cannot be started.
pub fn claim(key: LockKey, file: File) -> io::Result<Claim> {
    PROCESS.claim(key, file)
}

/// How many claims wait for the lock file `key` names: `None` if the process has no helper for
/// it, `Some(n)` if it has one, with `n` claims in its queue (0: every wait gave up, and the
/// helper stays until the lock is free).
pub fn waiting(key: &LockKey) -> Option<usize> {
    PROCESS.waiting(key)
}

/// Blocks until the process has no helper for the lock file `key` names, and joins the helpers
/// that have ended: at once if there is none, and without end while the lock stays held or
/// claimed. For a test, and for a process that wants its threads gone before it goes on.
pub fn settled(key: &LockKey) {
    PROCESS.settled(key);
}

/// The process's waits, which [`claim`], [`waiting`] and [`settled`] act on: their helpers are
/// the ones the process's [`Counters`] count.
static PROCESS: LazyLock<Waits> = LazyLock::new(|| Waits::new(Some(Counters::process())));

/// A table of waits and its helpers: the process has one ([`PROCESS`]), and a test makes its own
/// so that it counts no other test's helper.
#[derive(Clone, Debug)]
struct Waits(Arc<Shared>);

/// What the claims and the helpers of one [`Waits`] share.
#[derive(Debug)]
struct Shared {
    table: Mutex<Table>,
    /// Notified when a helper has left the table.
    left: Condvar,
    /// Where a helper's start and end are counted too, if anywhere.
    counters: Option<&'static Counters>,
}

/// The waits, by lock file. No thread blocks on a lock file while it holds this table's mutex.
#[derive(Debug, Default)]
struct Table {
    waits: BTreeMap<LockKey, Entry>,
    /// The helpers that have left the table and are not joined yet.
    ended: Vec<JoinHandle<()>>,
    /// The next claim's id.
    next_id: u64,
    /// Helpers started.
    started: u64,
    /// Helpers joined after they ended.
    joined: u64,
}

/// One lock file's waits: there from its helper's start until the helper ends.
#[derive(Debug)]
struct Entry {
    /// The claims that wait, oldest first, each with the channel its lock is sent on.
    claims: VecDeque<(u64, SyncSender<io::Result<Locked>>)>,
    /// Opens of the lock file that later claims brought, not locked: the helper blocks on one
    /// after it has handed its own, locked, to a claim.
    spares: Vec<File>,
    /// The helper's handle, which it moves to [`Table::ended`] when it ends.
    thread: Option<JoinHandle<()>>,
}

impl Waits {
    /// An empty table, whose helpers `counters` count as well.
    fn new(counters: Option<&'static Counters>) -> Waits {
        Waits(Arc::new(Shared {
            table: Mutex::new(Table::default()),
            left: Condvar::new(),
            counters,
        }))
    }

    /// The table, locked. A poisoned mutex is taken as it is: every change under it is whole
    /// before anything that can panic.
    fn table(&self) -> MutexGuard<'_, Table> {
        self.0.table.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// [`claim`], on this table.
    fn claim(&self, key: LockKey, file: File) -> io::Result<Claim> {
        let (sender, handoff) = mpsc::sync_channel(1);
        let mut table = self.table();
        let id = table.next_id;
        table.next_id += 1;
        if let Some(entry) = table.waits.get_mut(&key) {
            entry.claims.push_back((id, sender));
            entry.spares.push(file);
        } else {
            // The helper reads its entry only under the mutex, which is held until the entry
            // is in the table.
            let waits = self.clone();
            let thread = thread::Builder::new()
                .name(HELPER_NAME.to_owned())
                .spawn(move || waits.help(key, file))?;
            let entry = Entry {
                claims: VecDeque::from([(id, sender)]),
                spares: Vec::new(),
                thread: Some(thread),
            };
            table.waits.insert(key, entry);
            table.started += 1;
            if let Some(counters) = self.0.counters {
                counters.flock_helper_started();
            }
        }
        self.join_ended(table);
        Ok(Claim {
            waits: self.clone(),
            key,
            id,
            handoff,
        })
    }

    /// The helper's work for the lock file `key` names, begun on `file`: it blocks until it
    /// holds the lock, hands it to the oldest claim, and blocks again on a spare while a claim
    /// is left; with none left it ends.
    fn help(&self, key: LockKey, mut file: File) {
        loop {
            // 1. In the kernel's queue until the lock is this file's. The mutex is not held.
            let locked = lock_blocking(&file).map(|()| Locked(Some(file)));

            // 2. To the oldest claim that still listens. A lock nobody takes is let go here.
            let mut table = self.table();
            let Some(entry) = table.waits.get_mut(&key) else {
                // Not reached: only this helper removes its entry.
                return;
            };
            let mut unclaimed = Some(locked);
            while let Some(offer) = unclaimed.take() {
                let Some((_, claim)) = entry.claims.pop_front() else {
                    drop(offer);
                    break;
                };
                // A claim is sent to once, so its channel of one has room: no send blocks.
                if let Err(TrySendError::Full(back) | TrySendError::Disconnected(back)) =
                    claim.try_send(offer)
                {
                    unclaimed = Some(back);
                }
            }

            // 3. A claim is left: wait again, on a file a later claim brought.
            if !entry.claims.is_empty()
                && let Some(spare) = entry.spares.pop()
            {
                file = spare;
                continue;
            }

            // 4. None is left: out of the table, the handle left for whoever joins next.
            if let Some(mut entry) = table.waits.remove(&key)
                && let Some(thread) = entry.thread.take()
            {
                table.ended.push(thread);
            }
            if let Some(counters) = self.0.counters {
                counters.flock_helper_ended();
            }
            drop(table);
            self.0.left.notify_all();
            return;
        }
    }

    /// [`waiting`], on this table.
    fn waiting(&self, key: &LockKey) -> Option<usize> {
        self.table().waits.get(key).map(|entry| entry.claims.len())
    }

    /// [`settled`], on this table.
    fn settled(&self, key: &LockKey) {
        let mut table = self.table();
        while table.waits.contains_key(key) {
            table = (self.0.left.wait(table)).unwrap_or_else(PoisonError::into_inner);
        }
        self.join_ended(table);
    }

    /// Joins the helpers that have ended, after letting `table` go: each has left the table and
    /// is returning, so no join waits on a lock.
    fn join_ended(&self, mut table: MutexGuard<'_, Table>) {
        let ended = std::mem::take(&mut table.ended);
        drop(table);
        for thread in ended {
            // A helper that panicked has ended too.
            let _ = thread.join();
            self.table().joined += 1;
        }
    }

    /// How many helpers this table started, and how many of them were joined after they ended.
    #[cfg(test)]
    fn counts(&self) -> (u64, u64) {
        let table = self.table();
        (table.started, table.joined)
    }
}

/// Blocks until `file` holds the exclusive lock, asking again when a signal interrupts the wait
/// (std's `File::lock` reports `EINTR`).
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
    use std::fs::{File, TryLockError};
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use super::{Claim, LockKey, Locked, Waits};

    /// A wait that only a held lock outlasts: the test holds the lock itself, so it can end
    /// one way alone.
    const SHORT: Duration = Duration::from_millis(1);

    /// A wait no test reaches once the lock is let go: it only bounds a hang.
    const LONG: Duration = Duration::from_secs(60);

    /// A lock file in a directory of its own, a table of its own, and the file's key.
    struct Fixture {
        _dir: tempfile::TempDir,
        path: PathBuf,
        waits: Waits,
        key: LockKey,
    }

    impl Fixture {
        fn new() -> Fixture {
            Fixture::in_table(Waits::new(None))
        }

        /// A lock file of its own whose waits are in `waits`.
        fn in_table(waits: Waits) -> Fixture {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("lock");
            let key = LockKey::of(&File::create(&path).unwrap()).unwrap();
            Fixture {
                _dir: dir,
                path,
                waits,
                key,
            }
        }

        /// The lock, taken on a fresh open: it is free.
        fn hold(&self) -> File {
            let file = open(&self.path);
            file.try_lock().expect("the lock is free");
            file
        }

        /// A claim on a fresh open.
        fn claim(&self) -> Claim {
            self.waits.claim(self.key, open(&self.path)).unwrap()
        }

        /// Whether a fresh open finds the lock taken.
        fn taken(&self) -> bool {
            match open(&self.path).try_lock() {
                Ok(()) => false,
                Err(TryLockError::WouldBlock) => true,
                Err(TryLockError::Error(e)) => panic!("try the lock: {e}"),
            }
        }
    }

    /// A fresh open of the lock file at `path`.
    fn open(path: &Path) -> File {
        File::options().write(true).open(path).unwrap()
    }

    /// The lock `claim` is handed within [`LONG`].
    fn handed(claim: &Claim) -> Locked {
        claim.wait(LONG).expect("handed over").unwrap()
    }

    /// Sixty-four waits for a held lock, each given up, start one helper between them and leave
    /// it no claim; once the holder lets go the helper ends, with the lock free.
    #[test]
    fn sixty_four_waits_that_give_up_start_one_helper() {
        let lock = Fixture::new();
        let held = lock.hold();
        for wait in 0..64 {
            let claim = lock.claim();
            assert!(claim.wait(SHORT).is_none(), "wait {wait}: still held");
            drop(claim);
            assert_eq!(lock.waits.counts(), (1, 0), "wait {wait}: one helper");
            assert_eq!(lock.waits.waiting(&lock.key), Some(0), "wait {wait}");
        }

        held.unlock().unwrap();
        lock.waits.settled(&lock.key);
        assert_eq!(lock.waits.counts(), (1, 1), "the helper ended: joined");
        assert_eq!(lock.waits.waiting(&lock.key), None);
        assert!(!lock.taken(), "the helper kept the lock nobody claimed");
    }

    /// Two claims get the lock in the order they were made: the second only once the first
    /// has let it go.
    #[test]
    fn claims_are_served_oldest_first() {
        let lock = Fixture::new();
        let held = lock.hold();
        let a = lock.claim();
        let b = lock.claim();
        assert_eq!(lock.waits.waiting(&lock.key), Some(2));

        held.unlock().unwrap();
        let first = handed(&a);
        assert!(b.wait(SHORT).is_none(), "the first claim holds the lock");
        assert_eq!(lock.waits.waiting(&lock.key), Some(1));
        drop(first);
        let second = handed(&b);
        assert!(lock.taken(), "the second claim holds the lock");
        drop(second);

        lock.waits.settled(&lock.key);
        assert_eq!(lock.waits.counts(), (1, 1), "one helper served both");
        assert!(!lock.taken(), "let go");
    }

    /// A claim dropped before the lock is free is not handed it: the next one is.
    #[test]
    fn a_claim_that_left_is_passed_over() {
        let lock = Fixture::new();
        let held = lock.hold();
        let a = lock.claim();
        let b = lock.claim();
        drop(a);
        assert_eq!(lock.waits.waiting(&lock.key), Some(1));

        held.unlock().unwrap();
        let locked = handed(&b);
        lock.waits.settled(&lock.key);
        assert!(lock.taken(), "the claim that stayed holds the lock");
        drop(locked);
        assert!(!lock.taken(), "let go");
    }

    /// A lock handed to a claim that never takes it is let go when the claim is dropped.
    #[test]
    fn a_lock_sent_to_a_claim_that_then_leaves_is_let_go() {
        let lock = Fixture::new();
        let held = lock.hold();
        let a = lock.claim();

        held.unlock().unwrap();
        lock.waits.settled(&lock.key);
        assert!(lock.taken(), "the lock waits in the claim's channel");
        drop(a);
        assert!(!lock.taken(), "the dropped claim kept its lock");
    }

    /// The lock a claim was handed is the caller's as a file too, still locked.
    #[test]
    fn a_handed_lock_is_still_held_as_a_file() {
        let lock = Fixture::new();
        let held = lock.hold();
        let a = lock.claim();
        held.unlock().unwrap();
        let file = handed(&a).into_file();
        assert!(lock.taken(), "the file holds the lock");
        let again = a.wait(SHORT).expect("a claim is handed the lock once");
        assert!(again.is_err(), "a second lock for one claim");
        file.unlock().unwrap();
        assert!(!lock.taken(), "unlocked by its caller");
    }

    /// Each lock file has a helper of its own, and one ends without the other.
    #[test]
    fn two_files_have_two_helpers() {
        let one = Fixture::new();
        let two = Fixture::in_table(one.waits.clone());
        let waits = &one.waits;
        let (held_one, held_two) = (one.hold(), two.hold());
        let (a, b) = (one.claim(), two.claim());
        assert_eq!(waits.counts(), (2, 0), "a helper for each file");

        held_one.unlock().unwrap();
        drop(handed(&a));
        waits.settled(&one.key);
        assert_eq!(waits.counts(), (2, 1), "the first file's helper ended");
        assert_eq!(waits.waiting(&one.key), None);
        assert_eq!(waits.waiting(&two.key), Some(1), "the second's waits on");

        held_two.unlock().unwrap();
        drop(handed(&b));
        waits.settled(&two.key);
        assert_eq!(waits.counts(), (2, 2), "both ended");
    }
}
