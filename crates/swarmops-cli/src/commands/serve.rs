//! `swarmops serve` (M14 design §2): the command line and the environment read once into a
//! [`ServeInvocation`], which `swarmops_ops::serve::run` serves; this module never opens a store.
//!
//! ```text
//! --as | --user-family ─▶ IdentitySource     SSH_ORIGINAL_COMMAND | CGI variables | --hook ─▶ Entry
//!                    └──────────── ServeInvocation ─▶ ops serve::run ─▶ exit
//!                                                         └─ refused ─▶ ERR pkt-line (stdout)
//!                                                                       envelope (stderr)
//! ```
//!
//! The caller is the `--as` flag alone: `SWARMOPS_AGENT` and `SWARMOPS_RUN_ID` are never read
//! here, so that the serving account's environment names no caller. The hook's caller is the one
//! its parent resolved, [`SWARMOPS_SERVE_IDENTITY`]; a hook line with `--as`, or without that
//! variable, did not come from git, and is refused `not_from_git`.
//!
//! `serve --prepare` is the one store command here: it opens the store `--repo` names, with its
//! memo key, and prepares its `serve.git` (`swarmops_ops::serve::prepare`, the M17 design §4.3);
//! its envelope goes to stdout, as every store command's.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io;

use camino::{Utf8Path, Utf8PathBuf};
use serde::Serialize;
use serde_json::{Map, Value};
use swarmops_api::{ErrorCode, Identity, InfoReq, OpError, StoreOps as _};
use swarmops_ops::LocalOps;
use swarmops_ops::serve::prepare::{self, LockWait, Prepared, Seed};
use swarmops_ops::serve::{
    Entry, IdentitySource, MemoKeys, SWARMOPS_SERVE_IDENTITY, SWARMOPS_SERVE_STORE,
    ServeInvocation, write_refusal,
};
use swarmops_ws::workspace::Locate;

use super::run::{Ctx, failed};
use crate::cli::{HookKind, PrepareWait, ServeArgs};
use crate::context::{self, Env};
use crate::output::Printed;

/// What OpenSSH names the command a client asked a forced command for in.
const SSH_ORIGINAL_COMMAND: &str = "SSH_ORIGINAL_COMMAND";

/// The protocol a git client asks for, passed on to the git that serves it.
const GIT_PROTOCOL: &str = "GIT_PROTOCOL";

/// The repository git runs the hook in.
const GIT_DIR: &str = "GIT_DIR";

/// The object directory git keeps a push's objects in while `pre-receive` runs.
const GIT_QUARANTINE_PATH: &str = "GIT_QUARANTINE_PATH";

/// The CGI metavariables (RFC 3875 §4.1), and the HTTP headers `git http-backend` reads.
const CGI_VARS: [&str; 19] = [
    "AUTH_TYPE",
    "CONTENT_LENGTH",
    "CONTENT_TYPE",
    "GATEWAY_INTERFACE",
    "PATH_INFO",
    "PATH_TRANSLATED",
    "QUERY_STRING",
    "REMOTE_ADDR",
    "REMOTE_HOST",
    "REMOTE_IDENT",
    "REMOTE_USER",
    "REQUEST_METHOD",
    "SCRIPT_NAME",
    "SERVER_NAME",
    "SERVER_PORT",
    "SERVER_PROTOCOL",
    "SERVER_SOFTWARE",
    "HTTP_GIT_PROTOCOL",
    "HTTP_CONTENT_ENCODING",
];

/// The variables `context::memo_key` reads, kept for the memo key of the store served, which is
/// known once ops opens it.
const MEMO_KEY_VARS: [&str; 4] = [
    context::MEMO_KEY_DIR_ENV,
    "HOME",
    "XDG_STATE_HOME",
    "LOCALAPPDATA",
];

/// The HTTP statuses of a CGI refusal.
const STATUS_BAD_REQUEST: u16 = 400;
const STATUS_FORBIDDEN: u16 = 403;
const STATUS_NOT_FOUND: u16 = 404;
const STATUS_UNAVAILABLE: u16 = 503;
const STATUS_ERROR: u16 = 500;

/// How a refusal reaches the client.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Channel {
    /// git's protocol on stdout: an `ERR` pkt-line.
    Git,
    /// A CGI response on stdout: a `Status:` header and the text.
    Cgi,
}

/// Runs `serve`, operation `op`, with `args`: the connection served, or refused before any
/// protocol byte.
pub(super) fn run(ctx: &Ctx<'_>, op: &'static str, args: ServeArgs) -> Printed {
    if args.prepare {
        return run_prepare(ctx, op, &args);
    }
    let channel = if args.cgi { Channel::Cgi } else { Channel::Git };
    let served = invocation(ctx.global.agent.as_deref(), ctx.env, args)
        .and_then(|invocation| swarmops_ops::serve::run(&invocation));
    match served {
        Ok(served) => Printed {
            stdout: String::new(),
            exit: u8::try_from(served.exit).unwrap_or(u8::MAX),
        },
        Err(error) => refuse(ctx, op, channel, error),
    }
}

