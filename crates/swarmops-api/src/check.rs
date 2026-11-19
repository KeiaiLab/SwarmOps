//! A tree's checks (M30): what `check list` answers, from the check file of a changeset's tree,
//! `.swarmops/checks.toml`, or, in a tree without one, `check.argv` of
//! `.swarmops/workspaces.json` as the one check `default`.
//!
//! ```text
//!   CheckListReq {rev, stage?} ──▶ the tree rev names ──▶ .swarmops/checks.toml (version 1)
//!                                                         or workspaces.json's check.argv
//!   CheckListResp {vouched, tree, file, version, stage, checks: [CheckOut sorted by name]}
//!   CheckOut {name, argv, dir, timeout_s, definition_digest, stages, reports, inputs, line}
//! ```
//!
//! `check run` runs one stage's checks, one at a time, and answers one [`CheckRunResp`]: an
//! outcome per check ([`CheckRunOut`]), each check's log, its findings ([`CheckFindingOut`]) and
//! the reports they came from ([`CheckReportReadOut`]), and, for a check that did not pass, the
//! tails of its output.
//!
//! ```text
//!   CheckRunReq {workspace, stage, checks} ──▶ the workspace held clean and locked
//!   CheckRunResp {vouched, run, stage, workspace?, changeset?, tree?, file, version, dir,
//!                 inherited_env, inherited_all, cancelled, failed, findings_omitted?,
//!                 duration_ms,
//!                 checks: [CheckRunOut {name, outcome, …, findings, findings_total, reports, …}
//!                          in run order]}
//! ```
//!
//! `check log` reads one check's log of a run ([`CheckLogResp`]): its lines, each with its time
//! and stream, and whether it has ended. Asked for the failed checks ([`CheckLogReq::failed`]),
//! it answers the first one's log the same way, with the names of all of them and the logs of
//! the others beside it:
//!
//! ```text
//!   CheckLogReq {workspace, check | failed: true, run?, tail}
//!   CheckLogResp {vouched, run, check, log?, ended, …, lines,
//!                 failed?: [every check that did not pass, in run order],
//!                 more?: [CheckLogMoreOut, the log of each failed check after the first],
//!                 logs_omitted?}
//! ```
//!
//! Each check a run reaches leaves one run line ([`CheckRunLine`]) in a diagnostics file on the
//! host that ran it: which check, of which run and tree, how long it waited and ran, and how it
//! ended. Nothing decides from a run line.
//!
//! ```text
//!   run-lines.jsonl:  {at, run, check, attempt, stage, tree?, vouched, workspace?,
//!                      definition_digest, wait_ms, duration_ms, outcome, exit_code?, findings?,
//!                      labels?, peak_rss_kib?, run_dir?}      one JSON object a line
//! ```
//!
//! `check runs` reads those lines, newest first, filtered, a page at a time:
//!
//! ```text
//!   CheckRunsReq {workspace, checks, tree?, outcomes, run?, since?, until?, limit, cursor?}
//!   CheckRunsResp {vouched, source: run_lines, dir, runs: [CheckRunLine newest first],
//!                  cursor?, scanned, skipped?}
//! ```
//!
//! `check plan` says, for one stage, which checks a change needs and why ([`CheckPlanResp`]): a
//! changeset against its target, a change as it would land, a workspace's edits, or the changed
//! paths the caller gives. A plan informs; it starts and skips nothing.
//!
//! ```text
//!   CheckPlanReq {rev, target, landed, base?, stage, checks}
//!   CheckPlanResp {vouched, subject, stage, rev?, tree?, …, changed, widened?,
//!                  checks: [CheckPlanOut {name, verdict, reason, …} sorted by name]}
//! ```
//!
//! No request carries a path of the caller's host: a directory the caller names is read and
//! run by the CLI alone, and its answer is the same type with `vouched` set. A result names the
//! host's paths of its run directory and logs: a run happens on one host, and its logs stay
//! there.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ops::ItemOutcome;
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
    /// The digest of its definition (`argv`, `dir`, `timeout_s`, `reports`, `inputs`, the
    /// defaults filled in; not its name, nor its stages): 64 hex digits. Two checks that differ only in name share it.
    /// Provisional: the recipe may change in a later release, so compare digests of one release
    /// only.
    pub definition_digest: String,
    /// When its answer is wanted: one to six stages, in the stages' own order (`before-land`,
    /// `after-land`, `scheduled`, `release`, `manual`, `deploy`), whatever order the file wrote
    /// them in; `before-land` by default, and for the check `default` of
    /// `.swarmops/workspaces.json`.
    #[serde(default = "default_stages")]
    pub stages: Vec<CheckStage>,
    /// The JUnit and SARIF reports it leaves in its tree, which `check run` reads into findings
    /// once it has ended, in the order the file declares them. Omitted when it declares none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reports: Vec<CheckReportOut>,
    /// Its declared inputs: path patterns from the tree's root, in the order the file declares
    /// them. A segment is a literal, or `*` (any one segment); the last may be `**` (every depth
    /// below). Omitted when it declares none: such a check takes the whole tree.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<String>,
    /// The line of `.swarmops/checks.toml` that starts its table. Omitted for the check
    /// `default` read from `.swarmops/workspaces.json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
}

