//! Operation requests and responses, and [`StoreOps`], the boundary every store implements
//! (operations spec §0–§1, §7).
//!
//! Requests and responses alike ignore fields they do not know: the operations spec (§0) forbids
//! `deny_unknown_fields`, so stores and clients of different versions keep working together.
//! Absent options in these types are omitted, never written as null (unlike `OpError.hint`,
//! which is always written). Ids are lowercase hex. A response that could grow without bound is
//! capped by its request (`max_bytes`, `limit`, `max_changes`) and says when it was cut
//! (`truncated`, `changes_truncated`); `read_tree`, `log` and `path_history` also return a
//! `cursor` that resumes after the page. The default page of `read_tree`, `show`, `log` and
//! `path_history` stays within the operations spec's 64 KiB target for a default response (§9);
//! `read_file`'s 1 MiB is the spec's own.
//!
//! No store-facing type carries a filesystem path. The two exceptions are client-side:
//! [`ContentInput::File`], which the client reads and sends as bytes, and [`InitReq::path`],
//! because `init` runs on the client and is not a [`StoreOps`] method.
//!
//! Not in M1 yet:
//! - M2: `ReadFileReq.range`, `ReadTreeReq.depth`, the `size` of each `TreeEntryOut`, and
//!   `LogReq.first_parent`;
//! - M4 (with land): `idempotency_key` and `replayed` on `commit`;
//! - M4/M7: `lander`, `land_key`, `cr` and `annotations` on `ChangesetDetail` (`show`);
//! - M5: `RepoInfo.workspaces`.

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

/// The default [`ReadTreeReq::limit`]: 500 entries. An entry prints in about 100 bytes besides
/// its name, so a default page stays under the 64 KiB target for names of up to about 30 bytes.
pub const DEFAULT_READ_TREE_LIMIT: usize = 500;

/// The default [`ShowReq::max_changes`]: 400 changes. A change prints in about 110 bytes
/// besides its path, so a default page stays under the 64 KiB target for paths of up to about
/// 50 bytes.
pub const DEFAULT_MAX_CHANGES: usize = 400;

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

fn default_max_changes() -> usize {
    DEFAULT_MAX_CHANGES
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

/// `read_file` (CLI `cat`): read one file at a revision, without a checkout.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct ReadFileReq {
    /// The revision to read at.
    pub rev: Rev,
    /// The file's repository path.
    pub path: String,
    /// The most content bytes to return; a longer file comes back truncated.
    #[serde(default = "default_max_bytes")]
    pub max_bytes: u64,
}

/// `read_file` (CLI `cat`): a file's content.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct FileContent {
    /// The blob's id.
    pub blob: String,
    /// The whole blob's size in bytes, also when `content` is truncated.
    pub size: u64,
    /// The file's kind.
    pub kind: FileKind,
    /// How `content` encodes the bytes it returns: `utf8` when they are text, `base64` when they
    /// are not.
    pub encoding: ContentEncoding,
    /// The bytes returned: the whole blob, or the start of it when `truncated`. A cut keeps text
    /// as text: it falls where a character starts, so text may stop up to 3 bytes short of
    /// `max_bytes`, while bytes that are not text stop at exactly `max_bytes`.
    pub content: String,
    /// Whether `content` stops before the end of the blob.
    pub truncated: bool,
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
/// time.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct ReadTreeReq {
    /// The revision to read at.
    pub rev: Rev,
    /// The directory's repository path; omitted for the root.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The most entries to return: at least 1 (0 is `E_INVALID_ARG`).
    #[serde(default = "default_read_tree_limit")]
    #[schemars(range(min = 1))]
    pub limit: usize,
    /// Resume a listing: the previous page's `cursor`. Pass it back exactly as returned; do not
    /// construct it (cursors are opaque). This page starts after the entry it names.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

/// `read_tree` (CLI `ls`): a page of a directory's entries, in the tree's order (by name bytes).
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

/// One entry of a directory.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct TreeEntryOut {
    /// The entry's name: one path component.
    pub name: String,
    /// What the entry is.
    pub kind: EntryKindOut,
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
    /// When it was recorded: the committer signature's time, in Unix milliseconds.
    pub created_at: u64,
    /// The message.
    pub message: String,
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
/// string) and `create` (`true`). Both, neither, and an `expected_old` of null meet no branch,
/// as the operation refuses them. The wire does not change.
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
    /// `dangling_ref`, `corrupt_ref`, `index_mismatch`, `journal_corrupt` or `meta_corrupt`. A
    /// string, as `OpError.code` is, so that a client can read kinds newer than its own list.
    pub kind: String,
    /// The object: one whose content does not hash to its id or does not decode
    /// (`corrupt_object`), or one a changeset references that the store lacks
    /// (`missing_object`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub object: Option<String>,
    /// The changeset: the first, in seq order, that references the missing object
    /// (`missing_object`), the one that names a missing parent (`missing_parent`), or the one
    /// whose root is not its first parent's root with its delta applied (`root_mismatch`).
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

    /// Every ref (CLI `ref list`).
    fn ref_list(&self, req: RefListReq) -> Result<RefListResp, OpError>;

    /// Moves a ref, or creates one, by compare-and-swap (CLI `ref set`).
    fn ref_set(&self, who: &Identity, req: RefSetReq) -> Result<RefSetResp, OpError>;

    /// Removes a ref by compare-and-swap (CLI `ref delete`).
    fn ref_delete(&self, who: &Identity, req: RefDeleteReq) -> Result<RefSetResp, OpError>;

    /// Checks the whole store's consistency (CLI `verify`).
    fn verify(&self, req: VerifyReq) -> Result<VerifyReport, OpError>;
}

// The CLI holds a `dyn StoreOps` (operations spec §7); this stops compiling if the trait stops
// being object-safe.
const _: Option<&dyn StoreOps> = None;
