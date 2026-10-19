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
    /// The ref name is not `main` or `release/<name>`.
    RefNameInvalid => "E_REF_NAME_INVALID", EXIT_ERROR, false;
    /// Stored data failed verification. A metadata file that does not parse also names
    /// `details.fs_op` and `details.store_file`.
    StoreCorrupt => "E_STORE_CORRUPT", EXIT_ERROR, false;
    /// A malformed argument or request, including a command-line usage error.
    InvalidArg => "E_INVALID_ARG", EXIT_USAGE, false;
    /// The store path is not on a native Linux filesystem (for example WSL's `/mnt/*`).
    UnsupportedFs => "E_UNSUPPORTED_FS", EXIT_ERROR, false;
    /// The filesystem refused an operation: `details.fs_op` names it, and `details.store_file`
    /// the file of the store it was on, relative to the store's root.
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
    // Not raised in M1: token checks, `policy.toml` and the maintenance lease arrive later.
    /// The caller's token failed verification.
    Auth => "E_AUTH", EXIT_AUTH, false;
    /// A policy refused the operation for this identity.
    PolicyDenied => "E_POLICY_DENIED", EXIT_AUTH, false;
    /// Maintenance (gc or verify) holds the store's maintenance lease.
    MaintenanceBusy => "E_MAINTENANCE_BUSY", EXIT_RETRYABLE, true;
}

impl ErrorCode {
    /// The spec's `E_*` string, e.g. `"E_CAS_MISMATCH"`.
    pub fn as_str(&self) -> &'static str {
        self.row().0
    }

    /// The process exit code: 1 error, 2 usage, 4 retryable, 5 identity or policy, 6 not found
    /// (3, conflict, has no M1 code).
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
