//! The process's counters at the store's file boundary (the M21 design §2.1): how many object
//! reads and writes, journal writes and syncs, and waits for and holds of `meta/lock` this
//! process made, and how long its syncs, waits and holds took.
//!
//! ```text
//!   StoreFs::read          objects/<2>/<62>        ─▶ object_reads
//!   StoreFs::link_noclobber objects/<2>/<62>, placed ─▶ object_writes
//!   StoreFs::append(_durable) meta/journal*.{log,off} ─▶ journal_writes (+ journal_syncs)
//!   StoreFs::sync_data, truncate  the same files    ─▶ journal_syncs, journal_sync_us
//! ```
//!
//! Each count is one relaxed atomic addition; a clock is read only around a sync.
//! The counters make no [`super::FsOps`] call, so the calls a store makes are what they were.
//!
//! An object read is a `get`'s read of its payload. [`crate::ObjectStore::stat`] is not counted:
//! it looks at the file through `std::fs`, off the seam. The lock's figures stay 0 for now.
//!
//! The scope is the process: the CLI runs one operation per process, the MCP server one request
//! at a time and `serve` a process per connection.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// The process's counters ([`Counters::process`]); [`Counters::snapshot`] reads them.
#[derive(Debug)]
#[non_exhaustive]
pub struct Counters {
    object_reads: AtomicU64,
    object_writes: AtomicU64,
    journal_writes: AtomicU64,
    journal_syncs: AtomicU64,
    journal_sync_us: AtomicU64,
    lock_waits: AtomicU64,
    lock_wait_us: AtomicU64,
    lock_hold_us: AtomicU64,
    lock_timeouts: AtomicU64,
}

/// The one set of counters of this process.
static PROCESS: Counters = Counters {
    object_reads: AtomicU64::new(0),
    object_writes: AtomicU64::new(0),
    journal_writes: AtomicU64::new(0),
    journal_syncs: AtomicU64::new(0),
    journal_sync_us: AtomicU64::new(0),
    lock_waits: AtomicU64::new(0),
    lock_wait_us: AtomicU64::new(0),
    lock_hold_us: AtomicU64::new(0),
    lock_timeouts: AtomicU64::new(0),
};

/// What the counters held at one moment; [`Counts::since`] is what moved between two.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub struct Counts {
    /// Reads of a file under `objects/`.
    pub object_reads: u64,
    /// New files linked under `objects/`.
    pub object_writes: u64,
    /// Appends to the journal's files.
    pub journal_writes: u64,
    /// Syncs of the journal's files, a durable append's among them.
    pub journal_syncs: u64,
    /// Their time, in microseconds: a durable append's whole call, its write included.
    pub journal_sync_us: u64,
    /// Starts of a wait for `meta/lock`, one per writer's turn.
    pub lock_waits: u64,
    /// Their time, until the lock was given or the wait dropped, in microseconds.
    pub lock_wait_us: u64,
    /// The time the lock was held, in microseconds.
    pub lock_hold_us: u64,
    /// Acquires that gave up with `Busy`.
    pub lock_timeouts: u64,
}

impl Counters {
    /// This process's counters.
    pub fn process() -> &'static Counters {
        &PROCESS
    }

    /// What they hold now, each read on its own (relaxed).
    pub fn snapshot(&self) -> Counts {
        let read = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        Counts {
            object_reads: read(&self.object_reads),
            object_writes: read(&self.object_writes),
            journal_writes: read(&self.journal_writes),
            journal_syncs: read(&self.journal_syncs),
            journal_sync_us: read(&self.journal_sync_us),
            lock_waits: read(&self.lock_waits),
            lock_wait_us: read(&self.lock_wait_us),
            lock_hold_us: read(&self.lock_hold_us),
            lock_timeouts: read(&self.lock_timeouts),
        }
    }

    /// One read of an object file.
    pub(crate) fn object_read(&self) {
        self.object_reads.fetch_add(1, Ordering::Relaxed);
    }

    /// One new object file.
    pub(crate) fn object_write(&self) {
        self.object_writes.fetch_add(1, Ordering::Relaxed);
    }

    /// One append to a journal file.
    pub(crate) fn journal_write(&self) {
        self.journal_writes.fetch_add(1, Ordering::Relaxed);
    }

    /// One sync of a journal file, begun at `began`.
    pub(crate) fn journal_sync(&self, began: Instant) {
        self.journal_syncs.fetch_add(1, Ordering::Relaxed);
        self.journal_sync_us
            .fetch_add(micros_since(began), Ordering::Relaxed);
    }
}

impl Counts {
    /// What moved from `earlier` to these counts, field by field; a field below `earlier`'s
    /// (the two swapped) is 0.
    #[must_use]
    pub fn since(&self, earlier: &Counts) -> Counts {
        Counts {
            object_reads: self.object_reads.saturating_sub(earlier.object_reads),
            object_writes: self.object_writes.saturating_sub(earlier.object_writes),
            journal_writes: self.journal_writes.saturating_sub(earlier.journal_writes),
            journal_syncs: self.journal_syncs.saturating_sub(earlier.journal_syncs),
            journal_sync_us: self.journal_sync_us.saturating_sub(earlier.journal_sync_us),
            lock_waits: self.lock_waits.saturating_sub(earlier.lock_waits),
            lock_wait_us: self.lock_wait_us.saturating_sub(earlier.lock_wait_us),
            lock_hold_us: self.lock_hold_us.saturating_sub(earlier.lock_hold_us),
            lock_timeouts: self.lock_timeouts.saturating_sub(earlier.lock_timeouts),
        }
    }
}

/// The microseconds since `began`, saturated.
fn micros_since(began: Instant) -> u64 {
    u64::try_from(began.elapsed().as_micros()).unwrap_or(u64::MAX)
}
