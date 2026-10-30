//! Operation requests and responses, and [`StoreOps`], the boundary every store implements
//! (operations spec §0–§1, §7).
//!
//! Requests and responses alike ignore fields they do not know: the operations spec (§0) forbids
//! `deny_unknown_fields`, so stores and clients of different versions keep working together.
//! Absent options in these types are omitted, never written as null (unlike `OpError.hint`,
//! which is always written). Ids are lowercase hex. A response that could grow without bound is
//! capped by its request (`max_bytes`, `limit`, `max_changes`) and says when it was cut
//! (`truncated`, `changes_truncated`, `hunks_truncated`); `read_tree`, `log`, `path_history` and
//! `diff` also return a `cursor` that resumes after the page, and `read_file` a `next_offset`. The
//! cursor of `read_tree` or `diff`, and a `next_offset`, name a place, not a revision: page at the
//! full ids the first page resolved. The default page of `read_tree`, `show`, `log`,
//! `path_history` and `diff` stays within the operations spec's 64 KiB target for a default
//! response (§9); `read_file`'s 1 MiB is the spec's own.
//!
//! No store-facing type carries a filesystem path. The exceptions are client-side:
//! [`ContentInput::File`], which the client reads and sends as bytes, [`InitReq::path`],
//! because `init` runs on the client and is not a [`StoreOps`] method,
//! [`GitImportReq::git_path`] and [`GitExportReq::git_path`], because `git import` reads its
//! source repository, and `git export` writes its destination, on the caller's host, and
//! neither is a [`StoreOps`] method (nor an MCP tool), and the workspace operations' (M5:
//! [`WsCreateReq::path`], [`WsCreateReq::from`], [`WsInfo::path`]), which run on the caller's
//! host and which no server receives.
//!
//! Not yet:
//! - `LogReq.first_parent`: `log` follows first parents only (the M2 plan's decision 8), until
//!   imported merge histories need the field;
//! - M7: `cr` and `annotations` on `ChangesetDetail` (`show`).

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::OpError;
use crate::identity::Identity;
use crate::rev::Rev;

/// The default [`ReadFileReq::max_bytes`]: 1 MiB.
pub const DEFAULT_MAX_BYTES: u64 = 1024 * 1024;

/// The default [`LogReq::limit`]: 50 changesets, a page of about 13 KB with titles of typical
/// length.
pub const DEFAULT_LOG_LIMIT: usize = 50;

/// The default [`PathHistoryReq::limit`]: 50 changesets, a page of about 16 KB with titles of
/// typical length.
pub const DEFAULT_PATH_HISTORY_LIMIT: usize = 50;

/// The default [`ReadTreeReq::limit`]: 250 entries. An entry prints in about 140 bytes with a
/// 30-byte name and a size of five digits, and in about 245 at `depth` 3, where its `path` is
/// three such names long: so a default page stays under the 64 KiB target at that depth too, as
/// 300 entries would not. A test of `swarmops-ops` measures it.
pub const DEFAULT_READ_TREE_LIMIT: usize = 250;

/// The most levels [`ReadTreeReq::depth`] lists below a directory: 64.
pub const MAX_TREE_DEPTH: u32 = 64;

/// The default [`ShowReq::max_changes`]: 400 changes. A change prints in about 110 bytes
/// besides its path, so a default page stays under the 64 KiB target for paths of up to about
/// 50 bytes.
pub const DEFAULT_MAX_CHANGES: usize = 400;

/// The default [`DeriveReq::limit`]: 1,000 journal frames, each one transaction.
pub const DEFAULT_DERIVE_LIMIT: usize = 1000;

/// The default [`DiffReq::limit`]: 500 files. A file of a `stat` page prints in about 75 bytes
/// besides its path, so a default `names` or `stat` page stays under the 64 KiB target for paths
/// of up to about 40 bytes; a `unified` page is held within [`DiffReq::max_bytes`] as well.
pub const DEFAULT_DIFF_LIMIT: usize = 500;

/// The default [`DiffReq::max_bytes`]: 32 KiB of a `unified` page's files, as JSON.
pub const DEFAULT_DIFF_MAX_BYTES: u64 = 32_768;

/// The default [`DiffReq::context`]: 3 unchanged lines around each change, as git's.
pub const DEFAULT_DIFF_CONTEXT: u32 = 3;

/// The default and the most [`VerifyReq::max_findings`]: 1,000 problems, a report of about
/// 300 KB at most.
pub const VERIFY_MAX_FINDINGS: u32 = 1_000;

fn default_max_bytes() -> u64 {
    DEFAULT_MAX_BYTES
}

fn default_log_limit() -> usize {
    DEFAULT_LOG_LIMIT
}

fn default_path_history_limit() -> usize {
    DEFAULT_PATH_HISTORY_LIMIT
}

fn default_read_tree_limit() -> usize {
    DEFAULT_READ_TREE_LIMIT
}

fn default_read_tree_depth() -> u32 {
    1
}

fn default_max_changes() -> usize {
    DEFAULT_MAX_CHANGES
}

fn default_derive_limit() -> usize {
    DEFAULT_DERIVE_LIMIT
}

fn default_diff_limit() -> usize {
    DEFAULT_DIFF_LIMIT
}

fn default_diff_max_bytes() -> u64 {
    DEFAULT_DIFF_MAX_BYTES
}

fn default_diff_context() -> u32 {
    DEFAULT_DIFF_CONTEXT
}

/// `init`: create a store.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct InitReq {
    /// The directory to create the store in.
    pub path: String,
    /// The repository's name: at most 255 bytes of UTF-8.
    pub name: String,
    /// The store format to create, 1 or 2; omitted for this build's default. A format this
    /// build does not create is `E_INVALID_ARG`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<u32>,
}

impl InitReq {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(path: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            name: name.into(),
            format: None,
        }
    }
}

/// `info`: describe the store. It has no options yet.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct InfoReq {}

/// `init` and `info`: what the store is and holds.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RepoInfo {
    /// The store's id, fixed at `init`.
    pub repo_id: String,
    /// The name given at `init`.
    pub name: String,
    /// The changeset `main` points at; omitted until `main` exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub main: Option<String>,
    /// The store format version.
    pub store_version: u32,
    /// How many changesets the store holds.
    pub changesets: u64,
    /// How many refs the store holds.
    pub refs: u64,
}

impl RepoInfo {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(
        repo_id: impl Into<String>,
        name: impl Into<String>,
        store_version: u32,
        changesets: u64,
        refs: u64,
    ) -> Self {
        Self {
            repo_id: repo_id.into(),
            name: name.into(),
            main: None,
            store_version,
            changesets,
            refs,
        }
    }
}

/// `whoami`: the identity the caller's mutations carry.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct IdentityInfo {
    /// `<family>/<instance>`; omitted for an anonymous caller.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// The run the agent acts in, when it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// How this caller's identity was established: `trust` (claimed, not verified) or
    /// `anonymous`.
    pub auth: String,
}

impl IdentityInfo {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(auth: impl Into<String>) -> Self {
        Self {
            agent: None,
            run_id: None,
            auth: auth.into(),
        }
    }
}

/// `commit`: record a changeset from an explicit change list. It moves no ref, unless
/// `advance` names one.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CommitReq {
    /// The changeset to commit on top of. An explicit `parent` always wins; when it is omitted,
    /// the current head of the ref `advance` names is used, else of `main`, if that ref exists,
    /// and otherwise the commit is a root commit. `null` is the same as omitting it. Once `main`
    /// exists, a root commit needs an `advance` naming a ref that does not exist yet (a known M1
    /// limitation).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<Rev>,
    /// What to change, relative to the parent. The contents hold at most 1 GiB together, by the
    /// sizes they declare (inline text's length, the bytes base64 decodes to, a client file's
    /// length; a `blob` named by id counts nothing): more is `E_TOO_LARGE` with `scope`
    /// `commit`, refused before any content is read. Store large contents with `blob_put` first,
    /// and name them by id.
    pub changes: Vec<ChangeInput>,
    /// The commit message.
    pub message: String,
    /// Also move this ref (`main` or `release/<name>`) to the new changeset, in the same
    /// transaction as the changeset itself, so that both are recorded or neither. The move
    /// happens only if the ref still names the changeset's parent (for a root commit, only if the
    /// ref does not exist yet): it is always a one-step fast-forward. Otherwise nothing is
    /// recorded and the commit fails with `E_CAS_MISMATCH` (`details.committed: false`,
    /// `details.actual` what the ref names). Give as `parent` the full id the ref held when you
    /// read it: without `parent` the commit builds on the ref's head when it runs, so the move
    /// catches a race from there on, not a change made since you read the ref (M1 has no
    /// merge). `swarmops mcp` refuses `advance` without a full 64-hex `parent` (`E_INVALID_ARG`).
    /// A store that does not know this field ignores it and moves no ref: its response has no
    /// `advanced`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub advance: Option<String>,
    /// Makes a retry of this commit safe: a uuid v7 (anything else is `E_INVALID_ARG`). The
    /// commit takes the key's time, to the second, as its signature time and records the key as
    /// `commit_key` metadata, so the same request on the same parent makes the same changeset,
    /// which the store holds already: the response says `replayed`, and nothing is written.
    /// With `advance`, a ref whose head carries the key is a replay too. Give `parent` by its full
    /// id: without it, a retry after the head moved commits again, on the new head.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    /// Record the changeset even if it changes nothing.
    #[serde(default)]
    pub allow_empty: bool,
}

impl CommitReq {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(changes: Vec<ChangeInput>, message: impl Into<String>) -> Self {
        Self {
            parent: None,
            changes,
            message: message.into(),
            advance: None,
            idempotency_key: None,
            allow_empty: false,
        }
    }
}

/// One change of a commit.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ChangeInput {
    /// The repository path: `/`-separated, relative to the root.
    pub path: String,
    /// Whether to write the path or remove it.
    pub op: ChangeOp,
    /// The new content, at most 512 MiB (more is `E_TOO_LARGE` with `scope` `put`); a `put` needs
    /// it, a `delete` takes none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<ContentInput>,
    /// The kind of entry a `put` writes; `file` when omitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<ModeInput>,
}

impl ChangeInput {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(path: impl Into<String>, op: ChangeOp) -> Self {
        Self {
            path: path.into(),
            op,
            content: None,
            mode: None,
        }
    }
}

/// What a change does to its path, in a commit's change list and a changeset's changes alike.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ChangeOp {
    /// Write the content at the path, creating or replacing the entry.
    Put,
    /// Remove the path.
    Delete,
}

/// A `put`'s content, as an object with one key.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ContentInput {
    /// UTF-8 text, verbatim.
    Inline(String),
    /// Bytes in standard, padded base64.
    Base64(String),
    /// A blob the store holds already, by its id (64 hex digits), as `blob_put` returns it: the
    /// commit neither sends nor reads its bytes. An id the store holds no blob under (none, or
    /// a tree's or a changeset's) is `E_INVALID_ARG`, and nothing is written. `blob_put` takes
    /// the other forms only.
    Blob(String),
    /// A file on the client's machine. The client reads it and sends its bytes, so the store is
    /// never asked to read a path.
    File(String),
}

/// The kind of entry a `put` writes.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ModeInput {
    /// A regular file (git mode 100644).
    File,
    /// An executable file (git mode 100755).
    Exec,
    /// A symlink; the content is the link's target.
    Symlink,
}

/// `commit`: the changeset that was recorded.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CommitResp {
    /// The new changeset's id.
    pub changeset: String,
    /// The changeset it was committed on top of; omitted for a root commit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// The new root tree's id.
    pub root: String,
    /// How many paths the changeset changes.
    pub files_changed: u64,
    /// How many blobs this commit's own writes placed in the object store; blobs that were there
    /// already, or that a concurrent writer placed first, count 0.
    pub blobs_written: u64,
    /// The move of the ref the request's `advance` named: present exactly when the ref names
    /// `changeset` because of this request, or of an identical one before it
    /// (`already_applied`). A request with `advance` whose response lacks it went to a store
    /// that ignores the field, and moved no ref.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub advanced: Option<Advanced>,
    /// Problems found after the commit became durable, such as failed housekeeping; the commit
    /// itself succeeded. Omitted when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// The store held the changeset already, from an identical request before (a retry with the
    /// same `idempotency_key`), so nothing was written. Always written; a response from a store
    /// that does not know the field reads as `false`.
    #[serde(default)]
    pub replayed: bool,
}

impl CommitResp {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(
        changeset: impl Into<String>,
        root: impl Into<String>,
        files_changed: u64,
        blobs_written: u64,
    ) -> Self {
        Self {
            changeset: changeset.into(),
            parent: None,
            root: root.into(),
            files_changed,
            blobs_written,
            advanced: None,
            warnings: Vec::new(),
            replayed: false,
        }
    }
}

/// `commit` with `advance`: the ref's move, made in the commit's own transaction.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Advanced {
    /// The ref.
    pub name: String,
    /// What the ref held before: the changeset's parent; omitted for a root commit, whose move
    /// created the ref.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old: Option<String>,
    /// What it holds now: the changeset.
    pub new: String,
    /// The ref held the changeset already, from an identical request before, so nothing was
    /// written and a replay is safe.
    pub already_applied: bool,
}

impl Advanced {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(name: impl Into<String>, new: impl Into<String>, already_applied: bool) -> Self {
        Self {
            name: name.into(),
            old: None,
            new: new.into(),
            already_applied,
        }
    }
}

/// `blob_put` (CLI `blob put`): store content as a blob, outside any commit, so that a commit's
/// change list can name it by its id (`{"blob": id}`) instead of carrying its bytes. The blob is
/// durable once the call returns; until a commit names it, nothing references it.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct BlobPutReq {
    /// The content: inline, base64 or a client file, at most 512 MiB (more is `E_TOO_LARGE` with
    /// `scope` `put`). A `blob` names content the store holds already, and is `E_INVALID_ARG`
    /// here.
    pub content: ContentInput,
}

impl BlobPutReq {
    /// Takes every field.
    pub fn new(content: ContentInput) -> Self {
        Self { content }
    }
}

/// `blob_put`: the blob stored.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct BlobPutResp {
    /// The blob's id, which a change names it by: `{"blob": id}`.
    pub blob: String,
    /// The content's size in bytes.
    pub size: u64,
    /// Whether this call placed the blob in the object store; false when the store held it
    /// already, or a concurrent writer placed it first.
    pub written: bool,
}

impl BlobPutResp {
    /// Takes every field.
    pub fn new(blob: impl Into<String>, size: u64, written: bool) -> Self {
        Self {
            blob: blob.into(),
            size,
            written,
        }
    }
}

/// `blob_has` (CLI `blob has`): which of some blobs the store holds.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct BlobHasReq {
    /// The blob ids, 64 hex digits each; one that is not is `E_INVALID_ARG`.
    pub ids: Vec<String>,
}

