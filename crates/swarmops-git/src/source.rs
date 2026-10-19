//! What the git import reads from a repository: refs resolved to commits, and objects' raw bytes.

use crate::{GitOid, ObjectFormat, RawKind};

/// A git repository as the import reads it: the one seam between SwarmOps and a git library
/// (umbrella decision 3). [`GixSource`](crate::GixSource) implements it with gix; git2 would be a
/// second implementation, and a test an in-memory one. Everything above it parses the bytes it
/// returns with this crate's own parsers ([`raw`](crate::raw)).
///
/// Replacement objects (`refs/replace/`) and grafts never apply: every object is the one its id
/// names. A source refuses, when it opens, a repository it cannot read as it is (a SHA-256 one,
/// reftable refs, a shallow clone).
pub trait GitSource {
    /// The hash the repository names its objects by.
    fn object_format(&self) -> ObjectFormat;

    /// The commit `name` names: `HEAD`, a ref's full or short name (looked up in git's order:
    /// as given, then under `refs/`, `refs/tags/`, `refs/heads/`, `refs/remotes/`, and as
    /// `refs/remotes/<name>/HEAD`), or 40 hex digits of either case (an object's id, before any
    /// ref); annotated tags peeled. No revision syntax (`main~3`): such a name names no ref.
    /// [`SourceError::RefNotFound`] when nothing has that name, [`SourceError::NotACommit`] when
    /// it peels to a tree or a blob.
    fn resolve(&self, name: &str) -> Result<GitOid, SourceError>;

    /// The kind and the length of the object `oid`, from its header, without reading it all:
    /// [`SourceError::Missing`] when the repository lacks it.
    fn header(&self, oid: GitOid) -> Result<(RawKind, u64), SourceError>;

    /// The kind and the bytes of the object `oid`, without git's `<kind> <len>\0` header; bytes
    /// whose [`git_hash`](crate::git_hash) is `oid`, or [`SourceError::Corrupt`].
    /// [`SourceError::Missing`] when the repository lacks the object.
    fn read(&self, oid: GitOid) -> Result<(RawKind, Vec<u8>), SourceError>;
}

/// Why a [`GitSource`] could not open a repository or read from it.
///
/// A source runs on the caller's host (`git import` is a client-side operation), so its reasons
/// may name the repository's own files.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SourceError {
    /// There is no git repository it can open at the path it was given.
    #[error("no git repository it can open there: {reason}")]
    NotARepo {
        /// What the git library found instead.
        reason: String,
    },
    /// No ref and no object has that name.
    #[error("no ref and no commit is named {name:?}")]
    RefNotFound {
        /// The name, as given.
        name: String,
    },
    /// The name peels to an object that is not a commit.
    #[error("{name:?} names a {kind}, not a commit")]
    NotACommit {
        /// The name, as given.
        name: String,
        /// The kind of the object it peels to.
        kind: RawKind,
    },
    /// The repository is one the source does not read, refused before any read.
    #[error("the repository is not read as it is: {0}")]
    Unsupported(Unsupported),
    /// The repository lacks the object, as a partial clone lacks blobs.
    #[error("the repository lacks object {oid}")]
    Missing {
        /// The object's id.
        oid: GitOid,
    },
    /// The object's bytes are not the object its id names, or the git library cannot decode
    /// them.
    #[error("object {oid} is damaged: {reason}")]
    Corrupt {
        /// The object's id.
        oid: GitOid,
        /// What is wrong with it.
        reason: String,
    },
    /// The filesystem refused a read, or the git library failed otherwise.
    #[error("cannot read the repository: {reason}")]
    Io {
        /// What failed.
        reason: String,
    },
}

/// A repository a [`GitSource`] does not read as it is.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Unsupported {
    /// Its objects are named by a hash other than SHA-1 (`extensions.objectFormat`), as written.
    #[error("its objects are named by {0} (extensions.objectFormat), not SHA-1")]
    ObjectFormat(String),
    /// Its refs are kept in another format than loose files and `packed-refs`
    /// (`extensions.refStorage`, such as `reftable`), as written.
    #[error("its refs are kept as {0} (extensions.refStorage), not as files")]
    RefStorage(String),
    /// It is a shallow clone: some of its commits lack their parents.
    #[error("it is a shallow clone, whose oldest commits lack their parents")]
    Shallow,
}
