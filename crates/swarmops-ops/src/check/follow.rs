//! `check log` (M31-3): one check's log of a run, read on the host the run ran on, once or as it
//! grows (`--follow`).
//!
//! ```text
//!   the runs of a directory or a workspace ─▶ the run: --run, else the newest that lists the
//!                                             check (with --follow, the newest running first)
//!   <run>/logs/<check>.log ─▶ whole lines from the last offset ─▶ check::log::parse
//!   (a regular file, not followed if a link; read as a stream, at most READ_LIMIT bytes, only
//!    the lines a look gives kept)
//!   ended: its `end` line; or the run's lock free and no `end` line (the runner gone); or the
//!          run ended and its record says the check never started (`not_run`)
//! ```
//!
//! `check log --failed` (M31-13) reads the logs of the checks of one run that did not pass, each
//! as above, in one answer; and a run is found through its run lines when the caller names their
//! directory:
//!
//! ```text
//!   --failed:    --run, else the newest run ─▶ its record: every check not passed, in run order
//!                ─▶ the first min(failed, 20, 100,000 / tail) logs: the first at the top, the
//!                   others in `more`; the rest named in `failed` and counted in `logs_omitted`
//!   --run-lines: the run's newest line (check::runs) ─▶ its run_dir, whose last component is
//!                the run's id ─▶ the runs' directory to read the run from
//! ```
//!
//! Reads only: no lock is taken, nothing is written, no store operation is called.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead as _, BufReader, Read as _, Seek as _, SeekFrom};
use std::os::unix::fs::OpenOptionsExt as _;

use camino::{Utf8Path, Utf8PathBuf};
use serde_json::Value;
use swarmops_api::{
    CheckLogEnd, CheckLogLineOut, CheckLogReq, CheckLogResp, CheckLogStream, CheckOutcome,
    CheckRunResp, CheckRunsReq, ErrorCode, MAX_CHECK_LOG_TAIL, MAX_CHECK_RUNS_LIMIT,
    MAX_FAILED_LOGS, OpError,
};
use swarmops_ws::registry::Registry;
use swarmops_ws::workspace::{self, Locate};

use super::lines;
use super::log::{Kind, Line, MAX_CHECK_LOG_BYTES, dir_key, parse_line};
use super::run::{KEPT_CHECK_RUNS, RUN_FILE, RUN_LOCK, WS_RUNS_DIR, held};
use super::runs;
use crate::error;

/// The most lines one look of a follower gives; the rest come with the next looks.
pub const MAX_FOLLOW_LINES: usize = 1_000;

/// The most bytes of a log a reader reads: the writer's bound, and a MiB for its `cut` and `end`
/// lines. A file longer than that was not written by a run, and its rest is never read.
pub const READ_LIMIT: u64 = MAX_CHECK_LOG_BYTES + (1 << 20);

/// The size of one read of a log.
const READ_CHUNK: usize = 64 * 1024;

/// A run found on this host: its id, its directory, and its record.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct FoundRun {
    /// The run's id.
    pub id: String,
    /// Its directory.
    pub dir: Utf8PathBuf,
    /// Its result, as its record holds it.
    pub result: CheckRunResp,
    /// Whether the record says the run ended.
    pub ended: bool,
}

/// The runs of the directory `dir`, below `runs_root`, as `check run --dir` keeps them.
///
/// # Errors
///
/// `E_INVALID_ARG` for a `dir` that is no directory, or a host that gives no runs root.
pub fn dir_runs(dir: &Utf8Path, runs_root: Option<&Utf8Path>) -> Result<Utf8PathBuf, OpError> {
    let real = std::fs::canonicalize(dir).ok().filter(|p| p.is_dir());
    let Some(real) = real.and_then(|p| Utf8PathBuf::from_path_buf(p).ok()) else {
        let mut error = error::error(
            ErrorCode::InvalidArg,
            error::one_line(format!("--dir {:?} names no directory", dir.as_str())),
        );
        error.details.insert("reason".to_owned(), "dir".into());
        return Err(error.with_hint("name the directory whose run to read: --dir ."));
    };
    let Some(root) = runs_root else {
        let mut error = error::error(
            ErrorCode::InvalidArg,
            format!(
                "this host gives no place for check runs: set {}",
                super::run::CHECK_RUNS_DIR_ENV
            ),
        );
        error.details.insert("reason".to_owned(), "runs_dir".into());
        return Err(error);
    };
    Ok(root.join(dir_key(&real)))
}