impl CheckOut {
    /// Takes the required fields; `stages` is `before-land` alone, no report, no input, `line`
    /// none.
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
            reports: Vec::new(),
            inputs: Vec::new(),
            line: None,
        }
    }
}

/// A report a check declares: a file in its tree, of a format `check run` reads into findings.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CheckReportOut {
    /// Its format: `junit` or `sarif`.
    pub format: CheckReportFormat,
    /// Where the check leaves it: a repository path relative to the check's `dir`.
    pub path: String,
}

impl CheckReportOut {
    /// The report of `format` at `path`.
    pub fn new(format: CheckReportFormat, path: impl Into<String>) -> Self {
        Self {
            format,
            path: path.into(),
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
    /// The findings of the run that were counted and not returned: past the run's bound of
    /// 10,000 findings and 4 MiB of their text, every later finding is counted in its check's
    /// `findings_total` and here, and none is returned. Omitted when 0.
    #[serde(default, skip_serializing_if = "crate::ops::is_zero")]
    pub findings_omitted: u64,
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
            findings_omitted: 0,
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
    /// The host's words for why it did not start, one line, for `not_started`; or, for a check
    /// that ran and is `failed` because its log could not be written, why not.
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
    /// Its findings that the run returns, in order: those of its report file, in line order,
    /// then those of each report it declares, in declared order. Findings change no outcome.
    /// Omitted when none is returned.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<CheckFindingOut>,
    /// Every finding read from its reports, returned or not: more than `findings` holds once
    /// the run's bound is reached. Omitted when 0.
    #[serde(default, skip_serializing_if = "crate::ops::is_zero")]
    pub findings_total: u64,
    /// The reports its findings were read from, as this run read them: its report file first,
    /// when it wrote one. Omitted when there is none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reports: Vec<CheckReportReadOut>,
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
            findings: Vec::new(),
            findings_total: 0,
            reports: Vec::new(),
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
    /// Its log, or its own directory in the run's, could not be made or written (a full disk,
    /// say): nothing was run without a log.
    Log,
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

/// One finding of a check: something it reports at a level, about a path and lines of the tree
/// or about the tree as a whole. Read from the report file the check writes
/// (`SWARMOPS_CHECK_REPORT`, JSON lines), after the check has ended.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CheckFindingOut {
    /// How much it matters.
    pub level: CheckFindingLevel,
    /// What it is, in one line: at most 200 bytes, cut with `…`.
    pub title: String,
    /// What the report groups it under, when the report gives one. Omitted otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// The file it is about: a repository path from the tree's root. Omitted when the finding
    /// names no file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Its first line in `path`, from 1. Omitted when it names none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
    /// Its last line in `path`, at or after `line`. Omitted when it names none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u64>,
    /// What to know about it, its line breaks kept: at most 4 KiB, cut with `…`. Omitted when
    /// empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// The report it came from: its index, from 0, in its check's `reports`.
    pub report: u32,
}

impl CheckFindingOut {
    /// A finding of `level`, `title`, from report 0; no group, path, lines or message.
    pub fn new(level: CheckFindingLevel, title: impl Into<String>) -> Self {
        Self {
            level,
            title: title.into(),
            group: None,
            path: None,
            line: None,
            end_line: None,
            message: None,
            report: 0,
        }
    }
}

/// How much a finding matters. Findings change no check's outcome.
#[derive(
    Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash,
)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CheckFindingLevel {
    /// Something wrong.
    Error,
    /// Something likely wrong, or to look at.
    Warning,
    /// Something to know.
    Notice,
}

impl CheckFindingLevel {
    /// Every level, from the most to the least.
    pub const ALL: [CheckFindingLevel; 3] = [
        CheckFindingLevel::Error,
        CheckFindingLevel::Warning,
        CheckFindingLevel::Notice,
    ];

