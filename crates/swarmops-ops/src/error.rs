//! The error table (operations spec §0): every repository and store failure mapped to the
//! operations spec's code, with its `details` and, where there is an obvious next step, a
//! command as its hint. [`crate::LocalOps`]'s documentation gives the table. Every message is
//! one line: control characters in it are escaped. The mapped enums are `#[non_exhaustive]`:
//! each `match` ends in a wildcard arm, `E_INTERNAL`, so a new variant compiles unmapped.

use std::io;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use camino::Utf8Path;
use serde_json::{Map, Value};
use swarmops_api::{DEFAULT_MAX_CONFLICT_BYTES, ErrorCode, OpError};
use swarmops_core::{ChangesetId, ObjectId, RepoPath};
use swarmops_git::limits::LimitExceeded;
use swarmops_git::mirror::MirrorError;
use swarmops_git::sync::SyncError;
use swarmops_git::transport::{RejectReason, TransportError, UrlRefused};
use swarmops_git::{
    ConvertError, DivergedReason, ExportError, GitOid, ImportError, SinkError, SourceError,
    Unsupported,
};
use swarmops_repo::{MergeConflicts, RepoError, StoreError};
use swarmops_store::{IoFailure, Layout, LimitKind, MAX_OBJECT_LEN};

use crate::local;

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
        RepoError::CasMismatch {
            target,
            actual,
            expected,
            ..
        } => target_moved(&target, actual, expected),
        RepoError::OpNotFound { op, last, .. } => {
            let last = last.map_or(Value::Null, Value::from);
            error(ErrorCode::OpNotFound, message)
                .with_details(details([("op", op.into()), ("last", last)]))
                .with_hint("swarmops op log")
        }
        RepoError::NothingToUndo { op: number, .. } => error(ErrorCode::NothingToUndo, message)
            .with_details(details([("op", number.into())]))
            .with_hint("swarmops op log"),
        RepoError::UndoBlocked {
            op: number,
            name,
            expected,
            actual,
            ..
        } => undo_blocked(number, &name, expected, actual),
        // A land's (M4).
        e @ (RepoError::Conflict(_)
        | RepoError::NothingToLand(_)
        | RepoError::RetryExhausted { .. }
        | RepoError::KeyReused { .. }
        | RepoError::MergeChangeset(_)
        | RepoError::BaseNotAncestor { .. }
        | RepoError::NoMergeBase { .. }) => land_error(e, message),
        RepoError::MissingObject { id, .. } => {
            corrupt(message, during, "swarmops verify").with_details(object(id))
        }
        RepoError::Codec(_) => corrupt(message, during, "swarmops verify"),
        // A derivation that fails on immutable inputs fails again on every call: a bug. So is
        // a variant of another crate's enum this build does not know.
        RepoError::Derived { .. } | RepoError::Internal(_) => error(ErrorCode::Internal, message),
        // A cache file someone placed, or a faulty build wrote: a deep verify names them all.
        RepoError::DerivedCorrupt {
            name, changeset, ..
        } => corrupt(message, during, "swarmops verify --deep").with_details(details([
            ("derivation", name.into()),
            ("changeset", changeset.to_string().into()),
        ])),
        _ => unmapped(message),
    }
}

/// The operations-spec error for a land's error `e`, whose message is `message`: [`repo_error`]'s
/// table, a land's rows (M4). E_CONFLICT's report holds hunk texts within the default
/// `max_conflict_bytes` here; `land` gives it its request's ([`conflict`]). Any other error is
/// [`repo_error`]'s.
fn land_error(e: RepoError, message: String) -> OpError {
    match e {
        RepoError::Conflict(conflicts) => conflict(&conflicts, message, DEFAULT_MAX_CONFLICT_BYTES),
        RepoError::NothingToLand(reason) => error(ErrorCode::NothingToLand, message)
            .with_details(details([("reason", reason.as_str().into())])),
        RepoError::RetryExhausted { attempts, actual } => {
            error(ErrorCode::CasRetryExhausted, message)
                .with_details(details([
                    ("attempts", attempts.into()),
                    ("actual", actual.to_string().into()),
                ]))
                .with_hint("run the land again: nothing of this one was recorded")
        }
        RepoError::KeyReused { key, landed } => error(ErrorCode::InvalidArg, message)
            .with_details(details([
                ("key", key.into()),
                ("landed", landed.to_string().into()),
            ]))
            .with_hint(hint(format!("swarmops show {landed}"))),
        RepoError::MergeChangeset(id) => error(ErrorCode::InvalidArg, message)
            .with_details(details([("changeset", id.to_string().into())])),
        RepoError::BaseNotAncestor { base, head } => error(ErrorCode::InvalidArg, message)
            .with_details(details([
                ("base", base.to_string().into()),
                ("head", head.to_string().into()),
            ]))
            .with_hint(hint(format!("swarmops log {head}"))),
        RepoError::NoMergeBase { head, theirs } => error(ErrorCode::InvalidArg, message)
            .with_details(details([
                ("head", head.to_string().into()),
                ("theirs", theirs.to_string().into()),
            ]))
            .with_hint(hint(format!("swarmops log {head}"))),
        other => repo_error(other, During::Op),
    }
}

