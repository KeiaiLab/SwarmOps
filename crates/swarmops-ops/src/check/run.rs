//! `check run` (M31-1): the checks of one stage, one at a time in name order, each with no shell
//! and no stdin, in its own process group, under its timeout, its output in a log of its own.
//!
//! ```text
//!   --dir DIR:  DIR's checks (check::list_dir) ─┐
//!   workspace:  held clean and locked ──────────┤  (swarmops_ws::check::hold_for_run), its
//!               checks read from the store at   │   lock held to the end; the store only read
//!               the changeset its files are ────┤
//!                                               ▼
//!   select the stage's (or the named) ─▶ the run's directory, its lock, run.json (running),
//!                                        runs past the 20 kept removed
//!   each check, by name: its dir ─▶ its environment (check::env) ─▶ the process runner,
//!                        the output into its LineLog ─▶ its outcome ─▶ its group gone, its
//!                        report file and the reports it declares read (check::intake):
//!                        findings, within the run's bound; a declared report is looked at
//!                        before the check starts, so that one left from before is not read;
//!                        then its run line appended (check::lines), before the next starts
//!   run.json (ended), the lock let go ─▶ all passed: the result; else E_CHECK_FAILED{run}
//! ```
//!
//! In the directory form, a run also removes the runs of each other directory that is gone: a
//! key directory whose `dir` file names a path that is not there any more, none of whose runs
//! holds its lock. So the runs root does not grow with every throwaway directory checked.
//!
//! A run directory holds `run.json`, `lock` (held while the run runs), `logs/<check>.log` and
//! `checks/<check>/`, the paths each check's environment names. Nothing there is synced: they
//! are diagnostics, which nothing decides from. A set stop flag (a signal, in the CLI) kills the
//! running check's group: that check is `cancelled`, the ones after it `not_run`.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{self, Write as _};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use camino::{Utf8Path, Utf8PathBuf};
use serde_json::{Value, json};
use swarmops_api::{
    CheckListReq, CheckListResp, CheckNotStarted, CheckOut, CheckOutcome, CheckRunOut, CheckRunReq,
    CheckRunResp, CheckStage, ErrorCode, OpError,
};
use swarmops_ws::run::{self, Env, Measure, Output, Spec, Stream};
use swarmops_ws::workspace::Locate;

use super::env::{self, Pinned, REPORT_FILE};
use super::findings::Budget;
use super::intake;
use super::lines::{self, Sink};
use super::log::{End, LineLog, Start, dir_key};
use super::{LEGACY_FILE, list, list_dir};
use crate::error;

/// How many runs a directory or a workspace keeps: the newest 20, the one starting among them.
pub const KEPT_CHECK_RUNS: usize = 20;

/// Where a workspace keeps its runs, below its `.swarmws/`.
pub const WS_RUNS_DIR: &str = ".swarmws/check-runs";

/// The variable that names where a directory's runs go, else the state directory's
/// `swarmops/check-runs`.
pub const CHECK_RUNS_DIR_ENV: &str = "SWARMOPS_CHECK_RUNS_DIR";

/// The format of `run.json`.
pub const RUN_FILE_FORMAT: u32 = 1;

/// A run's record, in its directory.
pub const RUN_FILE: &str = "run.json";

/// The file a run's lock is held on, in its directory.
pub const RUN_LOCK: &str = "lock";

/// The mode of each directory a run makes: the runs root, a directory's key directory, a run's
/// directory and those in it, which only their owner reads, as `.swarmws/` is.
pub const RUNS_DIR_MODE: u32 = 0o700;

/// The file below a directory's runs that names the directory they are of.
const DIR_FILE: &str = "dir";

/// What a run takes from its host, which no API type carries: the variables to pass, the stop
/// flag, the caller's environment, where a directory's runs go, and where the run lines go.
#[non_exhaustive]
pub struct RunOpts<'a> {
    /// The names of the caller's variables the checks also get (`--pass`).
    pub pass: Vec<String>,
    /// Set: the running check's group is killed, and no later check starts.
    pub stop: Option<&'a AtomicBool>,
    /// The caller's environment.
    pub lookup: &'a dyn Fn(&str) -> Option<OsString>,
    /// Where a directory's runs go (the directory form); none when the host gives no place.
    pub runs_root: Option<Utf8PathBuf>,
    /// The directory the caller named for the run lines ([`lines::named`]); none: beside the
    /// directory's runs, or in the workspace's.
    pub lines: Option<Utf8PathBuf>,
}

