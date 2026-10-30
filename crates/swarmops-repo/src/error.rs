//! The repository's error type.

use std::io;

use swarmops_core::{BlobId, ChangesetId, CodecError, ObjectId, ObjectKind, RepoPath};
use swarmops_store::{IoFailure, Recovered, StoreError};
use thiserror::Error;

use crate::land::{MergeConflicts, NothingToLandReason};
use crate::oplog::{OpId, OpNotFoundReason};

/// Why a repository operation failed.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RepoError {
    /// The store failed: I/O, a corrupt object file, damaged metadata, a writer lock held for
    /// the whole wait ([`StoreError::Busy`], retryable), commits landing during every round of a
    /// snapshot ([`StoreError::SnapshotContended`], retryable), a journal append that failed and
    /// was undone ([`StoreError::NotCommitted`], retryable but for a refusal that lasts: for want
    /// of permission, or by a read-only filesystem), or a store that is not what the call
    /// needs ([`StoreError::Format`]: `open` of a directory that holds no store, or one of a
    /// layout this build does not read; [`StoreError::UnsupportedFs`]: a path under `/mnt/`).
    #[error(transparent)]
    Store(#[from] StoreError),
    /// An object is not canonical: a stored one does not decode, or the changeset a commit
    /// request describes cannot be encoded (metadata keys not strictly ascending, a `-0000`
    /// signature with a non-zero offset). A commit checks this before it writes anything.
    #[error("not a canonical object: {0}")]
    Codec(#[from] CodecError),
    /// A metadata key ends in `:b64` ([`crate::RESERVED_METADATA_KEY_SUFFIX`]), which the
    /// operations' `show` reserves: it gives a value that is not UTF-8 in base64, under its key
    /// with that suffix appended, so a stored key that ended in it could not be told from such
    /// an entry. Every writer refuses it before it writes anything ([`crate::check_metadata`]).
    #[error(
        "metadata key {key:?} ends in {suffix:?}, which show reserves",
        suffix = crate::RESERVED_METADATA_KEY_SUFFIX
    )]
    ReservedMetadataKey {
        /// The first key of the metadata that ends in it, as given.
        key: String,
    },
    /// A change puts a file below a file, or where a directory is, or deletes a directory: in
    /// the first parent's tree, or against another change of the same request.
    #[error("{path}: a file and a directory conflict there")]
    TypeConflict {
        /// The path of the change that conflicts.
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
    /// A parent the commit names is not in the store, nor, for a raw changeset, earlier in its
    /// batch ([`crate::Repo::commit_raw_batch`]).
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
    /// A change names by its id a blob the store does not hold: no object of that id, or one
    /// of another kind ([`crate::ChangeOp::PutBlob`]). Nothing was written; store the content
    /// first ([`crate::Repo::put_blob`]).
    #[error("{path}: the store holds no blob {id}")]
    MissingBlob {
        /// The path of the change.
        path: RepoPath,
        /// The id it names.
        id: BlobId,
    },
    /// The store lacks an object that its metadata or its trees reference, or holds an object
    /// of another kind under that id: a damaged store ([`crate::Repo::verify`] says more). For a
    /// raw changeset ([`crate::Repo::commit_raw_batch`]) it is the root it names, or a tree below
    /// that its delta's diff reads, which the caller was to store first: nothing was written.
    #[error("the store lacks {kind:?} object {id}")]
    MissingObject {
        /// The id referenced.
        id: ObjectId,
        /// The kind the reference expects.
        kind: ObjectKind,
    },
    /// A derived value cannot be made ([`crate::derive`]): its derivation is not registered, or
    /// is refused at registration ([`crate::derive::Registry::register`] says what it refuses),
    /// the values it needs form a cycle, a call returns `Missing` for a value it was given, or a
    /// key or a value does not encode or decode. Its inputs are immutable, so asking again fails
    /// again: a bug of the derivation.
    #[error("derivation {name}: {why}")]
    Derived {
        /// The derivation's name ([`crate::derive::Derivation::NAME`]).
        name: String,
        /// What went wrong, as one line.
        why: String,
    },
    /// A value the derived store holds (storage spec §5) that a reader's checks against the index
    /// and the objects find wrong: its file is whole and its trailer right, but the value is not
    /// one its derivation gives (a first-parent skip list whose entry 0 is not the first parent,
    /// with more entries than the generation leaves room for, whose jump is not as many
    /// generations down, disagrees with the list one level down, names a changeset the store does
    /// not hold, or ends where the chain goes on; a last touch that names a changeset the store does
    /// not hold, not below the one asked about, or whose delta does not touch the path). No crash
    /// writes one: someone placed the file, or a faulty build wrote it. `verify --deep` names every
    /// such value; removing its file, or all of `meta/derived/`, has the value derived again.
    #[error("derivation {name}: its stored value for changeset {changeset} is wrong: {why}")]
    DerivedCorrupt {
        /// The derivation's name ([`crate::derive::Derivation::NAME`]).
        name: String,
        /// The changeset whose value was asked for: the value may have been read at another
        /// changeset its derivation redirected to.
        changeset: ChangesetId,
        /// What is wrong with it, as one line.
        why: String,
    },
    /// `undo` of an operation the journal does not hold: its number is past the last one, or the
    /// frame at its number has another crc. The error never gives that frame's id: undoing it
    /// would undo another operation.
    #[error(
        "there is no operation {}: {} (the last is {})",
        OpId::new(*.op, *.crc),
        not_found_why(*.reason),
        .last.map_or_else(|| "none: the store has no operation yet".to_owned(), |last| last.to_string())
    )]
    OpNotFound {
        /// The number of the operation asked for.
        op: u64,
        /// The crc of the id asked for.
        crc: u32,
        /// The store's last operation; `None` when it has none.
        last: Option<u64>,
        /// Why there is none.
        reason: OpNotFoundReason,
        /// What the write transaction's recovery repaired before the operation was looked up,
        /// of what earlier writers left: those repairs stay made. Boxed, as for
        /// [`RepoError::RefMoved`].
        recovered: Box<Recovered>,
    },
    /// `undo` of an operation that left every ref where it found it: it moved none, or moved
    /// each one back. Nothing was written.
    #[error("operation {op} moved no ref, so there is nothing to undo")]
    NothingToUndo {
        /// The operation asked for.
        op: u64,
        /// What the write transaction's recovery repaired before the operation was read, of
        /// what earlier writers left: those repairs stay made. Boxed, as for
        /// [`RepoError::RefMoved`].
        recovered: Box<Recovered>,
    },
    /// `undo` found a ref the operation moved naming neither what the operation set it to nor
    /// what it named before: a later operation moved it. Nothing was written, the moves of the
    /// operation's other refs included; undo the later operations first, or, on `main`, which
    /// moves only forward, land a change that reverses the operation.
    #[error(
        "ref {name} has moved since operation {op} set it to {}: it names {} now, so nothing was \
         undone",
        .expected.map_or_else(|| "nothing".to_owned(), |id| id.to_string()),
        .actual.map_or_else(|| "nothing".to_owned(), |id| id.to_string())
    )]
    UndoBlocked {
        /// The operation the undo was of.
        op: u64,
        /// The ref.
        name: String,
        /// What the operation set it to, and the undo expected it to name; `None` for a
        /// deletion.
        expected: Option<ChangesetId>,
        /// What it names; `None` if it does not exist.
        actual: Option<ChangesetId>,
        /// What the write transaction's recovery repaired before the undo was refused, of what
        /// earlier writers left ([`swarmops_store::MetaWrite::recovered`]): those repairs stay
        /// made. Boxed, as for [`RepoError::RefMoved`].
        recovered: Box<Recovered>,
    },
    /// A move of `main` off its first-parent history: its head `head` is not on the first-parent
    /// chain of `target` (the M15 design §4). A ref set, or an undo, that asks for it is refused
    /// whole: nothing was written. The way back is landing a change that reverts it.
    #[error(
        "{name} moves only forward: {head} is not a first-parent ancestor of {target}{}, so \
         nothing was written",
        .op.map_or_else(String::new, |op| format!(" (the undo of operation {op})"))
    )]
    HistoryProtected {
        /// The protected ref: `main`.
        name: String,
        /// Its head, which the move would drop.
        head: ChangesetId,
        /// What the move asked it to name.
        target: ChangesetId,
        /// The operation whose undo asked for the move; `None` for a ref set.
        op: Option<OpId>,
        /// What the write transaction's recovery repaired before the undo was refused, of what
        /// earlier writers left: those repairs stay made. Nothing for a ref set, which judges on
        /// a snapshot. Boxed, as for [`RepoError::RefMoved`].
        recovered: Box<Recovered>,
    },
    /// A land's change conflicts with the target's head, or some of its resolutions did not
    /// apply, since the target's head holds their paths otherwise than their `against`: the
    /// merge's conflicts and the stale paths, with what the report of them needs
    /// ([`MergeConflicts`]). Nothing was recorded; resolve them, then land again. Boxed, as the
    /// conflicts would otherwise make every result this large.
    #[error("{}", conflict_message(.0))]
    Conflict(Box<MergeConflicts>),
    /// A land has nothing to add to the target: its head is on the target's first-parent chain,
    /// or the target holds its change already ([`NothingToLandReason`]). Nothing was recorded.
    #[error("nothing to land: {0}")]
    NothingToLand(NothingToLandReason),
    /// A land's target moved before each of its attempts could publish, `attempts` of them; it
    /// names `actual` now. Nothing was recorded, and the same land made again can succeed.
    #[error(
        "the target moved before each of the land's {attempts} attempt(s) could publish (it \
         names {actual}): nothing was recorded"
    )]
    RetryExhausted {
        /// How many attempts the land made.
        attempts: u32,
        /// What the target named when the last attempt found it moved.
        actual: ChangesetId,
    },
    /// A land with `expect_target` ([`crate::LandRequest::expect_target`]) found its target
    /// naming another changeset than the one it expected: before it merged, or when its one
    /// publish lost the race. Nothing was recorded; rebase the change onto what the target names,
    /// check it, and land it again.
    #[error(
        "ref {target} names {actual}, not {expected}, which the land expected: nothing was \
         recorded"
    )]
    CasMismatch {
        /// The land's target.
        target: String,
        /// What it names.
        actual: ChangesetId,
        /// What the land expected it to name.
        expected: ChangesetId,
        /// What the publish's write transaction repaired before the move was refused, of what
        /// earlier writers left, as [`RepoError::RefMoved`] carries it; nothing when the land
        /// found the target moved before it merged. Boxed, as for [`RepoError::RefMoved`].
        recovered: Box<Recovered>,
    },
    /// A land's idempotency key landed already, as the changeset `landed`, which is not this
    /// land's own on its target: it is squashed from another head, or it is on another ref
    /// (`target`). Nothing was recorded; a land of another change, or onto another target,
    /// takes a key of its own.
    #[error(
        "land key {key:?} landed already, as {landed} on {target}: another change or target \
         takes a key of its own"
    )]
    KeyReused {
        /// The key.
        key: String,
        /// The changeset it landed.
        landed: ChangesetId,
        /// The ref whose first-parent chain holds `landed`: the land's target, or another ref.
        target: String,
    },
    /// A land squashes a first-parent chain, and this changeset of the chain from the land's
    /// base to its head has two parents or more: a merge, which v1 does not land.
    #[error("changeset {0} has more than one parent: a land squashes a chain without merges")]
    MergeChangeset(ChangesetId),
    /// A land's base is not a first-parent ancestor of its head, so the head's chain does not
    /// start from it.
    #[error("{base} is not a first-parent ancestor of {head}, so it is not the change's base")]
    BaseNotAncestor {
        /// The base the land was given.
        base: ChangesetId,
        /// The head.
        head: ChangesetId,
    },
    /// A land given no base found none: its head and the target's head `theirs` share no
    /// first-parent ancestor (their chains start from different roots). Give it the base the
    /// change starts from.
    #[error("{head} and the target's head {theirs} share no first-parent ancestor: give a base")]
    NoMergeBase {
        /// The head.
        head: ChangesetId,
        /// The target's head.
        theirs: ChangesetId,
    },
    /// [`crate::Repo::derive_eager`] was given an `after` past the journal's last frame: no
    /// call of it read that frame, so it is no frame to go on after.
    #[error("frame {after} is past the journal's end")]
    PastJournalEnd {
        /// The frame given.
        after: u64,
    },
    /// A value of a non-exhaustive enum of the store's, the core's or the merge's that this
    /// build does not handle: the fail-closed value of a wildcard arm, which a variant added
    /// to that enum without a match of its own reaches. A bug of this build, never a state of
    /// the store; the string names the enum and the value.
    #[error("internal error, a bug of this build: {0}")]
    Internal(String),
}