    /// Its word, as a report file and the wire write it.
    pub fn as_str(self) -> &'static str {
        match self {
            CheckFindingLevel::Error => "error",
            CheckFindingLevel::Warning => "warning",
            CheckFindingLevel::Notice => "notice",
        }
    }

    /// The level named `name`, if it is one.
    pub fn parse(name: &str) -> Option<CheckFindingLevel> {
        CheckFindingLevel::ALL
            .into_iter()
            .find(|level| level.as_str() == name)
    }
}

/// A report a check's findings were read from, as one run read it, after the check ended.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CheckReportReadOut {
    /// Its format.
    pub format: CheckReportFormat,
    /// Where it was: the report file's absolute path on the host the check ran on; a declared
    /// report's path as its check declares it, relative to the check's `dir`.
    pub path: String,
    /// What came of it.
    pub state: CheckReportState,
    /// Why it was not read, for `refused`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fault: Option<CheckReportFault>,
    /// One line: for `refused`, why, in the host's words where they apply; for the report
    /// file, its first invalid line and why, or that it was cut. Omitted otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// The findings it gave, returned or not, for `read`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub findings: Option<u64>,
    /// The report file's lines that are no finding, each skipped. Omitted when 0.
    #[serde(default, skip_serializing_if = "crate::ops::is_zero")]
    pub invalid_lines: u64,
    /// Whether the report file was past 64 MiB: its lines up to the bound were read, the rest
    /// not. Omitted when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cut: bool,
    /// A JUnit report's test cases, for `read`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tests: Option<u64>,
    /// Those of them that failed or ended in an error, each a finding, for a JUnit report
    /// `read`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed: Option<u64>,
    /// Those of them skipped, for a JUnit report `read`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped: Option<u64>,
    /// A SARIF report's results, every kind, for `read`; those of kind `fail` are its findings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub results: Option<u64>,
}

impl CheckReportReadOut {
    /// The report of `format` at `path`, ended in `state`; nothing else known yet.
    pub fn new(
        format: CheckReportFormat,
        path: impl Into<String>,
        state: CheckReportState,
    ) -> Self {
        Self {
            format,
            path: path.into(),
            state,
            fault: None,
            message: None,
            findings: None,
            invalid_lines: 0,
            cut: false,
            tests: None,
            failed: None,
            skipped: None,
            results: None,
        }
    }
}

/// The format of a report a check's findings come from.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CheckReportFormat {
    /// The check's report file: one finding a line, as JSON, at `SWARMOPS_CHECK_REPORT`. Never
    /// declared.
    Findings,
    /// A JUnit XML report of test cases: each case that failed or ended in an error is a
    /// finding.
    Junit,
    /// A SARIF 2.1.0 log of results: each result of kind `fail` is a finding.
    Sarif,
}

impl CheckReportFormat {
    /// The formats a check may declare, in the order a message lists them.
    pub const DECLARED: [CheckReportFormat; 2] =
        [CheckReportFormat::Junit, CheckReportFormat::Sarif];

    /// Its word on the wire and in the check file.
    pub fn as_str(self) -> &'static str {
        match self {
            CheckReportFormat::Findings => "findings",
            CheckReportFormat::Junit => "junit",
            CheckReportFormat::Sarif => "sarif",
        }
    }
}

/// What came of a report after its check.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CheckReportState {
    /// Read: its findings are the check's.
    Read,
    /// Not there after the check.
    Missing,
    /// There, but not written by this run of the check: left from before, and not read.
    Stale,
    /// There, and not read as its format: `fault` says why.
    Refused,
}

/// Why a report was not read. A report that is not read is a state of that report, never an
/// error of the run.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CheckReportFault {
    /// Past its size bound.
    TooLarge,
    /// A symbolic link, a directory, a FIFO or another kind of entry; nothing behind it read.
    NotAFile,
    /// Not UTF-8.
    NotText,
    /// Not of its format's syntax.
    Syntax,
    /// A document type declaration, which is refused before anything is read.
    Doctype,
    /// Nested past its depth bound.
    TooDeep,
    /// Well formed, but not of its format's shape.
    Shape,
    /// It could not be read: the host's words are in `message`.
    Io,
}

