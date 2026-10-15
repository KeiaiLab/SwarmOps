//! Running a command (operations spec §1): its request built from the command line, the
//! operation called through `dyn StoreOps` (spec §7), and its envelope rendered.
//!
//! A command first checks what it can without the store: the store the user named (only
//! whether the variable is readable), a mutation's identity, its own arguments and a commit's
//! change list. Then it opens the store and runs the operation. So a malformed invocation
//! fails the same way whatever the store holds, and costs no store access.
//!
//! The envelope's `meta`: `repo` is the store as the user named it (for `init`, its
//! directory), omitted when the command uses no store (`whoami`, `schema`) or none was named;
//! `elapsed_ms` spans the whole command but its printing: reading the command line, opening
//! the store and the operation; `resolved` is every revision the operation resolved, as
//! `LocalOps::take_resolved` reports them, also when it failed; `auth` is the store's
//! authentication mode, `trust` in M1.
//!
//! A panic's envelope has the same `repo` and `resolved` (see [`Used`]), or neither when it
//! came before a store was named.
//!
//! `whoami` prints `IdentityInfo` as the API writes it: for a caller without an identity,
//! `{"auth":"anonymous"}`, with `agent` omitted rather than null.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read as _};
use std::path::Path;
use std::time::Instant;

use camino::Utf8Path;
use serde::Serialize;
use serde_json::{Map, Value};
use swarmops_api::{
    ChangeInput, CommitReq, Envelope, ErrorCode, InfoReq, LogReq, Meta, OpError, ReadFileReq,
    ReadTreeReq, RefDeleteReq, RefListReq, RefSetReq, RepoInfo, Rev, ShowReq, StoreOps,
    VerifyReport, VerifyReq, schema_for_all,
};
use swarmops_ops::LocalOps;
use swarmops_ops::identity::{AUTH_TRUST, identity_info};

use crate::cli::{self, Command, Global, RefCommand};
use crate::context::{self, Env};
use crate::output::{self, Format, Printed};

/// The hint of a failed deep `verify`, which leaves nothing to re-check.
const RESTORE_HINT: &str =
    "details.problems names each damaged object or record: restore them from a backup of the store";

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

/// What a command has used so far: the store the user named, and the store once opened. It
/// lives outside the code that can panic, so that a panic's envelope can report what the
/// command's own would: `meta.repo` and `meta.resolved`.
#[derive(Default)]
pub struct Used {
    /// The store as the user named it.
    repo: RefCell<Option<String>>,
    /// The store, once opened.
    store: RefCell<Option<LocalOps>>,
}

impl Used {
    /// `meta.repo` and `meta.resolved` as far as the command got: the store named, and what the
    /// opened store resolved since this was last asked.
    pub fn meta(&self) -> (Option<String>, BTreeMap<String, String>) {
        let resolved = self
            .store
            .borrow()
            .as_ref()
            .map(LocalOps::take_resolved)
            .unwrap_or_default();
        (self.repo.borrow().clone(), resolved)
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
            let (repo, resolved) = ctx.used.meta();
            ctx.finish(op, repo, resolved, result)
        }
        Command::Info => ctx.on_store(op, || Ok(InfoReq {}), |ops, req| ops.repo_info(req)),
        Command::Whoami => {
            let result = context::identity(agent, ctx.env).map(|who| identity_info(who.as_ref()));
            ctx.finish(op, None, BTreeMap::new(), result)
        }
        Command::Schema { name } => ctx.finish(op, None, BTreeMap::new(), schema(name.as_deref())),
        Command::Commit {
            message,
            changes,
            parent,
            allow_empty,
        } => ctx.on_store(
            op,
            || {
                let who = context::mutator(agent, ctx.env)?;
                let req = CommitReq {
                    parent: parent.map(Rev::from),
                    changes: read_changes(&changes)?,
                    message,
                    allow_empty,
                };
                Ok((who, req))
            },
            |ops, (who, req)| ops.commit(&who, req),
        ),
        Command::Cat { target, max_bytes } => ctx.on_store(
            op,
            || {
                let (rev, path) = file_target(&target)?;
                Ok(ReadFileReq {
                    rev: rev.into(),
                    path: path.to_owned(),
                    max_bytes,
                })
            },
            |ops, req| ops.read_file(req),
        ),
        Command::Ls {
            target,
            limit,
            cursor,
        } => ctx.on_store(
            op,
            || {
                let (rev, path) = split_target(&target);
                Ok(ReadTreeReq {
                    rev: rev.into(),
                    path: path.map(str::to_owned),
                    limit,
                    cursor,
                })
            },
            |ops, req| ops.read_tree(req),
        ),
        Command::Show { rev, max_changes } => ctx.on_store(
            op,
            || {
                Ok(ShowReq {
                    rev: rev.into(),
                    max_changes,
                })
            },
            |ops, req| ops.show(req),
        ),
        Command::Log { rev, limit, cursor } => ctx.on_store(
            op,
            || {
                Ok(LogReq {
                    rev: rev.map(Rev::from),
                    limit,
                    cursor,
                })
            },
            |ops, req| ops.log(req),
        ),
        Command::Ref(command) => run_ref(ctx, op, command),
        Command::Verify { deep } => ctx.on_store(
            op,
            || Ok(VerifyReq { deep }),
            |ops, req| ops.verify(req).and_then(|report| clean(report, deep)),
        ),
    }
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

