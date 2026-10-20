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
//! default page of `read_tree`, `show`, `log`, `path_history` and `diff` stays within the
//! operations spec's 64 KiB target for a default response (§9); `read_file`'s 1 MiB is the spec's
//! own.
//!
//! No store-facing type carries a filesystem path. The four exceptions are client-side:
//! [`ContentInput::File`], which the client reads and sends as bytes, [`InitReq::path`],
//! because `init` runs on the client and is not a [`StoreOps`] method, and
//! [`GitImportReq::git_path`] and [`GitExportReq::git_path`], because `git import` reads its
//! source repository, and `git export` writes its destination, on the caller's host, and
//! neither is a [`StoreOps`] method (nor an MCP tool).
//!
//! Not in M1 yet:
//! - M2: `LogReq.first_parent`;
//! - M5: `RepoInfo.workspaces`;
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
pub struct InitReq {
    /// The directory to create the store in.
    pub path: String,
    /// The repository's name: at most 255 bytes of UTF-8.
    pub name: String,
}

/// `info`: describe the store. It has no options yet.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct InfoReq {}

/// `init` and `info`: what the store is and holds.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
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

/// `whoami`: the identity the caller's mutations carry.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
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

/// `commit`: record a changeset from an explicit change list. It moves no ref, unless
/// `advance` names one.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
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

/// One change of a commit.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
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

/// What a change does to its path, in a commit's change list and a changeset's changes alike.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ChangeOp {
    /// Write the content at the path, creating or replacing the entry.
    Put,
    /// Remove the path.
    Delete,
}

/// A `put`'s content, as an object with one key.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
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

/// `commit` with `advance`: the ref's move, made in the commit's own transaction.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
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

/// `blob_put` (CLI `blob put`): store content as a blob, outside any commit, so that a commit's
/// change list can name it by its id (`{"blob": id}`) instead of carrying its bytes. The blob is
/// durable once the call returns; until a commit names it, nothing references it.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct BlobPutReq {
    /// The content: inline, base64 or a client file, at most 512 MiB (more is `E_TOO_LARGE` with
    /// `scope` `put`). A `blob` names content the store holds already, and is `E_INVALID_ARG`
    /// here.
    pub content: ContentInput,
}

/// `blob_put`: the blob stored.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct BlobPutResp {
    /// The blob's id, which a change names it by: `{"blob": id}`.
    pub blob: String,
    /// The content's size in bytes.
    pub size: u64,
    /// Whether this call placed the blob in the object store; false when the store held it
    /// already, or a concurrent writer placed it first.
    pub written: bool,
}

/// `blob_has` (CLI `blob has`): which of some blobs the store holds.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct BlobHasReq {
    /// The blob ids, 64 hex digits each; one that is not is `E_INVALID_ARG`.
    pub ids: Vec<String>,
}

/// `blob_has`: the blobs the store holds, of those asked for.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct BlobHasResp {
    /// The ids of `ids` that the store holds a blob under, in lowercase hex, in the order first
    /// given, each once; an id of another kind of object (a tree, a changeset) is not among them.
    pub present: Vec<String>,
}

/// `resolve_rev` (CLI `resolve`): the changeset a revision names, as every operation resolves it.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct ResolveRevReq {
    /// The revision to resolve.
    pub rev: Rev,
}

/// `resolve_rev` (CLI `resolve`): the changeset a revision names, and what kind of revision it is.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRev {
    /// The full id of the changeset the revision names.
    pub id: String,
    /// What the revision is: `ref` when it is exactly a ref's name, `changeset` otherwise.
    pub kind: RevKind,
}

/// What kind of revision `resolve_rev` resolved. M5 adds `workspace`, for a workspace's head and
/// the edits not committed yet (`ws:<name>`).
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
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
    /// The most content bytes to return; more comes back truncated, with the `next_offset` to
    /// read on from.
    #[serde(default = "default_max_bytes")]
    pub max_bytes: u64,
}

