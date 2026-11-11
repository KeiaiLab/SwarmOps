//! An agent with no checkout drives the store through `swarmops mcp` alone (the M21 design
//! §2.5, A56): the server runs in an empty directory outside any repository, and every
//! response passes the MCP tests' check, its schema included.

use std::fs;

use serde_json::json;

use crate::mcp::Server;
use crate::support::Store;

/// The agent the server acts as.
const AGENT: &str = "agent/e2e-1";

/// From an empty working directory, one session commits, reads, diffs, lists the history,
/// rebases, lands, reverts and undoes a ref's deletion; the store verifies clean and the
/// directory stays empty.
#[test]
fn a_process_outside_any_checkout_runs_the_store() {
    let store = Store::init();
    let cwd = tempfile::tempdir().expect("a temp dir");
    let args = ["--repo", store.path(), "--as", AGENT];
    let mut server = Server::start_in(&args, &[], cwd.path());
    server.initialize();

    // 1. A first commit, and main created on it.
    let first = json!({
        "message": "first",
        "changes": [
            {"path": "README.md", "op": "put", "content": {"inline": "# acme\n"}},
            {"path": "src/lib.rs", "op": "put", "content": {"inline": "pub fn f() {}\n"}},
        ],
    });
    let c1 = changeset(&server.ok("commit", first), "/result/changeset");
    server.ok(
        "ref_set",
        json!({"name": "main", "new": c1, "create": true}),
    );

    // 2. Reads of main.
    let readme = server.ok("read_file", json!({"rev": "main", "path": "README.md"}));
    assert_eq!(readme["result"]["content"], "# acme\n", "{readme}");
    server.ok("read_tree", json!({"rev": "main"}));

    // 3. A change on c1, and a publish on main since.
    let change = json!({
        "message": "change f",
        "parent": c1,
        "changes": [{"path": "src/lib.rs", "op": "put", "content": {"inline": "pub fn g() {}\n"}}],
    });
    let head = changeset(&server.ok("commit", change), "/result/changeset");
    let publish = json!({
        "message": "add docs",
        "parent": c1,
        "advance": "main",
        "changes": [{"path": "docs.md", "op": "put", "content": {"inline": "docs\n"}}],
    });
    server.ok("commit", publish);

    // 4. The history.
    server.ok("diff", json!({"from": c1, "to": "main"}));
    server.ok("log", json!({}));
    server.ok("path_history", json!({"rev": "main", "path": "README.md"}));
    server.ok("ancestry", json!({"from": c1, "to": "main"}));

    // 5. The change rebased, landed, and the land reverted.
    server.ok("rebase", json!({"head": head}));
    let landed = server.ok("land", json!({"head": head, "idempotency_key": "land-1"}));
    let landed = changeset(&landed, "/result/landed");
    let revert = json!({"changeset": landed, "reason": "not yet", "idempotency_key": "revert-1"});
    let reverted = changeset(&server.ok("revert", revert), "/result/land/landed");

    // 6. A release ref made and deleted, and the deletion, the newest operation, undone.
    server.ok(
        "ref_set",
        json!({"name": "release/v1", "new": c1, "create": true}),
    );
    server.ok(
        "ref_delete",
        json!({"name": "release/v1", "expected_old": c1}),
    );
    let ops = server.ok("op_log", json!({"limit": 1}));
    let id = ops["result"]["ops"][0]["id"].clone();
    server.ok("undo", json!({"id": id}));
    server.finish_clean();

    // 7. main holds the revert and the release ref is back; the store is sound, and the
    //    directory is as it was.
    assert_eq!(store.show("main")["id"], reverted.as_str());
    assert_eq!(store.show("release/v1")["id"], c1.as_str());
    store.assert_clean("after the session");
    let left = fs::read_dir(cwd.path())
        .expect("the working directory")
        .count();
    assert_eq!(left, 0, "the server wrote nothing in its working directory");
}

/// The changeset id at `pointer` in `envelope`.
fn changeset(envelope: &serde_json::Value, pointer: &str) -> String {
    envelope
        .pointer(pointer)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_else(|| panic!("no id at {pointer}: {envelope}"))
        .to_owned()
}