impl BlobHasReq {
    /// Takes every field.
    pub fn new(ids: Vec<String>) -> Self {
        Self { ids }
    }
}

/// `blob_has`: the blobs the store holds, of those asked for.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct BlobHasResp {
    /// The ids of `ids` that the store holds a blob under, in lowercase hex, in the order first
    /// given, each once; an id of another kind of object (a tree, a changeset) is not among them.
    pub present: Vec<String>,
}

impl BlobHasResp {
    /// Takes every field.
    pub fn new(present: Vec<String>) -> Self {
        Self { present }
    }
}

/// `resolve_rev` (CLI `resolve`): the changeset a revision names, as every operation resolves it.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ResolveRevReq {
    /// The revision to resolve.
    pub rev: Rev,
}

impl ResolveRevReq {
    /// Takes every field.
    pub fn new(rev: impl Into<Rev>) -> Self {
        Self { rev: rev.into() }
    }
}

/// `resolve_rev` (CLI `resolve`): the changeset a revision names, and what kind of revision it is.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ResolvedRev {
    /// The full id of the changeset the revision names.
    pub id: String,
    /// What the revision is: `ref` when it is exactly a ref's name, `changeset` otherwise.
    pub kind: RevKind,
}

impl ResolvedRev {
    /// Takes every field.
    pub fn new(id: impl Into<String>, kind: RevKind) -> Self {
        Self {
            id: id.into(),
            kind,
        }
    }
}

/// What kind of revision `resolve_rev` resolved.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RevKind {
    /// Exactly a ref's name (`main`, `release/<name>`): `id` is what the ref names now, and moves
    /// when the ref does.
    Ref,
    /// Any other revision: a full changeset id, a unique prefix of one, or a revision followed by
    /// `~N` suffixes, a ref's name among them.
    Changeset,
}

/// `read_file` (CLI `cat`): read one file at a revision, without a checkout: from an offset, or
/// a range of its lines.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ReadFileReq {
    /// The revision to read at.
    pub rev: Rev,
    /// The file's repository path.
    pub path: String,
    /// Where to start reading, in bytes from the file's start: a truncated read's `next_offset`
    /// reads on from where it stopped. An offset past the end gives empty content. Give this or
    /// `lines`, not both (`E_INVALID_ARG`).
    #[serde(default)]
    pub offset: u64,
    /// Read these lines instead, numbered from 1, both ends included: a line ends after its
    /// newline, and a last line without one is a line too. The lines of the range that exist
    /// come back, none past the file's last. Give this or `offset`, not both (`E_INVALID_ARG`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lines: Option<LineRange>,
    /// The most content bytes to return: at least 1 (0 is `E_INVALID_ARG`); more comes back
    /// truncated, with the `next_offset` to read on from.
    #[serde(default = "default_max_bytes")]
    #[schemars(range(min = 1))]
    pub max_bytes: u64,
}

impl ReadFileReq {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(rev: impl Into<Rev>, path: impl Into<String>) -> Self {
        Self {
            rev: rev.into(),
            path: path.into(),
            offset: 0,
            lines: None,
            max_bytes: default_max_bytes(),
        }
    }
}

/// A range of a file's lines, numbered from 1: `first` to `last`, both included.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct LineRange {
    /// The range's first line: at least 1.
    #[schemars(range(min = 1))]
    pub first: u64,
    /// The range's last line: at least `first`.
    #[schemars(range(min = 1))]
    pub last: u64,
}

impl LineRange {
    /// Lines `first` through `last`, unchecked: the store refuses `first > last` or a 0 with
    /// `E_INVALID_ARG`.
    pub fn new(first: u64, last: u64) -> Self {
        Self { first, last }
    }
}

/// `read_file` (CLI `cat`): a file's content, or the part of it the request names.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct FileContent {
    /// The blob's id.
    pub blob: String,
    /// The whole blob's size in bytes, also when `content` is part of it.
    pub size: u64,
    /// The file's kind.
    pub kind: FileKind,
    /// How `content` encodes the bytes it returns: `utf8` when they are text, `base64` when they
    /// are not (as when `offset` falls inside a character).
    pub encoding: ContentEncoding,
    /// The bytes returned: those from `offset`, or the requested lines, at most `max_bytes` of
    /// them. A cut keeps text as text: it falls where a character starts, so text may stop up to
    /// 3 bytes short of `max_bytes`, while bytes that are not text stop at exactly `max_bytes`.
    pub content: String,
    /// Whether `content` stops before the end of what was asked for: the file, from `offset`; or
    /// the requested lines that exist.
    pub truncated: bool,
    /// Where `content` starts in the file, in bytes: the request's `offset`, or where the first
    /// of its `lines` that exists starts (the file's size when none does); omitted when that is
    /// the file's start.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<u64>,
    /// For a request by `lines`: the lines of its range that exist, which `content` holds (the
    /// start of them when `truncated`); omitted when none does.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lines: Option<LineRange>,
    /// When `truncated`: where `content` stops, the `offset` that reads on from there, to the
    /// file's end: a `lines` range is not kept. It pins no revision: read on at the full id the
    /// first read resolved (the envelope's `meta.resolved`), or a ref that moved since gives
    /// another file's bytes, with no error. A cut text holds whole characters, so a `max_bytes`
    /// under 4 can stop before a longer character at the `offset` it started from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<u64>,
}

impl FileContent {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(
        blob: impl Into<String>,
        size: u64,
        kind: FileKind,
        encoding: ContentEncoding,
        content: impl Into<String>,
        truncated: bool,
    ) -> Self {
        Self {
            blob: blob.into(),
            size,
            kind,
            encoding,
            content: content.into(),
            truncated,
            offset: None,
            lines: None,
            next_offset: None,
        }
    }
}

/// How `FileContent.content` encodes bytes.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ContentEncoding {
    /// The bytes are UTF-8 text, and `content` is that text.
    Utf8,
    /// `content` is the bytes in standard, padded base64.
    Base64,
}

/// What a file is; never a directory.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum FileKind {
    /// A regular file.
    File,
    /// An executable file.
    Exec,
    /// A symlink.
    Symlink,
}

/// What a tree entry is.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum EntryKindOut {
    /// A regular file.
    File,
    /// An executable file.
    Exec,
    /// A symlink.
    Symlink,
    /// A directory.
    Dir,
}

/// `read_tree` (CLI `ls`): list one directory at a revision, without a checkout, a page at a
/// time; with `depth`, the directories below it too.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ReadTreeReq {
    /// The revision to read at.
    pub rev: Rev,
    /// The directory's repository path; omitted for the root.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// How many levels to list: 1 for the directory's own entries, 2 for theirs too, and so on, up
    /// to 64 (0 or more than 64 is `E_INVALID_ARG`).
    #[serde(default = "default_read_tree_depth")]
    #[schemars(range(min = 1, max = MAX_TREE_DEPTH))]
    pub depth: u32,
    /// The most entries to return: at least 1 (0 is `E_INVALID_ARG`).
    #[serde(default = "default_read_tree_limit")]
    #[schemars(range(min = 1))]
    pub limit: usize,
    /// Resume a listing: the previous page's `cursor`. Pass it back exactly as returned; do not
    /// construct it (cursors are opaque). This page starts after the entry it names. It pins no
    /// revision: list at the full id the first page resolved (the envelope's `meta.resolved`), or
    /// a ref that moved since lists another tree, with no error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

impl ReadTreeReq {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(rev: impl Into<Rev>) -> Self {
        Self {
            rev: rev.into(),
            path: None,
            depth: default_read_tree_depth(),
            limit: default_read_tree_limit(),
            cursor: None,
        }
    }
}

/// `read_tree` (CLI `ls`): a page of a directory's entries, in pre-order: a directory, then the
/// entries below it to the request's `depth`, each directory's entries by name bytes (at `depth`
/// 1, the tree's order).
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct TreeListing {
    /// The entries.
    pub entries: Vec<TreeEntryOut>,
    /// Whether more entries follow this page.
    pub truncated: bool,
    /// When `truncated`: where the next page starts. Opaque: pass it back as given, as the next
    /// request's `cursor`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

impl TreeListing {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(entries: Vec<TreeEntryOut>, truncated: bool) -> Self {
        Self {
            entries,
            truncated,
            cursor: None,
        }
    }
}

/// One entry of a listing.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct TreeEntryOut {
    /// The entry's name: one path component.
    pub name: String,
    /// Its path below the listed directory, given when the request's `depth` is more than 1: its
    /// name, for one of the directory's own entries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// What the entry is.
    pub kind: EntryKindOut,
    /// For a file, an executable or a symlink: its content's size in bytes; omitted for a
    /// directory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// A blob id for a file, an executable or a symlink; a tree id for a directory.
    pub id: String,
}

impl TreeEntryOut {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(name: impl Into<String>, kind: EntryKindOut, id: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            path: None,
            kind,
            size: None,
            id: id.into(),
        }
    }
}

/// `show`: one changeset in full.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ShowReq {
    /// The changeset to show.
    pub rev: Rev,
    /// The most entries of `changes` to return: at least 1 (0 is `E_INVALID_ARG`).
    #[serde(default = "default_max_changes")]
    #[schemars(range(min = 1))]
    pub max_changes: usize,
}

impl ShowReq {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(rev: impl Into<Rev>) -> Self {
        Self {
            rev: rev.into(),
            max_changes: default_max_changes(),
        }
    }
}

/// `show`: a changeset's header, changes and metadata. Byte strings are decoded as UTF-8, with
/// invalid bytes replaced, except metadata values (see `metadata`).
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ChangesetDetail {
    /// The changeset's id.
    pub id: String,
    /// Its position in the store's changeset index, in commit order.
    pub seq: u64,
    /// The parents' ids; the first is the mainline parent that `changes` are relative to.
    pub parents: Vec<String>,
    /// The root tree's id.
    pub root: String,
    /// Who wrote the change.
    pub author: Attribution,
    /// Who landed it, for a changeset a `land` made: its `lander` and `lander_run_id` metadata.
    /// Omitted for any other changeset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lander: Option<Attribution>,
    /// When it was recorded: the committer signature's time, in Unix milliseconds.
    pub created_at: u64,
    /// The message.
    pub message: String,
    /// The idempotency key of the land that made it (its `land_key` metadata), when it had one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub land_key: Option<String>,
    /// The changesets a land squashed into it, oldest first: the landed head's first-parent chain
    /// after the land's base, ending at that head (its `squashed_from` metadata). Omitted for a
    /// changeset no land made.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub squashed_from: Vec<String>,
    /// What changed relative to the first parent, ordered by path: at most `max_changes`
    /// entries.
    pub changes: Vec<ChangeOut>,
    /// How many paths the changeset changes in all, also when `changes` is truncated.
    pub files_changed: u64,
    /// Whether `changes` stops before the changeset's last change.
    pub changes_truncated: bool,
    /// SwarmOps metadata, such as `agent` and `run_id`. A value that is not UTF-8 is given in
    /// standard, padded base64, under its key with `:b64` appended. A commit refuses a key that
    /// ends in `:b64`, so `k:b64` is always the base64 of `k`'s value, which is not UTF-8.
    pub metadata: BTreeMap<String, String>,
    /// The git author and committer signatures, kept for git fidelity.
    pub signatures: Signatures,
}

impl ChangesetDetail {
    /// Takes the required fields; the others get their serde defaults.
    // Every argument is a required field of the response: no default can stand in.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: impl Into<String>,
        seq: u64,
        parents: Vec<String>,
        root: impl Into<String>,
        author: Attribution,
        created_at: u64,
        message: impl Into<String>,
        changes: Vec<ChangeOut>,
        files_changed: u64,
        changes_truncated: bool,
        metadata: BTreeMap<String, String>,
        signatures: Signatures,
    ) -> Self {
        Self {
            id: id.into(),
            seq,
            parents,
            root: root.into(),
            author,
            lander: None,
            created_at,
            message: message.into(),
            land_key: None,
            squashed_from: Vec::new(),
            changes,
            files_changed,
            changes_truncated,
            metadata,
            signatures,
        }
    }
}

/// Whom a changeset is attributed to (operations spec §5).
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Attribution {
    /// The agent that wrote it: the changeset's `agent` metadata, or `unknown/unknown` for a
    /// changeset that records none.
    pub agent: String,
    /// The run it was written in: the changeset's `run_id` metadata, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// Whom the agent acted for; recorded once the review model exists, so omitted in M1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_behalf_of: Option<String>,
}

impl Attribution {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(agent: impl Into<String>) -> Self {
        Self {
            agent: agent.into(),
            run_id: None,
            on_behalf_of: None,
        }
    }
}

/// A changeset's git signatures: git-fidelity data for import and export.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Signatures {
    /// The git author.
    pub author: SignatureOut,
    /// The git committer; its time is `created_at`.
    pub committer: SignatureOut,
}

impl Signatures {
    /// Takes every field.
    pub fn new(author: SignatureOut, committer: SignatureOut) -> Self {
        Self { author, committer }
    }
}

/// A git signature: who, and when.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SignatureOut {
    /// The name; the agent name for a native changeset.
    pub name: String,
    /// The email.
    pub email: String,
    /// Unix time in seconds.
    pub secs: i64,
    /// The UTC offset in seconds.
    pub offset_secs: i32,
}

impl SignatureOut {
    /// Takes every field.
    pub fn new(
        name: impl Into<String>,
        email: impl Into<String>,
        secs: i64,
        offset_secs: i32,
    ) -> Self {
        Self {
            name: name.into(),
            email: email.into(),
            secs,
            offset_secs,
        }
    }
}

/// One entry of a changeset's changes.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ChangeOut {
    /// The repository path.
    pub path: String,
    /// What happened to the path.
    pub op: ChangeOp,
    /// For a `put`: the file's kind.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<FileKind>,
    /// For a `put`: the content's blob id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>,
}

impl ChangeOut {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(path: impl Into<String>, op: ChangeOp) -> Self {
        Self {
            path: path.into(),
            op,
            kind: None,
            blob: None,
        }
    }
}

/// `log`: history along first parents, a page at a time.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct LogReq {
    /// Where to start; `main` when omitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rev: Option<Rev>,
    /// The most entries to return: at least 1 (0 is `E_INVALID_ARG`).
    #[serde(default = "default_log_limit")]
    #[schemars(range(min = 1))]
    pub limit: usize,
    /// Resume a log: the previous page's `cursor`. Pass it back exactly as returned; do not
    /// construct it (cursors are opaque). This page continues from the first parent of the
    /// changeset it names, and `rev` is not used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// Only the changesets that change one of these paths: a file (its content or kind, or
    /// whether it is there), or a directory and anything below it; one trailing `/` is taken.
    /// Omitted or empty for every changeset.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
}

impl Default for LogReq {
    /// The request a JSON object without fields reads.
    fn default() -> Self {
        Self {
            rev: None,
            limit: default_log_limit(),
            cursor: None,
            paths: Vec::new(),
        }
    }
}

