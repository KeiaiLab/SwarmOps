//! The SwarmOps command line (operations spec §0–§1), the program of two binaries: `swarmops`,
//! and `so`, its short form. One subcommand per operation, and on stdout exactly one envelope
//! per run, which agents script against, but for `mcp`, `serve` and `derive --follow` (below).
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
//! - `mcp` prints no envelope on stdout: it serves the operations over MCP on stdin and stdout
//!   until stdin ends, exiting 0 (the `mcp` module). Its stdout belongs
//!   to the protocol, so what it would print before it serves (the envelope of a command line it
//!   refuses, or of a panic) goes to stderr instead, with the same exit code. A panic while the
//!   line itself is read comes before anything knows the line runs `mcp`.
//! - `serve` prints no envelope on stdout either: it carries git's protocol there (with `--cgi`,
//!   the HTTP response), and a refusal before git starts is one `ERR` pkt-line (with `--cgi`, a
//!   `Status:` header and the same text), its envelope on stderr.
//! - `derive --follow` prints one envelope a line for each run that derived something, until it
//!   is killed or a run fails, whose envelope is the last.
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
mod entry;
mod mcp;
mod output;
#[cfg(test)]
mod reference;

pub use crate::entry::run;
