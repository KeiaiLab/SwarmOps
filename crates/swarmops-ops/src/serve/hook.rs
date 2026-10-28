//! `serve --hook proc-receive` (M14 design §2.4, §6): the hook git runs inside a push, which
//! decides each command, by access, import and compare-and-swap, or lands it.
//!
//! ```text
//! 1 from git? SWARMOPS_SERVE_STORE set, GIT_DIR = that store's serve.git (canonical)
//! 2 handshake (version=1) ─▶ commands, up to MAX_PUSH_COMMANDS
//! 3 store, memo key, access.toml, serve.git
//! 4 each command: refs/land/<target> ─▶ land::run · any other ─▶ push::decide
//! 5 one report each (ok | ng <CODE>: <message>), host paths masked
//! ```
//!
//! receive-pack runs the hook in the bare `serve.git` and hands it a relative `GIT_DIR` (`.`),
//! so both sides are compared canonical. git has migrated the push's quarantine before the hook
//! runs: no quarantine variable is looked for. A failure after the handshake refuses every
//! command (`ng`) rather than the connection, so that the client reads why.

use std::io::{self, Read, Write};
use std::path::Path;

use swarmops_api::{ErrorCode, Identity, OpError};
use swarmops_git::serve::proc_receive::{self, Command, Report};
use swarmops_git::serve::refmap::{self, Mapped};
use swarmops_git::serve::repo::ServeRepo;

use super::access::Access;
use super::guard::Redactor;
use super::refresh::serve_error;
use super::{ServeInvocation, Served, land, push, refused};
use crate::LocalOps;
use crate::error;

/// The most commands one push may hold: past it, every command is refused.
pub const MAX_PUSH_COMMANDS: usize = 1_000;

/// The serving repository, below a store's root.
const SERVE_GIT: &str = "git/serve.git";

/// What the hook decides each command with.
#[non_exhaustive]
pub struct HookCtx<'a> {
    /// The store served.
    pub ops: &'a LocalOps,
    /// Its serving repository.
    pub repo: &'a ServeRepo,
    /// The caller.
    pub who: &'a Identity,
    /// The store's grants.
    pub access: &'a Access,
}

impl<'a> HookCtx<'a> {
    /// The context of `who` pushing to the store `ops` serves through `repo`, under `access`.
    pub fn new(
        ops: &'a LocalOps,
        repo: &'a ServeRepo,
        who: &'a Identity,
        access: &'a Access,
    ) -> HookCtx<'a> {
        HookCtx {
            ops,
            repo,
            who,
            access,
        }
    }
}

/// Runs the hook for `who`, the caller its parent resolved, on this process's stdin and stdout.
pub fn run(invocation: &ServeInvocation, who: &Identity) -> Result<Served, OpError> {
    // 1. Run by git, inside a push serve started.
    let store = served_store(invocation)?;
    let redactor = Redactor::new(&[Path::new(&store)]);

    // 2. The protocol: the handshake and the commands, then a report on each.
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    let commands = handshake(&mut input, &mut output)?;
    let reports = decide_all(invocation, &store, who, &commands);
    let reports: Vec<Report> = reports
        .into_iter()
        .map(|report| redact(&redactor, report))
        .collect();
    proc_receive::write_reports(&mut output, &reports).map_err(|e| protocol(&e))?;
    Ok(Served::new(0))
}

/// The store the parent served, refused `not_from_git` unless both `SWARMOPS_SERVE_STORE` and
/// `GIT_DIR` are given and `GIT_DIR` is that store's `serve.git`.
fn served_store(invocation: &ServeInvocation) -> Result<String, OpError> {
    let not_from_git = |why: &str| {
        refused(
            ErrorCode::InvalidArg,
            "not_from_git",
            format!("serve --hook runs inside a push that serve started: {why}"),
        )
    };
    let (Some(store), Some(git_dir)) = (&invocation.serve_store, &invocation.git_dir) else {
        return Err(not_from_git("no store or GIT_DIR from its parent"));
    };

    // Both canonical: receive-pack names the repository `.`, from inside it.
    let ours = Path::new(store).join(SERVE_GIT).canonicalize();
    let given = git_dir.canonicalize();
    match (ours, given) {
        (Ok(ours), Ok(given)) if ours == given => Ok(store.clone()),
        _ => Err(not_from_git("GIT_DIR is not the store's serve.git")),
    }
}

