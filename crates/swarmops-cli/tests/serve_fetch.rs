//! `swarmops serve`'s read side (M14 design §2, §4), as stock git meets it over ssh: for now the
//! command's surface, which the M14 contract freeze fixed. A refusal before git starts is one
//! `ERR` pkt-line on stdout, which git prints as `remote error:`, and the envelope on stderr; the
//! caller is named by `--as` alone, never by `SWARMOPS_AGENT`.

use serde_json::{Value, json};
use tempfile::TempDir;

use crate::support::{self, Run};

/// The agent a forced command names.
const AGENT: &str = "person/ada";

/// What OpenSSH names the client's command in.
const SSH_ORIGINAL_COMMAND: &str = "SSH_ORIGINAL_COMMAND";

/// A fetch of the store `x`, as git over ssh asks for it.
const UPLOAD_X: &str = "git-upload-pack 'x'";

/// The MCP revision these tests open a session in.
const MCP_REVISION: &str = "2025-11-25";

/// What `run` refused with: the `ERR` pkt-line's text on stdout and the envelope on stderr, both
/// checked against the refusal's contract.
fn refusal(run: &Run) -> (String, Value) {
    // 1. stdout: one pkt-line, its four hex digits counting themselves.
    let stdout = run.stdout.as_str();
    let len = stdout
        .get(..4)
        .and_then(|digits| usize::from_str_radix(digits, 16).ok())
        .unwrap_or_else(|| panic!("a pkt-line on stdout: {run}"));
    assert_eq!(stdout.len(), len, "one pkt-line and nothing else: {run}");
    let text = stdout[4..]
        .strip_prefix("ERR ")
        .unwrap_or_else(|| panic!("an ERR pkt-line: {run}"))
        .to_owned();

    // 2. stderr: the envelope, with the run's exit code.
    let envelope: Value = serde_json::from_str(&run.stderr)
        .unwrap_or_else(|e| panic!("the envelope on stderr ({e}): {run}"));
    assert_eq!(
        (envelope["ok"].as_bool(), envelope["op"].as_str()),
        (Some(false), Some("serve")),
        "{run}"
    );
    assert_eq!(envelope["error"]["exit"], run.exit(), "{run}");
    let code = envelope["error"]["code"].as_str().unwrap();
    let message = envelope["error"]["message"].as_str().unwrap();
    assert_eq!(text, format!("{code}: {message}"), "{run}");
    (text, envelope["error"].clone())
}

/// `serve` without `--root` is a usage error: exit 2, its envelope on stderr, stdout left to
/// git.
#[test]
fn serve_requires_root() {
    for args in [&["serve"][..], &["serve", "--as", AGENT]] {
        let run = support::run(args, &[]);
        assert_eq!(run.exit(), 2, "{run}");
        assert_eq!(run.stdout, "", "{run}");
        let envelope: Value = serde_json::from_str(&run.stderr)
            .unwrap_or_else(|e| panic!("the envelope on stderr ({e}): {run}"));
        assert_eq!(envelope["error"]["code"], "E_INVALID_ARG", "{run}");
    }
}

/// `swarmops serve --stdio`, the store-to-store sync, is M11's: refused `not_served`, asked for
/// through the forced command or on the line.
#[test]
fn serve_refuses_stdio_until_m11() {
    let root = TempDir::new().unwrap();
    let root = root.path().to_str().unwrap();
    let line = ["serve", "--root", root, "--as", AGENT];
    let asked = support::run(&line, &[(SSH_ORIGINAL_COMMAND, "swarmops serve --stdio")]);
    let flagged = support::run(&[&line[..], &["--stdio"]].concat(), &[]);
    for run in [asked, flagged] {
        assert_eq!(run.exit(), 2, "{run}");
        let (text, error) = refusal(&run);
        assert!(text.starts_with("E_INVALID_ARG: "), "{run}");
        assert_eq!(error["details"]["reason"], "not_served", "{run}");
        assert!(!text.contains(root), "no host path: {run}");
    }
}

/// A fetch `serve` cannot serve yet is `E_INVALID_ARG` "not built" (until the upload side is).
#[test]
fn serve_stub_says_not_built() {
    let root = TempDir::new().unwrap();
    let line = [
        "serve",
        "--root",
        root.path().to_str().unwrap(),
        "--as",
        AGENT,
    ];
    let run = support::run(&line, &[(SSH_ORIGINAL_COMMAND, UPLOAD_X)]);
    assert_eq!(run.exit(), 2, "{run}");
    let (text, _) = refusal(&run);
    assert!(text.starts_with("E_INVALID_ARG: "), "{run}");
    assert!(text.contains("not built"), "{run}");
}

/// The serving account's `SWARMOPS_AGENT` names no caller: without `--as`, a connection is
/// `E_AUTH` `no_identity` before anything else is looked at.
#[test]
fn serve_ignores_agent_env() {
    let root = TempDir::new().unwrap();
    let line = ["serve", "--root", root.path().to_str().unwrap()];
    let env = [("SWARMOPS_AGENT", AGENT), (SSH_ORIGINAL_COMMAND, UPLOAD_X)];
    let run = support::run(&line, &env);
    assert_eq!(run.exit(), 5, "{run}");
    let (text, error) = refusal(&run);
    assert!(text.starts_with("E_AUTH: "), "{run}");
    assert!(!text.contains("not built"), "{run}");
    assert_eq!(error["details"]["reason"], "no_identity", "{run}");
}

/// `serve` is no MCP tool, and the MCP server's documentation says so beside `derive`.
#[test]
fn serve_has_no_mcp_tool() {
    // 1. tools/list, in a session of its own.
    let messages = [
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": MCP_REVISION,
            "capabilities": {},
            "clientInfo": {"name": "serve-test", "version": "1"},
        }}),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    ];
    let lines: Vec<String> = messages.iter().map(Value::to_string).collect();
    let input = lines.join("\n") + "\n";
    let run = support::run_bin(support::BIN, &["mcp", "--repo", "unused"], &[], &input);
    assert_eq!(run.exit(), 0, "{run}");
    let listed: Value = run
        .stdout
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|reply| reply["id"] == 2)
        .unwrap_or_else(|| panic!("a tools/list reply: {run}"));
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"commit"), "{names:?}");
    assert!(
        !names.iter().any(|name| name.contains("serve")),
        "{names:?}"
    );

    // 2. The module's documentation.
    let doc = include_str!("../src/mcp.rs");
    let said = doc
        .lines()
        .filter(|line| line.starts_with("//!"))
        .any(|line| line.contains("`derive` and `serve` have no tool"));
    assert!(said, "src/mcp.rs names serve beside derive");
}
