//! [`LocalOps`]: every [`StoreOps`] operation, run in this process against a store on the
//! local filesystem through `swarmops-repo` (operations spec §1, §7). Its documentation says
//! what each operation does and how failures map to error codes.

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::sync::{Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use camino::{Utf8Path, Utf8PathBuf};
use serde_json::{Map, Value};
use swarmops_api::{
    Attribution, ChangeOp, ChangeOut, ChangesetDetail, ChangesetSummary, CommitReq, CommitResp,
    EntryKindOut, ErrorCode, FileContent, FileKind, Identity, InfoReq, LogPage, LogReq, OpError,
    ReadFileReq, ReadTreeReq, RefDeleteReq, RefListReq, RefListResp, RefOut, RefSetReq, RefSetResp,
    RepoInfo, Rev, ShowReq, SignatureOut, Signatures, StoreOps, TreeEntryOut, TreeListing,
    VerifyReport, VerifyReq,
};
use swarmops_core::{
    ChangesetId, DeltaEntry, DeltaOp, EntryKind, ObjectId, ObjectKind, RepoPath, Signature,
    TreeEntry, hash_object,
};
use swarmops_repo::{CasResult, CommitRequest, LocalRepo, RepoError, StoreError, StoredChangeset};
use swarmops_store::Layout;

use crate::content;

/// The agent a changeset is attributed to when its metadata names none.
const UNKNOWN_AGENT: &str = "unknown/unknown";

/// The operations over a store on the local filesystem: [`StoreOps`], run in this process with
/// the repository library.
///
/// # Revisions
///
/// Each revision a request names is resolved once, by the repository's grammar (`main`,
/// `release/<name>`, a full changeset id or a unique prefix of 12 or more of its hex digits,
/// then any `~N` suffixes), and the operation works on the changeset it named. What each
/// revision named is kept for [`LocalOps::take_resolved`], which the CLI reports as
/// `meta.resolved`; so is `main` when an operation used it by default.
///
/// # Operations
///
/// - `commit` builds on `parent`, or, when it is omitted, on `main`'s head if `main` exists and
///   on nothing otherwise (a root changeset). The changeset is the caller's: author and
///   committer are both the agent name with the email `<instance>@<family>.swarmops.local` at
///   the current time, UTC; its metadata holds `agent` and, when the caller has one, `run_id`.
///   `files_changed` counts the changeset's delta, which leaves out puts of what a path holds
///   already. The change list follows [`crate::content`]'s rules.
/// - `read_file` returns at most `max_bytes` bytes from the file's start, cut in the raw bytes
///   and then encoded: UTF-8 text when they are UTF-8 (a cut through a character leaves bytes
///   that are not), base64 otherwise. A directory there is `E_IS_DIR`, nothing there
///   `E_PATH_NOT_FOUND`.
/// - `read_tree` lists a directory (the root for an omitted or empty path) in name order, a page
///   of at most `limit` entries after the `cursor` name; a file or nothing at the path is
///   `E_PATH_NOT_FOUND`.
/// - `show` attributes the changeset by its `agent` and `run_id` metadata (`unknown/unknown`
///   when it records no agent); `created_at` is the committer's time in milliseconds (0 before
///   1970); a metadata value that is not UTF-8 is given in base64 under its key with `:b64`
///   appended; `changes` holds at most `max_changes` entries.
/// - `log` follows first parents from `rev` (`main` when omitted) or, given a `cursor`, from the
///   first parent of the changeset it names, which must be a full id the store holds; a page
///   is `truncated` when older history follows its last entry.
/// - `ref_set` and `ref_delete` move a ref by compare-and-swap, recorded for the caller's agent.
///   A `ref_set` that finds the ref at its `new` target already is a replay: `already_applied`,
///   with the response the move gave. A `ref_delete` of a ref that does not exist is
///   `E_REF_NOT_FOUND`.
/// - `verify` checks the store and lists each problem as one line.
///
/// Limits of 0 (`limit`, `max_changes`) are `E_INVALID_ARG`.
///
/// # Errors
///
/// Every error's message is one line (control characters in it are escaped), and its hint,
/// where there is an obvious next step, is a command. Repository failures map to codes as
/// follows, `details` keys in braces:
///
/// - `E_REV_NOT_FOUND` `{rev}`: `NotFound`, and the store's `UnknownRefTarget`. The hint is
///   `swarmops ref list` for a ref name, `swarmops log` for an id or a `~N` walk.
/// - `E_AMBIGUOUS_ID` `{rev}`: `Ambiguous`; the hint says to give more digits.
/// - `E_INVALID_ARG`: `BadRev` `{rev}` (the hint gives the revision grammar), `DirPut` and
///   `DuplicatePath` `{path}`, and the store's `DuplicateParent` `{parent}`.
/// - `E_EXISTS`: `Exists`; the hint is `swarmops --repo <path> info`.
/// - `E_UNKNOWN_PARENT` `{parent}`: `UnknownParent`; the hint is `swarmops log`.
/// - `E_EMPTY_COMMIT`: `EmptyChanges`; the hint names `--allow-empty`.
/// - `E_TYPE_CONFLICT` `{path}`: `TypeConflict`.
/// - `E_PATH_NOT_FOUND` `{path}`: `PathNotFound`.
/// - `E_REF_NAME_INVALID` `{name}`: `RefName`, the repository's or the store's; the hint gives
///   the ref grammar.
/// - `E_BUSY` (retryable): the store's `Busy`.
/// - `E_NOT_A_REPO`, while opening: the store's `Format`, and an `Io` error of kind
///   `NotADirectory`. The hint is `swarmops init <path>` when the path holds no `FORMAT` file;
///   with one, the store is damaged or of a layout this build does not read.
/// - `E_STORE_CORRUPT`: `MissingObject`, and the store's `Corrupt` and `BadHeader`
///   `{object}`; `Codec`, and the store's `Codec`, `JournalCorrupt`, `IndexCorrupt`,
///   `IndexMismatch` and `MissingParent`; an `Io` error of kind `InvalidData` (a malformed
///   metadata file); and the store's `Format` other than while opening. Once the store is open,
///   the hint is `swarmops verify` (with `--deep` for an object whose bytes fail their id).
/// - `E_UNSUPPORTED_FS` `{path}`: the store's `UnsupportedFs`, a path under `/mnt/`; the hint
///   points at a native filesystem.
/// - `E_IO`: any other `Io` error.
/// - `E_INTERNAL`: the store's `PostCommit`, which the repository turns into a warning, so
///   that it never reaches here.
///
/// The operations raise their own errors besides: `E_IS_DIR` and `E_PATH_NOT_FOUND`
/// `{rev, path}` for reads; `E_CAS_MISMATCH` `{name, actual}` (`actual` null when the ref does
/// not exist), whose hint is the move that would succeed now; `E_REF_NOT_FOUND` `{name}`; and
/// `E_INVALID_ARG` for limits, cursors and requests that break their own rules.
pub struct LocalOps {
    repo: LocalRepo,
    /// The store's path as the caller gave it, for hints and display.
    store_path: Utf8PathBuf,
    /// What each revision resolved since the last [`LocalOps::take_resolved`] named.
    resolved: Mutex<BTreeMap<String, String>>,
}

impl fmt::Debug for LocalOps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalOps")
            .field("store_path", &self.store_path)
            .finish_non_exhaustive()
    }
}

