//! The SwarmOps command line (operations spec §0–§1), the program of two binaries: `swarmops`,
//! and `so`, its short form. One subcommand per operation, and on stdout exactly one envelope
//! per run, which agents script against.
//!
//! - Success prints `{"ok":true,"op":…,"api":1,"result":…,"meta":…}` and exits 0; a failure
//!   prints `{"ok":false,…,"error":{code,message,retryable,exit,details,hint},…}` and exits
//!   with `error.exit` (1 error, 2 usage, 4 retryable, 5 identity, 6 not found).
//! - A command line clap refuses is `E_INVALID_ARG` (exit 2): clap's message in `message`, the
//!   usage in `hint`. `--help` and `--version` alone print clap's text and exit 0.
//! - A panic is caught and printed as `E_INTERNAL` (exit 1), its message in `message`, with
//!   `meta.repo` and `meta.resolved` as far as the command got; the panic's location goes to
//!   stderr.
//! - stdout carries the envelope, or with `--human` its text rendering, and nothing else;
//!   diagnostics go to stderr, and there are none by default.
//! - `mcp` is the one command that prints no envelope on stdout: it serves the operations over
//!   MCP on stdin and stdout until stdin ends, exiting 0 (the `mcp` module). Its stdout belongs
//!   to the protocol, so what it would print before it serves (the envelope of a command line it
//!   refuses, or of a panic) goes to stderr instead, with the same exit code. A panic while the
//!   line itself is read comes before anything knows the line runs `mcp`.
//!
//! The envelope's `op` is the operation's library name (`cat` is `read_file`), or `swarmops`
//! for a command line that names no operation.
//!
//! Each binary does nothing but call [`run`], so the two behave alike. clap names the program by
//! the name it was invoked by in its usage lines (on the help screens, and in a usage error's
//! `hint`, the line the CLI adds where clap gives none included) and in its message for a
//! missing subcommand. Everything else names it `swarmops` whichever name ran it: `op`, the
//! hints and messages SwarmOps writes (commands, which run by either name), `--version` and the
//! MCP server's name.

mod cli;
mod commands;
mod context;
mod mcp;
mod output;
#[cfg(test)]
mod reference;

use std::ffi::OsString;
use std::io::{self, IsTerminal as _, Write as _};
use std::panic::{self, AssertUnwindSafe};
use std::process::ExitCode;
use std::time::Instant;

use crate::cli::Parsed;
use crate::commands::{Ctx, Used};
use crate::output::{Format, Printed, Stdout};

/// Runs the command line the process was started with, and gives the code to exit with: the
/// whole program, which `swarmops` and `so` both run.
pub fn run() -> ExitCode {
    let started = Instant::now();
    let stdout = if io::stdout().is_terminal() {
        Stdout::Terminal
    } else {
        Stdout::Other
    };
    // This guard catches a panic before the operation is known, while the line is read; the
    // one in `respond` a panic while the operation runs, and names it.
    let printed = guarded(
        cli::NO_OP,
        Format::Json {
            layout: stdout.layout(),
        },
        started,
        None,
        || respond(started, stdout),
    );
    emit(&printed)
}

/// What the command line asks for, done: the envelope to print, or nothing more for help or
/// the version, which clap has printed. For a line that runs `mcp`, the envelope has gone to
/// stderr ([`on_stderr`]).
fn respond(started: Instant, stdout: Stdout) -> Printed {
    let args: Vec<OsString> = std::env::args_os().collect();
    let parsed = cli::parse(&args);
    if parsed.runs_mcp() {
        on_stderr(answer(parsed, started, stdout))
    } else {
        answer(parsed, started, stdout)
    }
}

/// What `parsed` asks for, done: [`respond`] before it says where the envelope goes.
fn answer(parsed: Parsed, started: Instant, stdout: Stdout) -> Printed {
    let env = |name: &str| std::env::var_os(name);
    match parsed {
        Parsed::Text(text) => {
            // Text for a person, not an envelope, so a write that fails is not reported.
            let _ = text.print();
            Printed {
                stdout: String::new(),
                exit: 0,
            }
        }
        Parsed::Usage {
            error,
            bin,
            path,
            global,
        } => {
            let format = context::format(&global, &env, stdout)
                .unwrap_or_else(|_| context::json_format(&global, stdout));
            commands::usage(&error, bin.as_deref(), &path, format, started)
        }
        Parsed::Run(cli, op) => match context::format(&cli.global, &env, stdout) {
            Ok(format) => {
                // Made outside the guard, so that a panic's envelope can read what it used.
                let ctx = Ctx {
                    global: &cli.global,
                    env: &env,
                    format,
                    started,
                    used: Used::default(),
                };
                guarded(op, format, started, Some(&ctx.used), || {
                    commands::run(&ctx, op, cli.command)
                })
            }
            Err(error) => {
                let format = context::json_format(&cli.global, stdout);
                commands::failed(op, error, format, started)
            }
        },
    }
}

/// `printed`, with what it holds for stdout written to stderr instead: what `swarmops mcp` prints
/// before it serves, since its stdout belongs to the protocol. The exit code stays the same.
fn on_stderr(printed: Printed) -> Printed {
    let Printed { stdout, exit } = printed;
    if !stdout.is_empty() {
        // The exit code says the command failed even if this write does not reach anyone.
        let _ = io::stderr().write_all(stdout.as_bytes());
    }
    Printed {
        stdout: String::new(),
        exit,
    }
}

