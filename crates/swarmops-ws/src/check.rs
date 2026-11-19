//! The check of `ws land --check` (§5.4 of the M5 design): the command run in the rebased
//! workspace through the process runner, its failure or timeout mapped to `E_CHECK_FAILED`.
//!
//! ```text
//!  argv: the request's, else `check.argv` of the repository's .swarmops/workspaces.json,
//!        read from the store at the rebased changeset (never from the host's tree)
//!  run::run(argv, workspace root, env + SWARMOPS_WS, SWARMOPS_CHECK_TREE, SWARMOPS_CHECK_ATTEMPT,
//!           timeout, .swarmws/checks/<attempt>.log)
//!  exit 0: passed; else E_CHECK_FAILED{stage: "check", exit_code?, timed_out, stdout_tail,
//!                                     stderr_tail, log, changeset}
//! ```
//!
//! [`hold_for_run`] is what `check run`'s workspace form starts from: the workspace under its
//! lock, settled (not rebasing, not stale) and clean, so that its files are the tree of the
//! changeset they stand for while its checks run. [`edits`] is what `check plan`'s starts from:
//! the same workspace, settled, read without its lock, with the paths of its edits.

use std::time::Duration;

use camino::{Utf8Path, Utf8PathBuf};
use serde_json::{Map, Value};
use swarmops_api::{ErrorCode, OpError, ShowReq, StoreOps, VerdictState};

use crate::Ctx;
use crate::bootstrap::{self, REPO_CONFIG};
use crate::lock::WsLock;
use crate::materialize;
use crate::run::{self, Run};
use crate::scan::{self, Keep, Want};
use crate::state::{SWARMWS_DIR, State};
use crate::status;
use crate::workspace::{self, Locate, Workspace};

/// The directory of the checks' logs, in [`SWARMWS_DIR`]: `<attempt>.log` each.
pub const CHECKS_DIR: &str = "checks";

/// The check's name, as its environment gives it: the workspace's name.
pub const ENV_WS: &str = "SWARMOPS_WS";

/// The root tree id of the changeset the check runs on.
pub const ENV_TREE: &str = "SWARMOPS_CHECK_TREE";

/// The attempt the check runs in, from 1.
pub const ENV_ATTEMPT: &str = "SWARMOPS_CHECK_ATTEMPT";

/// A workspace held for a run of its checks: its lock, and what its files stand for. The lock is
/// held until this is dropped.
#[derive(Debug)]
#[non_exhaustive]
pub struct Held {
    /// The workspace's root directory.
    pub dir: Utf8PathBuf,
    /// Its name.
    pub name: String,
    /// The changeset its files stand for.
    pub changeset: String,
    /// That changeset's root tree: what the files are.
    pub tree: String,
    /// The workspace's lock.
    _lock: WsLock,
}

/// What a workspace holds beyond its head, read for a plan of its checks: the ends of its
/// committed chain and the paths of its edits.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Edits {
    /// The workspace's root directory.
    pub dir: Utf8PathBuf,
    /// Its name.
    pub name: String,
    /// The ref it lands on.
    pub target: String,
    /// The target's changeset its chain starts from.
    pub base: String,
    /// Its chain's last changeset: what its edits are against.
    pub head: String,
    /// The paths of its edits against its head, in path order: at most the limit asked for.
    pub paths: Vec<String>,
    /// Whether it holds more edits than `paths` lists.
    pub truncated: bool,
    /// Whether a sparse workspace holds entries off its prefixes: edits that are in no list.
    pub outside: bool,
}

/// The edits of the workspace `at` names, at most `limit` of their paths: read without its
/// lock, as `ws status` reads it, and its scan index kept. Refused as [`hold_for_run`] refuses
/// a workspace, with the same codes, when it is rebasing, stale, or its files stand for
/// another changeset than its head. Edits are what this is for: they are not refused. Nothing
/// is written to the store.
pub fn edits(ctx: &Ctx<'_>, at: &Locate, limit: usize) -> Result<Edits, OpError> {
    // 1. The workspace, without its lock: a read. Settled, as a run of its checks needs it.
    let ws = Workspace::open(ctx, at.clone())?;
    settled(ctx.store, &ws)?;

    // 2. Its edits against its head, the index kept for the next scan; and whether anything
    //    stands off its prefixes.
    let mut scanned = scan::scan(ctx, &ws, &Want::default())?;
    let outside = !std::mem::take(&mut scanned.outside).is_empty();
    let changes = scanned.keep(&ws.dir, &scan::binding_of(&ws), Keep::IfChanged);
    let truncated = changes.len() > limit;
    let paths = (changes.into_iter().take(limit))
        .map(|change| change.path)
        .collect();
    Ok(Edits {
        dir: ws.dir,
        name: ws.file.name,
        target: ws.file.target,
        base: ws.file.base,
        head: ws.file.head,
        paths,
        truncated,
        outside,
    })
}

