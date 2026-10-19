//! The store's error type.

use std::{fmt, io};

use camino::Utf8PathBuf;
use swarmops_core::{ChangesetId, CodecError, ObjectId};
use thiserror::Error;

use crate::fs_ops::FsMethod;

/// Why a store operation failed.
#[derive(Debug, Error)]
pub enum StoreError {
    /// The filesystem refused an operation of the store's, as the [`IoFailure`] says: which
    /// operation, on which file of the store, and the operating system's error as it came. Only
    /// what the filesystem refused (an OS `EINVAL` among it); a metadata file that does not read
    /// as one (a `journal.ckpt` or a ref file) is one of kind `InvalidData`.
    #[error("i/o error: {0}")]
    Io(#[from] IoFailure),
    /// An object file's content does not hash to its id (or its zstd stream is damaged).
    #[error("object {id} is corrupt: its content does not match its id")]
    Corrupt {
        /// The id that was requested.
        id: ObjectId,
    },
    /// An object file's header is malformed: magic, kind, encoding, reserved bytes, or a
    /// `ulen` that disagrees with the payload.
    #[error("object {id} has a malformed header")]
    BadHeader {
        /// The id that was requested.
        id: ObjectId,
    },
    /// Stored bytes are not a canonical object encoding.
    #[error("codec error: {0}")]
    Codec(#[from] CodecError),
    /// The store's `FORMAT` file is missing or names an unsupported version.
    #[error("unsupported store format: {0}")]
    Format(String),
    /// The store root is not on a native Linux filesystem (for example WSL's `/mnt/*` drvfs).
    #[error("store path {0} is not on a native Linux filesystem")]
    UnsupportedFs(Utf8PathBuf),
    /// Another writer held `meta/lock` for the whole wait. Retryable.
    #[error("the metadata store is busy: another writer holds meta/lock")]
    Busy,
    /// A snapshot ([`crate::MetaStore::read`]) found the metadata changing in every one of its
    /// rounds: in each, a commit landed while it read the ref files (the frames past the
    /// checkpoint were no longer the ones it had read), or a writer cut an entry of `journal.off`
    /// as it read them. It took no lock and wrote nothing. Retryable.
    #[error("the metadata changed during each of {rounds} attempts to take a snapshot")]
    SnapshotContended {
        /// How many rounds it made: [`crate::MetaStore::SNAPSHOT_ROUNDS`].
        rounds: u32,
    },
    /// A journal frame failed its checks where a torn write cannot explain it: a frame that
    /// `journal.off` lists fails its CRC, a frame's records do not decode, or a frame or an
    /// offset entry is malformed.
    #[error("the metadata journal is corrupt at offset {offset}")]
    JournalCorrupt {
        /// Where in `journal.log` the bad frame starts (for a bad `journal.off` entry, the
        /// offset the entry holds): always a position in `journal.log`. A record's own bytes
        /// that do not decode are a [`crate::BadRecord`] of [`crate::Record::decode`], at a
        /// position in those bytes.
        offset: u64,
    },
    /// A changeset names a parent that is not in the index.
    #[error("parent changeset {0} is not in the store")]
    MissingParent(ChangesetId),
    /// A changeset names the same parent twice.
    #[error("parent changeset {0} is listed twice")]
    DuplicateParent(ChangesetId),
    /// A ref name is not `main` or `release/<name>`.
    #[error("invalid ref name {0:?}")]
    RefName(String),
    /// A ref would point at a changeset that is not in the index.
    #[error("ref target {0} is not a changeset in the store")]
    UnknownRefTarget(ChangesetId),
    /// A request past a limit of the store's layout, refused before anything of it is queued
    /// or written: a definite failure, before any commit point.
    #[error("{what}: at most {max}, not {got}")]
    Limit {
        /// Which limit.
        what: LimitKind,
        /// The most the layout holds.
        max: u64,
        /// What the request asked for.
        got: u64,
    },
    /// An id prefix that is not 1 to 64 hex digits; carries the prefix as given.
    #[error("id prefix {0:?} is not 1 to 64 hex digits")]
    BadPrefix(String),
    /// A call the store's API does not take, and that no request leads to: a frame of no
    /// records, a rec the index handle does not count, entries past a sorted view's end. A bug
    /// in the caller; carries what the call asked for.
    #[error("a call the store does not take: {0}")]
    BadCall(String),
    /// The changeset index files hold what no crash leaves behind, as `what` says: a sorted
    /// view file that ends in a partial entry; sorted views out of order, out of step in a
    /// way the order compaction renames them in cannot explain, or covering records the
    /// columns do not hold; an id the index holds twice; a record whose parents lie off
    /// `parents.log`'s 32-byte boundaries or past its end; an index file that changed
    /// during an append (a writer without `meta/lock`); or an index file that is missing
    /// (a store has them all from its creation, and nothing removes one). The journal can
    /// rebuild the index.
    #[error("the changeset index is corrupt: {what}")]
    IndexCorrupt {
        /// What is wrong.
        what: &'static str,
    },
    /// The changeset index disagrees with the journal about record `seq` (found by verify).
    #[error("the changeset index disagrees with the journal at seq {seq}")]
    IndexMismatch {
        /// The record that disagrees.
        seq: u64,
    },
    /// A write transaction committed: its frame is durable at `frame_seq`. But a step after its
    /// commit point failed: listing the frame in `journal.off`, materializing it (index
    /// records, ref files), a compaction, the index sync or the checkpoint. The next
    /// [`crate::MetaStore::write`] repairs what is missing (it lists the frame, then replays).
    /// Retrying the same transaction is safe: an insert finds the changeset
    /// ([`crate::InsertOutcome::AlreadyPresent`]), and a ref move finds the ref at the caller's
    /// own target ([`crate::CasResult::Mismatch`], with that target as `actual`).
    #[error("the transaction committed at frame {frame_seq}, but a step after it failed: {source}")]
    PostCommit {
        /// The seq of the transaction's frame.
        frame_seq: u64,
        /// The step's error.
        source: Box<StoreError>,
    },
    /// A write transaction's journal append failed, and the writer, still holding
    /// `meta/lock`, cut what the append left back off `journal.log` and synced that cut: the
    /// transaction did not commit, and no later writer can list what the append wrote. The
    /// append is the transaction's own frame's, or, when the sync of the tail check that opens
    /// a write fails, its writing afresh of the frames `journal.off` did not list yet (those
    /// frames are cut back off with it). Or the tail check's listing of such a frame in
    /// `journal.off` failed, and what reached `journal.off` of the entry was cut back off it:
    /// the transaction never began, and the next tail check lists the frame. Retrying the
    /// transaction is safe: nothing of it is in the store. Retryable.
    #[error(
        "the transaction did not commit: a journal append failed, and what it wrote was cut \
         back off the journal: {source}"
    )]
    NotCommitted {
        /// The append's failure: of the write, or of the fdatasync after it.
        source: IoFailure,
    },
}

