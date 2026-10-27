//! The workspace tools of `swarmops mcp` (M5): eleven tools, each naming its workspace by
//! `workspace`, the registry name, on the store and identity the server was started with.
//!
//! A small client of the stateless revision, 2026-07-28: each request names its version in
//! `params._meta`, so no handshake. Every reply is checked to be one JSON-RPC response.

use std::io::{BufRead, BufReader, Write as _};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{Value, json};

use crate::support;
use crate::ws::{AGENT, Lab, MARKER, bootstrap_config, write};

/// The revision these requests name.
const LATEST: &str = "2026-07-28";

/// A running `swarmops mcp`, killed and reaped when dropped.
pub struct Client {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    last_id: u64,
}

impl Client {
    /// `swarmops mcp --repo <store> --as AGENT`, with `env` set and no other `SWARMOPS_*`.
    pub fn start(lab: &Lab, env: &[(&str, &str)]) -> Client {
        Client::start_in(lab, env, Command::new(support::BIN))
    }

    /// [`Client::start`], with `cwd` as the server's working directory.
    pub fn start_at(lab: &Lab, env: &[(&str, &str)], cwd: &Path) -> Client {
        let mut command = Command::new(support::BIN);
        command.current_dir(cwd);
        Client::start_in(lab, env, command)
    }

    /// Starts `command`, the binary, as [`Client::start`] describes.
    fn start_in(lab: &Lab, env: &[(&str, &str)], mut command: Command) -> Client {
        command.args(["mcp", "--repo", lab.path(), "--as", AGENT]);
        for name in support::swarmops_vars() {
            command.env_remove(name);
        }
        command
            .env(swarmops_store::ORDER_CHECK_ENV, "1")
            .envs(env.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = command.spawn().expect("start swarmops mcp");
        let stdin = child.stdin.take().expect("a piped stdin");
        let stdout = BufReader::new(child.stdout.take().expect("a piped stdout"));
        Client {
            child,
            stdin,
            stdout,
            last_id: 0,
        }
    }

    /// `tools/call` of `tool`: the envelope its text block holds, whatever it says.
    pub fn call(&mut self, tool: &str, arguments: Value) -> Value {
        self.last_id += 1;
        let mut request = json!({
            "jsonrpc": "2.0",
            "id": self.last_id,
            "method": "tools/call",
            "params": {
                "name": tool,
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": LATEST,
                    "io.modelcontextprotocol/clientCapabilities": {},
                    "io.modelcontextprotocol/clientInfo": {"name": "swarmops-tests", "version": "0"},
                },
            },
        });
        request["params"]["arguments"] = arguments;
        writeln!(self.stdin, "{request}").expect("write to the server");
        self.stdin.flush().expect("flush");
        let mut line = String::new();
        self.stdout.read_line(&mut line).expect("read a reply");
        let reply: Value = serde_json::from_str(&line)
            .unwrap_or_else(|e| panic!("stdout carries JSON-RPC only, not {line:?}: {e}"));
        assert_eq!(reply["id"], self.last_id, "{reply}");
        let envelope = reply["result"]["structuredContent"].clone();
        assert_eq!(envelope["op"], tool, "{reply}");
        envelope
    }

    /// The result of a call of `tool` that must succeed.
    pub fn ok(&mut self, tool: &str, arguments: Value) -> Value {
        let envelope = self.call(tool, arguments);
        assert_eq!(envelope["ok"], true, "{tool}: {envelope}");
        envelope["result"].clone()
    }