/// One run line: one check of one run of `check run`, as the host that ran it appends it to its
/// run lines file (`run-lines.jsonl`), one JSON object a line. Diagnostics: the file is not
/// synced and nothing decides from it. Later releases add keys; none is removed or changes its
/// meaning.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CheckRunLine {
    /// When the check started (for `not_started`: when it was tried), in Unix milliseconds on
    /// the host that ran it.
    pub at: u64,
    /// The run's id: a UUID v7.
    pub run: String,
    /// The check's name.
    pub check: String,
    /// Which attempt of the check in its run this is, from 1.
    pub attempt: u32,
    /// The stage run.
    pub stage: CheckStage,
    /// The root tree the check ran on: 64 hex digits. Omitted in the directory form, which knows
    /// no tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree: Option<String>,
    /// Whether only the caller stands behind the files the check ran on: `true` in the directory
    /// form.
    pub vouched: bool,
    /// The workspace's name. Omitted in the directory form.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// The digest of the check's definition, as `check list` gives it (provisional).
    pub definition_digest: String,
    /// The milliseconds from the run's start to the check's start: in a run of one check at a
    /// time, the time the checks before it took.
    pub wait_ms: u64,
    /// The check's milliseconds, from its start to the end of its output; 0 for `not_started`.
    pub duration_ms: u64,
    /// How it ended; never `not_run`, which leaves no line.
    pub outcome: CheckOutcome,
    /// Its exit code, when it exited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// The findings read from its reports. Omitted when 0.
    #[serde(default, skip_serializing_if = "crate::ops::is_zero")]
    pub findings: u64,
    /// The runner labels the check requires. Omitted when it requires none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    /// The most resident memory the check's process group held at once, in KiB, as the runner
    /// sampled it: the sum over the group's processes, about once a second, on Linux. A spike
    /// between two samples is not seen. Omitted on other hosts, and for a check no sample saw.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peak_rss_kib: Option<u64>,
    /// The run's directory on the host it ran on, absolute: where its record and logs are while
    /// the host keeps the run. Omitted when longer than 1,024 bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_dir: Option<String>,
}

impl CheckRunLine {
    /// The line of `check` of `run`, started at `at`, of `stage`, ended as `outcome`: attempt
    /// 1, not vouched, no digest, no wait, no duration, nothing else known.
    pub fn new(
        at: u64,
        run: impl Into<String>,
        check: impl Into<String>,
        stage: CheckStage,
        outcome: CheckOutcome,
    ) -> Self {
        Self {
            at,
            run: run.into(),
            check: check.into(),
            attempt: 1,
            stage,
            tree: None,
            vouched: false,
            workspace: None,
            definition_digest: String::new(),
            wait_ms: 0,
            duration_ms: 0,
            outcome,
            exit_code: None,
            findings: 0,
            labels: Vec::new(),
            peak_rss_kib: None,
            run_dir: None,
        }
    }
}

/// `check_runs` (CLI `check runs`): the run lines of a workspace's checks, newest first, those
/// that match every filter given, a page at a time. Reads only: no lock, no identity, nothing
/// written. Diagnostics: nothing decides from a run line.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CheckRunsReq {
    /// The workspace, by its name in the store's registry.
    pub workspace: String,
    /// Only the lines of these checks; every check when empty (the default).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<String>,
    /// Only the lines whose tree starts with these 12 to 64 lowercase hex digits. A line with
    /// no tree (a run of the directory form) never matches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree: Option<String>,
    /// Only the lines that ended as one of these: `passed`, `failed`, `timed_out`, `cancelled`
    /// or `not_started` (`not_run` leaves no line, and is refused); every outcome when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outcomes: Vec<CheckOutcome>,
    /// Only the lines of this run, by its id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    /// Only the lines of checks that started at or after this time, in Unix milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<u64>,
    /// Only the lines of checks that started before this time, in Unix milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<u64>,
    /// The most lines of a page: 1 to 1,000; 100 when omitted.
    #[serde(default = "default_runs_limit")]
    #[schemars(range(min = 1, max = MAX_CHECK_RUNS_LIMIT))]
    pub limit: usize,
    /// The cursor a page gave, to read the page after it, with the same filters: pass it back
    /// as it was returned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

impl CheckRunsReq {
    /// The newest run lines of `workspace`: no filter, the first page of 100.
    pub fn new(workspace: impl Into<String>) -> Self {
        Self {
            workspace: workspace.into(),
            checks: Vec::new(),
            tree: None,
            outcomes: Vec::new(),
            run: None,
            since: None,
            until: None,
            limit: DEFAULT_CHECK_RUNS_LIMIT,
            cursor: None,
        }
    }
}

/// The default [`CheckRunsReq::limit`]: 100 lines.
pub const DEFAULT_CHECK_RUNS_LIMIT: usize = 100;

/// The most lines [`CheckRunsReq::limit`] may ask for.
pub const MAX_CHECK_RUNS_LIMIT: usize = 1_000;

