//! Operation requests and responses, and [`StoreOps`], the boundary every store implements
//! (operations spec §0–§1, §7).
//!
//! Requests and responses alike ignore fields they do not know: the operations spec (§0) forbids
//! `deny_unknown_fields`, so stores and clients of different versions keep working together.
//! Absent options are omitted, never written as null. Ids are lowercase hex. A response that
//! could grow without bound is capped by its request (`max_bytes`, `limit`, `max_changes`) and
//! says when it was cut (`truncated`, `changes_truncated`); `ls` and `log` also return a
//! `cursor` that resumes after the page.
//!
//! No store-facing type carries a filesystem path. The two exceptions are client-side:
//! [`ContentInput::File`], which the client reads and sends as bytes, and [`InitReq::path`],
//! because `init` runs on the client and is not a [`StoreOps`] method.
//!
//! Not in M1 yet: `ContentInput::Blob` and `CatReq.range` (M2), `idempotency_key` and
//! `replayed` on `commit` (M4, with land), and `RepoInfo.workspaces` (M5).

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::OpError;
use crate::identity::Identity;
use crate::rev::Rev;

/// The default [`CatReq::max_bytes`]: 1 MiB.
pub const DEFAULT_MAX_BYTES: u64 = 1024 * 1024;

/// The default [`LogReq::limit`].
pub const DEFAULT_LOG_LIMIT: usize = 50;

/// The default [`LsReq::limit`].
pub const DEFAULT_LS_LIMIT: usize = 1000;

/// The default [`ShowReq::max_changes`].
pub const DEFAULT_MAX_CHANGES: usize = 1000;

fn default_max_bytes() -> u64 {
    DEFAULT_MAX_BYTES
}

fn default_log_limit() -> usize {
    DEFAULT_LOG_LIMIT
}

fn default_ls_limit() -> usize {
    DEFAULT_LS_LIMIT
}

fn default_max_changes() -> usize {
    DEFAULT_MAX_CHANGES
}

/// `init`: create a store.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct InitReq {
    /// The directory to create the store in.
    pub path: String,
    /// The repository's name.
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

/// `commit`: record a changeset from an explicit change list. It moves no ref.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct CommitReq {
    /// The changeset to commit on top of. An explicit `parent` always wins; when it is omitted,
    /// the current head of `main` is used if `main` exists, and otherwise the commit is a root
    /// commit. `null` is the same as omitting it. Once `main` exists, this API cannot express a
    /// root commit (a known M1 limitation).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<Rev>,
    /// What to change, relative to the parent.
    pub changes: Vec<ChangeInput>,
    /// The commit message.
    pub message: String,
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
    /// The new content; a `put` needs it, a `delete` takes none.
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
    /// How many blobs the commit wrote to the object store.
    pub blobs_written: u64,
    /// Problems found after the commit became durable, such as failed housekeeping; the commit
    /// itself succeeded. Omitted when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// `cat`: read one file at a revision, without a checkout.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct CatReq {
    /// The revision to read at.
    pub rev: Rev,
    /// The file's repository path.
    pub path: String,
    /// The most content bytes to return; a longer file comes back truncated.
    #[serde(default = "default_max_bytes")]
    pub max_bytes: u64,
}

/// `cat`: a file's content.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct FileContent {
    /// The blob's id.
    pub blob: String,
    /// The whole blob's size in bytes, also when `content` is truncated.
    pub size: u64,
    /// The file's kind.
    pub kind: FileKind,
    /// How `content` encodes the bytes it returns.
    pub encoding: ContentEncoding,
    /// The bytes returned: the whole blob, or at most `max_bytes` from its start when
    /// `truncated`.
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

/// `ls`: list one directory at a revision, without a checkout, a page at a time.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct LsReq {
    /// The revision to read at.
    pub rev: Rev,
    /// The directory's repository path; omitted for the root.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The most entries to return.
    #[serde(default = "default_ls_limit")]
    pub limit: usize,
    /// Resume a listing: the previous page's `cursor`; this page starts after that entry name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

