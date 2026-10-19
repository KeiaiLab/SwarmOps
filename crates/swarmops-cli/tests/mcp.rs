//! `swarmops mcp`, the MCP server over stdio, as an agent's harness drives it: the protocol
//! handshake of each revision it serves, one tool per `StoreOps` operation whose schema is
//! the request type's, tool results that carry the envelope the CLI prints, and JSON-RPC errors
//! for what is not a valid request.
//!
//! Each test starts the real binary (`CARGO_BIN_EXE_swarmops`) with `mcp` and talks to it
//! through its stdin and stdout, a line per message, with none of the developer's `SWARMOPS_*`
//! variables. Every line the server writes is checked to be one JSON-RPC response (the server
//! writes nothing else on stdout), every tool result to carry one envelope that keeps the
//! envelope contract, and every wait for a reply has a deadline. A server is killed and reaped
//! however its test ends.
//!
//! The protocol revisions are MCP's latest, 2026-07-28 (stateless: each request names its
//! version in `params._meta`), and the two before it, 2025-11-25 and 2025-06-18, which have a
//! handshake (a session that `initialize` opens).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Barrier, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use camino::Utf8Path;
use serde_json::{Value, json};
use swarmops_api::{ErrorCode, ShowReq, StoreOps, schema_for_all};
use swarmops_core::{ObjectKind, hash_object};
use swarmops_ops::{LocalOps, Resolved};

use crate::support::{self, Store};

/// The agent the servers act as.
const AGENT: &str = "agent/worker-17";

/// MCP's latest revision, 2026-07-28: no handshake, each request names its version.
const LATEST: &str = "2026-07-28";

/// The revision before it, 2025-11-25: a session that `initialize` opens.
const PREVIOUS: &str = "2025-11-25";

/// The earliest revision `initialize` still serves: 2025-06-18, the first with
/// `structuredContent` and `outputSchema`.
const EARLIEST: &str = "2025-06-18";

/// Every revision the server serves, newest first, as `server/discover` and a `-32022` error
/// list them: the latest in `params._meta`, the other two through `initialize`.
const SUPPORTED: [&str; 3] = [LATEST, PREVIOUS, EARLIEST];

/// A command line that runs `swarmops mcp`: its arguments, and the variables it sets.
type Startup<'a> = (&'a [&'a str], &'a [(&'a str, &'a str)]);

/// The tools, one per `StoreOps` operation, in the order `tools/list` gives them, each with
/// the request type its arguments are.
const TOOLS: [(&str, &str); 10] = [
    ("repo_info", "InfoReq"),
    ("read_file", "ReadFileReq"),
    ("read_tree", "ReadTreeReq"),
    ("show", "ShowReq"),
    ("log", "LogReq"),
    ("ref_list", "RefListReq"),
    ("ref_set", "RefSetReq"),
    ("ref_delete", "RefDeleteReq"),
    ("commit", "CommitReq"),
    ("verify", "VerifyReq"),
];

/// How long a test waits for one reply, or for the server to exit once its stdin is closed.
const REPLY_TIMEOUT: Duration = Duration::from_secs(60);

/// JSON-RPC's error codes, and MCP's for a protocol version it does not serve.
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const UNSUPPORTED_PROTOCOL_VERSION: i64 = -32022;

// ---------------------------------------------------------------------------------------------
// The tests.
// ---------------------------------------------------------------------------------------------

/// Every revision lists the same ten tools: `initialize` negotiates 2025-11-25 or 2025-06-18
/// (2025-11-25 for a client that asks for a version it does not serve), `server/discover`
/// advertises all three, and each tool's schema is its request type's, standing alone.
#[test]
fn mcp_initialize_lists_every_operation_as_a_tool() {
    let store = Store::init();
    let mut server = Server::start(&["--repo", store.path(), "--as", AGENT], &[]);

    // 2025-11-25: initialize, then the initialized notification, which gets no answer.
    let init = server.initialize();
    assert_eq!(init["protocolVersion"], PREVIOUS, "{init}");
    assert_eq!(init["capabilities"], json!({"tools": {}}), "{init}");
    assert_eq!(init["serverInfo"]["name"], "swarmops", "{init}");
    assert_eq!(
        init["serverInfo"]["version"],
        env!("CARGO_PKG_VERSION"),
        "{init}"
    );
    assert!(init["instructions"].is_string(), "{init}");
    assert!(
        init.get("resultType").is_none(),
        "2025-11-25 has none: {init}"
    );
    // A ping answers with an empty result, and is the first reply since the notification.
    assert_eq!(server.result("ping", json!({})), json!({}));
    let previous = server.result("tools/list", json!({}));
    let tools = previous["tools"].as_array().expect("a tool array").clone();
    assert_eq!(previous, json!({"tools": tools}), "one page, no cursor");
    let names: Vec<&str> = tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, TOOLS.map(|(name, _)| name));
    let all = schema_for_all();
    for ((name, request), tool) in TOOLS.iter().zip(&tools) {
        check_tool(name, request, tool, &all);
    }

    // 2026-07-28: discovery names every revision served, the ones with a handshake too, and the
    // same tools list, with the results' own fields.
    let discover = server.result("server/discover", json!({"_meta": latest_meta()}));
    assert_eq!(discover["resultType"], "complete", "{discover}");
    assert_eq!(
        discover["supportedVersions"],
        json!(SUPPORTED),
        "{discover}"
    );
    assert_eq!(discover["capabilities"], json!({"tools": {}}), "{discover}");
    assert_eq!(discover["instructions"], init["instructions"], "{discover}");
    assert_cacheable(&discover);
    assert_eq!(
        discover["_meta"]["io.modelcontextprotocol/serverInfo"],
        init["serverInfo"]
    );
    let latest = server.result("tools/list", json!({"_meta": latest_meta()}));
    assert_eq!(latest["tools"], Value::Array(tools), "the same tools");
    assert_eq!(latest["resultType"], "complete", "{latest}");
    assert_cacheable(&latest);
    assert_eq!(
        latest["_meta"]["io.modelcontextprotocol/serverInfo"],
        init["serverInfo"]
    );
    server.finish_clean();

    // The handshake serves 2025-06-18 too, as asked, with the same tools and results. A
    // client that asks for a version it does not serve gets 2025-11-25, to accept or refuse:
    // 2025-03-26 and older, and 2026-07-28, which has no handshake.
    for (asked, answered) in [
        (EARLIEST, EARLIEST),
        ("2025-03-26", PREVIOUS),
        ("2024-11-05", PREVIOUS),
        (LATEST, PREVIOUS),
    ] {
        let mut server = Server::start(&["--repo", store.path()], &[]);
        let init = server.result(
            "initialize",
            json!({"protocolVersion": asked, "capabilities": {}, "clientInfo": client_info()}),
        );
        assert_eq!(init["protocolVersion"], answered, "asked {asked}: {init}");
        let listed = server.result("tools/list", json!({}));
        assert_eq!(listed, previous, "asked {asked}");
        let info = server.ok("repo_info", json!({}));
        assert_eq!(info["result"]["changesets"], 0, "asked {asked}");
        server.finish_clean();
    }
}

/// The contract-change review gate for agents: the whole `tools/list` result of the latest
/// revision, every tool's name, title, description, input schema and annotations. The server's
/// version, which every release bumps and no contract rests on, is checked here and pinned as
/// `[version]`.
#[test]
fn mcp_tools_list_schema_snapshot() {
    let store = Store::init();
    let mut server = Server::start(&["--repo", store.path(), "--as", AGENT], &[]);
    let mut listed = server.result("tools/list", json!({"_meta": latest_meta()}));
    server.finish_clean();
    pin_server_version(&mut listed);
    insta::assert_json_snapshot!(listed);
}