/// What was being done when a repository error arose: a few errors mean something else while
/// a store is created or opened.
#[derive(Debug, Clone, Copy)]
enum During<'a> {
    /// `init` of a store at this path.
    Init(&'a Utf8Path),
    /// `open` of the store at this path.
    Open(&'a Utf8Path),
    /// An operation on an open store.
    Op,
}

impl LocalOps {
    /// Creates a store at `path` for a repository named `name`, creating the directory if it is
    /// missing, and opens it (CLI `init`). `E_EXISTS` if a store is there already,
    /// `E_UNSUPPORTED_FS` for a path under `/mnt/`.
    pub fn init(path: &Utf8Path, name: &str) -> Result<(LocalOps, RepoInfo), OpError> {
        let repo = LocalRepo::create(path, name).map_err(|e| repo_error(e, During::Init(path)))?;
        let ops = Self::with_repo(repo, path);
        let info = ops.repo_info(InfoReq {})?;
        Ok((ops, info))
    }

    /// Opens the store at `path`. `E_NOT_A_REPO` if there is none, or if what is there is not a
    /// store this build reads.
    pub fn open(path: &Utf8Path) -> Result<LocalOps, OpError> {
        let repo = LocalRepo::open(path).map_err(|e| repo_error(e, During::Open(path)))?;
        Ok(Self::with_repo(repo, path))
    }

    /// The store's path, as given to [`LocalOps::init`] or [`LocalOps::open`].
    pub fn store_path(&self) -> &Utf8Path {
        &self.store_path
    }

    /// The revisions resolved since the last call, each as given and the full changeset id it
    /// named: what the CLI reports as `meta.resolved`. Taking them empties the record, so one
    /// handle can serve several commands.
    pub fn take_resolved(&self) -> BTreeMap<String, String> {
        std::mem::take(&mut *self.resolved.lock().unwrap_or_else(PoisonError::into_inner))
    }

    fn with_repo(repo: LocalRepo, path: &Utf8Path) -> LocalOps {
        LocalOps {
            repo,
            store_path: path.to_owned(),
            resolved: Mutex::default(),
        }
    }

    /// The changeset `rev` names, recorded for [`LocalOps::take_resolved`].
    fn resolve(&self, rev: &Rev) -> Result<ChangesetId, OpError> {
        let id = self.repo.resolve(rev.as_str()).map_err(op)?;
        self.record(rev.as_str(), id);
        Ok(id)
    }

    /// What `main` names, if it exists, recorded as resolved: what an operation uses when its
    /// request names no revision.
    fn main_head(&self) -> Result<Option<ChangesetId>, OpError> {
        let head = self
            .repo
            .get_ref("main")
            .map_err(op)?
            .map(|state| state.target);
        if let Some(id) = head {
            self.record("main", id);
        }
        Ok(head)
    }

    fn record(&self, rev: &str, id: ChangesetId) {
        self.resolved
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(rev.to_owned(), id.to_string());
    }

    /// Changeset `id`, which a revision has just named.
    fn stored(&self, id: ChangesetId) -> Result<StoredChangeset, OpError> {
        self.repo
            .changeset(id)
            .map_err(op)?
            .ok_or_else(|| op(RepoError::NotFound(id.to_string())))
    }

    /// The entry at `path` in changeset `cs`'s tree, from its directory's listing; `None` if
    /// nothing is there.
    fn entry(&self, cs: ChangesetId, path: &RepoPath) -> Result<Option<TreeEntry>, OpError> {
        let Some(mut entries) = self
            .repo
            .list_tree(cs, path.parent().as_ref())
            .map_err(op)?
        else {
            return Ok(None);
        };
        let name = path.file_name().as_bytes();
        Ok(entries
            .binary_search_by(|entry| entry.name.as_bytes().cmp(name))
            .ok()
            .map(|at| entries.swap_remove(at)))
    }

    /// Why `read_tree` has no directory at `dir` (the root for `None`) in changeset `cs`,
    /// which `rev` named: a file is there, or nothing is.
    fn no_directory(&self, cs: ChangesetId, rev: &Rev, dir: Option<&RepoPath>) -> OpError {
        let path = dir.map_or("", RepoPath::as_str);
        match dir.map(|dir| self.entry(cs, dir)) {
            Some(Ok(Some(_))) => error(
                ErrorCode::PathNotFound,
                format!("{path:?} is a file at {rev}, not a directory"),
            )
            .with_details(rev_and_path(rev, path))
            .with_hint(format!("swarmops cat {rev}:{path}")),
            Some(Err(e)) => e,
            _ => path_not_found(rev, path),
        }
    }

    /// Where a log page resumes after `cursor`, the last id of the page before: that
    /// changeset's first parent, or `None` at a root. `E_INVALID_ARG` for a cursor that is not
    /// the full id of a changeset in the store.
    fn after_cursor(&self, cursor: &str) -> Result<Option<ChangesetId>, OpError> {
        let bad = |why: &str| {
            error(
                ErrorCode::InvalidArg,
                format!("{cursor:?} is not a log cursor: {why}"),
            )
            .with_details(details([("cursor", cursor.into())]))
            .with_hint("pass the previous page's cursor as it was given, or none to start over")
        };
        let id = ObjectId::from_hex(cursor)
            .map(ChangesetId)
            .map_err(|_| bad("a cursor is a full changeset id"))?;
        let stored = self
            .repo
            .changeset(id)
            .map_err(op)?
            .ok_or_else(|| bad("the store holds no such changeset"))?;
        Ok(stored.changeset.parents.first().copied())
    }
}

impl StoreOps for LocalOps {
    fn repo_info(&self, _req: InfoReq) -> Result<RepoInfo, OpError> {
        let info = self.repo.info().map_err(op)?;
        Ok(RepoInfo {
            repo_id: info.repo_id,
            name: info.name,
            main: info.main.map(|id| id.to_string()),
            store_version: info.store_version,
            changesets: info.changesets,
            refs: info.refs,
        })
    }

    fn commit(&self, who: &Identity, req: CommitReq) -> Result<CommitResp, OpError> {
        let CommitReq {
            parent,
            changes,
            message,
            allow_empty,
        } = req;
        let parent = match &parent {
            Some(rev) => Some(self.resolve(rev)?),
            None => self.main_head()?,
        };
        let changes = content::changes(changes)?;
        let signature = native_signature(who);
        let mut metadata = vec![("agent".to_owned(), who.agent().as_bytes().to_vec())];
        if let Some(run_id) = who.run_id() {
            metadata.push(("run_id".to_owned(), run_id.as_bytes().to_vec()));
        }
        let outcome = self
            .repo
            .commit(CommitRequest {
                parents: parent.into_iter().collect(),
                changes,
                author: signature.clone(),
                committer: signature,
                message: message.into_bytes(),
                metadata,
                allow_empty,
            })
            .map_err(op)?;
        // The delta leaves out puts of what was there already: count what it holds.
        let files_changed = self.stored(outcome.id)?.changeset.delta.len();
        Ok(CommitResp {
            changeset: outcome.id.to_string(),
            parent: parent.map(|id| id.to_string()),
            root: outcome.root.to_string(),
            files_changed: count(files_changed),
            blobs_written: count(outcome.blobs_written),
            warnings: outcome.warnings,
        })
    }

    fn read_file(&self, req: ReadFileReq) -> Result<FileContent, OpError> {
        let ReadFileReq {
            rev,
            path,
            max_bytes,
        } = req;
        let cs = self.resolve(&rev)?;
        let repo_path = content::repo_path(&path)?;
        let Some((kind, bytes)) = self.repo.read_blob_at(cs, &repo_path).map_err(op)? else {
            // No file is there: what is, if anything, can only be a directory.
            return Err(match self.entry(cs, &repo_path)? {
                Some(_) => error(
                    ErrorCode::IsDir,
                    format!("{path:?} is a directory at {rev}"),
                )
                .with_details(rev_and_path(&rev, &path))
                .with_hint(format!("swarmops ls {rev}:{path}")),
                None => path_not_found(&rev, &path),
            });
        };
        let keep = usize::try_from(max_bytes).map_or(bytes.len(), |max| max.min(bytes.len()));
        let (encoding, content) = content::encode(&bytes[..keep]);
        Ok(FileContent {
            // The store verified the bytes against the entry's id as it read them, so hashing
            // them gives that id, without a second walk down the trees for the entry.
            blob: hash_object(ObjectKind::Blob, &bytes).to_hex(),
            size: count(bytes.len()),
            kind: file_kind(kind),
            encoding,
            content,
            truncated: keep < bytes.len(),
        })
    }

    fn read_tree(&self, req: ReadTreeReq) -> Result<TreeListing, OpError> {
        let ReadTreeReq {
            rev,
            path,
            limit,
            cursor,
        } = req;
        at_least_one("limit", limit)?;
        let cs = self.resolve(&rev)?;
        let dir = match path.as_deref() {
            None | Some("") => None,
            Some(path) => Some(content::repo_path(path)?),
        };
        let Some(entries) = self.repo.list_tree(cs, dir.as_ref()).map_err(op)? else {
            return Err(self.no_directory(cs, &rev, dir.as_ref()));
        };
        let start = cursor.as_deref().map_or(0, |after| {
            entries.partition_point(|entry| entry.name.as_bytes() <= after.as_bytes())
        });
        let rest = &entries[start..];
        let page = &rest[..rest.len().min(limit)];
        let truncated = page.len() < rest.len();
        Ok(TreeListing {
            entries: page.iter().map(tree_entry_out).collect(),
            truncated,
            cursor: page
                .last()
                .filter(|_| truncated)
                .map(|last| last.name.clone()),
        })
    }

    fn show(&self, req: ShowReq) -> Result<ChangesetDetail, OpError> {
        let ShowReq { rev, max_changes } = req;
        at_least_one("max_changes", max_changes)?;
        let StoredChangeset {
            id, seq, changeset, ..
        } = self.stored(self.resolve(&rev)?)?;
        Ok(ChangesetDetail {
            id: id.to_string(),
            seq,
            parents: changeset.parents.iter().map(ToString::to_string).collect(),
            root: changeset.root.to_string(),
            author: attribution(&changeset.metadata),
            created_at: created_at(&changeset.committer),
            message: lossy(&changeset.message),
            changes: changeset
                .delta
                .iter()
                .take(max_changes)
                .map(change_out)
                .collect(),
            files_changed: count(changeset.delta.len()),
            changes_truncated: changeset.delta.len() > max_changes,
            metadata: changeset
                .metadata
                .iter()
                .map(|(key, value)| content::metadata_entry(key, value))
                .collect(),
            signatures: Signatures {
                author: signature_out(&changeset.author),
                committer: signature_out(&changeset.committer),
            },
        })
    }

    fn log(&self, req: LogReq) -> Result<LogPage, OpError> {
        let LogReq { rev, limit, cursor } = req;
        at_least_one("limit", limit)?;
        let from = match cursor {
            Some(cursor) => match self.after_cursor(&cursor)? {
                Some(parent) => parent,
                None => {
                    return Ok(LogPage {
                        entries: Vec::new(),
                        truncated: false,
                        cursor: None,
                    });
                }
            },
            None => self.resolve(&rev.unwrap_or_else(|| Rev::from("main")))?,
        };
        let changesets = self.repo.log_first_parent(from, limit).map_err(op)?;
        // A full page whose last changeset has a parent has older history after it.
        let last = changesets
            .last()
            .filter(|last| changesets.len() == limit && !last.changeset.parents.is_empty());
        Ok(LogPage {
            entries: changesets.iter().map(summary).collect(),
            truncated: last.is_some(),
            cursor: last.map(|last| last.id.to_string()),
        })
    }

    fn ref_list(&self, _req: RefListReq) -> Result<RefListResp, OpError> {
        let refs = self.repo.list_refs().map_err(op)?;
        Ok(RefListResp {
            refs: refs
                .into_iter()
                .map(|(name, state)| RefOut {
                    name,
                    target: state.target.to_string(),
                    version: state.version,
                })
                .collect(),
        })
    }

    fn ref_set(&self, who: &Identity, req: RefSetReq) -> Result<RefSetResp, OpError> {
        let RefSetReq {
            name,
            new,
            expected_old,
            create,
        } = req;
        if create == expected_old.is_some() {
            return Err(error(
                ErrorCode::InvalidArg,
                format!("ref set {name}: give exactly one of expected_old and create"),
            )
            .with_details(details([("name", name.into())]))
            .with_hint(
                "move a ref with --expect <its target now>, or make a new one with --create",
            ));
        }
        let new = self.resolve(&new)?;
        let expected = match &expected_old {
            Some(rev) => Some(self.resolve(rev)?),
            None => None,
        };
        let outcome = self
            .repo
            .cas_ref(&name, expected, Some(new), who.agent())
            .map_err(op)?;
        let replay = match outcome.result {
            CasResult::Applied { .. } => false,
            CasResult::Mismatch { actual } if actual == Some(new) => true,
            CasResult::Mismatch { actual } => {
                let hint = match actual {
                    Some(actual) => format!("swarmops ref set {name} {new} --expect {actual}"),
                    None => format!("swarmops ref set {name} {new} --create"),
                };
                return Err(cas_mismatch(&name, expected, actual, hint));
            }
        };
        Ok(RefSetResp {
            name,
            old: expected.map(|id| id.to_string()),
            new: Some(new.to_string()),
            already_applied: replay,
            warnings: outcome.warnings,
        })
    }

    fn ref_delete(&self, who: &Identity, req: RefDeleteReq) -> Result<RefSetResp, OpError> {
        let RefDeleteReq { name, expected_old } = req;
        let expected = self.resolve(&expected_old)?;
        let outcome = self
            .repo
            .cas_ref(&name, Some(expected), None, who.agent())
            .map_err(op)?;
        match outcome.result {
            CasResult::Applied { .. } => Ok(RefSetResp {
                name,
                old: Some(expected.to_string()),
                new: None,
                already_applied: false,
                warnings: outcome.warnings,
            }),
            CasResult::Mismatch { actual: None } => Err(error(
                ErrorCode::RefNotFound,
                format!("there is no ref {name}"),
            )
            .with_details(details([("name", name.into())]))
            .with_hint("swarmops ref list")),
            CasResult::Mismatch {
                actual: Some(actual),
            } => {
                let hint = format!("swarmops ref delete {name} --expect {actual}");
                Err(cas_mismatch(&name, Some(expected), Some(actual), hint))
            }
        }
    }

    fn verify(&self, req: VerifyReq) -> Result<VerifyReport, OpError> {
        let report = self.repo.verify(req.deep).map_err(op)?;
        Ok(VerifyReport {
            objects_checked: report.objects_checked,
            changesets_checked: report.changesets_checked,
            problems: report
                .problems
                .iter()
                .map(|problem| one_line(problem.to_string()))
                .collect(),
        })
    }
}

/// [`repo_error`] for an operation on an open store.
fn op(e: RepoError) -> OpError {
    repo_error(e, During::Op)
}

/// The operations-spec error for repository error `e`, raised while `during`: the table in
/// [`LocalOps`]'s documentation.
fn repo_error(e: RepoError, during: During<'_>) -> OpError {
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
fn error(code: ErrorCode, message: impl Into<String>) -> OpError {
    OpError::new(code, one_line(message.into()))
}

/// `text` with each control character (a line break, a tab, a NUL…) escaped, so that it is
/// one line.
fn one_line(text: String) -> String {
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

fn details<const N: usize>(entries: [(&str, Value); N]) -> Map<String, Value> {
    entries
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect()
}

fn object(id: ObjectId) -> Map<String, Value> {
    details([("object", id.to_hex().into())])
}

fn rev_and_path(rev: &Rev, path: &str) -> Map<String, Value> {
    details([("rev", rev.as_str().into()), ("path", path.into())])
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

/// Nothing at `path` in the changeset `rev` named; the hint lists the directory it would be in.
fn path_not_found(rev: &Rev, path: &str) -> OpError {
    let listing = match path.rsplit_once('/') {
        Some((dir, _)) => format!("swarmops ls {rev}:{dir}"),
        None => format!("swarmops ls {rev}"),
    };
    error(
        ErrorCode::PathNotFound,
        format!("nothing is at {path:?} in {rev}"),
    )
    .with_details(rev_and_path(rev, path))
    .with_hint(listing)
}

/// A ref move refused because ref `name` names `actual` (`None`: it does not exist), not
/// `expected` (`None`: the move was to create it); `hint` is the move that would succeed now.
fn cas_mismatch(
    name: &str,
    expected: Option<ChangesetId>,
    actual: Option<ChangesetId>,
    hint: String,
) -> OpError {
    let message = match (expected, actual) {
        (None, Some(actual)) => format!("ref {name} exists already: it names {actual}"),
        (Some(expected), Some(actual)) => format!("ref {name} names {actual}, not {expected}"),
        (Some(expected), None) => {
            format!("ref {name} does not exist, so it does not name {expected}")
        }
        (None, None) => format!("ref {name} does not exist"),
    };
    let actual = actual.map_or(Value::Null, |id| id.to_string().into());
    error(ErrorCode::CasMismatch, message)
        .with_details(details([("name", name.into()), ("actual", actual)]))
        .with_hint(hint)
}

/// `E_INVALID_ARG` unless `value`, the request's `field`, is at least 1.
fn at_least_one(field: &str, value: usize) -> Result<(), OpError> {
    if value == 0 {
        return Err(
            error(ErrorCode::InvalidArg, format!("{field} must be at least 1"))
                .with_details(details([(field, value.into())])),
        );
    }
    Ok(())
}

/// The signature of a changeset that `who` records now: the agent name, the email
/// `<instance>@<family>.swarmops.local`, the current time in UTC.
fn native_signature(who: &Identity) -> Signature {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
        });
    Signature {
        name: who.agent().as_bytes().to_vec(),
        email: format!("{}@{}.swarmops.local", who.instance(), who.family()).into_bytes(),
        secs,
        offset_secs: 0,
        negative_zero: false,
    }
}

/// Whom a changeset's `metadata` attributes it to.
fn attribution(metadata: &[(String, Vec<u8>)]) -> Attribution {
    let text = |key: &str| {
        metadata
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, value)| lossy(value))
    };
    Attribution {
        agent: text("agent").unwrap_or_else(|| UNKNOWN_AGENT.to_owned()),
        run_id: text("run_id"),
        on_behalf_of: None,
    }
}