/// The version handshake on `input` and `output`, then the push's commands.
fn handshake(input: &mut impl Read, output: &mut impl Write) -> Result<Vec<Command>, OpError> {
    proc_receive::handshake(input, output).map_err(|e| protocol(&e))?;
    proc_receive::read_commands(input).map_err(|e| protocol(&e))
}

/// A report on each of `commands`, pushed by `who` to the store at `store`: every one refused
/// when there are too many, or when the store cannot be opened.
fn decide_all(
    invocation: &ServeInvocation,
    store: &str,
    who: &Identity,
    commands: &[Command],
) -> Vec<Report> {
    let refuse_all = |e: &OpError| {
        (commands.iter())
            .map(|command| ng(&command.refname, e))
            .collect()
    };

    // 1. Bounded: past MAX_PUSH_COMMANDS, nothing is looked at.
    if commands.len() > MAX_PUSH_COMMANDS {
        let e = refused(
            ErrorCode::InvalidArg,
            "too_many_commands",
            format!(
                "a push holds {} commands, past {MAX_PUSH_COMMANDS}: push fewer refs at once",
                commands.len()
            ),
        );
        return refuse_all(&e);
    }

    // 2. The store, its memo key, its grants and its serving repository.
    let opened = open(invocation, store);
    let (ops, access, repo) = match opened {
        Ok(opened) => opened,
        Err(e) => return refuse_all(&e),
    };
    let ctx = HookCtx::new(&ops, &repo, who, &access);

    // 3. Each command: a land, or a ref moved.
    commands
        .iter()
        .map(
            |command| match refmap::from_git(ops.repo.format(), &command.refname) {
                Mapped::Land { target } => land::run(&ctx, command, &target),
                _ => push::decide(&ctx, command),
            },
        )
        .collect()
}

/// The store at `store`, with its memo key, its grants and its serving repository.
fn open(
    invocation: &ServeInvocation,
    store: &str,
) -> Result<(LocalOps, Access, ServeRepo), OpError> {
    let root = camino::Utf8Path::new(store);
    let ops = LocalOps::open(root)?;
    let repo_id = ops.repo.info().map_err(error::op)?.repo_id;
    let ops = match (invocation.memo_keys)(&repo_id)? {
        Some(key) => ops.with_memo_key(key),
        None => ops,
    };
    let access = Access::load(&ops)?;
    let repo = ServeRepo::open(root).map_err(serve_error)?;
    Ok((ops, access, repo))
}

/// The report that refuses `refname` with `e`: `ng <refname> <CODE>: <message>`, on one line.
pub(crate) fn ng(refname: &str, e: &OpError) -> Report {
    let reason = format!("{}: {}", e.code, e.message).replace(['\n', '\r'], " ");
    Report::ng(refname, reason)
}

/// `report`, its texts masked by `redactor`.
fn redact(redactor: &Redactor, report: Report) -> Report {
    match report {
        Report::Ng { refname, reason } => Report::ng(refname, redactor.redact(&reason)),
        other => other,
    }
}

/// The error of a protocol exchange with receive-pack that failed.
fn protocol(e: &io::Error) -> OpError {
    refused(
        ErrorCode::InvalidArg,
        "not_from_git",
        format!("proc-receive: {e}"),
    )
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use swarmops_api::{ErrorCode, OpError};
    use swarmops_git::serve::proc_receive::Report;

    use super::{ng, redact};
    use crate::serve::guard::Redactor;

    /// A refusal is one `ng` line, `<CODE>: <message>`, whatever newlines its message holds,
    /// and the store's path is masked in it.
    #[test]
    fn ng_is_one_masked_line() {
        let e = OpError::new(ErrorCode::Io, "/srv/so/acme/git: broken\nsecond line");
        let report = redact(
            &Redactor::new(&[Path::new("/srv/so/acme")]),
            ng("refs/heads/x", &e),
        );
        assert_eq!(
            report,
            Report::ng("refs/heads/x", "E_IO: <path>/git: broken second line")
        );
    }
}
