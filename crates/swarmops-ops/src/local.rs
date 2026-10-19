//! [`LocalOps`]: every [`StoreOps`] operation, run in this process against a store on the
//! local filesystem through `swarmops-repo` (operations spec §1, §7). Its documentation says
//! what each operation does and how failures map to error codes.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use camino::{Utf8Path, Utf8PathBuf};
use serde_json::{Map, Value};
use swarmops_api::{
    Attribution, ChangeOp, ChangeOut, ChangesetDetail, ChangesetSummary, CommitReq, CommitResp,
    EntryKindOut, ErrorCode, FileContent, FileKind, Identity, InfoReq, LogCutOut, LogPage, LogReq,
    OpError, ReadFileReq, ReadTreeReq, RecoveredOut, RefDeleteReq, RefListReq, RefListResp, RefOut,
    RefSetReq, RefSetResp, RepoInfo, Rev, ShowReq, SignatureOut, Signatures, StoreOps,
    TreeEntryOut, TreeListing, VerifyFinding, VerifyReport, VerifyReq,
};
use swarmops_core::bytes::to_u64;
use swarmops_core::{
    ChangesetId, DeltaEntry, DeltaOp, EntryKind, ObjectId, RepoPath, Signature, TreeEntry,
};
use swarmops_repo::{
    CasResult, CommitRequest, LocalRepo, MismatchCause, RepoError, StoredChangeset, VerifyProblem,
};
use swarmops_store::{LogCut, Recovered};

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
/// then any `~N` suffixes), and the operation works on the changeset it named. A request made
/// through [`LocalOps::recording`] records what each revision named, and `main` when an
/// operation used it by default, in the [`Resolved`] of that request alone, which the CLI and
/// the MCP server report as `meta.resolved`: requests that share a `LocalOps`, from one thread
/// or from several, never see each other's. An operation called on `LocalOps` itself records
/// nothing.
///
/// # Recovery
///
/// A write first repairs what earlier writers left when they crashed or failed after their
/// commit point (storage spec §3). `commit`, `ref_set` and `ref_delete` made through
/// [`LocalOps::recording`] record what their write repaired, when it repaired anything, in the
/// request's [`Resolved`], which the CLI and the MCP server report as `meta.recovered`: on
/// success and on `E_CAS_MISMATCH` alike, never as a warning, and not when the operation then
/// fails otherwise.
///
/// # Operations
///
/// - `commit` builds on `parent`, or, when it is omitted, on `main`'s head if `main` exists and
///   on nothing otherwise (a root changeset). The changeset is the caller's: author and
///   committer are both the agent name with the email `<instance>@<family>.swarmops.local` at
///   the current time, UTC (or at [`NOW_ENV`]'s, a test hook of builds with the `test-hooks`
///   feature); its metadata holds `agent` and, when the caller has one, `run_id`.
///   `files_changed` counts the changeset's delta, which leaves out puts of what a path holds
///   already. The change list follows [`crate::content`]'s rules. `file` content is read here,
///   in the caller's process: `LocalOps` runs on the client. A server that exposes
///   [`StoreOps`] over a wire must refuse [`ContentInput::File`](swarmops_api::ContentInput)
///   instead of reading a path its caller names (M10 adds the client-side pass that turns such
///   content into bytes before a request leaves the client); `swarmops mcp` is no such server,
///   since its caller starts it, as its own user, to serve that caller alone over stdio, and it
///   reads such content as the CLI does. A metadata key may not end in
///   `:b64`, which `show` reserves (`E_INVALID_ARG`).
///   With `advance`, the commit also moves that ref to its changeset, in the same transaction,
///   if the ref names the commit's parent (for a root commit, if the ref does not exist):
///   `advanced` reports the move, `already_applied` where an identical request made it before.
///   A ref that names anything else fails the commit with `E_CAS_MISMATCH`, and nothing is
///   recorded. Without `parent`, such a commit builds on that ref's head, recorded in the
///   request's [`Resolved`] under the ref's name, and on nothing if the ref does not exist (a
///   root changeset that creates it): the move then only detects a race from that moment on.
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
/// - `ref_set` and `ref_delete` move a ref by compare-and-swap, recorded for the caller's agent,
///   and decide every move under the store's lock, after the write's tail check, never from a
///   reader's snapshot. A move that finds the ref as it asks already (at `new` for `ref_set`,
///   whatever it expected; gone, or never there, for `ref_delete`) is a replay: `already_applied`,
///   with the response the move gave, and nothing is written, no journal frame and no new version
///   of the ref. So a `ref_set` whose `new` is the target it expects never moves the ref: it is a
///   replay where the ref holds that target, and a mismatch where it holds another.
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
///   `DuplicatePath` `{path}`, and the store's `DuplicateParent` `{parent}`, `Limit`
///   `{store_limit, max, got}` (a request past a limit of the store's layout: `store_limit` is
///   `parents`, `actor_bytes`, `ref_name_bytes`, `records` or `repo_name_bytes`) and `BadPrefix`
///   `{prefix}` (an id prefix that is not 1 to 64 hex digits). Of the requests of these
///   operations only `init`'s reaches either, with a name of more than 255 bytes
///   (`repo_name_bytes`): a commit has one parent, the identity bounds the actor, and revision
///   resolution takes 12 to 63 hex digits.
/// - `E_EXISTS`: `Exists`; the hint is `swarmops --repo <path> info`.
/// - `E_UNKNOWN_PARENT` `{parent}`: `UnknownParent`; the hint is `swarmops log`.
/// - `E_EMPTY_COMMIT`: `EmptyChanges`; the hint names `--allow-empty`.
/// - `E_TYPE_CONFLICT` `{path}`: `TypeConflict`.
/// - `E_PATH_NOT_FOUND` `{path}`: `PathNotFound`.
/// - `E_REF_NAME_INVALID` `{name}`: `RefName`, the repository's or the store's; the hint gives
///   the ref grammar.
/// - `E_CAS_MISMATCH` (retryable) `{name, actual, committed}`, `committed` always `false`:
///   `RefMoved`, a `commit` with `advance` whose ref named something else than the commit's
///   parent (`actual` null when the ref does not exist), so that the commit recorded nothing.
///   The message says to commit the change again on top of `actual` (`--parent <actual>
///   --advance <name>`), and the hint only reads, as for a ref move's below.
/// - `E_BUSY` (retryable): the store's `Busy` (a writer's lock wait ran out) or
///   `SnapshotContended` (a read's snapshot rounds ran out); either way nothing was committed (a
///   commit's flushed objects may remain, for GC). The hint says which, and to retry.
/// - `E_NOT_A_REPO`, while opening: the store's `Format`, and an `Io` error of kind
///   `NotADirectory`. The hint is `swarmops init <path>` when the path holds no `FORMAT` file;
///   with one, the store is damaged or of a layout this build does not read.
/// - `E_STORE_CORRUPT`: `MissingObject`, and the store's `Corrupt` and `BadHeader`
///   `{object}`; `Codec`, and the store's `Codec`, `JournalCorrupt`, `IndexCorrupt`,
///   `IndexMismatch` and `MissingParent`; an `Io` error of kind `InvalidData` (a malformed
///   metadata file) `{fs_op, store_file}`; and the store's `Format` other than while opening.
///   Once the store is open, the hint is `swarmops verify` (with `--deep` for an object whose
///   bytes fail their id).
/// - `E_UNSUPPORTED_FS` `{path}`: the store's `UnsupportedFs`, a path under `/mnt/`, on `init`
///   and on open alike; the hint points at a native filesystem.
/// - `E_IO` `{fs_op, store_file}`: any other `Io` error, whatever its kind (an OS `EINVAL` among
///   them). `fs_op` names the file operation that failed; `store_file` the file of the store it
///   was on, relative to the store's root (`.` for the root itself), and is absent for a path
///   outside it; no error carries a host path.
/// - `E_IO_TRANSIENT` (retryable) `{committed, fs_op, store_file}`, `committed` always `false`:
///   the store's `NotCommitted`, a journal append that failed and that the store cut back off
///   the journal, so the write committed nothing; `fs_op` and `store_file` name the append. The
///   hint says to run the same command again.
/// - `E_INTERNAL`: the store's `PostCommit`, which the repository turns into a warning (a
///   failure after the commit point, a failed listing in `journal.off` among them), so that it
///   never reaches here; and the store's `BadCall`, a call its API does not take, which only a
///   bug makes.
///
/// The operations raise their own errors besides:
///
/// - `E_IS_DIR` and `E_PATH_NOT_FOUND` `{rev, path}` for reads;
/// - `E_CAS_MISMATCH` `{name, actual}` (`actual` null when the ref does not exist), whose hint
///   only reads, `swarmops show <actual>` or `swarmops ref list`, so that an agent that runs it
///   cannot undo the move that won, and whose message says what to do next:
///   - to a `ref_set` that finds the ref moved, or created by another: commit its change again
///     on top of `actual` (`--parent <actual>`), re-applying its edits to the content there
///     rather than re-sending its old files, then retry with `--expect <actual>`;
///   - to a `ref_delete`: re-check before deleting;
///   - to a move of a ref that is gone: that it is gone;
/// - `E_TOO_LARGE` `{index, path, size, limit}` for a put over 512 MiB;
/// - `E_INVALID_ARG` for limits, cursors and requests that break their own rules.
pub struct LocalOps {
    repo: LocalRepo,
    /// The store's path as the caller gave it, for hints and display.
    store_path: Utf8PathBuf,
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
    /// `E_UNSUPPORTED_FS` for a path under `/mnt/`, `E_INVALID_ARG` for a name of more than 255
    /// bytes (`details.store_limit` `repo_name_bytes`), before anything is created. A store made
    /// whole whose reading back for the report fails is that read's error, saying the store is
    /// made (`details.created`).
    pub fn init(path: &Utf8Path, name: &str) -> Result<(LocalOps, RepoInfo), OpError> {
        let repo = LocalRepo::create(path, name).map_err(|e| repo_error(e, During::Init(path)))?;
        let ops = Self::with_repo(repo, path);
        let info = ops
            .repo_info(InfoReq {})
            .map_err(|e| crate::error::created_but_unread(path, e))?;
        Ok((ops, info))
    }

