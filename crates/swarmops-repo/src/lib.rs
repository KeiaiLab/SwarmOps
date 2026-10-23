//! SwarmOps repository engine, the "server" of the design (storage spec §4): a [`Repo`] turns a
//! change list into blobs, rebuilt trees and a changeset ([`Repo::commit`]), reads files and
//! directories at a changeset without a checkout ([`Repo::read_blob_at`], [`Repo::list_tree`]),
//! resolves revisions ([`Repo::resolve`]), walks first-parent history
//! ([`Repo::log_first_parent`]) and ancestry (the [`history`] module: [`RepoRead::ancestor`],
//! [`RepoRead::is_ancestor`], [`RepoRead::merge_base`]), moves refs by compare-and-swap
//! ([`Repo::cas_ref`]), and checks a whole store ([`Repo::verify`]). For the git import it records
//! changesets whose every field the caller gives, many in one journal frame
//! ([`Repo::commit_raw_batch`]). It reads the journal back as the store's operations
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
//! nothing ([`Repo::merge_with`]; the [`merge`] module). A land merges a change that way onto a
//! ref's head and publishes the result as one changeset, with the ref's move to it, in one write
//! transaction; a lost race re-merges and tries again, and a keyed land run again replays
//! ([`Repo::land`]; the [`land`] module). A rebase merges a change the same way and records the
//! result as one changeset on the ref's head, moving no ref ([`Repo::rebase`]; the [`rebase`]
//! module).
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
pub mod history;
pub mod land;
#[cfg(feature = "test-hooks")]
pub mod land_models;
pub mod log;
pub mod merge;
pub mod oplog;
pub mod path_history;
pub mod raw;
pub mod read;
pub mod rebase;
pub mod repo;
mod tree_edit;
pub mod verify;

pub use commit::{
    Advance, Advanced, BlobPut, Change, ChangeOp, CommitOutcome, CommitRequest,
    RESERVED_METADATA_KEY_SUFFIX, check_metadata,
};
pub use derive::{
    Derivation, DeriveCtx, DeriveError, FirstParentSkipList, LastModified, LastTouch, Need,
    PathKey, Registry, SkipList, Subject,
};
pub use error::RepoError;
pub use history::{FirstParents, SKIP_AFTER};
pub use land::{
    LandOutcome, LandRequest, LandStatus, MergeConflicts, NothingToLandReason, Resolution,
    Resolutions, Side,
};
pub use merge::MergeOutcome;
pub use oplog::{OpRecord, Operation, UndoOutcome};
pub use path_history::PathEvent;
pub use raw::{MAX_RAW_BATCH, RawBatch, RawBatchOutcome, RawChangeset, RawOutcome};
pub use read::{FileSpan, FileSpanRead, ListedEntry, RepoRead, StoredChangeset, TreePage};
pub use rebase::{RebaseOutcome, RebaseRequest, RebaseStatus};
pub use repo::{CasOutcome, LocalRepo, Repo, RepoInfo};
pub use swarmops_store::{CasResult, InsertOutcome, RefState, StoreError};
pub use verify::{Depth, MismatchCause, VerifyProblem, VerifyReport};
