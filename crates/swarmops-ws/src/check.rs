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

use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;
use serde_json::{Map, Value};
use swarmops_api::{ContentEncoding, ErrorCode, OpError, ReadFileReq, StoreOps};

use crate::run::{self, Run};
use crate::state::SWARMWS_DIR;
use crate::workspace::Workspace;

/// The repository's workspace settings, read from the store: `check.argv` is the default check.
pub const CONFIG_PATH: &str = ".swarmops/workspaces.json";

/// The directory of the checks' logs, in [`SWARMWS_DIR`]: `<attempt>.log` each.
pub const CHECKS_DIR: &str = "checks";

/// The most bytes of [`CONFIG_PATH`] read.
const CONFIG_MAX_BYTES: u64 = 64 * 1024;

/// The check's name, as its environment gives it: the workspace's name.
pub const ENV_WS: &str = "SWARMOPS_WS";

/// The root tree id of the changeset the check runs on.
pub const ENV_TREE: &str = "SWARMOPS_CHECK_TREE";

/// The attempt the check runs in, from 1.
pub const ENV_ATTEMPT: &str = "SWARMOPS_CHECK_ATTEMPT";

/// The part of [`CONFIG_PATH`] the check reads; other keys are left to their readers.
#[derive(Deserialize, Default)]
struct Config {
    /// The check's settings.
    #[serde(default)]
    check: Option<CheckConfig>,
}

/// `check` of [`CONFIG_PATH`].
#[derive(Deserialize, Default)]
struct CheckConfig {
    /// The command and its arguments.
    #[serde(default)]
    argv: Option<Vec<String>>,
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

/// `check.argv` of [`CONFIG_PATH`] at changeset `rev`; none when the file, its `check` or its
/// `argv` is missing or the argv is empty. `E_INVALID_ARG` for a file that does not parse.
pub(crate) fn configured(store: &dyn StoreOps, rev: &str) -> Result<Option<Vec<String>>, OpError> {
    // 1. The file, if the tree has one.
    let req = ReadFileReq {
        rev: rev.into(),
        path: CONFIG_PATH.to_owned(),
        offset: 0,
        lines: None,
        max_bytes: CONFIG_MAX_BYTES,
    };
    let file = match store.read_file(req) {
        Ok(file) => file,
        Err(e) if e.code == ErrorCode::PathNotFound.as_str() => return Ok(None),
        Err(e) => return Err(e),
    };

    // 2. Its bytes, parsed for `check.argv`.
    let bytes = match file.encoding {
        ContentEncoding::Utf8 => file.content.into_bytes(),
        ContentEncoding::Base64 => STANDARD.decode(&file.content).unwrap_or_default(),
    };
    let config: Config = serde_json::from_slice(&bytes).map_err(|e| {
        OpError::new(
            ErrorCode::InvalidArg,
            format!("{CONFIG_PATH} does not parse: {e}"),
        )
    })?;
    let argv = config.check.and_then(|check| check.argv);
    Ok(argv.filter(|argv| !argv.is_empty()))
}

/// `E_INVALID_ARG{details.missing: "check"}`: a checked land with no command to run.
pub(crate) fn missing() -> OpError {
    OpError::new(
        ErrorCode::InvalidArg,
        format!("--check names no command, and the repository's {CONFIG_PATH} has no check.argv"),
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