/// The other half of the gate: the whole `server/discover` result of the latest revision. It
/// holds the server's `instructions`, which a client loads before any tool (Claude Code, with
/// tool search on, loads only the tool names and these), and the revisions served. The server's
/// version is pinned as in the `tools/list` snapshot.
#[test]
fn mcp_discover_snapshot() {
    let store = Store::init();
    let mut server = Server::start(&["--repo", store.path(), "--as", AGENT], &[]);
    let mut discovered = server.result("server/discover", json!({"_meta": latest_meta()}));
    server.finish_clean();
    pin_server_version(&mut discovered);
    insta::assert_json_snapshot!(discovered);
}

/// A commit through the server, of every kind of content, then every read: each tool result is
/// a success envelope, the same as the CLI prints for the same request, and the CLI sees what
/// the server wrote.
#[test]
fn mcp_commit_then_read_round_trip() {
    let store = Store::init();
    let mut server = Server::start(
        &["--repo", store.path(), "--as", AGENT],
        &[("SWARMOPS_RUN_ID", "run-7")],
    );
    server.initialize();
    let c1 = commit_every_kind(&mut server, &store);
    read_every_way(&mut server, &c1);
    let c2 = commit_and_move_main(&mut server, &c1);
    the_cli_prints_the_same(&mut server, &store, &c2);
    server.finish_clean();

    // The CLI reads what the server wrote, and the store verifies clean.
    assert_eq!(store.show("main")["id"], c2.as_str());
    store.assert_clean("after the server's commits");
}

/// A first commit through `server`, of every kind of content (one of them a file in `store`'s
/// temp dir, which the server reads), and main created on it; its id.
fn commit_every_kind(server: &mut Server, store: &Store) -> String {
    let notes = store.tmp().join("notes.txt");
    fs::write(&notes, "kept on the client\n").unwrap();
    let committed = server.ok(
        "commit",
        json!({
            "message": "first commit\n\nwith a body",
            "changes": [
                {"path": "README.md", "op": "put", "content": {"inline": "# acme\n"}},
                {"path": "bin/run", "op": "put", "mode": "exec",
                 "content": {"inline": "#!/bin/sh\necho hi\n"}},
                {"path": "logo.bin", "op": "put", "content": {"base64": "AAEC/w=="}},
                {"path": "notes.txt", "op": "put", "content": {"file": notes.to_str().unwrap()}},
                {"path": "latest", "op": "put", "mode": "symlink",
                 "content": {"inline": "README.md"}},
            ],
        }),
    );
    assert_eq!(committed["result"]["files_changed"], 5, "{committed}");
    assert!(committed["result"].get("parent").is_none(), "a root commit");
    assert_eq!(committed["meta"]["repo"], store.path(), "{committed}");
    let c1 = committed["result"]["changeset"]
        .as_str()
        .unwrap()
        .to_owned();
    let created = server.ok(
        "ref_set",
        json!({"name": "main", "new": c1, "create": true}),
    );
    assert_eq!(
        created["result"],
        json!({"name": "main", "new": c1, "already_applied": false})
    );
    c1
}

/// Reads of `c1`, which main holds, through `server`: files by ref and by id prefix, text,
/// base64 and the client file's bytes, the root directory, the changeset with its attribution.
fn read_every_way(server: &mut Server, c1: &str) {
    let readme = server.ok("read_file", json!({"rev": "main", "path": "README.md"}));
    assert_eq!(readme["result"]["content"], "# acme\n");
    assert_eq!(readme["meta"]["resolved"], json!({"main": c1}));
    let logo = server.ok("read_file", json!({"rev": &c1[..12], "path": "logo.bin"}));
    assert_eq!(
        (&logo["result"]["encoding"], &logo["result"]["content"]),
        (&json!("base64"), &json!("AAEC/w=="))
    );
    let kept = server.ok("read_file", json!({"rev": "main", "path": "notes.txt"}));
    assert_eq!(kept["result"]["content"], "kept on the client\n");
    let root = server.ok("read_tree", json!({"rev": "main"}));
    let names: Vec<&str> = root["result"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["README.md", "bin", "latest", "logo.bin", "notes.txt"]
    );
    let shown = server.ok("show", json!({"rev": "main"}));
    assert_eq!(
        shown["result"]["author"],
        json!({"agent": AGENT, "run_id": "run-7"}),
        "the server's identity, and its run"
    );
    assert_eq!(shown["result"]["message"], "first commit\n\nwith a body");
}

/// A second commit through `server`, on main (at `c1`) by default, and main moved to it by
/// compare-and-swap; then the log, the refs, the store's description and a deep verify. Its id.
fn commit_and_move_main(server: &mut Server, c1: &str) -> String {
    let second = server.ok(
        "commit",
        json!({
            "message": "second",
            "changes": [
                {"path": "README.md", "op": "put", "content": {"inline": "# acme\n\nv2\n"}},
                {"path": "notes.txt", "op": "delete"},
            ],
        }),
    );
    assert_eq!(second["result"]["parent"], c1);
    assert_eq!(second["meta"]["resolved"], json!({"main": c1}));
    let c2 = second["result"]["changeset"].as_str().unwrap().to_owned();
    let moved = server.ok(
        "ref_set",
        json!({"name": "main", "new": c2, "expected_old": c1}),
    );
    assert_eq!(moved["result"]["old"], c1);
    let log = server.ok("log", json!({}));
    let ids: Vec<&str> = log["result"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [c2.as_str(), c1]);
    let refs = server.ok("ref_list", json!({}));
    assert_eq!(
        refs["result"]["refs"],
        json!([{"name": "main", "target": c2, "version": 2}])
    );
    let info = server.ok("repo_info", json!({}));
    assert_eq!(
        (&info["result"]["main"], &info["result"]["changesets"]),
        (&json!(c2), &json!(2))
    );
    let verified = server.ok("verify", json!({"deep": true}));
    assert_eq!(verified["result"]["problems"], json!([]));
    assert_eq!(verified["result"]["changesets_checked"], 2);
    c2
}

/// The same requests through `server` and through the CLI on `store`, where main holds `c2`,
/// print the same envelopes, byte for byte but for the time each took; the latest revision's
/// tool results carry the same envelopes too.
fn the_cli_prints_the_same(server: &mut Server, store: &Store, c2: &str) {
    let cases: [(&str, Value, Vec<&str>); 3] = [
        (
            "read_file",
            json!({"rev": "main~1", "path": "notes.txt"}),
            vec!["cat", "main~1:notes.txt"],
        ),
        (
            "show",
            json!({"rev": c2, "max_changes": 1}),
            vec!["show", c2, "--max-changes", "1"],
        ),
        ("ref_list", json!({}), vec!["ref", "list"]),
    ];
    for (tool, arguments, cli) in cases {
        let call = server.call(tool, arguments.clone());
        let printed = store.read(&cli);
        assert_eq!(printed.exit(), 0, "{printed}");
        assert_eq!(
            without_elapsed(&call.text),
            without_elapsed(printed.stdout.trim_end()),
            "{tool}: the CLI prints the same envelope"
        );
        let latest = server.call_latest(tool, arguments);
        assert_eq!(
            without_elapsed(&latest.text),
            without_elapsed(&call.text),
            "{tool}"
        );
        assert_eq!(latest.result["resultType"], "complete", "{}", latest.result);
    }
}