/// `log`: a page of changesets, newest first, following first parents.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct LogPage {
    /// The changesets.
    pub entries: Vec<ChangesetSummary>,
    /// Whether older history follows this page.
    pub truncated: bool,
    /// When `truncated`: where the next page continues. Opaque: pass it back as given, as the
    /// next request's `cursor`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

impl LogPage {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(entries: Vec<ChangesetSummary>, truncated: bool) -> Self {
        Self {
            entries,
            truncated,
            cursor: None,
        }
    }
}

/// One changeset of a log.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ChangesetSummary {
    /// The changeset's id.
    pub id: String,
    /// The parents' ids, mainline parent first.
    pub parents: Vec<String>,
    /// The agent that wrote it, as `show` gives `author.agent`.
    pub author_agent: String,
    /// When it was recorded: the committer signature's time, in Unix milliseconds.
    pub created_at: u64,
    /// The message's first line.
    pub title: String,
}

impl ChangesetSummary {
    /// Takes every field.
    pub fn new(
        id: impl Into<String>,
        parents: Vec<String>,
        author_agent: impl Into<String>,
        created_at: u64,
        title: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            parents,
            author_agent: author_agent.into(),
            created_at,
            title: title.into(),
        }
    }
}

/// `path_history` (CLI `history`): the changesets that changed what a path holds, newest first,
/// following first parents, a page at a time.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PathHistoryReq {
    /// Where to start; `main` when omitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rev: Option<Rev>,
    /// The repository path: a file, whose history is that of its content and kind, or a
    /// directory, whose history is that of everything below it. One trailing `/` is taken.
    pub path: String,
    /// The most entries to return: at least 1 (0 is `E_INVALID_ARG`).
    #[serde(default = "default_path_history_limit")]
    #[schemars(range(min = 1))]
    pub limit: usize,
    /// Resume a history: the previous page's `cursor`. Pass it back exactly as returned; do not
    /// construct it (cursors are opaque). This page continues from the first parent of the
    /// changeset it names, and `rev` is not used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

impl PathHistoryReq {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            rev: None,
            path: path.into(),
            limit: default_path_history_limit(),
            cursor: None,
        }
    }
}

/// `path_history` (CLI `history`): a page of a path's history, newest first.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PathHistoryPage {
    /// The path, without a trailing `/`.
    pub path: String,
    /// The changesets that changed what it holds.
    pub entries: Vec<PathHistoryEntry>,
    /// Whether an older changeset that changed it follows this page.
    pub truncated: bool,
    /// When `truncated`: where the next page continues. Opaque: pass it back as given, as the
    /// next request's `cursor`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

impl PathHistoryPage {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(path: impl Into<String>, entries: Vec<PathHistoryEntry>, truncated: bool) -> Self {
        Self {
            path: path.into(),
            entries,
            truncated,
            cursor: None,
        }
    }
}

/// One changeset of a path's history, and what the path holds after it.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PathHistoryEntry {
    /// The changeset's id.
    pub changeset: String,
    /// `put` when something is at the path after the changeset, `delete` when nothing is.
    pub op: ChangeOp,
    /// What is at the path after it; omitted after a `delete`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<EntryKindOut>,
    /// A blob id for a file, an executable or a symlink; a tree id for a directory; omitted after
    /// a `delete`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The agent that wrote the changeset, as `log` gives it.
    pub author_agent: String,
    /// When it was recorded: the committer signature's time, in Unix milliseconds.
    pub created_at: u64,
    /// The message's first line.
    pub title: String,
}

impl PathHistoryEntry {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(
        changeset: impl Into<String>,
        op: ChangeOp,
        author_agent: impl Into<String>,
        created_at: u64,
        title: impl Into<String>,
    ) -> Self {
        Self {
            changeset: changeset.into(),
            op,
            kind: None,
            id: None,
            author_agent: author_agent.into(),
            created_at,
            title: title.into(),
        }
    }
}

/// `diff`: the files that differ between two revisions, in path order, a page at a time.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DiffReq {
    /// The revision to diff from: the old side.
    pub from: Rev,
    /// The revision to diff to: the new side.
    pub to: Rev,
    /// Only the files at or below these repository paths (a file's path takes that file; one
    /// trailing `/` is taken); every file when empty or omitted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    /// What to say of each file: `names` its path and status; `stat` also whether it is binary
    /// and, for text, the lines it inserts and deletes; `unified` also its kinds, its blobs and,
    /// for text, its hunks.
    #[serde(default)]
    pub format: DiffFormat,
    /// The unchanged lines around each change in a `unified` hunk.
    #[serde(default = "default_diff_context")]
    pub context: u32,
    /// The most files to return: at least 1 (0 is `E_INVALID_ARG`).
    #[serde(default = "default_diff_limit")]
    #[schemars(range(min = 1))]
    pub limit: usize,
    /// Resume a diff: the previous page's `cursor`. Pass it back exactly as returned; do not
    /// construct it (cursors are opaque). This page starts after the file it names. It pins no
    /// revision: give the first page's `from` and `to`, full ids, as `from` and `to`, or a ref
    /// that moved since pages another diff, with no error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// The most bytes a `unified` page's files take, as the JSON array `files` they print as: at
    /// least 1 (0 is `E_INVALID_ARG`). The page stops before the first file that would take it
    /// past this, but always holds one file, whose hunks are cut to the first ones that fit
    /// (`hunks_truncated`) when they alone would pass it. A `names` or `stat` page is held by
    /// `limit` alone.
    #[serde(default = "default_diff_max_bytes")]
    #[schemars(range(min = 1))]
    pub max_bytes: u64,
}

impl DiffReq {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(from: impl Into<Rev>, to: impl Into<Rev>) -> Self {
        Self {
            from: from.into(),
            to: to.into(),
            paths: Vec::new(),
            format: DiffFormat::default(),
            context: default_diff_context(),
            limit: default_diff_limit(),
            cursor: None,
            max_bytes: default_diff_max_bytes(),
        }
    }
}

/// What `diff` says of each file.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DiffFormat {
    /// Its path and status.
    #[default]
    Names,
    /// Also whether it is binary and, for text, the lines it inserts and deletes.
    Stat,
    /// Also its kinds, its blobs and, for text, its hunks.
    Unified,
}

/// `diff`: a page of the files that differ between two revisions, in path order (by bytes).
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Diff {
    /// The changeset `from` named.
    pub from: String,
    /// The changeset `to` named.
    pub to: String,
    /// The files.
    pub files: Vec<FileDiff>,
    /// Whether more files follow this page.
    pub truncated: bool,
    /// When `truncated`: where the next page starts. Opaque: pass it back as given, as the next
    /// request's `cursor`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

impl Diff {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(
        from: impl Into<String>,
        to: impl Into<String>,
        files: Vec<FileDiff>,
        truncated: bool,
    ) -> Self {
        Self {
            from: from.into(),
            to: to.into(),
            files,
            truncated,
            cursor: None,
        }
    }
}

/// One file that differs: a file, an executable or a symlink whose content or kind changed, or
/// that is on one side only. A directory never is one: a file that becomes a directory is that
/// file deleted and the directory's files added. No rename is detected: a file that moved is
/// one deleted and one added. Which fields follow `status` depends on the request's `format`,
/// and a field that says nothing is omitted.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct FileDiff {
    /// The repository path.
    pub path: String,
    /// What happened to it.
    pub status: DiffStatus,
    /// `unified`: its kind in `from`; omitted for an added file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_kind: Option<FileKind>,
    /// `unified`: its kind in `to`; omitted for a deleted file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_kind: Option<FileKind>,
    /// `unified`: its blob id in `from`; omitted for an added file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_blob: Option<String>,
    /// `unified`: its blob id in `to`; omitted for a deleted file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_blob: Option<String>,
    /// `stat` and `unified`: whether it is binary, which gets no line counts and no hunks: a side
    /// that is not UTF-8, or holds a NUL in its first 8,000 bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binary: Option<bool>,
    /// `stat` and `unified`, for text within the line diff's bound: the lines the change
    /// inserts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub insertions: Option<u64>,
    /// `stat` and `unified`, for text within the line diff's bound: the lines the change
    /// deletes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deletions: Option<u64>,
    /// `unified`, for text: the hunks, in order; empty when only the exec bit or the kind
    /// changed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hunks: Option<Vec<DiffHunk>>,
    /// `stat` and `unified`: true for text past the line diff's bound (49,152 lines or 8 MiB a
    /// side), which gets no `insertions`, `deletions` or hunks (`hunks` empty for `unified`);
    /// and for `unified`, when `hunks` stops before the file's last hunk, to keep the page within
    /// `max_bytes`, `insertions` and `deletions` counting every line all the same. Omitted
    /// otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hunks_truncated: Option<bool>,
}

impl FileDiff {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(path: impl Into<String>, status: DiffStatus) -> Self {
        Self {
            path: path.into(),
            status,
            old_kind: None,
            new_kind: None,
            old_blob: None,
            new_blob: None,
            binary: None,
            insertions: None,
            deletions: None,
            hunks: None,
            hunks_truncated: None,
        }
    }
}

/// What happened to a file between the two revisions.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DiffStatus {
    /// It is in `to` only: in `from`, nothing or a directory is there.
    Added,
    /// It is in `from` only.
    Deleted,
    /// Its content or its exec bit changed: a file or an executable on both sides, or a
    /// symlink on both sides.
    Modified,
    /// A symlink on one side, a file or an executable on the other.
    TypeChanged,
}

/// A hunk of a unified diff: one or more changes and the unchanged lines around them, with the
/// numbers of its header, `@@ -old_start,old_lines +new_start,new_lines @@`.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DiffHunk {
    /// The first old line it holds, counted from 1; when it holds none, the line before where
    /// they would be (0 at the top), as git prints it.
    pub old_start: u64,
    /// How many old lines it holds: its context and removed lines.
    pub old_lines: u64,
    /// The first new line it holds, counted as `old_start` is.
    pub new_start: u64,
    /// How many new lines it holds: its context and added lines.
    pub new_lines: u64,
    /// Its lines, in order; each change's removed lines come before its added lines.
    pub lines: Vec<DiffLineOut>,
}

impl DiffHunk {
    /// Takes every field.
    pub fn new(
        old_start: u64,
        old_lines: u64,
        new_start: u64,
        new_lines: u64,
        lines: Vec<DiffLineOut>,
    ) -> Self {
        Self {
            old_start,
            old_lines,
            new_start,
            new_lines,
            lines,
        }
    }
}

/// One line of a hunk.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DiffLineOut {
    /// Whether both sides have it, only the new one, or only the old one.
    pub op: LineOp,
    /// The line with its line break, a carriage return before it kept; a file's last line has
    /// none when the file does not end in one.
    pub text: String,
}

impl DiffLineOut {
    /// Takes every field.
    pub fn new(op: LineOp, text: impl Into<String>) -> Self {
        Self {
            op,
            text: text.into(),
        }
    }
}

/// Which side of a diff a hunk's line is on.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum LineOp {
    /// Both: an unchanged line around a change.
    Context,
    /// The new side only.
    Add,
    /// The old side only.
    Remove,
}

/// `ref list`: list every ref, or those whose names start with a prefix.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct RefListReq {
    /// Only the refs whose names start with these bytes (`release/`, `tags/`); omitted for
    /// every ref.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
}

impl RefListReq {
    /// Every ref, with `prefix` the ones whose names start with it.
    pub fn new(prefix: Option<String>) -> Self {
        Self { prefix }
    }
}

/// What a ref's name names.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RefKind {
    /// `main`.
    Main,
    /// `release/<name>`.
    Release,
    /// A free-form branch (FORMAT 2).
    Branch,
    /// `tags/<path>`: created and deleted, never moved (FORMAT 2).
    Tag,
    /// `remote/<remote>/<path>`: a remote's ref as a fetch last saw it (FORMAT 2).
    RemoteTracking,
}

/// `ref list`: every ref.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RefListResp {
    /// The refs.
    pub refs: Vec<RefOut>,
}

impl RefListResp {
    /// Takes every field.
    pub fn new(refs: Vec<RefOut>) -> Self {
        Self { refs }
    }
}

/// One ref.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RefOut {
    /// Its name: `main`, `release/<name>`, or in a FORMAT 2 store a branch, `tags/<path>` or
    /// `remote/<remote>/<path>`.
    pub name: String,
    /// The changeset it points at.
    pub target: String,
    /// How many committed moves changed its target.
    pub version: u64,
    /// What its name names; omitted by stores older than this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<RefKind>,
}

impl RefOut {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(name: impl Into<String>, target: impl Into<String>, version: u64) -> Self {
        Self {
            name: name.into(),
            target: target.into(),
            version,
            kind: None,
        }
    }
}

/// `ref set`: move a ref, or create one, by compare-and-swap. Give exactly one of
/// `expected_old` and `create`; both or neither is `E_INVALID_ARG`.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[schemars(transform = one_of_expected_old_and_create)]
#[non_exhaustive]
pub struct RefSetReq {
    /// The ref: `main` or `release/<name>`.
    pub name: String,
    /// The revision to point it at.
    pub new: Rev,
    /// To move an existing ref: the target the caller last saw, by its full changeset id; if the
    /// ref holds another, the move fails with `E_CAS_MISMATCH`. A ref name or a relative
    /// revision is read from the store when the move runs instead of pinning the id the caller
    /// read: the ref's own name always matches, so the compare-and-swap protects nothing, and
    /// `main~1` on `main` never matches. An id prefix pins the id the caller read only until
    /// another changeset shares it; then it is `E_AMBIGUOUS_ID`. `swarmops mcp` refuses all
    /// three (`E_INVALID_ARG`), and the CLI's `--expect` help warns against them. Give this or
    /// `create`, not both.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_old: Option<Rev>,
    /// To create the ref. If it exists at another target, the move fails with `E_CAS_MISMATCH`;
    /// if it already holds `new`, nothing is written and the response is a replay
    /// (`already_applied`). Give this or `expected_old`, not both.
    #[serde(default)]
    pub create: bool,
}

impl RefSetReq {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(name: impl Into<String>, new: impl Into<Rev>) -> Self {
        Self {
            name: name.into(),
            new: new.into(),
            expected_old: None,
            create: false,
        }
    }
}

/// States in `RefSetReq`'s schema what `ref set` checks: exactly one of `expected_old` (a
/// string) and `create` (`true`). Both, and neither, meet no branch, as the operation refuses
/// them; an `expected_old` of null is no expectation, so with `create: true` it meets the second
/// branch, as the operation takes it, and without it no branch. The wire does not change.
fn one_of_expected_old_and_create(schema: &mut schemars::Schema) {
    schema.insert(
        "oneOf".to_owned(),
        serde_json::json!([
            {"required": ["expected_old"],
             "properties": {"expected_old": {"type": "string"}, "create": {"const": false}}},
            {"required": ["create"],
             "properties": {"create": {"const": true}, "expected_old": {"type": "null"}}},
        ]),
    );
}