/// Which limit of the store's layout a request went past ([`StoreError::Limit`]; storage spec
/// §2's `REPO` line and rule 1, and §3, "커밋 결과·오류·한도").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitKind {
    /// A changeset's parents: a journal record and `par.col` count them in one byte (255).
    Parents,
    /// A ref move's actor, in bytes: one byte of the record holds its length (255).
    ActorBytes,
    /// A ref move's ref name, in bytes, as the journal encodes it (255).
    /// [`crate::MetaWrite::cas_ref`] refuses a longer name first, as [`StoreError::RefName`].
    RefNameBytes,
    /// The records of one transaction: a frame counts them in a `u16le` (65,535).
    Records,
    /// A repository's name, in bytes (255): `REPO` holds it after the id line with no length
    /// field, so the limit is a choice, the one ref names and actors have. The repository's
    /// create refuses a longer name before it makes anything.
    RepoNameBytes,
    /// An object's payload, in bytes: [`crate::MAX_OBJECT_LEN`] (512 MiB). A put refuses a longer
    /// payload before it hashes or writes anything, and a batch put writes none of its objects.
    /// The API layer makes it `E_TOO_LARGE`, where every other limit is `E_INVALID_ARG`.
    ObjectBytes,
}

impl LimitKind {
    /// Every limit, in declaration order.
    pub const ALL: [LimitKind; 6] = [
        Self::Parents,
        Self::ActorBytes,
        Self::RefNameBytes,
        Self::Records,
        Self::RepoNameBytes,
        Self::ObjectBytes,
    ];

