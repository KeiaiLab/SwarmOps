//! The store's error type.

use std::io;

use camino::Utf8PathBuf;
use swarmops_core::{ChangesetId, CodecError, ObjectId};
use thiserror::Error;

/// Why a store operation failed.
#[derive(Debug, Error)]
pub enum StoreError {
    /// The filesystem refused an operation.
    #[error("i/o error: {0}")]
    Io(#[from] io::Error),
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
    /// A journal frame failed its checks where a torn write cannot explain it: a frame that
    /// `journal.off` lists fails its CRC, a frame's records do not decode, or a frame or an
    /// offset entry is malformed.
    #[error("the metadata journal is corrupt at offset {offset}")]
    JournalCorrupt {
        /// Where in `journal.log` the bad frame starts (for a bad `journal.off` entry, the
        /// offset the entry holds); from [`crate::Record::decode`], the position in its
        /// input where decoding stopped.
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
}
