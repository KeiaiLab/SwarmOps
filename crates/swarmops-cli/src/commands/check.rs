//! The `check` commands (M30, M31): a tree's checks, and a run of one stage of them.
//!
//! `check list` reads a tree of the store at a changeset through `dyn StoreOps`, as
//! `swarmops mcp`'s `check_list` does; `check list --dir` reads the files below a directory of
//! the caller's host and opens no store, so `--repo`, `SWARMOPS_REPO` and `--as` are read and not
//! used, and its envelope has no `meta.repo`.
//!
//! `check run` runs one stage's checks: of the workspace `--ws`, `--ws-path` or the working
//! directory names, through the `ws` commands' own store opening, or of a directory with
//! `--dir`, which opens no store. Before the first check starts it makes SIGINT, SIGTERM and
//! SIGHUP set one flag, and nothing else: each check runs in a process group of its own, which
//! the terminal's signal never reaches, so the runner kills the running check's group when the
//! flag is set and the run ends with its outcomes (`cancelled`, then `not_run`).
//!
//! ```text
//! so check run [NAME…] --dir DIR ─▶ check::run::run_dir(DIR, stage, names, opts)
//! so check run [NAME…] [--ws N]  ─▶ ws::on_named ─▶ check::run::run_ws(&WsCtx, req, opts)
//!                          opts: --pass names, the stop flag, the environment, the runs root
//! so check log CHECK --dir DIR | [--ws N] ─▶ the runs found (no store opened)
//!                                         ─▶ check::follow::Follower: its last lines, and
//!                                            with --follow one envelope a line as they come
//! ```
//!
//! `check log --follow` prints more than one envelope: the first with the last `--tail` lines,
//! then one for each look that found whole lines, flushed at once as `derive --follow` writes its
//! envelopes, the last the one that holds the log's end, exit 0. With `--human` it prints the
//! lines as the log holds them, and nothing else.

use std::collections::BTreeMap;
use std::io::{self, Write as _};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use camino::{Utf8Path, Utf8PathBuf};
use serde_json::Value;
use swarmops_api::{
    CheckListReq, CheckLogResp, CheckLogStream, CheckRunReq, ErrorCode, OpError, Rev,
};
use swarmops_ops::check::follow::{self, Follower};
use swarmops_ops::check::run::{self as check_run, RunOpts, WS_RUNS_DIR};
use swarmops_ws::RealFs;
use swarmops_ws::registry::Registry;
use swarmops_ws::workspace::{self, Locate};

use super::{Ctx, run as commands_run, ws};
use crate::cli::{CheckCommand, StageArg, WsAt};
use crate::context;
use crate::output::{Format, Printed};

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
        CheckCommand::Run {
            at,
            dir,
            stage,
            pass,
            names,
        } => {
            // 1. The flag the signals set, before any check starts.
            let stop = match stop_on_signals() {
                Ok(stop) => stop,
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
                    check_run::run_ws(wctx, req, &opts)
                },
            )
        }
        CheckCommand::Log {
            at,
            dir,
            check,
            run,
            tail,
            follow,
            interval_ms,
        } => {
            // 1. The runs: of the directory, or of the workspace.
            let runs = match &dir {
                Some(dir) => context::check_runs_root(ctx.env)
                    .and_then(|root| follow::dir_runs(dir, root.as_deref())),
                None => ws_runs(ctx, &at),
            };

            // 2. The log's last lines; with --follow, the rest as it comes.
            let started =
                runs.and_then(|runs| Follower::start(&runs, &check, run.as_deref(), tail, follow));
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
/// them and nothing else; else its envelope.
fn log_printed(ctx: &Ctx<'_>, op: &str, resp: Result<CheckLogResp, OpError>) -> Printed {
    match (ctx.format, resp) {
        (Format::Human, Ok(resp)) => Printed {
            stdout: (resp.lines.iter())
                .map(|line| as_logged(line) + "\n")
                .collect(),
            exit: 0,
        },
        (_, resp) => ctx.finish(op, ctx.used.meta(), resp),
    }
}

/// `line` as the log holds it: its time, its stream and its text; a line of `other` as it is.
fn as_logged(line: &swarmops_api::CheckLogLineOut) -> String {
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
