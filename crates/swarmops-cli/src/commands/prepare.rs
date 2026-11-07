//! `serve.git` prepared after every ref move (the M17 design §4.3): once an operation of
//! [`PREPARE_AFTER`] has succeeded on a store holding `access.toml` and its response is out, a
//! detached `swarmops serve --prepare --prepare-wait zero` makes that store's `serve.git` current,
//! so that the next connection finds nothing to refresh.
//!
//! ```text
//! commands::run ── op of PREPARE_AFTER, exit 0 ──▶ Used notes it ──▶ entry: emit, then spawn
//! mcp tools/call ─ the same rule, envelope ok ───▶ Server keeps it ─▶ line flushed, then spawn;
//!                                                                     reaped before each request
//! ```
//!
//! The run is its own process group, its stdin, stdout and stderr null, and nobody waits for it.
//! A spawn that fails is ignored, and so is the run's own failure: the next connection refreshes
//! as before. Neither changes the operation's envelope or exit code.

use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::{Child, Command as Process, Stdio};

use crate::cli::{Command, GitCommand};

/// The operations after which `serve.git` is prepared, and when they move a ref. `fetch` counts
/// on every success: its record names what it resolved, not what moved, and a preparation finds
/// a store that did not move current at once.
pub(crate) const PREPARE_AFTER: [(&str, Moves); 9] = [
    ("land", Moves::Always),
    ("commit", Moves::WithAdvance),
    ("ref_set", Moves::Always),
    ("ref_delete", Moves::Always),
    ("undo", Moves::Always),
    ("sync", Moves::Always),
    ("fetch", Moves::Always),
    ("git_import", Moves::Always),
    ("ws_land", Moves::Always),
];

/// The file in a store's root without which nobody is served (ops' `serve::access`): a store
/// without it gets no preparation.
const ACCESS_FILE: &str = "access.toml";

/// When an operation of [`PREPARE_AFTER`] moves a ref.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Moves {
    /// Whenever it succeeds.
    Always,
    /// Only when it names a ref to advance (`commit --advance`).
    WithAdvance,
}

/// Whether a commit names a ref to advance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Advance {
    /// It does: `--advance REF`, or MCP `advance`.
    Named,
    /// It does not, or the operation is no commit.
    Not,
}

/// A preparation to start once the response is out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct After {
    /// The store, as the user named it: the run starts in this process's directory.
    store: String,
    /// The repository a `git import` read, the run's `--seed-from`.
    seed: Option<String>,
}

impl After {
    /// The preparation of `store`, seeded from `seed`.
    pub(crate) fn new(store: String, seed: Option<String>) -> After {
        After { store, seed }
    }
}

/// Whether `op`, a commit's ref named as `advance` says, is one of [`PREPARE_AFTER`].
pub(crate) fn wanted(op: &str, advance: Advance) -> bool {
    PREPARE_AFTER
        .iter()
        .any(|&(name, moves)| name == op && (moves == Moves::Always || advance == Advance::Named))
}

/// Whether `command` is a commit that names a ref to advance.
pub(crate) fn advance_of(command: &Command) -> Advance {
    match command {
        Command::Commit {
            advance: Some(_), ..
        } => Advance::Named,
        _ => Advance::Not,
    }
}

/// The repository `command` imports from, when it is a `git import`.
pub(crate) fn seed_of(command: &Command) -> Option<String> {
    match command {
        Command::Git(GitCommand::Import { path, .. }) => Some(path.clone()),
        _ => None,
    }
}

/// Starts the preparation `after` names, detached; the child, which nobody needs to wait for.
/// `None` when the store holds no `access.toml`, or the spawn failed.
pub(crate) fn spawn(after: &After) -> Option<Child> {
    // 1. Only a served store.
    if !Path::new(&after.store).join(ACCESS_FILE).is_file() {
        return None;
    }

    // 2. This program, as `serve --prepare` that gives up at once when serve.lock is held.
    let program = std::env::current_exe().ok()?;
    let mut prepare = Process::new(program);
    prepare
        .args(["--repo", &after.store])
        .args(["serve", "--prepare", "--prepare-wait", "zero"]);
    if let Some(seed) = &after.seed {
        prepare.args(["--seed-from", seed]);
    }

    // 3. Detached: a process group of its own, no stdio a reader could wait on.
    prepare
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The direction design's list, with `git import` and `ws land` (the M17 design §10, D3);
    /// a commit only with `--advance`, and no read.
    #[test]
    fn the_ref_moves_are_prepared_after() {
        for op in [
            "land",
            "ref_set",
            "ref_delete",
            "undo",
            "sync",
            "fetch",
            "git_import",
            "ws_land",
        ] {
            assert!(wanted(op, Advance::Not), "{op}");
        }
        assert!(wanted("commit", Advance::Named));
        assert!(!wanted("commit", Advance::Not));
        for op in ["sync_status", "push", "show", "ws_commit", "serve_prepare"] {
            assert!(!wanted(op, Advance::Named), "{op}");
        }
    }
}
