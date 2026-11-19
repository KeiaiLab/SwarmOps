//! `check runs` (M31-12): the run lines of a directory or a workspace, newest first, filtered, a
//! page at a time.
//!
//! ```text
//!   <dir>/run-lines.1.jsonl ─┐  both opened before either is read; each a stream of lines of
//!   <dir>/run-lines.jsonl ───┤  at most 4 KiB, a longer one skipped to its end, never held
//!                            ▼
//!   a line that reads (UTF-8, JSON, a CheckRunLine, lines::valid) ─▶ the filters ─▶ below the
//!   cursor's key ─▶ the limit + 1 greatest keys (at, run, check, attempt), in an ordered map
//!                            ▼
//!   CheckRunsResp {runs: the first limit of them, newest first; cursor: r1.<at>.<run>.<attempt>
//!                  .<check> of the last, when one more matched; scanned; skipped}
//! ```
//!
//! The order is the key's, not the files': lines appended by several runs at once, a rotation
//! and a second reading change no page. A cursor names a place in that order, so a page after
//! lines were appended or rotated away repeats nothing and loses nothing that is still kept.
//! Memory is the page: one line and at most `limit + 1` lines are held, never a file.
//!
//! Reads only: no lock is taken, nothing is written, no store operation is called. The lines
//! are diagnostics, which nothing decides from.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader};
use std::os::unix::fs::OpenOptionsExt as _;

use camino::Utf8Path;
use swarmops_api::{
    CheckOutcome, CheckRunLine, CheckRunsReq, CheckRunsResp, ErrorCode, MAX_CHECK_RUNS_LIMIT,
    OpError,
};
use swarmops_ws::registry::Registry;
use swarmops_ws::workspace::{self, Locate};

use super::file::is_check_name;
use super::follow::dir_runs;
use super::lines::{self, MAX_RUN_LINE_BYTES, RUN_LINES, Sink};
use super::run::WS_RUNS_DIR;
use crate::error;

#[cfg(test)]
use swarmops_api::DEFAULT_CHECK_RUNS_LIMIT;

/// The first part of every cursor: its format.
const CURSOR_FORMAT: &str = "r1";

/// The most bytes of a cursor.
const MAX_CURSOR_BYTES: usize = 256;

/// The fewest hex digits a tree filter gives.
const MIN_TREE_HEX: usize = 12;

/// The most hex digits a tree filter gives: a whole tree id.
const MAX_TREE_HEX: usize = 64;

/// What orders the lines: when the check started, its run, its name, its attempt.
type Key = (u64, String, String, u32);

/// The key of `line`.
fn key_of(line: &CheckRunLine) -> Key {
    (line.at, line.run.clone(), line.check.clone(), line.attempt)
}

/// A place in the order of the lines: the key of the last line a page gave.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Cursor {
    /// When that line's check started.
    at: u64,
    /// Its run.
    run: String,
    /// Its attempt.
    attempt: u32,
    /// Its check.
    check: String,
}

impl Cursor {
    /// The cursor `text` writes: `r1.<at>.<run>.<attempt>.<check>`.
    ///
    /// # Errors
    ///
    /// `E_INVALID_ARG` with `details.reason` `cursor` for anything else.
    pub fn parse(text: &str) -> Result<Cursor, OpError> {
        Cursor::read(text).ok_or_else(|| {
            invalid(
                "the cursor is not one a page of check runs gave".to_owned(),
                "cursor",
            )
            .with_hint("pass a cursor back as it was returned, or leave it out for the first page")
        })
    }

    /// The cursor `text` writes, if it writes one.
    fn read(text: &str) -> Option<Cursor> {
        if text.len() > MAX_CURSOR_BYTES {
            return None;
        }
        let digits = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
        let mut parts = text.split('.');
        let format = parts.next()?;
        let (at, run, attempt, check) =
            (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
        let whole = format == CURSOR_FORMAT && parts.next().is_none();
        if !whole || !digits(at) || !digits(attempt) {
            return None;
        }
        if !lines::is_run_id(run) || !is_check_name(check) {
            return None;
        }
        Some(Cursor {
            at: at.parse().ok()?,
            run: run.to_owned(),
            attempt: attempt.parse().ok()?,
            check: check.to_owned(),
        })
    }

    /// The cursor that names `line`.
    fn of(line: &CheckRunLine) -> Cursor {
        Cursor {
            at: line.at,
            run: line.run.clone(),
            attempt: line.attempt,
            check: line.check.clone(),
        }
    }

    /// The key it names.
    fn key(&self) -> Key {
        (self.at, self.run.clone(), self.check.clone(), self.attempt)
    }
}

impl fmt::Display for Cursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Cursor {
            at,
            run,
            attempt,
            check,
        } = self;
        write!(f, "{CURSOR_FORMAT}.{at}.{run}.{attempt}.{check}")
    }
}

