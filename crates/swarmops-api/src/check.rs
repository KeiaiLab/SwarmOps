//! A tree's checks (M30): what `check list` answers, from the check file of a changeset's tree,
//! `.swarmops/checks.toml`, or, in a tree without one, `check.argv` of
//! `.swarmops/workspaces.json` as the one check `default`.
//!
//! ```text
//!   CheckListReq {rev, stage?} ──▶ the tree rev names ──▶ .swarmops/checks.toml (version 1)
//!                                                         or workspaces.json's check.argv
//!   CheckListResp {vouched, tree, file, version, stage, checks: [CheckOut sorted by name]}
//! ```
//!
//! `check run` runs one stage's checks, one at a time, and answers one [`CheckRunResp`]: an
//! outcome per check ([`CheckRunOut`]), each check's log and, for a check that did not pass, the
//! tails of its output.
//!
//! ```text
//!   CheckRunReq {workspace, stage, checks} ──▶ the workspace held clean and locked
//!   CheckRunResp {vouched, run, stage, workspace?, changeset?, tree?, file, version, dir,
//!                 inherited_env, inherited_all, cancelled, failed, duration_ms,
//!                 checks: [CheckRunOut {name, outcome, …} in run order]}
//! ```
//!
//! `check log` reads one check's log of a run ([`CheckLogResp`]): its lines, each with its time
//! and stream, and whether it has ended.
//!
//! No request carries a path of the caller's host: a directory the caller names is read and
//! run by the CLI alone, and its answer is the same type with `vouched` set. A result names the
//! host's paths of its run directory and logs: a run happens on one host, and its logs stay
//! there.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::rev::Rev;

/// `check_list` (CLI `check list`): the checks the tree of `rev` describes. Reads only: no lock,
/// no identity, nothing written.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CheckListReq {
    /// The changeset whose tree's checks to list; `main` when omitted.
    #[serde(default = "default_rev")]
    pub rev: Rev,
    /// Only the checks of this stage; every check when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage: Option<CheckStage>,
}

impl Default for CheckListReq {
    /// The request a JSON object without fields reads: `main`'s checks, every stage.
    fn default() -> Self {
        Self {
            rev: default_rev(),
            stage: None,
        }
    }
}

impl CheckListReq {
    /// The checks of `rev`'s tree, every stage.
    pub fn new(rev: impl Into<Rev>) -> Self {
        Self {
            rev: rev.into(),
            stage: None,
        }
    }
}

/// `check_list`: the checks a tree describes, sorted by name. The same tree gives the same
/// answer byte for byte, whichever changeset names it.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CheckListResp {
    /// Whether only the caller stands behind the file: `true` when it was read from a directory
    /// the caller named (`check list --dir`), which no gate accepts; `false` when it was read
    /// from the store, at `tree`.
    pub vouched: bool,
    /// The root tree the file was read from: 64 hex digits. Omitted for a directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree: Option<String>,
    /// The file the checks came from: `.swarmops/checks.toml`, or `.swarmops/workspaces.json`
    /// for the one check `default`. Omitted when the tree holds neither, or a `workspaces.json`
    /// with no `check.argv`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// The check file's `version`, 1. Omitted unless `file` is `.swarmops/checks.toml`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    /// The stage asked for: `checks` holds only the checks of it. Omitted when every check is
    /// listed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage: Option<CheckStage>,
    /// The checks, sorted by name, bytewise; empty when the tree describes none (or none of
    /// `stage`).
    pub checks: Vec<CheckOut>,
}

impl CheckListResp {
    /// `checks`, vouched for by the caller or not, with no tree, file or version.
    pub fn new(vouched: bool, checks: Vec<CheckOut>) -> Self {
        Self {
            vouched,
            tree: None,
            file: None,
            version: None,
            stage: None,
            checks,
        }
    }
}

/// One check: a command run with no shell, in a directory of the tree, for at most its time.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CheckOut {
    /// Its name: 1 to 64 bytes of `a-z`, `0-9`, `-` and `_`, starting with a letter or digit.
    pub name: String,
    /// The program and its arguments, run with no shell: at least the program.
    pub argv: Vec<String>,
    /// The directory of the tree it runs in, as a repository path; `.` is the root.
    pub dir: String,
    /// The most seconds it may run, its default (900) filled in. For the check `default` of
    /// `.swarmops/workspaces.json`, this is what a later runner will use: `ws land --check` does
    /// not read it, and takes its own `--check-timeout`.
    pub timeout_s: u64,
    /// The digest of its definition (`argv`, `dir`, `timeout_s`, the defaults filled in; not
    /// its name, nor its stages): 64 hex digits. Two checks that differ only in name share it.
    /// Provisional: the recipe may change in a later release, so compare digests of one release
    /// only.
    pub definition_digest: String,
    /// When its answer is wanted: one to six stages, in the stages' own order (`before-land`,
    /// `after-land`, `scheduled`, `release`, `manual`, `deploy`), whatever order the file wrote
    /// them in; `before-land` by default, and for the check `default` of
    /// `.swarmops/workspaces.json`.
    #[serde(default = "default_stages")]
    pub stages: Vec<CheckStage>,
    /// The line of `.swarmops/checks.toml` that starts its table. Omitted for the check
    /// `default` read from `.swarmops/workspaces.json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
}