impl<'a> RunOpts<'a> {
    /// A run that passes nothing more, with no stop flag, no runs root and no directory named
    /// for its lines, reading the caller's environment through `lookup`.
    pub fn new(lookup: &'a dyn Fn(&str) -> Option<OsString>) -> RunOpts<'a> {
        RunOpts {
            pass: Vec::new(),
            stop: None,
            lookup,
            runs_root: None,
            lines: None,
        }
    }

    /// Whether the stop flag is set.
    fn stopped(&self) -> bool {
        self.stop.is_some_and(|stop| stop.load(Ordering::SeqCst))
    }
}

/// `check run --dir`: the checks of `stage` (those of `names`, if any) of the directory `dir`,
/// as `check::list_dir` reads them, run there. `vouched`: only the caller stands behind the
/// files. The runs go below `opts.runs_root`, keyed by the directory's canonical path.
pub fn run_dir(
    dir: &Utf8Path,
    stage: CheckStage,
    names: &[String],
    opts: &RunOpts<'_>,
) -> Result<CheckRunResp, OpError> {
    // 1. The request, then the checks, read from the directory.
    env::check_pass(&opts.pass)?;
    let listed = list_dir(dir, None).map_err(rehint_dir)?;
    let selected = select(&listed, stage, names, dir.as_str(), &format!("--dir {dir}"))?;
    let root = canonical(dir)?;

    // 2. Where its runs go: below the runs root, by the directory's key.
    let Some(runs_root) = &opts.runs_root else {
        let mut error = error::error(
            ErrorCode::InvalidArg,
            format!(
                "this host gives no place for check runs: set {CHECK_RUNS_DIR_ENV} to an \
                 absolute directory"
            ),
        );
        error.details.insert("reason".to_owned(), "runs_dir".into());
        return Err(error);
    };
    let runs = runs_root.join(dir_key(&root));
    make_dir(&runs)?;
    let which = runs.join(DIR_FILE);
    if !which.exists() {
        std::fs::write(&which, format!("{root}\n")).map_err(|e| io_failed(&which, &e))?;
    }

    // 3. The runs of directories that are gone, then the run.
    forget_gone(runs_root, &runs);
    let mut subject = Subject::new(true, root, stage, runs);
    subject.hint_at = format!("--dir {dir}");
    run_checks(&subject, &listed, &selected, opts)
}

/// `check_run`: the checks of `req.stage` (those of `req.checks`, if any) of workspace
/// `req.workspace`, held clean and locked for the run, read from the store at the changeset its
/// files stand for, run in its directory. Not `vouched`: the files are that changeset's tree.
/// The store is only read. It needs no identity: it writes no store, only its run's files in the
/// workspace.
pub fn run_ws(
    ctx: &swarmops_ws::Ctx<'_>,
    req: CheckRunReq,
    opts: &RunOpts<'_>,
) -> Result<CheckRunResp, OpError> {
    // 1. The request, then the workspace held, then its checks, read from the store there.
    let CheckRunReq {
        workspace,
        stage,
        checks,
        ..
    } = req;
    env::check_pass(&opts.pass)?;
    let held = swarmops_ws::check::hold_for_run(ctx, &Locate::Name(workspace))?;
    let listed = list(ctx.store, CheckListReq::new(held.changeset.as_str()))?;
    let selected = select(&listed, stage, &checks, &held.name, &held.changeset)?;

    // 2. The run, in the workspace's private state, the lock held throughout.
    let runs = held.dir.join(WS_RUNS_DIR);
    make_dir(&runs)?;
    let mut subject = Subject::new(false, held.dir.clone(), stage, runs);
    subject.workspace = Some(held.name.clone());
    subject.changeset = Some(held.changeset.clone());
    subject.tree = Some(held.tree.clone());
    subject.hint_at = format!("--ws {}", held.name);
    let ran = run_checks(&subject, &listed, &selected, opts);
    drop(held);
    ran
}

/// What a run runs on: its form, its root, its stage, where its runs go.
struct Subject {
    /// Whether only the caller stands behind the files.
    vouched: bool,
    /// The directory the checks' `dir`s are below.
    root: Utf8PathBuf,
    /// The stage run.
    stage: CheckStage,
    /// The directory the run's own goes in, beside the runs kept.
    runs: Utf8PathBuf,
    /// The workspace's name, in the workspace form.
    workspace: Option<String>,
    /// The changeset its files stand for, in the workspace form.
    changeset: Option<String>,
    /// That changeset's root tree, in the workspace form.
    tree: Option<String>,
    /// How a hint names this form: `--dir DIR` or `--ws NAME`.
    hint_at: String,
}

impl Subject {
    /// A run of `stage` on `root`, kept below `runs`.
    fn new(vouched: bool, root: Utf8PathBuf, stage: CheckStage, runs: Utf8PathBuf) -> Subject {
        Subject {
            vouched,
            root,
            stage,
            runs,
            workspace: None,
            changeset: None,
            tree: None,
            hint_at: String::new(),
        }
    }
}

/// The checks of `listed` to run: those of `stage`, or of them those `names` names (each once),
/// by name. `E_INVALID_ARG` for a name the file does not define (`check`), one not of the stage
/// (`stage`), or nothing selected (`no_checks`); `at` names the directory or workspace, and
/// `list_at` is what `check list` takes to list the same file: `--dir DIR`, or the changeset.
fn select(
    listed: &CheckListResp,
    stage: CheckStage,
    names: &[String],
    at: &str,
    list_at: &str,
) -> Result<Vec<CheckOut>, OpError> {
    // 1. Each name, defined and of the stage.
    let wanted: BTreeSet<&str> = names.iter().map(String::as_str).collect();
    for name in &wanted {
        let Some(check) = listed.checks.iter().find(|c| c.name == *name) else {
            let mut error = invalid(format!("no check is named {name:?} in {at}"), "check");
            error.details.insert("check".to_owned(), (*name).into());
            return Err(error.with_hint(format!("swarmops check list {list_at} --stage {stage}")));
        };
        if !check.stages.contains(&stage) {
            let mut error = invalid(
                format!("the check {name} is not of the stage {stage}"),
                "stage",
            );
            error.details.insert("check".to_owned(), (*name).into());
            error
                .details
                .insert("stages".to_owned(), json!(check.stages));
            return Err(error.with_hint(format!(
                "give the check's stage: --stage {}",
                check.stages[0]
            )));
        }
    }

    // 2. The stage's checks, those named if any, in name order.
    let selected: Vec<CheckOut> = (listed.checks.iter())
        .filter(|c| c.stages.contains(&stage))
        .filter(|c| wanted.is_empty() || wanted.contains(c.name.as_str()))
        .cloned()
        .collect();
    if selected.is_empty() {
        let what = match &listed.file {
            Some(file) => format!("{file} of {at} names no check of the stage {stage}"),
            None => {
                format!("{at} has no .swarmops/checks.toml, and no check.argv in {LEGACY_FILE}")
            }
        };
        let mut error = invalid(format!("{what}: nothing to run"), "no_checks");
        error
            .details
            .insert("stage".to_owned(), stage.as_str().into());
        if let Some(file) = &listed.file {
            error
                .details
                .insert("file".to_owned(), file.as_str().into());
        }
        return Err(error.with_hint(format!(
            "swarmops check list {list_at} shows each check's stages"
        )));
    }
    Ok(selected)
}

/// The run of `selected`, of `listed`'s file, on `subject`: its directory made and locked, each
/// check run in turn, its record written at the start and the end. Every check passed: the
/// result; else `E_CHECK_FAILED` with the result in `details.run`.
fn run_checks(
    subject: &Subject,
    listed: &CheckListResp,
    selected: &[CheckOut],
    opts: &RunOpts<'_>,
) -> Result<CheckRunResp, OpError> {
    // 1. The run's directory, its lock, its record, and the runs past the kept count gone.
    let id = uuid::Uuid::now_v7().to_string();
    let dir = subject.runs.join(&id);
    make_dir(&dir.join("logs"))?;
    let lock_path = dir.join(RUN_LOCK);
    let lock = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| io_failed(&lock_path, &e))?;
    lock.lock().map_err(|e| io_failed(&lock_path, &e))?;
    let file = listed.file.clone().unwrap_or_default();
    let mut resp = CheckRunResp::new(subject.vouched, &id, subject.stage, &file, dir.as_str());
    resp.workspace.clone_from(&subject.workspace);
    resp.changeset.clone_from(&subject.changeset);
    resp.tree.clone_from(&subject.tree);
    resp.version = listed.version;
    resp.checks = (selected.iter())
        .map(|c| {
            CheckRunOut::new(
                &c.name,
                CheckOutcome::NotRun,
                &c.definition_digest,
                c.timeout_s,
            )
        })
        .collect();

