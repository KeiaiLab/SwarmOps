//! SwarmOps repository engine, the "server" of the design (storage spec §4): a [`Repo`] turns a
//! change list into blobs, rebuilt trees and a changeset ([`Repo::commit`]), reads files and
//! directories at a changeset without a checkout ([`Repo::read_blob_at`], [`Repo::list_tree`]),
//! resolves revisions ([`Repo::resolve`]), walks first-parent history
//! ([`Repo::log_first_parent`]), moves refs by compare-and-swap ([`Repo::cas_ref`]), and checks
//! a whole store ([`Repo::verify`]).
//!
//! [`LocalRepo`] is the repository on the local filesystem: the loose object files and the
//! native metadata store of `swarmops-store`, which own durability. The operations take and
//! return plain values with no filesystem paths in them, so that they can move behind an RPC
//! boundary later; only [`LocalRepo::create`] and [`LocalRepo::open`] take the store's path.

pub mod commit;
pub mod error;
pub mod log;
pub mod read;
pub mod repo;
mod tree_edit;
pub mod verify;

pub use commit::{Change, ChangeOp, CommitOutcome, CommitRequest};
pub use error::RepoError;
pub use read::StoredChangeset;
pub use repo::{CasOutcome, LocalRepo, Repo, RepoInfo};
pub use swarmops_store::{CasResult, InsertOutcome, RefState, StoreError};
pub use verify::{MismatchCause, VerifyProblem, VerifyReport};
