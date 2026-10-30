//! The merge table (the M15 plan, D9): the three commands that merge a change onto a target
//! that moved, `land`, `ws rebase` and `sync`, give each row of [`ROWS`] the same outcome, the
//! row's own.
//!
//! ```text
//!          ┌─ ours: the change, the workspace's commit, or main's unpublished change
//!   base ──┤
//!          └─ theirs: main, or the remote's main, moved on
//!
//!   land ours | ws rebase | sync origin  ──▶  Outcome::Clean(tree) or Outcome::Conflict(kinds)
//! ```
//!
//! Every path starts from a store whose `main` holds the row's base; `sync` serves the base
//! from the test kit's [`GitServer`] and clones it first.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use swarmops_testkit::remote::{GitServer, HttpAuth, HttpServer};
use tempfile::TempDir;

use crate::support::{Run, Store};

/// The agent whose change merges.
const AGENT: &str = "agent/worker-17";

/// The agent that moves `main` meanwhile.
const OTHER: &str = "agent/worker-18";

/// The served repository.
const REPO: &str = "repo";

/// The remote's name in the store.
const ORIGIN: &str = "origin";

/// The workspace `ws rebase` rebases.
const WS: &str = "w1";

/// The exit code of a merge that conflicts.
const EXIT_CONFLICT: i32 = 3;

/// The reason of the `E_NOTHING_TO_LAND` of a change the target holds already.
const NO_CHANGES: &str = "no_changes";

/// A file every base holds and no row edits, so that no tree goes empty.
const KEPT: (&str, &str) = ("kept.txt", "kept\n");

/// One edit of ours.
#[derive(Clone, Copy, Debug)]
enum Edit {
    /// The file at the path, with these contents.
    Put(&'static str, &'static str),
    /// The file at the path, deleted.
    Delete(&'static str),
}

/// What a merge ended in.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Outcome {
    /// Merged: each file of the result's tree, with its contents.
    Clean(BTreeMap<String, String>),
    /// Refused: each conflicting path, with its kind.
    Conflict(BTreeMap<String, String>),
}

/// The outcome a row expects, as `(path, contents)` or `(path, kind)` pairs.
#[derive(Clone, Copy, Debug)]
enum Expected {
    /// [`Outcome::Clean`] with these files, [`KEPT`] besides.
    Clean(&'static [(&'static str, &'static str)]),
    /// [`Outcome::Conflict`] on these paths.
    Conflict(&'static [(&'static str, &'static str)]),
}

impl Expected {
    /// The outcome itself.
    fn outcome(self) -> Outcome {
        let map = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> {
            let owned = pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()));
            owned.collect()
        };
        match self {
            Expected::Clean(files) => {
                let mut tree = map(files);
                tree.insert(KEPT.0.to_owned(), KEPT.1.to_owned());
                Outcome::Clean(tree)
            }
            Expected::Conflict(kinds) => Outcome::Conflict(map(kinds)),
        }
    }
}

/// One row of the table: the base, each side's edits, and the outcome.
struct Row {
    /// The row's name, in a failure.
    name: &'static str,
    /// The files of the base, [`KEPT`] besides.
    base: &'static [(&'static str, &'static str)],
    /// The change's edits.
    ours: &'static [Edit],
    /// The target's edits since the base: puts only, as a git commit makes them.
    theirs: &'static [(&'static str, &'static str)],
    /// What every command gives.
    expected: Expected,
}

/// The table: the same edit on both sides, edits of lines apart, a delete against a change,
/// and a file holding a NUL both changed.
const ROWS: [Row; 4] = [
    Row {
        name: "same edit",
        base: &[("f.txt", "1\n2\n3\n")],
        ours: &[Edit::Put("f.txt", "1\nsame\n3\n")],
        theirs: &[("f.txt", "1\nsame\n3\n")],
        expected: Expected::Clean(&[("f.txt", "1\nsame\n3\n")]),
    },
    Row {
        name: "disjoint ranges",
        base: &[("f.txt", "1\n2\n3\n4\n5\n")],
        ours: &[Edit::Put("f.txt", "one\n2\n3\n4\n5\n")],
        theirs: &[("f.txt", "1\n2\n3\n4\nfive\n")],
        expected: Expected::Clean(&[("f.txt", "one\n2\n3\n4\nfive\n")]),
    },
    Row {
        name: "delete against change",
        base: &[("gone.txt", "gone\n")],
        ours: &[Edit::Delete("gone.txt")],
        theirs: &[("gone.txt", "changed\n")],
        expected: Expected::Conflict(&[("gone.txt", "delete_modify")]),
    },
    Row {
        name: "binary",
        base: &[("logo.bin", "\0base\n")],
        ours: &[Edit::Put("logo.bin", "\0ours\n")],
        theirs: &[("logo.bin", "\0theirs\n")],
        expected: Expected::Conflict(&[("logo.bin", "binary")]),
    },
];

