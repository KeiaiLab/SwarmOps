//! Running a command (operations spec §1): its request built from the command line, the
//! operation called through `dyn StoreOps` (spec §7), or, for `git import` and `git export`, a
//! client-side operation that no `StoreOps` has, on the store's `LocalOps` ([`Ctx::on_local`]),
//! and its envelope rendered. The git remotes' commands (M13) are client-side operations too:
//! `remote`, `fetch`, `push` and `sync` run on the store's `LocalOps` as `git import` does, and
//! `clone`, which has no store to open yet, runs as `init` does.
//!
//! A command first checks what it can without the store: the store the user named (only
//! whether the variable is readable), a mutation's identity, its own arguments and a commit's
//! change list. Then it opens the store and runs the operation. So a malformed invocation
//! fails the same way whatever the store holds, and costs no store access.
//!
//! The envelope's `meta`: `repo` is the store as the user named it (for `init`, its
//! directory), omitted when the command uses no store (`whoami`, `schema`) or none was named;
//! `elapsed_ms` spans the whole command but its printing: reading the command line, opening
//! the store and the operation; `resolved` is every revision the operation resolved, as the
//! command's own `Resolved` records them (`LocalOps::recording`), also when it failed; `auth` is
//! the store's authentication mode, `trust` in M1; `recovered`, only when there is something to
//! say, is what the operation's write repaired before its own work, as the same record has it
//! (on success and on `E_CAS_MISMATCH`).
//!
//! A panic's envelope has the same `repo`, `resolved` and `recovered` (see [`Used`]), or none of
//! them when it came before a store was named.
//!
//! `whoami` prints `IdentityInfo` as the API writes it: for a caller without an identity,
//! `{"auth":"anonymous"}`, with `agent` omitted rather than null.

use std::any::Any;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read as _, Write as _};
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use camino::Utf8Path;
use serde::Serialize;
use serde_json::{Map, Value};
use swarmops_api::LandReq;
use swarmops_api::RebaseReq;
use swarmops_api::ResolutionsInput;
use swarmops_api::ResolveRevReq;
use swarmops_api::remote::DEFAULT_PUSH_SOURCE;
use swarmops_api::{BlobHasReq, BlobPutReq};
use swarmops_api::{
    ChangeInput, CommitReq, ContentInput, DEFAULT_DERIVE_LIMIT, DeriveReq, DeriveResp, DiffFormat,
    DiffReq, Envelope, ErrorCode, GitExportReq, GitImportReq, InfoReq, LineRange, LogReq, Meta,
    OpError, OpLogReq, PathHistoryReq, ReadFileReq, ReadTreeReq, RecoveredOut, RefDeleteReq,
    RefListReq, RefSetReq, RepoInfo, Rev, ShowReq, StoreOps, UndoReq, VerifyReport, VerifyReq,
    schema_for_all,
};
use swarmops_api::{
    CloneReq, CloneResp, FetchReq, Identity, PushReq, RemoteAddReq, RemoteListReq, RemoteRemoveReq,
    SyncMode, SyncReq, SyncStatusReq,
};
use swarmops_ops::content::MAX_REQUEST_BYTES;
use swarmops_ops::identity::{AUTH_TRUST, identity_info};
use swarmops_ops::{LocalOps, Recording, Resolved, shell_word};

use crate::cli::{
    self, BlobCommand, Command, DiffArgs, DiffFormatArg, GitCommand, Global, OpCommand, RefCommand,
    RemoteCommand, SyncCommand,
};
use crate::context::{self, Env};
use crate::mcp;
use crate::output::{self, Format, Printed};

/// The client file `blob put -` reads: this process's stdin, through the path it is open at.
const STDIN: &str = "/dev/stdin";

/// The hint of a change list past [`MAX_REQUEST_BYTES`]: what takes contents out of it.
const REQUEST_HINT: &str = "send large contents as {\"file\": path} content, or store them first \
                            with swarmops blob put and name them by id: {\"blob\": id}";

/// The hint of a failed deep `verify`, which leaves nothing to re-check.
const RESTORE_HINT: &str = "details.findings names each damaged object, record or ref by kind and \
                            id: restore them from a backup of the store";

/// What every command runs with besides its own arguments.
pub struct Ctx<'a> {
    /// The global flags.
    pub global: &'a Global,
    /// How environment variables are read.
    pub env: Env<'a>,
    /// How the envelope is printed.
    pub format: Format,
    /// When the process started, for `meta.elapsed_ms`.
    pub started: Instant,
    /// What the command has used so far, for `meta`, a panic's envelope's too.
    pub used: Used,
}

/// What a command has used so far: the store the user named, and the revisions its operation
/// resolved and what its write repaired. It lives outside the code that can panic, so that a
/// panic's envelope can report what the command's own would: `meta.repo`, `meta.resolved` and
/// `meta.recovered`.
#[derive(Default)]
pub struct Used {
    /// The store as the user named it.
    repo: RefCell<Option<String>>,
    /// What the command's operation resolved and repaired: its request's record, and no
    /// other's.
    resolved: Resolved,
}

impl Used {
    /// `meta.repo`, `meta.resolved` and `meta.recovered` as far as the command got: the store
    /// named, and what its operation resolved, and its write repaired, since this was last asked.
    pub fn meta(
        &self,
    ) -> (
        Option<String>,
        BTreeMap<String, String>,
        Option<RecoveredOut>,
    ) {
        (
            self.repo.borrow().clone(),
            self.resolved.take(),
            self.resolved.take_recovered(),
        )
    }
}

