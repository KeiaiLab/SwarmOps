//! The bootstrap of a new workspace (§7.3 of the M5 design): the files `.worktreeinclude` lists,
//! copied from the session's directory, then the repository's own bootstrap command
//! (`.swarmops/workspaces.json`, read from the store), run in the workspace.
//!
//! ```text
//! <from>/.worktreeinclude (gitignore syntax)
//!   └─ walk <from>, no link followed, .git and .swarmws skipped
//!        └─ a regular file it matches, the tree does not hold, the workspace lacks
//!             └─ copied, at most 10,000 files and 1 GiB
//! .swarmops/workspaces.json at the base ── bootstrap.argv ──▶ run in the workspace
//!                                                             SWARMOPS_WS=<name>
//! ```
//!
//! `create` runs neither: its callers (the hook, the CLI's `ws create`, the `ws_create` tool)
//! call [`run`] after a create that made the workspace, unless told to skip it.

use std::io;
use std::time::Duration;

use camino::{Utf8Path, Utf8PathBuf};
use ignore::WalkBuilder;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use serde::Deserialize;
use serde_json::{Map, Value};
use swarmops_api::{ContentEncoding, ErrorCode, OpError, ReadFileReq, StoreOps};

use crate::hostfs::{HostFs, HostKind};
use crate::materialize::{self, Tree};
use crate::state::SWARMWS_DIR;
use crate::workspace::Workspace;
use crate::{Ctx, run as runner};

/// The file, at the root of the source directory, listing what to copy.
pub const WORKTREEINCLUDE: &str = ".worktreeinclude";

/// The repository's workspace settings: a tracked file, read from the store.
pub const REPO_CONFIG: &str = ".swarmops/workspaces.json";

/// The most files the copies make.
pub const MAX_COPY_FILES: u64 = 10_000;

/// The most bytes the copies write: 1 GiB.
pub const MAX_COPY_BYTES: u64 = 1 << 30;

/// How long the bootstrap command may run, unless the repository says otherwise.
pub const BOOTSTRAP_TIMEOUT_S: u64 = 600;

/// The most bytes of [`REPO_CONFIG`] read.
const CONFIG_MAX_BYTES: u64 = 64 * 1024;

/// The bootstrap command's log, in [`SWARMWS_DIR`].
const BOOTSTRAP_LOG: &str = "checks/bootstrap.log";

/// The repository's workspace settings, [`REPO_CONFIG`].
#[derive(Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RepoConfig {
    /// The prefixes a new workspace holds; the whole tree when empty.
    #[serde(default)]
    pub(crate) prefixes: Vec<String>,
    /// The command a new workspace runs.
    pub(crate) bootstrap: Option<RepoCommand>,
    /// The command `ws land --check` runs.
    pub(crate) check: Option<RepoCommand>,
}

/// A command of [`RepoConfig`].
#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepoCommand {
    /// The program and its arguments, run without a shell.
    pub(crate) argv: Vec<String>,
    /// How long it may run, in seconds.
    pub(crate) timeout_s: Option<u64>,
}

/// One file to copy.
struct Planned {
    /// Its path below both directories.
    rel: String,
    /// Its permission bits in the source.
    mode: u32,
}

/// Bootstraps workspace `ws`: the files `from`'s `.worktreeinclude` lists copied in (nothing
/// without `from`), then the repository's bootstrap command run there. A command that fails or
/// runs past its timeout is `E_CHECK_FAILED` (`details.stage: "bootstrap"`), the workspace kept;
/// copies past the limits are `E_TOO_LARGE`, nothing copied.
pub fn run(ctx: &Ctx<'_>, ws: &Workspace, from: Option<&Utf8Path>) -> Result<(), OpError> {
    // 1. The copies.
    if let Some(from) = from {
        copy_included(ctx, ws, from)?;
    }

    // 2. The repository's command, read at the workspace's base.
    let config = repo_config(ctx.store, &ws.file.base)?.unwrap_or_default();
    let Some(command) = config.bootstrap.filter(|c| !c.argv.is_empty()) else {
        return Ok(());
    };
    let timeout = Duration::from_secs(command.timeout_s.unwrap_or(BOOTSTRAP_TIMEOUT_S));

    // 3. Run in the workspace, told its name.
    let log = ws.dir.join(SWARMWS_DIR).join(BOOTSTRAP_LOG);
    let env = [("SWARMOPS_WS", ws.file.name.as_str())];
    let done = runner::run(&command.argv, &ws.dir, &env, timeout, &log)?;
    if done.exit_code == Some(0) && !done.timed_out {
        return Ok(());
    }
    Err(failed(ws, &done))
}

