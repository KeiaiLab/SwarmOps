//! Error codes and the envelope's error object (operations spec §0).
//!
//! [`ErrorCode`] is the one table that gives every failure its spec `E_*` string, the process
//! exit code and whether retrying can help; [`OpError`] is what an operation returns and what
//! the CLI prints under `error`.

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;

/// Exit code 1: the operation failed.
const EXIT_ERROR: i32 = 1;
/// Exit code 2: the request or the command line is malformed.
const EXIT_USAGE: i32 = 2;
/// Exit code 3: the change conflicts with what it lands on; resolve it, then run the operation
/// again.
const EXIT_CONFLICT: i32 = 3;
/// Exit code 4: the operation can succeed when it is run again.
const EXIT_RETRYABLE: i32 = 4;
/// Exit code 5: an identity or policy requirement refused the operation.
const EXIT_AUTH: i32 = 5;
/// Exit code 6: something the request names does not exist.
const EXIT_NOT_FOUND: i32 = 6;

/// Declares [`ErrorCode`] from one table whose rows read `Variant => "E_STRING", exit,
/// retryable;`. The enum (serialized as its `E_*` string), [`ErrorCode::ALL`] and the accessors
/// all come from the same rows, so they cannot disagree.
macro_rules! error_codes {
    ($($(#[$doc:meta])* $variant:ident => $code:literal, $exit:expr, $retryable:literal;)+) => {
        /// Why an operation failed: a stable code with a fixed exit code and retry advice.
        ///
        /// Serialized as its `E_*` string, which `OpError.code` carries too, so a client can read
        /// codes newer than its copy of this enum.
        #[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
        #[non_exhaustive]
        pub enum ErrorCode {
            $($(#[$doc])* #[serde(rename = $code)] $variant,)+
        }

        impl ErrorCode {
            /// Every code, in declaration order.
            pub const ALL: &'static [ErrorCode] = &[$(ErrorCode::$variant),+];

            /// The code's row of the table: `E_*` string, exit code, retryable.
            const fn row(self) -> (&'static str, i32, bool) {
                match self {
                    $(ErrorCode::$variant => ($code, $exit, $retryable),)+
                }
            }
        }
    };
}

error_codes! {
    /// `init` found a store at the target path already.
    ExistsAlready => "E_EXISTS", EXIT_ERROR, false;
    /// No store at the given path, or no store was named.
    NotARepo => "E_NOT_A_REPO", EXIT_NOT_FOUND, false;
    /// A mutation was asked for without an identity.
    IdentityRequired => "E_IDENTITY_REQUIRED", EXIT_AUTH, false;
    /// A revision names no changeset.
    RevNotFound => "E_REV_NOT_FOUND", EXIT_NOT_FOUND, false;
    /// An id prefix matches more than one changeset; give more digits.
    AmbiguousId => "E_AMBIGUOUS_ID", EXIT_ERROR, false;
    /// The path does not exist at that revision.
    PathNotFound => "E_PATH_NOT_FOUND", EXIT_NOT_FOUND, false;
    /// A file operation named a directory.
    IsDir => "E_IS_DIR", EXIT_ERROR, false;
    /// The content is larger than the operation allows.
    TooLarge => "E_TOO_LARGE", EXIT_ERROR, false;
    /// The commit changes nothing and `allow_empty` is off.
    EmptyCommit => "E_EMPTY_COMMIT", EXIT_ERROR, false;
    /// A change puts an entry below a file, or a file onto a directory.
    TypeConflict => "E_TYPE_CONFLICT", EXIT_ERROR, false;
    /// The parent changeset is not in the store.
    // Exit 6: a not-found, though the name does not say so.
    UnknownParent => "E_UNKNOWN_PARENT", EXIT_NOT_FOUND, false;
    /// The ref no longer holds the expected target (or, with `create`, exists at another one);
    /// `details.actual` holds what it does, `null` when the ref does not exist.
    CasMismatch => "E_CAS_MISMATCH", EXIT_RETRYABLE, true;
    /// No ref by that name.
    RefNotFound => "E_REF_NOT_FOUND", EXIT_NOT_FOUND, false;
    /// The ref name is one no store format takes, or one the operation does not move:
    /// `details.reason` is `reserved` for a `remote/` name, which only a fetch writes, and `tag`
    /// for a tag named as the target of a land or a commit's `advance`.
    RefNameInvalid => "E_REF_NAME_INVALID", EXIT_ERROR, false;
    /// Stored data failed verification. A metadata file that does not parse also names
    /// `details.fs_op` and `details.store_file`.
    StoreCorrupt => "E_STORE_CORRUPT", EXIT_ERROR, false;
    /// A malformed argument or request, including a command-line usage error.
    InvalidArg => "E_INVALID_ARG", EXIT_USAGE, false;
    /// The store path is not on a native Linux filesystem (for example WSL's `/mnt/*`).
    UnsupportedFs => "E_UNSUPPORTED_FS", EXIT_ERROR, false;
    /// The filesystem refused an operation: `details.fs_op` names it, and `details.store_file`
    /// the file of the store it was on, relative to the store's root. A journal append it
    /// refused for want of permission or as a read-only filesystem also says
    /// `details.committed: false`: nothing was committed, and running the same operation again
    /// fails the same way until the hint's cause is fixed.
    Io => "E_IO", EXIT_ERROR, false;
    /// A journal append failed, and the store cut what it wrote back off the journal
    /// (`details.committed` is `false`): nothing was committed, so running the same operation
    /// again is safe. A failure after the commit point is no error: the operation succeeds,
    /// with a warning. `details.fs_op` and `details.store_file` name the append that failed.
    IoTransient => "E_IO_TRANSIENT", EXIT_RETRYABLE, true;
    /// Another writer held the store's metadata lock for the whole wait, or commits landed during
    /// every one of a read's attempts to take a snapshot; nothing was committed (a commit's
    /// flushed objects may remain, for GC), and running the operation again can succeed.
    Busy => "E_BUSY", EXIT_RETRYABLE, true;
    /// A bug in SwarmOps, such as a caught panic.
    Internal => "E_INTERNAL", EXIT_ERROR, false;
    // M4's merge and landing: `land` raises the first three, `undo` the other two.
    /// The change conflicts with the target: `details.report` lists each conflicting file with
    /// its kind, its hunks and who changed it on the target. Nothing was recorded; resolve the
    /// conflicts, then land again (the `hint` says how).
    Conflict => "E_CONFLICT", EXIT_CONFLICT, false;
    /// The target moved before every one of the land's attempts could publish; nothing was
    /// recorded, and running the land again can succeed.
    CasRetryExhausted => "E_CAS_RETRY_EXHAUSTED", EXIT_RETRYABLE, true;
    /// The land has nothing to add to the target: its head is on the target already, or its
    /// change is (`details.reason` says which).
    NothingToLand => "E_NOTHING_TO_LAND", EXIT_ERROR, false;
    /// The operation moved no ref, so there is nothing to restore.
    NothingToUndo => "E_NOTHING_TO_UNDO", EXIT_ERROR, false;
    /// The store has no operation by that number.
    OpNotFound => "E_OP_NOT_FOUND", EXIT_NOT_FOUND, false;
    // `serve`'s (M14): the caller and the store's access file.
    /// The serving entry could not name the caller, or no grant of the store's access file
    /// names it.
    Auth => "E_AUTH", EXIT_AUTH, false;
    /// The store's access file grants this identity no such action.
    PolicyDenied => "E_POLICY_DENIED", EXIT_AUTH, false;
    /// Another gc, repair or upgrade holds the store's maintenance lease; nothing was done, and
    /// running the operation again once it is done can succeed.
    MaintenanceBusy => "E_MAINTENANCE_BUSY", EXIT_RETRYABLE, true;
    // The store formats (M6).
    /// The ref name is one the store's format does not take but a later one does: `details`
    /// names `name`, `needs` (the format that takes it) and `store` (the store's); `swarmops
    /// upgrade` moves the store to it.
    FormatRequired => "E_FORMAT_REQUIRED", EXIT_ERROR, false;
    /// A tag was asked to move from one target to another: a tag is created and deleted, never
    /// moved (`details.name`). Nothing was written.
    TagImmutable => "E_TAG_IMMUTABLE", EXIT_ERROR, false;
    /// `main` was asked to be deleted: it is never deleted (`details.name`). Nothing was
    /// written.
    RefProtected => "E_REF_PROTECTED", EXIT_ERROR, false;
    // `git import` (M3): the source repository and its history.
    /// `git import` found no git repository it can open at `git_path` (`details.git_path`).
    GitNotARepo => "E_GIT_NOT_A_REPO", EXIT_NOT_FOUND, false;
    /// `git import`: the source repository has no `git_ref`, or it names no commit
    /// (`details.git_ref`; `details.kind` the object it names instead, such as a tree).
    GitRefNotFound => "E_GIT_REF_NOT_FOUND", EXIT_NOT_FOUND, false;
    /// `git import`: the target ref's head is not the import of `git_ref`'s commit or of an
    /// ancestor of it, because the source was rewritten or a native changeset was committed on
    /// the target, so nothing was written; there is no silent merge. `details` names
    /// `target_ref`, `target_head`, `git_head` and `base`, the git commit the head stands for
    /// (the one it holds, or for a native head the one it exports to); and `external_id`
    /// (`commit`, `changeset`) where the import passed over an external id entry whose changeset
    /// stands for another commit, which `verify --deep` reports.
    GitImportDiverged => "E_GIT_IMPORT_DIVERGED", EXIT_CONFLICT, false;
    /// `git import`: a commit holds content a changeset cannot keep as git has it; `details`
    /// names the `commit`, the `path` (also as `path_base64` when it is not UTF-8) and the
    /// lossy `class`, which the request's `lossy` can allow. The import stops before that
    /// commit, and the target ref does not move.
    GitImportLossy => "E_GIT_IMPORT_LOSSY", EXIT_ERROR, false;
    /// `git import` does not read the source: a SHA-256 repository, reftable refs, a shallow
    /// clone, a FIFO, a socket or a device where it reads a file of the git directory, a file it
    /// reads whole there that is longer than 16 MiB, a commit of more than 255 parents, or an
    /// entry whose mode is no kind of entry; `details.reason` says which (`object_format`,
    /// `ref_storage`, `shallow`, `special_file`, `file_too_long`, `parents`, `mode`),
    /// `details.file` names the file of `special_file` and `file_too_long` by its path in the git
    /// directory (`details.size` and `details.max` give the long one's length and the limit), and
    /// `details.commit` names the commit of `parents` and `mode`.
    GitUnsupported => "E_GIT_UNSUPPORTED", EXIT_ERROR, false;
    /// `git import` could not read the source: an object is missing (as in a partial clone),
    /// does not hash to its id or does not parse, or the filesystem refused a read
    /// (`details.reason`; `details.object` names the object when there is one).
    GitRead => "E_GIT_READ", EXIT_ERROR, false;
    // `git export` (M8): the history it writes and the repository it writes it into.
    /// `git export`: the branch holds commits that are not the export of the source ref's head
    /// or of an ancestor of it, so the branch did not move; there is no silent merge, and the
    /// request's `force` moves it anyway. `details.reason` says why: `branch_ahead` (commits
    /// made in git on top of the export of the head itself, which `git import` brings in),
    /// `both_moved` (commits made in git on top of an older export, while the source ref moved
    /// too), `branch_moved` (the branch names a commit that is no export of this history),
    /// `source_moved_back` (the source ref's head is behind the export the branch holds, after
    /// an `undo` or a `ref set` backwards), `lossy_history` (the branch holds the original
    /// commits of a history imported lossily, which the export would replace) or
    /// `moved_during_export` (another writer moved the branch while this export wrote; running
    /// the export again settles it, and what it wrote stays). The first five are found before
    /// anything is written. `details` also names `branch`, `git_head` (the commit the branch
    /// holds), `head_changeset` and, when the branch holds the export of a changeset,
    /// `exported_changeset`.
    GitExportDiverged => "E_GIT_EXPORT_DIVERGED", EXIT_CONFLICT, false;
    /// `git export` could not write the repository; `details.reason` says what failed: `io` (the
    /// filesystem refused a write; `details.path` names the file, relative to the git
    /// directory), `ref_locked` (the branch's lock file, `refs/heads/<branch>.lock`, exists:
    /// another git process holds it, or an earlier export died holding it, and git itself says
    /// to remove it once no git process runs; `details.path` names it) or `ref_name_conflict`
    /// (an existing ref stands where the branch's name needs a directory, or the other way
    /// round; `details.branch`). Running the export again after the cause is gone writes what
    /// is still missing.
    GitWrite => "E_GIT_WRITE", EXIT_ERROR, false;
    // The git remotes (M13): `fetch`, `push`, `sync` and the commands that ask a remote.
    /// A push or a sync found the remote branch elsewhere than it can move from: `details.reason`
    /// says why (`remote_moved`: it moved since the last fetch, and the push's compare-and-swap
    /// sent nothing; `not_fast_forward`; `behind`: the store ref is behind the remote;
    /// `diverged`; `lossy_history`; `no_common_base`: the sync found no changeset both hold),
    /// with `remote`, `branch` and, where known, `expected` and `main_moved`. Nothing was sent;
    /// the hint names the fetch or the sync to run.
    RemoteDiverged => "E_REMOTE_DIVERGED", EXIT_CONFLICT, false;
    /// The remote or git refused: `details.reason` is `auth`, `not_found`, `declined` (a hook or
    /// a rule of the remote's), `bad_objects` (a fetch received an object git's fsck refuses, or
    /// one its refs reach is missing; no tracking ref moved) or `unclassified` (a failure git's
    /// messages do not explain, never retried), with `remote` and what git said (`refs`,
    /// `remote_messages`, `git_exit`, `stderr_tail`, the token redacted).
    RemoteRejected => "E_REMOTE_REJECTED", EXIT_AUTH, false;
    /// The network or the remote's host did not answer: `details.phase` (`fetch`, `push`,
    /// `ls_remote`) and `details.cause` (`dns`, `connect`, `timeout`, `tls`, `disconnected`,
    /// `ssh`). Nothing changed in the store; running the command again can succeed.
    RemoteUnreachable => "E_REMOTE_UNREACHABLE", EXIT_RETRYABLE, true;
    /// The store records no remote of that name (`details.remote`).
    RemoteNotFound => "E_REMOTE_NOT_FOUND", EXIT_NOT_FOUND, false;
    // The workspaces (M5): client-side operations on a directory of the caller's host.
    /// `ws create` found a workspace of that name already, made by other arguments
    /// (`details.name`, `details.path` of the existing one).
    WsExists => "E_WS_EXISTS", EXIT_ERROR, false;
    /// No workspace by that name, or at that path (`details.name` when named).
    WsNotFound => "E_WS_NOT_FOUND", EXIT_NOT_FOUND, false;
    /// Another mutation held the workspace's lock for the whole wait (`details.holder`, when it
    /// is known); running the operation again can succeed.
    WsLocked => "E_WS_LOCKED", EXIT_RETRYABLE, true;
    /// The workspace holds edits the operation would overwrite or lose (`details.paths`, the
    /// first 20); the hint names the command that commits or discards them.
    WsDirty => "E_WS_DIRTY", EXIT_ERROR, false;
    /// `ws create` was given a directory that holds something already (`details.path`).
    PathNotEmpty => "E_PATH_NOT_EMPTY", EXIT_ERROR, false;
    /// A rebase still has conflicting paths no one resolved (`details.paths`).
    UnresolvedConflicts => "E_UNRESOLVED_CONFLICTS", EXIT_ERROR, false;
    /// `ws resolve` found a conflict marker line still in a file (`details.paths`).
    MarkersPresent => "E_MARKERS_PRESENT", EXIT_ERROR, false;
    /// The tree holds paths the workspace's volume cannot hold: `details.reason` says why
    /// (`case_collision`, `windows_name`, `reserved_name`) and `details.paths` names them.
    /// Nothing was written.
    WsUnportable => "E_WS_UNPORTABLE", EXIT_ERROR, false;
    /// A workspace's check or bootstrap command failed or ran past its time
    /// (`details.stage`: `check` or `bootstrap`; `exit_code`, `timed_out`, the tails of its
    /// output, `log`, `changeset`). Nothing was landed.
    CheckFailed => "E_CHECK_FAILED", EXIT_ERROR, false;
}

impl ErrorCode {
    /// The spec's `E_*` string, e.g. `"E_CAS_MISMATCH"`.
    pub fn as_str(&self) -> &'static str {
        self.row().0
    }

    /// The process exit code: 1 error, 2 usage, 3 conflict, 4 retryable, 5 identity or policy, 6
    /// not found.
    pub fn exit(&self) -> i32 {
        self.row().1
    }

    /// Whether the operation can succeed when it is run again: as it is after a transient
    /// failure, or, after a lost compare-and-swap, with what the error names re-read (the
    /// message says how; the `hint` reads it).
    pub fn retryable(&self) -> bool {
        self.row().2
    }
}

/// A failed operation: the envelope's `error` object.
///
/// `code`, `exit` and `retryable` come from one `ErrorCode`. `code` stays a string so a client
/// can read codes newer than its own table.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq, Error)]
#[error("{code}: {message}")]
#[schemars(transform = require_always_written)]
#[non_exhaustive]
pub struct OpError {
    /// The `E_*` code.
    pub code: String,
    /// What went wrong.
    pub message: String,
    /// Whether the operation can succeed when it is run again: as it is after a transient
    /// failure, or, after a lost compare-and-swap, with what the error names re-read (the
    /// message says how; the `hint` reads it).
    pub retryable: bool,
    /// The exit code of the CLI.
    pub exit: i32,
    /// Machine-readable context, such as `actual` for `E_CAS_MISMATCH`; `{}` when there is none.
    #[serde(default, deserialize_with = "null_as_empty")]
    pub details: Map<String, Value>,
    /// The next command to run when there is an obvious one, else null.
    pub hint: Option<String>,
}

/// Reads `details`, taking an explicit `null` as `{}`.
fn null_as_empty<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Map<String, Value>, D::Error> {
    Ok(Option::<Map<String, Value>>::deserialize(deserializer)?.unwrap_or_default())
}

/// Marks `details` and `hint` required in `OpError`'s schema, since both are always written.
/// (`#[schemars(required)]` on `hint` would also drop `null` from its type.)
fn require_always_written(schema: &mut schemars::Schema) {
    if let Some(Value::Array(required)) = schema.get_mut("required") {
        required.extend(["details", "hint"].map(Value::from));
    }
}

impl OpError {
    /// An error with `code`'s string, exit code and retry advice, no details (`{}`) and no hint.
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code: code.as_str().to_owned(),
            message: message.into(),
            retryable: code.retryable(),
            exit: code.exit(),
            details: Map::new(),
            hint: None,
        }
    }

    /// Replaces the message, keeping the code, details and hint.
    #[must_use]
    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = message.into();
        self
    }

    /// Replaces the details.
    #[must_use]
    pub fn with_details(mut self, details: Map<String, Value>) -> Self {
        self.details = details;
        self
    }

    /// Sets the hint.
    #[must_use]
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}
