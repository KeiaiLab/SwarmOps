//! The git remotes of a store (M13): the requests and responses of `clone`, `remote add`,
//! `remote remove`, `remote list`, `fetch`, `push`, `sync` and `sync status`.
//!
//! These are client-side operations: they spawn git on the caller's host against the store's
//! own mirror, so no `StoreOps` has them. `remote_list`, `fetch`, `push`, `sync` and
//! `sync_status` are MCP tools too, and take a remote by its name only. A remote's URL appears
//! only in the requests that write the store's remote configuration (`CloneReq`,
//! `RemoteAddReq`) and in [`RemoteInfo`]; a clone's directory is a client path, as `InitReq`'s
//! is. No credential is ever a field: a remote may name the environment variable that holds a
//! token (`token_env`), never the token.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ops::{ChangesetSummary, DEFAULT_MAX_ATTEMPTS, LossyClass, MAX_LAND_ATTEMPTS, RepoInfo};

/// The remote a clone records when the request names none.
pub const DEFAULT_REMOTE: &str = "origin";

/// The store ref a push sends when the request names none.
pub const DEFAULT_PUSH_SOURCE: &str = "main";

/// How many unpublished changesets `sync status` lists when the request says nothing.
pub const DEFAULT_SYNC_STATUS_LIMIT: usize = 50;

/// The most branches a fetch's response lists; past it, `branches_truncated` is set.
pub const MAX_FETCH_BRANCHES: usize = 1_000;

fn default_remote() -> String {
    DEFAULT_REMOTE.to_owned()
}

fn default_push_source() -> String {
    DEFAULT_PUSH_SOURCE.to_owned()
}

fn default_sync_attempts() -> u32 {
    DEFAULT_MAX_ATTEMPTS
}

fn default_sync_status_limit() -> usize {
    DEFAULT_SYNC_STATUS_LIMIT
}

// ---- clone ----

/// `clone`: a new store in a directory of the caller's host, holding a git remote's history,
/// with the remote recorded and its followed branch imported into `main`. It needs an
/// identity, as `git import` does: it moves `main`.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CloneReq {
    /// The remote's URL: `https://host/path`, `ssh://[user@]host/path` or the scp form
    /// `user@host:path`. A URL with a password, another scheme, or one that starts with `-` is
    /// refused; plain `http://` only to a loopback address.
    pub url: String,
    /// The directory to create the store in, on the caller's host: absent or empty. A
    /// directory holding a store with this remote at this URL continues an interrupted clone.
    pub path: String,
    /// The repository's name; by default the URL's last path component without `.git`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The remote branch `main` follows; by default the remote's default branch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// The name to record the remote under: `origin` by default.
    #[serde(default = "default_remote")]
    pub remote: String,
    /// The environment variable that holds a token for the remote, by its name; the token is
    /// never stored.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_env: Option<String>,
    /// The classes of content the import of this remote's commits may drop or normalize, as
    /// `git import`'s `lossy`.
    #[serde(default)]
    pub lossy: Vec<LossyClass>,
}

impl CloneReq {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(url: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            path: path.into(),
            name: None,
            branch: None,
            remote: default_remote(),
            token_env: None,
            lossy: Vec::new(),
        }
    }
}

/// `clone`: the store made, the remote recorded, its fetch, and where `main` stands.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CloneResp {
    /// The new store.
    pub repo: RepoInfo,
    /// The remote recorded.
    pub remote: RemoteInfo,
    /// The clone's fetch.
    pub fetch: FetchResp,
    /// How many changesets the import into `main` added.
    pub imported: u64,
    /// The changeset `main` names; omitted when the remote is empty.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub main: Option<String>,
}

impl CloneResp {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(repo: RepoInfo, remote: RemoteInfo, fetch: FetchResp, imported: u64) -> Self {
        Self {
            repo,
            remote,
            fetch,
            imported,
            main: None,
        }
    }
}

// ---- remote add, remove, list ----

/// `remote add`: record a git remote in the store. It needs no identity: it writes the store's
/// remote configuration and its mirror, no store object.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RemoteAddReq {
    /// The remote's name: lowercase letters, digits, `.`, `_` and `-`, 1 to 63 of them,
    /// starting with a letter or a digit, not ending in `.`, and not `status`.
    pub name: String,
    /// The remote's URL, as a clone's `url` is.
    pub url: String,
    /// The remote branch `main` follows; by default the remote's default branch, which asks
    /// the remote (a network call).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// The environment variable that holds a token for the remote, by its name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_env: Option<String>,
    /// The classes of content the import of this remote's commits may drop or normalize.
    #[serde(default)]
    pub lossy: Vec<LossyClass>,
}

