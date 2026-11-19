//! The `check` commands (M30, M31): a tree's checks, and a run of one stage of them.
//!
//! `check list` reads a tree of the store at a changeset through `dyn StoreOps`, as
//! `swarmops mcp`'s `check_list` does; `check list --dir` reads the files below a directory of
//! the caller's host and opens no store, so `--repo`, `SWARMOPS_REPO` and `--as` are read and not
//! used, and its envelope has no `meta.repo`.
//!
//! `check plan` says which checks of one stage a change needs: a changeset's or a land's on the
//! store, as `swarmops mcp`'s `check_plan` does; a workspace's, through the `ws` commands' store
//! opening; or the changed paths the caller gives with `--dir`, which opens no store and reads
//! the list of paths here, from its file or from standard input.
//!
//! `check run` runs one stage's checks: of the workspace `--ws`, `--ws-path` or the working
//! directory names, through the `ws` commands' own store opening, or of a directory with
//! `--dir`, which opens no store. Before the first check starts it makes SIGINT, SIGTERM and
//! SIGHUP set one flag, and nothing else: each check runs in a process group of its own, which
//! the terminal's signal never reaches, so the runner kills the running check's group when the
//! flag is set and the run ends with its outcomes (`cancelled`, then `not_run`).
//!
//! ```text
//! so check plan REV [--landed]   ─▶ on_store ─▶ check::plan::plan(ops, CheckPlanReq)
//! so check plan --ws N           ─▶ ws::on_named ─▶ check::plan::plan_ws(&WsCtx, …)
//! so check plan --dir DIR --changed P… --changed-from FILE|- [--nul]
//!                                ─▶ check::plan::plan_dir(DIR, stage, checks, Given)
//! so check run [NAME…] --dir DIR ─▶ check::run::run_dir(DIR, stage, names, opts)
//! so check run [NAME…] [--ws N]  ─▶ ws::on_named ─▶ check::run::run_ws(&WsCtx, req, opts)
//!                          opts: --pass names, the stop flag, the environment, the runs root,
//!                                the directory named for the run lines (context::run_lines_dir)
//! so check runs --dir DIR | [--ws N] | --run-lines DIR ─▶ check::runs::runs_dir | runs_at
//!                          (no store opened): a page of the run lines, filtered, newest first
//! so check log CHECK --dir DIR | [--ws N] ─▶ the runs found (no store opened)
//!                                         ─▶ check::follow::Follower: its last lines, and
//!                                            with --follow one envelope a line as they come
//! so check log --failed …                 ─▶ check::follow::failed_logs: the logs of the run's
//!                                            checks that did not pass, in one envelope
//! so check log … --run R --run-lines DIR  ─▶ check::follow::run_through_lines: the runs found
//!                                            by the run's line, in place of --dir or --ws
//! ```
//!
//! `check log --follow` prints more than one envelope: the first with the last `--tail` lines,
//! then one for each look that found whole lines, flushed at once as `derive --follow` writes its
//! envelopes, the last the one that holds the log's end, exit 0. With `--human` it prints the
//! lines as the log holds them, control characters escaped, and nothing else; of several failed
//! checks' logs, a line `==> NAME <==` before each.

use std::collections::BTreeMap;
use std::io::{self, Write as _};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use camino::{Utf8Path, Utf8PathBuf};
use serde_json::Value;
use swarmops_api::{
    CheckListReq, CheckLogLineOut, CheckLogResp, CheckLogStream, CheckPlanReq, CheckRunReq,
    CheckRunsReq, CheckRunsResp, ErrorCode, OpError, Rev,
};
use swarmops_ops::check::changed::{self, Sep};
use swarmops_ops::check::follow::{self, Follower};
use swarmops_ops::check::lines as check_lines;
use swarmops_ops::check::plan::{self, Given};
use swarmops_ops::check::run::{self as check_run, RunOpts, WS_RUNS_DIR};
use swarmops_ops::check::runs;
use swarmops_ws::RealFs;
use swarmops_ws::registry::Registry;
use swarmops_ws::workspace::{self, Locate};