    // 2. What the checks inherit: the legacy check its caller's whole environment.
    let legacy = file == LEGACY_FILE;
    let inherited = if legacy {
        Vec::new()
    } else {
        env::inherited(&opts.pass, opts.lookup)
    };
    resp.inherited_all = legacy;
    resp.inherited_env = inherited.iter().map(|(name, _)| name.clone()).collect();
    write_record(&dir, "running", &resp).map_err(|e| record_failed(e, &resp))?;
    prune(&subject.runs, &id);

    // 3. Each check in turn, until the run is stopped. It was stopped only if that cancelled a
    //    check or left one not run: a stop once the last check has exited changes nothing. A
    //    check reached leaves its run line before the next starts: diagnostics, whose failure
    //    is a warning and changes nothing here.
    let started = Instant::now();
    let mut budget = Budget::new();
    let mut sink = Sink::resolve(opts.lines.as_deref(), &subject.runs);
    let of = lines::Of {
        run: &id,
        stage: subject.stage,
        vouched: subject.vouched,
        tree: subject.tree.as_deref(),
        workspace: subject.workspace.as_deref(),
        run_dir: &dir,
    };
    for (at, check) in selected.iter().enumerate() {
        if opts.stopped() {
            break;
        }
        let cwd = cwd_of(subject, check);
        let before = intake::Before::look(&cwd, &check.reports);
        let ran = run_one(subject, &id, &dir, check, &inherited, legacy, opts);
        let mut out = ran.out;
        take_findings(&mut out, &dir, check, (&cwd, &before), &mut budget);
        let times = lines::Times {
            at_ms: ran.at_ms,
            wait_ms: millis(ran.started.saturating_duration_since(started)),
            peak_rss_kib: ran.peak_rss_kib,
        };
        if let Some(warning) = sink.append(&lines::line_of(&of, &out, &times)) {
            let _ = writeln!(io::stderr(), "warning: run lines: {warning}");
        }
        let cancelled = out.outcome == CheckOutcome::Cancelled;
        resp.checks[at] = out;
        if cancelled {
            break;
        }
    }
    resp.cancelled = (resp.checks.iter())
        .any(|c| matches!(c.outcome, CheckOutcome::Cancelled | CheckOutcome::NotRun));
    resp.duration_ms = millis(started.elapsed());
    resp.findings_omitted = budget.omitted();
    resp.failed = (resp.checks.iter())
        .filter(|c| c.outcome != CheckOutcome::Passed)
        .map(|c| c.name.clone())
        .collect();