/// Runs `command`, which names operation `op`, and renders its envelope.
pub fn run(ctx: &Ctx<'_>, op: &'static str, command: Command) -> Printed {
    let agent = ctx.global.agent.as_deref();
    match command {
        Command::Init { dir, name } => {
            if !dir.is_empty() {
                ctx.used.repo.replace(Some(dir.clone()));
            }
            let result = init(&dir, name.as_deref());
            let (repo, resolved, recovered) = ctx.used.meta();
            ctx.finish(op, (repo, resolved, recovered), result)
        }
        Command::Info => ctx.on_store(op, || Ok(InfoReq {}), |ops, req| ops.repo_info(req)),
        Command::Whoami => {
            let result = context::identity(agent, ctx.env).map(|who| identity_info(who.as_ref()));
            ctx.finish(op, (None, BTreeMap::new(), None), result)
        }
        Command::Schema { name } => {
            ctx.finish(op, (None, BTreeMap::new(), None), schema(name.as_deref()))
        }
        Command::Commit {
            message,
            changes,
            parent,
            allow_empty,
            advance,
            key,
        } => ctx.on_store(
            op,
            || {
                let who = context::mutator(agent, ctx.env)?;
                let req = CommitReq {
                    parent: parent.map(Rev::from),
                    changes: read_changes(&changes)?,
                    message,
                    advance,
                    idempotency_key: key,
                    allow_empty,
                };
                Ok((who, req))
            },
            |ops, (who, req)| ops.commit(&who, req),
        ),
        land @ Command::Land { .. } => run_land(ctx, op, land),
        rebase @ Command::Rebase { .. } => run_rebase(ctx, op, rebase),
        Command::Blob(command) => run_blob(ctx, op, command),
        Command::Resolve { rev } => resolve_rev(ctx, op, rev),
        Command::Cat {
            target,
            offset,
            lines,
            max_bytes,
        } => ctx.on_store(
            op,
            || cat_request(&target, offset, lines, max_bytes),
            |ops, req| ops.read_file(req),
        ),
        Command::Ls {
            target,
            depth,
            limit,
            cursor,
        } => ctx.on_store(
            op,
            || {
                let (rev, path) = split_target(&target);
                Ok(ReadTreeReq {
                    rev: rev.into(),
                    path: path.map(str::to_owned),
                    depth,
                    limit,
                    cursor,
                })
            },
            |ops, req| ops.read_tree(req),
        ),
        Command::Show { rev, max_changes } => show(ctx, op, rev, max_changes),
        Command::Log {
            rev,
            limit,
            cursor,
            paths,
        } => log(ctx, op, rev, limit, cursor, paths),
        Command::History {
            target,
            limit,
            cursor,
        } => history(ctx, op, &target, limit, cursor),
        Command::Diff(args) => diff(ctx, op, args),
        Command::Ref(command) => run_ref(ctx, op, command),
        Command::Op(command) => run_op(ctx, op, command),
        Command::Verify { deep } => ctx.on_store(
            op,
            || Ok(VerifyReq { deep }),
            |ops, req| ops.verify(req).and_then(|report| clean(report, deep)),
        ),
        Command::Git(command) => run_git(ctx, op, command),
        Command::Mcp => mcp::serve(ctx.global, ctx.env),
        derive @ Command::Derive { .. } => run_derive(ctx, op, &derive),
        // The rest: the git remotes' commands (M13).
        remotes => run_remotes(ctx, op, remotes),
    }
}

/// Runs `show`, which names operation `op`: the changeset `rev` names, with at most
/// `max_changes` of its changes.
fn show(ctx: &Ctx<'_>, op: &'static str, rev: String, max_changes: usize) -> Printed {
    let req = ShowReq {
        rev: rev.into(),
        max_changes,
    };
    ctx.on_store(op, || Ok(req), |ops, req| ops.show(req))
}

/// Runs `log`, which names operation `op`: `limit` changesets from `rev` (`main` by default) or
/// after `cursor`, only those that change `paths` if it names any.
fn log(
    ctx: &Ctx<'_>,
    op: &'static str,
    rev: Option<String>,
    limit: usize,
    cursor: Option<String>,
    paths: Vec<String>,
) -> Printed {
    let req = LogReq {
        rev: rev.map(Rev::from),
        limit,
        cursor,
        paths,
    };
    ctx.on_store(op, || Ok(req), |ops, req| ops.log(req))
}

/// Runs `history`, which names operation `op`: `limit` changesets of the history of `target`'s
/// path (`<rev>:<path>`), from its revision or after `cursor`.
fn history(
    ctx: &Ctx<'_>,
    op: &'static str,
    target: &str,
    limit: usize,
    cursor: Option<String>,
) -> Printed {
    ctx.on_store(
        op,
        || {
            let (rev, path) = history_target(target)?;
            Ok(PathHistoryReq {
                rev: Some(rev.into()),
                path: path.to_owned(),
                limit,
                cursor,
            })
        },
        |ops, req| ops.path_history(req),
    )
}

/// Runs `land`, which names operation `op`, as the caller: `command` is the land's arguments.
fn run_land(ctx: &Ctx<'_>, op: &'static str, command: Command) -> Printed {
    let Command::Land {
        head,
        base,
        target,
        message,
        key,
        max_attempts,
        max_conflict_bytes,
        resolve,
    } = command
    else {
        unreachable!("run passes land its own command alone");
    };
    ctx.on_store(
        op,
        || {
            let who = context::mutator(ctx.global.agent.as_deref(), ctx.env)?;
            let req = LandReq {
                head: head.into(),
                base: base.map(Rev::from),
                target,
                message,
                idempotency_key: key,
                max_attempts,
                max_conflict_bytes,
                expect_target: None,
                resolutions: resolve.as_deref().map(read_resolutions).transpose()?,
            };
            Ok((who, req))
        },
        |ops, (who, req)| ops.land(&who, req),
    )
}

/// Runs `rebase`, which names operation `op`.
fn run_rebase(ctx: &Ctx<'_>, op: &'static str, command: Command) -> Printed {
    let Command::Rebase {
        head,
        base,
        onto,
        message,
        key,
        max_conflict_bytes,
        resolve,
    } = command
    else {
        unreachable!("run passes rebase its own command alone");
    };
    ctx.on_store(
        op,
        || {
            let who = context::mutator(ctx.global.agent.as_deref(), ctx.env)?;
            let req = RebaseReq {
                head: head.into(),
                base: base.map(Rev::from),
                onto,
                message,
                idempotency_key: key,
                max_conflict_bytes,
                resolutions: resolve.as_deref().map(read_resolutions).transpose()?,
            };
            Ok((who, req))
        },
        |ops, (who, req)| ops.rebase(&who, req),
    )
}

/// Runs a `blob` subcommand, which names operation `op`.
fn run_blob(ctx: &Ctx<'_>, op: &'static str, command: BlobCommand) -> Printed {
    let agent = ctx.global.agent.as_deref();
    match command {
        BlobCommand::Put { file } => ctx.on_store(
            op,
            || {
                let who = context::mutator(agent, ctx.env)?;
                let file = if file == "-" { STDIN.to_owned() } else { file };
                let req = BlobPutReq {
                    content: ContentInput::File(file),
                };
                Ok((who, req))
            },
            |ops, (who, req)| ops.blob_put(&who, req),
        ),
        BlobCommand::Has { ids } => {
            ctx.on_store(op, || Ok(BlobHasReq { ids }), |ops, req| ops.blob_has(req))
        }
    }
}

/// Runs `derive`, which names operation `op`: once, or with `--follow` until it is killed.
fn run_derive(ctx: &Ctx<'_>, op: &'static str, command: &Command) -> Printed {
    let &Command::Derive {
        after,
        follow,
        interval_ms,
        from_start,
    } = command
    else {
        unreachable!("run passes derive its own command alone");
    };
    if follow {
        let interval = Duration::from_millis(interval_ms);
        return follow_journal(ctx, op, after, from_start, interval);
    }
    ctx.on_store(
        op,
        || {
            Ok(DeriveReq {
                after,
                limit: DEFAULT_DERIVE_LIMIT,
            })
        },
        |ops, req| ops.derive(req),
    )
}