use super::{Ctx, run as commands_run, ws};
use crate::cli::{CheckCommand, OutcomeArg, PlanArgs, RunsArgs, StageArg, WsAt};
use crate::context;
use crate::output::{self, Format, Printed};

/// Runs `check`'s `command`, which names operation `op`.
pub(super) fn run(ctx: &Ctx<'_>, op: &'static str, command: CheckCommand) -> Printed {
    match command {
        CheckCommand::List {
            dir: Some(dir),
            stage,
            ..
        } => {
            let no_store = (None, BTreeMap::new(), None);
            let stage = stage.map(StageArg::stage);
            ctx.finish(op, no_store, swarmops_ops::check::list_dir(&dir, stage))
        }
        CheckCommand::List {
            rev,
            dir: None,
            stage,
        } => ctx.on_store(
            op,
            || {
                let mut req = rev.map_or_else(CheckListReq::default, |rev| {
                    CheckListReq::new(Rev::from(rev))
                });
                req.stage = stage.map(StageArg::stage);
                Ok(req)
            },
            |ops, req| swarmops_ops::check::list(ops, req),
        ),
        plan @ CheckCommand::Plan(_) => plan_checks(ctx, op, plan),
        run @ CheckCommand::Run { .. } => run_checks(ctx, op, run),
        runs @ CheckCommand::Runs(_) => list_runs(ctx, op, runs),
        log @ CheckCommand::Log { .. } => read_log(ctx, op, log),
    }
}

/// `check plan`, which names operation `op`: the plan of a directory's changed paths, with no
/// store; of a workspace, through the `ws` commands' store opening; or of a changeset, on the
/// store.
fn plan_checks(ctx: &Ctx<'_>, op: &'static str, command: CheckCommand) -> Printed {
    let CheckCommand::Plan(args) = command else {
        unreachable!("run passes check plan its own command alone");
    };
    let PlanArgs {
        rev,
        at,
        dir,
        target,
        landed,
        base,
        changed,
        changed_from,
        nul,
        stage,
        checks,
    } = *args;
    let stage = stage.stage();

    // 1. A directory: no store. The list is read here, from its file or from stdin, bounded.
    if let Some(dir) = dir {
        let sep = if nul { Sep::Nul } else { Sep::Line };
        let planned = changed_list(changed_from.as_deref()).and_then(|list| {
            let given = match &list {
                None if changed.is_empty() => Given::None,
                list => Given::Paths {
                    flags: &changed,
                    list: list.as_deref().map(|bytes| (bytes, sep)),
                },
            };
            plan::plan_dir(&dir, stage, &checks, given)
        });
        return ctx.finish(op, (None, BTreeMap::new(), None), planned);
    }

    // 2. A workspace: its store, as the `ws` commands open it.
    if at.name.is_some() || at.ws_path.is_some() {
        return ws::on_named(
            ctx,
            op,
            at,
            |workspace| workspace,
            |wctx, workspace| plan::plan_ws(wctx, &Locate::Name(workspace), stage, &checks),
        );
    }

    // 3. A changeset, or the change as it would land: the store.
    ctx.on_store(
        op,
        || {
            let Some(rev) = rev else {
                let what = "check plan names no changeset, workspace or directory";
                return Err(OpError::new(ErrorCode::InvalidArg, what));
            };
            let mut req = CheckPlanReq::new(Rev::from(rev));
            if let Some(target) = target {
                req.target = target;
            }
            req.landed = landed;
            req.base = base.map(Rev::from);
            req.stage = stage;
            req.checks = checks;
            Ok(req)
        },
        |ops, req| plan::plan(ops, req),
    )
}

/// The bytes of the list of changed paths `from` names, if one is named: a file's, or
/// standard input's for `-`, at most one byte past the bound a plan reads. A list that does
/// not read is `E_IO`: an error, never a plan.
fn changed_list(from: Option<&Utf8Path>) -> Result<Option<Vec<u8>>, OpError> {
    match from {
        None => Ok(None),
        Some(stdin) if stdin == STDIN => changed::read_bounded(io::stdin().lock())
            .map(Some)
            .map_err(|e| changed::unread(STDIN, &e)),
        Some(file) => changed::read_from(file).map(Some),
    }
}