/// When a changeset with `committer` was recorded, in Unix milliseconds: 0 before 1970.
fn created_at(committer: &Signature) -> u64 {
    u64::try_from(committer.secs).map_or(0, |secs| secs.saturating_mul(1000))
}

fn summary(stored: &StoredChangeset) -> ChangesetSummary {
    let changeset = &stored.changeset;
    let first_line = changeset
        .message
        .split(|&b| b == b'\n')
        .next()
        .unwrap_or_default();
    ChangesetSummary {
        id: stored.id.to_string(),
        parents: changeset.parents.iter().map(ToString::to_string).collect(),
        author_agent: attribution(&changeset.metadata).agent,
        created_at: created_at(&changeset.committer),
        title: lossy(first_line.strip_suffix(b"\r").unwrap_or(first_line)),
    }
}

fn change_out(entry: &DeltaEntry) -> ChangeOut {
    let path = entry.path.to_string();
    match &entry.op {
        DeltaOp::Put { kind, id } => ChangeOut {
            path,
            op: ChangeOp::Put,
            kind: Some(file_kind(*kind)),
            blob: Some(id.to_string()),
        },
        DeltaOp::Delete => ChangeOut {
            path,
            op: ChangeOp::Delete,
            kind: None,
            blob: None,
        },
    }
}