impl CheckOut {
    /// Takes the required fields; `stages` is `before-land` alone, `line` none.
    pub fn new(
        name: impl Into<String>,
        argv: Vec<String>,
        dir: impl Into<String>,
        timeout_s: u64,
        definition_digest: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            argv,
            dir: dir.into(),
            timeout_s,
            definition_digest: definition_digest.into(),
            stages: default_stages(),
            line: None,
        }
    }
}

/// When a check's answer is wanted. A check names one or more; `check run` runs one stage's
/// checks.
#[derive(
    Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash,
)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum CheckStage {
    /// Before a change lands: the gate. The default.
    BeforeLand,
    /// After a change has landed.
    AfterLand,
    /// On a schedule.
    Scheduled,
    /// Before a release is cut.
    Release,
    /// Only when someone asks for it.
    Manual,
    /// Before a deployment.
    Deploy,
}

impl CheckStage {
    /// Every stage, in the order an answer gives them.
    pub const ALL: [CheckStage; 6] = [
        CheckStage::BeforeLand,
        CheckStage::AfterLand,
        CheckStage::Scheduled,
        CheckStage::Release,
        CheckStage::Manual,
        CheckStage::Deploy,
    ];

    /// Its name, as the check file and the wire write it.
    pub fn as_str(self) -> &'static str {
        match self {
            CheckStage::BeforeLand => "before-land",
            CheckStage::AfterLand => "after-land",
            CheckStage::Scheduled => "scheduled",
            CheckStage::Release => "release",
            CheckStage::Manual => "manual",
            CheckStage::Deploy => "deploy",
        }
    }

    /// The stage named `name`, if it is one.
    pub fn parse(name: &str) -> Option<CheckStage> {
        CheckStage::ALL
            .into_iter()
            .find(|stage| stage.as_str() == name)
    }
}

impl Default for CheckStage {
    /// `before-land`: the stage a check is of when its file names none.
    fn default() -> Self {
        CheckStage::BeforeLand
    }
}

impl std::fmt::Display for CheckStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `check_run` (CLI `check run`): run the checks of one stage of a workspace's tree, one at a
/// time in name order. The workspace must be clean (no edits, not rebasing, not stale); its lock
/// is held while the checks run, and the store is never written.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CheckRunReq {
    /// The workspace, by its name in the store's registry.
    pub workspace: String,
    /// The stage whose checks to run; `before-land` when omitted.
    #[serde(default)]
    pub stage: CheckStage,
    /// The checks to run, each of the stage; empty (the default): every check of the stage. A
    /// name given twice runs once.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<String>,
}

impl CheckRunReq {
    /// Every `before-land` check of `workspace`.
    pub fn new(workspace: impl Into<String>) -> Self {
        Self {
            workspace: workspace.into(),
            stage: CheckStage::BeforeLand,
            checks: Vec::new(),
        }
    }
}

/// `check_run`: every selected check's outcome, in the order they ran. A run where any check
/// did not pass is `E_CHECK_FAILED`, whose `details.run` is this whole result.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CheckRunResp {
    /// Whether only the caller stands behind the files the checks ran on: `true` in the
    /// directory form (`check run --dir`), which no gate accepts; `false` in a workspace, whose
    /// files were checked against `tree` first.
    pub vouched: bool,
    /// The run's id: a UUID v7, so that runs sort by their start.
    pub run: String,
    /// The stage run.
    pub stage: CheckStage,
    /// The workspace's name. Omitted in the directory form.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// The changeset the workspace's files stood for. Omitted in the directory form.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changeset: Option<String>,
    /// That changeset's root tree, 64 hex digits: the tree every check ran on. Omitted in the
    /// directory form, which knows no tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree: Option<String>,
    /// The file the checks came from: `.swarmops/checks.toml`, or `.swarmops/workspaces.json`
    /// for the one check `default`.
    pub file: String,
    /// The check file's `version`, 1. Omitted unless `file` is `.swarmops/checks.toml`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    /// The run's directory on the host it ran on, absolute: `run.json`, the logs and each
    /// check's own directory.
    pub dir: String,
    /// The names of the caller's variables the checks got besides the pinned ones, sorted; no
    /// values. Empty when `inherited_all`.
    pub inherited_env: Vec<String>,
    /// Whether the checks got the caller's whole environment: only the check `default` of
    /// `.swarmops/workspaces.json`, which keeps what `ws land --check` gives it.
    pub inherited_all: bool,
    /// Whether a signal stopped the run before its checks ended: the check running then is
    /// `cancelled`, those after it `not_run`. A signal once the last check has exited stops
    /// nothing: the run ends as its checks did.
    pub cancelled: bool,
    /// The checks that did not pass, in run order; empty when every one passed.
    pub failed: Vec<String>,
    /// The run's milliseconds, from its first check's start to its last's end.
    pub duration_ms: u64,
    /// One per selected check, in the order they ran (by name).
    pub checks: Vec<CheckRunOut>,
}

