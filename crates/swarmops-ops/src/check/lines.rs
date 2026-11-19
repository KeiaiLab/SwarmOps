//! Run lines (M31-11): one line of diagnostics for each check a run of `check run` reaches,
//! appended to a file on the host that ran it.
//!
//! ```text
//!   where:  --run-lines DIR ─ else SWARMOPS_CHECK_RUN_LINES_DIR (absolute) ─ else the form's
//!           own place: the workspace's .swarmws/check-runs/, or the directory's runs
//!                       │
//!                       ▼
//!   <dir>/run-lines.jsonl ◀─ one CheckRunLine a check, one O_APPEND write, unsynced
//!          └─ past 64 MiB: renamed to run-lines.1.jsonl under run-lines.lock (crate::records)
//! ```
//!
//! Diagnostics: nothing decides from a line, and a line that cannot be appended is a warning,
//! once a run, which changes no answer. The append takes no lock of a store and happens once the
//! check's process group is gone.

use std::ffi::OsStr;
use std::io;
use std::os::unix::fs::DirBuilderExt as _;

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_api::{CheckOutcome, CheckRunLine, CheckRunOut, CheckStage, ErrorCode, OpError};

use super::file::is_check_name;
use super::run::RUNS_DIR_MODE;
use crate::error;
use crate::records::{self, Files};

/// The most bytes the run lines' file holds before it is renamed to the one older file: 64 MiB,
/// as the operation records'.
pub const MAX_RUN_LINES_BYTES: u64 = 64 << 20;

/// The most bytes of one run line, its line end included: 4 KiB, what one append writes whole.
pub const MAX_RUN_LINE_BYTES: usize = records::MAX_LINE;

/// The most bytes of the run directory a line names; a longer one is left out of its line.
pub const MAX_RUN_LINE_DIR_BYTES: usize = 1_024;

/// The variable that names the directory the run lines go to, when `--run-lines` does not.
pub const RUN_LINES_DIR_ENV: &str = "SWARMOPS_CHECK_RUN_LINES_DIR";

/// The run lines' files, in their directory.
pub(crate) const RUN_LINES: Files = Files {
    current: RUN_LINES_FILE,
    older: RUN_LINES_OLDER_FILE,
    lock: "run-lines.lock",
};

/// The file the run lines are appended to.
pub const RUN_LINES_FILE: &str = "run-lines.jsonl";

/// The one older file, which a rotation replaces.
pub const RUN_LINES_OLDER_FILE: &str = "run-lines.1.jsonl";

/// The hex digits of a tree's id.
const TREE_HEX: usize = 64;

/// The directory a caller named for the run lines: `flag` (`--run-lines`), relative to the
/// working directory and made absolute; else `var`, the value of [`RUN_LINES_DIR_ENV`], which
/// must be absolute; none when neither is given, and the lines go to the form's own place.
///
/// # Errors
///
/// `E_INVALID_ARG` with `details.reason` `run_lines_dir` for a variable that is not an absolute
/// UTF-8 path, or a flag that cannot be made one.
pub fn named(flag: Option<&Utf8Path>, var: Option<&OsStr>) -> Result<Option<Utf8PathBuf>, OpError> {
    // 1. The flag, made absolute.
    if let Some(flag) = flag {
        let absolute = std::path::absolute(flag)
            .ok()
            .and_then(|path| Utf8PathBuf::from_path_buf(path).ok());
        return absolute.map(Some).ok_or_else(|| {
            refused_dir(format!(
                "--run-lines {:?} names no directory",
                flag.as_str()
            ))
        });
    }

    // 2. The variable, which must be absolute.
    let Some(var) = var else {
        return Ok(None);
    };
    let shown = var.to_string_lossy();
    match var.to_str().map(Utf8Path::new) {
        Some(dir) if dir.is_absolute() => Ok(Some(dir.to_owned())),
        _ => Err(refused_dir(format!(
            "{RUN_LINES_DIR_ENV} is {shown:?}: give an absolute directory"
        ))
        .with_hint(format!(
            "name an absolute directory, such as {RUN_LINES_DIR_ENV}=\"$PWD/{shown}\", or unset \
             it for the run lines to go beside the runs"
        ))),
    }
}

/// `E_INVALID_ARG` saying `what` of the run lines' directory, with `details.reason`
/// `run_lines_dir`.
pub(crate) fn refused_dir(what: String) -> OpError {
    let mut error = error::error(ErrorCode::InvalidArg, error::one_line(what));
    error
        .details
        .insert("reason".to_owned(), "run_lines_dir".into());
    error
}

/// Where a run's lines go, and whether an append to it has failed in this run.
#[derive(Debug)]
#[non_exhaustive]
pub struct Sink {
    /// The directory of the files.
    dir: Utf8PathBuf,
    /// Whether the caller named it ([`named`]); else it is the form's own place.
    named: bool,
    /// Whether an append failed: no later one is tried.
    failed: bool,
}

impl Sink {
    /// The lines' directory: the one the caller `named`, else `own`, the form's own place.
    pub fn resolve(named: Option<&Utf8Path>, own: &Utf8Path) -> Sink {
        Sink {
            dir: named.unwrap_or(own).to_owned(),
            named: named.is_some(),
            failed: false,
        }
    }

