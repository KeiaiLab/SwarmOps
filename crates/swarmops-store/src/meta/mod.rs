//! The native metadata store (storage spec §3): pure Rust, no external database.
//!
//! - [`Journal`] (`meta/journal.log`, `journal.off`, `journal.ckpt`): CRC-framed,
//!   append-only frames, one per write transaction. It is the source of truth; every other
//!   metadata file is a projection of it.
//! - [`Record`] and [`Frame`]: what the journal holds, in a fixed byte layout.
//! - [`MetaLock`] (`meta/lock`): the flock that serializes writers.
//! - [`Index`] (`meta/idx/*.col`, `meta/parents.log`, `meta/sorted/`): the columnar
//!   changeset index, one [`ChangesetMeta`] record per changeset in journal order, with
//!   [`SortedView`]s by id and by delta hash for `O(log n)` lookups.
//!
//! A write transaction holds the lock throughout: [`Journal::truncate_torn_tail`] and
//! [`Index::repair_tail`], replay of the frames past [`Journal::ckpt`] into the materialized
//! files, one [`Journal::append_frame`] (the commit point), then materialization (for the
//! index, one [`Index::append`] per changeset). A transaction is durable once its frame is:
//! the journal's syncs are its only per-transaction fsyncs. [`Index::sync`] and the
//! [`Journal::set_ckpt`] after it are batched instead: every 32 frames past the checkpoint,
//! before and after [`Index::compact`], and on an explicit flush or close. Readers take no
//! lock and overlay the frames past the checkpoint (storage spec §3).

pub mod frame;
pub mod index;
pub mod journal;
pub mod lock;
mod parents;
pub mod sorted;

pub use frame::{Frame, Record};
pub use index::{ChangesetMeta, Index};
pub use journal::Journal;
pub use lock::MetaLock;
pub use sorted::SortedView;