/// Each row's edits give one outcome through `land`, `ws rebase` and `sync`, the row's own.
#[test]
fn merge_table_agrees_across_commands() {
    for row in &ROWS {
        let outcomes = [
            ("land", by_land(row)),
            ("ws rebase", by_rebase(row)),
            ("sync", by_sync(row)),
        ];
        let expected = row.expected.outcome();
        let agree = outcomes.iter().all(|(_, outcome)| *outcome == expected);
        assert!(
            agree,
            "row {:?}: expected {expected:?}, got {outcomes:#?}",
            row.name
        );
    }
}

/// `land` of the change onto `main`, which another agent moved.
fn by_land(row: &Row) -> Outcome {
    let store = Store::init();
    let base = based(&store, row);

    // 1. The change on the base; main moved past the base.
    let ours = commit(&store, AGENT, &edits(row.ours), &base);
    let theirs = commit(&store, OTHER, &puts(row.theirs), &base);
    set_main(&store, &theirs, Some(&base));

    // 2. The land. A change the target holds already lands nothing: the merge is main's tree.
    let landed = store.run_as(AGENT, &["land", &ours], &[]);
    if landed.envelope()["error"]["details"]["reason"] == NO_CHANGES {
        landed.err("E_NOTHING_TO_LAND");
        return Outcome::Clean(tree(&store, &theirs));
    }
    outcome(&store, &landed, "/landed")
}

/// `ws rebase` of a workspace's commit onto `main`, which another agent moved.
fn by_rebase(row: &Row) -> Outcome {
    let store = Store::init();
    let base = based(&store, row);

    // 1. A workspace of the whole base, its edits committed.
    let dir = store.tmp().join(WS);
    let path = dir.to_str().expect("UTF-8 temp paths");
    let create = ["ws", "create", WS, "--path", path, "--all"];
    store.run_as(AGENT, &create, &[]).ok();
    for edit in row.ours {
        apply(&dir, *edit);
    }
    let line = ["ws", "commit", "--ws", WS, "-m", "ours"];
    store.run_as(AGENT, &line, &[]).ok();

    // 2. main moved past the base; the rebase.
    let theirs = commit(&store, OTHER, &puts(row.theirs), &base);
    set_main(&store, &theirs, Some(&base));
    let rebased = store.run_as(AGENT, &["ws", "rebase", "--ws", WS], &[]);
    outcome(&store, &rebased, "/changeset")
}

/// `sync` of `main`'s unpublished change onto the remote's `main`, which another writer moved.
fn by_sync(row: &Row) -> Outcome {
    let remote = Remote::new();
    let mut files = vec![KEPT];
    files.extend_from_slice(row.base);
    remote.server.commit_on(REPO, "main", "base", &files);

    // 1. A clone: main at the remote's head.
    let vars = remote.vars();
    let env: Vec<(&str, &str)> = vars.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let store = Store::init().with_env(&env);
    let url = remote.http.url(REPO);
    let add = ["remote", "add", ORIGIN, url.as_str(), "--branch", "main"];
    store.read(&add).ok();
    let fetched = store.read(&["fetch", ORIGIN]).ok();
    let base = fetched["head"]["changeset"].as_str().expect("a head");
    set_main(&store, base, None);

    // 2. The change on main; the remote moved past the base.
    let ours = commit(&store, AGENT, &edits(row.ours), base);
    set_main(&store, &ours, Some(base));
    remote.server.commit_on(REPO, "main", "theirs", row.theirs);

    // 3. The sync, and main after it.
    let synced = store.run_as(AGENT, &["sync", ORIGIN], &[]);
    if synced.exit() != 0 {
        return outcome(&store, &synced, "");
    }
    let main = store.read(&["resolve", "main"]).ok();
    Outcome::Clean(tree(&store, main["id"].as_str().expect("an id")))
}

/// A served repository and the git environment the binary runs with.
struct Remote {
    /// The remote's side.
    server: GitServer,
    /// The server's smart HTTP.
    http: HttpServer,
    /// git's empty `HOME` for the product's children, removed when the remote is dropped.
    home: TempDir,
}

impl Remote {
    /// [`REPO`], empty, served.
    fn new() -> Remote {
        let server = GitServer::new();
        server.create(REPO);
        let http = server.http(HttpAuth::Anonymous);
        let home = tempfile::tempdir().expect("a temp HOME");
        Remote { server, http, home }
    }

    /// The variables every run sets: the server's git, an empty `HOME`, no git configuration.
    fn vars(&self) -> Vec<(&'static str, String)> {
        vec![
            ("SWARMOPS_GIT", path_text(&real_git(&self.server))),
            ("HOME", path_text(self.home.path())),
            ("GIT_CONFIG_NOSYSTEM", "1".to_owned()),
            ("GIT_CONFIG_GLOBAL", "/dev/null".to_owned()),
        ]
    }
}