    /// Its name in the API's `details.store_limit`: `"parents"`, `"actor_bytes"`,
    /// `"ref_name_bytes"`, `"records"`, `"repo_name_bytes"` or `"object_bytes"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Parents => "parents",
            Self::ActorBytes => "actor_bytes",
            Self::RefNameBytes => "ref_name_bytes",
            Self::Records => "records",
            Self::RepoNameBytes => "repo_name_bytes",
            Self::ObjectBytes => "object_bytes",
        }
    }
}

/// What the limit counts, as a message says it: "a changeset's parents", "an actor's bytes",
/// "a ref name's bytes", "a transaction's records", "a repository name's bytes", "an object's
/// bytes".
impl fmt::Display for LimitKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Parents => "a changeset's parents",
            Self::ActorBytes => "an actor's bytes",
            Self::RefNameBytes => "a ref name's bytes",
            Self::Records => "a transaction's records",
            Self::RepoNameBytes => "a repository name's bytes",
            Self::ObjectBytes => "an object's bytes",
        })
    }
}

/// What the store asked of the filesystem, when it failed: one of the 14 [`crate::FsOps`]
/// methods ([`FileOp::Fs`], by its [`FsMethod`]), or a call the store makes on its own handles
/// and paths, off that seam: its reads of the metadata (storage spec §8), the lock, `init`'s
/// working out of where the store's path leads, and compressing an object. A closed list of 22;
/// [`FileOp::as_str`] gives the names `details.fs_op` carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileOp {
    /// A method of the [`crate::FsOps`] seam, named as [`FsMethod::as_str`] names it.
    Fs(FsMethod),
    /// The open of a file the store reads on its own handle: `journal.log`, `journal.off`, an
    /// index file, `meta/lock`.
    Open,
    /// A positioned read (`pread`) on such a handle.
    ReadAt,
    /// A measurement of a file's length, or of whether a path exists (`fstat`, `stat`).
    Stat,
    /// A listing of a directory: `meta/refs/release/`, an objects fan-out directory.
    ReadDir,
    /// Taking `meta/lock`'s exclusive lock, or waiting for it.
    Lock,
    /// The removal of a directory and all it holds: a crashed create's `meta.tmp/`.
    RemoveDirAll,
    /// Working out where a store's path leads: made absolute, its symlinks followed, made
    /// canonical. It names no file of the store.
    Resolve,
    /// The compression (zstd) of an object being written.
    Compress,
}

impl FileOp {
    /// Every operation: the [`crate::FsOps`] methods in [`FsMethod::ALL`]'s order, then the
    /// store's own in declaration order.
    pub const ALL: [FileOp; 22] = {
        const OWN: [FileOp; 8] = [
            FileOp::Open,
            FileOp::ReadAt,
            FileOp::Stat,
            FileOp::ReadDir,
            FileOp::Lock,
            FileOp::RemoveDirAll,
            FileOp::Resolve,
            FileOp::Compress,
        ];
        let mut ops = [FileOp::Open; 22];
        let mut i = 0;
        while i < FsMethod::ALL.len() {
            ops[i] = FileOp::Fs(FsMethod::ALL[i]);
            i += 1;
        }
        let mut j = 0;
        while j < OWN.len() {
            ops[FsMethod::ALL.len() + j] = OWN[j];
            j += 1;
        }
        ops
    };

