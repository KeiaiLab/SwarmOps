//! The repository's error type.

use std::io;

use swarmops_core::{ChangesetId, CodecError, ObjectId, ObjectKind, RepoPath};
use swarmops_store::StoreError;
use thiserror::Error;

/// Why a repository operation failed.
#[derive(Debug, Error)]
pub enum RepoError {
    /// The store failed: I/O, a corrupt object file, damaged metadata, a writer lock held for
    /// the whole wait ([`StoreError::Busy`], retryable), a journal append that failed and was
    /// undone ([`StoreError::NotCommitted`], retryable), or a store that is not what the call needs
    /// ([`StoreError::Format`]: `open` of a directory that holds no store, or one of a layout
    /// this build does not read; [`StoreError::UnsupportedFs`]: a path under `/mnt/`).
    #[error(transparent)]
    Store(#[from] StoreError),
    /// An object is not canonical: a stored one does not decode, or the changeset a commit
    /// request describes cannot be encoded (metadata keys not strictly ascending, a `-0000`
    /// signature with a non-zero offset). A commit checks this before it writes anything.
    #[error("not a canonical object: {0}")]
    Codec(#[from] CodecError),
    /// A change puts a file below a file, or where a directory is, or deletes a directory: in
    /// the first parent's tree, or against another change of the same request.
    #[error("{path}: a file and a directory conflict there")]
    TypeConflict {
        /// The path of the change that conflicts.
        path: RepoPath,
    },
    /// A change puts a directory. Changes put files, executables and symlinks; directories
    /// follow from the paths.
    #[error("{path}: a change cannot put a directory")]
    DirPut {
        /// The path of the change.
        path: RepoPath,
    },
    /// The change list is empty, or changes nothing (every change puts what its path holds
    /// already), and the request does not allow an empty changeset.
    #[error("the change list changes nothing")]
    EmptyChanges,
    /// `create` found a store there already: the directory holds a `FORMAT` file. (Without
    /// one, what is there is a store a create left half done, and create finishes it.)
    #[error("a store exists there already")]
    Exists,
    /// A parent the commit names is not in the store.
    #[error("parent changeset {0} is not in the store")]
    UnknownParent(ChangesetId),
    /// What was asked for is not in the store: the changeset a revision, an id or a ref names,
    /// or the changeset a ref is to point at. Carries what was asked for, as given.
    #[error("{0}: not found")]
    NotFound(String),
    /// A revision's id prefix matches more than one changeset; carries the revision.
    #[error("{0}: the id prefix matches more than one changeset")]
    Ambiguous(String),
    /// Not a revision: a revision is a ref name, a full id or an id prefix of 12 to 63 hex
    /// digits, each followed by any number of `~N` suffixes. Carries the string.
    #[error("{0:?} is not a revision")]
    BadRev(String),
    /// A ref name that is not `main` or `release/<x>`.
    #[error("invalid ref name {0:?}")]
    RefName(String),
    /// A change deletes a path that is no file in the first parent's tree.
    #[error("{path}: there is no file to delete there")]
    PathNotFound {
        /// The path of the change.
        path: RepoPath,
    },
    /// Two changes of one request name the same path.
    #[error("{path}: more than one change names it")]
    DuplicatePath {
        /// The path named twice.
        path: RepoPath,
    },
    /// The store lacks an object that its metadata or its trees reference, or holds an object
    /// of another kind under that id: a damaged store ([`crate::Repo::verify`] says more).
    #[error("the store lacks {kind:?} object {id}")]
    MissingObject {
        /// The id referenced.
        id: ObjectId,
        /// The kind the reference expects.
        kind: ObjectKind,
    },
}

impl From<io::Error> for RepoError {
    fn from(e: io::Error) -> Self {
        Self::Store(StoreError::Io(e))
    }
}