/// `E_CONFLICT` for a land whose change conflicts with its target as `conflicts` say, whose
/// message is `message`: `details.report` is the conflict report, its hunk texts within
/// `max_bytes` ([`crate::conflict::report`]), and the hint reads the target's side of the first
/// conflicting path, then names the land again with its resolutions, against the target's head
/// the report names (`land --resolve`).
pub(crate) fn conflict(conflicts: &MergeConflicts, message: String, max_bytes: u64) -> OpError {
    let report = match crate::conflict::report(conflicts, max_bytes) {
        Ok(report) => report,
        Err(e) => return e,
    };
    let side = match report.files.first() {
        Some(file) => shell_word(&format!("{}:{}", report.theirs, file.path)),
        None => format!("{}:<path>", report.theirs),
    };
    let next = hint(format!(
        "swarmops cat {side}, then swarmops land {} --resolve <file> with each path's resolution \
         and against = {}",
        report.ours, report.theirs
    ));
    let report = serde_json::to_value(&report).expect("a conflict report serializes");
    error(ErrorCode::Conflict, message)
        .with_details(details([("report", report)]))
        .with_hint(next)
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
        StoreError::FormatRequired { name, needs, store } => {
            error(ErrorCode::FormatRequired, message)
                .with_details(details([
                    ("name", name.into()),
                    ("needs", needs.into()),
                    ("store", store.into()),
                ]))
                .with_hint("swarmops upgrade")
        }
        StoreError::TagImmutable(name) => error(ErrorCode::TagImmutable, message)
            .with_details(details([("name", name.into())]))
            .with_hint("delete the tag with swarmops ref delete, then create it again"),
        StoreError::RefProtected(name) => error(ErrorCode::RefProtected, message)
            .with_details(details([("name", name.into())]))
            .with_hint("move it with `swarmops ref set` instead; a protected ref is never deleted"),
        StoreError::MaintenanceBusy => error(ErrorCode::MaintenanceBusy, message)
            .with_hint("retry once the other gc, repair or upgrade is done"),
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
                _ => return unmapped(message),
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
        StoreError::Internal(_) => error(ErrorCode::Internal, message),
        _ => unmapped(message),
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
pub(crate) fn created_but_unread(at: &Utf8Path, mut e: OpError) -> OpError {
    e.details.insert("created".to_owned(), true.into());
    e.message = one_line(format!(
        "the store at {at} is made, but reading it back failed: {}",
        e.message
    ));
    e.hint = Some(hint(format!(
        "swarmops --repo {} info",
        shell_word(at.as_str())
    )));
    e
}

/// An error of `code` with `message`, its control characters escaped so that it is one line.
pub(crate) fn error(code: ErrorCode, message: impl Into<String>) -> OpError {
    OpError::new(code, one_line(message.into()))
}

/// The hint of an error of another crate that this build maps to no code of its own.
const UNMAPPED_HINT: &str = "an error this build does not map: report it";

/// `E_INTERNAL` with `message`, for an error variant of another crate that this build does not
/// know (the enums are `#[non_exhaustive]`).
fn unmapped(message: String) -> OpError {
    error(ErrorCode::Internal, message).with_hint(UNMAPPED_HINT)
}

/// `E_INTERNAL` for a value of `ty`, an enum of another crate, of a variant this build does not
/// know, met where the operation projects it onto the API (the enums are `#[non_exhaustive]`).
pub(crate) fn unknown_variant(ty: &str) -> OpError {
    OpError::new(
        ErrorCode::Internal,
        format!("{ty}: a variant this build does not know"),
    )
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
    let mut error = local::cas_mismatch(name, actual, message);
    error.details.insert("committed".to_owned(), false.into());
    error
}

/// A land with `expect_target` (M5) that found its target `name` naming `actual`, not
/// `expected`, so that it landed nothing: `E_CAS_MISMATCH` `{name, actual, expected, committed}`,
/// `committed` always `false`, as a commit whose ref moved maps; the hint only reads.
fn target_moved(name: &str, actual: ChangesetId, expected: ChangesetId) -> OpError {
    let message = format!(
        "ref {name} names {actual}, not {expected}, so this land landed nothing: rebase the \
         change onto {actual}, check it, and land it again with --expect-target {actual}"
    );
    let mut error = local::cas_mismatch(name, Some(actual), message);
    error
        .details
        .insert("expected".to_owned(), expected.to_string().into());
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
    let mut error = local::cas_mismatch(name, actual, message).with_hint("swarmops op log");
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
            external_id,
        } => {
            let mut found = details([
                ("target_ref", target_ref.as_str().into()),
                ("target_head", target_head.to_string().into()),
                ("git_head", git_head.to_string().into()),
            ]);
            if let Some(base) = base {
                found.insert("base".to_owned(), base.to_string().into());
            }
            // An entry the walk passed over, which a deep verify reports, is the first thing to
            // look at; otherwise the target's history.
            let next = match external_id {
                Some(entry) => {
                    let entry = details([
                        ("commit", entry.commit.to_string().into()),
                        ("changeset", entry.changeset.to_string().into()),
                    ]);
                    found.insert("external_id".to_owned(), entry.into());
                    "swarmops verify --deep".to_owned()
                }
                None => hint(format!("swarmops log {}", shell_word(&target_ref))),
            };
            error(ErrorCode::GitImportDiverged, message)
                .with_details(found)
                .with_hint(next)
        }
        ImportError::CasMismatch { target_ref, actual } => {
            let message = format!(
                "{message}; every commit is recorded, and the same git import run again moves the \
                 ref if what it names leads to the tip, or fails with E_GIT_IMPORT_DIVERGED"
            );
            local::cas_mismatch(&target_ref, actual, one_line(message))
        }
        ImportError::Store {
            source,
            imported,
            last,
        } => {
            // A journal append the filesystem refused for good (ruling M16-R8) is mended by its
            // fix, which the hint names before the run again that resumes.
            let fix = match &*source {
                RepoError::Store(StoreError::NotCommitted { source }) => lasting_refusal(source),
                _ => None,
            };
            let stopped = op(*source).with_message(one_line(message));
            let mut stopped = progress(stopped, imported, last);
            if let Some(fix) = fix {
                stopped.hint = Some(format!("{fix}, then {RESUME_HINT}"));
            }
            stopped
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
        _ => unmapped(message),
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
        SourceError::Unsupported(why) => unsupported(why, message, git_path),
        SourceError::Missing { oid } => unreadable(message, Some(oid), "missing", git_path),
        SourceError::Corrupt { oid, .. } => unreadable(message, Some(oid), "corrupt", git_path),
        SourceError::TooLarge { oid, kind, size } => {
            let found = details([
                ("object", oid.to_string().into()),
                ("kind", kind.as_str().into()),
                ("size", size.into()),
                ("max", MAX_OBJECT_LEN.into()),
            ]);
            error(ErrorCode::TooLarge, message).with_details(found)
        }
        SourceError::Io { .. } => unreadable(message, None, "io", git_path),
        _ => unmapped(message),
    }
}

/// `E_GIT_UNSUPPORTED`: the repository at `git_path` is one the git side does not read or write
/// as it is, `why` (a SHA-256 repository, reftable refs, a shallow clone, which a hint deepens, a
/// FIFO, a socket or a device where it reads a file of the git directory, or a file it reads whole
/// there that is too long, each of which it names).
fn unsupported(why: Unsupported, message: String, git_path: &str) -> OpError {
    let found = match why {
        Unsupported::ObjectFormat(format) => details([
            ("reason", "object_format".into()),
            ("object_format", format.into()),
        ]),
        Unsupported::RefStorage(storage) => details([
            ("reason", "ref_storage".into()),
            ("ref_storage", storage.into()),
        ]),
        Unsupported::SpecialFile(file) => {
            details([("reason", "special_file".into()), ("file", file.into())])
        }
        Unsupported::FileTooLong { file, len, max } => details([
            ("reason", "file_too_long".into()),
            ("file", file.into()),
            ("size", len.into()),
            ("max", max.into()),
        ]),
        Unsupported::Shallow => {
            return error(ErrorCode::GitUnsupported, message)
                .with_details(details([("reason", "shallow".into())]))
                .with_hint(hint(format!(
                    "git -C {} fetch --unshallow",
                    shell_word(git_path)
                )));
        }
        _ => return unmapped(message),
    };
    error(ErrorCode::GitUnsupported, message).with_details(found)
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
        _ => unmapped(message),
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

/// The operations-spec error for a `git export` of `source_ref` into the branch `branch` of the
/// repository at `git_path` that failed with `e`: the table in [`crate::LocalOps`]'s
/// documentation. The repository is named by the path the request gave, its files relative to its
/// git directory and its objects by their git ids; a store failure is the code the operations give
/// it.
pub(crate) fn git_export_error(
    e: ExportError,
    git_path: &str,
    source_ref: &str,
    branch: &str,
) -> OpError {
    let message = e.to_string();
    match e {
        ExportError::SourceRef(name) => {
            op(RepoError::NotFound(name)).with_message(one_line(message))
        }
        ExportError::RefName(name) => ref_name_invalid(name, message),
        ExportError::CheckedOut {
            branch: checked_out,
        } => {
            let elsewhere = hint(format!(
                "detach the work tree's HEAD (git -C {} switch --detach), or export to another \
                 branch (--branch <name>)",
                shell_word(git_path)
            ));
            error(ErrorCode::InvalidArg, message)
                .with_details(details([
                    ("reason", "checked_out".into()),
                    ("branch", checked_out.into()),
                ]))
                .with_hint(elsewhere)
        }
        ExportError::Diverged {
            branch: diverged_branch,
            git_head,
            head,
            reason,
        } => {
            let found = Diverged {
                branch: &diverged_branch,
                git_head,
                head,
                reason,
            };
            found.error(message, git_path, source_ref)
        }
        ExportError::Sink(e) => sink_error(e, message, git_path, branch),
        ExportError::Repo(e) => op(e),
        ExportError::Bug { .. } => error(ErrorCode::Internal, message),
        _ => unmapped(message),
    }
}

/// What an [`ExportError::Diverged`] says: the branch, the commit it names, the source's head and
/// why that commit is no export the head goes on from.
struct Diverged<'a> {
    branch: &'a str,
    git_head: GitOid,
    head: ChangesetId,
    reason: DivergedReason,
}

