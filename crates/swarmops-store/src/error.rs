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
    /// transaction did not commit, and no later writer can list its frame. Retrying it is safe:
    /// nothing of it is in the store. Retryable.
    #[error(
        "the transaction did not commit: appending its journal frame failed, and the frame was \
         cut back off the journal: {source}"
    )]
    NotCommitted {
        /// The append's error: of the write, or of the fdatasync after it.
        source: io::Error,
    },
}

/// An `InvalidInput` I/O error: what the metadata store returns for a request it cannot carry
/// out as asked (a record past what its layout holds, a rec past the index's end).
pub(crate) fn invalid_input(message: String) -> StoreError {
    io::Error::new(io::ErrorKind::InvalidInput, message).into()
}
