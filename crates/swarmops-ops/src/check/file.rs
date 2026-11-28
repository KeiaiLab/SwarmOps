//! The check file's reader: `.swarmops/checks.toml`, version 1, and the one check `default` that
//! `check.argv` of `.swarmops/workspaces.json` gives a tree without it.
//!
//! ```text
//!   bytes ─ size ─ UTF-8 ─ TOML (spans kept) ─ version = 1 ─ [check.<name>] each:
//!                                                      name, argv, dir, timeout_s, stages,
//!                                                      reports: [{format, path}, …],
//!                                                      inputs: [pattern, …]
//!   any key but those ─▶ unknown_key at its line (a later key fails closed here)
//! ```
//!
//! The file is TOML 1.1, as the locked `toml` crate reads it (`\e`, `\xHH`, an inline table over
//! several lines). The walk reads the document with the span of every key and value, so each
//! refusal names its key and line from the document itself, never from a parser's wording.
//! Faults are found in file order after the version: the first one is the refusal.

use std::fmt::Write as _;

use serde::Deserialize;
use serde_json::{Map, Value};
use swarmops_api::{
    CheckOut, CheckReportFormat, CheckReportOut, CheckStage, DEFAULT_CHECK_TIMEOUT_S, ErrorCode,
    OpError,
};
use swarmops_core::RepoPath;
use toml::Spanned;
use toml::de::{DeTable, DeValue};

use super::{
    CHECK_FILE, CHECK_FILE_VERSION, LEGACY_FILE, MAX_ARGV_BYTES, MAX_ARGV_ITEMS,
    MAX_CHECK_FILE_BYTES, MAX_CHECK_INPUTS, MAX_CHECK_NAME_BYTES, MAX_CHECK_REPORTS,
    MAX_CHECK_TIMEOUT_S, MAX_CHECKS, MAX_LEGACY_BYTES, digest, inputs,
};
use crate::error::{line_of, one_line};

/// The name the one check of [`LEGACY_FILE`] takes.
pub const LEGACY_CHECK: &str = "default";

/// The directory a check runs in when its file names none: the tree's root.
pub const ROOT_DIR: &str = ".";

/// The top-level table of the checks.
const CHECKS_KEY: &str = "check";

/// The top-level key of the file's version.
const VERSION_KEY: &str = "version";

/// What the hint of a refused check file says it takes.
const CHECK_FILE_HINT: &str = "version 1 of .swarmops/checks.toml takes version = 1, then one \
[check.<name>] table a check, with argv (required), dir, timeout_s, stages, inputs (path \
patterns) and reports (tables of format, junit or sarif, and path)";

/// What the hint of a key version 1 does not take says.
const UNKNOWN_KEY_HINT: &str = "version 1 of .swarmops/checks.toml takes version and \
[check.<name>] tables of argv, dir, timeout_s, stages, inputs and reports (of format and path); a \
newer swarmops may read this key";

/// What the hint of a refused `workspaces.json` says it takes.
const LEGACY_HINT: &str = "check.argv of .swarmops/workspaces.json is an array of strings, the \
program first, and check.timeout_s a number of seconds; or name the checks in \
.swarmops/checks.toml";

/// A check file as version 1 reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CheckFile {
    /// The file's `version`: 1.
    pub version: u32,
    /// Its checks, sorted by name, bytewise.
    pub checks: Vec<Check>,
}

/// One check, its defaults filled in: its definition is `argv`, `dir`, `timeout_s`, `reports`
/// and `inputs`; its `stages` say when its answer is wanted.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Check {
    /// Its name.
    pub name: String,
    /// The program and its arguments, run with no shell.
    pub argv: Vec<String>,
    /// The directory of the tree it runs in: [`ROOT_DIR`] or a repository path.
    pub dir: String,
    /// The most seconds it may run.
    pub timeout_s: u64,
    /// When its answer is wanted: one to six stages, none twice, in their own order (that of
    /// [`CheckStage::ALL`]); `before-land` when the file names none.
    pub stages: Vec<CheckStage>,
    /// The JUnit and SARIF reports it leaves in its tree, in the order the file declares them;
    /// none when it declares none.
    pub reports: Vec<Report>,
    /// The path patterns of what it reads ([`mod@inputs`]), sorted bytewise, none twice; none when
    /// the file declares none: it reads the whole tree.
    pub inputs: Vec<String>,
    /// The line its table starts at; none for [`LEGACY_CHECK`].
    pub line: Option<u64>,
}

/// A report a check declares: a file it leaves in its tree, read into findings once it has
/// ended.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Report {
    /// Its format: [`CheckReportFormat::Junit`] or [`CheckReportFormat::Sarif`].
    pub format: CheckReportFormat,
    /// Where it is: a repository path relative to the check's `dir`.
    pub path: String,
}