/// `check_log`: the last `req.tail` lines of check `req.check`'s log of a run of workspace
/// `req.workspace`, found through the registry beside the store; `req.run`, else the newest run
/// that ran the check. With `req.failed` instead of a check: the logs of the checks of the run
/// that did not pass ([`failed_logs`]); `req.run`, else the workspace's newest run. Reads only.
///
/// # Errors
///
/// `E_INVALID_ARG` with `details.reason` `check` for a request of neither a check nor `failed`,
/// or of both; then as [`Follower::start`] or [`failed_logs`].
pub fn log_ws(ctx: &swarmops_ws::Ctx<'_>, req: CheckLogReq) -> Result<CheckLogResp, OpError> {
    let CheckLogReq {
        workspace,
        check,
        failed,
        run,
        tail,
        ..
    } = req;

    // 1. Exactly one of a check and the failed ones.
    if check.is_some() == failed {
        let what = "give exactly one of check and failed: a check's log, or the logs of the \
                    checks that did not pass"
            .to_owned();
        return Err(invalid(what, "check"));
    }

    // 2. The workspace's runs; then the check's log, or the failed ones'.
    let registry = Registry::open(&ctx.store_root, ctx.fs.clone());
    let at = format!("--ws {workspace}");
    let found = workspace::find(&Locate::Name(workspace), Some(&registry), ctx.fs.as_ref())?;
    let runs = found.dir.join(WS_RUNS_DIR);
    let Some(check) = check else {
        return failed_logs(&runs, &at, run.as_deref(), tail);
    };
    let (_, resp) = Follower::start(&runs, &at, &check, run.as_deref(), tail, false)?;
    Ok(resp)
}

/// `check log --failed`: the logs of the checks that did not pass in a run of the runs in
/// `runs`, `run` if given, else the newest run there, running or ended. Each log is its last
/// `tail` lines, as [`Follower::start`] gives that check's: the first failed check's is the
/// answer, the others' are its `more`, in run order, and `failed` names every one. At most
/// [`MAX_FAILED_LOGS`] logs and [`MAX_CHECK_LOG_TAIL`] lines are given: the rest are counted in
/// `logs_omitted`. Of a run that has not ended, the checks never started so far are left out.
/// `at` is how `check run` names the runs' directory or workspace, for a hint.
///
/// # Errors
///
/// `E_INVALID_ARG` with `details.reason`: `tail` (not 1 to 100,000); `run` (no such run, or no
/// run at all); `none_failed` (every check passed, or none has ended), with `details.run` and
/// `details.running`. `E_IO` for a record or a log that cannot be read.
pub fn failed_logs(
    runs: &Utf8Path,
    at: &str,
    run: Option<&str>,
    tail: usize,
) -> Result<CheckLogResp, OpError> {
    // 1. The request, and the run: the one named, else the newest.
    check_tail(tail)?;
    let found = match run {
        Some(run) => {
            let read = (lines::is_run_id(run).then(|| runs.join(run)))
                .map(|dir| found_at(run, &dir))
                .transpose()?;
            read.flatten().ok_or_else(|| {
                let mut error = invalid(format!("no run {run} here"), "run");
                error.details.insert("run".to_owned(), run.into());
                error
            })?
        }
        None => newest_run(runs)?.ok_or_else(|| {
            let spaced = spaced(at);
            invalid("no run here yet".to_owned(), "run")
                .with_hint(format!("start one: swarmops check run{spaced}"))
        })?,
    };

    // 2. Its checks that did not pass, in run order; one never started only once the run ended.
    let failed: Vec<String> = (found.result.checks.iter())
        .filter(|check| check.outcome != CheckOutcome::Passed)
        .filter(|check| found.ended || check.outcome != CheckOutcome::NotRun)
        .map(|check| check.name.clone())
        .collect();
    if failed.is_empty() {
        return Err(none_failed(&found, at));
    }

    // 3. As many logs as the bounds give, each as `check log` reads it.
    let within_lines = (MAX_CHECK_LOG_TAIL / tail).max(1);
    let given = failed.len().min(MAX_FAILED_LOGS).min(within_lines);
    let mut logs = Vec::with_capacity(given);
    for name in &failed[..given] {
        let (_, log) = Follower::start(runs, at, name, Some(&found.id), tail, false)?;
        logs.push(log);
    }

    // 4. The first at the top, the others beside it.
    let mut logs = logs.into_iter();
    let Some(mut first) = logs.next() else {
        return Err(none_failed(&found, at));
    };
    first.more = logs.map(Into::into).collect();
    first.logs_omitted = u64::try_from(failed.len() - given).unwrap_or(u64::MAX);
    first.failed = failed;
    Ok(first)
}