/// A range of a file's lines, numbered from 1: `first` to `last`, both included.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LineRange {
    /// The range's first line: at least 1.
    #[schemars(range(min = 1))]
    pub first: u64,
    /// The range's last line: at least `first`.
    #[schemars(range(min = 1))]
    pub last: u64,
}

/// `read_file` (CLI `cat`): a file's content, or the part of it the request names.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
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
    /// When `truncated`: where `content` stops, the `offset` that reads on from there. A cut text
    /// holds whole characters, so a `max_bytes` under 4 can stop before a longer character at
    /// the `offset` it started from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<u64>,
}

/// How `FileContent.content` encodes bytes.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ContentEncoding {
    /// The bytes are UTF-8 text, and `content` is that text.
    Utf8,
    /// `content` is the bytes in standard, padded base64.
    Base64,
}

/// What a file is; never a directory.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
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
    /// construct it (cursors are opaque). This page starts after the entry it names.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

/// `read_tree` (CLI `ls`): a page of a directory's entries, in pre-order: a directory, then the
/// entries below it to the request's `depth`, each directory's entries by name bytes (at `depth`
/// 1, the tree's order).
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
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

/// One entry of a listing.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
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

/// `show`: one changeset in full.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct ShowReq {
    /// The changeset to show.
    pub rev: Rev,
    /// The most entries of `changes` to return: at least 1 (0 is `E_INVALID_ARG`).
    #[serde(default = "default_max_changes")]
    #[schemars(range(min = 1))]
    pub max_changes: usize,
}

/// `show`: a changeset's header, changes and metadata. Byte strings are decoded as UTF-8, with
/// invalid bytes replaced, except metadata values (see `metadata`).
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
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

/// Whom a changeset is attributed to (operations spec §5).
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
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

/// A changeset's git signatures: git-fidelity data for import and export.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct Signatures {
    /// The git author.
    pub author: SignatureOut,
    /// The git committer; its time is `created_at`.
    pub committer: SignatureOut,
}

/// A git signature: who, and when.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
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

/// One entry of a changeset's changes.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
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

/// `log`: history along first parents, a page at a time.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
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

/// `log`: a page of changesets, newest first, following first parents.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
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

/// One changeset of a log.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
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

/// `path_history` (CLI `history`): the changesets that changed what a path holds, newest first,
/// following first parents, a page at a time.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
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

/// `path_history` (CLI `history`): a page of a path's history, newest first.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
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

/// One changeset of a path's history, and what the path holds after it.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
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

/// `diff`: the files that differ between two revisions, in path order, a page at a time.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
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
    /// construct it (cursors are opaque). This page starts after the file it names.
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

/// What `diff` says of each file.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "snake_case")]
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

/// One file that differs: a file, an executable or a symlink whose content or kind changed, or
/// that is on one side only. A directory never is one: a file that becomes a directory is that
/// file deleted and the directory's files added. No rename is detected: a file that moved is
/// one deleted and one added. Which fields follow `status` depends on the request's `format`,
/// and a field that says nothing is omitted.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
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
    /// `stat` and `unified`, for text: the lines the change inserts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub insertions: Option<u64>,
    /// `stat` and `unified`, for text: the lines the change deletes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deletions: Option<u64>,
    /// `unified`, for text: the hunks, in order; empty when only the exec bit or the kind
    /// changed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hunks: Option<Vec<DiffHunk>>,
    /// `unified`: true when `hunks` stops before the file's last hunk, to keep the page within
    /// `max_bytes`; omitted otherwise. `insertions` and `deletions` count every line all the
    /// same.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hunks_truncated: Option<bool>,
}

/// What happened to a file between the two revisions.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
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

/// One line of a hunk.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct DiffLineOut {
    /// Whether both sides have it, only the new one, or only the old one.
    pub op: LineOp,
    /// The line with its line break, a carriage return before it kept; a file's last line has
    /// none when the file does not end in one.
    pub text: String,
}

