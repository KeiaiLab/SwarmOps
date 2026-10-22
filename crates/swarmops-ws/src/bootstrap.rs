//! The bootstrap of a new workspace (§7.3 of the M5 design): the files `.worktreeinclude` lists,
//! copied from the session's directory, then the repository's own bootstrap command
//! (`.swarmops/workspaces.json`, read from the store), run in the workspace.
//!
//! Empty until the M5 plan's task W9, which writes `bootstrap::run` here.