impl Report {
    /// The report of `format` at `path`.
    pub fn new(format: CheckReportFormat, path: impl Into<String>) -> Report {
        Report {
            format,
            path: path.into(),
        }
    }
}

impl Check {
    /// The digest of its definition: 64 hex digits, by a provisional recipe that names the
    /// program, its arguments, the directory, the timeout, the reports and the inputs, not the
    /// check.
    pub fn definition_digest(&self) -> String {
        digest::definition(&digest::Definition {
            argv: &self.argv,
            dir: &self.dir,
            timeout_s: self.timeout_s,
            reports: &self.reports,
            inputs: &self.inputs,
        })
    }

    /// The check as `check list` gives it.
    pub fn out(&self) -> CheckOut {
        let mut out = CheckOut::new(
            self.name.clone(),
            self.argv.clone(),
            self.dir.clone(),
            self.timeout_s,
            self.definition_digest(),
        );
        out.stages.clone_from(&self.stages);
        out.reports = (self.reports.iter())
            .map(|report| CheckReportOut::new(report.format, report.path.clone()))
            .collect();
        out.inputs.clone_from(&self.inputs);
        out.line = self.line;
        out
    }

    /// Whether it is of `stage`.
    pub fn is_of(&self, stage: CheckStage) -> bool {
        self.stages.contains(&stage)
    }
}

/// Why a check file is refused: `details.reason` of `E_CHECK_FILE`, a closed set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Reason {
    /// More bytes than the file may hold.
    TooLarge,
    /// Not UTF-8.
    NotText,
    /// A symbolic link, a directory or another kind of entry at the path.
    NotAFile,
    /// Not TOML (JSON for the legacy file), or a key or table given twice.
    Syntax,
    /// No `version`, or one that is not the integer 1.
    Version,
    /// A key version 1 does not take.
    UnknownKey,
    /// A check name outside the grammar.
    Name,
    /// A value of the wrong type or outside its rule.
    Value,
    /// More checks than [`MAX_CHECKS`].
    TooMany,
}

impl Reason {
    /// Every reason, in the order the reference lists them.
    pub const ALL: [Reason; 9] = [
        Reason::TooLarge,
        Reason::NotText,
        Reason::NotAFile,
        Reason::Syntax,
        Reason::Version,
        Reason::UnknownKey,
        Reason::Name,
        Reason::Value,
        Reason::TooMany,
    ];

    /// Its word in `details.reason`.
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::TooLarge => "too_large",
            Reason::NotText => "not_text",
            Reason::NotAFile => "not_a_file",
            Reason::Syntax => "syntax",
            Reason::Version => "version",
            Reason::UnknownKey => "unknown_key",
            Reason::Name => "name",
            Reason::Value => "value",
            Reason::TooMany => "too_many",
        }
    }
}

/// A refused check file: what [`Refusal::into_error`] makes `E_CHECK_FILE` of.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Refusal {
    /// The file refused: [`CHECK_FILE`] or [`LEGACY_FILE`].
    pub file: &'static str,
    /// Why.
    pub reason: Reason,
    /// What is wrong, after the file's name and before the line.
    pub what: String,
    /// The 1-based line at fault, when the fault has one.
    pub line: Option<u64>,
    /// The key at fault, as a dotted path, when one is.
    pub key: Option<String>,
    /// For [`Reason::TooLarge`]: the file's size and its limit.
    pub size: Option<(u64, u64)>,
}

impl Refusal {
    /// A refusal of `file` for `reason`, saying `what`.
    pub(crate) fn new(file: &'static str, reason: Reason, what: impl Into<String>) -> Refusal {
        Refusal {
            file,
            reason,
            what: what.into(),
            line: None,
            key: None,
            size: None,
        }
    }

    /// The same, at `line`.
    fn at(mut self, line: u64) -> Refusal {
        self.line = Some(line);
        self
    }

    /// The same, naming `key`.
    fn key(mut self, key: String) -> Refusal {
        self.key = Some(key);
        self
    }

    /// A refusal of `file` for its `size`, past `limit`.
    pub(crate) fn too_large(file: &'static str, size: u64, limit: u64) -> Refusal {
        let mut refusal = Refusal::new(
            file,
            Reason::TooLarge,
            format!("is {size} bytes, more than {limit}"),
        );
        refusal.size = Some((size, limit));
        refusal
    }

    /// A refusal of `file`: what is at its path is not a file.
    pub(crate) fn not_a_file(file: &'static str) -> Refusal {
        Refusal::new(
            file,
            Reason::NotAFile,
            "is not a file: a symbolic link, a directory or another kind of entry",
        )
    }