    /// The error of a call of `tool` that must fail with `code`.
    pub fn err(&mut self, tool: &str, arguments: Value, code: &str) -> Value {
        let envelope = self.call(tool, arguments);
        assert_eq!(envelope["error"]["code"], code, "{tool}: {envelope}");
        envelope["error"].clone()
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A workspace tool names its workspace by registry name, so one server serves several
/// workspaces at once: each call reaches its own, a name the store does not know is
/// `E_WS_NOT_FOUND`, and a workspace not rebased onto its target cannot land exactly.
#[test]
fn mcp_ws_tools_address_workspaces_by_name() {
    let lab = Lab::new();
    let mut server = Client::start(&lab, &[]);

    // 1. Two workspaces, made by name; the same create again is the same workspace.
    let one = lab.beside("w1");
    let two = lab.beside("w2");
    for (name, dir) in [("w1", &one), ("w2", &two)] {
        let made = server.ok(
            "ws_create",
            json!({"name": name, "path": dir.to_str().unwrap(), "all": true}),
        );
        assert_eq!(made["created"], true, "{made}");
        assert_eq!(made["status_after"]["name"], name, "{made}");
    }
    let again = server.ok(
        "ws_create",
        json!({"name": "w1", "path": one.to_str().unwrap(), "all": true}),
    );
    assert_eq!(again["created"], false, "{again}");

    // 2. An edit in one is seen there only.
    write(&one, "src/lib.rs", "pub fn f() { 1 }\n");
    let status = server.ok("ws_status", json!({"workspace": "w1"}));
    assert_eq!(status["changes"][0]["path"], "src/lib.rs", "{status}");
    let quiet = server.ok("ws_status", json!({"workspace": "w2"}));
    assert_eq!(quiet["changes"], json!([]), "{quiet}");
    let diff = server.ok("ws_diff", json!({"workspace": "w1", "format": "unified"}));
    assert_eq!(diff["files"][0]["path"], "src/lib.rs", "{diff}");

    // 3. Commit it; info and list say so.
    let committed = server.ok(
        "ws_commit",
        json!({"workspace": "w1", "message": "edit lib", "status_after": "none"}),
    );
    assert!(committed.get("status_after").is_none(), "{committed}");
    let info = server.ok("ws_info", json!({"workspace": "w1"}));
    assert_eq!(info["head"], committed["changeset"], "{info}");
    let listed = server.ok("ws_list", json!({}));
    assert_eq!(
        listed["workspaces"].as_array().unwrap().len(),
        2,
        "{listed}"
    );

    // 4. A name the store does not know; and an exact land of a workspace never rebased.
    let missing = server.err("ws_status", json!({"workspace": "nope"}), "E_WS_NOT_FOUND");
    assert_eq!(missing["details"]["name"], "nope", "{missing}");
    let unrebased = server.err(
        "ws_land",
        json!({"workspace": "w1", "mode": "exact"}),
        "E_INVALID_ARG",
    );
    assert_eq!(unrebased["details"]["missing"], "rebase", "{unrebased}");

    // 5. Rebase (main has not moved), then the exact land.
    server.ok("ws_rebase", json!({"workspace": "w1"}));
    let landed = server.ok("ws_land", json!({"workspace": "w1", "mode": "exact"}));
    assert!(landed["land"]["landed"].is_string(), "{landed}");
    assert_eq!(landed["status_after"]["behind_by"], 0, "{landed}");

    // 6. Materialize and discard in the other.
    write(&two, "scratch.txt", "x\n");
    let discarded = server.ok(
        "ws_discard",
        json!({"workspace": "w2", "paths": ["scratch.txt"]}),
    );
    assert_eq!(
        discarded["discarded"],
        json!(["scratch.txt"]),
        "{discarded}"
    );
}

/// `ws_create` without `path` makes the workspace named after it in the server's working
/// directory, as its schema says and `so ws create` does.
#[test]
fn ws_create_without_a_path_makes_it_in_the_working_directory() {
    let lab = Lab::new();
    let cwd = tempfile::tempdir().expect("a temp dir");
    let mut server = Client::start_at(&lab, &[], cwd.path());

    // 1. Made, in `<cwd>/<name>`.
    let made = server.ok("ws_create", json!({"name": "here", "all": true}));
    assert_eq!(made["created"], true, "{made}");
    let dir = std::path::absolute(cwd.path().join("here")).expect("an absolute path");
    assert!(dir.join(".swarmws").is_dir(), "{made}");

    // 2. The registry has it there.
    let info = server.ok("ws_info", json!({"workspace": "here"}));
    assert_eq!(info["path"], dir.to_str().unwrap(), "{info}");
}

/// The `ws_create` tool bootstraps a new workspace as `so ws create` does (see `ws.rs`): the
/// repository's command runs and leaves its marker; `SWARMOPS_WS_BOOTSTRAP=skip` at the
/// server's start skips it; a failing command is `E_CHECK_FAILED` (`stage: "bootstrap"`).
/// Called by `ws_create_runs_the_bootstrap_through_the_binary_and_the_tool`.
pub fn bootstrap_through_the_tool(lab: &Lab) {
    // 1. The marker.
    let mut server = Client::start(lab, &[]);
    let dir = lab.beside("t1");
    let made = server.ok(
        "ws_create",
        json!({"name": "t1", "path": dir.to_str().unwrap(), "all": true}),
    );
    assert_eq!(made["created"], true, "{made}");
    assert!(dir.join(MARKER).is_file(), "{made}");

    // 2. The opt-out, read when the server starts.
    let mut skipping = Client::start(lab, &[("SWARMOPS_WS_BOOTSTRAP", "skip")]);
    let skipped = lab.beside("t2");
    skipping.ok(
        "ws_create",
        json!({"name": "t2", "path": skipped.to_str().unwrap(), "all": true}),
    );
    assert!(!skipped.join(MARKER).exists());

    // 3. A command that fails.
    lab.publish(".swarmops/workspaces.json", &bootstrap_config("exit 3"));
    let failing = lab.beside("t3");
    let error = server.err(
        "ws_create",
        json!({"name": "t3", "path": failing.to_str().unwrap(), "all": true}),
        "E_CHECK_FAILED",
    );
    assert_eq!(error["details"]["stage"], "bootstrap", "{error}");
    assert!(
        Path::new(failing.to_str().unwrap())
            .join(".swarmws")
            .is_dir()
    );
}