/// What `--changed-from` names for standard input.
const STDIN: &str = "-";

/// `check run`, which names operation `op`: the checks of a directory, with no store, or of a
/// workspace, through the `ws` commands' store opening.
fn run_checks(ctx: &Ctx<'_>, op: &'static str, command: CheckCommand) -> Printed {
    let CheckCommand::Run {
        at,
        dir,
        stage,
        pass,
        run_lines,
        names,
    } = command
    else {
        unreachable!("run passes check run its own command alone");
    };

    // 1. Where the run lines go, if the caller says; then the flag the signals set, before any
    //    check starts.
    let lines = context::run_lines_dir(run_lines.as_deref(), ctx.env);
    let (lines, stop) = match lines.and_then(|lines| Ok((lines, stop_on_signals()?))) {
        Ok(ready) => ready,
        Err(e) => return ctx.finish(op, ctx.used.meta(), Err::<(), _>(e)),
    };
    let stage = stage.stage();

    // 2. A directory: no store.
    if let Some(dir) = dir {
        let ran = context::check_runs_root(ctx.env).and_then(|runs_root| {
            let mut opts = RunOpts::new(ctx.env);
            opts.pass = pass;
            opts.stop = Some(&stop);
            opts.runs_root = runs_root;
            opts.lines = lines;
            check_run::run_dir(&dir, stage, &names, &opts)
        });
        return ctx.finish(op, (None, BTreeMap::new(), None), ran);
    }

    // 3. A workspace: its store, as the `ws` commands open it.
    ws::on_named(
        ctx,
        op,
        at,
        |workspace| {
            let mut req = CheckRunReq::new(workspace);
            req.stage = stage;
            req.checks = names;
            req
        },
        |wctx, req| {
            let mut opts = RunOpts::new(ctx.env);
            opts.pass = pass;
            opts.stop = Some(&stop);
            opts.lines = lines;
            check_run::run_ws(wctx, req, &opts)
        },
    )
}

/// `check runs`, which names operation `op`: a page of the run lines of a directory, of a
/// workspace, or of the directory the caller names. No store is opened.
fn list_runs(ctx: &Ctx<'_>, op: &'static str, command: CheckCommand) -> Printed {
    let CheckCommand::Runs(args) = command else {
        unreachable!("run passes check runs its own command alone");
    };
    let RunsArgs {
        at,
        dir,
        run_lines,
        checks,
        tree,
        outcomes,
        run,
        since,
        until,
        limit,
        cursor,
    } = *args;

    // 1. The request: the filters and the page, as the tool takes them. The workspace's name
    //    is not read here: the lines' directory is found below.
    let mut req = CheckRunsReq::new(at.name.clone().unwrap_or_default());
    req.checks = checks;
    req.tree = tree;
    req.outcomes = outcomes.into_iter().map(OutcomeArg::outcome).collect();
    req.run = run;
    req.since = since;
    req.until = until;
    req.limit = limit;
    req.cursor = cursor;

    // 2. The lines: where the caller says, else beside the directory's runs, else in the
    //    workspace's state.
    let page = context::run_lines_dir(run_lines.as_deref(), ctx.env)
        .and_then(|named| lines_page(ctx, &at, dir.as_deref(), named.as_deref(), &req));
    let meta = match dir {
        Some(_) => (None, BTreeMap::new(), None),
        None => ctx.used.meta(),
    };
    ctx.finish(op, meta, page)
}

/// The page `req` asks of the run lines in the directory `named`, if the caller named one;
/// else of those beside the runs of `dir`; else of those of the workspace `at` names.
fn lines_page(
    ctx: &Ctx<'_>,
    at: &WsAt,
    dir: Option<&Utf8Path>,
    named: Option<&Utf8Path>,
    req: &CheckRunsReq,
) -> Result<CheckRunsResp, OpError> {
    match (dir, named) {
        (Some(dir), _) => {
            let root = context::check_runs_root(ctx.env)?;
            runs::runs_dir(dir, root.as_deref(), named, req)
        }
        (None, Some(named)) => runs::runs_at(named, true, req),
        (None, None) => runs::runs_at(&ws_runs(ctx, at)?, false, req),
    }
}

