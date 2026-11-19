//! The environment a check gets (M31-2): the pinned `SWARMOPS_CHECK_*` variables, a short fixed
//! list of the caller's variables, and only those more the caller names with `--pass`.
//!
//! ```text
//!   pinned:   SWARMOPS_CHECK_NAME, _STAGE, _TREE, _ATTEMPT, _RUN, _REPORT, _OUTPUTS, _SUMMARY,
//!             _ARTIFACTS, _CHANGED; SWARMOPS_WS in a workspace
//!   caller's: HOME, LANG, LC_ALL, LC_CTYPE, LOGNAME, PATH, TMPDIR, TZ, USER, when set,
//!             and each --pass NAME, when set
//!   nothing else: the child's environment is cleared first
//! ```
//!
//! The one check `default` of `.swarmops/workspaces.json` keeps what `ws land --check` gives it:
//! the caller's whole environment, the pinned variables set over it.

use std::ffi::OsString;

use serde_json::Value;
use swarmops_api::{ErrorCode, OpError};

use crate::error;

/// The check's name.
pub const ENV_NAME: &str = "SWARMOPS_CHECK_NAME";

/// The stage run.
pub const ENV_STAGE: &str = "SWARMOPS_CHECK_STAGE";

/// The tree's 64 hex digits in a workspace; empty in the directory form, which knows no tree.
pub const ENV_TREE: &str = "SWARMOPS_CHECK_TREE";

/// The attempt, from 1.
pub const ENV_ATTEMPT: &str = "SWARMOPS_CHECK_ATTEMPT";

/// The run's id.
pub const ENV_RUN: &str = "SWARMOPS_CHECK_RUN";

/// The path of the check's report file, not made: JSON lines of findings, read once the check
/// has ended (M31-4, [`super::findings`]).
pub const ENV_REPORT: &str = "SWARMOPS_CHECK_REPORT";

/// The report file's name, in the check's own directory of its run.
pub const REPORT_FILE: &str = "report.jsonl";

/// The path of the check's outputs file, not made: M31-6's.
pub const ENV_OUTPUTS: &str = "SWARMOPS_CHECK_OUTPUTS";

/// The path of the check's summary, not made: M31-18's.
pub const ENV_SUMMARY: &str = "SWARMOPS_CHECK_SUMMARY";

/// The check's artifacts directory, made empty: M36-3's.
pub const ENV_ARTIFACTS: &str = "SWARMOPS_CHECK_ARTIFACTS";

/// The path of a file of the changed paths, one a line: empty until plans exist (M30-4), and a
/// check then takes the whole tree as changed.
pub const ENV_CHANGED: &str = "SWARMOPS_CHECK_CHANGED";

/// The workspace's name, in the workspace form, as `ws land --check` gives it.
pub const ENV_WS: &str = "SWARMOPS_WS";

/// The caller's variables every check gets, when the caller has them set.
pub const INHERITED: [&str; 9] = [
    "HOME", "LANG", "LC_ALL", "LC_CTYPE", "LOGNAME", "PATH", "TMPDIR", "TZ", "USER",
];

/// The most `--pass` names.
pub const MAX_PASS: usize = 64;

/// The most bytes of a `--pass` name.
pub const MAX_PASS_NAME_BYTES: usize = 128;

/// The prefix of the pinned names, which no `--pass` name takes.
const PINNED_PREFIX: &str = "SWARMOPS_CHECK_";

/// The values of the pinned variables of one check, in the order of the table above.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Pinned {
    /// Each pinned name and its value.
    pub vars: Vec<(&'static str, String)>,
}