/// `ref set` and `ref delete`: the move that was made.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RefSetResp {
    /// The ref.
    pub name: String,
    /// What the ref held before a move that was applied. On a replay (`already_applied`) it is
    /// `expected_old` as resolved, which the ref need not have held; omitted when `create` was
    /// given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old: Option<String>,
    /// Its target after; omitted when the ref was deleted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new: Option<String>,
    /// The ref was already in the requested state, so nothing changed and a replay is safe;
    /// `old` and `new` are then the request's `expected_old` and `new`, resolved to full ids.
    pub already_applied: bool,
    /// Problems found after the move became durable, such as failed housekeeping; the move
    /// itself succeeded. Omitted when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

impl RefSetResp {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(name: impl Into<String>, already_applied: bool) -> Self {
        Self {
            name: name.into(),
            old: None,
            new: None,
            already_applied,
            warnings: Vec::new(),
        }
    }
}

/// `ref delete`: remove a ref by compare-and-swap. `main` is never deleted
/// (`E_REF_PROTECTED`).
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RefDeleteReq {
    /// The ref.
    pub name: String,
    /// The target the caller last saw, by its full changeset id; if the ref holds another, the
    /// delete fails with `E_CAS_MISMATCH`. A ref name or a relative revision is read from the
    /// store when the delete runs instead of pinning the id the caller read: the ref's own name
    /// always matches, so the compare-and-swap protects nothing, and `main~1` on `main` never
    /// matches. An id prefix pins the id the caller read only until another changeset shares
    /// it; then it is `E_AMBIGUOUS_ID`. `swarmops mcp` refuses all three (`E_INVALID_ARG`), and
    /// the CLI's `--expect` help warns against them.
    pub expected_old: Rev,
}

impl RefDeleteReq {
    /// Takes every field.
    pub fn new(name: impl Into<String>, expected_old: impl Into<Rev>) -> Self {
        Self {
            name: name.into(),
            expected_old: expected_old.into(),
        }
    }
}

/// `verify`: check the whole store's consistency.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct VerifyReq {
    /// Re-hash every object, and check every derived value of the built-in derivations (the
    /// first-parent skip lists, the last touches of paths) against the objects. When `false`,
    /// verify checks structure only: parents, that referenced objects exist, the index against
    /// the journal, and refs.
    #[serde(default)]
    pub deep: bool,
    /// The most problems the report lists, 1 to 1,000, also the default; `found` counts them
    /// all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_findings: Option<u32>,
}

impl VerifyReq {
    /// A verify, `deep` or of structure only; the others get their serde defaults.
    pub fn new(deep: bool) -> Self {
        Self {
            deep,
            max_findings: None,
        }
    }
}

/// `verify`: what the consistency check found.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct VerifyReport {
    /// How many objects were checked.
    pub objects_checked: u64,
    /// How many changesets were checked.
    pub changesets_checked: u64,
    /// One line per problem found, for a reader of lines; empty for a clean store. `findings`
    /// holds the same problems, at the same indexes, for a program.
    pub problems: Vec<String>,
    /// One finding per problem found, for a program to act on: `findings[i]` is the problem
    /// that the line `problems[i]` describes. Empty for a clean store; a report without it
    /// reads as having none.
    #[serde(default)]
    pub findings: Vec<VerifyFinding>,
    /// The report lists the first `max_findings` problems only; `found` counts them all.
    #[serde(default)]
    pub truncated: bool,
    /// How many problems were found, listed or not; 0 from a store older than this field.
    #[serde(default)]
    pub found: u64,
}

impl VerifyReport {
    /// Takes the required fields; `findings` is empty, `truncated` false and `found` the
    /// problems' count.
    pub fn new(objects_checked: u64, changesets_checked: u64, problems: Vec<String>) -> Self {
        Self {
            objects_checked,
            changesets_checked,
            found: u64::try_from(problems.len()).unwrap_or(u64::MAX),
            problems,
            findings: Vec::new(),
            truncated: false,
        }
    }
}

/// `gc`: remove the store's leftovers (temp files past a day, a crashed create's staging,
/// derived values of older versions, what an upgrade left of the format before). It removes no
/// object and no changeset.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct GcReq {
    /// List what it would remove, and remove nothing.
    #[serde(default)]
    pub dry_run: bool,
}

impl GcReq {
    /// A gc that removes, or with `dry_run` only lists.
    pub fn new(dry_run: bool) -> Self {
        Self { dry_run }
    }
}

/// `gc`: what it removed, or would remove for a dry run.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct GcResp {
    /// How many leftovers of each kind; kinds with none are omitted.
    pub removed: Vec<GcCount>,
    /// The bytes of their files.
    pub bytes: u64,
    /// How many object files no changeset of the store reaches: counted, never removed.
    pub orphan_objects: u64,
}

impl GcResp {
    /// Takes every field.
    pub fn new(removed: Vec<GcCount>, bytes: u64, orphan_objects: u64) -> Self {
        Self {
            removed,
            bytes,
            orphan_objects,
        }
    }
}

/// `gc`: how many leftovers of one kind.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct GcCount {
    /// `object_temp`, `derived_temp`, `meta_staging`, `derived_version` or `upgrade_residue`.
    /// A string, as `OpError.code` is, so that a client can read kinds newer than its own list.
    pub kind: String,
    /// How many.
    pub count: u64,
}

impl GcCount {
    /// Takes every field.
    pub fn new(kind: impl Into<String>, count: u64) -> Self {
        Self {
            kind: kind.into(),
            count,
        }
    }
}

/// `repair`: rebuild what the journal can rebuild (`journal.off`, the index, the ref files),
/// quarantine damaged objects and drop damaged cache entries. It never cuts the journal.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct RepairReq {
    /// List what it would do, each with `done: false`, and write nothing.
    #[serde(default)]
    pub dry_run: bool,
}

impl RepairReq {
    /// A repair that mends, or with `dry_run` only lists.
    pub fn new(dry_run: bool) -> Self {
        Self { dry_run }
    }
}

/// `repair`: what it did, and what a verify after it found.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RepairResp {
    /// What it did, or would do, in order.
    pub actions: Vec<RepairAction>,
    /// The verify after it.
    pub after: VerifySummary,
}

impl RepairResp {
    /// Takes every field.
    pub fn new(actions: Vec<RepairAction>, after: VerifySummary) -> Self {
        Self { actions, after }
    }
}

/// `repair`: one thing it did, or would do.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RepairAction {
    /// `rebuild_offsets`, `rebuild_index`, `rebuild_ref`, `quarantine_object`, `drop_derived`
    /// or `drop_external_id`. A string, so that a client can read kinds newer than its own list.
    pub kind: String,
    /// On what: an object id, a ref name or a file class; never a path.
    pub target: String,
    /// Whether it was done: `false` for a dry run.
    pub done: bool,
}

impl RepairAction {
    /// Takes every field.
    pub fn new(kind: impl Into<String>, target: impl Into<String>, done: bool) -> Self {
        Self {
            kind: kind.into(),
            target: target.into(),
            done,
        }
    }
}

/// What a verify found, in counts: `repair`'s `after`.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct VerifySummary {
    /// How many objects were checked.
    pub objects_checked: u64,
    /// How many changesets were checked.
    pub changesets_checked: u64,
    /// How many problems were found; 0 for a sound store.
    pub found: u64,
}

impl VerifySummary {
    /// Takes every field.
    pub fn new(objects_checked: u64, changesets_checked: u64, found: u64) -> Self {
        Self {
            objects_checked,
            changesets_checked,
            found,
        }
    }
}

/// `upgrade`: move the store to the latest format this build writes. It cannot be undone:
/// older releases do not open the store afterwards.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct UpgradeReq {}

impl UpgradeReq {
    /// An upgrade: it has no options.
    pub fn new() -> Self {
        Self {}
    }
}

/// `upgrade`: the formats before and after.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct UpgradeResp {
    /// The format the store had.
    pub from: u32,
    /// The format it has now.
    pub to: u32,
    /// Whether it changed: `false` for a store of the latest format already.
    pub upgraded: bool,
}

impl UpgradeResp {
    /// Takes every field.
    pub fn new(from: u32, to: u32, upgraded: bool) -> Self {
        Self { from, to, upgraded }
    }
}

/// One problem `verify` found, as a program reads it: what kind of damage it is, and what it
/// names. The keys its kind does not use are omitted; the line at the same index of `problems`
/// says the same for a reader. Ids are lowercase hex.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct VerifyFinding {
    /// What is damaged: `corrupt_object`, `missing_object`, `missing_parent`, `root_mismatch`,
    /// `dangling_ref`, `corrupt_ref`, `index_mismatch`, `journal_corrupt`, `meta_corrupt`,
    /// `external_id_corrupt`, `external_id_dangling`, `external_id_mismatch` or
    /// `derived_mismatch`. A string, as `OpError.code` is, so that a client can read kinds newer
    /// than its own list.
    pub kind: String,
    /// The object: one whose content does not hash to its id or does not decode
    /// (`corrupt_object`), one a changeset references that the store lacks, or holds as
    /// another kind (`missing_object`), or the tree or blob a git export oid memo
    /// (`git_tree_oid`, `git_blob_oid`, `git_tree_oid_mac`, `git_blob_oid_mac`) is about
    /// (`derived_mismatch`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub object: Option<String>,
    /// The changeset: the first, in seq order, that references the missing object
    /// (`missing_object`), the one that names a missing parent (`missing_parent`), the one whose
    /// root is not its first parent's root with its delta applied (`root_mismatch`), the one
    /// an external id's entry names: not in the store (`external_id_dangling`), or with a
    /// `git.oid` that is another oid than the entry's key, or, found by a deep verify, with none
    /// and exporting to another commit (`external_id_mismatch`), or the one a derived value is
    /// about (`derived_mismatch`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changeset: Option<String>,
    /// The parent the changeset names, which the store does not hold (`missing_parent`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// The changeset record's place in the index, from 0 (`index_mismatch`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    /// Where the corrupt frame starts in `journal.log` (`journal_corrupt`); nothing after it
    /// is read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<u64>,
    /// The ref: one that names no changeset in the store (`dangling_ref`), or whose file does
    /// not read as a ref (`corrupt_ref`).
    #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
    pub ref_name: Option<String>,
    /// What disagrees with the record (`index_mismatch`), which says what mends it: `journal`
    /// (rebuild the index from the journal), `sorted_view` (rebuild the sorted views from the
    /// index columns) or `object` (derive the record from its object again).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cause: Option<String>,
    /// The system of the external id index whose entry it is: `git`, whose keys are git commit
    /// oids, or another (`external_id_corrupt`, `external_id_dangling`, `external_id_mismatch`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    /// The key, in lowercase hex: the external id whose entry it is, a git commit's oid for
    /// system `git` (`external_id_corrupt`, `external_id_dangling`, `external_id_mismatch`); or a
    /// derived value's key, its postcard bytes, omitted for a derivation whose key is `()`
    /// (`derived_mismatch`: a path's for `last_modified`; for a memo under a memo key, a `_mac`
    /// derivation, the id of the key it was made under).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// The derivation whose stored value is not the one the objects give: `first_parent_skip_list`,
    /// `last_modified`, or a git export oid memo `git_tree_oid`, `git_blob_oid` or `git_oid`, or
    /// one of them under a memo key, `git_tree_oid_mac`, `git_blob_oid_mac` or `git_oid_mac`,
    /// whose value, or under the store's key whose MAC, does not check (`derived_mismatch`, which
    /// a deep verify finds; `git_oid` and `git_oid_mac` name their `changeset`, the tree and blob
    /// memos their `object`). Removing the value's file under `meta/derived/<derivation>/`, or all
    /// of `meta/derived/`, has it derived again.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub derivation: Option<String>,
}

impl VerifyFinding {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(kind: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            object: None,
            changeset: None,
            parent: None,
            seq: None,
            offset: None,
            ref_name: None,
            cause: None,
            system: None,
            key: None,
            derivation: None,
        }
    }
}

fn default_git_ref() -> String {
    "HEAD".to_owned()
}

fn default_target_ref() -> String {
    "main".to_owned()
}

/// `git import`: bring a git repository's history into the store. Every commit that `git_ref`
/// reaches and the store does not hold yet becomes a changeset, parents first, and
/// `target_ref` moves to the changeset of `git_ref`'s commit. It runs on the caller's host, as
/// `init` does: no server receives this request, and it is no MCP tool. The git oids it takes
/// for the target's head, its trees and its blobs are derived from the store's objects, and
/// each memo it meets is checked against them; with the store's memo key, which the CLI keeps
/// outside the store, a memo whose MAC checks is taken, and nothing below it read. A memo that
/// disagrees, or whose MAC does not check, is `E_STORE_CORRUPT`, with nothing written.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct GitImportReq {
    /// The git repository to read, on the caller's host: its directory, absolute or relative to
    /// the caller's working directory (a bare repository, or a work tree with its `.git`). The
    /// caller reads it; no store or server is ever asked to read a path.
    pub git_path: String,
    /// The commit to import, with every commit it reaches: `HEAD` (the default), a ref name,
    /// short or full (`stable`, `refs/heads/stable`), looked up in git's order, or a commit id
    /// of 40 hex digits; a tag is peeled to its commit. There is no revision syntax such as
    /// `main~3`.
    #[serde(default = "default_git_ref")]
    pub git_ref: String,
    /// The ref to move to that commit's changeset: `main` (the default) or `release/<name>`,
    /// created if it does not exist. If it does, its head must be the import of that commit or
    /// of an ancestor of it, and only the commits after that head are imported; any other head
    /// (the source was rewritten, or a native changeset was committed on the ref) is
    /// `E_GIT_IMPORT_DIVERGED`, and nothing is written.
    #[serde(default = "default_target_ref")]
    pub target_ref: String,
    /// The classes of content the import may drop or normalize; a changeset that loses any
    /// names them in its `git.lossy` metadata. Empty by default: a commit the import converts
    /// that holds any class is refused with `E_GIT_IMPORT_LOSSY`, which names the commit, the
    /// path and the class. A commit the store holds already is reused as it is, even one an
    /// earlier import took with a loss.
    #[serde(default)]
    pub lossy: Vec<LossyClass>,
}

impl GitImportReq {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(git_path: impl Into<String>) -> Self {
        Self {
            git_path: git_path.into(),
            git_ref: default_git_ref(),
            target_ref: default_target_ref(),
            lossy: Vec::new(),
        }
    }
}