    // 4. The record of the end, the lock let go; the answer.
    let recorded = write_record(&dir, "ended", &resp);
    let _ = lock.unlock();
    drop(lock);
    recorded.map_err(|e| record_failed(e, &resp))?;
    if resp.failed.is_empty() {
        return Ok(resp);
    }
    Err(failed(&resp, &subject.hint_at))
}

/// One check as [`run_one`] ran it: its result, and what its run line holds beside it.
struct Ran {
    /// The check's result.
    out: CheckRunOut,
    /// When it started, or was tried: Unix milliseconds, its log's `started_at_ms`.
    at_ms: u64,
    /// The same moment on the monotonic clock.
    started: Instant,
    /// The peak of its group's sampled resident memory, in KiB.
    peak_rss_kib: Option<u64>,
}

/// Runs `check` of run `id`, its log and own directory in the run's `dir`, with what it
/// `inherited` of the caller's environment (or the whole of it, for the `legacy` check), its
/// group's memory sampled.
///
/// A log that cannot be written is the check's outcome, never the run's error: before the
/// program starts, `not_started: log`; once it ran, `failed`, its exit code and tails kept, the
/// reason in `message`. The run goes on with the next check, as after any outcome.
fn run_one(
    subject: &Subject,
    id: &str,
    dir: &Utf8Path,
    check: &CheckOut,
    inherited: &[(String, OsString)],
    legacy: bool,
    opts: &RunOpts<'_>,
) -> Ran {
    // 1. Its start, on both clocks; its log, started; its own directory and its environment.
    let (at_ms, started) = (now_ms(), Instant::now());
    let ran = |out: CheckRunOut, peak_rss_kib: Option<u64>| Ran {
        out,
        at_ms,
        started,
        peak_rss_kib,
    };
    let mut out = CheckRunOut::new(
        &check.name,
        CheckOutcome::NotStarted,
        &check.definition_digest,
        check.timeout_s,
    );
    let log_path = dir.join("logs").join(format!("{}.log", check.name));
    out.log = Some(log_path.to_string());
    let own = dir.join("checks").join(&check.name);
    let log = match start_log(subject, id, check, (&log_path, at_ms), &own) {
        Ok(log) => log,
        Err((path, e)) => {
            out.not_started = Some(CheckNotStarted::Log);
            out.message = Some(unwritten(&path, &e));
            return ran(out, None);
        }
    };
    let pinned = Pinned::new(
        &check.name,
        subject.stage.as_str(),
        subject.tree.as_deref(),
        id,
        &own,
        subject.workspace.as_deref(),
    );

    // 2. Its directory, which must be one.
    let cwd = cwd_of(subject, check);
    if !std::fs::metadata(&cwd).is_ok_and(|meta| meta.is_dir()) {
        out.not_started = Some(CheckNotStarted::Dir);
        out.message = Some(format!(
            "the check's directory {} is not a directory here",
            check.dir
        ));
        let _ = log.finish(End::new(CheckOutcome::NotStarted));
        return ran(out, None);
    }

    // 3. The process runner, its output into the log, its group's memory sampled.
    let only = env::only(inherited, &pinned);
    let pairs = pinned.pairs();
    let mut spec = Spec::new(Duration::from_secs(check.timeout_s));
    spec.env = if legacy {
        Env::Inherit(&pairs)
    } else {
        Env::Only(&only)
    };
    spec.stop = opts.stop;
    spec.measure = Measure::GroupRss;
    let (done, log, failed) = Slot::run(&check.argv, &cwd, &spec, log, &log_path);

    // 4. Its outcome, and the log's end. The output is the slot's, whose writes never fail:
    //    the runner refuses a program that did not start, or fails waiting for one (`failed`).
    let done = match done {
        Ok(done) => done,
        Err(e) => {
            let program = e.details.contains_key("program");
            let outcome = if program {
                out.not_started = Some(CheckNotStarted::Program);
                CheckOutcome::NotStarted
            } else {
                out.outcome = CheckOutcome::Failed;
                CheckOutcome::Failed
            };
            out.message = Some(error::one_line(e.message));
            if let Some(log) = log {
                let _ = log.finish(End::new(outcome));
            }
            return ran(out, None);
        }
    };
    let outcome = match (done.stopped, done.timed_out, done.exit_code) {
        (true, _, _) => CheckOutcome::Cancelled,
        (_, true, _) => CheckOutcome::TimedOut,
        (_, _, Some(0)) => CheckOutcome::Passed,
        _ => CheckOutcome::Failed,
    };
    let mut end = End::new(outcome);
    end.exit_code = done.exit_code;
    end.signal = done.signal;
    out.outcome = outcome;
    out.exit_code = done.exit_code;
    out.signal = done.signal;
    match finish(log, failed, end) {
        Ok(summary) => {
            out.duration_ms = summary.duration_ms;
            out.sections = summary.sections;
            out.dropped_bytes = summary.dropped_bytes;
        }
        // 5. A log that could not be written whole: the check fails, whatever it did.
        Err(e) => {
            out.outcome = CheckOutcome::Failed;
            out.message = Some(unwritten(&log_path, &e));
        }
    }
    if out.outcome != CheckOutcome::Passed {
        out.stdout_tail = Some(done.stdout_tail);
        out.stderr_tail = Some(done.stderr_tail);
    }
    ran(out, done.peak_rss_kib)
}