/// `ls`: a page of a directory's entries, in the tree's order (by name bytes).
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct TreeListing {
    /// The entries.
    pub entries: Vec<TreeEntryOut>,
    /// Whether more entries follow this page.
    pub truncated: bool,
    /// When `truncated`: the last entry's name; pass it as `cursor` to get the next page.
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
    /// The most entries of `changes` to return.
    #[serde(default = "default_max_changes")]
    pub max_changes: usize,
}

/// `show`: a changeset's header, changes and metadata. Byte strings are decoded as UTF-8, with
/// invalid bytes replaced.
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
    /// SwarmOps metadata, such as `agent` and `run_id`.
    pub metadata: BTreeMap<String, String>,
    /// The git author and committer signatures, kept for git fidelity.
    pub signatures: Signatures,
}

/// Whom a changeset is attributed to (operations spec §5).
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct Attribution {
    /// The agent that wrote it: the changeset's `agent` metadata.
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
    /// The most entries to return.
    #[serde(default = "default_log_limit")]
    pub limit: usize,
    /// Resume a log: the previous page's `cursor`, the last changeset id it returned. This page
    /// continues from that changeset's first parent, and `rev` is not used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

/// `log`: a page of changesets, newest first, following first parents.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct LogPage {
    /// The changesets.
    pub entries: Vec<ChangesetSummary>,
    /// Whether older history follows this page.
    pub truncated: bool,
    /// When `truncated`: the last entry's id; pass it as `cursor` to continue.
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
    /// The agent that wrote it.
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

/// `ref set`: move a ref, or create one, by compare-and-swap.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct RefSetReq {
    /// The ref: `main` or `release/<name>`.
    pub name: String,
    /// The revision to point it at.
    pub new: Rev,
    /// The target the caller last saw; if the ref holds another, the move fails with
    /// `E_CAS_MISMATCH`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_old: Option<Rev>,
    /// Create the ref; if it exists already, the move fails with `E_CAS_MISMATCH`.
    #[serde(default)]
    pub create: bool,
}

/// `ref set` and `ref delete`: the move that was made.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct RefSetResp {
    /// The ref.
    pub name: String,
    /// Its target before; omitted when the ref was created.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old: Option<String>,
    /// Its target after; omitted when the ref was deleted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new: Option<String>,
    /// The ref was already in the requested state, so nothing changed and a replay is safe.
    pub already_applied: bool,
}

/// `ref delete`: remove a ref by compare-and-swap.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct RefDeleteReq {
    /// The ref.
    pub name: String,
    /// The target the caller last saw; if the ref holds another, the delete fails with
    /// `E_CAS_MISMATCH`.
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
    /// One line per problem found; empty for a clean store.
    pub problems: Vec<String>,
}

/// One store's operations, implemented over a local store now and by a remote client later
/// (operations spec §7).
///
/// Synchronous and object-safe, so callers hold a `dyn StoreOps`. Mutations take the caller's
/// [`Identity`]; reads take none.
pub trait StoreOps {
    /// What the store is and holds (CLI `info`).
    fn repo_info(&self, req: InfoReq) -> Result<RepoInfo, OpError>;

    /// Records a changeset from an explicit change list; moves no ref (CLI `commit`).
    fn commit(&self, who: &Identity, req: CommitReq) -> Result<CommitResp, OpError>;

    /// Reads one file at a revision (CLI `cat`).
    fn read_file(&self, req: CatReq) -> Result<FileContent, OpError>;

    /// Lists one directory at a revision (CLI `ls`).
    fn read_tree(&self, req: LsReq) -> Result<TreeListing, OpError>;

    /// One changeset in full (CLI `show`).
    fn show(&self, req: ShowReq) -> Result<ChangesetDetail, OpError>;

    /// History along first parents, newest first (CLI `log`).
    fn log(&self, req: LogReq) -> Result<LogPage, OpError>;

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
