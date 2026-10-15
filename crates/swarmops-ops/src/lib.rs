//! SwarmOps operations (operations spec §0–§1, §5, §7): the client side of the store boundary,
//! between the serde contract of `swarmops-api` and the repository library `swarmops-repo`.
//!
//! - [`LocalOps`] implements [`StoreOps`](swarmops_api::StoreOps) over a store on the local
//!   filesystem. It resolves each request's revisions, turns requests into repository calls and
//!   their results into responses, and maps every failure to the operations spec's error codes.
//! - [`identity`] works out who the caller is, from `--as` or the environment, and refuses a
//!   mutation without an identity.
//! - [`content`] checks a commit's change list and turns its contents into bytes, reading
//!   `file` contents from the client's filesystem; and turns stored bytes back into JSON text.
//!
//! The CLI calls these and prints each result in one envelope. A remote client will implement
//! `StoreOps` over the same types later, resolving identities and file contents the same way.

pub mod content;
mod error;
pub mod identity;
mod local;

pub use local::{LocalOps, NOW_ENV};
