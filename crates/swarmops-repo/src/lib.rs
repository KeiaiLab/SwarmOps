//! SwarmOps repository engine, the "server" of the design (storage spec §4): a [`Repo`] turns a
//! change list into blobs, rebuilt trees and a changeset ([`Repo::commit`]), reads files and
//! directories at a changeset without a checkout ([`Repo::read_blob_at`], [`Repo::list_tree`]),
//! resolves revisions ([`Repo::resolve`]), walks first-parent history
//! ([`Repo::log_first_parent`]), moves refs by compare-and-swap ([`Repo::cas_ref`]), and checks
//! a whole store ([`Repo::verify`]).
//!
//! A request reads in one snapshot of the metadata ([`Repo::snapshot`], a [`RepoRead`]), which
//! resolves every revision and ref it names in the same view, and commits on the parents it
//! resolved there ([`Repo::commit_in`]); [`Repo`]'s read methods each take a snapshot of their
//! own.
//!
//! [`LocalRepo`] is the repository on the local filesystem: the loose object files and the
//! native metadata store of `swarmops-store`, which own durability. The operations take and
//! return plain values with no filesystem paths in them, so that they can move behind an RPC
//! boundary later; only [`LocalRepo::create`] and [`LocalRepo::open`] take the store's path.
//!
//! Values derived from the immutable objects are made when they are asked for, and cached in the
//! store ([`Repo::derived`]; the [`derive`](mod@derive) module).

pub mod commit;
pub mod derive;
pub mod error;
pub mod log;
pub mod read;
pub mod repo;
mod tree_edit;
pub mod verify;

pub use commit::{Advance, Advanced, Change, ChangeOp, CommitOutcome, CommitRequest};
pub use derive::{Derivation, DeriveCtx, DeriveError, Need, Registry, Subject};
pub use error::RepoError;
pub use read::{RepoRead, StoredChangeset};
pub use repo::{CasOutcome, LocalRepo, Repo, RepoInfo};
pub use swarmops_store::{CasResult, InsertOutcome, RefState, StoreError};
pub use verify::{MismatchCause, VerifyProblem, VerifyReport};

/// The merge algorithms (`swarmops-merge`): pure functions over trees and lines, with no I/O and
/// no store, which the engine's operations call over the trees they read.
pub use swarmops_merge as merge;
