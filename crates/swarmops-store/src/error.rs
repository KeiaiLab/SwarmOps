//! The store's error type.

use std::{fmt, io};

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
    /// frames are cut back off with it). Retrying the transaction is safe: nothing of it is in
    /// the store. Retryable.
    #[error(
        "the transaction did not commit: a journal append failed, and what it wrote was cut \
         back off the journal: {source}"
    )]
    NotCommitted {
        /// The append's error: of the write, or of the fdatasync after it.
        source: io::Error,
    },
}

/// Which limit of the store's layout a request went past ([`StoreError::Limit`]; storage spec
/// §2's `REPO` line, and §3, "커밋 결과·오류·한도").
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
}

impl LimitKind {
    /// Every limit, in declaration order.
    pub const ALL: [LimitKind; 5] = [
        Self::Parents,
        Self::ActorBytes,
        Self::RefNameBytes,
        Self::Records,
        Self::RepoNameBytes,
    ];

    /// Its name in the API's `details.store_limit`: `"parents"`, `"actor_bytes"`,
    /// `"ref_name_bytes"`, `"records"` or `"repo_name_bytes"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Parents => "parents",
            Self::ActorBytes => "actor_bytes",
            Self::RefNameBytes => "ref_name_bytes",
            Self::Records => "records",
            Self::RepoNameBytes => "repo_name_bytes",
        }
    }
}

/// What the limit counts, as a message says it: "a changeset's parents", "an actor's bytes",
/// "a ref name's bytes", "a transaction's records", "a repository name's bytes".
impl fmt::Display for LimitKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Parents => "a changeset's parents",
            Self::ActorBytes => "an actor's bytes",
            Self::RefNameBytes => "a ref name's bytes",
            Self::Records => "a transaction's records",
            Self::RepoNameBytes => "a repository name's bytes",
        })
    }
}

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
            ]
        );
    }
}
