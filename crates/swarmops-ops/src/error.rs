//! The error table (operations spec §0): every repository and store failure mapped to the
//! operations spec's code, with its `details` and, where there is an obvious next step, a
//! command as its hint. [`crate::LocalOps`]'s documentation gives the table. Every message is
//! one line: control characters in it are escaped.

use std::io;

use camino::Utf8Path;
use serde_json::{Map, Value};
use swarmops_api::{ErrorCode, OpError};
use swarmops_core::{ObjectId, RepoPath};
use swarmops_repo::{RepoError, StoreError};
use swarmops_store::Layout;

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
            let hint = rev_hint(&rev);
            error(ErrorCode::RevNotFound, message)
                .with_details(details([("rev", rev.into())]))
                .with_hint(hint)
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
            .with_hint(format!("swarmops --repo {at} info")),
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
        RepoError::DirPut { path: at } | RepoError::DuplicatePath { path: at } => {
            error(ErrorCode::InvalidArg, message).with_details(path(&at))
        }
        RepoError::PathNotFound { path: at } => {
            error(ErrorCode::PathNotFound, message).with_details(path(&at))
        }
        RepoError::RefName(name) => ref_name_invalid(name, message),
        RepoError::MissingObject { id, .. } => {
            corrupt(message, during, "swarmops verify").with_details(object(id))
        }
        RepoError::Codec(_) => corrupt(message, during, "swarmops verify"),
    }
}

/// The operations-spec error for store error `e`, whose message is `message`, raised while
/// `during`: [`repo_error`]'s table, the store's rows.
fn store_error(e: StoreError, message: String, during: During<'_>) -> OpError {
    match e {
        StoreError::Busy => error(ErrorCode::Busy, message)
            .with_hint("retry: another writer held the store's lock for the whole wait"),
        StoreError::Format(what) => match during {
            During::Open(at) => not_a_repo(at, what),
            During::Init(_) | During::Op => corrupt(message, during, "swarmops verify"),
        },
        StoreError::Io(io) if io.kind() == io::ErrorKind::InvalidData => {
            corrupt(message, during, "swarmops verify")
        }
        StoreError::Io(io) => match during {
            During::Open(at) if io.kind() == io::ErrorKind::NotADirectory => error(
                ErrorCode::NotARepo,
                format!("{at} is not a directory, so it holds no store"),
            ),
            _ => error(ErrorCode::Io, message),
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
        StoreError::PostCommit { .. } => error(ErrorCode::Internal, message),
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
        error.with_hint(format!("swarmops init {at}"))
    }
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
        Some((base, _)) => format!("swarmops log {base}"),
        None if rev.bytes().all(|b| b.is_ascii_hexdigit()) => "swarmops log".to_owned(),
        None => "swarmops ref list".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use swarmops_core::{ChangesetId, CodecError, ObjectKind};

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
            RepoError::DirPut { path: path() },
            op,
            ErrorCode::InvalidArg,
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
        maps(
            RepoError::RefName("x".into()),
            op,
            ErrorCode::RefNameInvalid,
        );
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
    }

    #[test]
    fn store_errors_map_to_their_codes() {
        let (op, at) = (During::Op, Utf8Path::new(AT));
        let store = RepoError::Store;
        let io = |kind: io::ErrorKind| store(StoreError::Io(io::Error::new(kind, "boom")));
        maps(store(StoreError::Busy), op, ErrorCode::Busy);
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
    fn rev_hints_point_at_the_refs_or_the_history() {
        assert_eq!(rev_hint("main"), "swarmops ref list");
        assert_eq!(rev_hint("release/1.2"), "swarmops ref list");
        assert_eq!(rev_hint("abcdef012345"), "swarmops log");
        assert_eq!(rev_hint("main~3"), "swarmops log main");
    }
}
