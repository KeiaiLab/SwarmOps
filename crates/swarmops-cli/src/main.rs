//! `swarmops`, the SwarmOps command line (operations spec §0–§1): one subcommand per
//! operation, and on stdout exactly one envelope per run, which agents script against.
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
//!
//! The envelope's `op` is the operation's library name (`cat` is `read_file`), or `swarmops`
//! for a command line that names no operation.

mod cli;
mod commands;
mod context;
mod output;

use std::ffi::OsString;
use std::io::{self, IsTerminal as _, Write as _};
use std::panic::{self, AssertUnwindSafe};
use std::process::ExitCode;
use std::time::Instant;

use swarmops_api::{ErrorCode, OpError};

use crate::cli::Parsed;
use crate::commands::{Ctx, Used};
use crate::output::{Format, Printed};

fn main() -> ExitCode {
    let started = Instant::now();
    let terminal = io::stdout().is_terminal();
    // This guard catches a panic before the operation is known, while the line is read; the
    // one in `respond` a panic while the operation runs, and names it.
    let printed = guarded(
        cli::NO_OP,
        Format::Json { pretty: terminal },
        started,
        None,
        || respond(started, terminal),
    );
    emit(&printed)
}

/// What the command line asks for, done: the envelope to print, or nothing more for help or
/// the version, which clap has printed.
fn respond(started: Instant, terminal: bool) -> Printed {
    let args: Vec<OsString> = std::env::args_os().collect();
    let env = |name: &str| std::env::var_os(name);
    match cli::parse(&args) {
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
            path,
            global,
        } => {
            let format = context::format(&global, &env, terminal)
                .unwrap_or_else(|_| context::json_format(&global, terminal));
            commands::usage(&error, &path, format, started)
        }
        Parsed::Run(cli, op) => match context::format(&cli.global, &env, terminal) {
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
                let format = context::json_format(&cli.global, terminal);
                commands::failed(op, error, format, started)
            }
        },
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
        let message = payload
            .downcast_ref::<&str>()
            .map(|message| (*message).to_owned())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "a panic without a message".to_owned());
        let error = OpError::new(
            ErrorCode::Internal,
            format!(
                "swarmops panicked, a bug (stderr has where): {}",
                output::printable(&message)
            ),
        );
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
        ChangeInput, ChangeOp, CommitReq, ContentInput, Identity, ShowReq, StoreOps,
    };
    use swarmops_ops::LocalOps;

    use super::*;
    use crate::cli::Global;

    fn envelope(printed: &Printed) -> Value {
        serde_json::from_str(&printed.stdout).expect("one JSON document")
    }

    #[test]
    fn a_panic_is_e_internal_with_its_message() {
        let format = Format::Json { pretty: false };
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
        };
        let id = ops.commit(&who, req).unwrap().changeset;
        drop(ops);

        let global = Global {
            repo: Some(store.clone()),
            ..Global::default()
        };
        let env = |_: &str| -> Option<OsString> { None };
        let format = Format::Json { pretty: false };
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
