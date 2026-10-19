//! SwarmOps store: content-addressed object files plus metadata.
//!
//! Objects live in loose, self-verifying files ([`FsObjectStore`]) under a store root
//! whose paths [`Layout`] computes. Metadata lives in `meta/` ([`meta`]), behind the
//! [`MetaStore`] facade: a CRC-framed, append-only [`Journal`] is its source of truth,
//! [`MetaLock`] serializes its writers, the columnar changeset [`Index`] answers lookups by
//! id, id prefix and delta hash, and ref files hold the refs. The object store's filesystem
//! access and the metadata store's writes go through [`FsOps`], so tests can count calls and,
//! in a build with the `test-hooks` feature, crash the process mid-write (`CrashAtFs`), fail
//! calls (`FailAtFs`) or check the order of the writes (`OrderCheckFs`).

pub mod error;
pub mod fs_objects;
pub mod fs_ops;
pub mod layout;
pub mod meta;
#[cfg(feature = "test-hooks")]
pub mod order_check;

pub use error::{LimitKind, StoreError};
pub use fs_objects::{FsObjectStore, ObjectStore};
pub use fs_ops::{
    CRASH_AT_ENV, FAIL_AT_ENV, FS_CALLS_ENV, FsMethod, FsOps, HookedFs, LinkOutcome,
    ORDER_CHECK_ENV, RealFs,
};
#[cfg(feature = "test-hooks")]
pub use fs_ops::{CrashAtFs, FailAt, FailAtFs, FailMode, InjectedError};
pub use layout::{Layout, is_native_linux_fs};
pub use meta::{
    BadRecord, CasResult, ChangesetMeta, CommitInfo, Frame, Index, InsertOutcome, Journal,
    JournalEntry, MetaLock, MetaRead, MetaStore, MetaWrite, Record, RefFile, RefMoveEntry,
    RefState, SortedView,
};
#[cfg(feature = "test-hooks")]
pub use order_check::OrderCheckFs;
