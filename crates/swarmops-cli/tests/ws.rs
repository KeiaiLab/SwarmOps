//! `so ws` and the commands that find a workspace (M5): the binary run inside workspaces, the
//! Claude Code hooks, and the bootstrap.
//!
//! Each test starts the real binary in a temp dir of its own, with none of the developer's
//! `SWARMOPS_*` variables, and speaks to it as an agent does: a store, `main` holding a few
//! files, workspaces made beside it. [`Lab`] is the harness `mcp_ws.rs` reuses.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde_json::{Value, json};

use crate::support::{self, Store};

/// The agent most commands act as.
pub const AGENT: &str = "agent/worker-17";

/// What the binary printed and how it ended.
pub struct Out {
    /// The exit code.
    pub exit: i32,
    /// Stdout.
    pub stdout: String,
    /// Stderr.
    pub stderr: String,
}

impl Out {
    /// The envelope on stdout, which must be one JSON document.
    pub fn envelope(&self) -> Value {
        serde_json::from_str(&self.stdout)
            .unwrap_or_else(|e| panic!("stdout is one envelope ({e}): {self}"))
    }

    /// The result of a command that must have succeeded.
    pub fn ok(&self) -> Value {
        assert_eq!(self.exit, 0, "{self}");
        let mut envelope = self.envelope();
        assert_eq!(envelope["ok"], true, "{self}");
        envelope["result"].take()
    }

    /// The error of a command that must have failed with `code` and exit code `exit`.
    pub fn err(&self, code: &str, exit: i32) -> Value {
        let mut envelope = self.envelope();
        assert_eq!(envelope["error"]["code"], code, "{self}");
        assert_eq!(self.exit, exit, "{self}");
        envelope["error"].take()
    }
}

impl std::fmt::Display for Out {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "exit {}\n--- stdout ---\n{}--- stderr ---\n{}",
            self.exit, self.stdout, self.stderr
        )
    }
}

/// A store whose `main` holds `README.md`, `src/lib.rs` and `docs/guide.md`, and the temp dir
/// workspaces go in.
pub struct Lab {
    /// The store, in `<tmp>/store`.
    pub store: Store,
}

impl Lab {
    /// A fresh store with its first changeset on `main`.
    pub fn new() -> Lab {
        let store = Store::init();
        let changes = store.changes_of(
            "first",
            &[
                ("README.md", "# acme\n"),
                ("src/lib.rs", "pub fn f() {}\n"),
                ("docs/guide.md", "guide\n"),
            ],
        );
        let first = store.run_as(
            AGENT,
            &["commit", "-m", "first", "--changes", &changes],
            &[],
        );
        let c1 = first.ok()["changeset"].as_str().unwrap().to_owned();
        let moved = store.run_as(AGENT, &["ref", "set", "main", &c1, "--create"], &[]);
        assert_eq!(moved.exit(), 0, "{moved}");
        Lab { store }
    }

    /// The store's path.
    pub fn path(&self) -> &str {
        self.store.path()
    }

    /// A directory beside the store, not made.
    pub fn beside(&self, name: &str) -> PathBuf {
        self.store.tmp().join(name)
    }

    /// `--repo <store> --as AGENT`, then `args`, run beside the store.
    pub fn so(&self, args: &[&str]) -> Out {
        let mut line = vec!["--repo", self.path(), "--as", AGENT];
        line.extend(args);
        run_in(self.store.tmp(), &[], &line, None)
    }

    /// Commits `content` at `path` on `main` and moves `main` there.
    pub fn publish(&self, path: &str, content: &str) {
        let before = main_of(self);
        let id = self.store.commit(AGENT, path, content, Some(&before));
        let moved = self.store.run_as(
            AGENT,
            &["ref", "set", "main", &id, "--expect", &before],
            &[],
        );
        assert_eq!(moved.exit(), 0, "{moved}");
    }

    /// Makes workspace `name` of the whole tree in `<tmp>/<name>`; its directory.
    pub fn workspace(&self, name: &str) -> PathBuf {
        let dir = self.beside(name);
        let path = dir.to_str().unwrap();
        let made = self.so(&["ws", "create", name, "--path", path, "--all"]);
        assert_eq!(made.ok()["created"], true, "{made}");
        dir
    }
}

