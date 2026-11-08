//! The push side over ssh (M14 design §6): the store opened, its read checked, `serve.git`
//! refreshed and its hooks in place, then git's `receive-pack` on it, which runs the hooks.
//!
//! ```text
//! 1 store under --root ─▶ 2 memo key ─▶ 3 access.toml: read ─▶ 4 serve.git refreshed
//! 5 git receive-pack ── quarantine, fsck, connectivity ──▶ pre-receive hook (serve --hook)
//!      ── the quarantine moved into serve.git ──▶ proc-receive hook (serve --hook)
//!      SWARMOPS_SERVE_EXE · SWARMOPS_SERVE_STORE · SWARMOPS_SERVE_IDENTITY · _CONNECTION
//!      RAYON_NUM_THREADS = SERVE_HOOK_THREADS (4): the hooks' compression threads
//! 6 its record, from step 1 on: git/serve-records.jsonl
//! ```
//!
//! receive-pack moves no ref itself: `pre-receive` refuses the whole push while its objects are
//! quarantined, and every command it accepts goes to `proc-receive`
//! (`receive.procReceiveRefs=refs/`), which decides it ([`super::hook`]). A caller who may not
//! read the store is refused before git starts; what it may push is decided per ref.

use std::ffi::OsString;
use std::io::Write as _;
use std::os::unix::process::ExitStatusExt as _;
use std::process::ExitStatus;

use camino::Utf8Path;

use swarmops_api::{Identity, OpError};
use swarmops_git::serve::child::{self, ChildEnv, Service};
use swarmops_git::serve::repo::ServeRepo;
use swarmops_git::transport::GitCli;

use super::access::{Access, Action};
use super::entry::{SshRequest, store_under};
use super::guard::Redactor;
use super::records::{Connection, SWARMOPS_SERVE_CONNECTION, Side};
use super::refresh::{Freshness, Refreshed, Scope, freshness, refresh, serve_error};
use super::{
    SWARMOPS_SERVE_EXE, SWARMOPS_SERVE_IDENTITY, SWARMOPS_SERVE_STORE, ServeInvocation, Served,
};
use crate::LocalOps;
use crate::error;

/// The most bytes a push may send: git's `receive.maxInputSize`.
pub const MAX_PUSH_BYTES: u64 = 2 << 30;

/// The threads a push's hook compresses the store's objects on: rayon's global pool, which
/// the hooks' object store uses, is otherwise one thread per core, each with its buffers.
pub(crate) const SERVE_HOOK_THREADS: usize = 4;

/// The variable that sizes rayon's global pool in the hooks receive-pack runs.
pub(crate) const RAYON_NUM_THREADS: &str = "RAYON_NUM_THREADS";

/// git's setting that bounds what a push sends.
const MAX_INPUT_SIZE: &str = "receive.maxInputSize";

/// The name the texts give the serving repository: never its host path.
const SERVE_GIT: &str = "serve.git";

/// The exit of a git killed by a signal: the shell's `128 + signal`.
const SIGNAL_EXIT: i32 = 128;

/// Serves `request`, a `receive-pack`, to `who`.
pub fn run(
    invocation: &ServeInvocation,
    who: &Identity,
    request: &SshRequest,
) -> Result<Served, OpError> {
    // 1. The store under the root; from here on, the connection is recorded.
    let connection = Connection::start();
    let redactor = Redactor::new(&[invocation.root.as_std_path()]);
    let store = store_under(&invocation.root, &request.store);
    let store = store.map_err(|e| redactor.redact_error(e))?;
    let served = serve(invocation, who, request, &store, &connection, &redactor);
    connection.finish(&store, who, Side::Push, &served, &redactor);
    served
}

/// Serves the receive-pack of the store at `store` to `who` on `connection`.
fn serve(
    invocation: &ServeInvocation,
    who: &Identity,
    request: &SshRequest,
    store: &Utf8Path,
    connection: &Connection,
    redactor: &Redactor,
) -> Result<Served, OpError> {
    let repo = prepare(invocation, who, request, store, redactor);
    let repo = repo.map_err(|e| redactor.redact_error(e))?;

    // 5. git receive-pack, on the connection's stdin and stdout; the hook it runs learns the
    //    binary, the store and the caller from its parent.
    let git = GitCli::find().map_err(|e| git_error(e, redactor))?;
    let mut env = ChildEnv::new();
    env.git_protocol = invocation.git_protocol.clone().map(Into::into);
    if let Some(exe) = &invocation.exe {
        env.vars
            .insert(SWARMOPS_SERVE_EXE.to_owned(), exe.clone().into_os_string());
    }
    env.vars.insert(
        SWARMOPS_SERVE_STORE.to_owned(),
        OsString::from(store.as_str()),
    );
    env.vars.insert(
        SWARMOPS_SERVE_IDENTITY.to_owned(),
        OsString::from(who.agent()),
    );
    env.vars.insert(
        SWARMOPS_SERVE_CONNECTION.to_owned(),
        OsString::from(connection.id()),
    );
    env.vars.insert(
        RAYON_NUM_THREADS.to_owned(),
        OsString::from(SERVE_HOOK_THREADS.to_string()),
    );
    env.config.push((
        MAX_INPUT_SIZE.to_owned(),
        OsString::from(MAX_PUSH_BYTES.to_string()),
    ));
    let status = child::run(&git, Service::ReceivePack, &repo, &env)
        .map_err(|e| redactor.redact_error(serve_error(e)))?;
    Ok(Served::new(exit_code(status)))
}

/// Steps 1 to 4: the store at `store`, which `request` names, opened with its memo key, `who`'s
/// read checked, and its `serve.git` refreshed (its hook made or replaced on the way); what the
/// refresh warns of goes to stderr, masked by `redactor`. The serving repository; the store is
/// closed again before git starts.
fn prepare(
    invocation: &ServeInvocation,
    who: &Identity,
    request: &SshRequest,
    store: &Utf8Path,
    redactor: &Redactor,
) -> Result<ServeRepo, OpError> {
    // 1. The store, which the request named under the root.
    let ops = LocalOps::open(store)?;

    // 2. Its memo key, kept by the serving account outside the store.
    let repo_id = ops.repo.info().map_err(error::op)?.repo_id;
    let ops = match (invocation.memo_keys)(&repo_id)? {
        Some(key) => ops.with_memo_key(key),
        None => ops,
    };

    // 3. The caller may read it: what it may push is the hook's to decide, ref by ref.
    Access::load(&ops)?.check(who, Action::Read, &request.store)?;

    // 4. Its serving repository, refreshed: the old values a push names are these exports,
    //    unless nothing moved since its last export (no lock taken then).
    let repo = ServeRepo::open(store).map_err(serve_error)?;
    let refreshed = match freshness(&ops, &repo)? {
        Freshness::Current => Refreshed::default(),
        Freshness::Stale => refresh(&ops, &repo, &Scope::All)?,
    };
    let mut stderr = std::io::stderr().lock();
    for warning in refreshed.warnings {
        let _ = writeln!(stderr, "warning: {}", redactor.redact(&warning));
    }
    Ok(repo)
}

/// The error of finding no git to run, masked.
fn git_error(e: swarmops_git::transport::TransportError, redactor: &Redactor) -> OpError {
    redactor.redact_error(error::transport_error(e, SERVE_GIT, error::Phase::Push))
}

/// The code a connection exits with: git's, or `128 + signal` for a git a signal ended.
fn exit_code(status: ExitStatus) -> i32 {
    status
        .code()
        .or_else(|| status.signal().map(|signal| SIGNAL_EXIT + signal))
        .unwrap_or(SIGNAL_EXIT)
}