/// Which lines a page takes: those that match every filter given, below the cursor's key.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Filter {
    /// Only these checks; every check when empty.
    checks: Vec<String>,
    /// Only trees that start with these hex digits.
    tree: Option<String>,
    /// Only these outcomes; every outcome when empty.
    outcomes: Vec<CheckOutcome>,
    /// Only this run.
    run: Option<String>,
    /// Only lines at or after this time.
    since: Option<u64>,
    /// Only lines before this time.
    until: Option<u64>,
    /// Only lines whose key is below this one: the cursor's.
    below: Option<Key>,
}

impl Filter {
    /// The filter and the page's limit `req` asks for.
    ///
    /// # Errors
    ///
    /// `E_INVALID_ARG` with `details.reason`: `limit` (not 1 to 1,000), `check` (not a check's
    /// name), `tree` (not 12 to 64 lowercase hex digits), `outcome` (`not_run`, which leaves no
    /// line), `run` (not a run's id), `time` (`since` after `until`), `cursor`.
    pub fn of(req: &CheckRunsReq) -> Result<(Filter, usize), OpError> {
        // 1. The page's size.
        if !(1..=MAX_CHECK_RUNS_LIMIT).contains(&req.limit) {
            let what = format!(
                "the limit is 1 to {MAX_CHECK_RUNS_LIMIT}, not {}",
                req.limit
            );
            return Err(invalid(what, "limit"));
        }

        // 2. Each filter, by its own rule.
        if let Some(name) = req.checks.iter().find(|name| !is_check_name(name)) {
            return Err(invalid(format!("{name:?} is not a check's name"), "check"));
        }
        let hex = |b: u8| b.is_ascii_digit() || (b'a'..=b'f').contains(&b);
        if let Some(tree) = &req.tree
            && !((MIN_TREE_HEX..=MAX_TREE_HEX).contains(&tree.len()) && tree.bytes().all(hex))
        {
            let what = format!(
                "{tree:?} is not a tree: give {MIN_TREE_HEX} to {MAX_TREE_HEX} lowercase hex \
                 digits of its id"
            );
            return Err(invalid(what, "tree"));
        }
        if req.outcomes.contains(&CheckOutcome::NotRun) {
            let what = "a check that was not run leaves no run line".to_owned();
            return Err(invalid(what, "outcome").with_hint(
                "the outcomes are passed, failed, timed_out, cancelled and not_started",
            ));
        }
        if let Some(run) = (req.run.as_ref()).filter(|run| !lines::is_run_id(run)) {
            return Err(invalid(format!("{run:?} is not a run's id"), "run"));
        }
        if let (Some(since), Some(until)) = (req.since, req.until)
            && since > until
        {
            let what = format!("since ({since}) is after until ({until})");
            return Err(invalid(what, "time"));
        }

        // 3. Where the page starts.
        let cursor = req.cursor.as_deref().map(Cursor::parse).transpose()?;
        let filter = Filter {
            checks: req.checks.clone(),
            tree: req.tree.clone(),
            outcomes: req.outcomes.clone(),
            run: req.run.clone(),
            since: req.since,
            until: req.until,
            below: cursor.as_ref().map(Cursor::key),
        };
        Ok((filter, req.limit))
    }

    /// Whether `line` matches every filter given.
    fn takes(&self, line: &CheckRunLine) -> bool {
        let tree = |prefix: &String| (line.tree.as_ref()).is_some_and(|t| t.starts_with(prefix));
        (self.checks.is_empty() || self.checks.contains(&line.check))
            && self.tree.as_ref().is_none_or(tree)
            && (self.outcomes.is_empty() || self.outcomes.contains(&line.outcome))
            && self.run.as_ref().is_none_or(|run| *run == line.run)
            && self.since.is_none_or(|since| since <= line.at)
            && self.until.is_none_or(|until| line.at < until)
    }
}