/// `derive --follow`: `derive` on the store, run after run, until the process is killed: from
/// the journal's end, from its first frame with `from_start`, or after frame `after`. A run that
/// read frames is followed by the next at once, one that read none after a wait of `interval`.
/// Each run that derived something prints its envelope at once, so that a reader of the output
/// has it as it comes. A run that fails, or a store that does not open, ends the command with its
/// envelope, which [`crate::run`] prints last, and its exit code; output that cannot be written
/// ends it with exit code 1, said on stderr.
fn follow_journal(
    ctx: &Ctx<'_>,
    op: &str,
    after: Option<u64>,
    from_start: bool,
    interval: Duration,
) -> Printed {
    let named = context::store_named(ctx.global.repo.as_deref(), ctx.env);
    if let Ok(Some(store)) = &named {
        ctx.used.repo.replace(Some(store.clone()));
    }
    let opened = named.and_then(|named| {
        let store = named.ok_or_else(|| context::no_store_named(ctx.global.repo.as_deref()))?;
        LocalOps::open(Utf8Path::new(&store))
    });
    let local = match opened {
        Ok(local) => local,
        Err(error) => return ctx.finish(op, ctx.used.meta(), Err::<DeriveResp, _>(error)),
    };
    let ops = local.recording(&ctx.used.resolved);
    let failed = |error, started| {
        let envelope = envelope::<DeriveResp>(op, ctx.used.meta(), Err(error), started);
        output::render(&envelope, ctx.format)
    };
    let mut after = after;
    if after.is_none() && !from_start {
        // A run of no frame gives where the journal ends: what it holds is left to readers.
        let started = Instant::now();
        match ops.derive(DeriveReq { after, limit: 0 }) {
            Ok(end) => after = end.next,
            Err(error) => return failed(error, started),
        }
    }
    loop {
        let started = Instant::now();
        let limit = DEFAULT_DERIVE_LIMIT;
        let run = match ops.derive(DeriveReq { after, limit }) {
            Ok(run) => run,
            Err(error) => return failed(error, started),
        };
        let caught_up = run.next.is_none();
        after = run.next.or(after);
        if run.derived > 0 {
            let envelope = envelope(op, ctx.used.meta(), Ok(run), started);
            if let Err(e) = write_now(&output::render(&envelope, ctx.format).stdout) {
                // The reader is gone, or stdout is broken: nobody gets what follows.
                let _ = writeln!(io::stderr(), "swarmops: cannot write the envelope: {e}");
                return Printed {
                    stdout: String::new(),
                    exit: 1,
                };
            }
        }
        if caught_up {
            thread::sleep(interval);
        }
    }
}

/// Writes `text` to stdout and flushes it, so that a reader has it at once: one envelope of
/// `derive --follow`'s output.
fn write_now(text: &str) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    stdout.write_all(text.as_bytes())?;
    stdout.flush()
}

/// Runs `diff`, which names operation `op`: the files that differ between `args`' two
/// revisions, a page at a time.
fn diff(ctx: &Ctx<'_>, op: &'static str, args: DiffArgs) -> Printed {
    let DiffArgs {
        from,
        to,
        paths,
        format,
        context,
        limit,
        cursor,
        max_bytes,
    } = args;
    let req = DiffReq {
        from: from.into(),
        to: to.into(),
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
    };
    ctx.on_store(op, || Ok(req), |ops, req| ops.diff(req))
}

/// Runs `resolve`, which names operation `op`: the changeset `rev` names, and its kind.
fn resolve_rev(ctx: &Ctx<'_>, op: &'static str, rev: String) -> Printed {
    let req = ResolveRevReq { rev: rev.into() };
    ctx.on_store(op, || Ok(req), |ops, req| ops.resolve_rev(req))
}

/// Runs a `ref` subcommand, which names operation `op`.
fn run_ref(ctx: &Ctx<'_>, op: &'static str, command: RefCommand) -> Printed {
    let agent = ctx.global.agent.as_deref();
    match command {
        RefCommand::List => ctx.on_store(op, || Ok(RefListReq {}), |ops, req| ops.ref_list(req)),
        RefCommand::Set {
            name,
            rev,
            expect,
            create,
        } => ctx.on_store(
            op,
            || {
                let req = RefSetReq {
                    name,
                    new: rev.into(),
                    expected_old: expect.map(Rev::from),
                    create,
                };
                Ok((context::mutator(agent, ctx.env)?, req))
            },
            |ops, (who, req)| ops.ref_set(&who, req),
        ),
        RefCommand::Delete { name, expect } => ctx.on_store(
            op,
            || {
                let req = RefDeleteReq {
                    name,
                    expected_old: expect.into(),
                };
                Ok((context::mutator(agent, ctx.env)?, req))
            },
            |ops, (who, req)| ops.ref_delete(&who, req),
        ),
    }
}

/// Runs an `op` subcommand, which names operation `op`.
fn run_op(ctx: &Ctx<'_>, op: &'static str, command: OpCommand) -> Printed {
    match command {
        OpCommand::Log {
            before,
            limit,
            max_changesets,
        } => ctx.on_store(
            op,
            || {
                Ok(OpLogReq {
                    before,
                    limit,
                    max_changesets,
                })
            },
            |ops, req| ops.op_log(req),
        ),
        OpCommand::Undo { op: number } => ctx.on_store(
            op,
            || {
                let who = context::mutator(ctx.global.agent.as_deref(), ctx.env)?;
                Ok((who, UndoReq { op: number }))
            },
            |ops, (who, req)| ops.undo(&who, req),
        ),
    }
}

