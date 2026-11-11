//! The MCP server answers as the CLI does (the M21 design §2.5): the same request through a tool
//! and through its command gives the same envelope, but for the time each took.
//!
//! [`PARITY`] lists the tools compared, each with its command line in [`case`];
//! [`PARITY_EXEMPT`] the tools that are not, each with its reason. Together they hold every tool
//! of `mcp.rs`' `TOOLS`: a task that adds a tool adds its line to one of them in the same commit.
//!
//! ```text
//! fixture store ─┬─▶ swarmops mcp ── tools/call <tool> {args} ─▶ envelope ─┐
//!                └─▶ swarmops <command line>  ─────────────────▶ envelope ─┴▶ equal, masked
//! ```

use std::path::Path;

use serde_json::{Value, json};
use swarmops_core::{ObjectKind, hash_object};

use crate::mcp::{Server, TOOLS};
use crate::support::Store;

/// The agent both sides act as.
const AGENT: &str = "agent/parity-1";

/// The `meta` fields two runs of one request differ in, masked before the comparison.
const MASKED: [&str; 1] = ["elapsed_ms"];

/// The workspace the fixture makes.
const WORKSPACE: &str = "w1";

/// The ref the fixture's change sits on, beside main.
const FEATURE: &str = "release/feature";

/// The README's first content.
const README: &str = "# acme\n";

/// The tools whose envelope [`every_tool_answers_as_the_cli_does`] compares with their
/// command's.
const PARITY: &[&str] = &[
    "repo_info",
    "resolve_rev",
    "read_file",
    "read_tree",
    "show",
    "log",
    "ancestry",
    "path_history",
    "diff",
    "merge_base",
    "op_log",
    "ref_list",
    "ref_log",
    "land_check",
    "blob_has",
    "verify",
    "gc",
    "repair",
    "remote_list",
    "sync_status",
    "ws_status",
    "ws_diff",
    "ws_list",
    "ws_info",
];

/// Why a mutation is not compared: the second request is another one.
const MUTATES: &str = "it changes the store, so the second of two runs is another request: new \
                       ids, a replay, or nothing left to do";

/// The tools [`every_tool_answers_as_the_cli_does`] does not compare, each with its reason.
const PARITY_EXEMPT: &[(&str, &str)] = &[
    ("ref_set", MUTATES),
    ("ref_delete", MUTATES),
    ("ref_txn", MUTATES),
    ("undo", MUTATES),
    ("commit", MUTATES),
    ("land", MUTATES),
    ("revert", MUTATES),
    ("land_group", MUTATES),
    ("rebase", MUTATES),
    ("blob_put", MUTATES),
    ("upgrade", MUTATES),
    ("fetch", MUTATES),
    ("push", MUTATES),
    ("sync", MUTATES),
    ("ws_create", MUTATES),
    ("ws_commit", MUTATES),
    ("ws_rebase", MUTATES),
    ("ws_resolve", MUTATES),
    ("ws_land", MUTATES),
    ("ws_materialize", MUTATES),
    ("ws_discard", MUTATES),
    ("ws_snapshot", MUTATES),
];

/// Each tool of [`PARITY`], called on the fixture through `swarmops mcp` and run as its command,
/// prints the same envelope, `meta`'s [`MASKED`] fields aside.
#[test]
fn every_tool_answers_as_the_cli_does() {
    let fixture = Fixture::new();
    let store = &fixture.store;
    let mut server = Server::start(&["--repo", store.path(), "--as", AGENT], &[]);
    server.initialize();
    for tool in PARITY {
        let (arguments, line) = case(tool, &fixture);
        let call = server.call(tool, arguments);
        let line: Vec<&str> = line.iter().map(String::as_str).collect();
        let printed = store.run_as(AGENT, &line, &[]);
        let envelope: Value = serde_json::from_str(printed.stdout.trim_end())
            .unwrap_or_else(|e| panic!("{tool}: one envelope on stdout: {e}: {printed}"));
        assert_eq!(
            masked(&call.envelope),
            masked(&envelope),
            "{tool}: the tool and `swarmops {}` answer alike",
            line.join(" ")
        );
    }
    server.finish_clean();
}