/// One page of run lines, as [`page`] reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Page {
    /// The lines, newest first.
    pub runs: Vec<CheckRunLine>,
    /// The cursor of the next page, when one more line matched.
    pub cursor: Option<String>,
    /// The lines read, matching or not.
    pub scanned: u64,
    /// Those of them that did not read as a run line.
    pub skipped: u64,
}

/// The page of at most `limit` lines that `filter` takes from the lines of `older`, then of
/// `current`: the greatest keys below the filter's cursor, newest first. A key the filter takes
/// twice is one line, the first read. A line that does not read is skipped and counted: longer than
/// [`MAX_RUN_LINE_BYTES`] (never held), not UTF-8, not a [`CheckRunLine`], of values a run does
/// not write ([`lines::valid`]), or the last one, with no line end. A read that fails ends its
/// file, counted as one such line.
pub fn page(older: impl BufRead, current: impl BufRead, filter: &Filter, limit: usize) -> Page {
    // 1. One pass over each file: the limit + 1 greatest keys that match.
    let mut kept: BTreeMap<Key, CheckRunLine> = BTreeMap::new();
    let (mut scanned, mut skipped) = (0_u64, 0_u64);
    let mut take = |read: Option<&[u8]>| {
        scanned += 1;
        let line = read
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .and_then(|text| serde_json::from_str::<CheckRunLine>(text).ok())
            .filter(lines::valid);
        let Some(line) = line else {
            skipped += 1;
            return;
        };
        let key = key_of(&line);
        if !filter.takes(&line) || filter.below.as_ref().is_some_and(|below| key >= *below) {
            return;
        }
        kept.entry(key).or_insert(line);
        if kept.len() > limit.saturating_add(1) {
            kept.pop_first();
        }
    };
    each_line(older, &mut take);
    each_line(current, &mut take);

    // 2. The first `limit` of them, newest first; a cursor when one more matched.
    let more = kept.len() > limit;
    let runs: Vec<CheckRunLine> = kept.into_values().rev().take(limit).collect();
    let cursor = (runs.last().filter(|_| more)).map(|last| Cursor::of(last).to_string());
    Page {
        runs,
        cursor,
        scanned,
        skipped,
    }
}

/// Gives `take` each line of `reader`, without its line end: the bytes of a whole line of at
/// most [`MAX_RUN_LINE_BYTES`], its end included; none for a longer one, which is passed over to
/// its end without being held, for a last line with no end, and for a read that fails, which
/// ends the file.
fn each_line(mut reader: impl BufRead, take: &mut impl FnMut(Option<&[u8]>)) {
    let mut line: Vec<u8> = Vec::new();
    let mut long = false;
    loop {
        // 1. What the reader holds next: nothing at the end, where a line with no end is one
        //    more that does not read.
        let held = match reader.fill_buf() {
            Ok(held) if !held.is_empty() => held,
            end => {
                if end.is_err() || long || !line.is_empty() {
                    take(None);
                }
                return;
            }
        };

        // 2. Up to the line's end, if it is there; kept only while the line fits.
        let end = held.iter().position(|b| *b == b'\n');
        let part = &held[..end.unwrap_or(held.len())];
        if !long && line.len() + part.len() < MAX_RUN_LINE_BYTES {
            line.extend_from_slice(part);
        } else {
            long = true;
            line.clear();
        }
        debug_assert!(
            line.len() <= MAX_RUN_LINE_BYTES,
            "a line held past its bound"
        );
        let used = end.map_or(held.len(), |end| end + 1);
        reader.consume(used);

        // 3. A whole line, given.
        if end.is_some() {
            take((!long).then_some(line.as_slice()));
            line.clear();
            long = false;
        }
    }
}

/// `check runs --dir`: a page of the run lines of the directory `dir`, read from the directory
/// the caller `named` ([`lines::named`]), else from beside its runs below `runs_root`. Vouched:
/// only the caller stands behind a directory's lines. `req.workspace` is not read.
///
/// # Errors
///
/// `E_INVALID_ARG` for a `dir` that is no directory, a host with no runs root, or a request
/// that does not read ([`Filter::of`]); `E_IO` for a lines file that cannot be read.
pub fn runs_dir(
    dir: &Utf8Path,
    runs_root: Option<&Utf8Path>,
    named: Option<&Utf8Path>,
    req: &CheckRunsReq,
) -> Result<CheckRunsResp, OpError> {
    let own = match named {
        Some(named) => named.to_owned(),
        None => dir_runs(dir, runs_root)?,
    };
    runs_at(&own, true, req)
}