    /// The directory of the files.
    pub fn dir(&self) -> &Utf8Path {
        &self.dir
    }

    /// Whether the caller named the directory.
    pub fn is_named(&self) -> bool {
        self.named
    }

    /// Appends `line`. A named directory that is not there is made first, one level, its
    /// owner's alone; one that is there is used as it is. The first append that fails gives the
    /// warning to say, in one line; after it nothing is tried, and nothing more is said.
    pub fn append(&mut self, line: &CheckRunLine) -> Option<String> {
        if self.failed {
            return None;
        }
        let made = if self.named {
            make_named(&self.dir)
        } else {
            Ok(())
        };
        let appended = made.and_then(|()| append_within(&self.dir, line, MAX_RUN_LINES_BYTES));
        let e = appended.err()?;
        self.failed = true;
        let file = self.dir.join(RUN_LINES_FILE);
        Some(error::one_line(format!("{file}: {e}")))
    }
}

/// Makes the directory `dir` a caller named, if nothing is there: one level, readable by its
/// owner alone, as a run's directories are. What is there already is left as it is.
fn make_named(dir: &Utf8Path) -> io::Result<()> {
    match std::fs::DirBuilder::new().mode(RUNS_DIR_MODE).create(dir) {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => Err(e),
        _ => Ok(()),
    }
}

/// Appends `line` to the run lines in `dir`, rotating past `bound` bytes.
pub(crate) fn append_within(dir: &Utf8Path, line: &CheckRunLine, bound: u64) -> io::Result<()> {
    records::append(dir.as_std_path(), &RUN_LINES, line, bound)
}

/// The run a line is of: what every check of it shares.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Of<'a> {
    /// The run's id.
    pub(crate) run: &'a str,
    /// The stage run.
    pub(crate) stage: CheckStage,
    /// Whether only the caller stands behind the files.
    pub(crate) vouched: bool,
    /// The tree, in the workspace form.
    pub(crate) tree: Option<&'a str>,
    /// The workspace's name, in the workspace form.
    pub(crate) workspace: Option<&'a str>,
    /// The run's directory.
    pub(crate) run_dir: &'a Utf8Path,
}

/// What the runner measured of one check, which its result does not carry.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Times {
    /// When the check started, or was tried: Unix milliseconds.
    pub(crate) at_ms: u64,
    /// The milliseconds from the run's start to the check's.
    pub(crate) wait_ms: u64,
    /// The peak of its group's sampled resident memory, in KiB.
    pub(crate) peak_rss_kib: Option<u64>,
}

/// The line of the check that ended as `out`, of the run `of`, measured as `times`. A run
/// directory past [`MAX_RUN_LINE_DIR_BYTES`] is left out.
pub(crate) fn line_of(of: &Of<'_>, out: &CheckRunOut, times: &Times) -> CheckRunLine {
    let mut line = CheckRunLine::new(times.at_ms, of.run, &out.name, of.stage, out.outcome);
    line.tree = of.tree.map(str::to_owned);
    line.vouched = of.vouched;
    line.workspace = of.workspace.map(str::to_owned);
    line.definition_digest.clone_from(&out.definition_digest);
    line.wait_ms = times.wait_ms;
    line.duration_ms = out.duration_ms;
    line.exit_code = out.exit_code;
    line.findings = out.findings_total;
    line.peak_rss_kib = times.peak_rss_kib;
    let dir = of.run_dir.as_str();
    line.run_dir = (dir.len() <= MAX_RUN_LINE_DIR_BYTES).then(|| dir.to_owned());
    line
}

/// Whether `line` holds what a run writes: a run that is a UUID as a run's id is written, a
/// check's name, a tree of 64 lowercase hex digits if any, an outcome other than `not_run`, a
/// run directory that is absolute and within [`MAX_RUN_LINE_DIR_BYTES`] if any, and no control
/// character in the workspace's name. A reader skips a line that fails it: the file may be
/// another account's.
pub fn valid(line: &CheckRunLine) -> bool {
    let hex = |b: u8| b.is_ascii_digit() || (b'a'..=b'f').contains(&b);
    let tree = |tree: &String| tree.len() == TREE_HEX && tree.bytes().all(hex);
    let dir =
        |dir: &String| dir.len() <= MAX_RUN_LINE_DIR_BYTES && Utf8Path::new(dir).is_absolute();
    is_run_id(&line.run)
        && is_check_name(&line.check)
        && line.tree.as_ref().is_none_or(tree)
        && line.outcome != CheckOutcome::NotRun
        && line.run_dir.as_ref().is_none_or(dir)
        && (line.workspace.as_ref()).is_none_or(|name| !name.contains(char::is_control))
}

/// Whether `run` is a run's id as a run writes it: a UUID, hyphenated, in lowercase.
pub(crate) fn is_run_id(run: &str) -> bool {
    uuid::Uuid::try_parse(run).is_ok_and(|id| id.hyphenated().to_string() == run)
}

#[cfg(test)]
mod tests;