impl CheckRunResp {
    /// A run `run` of `stage`, of the checks of `file`, kept in `dir`; vouched for by the
    /// caller or not; no check yet, nothing failed, nothing inherited.
    pub fn new(
        vouched: bool,
        run: impl Into<String>,
        stage: CheckStage,
        file: impl Into<String>,
        dir: impl Into<String>,
    ) -> Self {
        Self {
            vouched,
            run: run.into(),
            stage,
            workspace: None,
            changeset: None,
            tree: None,
            file: file.into(),
            version: None,
            dir: dir.into(),
            inherited_env: Vec::new(),
            inherited_all: false,
            cancelled: false,
            failed: Vec::new(),
            duration_ms: 0,
            checks: Vec::new(),
        }
    }
}

/// One check of a run: how it ended, and where its log is.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CheckRunOut {
    /// Its name.
    pub name: String,
    /// How it ended; only `passed` passes.
    pub outcome: CheckOutcome,
    /// The digest of its definition, as `check list` gives it (provisional).
    pub definition_digest: String,
    /// The timeout it ran under, in seconds.
    pub timeout_s: u64,
    /// Its milliseconds, from its start to the end of its output; 0 for `not_started` and
    /// `not_run`.
    pub duration_ms: u64,
    /// Its exit code, when it exited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// The signal that ended it, for a check `failed` by a signal the runner did not send.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
    /// Why it did not start, for `not_started`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_started: Option<CheckNotStarted>,
    /// The host's words for why it did not start, one line, for `not_started`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Its log's path on the host it ran on, absolute. Omitted for `not_run`, which has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log: Option<String>,
    /// The sections it opened with a marker line, in order. Omitted when it opened none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sections: Vec<CheckSectionOut>,
    /// The bytes of its output its log left out, past the log's bounds. Omitted when none.
    #[serde(default, skip_serializing_if = "crate::ops::is_zero")]
    pub dropped_bytes: u64,
    /// The last 8 KiB of its stdout, for a check that ran and did not pass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout_tail: Option<String>,
    /// The last 8 KiB of its stderr, for a check that ran and did not pass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr_tail: Option<String>,
}

impl CheckRunOut {
    /// Check `name` of `definition_digest` and `timeout_s`, ended as `outcome`; nothing else
    /// known yet.
    pub fn new(
        name: impl Into<String>,
        outcome: CheckOutcome,
        definition_digest: impl Into<String>,
        timeout_s: u64,
    ) -> Self {
        Self {
            name: name.into(),
            outcome,
            definition_digest: definition_digest.into(),
            timeout_s,
            duration_ms: 0,
            exit_code: None,
            signal: None,
            not_started: None,
            message: None,
            log: None,
            sections: Vec::new(),
            dropped_bytes: 0,
            stdout_tail: None,
            stderr_tail: None,
        }
    }
}

/// How a check of a run ended. Only `passed` passes.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CheckOutcome {
    /// It exited 0.
    Passed,
    /// It exited with another code, or a signal it was not sent by the runner ended it.
    Failed,
    /// It ran past its timeout, and its process group was killed.
    TimedOut,
    /// Its directory is missing, or its program did not start.
    NotStarted,
    /// It was running when a signal stopped the run, and its process group was killed.
    Cancelled,
    /// The run was stopped before it started: no process, no log.
    NotRun,
}

impl CheckOutcome {
    /// Its word on the wire and in a log.
    pub fn as_str(self) -> &'static str {
        match self {
            CheckOutcome::Passed => "passed",
            CheckOutcome::Failed => "failed",
            CheckOutcome::TimedOut => "timed_out",
            CheckOutcome::NotStarted => "not_started",
            CheckOutcome::Cancelled => "cancelled",
            CheckOutcome::NotRun => "not_run",
        }
    }
}

/// Why a check did not start.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CheckNotStarted {
    /// Its directory is not a directory of the tree on disk (a sparse workspace may lack it).
    Dir,
    /// Its program could not be started: not found, or not executable.
    Program,
}