/// `check log`, which names operation `op`: a check's log of a run, once or followed, or the
/// logs of the run's checks that did not pass.
fn read_log(ctx: &Ctx<'_>, op: &'static str, command: CheckCommand) -> Printed {
    let CheckCommand::Log {
        at,
        dir,
        check,
        failed,
        run_lines,
        run,
        tail,
        follow,
        interval_ms,
    } = command
    else {
        unreachable!("run passes check log its own command alone");
    };

    // 1. The runs: found through the run's line, or of the directory, or of the workspace.
    let through = run_lines.as_deref().zip(run.as_deref());
    let (runs, named) = match (through, &dir) {
        (Some((lines_dir, run)), _) => {
            let found = check_lines::named(Some(lines_dir), None)
                .and_then(|lines_dir| lines_dir.ok_or_else(no_lines_dir))
                .and_then(|lines_dir| follow::run_through_lines(&lines_dir, run));
            (found, format!("--run-lines {lines_dir}"))
        }
        (None, Some(dir)) => {
            let found = context::check_runs_root(ctx.env)
                .and_then(|root| follow::dir_runs(dir, root.as_deref()));
            (found, named_at(Some(dir), &at))
        }
        (None, None) => (ws_runs(ctx, &at), named_at(None, &at)),
    };

    // 2. The failed checks' logs, in one envelope.
    let Some(check) = check.filter(|_| !failed) else {
        let logs = runs.and_then(|runs| follow::failed_logs(&runs, &named, run.as_deref(), tail));
        return log_printed(ctx, op, logs);
    };

    // 3. The check's log: its last lines; with --follow, the rest as it comes.
    let started =
        runs.and_then(|runs| Follower::start(&runs, &named, &check, run.as_deref(), tail, follow));
    let (follower, first) = match started {
        Ok(started) => started,
        Err(e) => return ctx.finish(op, ctx.used.meta(), Err::<(), _>(e)),
    };
    if !follow || first.ended {
        return log_printed(ctx, op, Ok(first));
    }
    follow_log(
        ctx,
        op,
        follower,
        &first,
        Duration::from_millis(interval_ms),
    )
}

/// How the command line named the runs' directory or workspace, as `check run` takes it: `--dir
/// DIR`, `--ws NAME`, `--ws-path DIR`, or nothing for the workspace around the working directory.
fn named_at(dir: Option<&Utf8Path>, at: &WsAt) -> String {
    match (dir, &at.name, &at.ws_path) {
        (Some(dir), _, _) => format!("--dir {dir}"),
        (None, Some(name), _) => format!("--ws {name}"),
        (None, None, Some(path)) => format!("--ws-path {path}"),
        (None, None, None) => String::new(),
    }
}

/// The runs of the workspace `at` names, found as the `ws` commands find it: by name through
/// the registry beside the store named, or by its directory. No store is opened.
fn ws_runs(ctx: &Ctx<'_>, at: &WsAt) -> Result<Utf8PathBuf, OpError> {
    let start = ws::lookup_of(at)?;
    let store = context::store_for(ctx.global.repo.as_deref(), ctx.env, start.as_ref())?;
    if let Some(store) = &store {
        ctx.used.repo.replace(Some(store.clone()));
    }
    let locate = match (&at.name, start) {
        (Some(name), _) => Locate::Name(name.clone()),
        (None, Some(start)) => start,
        (None, None) => {
            return Err(OpError::new(
                ErrorCode::InvalidArg,
                "the working directory is not UTF-8: name the workspace with --ws or --ws-path",
            ));
        }
    };
    let fs = Arc::new(RealFs::new());
    let registry = store.map(|store| Registry::open(Utf8Path::new(&store), fs.clone()));
    let found = workspace::find(&locate, registry.as_ref(), fs.as_ref())?;
    Ok(found.dir.join(WS_RUNS_DIR))
}