/// A class of git content that a changeset cannot hold as git has it (storage spec §7). A
/// commit holding one is refused unless the import's `lossy` allows its class; an allowed class
/// is dropped or normalized, a directory a drop leaves empty is dropped with it, and the
/// changeset's `git.lossy` metadata names the classes, since it no longer renders back to the
/// commit's own git id.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum LossyClass {
    /// A submodule's entry (git mode 160000): dropped.
    Gitlinks,
    /// A name that is no path component: not UTF-8, `.`, `..`, empty, or holding a `/`:
    /// dropped.
    Paths,
    /// A mode written other than git's own `100644`, `100755`, `120000` and `40000`, such as
    /// `100664` or a zero-padded `040000`: a regular file's becomes a file or an executable by
    /// its owner's execute bit, and `040000` a directory.
    Modes,
    /// A tree whose entries are not in git's order, or that names one entry twice: put in order,
    /// the first of a name kept.
    TreeOrder,
    /// An entry naming an empty tree: dropped.
    EmptyTrees,
    /// A commit whose bytes the changeset cannot keep: a signature line, time or header line that
    /// renders back differently, no blank line before the message, or a parent listed twice
    /// (kept once).
    Nonstandard,
}

impl LossyClass {
    /// Every class, in declaration order.
    pub const ALL: [LossyClass; 6] = [
        LossyClass::Gitlinks,
        LossyClass::Paths,
        LossyClass::Modes,
        LossyClass::TreeOrder,
        LossyClass::EmptyTrees,
        LossyClass::Nonstandard,
    ];

    /// The class's name, as the wire, the CLI's `--lossy` and `git.lossy` metadata spell it,
    /// e.g. `"tree_order"`.
    pub fn as_str(self) -> &'static str {
        match self {
            LossyClass::Gitlinks => "gitlinks",
            LossyClass::Paths => "paths",
            LossyClass::Modes => "modes",
            LossyClass::TreeOrder => "tree_order",
            LossyClass::EmptyTrees => "empty_trees",
            LossyClass::Nonstandard => "nonstandard",
        }
    }
}

/// `git import`: what the import brought in, and where the target ref stands.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct GitImportResp {
    /// How many commits became changesets in this import.
    pub imported: u64,
    /// How many commits the import found in the store already, where its walk stopped: the
    /// commit of the target's head, or commits an earlier import made. Importing the commit the
    /// target names already imports 0 and skips 1.
    pub skipped: u64,
    /// The changeset of `git_ref`'s commit, which `target_ref` names now.
    pub head_changeset: String,
    /// `git_ref`'s commit: its git id, 40 lowercase hex digits.
    pub git_head_sha: String,
    /// The ref the import moved, or found at `head_changeset` already.
    pub target_ref: String,
    /// Whether this import moved `target_ref`, or created it; `false` when the ref named
    /// `head_changeset` already, after an earlier import of the same commit or a concurrent one.
    pub ref_moved: bool,
    /// What `target_ref` named when the import started; omitted when it did not exist, and the
    /// import created it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_head: Option<String>,
    /// The commits this import took lossily, as the request's `lossy` allowed: the first 100,
    /// in the order imported. `lossy_total` counts them all.
    pub lossy: Vec<LossyCommitOut>,
    /// How many commits this import took lossily, the ones past the first 100 included.
    pub lossy_total: u64,
    /// Problems the import met that did not stop it, such as failed housekeeping after the
    /// target's move became durable; the import itself succeeded. Omitted when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

impl GitImportResp {
    /// Takes the required fields; the others get their serde defaults.
    // Every argument is a required field of the response: no default can stand in.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        imported: u64,
        skipped: u64,
        head_changeset: impl Into<String>,
        git_head_sha: impl Into<String>,
        target_ref: impl Into<String>,
        ref_moved: bool,
        lossy: Vec<LossyCommitOut>,
        lossy_total: u64,
    ) -> Self {
        Self {
            imported,
            skipped,
            head_changeset: head_changeset.into(),
            git_head_sha: git_head_sha.into(),
            target_ref: target_ref.into(),
            ref_moved,
            old_head: None,
            lossy,
            lossy_total,
            warnings: Vec::new(),
        }
    }
}

/// `git import`: a commit taken lossily, and what it lost.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct LossyCommitOut {
    /// The git commit: its id, 40 lowercase hex digits.
    pub commit: String,
    /// The changeset made of it, whose `git.lossy` metadata names the same classes.
    pub changeset: String,
    /// The classes dropped or normalized in it, ordered by name, as `git.lossy` lists them.
    pub classes: Vec<LossyClass>,
}

impl LossyCommitOut {
    /// Takes every field.
    pub fn new(
        commit: impl Into<String>,
        changeset: impl Into<String>,
        classes: Vec<LossyClass>,
    ) -> Self {
        Self {
            commit: commit.into(),
            changeset: changeset.into(),
            classes,
        }
    }
}

fn default_source_ref() -> String {
    "main".to_owned()
}

fn default_branch() -> String {
    "main".to_owned()
}

/// `git export`: write the history behind a store ref into a branch of a git repository. Every
/// commit that the branch's last export does not reach is written, parents first: a changeset
/// that was imported from git keeps the commit it came from, and one made in the store becomes a
/// commit with its own signatures and `SwarmOps-*` trailers. The branch moves to the export of
/// the ref's head by compare-and-swap, and only once everything it reaches is durable. It runs
/// on the caller's host, as `git import` does: no server receives this request, and it is no MCP
/// tool. It derives the export oids it compares the branch with, names as parents and moves the
/// branch to, and takes or refuses memos, as `git import` does: with the store's memo key, a
/// memo whose MAC checks stops the walk of the history below it.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct GitExportReq {
    /// The git repository to write, on the caller's host: its directory, absolute or relative to
    /// the caller's working directory (a bare repository, or a work tree with its `.git`). It
    /// exists already: `git init --bare` makes one. The caller reads and writes it; no store or
    /// server is ever asked to read or write a path.
    pub git_path: String,
    /// The store ref whose history to export: `main` (the default) or `release/<name>`. Its head
    /// is the commit the branch ends at.
    #[serde(default = "default_source_ref")]
    pub source_ref: String,
    /// The git branch to write, by its short name (`main`, the default, or `stable`; the
    /// repository's `refs/heads/<branch>`), created if it does not exist. If it does, its commit
    /// must be the export of `source_ref`'s head or of an ancestor of it, and only the commits
    /// after that export are written; a branch at any other commit is `E_GIT_EXPORT_DIVERGED`,
    /// and nothing is written. In a repository with a work tree, the branch that `HEAD` names is
    /// refused (`E_INVALID_ARG`, with `details.reason` `checked_out`), as `git push` refuses it
    /// by default.
    #[serde(default = "default_branch")]
    pub branch: String,
    /// Move the branch although it diverged: to the export of `source_ref`'s head, by
    /// compare-and-swap against the commit the export read, so that a branch another writer
    /// moves meanwhile still stops it. It does not override a checked-out branch. Off by default.
    #[serde(default)]
    pub force: bool,
}

impl GitExportReq {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(git_path: impl Into<String>) -> Self {
        Self {
            git_path: git_path.into(),
            source_ref: default_source_ref(),
            branch: default_branch(),
            force: false,
        }
    }
}

/// `git export`: what the export wrote, and where the branch stands.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct GitExportResp {
    /// How many commits this export wrote to the repository: the commits of the ref's history
    /// that the branch's last export did not reach and the repository did not hold. A branch that
    /// is up to date exports 0.
    pub exported: u64,
    /// How many git objects (commits, trees and blobs) this export wrote to the repository.
    pub objects_written: u64,
    /// The changeset at `source_ref`'s head, which `head_sha` is the export of.
    pub head_changeset: String,
    /// The commit the branch names now, the export of `head_changeset`: its git id, 40 lowercase
    /// hex digits.
    pub head_sha: String,
    /// The branch the export wrote, by its short name.
    pub branch: String,
    /// The commit the branch named when the export started; omitted when the branch did not
    /// exist, and the export created it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_sha: Option<String>,
    /// Whether this export moved the branch, or created it; `false` when the branch named
    /// `head_sha` already, after an earlier export of the same head or a concurrent one.
    pub branch_moved: bool,
    /// Whether the branch held an earlier export that this one continued, so that only the
    /// commits after it were written; `false` for a branch the export created, or one that
    /// `force` moved from a history that was no export of this one.
    pub incremental: bool,
    /// How many of the exported commits have another id than the commit they were imported from:
    /// a commit the import took lossily (`git.lossy`) renders to other bytes than the git commit
    /// it came from, and so does every commit above it, whose parent has another id now; their
    /// signature headers no longer verify. Commits made in the store were never imported and are
    /// not counted.
    pub rewritten: u64,
    /// What the export has to say that did not stop it, such as how many of its commits it
    /// rewrote (`rewritten`). Omitted when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

impl GitExportResp {
    /// Takes the required fields; the others get their serde defaults.
    // Every argument is a required field of the response: no default can stand in.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        exported: u64,
        objects_written: u64,
        head_changeset: impl Into<String>,
        head_sha: impl Into<String>,
        branch: impl Into<String>,
        branch_moved: bool,
        incremental: bool,
        rewritten: u64,
    ) -> Self {
        Self {
            exported,
            objects_written,
            head_changeset: head_changeset.into(),
            head_sha: head_sha.into(),
            branch: branch.into(),
            old_sha: None,
            branch_moved,
            incremental,
            rewritten,
            warnings: Vec::new(),
        }
    }
}

/// `derive`: derive ahead of any reader the eager derived values (storage spec §5: each
/// changeset's first-parent skip list) of the changesets the journal records after a frame, so
/// that reads find them stored. It writes only the store's cache of derived values, so it takes
/// no identity.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DeriveReq {
    /// Go on after this journal frame: a previous call's `next`. From the journal's first frame
    /// when omitted. A frame past the journal's last is `E_INVALID_ARG`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<u64>,
    /// The most journal frames to read; a frame is one transaction, a commit or a ref's move. 0
    /// reads none and derives nothing: `next` is then the journal's last frame, from where the
    /// next calls derive only what is recorded later.
    #[serde(default = "default_derive_limit")]
    pub limit: usize,
}

impl Default for DeriveReq {
    /// The request a JSON object without fields reads.
    fn default() -> Self {
        Self {
            after: None,
            limit: default_derive_limit(),
        }
    }
}

/// `derive`: how many values one call derived, how many the store held already, and where to
/// go on.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DeriveResp {
    /// How many values it derived: of each eager derivation, one for each changeset its frames
    /// record whose value the store lacked. Values those needed on the way, an older
    /// changeset's, are derived too and not counted.
    pub derived: u64,
    /// How many of those values the store held already.
    pub cached: u64,
    /// The last frame it read: pass it as the next call's `after` to go on, through a journal
    /// longer than `limit` frames or through the frames recorded since. Omitted when it read no
    /// frame, as none follows `after`, the journal's last frame: it is at the journal's end (an
    /// `after` past that is `E_INVALID_ARG`). With `limit` 0, the journal's last frame, omitted
    /// while the journal holds none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<u64>,
}

impl DeriveResp {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(derived: u64, cached: u64) -> Self {
        Self {
            derived,
            cached,
            next: None,
        }
    }
}

/// The default [`LandReq::max_attempts`]: 8 (operations spec §3).
pub const DEFAULT_MAX_ATTEMPTS: u32 = 8;

/// The most [`LandReq::max_attempts`] a land takes: 32.
pub const MAX_LAND_ATTEMPTS: u32 = 32;

/// The default [`LandReq::max_conflict_bytes`]: 32 KiB of hunk text, half the operations spec's
/// 64 KiB target for a default response (§9).
pub const DEFAULT_MAX_CONFLICT_BYTES: u64 = 32 * 1024;

/// The default [`OpLogReq::limit`]: 50 operations.
pub const DEFAULT_OP_LOG_LIMIT: usize = 50;

/// The default [`OpLogReq::max_changesets`]: 20 changesets listed for each operation.
pub const DEFAULT_MAX_CHANGESETS: usize = 20;

fn default_target() -> String {
    "main".to_owned()
}

fn default_max_attempts() -> u32 {
    DEFAULT_MAX_ATTEMPTS
}

fn default_max_conflict_bytes() -> u64 {
    DEFAULT_MAX_CONFLICT_BYTES
}

fn default_op_log_limit() -> usize {
    DEFAULT_OP_LOG_LIMIT
}

fn default_max_changesets() -> usize {
    DEFAULT_MAX_CHANGESETS
}

/// `land`: merge a change three-way onto a target ref and publish it there as one new
/// changeset, in one transaction (operations spec §3). The change is `head` with its
/// first-parent chain after `base`, squashed: the landed changeset's parent is the target's
/// head, so the target stays linear. A conflict is the error `E_CONFLICT`, with the report in
/// `details.report`; nothing is recorded then.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct LandReq {
    /// The changeset to land: the change's head.
    pub head: Rev,
    /// Where the change starts: a first-parent ancestor of `head`, whose own changes are not part
    /// of it (anything else is `E_INVALID_ARG`). It need not be on the target. When omitted, the
    /// merge base of `head` and the target's head, along first parents.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<Rev>,
    /// The ref to land on: `main` or `release/<name>`, which must exist.
    #[serde(default = "default_target")]
    pub target: String,
    /// The landed changeset's message; `head`'s when omitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Makes a retry of this land safe: 1 to 64 letters, digits and `-`, such as a uuid v7. A land
    /// with a key first looks down the target's first parents, to `base`, for a changeset that
    /// landed with the same key: one squashed from this `head` is the replay, which writes
    /// nothing (`status` is `replayed`); one from another head is `E_INVALID_ARG`, a key used
    /// for another change.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    /// How many times to try to publish before failing with `E_CAS_RETRY_EXHAUSTED`: 1 to 32.
    #[serde(default = "default_max_attempts")]
    #[schemars(range(min = 1, max = MAX_LAND_ATTEMPTS))]
    pub max_attempts: u32,
    /// The most bytes of hunk text an `E_CONFLICT` report carries: from the first hunk whose
    /// texts would pass it, no hunk carries its texts (`truncated`), and the hunks' line
    /// positions and the blob ids stay. The report's files are bounded apart from it, as
    /// `ConflictReport.files` says.
    #[serde(default = "default_max_conflict_bytes")]
    pub max_conflict_bytes: u64,
    /// Publish only while the target names this changeset (M5): one attempt, and a target that
    /// names another is `E_CAS_MISMATCH`, nothing written; `max_attempts` does not apply. With a
    /// `head` rebased onto it, the landed tree is the rebased tree. `swarmops mcp` refuses one
    /// that is not a full 64-hex id (`E_INVALID_ARG`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expect_target: Option<Rev>,
    /// What to make of the paths an `E_CONFLICT` report named, decided without a checkout
    /// (operations spec §3, resolution path (b)). Each resolution applies to its path while the
    /// target holds there what `against` held, whether the path conflicts or not; a path the
    /// target changed since is stale, and the land fails `E_CONFLICT` again, with a fresh report
    /// that names it in `stale`. Conflicts left unresolved fail it too. `swarmops mcp` refuses
    /// an `against` that is not a full 64-hex id (`E_INVALID_ARG`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolutions: Option<ResolutionsInput>,
}