/// What `args`, the global `--as` (`agent`) and the environment `env` ask `serve` for.
fn invocation(
    agent: Option<&str>,
    env: Env<'_>,
    args: ServeArgs,
) -> Result<ServeInvocation, OpError> {
    // 1. --stdio is M11's.
    if args.stdio {
        return Err(reason(
            ErrorCode::InvalidArg,
            "not_served",
            "serve --stdio, the store-to-store sync, is not served yet",
        ));
    }

    // 2. The caller and the entry: the hook's from its parent, the others' from the line.
    let (identity, entry) = match args.hook {
        Some(HookKind::PreReceive) => (hook_identity(agent, env)?, Entry::PreReceive),
        Some(HookKind::ProcReceive) => (hook_identity(agent, env)?, Entry::Hook),
        None => (
            line_identity(agent, args.user_family)?,
            entry(env, args.cgi)?,
        ),
    };

    // 3. The rest of what the connection names, read once.
    let root = args.root.unwrap_or_default();
    let mut invocation = ServeInvocation::new(root, entry, identity, memo_keys(env));
    invocation.git_protocol = var(env, GIT_PROTOCOL)?;
    invocation.exe = std::env::current_exe().ok();
    invocation.serve_store = var(env, SWARMOPS_SERVE_STORE)?;
    invocation.git_dir = env(GIT_DIR).filter(|dir| !dir.is_empty()).map(Into::into);
    invocation.quarantine =
        (env(GIT_QUARANTINE_PATH).filter(|dir| !dir.is_empty())).map(Into::into);
    Ok(invocation)
}

/// The caller of a connection: `--as`, else `--user-family`, else none.
fn line_identity(
    agent: Option<&str>,
    user_family: Option<String>,
) -> Result<IdentitySource, OpError> {
    if let Some(agent) = agent {
        return Ok(IdentitySource::Fixed(Identity::parse(agent)?));
    }
    Ok(user_family.map_or(IdentitySource::Absent, IdentitySource::UserFamily))
}

/// The hook's caller: the one its parent resolved, never `--as`.
fn hook_identity(agent: Option<&str>, env: Env<'_>) -> Result<IdentitySource, OpError> {
    let not_from_git = |why: &str| {
        reason(
            ErrorCode::InvalidArg,
            "not_from_git",
            format!("serve --hook runs inside a push that serve started: {why}"),
        )
    };
    if agent.is_some() {
        return Err(not_from_git("it takes no --as"));
    }
    let Some(who) = var(env, SWARMOPS_SERVE_IDENTITY)? else {
        return Err(not_from_git("no caller from its parent"));
    };
    Ok(IdentitySource::Fixed(Identity::parse(&who)?))
}

/// The entry of a connection that is no hook: a CGI request, else an ssh forced command.
fn entry(env: Env<'_>, cgi: bool) -> Result<Entry, OpError> {
    if !cgi {
        let original_command = var(env, SSH_ORIGINAL_COMMAND)?.unwrap_or_default();
        return Ok(Entry::Ssh { original_command });
    }
    let mut vars = BTreeMap::new();
    for name in CGI_VARS {
        if let Some(value) = var(env, name)? {
            vars.insert(name.to_owned(), value);
        }
    }
    Ok(Entry::Cgi { vars })
}

/// The memo keys of the stores served, from [`MEMO_KEY_VARS`] as they are now.
fn memo_keys(env: Env<'_>) -> MemoKeys {
    let mut vars: BTreeMap<&'static str, OsString> = BTreeMap::new();
    for name in MEMO_KEY_VARS {
        if let Some(value) = env(name) {
            vars.insert(name, value);
        }
    }
    Box::new(move |repo_id: &str| {
        let env = |name: &str| vars.get(name).cloned();
        context::memo_key(&env, repo_id)
    })
}

/// What `serve --prepare` did, as its envelope's result gives it.
#[derive(Serialize)]
struct PrepareOut {
    /// What its refreshes did.
    refreshed: RefreshedOut,
    /// How many refreshes it ran: 0 when it was skipped or found the lock held.
    rounds: u32,
    /// How many tips it fetched from `--seed-from`: 0 or 1.
    seeded: u64,
    /// Why it exported nothing, when it did not try.
    skipped: Option<String>,
    /// What did not stop it, one line each; the reason it was skipped among them.
    warnings: Vec<String>,
}

/// The counts of [`PrepareOut::refreshed`].
#[derive(Serialize)]
struct RefreshedOut {
    /// Refs moved in `serve.git`, or made.
    moved: u64,
    /// Refs removed from it.
    removed: u64,
    /// Objects the exports wrote.
    objects: u64,
}

