//! `so`, the short form of `swarmops`: the same program by a shorter name. Each test runs both
//! binaries (`CARGO_BIN_EXE_so` and `CARGO_BIN_EXE_swarmops`) on the same command lines, with
//! none of the developer's `SWARMOPS_*` variables, and compares what they print.
//!
//! clap names the program by the name it was invoked by in its usage lines (on the help
//! screens, and in a usage error's hint, the line the CLI adds where clap gives none included)
//! and in its message for a missing subcommand. Everything else names it `swarmops` whichever
//! name ran it: the envelope's `op`, the hints and messages SwarmOps writes (commands, which run
//! by either name), `--version` and the MCP server's name.

use serde_json::{Value, json};

use crate::support::{BIN, SO, Store, run_bin, version_line};

/// Command lines clap refuses, each on a path of its own, and the usage line each one's hint
/// gives after the program's name: a missing subcommand, at the top and under `ref` (clap's
/// message names the program too); an unknown subcommand and unknown flags, with clap's usage
/// line; and a value its parser refuses, which clap gives no usage line, so the CLI adds one.
const REFUSED: [(&[&str], &str); 6] = [
    (&[], "[OPTIONS] <COMMAND>"),
    (&["ref"], "ref [OPTIONS] <COMMAND>"),
    (&["frobnicate"], "[OPTIONS] <COMMAND>"),
    (&["info", "--frob"], "info [OPTIONS]"),
    (&["--reop", "x", "info"], "--repo <STORE> <COMMAND>"),
    (&["log", "-n", "many"], "log [OPTIONS] [REV]"),
];

/// `so` is `swarmops` by another name: the same version; the same envelopes for `init`, `info`
/// and the CLI's own usage errors; the same for the command lines clap refuses, `mcp`'s on
/// stderr included, but for clap's text naming the program as it was invoked; and the same
/// answer to MCP's `initialize`.
#[test]
fn so_is_the_same_program_as_swarmops() {
    let version = version_line();
    for bin in [SO, BIN] {
        let run = run_bin(bin, &["--version"], &[], "");
        assert_eq!(
            (run.exit(), run.stdout.as_str()),
            (0, version.as_str()),
            "{bin}"
        );
    }

    // A new store each: alike but for where it is and its id.
    let stores = [Store::empty(), Store::empty()];
    let [by_so, by_swarmops] = [(SO, &stores[0]), (BIN, &stores[1])].map(|(bin, store)| {
        let mut init = envelope(bin, &["init", store.path()]);
        assert_eq!(init["meta"]["repo"], store.path(), "{bin}: {init}");
        assert!(init["result"]["repo_id"].is_string(), "{bin}: {init}");
        init["meta"]["repo"].take();
        init["result"]["repo_id"].take();
        init
    });
    assert_eq!(by_so, by_swarmops);

    // On one store, the same envelopes, the hints SwarmOps writes included: E_EXISTS's
    // `swarmops --repo <store> info`, and `swarmops ls main` from cat's own check of its target.
    let store = stores[0].path();
    for args in [
        &["init", store][..],
        &["--repo", store, "info"],
        &["cat", "main"],
    ] {
        assert_eq!(envelope(SO, args), envelope(BIN, args), "{args:?}");
    }
    for (args, _) in REFUSED {
        assert_eq!(
            as_swarmops(envelope(SO, args)),
            envelope(BIN, args),
            "{args:?}"
        );
    }
    let mcp = ["mcp", "--frob"];
    assert_eq!(as_swarmops(on_stderr(SO, &mcp)), on_stderr(BIN, &mcp));

    let [by_so, by_swarmops] = [SO, BIN].map(|bin| initialize(bin, store));
    assert_eq!(by_so["result"]["serverInfo"]["name"], "swarmops", "{by_so}");
    assert_eq!(by_so, by_swarmops);
}