/// `check_runs`: one page of run lines, newest first. Diagnostics: the lines are what the hosts
/// that ran the checks appended, unsynced, and nothing decides from them.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CheckRunsResp {
    /// Whether only the caller stands behind where the lines were read from: `true` for a
    /// directory's lines (`check runs --dir`) and for a directory the caller named; `false`
    /// for a workspace's own state.
    pub vouched: bool,
    /// What the lines were read from.
    pub source: CheckRunsSource,
    /// The directory the lines were read from, on this host, absolute.
    pub dir: String,
    /// The page: the lines that match, newest first, by the time each check started.
    pub runs: Vec<CheckRunLine>,
    /// The cursor of the next page; omitted when no more lines match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// The lines read from the files, matching or not.
    pub scanned: u64,
    /// The lines among them that did not read as a run line, each passed over. Omitted when 0.
    #[serde(default, skip_serializing_if = "crate::ops::is_zero")]
    pub skipped: u64,
}

impl CheckRunsResp {
    /// An empty page of the run lines in `dir`, vouched for by the caller or not.
    pub fn new(vouched: bool, dir: impl Into<String>) -> Self {
        Self {
            vouched,
            source: CheckRunsSource::RunLines,
            dir: dir.into(),
            runs: Vec::new(),
            cursor: None,
            scanned: 0,
            skipped: 0,
        }
    }
}

/// What a page of `check_runs` was read from.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CheckRunsSource {
    /// The run lines' files of a host: diagnostics that `check run` appended there.
    RunLines,
}

/// `check_plan` (CLI `check plan`): for one stage, which checks a change needs and why. The
/// change is the changeset `rev` against `target`, or, with `landed`, `rev` as `land_check`
/// would land it there. Reads only: no lock, no identity, nothing written. A plan informs: it
/// starts no check and skips none.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CheckPlanReq {
    /// The candidate: the changeset whose change to plan.
    pub rev: Rev,
    /// The ref the change would land on; `main` when omitted. The definitions are compared with
    /// the check file at its head.
    #[serde(default = "default_plan_target")]
    pub target: String,
    /// Plan the change as `land_check` lands it on `target`: its changed paths and its tree are
    /// the landed changeset's. A change that conflicts is `E_CONFLICT`, with no plan.
    #[serde(default)]
    pub landed: bool,
    /// With `landed`: where the change starts, as a land's `base`. `E_INVALID_ARG` with
    /// `details.reason: base` without `landed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<Rev>,
    /// The stage to plan; `before-land` when omitted. A plan is of one stage.
    #[serde(default)]
    pub stage: CheckStage,
    /// Only these checks of the plan; empty (the default): every one. A name the plan does not
    /// hold is `E_INVALID_ARG`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<String>,
}

impl CheckPlanReq {
    /// The `before-land` plan of the changeset `rev` against `main`.
    pub fn new(rev: impl Into<Rev>) -> Self {
        Self {
            rev: rev.into(),
            target: default_plan_target(),
            landed: false,
            base: None,
            stage: CheckStage::BeforeLand,
            checks: Vec::new(),
        }
    }
}

/// `check_plan`: each check of one stage, `run` or `not_applicable`, with one reason. The same
/// subject gives the same answer byte for byte: it holds no time and no path of a host.
///
/// Later releases add verdicts, reasons and causes. A reader takes a verdict it does not know
/// as `run`.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CheckPlanResp {
    /// Whether only the caller stands behind the plan: `true` in the directory form (`check
    /// plan --dir`), where the caller gives both the check file and the changed paths, and
    /// which no gate accepts; `false` when both were read from the store.
    pub vouched: bool,
    /// What was planned.
    pub subject: CheckPlanSubject,
    /// The stage planned.
    pub stage: CheckStage,
    /// The candidate changeset, a full id: the changeset planned, or a workspace's head.
    /// Omitted in the directory form.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
    /// The candidate's root tree: the changeset's, the landed changeset's for a clean
    /// `landed` plan, a workspace's head's when it holds no edit. Omitted when no tree in the
    /// store is what was planned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree: Option<String>,
    /// The workspace's name. Omitted but for a workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// The workspace's edits against its head, counted. Omitted but for a workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edits: Option<u64>,
    /// The ref the change would land on. Omitted in the directory form.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// The target's head, a full id: the definitions were compared with its check file.
    /// Omitted in the directory form.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub onto: Option<String>,
    /// Where the change starts: the merge base of the candidate and the target, or a
    /// workspace's base. Omitted for `landed`, in the directory form, and when the two share no
    /// history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// For `landed`: what landing the change would come to, `clean`, `replayed` or
    /// `nothing_to_land`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<ItemOutcome>,
    /// The file the candidate's checks came from, as `check_list` gives it. Omitted when the
    /// candidate has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// The check file's `version`, 1. Omitted unless `file` is `.swarmops/checks.toml`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    /// The changed paths read; 0 when they are not known.
    pub changed: u64,
    /// Why every check runs, when the whole plan is widened: the first cause that holds, in
    /// the causes' own order. Omitted when the plan is not widened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub widened: Option<CheckPlanWidened>,
    /// The checks of the stage, the candidate's and those only the target's head has, sorted
    /// by name, bytewise; empty when neither side defines one.
    pub checks: Vec<CheckPlanOut>,
}

