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
//! - Maintenance (M6): `gc`, `repair` and `upgrade`, [`StoreOps`](swarmops_api::StoreOps) methods
//!   that record nothing, so they take no identity.
//!
//! The CLI calls these and prints each result in one envelope. A remote client will implement
//! `StoreOps` over the same types later, resolving identities and file contents the same way.

mod authors;
mod blob_get;
mod conflict;
pub mod content;
mod error;
mod gc;
mod git;
pub mod identity;
mod local;
mod push;
mod rebase;
mod remote;
mod repair;
pub mod serve;
mod sync;
mod upgrade;

pub use error::shell_word;
pub use local::{LocalOps, NOW_ENV, Recording, Resolved};
/// The key [`LocalOps::with_memo_key`] takes, which the caller keeps outside the store.
pub use swarmops_git::{MEMO_KEY_LEN, MemoKey};