/// `check_runs`: a page of the run lines of workspace `req.workspace`, found through the
/// registry beside the store, read from the directory the caller `named`, else from the
/// workspace's own state. Reads only.
///
/// # Errors
///
/// As [`runs_dir`], and the workspace's own for a name the registry does not hold.
pub fn runs_ws(
    ctx: &swarmops_ws::Ctx<'_>,
    req: &CheckRunsReq,
    named: Option<&Utf8Path>,
) -> Result<CheckRunsResp, OpError> {
    let registry = Registry::open(&ctx.store_root, ctx.fs.clone());
    let locate = Locate::Name(req.workspace.clone());
    let found = workspace::find(&locate, Some(&registry), ctx.fs.as_ref())?;
    let sink = Sink::resolve(named, &found.dir.join(WS_RUNS_DIR));
    runs_at(sink.dir(), sink.is_named(), req)
}

/// A page of the run lines in `lines_dir`, as `req` filters and pages them, `vouched` or not.
/// Neither file there, or no such directory, is an empty page.
///
/// # Errors
///
/// `E_INVALID_ARG` for a request that does not read, or (`run_lines_dir`) a `lines_dir` that is
/// something other than a directory; `E_IO` with `details.path` for a lines file that does not
/// open or is no regular file.
pub fn runs_at(
    lines_dir: &Utf8Path,
    vouched: bool,
    req: &CheckRunsReq,
) -> Result<CheckRunsResp, OpError> {
    // 1. The request, before any file.
    let (filter, limit) = Filter::of(req)?;
    if std::fs::metadata(lines_dir).is_ok_and(|meta| !meta.is_dir()) {
        return Err(lines::refused_dir(format!(
            "the run lines' directory {lines_dir} is not a directory"
        )));
    }

    // 2. Both files opened before either is read: a rotation between reads no file twice.
    let older = open_lines(&lines_dir.join(RUN_LINES.older))?;
    let current = open_lines(&lines_dir.join(RUN_LINES.current))?;

    // 3. The page.
    let read = page(reader(older), reader(current), &filter, limit);
    let mut resp = CheckRunsResp::new(vouched, lines_dir.as_str());
    resp.runs = read.runs;
    resp.cursor = read.cursor;
    resp.scanned = read.scanned;
    resp.skipped = read.skipped;
    Ok(resp)
}

/// The lines of `file`, or none for a file that is not there.
fn reader(file: Option<File>) -> Box<dyn BufRead> {
    match file {
        Some(file) => Box::new(BufReader::new(file)),
        None => Box::new(io::empty()),
    }
}

/// The lines file at `path`, opened to read if it is a regular file: none when it is not there.
/// A link is not followed and a FIFO does not block the open (`O_NOFOLLOW | O_NONBLOCK`), as a
/// log is opened: the directory may be another account's.
fn open_lines(path: &Utf8Path) -> Result<Option<File>, OpError> {
    let opened = (OpenOptions::new().read(true))
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path);
    let file = match opened {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => return Err(not_lines(path)),
        Err(e) => return Err(io_failed(path, &e)),
    };
    let meta = file.metadata().map_err(|e| io_failed(path, &e))?;
    if !meta.is_file() {
        return Err(not_lines(path));
    }
    Ok(Some(file))
}

/// `E_IO` for a lines file at `path` that is not a regular file.
fn not_lines(path: &Utf8Path) -> OpError {
    let mut error = error::error(
        ErrorCode::Io,
        error::one_line(format!(
            "{path} is not a regular file: no run wrote it as its run lines"
        )),
    );
    error
        .details
        .insert("path".to_owned(), path.as_str().into());
    error
}

/// `E_INVALID_ARG` saying `what`, with `details.reason`.
fn invalid(what: String, reason: &str) -> OpError {
    let mut error = error::error(ErrorCode::InvalidArg, error::one_line(what));
    error.details.insert("reason".to_owned(), reason.into());
    error
}

/// `E_IO` for `path`, which failed with `e`.
fn io_failed(path: &Utf8Path, e: &io::Error) -> OpError {
    let mut error = error::error(
        ErrorCode::Io,
        error::one_line(format!("{path}: {}", e.kind())),
    );
    error
        .details
        .insert("path".to_owned(), path.as_str().into());
    error
}

#[cfg(test)]
mod tests;