impl LandReq {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(head: impl Into<Rev>) -> Self {
        Self {
            head: head.into(),
            base: None,
            target: default_target(),
            message: None,
            idempotency_key: None,
            max_attempts: default_max_attempts(),
            max_conflict_bytes: default_max_conflict_bytes(),
            expect_target: None,
            resolutions: None,
        }
    }
}

/// A land's resolutions (`land --resolve`): what to make of some paths of the merge of the change
/// onto the target, decided against one of the target's heads.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ResolutionsInput {
    /// The target's head the resolutions were decided against: the conflict report's `theirs`,
    /// by its full 64-hex id. A resolution applies while the target holds its path as this
    /// changeset does (the same file, the same directory, or nothing).
    pub against: Rev,
    /// What to make of each path, by its repository path.
    pub files: BTreeMap<String, ResolutionInput>,
}

impl ResolutionsInput {
    /// Takes every field.
    pub fn new(against: impl Into<Rev>, files: BTreeMap<String, ResolutionInput>) -> Self {
        Self {
            against: against.into(),
            files,
        }
    }
}

/// What a land makes of one path, and of every path below it: exactly one of `take`, `content`
/// and `delete` (anything else is `E_INVALID_ARG`, a rule the server checks), the rest of the
/// merge as it merged.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ResolutionInput {
    /// What one side holds there: `ours`, the change's head, or `theirs`, the target's head.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub take: Option<Side>,
    /// One file of new content, given as a commit's change gives it (inline, base64, a client
    /// file, or a stored blob by id), within a commit's limits; nothing below it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<ContentInput>,
    /// The kind of the file `content` makes, only beside it; when omitted, the kind of the file
    /// ours holds there, else theirs', else a regular file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<ModeInput>,
    /// Nothing there: no file at the path, nor below it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub delete: bool,
}

/// One side of a land's merge.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Side {
    /// The change: its head.
    Ours,
    /// The target: its head when the land merges.
    Theirs,
}

/// `land`: where the change landed. Ids are full.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct LandResp {
    /// `landed`, or `replayed` when a land with the same `idempotency_key` had landed the change
    /// already.
    pub status: LandStatus,
    /// The landed changeset: the change squashed into one changeset on the target.
    pub landed: String,
    /// The target's head the change landed on: `landed`'s parent.
    pub rebased_onto: String,
    /// The ref it landed on.
    pub target: String,
    /// The changeset the change was taken from: what `head`'s first-parent chain added after it
    /// is what landed.
    pub base: String,
    /// How many times the land tried to publish.
    pub attempts: u32,
    /// The target moved after the land read it, and the publish put the change on the new head
    /// itself, without another attempt.
    pub relocated: bool,
    /// How many paths the landed changeset changes.
    pub files_changed: u64,
    /// The changesets the land squashed, oldest first: `head`'s first-parent chain after `base`,
    /// ending at `head`.
    pub squashed_from: Vec<String>,
    /// The paths both sides changed whose contents merged cleanly into a content neither side
    /// had. Omitted when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub auto_merged: Vec<String>,
    /// Problems found after the land became durable, such as failed housekeeping; the land
    /// itself succeeded. Omitted when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

impl LandResp {
    /// Takes the required fields; the others get their serde defaults.
    // Every argument is a required field of the response: no default can stand in.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        status: LandStatus,
        landed: impl Into<String>,
        rebased_onto: impl Into<String>,
        target: impl Into<String>,
        base: impl Into<String>,
        attempts: u32,
        relocated: bool,
        files_changed: u64,
        squashed_from: Vec<String>,
    ) -> Self {
        Self {
            status,
            landed: landed.into(),
            rebased_onto: rebased_onto.into(),
            target: target.into(),
            base: base.into(),
            attempts,
            relocated,
            files_changed,
            squashed_from,
            auto_merged: Vec::new(),
            warnings: Vec::new(),
        }
    }
}

/// How a land ended.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum LandStatus {
    /// This request landed the change.
    Landed,
    /// A land with the same `idempotency_key` had landed it already; nothing was written.
    Replayed,
}

/// Why a land could not merge its change onto the target, file by file, or could not apply its
/// resolutions to it (operations spec §3): the report `E_CONFLICT` carries in `details.report`.
/// `ours` is the change, `theirs` the target. Ids are full.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ConflictReport {
    /// The merge base: the changeset the change starts from.
    pub base: String,
    /// The change's head.
    pub ours: String,
    /// The target's head the change was merged with.
    pub theirs: String,
    /// The files that conflict, in path order, while their entries stay within 24 KiB as JSON,
    /// texts aside: the first file always, with at least its first hunk, its hunks past the bound
    /// as one hunk that spans them, without texts; a later file whole, or else left out with
    /// every file after it (`truncated`). The error's message counts every conflicting path.
    pub files: Vec<FileConflictOut>,
    /// The kinds of conflict in `files`, each once.
    pub kinds: Vec<ConflictKindOut>,
    /// The paths both sides changed whose contents merged cleanly into a content neither side
    /// had. Omitted when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub auto_merged: Vec<String>,
    /// The paths whose resolutions did not apply, since the target holds there what their
    /// `against` did not: the target changed them after the report the resolutions answered.
    /// Read them at `theirs` and resolve them again, against it. Omitted when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stale: Vec<String>,
    /// Whether the report left something out: hunk texts, to stay within the request's
    /// `max_conflict_bytes`, or files and hunks, to stay within the bound of `files`.
    pub truncated: bool,
}

impl ConflictReport {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(
        base: impl Into<String>,
        ours: impl Into<String>,
        theirs: impl Into<String>,
        files: Vec<FileConflictOut>,
        kinds: Vec<ConflictKindOut>,
        truncated: bool,
    ) -> Self {
        Self {
            base: base.into(),
            ours: ours.into(),
            theirs: theirs.into(),
            files,
            kinds,
            auto_merged: Vec::new(),
            stale: Vec::new(),
            truncated,
        }
    }
}

/// One conflicting file. Its blobs are the file's content on each side, which `read_file` reads
/// at that side's changeset; a side without a file at the path (deleted, never added, or a
/// directory there) has none.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct FileConflictOut {
    /// The file's repository path.
    pub path: String,
    /// What kind of conflict it is.
    pub kind: ConflictKindOut,
    /// Its blob at the merge base.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_blob: Option<String>,
    /// Its blob in the change.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ours_blob: Option<String>,
    /// Its blob on the target.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theirs_blob: Option<String>,
    /// The places where the two sides' edits of its lines meet, in file order: for a text file
    /// both sides edited (`content` and `add_add`; contents not all text, or past the line
    /// diff's bound of 49,152 lines or 8 MiB a side, conflict as `binary`, with no hunks).
    /// Omitted when there are none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hunks: Vec<HunkOut>,
    /// The newest changeset on the target since the merge base that changed this path: what to
    /// read for the other side's edit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theirs_changeset: Option<String>,
    /// The agent that wrote `theirs_changeset`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theirs_agent: Option<String>,
}

impl FileConflictOut {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(path: impl Into<String>, kind: ConflictKindOut) -> Self {
        Self {
            path: path.into(),
            kind,
            base_blob: None,
            ours_blob: None,
            theirs_blob: None,
            hunks: Vec::new(),
            theirs_changeset: None,
            theirs_agent: None,
        }
    }
}

/// What kind of conflict a file has. `ours` is the change, `theirs` the target.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ConflictKindOut {
    /// Both sides edited the same lines of a text file, or lines next to each other.
    Content,
    /// Both sides changed the content of a file that is not text, or of a symlink, differently.
    Binary,
    /// The change deleted the file, and the target changed it.
    DeleteModify,
    /// The change changed the file, and the target deleted it.
    ModifyDelete,
    /// Both sides added the file, with contents that do not merge.
    AddAdd,
    /// Both sides changed the file's kind (file, executable or symlink), differently.
    Mode,
    /// One side has a file where the other has a directory, or the change deleted a file where
    /// the target made a directory.
    TypeChange,
}

/// One place where the two sides' edits of a text file meet: each side's lines there. The lines
/// are numbered from 1, and each side's range includes both ends, as `read_file` takes a range
/// of lines.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct HunkOut {
    /// Its number in the file, from 0.
    pub id: u32,
    /// The lines at the merge base.
    pub base: LinesOut,
    /// The change's lines.
    pub ours: LinesOut,
    /// The target's lines.
    pub theirs: LinesOut,
}

impl HunkOut {
    /// Takes every field.
    pub fn new(id: u32, base: LinesOut, ours: LinesOut, theirs: LinesOut) -> Self {
        Self {
            id,
            base,
            ours,
            theirs,
        }
    }
}

/// One side's lines of a hunk: `first` to `last`, numbered from 1, both included. A side without
/// lines there has `last` one less than `first`: its place is before line `first`.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct LinesOut {
    /// The first line.
    pub first: u64,
    /// The last line; `first` minus one when the side has none.
    pub last: u64,
    /// The lines' text; omitted from the first hunk whose texts would pass the report's
    /// `max_conflict_bytes`, and on the hunk that spans the first file's hunks past the
    /// report's bound (`truncated`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

impl LinesOut {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(first: u64, last: u64) -> Self {
        Self {
            first,
            last,
            text: None,
        }
    }
}

/// `op_log` (CLI `op log`): the store's operations, newest first, a page at a time. An operation
/// is one write transaction: a frame of the journal, numbered from 0 in commit order.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct OpLogReq {
    /// Start below this operation: the page holds the ones numbered less than it, so the last
    /// `op` of a page reads on from there. When omitted, from the newest.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<u64>,
    /// The most operations to return: at least 1 (0 is `E_INVALID_ARG`).
    #[serde(default = "default_op_log_limit")]
    #[schemars(range(min = 1))]
    pub limit: usize,
    /// The most changesets to list for each operation; `changesets_total` counts them all.
    #[serde(default = "default_max_changesets")]
    pub max_changesets: usize,
}

impl Default for OpLogReq {
    /// The request a JSON object without fields reads.
    fn default() -> Self {
        Self {
            before: None,
            limit: default_op_log_limit(),
            max_changesets: default_max_changesets(),
        }
    }
}

/// `op_log` (CLI `op log`): a page of operations, newest first.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct OpLogPage {
    /// The operations.
    pub ops: Vec<OperationOut>,
    /// Whether older operations follow this page: give the last one's `op` as `before` to read
    /// them.
    pub truncated: bool,
}

impl OpLogPage {
    /// Takes every field.
    pub fn new(ops: Vec<OperationOut>, truncated: bool) -> Self {
        Self { ops, truncated }
    }
}

/// One operation: a write transaction, as the journal recorded it.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct OperationOut {
    /// Its number: its journal frame's, from 0 in commit order. `undo` takes it.
    pub op: u64,
    /// What it did, read from what it recorded.
    pub kind: OperationKind,
    /// The changesets it recorded, in its order: at most the request's `max_changesets`.
    pub changesets: Vec<OpChangesetOut>,
    /// How many changesets it recorded in all, also past `max_changesets`.
    pub changesets_total: u64,
    /// The ref moves it made, in its order.
    pub ref_moves: Vec<RefMoveOut>,
}

impl OperationOut {
    /// Takes every field.
    pub fn new(
        op: u64,
        kind: OperationKind,
        changesets: Vec<OpChangesetOut>,
        changesets_total: u64,
        ref_moves: Vec<RefMoveOut>,
    ) -> Self {
        Self {
            op,
            kind,
            changesets,
            changesets_total,
            ref_moves,
        }
    }
}

/// What an operation did, read from what it recorded.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum OperationKind {
    /// It recorded changesets and moved no ref (`commit`).
    Commit,
    /// It recorded one changeset and moved a ref to it (`commit` with `advance`).
    Publish,
    /// It recorded one changeset that names a lander and moved a ref to it (`land`).
    Land,
    /// It moved refs and recorded no changeset (`ref set`, `ref delete`, `undo`).
    RefMove,
    /// It recorded a compaction of the changeset index, and nothing else.
    Compaction,
    /// Any other mix of changesets, ref moves and compactions.
    Mixed,
}

/// A changeset an operation recorded.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct OpChangesetOut {
    /// Its id.
    pub id: String,
    /// The agent that wrote it, as `show` gives `author.agent`.
    pub agent: String,
    /// When it was recorded: the committer signature's time, in Unix milliseconds.
    pub created_at: u64,
    /// The message's first line.
    pub title: String,
    /// The idempotency key of the land that made it, when it had one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub land_key: Option<String>,
}

impl OpChangesetOut {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(
        id: impl Into<String>,
        agent: impl Into<String>,
        created_at: u64,
        title: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            agent: agent.into(),
            created_at,
            title: title.into(),
            land_key: None,
        }
    }
}

/// A move of a ref, as the journal recorded it.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RefMoveOut {
    /// The ref.
    pub name: String,
    /// What it named before; omitted for the move that created it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old: Option<String>,
    /// What it names after; omitted for the move that deleted it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new: Option<String>,
    /// Who moved it.
    pub actor: String,
    /// When it moved, in Unix seconds, as its writer recorded it.
    pub at: i64,
}

impl RefMoveOut {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(name: impl Into<String>, actor: impl Into<String>, at: i64) -> Self {
        Self {
            name: name.into(),
            old: None,
            new: None,
            actor: actor.into(),
            at,
        }
    }
}

/// `undo` (CLI `op undo`): restore every ref an operation moved to what it named before, by
/// compare-and-swap, in one transaction: all of them or none. Changesets are never removed.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct UndoReq {
    /// The operation to undo: its `op` in the operation log.
    pub op: u64,
}

impl UndoReq {
    /// Takes every field.
    pub fn new(op: u64) -> Self {
        Self { op }
    }
}

/// `undo` (CLI `op undo`): the moves that restored the refs.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct UndoResp {
    /// The operation undone.
    pub undone: u64,
    /// The operation this undo recorded, which another undo can take; omitted when it wrote
    /// nothing (`already_applied`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub op: Option<u64>,
    /// The moves it made: each ref from what the operation set back to what it named before.
    pub ref_moves: Vec<RefMoveOut>,
    /// Every ref the operation moved already names what it named before, so nothing was written:
    /// an undo run again is a replay, and safe.
    pub already_applied: bool,
    /// Problems found after the undo became durable, such as failed housekeeping; the undo
    /// itself succeeded. Omitted when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

impl UndoResp {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(undone: u64, ref_moves: Vec<RefMoveOut>, already_applied: bool) -> Self {
        Self {
            undone,
            op: None,
            ref_moves,
            already_applied,
            warnings: Vec::new(),
        }
    }
}

