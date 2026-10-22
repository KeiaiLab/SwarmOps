//! SwarmOps' git side: the repositories `git import` takes history from and `git export` writes
//! it to, the exact bytes of git's commits and trees, the import that brings that history into a
//! store, and the export that writes a store's history back.
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
//! - [`native`]: the commit a native changeset exports to: its own signatures, and its message
//!   followed by SwarmOps' trailers ([`TRAILERS`], [`export_message`], [`render_commit`]).
//! - [`export_oid`]: [`ExportOid`], the derivation of the git commit each changeset exports to
//!   ([`ExportedOid`]), and the git oids of its trees and blobs, memoized per object
//!   ([`tree_oid`], [`blob_oid`]).
//! - [`convert`]: [`Converter`], which converts git's commits, trees and blobs into a changeset's
//!   fields ([`Converted`]), trees and blobs, refusing what the classes it allows do not cover
//!   ([`ConvertError`]); a commit is converted without loss only if it renders back to its own
//!   id.
//! - [`import`](mod@import): [`import()`], which brings every commit a git ref reaches that the
//!   store does not hold into a store, parents first and many to a journal frame, then moves a
//!   target ref to the tip's changeset ([`ImportRequest`], [`ImportReport`]); run again, it takes
//!   only what is new, and it refuses a target that diverged ([`ImportError`]).
//! - [`sink`]: [`GitSink`], what the export writes to a repository: objects, made durable before a
//!   branch moves, and the branch, moved by compare-and-swap.
//! - [`gix_sink`]: [`GixSink`], the [`GitSink`] of a repository on disk.
//! - [`export`](mod@export): [`export()`], which writes the history behind a store ref into a
//!   branch of a git repository, every commit the branch's last export does not reach, parents
//!   first, makes it durable and records its native commits' oids, then moves the branch by
//!   compare-and-swap ([`ExportRequest`], [`ExportReport`]); run again, it writes only what is
//!   new, and it refuses a branch that diverged ([`ExportError`], [`DivergedReason`]).
//! - [`transport`]: the git remotes' transport (M13), the user's git spawned against a store's
//!   mirror ([`transport::Transport`], [`transport::GitCli`]).
//! - [`mirror`]: a store's git mirror and its lock ([`mirror::Mirror`], [`mirror::MirrorLock`]).
//! - [`limits`]: GitHub's documented limits, checked before a push sends anything.
//! - [`sync`](mod@sync): the sync engine: the unpublished changes, those a remote holds already,
//!   and the rest landed onto the remote's head.
//!
//! Umbrella decision 3: gix is pinned (`=0.89.0`, its `sha1` feature alone, no default features)
//! and seen by [`gix_source`] and [`gix_sink`] alone; the first also computes the SHA-1 behind
//! [`git_hash`]. Everything above [`GitSource`] and [`GitSink`] reads and writes raw bytes, parsed
//! and rendered with this crate's own code, never with gix's, so git2 (0.21.0, the fallback) would
//! be a second [`GitSource`] and [`GitSink`] in place of those two modules, and nothing else would
//! change.

mod ancestry;
pub mod convert;
pub mod export;
pub mod export_oid;
pub mod gix_sink;
pub mod gix_source;
pub mod import;
pub mod limits;
pub mod lossy;
pub mod mirror;
pub mod native;
pub mod oid;
pub mod raw;
pub mod render;
pub mod sink;
pub mod source;
pub mod sync;
pub mod transport;

pub use convert::{ConvertError, Converted, Converter, GIT_LOSSY, GIT_OID};
pub use export::{DivergedReason, ExportError, ExportReport, ExportRequest, export};
pub use export_oid::{
    ExportObjects, ExportOid, ExportReader, ExportedOid, GitMemoMismatch, OidCache, blob_oid,
    real_blob_oid, real_tree_oid, register, tree_oid, verify_export_oids, verify_external_ids,
};
pub use gix_sink::GixSink;
pub use gix_source::GixSource;
pub use import::{
    ExternalIdMismatch, ImportError, ImportReport, ImportRequest, LossyCommit, import,
};
pub use lossy::{LossyClass, LossySet};
pub use native::{TRAILERS, export_message, render_commit};
pub use oid::{GitOid, ObjectFormat, ParseGitOidError, RawKind, git_hash};
pub use raw::{RawCommit, RawError, RawIdent, RawTree, RawTreeEntry};
pub use render::{commit_bytes, tree_bytes};
pub use sink::{BranchUpdate, GitSink, SinkError, Stored};
pub use source::{GitSource, SourceError, Unsupported};
