//! The pilot's workspace step (M5, acceptance 3), as a test of the binary: a Claude Code
//! subagent with `isolation: worktree`, in a directory that is no git repository, gets a
//! workspace from the `WorktreeCreate` hook, builds and tests there, and lands through MCP;
//! the `WorktreeRemove` hook then removes the workspace. The hook inputs are the fixtures the
//! ws crate pins from Claude Code's hooks reference, so no Claude Code and no network.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::mcp_ws::Client;
use crate::ws::{Lab, run_in, write};

/// The fixtures of the hook inputs, in the ws crate.
const FIXTURES: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../swarmops-ws/tests/fixtures/hooks"
);

/// The name Claude Code suggests in the create fixture: the workspace's name.
const NAME: &str = "feature-auth";

/// The subagent's edit.
const EDIT: &str = "pub fn f() -> u32 { 7 }\n";

/// The fixture `file`, its `field` set to `path`.
fn fixture(file: &str, field: &str, path: &Path) -> String {
    let text = fs::read_to_string(Path::new(FIXTURES).join(file)).expect("read a hook fixture");
    let mut input: Value = serde_json::from_str(&text).expect("a fixture is JSON");
    input[field] = json!(path.to_str().unwrap());
    input.to_string()
}

/// The root tree of changeset `id`, from `show`.
fn root_of(lab: &Lab, id: &str) -> String {
    let shown = lab.so(&["show", id]).ok();
    shown["root"].as_str().unwrap().to_owned()
}

/// Hook create, edit, `ws_commit`, `ws_rebase`, the build and test, `ws_land` with mode
/// exact, hook remove: `main` gains the edit, the landed root is the tested tree's, and the
/// workspace and its entry are gone.
#[test]
fn a_subagent_workspace_from_the_hook_lands_through_mcp() {
    let lab = Lab::new();
    let session = lab.beside("session");
    fs::create_dir_all(&session).unwrap();
    assert!(
        !session.join(".git").exists(),
        "the session is no git repository"
    );
    let root = lab.beside("wsroot");
    let env = [
        ("SWARMOPS_REPO", lab.path()),
        ("SWARMOPS_WS_ROOT", root.to_str().unwrap()),
    ];

    // 1. The create hook, from the session's directory: the workspace's path, alone.
    let input = fixture("worktree_create.json", "cwd", &session);
    let made = run_in(
        &session,
        &env,
        &["ws", "hook", "worktree-create"],
        Some(&input),
    );
    assert_eq!(made.exit, 0, "{made}");
    let dir = PathBuf::from(made.stdout.trim_end());
    assert!(dir.join("src/lib.rs").is_file(), "{made}");

    // 2. The subagent edits; through the session's MCP server it commits and rebases.
    write(&dir, "src/lib.rs", EDIT);
    let mut server = Client::start(&lab, &[]);
    server.ok(
        "ws_commit",
        json!({"workspace": NAME, "message": "f returns 7", "status_after": "none"}),
    );
    let rebased = server.ok("ws_rebase", json!({"workspace": NAME}));

    // 3. Its build and test, in the workspace's files: the rebased tree holds the edit.
    let built = fs::read_to_string(dir.join("src/lib.rs")).unwrap();
    assert_eq!(built, EDIT, "the tested file");

    // 4. The exact land: published only onto the head it was rebased onto, the tested tree.
    let landed = server.ok("ws_land", json!({"workspace": NAME, "mode": "exact"}));
    let landed_id = landed["land"]["landed"].as_str().unwrap();
    let tested = rebased["changeset"].as_str().unwrap();
    assert_eq!(root_of(&lab, landed_id), root_of(&lab, tested), "{landed}");
    let on_main = lab.so(&["cat", "main:src/lib.rs"]).ok();
    assert_eq!(on_main["content"], EDIT, "{on_main}");

    // 5. The remove hook: nothing left to rescue, the workspace and its entry gone.
    let gone = fixture("worktree_remove.json", "worktree_path", &dir);
    let removed = run_in(
        &session,
        &env,
        &["ws", "hook", "worktree-remove"],
        Some(&gone),
    );
    assert_eq!(
        (removed.exit, removed.stdout.as_str()),
        (0, ""),
        "{removed}"
    );
    assert!(!dir.exists(), "{removed}");
    let listed = lab.so(&["ws", "list"]).ok();
    assert_eq!(listed["workspaces"], json!([]), "{listed}");
}
