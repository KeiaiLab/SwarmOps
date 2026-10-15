//! SwarmOps core: object ids, repository paths and the canonical object codec.
//!
//! This crate performs no I/O. The byte layouts it produces are the hash
//! contract of the whole system (see the storage-core design spec).

pub mod error;
pub mod id;
pub mod object;
pub mod path;

pub use error::{IdError, PathError};
pub use id::{BlobId, ChangesetId, ObjectId, ObjectKind, TreeId, hash_object};
pub use object::{
    Changeset, CodecError, DeltaEntry, DeltaOp, EntryKind, Signature, Tree, TreeEntry,
};
pub use path::RepoPath;