/// Mutations take the server's identity, from `--as`, else `SWARMOPS_AGENT` with
/// `SWARMOPS_RUN_ID`: without one, `commit`, `ref_set` and `ref_delete` are each an
/// `E_IDENTITY_REQUIRED` tool error that writes nothing, in a session and in a 2026-07-28
/// request alike, and no call can supply one.
#[test]
fn mcp_mutation_without_identity_is_e_identity_required() {
    let store = Store::init();
    let mut server = Server::start(&["--repo", store.path()], &[]);
    server.initialize();
    let commit = json!({
        "message": "anonymous",
        "changes": [{"path": "a.txt", "op": "put", "content": {"inline": "a"}}],
    });
    let mutations = [
        ("commit", commit.clone()),
        (
            "ref_set",
            json!({"name": "main", "new": "main", "create": true}),
        ),
        (
            "ref_delete",
            json!({"name": "main", "expected_old": "0".repeat(64)}),
        ),
    ];
    // Each call is well formed, so the missing identity is what refuses it.
    for (tool, arguments) in mutations {
        let refused = server.err(tool, arguments, "E_IDENTITY_REQUIRED");
        assert_eq!(refused["error"]["exit"], 5, "{refused}");
        let hint = refused["error"]["hint"].as_str().unwrap();
        assert!(hint.contains("--as"), "the hint names --as: {hint}");
        assert_eq!(refused["meta"]["resolved"], json!({}), "nothing resolved");
    }
    // A 2026-07-28 request, which needs no session and is what Claude Code sends, is refused
    // alike.
    let latest = server.call_latest("commit", commit.clone()).envelope;
    assert_eq!(latest["error"]["code"], "E_IDENTITY_REQUIRED", "{latest}");
    assert_eq!(latest["error"]["exit"], 5, "{latest}");
    // An identity in the arguments is not one: the request types have no such field.
    let mut claimed = commit.clone();
    claimed["agent"] = json!(AGENT);
    claimed["as"] = json!(AGENT);
    claimed["identity"] = json!({"agent": AGENT});
    server.err("commit", claimed, "E_IDENTITY_REQUIRED");
    // Reads need none, and nothing was written.
    let info = server.ok("repo_info", json!({}));
    assert_eq!(info["result"]["changesets"], 0);
    assert_eq!(
        server.ok("ref_list", json!({}))["result"]["refs"],
        json!([])
    );
    server.finish_clean();
    assert_eq!(store.info()["changesets"], 0);

    // SWARMOPS_AGENT names the agent when --as does not, with SWARMOPS_RUN_ID's run.
    let env = [
        ("SWARMOPS_AGENT", "human/purple"),
        ("SWARMOPS_RUN_ID", "run-9"),
    ];
    let mut server = Server::start(&["--repo", store.path()], &env);
    server.initialize();
    let id = server.ok("commit", commit.clone())["result"]["changeset"]
        .as_str()
        .unwrap()
        .to_owned();
    let shown = server.ok("show", json!({"rev": id}));
    assert_eq!(
        shown["result"]["author"],
        json!({"agent": "human/purple", "run_id": "run-9"})
    );
    server.finish_clean();
    // --as wins over SWARMOPS_AGENT, as it does for the CLI; the run is still the variable's.
    let mut server = Server::start(&["--repo", store.path(), "--as", AGENT], &env);
    server.initialize();
    let id = server.ok("commit", commit.clone())["result"]["changeset"]
        .as_str()
        .unwrap()
        .to_owned();
    let shown = server.ok("show", json!({"rev": id}));
    assert_eq!(
        shown["result"]["author"],
        json!({"agent": AGENT, "run_id": "run-9"})
    );
    server.finish_clean();

    // An agent name that breaks the grammar refuses every mutation, said of where it came from.
    let mut server = Server::start(&["--repo", store.path(), "--as", "Not An Agent"], &[]);
    server.initialize();
    let bad = server.err("commit", commit, "E_INVALID_ARG");
    assert_eq!(bad["error"]["details"]["source"], "--as", "{bad}");
    assert_eq!(server.ok("repo_info", json!({}))["result"]["changesets"], 2);
    server.finish_clean();
}

/// A failed operation is a tool result with `isError`, whose structured content and text are
/// the CLI's failure envelope: the `E_*` code, the exit code and retry advice of its row, the
/// details and the hint. Arguments that do not fit the request type are `E_INVALID_ARG`, and a
/// store that is not there is `E_NOT_A_REPO` on each call, the server still serving.
#[test]
fn mcp_tool_errors_carry_the_error_envelope() {
    let store = Store::init();
    let c1 = store.commit(AGENT, "src/lib.rs", "pub fn f() {}\n", None);
    let mut server = Server::start(&["--repo", store.path(), "--as", AGENT], &[]);
    server.initialize();
    server.ok(
        "ref_set",
        json!({"name": "main", "new": c1, "create": true}),
    );
    let c2 = store.commit(AGENT, "src/main.rs", "fn main() {}\n", None);

    let missing_rev = server.err("show", json!({"rev": "release/none"}), "E_REV_NOT_FOUND");
    assert_eq!(missing_rev["error"]["exit"], 6);
    assert_eq!(
        missing_rev["error"]["details"],
        json!({"rev": "release/none"})
    );
    let missing_path = server.err(
        "read_file",
        json!({"rev": "main", "path": "src/none.rs"}),
        "E_PATH_NOT_FOUND",
    );
    assert_eq!(
        missing_path["error"]["details"],
        json!({"rev": "main", "path": "src/none.rs"})
    );
    assert_eq!(missing_path["meta"]["resolved"], json!({"main": c1}));
    server.err(
        "read_file",
        json!({"rev": "main", "path": "src"}),
        "E_IS_DIR",
    );
    let stale = server.err(
        "ref_set",
        json!({"name": "main", "new": c2, "create": true}),
        "E_CAS_MISMATCH",
    );
    assert_eq!(
        (&stale["error"]["exit"], &stale["error"]["retryable"]),
        (&json!(4), &json!(true))
    );
    assert_eq!(
        stale["error"]["details"],
        json!({"name": "main", "actual": c1})
    );
    let zero = server.err("log", json!({"limit": 0}), "E_INVALID_ARG");
    assert_eq!(zero["error"]["exit"], 2);
    server.err(
        "commit",
        json!({"message": "m", "changes": []}),
        "E_EMPTY_COMMIT",
    );

    // Arguments that do not fit the tool's request type: a missing field, a wrong type, a
    // value of an enum it does not have.
    let shapes = [
        ("read_file", json!({"path": "src/lib.rs"})),
        ("read_file", json!({"rev": 7, "path": "src/lib.rs"})),
        ("show", json!({"rev": "main", "max_changes": -1})),
        (
            "commit",
            json!({"message": "m", "changes": [{"path": "a", "op": "rename"}]}),
        ),
    ];
    for (tool, arguments) in shapes {
        let refused = server.err(tool, arguments, "E_INVALID_ARG");
        let hint = refused["error"]["hint"].as_str().unwrap();
        assert!(hint.contains("inputSchema"), "{hint}");
        assert_eq!(refused["meta"]["resolved"], json!({}), "{refused}");
    }

    // The CLI prints the same failure, and the latest revision's tool result carries it too.
    let call = server.call("show", json!({"rev": "release/none"}));
    let printed = store.read(&["show", "release/none"]);
    assert_eq!(printed.exit(), 6, "{printed}");
    assert_eq!(
        without_elapsed(&call.text),
        without_elapsed(printed.stdout.trim_end())
    );
    let latest = server.call_latest("show", json!({"rev": "release/none"}));
    assert_eq!(latest.result["isError"], true);
    assert_eq!(without_elapsed(&latest.text), without_elapsed(&call.text));

    // verify finds the blob gone: E_STORE_CORRUPT, the report in details.
    let blob = hash_object(ObjectKind::Blob, b"pub fn f() {}\n");
    fs::remove_file(store.layout().object_path(blob)).unwrap();
    let corrupt = server.err("verify", json!({}), "E_STORE_CORRUPT");
    let problems = corrupt["error"]["details"]["problems"].as_array().unwrap();
    assert_eq!(problems.len(), 1, "{corrupt}");
    assert_eq!(corrupt["error"]["hint"], "swarmops verify --deep");
    server.finish_clean();

    // A store that is not there: E_NOT_A_REPO on each call, and the server serves on.
    let missing = format!("{}/missing", store.tmp().to_str().unwrap());
    let mut server = Server::start(&["--repo", &missing, "--as", AGENT], &[]);
    server.initialize();
    for tool in ["repo_info", "log"] {
        let gone = server.err(tool, json!({}), "E_NOT_A_REPO");
        assert_eq!(gone["meta"]["repo"], missing.as_str(), "{gone}");
        let hint = gone["error"]["hint"].as_str().unwrap();
        assert!(hint.starts_with("swarmops init "), "{hint}");
    }
    server.finish_clean();
    assert!(!fs::exists(&missing).unwrap(), "nothing was created");
    // No store named at all: neither --repo nor SWARMOPS_REPO.
    let mut server = Server::start(&[], &[]);
    server.initialize();
    let unnamed = server.err("repo_info", json!({}), "E_NOT_A_REPO");
    assert!(unnamed["meta"].get("repo").is_none(), "{unnamed}");
    server.finish_clean();
}

