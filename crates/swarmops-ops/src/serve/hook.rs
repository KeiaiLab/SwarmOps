//! `serve --hook pre-receive` and `serve --hook proc-receive` (M14 design §2.4, §6; M16 design
//! §7): the hooks git runs inside a push; and `serve --hook pack-objects`, the one it runs
//! inside a fetch, which answers from the pack cache (the M17 design §5, [`pack_objects`]). The
//! push hooks judge each command with one `judge`, which moves no ref and writes nothing to
//! `serve.git`; `pre-receive` judges alone, while the push's objects are in git's quarantine,
//! and `proc-receive` judges again, then moves the ref by compare-and-swap, or lands it.
//!
//! ```text
//! 1 from git? SWARMOPS_SERVE_STORE set, GIT_DIR = that store's serve.git (canonical)
//!   pre-receive also: GIT_QUARANTINE_PATH = serve.git/objects/tmp_objdir-incoming-* (canonical)
//! 2 the commands: pre-receive's stdin lines · proc-receive's handshake (version=1), then
//!   pkt-lines; up to MAX_PUSH_COMMANDS
//! 3 store, memo key, access.toml, serve.git
//! 4 each command judged: mapping, access, annotated tag, forward or force, the import
//!   pre-receive:  all accepted ─▶ exit 0 · any refused ─▶ a line per command, exit 1: git
//!                 drops the quarantine, every ref "pre-receive hook declined"
//!   proc-receive: refs/land/<target> ─▶ land::apply · any other ─▶ push::apply
//! 5 proc-receive: one report each (ok | ng <CODE>: <message>), host paths masked
//! ```
//!
//! receive-pack runs the hooks in the bare `serve.git` and hands them a relative `GIT_DIR`
//! (`.`), so both sides are compared canonical. git has migrated the push's quarantine before
//! `proc-receive` runs, so the two hooks' answers differ only where another push moved a ref in
//! between. A failure after `proc-receive`'s handshake refuses every command (`ng`) rather than
//! the connection, so that the client reads why.

use std::ffi::OsString;
use std::io::{self, BufRead, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use swarmops_api::{ErrorCode, Identity, OpError};
use swarmops_core::ChangesetId;
use swarmops_git::serve::proc_receive::{self, Command, Report};
use swarmops_git::serve::refmap::{self, Mapped};
use swarmops_git::serve::repo::ServeRepo;
use swarmops_git::transport::{GitCli, Transport as _, TransportError};
use swarmops_git::{GitOid, GixSource, SourceError};
use swarmops_store::RefKind;

use super::access::Access;
use super::guard::Redactor;
use super::refresh::serve_error;
use super::{ServeInvocation, Served, land, packcache, push, refused};
use crate::LocalOps;
use crate::error;

/// The most commands one push may hold: past it, every command is refused.
pub const MAX_PUSH_COMMANDS: usize = 1_000;

/// The serving repository, below a store's root.
const SERVE_GIT: &str = "git/serve.git";

/// The object directory of `serve.git`, where git keeps a push's quarantine.
const OBJECTS: &str = "objects";

/// How git 2.43.0 names a push's quarantine in `serve.git/objects`.
const QUARANTINE_PREFIX: &str = "tmp_objdir-incoming-";

/// The exit of a `pre-receive` that refuses the push: git then drops its quarantine.
const PRE_RECEIVE_REFUSED: i32 = 1;

/// The line `pre-receive` gives a command it accepted, in a push it refuses.
const REFUSED_WITH_PUSH: &str = "accepted alone; refused with the push";

/// Where a hook reads a push's objects.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Objects<'a> {
    /// In `serve.git`, where git moved them before `proc-receive`.
    Served,
    /// In git's quarantine, then `serve.git`: what `pre-receive` sees.
    Quarantine(&'a Path),
}

impl Objects<'_> {
    /// The objects of `repo` as these hold them, to import from.
    pub(crate) fn source(self, repo: &ServeRepo) -> Result<GixSource, SourceError> {
        let git_dir = repo.git_dir().as_std_path();
        match self {
            Objects::Served => GixSource::open(git_dir),
            Objects::Quarantine(quarantine) => GixSource::open_quarantine(git_dir, quarantine),
        }
    }

    /// Whether the commit `ancestor` is `descendant` or one of its ancestors in `repo`, along
    /// any parent.
    pub(crate) fn is_ancestor(
        self,
        repo: &ServeRepo,
        ancestor: GitOid,
        descendant: GitOid,
    ) -> Result<bool, TransportError> {
        let git = GitCli::find()?;
        let git_dir = repo.git_dir().as_std_path();
        match self {
            Objects::Served => git.is_ancestor(git_dir, ancestor, descendant),
            Objects::Quarantine(quarantine) => {
                git.quarantine_is_ancestor(git_dir, quarantine, ancestor, descendant)
            }
        }
    }
}