/// The default [`BlobGetReq::max_bytes`]: 64 MiB of blob content, the batch a workspace's
/// materialisation reads at once.
pub const DEFAULT_BLOB_GET_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// The default [`WsStatusReq::limit`]: 500 changes.
pub const DEFAULT_WS_STATUS_LIMIT: usize = 500;

/// The default [`WsCheckInput::timeout_s`]: 900 s, as the operations spec's post-land hook.
pub const DEFAULT_CHECK_TIMEOUT_S: u64 = 900;

fn default_onto() -> String {
    "main".to_owned()
}

fn default_main_rev() -> Rev {
    Rev::from("main")
}

fn default_blob_get_max_bytes() -> u64 {
    DEFAULT_BLOB_GET_MAX_BYTES
}

fn default_ws_status_limit() -> usize {
    DEFAULT_WS_STATUS_LIMIT
}

fn default_check_timeout_s() -> u64 {
    DEFAULT_CHECK_TIMEOUT_S
}

/// `rebase` (M5): merge a change three-way onto a ref's head and record the result as one
/// changeset whose parent is that head, moving no ref. The merge, its conflicts and its
/// resolutions are a land's (operations spec §3): a conflict is the error `E_CONFLICT`, with the
/// report in `details.report`, and nothing is recorded then.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RebaseReq {
    /// The changeset to rebase: the change's head.
    pub head: Rev,
    /// Where the change starts, as a land's `base`: a first-parent ancestor of `head`; when
    /// omitted, the merge base of `head` and the head of `onto`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<Rev>,
    /// The ref to rebase onto: `main` or `release/<name>`, which must exist.
    #[serde(default = "default_onto")]
    pub onto: String,
    /// The recorded changeset's message; `head`'s when omitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Makes a retry safe: a uuid v7, as a commit's key. The same request on the same head of
    /// `onto` is the same changeset, replayed without writing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    /// The most bytes of hunk text an `E_CONFLICT` report carries, as a land's.
    #[serde(default = "default_max_conflict_bytes")]
    pub max_conflict_bytes: u64,
    /// What to make of the paths a conflict report named, as a land's resolutions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolutions: Option<ResolutionsInput>,
}

impl RebaseReq {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(head: impl Into<Rev>) -> Self {
        Self {
            head: head.into(),
            base: None,
            onto: default_onto(),
            message: None,
            idempotency_key: None,
            max_conflict_bytes: default_max_conflict_bytes(),
            resolutions: None,
        }
    }
}

/// `rebase`: the changeset that holds the change merged onto the ref's head. Ids are full.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RebaseResp {
    /// How the rebase ended.
    pub status: RebaseStatus,
    /// The changeset that holds the result: the recorded one, `head` when it is up to date, the
    /// ref's head when the change was absorbed.
    pub changeset: String,
    /// The root tree of `changeset`.
    pub root: String,
    /// The ref's head the change was merged onto: `changeset`'s parent when one was recorded.
    pub onto: String,
    /// The changeset the change was taken from.
    pub base: String,
    /// How many paths `changeset` changes against `onto`.
    pub files_changed: u64,
    /// The paths both sides changed whose contents merged cleanly into a content neither side
    /// had. Omitted when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub auto_merged: Vec<String>,
}

impl RebaseResp {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(
        status: RebaseStatus,
        changeset: impl Into<String>,
        root: impl Into<String>,
        onto: impl Into<String>,
        base: impl Into<String>,
        files_changed: u64,
    ) -> Self {
        Self {
            status,
            changeset: changeset.into(),
            root: root.into(),
            onto: onto.into(),
            base: base.into(),
            files_changed,
            auto_merged: Vec::new(),
        }
    }
}

/// How a rebase ended.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RebaseStatus {
    /// This request recorded the merged changeset.
    Rebased,
    /// The same keyed request had recorded it already; nothing was written.
    Replayed,
    /// The change already sits on the ref's head; nothing was written.
    UpToDate,
    /// The ref's head holds the change already; nothing was written.
    Absorbed,
}

/// `blob_get` (M5): blobs by id, for a client that writes them out (a workspace's
/// materialisation). Not an MCP tool.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct BlobGetReq {
    /// The blob ids, 64 hex digits each; one that is not is `E_INVALID_ARG`.
    pub ids: Vec<String>,
    /// The most bytes of content to return: the blobs past it are left for the next request
    /// (`truncated`).
    #[serde(default = "default_blob_get_max_bytes")]
    pub max_bytes: u64,
}

impl BlobGetReq {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(ids: Vec<String>) -> Self {
        Self {
            ids,
            max_bytes: default_blob_get_max_bytes(),
        }
    }
}

/// `blob_get`: the blobs found, in the order asked, and the ids the store holds no blob under.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct BlobGetResp {
    /// The blobs, each whole.
    pub blobs: Vec<BlobOut>,
    /// The ids the store holds no blob under.
    pub missing: Vec<String>,
    /// Whether blobs were left out to keep within `max_bytes`.
    pub truncated: bool,
}

impl BlobGetResp {
    /// Takes every field.
    pub fn new(blobs: Vec<BlobOut>, missing: Vec<String>, truncated: bool) -> Self {
        Self {
            blobs,
            missing,
            truncated,
        }
    }
}

/// One blob's content, encoded as a file's is.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct BlobOut {
    /// The blob's id.
    pub id: String,
    /// Its size in bytes.
    pub size: u64,
    /// How `content` encodes the bytes: `utf8` for text, `base64` otherwise.
    pub encoding: ContentEncoding,
    /// The bytes.
    pub content: String,
}

impl BlobOut {
    /// Takes every field.
    pub fn new(
        id: impl Into<String>,
        size: u64,
        encoding: ContentEncoding,
        content: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            size,
            encoding,
            content: content.into(),
        }
    }
}

/// Whether a workspace mutation's response carries the workspace's status after it.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum StatusAfter {
    /// The first page of the status, up to 100 changes.
    #[default]
    Page,
    /// No status.
    None,
}

/// What a workspace's rebase does.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum WsRebaseAction {
    /// Rebase the workspace's chain onto the target's head.
    #[default]
    Start,
    /// Rebase again with the resolved conflicts.
    Continue,
    /// Leave the rebase: restore the conflicting paths from the head.
    Abort,
}

/// How a workspace lands.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum LandMode {
    /// Merge onto the target's head, wherever it has moved.
    Merge,
    /// Publish only onto the target's head the workspace was rebased onto, so that the landed
    /// tree is the rebased one.
    Exact,
}

/// `ws_create` (M5, client-side): make a workspace, a directory holding a sparse copy of a
/// changeset's tree. The same request again is the same workspace (`created` false).
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WsCreateReq {
    /// The workspace's name: 1 to 64 of `a-z`, `0-9`, `.`, `_` and `-`, not starting with `.`.
    pub name: String,
    /// The directory to make it in, on the caller's host; when omitted, one named after it in the
    /// working directory of the process that serves the call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The changeset whose tree it holds.
    #[serde(default = "default_main_rev")]
    pub base: Rev,
    /// The repository paths whose subtrees are written out.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prefixes: Vec<String>,
    /// The whole tree instead of `prefixes`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub all: bool,
    /// A directory on the caller's host whose `.worktreeinclude` names files to copy in; when
    /// omitted, nothing is copied.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// The hook's session, 16 hex digits; set by the Claude Code hook only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Whether the response carries the status after the mutation.
    #[serde(default)]
    pub status_after: StatusAfter,
}

impl WsCreateReq {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            path: None,
            base: default_main_rev(),
            prefixes: Vec::new(),
            all: false,
            from: None,
            session: None,
            status_after: StatusAfter::default(),
        }
    }
}

/// A workspace: where it is and what it holds.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WsInfo {
    /// Its name.
    pub name: String,
    /// Its directory on the caller's host.
    pub path: String,
    /// The target's changeset its chain starts from.
    pub base: String,
    /// Its chain's last changeset; `base` when nothing is committed.
    pub head: String,
    /// The repository paths written out.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prefixes: Vec<String>,
    /// Whether the whole tree is written out.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub all: bool,
    /// Whether this request made it: false for one that existed.
    #[serde(default)]
    pub created: bool,
    /// Whether its directory is gone while its entry stays (`ws_list` with `check`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub orphan: bool,
    /// The status after the mutation; omitted when the request said `none`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_after: Option<WsStatus>,
}

impl WsInfo {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(
        name: impl Into<String>,
        path: impl Into<String>,
        base: impl Into<String>,
        head: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            path: path.into(),
            base: base.into(),
            head: head.into(),
            prefixes: Vec::new(),
            all: false,
            created: false,
            orphan: false,
            status_after: None,
        }
    }
}

/// `ws_status` (client-side): what changed in a workspace against its head.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WsStatusReq {
    /// The workspace, by name.
    pub workspace: String,
    /// Only these repository paths, and what is below them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    /// Hash every file instead of trusting unchanged stats.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub full: bool,
    /// The most changes to return: at least 1.
    #[serde(default = "default_ws_status_limit")]
    #[schemars(range(min = 1))]
    pub limit: usize,
    /// Where the previous page stopped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

impl WsStatusReq {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(workspace: impl Into<String>) -> Self {
        Self {
            workspace: workspace.into(),
            paths: Vec::new(),
            full: false,
            limit: default_ws_status_limit(),
            cursor: None,
        }
    }
}

/// A workspace's status: its changes against its head, a page at a time, and how far its target
/// has moved since its base.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WsStatus {
    /// The workspace's name.
    pub name: String,
    /// The target's changeset its chain starts from.
    pub base: String,
    /// Its chain's last changeset.
    pub head: String,
    /// The head of the workspace's target ref (`main`, or `release/<name>`), read first.
    pub main: String,
    /// How many changesets the target's first-parent history holds that `base`'s lacks: those
    /// past `base` when it is on that history; otherwise (a sync dropped it, or the target moved
    /// back) those before where the two histories meet, 0 when the target is an ancestor of
    /// `base`. At most 10,000.
    pub behind_by: u64,
    /// Whether the workspace is rebasing.
    pub state: WsState,
    /// The changes, by path.
    pub changes: Vec<WsChange>,
    /// The rebase in progress, when `state` is `rebasing`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rebasing: Option<WsRebasing>,
    /// Whether more changes follow: give `cursor` to read them.
    pub truncated: bool,
    /// Where this page stopped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

impl WsStatus {
    /// Takes the required fields; the others get their serde defaults.
    // Every argument is a required field of the response: no default can stand in.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        name: impl Into<String>,
        base: impl Into<String>,
        head: impl Into<String>,
        main: impl Into<String>,
        behind_by: u64,
        state: WsState,
        changes: Vec<WsChange>,
        truncated: bool,
    ) -> Self {
        Self {
            name: name.into(),
            base: base.into(),
            head: head.into(),
            main: main.into(),
            behind_by,
            state,
            changes,
            rebasing: None,
            truncated,
            cursor: None,
        }
    }
}

/// A workspace's state.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum WsState {
    /// No rebase in progress.
    Clean,
    /// A rebase stopped on conflicts.
    Rebasing,
}

/// One changed path of a workspace.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WsChange {
    /// The repository path.
    pub path: String,
    /// What changed.
    pub kind: WsChangeKind,
    /// The file's size now; omitted for a deleted path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

impl WsChange {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(path: impl Into<String>, kind: WsChangeKind) -> Self {
        Self {
            path: path.into(),
            kind,
            size: None,
        }
    }
}

/// What changed at a workspace path.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum WsChangeKind {
    /// A file the head does not hold.
    Added,
    /// Its content changed.
    Modified,
    /// It is gone.
    Deleted,
    /// A file became a symlink, or the other way round.
    TypeChanged,
    /// Its executable bit changed.
    ModeChanged,
}

/// A workspace's rebase in progress.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WsRebasing {
    /// The target's head it rebases onto.
    pub onto: String,
    /// The paths that still conflict.
    pub conflicts: Vec<String>,
}

impl WsRebasing {
    /// Takes every field.
    pub fn new(onto: impl Into<String>, conflicts: Vec<String>) -> Self {
        Self {
            onto: onto.into(),
            conflicts,
        }
    }
}

/// `ws_diff` (client-side): a workspace's edits against its head, as the store's diff gives a
/// diff (`from` the head, `to` `ws:<name>`).
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WsDiffReq {
    /// The workspace, by name.
    pub workspace: String,
    /// Only these repository paths, and what is below them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    /// How much each file says: as the store's diff.
    #[serde(default)]
    pub format: DiffFormat,
    /// Unchanged lines around each change.
    #[serde(default = "default_diff_context")]
    pub context: u32,
    /// The most files to return: at least 1.
    #[serde(default = "default_diff_limit")]
    #[schemars(range(min = 1))]
    pub limit: usize,
    /// Where the previous page stopped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// The most bytes of a `unified` page's files.
    #[serde(default = "default_diff_max_bytes")]
    pub max_bytes: u64,
}

impl WsDiffReq {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(workspace: impl Into<String>) -> Self {
        Self {
            workspace: workspace.into(),
            paths: Vec::new(),
            format: DiffFormat::default(),
            context: default_diff_context(),
            limit: default_diff_limit(),
            cursor: None,
            max_bytes: default_diff_max_bytes(),
        }
    }
}

/// `ws_commit` (client-side): record a workspace's edits as one changeset on its head.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WsCommitReq {
    /// The workspace, by name.
    pub workspace: String,
    /// The changeset's message.
    pub message: String,
    /// Only these repository paths; paths outside the workspace's prefixes join them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    /// A kind to record for a path, where the volume cannot hold the executable bit.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub modes: BTreeMap<String, ModeInput>,
    /// Whether the response carries the status after the mutation.
    #[serde(default)]
    pub status_after: StatusAfter,
}

impl WsCommitReq {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(workspace: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            workspace: workspace.into(),
            message: message.into(),
            paths: Vec::new(),
            modes: BTreeMap::new(),
            status_after: StatusAfter::default(),
        }
    }
}

/// `ws_commit`: the changeset recorded.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WsCommitResp {
    /// The new changeset: the workspace's head now.
    pub changeset: String,
    /// Its parent: the head before.
    pub parent: String,
    /// How many paths it changes.
    pub files_changed: u64,
    /// How many blobs the store did not hold and got.
    pub blobs_written: u64,
    /// A crashed commit's key replayed: the changeset was recorded before.
    pub replayed: bool,
    /// The status after the mutation; omitted when the request said `none`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_after: Option<WsStatus>,
}

impl WsCommitResp {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(
        changeset: impl Into<String>,
        parent: impl Into<String>,
        files_changed: u64,
        blobs_written: u64,
        replayed: bool,
    ) -> Self {
        Self {
            changeset: changeset.into(),
            parent: parent.into(),
            files_changed,
            blobs_written,
            replayed,
            status_after: None,
        }
    }
}