impl RepoError {
    /// The [`RepoError::Internal`] a wildcard arm over the foreign enum `ty` returns, e.g.
    /// `unknown_variant("CasResult")`.
    pub(crate) fn unknown_variant(ty: &str) -> Self {
        Self::Internal(format!("{ty}: a variant this build does not know"))
    }
}

/// Why [`RepoError::OpNotFound`] found none, as its message says it.
fn not_found_why(reason: OpNotFoundReason) -> &'static str {
    match reason {
        OpNotFoundReason::PastEnd => "its number is past the last operation",
        OpNotFoundReason::Checksum => {
            "the operation at its number has another crc, so the one named was taken back and \
             its number went to another, or the id is mistyped"
        }
    }
}

/// [`RepoError::Conflict`]'s message: how many paths conflict with the target's head, and how
/// many resolutions are stale.
fn conflict_message(conflicts: &MergeConflicts) -> String {
    let theirs = conflicts.theirs;
    match (conflicts.conflicts.len(), conflicts.stale.len()) {
        (n, 0) => format!(
            "{n} path(s) of the change conflict with the target's head {theirs}: nothing was \
             recorded"
        ),
        (0, stale) => format!(
            "{stale} resolution(s) are stale, as the target's head {theirs} holds their path(s) \
             otherwise than their against: nothing was recorded"
        ),
        (n, stale) => format!(
            "{n} path(s) of the change conflict with the target's head {theirs}, and {stale} \
             resolution(s) are stale, as it holds their path(s) otherwise than their against: \
             nothing was recorded"
        ),
    }
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

#[cfg(test)]
mod tests {
    use super::RepoError;

    /// `Internal` names itself a bug of this build, and keeps what the arm that made it says.
    #[test]
    fn internal_names_a_bug() {
        let error = RepoError::Internal("EntryTarget: an unknown variant".into());

        assert_eq!(
            error.to_string(),
            "internal error, a bug of this build: EntryTarget: an unknown variant"
        );
    }

    /// A wildcard arm's error names the enum it did not know.
    #[test]
    fn unknown_variant_names_the_enum() {
        let error = RepoError::unknown_variant("CasResult");

        assert!(matches!(
            &error,
            RepoError::Internal(what) if what == "CasResult: a variant this build does not know"
        ));
    }
}
