//! SwarmOps' git side: the repositories `git import` takes history from and `git export` writes
//! it to, and the exact bytes of git's commits and trees.
//!
//! - [`oid`]: git's object ids ([`GitOid`]), its object kinds ([`RawKind`]) and formats
//!   ([`ObjectFormat`]), and [`git_hash`], the id git gives an object's bytes.
//! - [`raw`]: git's commit and tree bytes parsed into their fields ([`RawCommit`], [`RawIdent`],
//!   [`RawTree`]) and rendered back: a commit git wrote renders to its own bytes, and one written
//!   another way to other bytes.
//! - [`source`]: [`GitSource`], what the import reads from a repository: refs resolved to
//!   commits, and objects' raw bytes, each checked against its id.
//! - [`gix_source`]: [`GixSource`], the [`GitSource`] of a repository on disk.
//! - [`lossy`]: the classes of git content a changeset cannot hold as git has it ([`LossyClass`]),
//!   and sets of them ([`LossySet`]).
//! - [`render`]: the git objects a changeset renders to, [`tree_bytes`] and [`commit_bytes`].
//! - [`convert`]: [`Converter`], which converts git's commits, trees and blobs into a changeset's
//!   fields ([`Converted`]), trees and blobs, refusing what the classes it allows do not cover
//!   ([`ConvertError`]); a commit is converted without loss only if it renders back to its own
//!   id.
//! - [`sink`]: [`GitSink`], what the export writes to a repository: objects, made durable before a
//!   branch moves, and the branch, moved by compare-and-swap.
//! - [`gix_sink`]: [`GixSink`], the [`GitSink`] of a repository on disk.
//!
//! Umbrella decision 3: gix is pinned (`=0.89.0`, its `sha1` feature alone, no default features)
//! and seen by [`gix_source`] and [`gix_sink`] alone; the first also computes the SHA-1 behind
//! [`git_hash`]. Everything above [`GitSource`] and [`GitSink`] reads and writes raw bytes, parsed
//! and rendered with this crate's own code, never with gix's, so git2 (0.21.0, the fallback) would
//! be a second [`GitSource`] and [`GitSink`] in place of those two modules, and nothing else would
//! change.

pub mod convert;
pub mod gix_sink;
pub mod gix_source;
pub mod lossy;
pub mod oid;
pub mod raw;
pub mod render;
pub mod sink;
pub mod source;

pub use convert::{ConvertError, Converted, Converter, GIT_LOSSY, GIT_OID};
pub use gix_sink::GixSink;
pub use gix_source::GixSource;
pub use lossy::{LossyClass, LossySet};
pub use oid::{GitOid, ObjectFormat, ParseGitOidError, RawKind, git_hash};
pub use raw::{RawCommit, RawError, RawIdent, RawTree, RawTreeEntry};
pub use render::{commit_bytes, tree_bytes};
pub use sink::{BranchUpdate, GitSink, SinkError, Stored};
pub use source::{GitSource, SourceError, Unsupported};
