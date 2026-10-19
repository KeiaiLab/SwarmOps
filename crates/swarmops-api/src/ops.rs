//! Operation requests and responses, and [`StoreOps`], the boundary every store implements
//! (operations spec §0–§1, §7).
//!
//! Requests and responses alike ignore fields they do not know: the operations spec (§0) forbids
//! `deny_unknown_fields`, so stores and clients of different versions keep working together.
//! Absent options in these types are omitted, never written as null (unlike `OpError.hint`,
//! which is always written). Ids are lowercase hex. A response that could grow without bound is
//! capped by its request (`max_bytes`, `limit`, `max_changes`) and says when it was cut
//! (`truncated`, `changes_truncated`); `read_tree` and `log` also return a `cursor` that resumes
//! after the page. The default page of `read_tree`, `show` and `log` stays within the operations
//! spec's 64 KiB target for a default response (§9); `read_file`'s 1 MiB is the spec's own.
//!
//! No store-facing type carries a filesystem path. The two exceptions are client-side:
//! [`ContentInput::File`], which the client reads and sends as bytes, and [`InitReq::path`],
//! because `init` runs on the client and is not a [`StoreOps`] method.
//!
//! Not in M1 yet:
//! - M2: `ContentInput::Blob`, `ReadFileReq.range`, `ReadTreeReq.depth`, the `size` of each
//!   `TreeEntryOut`, and `LogReq.paths` and `LogReq.first_parent`;
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
    /// What to change, relative to the parent.
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
    /// The new content, at most 512 MiB (more is `E_TOO_LARGE`); a `put` needs it, a `delete`
    /// takes none.
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

/// Why a land could not merge its change onto the target, file by file (operations spec §3): the
/// report `E_CONFLICT` carries in `details.report`. `ours` is the change, `theirs` the target.
/// Ids are full.
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

    /// Reads one file at a revision (CLI `cat`).
    fn read_file(&self, req: ReadFileReq) -> Result<FileContent, OpError>;

    /// Lists one directory at a revision (CLI `ls`).
    fn read_tree(&self, req: ReadTreeReq) -> Result<TreeListing, OpError>;

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
