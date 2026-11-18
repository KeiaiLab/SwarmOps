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
//! ```

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use serde_json::Value;
use swarmops_api::{CheckListReq, CheckRunReq, ErrorCode, OpError, Rev};
use swarmops_ops::check::run::{self as check_run, RunOpts};

use super::{Ctx, ws};
use crate::cli::{CheckCommand, StageArg};
use crate::context;
use crate::output::Printed;

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
    }
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
