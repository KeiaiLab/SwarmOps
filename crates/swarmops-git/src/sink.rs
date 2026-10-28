//! Where the git export writes: a repository's objects and one of its branches or tags, behind
//! [`GitSink`].

use crate::{GitOid, ObjectFormat, RawKind, Unsupported};

/// A git repository as the export writes it: the one seam between the export and a git library
/// on that side (umbrella decision 3), as [`GitSource`](crate::GitSource) is on the import's.
/// [`GixSink`](crate::GixSink) implements it with gix, and a test an in-memory one.
///
/// The export writes the objects a new commit reaches ([`write_object`]), makes them durable
/// ([`sync`]), and only then moves the branch ([`cas_branch`]) or the tag ([`cas_tag`]), so that
/// the ref never names an object a power loss can take. Branches are `refs/heads/<name>` and tags
/// `refs/tags/<name>` (lightweight: the ref names the commit), each named without that prefix. A
/// sink refuses, when it opens, a repository it cannot write as it is (a SHA-256 one, reftable
/// refs, a shallow clone, a FIFO, a socket or a device where it reads a file), as a source
/// refuses one it cannot read.
///
/// [`write_object`]: GitSink::write_object
/// [`sync`]: GitSink::sync
/// [`cas_branch`]: GitSink::cas_branch
/// [`cas_tag`]: GitSink::cas_tag
pub trait GitSink {
    /// The hash the repository names its objects by.
    fn object_format(&self) -> ObjectFormat;

    /// Where the repository holds the object `oid`: a loose file of its own, a pack (or an
    /// alternate repository), or nowhere.
    fn stored(&self, oid: GitOid) -> Result<Stored, SinkError>;

    /// The kind and the bytes of the object `oid`, without git's `<kind> <len>\0` header, checked
    /// against its id; a loose file is read before a pack. [`SinkError::Corrupt`] when they do not
    /// decode or do not hash to `oid`, [`SinkError::Missing`] when the repository lacks it.
    fn read(&self, oid: GitOid) -> Result<(RawKind, Vec<u8>), SinkError>;

    /// Writes `bytes` as an object of `kind`, and returns its id,
    /// [`git_hash(kind, bytes)`](crate::git_hash): as a loose file, unless the repository holds the
    /// object already; a loose file of that name that does not read back as the object is
    /// replaced. Not durable until [`sync`](GitSink::sync) names it.
    fn write_object(&mut self, kind: RawKind, bytes: &[u8]) -> Result<GitOid, SinkError>;

    /// Makes durable the loose files of exactly the objects `oids`, the fan-out directories that
    /// hold them, and the objects directory when this sink made one of those fan-outs. An object
    /// only in a pack or an alternate is durable already; one the repository lacks is
    /// [`SinkError::Missing`].
    fn sync(&mut self, oids: &[GitOid]) -> Result<(), SinkError>;

    /// The object the branch `refs/heads/<name>` names, its loose file before `packed-refs`;
    /// `None` when there is no such branch.
    fn branch(&self, name: &str) -> Result<Option<GitOid>, SinkError>;

    /// The branch `HEAD` names in a repository with a work tree, which `git push` refuses to move
    /// by default (`receive.denyCurrentBranch`); `None` for a bare repository or a detached
    /// `HEAD`.
    fn checked_out(&self) -> Result<Option<String>, SinkError>;

    /// Moves the branch `refs/heads/<name>` to `new` if it names `expected` now (`None`: no such
    /// branch), as one compare-and-swap against every other writer of that branch, and durably
    /// once it returns [`BranchUpdate::Applied`]. [`BranchUpdate::Mismatch`] when the branch names
    /// something else, which it leaves as it is. `new` must be an object the repository holds
    /// ([`SinkError::Missing`] otherwise, as git refuses a ref to an object it lacks); no reflog
    /// entry is written.
    fn cas_branch(
        &mut self,
        name: &str,
        expected: Option<GitOid>,
        new: GitOid,
    ) -> Result<BranchUpdate, SinkError>;

