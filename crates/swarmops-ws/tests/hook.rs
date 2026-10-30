//! The Claude Code adapter: the hook inputs' parsers, create, remove and the bootstrap, and
//! `ws delete`, which the remove hook ends with.
//!
//! The two inputs in `fixtures/hooks/` are the hooks reference's examples (`WorktreeCreate`,
//! `WorktreeRemove`), as read when the M5 plan's task W9 wrote them.

use std::sync::Arc;

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_api::{
    OpError, ReadFileReq, ShowReq, StatusAfter, StoreOps, WsCommitReq, WsDeleteReq, WsLandReq,
};
use swarmops_ws::hook::{self, Bootstrap, HookCreate, HookRemove, MAX_HOOK_INPUT};
use swarmops_ws::registry::{Entry, Registry};
use swarmops_ws::state::{self, SWARMWS_DIR};
use swarmops_ws::{Ctx, RealFs};

use crate::fixture::{Fixture, Put, file, tree_of, write};

/// The `WorktreeCreate` input of the hooks reference.
const CREATE_JSON: &[u8] = include_bytes!("fixtures/hooks/worktree_create.json");

/// The `WorktreeRemove` input of the hooks reference.
const REMOVE_JSON: &[u8] = include_bytes!("fixtures/hooks/worktree_remove.json");

/// The fixture's context without an identity, as the hook runs without `--as`.
fn anonymous(f: &Fixture) -> Ctx<'_> {
    let mut ctx = f.ctx();
    ctx.who = None;
    ctx
}