    /// A refusal of `file`: `.swarmops`, the directory it would be in, is a symbolic link,
    /// which neither form of `check list` follows.
    pub(crate) fn linked_dir(file: &'static str) -> Refusal {
        Refusal::new(
            file,
            Reason::NotAFile,
            "cannot be read: .swarmops is a symbolic link, which is not followed",
        )
    }

    /// `E_CHECK_FILE`: exit 1, not retryable; `details` `file`, `reason`, `vouched`, and
    /// `line`, `key`, `bytes` and `limit` where they apply. `vouched` says whether only the
    /// caller stands behind the file.
    pub fn into_error(self, vouched: bool) -> OpError {
        // 1. One line: the file, what is wrong, the line.
        let mut message = format!("{} {}", self.file, self.what);
        if let Some(line) = self.line {
            let _ = write!(message, " (line {line})");
        }

        // 2. The details, and the hint for the file and the reason.
        let mut details = Map::new();
        details.insert("file".to_owned(), self.file.into());
        details.insert("reason".to_owned(), self.reason.as_str().into());
        details.insert("vouched".to_owned(), vouched.into());
        if let Some(line) = self.line {
            details.insert("line".to_owned(), line.into());
        }
        if let Some(key) = self.key {
            details.insert("key".to_owned(), Value::from(key));
        }
        if let Some((bytes, limit)) = self.size {
            details.insert("bytes".to_owned(), bytes.into());
            details.insert("limit".to_owned(), limit.into());
        }
        let hint = match (self.file, self.reason) {
            (CHECK_FILE, Reason::UnknownKey) => UNKNOWN_KEY_HINT,
            (CHECK_FILE, _) => CHECK_FILE_HINT,
            _ => LEGACY_HINT,
        };
        OpError::new(ErrorCode::CheckFile, one_line(message))
            .with_details(details)
            .with_hint(hint)
    }
}

/// Reads `bytes` as `.swarmops/checks.toml`, version 1: its checks sorted by name, the defaults
/// filled in. The fuzz target's entry.
pub fn parse(bytes: &[u8]) -> Result<CheckFile, Refusal> {
    // 1. Bounded, text, TOML.
    let text = text_of(CHECK_FILE, bytes, MAX_CHECK_FILE_BYTES)?;
    let document = DeTable::parse(text).map_err(|e| {
        let refusal = Refusal::new(
            CHECK_FILE,
            Reason::Syntax,
            format!("is not TOML: {}", e.message()),
        );
        match e.span() {
            Some(span) => refusal.at(line_of(bytes, span.start)),
            None => refusal,
        }
    })?;
    let document = document.get_ref();
    let lines = Lines::of(bytes);
    let line = |span: std::ops::Range<usize>| lines.line(span.start);

    // 2. The version, before any other key: a later version's keys are not this one's to judge.
    let entries = in_file_order(document);
    let version = entries.iter().find(|(key, _)| key.get_ref() == VERSION_KEY);
    let Some((_, version)) = version else {
        return Err(Refusal::new(CHECK_FILE, Reason::Version, "has no version"));
    };
    if !is_integer(version.get_ref(), CHECK_FILE_VERSION.into()) {
        let what = format!("has version {}, not 1", shown(version.get_ref()));
        let refusal = Refusal::new(CHECK_FILE, Reason::Version, what);
        return Err(refusal.at(line(version.span())).key(VERSION_KEY.to_owned()));
    }

    // 3. The other keys, in file order: `check` alone.
    let mut checks = Vec::new();
    for (key, value) in entries {
        match key.get_ref().as_ref() {
            VERSION_KEY => {}
            CHECKS_KEY => {
                let DeValue::Table(table) = value.get_ref() else {
                    let what = format!("has {CHECKS_KEY} {}, not a table", shown(value.get_ref()));
                    let refusal = Refusal::new(CHECK_FILE, Reason::Value, what);
                    return Err(refusal.at(line(key.span())).key(CHECKS_KEY.to_owned()));
                };
                checks = checks_of(&lines, table)?;
            }
            other => return Err(unknown_key(dotted(&[other]), line(key.span()))),
        }
    }
    Ok(CheckFile {
        version: CHECK_FILE_VERSION,
        checks,
    })
}

