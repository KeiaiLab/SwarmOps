//! git's verbs through the binary (the git-familiar command design, §3 to §5): an alias runs its
//! target, a refused git verb carries the line that names what to type, and a line that ran before
//! prints what it printed, inside a workspace and outside one.
//!
//! The fixture is a store whose `main` holds two changesets (`README.md`, `src/lib.rs`,
//! `docs/guide.md`, then `a.txt`), workspace `w` of it beside the store, and the store's temp dir
//! as the place outside every workspace. No test opens a connection: `clone` runs with no
//! identity, which is refused first, and the store has no remote.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;

use crate::support::{self, BIN, SO};
use crate::ws::{AGENT, Lab, write};

/// The URL `clone` is given: never reached, the identity is checked first.
const URL: &str = "https://remote.invalid/x.git";

/// The keys whose values differ between two runs of one line.
const TIME_KEYS: &[&str] = &["elapsed_ms", "created_at", "at", "secs"];

/// The time every commit of a compared pair signs at.
const NOW: &str = "1700000000";

/// Where a line is typed in the fixture.
#[derive(Clone, Copy, Debug)]
enum At {
    /// The workspace's root.
    Root,
    /// `sub/`, below it.
    Below,
    /// The store's temp dir, in no workspace.
    Elsewhere,
}

/// The fixture: the store, its workspace `w` with `sub/`, and the place outside.
struct Fixture {
    lab: Lab,
    ws: PathBuf,
}

impl Fixture {
    /// A store whose `main` holds two changesets, `a.txt` in the second, and workspace `w`.
    fn new() -> Fixture {
        let lab = Lab::new();
        lab.publish("a.txt", "a\n");
        let ws = lab.workspace("w");
        fs::create_dir_all(ws.join("sub")).unwrap();
        Fixture { lab, ws }
    }

    /// The directory of `at`.
    fn dir(&self, at: At) -> PathBuf {
        match at {
            At::Root => self.ws.clone(),
            At::Below => self.ws.join("sub"),
            At::Elsewhere => self.lab.store.tmp().to_path_buf(),
        }
    }

    /// `line` run by `bin` at `at`, `--repo` the store outside a workspace.
    fn run(&self, bin: &str, at: At, line: &[&str]) -> Out {
        let mut words = Vec::new();
        if matches!(at, At::Elsewhere) {
            words.extend(["--repo", self.lab.path()]);
        }
        words.extend(line);
        run_at(bin, &self.dir(at), &words, &[])
    }

    /// `line` run as [`AGENT`] by `bin` at `at`.
    fn run_as(&self, bin: &str, at: At, line: &[&str]) -> Out {
        let mut words = vec!["--as", AGENT];
        words.extend(line);
        self.run(bin, at, &words)
    }
}

/// What a run printed and how it ended.
struct Out {
    line: Vec<String>,
    exit: i32,
    stdout: String,
    stderr: String,
}

impl Out {
    /// The envelope on stdout.
    fn envelope(&self) -> Value {
        serde_json::from_str(&self.stdout)
            .unwrap_or_else(|e| panic!("stdout is one envelope ({e}): {self}"))
    }

    /// The first line of `error.hint`.
    fn first_hint(&self) -> String {
        let envelope = self.envelope();
        let hint = envelope["error"]["hint"].as_str().unwrap_or_default();
        hint.lines().next().unwrap_or_default().to_owned()
    }
}

impl std::fmt::Display for Out {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:?}: exit {}\n--- stdout ---\n{}--- stderr ---\n{}",
            self.line, self.exit, self.stdout, self.stderr
        )
    }
}

/// Runs `bin` (the binary by either name) in `dir` with `words`, `env` set and none of this
/// process's `SWARMOPS_*` variables.
fn run_at(bin: &str, dir: &Path, words: &[&str], env: &[(&str, &str)]) -> Out {
    let mut command = Command::new(bin);
    for name in support::swarmops_vars() {
        command.env_remove(name);
    }
    command
        .args(words)
        .envs(env.iter().copied())
        .current_dir(dir)
        .stdin(Stdio::null());
    let done = command.output().expect("run swarmops");
    Out {
        line: words.iter().map(|word| (*word).to_owned()).collect(),
        exit: done.status.code().expect("swarmops exited"),
        stdout: String::from_utf8_lossy(&done.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&done.stderr).into_owned(),
    }
}