/// A fresh directory beside the store, made: a session's working directory, or the root of the
/// hook's workspaces.
fn made(f: &Fixture, name: &str) -> Utf8PathBuf {
    let dir = f.beside(name);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The create input of session `session` asking for `name`, standing in `cwd`.
fn asking(session: &str, name: &str, cwd: &Utf8Path) -> HookCreate {
    HookCreate::new(name.to_owned(), session.to_owned(), cwd.to_owned())
}

/// The registry's entries of the fixture's store.
fn entries(f: &Fixture) -> Vec<Entry> {
    Registry::open(&f.root, Arc::new(RealFs::new()))
        .list()
        .unwrap()
}

/// 16 hex digits of the BLAKE3 hash of `session`: what an entry the hook made records.
fn session_hash(session: &str) -> String {
    blake3::hash(session.as_bytes()).to_hex()[..16].to_owned()
}

/// The workspace name of the directory `dir`.
fn name_of(dir: &Utf8Path) -> String {
    state::read(&RealFs::new(), dir).unwrap().name
}

/// Commits `.swarmops/workspaces.json` holding `json` on `parent`, `main` moved to it.
fn with_config(f: &Fixture, parent: &str, json: &str) -> String {
    f.commit(
        Some(parent),
        &[Put::File(".swarmops/workspaces.json", json)],
    )
}

/// The create hook's path is absolute, under `<ws_root>/<repo>/`, named after the slug Claude
/// Code gave (lowercased, other characters made `-`), a directory holding the tree; its entry
/// records the session's hash, and the creator is `claude-code/<name>` without an identity.
#[test]
fn create_returns_the_absolute_path_the_cli_prints() {
    // 1. A store with one file, a session's directory, the workspaces' root.
    let f = Fixture::new();
    f.commit(None, &[Put::File("src/a.txt", "a\n")]);
    let cwd = made(&f, "session");
    let ws_root = made(&f, "workspaces");

    // 2. The hook, asked for a slug with capitals and a space.
    let ctx = anonymous(&f);
    let input = asking("abc123", "Feature Auth", &cwd);
    let path = hook::worktree_create(&ctx, &input, &ws_root, Bootstrap::Skip).unwrap();

    // 3. The path printed: absolute, the directory made, holding the tree.
    assert!(path.is_absolute(), "{path}");
    assert_eq!(path, ws_root.join("acme").join("feature-auth"));
    assert!(path.is_dir());
    assert_eq!(tree_of(&path), [file("src/a.txt", "a\n")]);

    // 4. Its entry: the session's hash, the hook's identity.
    let [entry] = entries(&f).try_into().unwrap();
    assert_eq!(entry.name, "feature-auth");
    assert_eq!(entry.path, path.as_str());
    assert_eq!(entry.session, Some(session_hash("abc123")));
    assert_eq!(entry.created_by, "claude-code/feature-auth");
}

/// The parsers read the reference's inputs, ignoring the fields they do not use; they refuse
/// more than 64 KiB, a missing field and a relative path, each `E_INVALID_ARG`.
#[test]
fn the_parsers_refuse_more_than_64_kib_and_a_missing_field() {
    // 1. The reference's inputs.
    let create = hook::read_create(CREATE_JSON).unwrap();
    assert_eq!(
        create,
        HookCreate::new(
            "feature-auth".to_owned(),
            "abc123".to_owned(),
            Utf8PathBuf::from("/home/user/acme"),
        )
    );
    let remove = hook::read_remove(REMOVE_JSON).unwrap();
    assert_eq!(
        remove.worktree_path,
        "/home/user/.local/state/swarmops/workspaces/acme/feature-auth"
    );

    // 2. An input of exactly 64 KiB is read; one byte more is refused.
    let padded = |len: usize| {
        let head = r#"{"name":"n","session_id":"s","cwd":"/x","pad":""#;
        let fill = "p".repeat(len - head.len() - 2);
        format!("{head}{fill}\"}}").into_bytes()
    };
    assert_eq!(padded(MAX_HOOK_INPUT).len(), MAX_HOOK_INPUT);
    assert!(hook::read_create(&padded(MAX_HOOK_INPUT)).is_ok());
    let refused = |result: Result<(), OpError>| result.unwrap_err().code;
    let long = padded(MAX_HOOK_INPUT + 1);
    assert_eq!(refused(hook::read_create(&long).map(drop)), "E_INVALID_ARG");
    assert_eq!(refused(hook::read_remove(&long).map(drop)), "E_INVALID_ARG");

    // 3. A missing field, or input that is not JSON.
    let no_name = br#"{"session_id":"s","cwd":"/x"}"#;
    assert_eq!(
        refused(hook::read_create(no_name).map(drop)),
        "E_INVALID_ARG"
    );
    let no_session = br#"{"name":"n","cwd":"/x"}"#;
    assert_eq!(
        refused(hook::read_create(no_session).map(drop)),
        "E_INVALID_ARG"
    );
    let no_path = br#"{"session_id":"s","cwd":"/x"}"#;
    assert_eq!(
        refused(hook::read_remove(no_path).map(drop)),
        "E_INVALID_ARG"
    );
    assert_eq!(
        refused(hook::read_create(b"not json").map(drop)),
        "E_INVALID_ARG"
    );

    // 4. A relative path, which nothing is relative to.
    let relative = br#"{"name":"n","session_id":"s","cwd":"x/y"}"#;
    assert_eq!(
        refused(hook::read_create(relative).map(drop)),
        "E_INVALID_ARG"
    );
    let relative = br#"{"worktree_path":"x/y"}"#;
    assert_eq!(
        refused(hook::read_remove(relative).map(drop)),
        "E_INVALID_ARG"
    );
}

/// Claude Code may retry a create: the same session and name is the same workspace, its
/// bootstrap not run again.
#[cfg(unix)]
#[test]
fn create_with_the_same_session_and_name_is_the_same_workspace() {
    // 1. A repository whose bootstrap appends a line to a file outside the workspace.
    let f = Fixture::new();
    let base = f.commit(None, &[Put::File("src/a.txt", "a\n")]);
    let ran = f.beside("ran.txt");
    let json = format!(r#"{{"bootstrap": {{"argv": ["sh", "-c", "echo once >> {ran}"]}}}}"#);
    with_config(&f, &base, &json);
    let cwd = made(&f, "session");
    let ws_root = made(&f, "workspaces");

    // 2. Created twice by the same session.
    let ctx = anonymous(&f);
    let input = asking("abc123", "feature-auth", &cwd);
    let first = hook::worktree_create(&ctx, &input, &ws_root, Bootstrap::Run).unwrap();
    let again = hook::worktree_create(&ctx, &input, &ws_root, Bootstrap::Run).unwrap();

    // 3. One workspace, one entry, one bootstrap.
    assert_eq!(first, again);
    assert_eq!(entries(&f).len(), 1);
    assert_eq!(std::fs::read_to_string(&ran).unwrap(), "once\n");
}

/// A session standing inside a workspace (a subagent of the session that made it) starts from
/// that workspace's head and holds its prefixes; the parent's uncommitted edits are not carried.
#[test]
fn create_inside_a_workspace_starts_from_its_head_and_prefixes() {
    // 1. A parent workspace holding `src`, with a commit and an uncommitted edit.
    let f = Fixture::new();
    let base = f.commit(
        None,
        &[Put::File("src/a.txt", "a\n"), Put::File("docs/d.md", "d\n")],
    );
    f.create("parent", &base, &["src"]);
    let parent = f.beside("parent");
    write(&parent, "src/a.txt", "committed\n");
    let mut req = WsCommitReq::new("parent".to_owned(), "parent's work".to_owned());
    req.status_after = StatusAfter::None;
    let head = swarmops_ws::commit(&f.ctx(), req).unwrap().changeset;
    write(&parent, "src/a.txt", "uncommitted\n");

    // 2. The hook, from a directory inside the parent.
    let ws_root = made(&f, "workspaces");
    let ctx = anonymous(&f);
    let input = asking("child-session", "child", &parent.join("src"));
    let path = hook::worktree_create(&ctx, &input, &ws_root, Bootstrap::Skip).unwrap();

    // 3. The parent's head and prefixes, its committed content.
    let child = state::read(&RealFs::new(), &path).unwrap();
    assert_eq!(
        (child.base.as_str(), child.head.as_str()),
        (head.as_str(), head.as_str())
    );
    assert_eq!(child.prefixes, ["src"]);
    assert!(!child.all);
    assert_eq!(tree_of(&path), [file("src/a.txt", "committed\n")]);
}

/// Outside a workspace, the repository's own prefixes (`.swarmops/workspaces.json`, read from
/// the store) are the new workspace's; without them, the whole tree.
#[test]
fn create_outside_a_workspace_takes_the_repository_s_prefixes() {
    // 1. A repository naming `src` as its prefix.
    let f = Fixture::new();
    let base = f.commit(
        None,
        &[Put::File("src/a.txt", "a\n"), Put::File("docs/d.md", "d\n")],
    );
    with_config(&f, &base, r#"{"prefixes": ["src"]}"#);
    let cwd = made(&f, "session");
    let ws_root = made(&f, "workspaces");

    // 2. The hook's workspace holds `src` only.
    let ctx = anonymous(&f);
    let input = asking("abc123", "w", &cwd);
    let path = hook::worktree_create(&ctx, &input, &ws_root, Bootstrap::Skip).unwrap();
    assert_eq!(
        state::read(&RealFs::new(), &path).unwrap().prefixes,
        ["src"]
    );
    assert_eq!(tree_of(&path), [file("src/a.txt", "a\n")]);
}

/// Two sessions asking for one slug get two workspaces: the second, finding the name taken by
/// another session, appends a suffix of its own; the first session's retry is its own workspace
/// again. A name a hand-made workspace holds is taken as well.
#[test]
fn a_name_taken_by_another_session_gets_a_suffix() {
    // 1. A store, two sessions in one directory.
    let f = Fixture::new();
    let base = f.commit(None, &[Put::File("a.txt", "a\n")]);
    let cwd = made(&f, "session");
    let ws_root = made(&f, "workspaces");
    let ctx = anonymous(&f);

    // 2. One slug, two sessions: two directories, two entries.
    let first = asking("session-one", "feature-auth", &cwd);
    let second = asking("session-two", "feature-auth", &cwd);
    let one = hook::worktree_create(&ctx, &first, &ws_root, Bootstrap::Skip).unwrap();
    let two = hook::worktree_create(&ctx, &second, &ws_root, Bootstrap::Skip).unwrap();
    assert_ne!(one, two);
    assert_eq!(name_of(&one), "feature-auth");
    let suffixed = name_of(&two);
    let suffix = suffixed.strip_prefix("feature-auth-").unwrap();
    assert_eq!(suffix.len(), 7);
    assert!(suffix.bytes().all(|b| b.is_ascii_hexdigit()), "{suffixed}");
    let sessions: Vec<Option<String>> = entries(&f).into_iter().map(|e| e.session).collect();
    assert_eq!(
        sessions,
        [
            Some(session_hash("session-one")),
            Some(session_hash("session-two"))
        ]
    );

    // 3. Each session's retry is its own workspace again.
    let again = hook::worktree_create(&ctx, &first, &ws_root, Bootstrap::Skip).unwrap();
    assert_eq!(again, one);
    let again = hook::worktree_create(&ctx, &second, &ws_root, Bootstrap::Skip).unwrap();
    assert_eq!(again, two);
    assert_eq!(entries(&f).len(), 2);

    // 4. A name a hand-made workspace (no session) holds: suffixed too.
    f.create("by-hand", &base, &[]);
    let input = asking("session-one", "by-hand", &cwd);
    let path = hook::worktree_create(&ctx, &input, &ws_root, Bootstrap::Skip).unwrap();
    assert!(name_of(&path).starts_with("by-hand-"), "{path}");
}

/// The remove hook commits uncommitted edits as a rescue changeset on the head, deletes the
/// directory and then the entry, and reports the head and the rescue.
#[test]
fn remove_commits_uncommitted_edits_as_a_rescue_and_reports_it() {
    // 1. A hook-made workspace with an edit.
    let f = Fixture::new();
    let base = f.commit(None, &[Put::File("src/a.txt", "a\n")]);
    let cwd = made(&f, "session");
    let ws_root = made(&f, "workspaces");
    let ctx = anonymous(&f);
    let input = asking("abc123", "feature-auth", &cwd);
    let path = hook::worktree_create(&ctx, &input, &ws_root, Bootstrap::Skip).unwrap();
    write(&path, "src/a.txt", "edited\n");

    // 2. Removed: the directory gone, then its entry.
    let remove = HookRemove::new(path.clone());
    let report = hook::worktree_remove(&ctx, &remove).unwrap();
    assert!(!path.exists());
    assert_eq!(entries(&f), []);

    // 3. The rescue: on the head, holding the edit, named after the workspace.
    let rescued = report.rescued.expect("a rescue changeset").to_string();
    assert_eq!(report.head.to_string(), rescued);
    let shown = f
        .ops
        .show({
            let mut req = ShowReq::new(rescued.as_str());
            req.max_changes = 10;
            req
        })
        .unwrap();
    assert_eq!(shown.parents, std::slice::from_ref(&base));
    assert_eq!(
        shown.message,
        "rescue: workspace feature-auth removed with uncommitted edits"
    );
    let mut read = ReadFileReq::new(rescued.as_str(), "src/a.txt");
    read.max_bytes = 1024;
    assert_eq!(f.ops.read_file(read).unwrap().content, "edited\n");

    // 4. A clean workspace leaves no rescue.
    let input = asking("abc123", "clean", &cwd);
    let path = hook::worktree_create(&ctx, &input, &ws_root, Bootstrap::Skip).unwrap();
    let report = hook::worktree_remove(&ctx, &HookRemove::new(path)).unwrap();
    assert_eq!((report.head.to_string(), report.rescued), (base, None));
}

/// A directory that is no workspace is not the hook's: it is found to be none (the CLI exits 0
/// on that), and the remove function refuses it without touching it.
#[test]
fn remove_of_a_directory_that_is_not_a_workspace_exits_zero() {
    // 1. A directory holding a file, no `.swarmws/`.
    let f = Fixture::new();
    f.commit(None, &[Put::File("a.txt", "a\n")]);
    let dir = made(&f, "plain");
    write(&dir, "keep.txt", "keep\n");
    let input = HookRemove::new(dir.clone());

    // 2. Not ours, and left as it was.
    assert_eq!(hook::ours(&input, &RealFs::new()).unwrap(), None);
    let refused = hook::worktree_remove(&anonymous(&f), &input).unwrap_err();
    assert_eq!(refused.code, "E_WS_NOT_FOUND", "{refused:?}");
    assert_eq!(tree_of(&dir), [file("keep.txt", "keep\n")]);

    // 3. A gone directory is none either.
    let gone = HookRemove::new(f.beside("gone"));

    assert_eq!(hook::ours(&gone, &RealFs::new()).unwrap(), None);
}

/// The bootstrap copies the regular files `.worktreeinclude` matches into the new workspace;
/// never a path the tree holds, a symlink, a file reached through a symlinked directory, or
/// anything into `.swarmws/`.
#[cfg(unix)]
#[test]
fn bootstrap_copies_worktreeinclude_matches_but_never_tracked_paths_links_or_escapes() {
    // 1. A store holding `src/a.txt`; a session directory listing what to copy.
    let f = Fixture::new();
    f.commit(None, &[Put::File("src/a.txt", "a\n")]);
    let cwd = made(&f, "session");
    write(
        &cwd,
        ".worktreeinclude",
        ".env\nsecrets/\nsrc/a.txt\n*.link\nout/\n.swarmws/\n",
    );
    write(&cwd, ".env", "TOKEN=1\n");
    write(&cwd, "secrets/key.pem", "key\n");
    write(&cwd, "src/a.txt", "the session's own\n");
    write(&cwd, "notes.txt", "not listed\n");
    write(&cwd, ".swarmws/extra.txt", "private\n");
    let outside = made(&f, "outside");
    write(&outside, "stolen.txt", "outside\n");
    std::os::unix::fs::symlink(outside.join("stolen.txt"), cwd.join("file.link")).unwrap();
    std::os::unix::fs::symlink(&outside, cwd.join("out")).unwrap();

    // 2. The hook, running the bootstrap.
    let ws_root = made(&f, "workspaces");
    let ctx = anonymous(&f);
    let input = asking("abc123", "w", &cwd);
    let path = hook::worktree_create(&ctx, &input, &ws_root, Bootstrap::Run).unwrap();

    // 3. The two listed files copied; the tracked file the tree's; nothing else.
    assert_eq!(
        tree_of(&path),
        [
            file(".env", "TOKEN=1\n"),
            file("secrets/key.pem", "key\n"),
            file("src/a.txt", "a\n"),
        ]
    );
    let swarmws = std::fs::read_dir(path.join(SWARMWS_DIR)).unwrap();
    let names: Vec<String> = swarmws
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert!(!names.contains(&"extra.txt".to_owned()), "{names:?}");

    // 4. Skipped: nothing copied.
    let input = asking("abc123", "skipped", &cwd);
    let path = hook::worktree_create(&ctx, &input, &ws_root, Bootstrap::Skip).unwrap();
    assert_eq!(tree_of(&path), [file("src/a.txt", "a\n")]);
}

/// The repository's bootstrap command runs in the new workspace with `SWARMOPS_WS` set; one
/// that fails fails the create with `E_CHECK_FAILED` (`stage: "bootstrap"`), the workspace kept
/// for inspection.
#[cfg(unix)]
#[test]
fn bootstrap_runs_the_repository_command_and_fails_the_create_on_failure() {
    // 1. A repository whose bootstrap writes the workspace's name.
    let f = Fixture::new();
    let base = f.commit(None, &[Put::File("a.txt", "a\n")]);
    let script = r#"echo \"$SWARMOPS_WS\" > boot.txt"#;
    let json = format!(r#"{{"bootstrap": {{"argv": ["sh", "-c", "{script}"]}}}}"#);
    let ok = with_config(&f, &base, &json);
    let cwd = made(&f, "session");
    let ws_root = made(&f, "workspaces");
    let ctx = anonymous(&f);

    // 2. It ran in the workspace.
    let input = asking("abc123", "booted", &cwd);
    let path = hook::worktree_create(&ctx, &input, &ws_root, Bootstrap::Run).unwrap();
    assert_eq!(
        std::fs::read_to_string(path.join("boot.txt")).unwrap(),
        "booted\n"
    );

    // 3. Skipped: it did not run.
    let input = asking("abc123", "unbooted", &cwd);
    let path = hook::worktree_create(&ctx, &input, &ws_root, Bootstrap::Skip).unwrap();
    assert!(!path.join("boot.txt").exists());

    // 4. A failing one: E_CHECK_FAILED with its outcome, the workspace kept.
    let json = r#"{"bootstrap": {"argv": ["sh", "-c", "echo broken >&2; exit 3"]}}"#;
    let failing = with_config(&f, &ok, json);
    let input = asking("abc123", "failed", &cwd);
    let failed = hook::worktree_create(&ctx, &input, &ws_root, Bootstrap::Run).unwrap_err();
    assert_eq!(failed.code, "E_CHECK_FAILED", "{failed:?}");
    assert_eq!(failed.details["stage"], "bootstrap");
    assert_eq!(failed.details["exit_code"], 3);
    assert_eq!(failed.details["timed_out"], false);
    assert_eq!(failed.details["stderr_tail"], "broken\n");
    assert_eq!(failed.details["changeset"], failing.as_str());
    let kept = ws_root.join("acme").join("failed");
    assert_eq!(name_of(&kept), "failed");
    let log = kept.join(failed.details["log"].as_str().unwrap());
    assert!(std::fs::read_to_string(log).unwrap().contains("broken"));
}

/// `config` prints the settings' `hooks` object with a command entry for each of the two events.
#[test]
fn config_prints_both_hook_entries() {
    let printed: serde_json::Value = serde_json::from_str(&hook::config()).unwrap();
    for (event, verb) in [
        ("WorktreeCreate", "worktree-create"),
        ("WorktreeRemove", "worktree-remove"),
    ] {
        let entry = &printed["hooks"][event][0]["hooks"][0];
        assert_eq!(entry["type"], "command", "{printed}");
        assert_eq!(entry["command"], format!("swarmops ws hook {verb}"));
    }
}

/// `ws delete` refuses a workspace with edits or with commits not landed, unless forced or
/// rescued; a clean one on its base goes without either.
#[test]
fn delete_refuses_dirty_or_unlanded_work_without_force_or_rescue() {
    // 1. A workspace with an edit.
    let f = Fixture::new();
    let base = f.commit(None, &[Put::File("a.txt", "a\n")]);
    let ctx = f.ctx();
    f.create("w", &base, &[]);
    let dir = f.beside("w");
    write(&dir, "a.txt", "edited\n");
    let delete = |name: &str, force: bool, rescue: bool| {
        let mut req = WsDeleteReq::new(name.to_owned());
        req.force = force;
        req.rescue = rescue;
        swarmops_ws::delete(&ctx, req)
    };

    // 2. Dirty: refused, nothing removed.
    let refused = delete("w", false, false).unwrap_err();
    assert_eq!(refused.code, "E_WS_DIRTY", "{refused:?}");
    assert_eq!(refused.details["paths"][0], "a.txt");
    assert!(dir.join("a.txt").exists());

    // 3. Committed but not landed: refused.
    let mut req = WsCommitReq::new("w".to_owned(), "work".to_owned());
    req.status_after = StatusAfter::None;
    let head = swarmops_ws::commit(&ctx, req).unwrap().changeset;
    let refused = delete("w", false, false).unwrap_err();
    assert_eq!(refused.code, "E_WS_DIRTY", "{refused:?}");
    assert_eq!(refused.details["head"], head.as_str());

    // 4. Forced: gone, its head reported, no rescue.
    write(&dir, "a.txt", "dropped\n");
    let done = delete("w", true, false).unwrap();
    assert_eq!(
        (done.deleted.as_str(), done.head.as_str()),
        ("w", head.as_str())
    );
    assert_eq!(done.rescued, None);
    assert!(!dir.exists());
    assert_eq!(entries(&f), []);

    // 5. Rescued: the edit committed first, then gone.
    f.create("r", &base, &[]);
    write(&f.beside("r"), "a.txt", "rescued\n");
    let done = delete("r", false, true).unwrap();
    let rescued = done.rescued.expect("a rescue");
    assert_eq!(done.head, rescued);
    assert!(!f.beside("r").exists());

    // 6. Clean on its base: gone without either.
    f.create("c", &base, &[]);
    let done = delete("c", false, false).unwrap();
    assert_eq!((done.head.as_str(), done.rescued), (base.as_str(), None));
    assert!(!f.beside("c").exists());
    assert_eq!(entries(&f), []);
}

/// Copies past the 10,000 files the bootstrap makes are refused before any is copied
/// (`E_TOO_LARGE`, naming the limit).
#[test]
fn bootstrap_refuses_copies_past_the_file_limit() {
    // 1. A session directory listing 10,001 files.
    let f = Fixture::new();
    f.commit(None, &[Put::File("a.txt", "a\n")]);
    let cwd = made(&f, "session");
    write(&cwd, ".worktreeinclude", "many/\n");
    let many = cwd.join("many");
    std::fs::create_dir_all(&many).unwrap();
    let limit = usize::try_from(swarmops_ws::bootstrap::MAX_COPY_FILES).unwrap();
    for n in 0..=limit {
        std::fs::write(many.join(format!("{n}.txt")), "").unwrap();
    }

    // 2. Refused, the workspace kept with nothing copied.
    let ws_root = made(&f, "workspaces");
    let input = asking("abc123", "w", &cwd);
    let ctx = anonymous(&f);
    let refused = hook::worktree_create(&ctx, &input, &ws_root, Bootstrap::Run).unwrap_err();
    assert_eq!(refused.code, "E_TOO_LARGE", "{refused:?}");
    assert_eq!(refused.details["limit"], "files");
    let kept = ws_root.join("acme").join("w");
    assert_eq!(tree_of(&kept), [file("a.txt", "a\n")]);
}

/// Whether changeset `id`'s tree holds a file at `path`.
fn tree_holds(f: &Fixture, id: &str, path: &str) -> bool {
    let mut req = ReadFileReq::new(id, path);
    req.max_bytes = 1024;
    match f.ops.read_file(req) {
        Ok(_) => true,
        Err(e) if e.code == "E_PATH_NOT_FOUND" => false,
        Err(e) => panic!("{path}: {e:?}"),
    }
}

/// A hook-made sparse workspace holds the root `.gitignore`, so the `.env` the bootstrap copies
/// is ignored: an edit commits and lands without it, and the remove hook deletes the directory
/// (the M15 design §5a, I3).
#[cfg(unix)]
#[test]
fn a_hook_sparse_workspace_with_copies_commits_lands_and_is_removed() {
    // 1. A repository of `src` whose root `.gitignore` names `.env`; a session listing `.env`.
    let f = Fixture::new();
    let base = f.commit(
        None,
        &[
            Put::File(".gitignore", ".env\n"),
            Put::File("src/a.txt", "a\n"),
        ],
    );
    with_config(&f, &base, r#"{"prefixes": ["src"]}"#);
    let cwd = made(&f, "session");
    write(&cwd, ".worktreeinclude", ".env\n");
    write(&cwd, ".env", "TOKEN=1\n");

    // 2. The hook's workspace: the root `.gitignore` among its prefixes, the copy on disk.
    let ws_root = made(&f, "workspaces");
    let ctx = anonymous(&f);
    let input = asking("abc123", "w", &cwd);
    let path = hook::worktree_create(&ctx, &input, &ws_root, Bootstrap::Run).unwrap();
    let saved = state::read(&RealFs::new(), &path).unwrap();
    assert!(
        saved.prefixes.contains(&".gitignore".to_owned()),
        "{saved:?}"
    );
    assert!(path.join(".env").exists());

    // 3. An edit commits and lands; neither tree holds the copy.
    write(&path, "src/a.txt", "edited\n");
    let mut commit = WsCommitReq::new(saved.name.clone(), "edit".to_owned());
    commit.status_after = StatusAfter::None;
    let committed = swarmops_ws::commit(&f.ctx(), commit).unwrap();
    let mut land = WsLandReq::new(saved.name.clone());
    land.status_after = StatusAfter::None;
    let landed = swarmops_ws::land(&f.ctx(), land).unwrap();
    for id in [committed.changeset.as_str(), landed.land.landed.as_str()] {
        assert!(tree_holds(&f, id, "src/a.txt"), "{id}");
        assert!(!tree_holds(&f, id, ".env"), "{id}");
    }

    // 4. The remove hook deletes the directory.
    hook::worktree_remove(&ctx, &HookRemove::new(path.clone())).unwrap();
    assert!(!path.exists());
}