impl Diverged<'_> {
    /// `E_GIT_EXPORT_DIVERGED` (exit 3), the branch being one of the repository at `git_path` and
    /// the head `source_ref`'s: `details` name the reason, the branch, the commit it names (null
    /// for a branch deleted while the export wrote), the head and, when the branch holds the
    /// export of a changeset, that changeset; the hint is the next step the reason calls for.
    fn error(self, message: String, git_path: &str, source_ref: &str) -> OpError {
        let Diverged {
            branch,
            git_head,
            head,
            reason,
        } = self;
        let path = shell_word(git_path);
        let full = shell_word(&format!("refs/heads/{branch}"));
        let source = shell_word(source_ref);
        let mut found = details([
            ("reason", reason.as_str().into()),
            ("branch", branch.into()),
            ("git_head", git_head.to_string().into()),
            ("head_changeset", head.to_string().into()),
        ]);
        let mut message = message;
        let next = match reason {
            DivergedReason::BranchAhead { exported } => {
                found.insert("exported_changeset".to_owned(), exported.to_string().into());
                format!(
                    "swarmops git import {path} --ref {full} --into {source} brings the commits \
                     made in git into the store, then run the export again; --force replaces \
                     them instead"
                )
            }
            DivergedReason::BothMoved { exported } => {
                found.insert("exported_changeset".to_owned(), exported.to_string().into());
                format!(
                    "both git and {source} moved past {exported}, and nothing is merged: git -C \
                     {path} log {full} shows the git side, and --force replaces it"
                )
            }
            DivergedReason::BranchMoved => format!(
                "git -C {path} log {full} shows what the branch holds; --force replaces it with \
                 the export"
            ),
            DivergedReason::SourceMovedBack { exported } => {
                found.insert("exported_changeset".to_owned(), exported.to_string().into());
                format!(
                    "swarmops op log names what moved {source}; --force moves the branch back to \
                     the export of its head"
                )
            }
            DivergedReason::LossyHistory { .. } => "export to another branch (--branch <name>), \
                                                     or --force replaces the original commits \
                                                     with the export"
                .to_owned(),
            DivergedReason::MovedDuringExport { actual } => {
                if actual.is_none() {
                    found.insert("git_head".to_owned(), Value::Null);
                    message = format!(
                        "the branch {branch} was deleted while the export wrote, so it has \
                         diverged from {head} (moved_during_export)"
                    );
                }
                "run the same git export again".to_owned()
            }
            _ => return unmapped(message),
        };
        error(ErrorCode::GitExportDiverged, message)
            .with_details(found)
            .with_hint(hint(next))
    }
}

/// [`git_export_error`] for the repository's failure `e`, whose message is `message`: the
/// repository at `git_path`, whose branch `branch` the export writes.
fn sink_error(e: SinkError, message: String, git_path: &str, branch: &str) -> OpError {
    let path = shell_word(git_path);
    match e {
        SinkError::NotARepo { .. } => error(ErrorCode::GitNotARepo, message)
            .with_details(details([("git_path", git_path.into())]))
            .with_hint(hint(format!(
                "git init --bare {path} makes an empty repository to export to"
            ))),
        SinkError::Unsupported(why) => unsupported(why, message, git_path),
        SinkError::BadBranch { name } => {
            let why = hint(format!(
                "git check-ref-format --branch {} says why git takes no such branch",
                shell_word(&name)
            ));
            error(ErrorCode::RefNameInvalid, message)
                .with_details(details([("name", name.into())]))
                .with_hint(why)
        }
        SinkError::RefLocked { lock } => {
            let remove = hint(format!(
                "once no git process runs in the repository, remove {lock} from its git directory \
                 (git -C {path} rev-parse --git-path {lock} names it), as git itself says, then \
                 run the export again"
            ));
            error(ErrorCode::GitWrite, message)
                .with_details(details([
                    ("reason", "ref_locked".into()),
                    ("path", lock.into()),
                ]))
                .with_hint(remove)
        }
        SinkError::RefConflict { .. } => error(ErrorCode::GitWrite, message)
            .with_details(details([
                ("reason", "ref_name_conflict".into()),
                ("branch", branch.into()),
            ]))
            .with_hint(hint(format!(
                "git -C {path} for-each-ref lists the refs in the way; export to another branch \
                 (--branch <name>)"
            ))),
        SinkError::Missing { oid } => unreadable(message, Some(oid), "missing", git_path),
        SinkError::Corrupt { oid, .. } => unreadable(message, Some(oid), "corrupt", git_path),
        SinkError::Read { .. } => unreadable(message, None, "io", git_path),
        SinkError::Write { what, .. } => error(ErrorCode::GitWrite, message)
            .with_details(details([("reason", "io".into()), ("path", what.into())]))
            .with_hint(
                "run the same git export again once the cause is gone: it writes only what is \
                 still missing",
            ),
        _ => unmapped(message),
    }
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

// ---- The git remotes (M13) ----
//
// The mappings of the remote commands' errors: the transport's, the mirror's, the sync engine's,
// GitHub's limits and the URL rules (§4.3 of the M13 plan's design). The commands call them from
// the M13 plan's tasks C6, C7 and C8b on; until then only the tests below do.

/// Which git call of a remote command failed: `details.phase` of `E_REMOTE_UNREACHABLE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code, reason = "the remote commands (M13 C6, C7) use it")]
pub(crate) enum Phase {
    /// `git fetch`.
    Fetch,
    /// `git push`.
    Push,
    /// `git ls-remote`, which reads a remote's default branch.
    LsRemote,
}

impl Phase {
    /// The phase as `details.phase` writes it.
    fn as_str(self) -> &'static str {
        match self {
            Phase::Fetch => "fetch",
            Phase::Push => "push",
            Phase::LsRemote => "ls_remote",
        }
    }
}

/// The error of a remote operation not written yet: `E_INTERNAL`, naming the M13 task that
/// writes it (`C6`). Every one is written now; its test keeps the shape.
#[cfg(test)]
pub(crate) fn planned(task: &str) -> OpError {
    error(ErrorCode::Internal, format!("planned in M13 ({task})"))
}

/// `E_REMOTE_NOT_FOUND`: the store records no remote named `remote`.
#[allow(dead_code, reason = "the remote commands (M13 C6) use it")]
pub(crate) fn remote_not_found(remote: &str) -> OpError {
    error(
        ErrorCode::RemoteNotFound,
        format!("the store records no remote {remote:?}"),
    )
    .with_details(details([("remote", remote.into())]))
    .with_hint("swarmops remote list names the remotes; swarmops remote add records one")
}

/// `E_INVALID_ARG`: a remote's URL the rules refuse, `why` as `details.reason`. The URL is not
/// repeated: it may hold a password.
#[allow(dead_code, reason = "the remote commands (M13 C6) use it")]
pub(crate) fn url_refused(why: UrlRefused) -> OpError {
    let (message, next) = match why {
        UrlRefused::CleartextHttp => (
            "a remote's URL is not plain http, but to a loopback address",
            "use the https:// URL of the remote",
        ),
        UrlRefused::Scheme => (
            "a remote's URL is https://, ssh:// or user@host:path",
            "give the remote's https:// or ssh:// URL",
        ),
        UrlRefused::CredentialInUrl => (
            "a remote's URL holds no password",
            "leave the token to git's credential helper, or name its variable with --token-env",
        ),
        UrlRefused::OptionLike => (
            "a remote's URL or host does not start with -",
            "give the remote's https:// or ssh:// URL",
        ),
        _ => {
            return unmapped(format!("a remote's URL is refused ({})", why.as_str()));
        }
    };
    error(ErrorCode::InvalidArg, message)
        .with_details(details([("reason", why.as_str().into())]))
        .with_hint(next)
}