impl CheckPlanResp {
    /// A plan of `subject` at `stage`, vouched for by the caller or not: no check, no changed
    /// path, nothing else named.
    pub fn new(vouched: bool, subject: CheckPlanSubject, stage: CheckStage) -> Self {
        Self {
            vouched,
            subject,
            stage,
            rev: None,
            tree: None,
            workspace: None,
            edits: None,
            target: None,
            onto: None,
            base: None,
            outcome: None,
            file: None,
            version: None,
            changed: 0,
            widened: None,
            checks: Vec::new(),
        }
    }
}

/// One check of a plan: whether the change needs it, and the one reason why.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CheckPlanOut {
    /// The check's name.
    pub name: String,
    /// Whether the change needs it. A reader takes a verdict it does not know as `run`.
    pub verdict: CheckPlanVerdict,
    /// Why: one reason, the first that holds.
    pub reason: CheckPlanReason,
    /// For `widened`: the plan's cause.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub widened: Option<CheckPlanWidened>,
    /// For `definition_changed`: how the definition differs from the target's head's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<CheckDefinitionChange>,
    /// For a `renamed` definition: the name the target's head has it under.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub was: Option<String>,
    /// For `input_changed`: the first changed path read that its inputs match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// For `input_changed`: the first of its patterns, in declared order, that matches `path`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    /// The digest of its definition: the candidate's; for a `deleted` definition, the
    /// target's. Provisional, as `check_list`'s.
    pub definition_digest: String,
}

impl CheckPlanOut {
    /// The check `name` with its verdict, its reason and its definition's digest; nothing else.
    pub fn new(
        name: impl Into<String>,
        verdict: CheckPlanVerdict,
        reason: CheckPlanReason,
        definition_digest: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            verdict,
            reason,
            widened: None,
            definition: None,
            was: None,
            path: None,
            pattern: None,
            definition_digest: definition_digest.into(),
        }
    }
}

/// What a plan is of.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CheckPlanSubject {
    /// A changeset against its target: what it changed since their merge base.
    Changeset,
    /// A change as `land_check` lands it on its target.
    Landed,
    /// A workspace: its committed chain and its edits.
    Workspace,
    /// The changed paths the caller gave, with the check file of a directory it named.
    Paths,
}

impl CheckPlanSubject {
    /// Every subject, in the order the reference lists them.
    pub const ALL: [CheckPlanSubject; 4] = [
        CheckPlanSubject::Changeset,
        CheckPlanSubject::Landed,
        CheckPlanSubject::Workspace,
        CheckPlanSubject::Paths,
    ];
}

/// Whether a change needs a check. Later releases add verdicts: a reader takes one it does not
/// know as `run`.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CheckPlanVerdict {
    /// The change needs the check.
    Run,
    /// No changed path is among the check's inputs, and its definition is the target's head's.
    NotApplicable,
}

impl CheckPlanVerdict {
    /// Every verdict, in the order the reference lists them.
    pub const ALL: [CheckPlanVerdict; 2] = [CheckPlanVerdict::Run, CheckPlanVerdict::NotApplicable];
}

/// Why a check has its verdict: the first of these that holds, in this order. Later releases
/// add reasons.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CheckPlanReason {
    /// `run`: its definition is not the one the target's head has under its name.
    DefinitionChanged,
    /// `run`: the whole plan is widened, since the changed paths or the target's definitions
    /// are not known in full.
    Widened,
    /// `run`: a changed path is among its inputs.
    InputChanged,
    /// `run`: it declares no inputs, so it takes the whole tree, and a path changed.
    NoInputs,
    /// `not_applicable`: no changed path is among its inputs.
    InputsUnchanged,
}