fn tree_entry_out(entry: &TreeEntry) -> TreeEntryOut {
    TreeEntryOut {
        name: entry.name.clone(),
        kind: match entry.kind {
            EntryKind::File => EntryKindOut::File,
            EntryKind::Exec => EntryKindOut::Exec,
            EntryKind::Symlink => EntryKindOut::Symlink,
            EntryKind::Dir => EntryKindOut::Dir,
        },
        id: entry.id.to_hex(),
    }
}

/// The kind of a file entry: a leaf, which the codec never lets be a directory.
fn file_kind(kind: EntryKind) -> FileKind {
    match kind {
        EntryKind::File => FileKind::File,
        EntryKind::Exec => FileKind::Exec,
        EntryKind::Symlink => FileKind::Symlink,
        EntryKind::Dir => unreachable!("a file entry or a delta put is never a directory"),
    }
}

fn signature_out(signature: &Signature) -> SignatureOut {
    SignatureOut {
        name: lossy(&signature.name),
        email: lossy(&signature.email),
        secs: signature.secs,
        offset_secs: signature.offset_secs,
    }
}

/// `bytes` as UTF-8, with invalid bytes replaced.
fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn count(n: usize) -> u64 {
    u64::try_from(n).expect("a usize fits in a u64")
}

#[cfg(test)]
mod tests {
    use swarmops_core::CodecError;

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