/// The operations-spec error for the transport's failure `e`, in `phase` of a command on
/// `remote`.
#[allow(dead_code, reason = "the remote commands (M13 C6, C7) use it")]
pub(crate) fn transport_error(e: TransportError, remote: &str, phase: Phase) -> OpError {
    let fetch = || hint(format!("swarmops fetch {}", shell_word(remote)));
    match e {
        TransportError::GitMissing => error(ErrorCode::GitUnsupported, "no git to run")
            .with_details(details([("reason", "git_missing".into())]))
            .with_hint("install git 2.39 or later, or name its binary in SWARMOPS_GIT"),
        TransportError::GitTooOld { found } => error(
            ErrorCode::GitUnsupported,
            format!("git {found} is older than 2.39"),
        )
        .with_details(details([
            ("reason", "git_too_old".into()),
            ("found", found.into()),
            ("needs", "2.39".into()),
        ]))
        .with_hint("install git 2.39 or later, or name its binary in SWARMOPS_GIT"),
        TransportError::Unreachable { cause, stderr_tail } => {
            let mut found = details([
                ("remote", remote.into()),
                ("phase", phase.as_str().into()),
                ("cause", cause.as_str().into()),
            ]);
            put_text(&mut found, "stderr_tail", stderr_tail);
            error(
                ErrorCode::RemoteUnreachable,
                format!("the remote {remote:?} did not answer ({})", cause.as_str()),
            )
            .with_details(found)
            .with_hint("run it again once the network is back: nothing changed")
        }
        TransportError::Rejected {
            reason,
            refs,
            remote_messages,
            git_exit,
            stderr_tail,
            token_env_unset,
        } => {
            let said = Said {
                refs,
                remote_messages,
                git_exit,
                stderr_tail,
                token_env_unset,
            };
            rejected(reason, said, remote)
        }
        TransportError::Stale { branch } => error(
            ErrorCode::RemoteDiverged,
            format!("{branch} on {remote:?} moved since the last fetch: nothing was sent"),
        )
        .with_details(details([
            ("remote", remote.into()),
            ("branch", branch.into()),
            ("reason", "remote_moved".into()),
        ]))
        .with_hint(fetch()),
        TransportError::NotFastForward { branch } => error(
            ErrorCode::RemoteDiverged,
            format!("{branch} on {remote:?} refused a push that is not a fast-forward"),
        )
        .with_details(details([
            ("remote", remote.into()),
            ("branch", branch.into()),
            ("reason", "not_fast_forward".into()),
        ]))
        .with_hint(hint(format!("swarmops sync {}", shell_word(remote)))),
        TransportError::Sha256 => error(
            ErrorCode::GitUnsupported,
            format!("the remote {remote:?} is a SHA-256 repository"),
        )
        .with_details(details([("reason", "sha256".into())])),
        _ => unmapped(format!("git failed on the remote {remote:?}")),
    }
}

/// What git said of a refusal: the refs the remote refused and its reasons, the remote's own
/// lines, git's exit status and the end of its stderr, and whether the token variable was unset.
struct Said {
    refs: Vec<(String, String)>,
    remote_messages: Vec<String>,
    git_exit: Option<i32>,
    stderr_tail: String,
    token_env_unset: bool,
}

/// `E_REMOTE_REJECTED` (exit 5): `remote` refused for `reason`, `said` as the details.
fn rejected(reason: RejectReason, said: Said, remote: &str) -> OpError {
    // 1. The details: each part git gave.
    let mut found = details([
        ("remote", remote.into()),
        ("reason", reason.as_str().into()),
    ]);
    if !said.refs.is_empty() {
        let refs = said
            .refs
            .into_iter()
            .map(|(name, message)| serde_json::json!({"ref": name, "message": message}))
            .collect();
        found.insert("refs".to_owned(), Value::Array(refs));
    }
    if !said.remote_messages.is_empty() {
        found.insert("remote_messages".to_owned(), said.remote_messages.into());
    }
    if let Some(exit) = said.git_exit {
        found.insert("git_exit".to_owned(), exit.into());
    }
    put_text(&mut found, "stderr_tail", said.stderr_tail);
    if said.token_env_unset {
        found.insert("token_env_unset".to_owned(), true.into());
    }

    // 2. The next step the reason calls for.
    let next = match reason {
        RejectReason::Auth => {
            "check the credential git uses for the remote, or the variable --token-env names"
        }
        RejectReason::NotFound => {
            "check the URL: a host such as GitHub answers so for a private repository the \
             credential cannot see"
        }
        RejectReason::Declined => {
            "details.refs and details.remote_messages give the remote's reason"
        }
        RejectReason::Unclassified => "details.stderr_tail gives what git said",
        RejectReason::BadObjects => {
            "the remote sent objects git refuses; nothing was moved; fix the remote's history"
        }
        _ => {
            let message = format!("the remote {remote:?} refused ({})", reason.as_str());
            return unmapped(message).with_details(found);
        }
    };
    error(
        ErrorCode::RemoteRejected,
        format!("the remote {remote:?} refused ({})", reason.as_str()),
    )
    .with_details(found)
    .with_hint(next)
}

/// The operations-spec error for the mirror's failure `e`.
#[allow(dead_code, reason = "the remote commands (M13 C6, C7, C8b) use it")]
pub(crate) fn mirror_error(e: MirrorError) -> OpError {
    let message = e.to_string();
    let again = "the mirror is a cache: run the command again; a mirror git cannot open is set \
                 aside and made again";
    match e {
        MirrorError::Busy => error(ErrorCode::Busy, message)
            .with_details(details([("lock", "git_mirror".into())]))
            .with_hint("run it again once the other fetch, push, sync or clone has ended"),
        MirrorError::GitLockBusy { file } => git_lock_busy(message, file),
        MirrorError::Sink(SinkError::RefLocked { lock }) => git_lock_busy(message, lock),
        MirrorError::Io(_) => error(ErrorCode::Io, message)
            .with_details(details([("store_file", "git".into())]))
            .with_hint(again),
        MirrorError::Sink(_) => error(ErrorCode::GitWrite, message)
            .with_details(details([("reason", "mirror".into())]))
            .with_hint(again),
        MirrorError::Source(_) => error(ErrorCode::GitRead, message)
            .with_details(details([("reason", "mirror".into())]))
            .with_hint(again),
        _ => unmapped(message),
    }
}

/// `E_BUSY` for a git lock file in the mirror, `file` (relative to `mirror.git`), that may
/// belong to a git child still writing; retryable, and no person removes it.
fn git_lock_busy(message: String, file: String) -> OpError {
    error(ErrorCode::Busy, message)
        .with_details(details([
            ("lock", "git_mirror_file".into()),
            ("file", file.into()),
        ]))
        .with_hint(
            "run it again in a few minutes: a dead process's lock file is cleared by the next run",
        )
}

/// The operations-spec error for the sync engine's failure `e`, in a sync of `remote`'s
/// `branch`.
pub(crate) fn sync_error(e: SyncError, remote: &str, branch: &str) -> OpError {
    let message = e.to_string();
    match e {
        SyncError::NoCommonBase => error(ErrorCode::RemoteDiverged, message)
            .with_details(details([
                ("remote", remote.into()),
                ("branch", branch.into()),
                ("reason", "no_common_base".into()),
            ]))
            .with_hint(hint(format!(
                "swarmops log main and the remote's history share nothing within 10,000 \
                 changesets: swarmops sync status {} says how far each is",
                shell_word(remote)
            ))),
        SyncError::Conflict {
            change,
            onto,
            conflicts,
        } => {
            let mut found = conflict(&conflicts, message, DEFAULT_MAX_CONFLICT_BYTES);
            found
                .details
                .insert("change".to_owned(), change.to_string().into());
            found
                .details
                .insert("onto".to_owned(), onto.to_string().into());
            found
        }
        SyncError::MergeChangeset(id) => error(ErrorCode::InvalidArg, message)
            .with_details(details([("changeset", id.to_string().into())]))
            .with_hint("push a merge changeset with git: the sync relocates single changes only"),
        SyncError::Repo(e) => land_error(e, message),
        _ => unmapped(message),
    }
}

/// `E_TOO_LARGE` for a push to `remote` past one of GitHub's limits, `e`: nothing was sent.
#[allow(dead_code, reason = "the push command (M13 C7) uses it")]
pub(crate) fn limit_error(e: LimitExceeded, remote: &str) -> OpError {
    let LimitExceeded {
        scope,
        path,
        size,
        limit,
        ..
    } = e;
    let (message, next) = match &path {
        Some(path) => (
            format!("{path} is {size} bytes, more than GitHub takes ({limit}): nothing was sent"),
            "take the file out of the history before pushing it to GitHub".to_owned(),
        ),
        None => (
            format!("the push is {size} bytes, more than GitHub takes ({limit}): nothing was sent"),
            "push an older state first with git, then the rest".to_owned(),
        ),
    };
    let mut found = details([
        ("scope", scope.as_str().into()),
        ("size", size.into()),
        ("limit", limit.into()),
        ("remote", remote.into()),
    ]);
    if let Some(path) = path {
        found.insert("path".to_owned(), path.into());
    }
    error(ErrorCode::TooLarge, message)
        .with_details(found)
        .with_hint(next)
}