/// Refuses workspace `ws` unless it is settled: not rebasing (`E_UNRESOLVED_CONFLICTS`), its
/// base still on its target's history (`E_WS_STALE`), its files standing for its head
/// (`E_INVALID_ARG`, `details.reason: materialized`). What a run of its checks and a plan of
/// them both need first.
fn settled(store: &dyn StoreOps, ws: &Workspace) -> Result<(), OpError> {
    if let State::Rebasing { .. } = ws.file.state {
        return Err(OpError::new(
            ErrorCode::UnresolvedConflicts,
            "the workspace is rebasing: finish the rebase before running its checks",
        )
        .with_hint("swarmops ws rebase --continue, or swarmops ws rebase --abort"));
    }
    status::refuse_stale(store, &ws.file)?;
    if ws.file.materialized != ws.file.head {
        let details = Map::from_iter([
            ("reason".to_owned(), Value::from("materialized")),
            (
                "materialized".to_owned(),
                Value::from(ws.file.materialized.as_str()),
            ),
            ("head".to_owned(), Value::from(ws.file.head.as_str())),
        ]);
        return Err(OpError::new(
            ErrorCode::InvalidArg,
            "the workspace's files stand for another changeset than its head: nothing to check",
        )
        .with_details(details)
        .with_hint(format!(
            "an operation on the workspace was cut short: swarmops ws status --ws {} shows \
             where it stands; finish that operation, then run the checks again",
            ws.file.name
        )));
    }
    Ok(())
}

/// The workspace `at` names, held for a run of its checks: its lock taken (`E_WS_LOCKED` after
/// the wait), not rebasing (`E_UNRESOLVED_CONFLICTS`), its base still on its target's history
/// (`E_WS_STALE`), its files standing for its head and holding no edit (`E_WS_DIRTY`, naming the
/// first 20). Then its files are the tree of `changeset`, and stay so while the lock is held: no
/// `ws` mutation moves them. Nothing is written, to the store or the workspace.
pub fn hold_for_run(ctx: &Ctx<'_>, at: &Locate) -> Result<Held, OpError> {
    // 1. Under the lock, settled.
    let (ws, lock) = workspace::open_locked(ctx, at)?;
    settled(ctx.store, &ws)?;

    // 2. Clean: no edit on disk.
    let changes = status::changes_of(ctx, &ws, &Want::default())?;
    if !changes.is_empty() {
        let paths = changes.into_iter().map(|change| change.path).collect();
        let hint = format!(
            "swarmops ws commit -m <message> to check the edits as a changeset, or \
             swarmops check run --dir {} to check the files as they are, vouched for",
            ws.dir
        );
        // `dirty`'s code, details and cut, with this command's words: a run overwrites nothing.
        let mut error = materialize::dirty(paths).with_hint(hint);
        error.message = format!(
            "the workspace holds edits its checks would not run on: {}",
            error.details["paths"]
        );
        return Err(error);
    }

    // 3. The tree the files are.
    let mut show = ShowReq::new(ws.file.materialized.as_str());
    show.max_changes = 1;
    let tree = ctx.store.show(show)?.root;
    Ok(Held {
        dir: ws.dir,
        name: ws.file.name,
        changeset: ws.file.materialized,
        tree,
        _lock: lock,
    })
}

/// What a check runs on: the rebased changeset and its tree, in an attempt.
pub(crate) struct Subject<'a> {
    /// The changeset: the workspace's head, rebased.
    pub(crate) changeset: &'a str,
    /// Its root tree's id.
    pub(crate) tree: &'a str,
    /// The attempt, from 1.
    pub(crate) attempt: u32,
}

/// `check.argv` of the repository's [`REPO_CONFIG`] at changeset `rev`; none when the file, its
/// `check` or its argv is missing. `E_INVALID_ARG` for a file that does not parse.
pub(crate) fn configured(store: &dyn StoreOps, rev: &str) -> Result<Option<Vec<String>>, OpError> {
    let config = bootstrap::repo_config(store, rev)?;
    let argv = config
        .and_then(|config| config.check)
        .map(|check| check.argv);
    Ok(argv.filter(|argv| !argv.is_empty()))
}

