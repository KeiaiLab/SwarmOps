//! The push side over ssh (M14 design §6): the store opened, its read checked, `serve.git`
//! refreshed and its hook in place, then git's `receive-pack` on it, which runs the hook.
//!
//! ```text
//! 1 store under --root ─▶ 2 memo key ─▶ 3 access.toml: read ─▶ 4 serve.git refreshed
//! 5 git receive-pack ── quarantine, fsck, connectivity ──▶ proc-receive hook (serve --hook)
//!      SWARMOPS_SERVE_EXE · SWARMOPS_SERVE_STORE · SWARMOPS_SERVE_IDENTITY
//! ```
//!
//! receive-pack moves no ref itself: every command goes to the hook
//! (`receive.procReceiveRefs=refs/`), which decides it ([`super::hook`]). A caller who may not
//! read the store is refused before git starts; what it may push is decided per ref.

use std::ffi::OsString;
use std::io::Write as _;
use std::os::unix::process::ExitStatusExt as _;
use std::process::ExitStatus;

use swarmops_api::{Identity, OpError};
use swarmops_git::serve::child::{self, ChildEnv, Service};
use swarmops_git::serve::repo::ServeRepo;
use swarmops_git::transport::GitCli;

use super::access::{Access, Action};
use super::entry::{SshRequest, store_under};
use super::guard::Redactor;
use super::refresh::{Scope, refresh, serve_error};
use super::{
    SWARMOPS_SERVE_EXE, SWARMOPS_SERVE_IDENTITY, SWARMOPS_SERVE_STORE, ServeInvocation, Served,
};
use crate::LocalOps;
use crate::error;

/// The most bytes a push may send: git's `receive.maxInputSize`.
pub const MAX_PUSH_BYTES: u64 = 2 << 30;

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
    let redactor = Redactor::new(&[invocation.root.as_std_path()]);
    let (repo, store) =
        prepare(invocation, who, request, &redactor).map_err(|e| redactor.redact_error(e))?;

    // 5. git receive-pack, on the connection's stdin and stdout; the hook it runs learns the
    //    binary, the store and the caller from its parent.
    let git = GitCli::find().map_err(|e| git_error(e, &redactor))?;
    let mut env = ChildEnv::new();
    env.git_protocol = invocation.git_protocol.clone().map(Into::into);
    if let Some(exe) = &invocation.exe {
        env.vars
            .insert(SWARMOPS_SERVE_EXE.to_owned(), exe.clone().into_os_string());
    }
    env.vars
        .insert(SWARMOPS_SERVE_STORE.to_owned(), OsString::from(store));
    env.vars.insert(
        SWARMOPS_SERVE_IDENTITY.to_owned(),
        OsString::from(who.agent()),
    );
    env.config.push((
        MAX_INPUT_SIZE.to_owned(),
        OsString::from(MAX_PUSH_BYTES.to_string()),
    ));
    let status = child::run(&git, Service::ReceivePack, &repo, &env)
        .map_err(|e| redactor.redact_error(serve_error(e)))?;
    Ok(Served::new(exit_code(status)))
}

/// Steps 1 to 4: the store `request` names opened with its memo key, `who`'s read checked, and
/// its `serve.git` refreshed (its hook made or replaced on the way); what the refresh warns of
/// goes to stderr, masked by `redactor`. The serving repository and the store's path; the store
/// is closed again before git starts.
fn prepare(
    invocation: &ServeInvocation,
    who: &Identity,
    request: &SshRequest,
    redactor: &Redactor,
) -> Result<(ServeRepo, String), OpError> {
    // 1. The store, under the root.
    let store = store_under(&invocation.root, &request.store)?;
    let ops = LocalOps::open(&store)?;

    // 2. Its memo key, kept by the serving account outside the store.
    let repo_id = ops.repo.info().map_err(error::op)?.repo_id;
    let ops = match (invocation.memo_keys)(&repo_id)? {
        Some(key) => ops.with_memo_key(key),
        None => ops,
    };

    // 3. The caller may read it: what it may push is the hook's to decide, ref by ref.
    Access::load(&ops)?.check(who, Action::Read, &request.store)?;

    // 4. Its serving repository, refreshed: the old values a push names are these exports.
    let repo = ServeRepo::open(&store).map_err(serve_error)?;
    let refreshed = refresh(&ops, &repo, &Scope::All)?;
    let mut stderr = std::io::stderr().lock();
    for warning in refreshed.warnings {
        let _ = writeln!(stderr, "warning: {}", redactor.redact(&warning));
    }
    Ok((repo, store.into_string()))
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