/// Reads `bytes` as `.swarmops/workspaces.json`: its `check.argv`, if it has one that is not
/// empty, as the one check [`LEGACY_CHECK`], at the root, with its `check.timeout_s` or the
/// default. None for a file without one.
///
/// Every file a workspace refuses is refused here too, since the shape and the size are a
/// workspace's. This also holds `check.argv` and `check.timeout_s` to version 1's rules, which
/// `ws land --check` does not: it runs an argv of an empty program, say, and never reads
/// `check.timeout_s`, taking its own `--check-timeout`. The timeout given here is the one a
/// later runner of the check will use.
pub fn parse_legacy(bytes: &[u8]) -> Result<Option<Check>, Refusal> {
    // 1. Bounded, text, JSON of the shape a workspace reads (unknown keys taken).
    let text = text_of(LEGACY_FILE, bytes, MAX_LEGACY_BYTES)?;
    let config: LegacyConfig = serde_json::from_str(text).map_err(|e| legacy_refusal(&e))?;

    // 2. The check, if any, under version 1's rules for argv and timeout_s.
    let Some(argv) = (config.check.map(|check| (check.argv, check.timeout_s)))
        .filter(|(argv, _)| !argv.is_empty())
    else {
        return Ok(None);
    };
    let (argv, timeout_s) = argv;
    let refused = |key: &str, what: String| {
        Refusal::new(LEGACY_FILE, Reason::Value, format!("has {key} {what}")).key(key.to_owned())
    };
    if let Err(what) = argv_rule(&argv) {
        return Err(refused("check.argv", what));
    }
    let timeout_s = timeout_s.unwrap_or(DEFAULT_CHECK_TIMEOUT_S);
    if let Err(what) = timeout_rule(timeout_s) {
        return Err(refused("check.timeout_s", what));
    }
    Ok(Some(Check {
        name: LEGACY_CHECK.to_owned(),
        argv,
        dir: ROOT_DIR.to_owned(),
        timeout_s,
        stages: vec![CheckStage::BeforeLand],
        reports: Vec::new(),
        inputs: Vec::new(),
        line: None,
    }))
}

/// The refusal of a legacy file that JSON `e` did not read: what kind of fault and where, in
/// words of its own. serde's wording would repeat the string at fault whole, up to the file's
/// 64 KiB, and its line twice.
fn legacy_refusal(e: &serde_json::Error) -> Refusal {
    use serde_json::error::Category;
    let (reason, what) = match e.classify() {
        Category::Data => (
            Reason::Value,
            "has a value of the wrong type or lacks a key, at column",
        ),
        Category::Eof => (Reason::Syntax, "is not JSON: it ends before its last value"),
        Category::Syntax | Category::Io => (Reason::Syntax, "is not JSON, at column"),
    };
    let what = match e.classify() {
        Category::Eof => what.to_owned(),
        _ => format!("{what} {}", e.column()),
    };
    let refusal = Refusal::new(LEGACY_FILE, reason, what);
    match e.line() {
        0 => refusal,
        line => refusal.at(u64::try_from(line).unwrap_or(u64::MAX)),
    }
}

/// The shape a workspace reads `.swarmops/workspaces.json` in (`swarmops-ws`'s `RepoConfig`), so
/// that a file a workspace refuses for its shape is refused here too. Unknown keys are taken.
#[derive(Deserialize)]
struct LegacyConfig {
    /// The prefixes a new workspace holds.
    #[serde(default)]
    #[allow(
        dead_code,
        reason = "read for its shape alone, as a workspace reads it"
    )]
    prefixes: Vec<String>,
    /// The command a new workspace runs.
    #[allow(
        dead_code,
        reason = "read for its shape alone, as a workspace reads it"
    )]
    bootstrap: Option<LegacyCommand>,
    /// The command `ws land --check` runs.
    check: Option<LegacyCommand>,
}

/// A command of [`LegacyConfig`].
#[derive(Deserialize)]
struct LegacyCommand {
    /// The program and its arguments.
    argv: Vec<String>,
    /// How long it may run, in seconds.
    timeout_s: Option<u64>,
}

/// Where a file's lines start, found in one pass, so that the line of each key and value is a
/// search, not a count from the file's start: a file of 1,000 checks would otherwise be read
/// 1,000 times.
pub(super) struct Lines {
    /// The offset of every `\n`, ascending.
    newlines: Vec<usize>,
}

impl Lines {
    /// The lines of `bytes`.
    pub(super) fn of(bytes: &[u8]) -> Lines {
        let newlines = (bytes.iter().enumerate())
            .filter(|(_, b)| **b == b'\n')
            .map(|(at, _)| at)
            .collect();
        Lines { newlines }
    }

    /// The 1-based line of the byte at `at`: one more than the `\n`s before it, as
    /// [`line_of`] counts.
    pub(super) fn line(&self, at: usize) -> u64 {
        let before = self.newlines.partition_point(|&newline| newline < at);
        u64::try_from(before).map_or(u64::MAX, |n| n.saturating_add(1))
    }
}

