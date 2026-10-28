//! `serve --hook proc-receive` (M14 design §2.4, §6): the hook git runs inside a push, which
//! decides each command, by access, import and compare-and-swap, or lands it.

use swarmops_api::{Identity, OpError};
use swarmops_git::serve::repo::ServeRepo;

use super::access::Access;
use super::{ServeInvocation, Served, not_built};
use crate::LocalOps;

/// The most commands one push may hold: past it, every command is refused.
pub const MAX_PUSH_COMMANDS: usize = 1_000;

/// What the hook decides each command with.
#[non_exhaustive]
pub struct HookCtx<'a> {
    /// The store served.
    pub ops: &'a LocalOps,
    /// Its serving repository.
    pub repo: &'a ServeRepo,
    /// The caller.
    pub who: &'a Identity,
    /// The store's grants.
    pub access: &'a Access,
}

impl<'a> HookCtx<'a> {
    /// The context of `who` pushing to the store `ops` serves through `repo`, under `access`.
    pub fn new(
        ops: &'a LocalOps,
        repo: &'a ServeRepo,
        who: &'a Identity,
        access: &'a Access,
    ) -> HookCtx<'a> {
        HookCtx {
            ops,
            repo,
            who,
            access,
        }
    }
}

/// Runs the hook for `who`, the caller its parent resolved.
pub fn run(invocation: &ServeInvocation, who: &Identity) -> Result<Served, OpError> {
    let _ = (invocation, who);
    Err(not_built("the proc-receive hook"))
}
