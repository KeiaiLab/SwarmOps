//! The `ws` commands (M5): a workspace's operations from the command line, and the Claude Code
//! hooks. Each command finds its store and its workspace (M5 design §2.5), builds the request
//! from its flags, and runs the `swarmops_ws` function of its name on the store it opened.
//!
//! ```text
//! so ws <verb> ─▶ store: --repo, $SWARMOPS_REPO, else the workspace's `repo`
//!               └▶ workspace: --ws NAME, --ws-path DIR, else the one around the working directory
//!                    └▶ swarmops_ws::<verb>(&Ctx, request) ─▶ the envelope
//!
//! so ws hook worktree-create < json ─▶ stdout: the path   (errors: an envelope on stderr)
//! so ws hook worktree-remove < json ─▶ stderr: head, rescue
//! ```

use std::collections::BTreeMap;
use std::io::{self, Read as _, Write as _};
use std::path::Path;
use std::sync::Arc;

use camino::{Utf8Path, Utf8PathBuf};
use serde::Serialize;
use swarmops_api::{
    DiffFormat, ErrorCode, Identity, InfoReq, LandMode, ModeInput, OpError, Side, StatusAfter,
    StoreOps, WsCheckInput, WsCommitReq, WsCreateReq, WsDeleteReq, WsDiffReq, WsDiscardReq, WsInfo,
    WsInfoReq, WsLandReq, WsListReq, WsMaterializeReq, WsRebaseAction, WsRebaseReq, WsResolveReq,
    WsStatusReq,
};
use swarmops_ops::LocalOps;
use swarmops_ws::hook::{self, Bootstrap, MAX_HOOK_INPUT};
use swarmops_ws::workspace::{Locate, Workspace, find, host_path};
use swarmops_ws::{Ctx as WsCtx, RealFs, RealHost, bootstrap};

use super::Ctx;
use crate::cli::{
    DiffFormatArg, HookVerb, LandModeArg, ModeArg, SideArg, StatusAfterArg, WsAfter, WsAt,
    WsCommand,
};
use crate::context;
use crate::output::{self, Printed};

/// Runs a `ws` subcommand, which names operation `op`.
pub(super) fn run(ctx: &Ctx<'_>, op: &'static str, command: WsCommand) -> Printed {
    match command {
        WsCommand::Create {
            name,
            path,
            base,
            prefixes,
            all,
            from,
            after,
        } => create(ctx, op, name, path, base, prefixes, all, from, after),
        WsCommand::Status {
            at,
            paths,
            full,
            limit,
            cursor,
        } => on_named(
            ctx,
            op,
            at,
            |workspace| WsStatusReq {
                workspace,
                paths,
                full,
                limit,
                cursor,
            },
            swarmops_ws::status,
        ),
        WsCommand::Diff {
            at,
            paths,
            format,
            context,
            limit,
            cursor,
            max_bytes,
        } => on_named(
            ctx,
            op,
            at,
            |workspace| WsDiffReq {
                workspace,
                paths,
                format: match format {
                    DiffFormatArg::Names => DiffFormat::Names,
                    DiffFormatArg::Stat => DiffFormat::Stat,
                    DiffFormatArg::Unified => DiffFormat::Unified,
                },
                context,
                limit,
                cursor,
                max_bytes,
            },
            swarmops_ws::diff,
        ),
        WsCommand::List { check } => on_ws(
            ctx,
            op,
            Ok(here_lookup()),
            || Ok(WsListReq { check }),
            swarmops_ws::list,
        ),
        WsCommand::Info { at } => on_named(
            ctx,
            op,
            at,
            |workspace| WsInfoReq { workspace },
            swarmops_ws::info,
        ),
        WsCommand::Delete {
            name,
            force,
            rescue,
        } => on_ws(
            ctx,
            op,
            Ok(here_lookup()),
            || {
                Ok(WsDeleteReq {
                    workspace: name,
                    force,
                    rescue,
                })
            },
            swarmops_ws::delete,
        ),
        WsCommand::Hook { verb } => hook_command(ctx, op, verb),
        other => run_edit(ctx, op, other),
    }
}