/// The name `bin` writes for itself in a hint.
fn name_of(bin: &str) -> &'static str {
    if bin == SO { "so" } else { "swarmops" }
}

/// `value` with every time key's value, and `meta.repo`, masked.
fn masked(mut value: Value) -> Value {
    fn walk(value: &mut Value) {
        match value {
            Value::Object(map) => {
                for (key, inner) in map.iter_mut() {
                    if TIME_KEYS.contains(&key.as_str()) {
                        *inner = Value::Null;
                        continue;
                    }
                    walk(inner);
                }
            }
            Value::Array(items) => items.iter_mut().for_each(walk),
            _ => {}
        }
    }
    walk(&mut value);
    value["meta"]["repo"] = Value::Null;
    value
}

/// Every alias of §3, where it is typed, and the `op` of its target. In order: the lines that
/// read the edit come before the commit that takes it.
const ALIASES: &[(&[&str], At, &str)] = &[
    (&["status"], At::Root, "ws_status"),
    (&["status", "-s", "--json"], At::Below, "ws_status"),
    (&["status", "a.txt"], At::Root, "ws_status"),
    (&["reflog", "main"], At::Root, "ref_log"),
    (&["reflog", "show", "main"], At::Elsewhere, "ref_log"),
    (&["worktree", "list"], At::Root, "ws_list"),
    (&["tag"], At::Root, "ref_list"),
    (&["tag", "-l"], At::Elsewhere, "ref_list"),
    (&["remote"], At::Root, "remote_list"),
    (&["remote", "-v"], At::Root, "remote_list"),
    (&["remote", "rm", "origin"], At::Root, "remote_remove"),
    (&["fetch"], At::Root, "fetch"),
    (&["push"], At::Root, "push"),
    (&["log", "--oneline", "--json"], At::Root, "log"),
    (&["log", "-3"], At::Below, "log"),
    (
        &["diff", "--stat", "--json", "main~1", "main"],
        At::Root,
        "diff",
    ),
    (&["diff"], At::Below, "ws_diff"),
    (&["diff", "--stat", "--json"], At::Root, "ws_diff"),
    (&["diff", "--path", "a.txt"], At::Root, "ws_diff"),
    (&["commit", "-m", "x"], At::Root, "ws_commit"),
    (&["rebase"], At::Root, "ws_rebase"),
    (&["restore", "a.txt"], At::Root, "ws_discard"),
];