    /// The object the lightweight tag `refs/tags/<name>` names, as [`branch`](GitSink::branch)
    /// reads a branch; `None` when there is no such tag. A sink that writes no tags keeps this
    /// body: [`SinkError::Unsupported`] ([`Unsupported::Tags`]).
    fn tag(&self, name: &str) -> Result<Option<GitOid>, SinkError> {
        let _ = name;
        Err(SinkError::Unsupported(Unsupported::Tags))
    }

    /// Moves the lightweight tag `refs/tags/<name>` to `new` if it names `expected` now, as
    /// [`cas_branch`](GitSink::cas_branch) moves a branch: one compare-and-swap, durable once it
    /// returns [`BranchUpdate::Applied`]. A sink that writes no tags keeps this body:
    /// [`SinkError::Unsupported`] ([`Unsupported::Tags`]).
    fn cas_tag(
        &mut self,
        name: &str,
        expected: Option<GitOid>,
        new: GitOid,
    ) -> Result<BranchUpdate, SinkError> {
        let _ = (name, expected, new);
        Err(SinkError::Unsupported(Unsupported::Tags))
    }
}

/// Where a [`GitSink`] holds an object.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Stored {
    /// The repository lacks it.
    Absent,
    /// A loose file of its own (`objects/<2 hex>/<38 hex>`), which an export cut short may have
    /// left unsynced, and a power loss emptied: one to read back before it is trusted.
    Loose,
    /// In a pack, or in an alternate repository: trusted, as git trusts it.
    Packed,
}

/// What a [`GitSink::cas_branch`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum BranchUpdate {
    /// The branch named what was expected and now names the new object, durably.
    Applied,
    /// The branch named something else, and is left as it was.
    Mismatch {
        /// What it names: `None` when there is no such branch.
        actual: Option<GitOid>,
    },
}

/// Why a [`GitSink`] could not open a repository, read it or write it.
///
/// A sink runs on the caller's host (`git export` is a client-side operation), so its errors may
/// name the repository's own files: relative to its git directory, and objects by their ids.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SinkError {
    /// There is no git repository it can open at the path it was given.
    #[error("no git repository it can open there: {reason}")]
    NotARepo {
        /// What the git library found instead.
        reason: String,
    },
    /// The repository is one the sink does not write, refused before any read.
    #[error("the repository is not written as it is: {0}")]
    Unsupported(Unsupported),
    /// The name is no branch or tag name git takes (`refs/heads/<name>` or `refs/tags/<name>` is
    /// no valid ref name).
    #[error("{name:?} is not a branch or tag name git takes")]
    BadBranch {
        /// The name, as given.
        name: String,
    },
    /// Another writer holds the branch's lock file, or one that died left it: as with git, it
    /// stays until it is removed once no git process runs.
    #[error("{lock} is there: another writer holds the branch, or one that died left it")]
    RefLocked {
        /// The lock file, relative to the git directory (`refs/heads/<name>.lock`).
        lock: String,
    },
    /// Another ref, a file or a directory is where the branch would go: a ref whose name is a
    /// directory of the branch's, a ref under the branch's name, or a symbolic ref.
    #[error("{name} is in the way of the branch")]
    RefConflict {
        /// What is in the way: the full name of that ref, or the branch's own.
        name: String,
    },
    /// The repository lacks the object.
    #[error("the repository lacks object {oid}")]
    Missing {
        /// The object's id.
        oid: GitOid,
    },
    /// The object's bytes are not the object its id names, or they do not decode.
    #[error("object {oid} is damaged: {reason}")]
    Corrupt {
        /// The object's id.
        oid: GitOid,
        /// What is wrong with it.
        reason: String,
    },
    /// Reading the repository failed otherwise.
    #[error("cannot read {what}: {reason}")]
    Read {
        /// What was read: a file relative to the git directory, or an object.
        what: String,
        /// What failed.
        reason: String,
    },
    /// Writing the repository, or making what was written durable, failed.
    #[error("cannot write {what}: {reason}")]
    Write {
        /// What was written: a file or a directory relative to the git directory.
        what: String,
        /// What failed.
        reason: String,
    },
}