impl Ctx<'_> {
    /// Runs operation `op` on the store the user named: `prepare` builds its input without
    /// the store, then `call` runs it on the store, opened. Both are kept in [`Ctx::used`].
    pub fn on_store<P, T: Serialize>(
        &self,
        op: &str,
        prepare: impl FnOnce() -> Result<P, OpError>,
        call: impl FnOnce(&dyn StoreOps, P) -> Result<T, OpError>,
    ) -> Printed {
        output::render(&self.envelope_on_store(op, prepare, call), self.format)
    }

    /// [`Ctx::on_store`]'s envelope, before it is printed: what `swarmops mcp` returns as a
    /// tool's result.
    pub fn envelope_on_store<P, T>(
        &self,
        op: &str,
        prepare: impl FnOnce() -> Result<P, OpError>,
        call: impl FnOnce(&dyn StoreOps, P) -> Result<T, OpError>,
    ) -> Envelope<T> {
        self.envelope_on_local(op, prepare, |ops, input| call(&ops, input))
    }

    /// Runs operation `op`, a client-side one that no [`StoreOps`] has (`git import`,
    /// `git export`), on the store the user named, as [`Ctx::on_store`] runs one: `prepare`
    /// builds its input without the store, then `call` runs it on the store, opened, handed the
    /// concrete operations of this one request, which record what the request resolves and
    /// repairs in [`Ctx::used`].
    pub fn on_local<P, T: Serialize>(
        &self,
        op: &str,
        prepare: impl FnOnce() -> Result<P, OpError>,
        call: impl FnOnce(Recording<'_>, P) -> Result<T, OpError>,
    ) -> Printed {
        output::render(&self.envelope_on_local(op, prepare, call), self.format)
    }

    /// [`Ctx::on_local`]'s envelope, before it is printed, and [`Ctx::envelope_on_store`]'s:
    /// what `swarmops mcp` returns as the result of a client-side tool (`fetch`, `push`, …).
    pub(crate) fn envelope_on_local<P, T>(
        &self,
        op: &str,
        prepare: impl FnOnce() -> Result<P, OpError>,
        call: impl FnOnce(Recording<'_>, P) -> Result<T, OpError>,
    ) -> Envelope<T> {
        self.envelope_at_root(op, prepare, |ops, _root, input| call(ops, input))
    }

    /// [`Ctx::envelope_on_local`], with `call` handed the store's root too, as the user named
    /// it: what a workspace tool of `swarmops mcp` runs on (M5), since `StoreOps` carries no
    /// path and the workspaces' registry is a directory of the store's. The one place that
    /// opens the store for both.
    pub(crate) fn envelope_at_root<P, T>(
        &self,
        op: &str,
        prepare: impl FnOnce() -> Result<P, OpError>,
        call: impl FnOnce(Recording<'_>, &Utf8Path, P) -> Result<T, OpError>,
    ) -> Envelope<T> {
        // 1. The store the user named, kept for meta.repo.
        let named = context::store_named(self.global.repo.as_deref(), self.env);
        if let Ok(Some(store)) = &named {
            self.used.repo.replace(Some(store.clone()));
        }

        // 2. The input, then the store opened, then the call on both.
        let result = named.and_then(|named| {
            let input = prepare()?;
            let store =
                named.ok_or_else(|| context::no_store_named(self.global.repo.as_deref()))?;
            let root = Utf8Path::new(&store);
            let ops = LocalOps::open(root)?;
            call(ops.recording(&self.used.resolved), root, input)
        });
        envelope(op, self.used.meta(), result, self.started)
    }

    /// `op`'s envelope for `result`, with `meta` from `used` (what [`Used::meta`] gives) and the
    /// clock.
    fn finish<T: Serialize>(
        &self,
        op: &str,
        used: UsedMeta,
        result: Result<T, OpError>,
    ) -> Printed {
        output::render(&envelope(op, used, result, self.started), self.format)
    }
}

/// What [`Used::meta`] gives: `meta.repo`, `meta.resolved` and `meta.recovered`.
type UsedMeta = (
    Option<String>,
    BTreeMap<String, String>,
    Option<RecoveredOut>,
);

/// The envelope of `op` failing with `error` before it could use a store.
pub fn failed(op: &str, error: OpError, format: Format, started: Instant) -> Printed {
    let envelope = envelope::<()>(op, (None, BTreeMap::new(), None), Err(error), started);
    output::render(&envelope, format)
}

/// The envelope of `op` failing with `error` after it used what `used` holds, which `meta`
/// reports.
pub fn failed_after(
    op: &str,
    error: OpError,
    used: &Used,
    format: Format,
    started: Instant,
) -> Printed {
    output::render(&failed_envelope(op, error, used, started), format)
}

/// [`failed_after`]'s envelope, before it is printed.
pub fn failed_envelope(op: &str, error: OpError, used: &Used, started: Instant) -> Envelope<()> {
    envelope(op, used.meta(), Err(error), started)
}

/// The error a panic with `payload` stands for: `E_INTERNAL`, a bug, with the panic's message
/// in one line (the panic hook has printed where it happened on stderr).
pub fn panicked(payload: &(dyn Any + Send)) -> OpError {
    let message = payload
        .downcast_ref::<&str>()
        .map(|message| (*message).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a panic without a message".to_owned());
    OpError::new(
        ErrorCode::Internal,
        format!(
            "swarmops panicked, a bug (stderr has where): {}",
            output::printable(&message)
        ),
    )
}

/// The envelope of a command line clap refused: `E_INVALID_ARG` (exit 2) with clap's message
/// in one line as `message`, and its tips and the usage of the subcommand `path` names as
/// `hint`, a line each. The usage names the program `bin`, as the line invoked it, whether it
/// is clap's or, where clap gives none, the CLI's. Control characters (from the line itself)
/// are escaped in both.
pub fn usage(
    error: &clap::Error,
    bin: Option<&str>,
    path: &[String],
    format: Format,
    started: Instant,
) -> Printed {
    let rendered = error.render().to_string();
    let mut paragraphs = rendered
        .split("\n\n")
        .map(str::trim)
        .filter(|paragraph| !paragraph.is_empty());
    let first = paragraphs.next().unwrap_or("the command line is malformed");
    let first = first.strip_prefix("error: ").unwrap_or(first);
    let message: Vec<&str> = first.lines().map(str::trim).collect();
    let mut hint: Vec<String> = paragraphs
        .flat_map(str::lines)
        .map(|text| output::printable(text.trim()))
        .collect();
    if !hint.iter().any(|text| text.starts_with("Usage:")) {
        let at = hint
            .iter()
            .position(|text| text.starts_with("For more information"))
            .unwrap_or(hint.len());
        hint.insert(at, output::printable(&cli::usage(bin, path)));
    }
    let message = output::printable(&message.join(" "));
    let error = OpError::new(ErrorCode::InvalidArg, message).with_hint(hint.join("\n"));
    failed(
        cli::op_name(path).unwrap_or(cli::NO_OP),
        error,
        format,
        started,
    )
}

/// `op`'s envelope for `result`, its `meta` from `used` (`repo`, `resolved` and `recovered`)
/// and the time since `started`.
fn envelope<T>(
    op: &str,
    (repo, resolved, recovered): UsedMeta,
    result: Result<T, OpError>,
    started: Instant,
) -> Envelope<T> {
    let meta = Meta {
        repo,
        elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        resolved,
        auth: AUTH_TRUST.to_owned(),
        recovered,
    };
    match result {
        Ok(result) => Envelope::ok(op, result, meta),
        Err(error) => Envelope::err(op, error, meta),
    }
}

/// Runs a `git` subcommand, which names operation `op`: client-side operations, which read or
/// write a git repository on this host and no [`StoreOps`] has. `git import` needs an identity,
/// `git export` none.
fn run_git(ctx: &Ctx<'_>, op: &'static str, command: GitCommand) -> Printed {
    let agent = ctx.global.agent.as_deref();
    match command {
        GitCommand::Import {
            path,
            git_ref,
            into,
            lossy,
            gitlinks,
        } => ctx.on_local(
            op,
            || {
                let req = GitImportReq {
                    git_path: path,
                    git_ref,
                    target_ref: into,
                    lossy: cli::lossy_classes(&lossy, gitlinks),
                };
                Ok((context::mutator(agent, ctx.env)?, req))
            },
            |ops, (who, req)| ops.git_import(&who, req),
        ),
        GitCommand::Export {
            path,
            source_ref,
            branch,
            force,
        } => ctx.on_local(
            op,
            || {
                Ok(GitExportReq {
                    git_path: path,
                    source_ref,
                    branch,
                    force,
                })
            },
            |ops, req| ops.git_export(req),
        ),
    }
}

/// Runs one of the git remotes' commands (M13), which names operation `op`.
fn run_remotes(ctx: &Ctx<'_>, op: &'static str, command: Command) -> Printed {
    match command {
        clone @ Command::Clone { .. } => run_clone(ctx, op, clone),
        Command::Remote(command) => run_remote(ctx, op, command),
        Command::Fetch { remote } => fetch(ctx, op, remote),
        Command::Push { remote, target } => push(ctx, op, remote, target.as_deref()),
        sync @ Command::Sync { .. } => run_sync(ctx, op, sync),
        other => unreachable!("run passes the remotes' commands alone, not {other:?}"),
    }
}

/// Runs `clone`, which names operation `op`, as the caller: `command` is the clone's arguments.
/// It runs as `init` does, since there is no store to open before it: the identity is checked
/// before anything is made, and the directory is the envelope's `repo`.
fn run_clone(ctx: &Ctx<'_>, op: &'static str, command: Command) -> Printed {
    let Command::Clone {
        url,
        into,
        name,
        branch,
        remote,
        token_env,
        lossy,
    } = command
    else {
        unreachable!("run passes clone its own command alone");
    };
    if !into.is_empty() {
        ctx.used.repo.replace(Some(into.clone()));
    }

    // The identity first: nothing is made without one.
    let result = context::mutator(ctx.global.agent.as_deref(), ctx.env).and_then(|who| {
        let req = CloneReq {
            url,
            path: into,
            name,
            branch,
            remote,
            token_env,
            lossy: cli::lossy_classes(&lossy, None),
        };
        clone(req, &who, &ctx.used.resolved)
    });
    ctx.finish(op, ctx.used.meta(), result)
}

/// `clone`: the store `req` makes, as `who`, its ref move recorded in `resolved`.
fn clone(req: CloneReq, who: &Identity, resolved: &Resolved) -> Result<CloneResp, OpError> {
    let (_, resp) = LocalOps::clone_repo(req, who, resolved)?;
    Ok(resp)
}

/// Runs a `remote` subcommand, which names operation `op`: the store's git remotes, which no
/// [`StoreOps`] has. None needs an identity: they write the remote configuration and the
/// mirror, no store object.
fn run_remote(ctx: &Ctx<'_>, op: &'static str, command: RemoteCommand) -> Printed {
    match command {
        RemoteCommand::Add {
            name,
            url,
            branch,
            token_env,
            lossy,
        } => ctx.on_local(
            op,
            || {
                Ok(RemoteAddReq {
                    name,
                    url,
                    branch,
                    token_env,
                    lossy: cli::lossy_classes(&lossy, None),
                })
            },
            |ops, req| ops.remote_add(req),
        ),
        RemoteCommand::Remove { name } => ctx.on_local(
            op,
            || Ok(RemoteRemoveReq { name }),
            |ops, req| ops.remote_remove(req),
        ),
        RemoteCommand::List => {
            ctx.on_local(op, || Ok(RemoteListReq {}), |ops, req| ops.remote_list(req))
        }
    }
}

/// Runs `fetch`, which names operation `op`: `remote`'s history into the store's mirror and its
/// followed branch into the store. It needs no identity.
fn fetch(ctx: &Ctx<'_>, op: &'static str, remote: String) -> Printed {
    ctx.on_local(op, || Ok(FetchReq { remote }), |ops, req| ops.fetch(req))
}

/// Runs `push`, which names operation `op`: `target` sent to `remote`. It needs no identity.
fn push(ctx: &Ctx<'_>, op: &'static str, remote: String, target: Option<&str>) -> Printed {
    ctx.on_local(
        op,
        || Ok(push_request(remote, target)),
        |ops, req| ops.push(req),
    )
}

/// `push`'s request: `target`, `<ref>[:<branch>]`, sent to `remote`; the ref is `main` and the
/// branch the remote's default for it when the line names none.
fn push_request(remote: String, target: Option<&str>) -> PushReq {
    let target = target.unwrap_or(DEFAULT_PUSH_SOURCE);
    let (source_ref, branch) = match target.split_once(':') {
        Some((source_ref, branch)) => (source_ref, Some(branch.to_owned())),
        None => (target, None),
    };
    PushReq {
        remote,
        source_ref: source_ref.to_owned(),
        branch,
    }
}

/// Runs `sync` or `sync status`, which names operation `op`: `command` is the sync's arguments.
/// `sync` needs an identity, as it moves main; `sync status` only reads.
fn run_sync(ctx: &Ctx<'_>, op: &'static str, command: Command) -> Printed {
    let Command::Sync {
        remote,
        no_push,
        max_attempts,
        status,
    } = command
    else {
        unreachable!("run passes sync its own command alone");
    };

    // 1. `sync status`, offline.
    if let Some(SyncCommand::Status { remote, limit }) = status {
        return ctx.on_local(
            op,
            || Ok(SyncStatusReq { remote, limit }),
            |ops, req| ops.sync_status(req),
        );
    }

    // 2. `sync`, as the caller.
    let mode = if no_push {
        SyncMode::Local
    } else {
        SyncMode::Publish
    };
    ctx.on_local(
        op,
        || {
            let remote = remote.ok_or_else(|| {
                OpError::new(ErrorCode::InvalidArg, "sync needs the remote to sync with")
            })?;
            let req = SyncReq {
                remote,
                mode,
                max_attempts,
            };
            Ok((context::mutator(ctx.global.agent.as_deref(), ctx.env)?, req))
        },
        |ops, (who, req)| ops.sync(&who, req),
    )
}

/// `init`: a store in `dir` for a repository named `name`, by default the directory's name.
fn init(dir: &str, name: Option<&str>) -> Result<RepoInfo, OpError> {
    if dir.is_empty() {
        return Err(OpError::new(
            ErrorCode::InvalidArg,
            "init needs a directory to create the store in",
        ));
    }
    let name = match name {
        Some(name) => name.to_owned(),
        None => default_name(dir)?,
    };
    let (_, info) = LocalOps::init(Utf8Path::new(dir), &name)?;
    Ok(info)
}

/// The name `init` gives a repository in `dir` when `--name` gives none: the directory's own
/// name, from the path, or for `.`, `..` and the like from the filesystem.
fn default_name(dir: &str) -> Result<String, OpError> {
    if let Some(name) = Utf8Path::new(dir).file_name() {
        return Ok(name.to_owned());
    }
    fs::canonicalize(dir)
        .ok()
        .as_deref()
        .and_then(Path::file_name)
        .and_then(OsStr::to_str)
        .map(str::to_owned)
        .ok_or_else(|| {
            OpError::new(
                ErrorCode::InvalidArg,
                format!("{dir:?} has no name to give the repository"),
            )
            .with_details(details([("dir", dir.into())]))
            .with_hint(format!("swarmops init {} --name <name>", quoted(dir)))
        })
}

/// `schema`: every type's JSON Schema, or `name`'s alone, keyed by type name either way.
fn schema(name: Option<&str>) -> Result<Value, OpError> {
    let all = schema_for_all();
    let Some(name) = name else {
        return Ok(all);
    };
    let Some(schema) = all.get(name) else {
        let types: Vec<Value> = all
            .as_object()
            .into_iter()
            .flat_map(|types| types.keys().cloned().map(Value::from))
            .collect();
        return Err(OpError::new(
            ErrorCode::InvalidArg,
            format!("the schema has no type {name:?}"),
        )
        .with_details(details([("name", name.into()), ("types", types.into())]))
        .with_hint("swarmops schema"));
    };
    Ok(Value::Object(Map::from_iter([(
        name.to_owned(),
        schema.clone(),
    )])))
}

/// A commit's change list: the JSON array of changes in file `source`, or on stdin for `-`, of
/// at most [`MAX_REQUEST_BYTES`] ([`read_request`]).
fn read_changes(source: &str) -> Result<Vec<ChangeInput>, OpError> {
    read_changes_within(source, MAX_REQUEST_BYTES)
}

/// [`read_changes`], with at most `limit` bytes of text.
fn read_changes_within(source: &str, limit: u64) -> Result<Vec<ChangeInput>, OpError> {
    let bytes = read_request(source, limit, "changes", "the changes")?;
    let changes: Vec<ChangeInput> = serde_json::from_slice(&bytes).map_err(|e| {
        OpError::new(
            ErrorCode::InvalidArg,
            format!(
                "the changes in {} are not a JSON array of changes: {e}",
                shown(source)
            ),
        )
        .with_details(details([("changes", source.into())]))
        .with_hint(
            r#"the changes are an array such as [{"path": "a.txt", "op": "put", "content": {"inline": "hi"}}]; swarmops schema ChangeInput gives the rest"#,
        )
    })?;
    if source == "-" {
        // The list has drained stdin: a change whose client file is this process's stdin would
        // read what is left of it (nothing from a pipe, the list itself from a redirected file)
        // and commit that. Stdout is the envelope's, and a read of it would block.
        let own = changes
            .iter()
            .enumerate()
            .find_map(|(index, change)| match &change.content {
                Some(ContentInput::File(file)) if is_own_stdio(file) => Some((index, change, file)),
                _ => None,
            });
        if let Some((index, change, file)) = own {
            return Err(OpError::new(
                ErrorCode::InvalidArg,
                output::printable(&format!(
                    "change {index} ({:?}): the client file {file:?} is this process's own \
                     stdin, which the change list came from, or its stdout: name a regular file, \
                     or send the bytes as inline or base64 content",
                    change.path
                )),
            )
            .with_details(details([
                ("changes", source.into()),
                ("index", index.into()),
                ("path", change.path.as_str().into()),
                ("file", file.as_str().into()),
            ])));
        }
    }
    Ok(changes)
}

/// A land's resolutions (`--resolve`): the JSON object of resolutions in file `source`, or on
/// stdin for `-`, of at most [`MAX_REQUEST_BYTES`] ([`read_request`]). Read from stdin, they may
/// not name this process's own stdin or stdout as a client file, as a change list may not.
fn read_resolutions(source: &str) -> Result<ResolutionsInput, OpError> {
    let bytes = read_request(source, MAX_REQUEST_BYTES, "resolve", "the resolutions")?;
    let resolutions: ResolutionsInput = serde_json::from_slice(&bytes).map_err(|e| {
        OpError::new(
            ErrorCode::InvalidArg,
            format!(
                "the resolutions in {} are not a JSON object of resolutions: {e}",
                shown(source)
            ),
        )
        .with_details(details([("resolve", source.into())]))
        .with_hint(
            r#"the resolutions are an object such as {"against": "<the report's theirs>", "files": {"a.txt": {"take": "ours"}}}; swarmops schema ResolutionsInput gives the rest"#,
        )
    })?;
    if source == "-" {
        // As for a change list read from stdin (`read_changes_within`).
        let own =
            (resolutions.files.iter()).find_map(|(path, resolution)| match &resolution.content {
                Some(ContentInput::File(file)) if is_own_stdio(file) => Some((path, file)),
                _ => None,
            });
        if let Some((path, file)) = own {
            return Err(OpError::new(
                ErrorCode::InvalidArg,
                output::printable(&format!(
                    "the resolution of {path:?}: the client file {file:?} is this process's own \
                     stdin, which the resolutions came from, or its stdout: name a regular file, \
                     or send the bytes as inline or base64 content"
                )),
            )
            .with_details(details([
                ("resolve", source.into()),
                ("path", path.as_str().into()),
                ("file", file.as_str().into()),
            ])));
        }
    }
    Ok(resolutions)
}

/// The text of a request that the client reads, its `flag`'s (`changes`, `resolve`), which its
/// errors name in their `details` and call `what` ("the changes", "the resolutions"): file
/// `source`'s, or stdin's for `-`, of at most `limit` bytes. A file's length is checked before it
/// is read, and either is read no further than one byte past the limit, so that a text past it is
/// refused (`E_TOO_LARGE`, scope `request`) before it is parsed. One that cannot be read is
/// `E_INVALID_ARG`.
fn read_request(source: &str, limit: u64, flag: &str, what: &str) -> Result<Vec<u8>, OpError> {
    let shown = shown(source);
    let text = if source == "-" {
        read_text(io::stdin().lock(), None, limit)
    } else {
        fs::File::open(source).and_then(|file| {
            let len = file.metadata()?.len();
            read_text(file, Some(len), limit)
        })
    }
    .map_err(|e| {
        OpError::new(
            ErrorCode::InvalidArg,
            format!("cannot read {what} from {shown}: {e}"),
        )
        .with_details(details([(flag, source.into())]))
    })?;
    text.map_err(|size| {
        OpError::new(
            ErrorCode::TooLarge,
            format!("{what} in {shown} are more than the {limit} bytes a request may hold"),
        )
        .with_details(details([
            ("size", size.into()),
            ("limit", limit.into()),
            ("scope", "request".into()),
            (flag, source.into()),
        ]))
        .with_hint(REQUEST_HINT)
    })
}

/// How a request's errors call file `source`: quoted, or `stdin` for `-`.
fn shown(source: &str) -> String {
    if source == "-" {
        "stdin".to_owned()
    } else {
        format!("{source:?}")
    }
}

/// The text `reader` holds, whose length is `len` if it has one, unless it is more than `limit`
/// bytes: then `Err` with its size, `len` itself, checked before a byte is read, or `limit + 1`,
/// read no further.
fn read_text(
    reader: impl io::Read,
    len: Option<u64>,
    limit: u64,
) -> io::Result<Result<Vec<u8>, u64>> {
    if let Some(len) = len.filter(|&len| len > limit) {
        return Ok(Err(len));
    }
    let mut bytes = Vec::new();
    reader
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)?;
    let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    Ok(if size > limit { Err(size) } else { Ok(bytes) })
}

/// Whether `path` names this process's own stdin or stdout (the same device and inode), which
/// `swarmops mcp` must never read: they carry the protocol, and a read would block on them.
/// `stat`, not `open`, so that a FIFO at `path` cannot block here either; a path that cannot be
/// stat'ed is not ours, and the read reports its own error later.
pub(crate) fn is_own_stdio(path: &str) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    let own = |fd: std::os::fd::BorrowedFd<'_>| {
        fd.try_clone_to_owned()
            .ok()
            .and_then(|owned| fs::File::from(owned).metadata().ok())
            .is_some_and(|ours| ours.dev() == meta.dev() && ours.ino() == meta.ino())
    };
    own(io::stdin().as_fd()) || own(io::stdout().as_fd())
}

