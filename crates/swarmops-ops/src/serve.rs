//! `swarmops serve` (M14 design §2): a store of a directory of stores served to stock git, one
//! process per connection, over ssh as a forced command, as a CGI program behind a web server, or
//! as the `proc-receive` hook git runs inside a push.
//!
//! ```text
//! ServeInvocation ─▶ run: 1 identity ─▶ 2 request ─▶ upload | receive (ssh) · cgi · hook
//!                                                       3 store ─▶ 4 access ─▶ 5 git ─▶ 6 refresh
//! ```
//!
//! [`run`] refuses before any protocol byte in that order (design §2.3), so a connection that
//! names no caller is `E_AUTH` whatever it asks for. `serve` names its caller from `--as` alone:
//! the CLI's fallback to `SWARMOPS_AGENT` and `SWARMOPS_RUN_ID` does not apply to it, since the
//! serving account's environment would name every key whose `authorized_keys` line lacks `--as`
//! ([`access::resolve_identity`] is the one resolver). The hook's caller is the one its parent
//! resolved, in [`SWARMOPS_SERVE_IDENTITY`].
//!
//! The steps after the request are the service's own (`upload::run`, `receive::run`,
//! `cgi::run`, `hook::run`); each answers `E_INVALID_ARG` "not built" until the M14 plan's task
//! that owns it fills its body.

pub mod access;
pub mod cgi;
pub mod entry;
pub mod guard;
pub mod hook;
pub mod land;
pub mod push;
pub mod receive;
pub mod refresh;
pub mod upload;

use std::collections::BTreeMap;
use std::path::PathBuf;

use camino::Utf8PathBuf;
use swarmops_api::{ErrorCode, Identity, OpError};
use swarmops_git::MemoKey;
use swarmops_git::serve::child::Service;

/// The variable the parent `serve` names its own binary in, for the hook file to run.
pub const SWARMOPS_SERVE_EXE: &str = "SWARMOPS_SERVE_EXE";

/// The variable the parent `serve` names the served store in, for the hook.
pub const SWARMOPS_SERVE_STORE: &str = "SWARMOPS_SERVE_STORE";

/// The variable the parent `serve` names the caller it resolved in: the hook's only identity.
pub const SWARMOPS_SERVE_IDENTITY: &str = "SWARMOPS_SERVE_IDENTITY";

/// The CGI metavariable a web server names its authenticated user in (RFC 3875 §4.1.11).
pub const REMOTE_USER: &str = "REMOTE_USER";

/// How a connection reached `serve`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Entry {
    /// An ssh forced command: what the client asked for, `SSH_ORIGINAL_COMMAND` (empty when
    /// unset).
    Ssh {
        /// `git-upload-pack '<path>'`, `git-receive-pack '<path>'`, …
        original_command: String,
    },
    /// A CGI request: its metavariables (RFC 3875), by name.
    Cgi {
        /// `PATH_INFO`, `REQUEST_METHOD`, `REMOTE_USER`, …
        vars: BTreeMap<String, String>,
    },
    /// git's `proc-receive` hook, inside a push the parent `serve` runs.
    Hook,
}

/// Who the caller is, as the command line names them.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum IdentitySource {
    /// `--as <agent>`: this identity.
    Fixed(Identity),
    /// `--user-family <family>`: `<family>/$REMOTE_USER`.
    UserFamily(String),
    /// Neither: every connection is refused, `E_AUTH`.
    Absent,
}

/// The memo key of the store whose id the argument is, as the CLI keeps it outside the store.
pub type MemoKeys = Box<dyn Fn(&str) -> Result<Option<MemoKey>, OpError>>;

/// One run of `serve`: everything it takes from the command line and the environment, read once
/// by the CLI.
#[non_exhaustive]
pub struct ServeInvocation {
    /// `--root`: the directory whose subdirectories are the stores served, by name.
    pub root: Utf8PathBuf,
    /// How the connection came.
    pub entry: Entry,
    /// Who the caller is.
    pub identity: IdentitySource,
    /// The client's `GIT_PROTOCOL`, passed on to git as it came.
    pub git_protocol: Option<String>,
    /// This process's binary, which the hook file runs ([`SWARMOPS_SERVE_EXE`]).
    pub exe: Option<PathBuf>,
    /// For the hook: the store its parent served ([`SWARMOPS_SERVE_STORE`]).
    pub serve_store: Option<String>,
    /// For the hook: the `GIT_DIR` git gave it.
    pub git_dir: Option<PathBuf>,
    /// The memo key of a store, by its id.
    pub memo_keys: MemoKeys,
}

impl ServeInvocation {
    /// The run of `entry` under `root` for the caller `identity`, with no protocol, binary, hook
    /// variable or `GIT_DIR` named yet.
    pub fn new(
        root: Utf8PathBuf,
        entry: Entry,
        identity: IdentitySource,
        memo_keys: MemoKeys,
    ) -> ServeInvocation {
        ServeInvocation {
            root,
            entry,
            identity,
            git_protocol: None,
            exe: None,
            serve_store: None,
            git_dir: None,
            memo_keys,
        }
    }
}

/// How a served connection ended: the exit code of its git child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Served {
    /// The code to exit with.
    pub exit: i32,
}

impl Served {
    /// A connection that ended with `exit`.
    pub fn new(exit: i32) -> Served {
        Served { exit }
    }
}

/// Serves `invocation`: the caller named first, then the request, then the service's own steps.
/// An error comes before any protocol byte, for the CLI to print.
pub fn run(invocation: &ServeInvocation) -> Result<Served, OpError> {
    // 1. The caller: --as, or a web server's user under --user-family.
    let remote_user = match &invocation.entry {
        Entry::Cgi { vars } => vars.get(REMOTE_USER).map(String::as_str),
        Entry::Ssh { .. } | Entry::Hook => None,
    };
    let who = access::resolve_identity(&invocation.identity, remote_user)?;

    // 2. The request, then the service's steps: store, access, git, refresh.
    match &invocation.entry {
        Entry::Ssh { original_command } => {
            let request = entry::parse_ssh(original_command)?;
            match request.service {
                Service::UploadPack => upload::run(invocation, &who, &request),
                Service::ReceivePack => receive::run(invocation, &who, &request),
                _ => Err(not_built("this git service over ssh")),
            }
        }
        Entry::Cgi { .. } => cgi::run(invocation, &who),
        Entry::Hook => hook::run(invocation, &who),
    }
}

/// The error of a part of `serve` not built yet: `E_INVALID_ARG`, "`what`: not built".
pub(crate) fn not_built(what: &str) -> OpError {
    OpError::new(ErrorCode::InvalidArg, format!("{what}: not built"))
}

/// An error whose `details.reason` is `reason`.
pub(crate) fn refused(code: ErrorCode, reason: &str, message: impl Into<String>) -> OpError {
    let mut details = serde_json::Map::new();
    details.insert("reason".to_owned(), reason.into());
    OpError::new(code, message).with_details(details)
}

/// Writes `error` as git's pre-protocol refusal, one `ERR <CODE>: <message>` pkt-line, which git
/// prints as `remote error: …`.
pub fn write_refusal(output: &mut impl std::io::Write, error: &OpError) -> std::io::Result<()> {
    let message = format!("{}: {}", error.code, error.message);
    swarmops_git::serve::pkt::write_err(output, &message)?;
    output.flush()
}