/// Runs `run`, which prints operation `op`'s envelope, and turns a panic in it into an
/// `E_INTERNAL` envelope (exit 1) with the panic's message, and the `meta.repo` and
/// `meta.resolved` of what the command had `used` (none before there is a command).
fn guarded(
    op: &str,
    format: Format,
    started: Instant,
    used: Option<&Used>,
    run: impl FnOnce() -> Printed,
) -> Printed {
    panic::catch_unwind(AssertUnwindSafe(run)).unwrap_or_else(|payload| {
        let error = commands::panicked(&*payload);
        match used {
            Some(used) => commands::failed_after(op, error, used, format, started),
            None => commands::failed(op, error, format, started),
        }
    })
}

/// Writes `printed` to stdout and exits with its code; if stdout fails, says so on stderr
/// and exits non-zero, since the caller did not get the envelope. Nothing to print (help or
/// the version, which clap printed) leaves stdout alone, so that a reader that went away
/// early is not reported.
fn emit(printed: &Printed) -> ExitCode {
    if printed.stdout.is_empty() {
        return ExitCode::from(printed.exit);
    }
    let mut stdout = io::stdout().lock();
    match stdout
        .write_all(printed.stdout.as_bytes())
        .and_then(|()| stdout.flush())
    {
        Ok(()) => ExitCode::from(printed.exit),
        Err(e) => {
            let _ = writeln!(io::stderr(), "swarmops: cannot write the envelope: {e}");
            ExitCode::from(printed.exit.max(1))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use camino::Utf8Path;
    use serde_json::{Value, json};
    use swarmops_api::{
        ChangeInput, ChangeOp, CommitReq, ContentInput, Identity, OpError, ShowReq, StoreOps,
    };
    use swarmops_ops::LocalOps;

    use super::*;
    use crate::cli::Global;
    use crate::output::Layout;

    fn envelope(printed: &Printed) -> Value {
        serde_json::from_str(&printed.stdout).expect("one JSON document")
    }

    #[test]
    fn a_panic_is_e_internal_with_its_message() {
        let format = Format::Json {
            layout: Layout::Compact,
        };
        let printed = guarded("commit", format, Instant::now(), None, || panic!("boom"));
        assert_eq!(printed.exit, 1);
        let json = envelope(&printed);
        assert_eq!(
            (json["ok"].as_bool(), json["op"].as_str()),
            (Some(false), Some("commit"))
        );
        assert_eq!(json["error"]["code"], "E_INTERNAL");
        assert_eq!(json["error"]["exit"], 1);
        let message = json["error"]["message"].as_str().unwrap();
        assert!(message.ends_with(": boom"), "{message}");
        // Before a command has named a store, there is none to report.
        assert_eq!(json["meta"].get("repo"), None);
        assert_eq!(json["meta"]["resolved"], json!({}));

        // A formatted message is a String payload, and stays on one line.
        let printed = guarded("show", format, Instant::now(), None, || {
            panic!("{}", "two\nlines")
        });
        let message = envelope(&printed)["error"]["message"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(message.ends_with(r": two\nlines"), "{message}");
    }

    /// A panic after the command named its store and resolved a revision reports both, as the
    /// command's own envelope would.
    #[test]
    fn a_panic_keeps_the_store_named_and_what_it_resolved() {
        let dir = tempfile::tempdir().unwrap();
        let store = format!("{}/store", dir.path().to_str().unwrap());
        let (ops, _) = LocalOps::init(Utf8Path::new(&store), "acme").unwrap();
        let who = Identity::parse("agent/worker-1").unwrap();
        let change = ChangeInput {
            path: "a.txt".to_owned(),
            op: ChangeOp::Put,
            content: Some(ContentInput::Inline("a".to_owned())),
            mode: None,
        };
        let req = CommitReq {
            parent: None,
            changes: vec![change],
            message: "m".to_owned(),
            allow_empty: false,
            advance: None,
            idempotency_key: None,
        };
        let id = ops.commit(&who, req).unwrap().changeset;
        drop(ops);

        let global = Global {
            repo: Some(store.clone()),
            ..Global::default()
        };
        let env = |_: &str| -> Option<OsString> { None };
        let format = Format::Json {
            layout: Layout::Compact,
        };
        let ctx = Ctx {
            global: &global,
            env: &env,
            format,
            started: Instant::now(),
            used: Used::default(),
        };
        let printed = guarded("show", format, ctx.started, Some(&ctx.used), || {
            ctx.on_store(
                "show",
                || Ok(()),
                |ops, ()| -> Result<(), OpError> {
                    let rev = id.as_str().into();
                    ops.show(ShowReq {
                        rev,
                        max_changes: 1,
                    })?;
                    panic!("boom after a read")
                },
            )
        });
        let json = envelope(&printed);
        assert_eq!(json["error"]["code"], "E_INTERNAL");
        assert_eq!(json["meta"]["repo"], store.as_str());
        assert_eq!(json["meta"]["resolved"], json!({ id.as_str(): id }));
    }

    #[test]
    fn no_panic_passes_the_envelope_through() {
        let format = Format::Human;
        let printed = Printed {
            stdout: "name: acme\n".to_owned(),
            exit: 0,
        };
        let passed = guarded("repo_info", format, Instant::now(), None, || {
            printed.clone()
        });
        assert_eq!(passed, printed);
    }
}
