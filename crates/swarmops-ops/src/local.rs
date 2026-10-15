//! [`LocalOps`]: every [`StoreOps`] operation, run in this process against a store on the
//! local filesystem through `swarmops-repo` (operations spec §1, §7). Its documentation says
//! what each operation does and how failures map to error codes.

use std::collections::BTreeMap;
use std::fmt;
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
use swarmops_repo::{CasResult, CommitRequest, LocalRepo, RepoError, StoredChangeset};

use crate::content;
use crate::error::{During, details, error, hint, one_line, op, repo_error, shell_word};

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
///   already. The change list follows [`crate::content`]'s rules. `file` content is read here,
///   in the caller's process: `LocalOps` runs on the client. A server that exposes
///   [`StoreOps`] over a wire must refuse [`ContentInput::File`](swarmops_api::ContentInput)
///   instead of reading a path its caller names (M10 adds the client-side pass that turns such
///   content into bytes before a request leaves the client). A metadata key may not end in
///   `:b64`, which `show` reserves (`E_INVALID_ARG`).
/// - `read_file` returns at most `max_bytes` bytes from the file's start: UTF-8 text when they
///   are text, a cut inside a character moved back to the character's start (so up to 3 bytes
///   fewer); otherwise exactly `max_bytes` bytes, in base64. A directory there is `E_IS_DIR`,
///   nothing there `E_PATH_NOT_FOUND`.
/// - `read_tree` lists a directory (the root for an omitted or empty path; one trailing `/` is
///   taken) in name order, a page of at most `limit` entries after the `cursor` name; a file or
///   nothing at the path is `E_PATH_NOT_FOUND`.
/// - `show` attributes the changeset by its `agent` and `run_id` metadata (`unknown/unknown`
///   when it records no agent); `created_at` is the committer's time in milliseconds (0 before
///   1970); a metadata value that is not UTF-8 is given in base64 under its key with `:b64`
///   appended, a suffix commits refuse in a key, so `k:b64` is always the base64 of `k`'s
///   value; `changes` holds at most `max_changes` entries.
/// - `log` follows first parents from `rev` (`main` when omitted) or, given a `cursor`, from the
///   first parent of the changeset it names, which must be a full id the store holds; a page
///   is `truncated` when older history follows its last entry.
/// - `ref_set` and `ref_delete` move a ref by compare-and-swap, recorded for the caller's agent.
///   A move that finds the ref as it asks already (at `new` for `ref_set`; gone, or never there,
///   for `ref_delete`) is a replay: `already_applied`, with the response the move gave.
/// - `verify` checks the store and lists each problem as one line.
///
/// Limits of 0 (`limit`, `max_changes`) are `E_INVALID_ARG`.
///
/// # Errors
///
/// Every error's message and hint is one line (control characters in it are escaped), and the
/// hint, where there is an obvious next step, is a command, each word a shell would split or
/// expand in single quotes. Repository failures map to codes as follows, `details` keys in
/// braces:
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
/// not exist), whose message gives the retry (commit on `actual`, then expect it) and whose hint
/// only reads, `swarmops show <actual>` or `swarmops ref list`, so that an agent that runs it
/// cannot undo the move that won; `E_TOO_LARGE`
/// `{index, path, size, limit}` for a put over 512 MiB; and `E_INVALID_ARG` for limits, cursors
/// and requests that break their own rules.
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
            .with_hint(hint(format!(
                "swarmops cat {}",
                shell_word(&format!("{rev}:{path}"))
            ))),
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
        content::check_metadata(&metadata)?;
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
                .with_hint(hint(format!(
                    "swarmops ls {}",
                    shell_word(&format!("{rev}:{path}"))
                ))),
                None => path_not_found(&rev, &path),
            });
        };
        let keep = usize::try_from(max_bytes).map_or(bytes.len(), |max| max.min(bytes.len()));
        let (encoding, content) = content::encode(&bytes, keep);
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
        // One trailing `/` is taken (`ls main:src/`); an error names the path as given.
        let given = path
            .as_deref()
            .map(|path| (path, path.strip_suffix('/').unwrap_or(path)));
        let dir = match given {
            None | Some((_, "")) => None,
            Some((path, trimmed)) => {
                Some(RepoPath::new(trimmed).or_else(|_| content::repo_path(path))?)
            }
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
                let message = match (expected, actual) {
                    (Some(_), Some(actual)) => format!(
                        "ref {name} moved to {actual}: commit your change on top of it \
                         (--parent {actual}), then retry with --expect {actual}"
                    ),
                    (None, Some(actual)) => {
                        format!("ref {name} exists already: it points at {actual}")
                    }
                    (Some(expected), None) => {
                        format!("ref {name} does not exist, so it does not point at {expected}")
                    }
                    (None, None) => format!("ref {name} does not exist"),
                };
                return Err(cas_mismatch(&name, actual, message));
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
        let replay = match outcome.result {
            CasResult::Applied { .. } => false,
            // Gone already, as the delete asks: a replay of it, or of another's.
            CasResult::Mismatch { actual: None } => true,
            CasResult::Mismatch {
                actual: Some(actual),
            } => {
                let message =
                    format!("ref {name} now points at {actual}; re-check before deleting");
                return Err(cas_mismatch(&name, Some(actual), message));
            }
        };
        Ok(RefSetResp {
            name,
            old: Some(expected.to_string()),
            new: None,
            already_applied: replay,
            warnings: outcome.warnings,
        })
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

fn rev_and_path(rev: &Rev, path: &str) -> Map<String, Value> {
    details([("rev", rev.as_str().into()), ("path", path.into())])
}

/// Nothing at `path` in the changeset `rev` named; the hint lists the directory it would be in.
fn path_not_found(rev: &Rev, path: &str) -> OpError {
    let listing = match path.rsplit_once('/') {
        Some((dir, _)) => shell_word(&format!("{rev}:{dir}")),
        None => shell_word(rev.as_str()),
    };
    error(
        ErrorCode::PathNotFound,
        format!("nothing is at {path:?} in {rev}"),
    )
    .with_details(rev_and_path(rev, path))
    .with_hint(hint(format!("swarmops ls {listing}")))
}

/// A ref move refused because ref `name` names `actual` (`None`: it does not exist), not what
/// the move expected; `message` says so and how to retry. The hint only reads (what the ref
/// names now, or the refs when it is gone), so that an agent that runs it cannot undo the move
/// that won.
fn cas_mismatch(name: &str, actual: Option<ChangesetId>, message: String) -> OpError {
    let next = match actual {
        Some(actual) => format!("swarmops show {actual}"),
        None => "swarmops ref list".to_owned(),
    };
    let actual = actual.map_or(Value::Null, |id| id.to_string().into());
    error(ErrorCode::CasMismatch, message)
        .with_details(details([("name", name.into()), ("actual", actual)]))
        .with_hint(next)
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
