//! SwarmOps operations (operations spec §0–§1, §5, §7): the client side of the store boundary,
//! between the serde contract of `swarmops-api` and the repository library `swarmops-repo`.
//!
//! - [`LocalOps`] implements [`StoreOps`](swarmops_api::StoreOps) over a store on the local
//!   filesystem. It resolves each request's revisions, turns requests into repository calls and
//!   their results into responses, and maps every failure to the operations spec's error codes.
//!   [`LocalOps::recording`] runs a request's operations so that what they resolve is recorded
//!   in that request's [`Resolved`] alone.
//! - [`identity`] works out who the caller is, from `--as` or the environment, and refuses a
//!   mutation without an identity.
//! - [`content`] checks a commit's change list and turns its contents into bytes, reading
//!   `file` contents from the client's filesystem; and turns stored bytes back into JSON text.
//! - `git import` ([`LocalOps::git_import`], [`Recording::git_import`]) brings a git
//!   repository's history into the store, reading the repository on the client's host: a
//!   client-side operation, as `init` is, and so no `StoreOps` method.
//! - The git remotes (M13): `clone` ([`LocalOps::clone_repo`]), `remote add|remove|list`,
//!   `fetch`, `push`, `sync` and `sync status` ([`Recording::fetch`], [`Recording::push`],
//!   [`Recording::sync`], …), client-side operations that spawn git against the store's mirror.
//!
//! - [`serve`] (M14): a directory of stores served to stock git over ssh and smart HTTP, its
//!   pushes decided by a `proc-receive` hook.
//!
//! - [`check`] (M30) reads a tree's checks: `.swarmops/checks.toml`, or `check.argv` of
//!   `.swarmops/workspaces.json`, from the store at a changeset or from a directory the caller
//!   names, for `check list`.
//!
//! - [`userdir`] names the host's per-user directory for what is kept outside every store.
//! - [`auth`] keeps the user's logins, a token per host, in one file under that directory:
//!   `auth login`, `auth status` and `auth logout`, client-side operations on no store.
//!
//! - [`update`] holds what the program's own update decides without a file or the network: the
//!   order of versions, the installer's receipt, whether an install updates itself, and what an
//!   automatic update remembers and says. The CLI reads the files and runs the children.
//!
//! - The landing queue (M22): `queue_enqueue`, `queue_status` and `queue_withdraw`, and the
//!   runner's [`QueueDriver`](swarmops_ws::QueueDriver), which [`LocalOps`] implements.
//!
//! - Maintenance (M6): `gc`, `repair` and `upgrade`, [`StoreOps`](swarmops_api::StoreOps) methods
//!   that record nothing, so they take no identity.
//!
//! The CLI calls these and prints each result in one envelope. A remote client will implement
//! `StoreOps` over the same types later, resolving identities and file contents the same way.

pub mod auth;
mod authors;
mod blob_get;
pub mod check;
mod conflict;
pub mod content;
mod error;
pub mod expose;
mod gc;
mod git;
pub mod health;
pub mod identity;
mod local;
pub mod native;
mod pattern;
mod phase;
mod push;
mod queue;
mod rebase;
mod records;
mod remote;
mod repair;
pub mod serve;
mod sync;
pub mod update;
mod upgrade;
pub mod userdir;

pub use error::shell_word;
pub use local::{LocalOps, NOW_ENV, Recording, Resolved};
/// The key [`LocalOps::with_memo_key`] takes, which the caller keeps outside the store.
pub use swarmops_git::{MEMO_KEY_LEN, MemoKey};