/// What [`judge`] found a command to do, for `proc-receive` to do it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Judged {
    /// The store ref `store`, of `kind`, moved to the changeset `tip`.
    Move {
        /// The store ref.
        store: String,
        /// Its kind.
        kind: RefKind,
        /// The pushed commit's changeset, imported.
        tip: ChangesetId,
    },
    /// The store ref `store`, of `kind`, deleted.
    Delete {
        /// The store ref.
        store: String,
        /// Its kind.
        kind: RefKind,
    },
    /// The changeset `tip` landed onto `target`.
    Land {
        /// The branch landed onto.
        target: String,
        /// The pushed commit's changeset, imported.
        tip: ChangesetId,
    },
}

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

/// Runs `pre-receive` for `who`, the caller its parent resolved, on this process's stdin: exit 0
/// when every command is accepted; else a line on stderr per command (`<ref> <CODE>: <message>`,
/// git's side band, which the client prints after `remote:`) and exit 1, so that git drops the
/// push's quarantine and no object of it enters `serve.git`.
pub fn pre_receive(invocation: &ServeInvocation, who: &Identity) -> Result<Served, OpError> {
    // 1. Run by git, inside a push serve started.
    let store = served_store(invocation)?;
    let redactor = Redactor::new(&[Path::new(&store)]);

    // 2. The commands, and the quarantine their objects are in: git makes none for a push that
    //    only deletes, which sends no pack.
    let commands = read_updates(&mut io::stdin().lock())?;
    let deletes_only = !commands.is_empty() && commands.iter().all(|c| c.new.is_none());
    let quarantine = match (&invocation.quarantine, deletes_only) {
        (None, true) => None,
        _ => Some(quarantine(invocation, &store)?),
    };
    let objects = quarantine
        .as_deref()
        .map_or(Objects::Served, Objects::Quarantine);

    // 3. Each command judged with those objects.
    let verdicts = judge_all(invocation, &store, who, &commands, objects);
    if verdicts.iter().all(|(_, verdict)| verdict.is_ok()) {
        return Ok(Served::new(0));
    }

    // 4. A refusal: a line per command, then the exit that makes git refuse every ref.
    let mut stderr = io::stderr().lock();
    for (command, verdict) in verdicts {
        let line = match verdict {
            Err(Report::Ng { reason, .. }) => format!("{} {reason}", command.refname),
            _ => format!("{} {REFUSED_WITH_PUSH}", command.refname),
        };
        // Advice: a client gone already reads nothing, and the refusal stands all the same.
        let _ = writeln!(stderr, "{}", redactor.redact(&error::one_line(line)));
    }
    Ok(Served::new(PRE_RECEIVE_REFUSED))
}

/// Runs `proc-receive` for `who`, the caller its parent resolved, on this process's stdin and
/// stdout.
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

/// Runs the pack-objects hook for `who`, the caller its parent resolved: `args` the line git
/// appended, stdin what git fed it, the pack on stdout, from the store's pack cache (the M17
/// design §5). An error, its texts masked, comes before any byte of the pack.
pub fn pack_objects(
    invocation: &ServeInvocation,
    who: &Identity,
    args: &[OsString],
) -> Result<Served, OpError> {
    let _ = who;

    // 1. Run by git, inside a fetch serve started: GIT_DIR, else the working directory, is
    //    serve.git.
    let cwd = std::env::current_dir().ok();
    let store = served_store_in(invocation, cwd.as_deref())?;
    let redactor = Redactor::new(&[Path::new(&store)]);

    // 2. What git asked for, then the pack.
    let repo = ServeRepo::open(camino::Utf8Path::new(&store)).map_err(serve_error);
    let repo = repo.map_err(|e| redactor.redact_error(e))?;
    let mut stdin = Vec::new();
    io::stdin()
        .lock()
        .read_to_end(&mut stdin)
        .map_err(|e| protocol(&e))?;
    let mut out = BufWriter::new(io::stdout().lock());
    packcache::send(&repo, args, &stdin, &mut out).map_err(|e| redactor.redact_error(e))?;
    out.flush().map_err(|e| protocol(&e))?;
    Ok(Served::new(0))
}

/// The refusal of a hook run that did not come from git, `why`.
fn not_from_git(why: &str) -> OpError {
    refused(
        ErrorCode::InvalidArg,
        "not_from_git",
        format!("serve --hook runs inside a push that serve started: {why}"),
    )
}

/// The quarantine git gave `pre-receive`, canonical: refused `not_from_git` unless
/// `GIT_QUARANTINE_PATH` (which git gives every push but one that only deletes) names a
/// directory directly in the store `store`'s `serve.git/objects` whose name starts with
/// [`QUARANTINE_PREFIX`].
fn quarantine(invocation: &ServeInvocation, store: &str) -> Result<PathBuf, OpError> {
    let Some(given) = &invocation.quarantine else {
        return Err(not_from_git("no GIT_QUARANTINE_PATH from git"));
    };

    // Both canonical: git names it `<serve.git>/./objects/tmp_objdir-incoming-XXXXXX`.
    let objects = Path::new(store)
        .join(SERVE_GIT)
        .join(OBJECTS)
        .canonicalize();
    let given = given.canonicalize();
    let (Ok(objects), Ok(given)) = (objects, given) else {
        return Err(not_from_git(
            "GIT_QUARANTINE_PATH is no quarantine of serve.git",
        ));
    };
    let named = given
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(QUARANTINE_PREFIX));
    if given.parent() != Some(objects.as_path()) || !named || !given.is_dir() {
        return Err(not_from_git(
            "GIT_QUARANTINE_PATH is no quarantine of serve.git",
        ));
    }
    Ok(given)
}