/// `bytes` as text, at most `limit` of them.
fn text_of<'b>(file: &'static str, bytes: &'b [u8], limit: u64) -> Result<&'b str, Refusal> {
    let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    if size > limit {
        return Err(Refusal::too_large(file, size, limit));
    }
    std::str::from_utf8(bytes).map_err(|e| {
        let line = line_of(bytes, e.valid_up_to());
        Refusal::new(file, Reason::NotText, "is not UTF-8").at(line)
    })
}

/// The checks of the table `check`, sorted by name.
fn checks_of(lines: &Lines, table: &DeTable<'_>) -> Result<Vec<Check>, Refusal> {
    let line = |span: std::ops::Range<usize>| lines.line(span.start);
    let mut checks = Vec::new();
    for (key, value) in in_file_order(table) {
        // 1. Within the count, a name of the grammar, a table.
        let name: &str = key.get_ref();
        let at = line(key.span());
        if checks.len() == MAX_CHECKS {
            let what = format!("has more than {MAX_CHECKS} checks");
            let refusal = Refusal::new(CHECK_FILE, Reason::TooMany, what);
            return Err(refusal.at(at).key(dotted(&[CHECKS_KEY, name])));
        }
        if !is_check_name(name) {
            let what = format!(
                "has a check name {}: 1 to {MAX_CHECK_NAME_BYTES} bytes of a-z, 0-9, - and _, \
                 starting with a letter or digit",
                quoted(name)
            );
            let refusal = Refusal::new(CHECK_FILE, Reason::Name, what);
            return Err(refusal.at(at).key(dotted(&[CHECKS_KEY, name])));
        }
        let DeValue::Table(fields) = value.get_ref() else {
            let key = dotted(&[CHECKS_KEY, name]);
            let what = format!("has {key} {}, not a table", shown(value.get_ref()));
            return Err(Refusal::new(CHECK_FILE, Reason::Value, what)
                .at(at)
                .key(key));
        };

        // 2. Its keys, in file order.
        checks.push(check_of(lines, name, at, fields)?);
    }
    checks.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(checks)
}

/// The check `name`, whose table starts at line `at` and holds `fields`.
fn check_of(lines: &Lines, name: &str, at: u64, fields: &DeTable<'_>) -> Result<Check, Refusal> {
    let line = |span: std::ops::Range<usize>| lines.line(span.start);
    let mut argv = None;
    let mut dir = None;
    let mut timeout_s = None;
    let mut stages = None;
    let mut reports = None;
    let mut inputs = None;
    for (key, value) in in_file_order(fields) {
        let field: &str = key.get_ref();
        let path = dotted(&[CHECKS_KEY, name, field]);
        let refused = |what: String, span: std::ops::Range<usize>| {
            let what = format!("has {path} {what}");
            Refusal::new(CHECK_FILE, Reason::Value, what)
                .at(line(span))
                .key(path.clone())
        };
        match field {
            "argv" => argv = Some(argv_of(value).map_err(|(what, span)| refused(what, span))?),
            "dir" => dir = Some(dir_of(value).map_err(|what| refused(what, value.span()))?),
            "timeout_s" => {
                let seconds = timeout_of(value).map_err(|what| refused(what, value.span()))?;
                timeout_s = Some(seconds);
            }
            "stages" => {
                stages = Some(stages_of(value).map_err(|(what, span)| refused(what, span))?);
            }
            "reports" => reports = Some(reports_of(lines, name, value)?),
            "inputs" => {
                inputs = Some(inputs_of(value).map_err(|(what, span)| refused(what, span))?);
            }
            _ => return Err(unknown_key(path, line(key.span()))),
        }
    }
    let Some(argv) = argv else {
        let path = dotted(&[CHECKS_KEY, name, "argv"]);
        let what = format!("has no {path}: a check is one command");
        return Err(Refusal::new(CHECK_FILE, Reason::Value, what)
            .at(at)
            .key(path));
    };
    Ok(Check {
        name: name.to_owned(),
        argv,
        dir: dir.unwrap_or_else(|| ROOT_DIR.to_owned()),
        timeout_s: timeout_s.unwrap_or(DEFAULT_CHECK_TIMEOUT_S),
        stages: stages.unwrap_or_else(|| vec![CheckStage::BeforeLand]),
        reports: reports.unwrap_or_default(),
        inputs: inputs.unwrap_or_default(),
        line: Some(at),
    })
}