/// Puts `text` into `found` as `key`, when there is any.
fn put_text(found: &mut Map<String, Value>, key: &str, text: String) {
    if !text.is_empty() {
        found.insert(key.to_owned(), text.into());
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
        ChangesetId::new(ObjectId::new([7; 32]))
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
        let missing_blob = RepoError::MissingBlob {
            path: path(),
            id: swarmops_core::BlobId::new(id().0),
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
            recovered: Box::default(),
        };
        maps(not_found, op, ErrorCode::OpNotFound);
        let nothing = RepoError::NothingToUndo {
            op: 2,
            recovered: Box::default(),
        };
        maps(nothing, op, ErrorCode::NothingToUndo);
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
        for e in land_errors() {
            let code = match e {
                RepoError::Conflict(_) => ErrorCode::Conflict,
                RepoError::NothingToLand(_) => ErrorCode::NothingToLand,
                RepoError::RetryExhausted { .. } => ErrorCode::CasRetryExhausted,
                _ => ErrorCode::InvalidArg,
            };
            maps(e, op, code);
        }
    }

    /// A derived value that a reader's checks find wrong is a store that fails its checks:
    /// `E_STORE_CORRUPT`, naming the derivation and the changeset, its hint a deep verify, which
    /// names every such value.
    #[test]
    fn a_derived_value_a_reader_refuses_is_store_corrupt() {
        let wrong = || RepoError::DerivedCorrupt {
            name: "last_modified".into(),
            changeset: id(),
            why: "it names a changeset whose delta does not touch the path".into(),
        };
        maps(wrong(), During::Op, ErrorCode::StoreCorrupt);
        let refused = repo_error(wrong(), During::Op);
        assert_eq!(refused.hint.as_deref(), Some("swarmops verify --deep"));
        assert_eq!(
            Value::Object(refused.details),
            serde_json::json!({"derivation": "last_modified", "changeset": id().to_string()})
        );
    }

    /// One of each error a land raises.
    fn land_errors() -> [RepoError; 8] {
        use swarmops_repo::{MergeConflicts, NothingToLandReason};
        let conflicts = MergeConflicts::new(
            id(),
            id(),
            id(),
            Vec::new(),
            Vec::new(),
            std::collections::BTreeMap::new(),
            Vec::new(),
        );
        [
            RepoError::Conflict(Box::new(conflicts)),
            RepoError::NothingToLand(NothingToLandReason::OnTarget),
            RepoError::NothingToLand(NothingToLandReason::NoChanges),
            RepoError::RetryExhausted {
                attempts: 8,
                actual: id(),
            },
            RepoError::KeyReused {
                key: "k-1".into(),
                landed: id(),
            },
            RepoError::MergeChangeset(id()),
            RepoError::BaseNotAncestor {
                base: id(),
                head: id(),
            },
            RepoError::NoMergeBase {
                head: id(),
                theirs: id(),
            },
        ]
    }

    /// A land's errors carry what they are about: the conflict report (exit 3, not retryable,
    /// with a hint that reads the target's side and names the land with its resolutions), the
    /// reason there is nothing to land, the attempts made and what the target names after the last
    /// (retryable, exit 4, with a hint to land again), the key and the changeset it landed, the
    /// merge changeset, the base and the head.
    #[test]
    fn land_errors_carry_their_details() {
        let full = id().to_string();
        let report = serde_json::json!({"base": full, "ours": full, "theirs": full, "files": [],
                                        "kinds": [], "truncated": false});
        let want = [
            ("E_CONFLICT", serde_json::json!({ "report": report })),
            (
                "E_NOTHING_TO_LAND",
                serde_json::json!({"reason": "on_target"}),
            ),
            (
                "E_NOTHING_TO_LAND",
                serde_json::json!({"reason": "no_changes"}),
            ),
            (
                "E_CAS_RETRY_EXHAUSTED",
                serde_json::json!({"attempts": 8, "actual": full}),
            ),
            (
                "E_INVALID_ARG",
                serde_json::json!({"key": "k-1", "landed": full}),
            ),
            ("E_INVALID_ARG", serde_json::json!({"changeset": full})),
            (
                "E_INVALID_ARG",
                serde_json::json!({"base": full, "head": full}),
            ),
            (
                "E_INVALID_ARG",
                serde_json::json!({"head": full, "theirs": full}),
            ),
        ];
        for (e, (code, details)) in land_errors().into_iter().zip(want) {
            let error = repo_error(e, During::Op);
            assert_eq!(error.code.as_str(), code, "{error:?}");
            assert_eq!(Value::Object(error.details), details, "{code}");
        }
        let [conflict, ..] = land_errors();
        let conflict = repo_error(conflict, During::Op);
        assert_eq!((conflict.exit, conflict.retryable), (3, false));
        assert_eq!(
            conflict.hint,
            Some(format!(
                "swarmops cat {full}:<path>, then swarmops land {full} --resolve <file> with each \
                 path's resolution and against = {full}"
            ))
        );
        let exhausted = repo_error(
            RepoError::RetryExhausted {
                attempts: 3,
                actual: id(),
            },
            During::Op,
        );
        assert_eq!((exhausted.exit, exhausted.retryable), (4, true));
        let hint = exhausted.hint.unwrap_or_default();
        assert!(hint.contains("land again"), "{hint}");
        let reused = repo_error(
            RepoError::KeyReused {
                key: "k-1".into(),
                landed: id(),
            },
            During::Op,
        );
        assert_eq!(reused.hint, Some(format!("swarmops show {full}")));
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

    /// A land whose target no longer names its `expect_target` (M5) maps as a commit whose ref
    /// moved does: one code, `E_CAS_MISMATCH` (exit 4, retryable), `committed: false`, the same
    /// read-only hint; its details add what the land expected.
    #[test]
    fn cas_mismatch_and_ref_moved_map_to_one_code() {
        let other = ChangesetId::new(swarmops_core::ObjectId::new([9; 32]));

        // 1. Both errors, one code.
        let moved = repo_error(
            RepoError::RefMoved {
                name: "main".into(),
                actual: Some(id()),
                recovered: Box::default(),
            },
            During::Op,
        );
        let mismatch = repo_error(
            RepoError::CasMismatch {
                target: "main".into(),
                actual: id(),
                expected: other,
                recovered: Box::default(),
            },
            During::Op,
        );
        for error in [&moved, &mismatch] {
            assert_eq!(error.code, ErrorCode::CasMismatch.as_str(), "{error:?}");
            assert_eq!((error.exit, error.retryable), (4, true), "{error:?}");
            assert_eq!(error.hint, Some(format!("swarmops show {}", id())));
        }

        // 2. The mismatch's details, and a message that says nothing landed.
        let want = details([
            ("name", "main".into()),
            ("actual", id().to_string().into()),
            ("expected", other.to_string().into()),
            ("committed", false.into()),
        ]);
        assert_eq!(mismatch.details, want);
        assert!(mismatch.message.contains("landed nothing"), "{mismatch:?}");
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

        let not_found = RepoError::OpNotFound {
            op: 9,
            last: None,
            recovered: Box::default(),
        };
        let not_found = repo_error(not_found, During::Op);
        assert_eq!(
            (not_found.code.as_str(), not_found.exit),
            ("E_OP_NOT_FOUND", 6)
        );
        assert_eq!(
            not_found.details,
            details([("op", 9.into()), ("last", Value::Null)])
        );
        assert_eq!(not_found.hint.as_deref(), Some("swarmops op log"));
        let nothing = RepoError::NothingToUndo {
            op: 3,
            recovered: Box::default(),
        };
        let nothing = repo_error(nothing, During::Op);
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
            store(StoreError::RefProtected("main".into())),
            op,
            ErrorCode::RefProtected,
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
    const COMMIT: GitOid = GitOid::new([0x11; 20]);
    const OBJECT: GitOid = GitOid::new([0x22; 20]);

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
        let special = unsupported(Unsupported::SpecialFile("refs/heads/main".into()));
        let want = json!({"reason": "special_file", "file": "refs/heads/main"});
        let error = git_maps(special, ErrorCode::GitUnsupported, &want);
        assert_eq!(error.hint, None);
        let long = unsupported(Unsupported::FileTooLong {
            file: "config".into(),
            len: 1 << 40,
            max: 16 << 20,
        });
        let want = json!({"reason": "file_too_long", "file": "config", "size": 1_u64 << 40,
                          "max": 16 << 20});
        git_maps(long, ErrorCode::GitUnsupported, &want);

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
            // A commit, a tree or a tag past the object limit, refused by its header unread: the
            // request is valid, so it is the limit's code, never `E_INVALID_ARG`.
            for kind in [RawKind::Commit, RawKind::Tree, RawKind::Tag] {
                let large = read(SourceError::TooLarge {
                    oid: OBJECT,
                    kind,
                    size: MAX_OBJECT_LEN + 1,
                });
                let want = json!({
                    "object": object,
                    "kind": kind.as_str(),
                    "size": MAX_OBJECT_LEN + 1,
                    "max": MAX_OBJECT_LEN,
                });
                let error = git_maps(large, ErrorCode::TooLarge, &want);
                assert_eq!(error.hint, None);
            }
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
    /// the git commit that head holds, if any (exit 3, the hint its log), and which names the
    /// external id entry the walk passed over, if any (the hint then a deep verify); and one that
    /// moved during the import, `E_CAS_MISMATCH` with a hint that only reads.
    #[test]
    fn git_import_target_errors_map_to_their_codes() {
        use serde_json::json;

        let bad = ImportError::RefName("feature/x".into());
        git_maps(
            bad,
            ErrorCode::RefNameInvalid,
            &json!({"name": "feature/x"}),
        );
        let head = ChangesetId::new(ObjectId::new([7; 32]));
        let diverged = |base| ImportError::Diverged {
            target_ref: "main".into(),
            target_head: head,
            base,
            git_head: COMMIT,
            external_id: None,
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
        let entry = swarmops_git::ExternalIdMismatch::new(OBJECT, head);
        let passed_over = ImportError::Diverged {
            target_ref: "main".into(),
            target_head: head,
            base: Some(COMMIT),
            git_head: COMMIT,
            external_id: Some(Box::new(entry)),
        };
        let want = json!({
            "target_ref": "main",
            "target_head": head.to_string(),
            "base": COMMIT.to_string(),
            "git_head": COMMIT.to_string(),
            "external_id": {"commit": OBJECT.to_string(), "changeset": head.to_string()},
        });
        let error = git_maps(passed_over, ErrorCode::GitImportDiverged, &want);
        assert_eq!(error.hint.as_deref(), Some("swarmops verify --deep"));
        let actual = ChangesetId::new(ObjectId::new([8; 32]));
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
    /// details, and hinting to run it again, but where it found the store damaged, and only after
    /// what to fix where the filesystem refused a journal append for good (ruling M16-R8); a frame
    /// that committed and failed after its commit point is `E_IO` with `committed: true` and its
    /// warnings.
    #[test]
    fn git_import_store_failures_say_how_far_it_got() {
        use serde_json::json;

        let commit = COMMIT.to_string();
        let stopped = |e: StoreError, imported, last| ImportError::Store {
            source: Box::new(RepoError::Store(e)),
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
        let refused = StoreError::NotCommitted {
            source: IoFailure::new(
                FileOp::Fs(FsMethod::AppendDurable),
                Some("meta/journal.log".into()),
                io::Error::from_raw_os_error(13),
            ),
        };
        let error = git_maps(stopped(refused, 0, None), ErrorCode::Io, &want);
        let fix_first =
            format!("make the store's files writable for this user, then {RESUME_HINT}");
        assert_eq!(error.hint.as_deref(), Some(fix_first.as_str()));
        let damaged = StoreError::Corrupt {
            id: ObjectId::new([9; 32]),
        };
        let want = json!({
            "object": ObjectId::new([9; 32]).to_hex(),
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

    /// `e`, a `git export` of `main` into the branch `main` of [`GIT_PATH`] that failed, maps to
    /// `code`: its string, exit code and retry advice, and `details` as `want`; the error.
    #[track_caller]
    fn export_maps(e: ExportError, code: ErrorCode, want: &Value) -> OpError {
        let shown = format!("{e:?}");
        let error = git_export_error(e, GIT_PATH, "main", "main");
        assert_eq!(error.code, code.as_str(), "{shown}");
        let advice = (error.exit, error.retryable);
        assert_eq!(advice, (code.exit(), code.retryable()), "{shown}");
        assert_eq!(&Value::Object(error.details.clone()), want, "{shown}");
        assert!(!error.message.is_empty(), "{shown}");
        error
    }

    /// Each reason a branch diverged is `E_GIT_EXPORT_DIVERGED`, exit 3: its details name the
    /// reason, the branch, the commit it names, the source's head and, where the branch holds the
    /// export of a changeset, that changeset; each hint names the next step the reason calls for,
    /// and `--force` but for a branch moved during the export, which is run again. A branch
    /// deleted during the export names no commit: null, and no forty zeros in the message.
    #[test]
    fn git_export_divergences_map_to_their_reasons() {
        use serde_json::json;

        let head = ChangesetId::new(ObjectId::new([7; 32]));
        let exported = ChangesetId::new(ObjectId::new([8; 32]));
        let diverged = |git_head, reason| ExportError::Diverged {
            branch: "main".into(),
            git_head,
            head,
            reason,
        };
        let want = |reason: &str, exported: Option<ChangesetId>| {
            let mut want = json!({
                "reason": reason,
                "branch": "main",
                "git_head": COMMIT.to_string(),
                "head_changeset": head.to_string(),
            });
            if let Some(exported) = exported {
                want["exported_changeset"] = exported.to_string().into();
            }
            want
        };
        let log = "git -C /nonexistent/swarmops-test/repo.git log refs/heads/main";
        let import = "swarmops git import /nonexistent/swarmops-test/repo.git --ref refs/heads/main \
                      --into main";
        let cases = [
            (
                DivergedReason::BranchAhead { exported },
                Some(exported),
                import,
            ),
            (DivergedReason::BothMoved { exported }, Some(exported), log),
            (DivergedReason::BranchMoved, None, log),
            (
                DivergedReason::SourceMovedBack { exported },
                Some(exported),
                "swarmops op log",
            ),
            (
                DivergedReason::LossyHistory {
                    changeset: exported,
                },
                None,
                "--branch <name>",
            ),
        ];
        let code = ErrorCode::GitExportDiverged;
        for (reason, holds, next) in cases {
            let want = want(reason.as_str(), holds);
            let error = export_maps(diverged(COMMIT, reason), code, &want);
            assert_eq!(error.exit, 3);
            let hint = error.hint.unwrap_or_default();
            assert!(hint.contains(next) && hint.contains("--force"), "{hint}");
        }
        let during = |actual| DivergedReason::MovedDuringExport { actual };
        let moved = diverged(COMMIT, during(Some(COMMIT)));
        let error = export_maps(moved, code, &want("moved_during_export", None));
        assert_eq!(error.hint.as_deref(), Some("run the same git export again"));
        let mut gone = want("moved_during_export", None);
        gone["git_head"] = Value::Null;
        let deleted = export_maps(diverged(GitOid::NULL, during(None)), code, &gone);
        assert!(deleted.message.contains("deleted"), "{deleted:?}");
        let zeros = GitOid::NULL.to_string();
        assert!(!deleted.message.contains(&zeros), "{deleted:?}");
    }

    /// What a `git export` request names, refused: a source ref the store does not have
    /// (`E_REV_NOT_FOUND`, whose hint lists the refs) or that is no ref name, and a branch a work
    /// tree has checked out (`E_INVALID_ARG`, whose hint detaches `HEAD`); a bug is
    /// `E_INTERNAL`, and a store failure the code the operations give it.
    #[test]
    fn git_export_request_errors_map_to_their_codes() {
        use serde_json::json;

        let missing = ExportError::SourceRef("release/none".into());
        let want = json!({"rev": "release/none"});
        let error = export_maps(missing, ErrorCode::RevNotFound, &want);
        assert_eq!(error.hint.as_deref(), Some("swarmops ref list"));
        let bad = ExportError::RefName("feature/x".into());
        let want = json!({"name": "feature/x"});
        export_maps(bad, ErrorCode::RefNameInvalid, &want);
        let checked_out = ExportError::CheckedOut {
            branch: "main".into(),
        };
        let want = json!({"reason": "checked_out", "branch": "main"});
        let error = export_maps(checked_out, ErrorCode::InvalidArg, &want);
        let hint = error.hint.unwrap_or_default();
        let detach = "git -C /nonexistent/swarmops-test/repo.git switch --detach";
        assert!(hint.contains(detach), "{hint}");
        let bug = ExportError::Bug {
            reason: "a rendering hashed to another oid".into(),
        };
        export_maps(bug, ErrorCode::Internal, &json!({}));
        let busy = ExportError::Repo(RepoError::Store(StoreError::Busy));
        export_maps(busy, ErrorCode::Busy, &json!({}));
    }

    /// The repository's refusals and failures: no repository (the hint makes one), one the
    /// export does not write as it is, a branch name git does not take (the hint is git's own
    /// check), a held lock and a ref in the branch's way (`E_GIT_WRITE`, each with what to do), a
    /// write that failed, and an object missing or damaged, or a read that failed (`E_GIT_READ`).
    #[test]
    fn git_export_repository_errors_map_to_their_codes() {
        use serde_json::json;

        let sink = ExportError::Sink;
        let not_a_repo = sink(SinkError::NotARepo {
            reason: "no HEAD".into(),
        });
        let want = json!({"git_path": GIT_PATH});
        let error = export_maps(not_a_repo, ErrorCode::GitNotARepo, &want);
        let make = "git init --bare /nonexistent/swarmops-test/repo.git makes an empty repository \
                    to export to";
        assert_eq!(error.hint.as_deref(), Some(make));
        let shallow = sink(SinkError::Unsupported(Unsupported::Shallow));
        export_maps(
            shallow,
            ErrorCode::GitUnsupported,
            &json!({"reason": "shallow"}),
        );
        let sha256 = Unsupported::ObjectFormat("sha256".into());
        let want = json!({"reason": "object_format", "object_format": "sha256"});
        export_maps(
            sink(SinkError::Unsupported(sha256)),
            ErrorCode::GitUnsupported,
            &want,
        );
        let special = Unsupported::SpecialFile("config".into());
        let want = json!({"reason": "special_file", "file": "config"});
        export_maps(
            sink(SinkError::Unsupported(special)),
            ErrorCode::GitUnsupported,
            &want,
        );
        let long = Unsupported::FileTooLong {
            file: "HEAD".into(),
            len: 1 << 40,
            max: 16 << 20,
        };
        let want = json!({"reason": "file_too_long", "file": "HEAD", "size": 1_u64 << 40,
                          "max": 16 << 20});
        export_maps(
            sink(SinkError::Unsupported(long)),
            ErrorCode::GitUnsupported,
            &want,
        );
        let bad = sink(SinkError::BadBranch {
            name: "a..b".into(),
        });
        let error = export_maps(bad, ErrorCode::RefNameInvalid, &json!({"name": "a..b"}));
        let check = "git check-ref-format --branch a..b says why git takes no such branch";
        assert_eq!(error.hint.as_deref(), Some(check));
        let locked = sink(SinkError::RefLocked {
            lock: "refs/heads/main.lock".into(),
        });
        let want = json!({"reason": "ref_locked", "path": "refs/heads/main.lock"});
        let error = export_maps(locked, ErrorCode::GitWrite, &want);
        let hint = error.hint.unwrap_or_default();
        assert!(hint.contains("remove refs/heads/main.lock"), "{hint}");
        let conflict = sink(SinkError::RefConflict {
            name: "refs/heads/main/x".into(),
        });
        let want = json!({"reason": "ref_name_conflict", "branch": "main"});
        export_maps(conflict, ErrorCode::GitWrite, &want);
        let full = sink(SinkError::Write {
            what: "objects/ab".into(),
            reason: "No space left on device".into(),
        });
        let want = json!({"reason": "io", "path": "objects/ab"});
        export_maps(full, ErrorCode::GitWrite, &want);

        let object = OBJECT.to_string();
        let missing = sink(SinkError::Missing { oid: OBJECT });
        let want = json!({"reason": "missing", "object": object});
        let error = export_maps(missing, ErrorCode::GitRead, &want);
        let fsck = "git -C /nonexistent/swarmops-test/repo.git fsck";
        assert_eq!(error.hint.as_deref(), Some(fsck));
        let corrupt = sink(SinkError::Corrupt {
            oid: OBJECT,
            reason: "it is empty".into(),
        });
        let want = json!({"reason": "corrupt", "object": object});
        export_maps(corrupt, ErrorCode::GitRead, &want);
        let unread = sink(SinkError::Read {
            what: "packed-refs".into(),
            reason: "Input/output error".into(),
        });
        export_maps(unread, ErrorCode::GitRead, &json!({"reason": "io"}));
    }

    // ---- The git remotes (M13) ----

    /// `error` has `code`'s string, exit code and retry advice, and `want` as its details.
    #[track_caller]
    fn is(error: &OpError, code: ErrorCode, want: &Value) {
        assert_eq!(error.code, code.as_str(), "{error:?}");
        let advice = (error.exit, error.retryable);
        assert_eq!(advice, (code.exit(), code.retryable()), "{error:?}");
        assert_eq!(&Value::Object(error.details.clone()), want, "{error:?}");
        assert!(!error.message.is_empty() && !error.message.contains('\n'));
    }

    /// Every failure of the transport maps to its code (the M13 plan, §4.3 and §3.5): the four
    /// remote codes, and `E_GIT_UNSUPPORTED` for a git that is missing or too old and for a
    /// SHA-256 remote; an empty stderr tail is left out.
    #[test]
    fn transport_errors_map_to_the_remote_codes() {
        use serde_json::json;
        use swarmops_git::transport::{Cause, RejectReason};

        // 1. git itself.
        let missing = transport_error(TransportError::GitMissing, "origin", Phase::Fetch);
        is(
            &missing,
            ErrorCode::GitUnsupported,
            &json!({"reason": "git_missing"}),
        );
        let old = TransportError::GitTooOld {
            found: "2.30.1".into(),
        };
        let old = transport_error(old, "origin", Phase::Fetch);
        let want = json!({"reason": "git_too_old", "found": "2.30.1", "needs": "2.39"});
        is(&old, ErrorCode::GitUnsupported, &want);
        let sha256 = transport_error(TransportError::Sha256, "origin", Phase::Fetch);
        is(
            &sha256,
            ErrorCode::GitUnsupported,
            &json!({"reason": "sha256"}),
        );

        // 2. The network: retryable, with the phase and the cause.
        let down = TransportError::Unreachable {
            cause: Cause::Connect,
            stderr_tail: "fatal: unable to access".into(),
        };
        let down = transport_error(down, "origin", Phase::LsRemote);
        let want = json!({"remote": "origin", "phase": "ls_remote", "cause": "connect",
                          "stderr_tail": "fatal: unable to access"});
        is(&down, ErrorCode::RemoteUnreachable, &want);
        assert!(down.retryable);
        let quiet = TransportError::Unreachable {
            cause: Cause::Timeout,
            stderr_tail: String::new(),
        };
        let quiet = transport_error(quiet, "origin", Phase::Push);
        let want = json!({"remote": "origin", "phase": "push", "cause": "timeout"});
        is(&quiet, ErrorCode::RemoteUnreachable, &want);

        // 3. A refusal: exit 5, not retryable, with what git said.
        let declined = TransportError::Rejected {
            reason: RejectReason::Declined,
            refs: vec![("refs/heads/main".into(), "pre-receive hook declined".into())],
            remote_messages: vec!["no pushes on Fridays".into()],
            git_exit: Some(1),
            stderr_tail: String::new(),
            token_env_unset: true,
        };
        let declined = transport_error(declined, "origin", Phase::Push);
        let refs = json!([{"ref": "refs/heads/main", "message": "pre-receive hook declined"}]);
        let want = json!({"remote": "origin", "reason": "declined", "refs": refs,
                          "remote_messages": ["no pushes on Fridays"], "git_exit": 1,
                          "token_env_unset": true});
        is(&declined, ErrorCode::RemoteRejected, &want);
        for reason in [
            RejectReason::Auth,
            RejectReason::NotFound,
            RejectReason::Unclassified,
            RejectReason::BadObjects,
        ] {
            let refused = TransportError::Rejected {
                reason,
                refs: Vec::new(),
                remote_messages: Vec::new(),
                git_exit: None,
                stderr_tail: String::new(),
                token_env_unset: false,
            };
            let refused = transport_error(refused, "origin", Phase::Fetch);
            let want = json!({"remote": "origin", "reason": reason.as_str()});
            is(&refused, ErrorCode::RemoteRejected, &want);
            assert!(!refused.retryable && refused.hint.is_some());
        }

        // 4. A remote that moved: exit 3, with the fetch or the sync to run.
        let stale = TransportError::Stale {
            branch: "main".into(),
        };
        let stale = transport_error(stale, "origin", Phase::Push);
        let want = json!({"remote": "origin", "branch": "main", "reason": "remote_moved"});
        is(&stale, ErrorCode::RemoteDiverged, &want);
        assert_eq!(stale.hint.as_deref(), Some("swarmops fetch origin"));
        let behind = TransportError::NotFastForward {
            branch: "main".into(),
        };
        let behind = transport_error(behind, "origin", Phase::Push);
        let want = json!({"remote": "origin", "branch": "main", "reason": "not_fast_forward"});
        is(&behind, ErrorCode::RemoteDiverged, &want);
        assert_eq!(behind.hint.as_deref(), Some("swarmops sync origin"));
    }

    /// The mirror's lock and git's lock files in it are `E_BUSY`, retryable, and say which
    /// lock: the export's `RefLocked` in the mirror too, which no person is told to remove.
    #[test]
    fn mirror_errors_map_to_their_codes() {
        use serde_json::json;

        let held = mirror_error(MirrorError::Busy);
        is(&held, ErrorCode::Busy, &json!({"lock": "git_mirror"}));
        let file = "refs/remotes/origin/main.lock";
        let young = mirror_error(MirrorError::GitLockBusy { file: file.into() });
        let want = json!({"lock": "git_mirror_file", "file": file});
        is(&young, ErrorCode::Busy, &want);
        let lock = "refs/heads/push/origin/main.lock";
        let staged = mirror_error(MirrorError::Sink(SinkError::RefLocked {
            lock: lock.into(),
        }));
        let want = json!({"lock": "git_mirror_file", "file": lock});
        is(&staged, ErrorCode::Busy, &want);
        assert!(!staged.hint.unwrap_or_default().contains("remove"));

        let io = mirror_error(MirrorError::Io(io::Error::other("disk full")));
        is(&io, ErrorCode::Io, &json!({"store_file": "git"}));
        let written = MirrorError::Sink(SinkError::BadBranch { name: "x".into() });
        let written = mirror_error(written);
        is(&written, ErrorCode::GitWrite, &json!({"reason": "mirror"}));
    }

    /// The sync engine's failures: no common base is `E_REMOTE_DIVERGED`, a conflict is a
    /// land's `E_CONFLICT` with the change and what it landed onto, and a merge changeset is
    /// refused as a land refuses one.
    #[test]
    fn sync_errors_map_to_their_codes() {
        use serde_json::json;
        use swarmops_repo::MergeConflicts;

        let unrelated = sync_error(SyncError::NoCommonBase, "origin", "main");
        let want = json!({"remote": "origin", "branch": "main", "reason": "no_common_base"});
        is(&unrelated, ErrorCode::RemoteDiverged, &want);

        let conflicts = MergeConflicts::new(
            id(),
            id(),
            id(),
            Vec::new(),
            Vec::new(),
            std::collections::BTreeMap::new(),
            Vec::new(),
        );
        let conflicted = SyncError::Conflict {
            change: id(),
            onto: id(),
            conflicts: Box::new(conflicts),
        };
        let conflicted = sync_error(conflicted, "origin", "main");
        assert_eq!(conflicted.code, "E_CONFLICT");
        assert_eq!(conflicted.details["change"], id().to_string());
        assert_eq!(conflicted.details["onto"], id().to_string());
        assert!(conflicted.details.contains_key("report"));

        let merge = sync_error(SyncError::MergeChangeset(id()), "origin", "main");
        is(
            &merge,
            ErrorCode::InvalidArg,
            &json!({"changeset": id().to_string()}),
        );
        let repo = sync_error(
            SyncError::Repo(RepoError::NotFound("main".into())),
            "o",
            "m",
        );
        assert_eq!(repo.code, "E_REV_NOT_FOUND");
    }

    /// GitHub's limits are `E_TOO_LARGE` with the scope, the sizes and the remote, and the
    /// file's path when a file broke them; the URL rules are `E_INVALID_ARG` with the reason and
    /// never the URL; an unknown remote is `E_REMOTE_NOT_FOUND`; a stub names its task.
    #[test]
    fn limits_urls_and_names_map_to_their_codes() {
        use serde_json::json;
        use swarmops_git::limits::LimitScope;

        let file = LimitExceeded::new(
            LimitScope::RemoteFile,
            Some("assets/big.bin".into()),
            104_857_601,
            104_857_600,
        );
        let file = limit_error(file, "origin");
        let want = json!({"scope": "remote_file", "path": "assets/big.bin", "size": 104_857_601_u64,
                          "limit": 104_857_600_u64, "remote": "origin"});
        is(&file, ErrorCode::TooLarge, &want);
        let push = LimitExceeded::new(LimitScope::RemotePush, None, 2_000_000_001, 2_000_000_000);
        let push = limit_error(push, "origin");
        let want = json!({"scope": "remote_push", "size": 2_000_000_001_u64,
                          "limit": 2_000_000_000_u64, "remote": "origin"});
        is(&push, ErrorCode::TooLarge, &want);

        for (why, reason) in [
            (UrlRefused::CleartextHttp, "cleartext_http"),
            (UrlRefused::Scheme, "url_scheme"),
            (UrlRefused::CredentialInUrl, "credential_in_url"),
            (UrlRefused::OptionLike, "url_option"),
        ] {
            is(
                &url_refused(why),
                ErrorCode::InvalidArg,
                &json!({"reason": reason}),
            );
        }

        let unknown = remote_not_found("upstream");
        is(
            &unknown,
            ErrorCode::RemoteNotFound,
            &json!({"remote": "upstream"}),
        );
        let stub = planned("C6");
        is(&stub, ErrorCode::Internal, &json!({}));
        assert_eq!(stub.message, "planned in M13 (C6)");
    }

    /// The repository's and the store's internal errors, the values of their callers' wildcard
    /// arms, are `E_INTERNAL` with their message and no hint; a projection's unknown variant is
    /// `E_INTERNAL` naming its enum.
    #[test]
    fn internal_errors_map_to_e_internal() {
        let what = "EntryTarget: a variant this build does not know";
        let internal = [
            RepoError::Internal(what.to_owned()),
            RepoError::Store(StoreError::Internal(what.to_owned())),
        ];
        for e in internal {
            let error = op(e);
            assert_eq!(error.code, ErrorCode::Internal.as_str(), "{error:?}");
            assert!(error.message.contains(what), "{error:?}");
            assert_eq!(error.hint, None, "{error:?}");
        }

        let error = unknown_variant("LandStatus");
        assert_eq!(error.code, ErrorCode::Internal.as_str());
        assert_eq!(
            error.message,
            "LandStatus: a variant this build does not know"
        );
        assert!(!error.retryable);
    }
}