/// The store the parent served, refused `not_from_git` unless both `SWARMOPS_SERVE_STORE` and
/// `GIT_DIR` are given and `GIT_DIR` is that store's `serve.git`.
fn served_store(invocation: &ServeInvocation) -> Result<String, OpError> {
    served_store_in(invocation, None)
}

/// [`served_store`], with `cwd` standing for the `GIT_DIR` git did not give.
fn served_store_in(invocation: &ServeInvocation, cwd: Option<&Path>) -> Result<String, OpError> {
    let git_dir = invocation.git_dir.as_deref().or(cwd);
    let (Some(store), Some(git_dir)) = (&invocation.serve_store, git_dir) else {
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

/// The commands of `pre-receive`'s `input`: a line `<old> <new> <ref>` each, git's zero id for
/// none.
fn read_updates(input: &mut impl BufRead) -> Result<Vec<Command>, OpError> {
    let malformed = |line: &str| protocol(&io::Error::other(format!("pre-receive: {line:?}")));
    let mut commands = Vec::new();
    for line in input.lines() {
        let line = line.map_err(|e| protocol(&e))?;
        let mut parts = line.splitn(3, ' ');
        let (Some(old), Some(new), Some(refname)) = (parts.next(), parts.next(), parts.next())
        else {
            return Err(malformed(&line));
        };
        let id = |hex: &str| hex.parse::<GitOid>().map_err(|_| malformed(&line));
        let present = |oid: GitOid| (oid != GitOid::NULL).then_some(oid);
        commands.push(Command::new(present(id(old)?), present(id(new)?), refname));
    }
    Ok(commands)
}

/// Each of `commands`, pushed by `who` to the store at `store`, judged with `objects`
/// ([`judge`]), beside the context its writes need: every one refused when there are too many,
/// or when the store cannot be opened.
fn judge_all(
    invocation: &ServeInvocation,
    store: &str,
    who: &Identity,
    commands: &[Command],
    objects: Objects<'_>,
) -> Vec<(Command, Result<Judged, Report>)> {
    let refuse_all = |e: &OpError| {
        (commands.iter())
            .map(|command| (command.clone(), Err(ng(&command.refname, e))))
            .collect()
    };

    // 1. Bounded: past MAX_PUSH_COMMANDS, nothing is looked at.
    if let Err(e) = bounded(commands) {
        return refuse_all(&e);
    }

    // 2. The store, its memo key, its grants and its serving repository.
    let (ops, access, repo) = match open(invocation, store) {
        Ok(opened) => opened,
        Err(e) => return refuse_all(&e),
    };
    let ctx = HookCtx::new(&ops, &repo, who, &access);

    // 3. Each command judged.
    (commands.iter())
        .map(|command| (command.clone(), judge(&ctx, command, objects)))
        .collect()
}

/// The refusal of a push of more than [`MAX_PUSH_COMMANDS`] commands.
fn bounded(commands: &[Command]) -> Result<(), OpError> {
    if commands.len() <= MAX_PUSH_COMMANDS {
        return Ok(());
    }
    Err(refused(
        ErrorCode::InvalidArg,
        "too_many_commands",
        format!(
            "a push holds {} commands, past {MAX_PUSH_COMMANDS}: push fewer refs at once",
            commands.len()
        ),
    ))
}

/// What `command` does, judged with `objects` (M16 design §7), or the report that refuses it:
/// its mapping, the caller's access, an annotated tag, forward or force, and its tip's import,
/// objects only. It moves no ref and writes nothing to `serve.git`; a land is judged for access
/// and import alone (a conflict and a lost race are its land's).
pub(crate) fn judge(
    ctx: &HookCtx<'_>,
    command: &Command,
    objects: Objects<'_>,
) -> Result<Judged, Report> {
    match refmap::from_git(ctx.ops.repo.format(), &command.refname) {
        Mapped::Land { target } => land::judge(ctx, command, &target, objects),
        _ => push::judge(ctx, command, objects),
    }
}

/// A report on each of `commands`, pushed by `who` to the store at `store`: each judged
/// ([`judge`]), then moved or landed; every one refused when there are too many, or when the
/// store cannot be opened.
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
    if let Err(e) = bounded(commands) {
        return refuse_all(&e);
    }

    // 2. The store, its memo key, its grants and its serving repository.
    let opened = open(invocation, store);
    let (ops, access, repo) = match opened {
        Ok(opened) => opened,
        Err(e) => return refuse_all(&e),
    };
    let ctx = HookCtx::new(&ops, &repo, who, &access);

    // 3. Each command judged again, then a ref moved, or a land.
    (commands.iter())
        .map(|command| match judge(&ctx, command, Objects::Served) {
            Err(report) => report,
            Ok(Judged::Land { target, tip }) => land::apply(&ctx, command, &target, tip),
            Ok(judged) => push::apply(&ctx, command, judged),
        })
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
