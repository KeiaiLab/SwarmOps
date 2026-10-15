//! SwarmOps store: content-addressed object files plus metadata.
//!
//! Objects live in loose, self-verifying files ([`FsObjectStore`]) under a store root
//! whose paths [`Layout`] computes; all their filesystem access goes through [`FsOps`],
//! so tests can count calls or crash the process mid-write ([`CrashAtFs`]).

pub mod error;
pub mod fs_objects;
pub mod fs_ops;
pub mod layout;

pub use error::StoreError;
pub use fs_objects::{FsObjectStore, ObjectStore};
pub use fs_ops::{CRASH_AT_ENV, CrashAtFs, FsOps, LinkOutcome, RealFs};
pub use layout::{Layout, is_native_linux_fs};