/// The directory `check` runs in: its `dir` below `subject`'s root.
fn cwd_of(subject: &Subject, check: &CheckOut) -> Utf8PathBuf {
    if check.dir == super::ROOT_DIR {
        subject.root.clone()
    } else {
        subject.root.join(&check.dir)
    }
}

/// Takes in the findings of `out`, the result of `check` of the run in `dir` that has ended and
/// whose process group is gone, through the run's `budget`: its report file, then the reports it
/// declares below the directory it ran in, as they were `before` it started. For `passed`,
/// `failed` and `timed_out`; not for a check a stop cut short, whose reports are partial, nor
/// one that never ran.
fn take_findings(
    out: &mut CheckRunOut,
    dir: &Utf8Path,
    check: &CheckOut,
    (cwd, before): (&Utf8Path, &intake::Before),
    budget: &mut Budget,
) {
    let ended = [
        CheckOutcome::Passed,
        CheckOutcome::Failed,
        CheckOutcome::TimedOut,
    ];
    if !ended.contains(&out.outcome) {
        return;
    }
    let report = dir.join("checks").join(&out.name).join(REPORT_FILE);
    let mut taken = intake::take_in(&report, budget);
    intake::take_declared(&mut taken, cwd, &check.dir, &check.reports, before, budget);
    out.findings = taken.findings;
    out.findings_total = taken.total;
    out.reports = taken.reports;
}