/// The reports of check `name`'s `reports`, in declared order, or the refusal of the first
/// fault: not an array, empty, more than [`MAX_CHECK_REPORTS`], an item that is not a table, a
/// table without `format` or `path` or with another key, a format that is not one a check may
/// declare, a path that is not a repository path or given twice (at the second).
fn reports_of(
    lines: &Lines,
    name: &str,
    value: &Spanned<DeValue<'_>>,
) -> Result<Vec<Report>, Refusal> {
    let line = |span: std::ops::Range<usize>| lines.line(span.start);
    let refused = |field: Option<&str>, what: String, span: std::ops::Range<usize>| {
        let key = match field {
            Some(field) => dotted(&[CHECKS_KEY, name, "reports", field]),
            None => dotted(&[CHECKS_KEY, name, "reports"]),
        };
        let what = format!("has {key} {what}");
        Refusal::new(CHECK_FILE, Reason::Value, what)
            .at(line(span))
            .key(key)
    };

    // 1. An array of 1 to MAX_CHECK_REPORTS items.
    let DeValue::Array(items) = value.get_ref() else {
        let what = format!("{}, not an array of report tables", shown(value.get_ref()));
        return Err(refused(None, what, value.span()));
    };
    if items.is_empty() {
        let what = "empty: declare one report or more, or leave the key out".to_owned();
        return Err(refused(None, what, value.span()));
    }
    let mut reports: Vec<Report> = Vec::with_capacity(items.len().min(MAX_CHECK_REPORTS));
    for item in items {
        if reports.len() == MAX_CHECK_REPORTS {
            let what = format!("holding more than {MAX_CHECK_REPORTS} reports");
            return Err(refused(None, what, item.span()));
        }

        // 2. Each a table of `format` and `path`, in file order.
        let DeValue::Table(fields) = item.get_ref() else {
            let what = format!("holding {}, not a table", shown(item.get_ref()));
            return Err(refused(None, what, item.span()));
        };
        let mut format = None;
        let mut path = None;
        for (key, value) in in_file_order(fields) {
            let field: &str = key.get_ref();
            match field {
                "format" => {
                    let word = match value.get_ref() {
                        DeValue::String(word) => Some(word.as_ref()),
                        _ => None,
                    };
                    let known = (CheckReportFormat::DECLARED.into_iter())
                        .find(|format| Some(format.as_str()) == word);
                    let Some(known) = known else {
                        let what = format!(
                            "{}, not a report format: junit or sarif",
                            shown(value.get_ref())
                        );
                        return Err(refused(Some("format"), what, value.span()));
                    };
                    format = Some(known);
                }
                "path" => {
                    let DeValue::String(text) = value.get_ref() else {
                        let what = format!("{}, not a string", shown(value.get_ref()));
                        return Err(refused(Some("path"), what, value.span()));
                    };
                    let Ok(repo_path) = RepoPath::new(text.as_ref()) else {
                        let what = format!(
                            "{}, not a repository path below the check's dir",
                            quoted(text)
                        );
                        return Err(refused(Some("path"), what, value.span()));
                    };
                    if reports
                        .iter()
                        .any(|report| report.path == repo_path.as_str())
                    {
                        let what = format!("{} twice", quoted(text));
                        return Err(refused(Some("path"), what, value.span()));
                    }
                    path = Some(repo_path.as_str().to_owned());
                }
                _ => {
                    let path = dotted(&[CHECKS_KEY, name, "reports", field]);
                    return Err(unknown_key(path, line(key.span())));
                }
            }
        }

        // 3. Both given.
        let (Some(format), Some(path)) = (format, path) else {
            let field = if format.is_none() { "format" } else { "path" };
            let key = dotted(&[CHECKS_KEY, name, "reports", field]);
            let what = format!("has no {key}: a report names its format and its path");
            return Err(Refusal::new(CHECK_FILE, Reason::Value, what)
                .at(line(item.span()))
                .key(key));
        };
        reports.push(Report::new(format, path));
    }
    Ok(reports)
}

/// `stages`' stages, in their own order, or what is wrong with them and where: not an array,
/// empty, an item that is not a string or not a stage, or a stage given twice (at the second).
fn stages_of(
    value: &Spanned<DeValue<'_>>,
) -> Result<Vec<CheckStage>, (String, std::ops::Range<usize>)> {
    let DeValue::Array(items) = value.get_ref() else {
        let what = format!("{}, not an array of stages", shown(value.get_ref()));
        return Err((what, value.span()));
    };
    if items.is_empty() {
        let what = "empty: a check names at least one stage".to_owned();
        return Err((what, value.span()));
    }
    let mut stages = Vec::with_capacity(CheckStage::ALL.len());
    for item in items {
        let DeValue::String(text) = item.get_ref() else {
            let what = format!("holds {}, not a string", shown(item.get_ref()));
            return Err((what, item.span()));
        };
        let Some(stage) = CheckStage::parse(text) else {
            let known: Vec<&str> = CheckStage::ALL.iter().map(|s| s.as_str()).collect();
            let what = format!(
                "holds {}, not a stage: one of {}",
                quoted(text),
                known.join(", ")
            );
            return Err((what, item.span()));
        };
        if stages.contains(&stage) {
            let what = format!("holds {} twice", quoted(text));
            return Err((what, item.span()));
        }
        stages.push(stage);
    }
    stages.sort();
    Ok(stages)
}