/// `<rev>:<path>` split at the first `:`, or all of `target` as the revision when it holds
/// none (`ls <rev>`).
///
/// This is the M1 revision grammar's split, where a revision never holds a `:` and a path
/// may. `ws:<name>` and `cr:CR-n[@k]` revisions arrive in later milestones, and the split must
/// become prefix-aware then.
fn split_target(target: &str) -> (&str, Option<&str>) {
    match target.split_once(':') {
        Some((rev, path)) => (rev, Some(path)),
        None => (target, None),
    }
}

/// `cat`'s request: `target`'s revision and path ([`file_target`]), read from `offset` or by
/// `lines`, `max_bytes` at most.
fn cat_request(
    target: &str,
    offset: u64,
    lines: Option<LineRange>,
    max_bytes: u64,
) -> Result<ReadFileReq, OpError> {
    let (rev, path) = file_target(target)?;
    Ok(ReadFileReq {
        rev: rev.into(),
        path: path.to_owned(),
        offset,
        lines,
        max_bytes,
    })
}

/// `cat`'s `<rev>:<path>`, as [`split_target`] splits it; a target without a path is
/// `E_INVALID_ARG`.
fn file_target(target: &str) -> Result<(&str, &str), OpError> {
    let (rev, path) = split_target(target);
    path.map(|path| (rev, path)).ok_or_else(|| {
        OpError::new(
            ErrorCode::InvalidArg,
            format!("{target:?} names no file: cat takes <rev>:<path>"),
        )
        .with_details(details([("target", target.into())]))
        .with_hint(format!("swarmops ls {}", quoted(target)))
    })
}