impl RemoteAddReq {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(name: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            url: url.into(),
            branch: None,
            token_env: None,
            lossy: Vec::new(),
        }
    }
}

/// A remote the store records.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RemoteInfo {
    /// The remote's name.
    pub name: String,
    /// The remote's URL, as it was given.
    pub url: String,
    /// The remote branch `main` follows.
    pub branch: String,
    /// The environment variable that holds a token for the remote, by its name; omitted when
    /// the remote names none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_env: Option<String>,
    /// The classes of content the import of this remote's commits may drop or normalize.
    pub lossy: Vec<LossyClass>,
    /// What adding the remote has to say that did not stop it, such as
    /// `default_branch_unknown` (the remote named no default branch, so `branch` is `main`).
    /// Omitted when there is nothing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warnings: Option<Vec<String>>,
}

impl RemoteInfo {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(
        name: impl Into<String>,
        url: impl Into<String>,
        branch: impl Into<String>,
        lossy: Vec<LossyClass>,
    ) -> Self {
        Self {
            name: name.into(),
            url: url.into(),
            branch: branch.into(),
            token_env: None,
            lossy,
            warnings: None,
        }
    }
}

/// `remote remove`: forget a remote, and the mirror's refs of it. No changeset is removed.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RemoteRemoveReq {
    /// The remote's name.
    pub name: String,
}

impl RemoteRemoveReq {
    /// Takes every field.
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }
}

/// `remote remove`: the remote forgotten.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RemoteRemoveResp {
    /// The remote's name.
    pub name: String,
    /// How many of the mirror's refs of the remote were removed.
    pub removed_refs: u64,
}

impl RemoteRemoveResp {
    /// Takes every field.
    pub fn new(name: impl Into<String>, removed_refs: u64) -> Self {
        Self {
            name: name.into(),
            removed_refs,
        }
    }
}

/// `remote list`: the remotes the store records.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct RemoteListReq {}

/// `remote list`: every remote, in the order they were added.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RemoteList {
    /// The remotes; empty when the store has none.
    pub remotes: Vec<RemoteInfo>,
}

impl RemoteList {
    /// Takes every field.
    pub fn new(remotes: Vec<RemoteInfo>) -> Self {
        Self { remotes }
    }
}

// ---- fetch ----

/// `fetch`: bring a remote's branches and tags into the store's mirror, and its followed
/// branch's commits into the store as changesets. No ref of the store moves, so it needs no
/// identity.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct FetchReq {
    /// The remote, by its name.
    pub remote: String,
}

impl FetchReq {
    /// Takes every field.
    pub fn new(remote: impl Into<String>) -> Self {
        Self {
            remote: remote.into(),
        }
    }
}

/// A remote branch's head: its commit, and the changeset that commit is in the store.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RemoteHead {
    /// The commit's git id, 40 lowercase hex digits.
    pub sha: String,
    /// The changeset id of that commit in the store.
    pub changeset: String,
}

impl RemoteHead {
    /// Takes every field.
    pub fn new(sha: impl Into<String>, changeset: impl Into<String>) -> Self {
        Self {
            sha: sha.into(),
            changeset: changeset.into(),
        }
    }
}

/// A remote branch a fetch saw move, appear or go.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct BranchUpdate {
    /// The branch, by its short name.
    pub branch: String,
    /// The commit it named before; omitted for a branch that appeared.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old: Option<String>,
    /// The commit it names now; omitted for a branch the remote deleted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new: Option<String>,
}

impl BranchUpdate {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(branch: impl Into<String>) -> Self {
        Self {
            branch: branch.into(),
            old: None,
            new: None,
        }
    }
}

/// `fetch`: where the followed branch stands, and what moved.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct FetchResp {
    /// The remote, by its name.
    pub remote: String,
    /// The followed branch.
    pub branch: String,
    /// The followed branch's head after the fetch; omitted when the remote has no such branch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<RemoteHead>,
    /// The followed branch's head as the last fetch saw it; omitted when none did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_head: Option<RemoteHead>,
    /// How many changesets the import of the followed branch added.
    pub imported: u64,
    /// The remote branches that moved, appeared or went, at most 1,000.
    pub branches: Vec<BranchUpdate>,
    /// Whether more branches moved than `branches` lists.
    pub branches_truncated: bool,
    /// How many tags the fetch brought into the mirror (they are never imported).
    pub tags_fetched: u64,
    /// What the fetch has to say that did not stop it. Omitted when there is nothing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warnings: Option<Vec<String>>,
}