/// `inputs`' patterns, sorted bytewise, or what is wrong with them and where: not an array,
/// empty, an item that is not a string, or what [`inputs::parse`] refuses (a pattern outside the
/// grammar or its bounds, one given twice, one past [`MAX_CHECK_INPUTS`]), at the item's line.
/// The first fault in file order is the one named.
fn inputs_of(
    value: &Spanned<DeValue<'_>>,
) -> Result<Vec<String>, (String, std::ops::Range<usize>)> {
    let DeValue::Array(items) = value.get_ref() else {
        let what = format!("{}, not an array of path patterns", shown(value.get_ref()));
        return Err((what, value.span()));
    };
    if items.is_empty() {
        let what = "empty: declare one pattern or more, or leave the key out for the whole tree";
        return Err((what.to_owned(), value.span()));
    }

    // 1. The strings, up to the first item that is not one, and no further than the first past
    //    the count, which is refused whatever follows it.
    let mut patterns = Vec::with_capacity(items.len().min(MAX_CHECK_INPUTS + 1));
    let mut other = None;
    for item in items.iter().take(MAX_CHECK_INPUTS + 1) {
        let DeValue::String(text) = item.get_ref() else {
            other = Some(item);
            break;
        };
        patterns.push(text.to_string());
    }

    // 2. Their rules, a pattern at fault before the item after it that is no string.
    if let Err((at, what)) = inputs::parse(&patterns) {
        let span = items.get(at).map_or_else(|| value.span(), Spanned::span);
        return Err((what, span));
    }
    if let Some(item) = other {
        let what = format!("holds {}, not a string", shown(item.get_ref()));
        return Err((what, item.span()));
    }
    patterns.sort();
    Ok(patterns)
}

/// `argv`'s strings, or what is wrong with them and where.
fn argv_of(value: &Spanned<DeValue<'_>>) -> Result<Vec<String>, (String, std::ops::Range<usize>)> {
    let DeValue::Array(items) = value.get_ref() else {
        let what = format!("{}, not an array of strings", shown(value.get_ref()));
        return Err((what, value.span()));
    };
    let mut argv = Vec::with_capacity(items.len().min(MAX_ARGV_ITEMS));
    for item in items {
        let DeValue::String(text) = item.get_ref() else {
            let what = format!("holds {}, not a string", shown(item.get_ref()));
            return Err((what, item.span()));
        };
        argv.push(text.to_string());
    }
    argv_rule(&argv).map_err(|what| (what, value.span()))?;
    Ok(argv)
}

/// Whether `argv` keeps version 1's rule: 1 to [`MAX_ARGV_ITEMS`] strings of at most
/// [`MAX_ARGV_BYTES`] together, none holding a NUL, the program not empty.
fn argv_rule(argv: &[String]) -> Result<(), String> {
    let bytes: usize = argv.iter().map(String::len).sum();
    match argv.first() {
        None => Err("empty: a check names its program".to_owned()),
        Some(_) if argv.len() > MAX_ARGV_ITEMS => Err(format!(
            "of {} strings, more than {MAX_ARGV_ITEMS}",
            argv.len()
        )),
        Some(_) if u64::try_from(bytes).unwrap_or(u64::MAX) > MAX_ARGV_BYTES => {
            Err(format!("of {bytes} bytes, more than {MAX_ARGV_BYTES}"))
        }
        Some(_) if argv.iter().any(|arg| arg.contains('\0')) => {
            Err("holding a NUL byte".to_owned())
        }
        Some(program) if program.is_empty() => Err("with an empty program".to_owned()),
        Some(_) => Ok(()),
    }
}

/// `dir`'s directory, or what is wrong with it.
fn dir_of(value: &Spanned<DeValue<'_>>) -> Result<String, String> {
    let DeValue::String(text) = value.get_ref() else {
        return Err(format!("{}, not a string", shown(value.get_ref())));
    };
    if text.as_ref() == ROOT_DIR {
        return Ok(ROOT_DIR.to_owned());
    }
    RepoPath::new(text.as_ref())
        .map(|path| path.as_str().to_owned())
        .map_err(|e| format!("{}, not . or a repository path: {e}", quoted(text)))
}