/// `history`'s `<rev>:<path>`, as [`split_target`] splits it; a target without a path is
/// `E_INVALID_ARG`.
fn history_target(target: &str) -> Result<(&str, &str), OpError> {
    let (rev, path) = split_target(target);
    path.map(|path| (rev, path)).ok_or_else(|| {
        OpError::new(
            ErrorCode::InvalidArg,
            format!("{target:?} names no path: history takes <rev>:<path>"),
        )
        .with_details(details([("target", target.into())]))
        .with_hint(format!("swarmops ls {}", quoted(target)))
    })
}

/// `verify`'s report as the command's outcome: a success for a clean store, else
/// `E_STORE_CORRUPT` (exit 1) with the whole report in `details`. The hint is a deep
/// verify after a structural one, which finds damaged bytes too, and after a deep one the
/// restore that remains.
pub(crate) fn clean(report: VerifyReport, deep: bool) -> Result<VerifyReport, OpError> {
    let Some(first) = report.problems.first() else {
        return Ok(report);
    };
    let message = match report.problems.len() {
        1 => format!("verify found a problem: {first}"),
        n => format!("verify found {n} problems, the first: {first}"),
    };
    let details = match serde_json::to_value(&report) {
        Ok(Value::Object(details)) => details,
        _ => Map::new(),
    };
    let hint = if deep {
        RESTORE_HINT
    } else {
        "swarmops verify --deep"
    };
    Err(OpError::new(ErrorCode::StoreCorrupt, message)
        .with_details(details)
        .with_hint(hint))
}