/// Every hinted line, where it is typed, and the first line of its hint, `{so}` the program's
/// name: the 36 words, the 4 bare forms, the 4 lines below the root, `diff` with one revision.
const HINTED: &[(&[&str], At, &str)] = &[
    (
        &["add", "a.txt"],
        At::Root,
        "git add: there is no staging area; {so} commit -m MSG [PATH]... in a workspace takes \
         every change, or the named paths",
    ),
    (
        &["rm", "a.txt"],
        At::Root,
        "git rm: delete the file in the workspace; {so} commit -m MSG records it",
    ),
    (
        &["mv", "a.txt", "b.txt"],
        At::Root,
        "git mv: move the file in the workspace; {so} commit -m MSG records it",
    ),
    (
        &["branch"],
        At::Elsewhere,
        "git branch: {so} ref list lists refs; {so} ref set NAME REV --create makes one; {so} ref \
         delete NAME --expect ID removes one",
    ),
    (
        &["switch", "main"],
        At::Root,
        "git switch: a workspace does not switch; {so} ws create NAME --base REV makes another",
    ),
    (
        &["checkout", "main"],
        At::Root,
        "git checkout: {so} ws create NAME --base REV for another tree; {so} restore PATH... to \
         drop edits",
    ),
    (
        &["restore"],
        At::Root,
        "git restore: name the paths, {so} restore PATH...; {so} ws discard drops every edit",
    ),
    (
        &["merge", "main"],
        At::Root,
        "git merge: SwarmOps lands; {so} ws land from a workspace, or {so} land HEAD",
    ),
    (
        &["pull"],
        At::Root,
        "git pull: {so} sync REMOTE --no-push brings the remote into main; {so} rebase in a \
         workspace follows it",
    ),
    (
        &["stash"],
        At::Root,
        "git stash: {so} ws snapshot records the edits and leaves the files; nothing pops",
    ),
    (
        &["reset", "--hard"],
        At::Root,
        "git reset: {so} ws discard [PATH]... drops edits; {so} op undo OP takes an operation \
         back; {so} ref set moves a ref",
    ),
    (
        &["cherry-pick", "main"],
        At::Root,
        "git cherry-pick: not built; {so} land HEAD --base BASE lands one change's range",
    ),
    (
        &["blame", "a.txt"],
        At::Root,
        "git blame: not built yet (M7); {so} history REV:PATH lists the changesets that touched \
         a path",
    ),
    (
        &["grep", "x"],
        At::Root,
        "git grep: search the files of a workspace; {so} cat REV:PATH reads one at a revision",
    ),
    (
        &["config", "user.name"],
        At::Elsewhere,
        "git config: identity is --as or SWARMOPS_AGENT ({so} whoami); the store is --repo or \
         SWARMOPS_REPO; remotes are {so} remote",
    ),
    (
        &["tag", "v1"],
        At::Root,
        "git tag: {so} tag lists; {so} ref set tags/NAME REV --create makes one; {so} ref delete \
         tags/NAME --expect ID removes one",
    ),
    (
        &["worktree", "add", "d"],
        At::Root,
        "git worktree: {so} ws create NAME [--path DIR] [--base REV]; {so} ws list; {so} ws \
         delete NAME",
    ),
    (
        &["reflog"],
        At::Root,
        "git reflog: {so} reflog NAME lists a ref's moves; {so} op log every operation; nothing \
         expires",
    ),
    (
        &["rev-parse", "main"],
        At::Root,
        "git rev-parse: {so} resolve REV gives the full id; {so} ws info a workspace's head and \
         path",
    ),
    (
        &["rev-list", "main"],
        At::Root,
        "git rev-list: {so} log REV -n N; {so} ancestry FROM TO",
    ),
    (&["ls-files"], At::Root, "git ls-files: {so} ls REV[:PATH]"),
    (
        &["ls-tree", "main"],
        At::Root,
        "git ls-tree: {so} ls REV[:PATH] --depth N",
    ),
    (
        &["cat-file", "-p", "main"],
        At::Root,
        "git cat-file: {so} cat REV:PATH; {so} blob get ID; {so} blob has ID",
    ),
    (
        &["hash-object", "a.txt"],
        At::Root,
        "git hash-object: {so} blob put FILE",
    ),
    (
        &["describe"],
        At::Root,
        "git describe: not built; {so} tag lists the tags",
    ),
    (
        &["for-each-ref"],
        At::Root,
        "git for-each-ref: {so} ref list [--prefix P]",
    ),
    (&["show-ref"], At::Root, "git show-ref: {so} ref list"),
    (
        &["symbolic-ref", "HEAD"],
        At::Root,
        "git symbolic-ref: there are no symbolic refs; {so} ws info gives a workspace's head and \
         target",
    ),
    (
        &["update-ref", "main", "main"],
        At::Root,
        "git update-ref: {so} ref set NAME REV --expect ID, or --create; {so} ref delete; {so} \
         ref txn",
    ),
    (
        &["name-rev", "main"],
        At::Root,
        "git name-rev: not built; {so} ref list gives each ref's changeset",
    ),
    (
        &["ls-remote"],
        At::Root,
        "git ls-remote: {so} fetch REMOTE, then {so} ref list --prefix remote/REMOTE/; {so} sync \
         status REMOTE",
    ),
    (
        &["diff-tree", "main"],
        At::Root,
        "git diff-tree: {so} diff FROM TO --format names; {so} show REV",
    ),
    (
        &["clean", "-f"],
        At::Root,
        "git clean: {so} ws discard PATH... drops the named paths' edits, new files included; \
         without a path, every edit",
    ),
    (
        &["archive", "main"],
        At::Root,
        "git archive: git init --bare PATH, {so} git export PATH, then git archive there",
    ),
    (&["bisect", "start"], At::Root, "git bisect: not built"),
    (
        &["shortlog"],
        At::Root,
        "git shortlog: {so} log, counted by author_agent",
    ),
    // The bare forms, refused for what they lack.
    (
        &["diff"],
        At::Elsewhere,
        "in a workspace this is {so} ws diff; {so} ws create NAME makes one",
    ),
    (
        &["rebase"],
        At::Elsewhere,
        "in a workspace this is {so} ws rebase; {so} ws create NAME makes one",
    ),
    (
        &["commit", "-m", "x"],
        At::Elsewhere,
        "{so} commit -m MSG [PATH]... in a workspace commits its changes; {so} ws create NAME \
         makes one",
    ),
    (
        &["clone", URL],
        At::Elsewhere,
        "name the directory: {so} clone URL DIR",
    ),
    // A path typed below the root.
    (
        &["restore", "a.txt"],
        At::Below,
        "paths are read from the workspace's root, not this directory: {so} ws discard \
         ROOT/RELATIVE/PATH",
    ),
    (
        &["commit", "-m", "x", "a.txt"],
        At::Below,
        "paths are read from the workspace's root, not this directory: {so} ws commit -m MSG \
         ROOT/RELATIVE/PATH",
    ),
    (
        &["status", "a.txt"],
        At::Below,
        "paths are read from the workspace's root, not this directory: {so} ws status \
         ROOT/RELATIVE/PATH",
    ),
    (
        &["diff", "--path", "a.txt"],
        At::Below,
        "paths are read from the workspace's root, not this directory: {so} ws diff \
         ROOT/RELATIVE/PATH",
    ),
    // One revision, in a workspace.
    (
        &["diff", "main"],
        At::Root,
        "a path is {so} diff --path P; two revisions are {so} diff FROM TO",
    ),
];