/// Runs the binary in `cwd` with `args`, `env` set and `stdin` on its standard input.
pub fn run_in(cwd: &Path, env: &[(&str, &str)], args: &[&str], stdin: Option<&str>) -> Out {
    let mut command = support::swarmops(args, env);
    command.current_dir(cwd).stdin(Stdio::piped());
    let mut child = command.spawn().expect("start swarmops");
    let mut pipe = child.stdin.take().expect("a piped stdin");
    if let Some(text) = stdin {
        pipe.write_all(text.as_bytes()).expect("write stdin");
    }
    drop(pipe);
    let done = child.wait_with_output().expect("wait for swarmops");
    Out {
        exit: done.status.code().expect("swarmops exited"),
        stdout: String::from_utf8_lossy(&done.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&done.stderr).into_owned(),
    }
}

/// Runs `args` inside `dir` with the store named by the workspace around it, as `AGENT`.
pub fn inside(dir: &Path, args: &[&str]) -> Out {
    let mut line = vec!["--as", AGENT];
    line.extend(args);
    run_in(dir, &[], &line, None)
}

/// Writes `text` at `path` below `dir`, making its directories.
pub fn write(dir: &Path, path: &str, text: &str) {
    let file = dir.join(path);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(file, text).unwrap();
}

/// A command run inside a workspace, with no `--repo` and no `SWARMOPS_REPO`, finds the store
/// the workspace's `workspace.json` names: any command, from the workspace's root or below it;
/// `--repo` and the variable still win; and a directory beside the workspace has no store.
#[test]
fn a_command_inside_a_workspace_finds_its_store() {
    let lab = Lab::new();
    let dir = lab.workspace("w1");
    write(&dir, "src/new.rs", "pub fn n() {}\n");

    // 1. Any command, from the root and from a directory below it.
    let info = inside(&dir, &["info"]);
    assert_eq!(info.ok()["name"], "store", "{info}");
    assert_eq!(info.envelope()["meta"]["repo"], lab.path());
    let deep = inside(&dir.join("src"), &["ws", "status"]);
    let status = deep.ok();
    assert_eq!(status["name"], "w1", "{deep}");
    assert_eq!(status["changes"][0]["path"], "src/new.rs", "{deep}");

    // 2. The variable beats the workspace's store: here, the same store, said by the variable.
    let by_env = run_in(&dir, &[("SWARMOPS_REPO", lab.path())], &["info"], None);
    assert_eq!(by_env.envelope()["meta"]["repo"], lab.path());

    // 3. A selector names the workspace from elsewhere: by name, and by directory.
    let named = lab.so(&["ws", "info", "--ws", "w1"]);
    assert_eq!(named.ok()["path"], dir.to_str().unwrap(), "{named}");
    let by_path = run_in(
        lab.store.tmp(),
        &[],
        &[
            "--as",
            AGENT,
            "ws",
            "info",
            "--ws-path",
            dir.to_str().unwrap(),
        ],
        None,
    );
    assert_eq!(by_path.ok()["name"], "w1", "{by_path}");

    // 4. A directory beside the workspace is no workspace, and names no store.
    let beside = run_in(lab.store.tmp(), &[], &["info"], None);
    beside.err("E_NOT_A_REPO", 6);
    let none = run_in(
        lab.store.tmp(),
        &[],
        &["--repo", lab.path(), "ws", "status"],
        None,
    );
    none.err("E_WS_NOT_FOUND", 6);
}