    /// Opens the store at `path`. `E_NOT_A_REPO` if there is none, or if what is there is not a
    /// store this build reads; `E_UNSUPPORTED_FS` if the path leads under `/mnt/`.
    pub fn open(path: &Utf8Path) -> Result<LocalOps, OpError> {
        let repo = LocalRepo::open(path).map_err(|e| repo_error(e, During::Open(path)))?;
        Ok(Self::with_repo(repo, path))
    }

    /// The store's path, as given to [`LocalOps::init`] or [`LocalOps::open`].
    pub fn store_path(&self) -> &Utf8Path {
        &self.store_path
    }

    /// The operations of one request: each revision they resolve is recorded in `resolved`,
    /// and in no other request's.
    pub fn recording<'a>(&'a self, resolved: &'a Resolved) -> Recording<'a> {
        Recording {
            ops: self,
            resolved,
        }
    }

    fn with_repo(repo: LocalRepo, path: &Utf8Path) -> LocalOps {
        LocalOps {
            repo,
            store_path: path.to_owned(),
        }
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

/// The revisions one request resolved: each as given, and the full changeset id it named. The
/// CLI and the MCP server report it as `meta.resolved`. And what the request's write repaired
/// before its own work, if it repaired anything (`commit`, `ref_set` and `ref_delete` record it,
/// on success and on `E_CAS_MISMATCH` alike), which they report as `meta.recovered`.
///
/// Each request has its own: [`LocalOps::recording`] records into it what that request's
/// operations resolve and repair, and nothing else does, so that what one request resolved or
/// repaired never shows in another's, also when the two share a `LocalOps` across threads.
#[derive(Debug, Default)]
pub struct Resolved {
    revs: RefCell<BTreeMap<String, String>>,
    recovered: Cell<Option<Recovered>>,
}

impl Resolved {
    /// What has been recorded, each revision as given and the id it named; taking it empties
    /// the record.
    pub fn take(&self) -> BTreeMap<String, String> {
        self.revs.take()
    }

    /// What the request's write repaired before its own work, if it repaired anything; taking
    /// it empties the record.
    pub fn take_recovered(&self) -> Option<RecoveredOut> {
        self.recovered.take().map(recovered_out)
    }

    fn record(&self, rev: &str, id: ChangesetId) {
        self.revs
            .borrow_mut()
            .insert(rev.to_owned(), id.to_string());
    }

    /// Records `recovered`, what the request's write repaired, unless it repaired nothing.
    fn record_recovered(&self, recovered: Recovered) {
        if !recovered.is_empty() {
            self.recovered.set(Some(recovered));
        }
    }
}

/// `recovered` as the envelope's `meta.recovered` gives it.
fn recovered_out(recovered: Recovered) -> RecoveredOut {
    let Recovered {
        log_cut,
        frames_listed,
        frames_rewritten,
        off_bytes_dropped,
        index_records_dropped,
        views_rebuilt,
        records_replayed,
        refs_replayed,
    } = recovered;
    RecoveredOut {
        log_cut: log_cut.map(|LogCut { at, bytes }| LogCutOut { at, bytes }),
        frames_listed,
        frames_rewritten,
        off_bytes_dropped,
        index_records_dropped,
        views_rebuilt,
        records_replayed,
        refs_replayed,
    }
}

/// [`LocalOps`]'s operations as one request: [`StoreOps`], recording each revision they resolve
/// in the request's [`Resolved`] ([`LocalOps::recording`]).
#[derive(Debug, Clone, Copy)]
pub struct Recording<'a> {
    ops: &'a LocalOps,
    resolved: &'a Resolved,
}

impl Recording<'_> {
    /// The changeset `rev` names, recorded as resolved.
    fn resolve(&self, rev: &Rev) -> Result<ChangesetId, OpError> {
        let id = self.ops.repo.resolve(rev.as_str()).map_err(op)?;
        self.resolved.record(rev.as_str(), id);
        Ok(id)
    }