/// The repository's workspace settings at `rev`; none when it holds no [`REPO_CONFIG`].
pub(crate) fn repo_config(store: &dyn StoreOps, rev: &str) -> Result<Option<RepoConfig>, OpError> {
    // 1. The file, read from the store, never from a host tree.
    let mut req = ReadFileReq::new(rev, REPO_CONFIG);
    req.max_bytes = CONFIG_MAX_BYTES;
    let read = match store.read_file(req) {
        Ok(read) => read,
        Err(e) if e.code == ErrorCode::PathNotFound.as_str() => return Ok(None),
        Err(e) => return Err(e),
    };

    // 2. Whole, text, and the settings' shape.
    if read.truncated || read.encoding != ContentEncoding::Utf8 {
        return Err(bad_config(&format!(
            "it is not text of at most {CONFIG_MAX_BYTES} bytes"
        )));
    }
    let config = serde_json::from_str(&read.content).map_err(|e| bad_config(&e.to_string()))?;
    Ok(Some(config))
}

/// Copies into workspace `ws` the regular files below `from` that its `.worktreeinclude` lists,
/// and that neither the workspace's tree nor its directory holds.
fn copy_included(ctx: &Ctx<'_>, ws: &Workspace, from: &Utf8Path) -> Result<(), OpError> {
    // 1. The list; none, nothing to copy.
    let host = ctx.host.as_ref();
    let Some(include) = included(host, from)? else {
        return Ok(());
    };

    // 2. The files it lists, checked against the limits before any is copied.
    let tree = Tree::new(ctx.store, &ws.file.head);
    let mut copies = Vec::new();
    let mut bytes = 0u64;
    for found in listed(from, &include)? {
        let (rel, size, mode) = found;
        let dest = ws.dir.join(&rel);
        let held =
            (host.symlink_metadata(&dest)).map_err(|e| materialize::host_failed(&rel, &e))?;
        if held.is_some() || tree.entry(&rel)?.is_some() {
            continue;
        }
        bytes += size;
        copies.push(Planned { rel, mode });
        if u64::try_from(copies.len()).unwrap_or(u64::MAX) > MAX_COPY_FILES {
            return Err(too_many("files", MAX_COPY_FILES));
        }
        if bytes > MAX_COPY_BYTES {
            return Err(too_many("bytes", MAX_COPY_BYTES));
        }
    }

    // 3. Each copied, its directories made, its permission bits kept.
    for copy in copies {
        let dest = ws.dir.join(&copy.rel);
        if let Some(parent) = dest.parent() {
            (host.create_dir_all(parent)).map_err(|e| materialize::host_failed(&copy.rel, &e))?;
        }
        let content = (host.read_file(&from.join(&copy.rel)))
            .map_err(|e| materialize::host_failed(&copy.rel, &e))?;
        (host.write_file(&dest, &content)).map_err(|e| materialize::host_failed(&copy.rel, &e))?;
        (host.set_mode(&dest, copy.mode)).map_err(|e| materialize::host_failed(&copy.rel, &e))?;
    }
    Ok(())
}

/// The matcher of `from`'s `.worktreeinclude`; none when there is no such regular file.
fn included(host: &dyn HostFs, from: &Utf8Path) -> Result<Option<Gitignore>, OpError> {
    // 1. A regular file, never a link.
    let path = from.join(WORKTREEINCLUDE);
    let meta = (host.symlink_metadata(&path))
        .map_err(|e| materialize::host_failed(WORKTREEINCLUDE, &e))?;
    if meta.is_none_or(|meta| meta.kind != HostKind::File) {
        return Ok(None);
    }

    // 2. Its lines, in gitignore syntax.
    let bytes =
        (host.read_file(&path)).map_err(|e| materialize::host_failed(WORKTREEINCLUDE, &e))?;
    let text = String::from_utf8_lossy(&bytes);
    let mut builder = GitignoreBuilder::new(from);
    for line in text.lines() {
        builder.add_line(None, line).map_err(|e| bad_include(&e))?;
    }
    builder.build().map(Some).map_err(|e| bad_include(&e))
}

