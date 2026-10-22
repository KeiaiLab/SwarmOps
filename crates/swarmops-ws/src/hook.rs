//! The Claude Code adapter (§7 of the M5 design): the two commands a settings file registers as
//! the `WorktreeCreate` and `WorktreeRemove` hooks. Create makes a workspace for the session and
//! gives the path Claude Code prints; remove keeps uncommitted edits as a rescue changeset, then
//! deletes the workspace.
//!
//! The parsers take the hook's stdin bytes, at most [`MAX_HOOK_INPUT`]; the two hook functions
//! work against a [`Ctx`] the CLI built, and read no environment variable: the CLI hands what it
//! read as arguments ([`Bootstrap`], the workspaces' root).
//!
//! The M5 plan's task W1 named these; task W9 writes their bodies, which until then fail with
//! `E_INVALID_ARG` naming it.

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_api::{ErrorCode, OpError};
use swarmops_core::ChangesetId;

use crate::Ctx;

/// The most bytes of hook input the parsers read.
pub const MAX_HOOK_INPUT: usize = 64 * 1024;

/// The `WorktreeCreate` hook's input: the fields a workspace is made from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookCreate {
    /// The slug Claude Code suggests.
    pub name: String,
    /// The session that asks.
    pub session_id: String,
    /// Where the session stands.
    pub cwd: Utf8PathBuf,
}

/// The `WorktreeRemove` hook's input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookRemove {
    /// The directory to remove: the path the create hook printed.
    pub worktree_path: Utf8PathBuf,
}

/// Whether a new workspace's bootstrap runs: `SWARMOPS_WS_BOOTSTRAP=skip`, as the CLI read it,
/// is [`Bootstrap::Skip`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Bootstrap {
    /// Copy the `.worktreeinclude` files and run the repository's bootstrap command.
    Run,
    /// Neither.
    Skip,
}

/// What the remove hook left in the store: the workspace's head, and the rescue changeset of its
/// uncommitted edits, when it had some.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RemoveReport {
    /// The workspace's head when it was removed.
    pub head: ChangesetId,
    /// The rescue changeset, on `head`.
    pub rescued: Option<ChangesetId>,
}

/// The error of each unbuilt entry point, naming it.
fn not_built(what: &str) -> OpError {
    OpError::new(
        ErrorCode::InvalidArg,
        format!("{what} is not built yet (the M5 plan's task W9)"),
    )
}

/// The `WorktreeCreate` hook's input, from its stdin bytes.
pub fn read_create(_stdin: &[u8]) -> Result<HookCreate, OpError> {
    Err(not_built("the worktree-create hook's input"))
}

/// The `WorktreeRemove` hook's input, from its stdin bytes.
pub fn read_remove(_stdin: &[u8]) -> Result<HookRemove, OpError> {
    Err(not_built("the worktree-remove hook's input"))
}

/// Makes the session's workspace under `ws_root` and runs its bootstrap as `bootstrap` says:
/// the absolute path the CLI prints.
pub fn worktree_create(
    _ctx: &Ctx<'_>,
    _input: &HookCreate,
    _ws_root: &Utf8Path,
    _bootstrap: Bootstrap,
) -> Result<Utf8PathBuf, OpError> {
    Err(not_built("the worktree-create hook"))
}

/// Keeps the workspace's uncommitted edits as a rescue changeset, then deletes it.
pub fn worktree_remove(_ctx: &Ctx<'_>, _input: &HookRemove) -> Result<RemoveReport, OpError> {
    Err(not_built("the worktree-remove hook"))
}

/// The JSON of the two hook entries, for a Claude Code settings file; empty until the M5 plan's
/// task W9 writes it.
pub fn config() -> String {
    String::new()
}