/// `timeout_s`'s seconds, or what is wrong with them.
fn timeout_of(value: &Spanned<DeValue<'_>>) -> Result<u64, String> {
    let seconds = match value.get_ref() {
        DeValue::Integer(integer) => u64::from_str_radix(integer.as_str(), integer.radix()).ok(),
        _ => None,
    };
    let Some(seconds) = seconds else {
        return Err(format!(
            "{}, not a whole number of seconds from 1 to {MAX_CHECK_TIMEOUT_S}",
            shown(value.get_ref())
        ));
    };
    timeout_rule(seconds).map(|()| seconds)
}

/// Whether `seconds` is from 1 to [`MAX_CHECK_TIMEOUT_S`].
fn timeout_rule(seconds: u64) -> Result<(), String> {
    if (1..=MAX_CHECK_TIMEOUT_S).contains(&seconds) {
        return Ok(());
    }
    Err(format!(
        "{seconds}, not a whole number of seconds from 1 to {MAX_CHECK_TIMEOUT_S}"
    ))
}

/// Whether `name` is a check's name: 1 to [`MAX_CHECK_NAME_BYTES`] bytes of `a-z`, `0-9`, `-`
/// and `_`, the first a letter or digit.
pub(super) fn is_check_name(name: &str) -> bool {
    let allowed = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_';
    let first = name.bytes().next();
    first.is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && name.len() <= MAX_CHECK_NAME_BYTES
        && name.bytes().all(allowed)
}

/// Whether `value` is the integer `expected`, written in any base TOML takes.
fn is_integer(value: &DeValue<'_>, expected: u64) -> bool {
    match value {
        DeValue::Integer(integer) => {
            u64::from_str_radix(integer.as_str(), integer.radix()).ok() == Some(expected)
        }
        _ => false,
    }
}

/// `unknown_key` for the key at `path`, on `line`.
fn unknown_key(path: String, line: u64) -> Refusal {
    let what = format!("has an unknown key {path}");
    Refusal::new(CHECK_FILE, Reason::UnknownKey, what)
        .at(line)
        .key(path)
}

/// The entries of `table` in the order the file holds them.
fn in_file_order<'t, 'i>(
    table: &'t DeTable<'i>,
) -> Vec<(
    &'t Spanned<std::borrow::Cow<'i, str>>,
    &'t Spanned<DeValue<'i>>,
)> {
    let mut entries: Vec<_> = table.iter().collect();
    entries.sort_by_key(|(key, _)| key.span().start);
    entries
}

/// The dotted path of `keys`: a bare key as it is, any other quoted; each cut as [`clipped`]
/// cuts it.
fn dotted(keys: &[&str]) -> String {
    let bare = |key: &&str| {
        !key.is_empty()
            && (key.bytes()).all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    };
    let parts: Vec<String> = (keys.iter())
        .map(|key| {
            if bare(key) {
                as_written(key)
            } else {
                quoted(key)
            }
        })
        .collect();
    parts.join(".")
}

/// The most characters of a string, a key or a number that a refusal repeats, escapes counted.
const SHOWN: usize = 64;

/// The start of `text` that a refusal repeats: as many characters as fit in [`SHOWN`] once
/// escaped as a quoted string escapes them; and whether that is less than all of it.
fn clipped(text: &str) -> (&str, bool) {
    let mut width = 0;
    for (at, c) in text.char_indices() {
        width += c.escape_debug().count();
        if width > SHOWN {
            return (&text[..at], true);
        }
    }
    (text, false)
}

/// `text` as written, at most [`SHOWN`] characters of it, `…` after a cut.
fn as_written(text: &str) -> String {
    match clipped(text) {
        (shown, true) => format!("{shown}…"),
        (shown, false) => shown.to_owned(),
    }
}

/// `text` quoted, its control characters escaped: at most [`SHOWN`] characters of it, escapes
/// counted, `…` after a cut.
pub(super) fn quoted(text: &str) -> String {
    match clipped(text) {
        (shown, true) => format!("{shown:?}…"),
        (shown, false) => format!("{shown:?}"),
    }
}

/// What `value` is, for a refusal: a string quoted, a number or boolean as written, else its
/// kind; a string or number cut as [`clipped`] cuts it, never repeated whole.
fn shown(value: &DeValue<'_>) -> String {
    match value {
        DeValue::String(text) => quoted(text),
        DeValue::Integer(integer) => as_written(&integer.to_string()),
        DeValue::Float(float) => as_written(&float.to_string()),
        DeValue::Boolean(b) => b.to_string(),
        DeValue::Datetime(_) => "a date".to_owned(),
        DeValue::Array(_) => "an array".to_owned(),
        DeValue::Table(_) => "a table".to_owned(),
    }
}