/// `E_INVALID_ARG` (`none_failed`) for the run `found`, none of whose checks did not pass: the
/// run and whether it still runs in the details, and what to read or do instead in the hint.
fn none_failed(found: &FoundRun, at: &str) -> OpError {
    let running = held(&found.dir.join(RUN_LOCK));
    let (id, spaced) = (&found.id, spaced(at));
    let (what, hint) = if running {
        (
            format!("no check of the run {id} has failed so far: it is still running"),
            format!(
                "wait for the run to end, or follow one check: swarmops check log NAME{spaced} \
                 --run {id} --follow"
            ),
        )
    } else {
        (
            format!("no check of the run {id} failed"),
            format!("read one check's log: swarmops check log NAME{spaced} --run {id}"),
        )
    };
    let mut error = invalid(what, "none_failed");
    error.details.insert("run".to_owned(), id.as_str().into());
    error.details.insert("running".to_owned(), running.into());
    error.with_hint(hint)
}

/// The newest run in `runs` whose record reads, running or ended: the one of the greatest id.
///
/// # Errors
///
/// `E_IO` for a record that cannot be read.
pub fn newest_run(runs: &Utf8Path) -> Result<Option<FoundRun>, OpError> {
    for name in &run_names(runs) {
        if let Some(found) = found_at(name, &runs.join(name))? {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

/// The runs' directory to read run `run` from, found through the run lines in `lines_dir`: the
/// parent of the run directory the run's newest line names.
///
/// # Errors
///
/// `E_INVALID_ARG` with `details.reason`: `run` (not a run's id, or no line of that run there);
/// `run_dir` (its line names no run directory, or one whose last component is not the run's
/// id); `run_gone` (the directory is not there any more), with `details.failed`, the checks of
/// the run that the lines say did not pass. As [`runs::runs_at`] for lines that cannot be read.
pub fn run_through_lines(lines_dir: &Utf8Path, run: &str) -> Result<Utf8PathBuf, OpError> {
    // 1. The run's lines, newest first.
    let unknown = |what: String| {
        let mut error = invalid(what, "run");
        error.details.insert("run".to_owned(), run.into());
        error
    };
    if !lines::is_run_id(run) {
        return Err(unknown(format!("{run:?} is not a run's id")));
    }
    let mut req = CheckRunsReq::new(String::new());
    req.run = Some(run.to_owned());
    req.limit = MAX_CHECK_RUNS_LIMIT;
    let page = runs::runs_at(lines_dir, true, &req)?;
    let Some(newest) = page.runs.first() else {
        return Err(unknown(format!(
            "the run lines in {lines_dir} hold no line of the run {run}"
        )));
    };

    // 2. Where the newest of them says the run is: a directory named as the run.
    let dir = (newest.run_dir.as_deref().map(Utf8Path::new))
        .filter(|dir| dir.is_absolute() && dir.file_name() == Some(run));
    let Some((dir, parent)) = dir.and_then(|dir| Some((dir, dir.parent()?))) else {
        return Err(invalid(
            format!("the run lines of the run {run} name no run directory of it"),
            "run_dir",
        )
        .with_hint("name where the run ran instead: --dir DIR, or --ws NAME"));
    };

    // 3. Still there: a host keeps the newest runs only.
    if !dir.is_dir() {
        let failed: Vec<&str> = (page.runs.iter().rev())
            .filter(|line| line.outcome != CheckOutcome::Passed)
            .map(|line| line.check.as_str())
            .collect();
        let mut error = invalid(
            format!("the run {run} is not kept any more: {dir} is gone"),
            "run_gone",
        );
        error.details.insert("run".to_owned(), run.into());
        error
            .details
            .insert("failed".to_owned(), serde_json::json!(failed));
        return Err(error.with_hint(format!(
            "a host keeps the newest {KEPT_CHECK_RUNS} runs of a directory or workspace: run \
             the checks again to read their logs"
        )));
    }
    Ok(parent.to_owned())
}

/// `at`, how `check run` names where the runs are, after a space; nothing when it names
/// nothing.
fn spaced(at: &str) -> String {
    if at.is_empty() {
        String::new()
    } else {
        format!(" {at}")
    }
}

/// `E_INVALID_ARG` (`tail`) unless `tail` is 1 to [`MAX_CHECK_LOG_TAIL`].
fn check_tail(tail: usize) -> Result<(), OpError> {
    if (1..=MAX_CHECK_LOG_TAIL).contains(&tail) {
        return Ok(());
    }
    let mut error = invalid(
        format!("--tail is 1 to {MAX_CHECK_LOG_TAIL}, not {tail}"),
        "tail",
    );
    error.details.insert("tail".to_owned(), tail.into());
    Err(error)
}

/// One check's log of a run, read from where the last look stopped.
#[derive(Debug)]
#[non_exhaustive]
pub struct Follower {
    /// The run.
    run: FoundRun,
    /// The check.
    check: String,
    /// The log's path.
    log: Utf8PathBuf,
    /// The bytes of whole lines read so far.
    offset: u64,
    /// The lines read so far.
    lines: u64,
    /// Lines read and not given yet, past a look's [`MAX_FOLLOW_LINES`].
    queued: VecDeque<Line>,
    /// How the log ended, once a look has seen it.
    ended: Option<Ended>,
    /// The log's `end` line, once a read has passed it, whether or not it was kept.
    end_line: Option<Ended>,
}

/// What one read of a log found: the last lines of those it read, as many as it was to keep, and
/// whether it read up to the end of what the log holds now.
struct Read {
    /// The lines kept.
    lines: VecDeque<Line>,
    /// Whether no whole line is left past them.
    at_end: bool,
}

/// How a log ended.
#[derive(Debug, Clone, Copy)]
struct Ended {
    /// By its `end` line, or the runner gone.
    by: CheckLogEnd,
    /// The outcome, by an `end` line or the run's record.
    outcome: Option<CheckOutcome>,
    /// The check's milliseconds, by an `end` line.
    duration_ms: Option<u64>,
}

impl Follower {
    /// The log of `check` in the runs in `runs` (`run`, else the newest that lists `check`,
    /// with `following` the newest of them still running first), and its last `tail` lines.
    /// `at` is how `check run` names the runs' directory or workspace (`--dir DIR`, `--ws NAME`,
    /// or nothing for the one around the working directory), for a hint.
    ///
    /// # Errors
    ///
    /// `E_INVALID_ARG` for a `tail` out of 1 to 100,000 (`tail`), no run that lists `check`
    /// (`run`, `details.check`), or a `run` there is not (`run`, `details.run`); `E_IO` for a log
    /// that cannot be read.
    pub fn start(
        runs: &Utf8Path,
        at: &str,
        check: &str,
        run: Option<&str>,
        tail: usize,
        following: bool,
    ) -> Result<(Follower, CheckLogResp), OpError> {
        // 1. The request, and the run.
        check_tail(tail)?;
        let found = find_run(runs, at, check, run, following)?;
        let log = found.dir.join("logs").join(format!("{check}.log"));
        let mut follower = Follower {
            run: found,
            check: check.to_owned(),
            log,
            offset: 0,
            lines: 0,
            queued: VecDeque::new(),
            ended: None,
            end_line: None,
        };

        // 2. Every whole line so far, the last `tail` of them kept and given.
        let mut read = follower.read_on(tail, usize::MAX)?;
        let last = follower.settle(read.at_end, tail)?;
        read.lines.extend(last);
        let first = read.lines.len().saturating_sub(tail);
        let given: Vec<Line> = read.lines.into_iter().skip(first).collect();
        let resp = follower.answer(given, follower.ended.is_some());
        Ok((follower, resp))
    }

    /// The run read.
    pub fn run(&self) -> &FoundRun {
        &self.run
    }

    /// Whether a look has given the log's end: no look gives more.
    pub fn done(&self) -> bool {
        self.ended.is_some() && self.queued.is_empty()
    }

    /// The lines written since the last look, at most [`MAX_FOLLOW_LINES`], and the end once
    /// every line before it is given; none when there is nothing new.
    ///
    /// # Errors
    ///
    /// `E_IO` for a log or a record that cannot be read.
    pub fn look(&mut self) -> Result<Option<CheckLogResp>, OpError> {
        // 1. New lines, at most a look's, once those queued are given; and whether the log
        //    ended.
        let was_ended = self.ended.is_some();
        if !was_ended && self.queued.is_empty() {
            let read = self.read_on(MAX_FOLLOW_LINES, MAX_FOLLOW_LINES)?;
            self.queued.extend(read.lines);
            let last = self.settle(read.at_end, usize::MAX)?;
            self.queued.extend(last);
        }
        let newly_ended = !was_ended && self.ended.is_some();
        if self.queued.is_empty() && !newly_ended {
            return Ok(None);
        }

        // 2. At most a look's lines; the end only with the last of them.
        let count = self.queued.len().min(MAX_FOLLOW_LINES);
        let given: Vec<Line> = self.queued.drain(..count).collect();
        let ended = self.ended.is_some() && self.queued.is_empty();
        Ok(Some(self.answer(given, ended)))
    }

    /// The whole lines the log holds past the last offset, at most `most` of them, numbered on
    /// from the last line read; of them the last `keep`. The log is read as a stream, up to
    /// [`READ_LIMIT`] bytes from its start: memory holds the lines kept and one line being read.
    fn read_on(&mut self, keep: usize, most: usize) -> Result<Read, OpError> {
        // 1. The bytes past the offset; none while the log is not there.
        let mut read = Read {
            lines: VecDeque::new(),
            at_end: true,
        };
        let Some(mut file) = open_log(&self.log)? else {
            return Ok(read);
        };
        (file.seek(SeekFrom::Start(self.offset))).map_err(|e| io_failed(&self.log, &e))?;
        let left = READ_LIMIT.saturating_sub(self.offset);
        let mut stream = BufReader::with_capacity(READ_CHUNK, file.take(left));

        // 2. Each whole line: a line still being written waits for the next look.
        let mut bytes = Vec::new();
        let mut count = 0;
        while count < most {
            bytes.clear();
            let n = (stream.read_until(b'\n', &mut bytes)).map_err(|e| io_failed(&self.log, &e))?;
            let Some(text) = bytes.strip_suffix(b"\n") else {
                return Ok(read);
            };
            let text = text.strip_suffix(b"\r").unwrap_or(text);
            self.offset += u64::try_from(n).unwrap_or(u64::MAX);
            self.lines += 1;
            count += 1;
            let line = parse_line(self.lines, &String::from_utf8_lossy(text));
            if let Some(end) = line.event("end") {
                self.end_line = Some(ended_by_line(&end));
            }
            read.lines.push_back(line);
            if read.lines.len() > keep {
                read.lines.pop_front();
            }
        }

        // 3. Stopped at `most`: whether a whole line is left.
        read.at_end = !(stream.fill_buf()).is_ok_and(|rest| rest.contains(&b'\n'));
        Ok(read)
    }

    /// Whether the log ended: its `end` line read; else, once a read reached the end of what the
    /// log holds (`at_end`) and the run's lock is free, the lines the runner wrote last (of them
    /// the last `keep`, which it gives) and then the run's record.
    fn settle(&mut self, at_end: bool, keep: usize) -> Result<VecDeque<Line>, OpError> {
        // 1. Its end line.
        if let Some(end) = self.end_line {
            self.ended = Some(end);
            return Ok(VecDeque::new());
        }

        // 2. More to read, or the runner still running: more may come.
        if !at_end || held(&self.run.dir.join(RUN_LOCK)) {
            return Ok(VecDeque::new());
        }

        // 3. The runner gone: the last lines it wrote, then its record.
        let last = self.read_on(keep, usize::MAX)?.lines;
        if let Some(end) = self.end_line {
            self.ended = Some(end);
            return Ok(last);
        }
        let record = read_record(&self.run.dir)?;
        let outcome = (record.as_ref())
            .filter(|(_, ended)| *ended)
            .and_then(|(result, _)| result.checks.iter().find(|c| c.name == self.check))
            .map(|c| c.outcome);
        self.ended = Some(match outcome {
            Some(CheckOutcome::NotRun) if self.lines == 0 => Ended {
                by: CheckLogEnd::End,
                outcome: Some(CheckOutcome::NotRun),
                duration_ms: None,
            },
            _ => Ended {
                by: CheckLogEnd::RunnerGone,
                outcome: None,
                duration_ms: None,
            },
        });
        Ok(last)
    }

    /// The answer of `lines`, which end the log when `ended`.
    fn answer(&self, lines: Vec<Line>, ended: bool) -> CheckLogResp {
        let result = &self.run.result;
        let mut resp = CheckLogResp::new(result.vouched, &result.run, &self.check);
        let started = self.lines > 0 || self.log.exists();
        resp.log = started.then(|| self.log.to_string());
        resp.total_lines = self.lines;
        resp.first_line = lines.first().map_or(self.lines + 1, |line| line.n);
        resp.lines = lines.into_iter().map(line_out).collect();
        if let (true, Some(end)) = (ended, self.ended) {
            resp.ended = true;
            resp.ended_by = Some(end.by);
            resp.outcome = end.outcome;
            resp.duration_ms = end.duration_ms;
        }
        resp
    }
}

/// The run in `runs` to read `check`'s log of: `run` if given, else the newest whose record lists
/// `check`, with `running_first` the newest of those still running before any other. `at` is
/// how `check run` names where the runs are, for the hint.
///
/// # Errors
///
/// `E_INVALID_ARG` with `details.reason` `run`: no such run (`details.run`), or none that lists
/// `check` (`details.check`).
pub fn find_run(
    runs: &Utf8Path,
    at: &str,
    check: &str,
    run: Option<&str>,
    running_first: bool,
) -> Result<FoundRun, OpError> {
    // 1. A run named: it, if it is there and lists the check.
    let lists = |found: &FoundRun| found.result.checks.iter().any(|c| c.name == check);
    if let Some(run) = run {
        let found = (uuid::Uuid::try_parse(run).ok())
            .map(|_| runs.join(run))
            .map(|dir| found_at(run, &dir))
            .transpose()?
            .flatten()
            .filter(lists);
        return found.ok_or_else(|| {
            let mut error = invalid(format!("no run {run} here ran the check {check}"), "run");
            error.details.insert("run".to_owned(), run.into());
            error.details.insert("check".to_owned(), check.into());
            error
        });
    }

    // 2. Else the newest that lists it; following, the newest still running first.
    let mut newest = None;
    for name in &run_names(runs) {
        let Some(found) = found_at(name, &runs.join(name))?.filter(lists) else {
            continue;
        };
        if !running_first || held(&found.dir.join(RUN_LOCK)) {
            return Ok(found);
        }
        newest.get_or_insert(found);
    }
    newest.ok_or_else(|| {
        let mut error = invalid(format!("no run here ran the check {check}"), "run");
        error.details.insert("check".to_owned(), check.into());
        let spaced = spaced(at);
        error.with_hint(format!("start one: swarmops check run{spaced}"))
    })
}

/// The names of the runs in `runs`, newest first: a run's id sorts by its start.
fn run_names(runs: &Utf8Path) -> Vec<String> {
    let mut names: Vec<String> = match runs.read_dir_utf8() {
        Ok(entries) => (entries.flatten())
            .map(|entry| entry.file_name().to_owned())
            .filter(|name| uuid::Uuid::try_parse(name).is_ok())
            .collect(),
        Err(_) => Vec::new(),
    };
    names.sort_unstable_by(|a, b| b.cmp(a));
    names
}

/// The run `id` in `dir`, if its record reads.
fn found_at(id: &str, dir: &Utf8Path) -> Result<Option<FoundRun>, OpError> {
    Ok(read_record(dir)?.map(|(result, ended)| FoundRun {
        id: id.to_owned(),
        dir: dir.to_owned(),
        result,
        ended,
    }))
}

/// The log at `path`, opened to read if it is a regular file: none while it is not there. A link
/// is not followed and a FIFO does not block the open (`O_NOFOLLOW | O_NONBLOCK`); anything but a
/// regular file is `E_IO`, since a check could put one at its log's path.
fn open_log(path: &Utf8Path) -> Result<Option<File>, OpError> {
    let file = match (OpenOptions::new().read(true))
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => return Err(not_a_log(path)),
        Err(e) => return Err(io_failed(path, &e)),
    };
    let meta = file.metadata().map_err(|e| io_failed(path, &e))?;
    if !meta.is_file() {
        return Err(not_a_log(path));
    }
    Ok(Some(file))
}

/// `E_IO` for a log at `path` that is not a regular file.
fn not_a_log(path: &Utf8Path) -> OpError {
    let mut error = error::error(
        ErrorCode::Io,
        format!("{path} is not a regular file: no run wrote it as a log"),
    );
    error
        .details
        .insert("path".to_owned(), path.as_str().into());
    error
}

/// The record of the run in `dir`: its result and whether it ended; none when there is no record
/// this build reads.
fn read_record(dir: &Utf8Path) -> Result<Option<(CheckRunResp, bool)>, OpError> {
    let path = dir.join(RUN_FILE);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io_failed(&path, &e)),
    };
    let Ok(mut record) = serde_json::from_slice::<Value>(&bytes) else {
        return Ok(None);
    };
    let ended = record["state"] == "ended";
    Ok(serde_json::from_value(record["result"].take())
        .ok()
        .map(|result| (result, ended)))
}

/// How an `end` line's object says the check ended.
fn ended_by_line(end: &serde_json::Map<String, Value>) -> Ended {
    Ended {
        by: CheckLogEnd::End,
        outcome: serde_json::from_value(end.get("outcome").cloned().unwrap_or_default()).ok(),
        duration_ms: end.get("duration_ms").and_then(Value::as_u64),
    }
}

/// `line` as the answer gives it.
fn line_out(line: Line) -> CheckLogLineOut {
    let stream = match line.kind {
        Kind::Out => CheckLogStream::Out,
        Kind::Err => CheckLogStream::Err,
        Kind::Run => CheckLogStream::Run,
        _ => CheckLogStream::Other,
    };
    let mut out = CheckLogLineOut::new(line.n, stream, line.text);
    out.at_ms = line.at_ms;
    out
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