impl FetchResp {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(
        remote: impl Into<String>,
        branch: impl Into<String>,
        imported: u64,
        branches: Vec<BranchUpdate>,
        branches_truncated: bool,
        tags_fetched: u64,
    ) -> Self {
        Self {
            remote: remote.into(),
            branch: branch.into(),
            head: None,
            old_head: None,
            imported,
            branches,
            branches_truncated,
            tags_fetched,
            warnings: None,
        }
    }
}

// ---- push ----

/// `push`: send a store ref to a remote branch by compare-and-swap, never forcing: the remote
/// moves only from the head the last fetch saw to a descendant of it. A push is an export, so
/// it needs no identity.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PushReq {
    /// The remote, by its name.
    pub remote: String,
    /// The store ref to send: `main` (the default) or `release/<name>`.
    #[serde(default = "default_push_source")]
    pub source_ref: String,
    /// The remote branch; by default the remote's followed branch for `main`, and the same
    /// name for `release/<name>`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

impl PushReq {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(remote: impl Into<String>) -> Self {
        Self {
            remote: remote.into(),
            source_ref: default_push_source(),
            branch: None,
        }
    }
}

/// What a push did to the remote branch.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PushStatus {
    /// The branch moved to the export of the ref's head.
    Pushed,
    /// The branch named it already: nothing was sent.
    UpToDate,
    /// The branch did not exist, and the push created it.
    Created,
}

/// `push`: where the remote branch stands.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PushResp {
    /// The remote, by its name.
    pub remote: String,
    /// The remote branch.
    pub branch: String,
    /// What the push did.
    pub status: PushStatus,
    /// The commit the branch named before; omitted when it did not exist.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old: Option<String>,
    /// The commit the branch names now: the export of `head_changeset`.
    pub new: String,
    /// The changeset at the store ref's head.
    pub head_changeset: String,
    /// How many git objects the push's export wrote into the mirror.
    pub objects_written: u64,
    /// How many of the exported commits have another id than the commit they were imported
    /// from, as `git export`'s `rewritten`.
    pub rewritten: u64,
    /// What the push has to say that did not stop it. Omitted when there is nothing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warnings: Option<Vec<String>>,
}

impl PushResp {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(
        remote: impl Into<String>,
        branch: impl Into<String>,
        status: PushStatus,
        new: impl Into<String>,
        head_changeset: impl Into<String>,
        objects_written: u64,
        rewritten: u64,
    ) -> Self {
        Self {
            remote: remote.into(),
            branch: branch.into(),
            status,
            old: None,
            new: new.into(),
            head_changeset: head_changeset.into(),
            objects_written,
            rewritten,
            warnings: None,
        }
    }
}

// ---- sync ----

/// Whether `sync` sends what it landed.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SyncMode {
    /// Land onto the remote's head, move `main`, and push it.
    #[default]
    Publish,
    /// The same without the push: for a branch that takes pull requests only.
    Local,
}

/// `sync`: fetch a remote, land `main`'s unpublished changes onto the remote's head, move
/// `main` there and push it. It moves `main`, so it needs an identity.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SyncReq {
    /// The remote, by its name.
    pub remote: String,
    /// `publish` (the default) pushes; `local` stops before the push.
    #[serde(default)]
    pub mode: SyncMode,
    /// How many times to fetch and land again when the remote or `main` moved meanwhile: 1 to
    /// 32.
    #[serde(default = "default_sync_attempts")]
    #[schemars(range(min = 1, max = MAX_LAND_ATTEMPTS))]
    pub max_attempts: u32,
}

impl SyncReq {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(remote: impl Into<String>) -> Self {
        Self {
            remote: remote.into(),
            mode: SyncMode::default(),
            max_attempts: default_sync_attempts(),
        }
    }
}

/// What a sync did.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SyncStatusKind {
    /// `main` and the remote held the same: nothing moved.
    UpToDate,
    /// The landed changes were pushed.
    Pushed,
    /// `main` had nothing unpublished and moved to the remote's head.
    FastForwarded,
    /// `main` moved onto the remote's head and nothing was pushed (`mode: local`).
    LandedLocally,
}

/// An unpublished change a sync landed onto the remote's head.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PublishedChange {
    /// The change as `main` held it.
    pub changeset: String,
    /// The changeset it was landed as, on the remote's head.
    pub landed_as: String,
    /// The commit `landed_as` exports to.
    pub sha: String,
}

