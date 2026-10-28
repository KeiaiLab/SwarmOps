//! The native metadata store (storage spec §3): pure Rust, no external database.
//!
//! - [`MetaStore`] is the facade: [`MetaStore::write`] starts a write transaction
//!   ([`MetaWrite`]), [`MetaStore::read`] takes a lock-free snapshot ([`MetaRead`]).
//! - [`Journal`] (`meta/journal.log`, `journal.off`, `journal.ckpt`): CRC-framed,
//!   append-only frames, one per write transaction. It is the source of truth; every other
//!   metadata file is a projection of it.
//! - [`Record`] and [`Frame`]: what the journal holds, in a fixed byte layout.
//! - [`MetaLock`] (`meta/lock`): the flock that serializes writers and committed readers.
//! - [`Index`] (`meta/idx/*.col`, `meta/parents.log`, `meta/sorted/`): the columnar
//!   changeset index, one [`ChangesetMeta`] record per changeset in journal order, with
//!   [`SortedView`]s by id and by delta hash for `O(log n)` lookups.
//! - Refs (`meta/refs/main`, `meta/refs/release/<x>`): one file per ref, `<64 hex>\n<version>\n`
//!   ([`RefState`]), replaced atomically. [`RefKind`] says what a name names.
//! - [`fsck`], [`upgrade`] and [`MetaCheck`]: the rebuild of the journal's projections, the
//!   upgrade to FORMAT 2, and the metadata checks of verify.
//! - [`DerivedStore`] (`meta/derived/`): values derived from immutable objects, one checksummed
//!   file per [`DerivedSlot`]. No projection of the journal but a cache beside it: written without
//!   the lock and without a sync, and derived again when a file is missing or damaged.
//!
//! A write transaction holds the lock throughout: [`Journal::truncate_torn_tail`] and
//! [`Index::repair_tail`], replay of the frames past [`Journal::ckpt`] into the materialized
//! files, one [`Journal::append_frame`] (the commit point), then materialization (for the
//! index, one [`Index::append_batch`] of the frame's changesets; for each moved ref, its file
//! replaced). A transaction is durable once its frame is: the journal's syncs (and a moved
//! ref's file replacement) are its only per-transaction fsyncs. [`Index::sync`] and the
//! [`Journal::set_ckpt`] after it are batched instead: every 32 frames past the checkpoint,
//! with [`Index::compact`] (which syncs the index before it writes the sorted views), and on
//! an explicit [`MetaStore::flush`]. Local readers ([`MetaStore::read`]) take no lock and overlay
//! the frames past the checkpoint (storage spec §3); committed readers
//! ([`MetaStore::read_committed`]) take it for their tail check and their snapshot.

pub mod check;
mod column;
pub mod derived;
pub mod frame;
pub mod fsck;
pub mod index;
pub mod journal;
pub mod lock;
mod parents;
pub mod refs;
mod replay;
pub mod sorted;
pub mod store;
pub mod upgrade;

/// How many entries of `journal.off`, an index column or a sorted view a bounded read takes at
/// once (the M6 design §5): no read allocates by a file's length.
pub(crate) const READ_WINDOW: u64 = 4_096;

/// The most changesets a lookup by id prefix or by delta hash returns; past it the caller is
/// told the answer was cut (the M6 design §5).
pub(crate) const MAX_LOOKUP_HITS: usize = 4_096;

pub use check::MetaCheck;
pub use derived::{DerivedSlot, DerivedStore, ListedValue, StoredValue};
pub use frame::{BadRecord, Frame, Record};
pub use fsck::Rebuilt;
pub use index::{ChangesetMeta, Index};
pub use journal::{Journal, LogCut, TailRepair};
pub use lock::MetaLock;
pub use refs::{RefFile, RefKind, RefState};
pub use sorted::SortedView;
pub use store::{
    CasResult, CommitInfo, InsertOutcome, JournalEntry, MetaRead, MetaStore, MetaWrite, Recovered,
    RefMoveEntry,
};
pub use upgrade::Upgraded;
