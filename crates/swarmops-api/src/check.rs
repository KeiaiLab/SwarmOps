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
//! No type here carries a path of the caller's host: a directory the caller names is read by
//! the CLI alone, and its answer is the same [`CheckListResp`] with `vouched` set.

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

fn default_rev() -> Rev {
    Rev::from("main")
}

fn default_stages() -> Vec<CheckStage> {
    vec![CheckStage::BeforeLand]
}