impl CheckPlanReason {
    /// Every reason, in the order that decides.
    pub const ALL: [CheckPlanReason; 5] = [
        CheckPlanReason::DefinitionChanged,
        CheckPlanReason::Widened,
        CheckPlanReason::InputChanged,
        CheckPlanReason::NoInputs,
        CheckPlanReason::InputsUnchanged,
    ];
}

/// Why a whole plan is widened, so that every check runs. When several hold, the first in
/// this order is the one named. Later releases add causes.
#[derive(
    Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash,
)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CheckPlanWidened {
    /// The changed paths are not known: the candidate and the target share no history, a
    /// workspace holds entries off its prefixes, or the directory form was given no path.
    PathsUnknown,
    /// More paths changed than a plan reads, or than the list given holds.
    PathsTruncated,
    /// A changed path given is not a repository path.
    PathUnreadable,
    /// The check file is among the changed paths and cannot be compared: in the directory
    /// form, or merged by a land.
    CheckFileChanged,
    /// The check file at the target's head does not read.
    TargetUnread,
}

impl CheckPlanWidened {
    /// Every cause, in the order that picks the one named.
    pub const ALL: [CheckPlanWidened; 5] = [
        CheckPlanWidened::PathsUnknown,
        CheckPlanWidened::PathsTruncated,
        CheckPlanWidened::PathUnreadable,
        CheckPlanWidened::CheckFileChanged,
        CheckPlanWidened::TargetUnread,
    ];
}

/// How a check's definition differs from the one at the target's head.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CheckDefinitionChange {
    /// The target's head has no check of this name, and none this one is renamed from.
    Added,
    /// The target's head has this definition under one other name, `was`, which the candidate
    /// no longer has.
    Renamed,
    /// The target's head has a check of this name with another definition.
    Modified,
    /// The candidate has no check of this name: the target's head does.
    Deleted,
}

impl CheckDefinitionChange {
    /// Every change, in the order the reference lists them.
    pub const ALL: [CheckDefinitionChange; 4] = [
        CheckDefinitionChange::Added,
        CheckDefinitionChange::Renamed,
        CheckDefinitionChange::Modified,
        CheckDefinitionChange::Deleted,
    ];
}

/// `check_log` (CLI `check log`): a check's log of a run of a workspace, its last lines; or,
/// with `failed`, the logs of the checks of a run that did not pass. Exactly one of `check` and
/// `failed` is given. Reads only: no lock, nothing written.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CheckLogReq {
    /// The workspace, by its name in the store's registry.
    pub workspace: String,
    /// The check whose log to read. Omitted with `failed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<String>,
    /// Read the logs of the checks of the run that did not pass, instead of one check's.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub failed: bool,
    /// The run, by its id; when omitted, the newest run of the workspace that ran `check`, or
    /// with `failed` the newest run of the workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    /// The most lines to give of a log, the last ones: 1 to 100,000; 1,000 when omitted. With
    /// `failed`, of each log.
    #[serde(default = "default_log_tail")]
    pub tail: usize,
}

impl CheckLogReq {
    /// The last lines of `check`'s log of the newest run of `workspace` that ran it.
    pub fn new(workspace: impl Into<String>, check: impl Into<String>) -> Self {
        Self {
            workspace: workspace.into(),
            check: Some(check.into()),
            failed: false,
            run: None,
            tail: DEFAULT_CHECK_LOG_TAIL,
        }
    }

    /// The last lines of the log of each check that did not pass in the newest run of
    /// `workspace`.
    pub fn failed(workspace: impl Into<String>) -> Self {
        Self {
            workspace: workspace.into(),
            check: None,
            failed: true,
            run: None,
            tail: DEFAULT_CHECK_LOG_TAIL,
        }
    }
}

/// The default [`CheckLogReq::tail`]: 1,000 lines.
pub const DEFAULT_CHECK_LOG_TAIL: usize = 1_000;

/// The most lines [`CheckLogReq::tail`] may ask for, and the most lines the failed checks' logs
/// of one answer hold together.
pub const MAX_CHECK_LOG_TAIL: usize = 100_000;

/// The most logs one answer of the failed checks' logs holds: the rest are named and counted.
pub const MAX_FAILED_LOGS: usize = 20;

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
    /// With `failed`: every check of the run that did not pass, in run order; `check` is the
    /// first of them. Omitted for one check's log.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed: Vec<String>,
    /// With `failed`: the logs of the failed checks after the first, in run order. Omitted when
    /// there is no other.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub more: Vec<CheckLogMoreOut>,
    /// With `failed`: the failed checks whose logs this answer leaves out, past 20 logs or
    /// 100,000 lines; `failed` names them. Omitted when 0.
    #[serde(default, skip_serializing_if = "crate::ops::is_zero")]
    pub logs_omitted: u64,
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
            failed: Vec::new(),
            more: Vec::new(),
            logs_omitted: 0,
        }
    }
}