impl Ctx<'_> {
    /// Runs operation `op` on the store the user named: `prepare` builds its input without
    /// the store, then `call` runs it on the store, opened. Both are kept in [`Ctx::used`].
    pub fn on_store<P, T: Serialize>(
        &self,
        op: &str,
        prepare: impl FnOnce() -> Result<P, OpError>,
        call: impl FnOnce(&dyn StoreOps, P) -> Result<T, OpError>,
    ) -> Printed {
        let named = context::store_named(self.global.repo.as_deref(), self.env);
        if let Ok(Some(store)) = &named {
            self.used.repo.replace(Some(store.clone()));
        }
        let result = named.and_then(|named| {
            let input = prepare()?;
            let store =
                named.ok_or_else(|| context::no_store_named(self.global.repo.as_deref()))?;
            self.used
                .store
                .replace(Some(LocalOps::open(Utf8Path::new(&store))?));
            let opened = self.used.store.borrow();
            call(
                opened.as_ref().expect("the store was opened just now"),
                input,
            )
        });
        let (repo, resolved) = self.used.meta();
        self.finish(op, repo, resolved, result)
    }

    /// `op`'s envelope for `result`, with `meta` from `repo`, `resolved` and the clock.
    fn finish<T: Serialize>(
        &self,
        op: &str,
        repo: Option<String>,
        resolved: BTreeMap<String, String>,
        result: Result<T, OpError>,
    ) -> Printed {
        envelope(op, repo, resolved, result, self.format, self.started)
    }
}

/// The envelope of `op` failing with `error` before it could use a store.
pub fn failed(op: &str, error: OpError, format: Format, started: Instant) -> Printed {
    envelope::<()>(op, None, BTreeMap::new(), Err(error), format, started)
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
    let (repo, resolved) = used.meta();
    envelope::<()>(op, repo, resolved, Err(error), format, started)
}

/// The envelope of a command line clap refused: `E_INVALID_ARG` (exit 2) with clap's message
/// in one line as `message`, and its tips and the usage of the subcommand `path` names as
/// `hint`, a line each. Control characters (from the line itself) are escaped in both.
pub fn usage(error: &clap::Error, path: &[String], format: Format, started: Instant) -> Printed {
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
        hint.insert(at, cli::usage(path));
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

fn envelope<T: Serialize>(
    op: &str,
    repo: Option<String>,
    resolved: BTreeMap<String, String>,
    result: Result<T, OpError>,
    format: Format,
    started: Instant,
) -> Printed {
    let meta = Meta {
        repo,
        elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        resolved,
        auth: AUTH_TRUST.to_owned(),
    };
    let envelope = match result {
        Ok(result) => Envelope::ok(op, result, meta),
        Err(error) => Envelope::err(op, error, meta),
    };
    output::render(&envelope, format)
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

/// A commit's change list: the JSON array of changes in file `source`, or on stdin for `-`.
fn read_changes(source: &str) -> Result<Vec<ChangeInput>, OpError> {
    let shown = if source == "-" {
        "stdin".to_owned()
    } else {
        format!("{source:?}")
    };
    let refuse = |what: String| {
        OpError::new(ErrorCode::InvalidArg, what)
            .with_details(details([("changes", source.into())]))
    };
    let bytes = if source == "-" {
        let mut bytes = Vec::new();
        io::stdin().read_to_end(&mut bytes).map(|_| bytes)
    } else {
        fs::read(source)
    }
    .map_err(|e| refuse(format!("cannot read the changes from {shown}: {e}")))?;
    serde_json::from_slice(&bytes).map_err(|e| {
        refuse(format!("the changes in {shown} are not a JSON array of changes: {e}")).with_hint(
            r#"the changes are an array such as [{"path": "a.txt", "op": "put", "content": {"inline": "hi"}}]; swarmops schema ChangeInput gives the rest"#,
        )
    })
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

/// `verify`'s report as the command's outcome: a success for a clean store, else
/// `E_STORE_CORRUPT` (exit 1) with the whole report in `details`. The hint is a deep
/// verify after a structural one, which finds damaged bytes too, and after a deep one the
/// restore that remains.
fn clean(report: VerifyReport, deep: bool) -> Result<VerifyReport, OpError> {
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

/// `arg` as one shell word in a hint: as it is when it is made of `[A-Za-z0-9_./:@-]` only,
/// else in single quotes, each `'` in it written `'\''`; control characters escaped, so that
/// the hint stays one line (the rule of the ops crate's hints).
fn quoted(arg: &str) -> String {
    let plain = !arg.is_empty()
        && arg
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_./:@-".contains(&b));
    if plain {
        arg.to_owned()
    } else {
        output::printable(&format!("'{}'", arg.replace('\'', r"'\''")))
    }
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

    #[test]
    fn verify_problems_are_store_corruption() {
        let report = |problems: &[&str]| VerifyReport {
            objects_checked: 5,
            changesets_checked: 2,
            problems: problems.iter().map(|p| (*p).to_owned()).collect(),
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

    #[test]
    fn shell_words_are_quoted_when_they_must_be() {
        assert_eq!(quoted("/tmp/a-b_c.d@e:f"), "/tmp/a-b_c.d@e:f");
        assert_eq!(quoted("main~1"), "'main~1'");
        assert_eq!(quoted("a b"), "'a b'");
        assert_eq!(quoted("it's"), r"'it'\''s'");
        assert_eq!(quoted(""), "''");
        assert_eq!(quoted("a\nb"), r"'a\nb'");
    }
}