    /// Its name in the API's `details.fs_op`: the method's for [`FileOp::Fs`]
    /// (`"append_durable"`, say), else `"open"`, `"read_at"`, `"stat"`, `"read_dir"`, `"lock"`,
    /// `"remove_dir_all"`, `"resolve"` or `"compress"`.
    pub const fn as_str(self) -> &'static str {
        match self {
            FileOp::Fs(method) => method.as_str(),
            FileOp::Open => "open",
            FileOp::ReadAt => "read_at",
            FileOp::Stat => "stat",
            FileOp::ReadDir => "read_dir",
            FileOp::Lock => "lock",
            FileOp::RemoveDirAll => "remove_dir_all",
            FileOp::Resolve => "resolve",
            FileOp::Compress => "compress",
        }
    }
}

/// The method, as an operation ([`FileOp::Fs`]).
impl From<FsMethod> for FileOp {
    fn from(method: FsMethod) -> FileOp {
        FileOp::Fs(method)
    }
}

/// Its name ([`FileOp::as_str`]).
impl fmt::Display for FileOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A filesystem operation of the store's that failed: what it was, the file or directory of the
/// store it was on, and the operating system's error as it came. [`StoreError::Io`] and
/// [`StoreError::NotCommitted`] carry it; [`crate::StoreFs`] makes it, for every
/// [`crate::FsOps`] call and for the store's calls on its own handles.
///
/// It displays as `"<op> <file>: <error>"` (`"sync_data meta/journal.off: Input/output error (os
/// error 5)"`), or `"<op>: <error>"` with no file, then `"; <also>"` if something else failed;
/// its [`std::error::Error::source`] is the operating system's error.
#[derive(Debug)]
pub struct IoFailure {
    /// The operation that failed.
    pub op: FileOp,
    /// The file or directory it was on, relative to the store's root (`meta/journal.off`,
    /// `objects/ab/<62 hex digits>`; `.` for the root itself): never absolute, never with a
    /// `..`. `None` for a path outside the root (the directories `init` works out and makes
    /// above the store) and for a failure on no one path.
    pub file: Option<Utf8PathBuf>,
    /// The operating system's error, untouched: its `kind()` and `raw_os_error()` hold.
    pub source: io::Error,
    /// What else failed while the store handled this failure (cutting back what a failed append
    /// wrote, say), or what the failed call was doing, as one line; `None` when there is
    /// nothing more to say.
    pub also: Option<String>,
}

impl IoFailure {
    /// The failure of `op` on `file` (relative to the store's root), with the operating
    /// system's error `source`, and nothing else.
    pub fn new(op: FileOp, file: Option<Utf8PathBuf>, source: io::Error) -> IoFailure {
        IoFailure {
            op,
            file,
            source,
            also: None,
        }
    }

    /// The operating system's error's kind.
    pub fn kind(&self) -> io::ErrorKind {
        self.source.kind()
    }

    /// The operating system's error number, if it gave one (an error a test double or the store
    /// made up has none).
    pub fn raw_os_error(&self) -> Option<i32> {
        self.source.raw_os_error()
    }

    /// This failure, with `also` said of it too: after what was said already, if anything was,
    /// following a `"; "`.
    #[must_use]
    pub fn with_also(mut self, also: impl Into<String>) -> IoFailure {
        let also = also.into();
        self.also = Some(match self.also.take() {
            Some(before) => format!("{before}; {also}"),
            None => also,
        });
        self
    }
}

impl fmt::Display for IoFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.file {
            Some(file) => write!(f, "{} {file}: {}", self.op, self.source)?,
            None => write!(f, "{}: {}", self.op, self.source)?,
        }
        match &self.also {
            Some(also) => write!(f, "; {also}"),
            None => Ok(()),
        }
    }
}

impl std::error::Error for IoFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