/// Each alias runs its target in the fixture: the envelope's `op` is the target's, and the line
/// is not refused as a usage error. `clone` runs with no identity (refused before any
/// connection), `fetch`, `push` and `remote rm` on a store with no remote.
#[test]
fn every_alias_runs_its_target() {
    let fx = Fixture::new();
    write(&fx.ws, "a.txt", "edited\n");
    write(&fx.ws, "new.txt", "new\n");

    for &(line, at, op) in ALIASES {
        let out = fx.run_as(BIN, at, line);
        let envelope = out.envelope();
        assert_eq!(envelope["op"], op, "{out}");
        assert_ne!(envelope["error"]["code"], "E_INVALID_ARG", "{out}");
    }

    // `clone URL DIR`: the identity is asked for before any connection.
    let clone = fx.run(BIN, At::Elsewhere, &["clone", URL, "copy"]);
    let envelope = clone.envelope();
    assert_eq!(
        (clone.exit, &envelope["op"], &envelope["error"]["code"]),
        (
            5,
            &Value::from("clone_repo"),
            &Value::from("E_IDENTITY_REQUIRED")
        ),
        "{clone}"
    );
}

/// Outside a workspace the bare forms of `diff`, `commit` and `rebase` stay their commands'
/// refusals; `commit` with no message stays `commit`'s inside a workspace too.
#[test]
fn the_bare_forms_stay_refused_outside_a_workspace() {
    let fx = Fixture::new();
    let bare: [(&[&str], At, &str); 4] = [
        (&["diff"], At::Elsewhere, "diff"),
        (&["commit", "-m", "x"], At::Elsewhere, "commit"),
        (&["rebase"], At::Elsewhere, "rebase"),
        (&["commit"], At::Root, "commit"),
    ];
    for (line, at, op) in bare {
        let out = fx.run_as(BIN, at, line);
        let envelope = out.envelope();
        assert_eq!(
            (out.exit, &envelope["op"], &envelope["error"]["code"]),
            (2, &Value::from(op), &Value::from("E_INVALID_ARG")),
            "{out}"
        );
    }
}