/// `ws_rebase` (client-side): move a workspace's chain onto its target's head, as `rebase`
/// records it, and its files with it.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WsRebaseReq {
    /// The workspace, by name.
    pub workspace: String,
    /// The ref to rebase onto.
    #[serde(default = "default_onto")]
    pub onto: String,
    /// Start, continue after resolving, or abort.
    #[serde(default)]
    pub action: WsRebaseAction,
    /// Commit the workspace's edits first, with this message; without it, edits are
    /// `E_WS_DIRTY`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Whether the response carries the status after the mutation.
    #[serde(default)]
    pub status_after: StatusAfter,
}

impl WsRebaseReq {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(workspace: impl Into<String>) -> Self {
        Self {
            workspace: workspace.into(),
            onto: default_onto(),
            action: WsRebaseAction::default(),
            message: None,
            status_after: StatusAfter::default(),
        }
    }
}

/// `ws_rebase`: where the workspace stands now.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WsRebaseResp {
    /// How the rebase ended; `None` after an abort.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<RebaseStatus>,
    /// The workspace's head now.
    pub changeset: String,
    /// The target's head it was rebased onto.
    pub onto: String,
    /// The status after the mutation; omitted when the request said `none`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_after: Option<WsStatus>,
}

impl WsRebaseResp {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(changeset: impl Into<String>, onto: impl Into<String>) -> Self {
        Self {
            status: None,
            changeset: changeset.into(),
            onto: onto.into(),
            status_after: None,
        }
    }
}

/// `ws_resolve` (client-side): mark conflicting paths of a rebase resolved.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WsResolveReq {
    /// The workspace, by name.
    pub workspace: String,
    /// The paths resolved.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    /// Every conflicting path.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub all: bool,
    /// Take one side's content instead of the file's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub take: Option<Side>,
    /// Resolve a file that still holds a conflict marker line.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub force: bool,
    /// Whether the response carries the status after the mutation.
    #[serde(default)]
    pub status_after: StatusAfter,
}

impl WsResolveReq {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(workspace: impl Into<String>) -> Self {
        Self {
            workspace: workspace.into(),
            paths: Vec::new(),
            all: false,
            take: None,
            force: false,
            status_after: StatusAfter::default(),
        }
    }
}

/// `ws_resolve`: the paths resolved and those left.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WsResolveResp {
    /// The paths this request resolved.
    pub resolved: Vec<String>,
    /// The paths that still conflict.
    pub remaining: Vec<String>,
    /// The status after the mutation; omitted when the request said `none`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_after: Option<WsStatus>,
}

impl WsResolveResp {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(resolved: Vec<String>, remaining: Vec<String>) -> Self {
        Self {
            resolved,
            remaining,
            status_after: None,
        }
    }
}

/// The check a workspace's land runs.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WsCheckInput {
    /// The command and its arguments, run without a shell in the workspace; when omitted, the
    /// repository's `.swarmops/workspaces.json` `check.argv`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub argv: Option<Vec<String>>,
    /// The most seconds the command runs before it is killed.
    #[serde(default = "default_check_timeout_s")]
    pub timeout_s: u64,
}

impl Default for WsCheckInput {
    /// The request a JSON object without fields reads.
    fn default() -> Self {
        Self {
            argv: None,
            timeout_s: default_check_timeout_s(),
        }
    }
}

/// `ws_land` (client-side): land a workspace's chain on its target; with a check, rebase, run
/// the check there, and publish only the tree the check ran on.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WsLandReq {
    /// The workspace, by name.
    pub workspace: String,
    /// The landed changeset's message, and the message to commit edits with first.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// The land's key, a uuid v7; when omitted, the workspace makes one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    /// The ref to land on; the workspace's target when omitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// The check to run on the rebased tree before it lands.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub check: Option<WsCheckInput>,
    /// `merge` or `exact`; when omitted, `exact` with a check and `merge` without.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<LandMode>,
    /// How many times to rebase, check and land before `E_CAS_RETRY_EXHAUSTED`: 1 to 32.
    #[serde(default = "default_max_attempts")]
    #[schemars(range(min = 1, max = MAX_LAND_ATTEMPTS))]
    pub max_attempts: u32,
    /// Whether the response carries the status after the mutation.
    #[serde(default)]
    pub status_after: StatusAfter,
}

impl WsLandReq {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(workspace: impl Into<String>) -> Self {
        Self {
            workspace: workspace.into(),
            message: None,
            idempotency_key: None,
            target: None,
            check: None,
            mode: None,
            max_attempts: default_max_attempts(),
            status_after: StatusAfter::default(),
        }
    }
}

/// `ws_land`: the land, and how many attempts it took.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WsLandResp {
    /// The store's land.
    pub land: LandResp,
    /// How many rebase, check and land attempts it took.
    pub attempts: u32,
    /// The status after the mutation; omitted when the request said `none`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_after: Option<WsStatus>,
}

impl WsLandResp {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(land: LandResp, attempts: u32) -> Self {
        Self {
            land,
            attempts,
            status_after: None,
        }
    }
}

/// `ws_materialize` (client-side): write out more of the tree.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WsMaterializeReq {
    /// The workspace, by name.
    pub workspace: String,
    /// The repository paths to add to its prefixes.
    pub paths: Vec<String>,
    /// Whether the response carries the status after the mutation.
    #[serde(default)]
    pub status_after: StatusAfter,
}

impl WsMaterializeReq {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(workspace: impl Into<String>, paths: Vec<String>) -> Self {
        Self {
            workspace: workspace.into(),
            paths,
            status_after: StatusAfter::default(),
        }
    }
}

/// `ws_materialize`: what was written.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WsMaterializeResp {
    /// The prefixes added.
    pub materialized: Vec<String>,
    /// How many bytes were written.
    pub bytes: u64,
    /// The status after the mutation; omitted when the request said `none`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_after: Option<WsStatus>,
}

impl WsMaterializeResp {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(materialized: Vec<String>, bytes: u64) -> Self {
        Self {
            materialized,
            bytes,
            status_after: None,
        }
    }
}

/// `ws_discard` (client-side): restore paths from the workspace's head.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WsDiscardReq {
    /// The workspace, by name.
    pub workspace: String,
    /// The repository paths to restore; every changed one when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    /// Whether the response carries the status after the mutation.
    #[serde(default)]
    pub status_after: StatusAfter,
}

impl WsDiscardReq {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(workspace: impl Into<String>) -> Self {
        Self {
            workspace: workspace.into(),
            paths: Vec::new(),
            status_after: StatusAfter::default(),
        }
    }
}

/// `ws_discard`: the paths restored.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WsDiscardResp {
    /// The paths restored.
    pub discarded: Vec<String>,
    /// The status after the mutation; omitted when the request said `none`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_after: Option<WsStatus>,
}

impl WsDiscardResp {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(discarded: Vec<String>) -> Self {
        Self {
            discarded,
            status_after: None,
        }
    }
}

/// `ws_list` (client-side): the store's workspaces, from its registry.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct WsListReq {
    /// Also open each workspace, and report one whose directory is gone as an orphan.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub check: bool,
}

/// `ws_list`: the workspaces, by name.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WsList {
    /// The workspaces.
    pub workspaces: Vec<WsInfo>,
}

impl WsList {
    /// Takes every field.
    pub fn new(workspaces: Vec<WsInfo>) -> Self {
        Self { workspaces }
    }
}

/// `ws_info` (client-side): one workspace.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WsInfoReq {
    /// The workspace, by name.
    pub workspace: String,
}

impl WsInfoReq {
    /// Takes every field.
    pub fn new(workspace: impl Into<String>) -> Self {
        Self {
            workspace: workspace.into(),
        }
    }
}

/// `ws_delete` (client-side, CLI only): remove a workspace's directory, then its entry.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WsDeleteReq {
    /// The workspace, by name.
    pub workspace: String,
    /// Delete it even with edits or unlanded commits.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub force: bool,
    /// Commit its edits as a rescue changeset first.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub rescue: bool,
}

impl WsDeleteReq {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(workspace: impl Into<String>) -> Self {
        Self {
            workspace: workspace.into(),
            force: false,
            rescue: false,
        }
    }
}

/// `ws_delete`: what the deleted workspace left in the store.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WsDeleteResp {
    /// The workspace's name.
    pub deleted: String,
    /// Its head when it was deleted.
    pub head: String,
    /// The rescue changeset of its edits, when there were some.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rescued: Option<String>,
}

impl WsDeleteResp {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(deleted: impl Into<String>, head: impl Into<String>) -> Self {
        Self {
            deleted: deleted.into(),
            head: head.into(),
            rescued: None,
        }
    }
}

/// One store's operations, implemented over a local store now and by a remote client later
/// (operations spec §7).
///
/// Synchronous and object-safe, so callers hold a `dyn StoreOps`. Mutations take the caller's
/// [`Identity`]; reads take none.
pub trait StoreOps {
    /// What the store is and holds (CLI `info`).
    fn repo_info(&self, req: InfoReq) -> Result<RepoInfo, OpError>;

    /// Records a changeset from an explicit change list (CLI `commit`); with `advance`, also
    /// moves that ref to it in the same transaction, and otherwise moves no ref.
    ///
    /// [`ContentInput::File`] names a file on the caller's machine, so it is resolved on the
    /// client: a store that runs in the caller's process reads the file there, and a store that
    /// serves these operations over a wire must refuse such content rather than read a path its
    /// caller names (the first such server, M11's `serve`, comes with the client-side pass that
    /// turns it into bytes before a request leaves the client).
    fn commit(&self, who: &Identity, req: CommitReq) -> Result<CommitResp, OpError>;

    /// Merges a change three-way onto a ref, `main` by default, and publishes the result there as
    /// one new changeset, with the ref's move, in one transaction (CLI `land`). A change that
    /// conflicts is `E_CONFLICT`, with the conflict report in `details.report`, and records
    /// nothing.
    fn land(&self, who: &Identity, req: LandReq) -> Result<LandResp, OpError>;

    /// Merges a change three-way onto a ref's head and records the result as one changeset
    /// whose parent is that head, moving no ref (CLI `rebase`, M5). A conflict is `E_CONFLICT`,
    /// as a land's, and records nothing.
    fn rebase(&self, who: &Identity, req: RebaseReq) -> Result<RebaseResp, OpError>;

    /// The changeset a revision names, and whether the revision is exactly a ref's name (CLI
    /// `resolve`).
    fn resolve_rev(&self, req: ResolveRevReq) -> Result<ResolvedRev, OpError>;

    /// Stores content as a blob, outside any commit, for a commit to name by its id (CLI `blob
    /// put`). A mutation, so it takes the caller's identity, which it records nowhere: a blob has
    /// no metadata.
    fn blob_put(&self, who: &Identity, req: BlobPutReq) -> Result<BlobPutResp, OpError>;

    /// Which of the given blob ids the store holds blobs under (CLI `blob has`).
    fn blob_has(&self, req: BlobHasReq) -> Result<BlobHasResp, OpError>;

    /// Blobs by id, up to a byte budget, and the ids it holds none under (CLI `blob get`, M5).
    fn blob_get(&self, req: BlobGetReq) -> Result<BlobGetResp, OpError>;

    /// Reads one file at a revision (CLI `cat`).
    fn read_file(&self, req: ReadFileReq) -> Result<FileContent, OpError>;

    /// Lists one directory at a revision (CLI `ls`).
    fn read_tree(&self, req: ReadTreeReq) -> Result<TreeListing, OpError>;

    /// One changeset in full (CLI `show`).
    fn show(&self, req: ShowReq) -> Result<ChangesetDetail, OpError>;

    /// History along first parents, newest first (CLI `log`); with `paths`, only the changesets
    /// that change them.
    fn log(&self, req: LogReq) -> Result<LogPage, OpError>;

    /// The changesets that changed what a path holds, newest first, along first parents (CLI
    /// `history`).
    fn path_history(&self, req: PathHistoryReq) -> Result<PathHistoryPage, OpError>;

    /// The files that differ between two revisions, a page at a time (CLI `diff`).
    fn diff(&self, req: DiffReq) -> Result<Diff, OpError>;

    /// Every ref, or those whose names start with a prefix (CLI `ref list`).
    fn ref_list(&self, req: RefListReq) -> Result<RefListResp, OpError>;

    /// Moves a ref, or creates one, by compare-and-swap (CLI `ref set`).
    fn ref_set(&self, who: &Identity, req: RefSetReq) -> Result<RefSetResp, OpError>;

    /// Removes a ref by compare-and-swap (CLI `ref delete`); `main` is never deleted
    /// (`E_REF_PROTECTED`).
    fn ref_delete(&self, who: &Identity, req: RefDeleteReq) -> Result<RefSetResp, OpError>;

    /// The store's operations, newest first, a page at a time (CLI `op log`).
    fn op_log(&self, req: OpLogReq) -> Result<OpLogPage, OpError>;

    /// Restores every ref an operation moved to what it named before, by compare-and-swap in one
    /// transaction, all of them or none (CLI `op undo`).
    fn undo(&self, who: &Identity, req: UndoReq) -> Result<UndoResp, OpError>;

    /// Checks the whole store's consistency (CLI `verify`).
    fn verify(&self, req: VerifyReq) -> Result<VerifyReport, OpError>;

    /// Derives ahead of any reader the eager derived values of the changesets the journal
    /// records after a frame, and says where to go on (CLI `derive`). It writes only the store's
    /// cache of derived values, so it takes no identity.
    fn derive(&self, req: DeriveReq) -> Result<DeriveResp, OpError>;

    /// Removes the store's leftovers, and no object or changeset (CLI `gc`). Maintenance: it
    /// records nothing, so it takes no identity. A store that does not have it answers
    /// `E_INVALID_ARG`.
    fn gc(&self, req: GcReq) -> Result<GcResp, OpError> {
        let _ = req;
        Err(not_built("gc"))
    }

    /// Rebuilds what the journal can rebuild and quarantines damaged objects (CLI `repair`).
    /// Maintenance, as `gc` is. A store that does not have it answers `E_INVALID_ARG`.
    fn repair(&self, req: RepairReq) -> Result<RepairResp, OpError> {
        let _ = req;
        Err(not_built("repair"))
    }

    /// Moves the store to the latest format this build writes (CLI `upgrade`); irreversible.
    /// Maintenance, as `gc` is. A store that does not have it answers `E_INVALID_ARG`.
    fn upgrade(&self, req: UpgradeReq) -> Result<UpgradeResp, OpError> {
        let _ = req;
        Err(not_built("upgrade"))
    }
}

/// `E_INVALID_ARG`: operation `op`, which this store does not have.
fn not_built(op: &str) -> OpError {
    OpError::new(
        crate::error::ErrorCode::InvalidArg,
        format!("this store does not have the {op} operation"),
    )
}

// The CLI holds a `dyn StoreOps` (operations spec §7); this stops compiling if the trait stops
// being object-safe.
const _: Option<&dyn StoreOps> = None;