/// A workspace whose store is not the one resolved is refused, so a stray `SWARMOPS_REPO` cannot
/// commit one store's files into another: `E_INVALID_ARG` (exit 2) with both repository ids.
#[test]
fn a_workspace_of_another_store_is_refused() {
    let lab = Lab::new();
    let dir = lab.workspace("w1");
    let other = Store::init();
    let c1 = other.commit(AGENT, "a.txt", "a\n", None);
    other
        .run_as(AGENT, &["ref", "set", "main", &c1, "--create"], &[])
        .ok();
    write(&dir, "x.txt", "x\n");

    let refused = run_in(
        &dir,
        &[("SWARMOPS_REPO", other.path())],
        &["--as", AGENT, "ws", "commit", "-m", "x"],
        None,
    );
    let error = refused.err("E_INVALID_ARG", 2);
    let workspace_id = error["details"]["workspace_repo_id"].as_str().unwrap();
    let store_id = error["details"]["store_repo_id"].as_str().unwrap();
    assert_ne!(workspace_id, store_id, "{refused}");
    assert_eq!(workspace_id, lab.store.info()["repo_id"], "{refused}");
    assert_eq!(store_id, other.info()["repo_id"], "{refused}");
}

/// Every workspace mutation but `delete` and the hook (M5 design §6.3) answers with the
/// workspace's status after it, so that an agent sees the outcome and the next state in one
/// call: create, materialize, commit, discard, rebase, resolve and land.
#[test]
fn every_ws_mutation_returns_status_after() {
    let lab = Lab::new();

    // 1. create (a prefix), materialize more, commit an edit, discard another.
    let dir = lab.beside("w1");
    let path = dir.to_str().unwrap();
    let made = lab.so(&["ws", "create", "w1", "--path", path, "--prefix", "src"]);
    let made = made.ok();
    assert_eq!(made["status_after"]["state"], "clean", "{made}");
    let more = inside(&dir, &["ws", "materialize", "docs"]).ok();
    assert_eq!(more["status_after"]["name"], "w1", "{more}");
    write(&dir, "src/lib.rs", "pub fn f() { 1 }\n");
    write(&dir, "docs/guide.md", "edited\n");
    let committed = inside(&dir, &["ws", "commit", "-m", "edit lib", "src"]).ok();
    assert_eq!(
        committed["status_after"]["changes"][0]["path"], "docs/guide.md",
        "the other edit remains: {committed}"
    );
    let discarded = inside(&dir, &["ws", "discard"]).ok();
    assert_eq!(
        discarded["status_after"]["changes"],
        json!([]),
        "{discarded}"
    );

    // 2. main moves on the same file: a rebase conflicts, resolve and rebase --continue end it.
    let c2 = lab
        .store
        .commit(AGENT, "src/lib.rs", "pub fn f() { 2 }\n", None);
    lab.store
        .run_as(
            AGENT,
            &["ref", "set", "main", &c2, "--expect", &main_of(&lab)],
            &[],
        )
        .ok();
    let conflict = inside(&dir, &["ws", "rebase"]);
    conflict.err("E_CONFLICT", 3);
    write(&dir, "src/lib.rs", "pub fn f() { 3 }\n");
    let resolved = inside(&dir, &["ws", "resolve", "src/lib.rs"]).ok();
    assert_eq!(resolved["resolved"], json!(["src/lib.rs"]), "{resolved}");
    assert_eq!(resolved["status_after"]["state"], "rebasing", "{resolved}");
    let rebased = inside(&dir, &["ws", "rebase", "--continue"]).ok();
    assert_eq!(rebased["status_after"]["state"], "clean", "{rebased}");

    // 3. The land, and the status after it.
    let landed = inside(&dir, &["ws", "land", "-m", "land it"]).ok();
    assert_eq!(landed["status_after"]["behind_by"], 0, "{landed}");
    assert!(landed["land"]["landed"].is_string(), "{landed}");
}

/// The hint of a commit refused for a new file at the root, run as it is inside the workspace,
/// widens the prefixes; the next commit holds the file (the M15 design §5a, I4).
#[test]
fn ws_follows_the_outside_hint_for_a_new_root_file() {
    // 1. A workspace of `src`; a new file at its root.
    let lab = Lab::new();
    let dir = lab.beside("w1");
    let path = dir.to_str().unwrap();
    lab.so(&["ws", "create", "w1", "--path", path, "--prefix", "src"])
        .ok();
    write(&dir, "notes.txt", "notes\n");

    // 2. The commit refused; its hint, run inside the workspace, succeeds.
    let refused = inside(&dir, &["ws", "commit", "-m", "notes"]);
    let error = refused.err("E_WS_OUTSIDE_PREFIXES", 1);
    let hint = error["hint"].as_str().unwrap();
    let args: Vec<&str> = hint
        .strip_prefix("swarmops ")
        .unwrap_or_else(|| panic!("{refused}"))
        .split(' ')
        .collect();
    inside(&dir, &args).ok();

    // 3. The next commit holds the file.
    let committed = inside(&dir, &["ws", "commit", "-m", "notes"]).ok();
    let changeset = committed["changeset"].as_str().unwrap();
    let shown = lab.so(&["cat", &format!("{changeset}:notes.txt")]).ok();
    assert_eq!(shown["content"], "notes\n", "{shown}");
}

