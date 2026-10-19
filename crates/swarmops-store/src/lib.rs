//! SwarmOps store: content-addressed object files plus metadata.
//!
//! Objects live in loose, self-verifying files ([`FsObjectStore`]), read whole or handed over in
//! pieces as they decode ([`ObjectStore::read_chunks`]), under a store root whose paths
//! [`Layout`] computes. Metadata lives in `meta/` ([`meta`]), behind the
//! [`MetaStore`] facade: a CRC-framed, append-only [`Journal`] is its source of truth,
//! [`MetaLock`] serializes its writers, the columnar changeset [`Index`] answers lookups by
//! id, id prefix and delta hash, and ref files hold the refs. Values derived from the objects
//! are cached beside them in `meta/derived/` ([`DerivedStore`]), outside the journal. The object
//! store's filesystem access (all but its listing and its header reads, [`ObjectStore::list`] and
//! [`ObjectStore::stat`], which use `std::fs`), the metadata store's writes and the derived
//! values' reads and writes go through [`FsOps`], so tests can count calls and, in a build with
//! the `test-hooks` feature, crash the process mid-write (`CrashAtFs`), fail calls (`FailAtFs`)
//! or check the order of the writes (`OrderCheckFs`).

pub mod error;
pub mod fs_objects;
pub mod fs_ops;
pub mod layout;
pub mod meta;
#[cfg(feature = "test-hooks")]
pub mod order_check;

pub use error::{FileOp, IoFailure, LimitKind, StoreError};
pub use fs_objects::{FsObjectStore, ObjectStat, ObjectStore, PutOutcome};
pub use fs_ops::{
    CRASH_AT_ENV, FAIL_AT_ENV, FS_CALLS_ENV, FsMethod, FsOps, HookedFs, LinkOutcome,
    ORDER_CHECK_ENV, RealFs, StoreFs,
};
#[cfg(feature = "test-hooks")]
pub use fs_ops::{CrashAtFs, FailAt, FailAtFs, FailMode, InjectedError};
pub use layout::{Layout, is_native_linux_fs};
pub use meta::refs::check_ref_name;
pub use meta::{
    BadRecord, CasResult, ChangesetMeta, CommitInfo, DerivedSlot, DerivedStore, Frame, Index,
    InsertOutcome, Journal, JournalEntry, LogCut, MetaLock, MetaRead, MetaStore, MetaWrite, Record,
    Recovered, RefFile, RefMoveEntry, RefState, SortedView, TailRepair,
};
#[cfg(feature = "test-hooks")]
pub use order_check::OrderCheckFs;