/// Runs the `ws` subcommands that change a workspace's files or history.
fn run_edit(ctx: &Ctx<'_>, op: &'static str, command: WsCommand) -> Printed {
    match command {
        WsCommand::Commit {
            at,
            message,
            paths,
            modes,
            after,
        } => on_named(
            ctx,
            op,
            at,
            |workspace| WsCommitReq {
                workspace,
                message,
                paths,
                modes: modes
                    .into_iter()
                    .map(|(path, kind)| (path, mode_input(kind)))
                    .collect::<BTreeMap<_, _>>(),
                status_after: status_after(after),
            },
            swarmops_ws::commit,
        ),
        WsCommand::Rebase {
            at,
            onto,
            message,
            resume,
            abort,
            after,
        } => on_named(
            ctx,
            op,
            at,
            |workspace| WsRebaseReq {
                workspace,
                onto,
                action: match (resume, abort) {
                    (true, _) => WsRebaseAction::Continue,
                    (_, true) => WsRebaseAction::Abort,
                    _ => WsRebaseAction::Start,
                },
                message,
                status_after: status_after(after),
            },
            swarmops_ws::rebase,
        ),
        WsCommand::Resolve {
            at,
            paths,
            all,
            take,
            force,
            after,
        } => on_named(
            ctx,
            op,
            at,
            |workspace| WsResolveReq {
                workspace,
                paths,
                all,
                take: take.map(|side| match side {
                    SideArg::Ours => Side::Ours,
                    SideArg::Theirs => Side::Theirs,
                }),
                force,
                status_after: status_after(after),
            },
            swarmops_ws::resolve,
        ),
        other => run_tree(ctx, op, other),
    }
}

/// Runs the `ws` subcommands that land a workspace or change the tree on disk.
fn run_tree(ctx: &Ctx<'_>, op: &'static str, command: WsCommand) -> Printed {
    match command {
        WsCommand::Land {
            at,
            message,
            key,
            target,
            check,
            check_timeout,
            mode,
            max_attempts,
            argv,
            after,
        } => on_named(
            ctx,
            op,
            at,
            |workspace| WsLandReq {
                workspace,
                message,
                idempotency_key: key,
                target,
                check: check.then(|| WsCheckInput {
                    argv: (!argv.is_empty()).then_some(argv),
                    timeout_s: check_timeout,
                }),
                mode: mode.map(|mode| match mode {
                    LandModeArg::Merge => LandMode::Merge,
                    LandModeArg::Exact => LandMode::Exact,
                }),
                max_attempts,
                status_after: status_after(after),
            },
            swarmops_ws::land,
        ),
        WsCommand::Materialize { at, paths, after } => on_named(
            ctx,
            op,
            at,
            |workspace| WsMaterializeReq {
                workspace,
                paths,
                status_after: status_after(after),
            },
            swarmops_ws::materialize,
        ),
        WsCommand::Discard { at, paths, after } => on_named(
            ctx,
            op,
            at,
            |workspace| WsDiscardReq {
                workspace,
                paths,
                status_after: status_after(after),
            },
            swarmops_ws::discard,
        ),
        other => unreachable!("{other:?} is run by another function of this module"),
    }
}

/// The workspace context of one call: the store opened for it, the root it was opened at
/// (absolute, so that the registry and the workspaces' `repo` hold a path from anywhere), the
/// caller's identity if there is one, and the host's two file layers.
pub(crate) fn context<'a>(
    store: &'a dyn StoreOps,
    root: &Utf8Path,
    who: Option<&Identity>,
) -> WsCtx<'a> {
    let absolute = std::path::absolute(root.as_std_path())
        .ok()
        .and_then(|path| Utf8PathBuf::from_path_buf(path).ok())
        .unwrap_or_else(|| root.to_owned());
    WsCtx {
        store,
        store_root: absolute,
        who: who.cloned(),
        fs: Arc::new(RealFs),
        host: Arc::new(RealHost),
    }
}