/// Each hinted line is refused (exit 2, `E_INVALID_ARG`) with its line first in the hint, by
/// either binary, the program named as it was run.
#[test]
fn every_hinted_word_is_refused_with_its_line() {
    let fx = Fixture::new();
    assert_eq!(HINTED.len(), 45);
    for bin in [SO, BIN] {
        for &(line, at, first) in HINTED {
            let out = fx.run_as(bin, at, line);
            let envelope = out.envelope();
            assert_eq!(
                (out.exit, &envelope["error"]["code"]),
                (2, &Value::from("E_INVALID_ARG")),
                "{out}"
            );
            assert_eq!(
                out.first_hint(),
                first.replace("{so}", name_of(bin)),
                "{out}"
            );
        }
    }
}

/// The check can fail: a word without a row, and a git word below a command, get clap's own
/// hint, no git line.
#[test]
fn a_word_without_a_row_gets_no_line() {
    let fx = Fixture::new();
    let lines: [&[&str]; 5] = [
        &["frobnicate"],
        &["am"],
        &["ws", "add", "x"],
        &["remote", "config"],
        &["git", "archive"],
    ];
    for line in lines {
        let out = fx.run_as(BIN, At::Root, line);
        assert_eq!(out.exit, 2, "{out}");
        let first = out.first_hint();
        assert!(!first.starts_with("git "), "{out}");
    }
}