/// `check log --follow`: `first`, printed at once, then each look's lines as they come, every
/// `interval` while there is nothing new, until the envelope that holds the log's end, exit 0.
/// A look that fails ends it with its envelope; output that cannot be written ends it with exit
/// code 1, said on stderr.
fn follow_log(
    ctx: &Ctx<'_>,
    op: &str,
    mut follower: Follower,
    first: &CheckLogResp,
    interval: Duration,
) -> Printed {
    let mut next = Some(first.clone());
    loop {
        // 1. What the last look found, at once.
        if let Some(resp) = next.take() {
            let ended = resp.ended;
            let printed = log_printed(ctx, op, Ok(resp));
            if let Err(e) = commands_run::write_now(&printed.stdout) {
                let _ = writeln!(io::stderr(), "swarmops: cannot write the envelope: {e}");
                return Printed {
                    stdout: String::new(),
                    exit: 1,
                };
            }
            if ended {
                return Printed {
                    stdout: String::new(),
                    exit: 0,
                };
            }
        }

        // 2. The next look, after a wait when there was nothing new.
        match follower.look() {
            Ok(Some(resp)) => next = Some(resp),
            Ok(None) => std::thread::sleep(interval),
            Err(e) => return ctx.finish(op, ctx.used.meta(), Err::<(), _>(e)),
        }
    }
}

/// The answer of `check log`, printed: in the human format the log's lines as the log holds
/// them, each control character escaped (`output::printable`, so that what a check printed cannot
/// drive the terminal), and nothing else; else its envelope. Several failed checks' logs come
/// one after another, a line `==> NAME <==` before each; one log comes with no such line.
fn log_printed(ctx: &Ctx<'_>, op: &str, resp: Result<CheckLogResp, OpError>) -> Printed {
    let resp = match (ctx.format, resp) {
        (Format::Human, Ok(resp)) => resp,
        (_, resp) => return ctx.finish(op, ctx.used.meta(), resp),
    };
    let printed = |lines: &[CheckLogLineOut]| -> String {
        (lines.iter())
            .map(|line| output::printable(&as_logged(line)) + "\n")
            .collect()
    };
    let mut stdout = String::new();
    if resp.more.is_empty() {
        stdout.push_str(&printed(&resp.lines));
    } else {
        let logs = std::iter::once((&resp.check, &resp.lines))
            .chain(resp.more.iter().map(|log| (&log.check, &log.lines)));
        for (check, lines) in logs {
            let heading = output::printable(&format!("==> {check} <=="));
            stdout.push_str(&heading);
            stdout.push('\n');
            stdout.push_str(&printed(lines));
        }
    }
    Printed { stdout, exit: 0 }
}

/// `E_INVALID_ARG` for a `--run-lines` that named no directory.
fn no_lines_dir() -> OpError {
    OpError::new(ErrorCode::InvalidArg, "--run-lines names no directory")
}

/// `line` as the log holds it: its time, its stream and its text; a line of `other` as it is.
fn as_logged(line: &CheckLogLineOut) -> String {
    let kind = match line.stream {
        CheckLogStream::Out => "out",
        CheckLogStream::Err => "err",
        CheckLogStream::Run => "run",
        _ => return line.text.clone(),
    };
    let ms = line.at_ms.unwrap_or_default();
    format!("{:>5}.{:03} {kind} {}", ms / 1_000, ms % 1_000, line.text)
}

/// A flag that SIGINT, SIGTERM and SIGHUP set, and nothing else: from now on none of them ends
/// the process, and a second changes nothing. `E_IO` when one cannot be registered.
fn stop_on_signals() -> Result<Arc<AtomicBool>, OpError> {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};

    let stop = Arc::new(AtomicBool::new(false));
    for signal in [SIGINT, SIGTERM, SIGHUP] {
        signal_hook::flag::register(signal, Arc::clone(&stop)).map_err(|e| {
            let mut error = OpError::new(
                ErrorCode::Io,
                format!("cannot catch signal {signal} for the run: {e}"),
            );
            error
                .details
                .insert("signal".to_owned(), Value::from(signal));
            error
        })?;
    }
    Ok(stop)
}
