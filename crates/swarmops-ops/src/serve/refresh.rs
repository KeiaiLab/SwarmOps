//! What `serve.git` advertises is the committed snapshot's export (M14 design §4): before each
//! advertisement, every served store ref exported into it, and a ref the store no longer has
//! removed.

use swarmops_api::OpError;
use swarmops_git::serve::repo::ServeRepo;

use super::not_built;
use crate::LocalOps;

/// How many times a refresh reads a ref again after its compare-and-swap in `serve.git` failed.
pub const SERVE_REFRESH_ATTEMPTS: u32 = 8;

/// Which refs a refresh exports.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Scope {
    /// Every ref served.
    All,
    /// These store refs alone.
    Refs(Vec<String>),
}

/// What a refresh did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Refreshed {
    /// Refs moved in `serve.git`, or made.
    pub moved: u64,
    /// Refs removed from it.
    pub removed: u64,
}

/// Exports `scope`'s refs of the store `ops` serves into `repo`, from its committed snapshot.
pub fn refresh(ops: &LocalOps, repo: &ServeRepo, scope: &Scope) -> Result<Refreshed, OpError> {
    let _ = (ops, repo, scope);
    Err(not_built("the refresh of serve.git"))
}