// Nothing turns a bare `io::Error` into a `StoreError`: a failure is located first, through
// `StoreFs` or `IoFailure::new`. This fails to compile if a `From<io::Error>` comes back: both
// impls below then apply to `StoreError`, and the type `_` stands for is ambiguous.
const _: fn() = || {
    trait FromIoError<Marker> {
        fn check() {}
    }
    impl<T: ?Sized> FromIoError<()> for T {}
    impl<T: From<io::Error>> FromIoError<u8> for T {}
    let _ = <StoreError as FromIoError<_>>::check;
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limit_errors_say_which_limit_and_by_how_much() {
        let limit = StoreError::Limit {
            what: LimitKind::Parents,
            max: 255,
            got: 256,
        };
        assert_eq!(
            limit.to_string(),
            "a changeset's parents: at most 255, not 256"
        );
        assert_eq!(
            LimitKind::ALL.map(LimitKind::as_str),
            [
                "parents",
                "actor_bytes",
                "ref_name_bytes",
                "records",
                "repo_name_bytes",
                "object_bytes",
            ]
        );
        assert_eq!(
            LimitKind::ALL.map(|what| what.to_string()),
            [
                "a changeset's parents",
                "an actor's bytes",
                "a ref name's bytes",
                "a transaction's records",
                "a repository name's bytes",
                "an object's bytes",
            ]
        );
    }

    /// An I/O failure says which operation failed, on which file of the store (relative to its
    /// root), and the operating system's error as it came, its errno kept; what else failed
    /// while the store handled it follows. `FileOp::ALL` names 22 operations: the 14 `FsOps`
    /// methods first, by the names `FsMethod` gives them, then the store's own.
    #[test]
    fn an_io_failure_says_what_failed_and_where() {
        let sync = || {
            IoFailure::new(
                FileOp::Fs(FsMethod::SyncData),
                Some("meta/journal.off".into()),
                io::Error::from_raw_os_error(5),
            )
        };
        let failure = sync();
        assert_eq!(
            failure.to_string(),
            "sync_data meta/journal.off: Input/output error (os error 5)"
        );
        assert_eq!(failure.raw_os_error(), Some(5));
        assert_eq!(failure.kind(), failure.source.kind());
        let source = std::error::Error::source(&failure).map(ToString::to_string);
        assert_eq!(source.as_deref(), Some("Input/output error (os error 5)"));
        assert_eq!(
            StoreError::Io(sync()).to_string(),
            "i/o error: sync_data meta/journal.off: Input/output error (os error 5)"
        );
        let lock = IoFailure::new(FileOp::Lock, None, io::Error::other("no lock"));
        assert_eq!(lock.to_string(), "lock: no lock");
        assert!(sync().with_also("x").to_string().ends_with("; x"));
        assert_eq!(
            sync().with_also("x").with_also("y").also.as_deref(),
            Some("x; y")
        );
        let append = IoFailure::new(
            FileOp::Fs(FsMethod::AppendDurable),
            Some("meta/journal.log".into()),
            io::Error::from_raw_os_error(28),
        );
        let not_committed = StoreError::NotCommitted { source: append };
        assert!(
            not_committed.to_string().ends_with(
                ": append_durable meta/journal.log: No space left on device (os error 28)"
            ),
            "{not_committed}"
        );

        let names: Vec<&str> = FileOp::ALL.iter().map(|op| op.as_str()).collect();
        let methods: Vec<&str> = FsMethod::ALL.iter().map(|method| method.as_str()).collect();
        assert_eq!(names[..14], methods[..], "the FsOps methods first");
        assert_eq!(
            names[14..],
            [
                "open",
                "read_at",
                "stat",
                "read_dir",
                "lock",
                "remove_dir_all",
                "resolve",
                "compress",
            ]
        );
        let mut distinct = names.clone();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(distinct.len(), 22, "22 distinct names");
        for op in FileOp::ALL {
            assert_eq!(op.to_string(), op.as_str());
        }
        for method in FsMethod::ALL {
            assert_eq!(FileOp::from(method), FileOp::Fs(method));
        }
    }
}