/// What a call resolved is that call's alone: through the server, each tool result's
/// `meta.resolved` holds its own revisions only, a failed call's too; and two threads that
/// share one `LocalOps` never see each other's.
#[test]
fn mcp_calls_do_not_share_resolution() {
    let store = Store::init();
    let c1 = store.commit(AGENT, "a.txt", "a", None);
    let c2 = store.commit(AGENT, "b.txt", "b", Some(&c1));

    // Two threads, one LocalOps, each resolving its own revision again and again, a request
    // at a time: each request's record holds exactly the revision it named.
    let ops = LocalOps::open(Utf8Path::new(store.path())).unwrap();
    let mixed = Mutex::new(Vec::new());
    let start = Barrier::new(2);
    thread::scope(|scope| {
        for (rev, id) in [(&c1[..12], &c1), (&c2[..12], &c2)] {
            let (ops, mixed, start) = (&ops, &mixed, &start);
            scope.spawn(move || {
                start.wait();
                for _ in 0..300 {
                    let req = ShowReq {
                        rev: rev.into(),
                        max_changes: 1,
                    };
                    let resolved = Resolved::default();
                    assert_eq!(ops.recording(&resolved).show(req).unwrap().id, *id);
                    let resolved = resolved.take();
                    if resolved != BTreeMap::from([(rev.to_owned(), id.clone())]) {
                        mixed.lock().unwrap().push(resolved);
                    }
                }
            });
        }
    });
    // An operation called on the shared handle itself records into no request.
    let resolved = Resolved::default();
    let request = ops.recording(&resolved);
    ops.show(ShowReq {
        rev: c2[..12].into(),
        max_changes: 1,
    })
    .unwrap();
    request
        .show(ShowReq {
            rev: c1[..12].into(),
            max_changes: 1,
        })
        .unwrap();
    assert_eq!(
        resolved.take(),
        BTreeMap::from([(c1[..12].to_owned(), c1.clone())])
    );
    let mixed = mixed.into_inner().unwrap();
    assert!(
        mixed.is_empty(),
        "{} requests saw another's revisions, or lost their own: {:?}",
        mixed.len(),
        mixed.first()
    );
    drop(ops);

    // Through the server, one call after another.
    let mut server = Server::start(&["--repo", store.path(), "--as", AGENT], &[]);
    server.initialize();
    server.ok(
        "ref_set",
        json!({"name": "main", "new": c2, "create": true}),
    );
    let read = server.ok("read_file", json!({"rev": "main~1", "path": "a.txt"}));
    assert_eq!(read["meta"]["resolved"], json!({"main~1": c1}));
    let show = server.ok("show", json!({"rev": &c1[..12]}));
    assert_eq!(show["meta"]["resolved"], json!({&c1[..12]: c1}));
    let stale = server.err(
        "ref_set",
        json!({"name": "main", "new": "main~1", "expected_old": c1}),
        "E_CAS_MISMATCH",
    );
    assert_eq!(
        stale["meta"]["resolved"],
        json!({"main~1": c1, c1.as_str(): c1})
    );
    let info = server.ok("repo_info", json!({}));
    assert_eq!(info["meta"]["resolved"], json!({}), "resolves nothing");
    let log = server.ok("log", json!({}));
    assert_eq!(log["meta"]["resolved"], json!({"main": c2}));
    let refs = server.ok("ref_list", json!({}));
    assert_eq!(refs["meta"]["resolved"], json!({}));
    server.finish_clean();
}

/// A ref move through the server is a compare-and-swap only when it names what it expects: an
/// `expected_old` that is not a full changeset id (a ref name, a relative revision, an id
/// prefix) is read when the call runs instead of pinning the id the caller read; the server
/// refuses it with `E_INVALID_ARG` and moves nothing. With the full id the caller last read, a
/// stale expectation is `E_CAS_MISMATCH`, and the same move again finds its work done. The CLI
/// keeps taking any revision for `--expect`.
#[test]
fn mcp_compare_and_swap_needs_a_concrete_expected_id() {
    let store = Store::init();
    let mut server = Server::start(&["--repo", store.path(), "--as", AGENT], &[]);
    server.initialize();
    let first = json!({"message": "c1", "changes": [{"path": "c1", "op": "put",
                                                    "content": {"inline": "c1"}}]});
    let c1 = server.ok("commit", first)["result"]["changeset"]
        .as_str()
        .unwrap()
        .to_owned();
    server.ok(
        "ref_set",
        json!({"name": "main", "new": c1, "create": true}),
    );
    // Agent A builds c3 on c1 while agent B lands c2.
    let c2 = commit_on(&mut server, "c2", &c1);
    let c3 = commit_on(&mut server, "c3", &c1);
    server.ok(
        "ref_set",
        json!({"name": "main", "new": c2, "expected_old": c1}),
    );
    vague_expectations_are_refused(&mut server, &c2, &c3);
    concrete_expectations_compare_and_swap(&mut server, &c1, &c2, &c3);
    server.finish_clean();

    // The CLI still takes a ref name for --expect, which its help warns against.
    store
        .run_as(AGENT, &["ref", "set", "release/b", &c1, "--create"], &[])
        .ok();
    let deleted = store.run_as(
        AGENT,
        &["ref", "delete", "release/b", "--expect", "release/b"],
        &[],
    );
    assert_eq!(deleted.ok()["old"], c1.as_str());
}