/// Makes the workspace `req` names, then bootstraps it when this call made it and `how` runs
/// it (M5 design §7.3): the files `req.from`'s `.worktreeinclude` lists, then the repository's
/// command. Shared by `so ws create` and the `ws_create` tool.
pub(crate) fn create_bootstrapped(
    ctx: &WsCtx<'_>,
    req: WsCreateReq,
    how: Bootstrap,
) -> Result<WsInfo, OpError> {
    // 1. `from` is checked before anything is made, so that a bad one leaves nothing.
    let from = req
        .from
        .as_deref()
        .map(|from| host_path(Path::new(from)))
        .transpose()?;

    // 2. The workspace.
    let made = swarmops_ws::create(ctx, req)?;
    if !made.created || how == Bootstrap::Skip {
        return Ok(made);
    }

    // 3. Its bootstrap; a failure leaves the workspace for inspection.
    let ws = Workspace::open(ctx, Locate::Path(Utf8PathBuf::from(&made.path)))?;
    bootstrap::run(ctx, &ws, from.as_deref())?;
    Ok(made)
}

/// `ws create`.
#[expect(
    clippy::too_many_arguments,
    reason = "one argument per flag of the command"
)]
fn create(
    ctx: &Ctx<'_>,
    op: &'static str,
    name: String,
    path: Option<String>,
    base: String,
    prefixes: Vec<String>,
    all: bool,
    from: Option<String>,
    after: WsAfter,
) -> Printed {
    on_ws(
        ctx,
        op,
        Ok(here_lookup()),
        || {
            // The directory defaults to NAME in the working directory; a relative one is
            // relative to it too.
            context::mutator(ctx.global.agent.as_deref(), ctx.env)?;
            let dir = match path {
                Some(path) => absolute(&path)?,
                None => absolute(&name)?,
            };
            let from = from.map(|from| absolute(&from)).transpose()?;
            let how = context::bootstrap(ctx.env)?;
            let req = WsCreateReq {
                name,
                path: Some(dir.into_string()),
                base: base.into(),
                prefixes,
                all,
                from: from.map(Utf8PathBuf::into_string),
                session: None,
                status_after: status_after(after),
            };
            Ok((req, how))
        },
        |wctx, (req, how)| create_bootstrapped(wctx, req, how),
    )
}