fn details<const N: usize>(entries: [(&str, Value); N]) -> Map<String, Value> {
    entries
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect()
}

/// `arg` as one shell word in a hint ([`shell_word`]), control characters escaped so that the
/// hint stays one line (the rule of the ops crate's hints).
fn quoted(arg: &str) -> String {
    output::printable(&shell_word(arg))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cat_splits_its_target_at_the_first_colon() {
        assert_eq!(file_target("main:a/b").unwrap(), ("main", "a/b"));
        assert_eq!(file_target("main~2:a:b:c").unwrap(), ("main~2", "a:b:c"));
        assert_eq!(file_target("main:").unwrap(), ("main", ""));
        let refused = file_target("main").unwrap_err();
        assert_eq!((refused.code.as_str(), refused.exit), ("E_INVALID_ARG", 2));
        assert_eq!(refused.hint.as_deref(), Some("swarmops ls main"));
        assert_eq!(
            file_target("my rev").unwrap_err().hint.as_deref(),
            Some("swarmops ls 'my rev'")
        );
    }

    #[test]
    fn history_splits_its_target_at_the_first_colon() {
        assert_eq!(history_target("main:src/").unwrap(), ("main", "src/"));
        assert_eq!(history_target("main~1:a:b").unwrap(), ("main~1", "a:b"));
        let refused = history_target("src/lib.rs").unwrap_err();
        assert_eq!((refused.code.as_str(), refused.exit), ("E_INVALID_ARG", 2));
        assert_eq!(refused.details["target"], "src/lib.rs");
        assert_eq!(refused.hint.as_deref(), Some("swarmops ls src/lib.rs"));
    }

    #[test]
    fn init_names_the_repository_after_its_directory() {
        assert_eq!(default_name("/tmp/x/acme").unwrap(), "acme");
        assert_eq!(default_name("acme/").unwrap(), "acme");
        assert_eq!(default_name("acme/.").unwrap(), "acme");
        let here = std::env::current_dir().unwrap();
        let here = here.file_name().unwrap().to_str().unwrap();
        assert_eq!(default_name(".").unwrap(), here);
        let root = default_name("/").unwrap_err();
        assert_eq!(root.code, "E_INVALID_ARG");
        assert_eq!(root.hint.as_deref(), Some("swarmops init / --name <name>"));
        assert_eq!(init("", None).unwrap_err().code, "E_INVALID_ARG");
    }

    #[test]
    fn a_change_list_is_a_json_array_of_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("changes.json");
        fs::write(&path, r#"[{"path": "a", "op": "delete"}]"#).unwrap();
        let path = path.to_str().unwrap();
        assert_eq!(read_changes(path).unwrap().len(), 1);
        fs::write(path, r#"{"path": "a", "op": "delete"}"#).unwrap();
        let not_array = read_changes(path).unwrap_err();
        assert_eq!(
            (not_array.code.as_str(), not_array.exit),
            ("E_INVALID_ARG", 2)
        );
        assert_eq!(not_array.details["changes"], path);
        assert!(
            not_array.message.contains("line 1 column"),
            "{}",
            not_array.message
        );
        assert!(not_array.hint.is_some());
        let missing = read_changes("/nonexistent/changes.json").unwrap_err();
        assert_eq!(missing.code, "E_INVALID_ARG");
        assert!(!missing.message.contains('\n'));
    }

    /// A change list past its limit is refused by its length, before it is read or parsed
    /// (`E_TOO_LARGE`, scope `request`), and one at the limit is read; text without a length
    /// (stdin) is read no further than one byte past the limit, and a length past it refuses the
    /// text unread.
    #[test]
    fn a_change_list_past_its_limit_is_refused_before_it_is_parsed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("changes.json");
        let path = path.to_str().unwrap();
        fs::write(path, "[]").unwrap();
        assert_eq!(read_changes_within(path, 2).unwrap(), Vec::new());
        fs::write(path, "[ ]").unwrap();
        let refused = read_changes_within(path, 2).unwrap_err();
        assert_eq!((refused.code.as_str(), refused.exit), ("E_TOO_LARGE", 1));
        assert_eq!(
            Value::Object(refused.details),
            serde_json::json!({"size": 3, "limit": 2, "scope": "request", "changes": path})
        );
        assert!(refused.hint.is_some_and(|hint| hint.contains("blob put")));
        assert_eq!(read_text(io::repeat(b' '), None, 8).unwrap(), Err(9));
        assert_eq!(read_text(io::repeat(b' '), Some(100), 8).unwrap(), Err(100));
        assert_eq!(read_text(&b"[]"[..], None, 8).unwrap(), Ok(b"[]".to_vec()));
    }

    #[test]
    fn verify_problems_are_store_corruption() {
        let report = |problems: &[&str]| VerifyReport {
            objects_checked: 5,
            changesets_checked: 2,
            problems: problems.iter().map(|p| (*p).to_owned()).collect(),
            findings: Vec::new(),
        };
        assert_eq!(clean(report(&[]), false).unwrap(), report(&[]));
        let one = clean(report(&["object x is missing"]), false).unwrap_err();
        assert_eq!((one.code.as_str(), one.exit), ("E_STORE_CORRUPT", 1));
        assert_eq!(one.message, "verify found a problem: object x is missing");
        assert_eq!(one.hint.as_deref(), Some("swarmops verify --deep"));
        assert_eq!(
            Value::Object(one.details),
            serde_json::to_value(report(&["object x is missing"])).unwrap()
        );
        let two = clean(report(&["a", "b"]), true).unwrap_err();
        assert_eq!(two.message, "verify found 2 problems, the first: a");
        assert_eq!(two.hint.as_deref(), Some(RESTORE_HINT));
    }

    #[test]
    fn schema_gives_one_type_by_name() {
        let one = schema(Some("Rev")).unwrap();
        assert_eq!(one.as_object().unwrap().len(), 1);
        assert_eq!(one["Rev"], schema_for_all()["Rev"]);
        let unknown = schema(Some("rev")).unwrap_err();
        assert_eq!((unknown.code.as_str(), unknown.exit), ("E_INVALID_ARG", 2));
        assert!(
            unknown.details["types"]
                .as_array()
                .unwrap()
                .contains(&"Rev".into())
        );
    }

    /// The usage line the CLI adds where clap gives none names the program as the line invoked
    /// it, its control characters escaped like the rest of the line's text: here a carriage
    /// return, which clap's text keeps (it strips escape sequences only).
    #[test]
    fn the_added_usage_line_escapes_the_program_name() {
        let line = ["/tmp/s\ro", "log", "-n", "many"].map(std::ffi::OsString::from);
        let cli::Parsed::Usage {
            error, bin, path, ..
        } = cli::parse(&line)
        else {
            panic!("a usage error");
        };
        let format = Format::Json { pretty: false };
        let printed = usage(&error, bin.as_deref(), &path, format, Instant::now());
        let envelope: Value = serde_json::from_str(&printed.stdout).unwrap();
        let hint = envelope["error"]["hint"].as_str().unwrap();
        let line = r"Usage: s\ro log [OPTIONS] [REV]";
        assert!(hint.lines().any(|text| text == line), "{hint:?}");
    }

    /// A newline in the line's own words is escaped like any control character, a blank line
    /// included: it cuts `message` short nowhere, and no word of the line becomes a hint line
    /// of its own, a `Usage:` line least of all. Here in a refused value, and in the program's
    /// name where clap puts it in its text.
    #[test]
    fn newlines_in_the_line_are_escaped() {
        let value = "x\n\nUsage: so ref delete main --expect 0000\n\ny";
        let program = "/tmp/s\n\nUsage: forged";
        for (line, message, usage_line) in [
            (
                vec!["so", "log", "-n", value],
                r"invalid value 'x\n\nUsage: so ref delete main --expect 0000\n\ny' for ",
                "Usage: so log [OPTIONS] [REV]",
            ),
            (
                vec![program, "ref"],
                r"'s\n\nUsage: forged ref' requires a subcommand",
                r"Usage: s\n\nUsage: forged ref [OPTIONS] <COMMAND>",
            ),
        ] {
            let line: Vec<std::ffi::OsString> = line.into_iter().map(Into::into).collect();
            let cli::Parsed::Usage {
                error, bin, path, ..
            } = cli::parse(&line)
            else {
                panic!("{line:?}: a usage error");
            };
            let format = Format::Json { pretty: false };
            let printed = usage(&error, bin.as_deref(), &path, format, Instant::now());
            let envelope: Value = serde_json::from_str(&printed.stdout).unwrap();
            let reply = &envelope["error"];
            assert!(
                reply["message"].as_str().unwrap().starts_with(message),
                "{reply}"
            );
            let hint = reply["hint"].as_str().unwrap();
            assert_eq!(
                hint.lines().collect::<Vec<_>>(),
                [usage_line, "For more information, try '--help'."],
                "{hint:?}"
            );
        }
    }

    #[test]
    fn shell_words_are_quoted_when_they_must_be() {
        assert_eq!(quoted("/tmp/a-b_c.d@e:f"), "/tmp/a-b_c.d@e:f");
        assert_eq!(quoted("main~1"), "'main~1'");
        assert_eq!(quoted("a b"), "'a b'");
        assert_eq!(quoted("it's"), r"'it'\''s'");
        assert_eq!(quoted(""), "''");
        assert_eq!(quoted("a\nb"), r"'a\nb'");
    }

    /// A panic's envelope reports what the command's write repaired before the panic, as the
    /// command's own envelope would ([`Used`]): here a commit whose write listed again the frame
    /// of a ref move that `journal.off` no longer listed.
    #[test]
    fn a_panic_s_envelope_reports_what_the_write_repaired() {
        use camino::Utf8PathBuf;
        use swarmops_api::{ChangeOp, Identity};
        use swarmops_store::Layout;
        use swarmops_testkit::store::{file_len, set_len};

        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
        let (ops, _) = LocalOps::init(&root, "acme").unwrap();
        let who = Identity::parse("agent/worker-17").unwrap();
        let commit = |content: &str| CommitReq {
            parent: None,
            changes: vec![ChangeInput {
                path: "f".to_owned(),
                op: ChangeOp::Put,
                content: Some(ContentInput::Inline(content.to_owned())),
                mode: None,
            }],
            message: "m".to_owned(),
            advance: None,
            idempotency_key: None,
            allow_empty: false,
        };
        let c1 = ops.commit(&who, commit("1")).unwrap().changeset;
        let main = RefSetReq {
            name: "main".to_owned(),
            new: c1.as_str().into(),
            expected_old: None,
            create: true,
        };
        ops.ref_set(&who, main).unwrap();
        drop(ops);
        // main's frame stays whole in journal.log, its entry gone from journal.off, as when its
        // writer died before listing it: the next write lists it.
        let off = Layout::new(&root).journal_off();
        set_len(&off, file_len(&off) - 8);

        let used = Used::default();
        let ops = LocalOps::open(&root).unwrap();
        ops.recording(&used.resolved)
            .commit(&who, commit("2"))
            .unwrap();
        let envelope = failed_envelope("commit", panicked(&"boom"), &used, Instant::now());
        let listed = RecoveredOut {
            frames_listed: 1,
            ..RecoveredOut::default()
        };
        assert_eq!(envelope.meta.recovered, Some(listed));
    }
}