/// The log at `path` of `check` of run `id` on `subject`, its `start` line written as of
/// `at_ms`, and the check's own directory `own` with its `artifacts`; else the path that could
/// not be written, and why.
fn start_log(
    subject: &Subject,
    id: &str,
    check: &CheckOut,
    (path, at_ms): (&Utf8Path, u64),
    own: &Utf8Path,
) -> Result<LineLog<File>, (Utf8PathBuf, io::Error)> {
    let mut start = Start::new(&check.name, id, subject.stage.as_str());
    start.tree.clone_from(&subject.tree);
    start.argv.clone_from(&check.argv);
    start.dir.clone_from(&check.dir);
    start.timeout_s = check.timeout_s;
    let log = (File::create(path))
        .and_then(|file| LineLog::start(file, &start, at_ms))
        .map_err(|e| (path.to_owned(), e))?;
    let artifacts = own.join("artifacts");
    private_dirs(&artifacts).map_err(|e| (artifacts, e))?;
    Ok(log)
}

/// Ends `log` as `end`, unless a write to it `failed` during the run: what it says, or the first
/// write that failed.
fn finish(
    log: Option<LineLog<File>>,
    failed: Option<io::Error>,
    end: End,
) -> io::Result<super::log::Summary> {
    match (log, failed) {
        (_, Some(e)) => Err(e),
        (Some(log), None) => log.finish(end).map(|(_, summary)| summary),
        (None, None) => Err(io::Error::other("the log went missing during the run")),
    }
}

/// The log a check's run writes into, which the caller takes back once the run is over, and the
/// first write to it that failed. A write never fails to the runner: once one has, the rest of
/// the output goes only to the tails, and the check keeps running to its end.
struct Slot {
    /// The log; none once taken back.
    log: Option<LineLog<File>>,
    /// The first write that failed.
    failed: Option<io::Error>,
}

impl Slot {
    /// Runs `argv` in `cwd` as `spec` says, its output into `log` at `path`: what the runner
    /// says, the log back, and the first write to it that failed.
    fn run(
        argv: &[String],
        cwd: &Utf8Path,
        spec: &Spec<'_>,
        log: LineLog<File>,
        path: &Utf8Path,
    ) -> (
        Result<run::Run, OpError>,
        Option<LineLog<File>>,
        Option<io::Error>,
    ) {
        let slot = Arc::new(Mutex::new(Slot {
            log: Some(log),
            failed: None,
        }));
        let output: Arc<Mutex<dyn Output>> = slot.clone();
        let ran = run::run_with(argv, cwd, spec, &output, path);
        let mut slot = slot.lock().unwrap_or_else(PoisonError::into_inner);
        (ran, slot.log.take(), slot.failed.take())
    }
}

impl Output for Slot {
    fn write(&mut self, stream: Stream, bytes: &[u8]) -> io::Result<()> {
        if self.failed.is_some() {
            return Ok(());
        }
        if let Some(log) = &mut self.log
            && let Err(e) = log.write(stream, bytes)
        {
            self.failed = Some(e);
        }
        Ok(())
    }
}

/// Why a check's log at `path` could not be written: `e`, in one line.
fn unwritten(path: &Utf8Path, e: &io::Error) -> String {
    error::one_line(format!("its log could not be written: {path}: {e}"))
}

/// `e`, an `E_IO` writing the record of the run `resp`, with the run in `details.run` and a hint:
/// the checks' outcomes are not lost with the record.
fn record_failed(mut e: OpError, resp: &CheckRunResp) -> OpError {
    let run = serde_json::to_value(resp).unwrap_or(Value::Null);
    e.details.insert("run".to_owned(), run);
    e.with_hint(format!(
        "the run's directory {} could not be written: make room there (or set \
         {CHECK_RUNS_DIR_ENV} elsewhere, for --dir), then run the checks again",
        resp.dir
    ))
}