impl PublishedChange {
    /// Takes every field.
    pub fn new(
        changeset: impl Into<String>,
        landed_as: impl Into<String>,
        sha: impl Into<String>,
    ) -> Self {
        Self {
            changeset: changeset.into(),
            landed_as: landed_as.into(),
            sha: sha.into(),
        }
    }
}

/// How a sync found that the remote holds a change already.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DropReason {
    /// A commit the remote's head reaches names it in a `SwarmOps-Changeset`,
    /// `SwarmOps-Squashed-From` or `SwarmOps-Land-Key` line.
    Trailer,
    /// It merges onto the remote's head as no change.
    NoOp,
}

/// An unpublished change a sync dropped, since the remote holds it already. It stays in the
/// store, and an undo of the sync's ref move brings it back on `main`.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DroppedChange {
    /// The change.
    pub changeset: String,
    /// How the sync found it on the remote.
    pub by: DropReason,
}

impl DroppedChange {
    /// Takes every field.
    pub fn new(changeset: impl Into<String>, by: DropReason) -> Self {
        Self {
            changeset: changeset.into(),
            by,
        }
    }
}

/// A head before and after a sync.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct HeadMove {
    /// What it named before; omitted when it did not exist.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old: Option<String>,
    /// What it names now.
    pub new: String,
}

impl HeadMove {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(new: impl Into<String>) -> Self {
        Self {
            old: None,
            new: new.into(),
        }
    }
}

/// `sync`: what it landed, dropped and sent, and where `main` and the remote stand.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SyncResp {
    /// The remote, by its name.
    pub remote: String,
    /// The followed branch.
    pub branch: String,
    /// What the sync did.
    pub status: SyncStatusKind,
    /// How many attempts it took.
    pub attempts: u32,
    /// The changes landed onto the remote's head, oldest first.
    pub published: Vec<PublishedChange>,
    /// The changes the remote held already.
    pub dropped: Vec<DroppedChange>,
    /// `main`'s changeset before and after.
    pub main: HeadMove,
    /// The remote branch's commit before and after.
    pub remote_head: HeadMove,
    /// What the sync has to say that did not stop it. Omitted when there is nothing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warnings: Option<Vec<String>>,
}

impl SyncResp {
    /// Takes the required fields; the others get their serde defaults.
    // Every argument is a required field of the response: no default can stand in.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        remote: impl Into<String>,
        branch: impl Into<String>,
        status: SyncStatusKind,
        attempts: u32,
        published: Vec<PublishedChange>,
        dropped: Vec<DroppedChange>,
        main: HeadMove,
        remote_head: HeadMove,
    ) -> Self {
        Self {
            remote: remote.into(),
            branch: branch.into(),
            status,
            attempts,
            published,
            dropped,
            main,
            remote_head,
            warnings: None,
        }
    }
}

// ---- sync status ----

/// `sync status`: how `main` and a remote's followed branch stand, as the last fetch saw the
/// remote. It reads only the store and its mirror: no network, no lock.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SyncStatusReq {
    /// The remote, by its name.
    pub remote: String,
    /// The most unpublished changesets to list.
    #[serde(default = "default_sync_status_limit")]
    pub limit: usize,
}

impl SyncStatusReq {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(remote: impl Into<String>) -> Self {
        Self {
            remote: remote.into(),
            limit: default_sync_status_limit(),
        }
    }
}

/// `sync status`: what is unpublished, and how far behind `main` is.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SyncStatus {
    /// The remote, by its name.
    pub remote: String,
    /// The followed branch.
    pub branch: String,
    /// Whether a fetch ever saw the followed branch.
    pub fetched: bool,
    /// The followed branch's head as the last fetch saw it; omitted when none did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_head: Option<RemoteHead>,
    /// The newest changeset on `main`'s first-parent chain that the remote's head reaches;
    /// omitted when there is none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// How many changes on `main` are above the base: unpublished.
    pub ahead: u64,
    /// How many changesets on the remote's first-parent chain are above the base.
    pub behind: u64,
    /// The unpublished changes, newest first, at most `limit`.
    pub unpublished: Vec<ChangesetSummary>,
    /// Whether a count or the list stopped at its bound.
    pub truncated: bool,
}

impl SyncStatus {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(
        remote: impl Into<String>,
        branch: impl Into<String>,
        fetched: bool,
        ahead: u64,
        behind: u64,
        unpublished: Vec<ChangesetSummary>,
        truncated: bool,
    ) -> Self {
        Self {
            remote: remote.into(),
            branch: branch.into(),
            fetched,
            remote_head: None,
            base: None,
            ahead,
            behind,
            unpublished,
            truncated,
        }
    }
}