/// Every tool of `TOOLS` is in [`PARITY`] or in [`PARITY_EXEMPT`], in one of them only, and
/// both name tools only.
#[test]
fn the_parity_table_is_exhaustive() {
    let tools = TOOLS.map(|(name, _, _)| name);
    let problems = unlisted(&tools, PARITY, PARITY_EXEMPT);
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// [`the_parity_table_is_exhaustive`]'s count catches a planted tool in neither table.
#[test]
fn a_tool_missing_from_parity_is_caught() {
    let mut tools = TOOLS.map(|(name, _, _)| name).to_vec();
    tools.push("planted_tool");
    assert_eq!(
        unlisted(&tools, PARITY, PARITY_EXEMPT),
        ["planted_tool: in neither PARITY nor PARITY_EXEMPT"]
    );
}

/// What breaks the tables' rule for `tools`: a tool in neither table or in both, a line that
/// names no tool.
fn unlisted(tools: &[&str], parity: &[&str], exempt: &[(&str, &str)]) -> Vec<String> {
    let mut problems = Vec::new();

    // 1. Each tool in one table.
    for tool in tools {
        let compared = parity.contains(tool);
        let exempted = exempt.iter().any(|(name, _)| name == tool);
        if compared == exempted {
            let which = if compared { "both" } else { "neither" };
            problems.push(format!(
                "{tool}: in {which} PARITY {} PARITY_EXEMPT",
                if compared { "and" } else { "nor" }
            ));
        }
    }

    // 2. Each line a tool.
    let lines = parity.iter().chain(exempt.iter().map(|(name, _)| name));
    for name in lines {
        if !tools.contains(name) {
            problems.push(format!("{name}: no such tool"));
        }
    }
    problems
}

/// `envelope` with `meta`'s [`MASKED`] fields set to 0.
fn masked(envelope: &Value) -> Value {
    let mut envelope = envelope.clone();
    for key in MASKED {
        if envelope["meta"].get(key).is_some() {
            envelope["meta"][key] = json!(0);
        }
    }
    envelope
}

/// The request of `tool` the comparison sends, and its command line after `swarmops`.
fn case(tool: &str, fixture: &Fixture) -> (Value, Vec<String>) {
    let (c1, blob) = (fixture.c1.as_str(), fixture.blob.as_str());
    let (arguments, line): (Value, &[&str]) = match tool {
        "repo_info" => (json!({}), &["info"]),
        "resolve_rev" => (json!({"rev": "main~1"}), &["resolve", "main~1"]),
        "read_file" => (
            json!({"rev": "main", "path": "README.md"}),
            &["cat", "main:README.md"],
        ),
        "read_tree" => (json!({"rev": "main"}), &["ls", "main"]),
        "show" => (json!({"rev": "main"}), &["show", "main"]),
        "log" => (json!({}), &["log"]),
        "ancestry" => (json!({"from": c1, "to": "main"}), &["ancestry", c1, "main"]),
        "path_history" => (
            json!({"rev": "main", "path": "README.md"}),
            &["history", "main:README.md"],
        ),
        "diff" => (
            json!({"from": "main~1", "to": "main"}),
            &["diff", "main~1", "main"],
        ),
        "merge_base" => (
            json!({"rev": FEATURE, "onto": "main"}),
            &["merge-base", FEATURE, "main"],
        ),
        "op_log" => (json!({}), &["op", "log"]),
        "ref_list" => (json!({}), &["ref", "list"]),
        "ref_log" => (json!({"name": "main"}), &["ref", "log", "main"]),
        "land_check" => (
            json!({"changes": [{"head": FEATURE}]}),
            &["land-check", FEATURE],
        ),
        "blob_has" => (json!({"ids": [blob]}), &["blob", "has", blob]),
        "verify" => (json!({}), &["verify"]),
        "gc" => (json!({"dry_run": true}), &["gc", "--dry-run"]),
        "repair" => (json!({"dry_run": true}), &["repair", "--dry-run"]),
        "remote_list" => (json!({}), &["remote", "list"]),
        "sync_status" => (json!({"remote": "origin"}), &["sync", "status", "origin"]),
        "ws_status" => (
            json!({"workspace": WORKSPACE}),
            &["ws", "status", "--ws", WORKSPACE],
        ),
        "ws_diff" => (
            json!({"workspace": WORKSPACE}),
            &["ws", "diff", "--ws", WORKSPACE],
        ),
        "ws_list" => (json!({}), &["ws", "list"]),
        "ws_info" => (
            json!({"workspace": WORKSPACE}),
            &["ws", "info", "--ws", WORKSPACE],
        ),
        _ => panic!("{tool}: no case; add one, or the tool to PARITY_EXEMPT"),
    };
    (
        arguments,
        line.iter().map(|word| (*word).to_owned()).collect(),
    )
}

/// The store both sides read: main at its second changeset, a change on [`FEATURE`] from the
/// first, and a workspace with an edit.
struct Fixture {
    store: Store,
    /// main's first changeset.
    c1: String,
    /// The README's first content's blob id.
    blob: String,
}

impl Fixture {
    /// Builds it through the CLI.
    fn new() -> Fixture {
        let store = Store::init();

        // 1. main: two changesets.
        let c1 = store.commit(AGENT, "README.md", README, None);
        store
            .run_as(AGENT, &["ref", "set", "main", &c1, "--create"], &[])
            .ok();
        let c2 = store.commit(AGENT, "README.md", "# acme\n\nv2\n", Some(&c1));
        store
            .run_as(AGENT, &["ref", "set", "main", &c2, "--expect", &c1], &[])
            .ok();

        // 2. A change from c1, on a ref of its own.
        let change = store.commit(AGENT, "docs.md", "docs\n", Some(&c1));
        store
            .run_as(AGENT, &["ref", "set", FEATURE, &change, "--create"], &[])
            .ok();

        // 3. A workspace of the whole tree, with an edit.
        let dir = store.tmp().join(WORKSPACE);
        let dir = dir.to_str().expect("UTF-8 temp paths");
        let create = ["ws", "create", WORKSPACE, "--path", dir, "--all"];
        store.run_as(AGENT, &create, &[]).ok();
        std::fs::write(Path::new(dir).join("README.md"), "# edited\n").expect("an edit");

        let blob = hash_object(ObjectKind::Blob, README.as_bytes()).to_hex();
        Fixture { store, c1, blob }
    }
}