/// The regular files below `from` that `include` matches, as `(path, size, mode)`: no link
/// followed or listed, `.git` directories and the root's `.swarmws` skipped, nothing whose
/// resolved path leaves `from`.
fn listed(from: &Utf8Path, include: &Gitignore) -> Result<Vec<(String, u64, u32)>, OpError> {
    // 1. Every entry, no filter of the walker's own, no link followed.
    let root = from.to_path_buf();
    let mut builder = WalkBuilder::new(from);
    builder
        .standard_filters(false)
        .hidden(false)
        .parents(false)
        .follow_links(false);
    builder.filter_entry(move |entry| {
        let is_dir = entry.file_type().is_some_and(|kind| kind.is_dir());
        let at_root = entry.path().parent() == Some(root.as_std_path());
        let git = is_dir && entry.file_name() == ".git";
        let state = at_root && entry.file_name() == SWARMWS_DIR;
        !(git || state)
    });

    // 2. The regular files the list matches, inside `from` once resolved.
    let resolved_root =
        std::fs::canonicalize(from).map_err(|e| materialize::host_failed(".", &e))?;
    let mut found = Vec::new();
    for entry in builder.build() {
        let entry = entry.map_err(|e| walk_failed(&e))?;
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let Some(rel) = relative(from, entry.path()) else {
            continue;
        };
        if !include.matched_path_or_any_parents(&rel, false).is_ignore() {
            continue;
        }
        let resolved =
            std::fs::canonicalize(entry.path()).map_err(|e| materialize::host_failed(&rel, &e))?;
        if !resolved.starts_with(&resolved_root) {
            continue;
        }
        let meta = entry.metadata().map_err(|e| walk_failed(&e))?;
        found.push((rel, meta.len(), mode_of(&meta)));
    }
    Ok(found)
}

/// `path` below `root`, `/`-separated; none when it is not below it or not UTF-8.
fn relative(root: &Utf8Path, path: &std::path::Path) -> Option<String> {
    let rel = Utf8Path::from_path(path.strip_prefix(root).ok()?)?;
    let parts: Vec<&str> = rel.components().map(|c| c.as_str()).collect();
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// The permission bits of a file's metadata `meta`.
#[cfg(unix)]
fn mode_of(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o7777
}

/// No permission bits where the platform has none.
#[cfg(not(unix))]
fn mode_of(_meta: &std::fs::Metadata) -> u32 {
    0
}

/// `E_CHECK_FAILED` of workspace `ws`'s bootstrap command, which ended as `done` says.
fn failed(ws: &Workspace, done: &runner::Run) -> OpError {
    let ended = match (done.timed_out, done.exit_code) {
        (true, _) => "ran past its timeout".to_owned(),
        (false, Some(code)) => format!("exited with {code}"),
        (false, None) => "was killed".to_owned(),
    };
    let log: Utf8PathBuf = Utf8Path::new(SWARMWS_DIR).join(BOOTSTRAP_LOG);
    OpError::new(
        ErrorCode::CheckFailed,
        format!(
            "the bootstrap command of workspace {:?} {ended}: the workspace is kept, its \
             output in {log}",
            ws.file.name
        ),
    )
    .with_details(Map::from_iter([
        ("stage".to_owned(), Value::from("bootstrap")),
        ("exit_code".to_owned(), Value::from(done.exit_code)),
        ("timed_out".to_owned(), Value::from(done.timed_out)),
        (
            "stdout_tail".to_owned(),
            Value::from(done.stdout_tail.as_str()),
        ),
        (
            "stderr_tail".to_owned(),
            Value::from(done.stderr_tail.as_str()),
        ),
        ("log".to_owned(), Value::from(log.as_str())),
        ("changeset".to_owned(), Value::from(ws.file.head.as_str())),
    ]))
}

/// `E_TOO_LARGE` for copies past the limit of `what` (`files` or `bytes`), `max`.
fn too_many(what: &str, max: u64) -> OpError {
    OpError::new(
        ErrorCode::TooLarge,
        format!("{WORKTREEINCLUDE} lists more than {max} {what} to copy: nothing was copied"),
    )
    .with_details(Map::from_iter([
        ("limit".to_owned(), Value::from(what)),
        ("max".to_owned(), Value::from(max)),
    ]))
}

/// `E_INVALID_ARG` for a [`REPO_CONFIG`] that cannot be read, `why`.
fn bad_config(why: &str) -> OpError {
    OpError::new(
        ErrorCode::InvalidArg,
        format!("the repository's {REPO_CONFIG} cannot be read: {why}"),
    )
}

/// `E_INVALID_ARG` for a `.worktreeinclude` line that is no pattern.
fn bad_include(e: &ignore::Error) -> OpError {
    OpError::new(
        ErrorCode::InvalidArg,
        format!("{WORKTREEINCLUDE} holds a line that is no pattern: {e}"),
    )
}

/// `E_IO` for a walk of the source directory that failed with `e`.
fn walk_failed(e: &dyn std::fmt::Display) -> OpError {
    materialize::host_failed(".", &io::Error::other(e.to_string()))
}
