//! SwarmOps store: content-addressed object files plus metadata.
//!
//! Objects live in loose, self-verifying files ([`FsObjectStore`]) under a store root
//! whose paths [`Layout`] computes. Metadata lives in `meta/` ([`meta`]), behind the
//! [`MetaStore`] facade: a CRC-framed, append-only [`Journal`] is its source of truth,
//! [`MetaLock`] serializes its writers, the columnar changeset [`Index`] answers lookups by
//! id, id prefix and delta hash, and ref files hold the refs. The object store's filesystem
//! access and the metadata store's writes go through [`FsOps`], so tests can count calls or
//! crash the process mid-write ([`CrashAtFs`]).

pub mod error;
pub mod fs_objects;
pub mod fs_ops;
pub mod layout;
pub mod meta;

pub use error::StoreError;
pub use fs_objects::{FsObjectStore, ObjectStore};
pub use fs_ops::{CRASH_AT_ENV, CrashAtFs, FS_CALLS_ENV, FsOps, LinkOutcome, RealFs};
pub use layout::{Layout, is_native_linux_fs};
pub use meta::{
    CasResult, ChangesetMeta, CommitInfo, Frame, Index, InsertOutcome, Journal, JournalEntry,
    MetaLock, MetaRead, MetaStore, MetaWrite, Record, RefFile, RefMoveEntry, RefState, SortedView,
};
