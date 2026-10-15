//! Hashed objects and their canonical byte encodings.
//!
//! The layouts here are the hash contract (storage-core spec §1). They change
//! only by introducing a new magic/version, never in place.

pub mod changeset;
pub mod codec;
pub mod tree;
pub mod varint;

pub use changeset::{Changeset, DeltaEntry, DeltaOp, Signature};
pub use codec::CodecError;
pub use tree::{EntryKind, Tree, TreeEntry};