/// The real git the server runs, as an absolute path.
fn real_git(server: &GitServer) -> PathBuf {
    let program = server.git_program();
    if program.is_absolute() {
        return program.to_path_buf();
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
        .expect("git on PATH")
}

/// `path` as text.
fn path_text(path: &Path) -> String {
    path.to_str().expect("UTF-8 temp paths").to_owned()
}

/// `main` made at a changeset of the row's base, [`KEPT`] besides; the changeset.
fn based(store: &Store, row: &Row) -> String {
    let mut files = vec![KEPT];
    files.extend_from_slice(row.base);
    let changes = write_changes(store, "base", &puts(&files));
    let line = ["commit", "-m", "base", "--changes", &changes];
    let base = store.run_as(AGENT, &line, &[]).ok();
    let base = base["changeset"].as_str().expect("a changeset").to_owned();
    set_main(store, &base, None);
    base
}

/// A commit of `changes` on `parent` as `agent`; the changeset.
fn commit(store: &Store, agent: &str, changes: &[Value], parent: &str) -> String {
    let changes = write_changes(store, agent.replace('/', "-").as_str(), changes);
    let line = [
        "commit",
        "-m",
        agent,
        "--changes",
        &changes,
        "--parent",
        parent,
    ];
    let made = store.run_as(agent, &line, &[]).ok();
    made["changeset"].as_str().expect("a changeset").to_owned()
}

/// Moves `main` to `to` from `from`, or makes it.
fn set_main(store: &Store, to: &str, from: Option<&str>) {
    let mut line = vec!["ref", "set", "main", to];
    match from {
        Some(from) => line.extend(["--expect", from]),
        None => line.push("--create"),
    }
    store.run_as(OTHER, &line, &[]).ok();
}

/// Writes the change list `changes` to the temp dir as `<name>.json`; its path.
fn write_changes(store: &Store, name: &str, changes: &[Value]) -> String {
    let file = store.tmp().join(format!("{name}.json"));
    fs::write(&file, Value::from(changes.to_vec()).to_string()).expect("write a change list");
    path_text(&file)
}

/// The change list of `files`, each put.
fn puts(files: &[(&str, &str)]) -> Vec<Value> {
    let put = |(path, content): &(&str, &str)| -> Value {
        json!({"path": path, "op": "put", "content": {"inline": content}})
    };
    files.iter().map(put).collect()
}

/// The change list of `edits`.
fn edits(edits: &[Edit]) -> Vec<Value> {
    let change = |edit: &Edit| -> Value {
        match edit {
            Edit::Put(path, content) => {
                json!({"path": path, "op": "put", "content": {"inline": content}})
            }
            Edit::Delete(path) => json!({"path": path, "op": "delete"}),
        }
    };
    edits.iter().map(change).collect()
}

/// `edit` made in the workspace directory `dir`.
fn apply(dir: &Path, edit: Edit) {
    match edit {
        Edit::Put(path, content) => fs::write(dir.join(path), content).expect("write a file"),
        Edit::Delete(path) => fs::remove_file(dir.join(path)).expect("delete a file"),
    }
}

/// The outcome of the merge `run`: the tree of the changeset at `pointer` in its result, or
/// the kinds of its `E_CONFLICT`.
fn outcome(store: &Store, run: &Run, pointer: &str) -> Outcome {
    if run.exit() != EXIT_CONFLICT {
        let result = run.ok();
        let id = result.pointer(pointer).and_then(Value::as_str);
        let id = id.unwrap_or_else(|| panic!("no changeset at {pointer}: {result}"));
        return Outcome::Clean(tree(store, id));
    }
    let error = run.err("E_CONFLICT");
    let files = error["details"]["report"]["files"].as_array().cloned();
    let kind = |file: Value| -> (String, String) {
        let path = file["path"].as_str().expect("a path").to_owned();
        (path, file["kind"].as_str().expect("a kind").to_owned())
    };
    Outcome::Conflict(
        files
            .expect("the report's files")
            .into_iter()
            .map(kind)
            .collect(),
    )
}

/// Each file of changeset `id`'s tree, with its contents.
fn tree(store: &Store, id: &str) -> BTreeMap<String, String> {
    let listed = store.read(&["ls", id]).ok();
    let entries = listed["entries"].as_array().expect("entries").clone();
    let file = |entry: Value| -> (String, String) {
        let name = entry["name"].as_str().expect("a name").to_owned();
        let read = store.read(&["cat", &format!("{id}:{name}")]).ok();
        (name, read["content"].as_str().expect("contents").to_owned())
    };
    entries.into_iter().map(file).collect()
}
