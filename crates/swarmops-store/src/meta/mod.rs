//! The native metadata store (storage spec §3): pure Rust, no external database.
//!
//! - [`Journal`] (`meta/journal.log`, `journal.off`, `journal.ckpt`): CRC-framed,
//!   append-only frames, one per write transaction. It is the source of truth; every other
//!   metadata file is a projection of it.
//! - [`Record`] and [`Frame`]: what the journal holds, in a fixed byte layout.
//! - [`MetaLock`] (`meta/lock`): the flock that serializes writers.
//!
//! A write transaction holds the lock throughout: [`Journal::truncate_torn_tail`], replay
//! of the frames past [`Journal::ckpt`] into the materialized files, one
//! [`Journal::append_frame`] (the commit point), materialization, then
//! [`Journal::set_ckpt`]. Readers take no lock.

pub mod frame;
pub mod journal;
pub mod lock;

pub use frame::{Frame, Record};
pub use journal::Journal;
pub use lock::MetaLock;