/// The log of one more failed check, in an answer of the failed checks' logs: what `check_log`
/// answers for that check alone, key for key.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CheckLogMoreOut {
    /// As the run's: `true` for a run of the directory form.
    pub vouched: bool,
    /// The run's id.
    pub run: String,
    /// The check's name.
    pub check: String,
    /// The log's path on the host it ran on. Omitted for a check the run never started, which
    /// has none.
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

impl From<CheckLogResp> for CheckLogMoreOut {
    /// `log`, one check's log, as one more of an answer: its own keys, and none of `failed`,
    /// `more` and `logs_omitted`.
    fn from(log: CheckLogResp) -> Self {
        Self {
            vouched: log.vouched,
            run: log.run,
            check: log.check,
            log: log.log,
            ended: log.ended,
            ended_by: log.ended_by,
            outcome: log.outcome,
            duration_ms: log.duration_ms,
            first_line: log.first_line,
            total_lines: log.total_lines,
            lines: log.lines,
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

fn default_runs_limit() -> usize {
    DEFAULT_CHECK_RUNS_LIMIT
}

fn default_log_tail() -> usize {
    DEFAULT_CHECK_LOG_TAIL
}

fn default_rev() -> Rev {
    Rev::from("main")
}

fn default_plan_target() -> String {
    "main".to_owned()
}

fn default_stages() -> Vec<CheckStage> {
    vec![CheckStage::BeforeLand]
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use serde_json::Value;

    use super::{CheckOutcome, CheckRunLine, CheckStage};

    /// The names of the properties `T`'s schema declares.
    fn properties<T: schemars::JsonSchema>() -> BTreeSet<String> {
        let schema = schemars::schema_for!(T).to_value();
        let names = schema["properties"].as_object().map(|p| p.keys().cloned());
        names.into_iter().flatten().collect()
    }

    /// A run line with every field set writes each of them, the schema names exactly those, and
    /// the line reads back.
    #[test]
    fn every_run_line_field_is_in_the_schema() {
        // 1. Every field set.
        let run = "0193a5b0-0000-7000-8000-000000000001";
        let mut line = CheckRunLine::new(1, run, "unit", CheckStage::Manual, CheckOutcome::Failed);
        line.attempt = 2;
        line.tree = Some("ab".repeat(32));
        line.vouched = true;
        line.workspace = Some("w".to_owned());
        line.definition_digest = "cd".repeat(32);
        line.wait_ms = 3;
        line.duration_ms = 4;
        line.exit_code = Some(5);
        line.findings = 6;
        line.labels = vec!["linux".to_owned()];
        line.peak_rss_kib = Some(7);
        line.run_dir = Some("/runs/r".to_owned());

        // 2. Written, its keys are the schema's properties, in the struct's order, and it reads
        //    back.
        let text = serde_json::to_string(&line).unwrap();
        let Value::Object(written) = serde_json::from_str(&text).unwrap() else {
            panic!("a run line is written as an object");
        };
        let keys: BTreeSet<String> = written.keys().cloned().collect();
        assert_eq!(
            keys,
            properties::<CheckRunLine>(),
            "fields against the schema"
        );
        assert!(text.starts_with(r#"{"at":1,"run":"#), "{text}");
        let read: CheckRunLine = serde_json::from_value(Value::Object(written)).unwrap();
        assert_eq!(read, line, "read back");
    }

    /// A line of the fewest keys reads, with a key no build knows; one without a required key
    /// does not.
    #[test]
    fn a_run_line_reads_with_its_required_keys_alone() {
        let least = serde_json::json!({
            "at": 1, "run": "r", "check": "unit", "attempt": 1, "stage": "before-land",
            "vouched": false, "definition_digest": "", "wait_ms": 0, "duration_ms": 0,
            "outcome": "passed", "later": true,
        });
        let read: CheckRunLine = serde_json::from_value(least.clone()).unwrap();
        let built = CheckRunLine::new(1, "r", "unit", CheckStage::BeforeLand, CheckOutcome::Passed);
        assert_eq!(read, built, "the constructor's defaults");
        let mut short = least;
        short.as_object_mut().unwrap().remove("outcome");
        assert!(
            serde_json::from_value::<CheckRunLine>(short).is_err(),
            "no outcome"
        );
    }
}
