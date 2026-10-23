//! What the git import reads from a repository: refs resolved to commits, and objects' raw bytes.

use swarmops_core::bytes;
use swarmops_store::MAX_OBJECT_LEN;

use crate::{GitOid, ObjectFormat, RawKind};

/// A git repository as the import reads it: the one seam between SwarmOps and a git library
/// (umbrella decision 3). [`GixSource`](crate::GixSource) implements it with gix; git2 would be a
/// second implementation, and a test an in-memory one. Everything above it parses the bytes it
/// returns with this crate's own parsers ([`raw`](crate::raw)).
///
/// Replacement objects (`refs/replace/`) and grafts never apply: every object is the one its id
/// names. A source refuses, when it opens, a repository it cannot read as it is (a SHA-256 one,
/// reftable refs, a shallow clone, a FIFO, a socket or a device where it reads a file).
pub trait GitSource {
    /// The hash the repository names its objects by.
    fn object_format(&self) -> ObjectFormat;

    /// The commit `name` names: `HEAD`, a ref's full or short name (looked up in git's order:
    /// as given, then under `refs/`, `refs/tags/`, `refs/heads/`, `refs/remotes/`, and as
    /// `refs/remotes/<name>/HEAD`), or 40 hex digits of either case (an object's id, before any
    /// ref); annotated tags peeled. No revision syntax (`main~3`): such a name names no ref.
    /// [`SourceError::RefNotFound`] when nothing has that name, [`SourceError::NotACommit`] when
    /// it peels to a tree or a blob, and [`SourceError::TooLarge`] for an annotated tag on the way
    /// whose header says it is longer than an object holds, which is not read.
    fn resolve(&self, name: &str) -> Result<GitOid, SourceError>;

    /// The kind and the length of the object `oid`, from its header, without reading it all:
    /// [`SourceError::Missing`] when the repository lacks it.
    fn header(&self, oid: GitOid) -> Result<(RawKind, u64), SourceError>;

    /// The kind and the bytes of the object `oid`, without git's `<kind> <len>\0` header; bytes
    /// whose [`git_hash`](crate::git_hash) is `oid`, or [`SourceError::Corrupt`].
    /// [`SourceError::Missing`] when the repository lacks the object.
    ///
    /// It reads the object whole, whatever its length: the import reads none whose
    /// [`header`](GitSource::header) says it is longer than an object holds ([`MAX_OBJECT_LEN`]),
    /// refusing a commit, a tree or an annotated tag as [`SourceError::TooLarge`], and a blob with
    /// the conversion's own refusal, which names its path.
    fn read(&self, oid: GitOid) -> Result<(RawKind, Vec<u8>), SourceError>;
}

/// The kind and the bytes of the object `oid` of `source`, read with [`GitSource::read`] once its
/// [`header`](GitSource::header) says it is at most [`MAX_OBJECT_LEN`] bytes long, the most an
/// object holds: a longer one is [`SourceError::TooLarge`] before a byte of it is read, and so is
/// one whose bytes come out longer all the same, from a source whose header said less.
///
/// The import reads every commit, tree and annotated tag it parses through this gate, as the
/// conversion reads a blob through its own ([`ConvertError::TooLarge`], which names the blob's
/// path), so that a crafted repository's object, a small file that its header says inflates to
/// more than an object holds, is refused before it is read whole.
///
/// [`ConvertError::TooLarge`]: crate::ConvertError::TooLarge
pub(crate) fn read_bounded(
    source: &dyn GitSource,
    oid: GitOid,
) -> Result<(RawKind, Vec<u8>), SourceError> {
    let (kind, size) = source.header(oid)?;
    if size > MAX_OBJECT_LEN {
        return Err(SourceError::TooLarge { oid, kind, size });
    }
    let (kind, bytes) = source.read(oid)?;
    let size = bytes::to_u64(bytes.len());
    if size > MAX_OBJECT_LEN {
        return Err(SourceError::TooLarge { oid, kind, size });
    }
    Ok((kind, bytes))
}

/// Why a [`GitSource`] could not open a repository or read from it, or why the import does not
/// read an object it holds ([`SourceError::TooLarge`]).
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
    /// The object is longer than an object holds ([`MAX_OBJECT_LEN`]), as its header says, so it
    /// is not read: the import reads each commit, tree and annotated tag it parses only once its
    /// header says it is at most that long, and refuses a longer one before a byte of it is read.
    #[error(
        "git {kind} {oid} is {size} bytes, and an object holds at most {max}",
        max = MAX_OBJECT_LEN
    )]
    TooLarge {
        /// The object's id.
        oid: GitOid,
        /// Its kind, as its header says.
        kind: RawKind,
        /// Its length: as its header says, or as read from a source whose header said less.
        size: u64,
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
    /// A file it reads in its git directory, named by its path there (`config`,
    /// `refs/heads/main`, `objects/info/alternates`, …; an alternate object directory elsewhere
    /// by its own path), is a FIFO, a socket or a device, or a symlink to one: opening a FIFO
    /// blocks until something writes to it, and a device's read can block or never end. Refused
    /// before any file of the repository is read.
    #[error("its {0} is a FIFO, a socket or a device, not a regular file")]
    SpecialFile(String),
    /// A file it reads whole in its git directory (`HEAD` and every other ref file, `config`,
    /// `config.worktree`, `objects/info/alternates`), named as
    /// [`SpecialFile`](Unsupported::SpecialFile) names one, is longer than the most it reads of
    /// such a file, which none that git writes comes near: a crafted one, sparse so that it
    /// costs no disk, would be read into memory whole. Refused before it is read.
    #[error("its {file} is {len} bytes long, more than the {max} it reads of it")]
    FileTooLong {
        /// The file, by its path in the git directory.
        file: String,
        /// Its length.
        len: u64,
        /// The most it reads of such a file.
        max: u64,
    },
}