/// The head of `main` in the lab's store.
pub fn main_of(lab: &Lab) -> String {
    let resolved = lab.store.read(&["resolve", "main"]).ok();
    resolved["id"].as_str().unwrap().to_owned()
}

/// `--status-after none` leaves `status_after` out of a mutation's result, as the request's
/// `status_after: "none"` does.
#[test]
fn status_after_none_leaves_it_out() {
    let lab = Lab::new();
    let dir = lab.beside("w1");
    let path = dir.to_str().unwrap();
    let made = lab
        .so(&[
            "ws",
            "create",
            "w1",
            "--path",
            path,
            "--all",
            "--status-after",
            "none",
        ])
        .ok();
    assert!(made.get("status_after").is_none(), "{made}");
    write(&dir, "new.txt", "n\n");
    let committed = inside(&dir, &["ws", "commit", "-m", "n", "--status-after", "none"]).ok();
    assert!(committed.get("status_after").is_none(), "{committed}");
    assert!(committed["changeset"].is_string(), "{committed}");
}

/// The hook JSON for `event`, standing in `cwd`.
fn hook_json(event: &str, cwd: &Path, extra: &Value) -> String {
    let mut input = json!({
        "session_id": "abc123",
        "transcript_path": "/tmp/transcript.jsonl",
        "cwd": cwd.to_str().unwrap(),
        "hook_event_name": event,
    });
    for (key, value) in extra.as_object().unwrap() {
        input[key] = value.clone();
    }
    input.to_string()
}

