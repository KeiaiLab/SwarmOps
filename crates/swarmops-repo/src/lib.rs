//! SwarmOps repository engine, the "server" of the design (storage spec §4): a [`Repo`] turns a
//! change list into blobs, rebuilt trees and a changeset ([`Repo::commit`]), reads files and
//! directories at a changeset without a checkout ([`Repo::read_blob_at`], [`Repo::list_tree`]),
//! resolves revisions ([`Repo::resolve`]), walks first-parent history
//! ([`Repo::log_first_parent`]), moves refs by compare-and-swap ([`Repo::cas_ref`]), and checks
//! a whole store ([`Repo::verify`]). It reads the journal back as the store's operations
//! ([`Repo::op_log`]), and undoes an operation's ref moves by compare-and-swap ([`Repo::undo`]).
//!
//! A request reads in one snapshot of the metadata ([`Repo::snapshot`], a [`RepoRead`]), which
//! resolves every revision and ref it names in the same view, and commits on the parents it
//! resolved there ([`Repo::commit_in`]); [`Repo`]'s read methods each take a snapshot of their
//! own. A snapshot also reads part of a file, keeping only that part while it verifies the whole
//! blob ([`RepoRead::read_file_span`]), and lists the directories below one to a depth, with each
//! leaf's size ([`RepoRead::list_tree_depth`]). A diff of two changesets reads both their trees in
//! one ([`RepoRead::diff`]).
//!
//! A three-way merge of what one changeset changed onto another reads only the paths that change
//! touches, and returns the delta, the contents it merged and the conflicts as data, writing
//! nothing ([`Repo::merge_with`]; the [`merge`] module).
//!
//! [`LocalRepo`] is the repository on the local filesystem: the loose object files and the
//! native metadata store of `swarmops-store`, which own durability. The operations take and
//! return plain values with no filesystem paths in them, so that they can move behind an RPC
//! boundary later; only [`LocalRepo::create`] and [`LocalRepo::open`] take the store's path.
//!
//! Values derived from the immutable objects are made when they are asked for, and cached in the
//! store ([`Repo::derived`]; the [`derive`](mod@derive) module). A path's history chases one of
//! them, the changeset that last touched the path ([`LastModified`]; the [`path_history`] module).

pub mod commit;
pub mod derive;
pub mod diff;
pub mod error;
#[cfg(feature = "test-hooks")]
pub mod land_models;
pub mod log;
pub mod merge;
pub mod oplog;
pub mod path_history;
pub mod read;
pub mod repo;
mod tree_edit;
pub mod verify;

pub use commit::{Advance, Advanced, BlobPut, Change, ChangeOp, CommitOutcome, CommitRequest};
pub use derive::{
    Derivation, DeriveCtx, DeriveError, FirstParentSkipList, LastModified, LastTouch, Need,
    PathKey, Registry, SkipList, Subject,
};
pub use error::RepoError;
pub use merge::MergeOutcome;
pub use oplog::{OpRecord, Operation, UndoOutcome};
pub use path_history::PathEvent;
pub use read::{FileSpan, FileSpanRead, ListedEntry, RepoRead, StoredChangeset, TreePage};
pub use repo::{CasOutcome, LocalRepo, Repo, RepoInfo};
pub use swarmops_store::{CasResult, InsertOutcome, RefState, StoreError};
pub use verify::{MismatchCause, VerifyProblem, VerifyReport};