/// `E_INVALID_ARG{details.missing: "check"}`: a checked land with no command to run.
pub(crate) fn missing() -> OpError {
    OpError::new(
        ErrorCode::InvalidArg,
        format!("--check names no command, and the repository's {REPO_CONFIG} has no check.argv"),
    )
    .with_details(Map::from_iter([(
        "missing".to_owned(),
        Value::from("check"),
    )]))
    .with_hint("swarmops ws land --check -- <command> [args…]")
}

/// Runs `argv` in workspace `ws` on `on`, for at most `timeout`: `Ok` when it exits 0, else
/// `E_CHECK_FAILED` with what it did.
pub(crate) fn run(
    ws: &Workspace,
    argv: &[String],
    on: &Subject<'_>,
    timeout: Duration,
) -> Result<(), OpError> {
    // 1. Its environment and its log.
    let attempt = on.attempt.to_string();
    let env = [
        (ENV_WS, ws.file.name.as_str()),
        (ENV_TREE, on.tree),
        (ENV_ATTEMPT, attempt.as_str()),
    ];
    let log = format!("{SWARMWS_DIR}/{CHECKS_DIR}/{attempt}.log");

    // 2. Run in the workspace's root; exit 0 passes.
    let done = run::run(argv, &ws.dir, &env, timeout, &ws.dir.join(&log))?;
    if done.exit_code == Some(0) && !done.timed_out {
        return Ok(());
    }
    Err(failed(&done, &log, on.changeset))
}

/// A queue runner's check (the M22 core design §5 step 6): `argv` run in the workspace at
/// `dir`, named `name`, on candidate tree `tree`, for at most `timeout`; its verdict and the
/// tail of its output. `started` is told the check's pid, its process group's id, once it runs.
///
/// ```text
///  exit 0                         passed
///  another exit, or no start      failed
///  killed by the runner's timeout timed_out
///  killed by another signal       cancelled   (no one is blamed)
/// ```
pub(crate) fn verdict(
    argv: &[String],
    dir: &Utf8Path,
    name: &str,
    tree: &str,
    timeout: Duration,
    started: impl FnOnce(u32),
) -> Result<(VerdictState, String), OpError> {
    // 1. Its environment and its log, as a checked land's first attempt.
    let env = [(ENV_WS, name), (ENV_TREE, tree), (ENV_ATTEMPT, "1")];
    let log = dir.join(SWARMWS_DIR).join(CHECKS_DIR).join("1.log");

    // 2. Run; a command that cannot start fails the candidate, a host failure is the caller's.
    let done = match run::run_started(argv, dir, &env, timeout, &log, started) {
        Ok(done) => done,
        Err(e) if e.code == ErrorCode::InvalidArg.as_str() => {
            return Ok((VerdictState::Failed, e.message));
        }
        Err(e) => return Err(e),
    };

    // 3. The verdict: a signal the runner did not send blames no one.
    let state = match (done.timed_out, done.exit_code) {
        (true, _) => VerdictState::TimedOut,
        (false, Some(0)) => VerdictState::Passed,
        (false, Some(_)) => VerdictState::Failed,
        (false, None) => VerdictState::Cancelled,
    };
    Ok((state, format!("{}{}", done.stdout_tail, done.stderr_tail)))
}

/// `E_CHECK_FAILED` for the check that did `done`, its log at `log`, on `changeset`.
fn failed(done: &Run, log: &str, changeset: &str) -> OpError {
    // 1. Why, in one line.
    let why = match (done.timed_out, done.exit_code) {
        (true, _) => "ran past its timeout".to_owned(),
        (false, Some(code)) => format!("exited with {code}"),
        (false, None) => "was killed by a signal".to_owned(),
    };

    // 2. What it did, in the details.
    let mut details = Map::from_iter([
        ("stage".to_owned(), Value::from("check")),
        ("timed_out".to_owned(), Value::from(done.timed_out)),
        (
            "stdout_tail".to_owned(),
            Value::from(done.stdout_tail.as_str()),
        ),
        (
            "stderr_tail".to_owned(),
            Value::from(done.stderr_tail.as_str()),
        ),
        ("log".to_owned(), Value::from(log)),
        ("changeset".to_owned(), Value::from(changeset)),
    ]);
    if let Some(code) = done.exit_code {
        details.insert("exit_code".to_owned(), Value::from(code));
    }
    OpError::new(
        ErrorCode::CheckFailed,
        format!("the check {why}: nothing landed, the workspace stands on the rebased changeset"),
    )
    .with_details(details)
    .with_hint(format!(
        "see {log}; fix, ws commit, then ws land --check again"
    ))
}