/// The hooks through the binary: `worktree-create` prints the workspace's path alone on stdout
/// and exits 0, from either store order of §7.1 step 2 (the variable alone, or the parent
/// workspace's own store alone); a store named beside a workspace of another is refused (exit
/// 2), and none at all is `E_NOT_A_REPO` (exit 6), each as an envelope on stderr with stdout
/// empty. `worktree-remove` keeps uncommitted edits as a rescue changeset, deletes the
/// workspace, reports on stderr and exits 0; a directory that is no workspace of ours exits 0
/// with a note, and `config` prints the two settings entries.
#[test]
fn the_hook_commands_run_through_the_binary() {
    let lab = Lab::new();
    let session = lab.beside("session");
    fs::create_dir_all(&session).unwrap();
    let root = lab.beside("wsroot");
    let env = [
        ("SWARMOPS_REPO", lab.path()),
        ("SWARMOPS_WS_ROOT", root.to_str().unwrap()),
    ];
    let create = ["ws", "hook", "worktree-create"];

    // 1. The variable alone: the path, one line, a workspace there.
    let input = hook_json("WorktreeCreate", &session, &json!({"name": "Feature Auth"}));
    let made = run_in(&session, &env, &create, Some(&input));
    assert_eq!(made.exit, 0, "{made}");
    assert_eq!(made.stderr, "", "{made}");
    assert_eq!(made.stdout.lines().count(), 1, "{made}");
    let dir = PathBuf::from(made.stdout.trim_end());
    assert_eq!(dir, root.join("store").join("feature-auth"), "{made}");
    assert!(dir.join(".swarmws/workspace.json").is_file(), "{made}");
    assert!(dir.join("src/lib.rs").is_file(), "{made}");

    // 2. The same session and name again is the same workspace.
    let again = run_in(&session, &env, &create, Some(&input));
    assert_eq!(again.stdout, made.stdout, "{again}");

    // 3. A session standing in that workspace, no variable: its store, its head.
    write(&dir, "src/lib.rs", "pub fn f() { 9 }\n");
    inside(&dir, &["ws", "commit", "-m", "parent work"]).ok();
    write(&dir, "scratch.txt", "uncommitted\n");
    let sub = hook_json("WorktreeCreate", &dir, &json!({"name": "sub"}));
    let sub_env = [("SWARMOPS_WS_ROOT", root.to_str().unwrap())];
    let child = run_in(&dir, &sub_env, &create, Some(&sub));
    assert_eq!(child.exit, 0, "{child}");
    let child_dir = PathBuf::from(child.stdout.trim_end());
    assert_eq!(
        fs::read_to_string(child_dir.join("src/lib.rs")).unwrap(),
        "pub fn f() { 9 }\n",
        "a subagent starts from its parent's committed work"
    );
    assert!(!child_dir.join("scratch.txt").exists(), "{child}");

    // 4. The parent's store and another store named beside it: refused, exit 2, on stderr.
    let other = Store::init();
    let c1 = other.commit(AGENT, "a.txt", "a\n", None);
    other
        .run_as(AGENT, &["ref", "set", "main", &c1, "--create"], &[])
        .ok();
    let clash_env = [
        ("SWARMOPS_REPO", other.path()),
        ("SWARMOPS_WS_ROOT", root.to_str().unwrap()),
    ];
    let clash = run_in(&dir, &clash_env, &create, Some(&sub));
    assert_eq!((clash.exit, clash.stdout.as_str()), (2, ""), "{clash}");
    let refused: Value = serde_json::from_str(&clash.stderr).unwrap();
    assert_eq!(refused["error"]["code"], "E_INVALID_ARG", "{clash}");
    assert!(
        refused["error"]["details"]["workspace_repo_id"].is_string(),
        "{clash}"
    );

    // 5. No store anywhere: exit 6.
    let none = run_in(&session, &[], &create, Some(&input));
    assert_eq!((none.exit, none.stdout.as_str()), (6, ""), "{none}");
    let lost: Value = serde_json::from_str(&none.stderr).unwrap();
    assert_eq!(lost["error"]["code"], "E_NOT_A_REPO", "{none}");

    removal_and_config(&lab, &session, &env, &dir);
}