/// Which side of a diff a hunk's line is on.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum LineOp {
    /// Both: an unchanged line around a change.
    Context,
    /// The new side only.
    Add,
    /// The old side only.
    Remove,
}

/// `ref list`: list every ref. It has no options yet.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct RefListReq {}

/// `ref list`: every ref.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct RefListResp {
    /// The refs.
    pub refs: Vec<RefOut>,
}

/// One ref.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct RefOut {
    /// `main` or `release/<name>`.
    pub name: String,
    /// The changeset it points at.
    pub target: String,
    /// How many committed moves changed its target.
    pub version: u64,
}

/// `ref set`: move a ref, or create one, by compare-and-swap. Give exactly one of
/// `expected_old` and `create`; both or neither is `E_INVALID_ARG`.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[schemars(transform = one_of_expected_old_and_create)]
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

/// `ref delete`: remove a ref by compare-and-swap.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
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

/// `verify`: check the whole store's consistency.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct VerifyReq {
    /// Re-hash every object. When `false`, verify checks structure only: parents, that
    /// referenced objects exist, the index against the journal, and refs.
    #[serde(default)]
    pub deep: bool,
}

/// `verify`: what the consistency check found.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
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
}

/// One problem `verify` found, as a program reads it: what kind of damage it is, and what it
/// names. The keys its kind does not use are omitted; the line at the same index of `problems`
/// says the same for a reader. Ids are lowercase hex.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct VerifyFinding {
    /// What is damaged: `corrupt_object`, `missing_object`, `missing_parent`, `root_mismatch`,
    /// `dangling_ref`, `corrupt_ref`, `index_mismatch`, `journal_corrupt`, `meta_corrupt`,
    /// `external_id_corrupt`, `external_id_dangling` or `external_id_mismatch`. A string, as
    /// `OpError.code` is, so that a client can read kinds newer than its own list.
    pub kind: String,
    /// The object: one whose content does not hash to its id or does not decode
    /// (`corrupt_object`), or one a changeset references that the store lacks, or holds as
    /// another kind (`missing_object`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub object: Option<String>,
    /// The changeset: the first, in seq order, that references the missing object
    /// (`missing_object`), the one that names a missing parent (`missing_parent`), the one whose
    /// root is not its first parent's root with its delta applied (`root_mismatch`), or the one
    /// an external id's entry names: not in the store (`external_id_dangling`), or with a
    /// `git.oid` that is another oid than the entry's key (`external_id_mismatch`).
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
    /// The external id whose entry it is, in lowercase hex: a git commit's oid for system `git`
    /// (`external_id_corrupt`, `external_id_dangling`, `external_id_mismatch`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
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
/// `init` does: no server receives this request, and it is no MCP tool.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
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
    /// names them in its `git.lossy` metadata. Empty by default: a commit holding any class is
    /// refused with `E_GIT_IMPORT_LOSSY`, which names the commit, the path and the class.
    #[serde(default)]
    pub lossy: Vec<LossyClass>,
}

/// A class of git content that a changeset cannot hold as git has it (storage spec §7). A
/// commit holding one is refused unless the import's `lossy` allows its class; an allowed class
/// is dropped or normalized, a directory a drop leaves empty is dropped with it, and the
/// changeset's `git.lossy` metadata names the classes, since it no longer renders back to the
/// commit's own git id.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum LossyClass {
    /// A submodule's entry (git mode 160000): dropped.
    Gitlinks,
    /// A name that is not UTF-8, or is `.` or `..`: dropped.
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

/// `git import`: a commit taken lossily, and what it lost.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct LossyCommitOut {
    /// The git commit: its id, 40 lowercase hex digits.
    pub commit: String,
    /// The changeset made of it, whose `git.lossy` metadata names the same classes.
    pub changeset: String,
    /// The classes dropped or normalized in it, ordered by name, as `git.lossy` lists them.
    pub classes: Vec<LossyClass>,
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
/// tool.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
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

/// `git export`: what the export wrote, and where the branch stands.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
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

