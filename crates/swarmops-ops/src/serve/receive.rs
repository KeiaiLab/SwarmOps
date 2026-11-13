//! The push side over ssh (M14 design §6): the store opened, its read checked, `serve.git`
//! refreshed and its hooks in place, then git's `receive-pack` on it, which runs the hooks.
//!
//! ```text
//! 1 store under --root ─▶ 2 access.toml: read ─▶ 3 git found ─▶ 4 memo key
//!      ─▶ 5 serve.git refreshed (steps 1 to 5: the fetch side's `prepare`)
//! 6 git receive-pack ── quarantine, fsck, connectivity ──▶ pre-receive hook (serve --hook)
//!      ── the quarantine moved into serve.git ──▶ proc-receive hook (serve --hook)
//!      SWARMOPS_SERVE_EXE · SWARMOPS_SERVE_STORE · SWARMOPS_SERVE_IDENTITY · _CONNECTION
//!      RAYON_NUM_THREADS = SERVE_HOOK_THREADS (4): the hooks' compression threads
//! 7 its record, from step 1 on: git/serve-records.jsonl
//! ```
//!
//! receive-pack moves no ref itself: `pre-receive` refuses the whole push while its objects are
//! quarantined, and every command it accepts goes to `proc-receive`
//! (`receive.procReceiveRefs=refs/`), which decides it ([`super::hook`]). A caller who may not
//! read the store is refused before git starts; what it may push is decided per ref.

use std::ffi::OsString;

use camino::Utf8Path;

use swarmops_api::{Identity, OpError};
use swarmops_git::serve::child::{self, ChildEnv, Service};

use super::entry::{SshRequest, store_under};
use super::guard::Redactor;
use super::records::{Connection, SWARMOPS_SERVE_CONNECTION, Side};
use super::refresh::serve_error;
use super::upload::{exit_code, prepare};
use super::{
    SWARMOPS_SERVE_EXE, SWARMOPS_SERVE_IDENTITY, SWARMOPS_SERVE_STORE, ServeInvocation, Served,
    reply_then_step, step_store,
};

/// The most bytes a push may send: git's `receive.maxInputSize`.
pub const MAX_PUSH_BYTES: u64 = 2 << 30;

/// The threads a push's hook compresses the store's objects on: rayon's global pool, which
/// the hooks' object store uses, is otherwise one thread per core, each with its buffers.
pub(crate) const SERVE_HOOK_THREADS: usize = 4;

/// The variable that sizes rayon's global pool in the hooks receive-pack runs.
pub(crate) const RAYON_NUM_THREADS: &str = "RAYON_NUM_THREADS";

/// git's setting that bounds what a push sends.
const MAX_INPUT_SIZE: &str = "receive.maxInputSize";

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
    // 1-5. The read, git, the memo key, serve.git refreshed.
    let prepared = prepare(invocation, who, &request.store, store, redactor);
    let (repo, git) = prepared.map_err(|e| redactor.redact_error(e))?;

    // 6. git receive-pack, on the connection's stdin and stdout; the hook it runs learns the
    //    binary, the store and the caller from its parent.
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
    //    Once git has sent its reply, the view work the push's commits left is stepped.
    let status = reply_then_step(
        || child::run(&git, Service::ReceivePack, &repo, &env),
        || step_store(store),
        redactor,
        &mut std::io::stderr(),
    );
    let status = status.map_err(|e| redactor.redact_error(serve_error(e)))?;
    Ok(Served::new(exit_code(status)))
}