/// The remove hook and `config`, the end of [`the_hook_commands_run_through_the_binary`].
fn removal_and_config(lab: &Lab, session: &Path, env: &[(&str, &str)], dir: &Path) {
    // 6. Remove: the edit is kept as a rescue changeset, the directory and the entry go.
    let gone = hook_json(
        "WorktreeRemove",
        session,
        &json!({"worktree_path": dir.to_str().unwrap()}),
    );
    let removed = run_in(
        session,
        env,
        &["ws", "hook", "worktree-remove"],
        Some(&gone),
    );
    assert_eq!(
        (removed.exit, removed.stdout.as_str()),
        (0, ""),
        "{removed}"
    );
    assert!(
        removed.stderr.contains("uncommitted edits kept as"),
        "{removed}"
    );
    assert!(!dir.exists(), "{removed}");
    let listed = lab.so(&["ws", "list"]).ok();
    let names: Vec<&str> = listed["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .map(|ws| ws["name"].as_str().unwrap())
        .collect();
    assert!(!names.contains(&"feature-auth"), "{listed}");

    // 7. A directory that is no workspace: left alone, exit 0, no store needed.
    let stray = hook_json(
        "WorktreeRemove",
        session,
        &json!({"worktree_path": session.to_str().unwrap()}),
    );
    let left = run_in(
        session,
        &[],
        &["ws", "hook", "worktree-remove"],
        Some(&stray),
    );
    assert_eq!((left.exit, left.stdout.as_str()), (0, ""), "{left}");
    assert!(left.stderr.contains("not a SwarmOps workspace"), "{left}");
    assert!(session.exists());

    // 8. The settings entries.
    let config = run_in(session, &[], &["ws", "hook", "config"], None);
    let settings: Value = serde_json::from_str(&config.stdout).unwrap();
    assert_eq!(
        settings["hooks"]["WorktreeCreate"][0]["hooks"][0]["command"],
        "swarmops ws hook worktree-create",
        "{config}"
    );
    assert_eq!(config.exit, 0, "{config}");
}

/// The repository's `.swarmops/workspaces.json`, with a bootstrap `command` that writes
/// `MARKER` in the new workspace.
pub const MARKER: &str = "bootstrap.marker";

/// A repository config whose bootstrap runs `script` through `sh -c`.
pub fn bootstrap_config(script: &str) -> String {
    json!({"bootstrap": {"argv": ["sh", "-c", script], "timeout_s": 60}}).to_string()
}

/// `ws create` bootstraps a new workspace: the repository's command runs there and writes its
/// marker; the same create again (`created: false`) runs nothing; `SWARMOPS_WS_BOOTSTRAP=skip`
/// skips it; a failing command is `E_CHECK_FAILED` with `stage: "bootstrap"`, and the workspace
/// stays for inspection. The `ws_create` tool does the same (`mcp_ws.rs`).
#[test]
fn ws_create_runs_the_bootstrap_through_the_binary_and_the_tool() {
    let lab = Lab::new();
    lab.publish(
        ".swarmops/workspaces.json",
        &bootstrap_config(&format!("touch {MARKER}")),
    );

    // 1. New: the marker is there. Again: the command does not run, so a removed marker stays so.
    let dir = lab.beside("w1");
    let path = dir.to_str().unwrap();
    let made = lab.so(&["ws", "create", "w1", "--path", path, "--all"]);
    assert_eq!(made.ok()["created"], true, "{made}");
    assert!(dir.join(MARKER).is_file(), "{made}");
    fs::remove_file(dir.join(MARKER)).unwrap();
    let again = lab.so(&["ws", "create", "w1", "--path", path, "--all"]);
    assert_eq!(again.ok()["created"], false, "{again}");
    assert!(!dir.join(MARKER).exists(), "{again}");

    // 2. The opt-out.
    let skipped = lab.beside("w2");
    let skip_path = skipped.to_str().unwrap();
    let line = [
        "--repo",
        lab.path(),
        "--as",
        AGENT,
        "ws",
        "create",
        "w2",
        "--path",
        skip_path,
    ];
    let out = run_in(
        lab.store.tmp(),
        &[("SWARMOPS_WS_BOOTSTRAP", "skip")],
        &line,
        None,
    );
    assert_eq!(out.ok()["created"], true, "{out}");
    assert!(!skipped.join(MARKER).exists(), "{out}");

    // 3. The files `--from` lists are copied in first.
    let source = lab.beside("source");
    write(&source, ".worktreeinclude", ".env\n");
    write(&source, ".env", "TOKEN=1\n");
    let copied = lab.beside("w3");
    let copied_path = copied.to_str().unwrap();
    let from = source.to_str().unwrap();
    let out = lab.so(&["ws", "create", "w3", "--path", copied_path, "--from", from]);
    assert_eq!(out.ok()["created"], true, "{out}");
    assert_eq!(
        fs::read_to_string(copied.join(".env")).unwrap(),
        "TOKEN=1\n"
    );

    // 4. A command that fails: E_CHECK_FAILED, exit 1, the workspace kept.
    lab.publish(
        ".swarmops/workspaces.json",
        &bootstrap_config("echo broken >&2; exit 3"),
    );
    let failing = lab.beside("w4");
    let failing_path = failing.to_str().unwrap();
    let out = lab.so(&["ws", "create", "w4", "--path", failing_path, "--all"]);
    let error = out.err("E_CHECK_FAILED", 1);
    assert_eq!(error["details"]["stage"], "bootstrap", "{out}");
    assert_eq!(error["details"]["exit_code"], 3, "{out}");
    assert!(failing.join(".swarmws/workspace.json").is_file(), "{out}");

    // 5. The tool does the same, on a repository whose command works again.
    lab.publish(
        ".swarmops/workspaces.json",
        &bootstrap_config(&format!("touch {MARKER}")),
    );
    crate::mcp_ws::bootstrap_through_the_tool(&lab);
}