/// clap's usage lines name the program as it was invoked, `so` or `swarmops`: in the hint of
/// every command line clap refuses, the line the CLI adds where clap gives none (`log -n many`)
/// included, in `mcp`'s on stderr, and on the help screens; so does clap's message for a
/// missing subcommand. A hint SwarmOps writes itself is a command, and says `swarmops` by
/// either name.
#[test]
fn usage_names_the_invoked_binary() {
    for (bin, name) in [(SO, "so"), (BIN, "swarmops")] {
        for (args, usage) in REFUSED {
            let refused = envelope(bin, args);
            let line = format!("Usage: {name} {usage}");
            assert_eq!(usage_line(&refused), line, "{bin} {args:?}");
        }
        let refused = on_stderr(bin, &["mcp", "--frob"]);
        let line = format!("Usage: {name} mcp [OPTIONS]");
        assert_eq!(usage_line(&refused), line, "{bin}");
        for (args, command) in [
            (&[][..], name.to_owned()),
            (&["ref"], format!("{name} ref")),
        ] {
            let refused = envelope(bin, args);
            let message = refused["error"]["message"].as_str().unwrap();
            let missing = format!("'{command}' requires a subcommand");
            assert!(message.starts_with(&missing), "{bin}: {message}");
        }
        let own = envelope(bin, &["cat", "main"]);
        assert_eq!(own["error"]["hint"], "swarmops ls main", "{bin}");

        for (args, usage) in [
            (&["--help"][..], "[OPTIONS] <COMMAND>"),
            (&["log", "--help"], "log [OPTIONS] [REV]"),
            (
                &["help", "ref", "set"],
                "ref set [OPTIONS] <--expect <REV>|--create> <NAME> <REV>",
            ),
        ] {
            let help = run_bin(bin, args, &[], "");
            assert_eq!(help.exit(), 0, "{bin}: {help}");
            let line = format!("Usage: {name} {usage}");
            assert!(
                help.stdout.lines().any(|l| l == line),
                "{bin}: {line}: {help}"
            );
        }
    }
}

/// The envelope `bin` prints for `args`, checked against the contract, with nothing on stderr;
/// its `elapsed_ms` is 0 ([`timeless`]).
fn envelope(bin: &str, args: &[&str]) -> Value {
    let run = run_bin(bin, args, &[], "");
    assert_eq!(run.stderr, "", "{bin}: nothing on stderr: {run}");
    timeless(run.envelope())
}

/// The envelope `bin` prints for `args`, a command line `mcp` refuses: on stderr, since stdout
/// belongs to the protocol, with exit 2. Its `elapsed_ms` is 0 ([`timeless`]).
fn on_stderr(bin: &str, args: &[&str]) -> Value {
    let run = run_bin(bin, args, &[], "");
    assert_eq!((run.exit(), run.stdout.as_str()), (2, ""), "{bin}: {run}");
    let envelope = serde_json::from_str(&run.stderr)
        .unwrap_or_else(|e| panic!("{bin}: one envelope on stderr ({e}): {run}"));
    timeless(envelope)
}

/// `envelope` with its `elapsed_ms` 0: the one field two runs of a command differ in.
fn timeless(mut envelope: Value) -> Value {
    envelope["meta"]["elapsed_ms"] = 0.into();
    envelope
}

/// The answer of `bin mcp --repo <store>` to MCP's `initialize`, the one message it is given:
/// one line on stdout, nothing on stderr, and exit 0 when its stdin ends.
fn initialize(bin: &str, store: &str) -> Value {
    let client = json!({"name": "swarmops-tests", "version": "0"});
    let params = json!({"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": client});
    let request = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": params});
    let run = run_bin(bin, &["mcp", "--repo", store], &[], &format!("{request}\n"));
    assert_eq!((run.exit(), run.stderr.as_str()), (0, ""), "{bin}: {run}");
    assert_eq!(run.stdout.lines().count(), 1, "{bin}: one answer: {run}");
    serde_json::from_str(&run.stdout)
        .unwrap_or_else(|e| panic!("{bin}: a JSON-RPC answer ({e}): {run}"))
}

/// The one usage line in the hint of `refused`, an envelope.
fn usage_line(refused: &Value) -> &str {
    let hint = refused["error"]["hint"].as_str().expect("a hint");
    let mut usage = hint.lines().filter(|line| line.starts_with("Usage: "));
    let line = usage
        .next()
        .unwrap_or_else(|| panic!("a usage line: {hint}"));
    assert_eq!(usage.next(), None, "one usage line: {hint}");
    line
}

/// `refused`, the envelope `so` printed for a command line clap refused, as `swarmops` prints
/// it: with `swarmops` for `so` where clap names the program, in the hint's usage line and in
/// the message of a missing subcommand (`'so ref' requires a subcommand …`).
fn as_swarmops(mut refused: Value) -> Value {
    let error = &mut refused["error"];
    let message = error["message"].as_str().expect("a message");
    let message = message
        .replace("'so'", "'swarmops'")
        .replace("'so ", "'swarmops ");
    let hint: Vec<String> = error["hint"]
        .as_str()
        .expect("a hint")
        .lines()
        .map(|line| match line.strip_prefix("Usage: so ") {
            Some(usage) => format!("Usage: swarmops {usage}"),
            None => line.to_owned(),
        })
        .collect();
    error["message"] = message.into();
    error["hint"] = hint.join("\n").into();
    refused
}