/// Runs `serve --prepare`, operation `op`, on the store `--repo` (or the current directory)
/// names.
fn run_prepare(ctx: &Ctx<'_>, op: &'static str, args: &ServeArgs) -> Printed {
    // 1. The store named, kept for meta.repo.
    let start = context::here().map(Locate::Cwd);
    let named = context::store_for(ctx.global.repo.as_deref(), ctx.env, start.as_ref());
    if let Ok(Some(store)) = &named {
        ctx.used.repo.replace(Some(store.clone()));
    }

    // 2. Prepared.
    let result = named.and_then(|named| {
        let store = named.ok_or_else(|| context::no_store_named(ctx.global.repo.as_deref()))?;
        prepared(ctx.env, Utf8Path::new(&store), args)
    });
    ctx.finish(op, ctx.used.meta(), result)
}

/// The preparation of the store at `root`, with its memo key, as `args` say.
fn prepared(env: Env<'_>, root: &Utf8Path, args: &ServeArgs) -> Result<PrepareOut, OpError> {
    // 1. The store and its memo key, kept by this account outside the store.
    let ops = LocalOps::open(root)?;
    let repo_id = ops.repo_info(InfoReq::default())?.repo_id;
    let ops = match context::memo_key(env, &repo_id)? {
        Some(key) => ops.with_memo_key(key),
        None => ops,
    };

    // 2. The seed, as an absolute path: git runs in serve.git.
    let seed = match &args.seed_from {
        Some(dir) => Seed::GitDir(absolute(dir)?),
        None => Seed::None,
    };
    let wait = match args.prepare_wait {
        PrepareWait::Zero => LockWait::Zero,
        PrepareWait::Bounded => LockWait::Bounded,
    };

    // 3. serve.git prepared, by its absolute path: git runs in it.
    let root = absolute(root)?;
    let repo = prepare::serve_repo(&root)?;
    let done = prepare::prepare(&ops, &repo, &seed, wait)?;
    Ok(prepare_out(done))
}

/// `done` as the envelope gives it: the reason it was skipped is a warning too.
fn prepare_out(done: Prepared) -> PrepareOut {
    let mut warnings = done.refreshed.warnings;
    if let Some(skipped) = &done.skipped {
        warnings.push(format!("prepare_skipped: {skipped}"));
    }
    PrepareOut {
        refreshed: RefreshedOut {
            moved: done.refreshed.moved,
            removed: done.refreshed.removed,
            objects: done.refreshed.objects,
        },
        rounds: done.rounds,
        seeded: done.seeded,
        skipped: done.skipped,
        warnings,
    }
}

/// `dir` from the current directory, when it is relative.
fn absolute(dir: &Utf8Path) -> Result<Utf8PathBuf, OpError> {
    if dir.is_absolute() {
        return Ok(dir.to_owned());
    }
    let here = context::here().ok_or_else(|| {
        OpError::new(
            ErrorCode::InvalidArg,
            format!("{dir}: the current directory cannot be read"),
        )
    })?;
    Ok(here.join(dir))
}

/// Environment variable `name`: empty is unset, and a value that is not UTF-8 is
/// `E_INVALID_ARG`, said of the variable (`details.source`).
fn var(env: Env<'_>, name: &str) -> Result<Option<String>, OpError> {
    let Some(value) = env(name).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    value.into_string().map(Some).map_err(|_| {
        let mut details = Map::new();
        details.insert("source".to_owned(), Value::from(name));
        OpError::new(
            ErrorCode::InvalidArg,
            format!("{name}: the value is not UTF-8"),
        )
        .with_details(details)
    })
}

/// `code`'s error with `message`, its `details.reason` `why`.
fn reason(code: ErrorCode, why: &str, message: impl Into<String>) -> OpError {
    let mut details = Map::new();
    details.insert("reason".to_owned(), Value::from(why));
    OpError::new(code, message).with_details(details)
}

/// The refusal of `error` before any protocol byte: on stdout what the client reads (an `ERR`
/// pkt-line, or a CGI response), and the envelope, which the entry writes to stderr.
fn refuse(ctx: &Ctx<'_>, op: &'static str, channel: Channel, error: OpError) -> Printed {
    // 1. The client's text: a write that fails leaves the exit code and the envelope to say it.
    let mut stdout = io::stdout().lock();
    let _ = match channel {
        Channel::Git => write_refusal(&mut stdout, &error),
        Channel::Cgi => write_cgi(&mut stdout, &error),
    };

    // 2. The envelope.
    failed(op, error, ctx.format, ctx.started)
}

/// Writes `error` as a CGI response: its status, then `<CODE>: <message>` as text.
fn write_cgi(output: &mut impl io::Write, error: &OpError) -> io::Result<()> {
    let status = match error.code.as_str() {
        code if code == ErrorCode::Auth.as_str() || code == ErrorCode::PolicyDenied.as_str() => {
            STATUS_FORBIDDEN
        }
        code if code == ErrorCode::NotARepo.as_str() => STATUS_NOT_FOUND,
        _ if error.retryable => STATUS_UNAVAILABLE,
        code if code == ErrorCode::InvalidArg.as_str() => STATUS_BAD_REQUEST,
        _ => STATUS_ERROR,
    };
    write!(
        output,
        "Status: {status}\r\nContent-Type: text/plain\r\n\r\n{}: {}\n",
        error.code, error.message
    )?;
    output.flush()
}