/// `derive`: derive ahead of any reader the eager derived values (storage spec §5: each
/// changeset's first-parent skip list) of the changesets the journal records after a frame, so
/// that reads find them stored. It writes only the store's cache of derived values, so it takes
/// no identity.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct DeriveReq {
    /// Go on after this journal frame: a previous call's `next`. From the journal's first frame
    /// when omitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<u64>,
    /// The most journal frames to read; a frame is one transaction, a commit or a ref's move. 0
    /// reads none and derives nothing: `next` is then the journal's last frame, from where the
    /// next calls derive only what is recorded later.
    #[serde(default = "default_derive_limit")]
    pub limit: usize,
}

/// `derive`: how many values one call derived, how many the store held already, and where to
/// go on.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct DeriveResp {
    /// How many values it derived: of each eager derivation, one for each changeset its frames
    /// record whose value the store lacked. Values those needed on the way, an older
    /// changeset's, are derived too and not counted.
    pub derived: u64,
    /// How many of those values the store held already.
    pub cached: u64,
    /// The last frame it read: pass it as the next call's `after` to go on, through a journal
    /// longer than `limit` frames or through the frames recorded since. Omitted when it read no
    /// frame, as none follows `after`: it is at the journal's end. With `limit` 0, the journal's
    /// last frame, omitted while the journal holds none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<u64>,
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
    /// The most bytes of hunk text an `E_CONFLICT` report carries: past it, the texts are left
    /// out (`truncated`), and the line positions and blob ids stay.
    #[serde(default = "default_max_conflict_bytes")]
    pub max_conflict_bytes: u64,
    /// What to make of the paths an `E_CONFLICT` report named, decided without a checkout
    /// (operations spec §3, resolution path (b)). Each resolution applies to its path while the
    /// target holds there what `against` held, whether the path conflicts or not; a path the
    /// target changed since is stale, and the land fails `E_CONFLICT` again, with a fresh report
    /// that names it in `stale`. Conflicts left unresolved fail it too. `swarmops mcp` refuses
    /// an `against` that is not a full 64-hex id (`E_INVALID_ARG`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolutions: Option<ResolutionsInput>,
}

/// A land's resolutions (`land --resolve`): what to make of some paths of the merge of the change
/// onto the target, decided against one of the target's heads.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct ResolutionsInput {
    /// The target's head the resolutions were decided against: the conflict report's `theirs`,
    /// by its full 64-hex id. A resolution applies while the target holds its path as this
    /// changeset does (the same file, the same directory, or nothing).
    pub against: Rev,
    /// What to make of each path, by its repository path.
    pub files: BTreeMap<String, ResolutionInput>,
}

/// What a land makes of one path, and of every path below it: exactly one of `take`, `content`
/// and `delete` (anything else is `E_INVALID_ARG`, a rule the server checks), the rest of the
/// merge as it merged.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
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
pub enum Side {
    /// The change: its head.
    Ours,
    /// The target: its head when the land merges.
    Theirs,
}

/// `land`: where the change landed. Ids are full.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
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
    /// The paths both sides changed that merged cleanly. Omitted when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub auto_merged: Vec<String>,
    /// Problems found after the land became durable, such as failed housekeeping; the land
    /// itself succeeded. Omitted when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// How a land ended.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
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
pub struct ConflictReport {
    /// The merge base: the changeset the change starts from.
    pub base: String,
    /// The change's head.
    pub ours: String,
    /// The target's head the change was merged with.
    pub theirs: String,
    /// The files that conflict, by path.
    pub files: Vec<FileConflictOut>,
    /// The kinds of conflict in `files`, each once.
    pub kinds: Vec<ConflictKindOut>,
    /// The paths both sides changed that merged cleanly. Omitted when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub auto_merged: Vec<String>,
    /// The paths whose resolutions did not apply, since the target holds there what their
    /// `against` did not: the target changed them after the report the resolutions answered.
    /// Read them at `theirs` and resolve them again, against it. Omitted when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stale: Vec<String>,
    /// Whether hunk texts were left out to keep the report within the request's
    /// `max_conflict_bytes`; the line positions and blob ids are always there.
    pub truncated: bool,
}