/// A `commit`'s client file is read by this server, so a relative path (which the server would
/// read from a working directory the agent cannot see: here the crate's, where `src/lib.rs`
/// exists) and a path that is this process's own stdin or stdout (the protocol's pipes, which
/// a read would block on) are refused before anything is resolved or opened, with the change
/// and the file named; the server serves on, and the store is untouched.
#[test]
fn a_client_file_must_be_absolute_and_not_the_servers_stdio() {
    let store = Store::init();
    let mut server = Server::start(&["--repo", store.path(), "--as", AGENT], &[]);
    server.initialize();
    for (file, why) in [
        ("src/lib.rs", "relative path"),
        ("/dev/stdin", "stdin or stdout"),
        ("/dev/stdout", "stdin or stdout"),
    ] {
        let refused = server.err(
            "commit",
            json!({
                "message": "x",
                "changes": [
                    {"path": "README.md", "op": "put", "content": {"inline": "# acme\n"}},
                    {"path": "lib.rs", "op": "put", "content": {"file": file}},
                ],
            }),
            "E_INVALID_ARG",
        );
        assert_eq!(
            refused["error"]["details"],
            json!({"index": 1, "path": "lib.rs", "file": file}),
            "{refused}"
        );
        let message = refused["error"]["message"].as_str().unwrap();
        assert!(message.contains(why), "{file}: {message}");
        assert_eq!(refused["meta"]["resolved"], json!({}), "{refused}");
        assert_eq!(server.result("ping", json!({})), json!({}));
        let info = server.ok("repo_info", json!({}));
        assert_eq!(info["result"]["changesets"], 0, "{file}: {info}");
    }
    server.finish_clean();
    store.assert_clean("after the refused commits");
}
/// A changeset committed through `server` on `parent`, putting a file named `name`; its id.
fn commit_on(server: &mut Server, name: &str, parent: &str) -> String {
    let arguments = json!({
        "message": name,
        "parent": parent,
        "changes": [{"path": name, "op": "put", "content": {"inline": name}}],
    });
    server.ok("commit", arguments)["result"]["changeset"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// The refs, as `ref_list` gives them through `server`.
fn refs_of(server: &mut Server) -> Value {
    server.ok("ref_list", json!({}))["result"]["refs"].take()
}

/// With main at `c2`: agent A's move to `c3` expecting "main" would drop c2, a lost update; a
/// move to main~1 expecting "main" would move main again at every call; an id's prefix or an
/// id with ~N is resolved when the call runs too. Each is refused, nothing is resolved, and
/// main stays at `c2`.
fn vague_expectations_are_refused(server: &mut Server, c2: &str, c3: &str) {
    let held = json!([{"name": "main", "target": c2, "version": 2}]);
    let refused = [
        (
            "ref_set",
            json!({"name": "main", "new": c3, "expected_old": "main"}),
        ),
        (
            "ref_set",
            json!({"name": "main", "new": "main~1", "expected_old": "main"}),
        ),
        (
            "ref_set",
            json!({"name": "main", "new": c3, "expected_old": "main~1"}),
        ),
        (
            "ref_set",
            json!({"name": "main", "new": c3, "expected_old": &c2[..12]}),
        ),
        (
            "ref_set",
            json!({"name": "main", "new": c3, "expected_old": format!("{c2}~0")}),
        ),
        (
            "ref_delete",
            json!({"name": "main", "expected_old": "main"}),
        ),
        (
            "ref_delete",
            json!({"name": "main", "expected_old": &c2[..12]}),
        ),
    ];
    for (tool, arguments) in refused {
        let refused = server.err(tool, arguments.clone(), "E_INVALID_ARG");
        let message = refused["error"]["message"].as_str().unwrap();
        assert!(
            message.contains("full changeset id") && message.contains("compare-and-swap"),
            "{message}"
        );
        assert_eq!(
            refused["error"]["details"],
            json!({"name": "main", "expected_old": arguments["expected_old"]}),
            "{refused}"
        );
        assert_eq!(refused["error"]["hint"], "swarmops ref list", "{refused}");
        assert_eq!(refused["meta"]["resolved"], json!({}), "{refused}");
        assert_eq!(refs_of(server), held, "{tool} {arguments}");
    }
}

/// With main at `c2` and `c3` built on `c1`: the full id main holds moves it, and the same move
/// again finds its work done, as does a move to the target main holds; a stale full id is
/// `E_CAS_MISMATCH`; a relative `new` is safe with a concrete expectation; a delete by full id
/// deletes, and again finds the ref gone.
fn concrete_expectations_compare_and_swap(server: &mut Server, c1: &str, c2: &str, c3: &str) {
    let c4 = commit_on(server, "c4", c2);
    let set = json!({"name": "main", "new": c4, "expected_old": c2});
    let moved = server.ok("ref_set", set.clone());
    assert_eq!(moved["result"]["already_applied"], false);
    let again = server.ok("ref_set", set);
    assert_eq!(again["result"]["already_applied"], true);
    // A move to the target it expects, which main holds, is a replay too, whatever it says: the
    // version below has not moved since.
    for _ in 0..2 {
        let still = server.ok(
            "ref_set",
            json!({"name": "main", "new": c4, "expected_old": c4}),
        );
        assert_eq!(
            still["result"],
            json!({"name": "main", "old": c4, "new": c4, "already_applied": true})
        );
    }
    let stale = server.err(
        "ref_set",
        json!({"name": "main", "new": c3, "expected_old": c2}),
        "E_CAS_MISMATCH",
    );
    assert_eq!(
        stale["error"]["details"],
        json!({"name": "main", "actual": c4})
    );
    assert_eq!(
        refs_of(server),
        json!([{"name": "main", "target": c4, "version": 3}])
    );
    let back = json!({"name": "main", "new": "main~1", "expected_old": c4});
    server.ok("ref_set", back.clone());
    server.err("ref_set", back, "E_CAS_MISMATCH");
    assert_eq!(
        refs_of(server),
        json!([{"name": "main", "target": c2, "version": 4}])
    );
    server.ok(
        "ref_set",
        json!({"name": "release/a", "new": c1, "create": true}),
    );
    let delete = json!({"name": "release/a", "expected_old": c1});
    let deleted = server.ok("ref_delete", delete.clone());
    assert_eq!(deleted["result"]["already_applied"], false);
    let gone = server.ok("ref_delete", delete);
    assert_eq!(gone["result"]["already_applied"], true);
}

/// A command line that `swarmops mcp` refuses starts no server, and its envelope goes to
/// stderr, since stdout belongs to the protocol; the exit code is the error's. Every other
/// command still prints such an envelope on stdout.
#[test]
fn mcp_startup_failures_go_to_stderr() {
    let cases: [Startup<'_>; 3] = [
        (&["mcp", "--no-such-flag"], &[]),
        (&["mcp", "extra"], &[]),
        (&["mcp"], &[("SWARMOPS_FORMAT", "yaml")]),
    ];
    for (args, env) in cases {
        let run = support::run(args, env);
        assert!(
            run.stdout.is_empty(),
            "{args:?} {env:?}: nothing on stdout, not {:?}",
            run.stdout
        );
        assert_eq!(run.exit(), 2, "{args:?} {env:?}");
        let envelope: Value = serde_json::from_str(run.stderr.trim_end())
            .unwrap_or_else(|e| panic!("one envelope on stderr ({e}): {}", run.stderr));
        assert_eq!(
            (&envelope["op"], &envelope["error"]["code"]),
            (&json!("mcp"), &json!("E_INVALID_ARG")),
            "{}",
            run.stderr
        );
    }
    let run = support::run(&["info", "--no-such-flag"], &[]);
    assert!(run.stderr.is_empty(), "{run}");
    assert_eq!(run.exit(), 2);
    let envelope: Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(envelope["error"]["code"], "E_INVALID_ARG");
}

/// What is not a valid request gets a JSON-RPC error, and the server serves on: malformed
/// JSON, a batch, a message without the 2.0 shape, an unknown method or tool, params that do
/// not fit, a request that names no version before any `initialize`, and a version not served.
/// Notifications and stray responses get no answer, and stdout carries nothing but responses.
#[test]
fn mcp_rejects_unknown_methods_and_bad_json() {
    let store = Store::init();
    let mut server = Server::start(&["--repo", store.path(), "--as", AGENT], &[]);

    malformed_messages_are_refused(&mut server);
    requests_before_initialize_are_refused(&mut server);

    // Notifications, known or not, and responses from the client get no answer.
    server.send(&json!({"jsonrpc": "2.0", "method": "notifications/frobnicated"}));
    server.send(
        &json!({"jsonrpc": "2.0", "method": "notifications/cancelled",
                        "params": {"requestId": 1}}),
    );
    server.send(&json!({"jsonrpc": "2.0", "id": 99, "result": {}}));
    server.send_line(b"");
    server.send_line(b"   ");

    server.initialize();
    // In the session, a request that names its revision in _meta is the session's.
    let claimed = json!({"_meta": {"io.modelcontextprotocol/protocolVersion": PREVIOUS}});
    assert_eq!(
        server.result("tools/list", claimed)["tools"]
            .as_array()
            .unwrap()
            .len(),
        10
    );
    server.error("frobnicate", json!({}), METHOD_NOT_FOUND);
    let unknown = server.error(
        "tools/call",
        json!({"name": "rm_rf", "arguments": {}}),
        INVALID_PARAMS,
    );
    assert!(
        unknown["error"]["message"]
            .as_str()
            .unwrap()
            .contains("rm_rf"),
        "{unknown}"
    );
    server.error("tools/call", json!({"arguments": {}}), INVALID_PARAMS);
    server.error(
        "tools/call",
        json!({"name": "show", "arguments": ["main"]}),
        INVALID_PARAMS,
    );
    server.error("tools/list", json!({"cursor": "page-2"}), INVALID_PARAMS);
    server.error(
        "tools/call",
        json!({"name": "rm_rf", "arguments": {}, "_meta": latest_meta()}),
        INVALID_PARAMS,
    );

    // Still serving: the last request answers, and stdout held nothing but the replies read.
    assert_eq!(server.result("ping", json!({})), json!({}));
    let (status, stderr) = server.finish();
    assert!(status.success(), "{status}: {stderr}");
    assert!(
        stderr.contains("not a request"),
        "the stray response is reported on stderr: {stderr}"
    );
}

/// Messages that are not valid requests, sent to `server`: unreadable lines, batches and other
/// values that are not one object, ids that are not strings or integers (each answered without
/// an id, which MCP allows for an id that could not be read and its schema types as an optional
/// string or integer), objects without the 2.0 shape, and params that are not an object.
fn malformed_messages_are_refused(server: &mut Server) {
    server.send_line(b"this is not json");
    server.reply_error(None, PARSE_ERROR);
    server.send_line(b"{\"jsonrpc\": \"2.0\", \"id\": 1, \"method\": \"ping\"");
    server.reply_error(None, PARSE_ERROR);
    server.send_line(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"\xff\"}");
    server.reply_error(None, PARSE_ERROR);
    server.send(&json!([{"jsonrpc": "2.0", "id": 1, "method": "ping"}]));
    server.reply_error(None, INVALID_REQUEST);
    server.send(&json!("ping"));
    server.reply_error(None, INVALID_REQUEST);
    // An id is a string or an integer.
    for id in [
        json!(null),
        json!(1.5),
        json!(2.0),
        json!({}),
        json!([1]),
        json!(true),
    ] {
        server.send(&json!({"jsonrpc": "2.0", "id": id, "method": "ping"}));
        server.reply_error(None, INVALID_REQUEST);
    }
    assert_eq!(
        server.request_with_id(&json!(-7), "ping")["result"],
        json!({})
    );
    assert_eq!(
        server.request_with_id(&json!("x"), "ping")["result"],
        json!({})
    );
    server.send(&json!({"jsonrpc": "2.0", "id": 2}));
    server.reply_error(Some(&json!(2)), INVALID_REQUEST);
    server.send(&json!({"jsonrpc": "1.0", "id": 3, "method": "ping"}));
    server.reply_error(Some(&json!(3)), INVALID_REQUEST);
    server.send(&json!({"id": "four", "method": "ping"}));
    server.reply_error(Some(&json!("four")), INVALID_REQUEST);
    server.send(&json!({"jsonrpc": "2.0", "id": 5, "method": "ping", "params": [1]}));
    server.reply_error(Some(&json!(5)), INVALID_PARAMS);
}

/// Requests that `server`, with no session open, cannot serve: a ping is answered, but a request
/// that names no version in `_meta` has no rules to go by, one that names a revision with a
/// handshake is told to send `initialize` first, and the latest revision's requests are
/// refused for a version not served, a `_meta` without the client's capabilities, or a method
/// that revision does not have.
fn requests_before_initialize_are_refused(server: &mut Server) {
    assert_eq!(server.result("ping", json!({})), json!({}));
    for version in [PREVIOUS, EARLIEST] {
        let meta = json!({
            "io.modelcontextprotocol/protocolVersion": version,
            "io.modelcontextprotocol/clientCapabilities": {},
        });
        for method in ["tools/list", "ping"] {
            let early = server.error(method, json!({"_meta": meta}), UNSUPPORTED_PROTOCOL_VERSION);
            assert_eq!(
                early["error"]["data"],
                json!({"supported": SUPPORTED, "requested": version}),
                "{early}"
            );
            let message = early["error"]["message"].as_str().unwrap();
            assert!(message.contains("initialize"), "{message}");
        }
    }
    let early = server.error("tools/list", json!({}), INVALID_PARAMS);
    assert!(
        early["error"]["message"]
            .as_str()
            .unwrap()
            .contains("io.modelcontextprotocol/protocolVersion"),
        "{early}"
    );
    // The latest revision's requests: a version it does not serve, a _meta without the client
    // capabilities that revision requires, a method that revision does not have.
    let future = json!({
        "io.modelcontextprotocol/protocolVersion": "2099-01-01",
        "io.modelcontextprotocol/clientCapabilities": {},
    });
    let unsupported = server.error(
        "tools/list",
        json!({"_meta": future}),
        UNSUPPORTED_PROTOCOL_VERSION,
    );
    assert_eq!(
        unsupported["error"]["data"],
        json!({"supported": SUPPORTED, "requested": "2099-01-01"})
    );
    let no_capabilities = json!({"io.modelcontextprotocol/protocolVersion": LATEST});
    server.error(
        "tools/list",
        json!({"_meta": no_capabilities}),
        INVALID_PARAMS,
    );
    server.error("ping", json!({"_meta": latest_meta()}), METHOD_NOT_FOUND);
    server.error(
        "frobnicate",
        json!({"_meta": latest_meta()}),
        METHOD_NOT_FOUND,
    );
}

// ---------------------------------------------------------------------------------------------
// The client end.
// ---------------------------------------------------------------------------------------------

/// A running `swarmops mcp`, and the client end of its stdio: requests written to its stdin a
/// line each, replies read from its stdout a line each. Dropped, it is killed if it still runs,
/// and reaped either way.
struct Server {
    child: Child,
    stdin: Option<ChildStdin>,
    /// The lines the server writes on stdout, as a thread reads them.
    stdout: Receiver<String>,
    /// The thread that reads stdout to its end, line by line, into [`Server::stdout`].
    stdout_reader: Option<JoinHandle<()>>,
    /// The thread that reads stderr to its end.
    stderr: Option<JoinHandle<Vec<u8>>>,
    /// The id of the last request sent.
    last_id: u64,
}

/// One tool result, checked: the result itself, its structured content (an envelope), and its
/// text block.
struct ToolCall {
    result: Value,
    envelope: Value,
    text: String,
}

impl Server {
    /// `swarmops mcp`, then `args`, with the order check on, `env` set and no other `SWARMOPS_*`
    /// variable.
    fn start(args: &[&str], env: &[(&str, &str)]) -> Server {
        let mut command = Command::new(support::BIN);
        command.arg("mcp").args(args);
        for name in support::swarmops_vars() {
            command.env_remove(name);
        }
        command.env(swarmops_store::ORDER_CHECK_ENV, "1");
        command
            .envs(env.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().expect("start swarmops mcp");
        let stdout = child.stdout.take().expect("a piped stdout");
        let (lines, stdout_lines) = mpsc::channel();
        let stdout_reader = thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if lines.send(line).is_err() {
                    break;
                }
            }
        });
        let mut stderr = child.stderr.take().expect("a piped stderr");
        let stderr = thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = stderr.read_to_end(&mut bytes);
            bytes
        });
        Server {
            stdin: child.stdin.take(),
            child,
            stdout: stdout_lines,
            stdout_reader: Some(stdout_reader),
            stderr: Some(stderr),
            last_id: 0,
        }
    }

    /// Writes `line` and a line break to the server's stdin.
    fn send_line(&mut self, line: &[u8]) {
        let stdin = self.stdin.as_mut().expect("stdin is open");
        stdin
            .write_all(line)
            .and_then(|()| stdin.write_all(b"\n"))
            .and_then(|()| stdin.flush())
            .expect("write to the server");
    }

    /// Writes `message` as one line.
    fn send(&mut self, message: &Value) {
        self.send_line(message.to_string().as_bytes());
    }

    /// The next line the server writes, within [`REPLY_TIMEOUT`]: it must be one JSON-RPC
    /// response, never a request or a notification of the server's own.
    fn reply(&mut self) -> Value {
        let line = self
            .stdout
            .recv_timeout(REPLY_TIMEOUT)
            .unwrap_or_else(|e| panic!("no reply within {REPLY_TIMEOUT:?}: {e}"));
        let reply: Value = serde_json::from_str(&line)
            .unwrap_or_else(|e| panic!("stdout carries JSON-RPC only, not {line:?}: {e}"));
        assert_eq!(reply["jsonrpc"], "2.0", "a JSON-RPC 2.0 message: {line}");
        assert!(
            reply.get("method").is_none(),
            "the server sends no requests or notifications: {line}"
        );
        assert_ne!(
            reply.get("result").is_some(),
            reply.get("error").is_some(),
            "a response holds a result or an error: {line}"
        );
        if let Some(error) = reply.get("error") {
            assert!(error["code"].is_i64(), "{line}");
            let message = error["message"].as_str().expect("an error message");
            assert!(!message.is_empty(), "{line}");
        }
        reply
    }

    /// The next reply, which must be an error with `code`, answering request `id` (`None`: a
    /// message whose id could not be read, answered without one).
    fn reply_error(&mut self, id: Option<&Value>, code: i64) -> Value {
        let reply = self.reply();
        assert_eq!(reply["error"]["code"], code, "{reply}");
        assert_eq!(reply.get("id"), id, "{reply}");
        reply
    }

    /// Sends request `method` without params under `id`, which the reply must carry: the
    /// reply.
    fn request_with_id(&mut self, id: &Value, method: &str) -> Value {
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method}));
        let reply = self.reply();
        assert_eq!(reply["id"], *id, "{reply}");
        reply
    }

    /// Sends request `method` with `params` under a fresh id; its response.
    fn request(&mut self, method: &str, params: Value) -> Value {
        self.last_id += 1;
        let id = self.last_id;
        let mut request = json!({"jsonrpc": "2.0", "id": id, "method": method});
        request["params"] = params;
        self.send(&request);
        let reply = self.reply();
        assert_eq!(reply["id"], id, "the reply answers the request: {reply}");
        reply
    }

    /// The result of request `method`, which must succeed.
    fn result(&mut self, method: &str, params: Value) -> Value {
        let mut reply = self.request(method, params);
        assert!(reply.get("error").is_none(), "{method}: {reply}");
        reply["result"].take()
    }

    /// The reply to request `method`, which must fail with JSON-RPC error `code`.
    fn error(&mut self, method: &str, params: Value, code: i64) -> Value {
        let reply = self.request(method, params);
        assert_eq!(reply["error"]["code"], code, "{method}: {reply}");
        reply
    }

    /// Opens a 2025-11-25 session: `initialize` and its result, then the initialized
    /// notification.
    fn initialize(&mut self) -> Value {
        let init = self.result(
            "initialize",
            json!({"protocolVersion": PREVIOUS, "capabilities": {}, "clientInfo": client_info()}),
        );
        self.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        init
    }

    /// `tools/call` of `tool` with `arguments`, in the session `initialize` opened: the tool
    /// result, checked ([`check_tool_result`]).
    fn call(&mut self, tool: &str, arguments: Value) -> ToolCall {
        let mut params = json!({"name": tool});
        params["arguments"] = arguments;
        let result = self.result("tools/call", params);
        assert!(result.get("resultType").is_none(), "2025-11-25: {result}");
        check_tool_result(tool, result)
    }

    /// [`Server::call`] as a request of the latest revision.
    fn call_latest(&mut self, tool: &str, arguments: Value) -> ToolCall {
        let mut params = json!({"name": tool, "_meta": latest_meta()});
        params["arguments"] = arguments;
        let result = self.result("tools/call", params);
        assert_eq!(result["resultType"], "complete", "{result}");
        assert_eq!(
            result["_meta"]["io.modelcontextprotocol/serverInfo"]["name"], "swarmops",
            "{result}"
        );
        check_tool_result(tool, result)
    }

    /// The envelope of a call of `tool` that must succeed.
    fn ok(&mut self, tool: &str, arguments: Value) -> Value {
        let call = self.call(tool, arguments);
        assert_eq!(call.envelope["ok"], true, "{tool}: {}", call.envelope);
        call.envelope
    }

    /// The envelope of a call of `tool` that must fail with `code`.
    fn err(&mut self, tool: &str, arguments: Value, code: &str) -> Value {
        let call = self.call(tool, arguments);
        assert_eq!(
            call.envelope["error"]["code"], code,
            "{tool}: {}",
            call.envelope
        );
        call.envelope
    }

    /// Closes the server's stdin and waits, within [`REPLY_TIMEOUT`], for it to exit: its exit
    /// status and what it wrote on stderr. It must write nothing more on stdout.
    fn finish(mut self) -> (ExitStatus, String) {
        drop(self.stdin.take());
        let deadline = Instant::now() + REPLY_TIMEOUT;
        let status = loop {
            if let Some(status) = self.child.try_wait().expect("look whether it exited") {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "still running {REPLY_TIMEOUT:?} after its stdin closed"
            );
            thread::sleep(Duration::from_millis(1));
        };
        let stderr = self.stderr.take().expect("finished once");
        let stderr = String::from_utf8_lossy(&stderr.join().expect("read stderr")).into_owned();
        // The server has exited, so its stdout is at its end: once the reader has read every
        // line it wrote, nothing it wrote can be missed.
        let reader = self.stdout_reader.take().expect("finished once");
        reader.join().expect("read stdout");
        let rest: Vec<String> = self.stdout.try_iter().collect();
        assert!(
            rest.is_empty(),
            "unread lines on stdout: {rest:?}; stderr: {stderr}"
        );
        (status, stderr)
    }

    /// [`Server::finish`], which must end in exit 0 with nothing on stderr.
    fn finish_clean(self) {
        let (status, stderr) = self.finish();
        assert!(status.success(), "{status}: {stderr}");
        assert_eq!(stderr, "", "nothing on stderr");
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // An error is a server reaped already.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The `_meta` of a request of the latest revision.
fn latest_meta() -> Value {
    json!({
        "io.modelcontextprotocol/protocolVersion": LATEST,
        "io.modelcontextprotocol/clientCapabilities": {},
        "io.modelcontextprotocol/clientInfo": client_info(),
    })
}

/// Who the tests say the client is.
fn client_info() -> Value {
    json!({"name": "swarmops-tests", "version": "0"})
}

/// Checks a tool result of `tool`: one text block, whose text is the structured content, an
/// envelope that keeps the contract (see [`check_envelope`]), and `isError` exactly when the
/// envelope is a failure's.
fn check_tool_result(tool: &str, mut result: Value) -> ToolCall {
    let content = result["content"].as_array().expect("a content array");
    let [block] = &content[..] else {
        panic!("one content block: {result}");
    };
    assert_eq!(block["type"], "text", "{result}");
    let text = block["text"].as_str().expect("a text block").to_owned();
    assert!(!text.contains('\n'), "one compact line: {text}");
    let envelope = result["structuredContent"].take();
    assert_eq!(
        serde_json::from_str::<Value>(&text).expect("the text is JSON"),
        envelope,
        "the text block is the structured content"
    );
    result["structuredContent"] = envelope.clone();
    check_envelope(tool, &envelope);
    assert_eq!(result["isError"], envelope["ok"] == false, "{result}");
    ToolCall {
        result,
        envelope,
        text,
    }
}

/// Checks an envelope of `tool` against the contract, as the CLI tests check the CLI's: `ok`
/// with a `result` and no `error`, or a failure with an `error` whose `exit` and `retryable`
/// are its code's row of the table, and no `result`; `api` 1, `op` the tool, and `meta`.
fn check_envelope(tool: &str, envelope: &Value) {
    assert_eq!(envelope["api"], 1, "{envelope}");
    assert_eq!(envelope["op"], tool, "{envelope}");
    assert!(envelope["meta"]["elapsed_ms"].is_u64(), "{envelope}");
    assert!(envelope["meta"]["resolved"].is_object(), "{envelope}");
    assert_eq!(envelope["meta"]["auth"], "trust", "{envelope}");
    if envelope["ok"] == true {
        assert!(envelope.get("result").is_some(), "{envelope}");
        assert!(envelope.get("error").is_none(), "{envelope}");
        return;
    }
    assert_eq!(envelope["ok"], false, "{envelope}");
    assert!(envelope.get("result").is_none(), "{envelope}");
    let error = &envelope["error"];
    let code = ErrorCode::ALL
        .iter()
        .find(|code| error["code"] == code.as_str())
        .unwrap_or_else(|| panic!("a known code: {envelope}"));
    assert_eq!(error["exit"], code.exit(), "{envelope}");
    assert_eq!(error["retryable"], code.retryable(), "{envelope}");
    assert!(error["message"].is_string(), "{envelope}");
    assert!(error["details"].is_object(), "{envelope}");
    assert!(
        error["hint"].is_string() || error["hint"].is_null(),
        "{envelope}"
    );
}

/// Checks the tool `name` lists: its request type's schema (`request`, as `schema_for_all`
/// gives it in `all`) standing alone, with no references; a title, a one-line description, and
/// the annotations of what it does to the store; and for `read_file` alone, the `_meta` that
/// raises the size of a result Claude Code keeps in the conversation to 200,000 characters.
fn check_tool(name: &str, request: &str, tool: &Value, all: &Value) {
    let keys: BTreeSet<&str> = tool
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    let mut expected =
        BTreeSet::from(["name", "title", "description", "inputSchema", "annotations"]);
    if name == "read_file" {
        expected.insert("_meta");
        assert_eq!(
            tool["_meta"],
            json!({"anthropic/maxResultSizeChars": 200_000}),
            "{tool}"
        );
    }
    assert_eq!(keys, expected, "{tool}");
    for field in ["title", "description"] {
        let text = tool[field].as_str().unwrap();
        assert!(!text.is_empty() && !text.contains('\n'), "{name}: {text:?}");
    }
    let schema = &tool["inputSchema"];
    let original = &all[request];
    assert_eq!(schema["type"], "object", "{name}");
    assert_eq!(schema["title"], request, "{name}");
    assert_eq!(schema["$schema"], original["$schema"], "{name}");
    assert_eq!(schema["description"], original["description"], "{name}");
    assert_eq!(schema["required"], original["required"], "{name}");
    let fields = |schema: &Value| -> Vec<String> {
        schema["properties"]
            .as_object()
            .map(|properties| properties.keys().cloned().collect())
            .unwrap_or_default()
    };
    assert_eq!(fields(schema), fields(original), "{name}");
    assert_no_references(name, schema);
    let (read_only, destructive, idempotent) = match name {
        "commit" => (false, false, false),
        "ref_set" | "ref_delete" => (false, true, true),
        _ => (true, false, true),
    };
    assert_eq!(
        tool["annotations"],
        json!({
            "readOnlyHint": read_only,
            "destructiveHint": destructive,
            "idempotentHint": idempotent,
            "openWorldHint": false,
        }),
        "{name}"
    );
}

/// Asserts that `schema` holds no `$ref` and no `$defs` anywhere: it stands alone.
fn assert_no_references(name: &str, schema: &Value) {
    match schema {
        Value::Object(map) => {
            for key in ["$ref", "$defs", "definitions"] {
                assert!(!map.contains_key(key), "{name}: {key} in {schema}");
            }
            map.values()
                .for_each(|inner| assert_no_references(name, inner));
        }
        Value::Array(items) => items
            .iter()
            .for_each(|inner| assert_no_references(name, inner)),
        _ => {}
    }
}

/// Asserts a 2026-07-28 result's caching fields: a freshness in milliseconds, and a scope.
fn assert_cacheable(result: &Value) {
    assert!(result["ttlMs"].is_u64(), "{result}");
    assert_eq!(result["cacheScope"], "public", "{result}");
}

/// `result`'s server version, which every release bumps and no contract rests on, checked
/// against this build's and replaced by `[version]`, for a snapshot.
fn pin_server_version(result: &mut Value) {
    let server_info = &mut result["_meta"]["io.modelcontextprotocol/serverInfo"];
    assert_eq!(
        server_info["version"],
        env!("CARGO_PKG_VERSION"),
        "{server_info}"
    );
    server_info["version"] = json!("[version]");
}

/// `text`, an envelope as printed, with the number of its `elapsed_ms` replaced by 0: the one
/// field two runs of the same request differ in.
fn without_elapsed(text: &str) -> String {
    let key = "\"elapsed_ms\":";
    let at = text
        .find(key)
        .map(|at| at + key.len())
        .expect("an elapsed_ms");
    let digits = text[at..].bytes().take_while(u8::is_ascii_digit).count();
    format!("{}0{}", &text[..at], &text[at + digits..])
}