/// `E_CHECK_FAILED` for the run `resp`: one line naming each check that did not pass, its
/// details the whole result, its hint `check log` for the first, or `check log --failed` when
/// two or more have a log to read, in the run's form `at`.
fn failed(resp: &CheckRunResp, at: &str) -> OpError {
    // 1. One line.
    let not_passed: Vec<&CheckRunOut> = (resp.checks.iter())
        .filter(|c| c.outcome != CheckOutcome::Passed)
        .collect();
    let message = if resp.cancelled {
        let cancelled: Vec<&str> = (not_passed.iter())
            .filter(|c| c.outcome == CheckOutcome::Cancelled)
            .map(|c| c.name.as_str())
            .collect();
        let not_run = not_passed
            .iter()
            .filter(|c| c.outcome == CheckOutcome::NotRun)
            .count();
        let mut parts = Vec::new();
        if !cancelled.is_empty() {
            parts.push(format!("{} cancelled", cancelled.join(", ")));
        }
        parts.push(format!("{not_run} not run"));
        format!("the run was stopped: {}", parts.join(", "))
    } else {
        let each: Vec<String> = not_passed.iter().map(|c| why(c)).collect();
        format!(
            "{} of {} checks did not pass: {}",
            not_passed.len(),
            resp.checks.len(),
            each.join(", ")
        )
    };

    // 2. The details: the run; the hint: the first check's findings, if it has any, and its
    //    log, or the logs of all that did not pass when they are two or more.
    let mut error = error::error(ErrorCode::CheckFailed, error::one_line(message));
    let run = serde_json::to_value(resp).unwrap_or(Value::Null);
    error.details.insert("run".to_owned(), run);
    let mut logged = (resp.checks.iter().enumerate())
        .filter(|(_, c)| c.outcome != CheckOutcome::Passed && c.log.is_some());
    let (first, several) = (logged.next(), logged.next().is_some());
    let together = format!("swarmops check log --failed {at} --run {}", resp.run);
    match first {
        Some((i, check)) if several && !check.findings.is_empty() => error.with_hint(format!(
            "read its findings in details.run.checks[{i}].findings, then the logs: {together}"
        )),
        Some(_) if several => error.with_hint(format!("read their logs: {together}")),
        Some((i, check)) if !check.findings.is_empty() => error.with_hint(format!(
            "read its findings in details.run.checks[{i}].findings, then its log: swarmops \
             check log {} {at} --run {}",
            check.name, resp.run
        )),
        Some((_, check)) => error.with_hint(format!(
            "read its log: swarmops check log {} {at} --run {}",
            check.name, resp.run
        )),
        None => error.with_hint("run the checks again: none of them started"),
    }
}

/// Why `check` did not pass, in a few words, with the count of its findings when it has any.
fn why(check: &CheckRunOut) -> String {
    let name = &check.name;
    let found = match check.findings_total {
        0 => String::new(),
        1 => ", 1 finding".to_owned(),
        n => format!(", {n} findings"),
    };
    match (check.outcome, check.exit_code, check.signal) {
        (CheckOutcome::Failed, _, _) if let Some(message) = &check.message => {
            format!("{name} failed ({message}{found})")
        }
        (CheckOutcome::Failed, Some(code), _) => format!("{name} failed (exit {code}{found})"),
        (CheckOutcome::Failed, _, Some(signal)) => {
            format!("{name} failed (signal {signal}{found})")
        }
        (CheckOutcome::TimedOut, _, _) => {
            format!("{name} timed out ({} s{found})", check.timeout_s)
        }
        (CheckOutcome::NotStarted, _, _) => format!("{name} did not start"),
        (outcome, _, _) => format!("{name} {}", outcome.as_str().replace('_', " ")),
    }
}

/// Replaces `run.json` of the run in `dir` with `resp` in `state`: written beside it, then
/// renamed over it. Not synced: a diagnostic.
fn write_record(dir: &Utf8Path, state: &str, resp: &CheckRunResp) -> Result<(), OpError> {
    let record = json!({"format": RUN_FILE_FORMAT, "state": state, "result": resp});
    let path = dir.join(RUN_FILE);
    let next = dir.join(format!("{RUN_FILE}.next"));
    let mut text = serde_json::to_vec_pretty(&record).unwrap_or_default();
    text.push(b'\n');
    let written = File::create(&next).and_then(|mut file| file.write_all(&text));
    written.map_err(|e| io_failed(&next, &e))?;
    std::fs::rename(&next, &path).map_err(|e| io_failed(&path, &e))
}