/// A section a check opened with a marker line: from its marker to the next one, or to the end
/// of the check's output.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CheckSectionOut {
    /// Its title, as the marker gave it (at most 200 bytes, cut with `…`).
    pub title: String,
    /// When it opened: milliseconds since the check's start.
    pub start_ms: u64,
    /// How long it lasted, in milliseconds.
    pub duration_ms: u64,
}

impl CheckSectionOut {
    /// The section `title`, opened at `start_ms`, of `duration_ms`.
    pub fn new(title: impl Into<String>, start_ms: u64, duration_ms: u64) -> Self {
        Self {
            title: title.into(),
            start_ms,
            duration_ms,
        }
    }
}

/// `check_log` (CLI `check log`): a check's log of a run of a workspace, its last lines. Reads
/// only: no lock, nothing written.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CheckLogReq {
    /// The workspace, by its name in the store's registry.
    pub workspace: String,
    /// The check whose log to read.
    pub check: String,
    /// The run, by its id; the newest run of the workspace that ran `check` when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    /// The most lines to give, the last ones: 1 to 100,000; 1,000 when omitted.
    #[serde(default = "default_log_tail")]
    pub tail: usize,
}

impl CheckLogReq {
    /// The last lines of `check`'s log of the newest run of `workspace` that ran it.
    pub fn new(workspace: impl Into<String>, check: impl Into<String>) -> Self {
        Self {
            workspace: workspace.into(),
            check: check.into(),
            run: None,
            tail: DEFAULT_CHECK_LOG_TAIL,
        }
    }
}

/// The default [`CheckLogReq::tail`]: 1,000 lines.
pub const DEFAULT_CHECK_LOG_TAIL: usize = 1_000;

/// The most lines [`CheckLogReq::tail`] may ask for.
pub const MAX_CHECK_LOG_TAIL: usize = 100_000;

/// `check_log`: lines of a check's log, and whether it has ended. `check log --follow` prints one
/// of these first, then one for each batch of lines the check writes, the last with `ended`.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CheckLogResp {
    /// As the run's: `true` for a run of the directory form.
    pub vouched: bool,
    /// The run's id.
    pub run: String,
    /// The check's name.
    pub check: String,
    /// The log's path on the host it ran on. Omitted for a check the run never started
    /// (`not_run`), which has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log: Option<String>,
    /// Whether the log is complete: no line follows.
    pub ended: bool,
    /// How it ended, when `ended`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_by: Option<CheckLogEnd>,
    /// The check's outcome, when `ended_by` is `end`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<CheckOutcome>,
    /// The check's milliseconds, when its log's `end` line gives them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// The number of the first line given, from 1.
    pub first_line: u64,
    /// The log's lines so far.
    pub total_lines: u64,
    /// The lines given, in order.
    pub lines: Vec<CheckLogLineOut>,
}

impl CheckLogResp {
    /// The log of `check` of run `run`, vouched for or not; not ended, no line.
    pub fn new(vouched: bool, run: impl Into<String>, check: impl Into<String>) -> Self {
        Self {
            vouched,
            run: run.into(),
            check: check.into(),
            log: None,
            ended: false,
            ended_by: None,
            outcome: None,
            duration_ms: None,
            first_line: 1,
            total_lines: 0,
            lines: Vec::new(),
        }
    }
}

/// One line of a check's log.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CheckLogLineOut {
    /// Its number in the log, from 1.
    pub n: u64,
    /// Its time: milliseconds since the check's start. Omitted for a line of `other`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at_ms: Option<u64>,
    /// Where it came from.
    pub stream: CheckLogStream,
    /// Its text.
    pub text: String,
}

impl CheckLogLineOut {
    /// Line `n` of `stream`, `text`, with no time.
    pub fn new(n: u64, stream: CheckLogStream, text: impl Into<String>) -> Self {
        Self {
            n,
            at_ms: None,
            stream,
            text: text.into(),
        }
    }
}

/// Where a line of a check's log came from.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CheckLogStream {
    /// The check's stdout.
    Out,
    /// The check's stderr.
    Err,
    /// The runner's own line: `start`, `section`, `cut` or `end`, and a JSON object.
    Run,
    /// A line not in the log's format, as a check writing into its own log could leave.
    Other,
}

/// How a check's log ended.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CheckLogEnd {
    /// Its `end` line, or the run's record of a check it never started.
    End,
    /// The run's lock was let go with no `end` line: the runner died.
    RunnerGone,
}

fn default_log_tail() -> usize {
    DEFAULT_CHECK_LOG_TAIL
}

fn default_rev() -> Rev {
    Rev::from("main")
}

fn default_stages() -> Vec<CheckStage> {
    vec![CheckStage::BeforeLand]
}