/// `so` and `swarmops` answer an alias alike, and a hinted word alike but for the name the
/// hint writes.
#[test]
fn so_and_swarmops_answer_alike() {
    let fx = Fixture::new();
    let lines: [(&[&str], At); 6] = [
        (&["status"], At::Root),
        (&["log", "-3"], At::Root),
        (&["tag"], At::Elsewhere),
        (&["reflog", "main"], At::Below),
        (&["add", "a.txt"], At::Root),
        (&["diff"], At::Elsewhere),
    ];
    for (line, at) in lines {
        let [by_so, by_swarmops] = [SO, BIN].map(|bin| {
            let out = fx.run_as(bin, at, line);
            let mut envelope = masked(out.envelope());
            let hint = envelope["error"]["hint"].as_str().map(|hint| {
                hint.split(' ')
                    .map(|word| match word {
                        "so" => "swarmops",
                        "(so" => "(swarmops",
                        word => word,
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            });
            if let Some(hint) = hint {
                envelope["error"]["hint"] = Value::from(hint);
            }
            envelope
        });
        assert_eq!(by_so, by_swarmops, "{line:?}");
    }
}

/// A line that parses without the layer prints what it printed, wherever it is typed: help the
/// same text, and an envelope (times and the store's path masked) the same inside a workspace as
/// outside, each on a store of the same state.
#[test]
fn a_valid_line_prints_what_it_printed() {
    let fx = Fixture::new();

    // 1. Help, at the root, below it and outside.
    let helps: [&[&str]; 5] = [
        &["diff", "--help"],
        &["commit", "--help"],
        &["rebase", "-h"],
        &["remote", "--help"],
        &["remote", "-h"],
    ];
    for line in helps {
        let outs = [At::Root, At::Below, At::Elsewhere].map(|at| {
            let out = run_at(BIN, &fx.dir(at), line, &[]);
            assert_eq!(out.exit, 0, "{out}");
            out.stdout
        });
        assert!(
            outs[0].contains("Usage: swarmops "),
            "{line:?}: {}",
            outs[0]
        );
        assert_eq!(outs[0], outs[1], "{line:?}");
        assert_eq!(outs[0], outs[2], "{line:?}");
    }

    // 2. Envelopes: inside on the workspace's store, outside on a copy of it.
    let copy = support::Store::copy_of(&fx.lab.store);
    let changes = fx.lab.store.changes("x", "c.txt", "c\n");
    let lines: [&[&str]; 5] = [
        &[
            "commit",
            "-m",
            "x",
            "--changes",
            &changes,
            "--parent",
            "main",
        ],
        &["diff", "main~1", "main"],
        &["rebase", "main", "--onto", "main~1"],
        &["fetch", "origin"],
        &["clone", URL, "--into", "copy"],
    ];
    let now = [(swarmops_ops::NOW_ENV, NOW)];
    for line in lines {
        let as_agent = line[0] != "clone";
        let mut inside = Vec::new();
        let mut outside = vec!["--repo", copy.path()];
        if as_agent {
            inside.extend(["--as", AGENT]);
            outside.extend(["--as", AGENT]);
        }
        inside.extend(line);
        outside.extend(line);
        let by_ws = run_at(BIN, &fx.ws, &inside, &now);
        let by_store = run_at(BIN, copy.tmp(), &outside, &now);
        assert_eq!(by_ws.exit, by_store.exit, "{by_ws}\n{by_store}");
        assert_eq!(
            masked(by_ws.envelope()),
            masked(by_store.envelope()),
            "{by_ws}\n{by_store}"
        );
    }
}

/// Below the root a line that carries a path is refused, its hint naming the root-relative form,
/// and no file changes: not the root's `a.txt`, not `sub/a.txt`.
#[test]
fn a_path_below_the_root_is_refused_and_no_file_changes() {
    let fx = Fixture::new();
    write(&fx.ws, "a.txt", "edited\n");
    write(&fx.ws, "sub/a.txt", "new below\n");
    let files = || ["a.txt", "sub/a.txt"].map(|path| fs::read(fx.ws.join(path)).unwrap());
    let before = files();

    let below: [(&[&str], &str); 4] = [
        (&["restore", "a.txt"], "ws discard"),
        (&["commit", "-m", "x", "a.txt"], "ws commit -m MSG"),
        (&["status", "a.txt"], "ws status"),
        (&["diff", "--path", "a.txt"], "ws diff"),
    ];
    for (line, form) in below {
        let out = fx.run_as(SO, At::Below, line);
        let envelope = out.envelope();
        assert_eq!(
            (out.exit, &envelope["error"]["code"]),
            (2, &Value::from("E_INVALID_ARG")),
            "{out}"
        );
        let first = format!(
            "paths are read from the workspace's root, not this directory: so {form} \
             ROOT/RELATIVE/PATH"
        );
        assert_eq!(out.first_hint(), first, "{out}");
    }
    assert_eq!(files(), before);
}

/// What the design states as differences from git: `restore` of a file never committed removes
/// it; `log` in a workspace with a commit starts from `main`; `commit -m` records a new file.
#[test]
fn the_stated_differences_hold() {
    let fx = Fixture::new();
    let main = fx.lab.so(&["resolve", "main"]).ok()["id"]
        .as_str()
        .unwrap()
        .to_owned();

    // 1. A new file, committed by `commit -m`.
    write(&fx.ws, "new.txt", "new\n");
    let commit = fx.run_as(SO, At::Root, &["commit", "-m", "x"]);
    assert_eq!(commit.exit, 0, "{commit}");
    let changeset = commit.envelope()["result"]["changeset"]
        .as_str()
        .unwrap()
        .to_owned();
    let shown = fx.lab.so(&["show", &changeset]).ok();
    assert!(shown.to_string().contains("new.txt"), "{shown}");

    // 2. `log` starts from `main`, not the workspace's head.
    let log = fx.run_as(SO, At::Root, &["log", "-1"]);
    assert_eq!(log.exit, 0, "{log}");
    let entries = log.envelope()["result"]["entries"].clone();
    assert_eq!(entries[0]["id"], Value::from(main), "{log}");

    // 3. `restore` of a file never committed removes it.
    write(&fx.ws, "fresh.txt", "fresh\n");
    let restore = fx.run_as(SO, At::Root, &["restore", "fresh.txt"]);
    assert_eq!(restore.exit, 0, "{restore}");
    assert!(!fx.ws.join("fresh.txt").exists());
}

/// A git hint writes the name the program was run by: `swarmops` or `so`.
#[test]
fn a_hint_names_the_program_it_was_run_as() {
    let fx = Fixture::new();
    for bin in [SO, BIN] {
        let out = fx.run_as(bin, At::Root, &["add", "x"]);
        let name = name_of(bin);
        let first = out.first_hint();
        assert!(
            first.starts_with(&format!(
                "git add: there is no staging area; {name} commit "
            )),
            "{out}"
        );
    }
}