impl Pinned {
    /// The pinned variables of check `name` of run `run` at `stage`, on `tree` (none in the
    /// directory form), its own directory `own` (absolute), in workspace `workspace` if any.
    pub fn new(
        name: &str,
        stage: &str,
        tree: Option<&str>,
        run: &str,
        own: &camino::Utf8Path,
        workspace: Option<&str>,
    ) -> Pinned {
        let mut vars = vec![
            (ENV_NAME, name.to_owned()),
            (ENV_STAGE, stage.to_owned()),
            (ENV_TREE, tree.unwrap_or_default().to_owned()),
            (ENV_ATTEMPT, "1".to_owned()),
            (ENV_RUN, run.to_owned()),
            (ENV_REPORT, own.join(REPORT_FILE).into_string()),
            (ENV_OUTPUTS, own.join("outputs").into_string()),
            (ENV_SUMMARY, own.join("summary.md").into_string()),
            (ENV_ARTIFACTS, format!("{}/", own.join("artifacts"))),
            (ENV_CHANGED, String::new()),
        ];
        if let Some(workspace) = workspace {
            vars.push((ENV_WS, workspace.to_owned()));
        }
        Pinned { vars }
    }

    /// The pinned variables as `(name, value)` pairs of borrowed text.
    pub fn pairs(&self) -> Vec<(&str, &str)> {
        self.vars
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
            .collect()
    }
}

/// Whether `name` may be passed with `--pass`: 1 to [`MAX_PASS_NAME_BYTES`] bytes of
/// `[A-Za-z_][A-Za-z0-9_]*`, not a pinned name. `E_INVALID_ARG` with `details.reason` `pass`
/// and `details.pass` otherwise, and for more than [`MAX_PASS`] names.
pub fn check_pass(names: &[String]) -> Result<(), OpError> {
    let refused = |what: String, name: Option<&str>| {
        let mut error = error::error(ErrorCode::InvalidArg, error::one_line(what)).with_hint(
            "--pass names a variable of your environment the checks also get: a name of \
             letters, digits and _, not a SWARMOPS_CHECK_ name or SWARMOPS_WS",
        );
        error.details.insert("reason".to_owned(), "pass".into());
        if let Some(name) = name {
            error.details.insert("pass".to_owned(), Value::from(name));
        }
        error
    };
    if names.len() > MAX_PASS {
        return Err(refused(
            format!(
                "--pass is given {} times, more than {MAX_PASS}",
                names.len()
            ),
            None,
        ));
    }
    for name in names {
        let bytes = name.as_bytes();
        let shape = !bytes.is_empty()
            && bytes.len() <= MAX_PASS_NAME_BYTES
            && (bytes[0].is_ascii_alphabetic() || bytes[0] == b'_')
            && bytes
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || *b == b'_');
        if !shape {
            return Err(refused(
                format!("--pass {name:?} is not a variable's name"),
                Some(name),
            ));
        }
        if name.starts_with(PINNED_PREFIX) || name == ENV_WS {
            return Err(refused(
                format!("--pass {name} names a variable SwarmOps sets for each check"),
                Some(name),
            ));
        }
    }
    Ok(())
}

/// What a check gets of the caller's environment, which `lookup` reads: the fixed list and the
/// `pass` names the caller has set, sorted, each with its value, none twice.
pub fn inherited(
    pass: &[String],
    lookup: &dyn Fn(&str) -> Option<OsString>,
) -> Vec<(String, OsString)> {
    let mut names: Vec<&str> = INHERITED.to_vec();
    names.extend(pass.iter().map(String::as_str));
    names.sort_unstable();
    names.dedup();
    (names.into_iter())
        .filter_map(|name| lookup(name).map(|value| (name.to_owned(), value)))
        .collect()
}

/// The environment of a check of `.swarmops/checks.toml`: what it `inherited`, then the pinned
/// variables, which no inherited one overrides (a pinned name is never taken from the caller).
pub fn only(inherited: &[(String, OsString)], pinned: &Pinned) -> Vec<(String, OsString)> {
    let mut vars: Vec<(String, OsString)> = (inherited.iter())
        .filter(|(name, _)| !pinned.vars.iter().any(|(pin, _)| pin == name))
        .cloned()
        .collect();
    vars.extend(
        (pinned.vars.iter()).map(|(name, value)| ((*name).to_owned(), OsString::from(value))),
    );
    vars
}
