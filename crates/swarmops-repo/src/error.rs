//! The repository's error type.

use std::io;

use swarmops_core::{ChangesetId, CodecError, ObjectId, ObjectKind, RepoPath};
use swarmops_store::{IoFailure, Recovered, StoreError};
use thiserror::Error;

/// Why a repository operation failed.
#[derive(Debug, Error)]
pub enum RepoError {
    /// The store failed: I/O, a corrupt object file, damaged metadata, a writer lock held for
    /// the whole wait ([`StoreError::Busy`], retryable), commits landing during every round of a
    /// snapshot ([`StoreError::SnapshotContended`], retryable), a journal append that failed and
    /// was undone ([`StoreError::NotCommitted`], retryable), or a store that is not what the call
    /// needs ([`StoreError::Format`]: `open` of a directory that holds no store, or one of a
    /// layout this build does not read; [`StoreError::UnsupportedFs`]: a path under `/mnt/`).
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
    /// A commit with `advance` found its ref naming something other than the changeset's first
    /// parent, or, for a root changeset, found the ref there: another move came first. Nothing
    /// was recorded, neither the changeset nor the move; only the objects the commit flushed
    /// remain, unreferenced, for GC. Re-read the ref before committing on what it names.
    #[error(
        "ref {name} does not name the commit's first parent (it names {}): nothing was recorded",
        .actual.map_or_else(|| "nothing".to_owned(), |id| id.to_string())
    )]
    RefMoved {
        /// The ref.
        name: String,
        /// What it names; `None` if it does not exist.
        actual: Option<ChangesetId>,
        /// What the write transaction's recovery repaired before the move was refused, of what
        /// earlier writers left ([`swarmops_store::MetaWrite::recovered`]): those repairs stay
        /// made. Boxed, as the variant would otherwise make every result this large.
        recovered: Box<Recovered>,
    },
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

/// A filesystem failure of the store's, located ([`StoreError::Io`]).
impl From<IoFailure> for RepoError {
    fn from(e: IoFailure) -> Self {
        Self::Store(StoreError::Io(e))
    }
}

// Nothing turns a bare `io::Error` into a `RepoError`: a failure is located first, as the
// store's are. This fails to compile if a `From<io::Error>` comes back: both impls below then
// apply to `RepoError`, and the type `_` stands for is ambiguous.
const _: fn() = || {
    trait FromIoError<Marker> {
        fn check() {}
    }
    impl<T: ?Sized> FromIoError<()> for T {}
    impl<T: From<io::Error>> FromIoError<u8> for T {}
    let _ = <RepoError as FromIoError<_>>::check;
};