    /// What `main` names, if it exists, recorded as resolved: what an operation uses when its
    /// request names no revision.
    fn main_head(&self) -> Result<Option<ChangesetId>, OpError> {
        self.head("main")
    }

    /// What ref `name` names, if it exists, recorded as resolved under the ref's name: `main`'s
    /// head for an operation whose request names no revision, the advanced ref's for a commit
    /// with `advance` and no parent.
    fn head(&self, name: &str) -> Result<Option<ChangesetId>, OpError> {
        let head = self
            .ops
            .repo
            .get_ref(name)
            .map_err(op)?
            .map(|state| state.target);
        if let Some(id) = head {
            self.resolved.record(name, id);
        }
        Ok(head)
    }
}

impl StoreOps for Recording<'_> {
    fn repo_info(&self, _req: InfoReq) -> Result<RepoInfo, OpError> {
        let info = self.ops.repo.info().map_err(op)?;
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
            advance,
            allow_empty,
        } = req;
        let parent = match (&parent, &advance) {
            (Some(rev), _) => Some(self.resolve(rev)?),
            // A commit that advances a ref builds on that ref's head by default.
            (None, Some(name)) => self.head(name)?,
            (None, None) => self.main_head()?,
        };
        let changes = content::changes(changes)?;
        let signature = native_signature(who);
        let mut metadata = vec![("agent".to_owned(), who.agent().as_bytes().to_vec())];
        if let Some(run_id) = who.run_id() {
            metadata.push(("run_id".to_owned(), run_id.as_bytes().to_vec()));
        }
        content::check_metadata(&metadata)?;
        let outcome = self
            .ops
            .repo
            .commit(CommitRequest {
                parents: parent.into_iter().collect(),
                changes,
                author: signature.clone(),
                committer: signature,
                message: message.into_bytes(),
                metadata,
                allow_empty,
                advance: advance.map(|name| swarmops_repo::Advance {
                    name,
                    actor: who.agent().to_owned(),
                }),
            })
            .map_err(|e| {
                // A move refused under the lock is E_CAS_MISMATCH, which reports what the write
                // repaired first, as a ref move's does.
                if let RepoError::RefMoved { recovered, .. } = &e {
                    self.resolved.record_recovered(**recovered);
                }
                op(e)
            })?;
        self.resolved.record_recovered(outcome.recovered);
        Ok(CommitResp {
            changeset: outcome.id.to_string(),
            parent: parent.map(|id| id.to_string()),
            root: outcome.root.to_string(),
            files_changed: to_u64(outcome.files_changed),
            blobs_written: to_u64(outcome.blobs_written),
            advanced: outcome.advanced.map(|moved| swarmops_api::Advanced {
                name: moved.name,
                old: moved.old.map(|id| id.to_string()),
                new: outcome.id.to_string(),
                already_applied: moved.already_applied,
            }),
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
        let Some((kind, blob, bytes)) = self.ops.repo.read_blob_at(cs, &repo_path).map_err(op)?
        else {
            // No file is there: what is, if anything, can only be a directory.
            return Err(match self.ops.entry(cs, &repo_path)? {
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
            blob: blob.to_string(),
            size: to_u64(bytes.len()),
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
        // One trailing `/` is taken (`ls main:src/`). A path malformed even without it is
        // E_INVALID_ARG naming it as given; past that, an error (no directory there) names the
        // path without the `/`.
        let given = path
            .as_deref()
            .map(|path| (path, path.strip_suffix('/').unwrap_or(path)));
        let dir = match given {
            None | Some((_, "")) => None,
            Some((path, trimmed)) => {
                Some(RepoPath::new(trimmed).or_else(|_| content::repo_path(path))?)
            }
        };
        let Some(entries) = self.ops.repo.list_tree(cs, dir.as_ref()).map_err(op)? else {
            return Err(self.ops.no_directory(cs, &rev, dir.as_ref()));
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
        } = self.ops.stored(self.resolve(&rev)?)?;
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
            files_changed: to_u64(changeset.delta.len()),
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
            Some(cursor) => match self.ops.after_cursor(&cursor)? {
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
        let changesets = self.ops.repo.log_first_parent(from, limit).map_err(op)?;
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
        let refs = self.ops.repo.list_refs().map_err(op)?;
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
        // Every move is compared under the lock, after the write's tail check, never on a reader's
        // snapshot, which can show another writer's frame before its sync. A ref that holds `new`
        // already is a replay, whatever `expected` says: a mismatch whose actual target is `new`,
        // or, for a move to the target it expects, the match itself, which the repository does
        // not record.
        let outcome = self
            .ops
            .repo
            .cas_ref(&name, expected, Some(new), who.agent())
            .map_err(op)?;
        self.resolved.record_recovered(outcome.recovered);
        let replay = match outcome.result {
            CasResult::Applied { old } => old == Some(new),
            CasResult::Mismatch { actual } if actual == Some(new) => true,
            CasResult::Mismatch { actual } => {
                let message = match (expected, actual) {
                    (Some(_), Some(actual)) => {
                        retry_on(&format!("ref {name} moved to {actual}"), actual)
                    }
                    (None, Some(actual)) => retry_on(
                        &format!("ref {name} exists already and points at {actual}"),
                        actual,
                    ),
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
            .ops
            .repo
            .cas_ref(&name, Some(expected), None, who.agent())
            .map_err(op)?;
        self.resolved.record_recovered(outcome.recovered);
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
        let report = self.ops.repo.verify(req.deep).map_err(op)?;
        Ok(VerifyReport {
            objects_checked: report.objects_checked,
            changesets_checked: report.changesets_checked,
            problems: report
                .problems
                .iter()
                .map(|problem| one_line(problem.to_string()))
                .collect(),
            findings: report.problems.iter().map(finding).collect(),
        })
    }
}

/// Each operation as a request of its own, whose resolved revisions go unrecorded: to report
/// them, call it through [`LocalOps::recording`].
impl StoreOps for LocalOps {
    fn repo_info(&self, req: InfoReq) -> Result<RepoInfo, OpError> {
        self.recording(&Resolved::default()).repo_info(req)
    }

    fn commit(&self, who: &Identity, req: CommitReq) -> Result<CommitResp, OpError> {
        self.recording(&Resolved::default()).commit(who, req)
    }

    fn read_file(&self, req: ReadFileReq) -> Result<FileContent, OpError> {
        self.recording(&Resolved::default()).read_file(req)
    }

    fn read_tree(&self, req: ReadTreeReq) -> Result<TreeListing, OpError> {
        self.recording(&Resolved::default()).read_tree(req)
    }

    fn show(&self, req: ShowReq) -> Result<ChangesetDetail, OpError> {
        self.recording(&Resolved::default()).show(req)
    }

    fn log(&self, req: LogReq) -> Result<LogPage, OpError> {
        self.recording(&Resolved::default()).log(req)
    }

    fn ref_list(&self, req: RefListReq) -> Result<RefListResp, OpError> {
        self.recording(&Resolved::default()).ref_list(req)
    }

    fn ref_set(&self, who: &Identity, req: RefSetReq) -> Result<RefSetResp, OpError> {
        self.recording(&Resolved::default()).ref_set(who, req)
    }

    fn ref_delete(&self, who: &Identity, req: RefDeleteReq) -> Result<RefSetResp, OpError> {
        self.recording(&Resolved::default()).ref_delete(who, req)
    }

    fn verify(&self, req: VerifyReq) -> Result<VerifyReport, OpError> {
        self.recording(&Resolved::default()).verify(req)
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

/// The message of a `ref_set` that lost to another move, which left the ref at `actual`
/// (`what` says how): commit the change again on top of `actual`, its edits re-applied to the
/// content there (M1 has no merge, so whole files sent again would revert the other's edits),
/// then expect `actual`.
fn retry_on(what: &str, actual: ChangesetId) -> String {
    format!(
        "{what}: commit your change on top of it (--parent {actual}), then retry with --expect \
         {actual}; re-apply your edits to the content at {actual} rather than re-sending your \
         old files"
    )
}

/// A ref move refused because ref `name` names `actual` (`None`: it does not exist), not what
/// the move expected; `message` says so and how to retry. The hint only reads (what the ref
/// names now, or the refs when it is gone), so that an agent that runs it cannot undo the move
/// that won.
pub(crate) fn cas_mismatch(name: &str, actual: Option<ChangesetId>, message: String) -> OpError {
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

/// Test hook, in a build with the `test-hooks` feature: with `SWARMOPS_TEST_NOW=<unix seconds>`
/// set to an integer, [`LocalOps`] signs the changesets it commits at that time instead of the
/// clock's. The same commit made twice is then the same changeset, whenever it runs: a crash
/// test's runs write the same objects, and so make the same filesystem calls, which the store's
/// own test hooks count and crash at (`swarmops_store::CRASH_AT_ENV`,
/// `swarmops_store::FS_CALLS_ENV`). Unset, not an integer, or in a build without the feature (a
/// release build among them): the clock's time.
pub const NOW_ENV: &str = "SWARMOPS_TEST_NOW";

/// The signature of a changeset that `who` records now: the agent name, the email
/// `<instance>@<family>.swarmops.local`, the current time in UTC ([`signing_time`]).
fn native_signature(who: &Identity) -> Signature {
    Signature {
        name: who.agent().as_bytes().to_vec(),
        email: format!("{}@{}.swarmops.local", who.instance(), who.family()).into_bytes(),
        secs: signing_time(),
        offset_secs: 0,
        negative_zero: false,
    }
}

/// Now, in Unix seconds: [`NOW_ENV`]'s seconds if the test hook gives them ([`test_now`]),
/// else the clock's.
fn signing_time() -> i64 {
    if let Some(secs) = test_now() {
        return secs;
    }
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
        })
}

/// [`NOW_ENV`]'s seconds, if it is set to an integer, in a build with the `test-hooks` feature.
#[cfg(feature = "test-hooks")]
fn test_now() -> Option<i64> {
    std::env::var(NOW_ENV)
        .ok()
        .and_then(|secs| secs.parse().ok())
}

/// In a build without the `test-hooks` feature, [`NOW_ENV`] is ignored.
#[cfg(not(feature = "test-hooks"))]
fn test_now() -> Option<i64> {
    None
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

/// `problem` as a program reads it (operations spec §1, `verify`): its kind, a string of its
/// own for each variant, and what it names, ids in lowercase hex. The match has no catch-all,
/// so a new kind of problem does not compile until it has a finding.
fn finding(problem: &VerifyProblem) -> VerifyFinding {
    let of = |kind: &str| VerifyFinding {
        kind: kind.to_owned(),
        object: None,
        changeset: None,
        parent: None,
        seq: None,
        offset: None,
        ref_name: None,
        cause: None,
    };
    match problem {
        VerifyProblem::CorruptObject(id) => VerifyFinding {
            object: Some(id.to_hex()),
            ..of("corrupt_object")
        },
        VerifyProblem::MissingObject { referenced_by, id } => VerifyFinding {
            object: Some(id.to_hex()),
            changeset: Some(referenced_by.to_string()),
            ..of("missing_object")
        },
        VerifyProblem::MissingParent { child, parent } => VerifyFinding {
            changeset: Some(child.to_string()),
            parent: Some(parent.to_string()),
            ..of("missing_parent")
        },
        VerifyProblem::RootMismatch(id) => VerifyFinding {
            changeset: Some(id.to_string()),
            ..of("root_mismatch")
        },
        VerifyProblem::DanglingRef(name) => VerifyFinding {
            ref_name: Some(name.clone()),
            ..of("dangling_ref")
        },
        VerifyProblem::CorruptRef(name) => VerifyFinding {
            ref_name: Some(name.clone()),
            ..of("corrupt_ref")
        },
        VerifyProblem::IndexMismatch { seq, cause } => VerifyFinding {
            seq: Some(*seq),
            cause: Some(mismatch_cause(*cause).to_owned()),
            ..of("index_mismatch")
        },
        VerifyProblem::JournalCorrupt { offset } => VerifyFinding {
            offset: Some(*offset),
            ..of("journal_corrupt")
        },
        VerifyProblem::MetaCorrupt(_) => of("meta_corrupt"),
    }
}

/// What a finding's `cause` says disagrees, which names what mends it.
fn mismatch_cause(cause: MismatchCause) -> &'static str {
    match cause {
        MismatchCause::Journal => "journal",
        MismatchCause::SortedView => "sorted_view",
        MismatchCause::Object => "object",
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;
    use swarmops_api::{ChangeInput, ContentInput, RecoveredOut, RefSetReq};
    use swarmops_store::FsOps;
    use swarmops_testkit::faults::FsyncModelFs;
    use swarmops_testkit::fs::FailingFs;
    use swarmops_testkit::store::file_len;

    use super::*;

    /// The real filesystem, except that calls of `method` on a path ending in `suffix` fail
    /// with an injected error.
    fn failing(method: &'static str, suffix: &'static str) -> FailingFs {
        FailingFs::new(move |called, path| called == method && path.ends_with(suffix))
    }

    /// `LocalOps` over the store at `root`, its every filesystem call going through `fs`.
    fn ops_on(root: &Utf8Path, fs: impl FsOps + 'static) -> LocalOps {
        let repo = LocalRepo::open_with_fs(root, Arc::new(fs)).expect("open the store");
        LocalOps::with_repo(repo, root)
    }

    /// The one warning `warnings` holds, which must name the injected failure.
    #[track_caller]
    fn one_injected(warnings: &[String]) {
        let [warning] = warnings else {
            panic!("expected one warning, got {warnings:?}");
        };
        assert!(warning.contains("injected failure"), "{warning}");
        assert!(warning.contains("committed"), "{warning}");
    }

    /// Each problem `verify` can find, `IndexMismatch` once for each cause, is a finding of its
    /// own kind with exactly the keys that kind names, its ids in lowercase hex, so that a
    /// program reads what is damaged without parsing the line. No two problems share a kind but
    /// `IndexMismatch`'s causes, which `cause` tells apart.
    #[test]
    fn every_verify_problem_is_a_finding_with_its_keys() {
        let (object, child, parent) = ("ab".repeat(32), "cd".repeat(32), "ef".repeat(32));
        let id = |hex: &str| ObjectId::from_hex(&hex.to_uppercase()).unwrap();
        let changeset = |hex: &str| ChangesetId(id(hex));
        let mismatch = |cause| VerifyProblem::IndexMismatch { seq: 7, cause };
        let cases = [
            (
                VerifyProblem::CorruptObject(id(&object)),
                json!({"kind": "corrupt_object", "object": object}),
            ),
            (
                VerifyProblem::MissingObject {
                    referenced_by: changeset(&child),
                    id: id(&object),
                },
                json!({"kind": "missing_object", "object": object, "changeset": child}),
            ),
            (
                VerifyProblem::MissingParent {
                    child: changeset(&child),
                    parent: changeset(&parent),
                },
                json!({"kind": "missing_parent", "changeset": child, "parent": parent}),
            ),
            (
                VerifyProblem::RootMismatch(changeset(&child)),
                json!({"kind": "root_mismatch", "changeset": child}),
            ),
            (
                VerifyProblem::DanglingRef("main".to_owned()),
                json!({"kind": "dangling_ref", "ref": "main"}),
            ),
            (
                VerifyProblem::CorruptRef("release/1.0".to_owned()),
                json!({"kind": "corrupt_ref", "ref": "release/1.0"}),
            ),
            (
                mismatch(MismatchCause::Journal),
                json!({"kind": "index_mismatch", "seq": 7, "cause": "journal"}),
            ),
            (
                mismatch(MismatchCause::SortedView),
                json!({"kind": "index_mismatch", "seq": 7, "cause": "sorted_view"}),
            ),
            (
                mismatch(MismatchCause::Object),
                json!({"kind": "index_mismatch", "seq": 7, "cause": "object"}),
            ),
            (
                VerifyProblem::JournalCorrupt { offset: 4096 },
                json!({"kind": "journal_corrupt", "offset": 4096}),
            ),
            (
                VerifyProblem::MetaCorrupt("journal.off is missing".to_owned()),
                json!({"kind": "meta_corrupt"}),
            ),
        ];
        let mut kinds: BTreeMap<String, Vec<&VerifyProblem>> = BTreeMap::new();
        for (problem, expected) in &cases {
            let found = finding(problem);
            assert_eq!(
                serde_json::to_value(&found).unwrap(),
                *expected,
                "{problem:?}"
            );
            kinds.entry(found.kind).or_default().push(problem);
        }
        assert_eq!(kinds.len(), 9, "{kinds:?}");
        for (kind, problems) in &kinds {
            let shared = problems.len() > 1;
            assert!(!shared || kind == "index_mismatch", "{kind}: {problems:?}");
        }
    }

    #[test]
    fn failures_after_the_commit_point_come_back_as_warnings() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
        drop(LocalOps::init(&root, "acme").unwrap());
        let who = Identity::parse("agent/worker-17").unwrap();

        // Materializing the commit's frame into the index fails once the frame is durable: the
        // commit succeeded, and says what went wrong after.
        let ops = ops_on(&root, failing("append", "idx/ids.col"));
        let put = ChangeInput {
            path: "f".to_owned(),
            op: ChangeOp::Put,
            content: Some(ContentInput::Inline("x".to_owned())),
            mode: None,
        };
        let req = CommitReq {
            parent: None,
            changes: vec![put],
            message: "m".to_owned(),
            allow_empty: false,
            advance: None,
        };
        let committed = ops.commit(&who, req).unwrap();
        one_injected(&committed.warnings);
        assert_eq!(committed.files_changed, 1);
        drop(ops);

        // Writing the ref file fails once the move's frame is durable: likewise.
        let ops = ops_on(&root, failing("atomic_replace", "refs/main"));
        let req = RefSetReq {
            name: "main".to_owned(),
            new: committed.changeset.as_str().into(),
            expected_old: None,
            create: true,
        };
        let moved = ops.ref_set(&who, req).unwrap();
        one_injected(&moved.warnings);
        assert!(!moved.already_applied);
        drop(ops);

        // Both are committed: a fresh handle sees them.
        let fresh = LocalOps::open(&root).unwrap();
        let info = fresh.repo_info(InfoReq {}).unwrap();
        assert_eq!(info.main.as_deref(), Some(committed.changeset.as_str()));
        assert_eq!(info.changesets, 1);
    }

    /// A commit whose journal frame cannot be appended (journal.log refuses it) committed
    /// nothing: `E_IO_TRANSIENT`, retryable, `committed: false`, with a hint to run it again. A
    /// fresh handle sees no changeset.
    #[test]
    fn a_commit_whose_journal_append_fails_is_retryable_and_not_committed() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
        drop(LocalOps::init(&root, "acme").unwrap());
        let who = Identity::parse("agent/worker-17").unwrap();
        let ops = ops_on(&root, failing("append_durable", "journal.log"));
        let put = ChangeInput {
            path: "f".to_owned(),
            op: ChangeOp::Put,
            content: Some(ContentInput::Inline("x".to_owned())),
            mode: None,
        };
        let req = CommitReq {
            parent: None,
            changes: vec![put],
            message: "m".to_owned(),
            allow_empty: false,
            advance: None,
        };
        let error = ops.commit(&who, req).unwrap_err();
        assert_eq!(
            (error.code.as_str(), error.exit, error.retryable),
            ("E_IO_TRANSIENT", 4, true),
            "{error:?}"
        );
        assert_eq!(error.details["committed"], false, "{error:?}");
        assert!(error.hint.is_some(), "{error:?}");
        drop(ops);

        let fresh = LocalOps::open(&root).unwrap();
        assert_eq!(fresh.repo_info(InfoReq {}).unwrap().changesets, 0);
    }

    /// A commit whose write cannot list the frame a commit before it left whole and unlisted (that
    /// commit's own listing in journal.off failed: committed, with a warning) committed nothing
    /// of its own: `E_IO_TRANSIENT`, retryable, `committed: false`, naming the append that failed,
    /// where it was the non-retryable `E_IO`. Run again where journal.off takes the listing, it
    /// commits, and the store holds both changesets.
    #[test]
    fn a_commit_whose_tail_check_cannot_list_a_frame_is_retryable_and_not_committed() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
        drop(LocalOps::init(&root, "acme").unwrap());
        let who = Identity::parse("agent/worker-17").unwrap();
        let ops = ops_on(&root, failing("append_durable", "journal.off"));
        let commit = |path: &str| CommitReq {
            parent: None,
            changes: vec![ChangeInput {
                path: path.to_owned(),
                op: ChangeOp::Put,
                content: Some(ContentInput::Inline("x".to_owned())),
                mode: None,
            }],
            message: "m".to_owned(),
            allow_empty: false,
            advance: None,
        };
        one_injected(&ops.commit(&who, commit("f")).unwrap().warnings);
        let error = ops.commit(&who, commit("g")).unwrap_err();
        assert_eq!(
            (error.code.as_str(), error.exit, error.retryable),
            ("E_IO_TRANSIENT", 4, true),
            "{error:?}"
        );
        assert_eq!(error.details["committed"], false, "{error:?}");
        assert_eq!(error.details["fs_op"], "append_durable", "{error:?}");
        assert_eq!(error.details["store_file"], "meta/journal.off", "{error:?}");
        drop(ops);

        let fresh = LocalOps::open(&root).unwrap();
        fresh.commit(&who, commit("g")).unwrap();
        assert_eq!(fresh.repo_info(InfoReq {}).unwrap().changesets, 2);
    }

    /// What a request's write repaired first goes into that request's record alone, which the
    /// CLI and the MCP server report as `meta.recovered`, on `E_CAS_MISMATCH` too: after a ref
    /// move whose listing in `journal.off` failed, a mismatching `ref_set`'s write lists the
    /// frame and writes the ref file, and records that; another request on the same `LocalOps`,
    /// whose write repairs nothing, records nothing.
    #[test]
    fn a_request_records_what_its_own_write_repaired() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
        let (ops, _) = LocalOps::init(&root, "acme").unwrap();
        let who = Identity::parse("agent/worker-17").unwrap();
        let commit = |content: &str| {
            let put = ChangeInput {
                path: "f".to_owned(),
                op: ChangeOp::Put,
                content: Some(ContentInput::Inline(content.to_owned())),
                mode: None,
            };
            let req = CommitReq {
                parent: None,
                changes: vec![put],
                message: "m".to_owned(),
                allow_empty: false,
                advance: None,
            };
            ops.commit(&who, req).unwrap().changeset
        };
        let (c1, c2) = (commit("1"), commit("2"));
        let set = |new: &str, expected_old: Option<&str>| RefSetReq {
            name: "main".to_owned(),
            new: new.into(),
            expected_old: expected_old.map(Rev::from),
            create: expected_old.is_none(),
        };
        let created = ops_on(&root, failing("append_durable", "journal.off"))
            .ref_set(&who, set(&c1, None))
            .unwrap();
        one_injected(&created.warnings);

        let (first, second) = (Resolved::default(), Resolved::default());
        // main holds c1, not c2.
        let stale = ops
            .recording(&first)
            .ref_set(&who, set(&c2, Some(&c2)))
            .unwrap_err();
        assert_eq!(stale.code.as_str(), "E_CAS_MISMATCH", "{stale:?}");
        let listed_and_written = RecoveredOut {
            frames_listed: 1,
            refs_replayed: 1,
            ..RecoveredOut::default()
        };
        assert_eq!(first.take_recovered(), Some(listed_and_written));
        assert_eq!(first.take_recovered(), None, "taking it empties the record");
        let replay = ops.recording(&second).ref_set(&who, set(&c1, Some(&c1)));
        assert!(replay.unwrap().already_applied);
        assert_eq!(second.take_recovered(), None, "its write repaired nothing");
    }

    /// A commit with `advance` whose ref moved first is `E_CAS_MISMATCH`, which records what its
    /// write repaired first, as a ref move's does: after a ref move whose listing in
    /// `journal.off` failed, the losing commit's write lists the frame and writes the ref file.
    #[test]
    fn an_advancing_commit_that_loses_records_what_its_write_repaired() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
        let (ops, _) = LocalOps::init(&root, "acme").unwrap();
        let who = Identity::parse("agent/worker-17").unwrap();
        let req = |parent: Option<&str>, content: &str, advance: Option<&str>| CommitReq {
            parent: parent.map(Rev::from),
            changes: vec![ChangeInput {
                path: "f".to_owned(),
                op: ChangeOp::Put,
                content: Some(ContentInput::Inline(content.to_owned())),
                mode: None,
            }],
            message: "m".to_owned(),
            advance: advance.map(str::to_owned),
            allow_empty: false,
        };
        let c0 = ops.commit(&who, req(None, "0", None)).unwrap().changeset;
        let c1 = ops.commit(&who, req(None, "1", None)).unwrap().changeset;
        let main = RefSetReq {
            name: "main".to_owned(),
            new: c1.as_str().into(),
            expected_old: None,
            create: true,
        };
        let created = ops_on(&root, failing("append_durable", "journal.off"))
            .ref_set(&who, main)
            .unwrap();
        one_injected(&created.warnings);

        let resolved = Resolved::default();
        // main holds c1, not c0.
        let lost = ops
            .recording(&resolved)
            .commit(&who, req(Some(&c0), "2", Some("main")))
            .unwrap_err();
        assert_eq!(lost.code.as_str(), "E_CAS_MISMATCH", "{lost:?}");
        assert_eq!(lost.details["committed"], false, "{lost:?}");
        let listed_and_written = RecoveredOut {
            frames_listed: 1,
            refs_replayed: 1,
            ..RecoveredOut::default()
        };
        assert_eq!(resolved.take_recovered(), Some(listed_and_written));
        assert_eq!(ops.repo_info(InfoReq {}).unwrap().changesets, 2);
    }

    /// A `ref_set` to the target it expects is decided under the lock, after its write's tail
    /// check, as every `ref_set` is (operations spec §4): never from a snapshot, which shows a
    /// whole frame `journal.off` does not list as if it were committed, though its writer may not
    /// have synced it. `main` moves from c1 to c2 in a frame whose listing fails: committed with a
    /// warning, and readers see c2. (a) The self-move to c2 lists the frame before it answers its
    /// replay, and writes no frame of its own. (b) Where the tail check can neither sync the frame
    /// nor write it afresh, it cuts it, and the self-move is a retryable `E_IO_TRANSIENT` that
    /// committed nothing, not a replay of a move no reader sees afterwards.
    #[test]
    fn a_move_to_the_target_it_expects_is_decided_after_the_tail_check() {
        for tail_check_fails in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
            let (ops, _) = LocalOps::init(&root, "acme").unwrap();
            let who = Identity::parse("agent/worker-17").unwrap();
            let commit = |parent: Option<&str>, content: &str| {
                let put = ChangeInput {
                    path: "f".to_owned(),
                    op: ChangeOp::Put,
                    content: Some(ContentInput::Inline(content.to_owned())),
                    mode: None,
                };
                let req = CommitReq {
                    parent: parent.map(Rev::from),
                    changes: vec![put],
                    message: "m".to_owned(),
                    allow_empty: false,
                    advance: None,
                };
                ops.commit(&who, req).unwrap().changeset
            };
            let c1 = commit(None, "1");
            let c2 = commit(Some(&c1), "2");
            let set = |new: &str, expected_old: Option<&str>| RefSetReq {
                name: "main".to_owned(),
                new: new.into(),
                expected_old: expected_old.map(Rev::from),
                create: expected_old.is_none(),
            };
            ops.ref_set(&who, set(&c1, None)).unwrap();
            // The move to c2: its frame is whole and durable in journal.log, and its listing in
            // journal.off fails, so readers overlay it, and the next write's tail check must
            // sync and list it.
            let moved = ops_on(&root, failing("append_durable", "journal.off"))
                .ref_set(&who, set(&c2, Some(&c1)))
                .unwrap();
            one_injected(&moved.warnings);
            let main = |ops: &LocalOps| ops.repo_info(InfoReq {}).unwrap().main;
            assert_eq!(main(&ops).as_deref(), Some(c2.as_str()), "readers see c2");
            let (log, off) = (root.join("meta/journal.log"), root.join("meta/journal.off"));
            let lens = || (file_len(&log), file_len(&off));
            let (log_len, off_len) = lens();
            let self_move = set(&c2, Some(&c2));
            if tail_check_fails {
                // The tail check's sync of journal.log fails, and so does its rewrite.
                let model = Arc::new(FsyncModelFs::new().failing(|call| {
                    call.path.ends_with("journal.log")
                        && (call.method == "sync_data" || call.method == "append_durable")
                }));
                let error = ops_on(&root, Arc::clone(&model))
                    .ref_set(&who, self_move)
                    .unwrap_err();
                assert_eq!(
                    (error.code.as_str(), error.exit, error.retryable),
                    ("E_IO_TRANSIENT", 4, true),
                    "{error:?}"
                );
                assert_eq!(error.details["committed"], false, "{error:?}");
                assert_eq!(
                    model.failed(),
                    [format!("sync_data {log}"), format!("append_durable {log}")]
                );
                let fresh = LocalOps::open(&root).unwrap();
                assert_eq!(
                    main(&fresh).as_deref(),
                    Some(c1.as_str()),
                    "the cut took the move back"
                );
            } else {
                let replay = ops.ref_set(&who, self_move).unwrap();
                assert!(
                    replay.already_applied && replay.warnings.is_empty(),
                    "{replay:?}"
                );
                assert_eq!(
                    lens(),
                    (log_len, off_len + 8),
                    "its write listed the frame before it answered, and wrote no frame of its own"
                );
            }
        }
    }
}
