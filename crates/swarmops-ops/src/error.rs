//! The error table (operations spec §0): every repository and store failure mapped to the
//! operations spec's code, with its `details` and, where there is an obvious next step, a
//! command as its hint. [`crate::LocalOps`]'s documentation gives the table. Every message is
//! one line: control characters in it are escaped.

use std::io;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use camino::Utf8Path;
use serde_json::{Map, Value};
use swarmops_api::{ErrorCode, OpError};
use swarmops_core::{ChangesetId, ObjectId, RepoPath};
use swarmops_git::{ConvertError, GitOid, ImportError, SourceError, Unsupported};
use swarmops_repo::{RepoError, StoreError};
use swarmops_store::{IoFailure, Layout, LimitKind, MAX_OBJECT_LEN};

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
        RepoError::ReservedMetadataKey { key } => {
            error(ErrorCode::InvalidArg, message).with_details(details([("key", key.into())]))
        }
        RepoError::PathNotFound { path: at } => {
            error(ErrorCode::PathNotFound, message).with_details(path(&at))
        }
        RepoError::RefName(name) => ref_name_invalid(name, message),
        RepoError::RefMoved { name, actual, .. } => ref_moved(&name, actual),
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
        | StoreError::MissingParent(_)
        | StoreError::ExternalIdCorrupt { .. } => corrupt(message, during, "swarmops verify"),
        StoreError::RefName(name) => ref_name_invalid(name, message),
        StoreError::UnknownRefTarget(id) => repo_error(RepoError::NotFound(id.to_string()), during),
        StoreError::DuplicateParent(id) => error(ErrorCode::InvalidArg, message)
            .with_details(details([("parent", id.to_string().into())])),
        StoreError::Limit { what, max, got } => {
            // An object past the most an object holds is content too large; every other limit
            // is a request that breaks the layout's rules.
            let code = match what {
                LimitKind::ObjectBytes => ErrorCode::TooLarge,
                LimitKind::Parents
                | LimitKind::ActorBytes
                | LimitKind::RefNameBytes
                | LimitKind::Records
                | LimitKind::RepoNameBytes => ErrorCode::InvalidArg,
            };
            error(code, message).with_details(details([
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
            match lasting_refusal(&source) {
                Some(fix) => error(ErrorCode::Io, message)
                    .with_details(details)
                    .with_hint(format!(
                        "{fix}, then run the same command again: nothing of this one was \
                         committed"
                    )),
                None => error(ErrorCode::IoTransient, message)
                    .with_details(details)
                    .with_hint("run the same command again: nothing of this one was committed"),
            }
        }
    }
}

/// What to fix first when the filesystem refused a journal append in a way that running the
/// same command again meets again: for want of permission (`EACCES`, `EPERM`), or as a
/// read-only filesystem (`EROFS`). The store's `NotCommitted` is then the non-retryable
/// `E_IO`, `committed: false` all the same (ruling M16-R8: retryable means that running the
/// operation again can succeed). `None` for any other failure, which a run again can get past.
fn lasting_refusal(failure: &IoFailure) -> Option<&'static str> {
    match failure.kind() {
        io::ErrorKind::PermissionDenied => Some("make the store's files writable for this user"),
        io::ErrorKind::ReadOnlyFilesystem => Some("remount the store's filesystem read-write"),
        _ => None,
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

/// The hint of a `git import` that stopped after the frames it committed.
const RESUME_HINT: &str =
    "run the same git import again: it continues after the last committed frame";

/// The operations-spec error for a `git import` of `git_ref` from the repository at `git_path`
/// that failed with `e`: the table in [`crate::LocalOps`]'s documentation. The source and its
/// objects are named as the caller gave them and by their git ids; an import that stopped after
/// it committed frames says how far it got (`details.imported`, `details.last_commit`).
pub(crate) fn git_import_error(e: ImportError, git_path: &str, git_ref: &str) -> OpError {
    let message = e.to_string();
    match e {
        ImportError::Source(e) => source_error(e, message, git_path, git_ref),
        ImportError::Convert(e) => convert_error(e, message, git_path, git_ref),
        ImportError::RefName(name) => ref_name_invalid(name, message),
        ImportError::Diverged {
            target_ref,
            target_head,
            base,
            git_head,
        } => {
            let mut found = details([
                ("target_ref", target_ref.as_str().into()),
                ("target_head", target_head.to_string().into()),
                ("git_head", git_head.to_string().into()),
            ]);
            if let Some(base) = base {
                found.insert("base".to_owned(), base.to_string().into());
            }
            error(ErrorCode::GitImportDiverged, message)
                .with_details(found)
                .with_hint(hint(format!("swarmops log {}", shell_word(&target_ref))))
        }
        ImportError::CasMismatch { target_ref, actual } => {
            let message = format!(
                "{message}; every commit is recorded, and the same git import run again moves the \
                 ref if what it names leads to the tip, or fails with E_GIT_IMPORT_DIVERGED"
            );
            cas_mismatch(&target_ref, actual, one_line(message))
        }
        ImportError::Store {
            source,
            imported,
            last,
        } => {
            let stopped = OpError {
                message: one_line(message),
                ..op(source)
            };
            progress(stopped, imported, last)
        }
        ImportError::PostCommit {
            warnings,
            imported,
            last,
        } => {
            // A step after the frame's commit point failed: an I/O failure that the store did not
            // cut back, so the frame is committed.
            let found = details([("committed", true.into()), ("warnings", warnings.into())]);
            let stopped = error(ErrorCode::Io, message).with_details(found);
            progress(stopped, imported, last)
        }
    }
}

/// [`git_import_error`] for the source's failure `e`, whose message is `message`.
fn source_error(e: SourceError, message: String, git_path: &str, git_ref: &str) -> OpError {
    let refs = || hint(format!("git -C {} for-each-ref", shell_word(git_path)));
    match e {
        SourceError::NotARepo { .. } => error(ErrorCode::GitNotARepo, message)
            .with_details(details([("git_path", git_path.into())])),
        SourceError::RefNotFound { .. } => error(ErrorCode::GitRefNotFound, message)
            .with_details(details([("git_ref", git_ref.into())]))
            .with_hint(refs()),
        SourceError::NotACommit { kind, .. } => error(ErrorCode::GitRefNotFound, message)
            .with_details(details([
                ("git_ref", git_ref.into()),
                ("kind", kind.as_str().into()),
            ]))
            .with_hint(refs()),
        SourceError::Unsupported(why) => {
            let found = match why {
                Unsupported::ObjectFormat(format) => details([
                    ("reason", "object_format".into()),
                    ("object_format", format.into()),
                ]),
                Unsupported::RefStorage(storage) => details([
                    ("reason", "ref_storage".into()),
                    ("ref_storage", storage.into()),
                ]),
                Unsupported::Shallow => {
                    return error(ErrorCode::GitUnsupported, message)
                        .with_details(details([("reason", "shallow".into())]))
                        .with_hint(hint(format!(
                            "git -C {} fetch --unshallow",
                            shell_word(git_path)
                        )));
                }
            };
            error(ErrorCode::GitUnsupported, message).with_details(found)
        }
        SourceError::Missing { oid } => unreadable(message, Some(oid), "missing", git_path),
        SourceError::Corrupt { oid, .. } => unreadable(message, Some(oid), "corrupt", git_path),
        SourceError::Io { .. } => unreadable(message, None, "io", git_path),
    }
}

/// [`git_import_error`] for the conversion's refusal `e`, whose message is `message`.
fn convert_error(e: ConvertError, message: String, git_path: &str, git_ref: &str) -> OpError {
    let commit_of = |commit: GitOid| details([("commit", commit.to_string().into())]);
    match e {
        ConvertError::Source(e) => source_error(e, message, git_path, git_ref),
        ConvertError::Malformed { oid, .. } => {
            unreadable(message, Some(oid), "malformed", git_path)
        }
        ConvertError::Lossy {
            commit,
            class,
            path,
            ..
        } => {
            let mut found = commit_of(commit);
            found.insert("class".to_owned(), class.as_str().into());
            put_path(&mut found, &path);
            error(ErrorCode::GitImportLossy, message)
                .with_details(found)
                .with_hint(hint(format!(
                    "run it again with --lossy {class} to drop or normalize it and mark the \
                     changeset (git.lossy)"
                )))
        }
        ConvertError::Parents { commit, parents } => {
            let mut found = details([("reason", "parents".into())]);
            found.extend(commit_of(commit));
            found.insert("parents".to_owned(), parents.into());
            // A changeset counts its parents in one byte (storage spec §1).
            found.insert("max".to_owned(), u8::MAX.into());
            error(ErrorCode::GitUnsupported, message).with_details(found)
        }
        ConvertError::Mode { commit, path, mode } => {
            let mut found = details([("reason", "mode".into())]);
            found.extend(commit_of(commit));
            put_path(&mut found, &path);
            found.insert("mode".to_owned(), mode.escape_ascii().to_string().into());
            error(ErrorCode::GitUnsupported, message).with_details(found)
        }
        ConvertError::TooLarge { commit, path, size } => {
            let mut found = commit_of(commit);
            put_path(&mut found, &path);
            found.insert("size".to_owned(), size.into());
            found.insert("max".to_owned(), MAX_OBJECT_LEN.into());
            error(ErrorCode::TooLarge, message).with_details(found)
        }
        ConvertError::Bug { commit } => {
            error(ErrorCode::Internal, message).with_details(commit_of(commit))
        }
    }
}

/// `E_GIT_READ`: the source could not give an object, `object` when there is one, for `reason`
/// (`missing`, `corrupt`, `malformed` or `io`); the hint checks the repository, but after an
/// `io` failure, which names no object.
fn unreadable(message: String, object: Option<GitOid>, reason: &str, git_path: &str) -> OpError {
    let mut found = details([("reason", reason.into())]);
    let Some(object) = object else {
        return error(ErrorCode::GitRead, message).with_details(found);
    };
    found.insert("object".to_owned(), object.to_string().into());
    error(ErrorCode::GitRead, message)
        .with_details(found)
        .with_hint(hint(format!("git -C {} fsck", shell_word(git_path))))
}

/// Puts `path`, an entry's path as git has it, into `found` as `path`; a path that is not UTF-8
/// also as `path_base64`, its exact bytes, `path` then holding it with each byte that is not
/// UTF-8 replaced (U+FFFD).
fn put_path(found: &mut Map<String, Value>, path: &[u8]) {
    let text = String::from_utf8_lossy(path);
    if std::str::from_utf8(path).is_err() {
        found.insert("path_base64".to_owned(), STANDARD.encode(path).into());
    }
    found.insert("path".to_owned(), text.into_owned().into());
}

/// `stopped`, the error of an import that stopped after it recorded `imported` commits, the last
/// of them `last`: it says so (`details.imported`, and `details.last_commit`, null when none
/// was recorded), and, unless it found the store damaged, which an import run again does not
/// mend, its hint is to run the same import again.
fn progress(mut stopped: OpError, imported: u64, last: Option<GitOid>) -> OpError {
    let last = last.map_or(Value::Null, |last| last.to_string().into());
    stopped
        .details
        .insert("imported".to_owned(), imported.into());
    stopped.details.insert("last_commit".to_owned(), last);
    if stopped.code != ErrorCode::StoreCorrupt.as_str() {
        stopped.hint = Some(RESUME_HINT.to_owned());
    }
    stopped
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
            RepoError::ReservedMetadataKey {
                key: "x:b64".into(),
            },
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
        let moved = RepoError::RefMoved {
            name: "main".into(),
            actual: Some(id()),
            recovered: Box::default(),
        };
        maps(moved, op, ErrorCode::CasMismatch);
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

    /// A commit with `advance` whose ref moved first recorded nothing: `E_CAS_MISMATCH` (exit 4,
    /// retryable) with what the ref names and `committed: false`, a message that says to commit
    /// again on top of it, and a hint that only reads. For a ref that is gone, `actual` is null,
    /// the message says the ref does not exist and the commit recorded nothing, and the hint
    /// lists the refs.
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
        let said = "ref main does not exist, so it does not name this commit's parent";
        assert!(gone.message.starts_with(said), "{gone:?}");
        assert!(gone.message.contains("recorded nothing"), "{gone:?}");
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
            StoreError::ExternalIdCorrupt {
                system: "git".into(),
                key: "ab".repeat(20),
            },
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

    /// The store's refusal of an object longer than an object holds (`ObjectBytes`) is
    /// `E_TOO_LARGE`, exit 1 and not retryable, with the `details` of every store limit: which
    /// limit, the most it holds and what was asked. Every other store limit stays
    /// `E_INVALID_ARG`.
    #[test]
    fn an_object_limit_is_e_too_large() {
        let limit = |what| {
            RepoError::Store(StoreError::Limit {
                what,
                max: 536_870_912,
                got: 536_870_913,
            })
        };
        let error = repo_error(limit(LimitKind::ObjectBytes), During::Op);
        assert_eq!(
            (error.code.as_str(), error.exit, error.retryable),
            ("E_TOO_LARGE", 1, false)
        );
        let want = serde_json::json!({
            "store_limit": "object_bytes",
            "max": 536_870_912,
            "got": 536_870_913
        });
        assert_eq!(Value::Object(error.details), want);
        for what in LimitKind::ALL {
            if what != LimitKind::ObjectBytes {
                maps(limit(what), During::Op, ErrorCode::InvalidArg);
            }
        }
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

    /// A journal append that the filesystem refused in a way that running the same command again
    /// meets again, for want of permission (`EACCES`, `EPERM`) or as a read-only filesystem
    /// (`EROFS`), and that the store cut back, committed nothing all the same: `E_IO`, exit 1,
    /// not retryable, `committed: false`, naming the append, with a hint that says what to fix
    /// before running it again (ruling M16-R8). Any other failure there, `EIO` or `ENOSPC`, stays
    /// the retryable `E_IO_TRANSIENT`.
    #[test]
    fn a_journal_append_refused_for_good_is_e_io_and_not_committed() {
        let not_committed = |errno| {
            let source = IoFailure::new(
                FileOp::Fs(FsMethod::AppendDurable),
                Some("meta/journal.off".into()),
                io::Error::from_raw_os_error(errno),
            );
            repo_error(
                RepoError::Store(StoreError::NotCommitted { source }),
                During::Op,
            )
        };
        for (errno, fix) in [(13, "writable"), (1, "writable"), (30, "read-write")] {
            let error = not_committed(errno);
            assert_eq!(
                (error.code.as_str(), error.exit, error.retryable),
                ("E_IO", 1, false),
                "errno {errno}"
            );
            assert_eq!(
                Value::Object(error.details),
                serde_json::json!({
                    "committed": false,
                    "fs_op": "append_durable",
                    "store_file": "meta/journal.off"
                }),
                "errno {errno}"
            );
            assert!(
                error.message.contains("did not commit"),
                "{}",
                error.message
            );
            let hint = error.hint.unwrap_or_default();
            assert!(
                hint.contains(fix) && hint.contains("same command"),
                "errno {errno}: {hint}"
            );
        }
        for errno in [5, 28] {
            let error = not_committed(errno);
            assert_eq!(
                (error.code.as_str(), error.exit, error.retryable),
                ("E_IO_TRANSIENT", 4, true),
                "errno {errno}"
            );
            assert_eq!(error.details["committed"], false, "errno {errno}");
        }
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

    /// The source repository and git ids the `git import` mapping tests name.
    const GIT_PATH: &str = "/nonexistent/swarmops-test/repo.git";
    const GIT_REF: &str = "stable";
    const COMMIT: GitOid = GitOid([0x11; 20]);
    const OBJECT: GitOid = GitOid([0x22; 20]);

    /// `e`, a `git import` of [`GIT_REF`] from [`GIT_PATH`] that failed, maps to `code`: its
    /// string, exit code and retry advice, and `details` as `want`; the error.
    #[track_caller]
    fn git_maps(e: ImportError, code: ErrorCode, want: &Value) -> OpError {
        let shown = format!("{e:?}");
        let error = git_import_error(e, GIT_PATH, GIT_REF);
        assert_eq!(error.code, code.as_str(), "{shown}");
        let advice = (error.exit, error.retryable);
        assert_eq!(advice, (code.exit(), code.retryable()), "{shown}");
        assert_eq!(&Value::Object(error.details.clone()), want, "{shown}");
        assert!(!error.message.is_empty(), "{shown}");
        error
    }

    /// The source's refusals of a `git import`, read for the walk or for the conversion alike:
    /// no repository, a ref that names nothing or no commit, a repository it does not read as it
    /// is, and an object it cannot give; each with its `details`, and a hint where a git command
    /// is the next step.
    #[test]
    fn git_import_source_errors_map_to_their_codes() {
        use serde_json::json;
        use swarmops_git::RawKind;

        let source = ImportError::Source;
        let not_a_repo = SourceError::NotARepo {
            reason: "no HEAD".into(),
        };
        let want = json!({"git_path": GIT_PATH});
        git_maps(source(not_a_repo), ErrorCode::GitNotARepo, &want);
        let unknown = SourceError::RefNotFound {
            name: GIT_REF.into(),
        };
        let want = json!({"git_ref": GIT_REF});
        let error = git_maps(source(unknown), ErrorCode::GitRefNotFound, &want);
        let refs = "git -C /nonexistent/swarmops-test/repo.git for-each-ref";
        assert_eq!(error.hint.as_deref(), Some(refs));
        let tree = SourceError::NotACommit {
            name: GIT_REF.into(),
            kind: RawKind::Tree,
        };
        let want = json!({"git_ref": GIT_REF, "kind": "tree"});
        git_maps(source(tree), ErrorCode::GitRefNotFound, &want);
        let unsupported = |why| source(SourceError::Unsupported(why));
        let sha256 = unsupported(Unsupported::ObjectFormat("sha256".into()));
        let want = json!({"reason": "object_format", "object_format": "sha256"});
        git_maps(sha256, ErrorCode::GitUnsupported, &want);
        let reftable = unsupported(Unsupported::RefStorage("reftable".into()));
        let want = json!({"reason": "ref_storage", "ref_storage": "reftable"});
        git_maps(reftable, ErrorCode::GitUnsupported, &want);
        let shallow = unsupported(Unsupported::Shallow);
        let want = json!({"reason": "shallow"});
        let error = git_maps(shallow, ErrorCode::GitUnsupported, &want);
        let deepen = "git -C /nonexistent/swarmops-test/repo.git fetch --unshallow";
        assert_eq!(error.hint.as_deref(), Some(deepen));

        let object = OBJECT.to_string();
        let walked: fn(SourceError) -> ImportError = ImportError::Source;
        let converted: fn(SourceError) -> ImportError =
            |e| ImportError::Convert(ConvertError::Source(e));
        for read in [walked, converted] {
            let missing = read(SourceError::Missing { oid: OBJECT });
            let want = json!({"reason": "missing", "object": object});
            let error = git_maps(missing, ErrorCode::GitRead, &want);
            let fsck = "git -C /nonexistent/swarmops-test/repo.git fsck";
            assert_eq!(error.hint.as_deref(), Some(fsck));
            let corrupt = read(SourceError::Corrupt {
                oid: OBJECT,
                reason: "its bytes hash to another id".into(),
            });
            let want = json!({"reason": "corrupt", "object": object});
            git_maps(corrupt, ErrorCode::GitRead, &want);
            let failed = read(SourceError::Io {
                reason: "Input/output error".into(),
            });
            let error = git_maps(failed, ErrorCode::GitRead, &json!({"reason": "io"}));
            assert_eq!(error.hint, None);
        }
    }

    /// The conversion's refusals of a commit: an object that does not parse, content of a lossy
    /// class the request does not allow (a path that is not UTF-8 given in base64 too, and no
    /// path for `nonstandard`), what no class covers, a blob past the object limit, and a bug.
    #[test]
    fn git_import_conversion_errors_map_to_their_codes() {
        use serde_json::json;
        use swarmops_git::LossyClass;

        let convert = ImportError::Convert;
        let commit = COMMIT.to_string();
        let malformed = ConvertError::Malformed {
            oid: OBJECT,
            what: "no tree line",
        };
        let want = json!({"reason": "malformed", "object": OBJECT.to_string()});
        git_maps(convert(malformed), ErrorCode::GitRead, &want);
        let lossy = |class, path: &[u8]| {
            convert(ConvertError::Lossy {
                commit: COMMIT,
                class,
                path: path.to_vec(),
                detail: "what is wrong".into(),
            })
        };
        let gitlink = lossy(LossyClass::Gitlinks, b"lib/sub");
        let want = json!({"commit": commit, "class": "gitlinks", "path": "lib/sub"});
        let error = git_maps(gitlink, ErrorCode::GitImportLossy, &want);
        let hint = error.hint.unwrap_or_default();
        assert!(hint.contains("--lossy gitlinks"), "{hint}");
        let latin1 = lossy(LossyClass::Paths, b"caf\xe9.txt");
        let want = json!({
            "commit": commit,
            "class": "paths",
            "path": "caf\u{fffd}.txt",
            "path_base64": "Y2Fm6S50eHQ=",
        });
        git_maps(latin1, ErrorCode::GitImportLossy, &want);
        let headers = lossy(LossyClass::Nonstandard, b"");
        let want = json!({"commit": commit, "class": "nonstandard", "path": ""});
        git_maps(headers, ErrorCode::GitImportLossy, &want);
        let parents = ConvertError::Parents {
            commit: COMMIT,
            parents: 256,
        };
        let want = json!({"reason": "parents", "commit": commit, "parents": 256, "max": 255});
        git_maps(convert(parents), ErrorCode::GitUnsupported, &want);
        let mode = ConvertError::Mode {
            commit: COMMIT,
            path: b"a/b".to_vec(),
            mode: b"123".to_vec(),
        };
        let want = json!({"reason": "mode", "commit": commit, "path": "a/b", "mode": "123"});
        git_maps(convert(mode), ErrorCode::GitUnsupported, &want);
        let large = ConvertError::TooLarge {
            commit: COMMIT,
            path: b"big.bin".to_vec(),
            size: MAX_OBJECT_LEN + 1,
        };
        let want = json!({
            "commit": commit,
            "path": "big.bin",
            "size": MAX_OBJECT_LEN + 1,
            "max": MAX_OBJECT_LEN,
        });
        git_maps(convert(large), ErrorCode::TooLarge, &want);
        let bug = convert(ConvertError::Bug { commit: COMMIT });
        git_maps(bug, ErrorCode::Internal, &json!({"commit": commit}));
    }

    /// A target that is no ref name; one whose head the tip does not lead from, whose `base` is
    /// the git commit that head holds, if any (exit 3, the hint its log); and one that moved
    /// during the import, `E_CAS_MISMATCH` with a hint that only reads.
    #[test]
    fn git_import_target_errors_map_to_their_codes() {
        use serde_json::json;

        let bad = ImportError::RefName("feature/x".into());
        git_maps(
            bad,
            ErrorCode::RefNameInvalid,
            &json!({"name": "feature/x"}),
        );
        let head = ChangesetId(ObjectId([7; 32]));
        let diverged = |base| ImportError::Diverged {
            target_ref: "main".into(),
            target_head: head,
            base,
            git_head: COMMIT,
        };
        let want = json!({
            "target_ref": "main",
            "target_head": head.to_string(),
            "base": OBJECT.to_string(),
            "git_head": COMMIT.to_string(),
        });
        let error = git_maps(diverged(Some(OBJECT)), ErrorCode::GitImportDiverged, &want);
        assert_eq!(error.exit, 3);
        assert_eq!(error.hint.as_deref(), Some("swarmops log main"));
        let want = json!({
            "target_ref": "main",
            "target_head": head.to_string(),
            "git_head": COMMIT.to_string(),
        });
        git_maps(diverged(None), ErrorCode::GitImportDiverged, &want);
        let actual = ChangesetId(ObjectId([8; 32]));
        let moved = ImportError::CasMismatch {
            target_ref: "main".into(),
            actual: Some(actual),
        };
        let want = json!({"name": "main", "actual": actual.to_string()});
        let error = git_maps(moved, ErrorCode::CasMismatch, &want);
        assert_eq!(error.hint, Some(format!("swarmops show {actual}")));
    }

    /// A store failure stops an import with the code the operations give it, saying how far the
    /// import got (`imported`, `last_commit`, null when no frame committed) beside its own
    /// details, and hinting to run it again, but where it found the store damaged; a frame that
    /// committed and failed after its commit point is `E_IO` with `committed: true` and its
    /// warnings.
    #[test]
    fn git_import_store_failures_say_how_far_it_got() {
        use serde_json::json;

        let commit = COMMIT.to_string();
        let stopped = |e: StoreError, imported, last| ImportError::Store {
            source: RepoError::Store(e),
            imported,
            last,
        };
        let want = json!({"imported": 2, "last_commit": commit});
        let busy = stopped(StoreError::Busy, 2, Some(COMMIT));
        let error = git_maps(busy, ErrorCode::Busy, &want);
        assert!(error.message.contains("after 2 commits"), "{error:?}");
        assert_eq!(error.hint.as_deref(), Some(RESUME_HINT));
        let cut = StoreError::NotCommitted {
            source: IoFailure::new(
                FileOp::Fs(FsMethod::AppendDurable),
                Some("meta/journal.log".into()),
                io::Error::from_raw_os_error(5),
            ),
        };
        let want = json!({
            "committed": false,
            "fs_op": "append_durable",
            "store_file": "meta/journal.log",
            "imported": 0,
            "last_commit": null,
        });
        let error = git_maps(stopped(cut, 0, None), ErrorCode::IoTransient, &want);
        assert_eq!(error.hint.as_deref(), Some(RESUME_HINT));
        let damaged = StoreError::Corrupt {
            id: ObjectId([9; 32]),
        };
        let want = json!({
            "object": ObjectId([9; 32]).to_hex(),
            "imported": 1,
            "last_commit": commit,
        });
        let damaged = stopped(damaged, 1, Some(COMMIT));
        let error = git_maps(damaged, ErrorCode::StoreCorrupt, &want);
        assert_eq!(error.hint.as_deref(), Some("swarmops verify --deep"));
        let post = ImportError::PostCommit {
            warnings: vec!["frame 3 committed, but its listing failed".into()],
            imported: 4,
            last: Some(COMMIT),
        };
        let want = json!({
            "committed": true,
            "warnings": ["frame 3 committed, but its listing failed"],
            "imported": 4,
            "last_commit": commit,
        });
        let error = git_maps(post, ErrorCode::Io, &want);
        assert_eq!(error.hint.as_deref(), Some(RESUME_HINT));
    }
}