/// Runs a workspace command that names its workspace: `build` makes the request from the
/// workspace's name, which `at` gives, or else the workspace around the working directory (or
/// `--ws-path`) does.
fn on_named<R, T: Serialize>(
    ctx: &Ctx<'_>,
    op: &'static str,
    at: WsAt,
    build: impl FnOnce(String) -> R,
    call: impl FnOnce(&WsCtx<'_>, R) -> Result<T, OpError>,
) -> Printed {
    let start = lookup_of(&at);
    let locate = start.clone();
    on_ws(
        ctx,
        op,
        start,
        || Ok(()),
        |wctx, ()| {
            let name = match at.name {
                Some(name) => name,
                None => name_at(wctx, locate?)?,
            };
            call(wctx, build(name))
        },
    )
}

/// Runs a workspace operation on the store, opened: the identity is the caller's if there is
/// one, `prepare` builds what the call needs without the store, and `start` is where a store is
/// looked for when none is named.
fn on_ws<P, T: Serialize>(
    ctx: &Ctx<'_>,
    op: &'static str,
    start: Result<Option<Locate>, OpError>,
    prepare: impl FnOnce() -> Result<P, OpError>,
    call: impl FnOnce(&WsCtx<'_>, P) -> Result<T, OpError>,
) -> Printed {
    let agent = ctx.global.agent.as_deref();
    let start = match start {
        Ok(start) => start,
        Err(e) => return ctx.finish(op, ctx.used.meta(), Err::<(), _>(e)),
    };
    let envelope = ctx.envelope_found_from(
        op,
        start.as_ref(),
        || Ok((context::identity(agent, ctx.env)?, prepare()?)),
        |ops, root, (who, input)| {
            let wctx = context(&ops, root, who.as_ref());
            call(&wctx, input)
        },
    );
    output::render(&envelope, ctx.format)
}

/// The workspace around the working directory, as a lookup.
fn here_lookup() -> Option<Locate> {
    context::here().map(Locate::Cwd)
}

/// Where a store is looked for, and a workspace found, for the selector `at`: `--ws-path`'s
/// directory, else the working directory. `Err` for a directory that is no path the workspace
/// crate takes.
fn lookup_of(at: &WsAt) -> Result<Option<Locate>, OpError> {
    match &at.ws_path {
        Some(dir) => Ok(Some(Locate::Path(absolute(dir)?))),
        None => Ok(here_lookup()),
    }
}

/// The name of the workspace `at` finds, opened against the store: a workspace of another store
/// is refused here (`E_INVALID_ARG`, M5 design §2.5), and none is `E_WS_NOT_FOUND`.
fn name_at(ctx: &WsCtx<'_>, at: Option<Locate>) -> Result<String, OpError> {
    let Some(at) = at else {
        return Err(OpError::new(
            ErrorCode::InvalidArg,
            "the working directory is not UTF-8: name the workspace with --ws or --ws-path",
        ));
    };
    Ok(Workspace::open(ctx, at)?.file.name)
}

/// `path` as the absolute path the workspace crate takes: relative to the working directory.
fn absolute(path: &str) -> Result<Utf8PathBuf, OpError> {
    let given = Path::new(path);
    if given.is_absolute() {
        return host_path(given);
    }
    let Some(here) = context::here() else {
        return Err(OpError::new(
            ErrorCode::InvalidArg,
            format!("{path:?} is relative and the working directory is not UTF-8"),
        ));
    };
    host_path(here.join(path).as_std_path())
}

/// `--status-after`, as the request has it.
fn status_after(after: WsAfter) -> StatusAfter {
    match after.status_after {
        StatusAfterArg::Page => StatusAfter::Page,
        StatusAfterArg::None => StatusAfter::None,
    }
}

/// `--mode PATH=KIND`'s kind, as a commit has it.
fn mode_input(kind: ModeArg) -> ModeInput {
    match kind {
        ModeArg::File => ModeInput::File,
        ModeArg::Exec => ModeInput::Exec,
        ModeArg::Symlink => ModeInput::Symlink,
    }
}

// ---- The hooks ----

/// What a hook prints: a line on stdout, or a note on stderr.
enum Said {
    /// The path the create hook gives Claude Code, alone on stdout.
    Stdout(String),
    /// What the remove hook reports, on stderr; stdout stays empty.
    Stderr(String),
}

/// `ws hook`: stdout carries what the hook expects and nothing else, and every failure is an
/// envelope on stderr with its exit code (M5 design §7).
fn hook_command(ctx: &Ctx<'_>, op: &'static str, verb: HookVerb) -> Printed {
    let said = match verb {
        HookVerb::Config => {
            return Printed {
                stdout: hook::config(),
                exit: 0,
            };
        }
        HookVerb::WorktreeCreate => hook_create(ctx),
        HookVerb::WorktreeRemove => hook_remove(ctx),
    };
    match said {
        Ok(Said::Stdout(line)) => Printed {
            stdout: format!("{line}\n"),
            exit: 0,
        },
        Ok(Said::Stderr(note)) => {
            let _ = writeln!(io::stderr(), "{note}");
            Printed {
                stdout: String::new(),
                exit: 0,
            }
        }
        Err(failed) => {
            let printed = ctx.finish(op, ctx.used.meta(), Err::<(), _>(failed));
            let _ = io::stderr().write_all(printed.stdout.as_bytes());
            Printed {
                stdout: String::new(),
                exit: printed.exit,
            }
        }
    }
}

/// The hook's stdin: at most one byte past [`MAX_HOOK_INPUT`], which the parser then refuses.
fn read_stdin() -> Result<Vec<u8>, OpError> {
    let mut bytes = Vec::new();
    let limit = u64::try_from(MAX_HOOK_INPUT).unwrap_or(u64::MAX) + 1;
    io::stdin()
        .lock()
        .take(limit)
        .read_to_end(&mut bytes)
        .map_err(|e| OpError::new(ErrorCode::Io, format!("read the hook's input: {e}")))?;
    Ok(bytes)
}

/// `worktree-create`: the store is the one the working directory's workspace names (a subagent
/// inherits its parent's), else `--repo` / `SWARMOPS_REPO`; both naming different stores is
/// refused (M5 design §7.1 step 2).
fn hook_create(ctx: &Ctx<'_>) -> Result<Said, OpError> {
    // 1. The input, and the two places a store can come from.
    let input = hook::read_create(&read_stdin()?)?;
    let parent = match find(&Locate::Cwd(input.cwd.clone()), None, &RealFs) {
        Ok(found) => Some(found.file),
        Err(e) if e.code == ErrorCode::WsNotFound.as_str() => None,
        Err(e) => return Err(e),
    };
    let named = context::store_named(ctx.global.repo.as_deref(), ctx.env)?;

    // 2. The parent's store wins; a different store named beside it is refused.
    let store = match (&parent, named) {
        (Some(file), Some(named)) => {
            refuse_other_store(&file.repo_id, &file.repo, &named)?;
            file.repo.clone()
        }
        (Some(file), None) => file.repo.clone(),
        (None, Some(named)) => named,
        (None, None) => return Err(context::no_store_named(ctx.global.repo.as_deref())),
    };

    // 3. Where workspaces go, whether to bootstrap, and as whom.
    let ws_root = context::ws_root(ctx.env)?;
    let how = context::bootstrap(ctx.env)?;

    // 4. The workspace, made on that store.
    let path = on_store(ctx, &store, |wctx| {
        hook::worktree_create(wctx, &input, &ws_root, how)
    })?;
    Ok(Said::Stdout(path.to_string()))
}

/// `worktree-remove`: a directory that is no workspace of ours is left alone (exit 0, a note).
fn hook_remove(ctx: &Ctx<'_>) -> Result<Said, OpError> {
    // 1. The input, and whether the directory is ours; no store is opened to ask.
    let input = hook::read_remove(&read_stdin()?)?;
    let Some(found) = hook::ours(&input, &RealFs)? else {
        let note = format!(
            "swarmops: {} is not a SwarmOps workspace; left alone",
            input.worktree_path
        );
        return Ok(Said::Stderr(note));
    };

    // 2. Removed on the store the workspace names.
    let report = on_store(ctx, &found.file.repo, |wctx| {
        hook::worktree_remove(wctx, &input)
    })?;
    let note = match report.rescued {
        Some(rescued) => format!(
            "swarmops: removed workspace {}; head {}, uncommitted edits kept as {}",
            found.file.name, report.head, rescued
        ),
        None => format!(
            "swarmops: removed workspace {}; head {}",
            found.file.name, report.head
        ),
    };
    Ok(Said::Stderr(note))
}

/// `E_INVALID_ARG{workspace_repo_id, store_repo_id}` when the store at `named` is not the one
/// whose id is `repo_id`; `repo` is the workspace's own path to its store, which needs no check.
fn refuse_other_store(repo_id: &str, repo: &str, named: &str) -> Result<(), OpError> {
    if repo == named {
        return Ok(());
    }
    let ops = LocalOps::open(Utf8Path::new(named))?;
    let store_repo_id = ops.repo_info(InfoReq {})?.repo_id;
    if store_repo_id == repo_id {
        return Ok(());
    }
    Err(OpError::new(
        ErrorCode::InvalidArg,
        format!(
            "the workspace around the working directory belongs to another store than the one \
             named (its repo_id {repo_id}, this store's {store_repo_id})"
        ),
    )
    .with_details(serde_json::Map::from_iter([
        ("workspace_repo_id".to_owned(), repo_id.into()),
        ("store_repo_id".to_owned(), store_repo_id.into()),
    ])))
}

/// Opens the store at `store`, as the user's identity if there is one, and runs `call` with the
/// workspace context on it. Its error is the hook's.
fn on_store<T>(
    ctx: &Ctx<'_>,
    store: &str,
    call: impl FnOnce(&WsCtx<'_>) -> Result<T, OpError>,
) -> Result<T, OpError> {
    // 1. The identity, if any; a bad one is the hook's failure.
    let who = context::identity(ctx.global.agent.as_deref(), ctx.env)?;

    // 2. The store opened, and the call on it. The context is the hook's own `Ctx` naming that
    //    store, so that `meta.repo` and a panic's envelope say which.
    ctx.used.repo.replace(Some(store.to_owned()));
    let root = Utf8Path::new(store);
    let local = LocalOps::open(root)?;
    let recording = local.recording(&ctx.used.resolved);
    call(&context(&recording, root, who.as_ref()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A relative path is taken from the working directory; an absolute one stays.
    #[test]
    fn a_path_is_absolute_from_the_working_directory() {
        let here = context::here().expect("a UTF-8 working directory");
        assert_eq!(absolute("x/y").unwrap(), here.join("x/y"));
        assert_eq!(absolute("/a/b").unwrap(), Utf8PathBuf::from("/a/b"));
    }
}