/// Removes the runs in `runs` past the newest [`KEPT_CHECK_RUNS`], counting `current`, which
/// stays; a run whose lock is held stays too. A removal that fails is passed over.
fn prune(runs: &Utf8Path, current: &str) {
    let Ok(entries) = runs.read_dir_utf8() else {
        return;
    };
    let mut names: Vec<String> = (entries.flatten())
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
        .map(|entry| entry.file_name().to_owned())
        .filter(|name| name != current && uuid::Uuid::try_parse(name).is_ok())
        .collect();
    names.sort();
    let keep = KEPT_CHECK_RUNS.saturating_sub(1);
    let gone = names.len().saturating_sub(keep);
    for name in &names[..gone] {
        let dir = runs.join(name);
        if held(&dir.join(RUN_LOCK)) {
            continue;
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Removes the key directories of `root`, but `current`, whose `dir` file names a directory that
/// is not there any more and none of whose runs may hold its lock ([`held`]). One whose `dir`
/// file does not read, or whose directory cannot be looked at, stays; a removal that fails is
/// passed over.
fn forget_gone(root: &Utf8Path, current: &Utf8Path) {
    let Ok(entries) = root.read_dir_utf8() else {
        return;
    };
    for entry in entries.flatten() {
        // 1. Another key directory, naming a directory that is gone.
        let key = entry.path();
        if key == current || !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let Ok(named) = std::fs::read_to_string(key.join(DIR_FILE)) else {
            continue;
        };
        let named = named.strip_suffix('\n').unwrap_or(&named);
        let gone =
            std::fs::symlink_metadata(named).is_err_and(|e| e.kind() == io::ErrorKind::NotFound);
        if named.is_empty() || !gone {
            continue;
        }

        // 2. None of its runs running: it goes.
        let Ok(runs) = key.read_dir_utf8() else {
            continue;
        };
        let running = (runs.flatten())
            .filter(|run| run.file_type().is_ok_and(|t| t.is_dir()))
            .any(|run| held(&run.path().join(RUN_LOCK)));
        if !running {
            let _ = std::fs::remove_dir_all(key);
        }
    }
}

/// Whether the lock on `path` may be held by a running run: false only when it is not there or
/// is taken and let go at once. It is opened to read, which a lock needs no more than (the
/// file may be another account's, or read-only); a lock that cannot be opened or tried is
/// taken as held, so that a follower keeps following and the run is never pruned.
pub(crate) fn held(path: &Utf8Path) -> bool {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return false,
        Err(_) => return true,
    };
    match file.try_lock() {
        Ok(()) => {
            let _ = file.unlock();
            false
        }
        Err(TryLockError::WouldBlock | TryLockError::Error(_)) => true,
    }
}

/// `dir`, canonical: links resolved, absolute.
fn canonical(dir: &Utf8Path) -> Result<Utf8PathBuf, OpError> {
    let real = std::fs::canonicalize(dir).map_err(|e| io_failed(dir, &e))?;
    Utf8PathBuf::from_path_buf(real).map_err(|path| {
        invalid(
            format!("the directory {} is not UTF-8", path.display()),
            "dir",
        )
    })
}

/// Makes `dir` and its parents ([`private_dirs`]), or `E_IO`.
fn make_dir(dir: &Utf8Path) -> Result<(), OpError> {
    private_dirs(dir).map_err(|e| io_failed(dir, &e))
}

/// Makes `dir` and its parents, each made here readable by its owner alone ([`RUNS_DIR_MODE`]):
/// a check's log can hold whatever it printed, secrets among them.
fn private_dirs(dir: &Utf8Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    (std::fs::DirBuilder::new()
        .recursive(true)
        .mode(RUNS_DIR_MODE))
    .create(dir)
}

/// `E_INVALID_ARG` saying `what`, with `details.reason`.
fn invalid(what: String, reason: &str) -> OpError {
    let mut error = error::error(ErrorCode::InvalidArg, error::one_line(what));
    error.details.insert("reason".to_owned(), reason.into());
    error
}

/// `list_dir`'s refusal of a `--dir` that names no directory, with `check run`'s hint.
fn rehint_dir(e: OpError) -> OpError {
    if e.details.get("reason").and_then(Value::as_str) == Some("dir") {
        return e.with_hint("name the directory whose checks to run: --dir .");
    }
    e
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

/// The wall clock, in Unix milliseconds.
fn now_ms() -> u64 {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    millis(since)
}

/// `elapsed` in whole milliseconds.
fn millis(elapsed: Duration) -> u64 {
    u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests;
