//! The error table (operations spec §0): every repository and store failure mapped to the
//! operations spec's code, with its `details` and, where there is an obvious next step, a
//! command as its hint. [`crate::LocalOps`]'s documentation gives the table. Every message is
//! one line: control characters in it are escaped.

use std::io;

use camino::Utf8Path;
use serde_json::{Map, Value};
use swarmops_api::{ErrorCode, OpError};
use swarmops_core::{ChangesetId, ObjectId, RepoPath};
use swarmops_repo::{RepoError, StoreError};
use swarmops_store::{IoFailure, Layout};

use crate::local::cas_mismatch;

/// What was being done when a repository error arose: a few errors mean something else while
/// a store is created or opened.
#[derive(Debug, Clone, Copy)]
pub(crate) enum During<'a> {
    /// `init` of a store at this path.
    Init(&'a Utf8Path),
    /// `open` of the store at this path.
    Open(&'a Utf8Path),
    /// An operation on an open store.
    Op,
}

/// [`repo_error`] for an operation on an open store.
pub(crate) fn op(e: RepoError) -> OpError {
    repo_error(e, During::Op)
}

/// The operations-spec error for repository error `e`, raised while `during`: the table in
/// [`crate::LocalOps`]'s documentation.
pub(crate) fn repo_error(e: RepoError, during: During<'_>) -> OpError {
    let message = e.to_string();
    let path = |path: &RepoPath| details([("path", path.as_str().into())]);
    match e {
        RepoError::Store(e) => store_error(e, message, during),
        RepoError::NotFound(rev) => {
            let next = rev_hint(&rev);
            error(ErrorCode::RevNotFound, message)
                .with_details(details([("rev", rev.into())]))
                .with_hint(next)
        }
        RepoError::Ambiguous(rev) => error(ErrorCode::AmbiguousId, message)
            .with_details(details([("rev", rev.into())]))
            .with_hint("give more digits of the id; swarmops log shows whole ids"),
        RepoError::BadRev(rev) => error(ErrorCode::InvalidArg, message)
            .with_details(details([("rev", rev.into())]))
            .with_hint(
                "a revision is main, release/<name>, a changeset id or 12 or more of its first \
                 hex digits, then optionally ~N for the N-th first parent",
            ),
        RepoError::Exists => match during {
            During::Init(at) => error(
                ErrorCode::ExistsAlready,
                format!("a store exists at {at} already"),
            )
            .with_hint(hint(format!(
                "swarmops --repo {} info",
                shell_word(at.as_str())
            ))),
            During::Open(_) | During::Op => error(ErrorCode::ExistsAlready, message),
        },
        RepoError::UnknownParent(id) => error(ErrorCode::UnknownParent, message)
            .with_details(details([("parent", id.to_string().into())]))
            .with_hint("swarmops log"),
        RepoError::EmptyChanges => error(ErrorCode::EmptyCommit, message)
            .with_hint("pass --allow-empty to record a changeset that changes nothing"),
        RepoError::TypeConflict { path: at } => {
            error(ErrorCode::TypeConflict, message).with_details(path(&at))
        }
        RepoError::DuplicatePath { path: at } => {
            error(ErrorCode::InvalidArg, message).with_details(path(&at))
        }
        RepoError::PathNotFound { path: at } => {
            error(ErrorCode::PathNotFound, message).with_details(path(&at))
        }
        // A mistake in the request, not damage (Decision 12): the change list names content the
        // store does not hold. `commit` adds the change's index.
        RepoError::MissingBlob { path: at, id } => error(
            ErrorCode::InvalidArg,
            format!("the store holds no blob {id}"),
        )
        .with_details(details([
            ("path", at.as_str().into()),
            ("blob", id.to_string().into()),
        ]))
        .with_hint("swarmops blob put <file>"),
        RepoError::RefName(name) => ref_name_invalid(name, message),
        RepoError::RefMoved { name, actual, .. } => ref_moved(&name, actual),
        RepoError::OpNotFound { op: number, last } => {
            let last = last.map_or(Value::Null, Value::from);
            error(ErrorCode::OpNotFound, message)
                .with_details(details([("op", number.into()), ("last", last)]))
                .with_hint("swarmops op log")
        }
        RepoError::NothingToUndo(number) => error(ErrorCode::NothingToUndo, message)
            .with_details(details([("op", number.into())]))
            .with_hint("swarmops op log"),
        RepoError::UndoBlocked {
            op: number,
            name,
            expected,
            actual,
            ..
        } => undo_blocked(number, &name, expected, actual),
        RepoError::MissingObject { id, .. } => {
            corrupt(message, during, "swarmops verify").with_details(object(id))
        }
        RepoError::Codec(_) => corrupt(message, during, "swarmops verify"),
        // A derivation that fails on immutable inputs fails again on every call: a bug.
        RepoError::Derived { .. } => error(ErrorCode::Internal, message),
    }
}

/// The operations-spec error for store error `e`, whose message is `message`, raised while
/// `during`: [`repo_error`]'s table, the store's rows.
fn store_error(e: StoreError, message: String, during: During<'_>) -> OpError {
    match e {
        StoreError::Busy => error(ErrorCode::Busy, message)
            .with_hint("retry: another writer held the store's lock for the whole wait"),
        StoreError::SnapshotContended { .. } => error(ErrorCode::Busy, message)
            .with_hint("retry: commits landed during every attempt to take a read snapshot"),
        StoreError::Format(what) => match during {
            During::Open(at) => not_a_repo(at, what),
            During::Init(_) | During::Op => corrupt(message, during, "swarmops verify"),
        },
        StoreError::Io(io) if io.kind() == io::ErrorKind::InvalidData => {
            corrupt(message, during, "swarmops verify").with_details(located(&io))
        }
        StoreError::Io(io) => match during {
            During::Open(at) if io.kind() == io::ErrorKind::NotADirectory => error(
                ErrorCode::NotARepo,
                format!("{at} is not a directory, so it holds no store"),
            ),
            _ => error(ErrorCode::Io, message).with_details(located(&io)),
        },
        StoreError::UnsupportedFs(at) => error(ErrorCode::UnsupportedFs, message)
            .with_details(details([("path", at.as_str().into())]))
            .with_hint(
                "put the store on a native Linux filesystem, such as under \
                 ~/.local/state/swarmops/repos/; /mnt/* holds Windows drives under WSL",
            ),
        StoreError::Corrupt { id } | StoreError::BadHeader { id } => {
            corrupt(message, during, "swarmops verify --deep").with_details(object(id))
        }
        StoreError::Codec(_)
        | StoreError::JournalCorrupt { .. }
        | StoreError::IndexCorrupt { .. }
        | StoreError::IndexMismatch { .. }
        | StoreError::MissingParent(_) => corrupt(message, during, "swarmops verify"),
        StoreError::RefName(name) => ref_name_invalid(name, message),
        StoreError::UnknownRefTarget(id) => repo_error(RepoError::NotFound(id.to_string()), during),
        StoreError::DuplicateParent(id) => error(ErrorCode::InvalidArg, message)
            .with_details(details([("parent", id.to_string().into())])),
        StoreError::Limit { what, max, got } => {
            error(ErrorCode::InvalidArg, message).with_details(details([
                ("store_limit", what.as_str().into()),
                ("max", max.into()),
                ("got", got.into()),
            ]))
        }
        StoreError::BadPrefix(prefix) => {
            error(ErrorCode::InvalidArg, message).with_details(details([("prefix", prefix.into())]))
        }
        StoreError::PostCommit { .. } | StoreError::BadCall(_) => {
            error(ErrorCode::Internal, message)
        }
        StoreError::NotCommitted { source } => {
            let mut details = located(&source);
            details.insert("committed".to_owned(), false.into());
            error(ErrorCode::IoTransient, message)
                .with_details(details)
                .with_hint("run the same command again: nothing of this one was committed")
        }
    }
}

/// What a filesystem failure names: the file operation that failed (`fs_op`), and, where it was
/// on a file of the store, that file relative to the store's root (`store_file`), never a host
/// path (operations spec §0).
fn located(io: &IoFailure) -> Map<String, Value> {
    let mut located = details([("fs_op", io.op.as_str().into())]);
    if let Some(file) = &io.file {
        located.insert("store_file".to_owned(), file.as_str().into());
    }
    located
}

/// The error of an `init` that made the store at `at`, whole and durable, but then failed with
/// `e` to read it back for its report: `e`, saying that the store is there
/// (`details.created: true`, and a hint to read it), so that a client that retries does not
/// find out only from `E_EXISTS`.
pub(crate) fn created_but_unread(at: &Utf8Path, e: OpError) -> OpError {
    let mut details = e.details;
    details.insert("created".to_owned(), true.into());
    OpError {
        message: one_line(format!(
            "the store at {at} is made, but reading it back failed: {}",
            e.message
        )),
        details,
        hint: Some(hint(format!(
            "swarmops --repo {} info",
            shell_word(at.as_str())
        ))),
        ..e
    }
}

/// An error of `code` with `message`, its control characters escaped so that it is one line.
pub(crate) fn error(code: ErrorCode, message: impl Into<String>) -> OpError {
    OpError::new(code, one_line(message.into()))
}

/// `text` with each control character (a line break, a tab, a NUL…) escaped, so that it is
/// one line.
pub(crate) fn one_line(text: String) -> String {
    if !text.contains(char::is_control) {
        return text;
    }
    text.chars()
        .flat_map(|c| {
            let escaped: Vec<char> = if c.is_control() {
                c.escape_default().collect()
            } else {
                vec![c]
            };
            escaped
        })
        .collect()
}

pub(crate) fn details<const N: usize>(entries: [(&str, Value); N]) -> Map<String, Value> {
    entries
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect()
}

fn object(id: ObjectId) -> Map<String, Value> {
    details([("object", id.to_hex().into())])
}

/// Stored data that fails its checks; once the store is open, `hint` (a verify command) finds
/// what else is damaged.
fn corrupt(message: String, during: During<'_>, hint: &str) -> OpError {
    let error = error(ErrorCode::StoreCorrupt, message);
    match during {
        During::Op => error.with_hint(hint),
        During::Init(_) | During::Open(_) => error,
    }
}

/// No store at `at`, as `what` says. Without a `FORMAT` file nothing of a store is there, and
/// init makes one; with one, the store is damaged or of a layout this build does not read,
/// which init cannot mend.
fn not_a_repo(at: &Utf8Path, what: String) -> OpError {
    let error = error(ErrorCode::NotARepo, what);
    if Layout::new(at).format_file().exists() {
        error
    } else {
        error.with_hint(hint(format!("swarmops init {}", shell_word(at.as_str()))))
    }
}

/// A commit with `advance` that recorded nothing because ref `name` named `actual` (`None`: it
/// did not exist), not the commit's parent: `E_CAS_MISMATCH` `{name, actual, committed: false}`,
/// whose message says to commit the change again on top of `actual`, and whose hint only reads,
/// as for a ref move ([`crate::local::cas_mismatch`]).
fn ref_moved(name: &str, actual: Option<ChangesetId>) -> OpError {
    let message = match actual {
        Some(actual) => format!(
            "ref {name} moved to {actual}, so this commit recorded nothing: commit your change on \
             top of it (--parent {actual} --advance {name}), re-applying your edits to the \
             content at {actual} rather than re-sending your old files"
        ),
        None => format!(
            "ref {name} does not exist, so it does not name this commit's parent, and this commit \
             recorded nothing"
        ),
    };
    let mut error = cas_mismatch(name, actual, message);
    error.details.insert("committed".to_owned(), false.into());
    error
}

/// An `undo` of operation `op` that found ref `name` at `actual` (`None`: it does not exist), not
/// at `expected`, what the operation set it to, so that the undo wrote nothing: `E_CAS_MISMATCH`
/// `{name, actual, op}`, whose message says to undo the operations that moved the ref since
/// first, or to land a change that reverses this one, and whose hint only reads: the operation
/// log.
fn undo_blocked(
    op: u64,
    name: &str,
    expected: Option<ChangesetId>,
    actual: Option<ChangesetId>,
) -> OpError {
    let shown = |target: Option<ChangesetId>| {
        target.map_or_else(|| "nothing".to_owned(), |id| id.to_string())
    };
    let message = format!(
        "ref {name} has moved since operation {op} set it to {}: it names {} now, so this undo \
         wrote nothing; undo the operations that moved it since, newest first, then this one \
         again, or land a change that reverses operation {op}",
        shown(expected),
        shown(actual)
    );
    let mut error = cas_mismatch(name, actual, message).with_hint("swarmops op log");
    error.details.insert("op".to_owned(), op.into());
    error
}

fn ref_name_invalid(name: String, message: String) -> OpError {
    error(ErrorCode::RefNameInvalid, message)
        .with_details(details([("name", name.into())]))
        .with_hint(
            "a ref name is main or release/<name>, the name of letters, digits, '.', '_' and \
             '-', not starting with '.'",
        )
}

/// The command that shows what `rev` could have named: the history before a `~N` walk, the
/// history for an id, the refs for a ref name.
fn rev_hint(rev: &str) -> String {
    match rev.split_once('~') {
        Some((base, _)) => hint(format!("swarmops log {}", shell_word(base))),
        None if rev.bytes().all(|b| b.is_ascii_hexdigit()) => "swarmops log".to_owned(),
        None => "swarmops ref list".to_owned(),
    }
}

/// A hint, made one line as messages are.
pub(crate) fn hint(text: impl Into<String>) -> String {
    one_line(text.into())
}

/// `word` as one shell word in a hint: as it is when it is made of `[A-Za-z0-9_./:@-]` only,
/// else in single quotes, each `'` in it written `'\''`. A hint is made one line afterwards, its
/// control characters escaped.
pub fn shell_word(word: &str) -> String {
    let plain = !word.is_empty()
        && word
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_./:@-".contains(&b));
    if plain {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use swarmops_core::{ChangesetId, CodecError, ObjectKind};
    use swarmops_store::{FileOp, FsMethod, IoFailure, LimitKind};

    use super::*;

    const AT: &str = "/nonexistent/swarmops-test/store";

    /// `e`, raised while `during`, maps to `code`: its string, exit code and retry advice.
    #[track_caller]
    fn maps(e: RepoError, during: During<'_>, code: ErrorCode) {
        let shown = format!("{e:?} during {during:?}");
        let error = repo_error(e, during);
        assert_eq!(error.code, code.as_str(), "{shown}");
        assert_eq!(
            (error.exit, error.retryable),
            (code.exit(), code.retryable()),
            "{shown}"
        );
        assert!(!error.message.is_empty(), "{shown}");
    }

    fn id() -> ChangesetId {
        ChangesetId(ObjectId([7; 32]))
    }

    fn path() -> RepoPath {
        RepoPath::new("a/b").unwrap()
    }

    #[test]
    fn repository_errors_map_to_their_codes() {
        let op = During::Op;
        maps(
            RepoError::NotFound("main".into()),
            op,
            ErrorCode::RevNotFound,
        );
        maps(
            RepoError::Ambiguous("ababab".into()),
            op,
            ErrorCode::AmbiguousId,
        );
        maps(RepoError::BadRev("HEAD".into()), op, ErrorCode::InvalidArg);
        let init = During::Init(Utf8Path::new(AT));
        maps(RepoError::Exists, init, ErrorCode::ExistsAlready);
        maps(RepoError::UnknownParent(id()), op, ErrorCode::UnknownParent);
        maps(RepoError::EmptyChanges, op, ErrorCode::EmptyCommit);
        maps(
            RepoError::TypeConflict { path: path() },
            op,
            ErrorCode::TypeConflict,
        );
        maps(
            RepoError::DuplicatePath { path: path() },
            op,
            ErrorCode::InvalidArg,
        );
        maps(
            RepoError::PathNotFound { path: path() },
            op,
            ErrorCode::PathNotFound,
        );
        let missing_blob = RepoError::MissingBlob {
            path: path(),
            id: swarmops_core::BlobId(id().0),
        };
        maps(missing_blob, op, ErrorCode::InvalidArg);
        maps(
            RepoError::RefName("x".into()),
            op,
            ErrorCode::RefNameInvalid,
        );
        let moved = RepoError::RefMoved {
            name: "main".into(),
            actual: Some(id()),
            recovered: Box::default(),
        };
        maps(moved, op, ErrorCode::CasMismatch);
        let not_found = RepoError::OpNotFound {
            op: 7,
            last: Some(3),
        };
        maps(not_found, op, ErrorCode::OpNotFound);
        maps(RepoError::NothingToUndo(2), op, ErrorCode::NothingToUndo);
        let blocked = RepoError::UndoBlocked {
            op: 2,
            name: "main".into(),
            expected: Some(id()),
            actual: None,
            recovered: Box::default(),
        };
        maps(blocked, op, ErrorCode::CasMismatch);
        let missing = RepoError::MissingObject {
            id: id().0,
            kind: ObjectKind::Blob,
        };
        maps(missing, op, ErrorCode::StoreCorrupt);
        maps(
            RepoError::Codec(CodecError::Magic),
            op,
            ErrorCode::StoreCorrupt,
        );
        let derived = RepoError::Derived {
            name: "depth".into(),
            why: "a cycle".into(),
        };
        maps(derived, op, ErrorCode::Internal);
    }

    /// A commit with `advance` whose ref moved first recorded nothing: `E_CAS_MISMATCH` (exit 4,
    /// retryable) with what the ref names and `committed: false`, a message that says to commit
    /// again on top of it, and a hint that only reads; `actual` is null, and the hint the refs,
    /// for a ref that is gone.
    #[test]
    fn a_commit_whose_ref_moved_is_a_cas_mismatch_that_committed_nothing() {
        let moved = |actual| {
            let e = RepoError::RefMoved {
                name: "main".into(),
                actual,
                recovered: Box::default(),
            };
            repo_error(e, During::Op)
        };
        let error = moved(Some(id()));
        assert_eq!(error.code, ErrorCode::CasMismatch.as_str());
        assert_eq!((error.exit, error.retryable), (4, true));
        let actual = Value::from(id().to_string());
        let want = details([
            ("name", "main".into()),
            ("actual", actual),
            ("committed", false.into()),
        ]);
        assert_eq!(error.details, want);
        let advice = format!("--parent {} --advance main", id());
        assert!(error.message.contains(&advice), "{error:?}");
        assert_eq!(error.hint, Some(format!("swarmops show {}", id())));

        let gone = moved(None);
        let want = details([
            ("name", "main".into()),
            ("actual", Value::Null),
            ("committed", false.into()),
        ]);
        assert_eq!(gone.details, want);
        assert_eq!(gone.hint.as_deref(), Some("swarmops ref list"));
    }

    /// An `undo` that found a ref moved since its operation wrote nothing: `E_CAS_MISMATCH` (exit
    /// 4) with the ref, what it names and the operation, a message that says to undo the later
    /// operations first or land a change that reverses this one, and the operation log as its
    /// read-only hint. An undo of an operation past the last, or of one that moved no ref, names
    /// the operation too.
    #[test]
    fn an_undo_whose_ref_moved_since_is_a_cas_mismatch_that_names_the_operation() {
        let blocked = repo_error(
            RepoError::UndoBlocked {
                op: 4,
                name: "release/r".into(),
                expected: Some(id()),
                actual: None,
                recovered: Box::default(),
            },
            During::Op,
        );
        assert_eq!(blocked.code, ErrorCode::CasMismatch.as_str());
        assert_eq!((blocked.exit, blocked.retryable), (4, true));
        let want = details([
            ("name", "release/r".into()),
            ("actual", Value::Null),
            ("op", 4.into()),
        ]);
        assert_eq!(blocked.details, want);
        for advice in [
            format!("since operation 4 set it to {}", id()),
            "it names nothing now".to_owned(),
            "undo the operations that moved it since, newest first, then this one again".to_owned(),
            "land a change that reverses operation 4".to_owned(),
        ] {
            assert!(blocked.message.contains(&advice), "{advice}: {blocked:?}");
        }
        assert_eq!(blocked.hint.as_deref(), Some("swarmops op log"));

        let not_found = repo_error(RepoError::OpNotFound { op: 9, last: None }, During::Op);
        assert_eq!(
            (not_found.code.as_str(), not_found.exit),
            ("E_OP_NOT_FOUND", 6)
        );
        assert_eq!(
            not_found.details,
            details([("op", 9.into()), ("last", Value::Null)])
        );
        assert_eq!(not_found.hint.as_deref(), Some("swarmops op log"));
        let nothing = repo_error(RepoError::NothingToUndo(3), During::Op);
        assert_eq!(
            (nothing.code.as_str(), nothing.exit),
            ("E_NOTHING_TO_UNDO", 1)
        );
        assert_eq!(nothing.details, details([("op", 3.into())]));
    }

    #[test]
    fn store_errors_map_to_their_codes() {
        let (op, at) = (During::Op, Utf8Path::new(AT));
        let store = RepoError::Store;
        let io = |kind: io::ErrorKind| {
            let failure = IoFailure::new(FileOp::Open, None, io::Error::new(kind, "boom"));
            store(StoreError::Io(failure))
        };
        maps(store(StoreError::Busy), op, ErrorCode::Busy);
        let contended = StoreError::SnapshotContended { rounds: 32 };
        maps(store(contended), op, ErrorCode::Busy);
        let format = || store(StoreError::Format("no FORMAT file".into()));
        maps(format(), During::Open(at), ErrorCode::NotARepo);
        maps(format(), During::Init(at), ErrorCode::StoreCorrupt);
        maps(format(), op, ErrorCode::StoreCorrupt);
        let damage = [
            StoreError::Corrupt { id: id().0 },
            StoreError::BadHeader { id: id().0 },
            StoreError::Codec(CodecError::Truncated),
            StoreError::JournalCorrupt { offset: 9 },
            StoreError::IndexCorrupt { what: "x" },
            StoreError::IndexMismatch { seq: 3 },
            StoreError::MissingParent(id()),
        ];
        for e in damage {
            maps(store(e), op, ErrorCode::StoreCorrupt);
        }
        maps(io(io::ErrorKind::PermissionDenied), op, ErrorCode::Io);
        maps(io(io::ErrorKind::InvalidData), op, ErrorCode::StoreCorrupt);
        maps(
            io(io::ErrorKind::NotADirectory),
            During::Open(at),
            ErrorCode::NotARepo,
        );
        maps(io(io::ErrorKind::NotADirectory), op, ErrorCode::Io);
        let mnt = store(StoreError::UnsupportedFs("/mnt/c/store".into()));
        maps(mnt, During::Init(at), ErrorCode::UnsupportedFs);
        let mnt = store(StoreError::UnsupportedFs("/mnt/c/store".into()));
        maps(mnt, During::Open(at), ErrorCode::UnsupportedFs);
        maps(
            store(StoreError::RefName("x".into())),
            op,
            ErrorCode::RefNameInvalid,
        );
        maps(
            store(StoreError::UnknownRefTarget(id())),
            op,
            ErrorCode::RevNotFound,
        );
        maps(
            store(StoreError::DuplicateParent(id())),
            op,
            ErrorCode::InvalidArg,
        );
        let post = StoreError::PostCommit {
            frame_seq: 1,
            source: Box::new(StoreError::Busy),
        };
        maps(store(post), op, ErrorCode::Internal);
        let not_committed = StoreError::NotCommitted {
            source: IoFailure::new(
                FileOp::Fs(FsMethod::AppendDurable),
                None,
                io::Error::other("boom"),
            ),
        };
        maps(store(not_committed), op, ErrorCode::IoTransient);
    }

    /// The store's refusals of a request past a limit of its layout, and of an id prefix that
    /// is not one, are mistakes in the request: `E_INVALID_ARG`, saying which limit and by how
    /// much, or which prefix. A call the store does not take is a bug: `E_INTERNAL`. An `Io`
    /// error stays `E_IO` whatever its kind: an OS `EINVAL` is `InvalidInput`, and no arm takes
    /// that kind for a refusal.
    #[test]
    fn store_limits_are_invalid_arguments_and_os_einval_stays_e_io() {
        let op = During::Op;
        let store = RepoError::Store;
        let limit = || StoreError::Limit {
            what: LimitKind::Parents,
            max: 255,
            got: 256,
        };
        maps(store(limit()), op, ErrorCode::InvalidArg);
        let error = repo_error(store(limit()), op);
        assert_eq!(
            (error.code.as_str(), error.exit, error.retryable),
            ("E_INVALID_ARG", 2, false)
        );
        assert_eq!(
            Value::Object(error.details),
            serde_json::json!({"store_limit": "parents", "max": 255, "got": 256})
        );
        let prefix = || StoreError::BadPrefix("xyz".into());
        maps(store(prefix()), op, ErrorCode::InvalidArg);
        let error = repo_error(store(prefix()), op);
        assert_eq!(
            Value::Object(error.details),
            serde_json::json!({"prefix": "xyz"})
        );
        let call = StoreError::BadCall("rec 9 is past the index's 2 records".into());
        maps(store(call), op, ErrorCode::Internal);
        let einval = io::Error::from_raw_os_error(22);
        assert_eq!(einval.kind(), io::ErrorKind::InvalidInput);
        let einval = IoFailure::new(FileOp::Fs(FsMethod::Truncate), None, einval);
        let error = repo_error(store(StoreError::Io(einval)), op);
        assert_eq!((error.code.as_str(), error.exit), ("E_IO", 1));
    }

    /// A journal append that failed and was cut back off the journal committed nothing:
    /// `E_IO_TRANSIENT`, retryable, `committed: false`, with a hint to run it again.
    #[test]
    fn a_write_cut_back_off_the_journal_is_retryable_and_not_committed() {
        let not_committed = StoreError::NotCommitted {
            source: IoFailure::new(
                FileOp::Fs(FsMethod::AppendDurable),
                Some("meta/journal.log".into()),
                io::Error::other("the fdatasync failed"),
            ),
        };
        let error = repo_error(RepoError::Store(not_committed), During::Op);
        assert_eq!(
            (error.code.as_str(), error.exit, error.retryable),
            ("E_IO_TRANSIENT", 4, true)
        );
        assert_eq!(error.details["committed"], false);
        assert!(
            error.message.contains("did not commit"),
            "{}",
            error.message
        );
        let hint = error.hint.unwrap_or_default();
        assert!(hint.contains("same command"), "{hint}");
    }

    /// A filesystem failure names the file operation that failed (`details.fs_op`) and, where it
    /// was on a file of the store, that file relative to the store's root
    /// (`details.store_file`): on `E_IO`, which has no hint (no command fixes a file's owner); on
    /// the `E_STORE_CORRUPT` of a metadata file that does not parse; and on the `E_IO_TRANSIENT`
    /// of a journal append the store cut back, beside `committed`. A failure on no file of the
    /// store has no `store_file`; an open of a store that is no directory is still
    /// `E_NOT_A_REPO`.
    #[test]
    fn io_errors_name_the_operation_and_the_store_file() {
        let (op, at) = (During::Op, Utf8Path::new(AT));
        let failure = |what: FileOp, file: Option<&str>, source: io::Error| {
            IoFailure::new(what, file.map(Into::into), source)
        };
        let io = |failure| repo_error(RepoError::Store(StoreError::Io(failure)), op);
        let eio = || io::Error::from_raw_os_error(5);

        let sync = failure(
            FileOp::Fs(FsMethod::SyncData),
            Some("meta/journal.off"),
            eio(),
        );
        let error = io(sync);
        assert_eq!((error.code.as_str(), error.exit), ("E_IO", 1));
        assert_eq!(
            error.message,
            "i/o error: sync_data meta/journal.off: Input/output error (os error 5)"
        );
        assert_eq!(error.hint, None);
        assert_eq!(
            Value::Object(error.details),
            serde_json::json!({"fs_op": "sync_data", "store_file": "meta/journal.off"})
        );

        let malformed = io::Error::new(io::ErrorKind::InvalidData, "holds 3 bytes, not a u64le");
        let ckpt = failure(
            FileOp::Fs(FsMethod::Read),
            Some("meta/journal.ckpt"),
            malformed,
        );
        let error = io(ckpt);
        assert_eq!(error.code.as_str(), "E_STORE_CORRUPT");
        assert_eq!(error.hint.as_deref(), Some("swarmops verify"));
        assert_eq!(
            Value::Object(error.details),
            serde_json::json!({"fs_op": "read", "store_file": "meta/journal.ckpt"})
        );

        let lock = failure(FileOp::Lock, None, io::Error::other("ended without it"));
        let error = io(lock);
        assert_eq!(error.code.as_str(), "E_IO");
        assert_eq!(
            Value::Object(error.details),
            serde_json::json!({"fs_op": "lock"})
        );

        let append = failure(
            FileOp::Fs(FsMethod::AppendDurable),
            Some("meta/journal.log"),
            eio(),
        );
        let not_committed = StoreError::NotCommitted { source: append };
        let error = repo_error(RepoError::Store(not_committed), op);
        assert_eq!((error.code.as_str(), error.exit), ("E_IO_TRANSIENT", 4));
        assert_eq!(
            Value::Object(error.details),
            serde_json::json!({
                "committed": false,
                "fs_op": "append_durable",
                "store_file": "meta/journal.log"
            })
        );

        let not_a_dir = io::Error::from(io::ErrorKind::NotADirectory);
        let file = failure(FileOp::Fs(FsMethod::Read), Some("FORMAT"), not_a_dir);
        let error = repo_error(RepoError::Store(StoreError::Io(file)), During::Open(at));
        assert_eq!(error.code.as_str(), "E_NOT_A_REPO");
    }

    /// A snapshot whose every round met a new frame took no lock and wrote nothing: `E_BUSY`,
    /// retryable, no details, and a hint that names the snapshot, not the lock wait.
    #[test]
    fn a_contended_snapshot_is_busy_with_a_read_hint() {
        let contended = StoreError::SnapshotContended { rounds: 32 };
        let error = repo_error(RepoError::Store(contended), During::Op);
        assert_eq!(
            (error.code.as_str(), error.exit, error.retryable),
            ("E_BUSY", 4, true)
        );
        assert!(error.details.is_empty(), "{:?}", error.details);
        assert!(error.message.contains("32"), "{}", error.message);
        let hint = error.hint.unwrap_or_default();
        assert!(hint.contains("snapshot"), "{hint}");
        assert!(!hint.contains("lock"), "{hint}");
    }

    /// An `init` that made the store whole but failed to read it back for its report says
    /// so: the read's code, `details.created: true`, and a hint to read the store.
    #[test]
    fn an_init_that_made_the_store_says_so_when_reading_it_back_fails() {
        let read = error(ErrorCode::Io, "i/o error: Input/output error (os error 5)");
        let error = created_but_unread(Utf8Path::new(AT), read);
        assert_eq!((error.code.as_str(), error.exit), ("E_IO", 1));
        assert_eq!(error.details["created"], true);
        assert!(
            error
                .message
                .starts_with(&format!("the store at {AT} is made")),
            "{}",
            error.message
        );
        let hint = format!("swarmops --repo {AT} info");
        assert_eq!(error.hint.as_deref(), Some(hint.as_str()));
    }

    #[test]
    fn corruption_hints_need_an_open_store() {
        let hint = |e: StoreError, during| repo_error(RepoError::Store(e), during).hint;
        let corrupt = || StoreError::Corrupt { id: id().0 };
        assert_eq!(
            hint(corrupt(), During::Op).as_deref(),
            Some("swarmops verify --deep")
        );
        assert_eq!(hint(corrupt(), During::Open(Utf8Path::new(AT))), None);
        let format = StoreError::Format("bad meta".into());
        assert_eq!(hint(format, During::Init(Utf8Path::new(AT))), None);
        let missing = RepoError::MissingObject {
            id: id().0,
            kind: ObjectKind::Tree,
        };
        let error = repo_error(missing, During::Op);
        assert_eq!(error.hint.as_deref(), Some("swarmops verify"));
        assert_eq!(error.details["object"], id().0.to_hex());
    }

    #[test]
    fn messages_are_one_line() {
        assert_eq!(one_line("plain".to_owned()), "plain");
        assert_eq!(one_line("a\nb\tc\0".to_owned()), "a\\nb\\tc\\u{0}");
        let e = RepoError::TypeConflict {
            path: RepoPath::new("odd\nname").unwrap(),
        };
        let error = repo_error(e, During::Op);
        assert_eq!(
            error.message,
            "odd\\nname: a file and a directory conflict there"
        );
        assert_eq!(
            error.details["path"], "odd\nname",
            "details keep the path as it is"
        );
    }

    #[test]
    fn shell_words_are_quoted_unless_plain() {
        assert_eq!(shell_word("main:src/lib.rs"), "main:src/lib.rs");
        assert_eq!(shell_word("a@b-c_d.e"), "a@b-c_d.e");
        assert_eq!(shell_word("my dir"), "'my dir'");
        assert_eq!(shell_word("main~1:src"), "'main~1:src'");
        assert_eq!(shell_word("it's"), r"'it'\''s'");
        assert_eq!(shell_word(""), "''");
        assert_eq!(hint(format!("ls {}", shell_word("a\nb"))), r"ls 'a\nb'");
    }

    #[test]
    fn rev_hints_point_at_the_refs_or_the_history() {
        assert_eq!(rev_hint("main"), "swarmops ref list");
        assert_eq!(rev_hint("release/1.2"), "swarmops ref list");
        assert_eq!(rev_hint("abcdef012345"), "swarmops log");
        assert_eq!(rev_hint("main~3"), "swarmops log main");
    }
}