/// One conflicting file. Its blobs are the file's content on each side, which `read_file` reads
/// at that side's changeset; a side without a file at the path (deleted, never added, or a
/// directory there) has none.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
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
    /// both sides edited (`content`, and `add_add` of two texts). Omitted when there are none.
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

/// What kind of conflict a file has. `ours` is the change, `theirs` the target.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
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
    /// One side has a file where the other has a directory.
    TypeChange,
}

/// One place where the two sides' edits of a text file meet: each side's lines there. The lines
/// are numbered from 1, and each side's range includes both ends, as `read_file` takes a range
/// of lines.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
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

/// One side's lines of a hunk: `first` to `last`, numbered from 1, both included. A side without
/// lines there has `last` one less than `first`: its place is before line `first`.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct LinesOut {
    /// The first line.
    pub first: u64,
    /// The last line; `first` minus one when the side has none.
    pub last: u64,
    /// The lines' text; omitted when the report would pass its `max_conflict_bytes`
    /// (`truncated`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// `op_log` (CLI `op log`): the store's operations, newest first, a page at a time. An operation
/// is one write transaction: a frame of the journal, numbered from 0 in commit order.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
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

/// `op_log` (CLI `op log`): a page of operations, newest first.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct OpLogPage {
    /// The operations.
    pub ops: Vec<OperationOut>,
    /// Whether older operations follow this page: give the last one's `op` as `before` to read
    /// them.
    pub truncated: bool,
}

/// One operation: a write transaction, as the journal recorded it.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
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

/// What an operation did, read from what it recorded.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
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

/// A move of a ref, as the journal recorded it.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
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

/// `undo` (CLI `op undo`): restore every ref an operation moved to what it named before, by
/// compare-and-swap, in one transaction: all of them or none. Changesets are never removed.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct UndoReq {
    /// The operation to undo: its `op` in the operation log.
    pub op: u64,
}

/// `undo` (CLI `op undo`): the moves that restored the refs.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
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
    /// caller names (M10 adds the client-side pass that turns it into bytes before a request
    /// leaves the client).
    fn commit(&self, who: &Identity, req: CommitReq) -> Result<CommitResp, OpError>;

    /// Merges a change three-way onto a ref, `main` by default, and publishes the result there as
    /// one new changeset, with the ref's move, in one transaction (CLI `land`). A change that
    /// conflicts is `E_CONFLICT`, with the conflict report in `details.report`, and records
    /// nothing.
    fn land(&self, who: &Identity, req: LandReq) -> Result<LandResp, OpError>;

    /// The changeset a revision names, and whether the revision is exactly a ref's name (CLI
    /// `resolve`).
    fn resolve_rev(&self, req: ResolveRevReq) -> Result<ResolvedRev, OpError>;

    /// Stores content as a blob, outside any commit, for a commit to name by its id (CLI `blob
    /// put`). A mutation, so it takes the caller's identity, which it records nowhere: a blob has
    /// no metadata.
    fn blob_put(&self, who: &Identity, req: BlobPutReq) -> Result<BlobPutResp, OpError>;

    /// Which of the given blob ids the store holds blobs under (CLI `blob has`).
    fn blob_has(&self, req: BlobHasReq) -> Result<BlobHasResp, OpError>;

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

    /// Every ref (CLI `ref list`).
    fn ref_list(&self, req: RefListReq) -> Result<RefListResp, OpError>;

    /// Moves a ref, or creates one, by compare-and-swap (CLI `ref set`).
    fn ref_set(&self, who: &Identity, req: RefSetReq) -> Result<RefSetResp, OpError>;

    /// Removes a ref by compare-and-swap (CLI `ref delete`).
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
}

// The CLI holds a `dyn StoreOps` (operations spec §7); this stops compiling if the trait stops
// being object-safe.
const _: Option<&dyn StoreOps> = None;
