//! The `swarmops` binary as agents script it (operations spec §0, §1, §8): every command prints
//! exactly one JSON envelope on stdout and exits with the code of its error, and golden
//! snapshots pin the envelopes.
//!
//! Each test spawns the real binary (`CARGO_BIN_EXE_swarmops`) against stores in a temp dir of
//! its own, with every `SWARMOPS_*` variable it does not set removed, so the developer's
//! environment never leaks in. A test records each command it runs as one step of a
//! transcript (its arguments, the variables it set, its exit code and what it printed) and
//! snapshots the transcript at the end, with what changes from run to run replaced by stable
//! names: ids as `[c1]` or `[id3]`, the temp dir as `[tmp]`, times and `elapsed_ms` by their key.
//! Every step is checked against the envelope contract as it runs (see [`check`]). The
//! `--human` test snapshots text instead: each command, its exit code and what it printed.

use std::fmt;
use std::fmt::Write as _;
use std::fs;
use std::ops::Index;

use assert_cmd::Command;
use camino::Utf8PathBuf;
use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::ser::{SerializeMap, SerializeSeq, Serializer};
use serde::{Deserialize, Serialize};
use serde_json::{Number, Value, json};
use swarmops_api::ErrorCode;
use swarmops_core::{ObjectKind, hash_object};
use swarmops_store::{Layout, ORDER_CHECK_ENV};
use tempfile::TempDir;

use crate::support;

/// The agent most mutations act as.
const AGENT: &str = "agent/worker-17";

/// The keys whose numbers are times or durations, replaced by `[<key>]` in snapshots.
const TIME_KEYS: &[&str] = &["elapsed_ms", "created_at", "secs"];

/// A store path under `/mnt/`, where WSL mounts Windows drives: `init` and every open refuse it
/// by its name, so it need not exist, and nothing is made there.
const MNT_STORE: &str = "/mnt/swarmops-cli-test/store";

/// One `--human` run: the variables set, the arguments, the exit code it must give.
type HumanRun<'a> = (&'a [(&'a str, &'a str)], &'a [&'a str], i32);

// ---------------------------------------------------------------------------------------------
// The tests.
// ---------------------------------------------------------------------------------------------

#[test]
fn init_info_whoami_golden() {
    let mut s = Session::new();
    let store = s.store();

    // init names the repository after its directory, unless --name says otherwise.
    let init = s.run(&["init", &store]);
    assert_eq!(init.exit, 0);
    assert_eq!(init["result"]["name"], "store");
    assert_eq!(init["result"]["changesets"], 0);
    assert_eq!(
        init["meta"]["repo"],
        store.as_str(),
        "meta.repo is the path as given"
    );
    s.name_at(&init, "/result/repo_id", "repo_id");
    let acme = format!("{}/acme-store", s.tmp());
    let named = s.run(&["init", &acme, "--name", "acme"]);
    assert_eq!(named["result"]["name"], "acme");
    s.name_at(&named, "/result/repo_id", "repo_id:acme");

    // info reads the store --repo names, else SWARMOPS_REPO's; --repo wins.
    let info = s.run(&["--repo", &store, "info"]);
    assert_eq!(info["result"], init["result"]);
    let from_env = s.run_env(&[("SWARMOPS_REPO", &acme)], &["info"]);
    assert_eq!(from_env["result"], named["result"]);
    assert_eq!(from_env["meta"]["repo"], acme.as_str());
    let both = s.run_env(&[("SWARMOPS_REPO", &acme)], &["info", "--repo", &store]);
    assert_eq!(
        both["result"], init["result"],
        "--repo wins, after the subcommand too"
    );

    // A second init there is E_EXISTS, exit 1.
    let again = s.run(&["init", &store]);
    assert_eq!((again.exit, again.code()), (1, "E_EXISTS"));

    // whoami: anonymous without an identity; --as, else SWARMOPS_AGENT, with SWARMOPS_RUN_ID.
    let anonymous = s.run(&["whoami"]);
    assert_eq!(anonymous["result"], json!({"auth": "anonymous"}));
    assert!(
        anonymous["meta"].get("repo").is_none(),
        "whoami uses no store"
    );
    let flag = s.run(&["--as", AGENT, "whoami"]);
    assert_eq!(flag["result"], json!({"agent": AGENT, "auth": "trust"}));
    let env = [
        ("SWARMOPS_AGENT", "human/purple"),
        ("SWARMOPS_RUN_ID", "run-7"),
    ];
    let from_env = s.run_env(&env, &["whoami"]);
    assert_eq!(
        from_env["result"],
        json!({"agent": "human/purple", "run_id": "run-7", "auth": "trust"})
    );
    let flag_wins = s.run_env(&env, &["whoami", "--as", AGENT]);
    assert_eq!(flag_wins["result"]["agent"], AGENT);
    // A name that breaks the grammar is E_INVALID_ARG, said of where it came from.
    let bad = s.run(&["--as", "Not An Agent", "whoami"]);
    assert_eq!((bad.exit, bad.code()), (2, "E_INVALID_ARG"));
    assert_eq!(bad["error"]["details"]["source"], "--as");
    let bad_env = s.run_env(&[("SWARMOPS_AGENT", "nobody")], &["whoami"]);
    assert_eq!(bad_env["error"]["details"]["source"], "SWARMOPS_AGENT");

    insta::assert_json_snapshot!(s.transcript());
}

#[test]
fn commit_cat_ls_show_log_golden() {
    let mut s = Session::new();
    let store = s.store();
    let init = s.run(&["init", &store, "--name", "acme"]);
    s.name_at(&init, "/result/repo_id", "repo_id");
    let [c1, c2] = commit_twice(&mut s, &store);
    cat_at(&mut s, &store, &c1);
    ls_show_log(&mut s, &store, [&c1, &c2]);
    insta::assert_json_snapshot!(s.transcript());
}

/// Two commits, main moved to each: the first from a file with every kind of content, the
/// second from stdin on main by default. Their ids.
fn commit_twice(s: &mut Session, store: &str) -> [String; 2] {
    let notes = s.file("notes.txt", "kept on the client\n");
    let changes = s.file(
        "changes.json",
        &json!([
            {"path": "README.md", "op": "put", "content": {"inline": "# acme\n"}},
            {"path": "src/main.rs", "op": "put", "content": {"inline": "fn main() {}\n"}},
            {"path": "bin/run", "op": "put", "mode": "exec",
             "content": {"inline": "#!/bin/sh\necho hi\n"}},
            {"path": "logo.bin", "op": "put", "content": {"base64": "AAEC/w=="}},
            {"path": "notes.txt", "op": "put", "content": {"file": notes}},
            {"path": "latest", "op": "put", "mode": "symlink", "content": {"inline": "README.md"}},
        ])
        .to_string(),
    );
    let mutate = ["--repo", store, "--as", AGENT];
    let first = s.run_env(
        &[("SWARMOPS_RUN_ID", "run-1")],
        &[
            &mutate[..],
            &[
                "commit",
                "-m",
                "first commit\n\nwith a body",
                "--changes",
                &changes,
            ],
        ]
        .concat(),
    );
    assert_eq!(first.exit, 0);
    assert!(first["result"].get("parent").is_none(), "a root commit");
    assert_eq!(first["result"]["files_changed"], 6);
    let c1 = s.name_at(&first, "/result/changeset", "c1");
    s.name_at(&first, "/result/root", "root1");
    s.run(&[&mutate[..], &["ref", "set", "main", &c1, "--create"]].concat());

    let stdin = json!([
        {"path": "README.md", "op": "put", "content": {"inline": "# acme\n\nversion 2\n"}},
        {"path": "notes.txt", "op": "delete"},
    ])
    .to_string();
    let second = s.run_stdin(
        &stdin,
        &[&mutate[..], &["commit", "-m", "second", "--changes", "-"]].concat(),
    );
    assert_eq!(second["result"]["parent"], c1.as_str());
    assert_eq!(second["meta"]["resolved"], json!({"main": c1}));
    let c2 = s.name_at(&second, "/result/changeset", "c2");
    s.name_at(&second, "/result/root", "root2");
    let moved = s.run(&[&mutate[..], &["ref", "set", "main", &c2, "--expect", &c1]].concat());
    assert_eq!(moved.exit, 0);
    [c1, c2]
}

/// cat: text, base64, a file at an older revision, a symlink, a truncated read and the read on
/// from its `next_offset`, a range of lines, and `--offset` with `--lines` or a range that is no
/// A-B, which are usage errors. A rev is split from its path at the first ':', so a path may hold
/// more.
fn cat_at(s: &mut Session, store: &str, c1: &str) {
    let readme = s.run(&["--repo", store, "cat", "main:README.md"]);
    assert_eq!(readme["result"]["content"], "# acme\n\nversion 2\n");
    assert_eq!(readme["result"]["encoding"], "utf8");
    let prefix = &c1[..12];
    s.name(prefix, "c1:12");
    let logo = s.run(&["--repo", store, "cat", &format!("{prefix}:logo.bin")]);
    assert_eq!(logo["result"]["encoding"], "base64");
    assert_eq!(logo["result"]["content"], "AAEC/w==");
    assert_eq!(logo["meta"]["resolved"], json!({prefix: c1}));
    let old_notes = s.run(&["--repo", store, "cat", "main~1:notes.txt"]);
    assert_eq!(old_notes["result"]["content"], "kept on the client\n");
    assert_eq!(old_notes["meta"]["resolved"], json!({"main~1": c1}));
    let link = s.run(&["--repo", store, "cat", "main:latest"]);
    assert_eq!(link["result"]["kind"], "symlink");
    let cut = s.run(&[
        "--repo",
        store,
        "cat",
        "main:src/main.rs",
        "--max-bytes",
        "4",
    ]);
    assert_eq!(cut["result"]["content"], "fn m");
    assert_eq!(cut["result"]["truncated"], true);
    assert_eq!(cut["result"]["next_offset"], 4);
    let rest = s.run(&["--repo", store, "cat", "main:src/main.rs", "--offset", "4"]);
    assert_eq!(rest["result"]["content"], "ain() {}\n");
    assert_eq!(rest["result"]["offset"], 4);
    let third = s.run(&["--repo", store, "cat", "main:README.md", "--lines", "3-3"]);
    assert_eq!(third["result"]["content"], "version 2\n");
    assert_eq!(third["result"]["lines"], json!({"first": 3, "last": 3}));
    for refused in [&["--offset", "1", "--lines", "1-2"][..], &["--lines", "3"]] {
        let args = [&["--repo", store, "cat", "main:README.md"][..], refused].concat();
        let usage = s.run(&args);
        assert_eq!(
            (usage.exit, usage.code()),
            (2, "E_INVALID_ARG"),
            "{refused:?}"
        );
    }
    let colon = s.run(&["--repo", store, "cat", "main:no:such:file"]);
    assert_eq!(colon["error"]["details"]["path"], "no:such:file");
}

/// ls (the root without a path or with an empty one, a directory with or without a trailing
/// `/`, pages, `--depth` with each entry's path, and its pages), show (by ref and by prefix) and
/// log (from main, a page at a time).
fn ls_show_log(s: &mut Session, store: &str, [c1, c2]: [&str; 2]) {
    let root = s.run(&["--repo", store, "ls", "main"]);
    let names: Vec<&str> = root["result"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["README.md", "bin", "latest", "logo.bin", "src"]);
    let empty_path = s.run(&["--repo", store, "ls", "main:"]);
    assert_eq!(empty_path["result"], root["result"]);
    let bin = s.run(&["--repo", store, "ls", "main:bin"]);
    let slash = s.run(&["--repo", store, "ls", "main:bin/"]);
    assert_eq!(slash["result"], bin["result"], "a directory may end in /");
    let page = s.run(&["--repo", store, "ls", "main", "--limit", "2"]);
    assert_eq!(page["result"]["truncated"], true);
    assert_eq!(page["result"]["cursor"], "bin");
    let rest = s.run(&[
        "--repo", store, "ls", "main", "--limit", "2", "--cursor", "bin",
    ]);
    assert_eq!(rest["result"]["entries"][0]["name"], "latest");
    let deep = s.run(&["--repo", store, "ls", "main", "--depth", "2"]);
    let paths: Vec<&str> = deep["result"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["path"].as_str().unwrap())
        .collect();
    assert_eq!(
        paths,
        [
            "README.md",
            "bin",
            "bin/run",
            "latest",
            "logo.bin",
            "src",
            "src/main.rs"
        ]
    );
    let deep_page = s.run(&[
        "--repo", store, "ls", "main", "--depth", "2", "--limit", "2", "--cursor", "bin",
    ]);
    assert_eq!(deep_page["result"]["entries"][0]["path"], "bin/run");
    assert_eq!(deep_page["result"]["cursor"], "latest");

    let shown = s.run(&["--repo", store, "show", "main"]);
    assert_eq!(shown["result"]["id"], c2);
    assert_eq!(shown["result"]["parents"], json!([c1]));
    let shown_first = s.run(&["--repo", store, "show", &c1[..12], "--max-changes", "2"]);
    assert_eq!(shown_first["result"]["author"]["run_id"], "run-1");
    assert_eq!(shown_first["result"]["changes_truncated"], true);

    let log = s.run(&["--repo", store, "log"]);
    assert_eq!(log["result"]["entries"][0]["id"], c2);
    assert_eq!(log["result"]["entries"][1]["title"], "first commit");
    let one = s.run(&["--repo", store, "log", "-n", "1"]);
    assert_eq!(one["result"]["cursor"], c2);
    let older = s.run(&["--repo", store, "log", "--cursor", c2]);
    assert_eq!(older["result"]["entries"][0]["id"], c1);
}

/// `resolve`: the full id of the changeset a revision names and its kind, `ref` for exactly a
/// ref's name and `changeset` for a `~N` walk, a prefix or a full id, with the revision in
/// `meta.resolved`; a revision that names nothing is `E_REV_NOT_FOUND` (exit 6), and one that is
/// no revision `E_INVALID_ARG` (exit 2).
#[test]
fn resolve_golden() {
    let mut s = Session::new();
    let store = s.store();
    let init = s.run(&["init", &store, "--name", "acme"]);
    s.name_at(&init, "/result/repo_id", "repo_id");
    let [c1, c2] = commit_twice(&mut s, &store);
    let resolve = |s: &mut Session, rev: &str| s.run(&["--repo", &store, "resolve", rev]);

    let main = resolve(&mut s, "main");
    assert_eq!(main["result"], json!({"id": c2, "kind": "ref"}));
    assert_eq!(main["meta"]["resolved"], json!({"main": c2}));
    let back = resolve(&mut s, "main~1");
    assert_eq!(back["result"], json!({"id": c1, "kind": "changeset"}));
    let prefix = &c1[..12];
    s.name(prefix, "c1:12");
    let by_prefix = resolve(&mut s, prefix);
    assert_eq!(by_prefix["result"], json!({"id": c1, "kind": "changeset"}));
    assert_eq!(by_prefix["meta"]["resolved"], json!({prefix: c1}));
    let full = resolve(&mut s, &c2);
    assert_eq!(full["result"], json!({"id": c2, "kind": "changeset"}));

    let missing = resolve(&mut s, "release/none");
    assert_eq!((missing.exit, missing.code()), (6, "E_REV_NOT_FOUND"));
    let past_root = resolve(&mut s, "main~2");
    assert_eq!((past_root.exit, past_root.code()), (6, "E_REV_NOT_FOUND"));
    let bad = resolve(&mut s, "HEAD");
    assert_eq!((bad.exit, bad.code()), (2, "E_INVALID_ARG"));
    insta::assert_json_snapshot!(s.transcript());
}

/// `history` and `log --path`: a file's history and a directory's, a page at a time with its
/// cursor (a directory may end in `/`); the log of the changesets that change some paths; and the
/// errors: a path nothing ever touched (`E_PATH_NOT_FOUND`), a target without a path and a limit
/// of 0 (`E_INVALID_ARG`).
#[test]
fn history_and_log_path_golden() {
    let mut s = Session::new();
    let store = s.store();
    let init = s.run(&["init", &store, "--name", "acme"]);
    s.name_at(&init, "/result/repo_id", "repo_id");
    let c1 = s.commit(&store, None, "src/lib.rs", "pub fn f() {}\n", "c1");
    let c2 = s.commit(&store, Some(&c1), "README.md", "# acme\n", "c2");
    let c3 = s.commit(
        &store,
        Some(&c2),
        "src/lib.rs",
        "pub fn f() -> u8 { 1 }\n",
        "c3",
    );
    let c4 = s.commit(&store, Some(&c3), "src/main.rs", "fn main() {}\n", "c4");
    let main = [
        "--repo", &store, "--as", AGENT, "ref", "set", "main", &c4, "--create",
    ];
    assert_eq!(s.run(&main).exit, 0);
    let ids = |reply: &Reply, key: &str| -> Vec<String> {
        let entries = reply["result"]["entries"].as_array().unwrap();
        entries
            .iter()
            .map(|entry| entry[key].as_str().unwrap().to_owned())
            .collect()
    };

    let file = s.run(&["--repo", &store, "history", "main:src/lib.rs"]);
    assert_eq!(ids(&file, "changeset"), [c3.as_str(), c1.as_str()]);
    assert_eq!(file["result"]["entries"][0]["kind"], "file");
    assert_eq!(file["meta"]["resolved"], json!({"main": c4}));
    let first = s.run(&["--repo", &store, "history", "main:src", "-n", "1"]);
    assert_eq!(ids(&first, "changeset"), [c4.as_str()]);
    assert_eq!(first["result"]["entries"][0]["kind"], "dir");
    assert_eq!(first["result"]["cursor"], c4.as_str());
    let second = s.run(&[
        "--repo",
        &store,
        "history",
        "main:src/",
        "-n",
        "1",
        "--cursor",
        &c4,
    ]);
    assert_eq!(ids(&second, "changeset"), [c3.as_str()]);
    assert_eq!(second["result"]["path"], "src");
    let last = s.run(&["--repo", &store, "history", "main:src", "--cursor", &c3]);
    assert_eq!(ids(&last, "changeset"), [c1.as_str()]);
    assert_eq!(last["result"]["truncated"], false);

    let logged = s.run(&["--repo", &store, "log", "--path", "src"]);
    assert_eq!(ids(&logged, "id"), [c4.as_str(), c3.as_str(), c1.as_str()]);
    let two = ["--path", "README.md", "--path", "src/main.rs"];
    let paged = s.run(&[&["--repo", &store, "log", "-n", "1"][..], &two[..]].concat());
    assert_eq!(ids(&paged, "id"), [c4.as_str()]);
    assert_eq!(paged["result"]["cursor"], c4.as_str());

    let nope = s.run(&["--repo", &store, "history", "main:nope"]);
    let code = ErrorCode::PathNotFound;
    assert_eq!((nope.exit, nope.code()), (code.exit(), code.as_str()));
    let no_path = s.run(&["--repo", &store, "history", "main"]);
    assert_eq!((no_path.exit, no_path.code()), (2, "E_INVALID_ARG"));
    let zero = s.run(&["--repo", &store, "history", "main:src", "-n", "0"]);
    assert_eq!((zero.exit, zero.code()), (2, "E_INVALID_ARG"));
    insta::assert_json_snapshot!(s.transcript());
}

/// diff: the files that differ between two changesets, by name (the default), with line counts
/// (`stat`) and with unified hunks (`--context` lines around each change), every status among
/// them and binary content too; a page at a time (`--limit`, `--cursor`), within `--path`s, and
/// within `--max-bytes`; a changeset against itself; and its revision and usage errors.
#[test]
fn diff_names_stat_unified_golden() {
    let mut s = Session::new();
    let store = s.store();
    let init = s.run(&["init", &store, "--name", "acme"]);
    s.name_at(&init, "/result/repo_id", "repo_id");
    let [c1, c2] = changesets_to_diff(&mut s, &store);
    diff_formats(&mut s, &store, [&c1, &c2]);
    diff_pages_and_errors(&mut s, &store, [&c1, &c2]);
    insta::assert_json_snapshot!(s.transcript());
}

/// Two changesets to diff, the second on the first, with each status between them and binary
/// content too; their ids, named `[c1]` and `[c2]`.
fn changesets_to_diff(s: &mut Session, store: &str) -> [String; 2] {
    let mutate = ["--repo", store, "--as", AGENT];
    let first = s.file(
        "first.json",
        &json!([
            {"path": "README.md", "op": "put", "content": {"inline": "# acme\n\nline 3\nline 4\n"}},
            {"path": "bin/run", "op": "put", "content": {"inline": "#!/bin/sh\necho hi\n"}},
            {"path": "latest", "op": "put", "mode": "symlink", "content": {"inline": "README.md"}},
            {"path": "logo.bin", "op": "put", "content": {"base64": "AAEC/w=="}},
            {"path": "notes.txt", "op": "put", "content": {"inline": "kept\n"}},
        ])
        .to_string(),
    );
    let c1 = s.run(&[&mutate[..], &["commit", "-m", "first", "--changes", &first]].concat());
    let c1 = s.name_at(&c1, "/result/changeset", "c1");
    let second = s.file(
        "second.json",
        &json!([
            {"path": "README.md", "op": "put",
             "content": {"inline": "# acme\n\nline three\nline 4\n"}},
            {"path": "bin/run", "op": "put", "mode": "exec",
             "content": {"inline": "#!/bin/sh\necho hi\n"}},
            {"path": "latest", "op": "put", "content": {"inline": "README.md"}},
            {"path": "logo.bin", "op": "put", "content": {"base64": "AAEC/g=="}},
            {"path": "notes.txt", "op": "delete"},
            {"path": "src/lib.rs", "op": "put", "content": {"inline": "pub fn f() {}\n"}},
        ])
        .to_string(),
    );
    let args = [
        "commit",
        "-m",
        "second",
        "--parent",
        &c1,
        "--changes",
        &second,
    ];
    let c2 = s.run(&[&mutate[..], &args].concat());
    let c2 = s.name_at(&c2, "/result/changeset", "c2");
    [c1, c2]
}

/// diff of `c1` to `c2` in each format: names (the default, and both revisions resolved), stat,
/// and unified with the default context and with none.
fn diff_formats(s: &mut Session, store: &str, [c1, c2]: [&str; 2]) {
    let diff = ["--repo", store, "diff", c1, c2];
    let with = |more: &[&'static str]| [&diff[..], more].concat();
    let names = s.run(&diff);
    let statuses: Vec<(&str, &str)> = names["result"]["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| {
            (
                file["path"].as_str().unwrap(),
                file["status"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        statuses,
        [
            ("README.md", "modified"),
            ("bin/run", "modified"),
            ("latest", "type_changed"),
            ("logo.bin", "modified"),
            ("notes.txt", "deleted"),
            ("src/lib.rs", "added"),
        ]
    );
    assert_eq!(names["meta"]["resolved"], json!({c1: c1, c2: c2}));
    let stat = s.run(&with(&["--format", "stat"]));
    assert_eq!(
        stat["result"]["files"][3],
        json!({"path": "logo.bin", "status": "modified", "binary": true})
    );
    let unified = s.run(&with(&["--format", "unified"]));
    assert_eq!(
        unified["result"]["files"][0]["hunks"][0]["lines"][2],
        json!({"op": "remove", "text": "line 3\n"})
    );
    let tight = s.run(&with(&["--format", "unified", "--context", "0"]));
    assert_eq!(tight["result"]["files"][0]["hunks"][0]["old_start"], 3);
}

/// diff of `c1` to `c2` a page at a time, within paths, and within --max-bytes; of a changeset
/// against itself; and its errors: a revision that names nothing, a limit of 0, a format it does
/// not know.
fn diff_pages_and_errors(s: &mut Session, store: &str, [c1, c2]: [&str; 2]) {
    let diff = ["--repo", store, "diff", c1, c2];
    let with = |more: &[&'static str]| [&diff[..], more].concat();
    let page = s.run(&with(&["--limit", "2"]));
    assert_eq!(
        (&page["result"]["truncated"], &page["result"]["cursor"]),
        (&json!(true), &json!("bin/run"))
    );
    let next = s.run(&with(&["--limit", "2", "--cursor", "bin/run"]));
    assert_eq!(next["result"]["files"][0]["path"], "latest");
    let within = s.run(&with(&[
        "--path",
        "src/",
        "--path",
        "README.md",
        "--format",
        "stat",
    ]));
    assert_eq!(within["result"]["files"].as_array().unwrap().len(), 2);
    // The first file comes whatever --max-bytes says, its hunks cut to those that fit.
    let cut = s.run(&with(&["--format", "unified", "--max-bytes", "1"]));
    assert_eq!(cut["result"]["files"].as_array().unwrap().len(), 1);
    assert_eq!(cut["result"]["files"][0]["hunks_truncated"], true);
    let same = s.run(&["--repo", store, "diff", c2, c2]);
    assert_eq!(same["result"]["files"], json!([]));

    let unknown = s.run(&["--repo", store, "diff", c1, "release/nope"]);
    assert_eq!((unknown.exit, unknown.code()), (6, "E_REV_NOT_FOUND"));
    let zero = s.run(&with(&["--limit", "0"]));
    assert_eq!((zero.exit, zero.code()), (2, "E_INVALID_ARG"));
    let format = s.run(&with(&["--format", "patch"]));
    assert_eq!(
        (format.exit, format.code(), &format["op"]),
        (2, "E_INVALID_ARG", &json!("diff"))
    );
}

#[test]
fn ref_set_expect_mismatch_exits_4() {
    let mut s = Session::new();
    let store = s.store();
    s.run(&["init", &store]);
    let c1 = s.commit(&store, None, "a.txt", "a", "c1");
    let c2 = s.commit(&store, Some(&c1), "b.txt", "b", "c2");
    let set = |s: &mut Session, args: &[&str]| {
        let mut all = vec!["--repo", &store, "--as", AGENT, "ref", "set"];
        all.extend_from_slice(args);
        s.run(&all)
    };

    let created = set(&mut s, &["main", &c1, "--create"]);
    assert_eq!(
        created["result"],
        json!({"name": "main", "new": c1, "already_applied": false})
    );
    // main holds c1, not c2: E_CAS_MISMATCH, exit 4, retryable, with what main holds and the
    // move that would work now.
    let stale = set(&mut s, &["main", &c2, "--expect", &c2]);
    assert_eq!((stale.exit, stale.code()), (4, "E_CAS_MISMATCH"));
    assert_eq!(stale["error"]["retryable"], true);
    assert_eq!(
        stale["error"]["details"],
        json!({"name": "main", "actual": c1})
    );
    let hint = stale["error"]["hint"].as_str().unwrap();
    assert!(hint.contains(&c1), "the hint names what main holds: {hint}");
    let moved = set(&mut s, &["main", &c2, "--expect", &c1]);
    assert_eq!(moved["result"]["already_applied"], false);
    // The same move again is a replay.
    let replay = set(&mut s, &["main", &c2, "--expect", &c1]);
    assert_eq!(replay.exit, 0);
    assert_eq!(replay["result"]["already_applied"], true);
    let exists = set(&mut s, &["main", &c1, "--create"]);
    assert_eq!((exists.exit, exists.code()), (4, "E_CAS_MISMATCH"));
    assert_eq!(exists["error"]["details"]["actual"], c2.as_str());

    let release = set(&mut s, &["release/1.0", "main~1", "--create"]);
    assert_eq!(release["meta"]["resolved"], json!({"main~1": c1}));
    let list = s.run(&["--repo", &store, "ref", "list"]);
    assert_eq!(
        list["result"]["refs"],
        json!([
            {"name": "main", "target": c2, "version": 2},
            {"name": "release/1.0", "target": c1, "version": 1},
        ])
    );

    let delete = |s: &mut Session, expect: &str| {
        s.run(&[
            "--repo",
            &store,
            "--as",
            AGENT,
            "ref",
            "delete",
            "release/1.0",
            "--expect",
            expect,
        ])
    };
    let wrong = delete(&mut s, &c2);
    assert_eq!((wrong.exit, wrong.code()), (4, "E_CAS_MISMATCH"));
    let deleted = delete(&mut s, &c1);
    assert_eq!(
        deleted["result"],
        json!({"name": "release/1.0", "old": c1, "already_applied": false})
    );
    // Deleting it again finds it gone, as asked: a replay too.
    let gone = delete(&mut s, &c1);
    assert_eq!(gone.exit, 0);
    assert_eq!(gone["result"]["already_applied"], true);

    insta::assert_json_snapshot!(s.transcript());
}

/// A `ref set` to what the ref holds already is a replay whatever it expects: `already_applied`,
/// and nothing is written, the journal as long as before and the ref's version where it was.
/// That covers the same move made again (its `--expect` stale by then) and a move to the
/// target it expects. A matching `--expect` with another target still moves the ref.
#[test]
fn a_move_to_the_current_target_is_a_replay() {
    let mut s = Session::new();
    let store = s.store();
    s.run(&["init", &store]);
    let c1 = s.commit(&store, None, "a.txt", "a", "c1");
    let c2 = s.commit(&store, Some(&c1), "b.txt", "b", "c2");
    let journal = Layout::new(Utf8PathBuf::from(&store)).journal_log();
    let set = |s: &mut Session, args: &[&str]| {
        let mut all = vec!["--repo", &store, "--as", AGENT, "ref", "set"];
        all.extend_from_slice(args);
        s.run(&all)
    };
    let version = |s: &mut Session| {
        let list = s.run(&["--repo", &store, "ref", "list"]);
        list["result"]["refs"][0]["version"].clone()
    };

    set(&mut s, &["main", &c1, "--create"]);
    let moved = set(&mut s, &["main", &c2, "--expect", &c1]);
    assert_eq!(moved["result"]["already_applied"], false);
    assert_eq!(version(&mut s), 2);
    let written = fs::metadata(&journal).unwrap().len();
    for expect in [&c1, &c1, &c2, &c2] {
        let replay = set(&mut s, &["main", &c2, "--expect", expect]);
        assert_eq!(
            replay["result"],
            json!({"name": "main", "old": expect, "new": c2, "already_applied": true}),
            "--expect {expect}"
        );
        assert_eq!(version(&mut s), 2, "--expect {expect}");
        assert_eq!(
            fs::metadata(&journal).unwrap().len(),
            written,
            "nothing written"
        );
    }
    let back = set(&mut s, &["main", &c1, "--expect", &c2]);
    assert_eq!(back["result"]["already_applied"], false);
    assert_eq!(version(&mut s), 3);
    assert!(fs::metadata(&journal).unwrap().len() > written);
}

/// `commit --advance REF` publishes in one call: the changeset and the move of REF to it in one
/// transaction, which `result.advanced` reports. A root commit creates the ref; on `--parent`, by
/// its full id, it moves the ref one step on; against a stale `--parent` it records nothing
/// (`E_CAS_MISMATCH`, exit 4, `committed: false`); without `--parent` it builds on REF's head,
/// which `meta.resolved` names; and `--human` prints the move.
#[test]
fn commit_advance_publishes_the_changeset_and_moves_the_ref() {
    let mut s = Session::new();
    let store = s.store();
    s.run(&["init", &store]);
    let publish = |s: &mut Session, name: &str, parent: Option<&str>| {
        let list =
            json!([{"path": format!("{name}.txt"), "op": "put", "content": {"inline": name}}]);
        let changes = s.file(&format!("{name}.json"), &list.to_string());
        let mut args = vec![
            "--repo",
            &store,
            "--as",
            AGENT,
            "commit",
            "-m",
            name,
            "--changes",
            &changes,
            "--advance",
            "main",
        ];
        if let Some(parent) = parent {
            args.extend(["--parent", parent]);
        }
        s.run(&args)
    };

    let created = publish(&mut s, "c1", None);
    let c1 = s.name_at(&created, "/result/changeset", "c1");
    assert_eq!(
        created["result"]["advanced"],
        json!({"name": "main", "new": c1, "already_applied": false})
    );
    let moved = publish(&mut s, "c2", Some(&c1));
    let c2 = s.name_at(&moved, "/result/changeset", "c2");
    assert_eq!(
        moved["result"]["advanced"],
        json!({"name": "main", "old": c1, "new": c2, "already_applied": false})
    );
    let list = s.run(&["--repo", &store, "ref", "list"]);
    assert_eq!(
        list["result"]["refs"],
        json!([{"name": "main", "target": c2, "version": 2}])
    );

    let stale = publish(&mut s, "c3", Some(&c1));
    assert_eq!((stale.exit, stale.code()), (4, "E_CAS_MISMATCH"));
    assert_eq!(
        stale["error"]["details"],
        json!({"name": "main", "actual": c2, "committed": false})
    );
    let message = stale["error"]["message"].as_str().unwrap();
    let advice = format!("--parent {c2} --advance main");
    assert!(message.contains(&advice), "{message}");
    let info = s.run(&["--repo", &store, "info"]);
    assert_eq!(
        info["result"]["changesets"], 2,
        "the stale commit recorded nothing"
    );

    let on_head = publish(&mut s, "c4", None);
    let c4 = s.name_at(&on_head, "/result/changeset", "c4");
    assert_eq!(on_head["result"]["parent"], c2.as_str());
    assert_eq!(on_head["meta"]["resolved"], json!({"main": c2}));
    assert_eq!(on_head["result"]["advanced"]["old"], c2.as_str());

    let list = json!([{"path": "c5.txt", "op": "put", "content": {"inline": "c5"}}]);
    let changes = s.file("c5.json", &list.to_string());
    let args = [
        "--human",
        "--repo",
        &store,
        "--as",
        AGENT,
        "commit",
        "-m",
        "c5",
        "--changes",
        &changes,
        "--parent",
        &c4,
        "--advance",
        "main",
    ];
    let human = Session::run_raw(&[], &args, None);
    assert_eq!(human.exit, 0, "{}", human.stdout);
    for line in [
        "advanced.name: main".to_owned(),
        format!("advanced.old: {c4}"),
        "advanced.already_applied: false".to_owned(),
    ] {
        assert!(
            human.stdout.lines().any(|l| l == line),
            "{line}: {}",
            human.stdout
        );
    }
}

#[test]
fn missing_identity_exits_5() {
    let mut s = Session::new();
    let store = s.store();
    s.run(&["init", &store]);
    let changes = s.file(
        "changes.json",
        r#"[{"path": "a.txt", "op": "put", "content": {"inline": "a"}}]"#,
    );
    let commit = ["--repo", &store, "commit", "-m", "x", "--changes", &changes];

    let refused = s.run(&commit);
    assert_eq!((refused.exit, refused.code()), (5, "E_IDENTITY_REQUIRED"));
    assert!(
        refused["error"]["hint"].as_str().unwrap().contains("--as"),
        "the hint names --as"
    );
    // An empty SWARMOPS_AGENT counts as unset.
    let empty = s.run_env(&[("SWARMOPS_AGENT", "")], &commit);
    assert_eq!(empty.exit, 5);
    // Every mutation needs one, checked before any revision is resolved.
    let set = s.run(&["--repo", &store, "ref", "set", "main", "abc", "--create"]);
    assert_eq!((set.exit, set.code()), (5, "E_IDENTITY_REQUIRED"));
    let delete = s.run(&["--repo", &store, "ref", "delete", "main", "--expect", "abc"]);
    assert_eq!((delete.exit, delete.code()), (5, "E_IDENTITY_REQUIRED"));
    // Reads need none, and nothing was written.
    let info = s.run(&["--repo", &store, "info"]);
    assert_eq!(info["result"]["changesets"], 0);

    insta::assert_json_snapshot!(s.transcript());
}

#[test]
fn not_a_repo_exits_6() {
    let mut s = Session::new();
    let missing = format!("{}/missing", s.tmp());
    let file = s.file("file", "not a store");

    // No store named at all: neither --repo nor SWARMOPS_REPO.
    let unnamed = s.run(&["info"]);
    assert_eq!((unnamed.exit, unnamed.code()), (6, "E_NOT_A_REPO"));
    assert!(unnamed["meta"].get("repo").is_none());
    let hint = unnamed["error"]["hint"].as_str().unwrap();
    assert!(
        hint.contains("--repo") && hint.contains("SWARMOPS_REPO"),
        "{hint}"
    );
    // A path that holds no store: missing, an empty directory, a file.
    let gone = s.run(&["--repo", &missing, "info"]);
    assert_eq!((gone.exit, gone.code()), (6, "E_NOT_A_REPO"));
    assert_eq!(gone["meta"]["repo"], missing.as_str());
    let hint = gone["error"]["hint"].as_str().unwrap();
    assert!(
        hint.starts_with("swarmops init ") && hint.contains(&missing),
        "the hint creates a store there: {hint}"
    );
    let tmp = s.tmp();
    assert_eq!(s.run(&["--repo", &tmp, "log"]).exit, 6);
    assert_eq!(s.run(&["--repo", &file, "cat", "main:x"]).exit, 6);
    let from_env = s.run_env(&[("SWARMOPS_REPO", &missing)], &["show", "main"]);
    assert_eq!((from_env.exit, from_env.code()), (6, "E_NOT_A_REPO"));
    assert_eq!(from_env["meta"]["repo"], missing.as_str());
    // An explicit empty --repo names no store: it does not fall back to SWARMOPS_REPO.
    let empty = s.run_env(&[("SWARMOPS_REPO", &missing)], &["--repo", "", "info"]);
    assert_eq!((empty.exit, empty.code()), (6, "E_NOT_A_REPO"));
    assert!(empty["meta"].get("repo").is_none(), "no store named");
    assert_eq!(empty["error"]["message"], "no store named: --repo is empty");
    assert_eq!(empty["error"]["hint"], unnamed["error"]["hint"]);
    assert!(!fs::exists(&missing).unwrap(), "nothing was created");

    insta::assert_json_snapshot!(s.transcript());
}

/// A store that leads under `/mnt/` is refused on open as `init` refuses it ([`MNT_STORE`]):
/// `info` on a path there, and on a symlink that leads there, is `E_UNSUPPORTED_FS` with the
/// `/mnt` path in `details.path`, before anything is read, and nothing is made there.
#[test]
fn a_store_that_leads_under_mnt_is_refused_on_open() {
    let mut s = Session::new();
    let direct = s.run(&["--repo", MNT_STORE, "info"]);
    assert_eq!((direct.exit, direct.code()), (1, "E_UNSUPPORTED_FS"));
    assert_eq!(direct["error"]["details"]["path"], MNT_STORE);
    let link = format!("{}/link", s.tmp());
    std::os::unix::fs::symlink(MNT_STORE, &link).unwrap();
    let linked = s.run(&["--repo", &link, "info"]);
    assert_eq!((linked.exit, linked.code()), (1, "E_UNSUPPORTED_FS"));
    assert_eq!(linked["error"]["details"]["path"], MNT_STORE);
    assert!(
        !fs::exists(MNT_STORE).unwrap_or(false),
        "nothing was made under /mnt"
    );
}

/// `init` refuses a repository name of more than 255 bytes before it makes anything:
/// `E_INVALID_ARG`, exit 2, with the store's limit in `details`. A name of 255 bytes is kept
/// whole.
#[test]
fn init_refuses_a_name_of_more_than_255_bytes() {
    let mut s = Session::new();
    let store = s.store();
    let refused = s.run(&["init", &store, "--name", &"n".repeat(256)]);
    assert_eq!((refused.exit, refused.code()), (2, "E_INVALID_ARG"));
    assert_eq!(
        refused["error"]["details"],
        json!({"store_limit": "repo_name_bytes", "max": 255, "got": 256})
    );
    assert!(!fs::exists(&store).unwrap(), "nothing was made");

    let made = s.run(&["init", &store, "--name", &"n".repeat(255)]);
    assert_eq!(made.exit, 0, "{}", made.json);
    let info = s.run(&["--repo", &store, "info"]);
    assert_eq!(info["result"]["name"].as_str().map(str::len), Some(255));
}

/// `commit --changes -` has read the change list from stdin, so a change whose client file is
/// this process's stdin would read what is left of it (nothing from a pipe, the list itself
/// from a redirected file) and commit that: it is refused (`E_INVALID_ARG`, exit 2) and nothing
/// is committed. With the list in a file, `/dev/stdin` content commits the bytes piped in.
#[test]
fn a_change_list_from_stdin_cannot_name_stdin_as_a_client_file() {
    let mut s = Session::new();
    let store = s.store();
    s.run(&["init", &store]);
    let list = json!([
        {"path": "a.txt", "op": "put", "content": {"inline": "a\n"}},
        {"path": "piped.txt", "op": "put", "content": {"file": "/dev/stdin"}},
    ])
    .to_string();
    let commit = [
        "--repo",
        &store,
        "--as",
        AGENT,
        "commit",
        "-m",
        "x",
        "--changes",
    ];
    let refused = s.run_stdin(&list, &[&commit[..], &["-"]].concat());
    assert_eq!((refused.exit, refused.code()), (2, "E_INVALID_ARG"));
    assert_eq!(
        refused["error"]["details"],
        json!({"changes": "-", "index": 1, "path": "piped.txt", "file": "/dev/stdin"})
    );
    let message = refused["error"]["message"].as_str().unwrap();
    assert!(message.contains("stdin"), "{message}");
    assert_eq!(
        s.run(&["--repo", &store, "info"])["result"]["changesets"],
        0
    );

    let file = s.file("changes.json", &list);
    let committed = s.run_stdin("piped bytes\n", &[&commit[..], &[file.as_str()]].concat());
    assert_eq!(committed.exit, 0, "{}", committed.json);
    let c1 = s.name_at(&committed, "/result/changeset", "c1");
    let piped = s.run(&["--repo", &store, "cat", &format!("{c1}:piped.txt")]);
    assert_eq!(piped["result"]["content"], "piped bytes\n");
}

/// `blob put` stores a file's content, or stdin's (`-`), as a blob and prints its id, its size
/// and whether it wrote it; `blob has` gives those of the ids it is given that the store holds
/// as blobs; and a commit names a stored blob by id (`{"blob": id}`), writing no blob. `blob
/// put` is a mutation, so it needs an identity (exit 5 without one, and nothing is stored);
/// `blob has` is a read, and an id that is not 64 hex digits is a usage error.
#[test]
fn blob_put_then_a_commit_names_the_blob_by_id() {
    let mut s = Session::new();
    let store = s.store();
    s.run(&["init", &store]);
    let content = "named by id\n";
    let id = hash_object(ObjectKind::Blob, content.as_bytes()).to_hex();
    let file = s.file("payload.txt", content);
    let put = s.run(&["--repo", &store, "--as", AGENT, "blob", "put", &file]);
    assert_eq!(put.exit, 0, "{}", put.json);
    assert_eq!(put["op"], "blob_put");
    assert_eq!(
        put["result"],
        json!({"blob": id, "size": content.len(), "written": true})
    );
    let again = s.run_stdin(
        content,
        &["--repo", &store, "--as", AGENT, "blob", "put", "-"],
    );
    assert_eq!(again.exit, 0, "{}", again.json);
    assert_eq!(
        again["result"],
        json!({"blob": id, "size": content.len(), "written": false})
    );

    let other = s.file("other.txt", "other\n");
    let other_id = hash_object(ObjectKind::Blob, b"other\n").to_hex();
    let anonymous = s.run(&["--repo", &store, "blob", "put", &other]);
    assert_eq!(
        (anonymous.exit, anonymous.code()),
        (5, "E_IDENTITY_REQUIRED")
    );
    let has = s.run(&["--repo", &store, "blob", "has", &id, &other_id]);
    assert_eq!(has.exit, 0, "{}", has.json);
    assert_eq!(has["op"], "blob_has");
    assert_eq!(has["result"], json!({"present": [id]}));
    let bad = s.run(&["--repo", &store, "blob", "has", "xyz"]);
    assert_eq!((bad.exit, bad.code()), (2, "E_INVALID_ARG"));

    let changes = s.file(
        "by-id.json",
        &json!([{"path": "named.txt", "op": "put", "content": {"blob": id}}]).to_string(),
    );
    let commit = s.run(&[
        "--repo",
        &store,
        "--as",
        AGENT,
        "commit",
        "-m",
        "by id",
        "--changes",
        &changes,
    ]);
    assert_eq!(commit.exit, 0, "{}", commit.json);
    assert_eq!(commit["result"]["blobs_written"], 0);
    let c1 = s.name_at(&commit, "/result/changeset", "c1");
    let cat = s.run(&["--repo", &store, "cat", &format!("{c1}:named.txt")]);
    assert_eq!(
        (&cat["result"]["content"], &cat["result"]["blob"]),
        (&json!(content), &json!(id))
    );
}

/// A change list past 64 MiB, the most a request's text may hold, is refused by its length
/// (`E_TOO_LARGE`, scope `request`) before a byte of it is read or parsed: its bytes are zeros,
/// which no JSON parser takes. Nothing is committed.
#[test]
fn a_change_list_past_64_mib_is_refused_before_it_is_parsed() {
    let mut s = Session::new();
    let store = s.store();
    s.run(&["init", &store]);
    let limit = swarmops_ops::content::MAX_REQUEST_BYTES;
    let huge = format!("{}/huge.json", s.tmp());
    fs::File::create(&huge).unwrap().set_len(limit + 1).unwrap();
    let refused = s.run(&[
        "--repo",
        &store,
        "--as",
        AGENT,
        "commit",
        "-m",
        "x",
        "--changes",
        &huge,
    ]);
    assert_eq!((refused.exit, refused.code()), (1, "E_TOO_LARGE"));
    assert_eq!(
        refused["error"]["details"],
        json!({"size": limit + 1, "limit": limit, "scope": "request", "changes": huge})
    );
    assert_eq!(
        s.run(&["--repo", &store, "info"])["result"]["changesets"],
        0
    );
}

#[test]
fn usage_error_exits_2_with_envelope() {
    let mut s = Session::new();
    let cases: &[(&[&str], &str)] = &[
        (&[], "swarmops"),
        (&["frobnicate"], "swarmops"),
        (&["ref"], "swarmops"),
        (&["commit"], "commit"),
        (&["--reop", "x", "info"], "swarmops"),
        (&["info", "--frob"], "repo_info"),
        (&["ref", "set", "main", "abc"], "ref_set"),
        (
            &["ref", "set", "main", "abc", "--expect", "x", "--create"],
            "ref_set",
        ),
        (&["log", "-n", "many"], "log"),
        (&["--pretty=maybe", "info"], "swarmops"),
        (&["--repo"], "swarmops"),
        // The CLI's own grammar, checked before any store is opened.
        (&["cat", "main"], "read_file"),
        (&["schema", "NoSuchType"], "schema"),
    ];
    let mut replies = Vec::new();
    for (args, op) in cases {
        let usage = s.run(args);
        assert_eq!((usage.exit, usage.code()), (2, "E_INVALID_ARG"), "{args:?}");
        assert_eq!(usage["op"], *op, "{args:?}");
        let message = usage["error"]["message"].as_str().unwrap();
        assert!(!message.contains('\n'), "one line: {message:?}");
        replies.push(usage);
    }
    let unknown = &replies[1];
    assert_eq!(
        unknown["error"]["message"],
        "unrecognized subcommand 'frobnicate'"
    );
    assert!(
        unknown["error"]["hint"]
            .as_str()
            .unwrap()
            .starts_with("Usage: swarmops "),
        "the hint is the usage"
    );

    // --help and --version print clap's text, not an envelope, and exit 0.
    for (args, start) in [
        (&["--help"][..], "Source control for coding-agent swarms"),
        (&["commit", "--help"], "Record a changeset"),
        (&["--version"], "swarmops 0.1.0\n"),
    ] {
        let output = swarmops().args(args).output().unwrap();
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert_eq!(output.status.code(), Some(0), "{args:?}");
        assert!(stdout.starts_with(start), "{args:?}: {stdout}");
        assert!(serde_json::from_str::<Value>(&stdout).is_err(), "{args:?}");
    }

    insta::assert_json_snapshot!(s.transcript());
}

/// `commit --advance`'s help says what an agent acts on before and after a lost race: the
/// changeset and the move are recorded together or not at all, only while REF still names
/// `--parent`, and after `E_CAS_MISMATCH` nothing was recorded, so the change is committed again
/// on the error's `actual`. Help text is no contract (docs/policies/versioning-and-stability.md):
/// this pins those facts, not the words around them.
#[test]
fn commit_advance_help_says_what_a_lost_race_recorded() {
    let help = |flag: &str| {
        let output = swarmops().args(["commit", flag]).output().unwrap();
        assert_eq!(output.status.code(), Some(0), "commit {flag}");
        let text = String::from_utf8(output.stdout).unwrap();
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    };
    let short = help("-h");
    let when = "in the same transaction, if REF still names --parent";
    assert!(short.contains(when), "{when:?} not in: {short}");
    let long = help("--help");
    for fact in [
        "in the same transaction: both are recorded, or neither",
        "After E_CAS_MISMATCH nothing was recorded, neither the changeset nor the move",
        "commit with --parent <actual> --advance REF",
    ] {
        assert!(long.contains(fact), "{fact:?} not in: {long}");
    }
}

#[test]
fn schema_snapshot() {
    let mut s = Session::new();
    let all = s.run(&["schema"]);
    assert_eq!((all.exit, all["op"].as_str()), (0, Some("schema")));
    assert!(all["meta"].get("repo").is_none(), "schema uses no store");
    let types = all["result"].as_object().unwrap();
    for name in [
        "Envelope",
        "OpError",
        "CommitReq",
        "VerifyReport",
        "ErrorCode",
    ] {
        assert!(types.contains_key(name), "{name}");
    }
    // One type's entry alone, keyed the same way.
    let one = s.run(&["schema", "CommitReq"]);
    assert_eq!(one["result"], json!({"CommitReq": types["CommitReq"]}));
    let unknown = s.run(&["schema", "commit"]);
    assert_eq!((unknown.exit, unknown.code()), (2, "E_INVALID_ARG"));
    assert_eq!(unknown["error"]["details"]["name"], "commit");

    // The contract-change review gate: the whole of `swarmops schema`, as printed.
    insta::assert_json_snapshot!(s.redact(&all));
}

#[test]
fn human_mode_prints_text_not_json() {
    let mut s = Session::new();
    let store = s.store();
    let init = s.run(&["init", &store, "--name", "acme"]);
    s.name_at(&init, "/result/repo_id", "repo_id");
    let c1 = s.commit(&store, None, "README.md", "# acme\nline two\n", "c1");
    s.run(&[
        "--repo", &store, "--as", AGENT, "ref", "set", "main", &c1, "--create",
    ]);
    let missing = format!("{}/missing", s.tmp());

    let mut text = String::new();
    let human: &[HumanRun<'_>] = &[
        (&[], &["--human", "--repo", &store, "info"], 0),
        (
            &[],
            &["--human", "--repo", &store, "cat", "main:README.md"],
            0,
        ),
        (&[], &["--human", "--repo", &store, "ls", "main"], 0),
        (&[], &["--human", "--repo", &store, "ref", "list"], 0),
        (&[], &["--human", "whoami"], 0),
        (&[], &["--human", "--repo", &missing, "info"], 6),
        (
            &[],
            &[
                "--human", "--repo", &store, "ref", "set", "main", "main", "--create",
            ],
            5,
        ),
        (&[], &["--human", "frobnicate"], 2),
        (
            &[("SWARMOPS_FORMAT", "human")],
            &["--repo", &store, "info"],
            0,
        ),
        (&[], &["--json", "--human", "--repo", &store, "info"], 0),
    ];
    for (env, args, exit) in human {
        let output = Session::run_raw(env, args, None);
        assert_eq!(output.exit, *exit, "{args:?}: {}", output.stdout);
        assert!(
            output.stderr.is_empty(),
            "{args:?}: nothing on stderr: {}",
            output.stderr
        );
        assert!(
            serde_json::from_str::<Value>(&output.stdout).is_err(),
            "{args:?} printed JSON: {}",
            output.stdout
        );
        assert!(output.stdout.ends_with('\n'), "{args:?}");
        text.push_str(&s.text_step(env, args, &output));
    }
    let info = Session::run_raw(&[], &["--human", "--repo", &store, "info"], None);
    assert!(info.stdout.contains("name: acme\n"), "{}", info.stdout);
    // The same commands as JSON keep their exit codes; a later --json wins, and so does a flag
    // over SWARMOPS_FORMAT.
    assert_eq!(s.run(&["--repo", &missing, "info"]).exit, 6);
    let json_wins = s.run(&["--human", "--json", "--repo", &store, "info"]);
    assert_eq!(json_wins["result"]["name"], "acme");
    let flag_wins = s.run_env(
        &[("SWARMOPS_FORMAT", "human")],
        &["--json", "--repo", &store, "info"],
    );
    assert_eq!(flag_wins["result"]["name"], "acme");
    let bad = s.run_env(&[("SWARMOPS_FORMAT", "yaml")], &["--repo", &store, "info"]);
    assert_eq!((bad.exit, bad.code()), (2, "E_INVALID_ARG"));
    assert_eq!(bad["error"]["details"]["source"], "SWARMOPS_FORMAT");

    insta::assert_snapshot!(text);
}

/// The compact form (the default without a terminal) is one line, in the spec's field order;
/// `--pretty` prints the same envelope indented.
#[test]
fn envelopes_keep_the_spec_field_order() {
    let mut s = Session::new();
    let store = s.store();
    s.run(&["init", &store]);
    let compact = Session::run_raw(&[], &["--repo", &store, "info"], None);
    let line = compact.stdout.strip_suffix('\n').unwrap();
    assert!(!line.contains('\n'), "{line}");
    assert!(
        line.starts_with(r#"{"ok":true,"op":"repo_info","api":1,"result":{"repo_id":"#),
        "{line}"
    );
    assert!(
        line.contains(r#"},"meta":{"repo":"#) && line.ends_with(r#""auth":"trust"}}"#),
        "{line}"
    );
    let failed = Session::run_raw(&[], &["--repo", &store, "show", "release/nope"], None);
    assert!(
        failed.stdout.starts_with(
            r#"{"ok":false,"op":"show","api":1,"error":{"code":"E_REV_NOT_FOUND","message":"#
        ),
        "{}",
        failed.stdout
    );
    assert!(
        failed
            .stdout
            .contains(r#","details":{"rev":"release/nope"},"hint":"#),
        "{}",
        failed.stdout
    );

    let pretty = Session::run_raw(&[], &["--pretty", "--repo", &store, "info"], None);
    assert!(
        pretty
            .stdout
            .starts_with("{\n  \"ok\": true,\n  \"op\": \"repo_info\",\n")
    );
    let reparsed: Value = serde_json::from_str(&pretty.stdout).unwrap();
    let mut compact: Value = serde_json::from_str(&compact.stdout).unwrap();
    compact["meta"]["elapsed_ms"] = reparsed["meta"]["elapsed_ms"].clone();
    assert_eq!(reparsed, compact);
    let off = Session::run_raw(&[], &["--pretty=false", "--repo", &store, "info"], None);
    assert_eq!(off.stdout.lines().count(), 1);
}

/// `verify` exits 1 with `E_STORE_CORRUPT` when it finds problems, the whole report in
/// `details`, each problem a line in `problems` and a finding at the same index in `findings`;
/// a clean store is a success envelope with the report as `result`.
#[test]
fn verify_problems_exit_1_with_the_report() {
    let mut s = Session::new();
    let store = s.store();
    s.run(&["init", &store]);
    let c1 = s.commit(&store, None, "f.txt", "content", "c1");
    for deep in [false, true] {
        let mut args = vec!["--repo", store.as_str(), "verify"];
        if deep {
            args.push("--deep");
        }
        let clean = s.run(&args);
        assert_eq!(clean.exit, 0);
        assert_eq!(clean["result"]["problems"], json!([]));
        assert_eq!(clean["result"]["findings"], json!([]));
        assert_eq!(clean["result"]["changesets_checked"], 1);
    }

    // The blob is gone from the store.
    let blob = hash_object(ObjectKind::Blob, b"content");
    fs::remove_file(Layout::new(Utf8PathBuf::from(&store)).object_path(blob)).unwrap();
    // A structural verify's next step is a deep one, which finds damaged bytes too; a deep
    // one's is restoring what it names.
    for (deep, next) in [(false, "swarmops verify --deep"), (true, "backup")] {
        let mut args = vec!["--repo", store.as_str(), "verify"];
        if deep {
            args.push("--deep");
        }
        let damaged = s.run(&args);
        assert_eq!((damaged.exit, damaged.code()), (1, "E_STORE_CORRUPT"));
        let details = &damaged["error"]["details"];
        assert_eq!(details["changesets_checked"], 1);
        assert!(details["objects_checked"].as_u64().unwrap() >= 2);
        let problems = details["problems"].as_array().unwrap();
        assert_eq!(problems.len(), 1, "{problems:?}");
        let findings = details["findings"].as_array().unwrap();
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0]["kind"], "missing_object", "{findings:?}");
        assert_eq!(
            findings[0]["object"],
            blob.to_hex().as_str(),
            "{findings:?}"
        );
        assert_eq!(findings[0]["changeset"], c1.as_str(), "{findings:?}");
        let hint = damaged["error"]["hint"].as_str().unwrap();
        assert!(hint.contains(next), "{hint}");
    }

    insta::assert_json_snapshot!(s.transcript());
}

/// A read of a big file never prints more than `--max-bytes` of it (plan review focus 4).
#[test]
fn cat_truncates_to_max_bytes() {
    let mut s = Session::new();
    let store = s.store();
    s.run(&["init", &store]);
    let big = "0123456789abcdef".repeat(3 * 1024 * 1024 / 16);
    let path = s.file("big.txt", &big);
    let changes = s.file(
        "changes.json",
        &json!([{"path": "big.txt", "op": "put", "content": {"file": path}}]).to_string(),
    );
    let commit = s.run(&[
        "--repo",
        &store,
        "--as",
        AGENT,
        "commit",
        "-m",
        "big",
        "--changes",
        &changes,
    ]);
    assert_eq!(commit.exit, 0);
    let c1 = commit["result"]["changeset"].as_str().unwrap().to_owned();

    let peek = Session::run_raw(
        &[],
        &[
            "--repo",
            &store,
            "cat",
            &format!("{c1}:big.txt"),
            "--max-bytes",
            "1024",
        ],
        None,
    );
    assert!(peek.stdout.len() < 2048, "{} bytes", peek.stdout.len());
    let peek: Value = serde_json::from_str(&peek.stdout).unwrap();
    assert_eq!(peek["result"]["size"], 3 * 1024 * 1024);
    assert_eq!(peek["result"]["truncated"], true);
    assert_eq!(peek["result"]["content"], big[..1024]);
    // The default is 1 MiB.
    let default = Session::run_raw(
        &[],
        &["--repo", &store, "cat", &format!("{c1}:big.txt")],
        None,
    );
    let default: Value = serde_json::from_str(&default.stdout).unwrap();
    assert_eq!(
        default["result"]["content"].as_str().unwrap().len(),
        1024 * 1024
    );
    assert_eq!(default["result"]["truncated"], true);
}

/// Every error code M1 can raise reaches the CLI with its exit code (operations spec §8: each
/// `E_` code at least once), and the golden snapshot pins each one's message, details and hint.
#[test]
fn error_codes_reach_the_cli() {
    /// Codes this test cannot raise through the binary, and why.
    const NOT_RAISED: &[(&str, &str)] = &[
        (
            "E_AMBIGUOUS_ID",
            "needs two changesets sharing a 12-digit prefix (ops tests)",
        ),
        (
            "E_UNKNOWN_PARENT",
            "a parent is resolved first, so it is E_REV_NOT_FOUND",
        ),
        ("E_REF_NOT_FOUND", "no M1 operation looks a ref up by name"),
        (
            "E_IO_TRANSIENT",
            "needs a journal append that fails: tests/faults.rs raises it",
        ),
        ("E_BUSY", "needs the store's lock held for its 10 s wait"),
        ("E_INTERNAL", "a caught panic: the binary's unit tests"),
        ("E_AUTH", "tokens arrive after M1"),
        ("E_POLICY_DENIED", "policies arrive after M1"),
        (
            "E_MAINTENANCE_BUSY",
            "the maintenance lease arrives after M1",
        ),
    ];
    let mut s = Session::new();
    let store = s.store();
    let init = s.run(&["init", &store]);
    s.name_at(&init, "/result/repo_id", "repo_id");
    let c1 = s.commit(&store, None, "src/lib.rs", "pub fn f() {}\n", "c1");
    s.run(&[
        "--repo", &store, "--as", AGENT, "ref", "set", "main", &c1, "--create",
    ]);
    // On main's head, but main stays where it is: a commit without --advance moves no ref.
    let c2 = s.commit(&store, None, "src/main.rs", "fn main() {}\n", "c2");

    let mut raised = Vec::new();
    for (code, args) in error_cases(&s, &store, &c2) {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let failed = s.run(&args);
        assert_eq!(failed.code(), code, "{args:?}");
        raised.push(code);
    }
    // `git import`'s, from git fixtures, each named after its fixture in the transcript.
    let fixtures = ["refs", "gitlinks", "shallow", "partial_clone"].map(|name| {
        let fixture = swarmops_testkit::git::fixture(name);
        s.name(fixture.dir.to_str().expect("UTF-8 temp paths"), name);
        (name, fixture)
    });
    let repo = |name: &str| {
        let (_, fixture) = (fixtures.iter().find(|(named, _)| *named == name))
            .unwrap_or_else(|| panic!("no fixture {name}"));
        let repo = fixture
            .repo
            .as_deref()
            .expect("the fixture holds a repository");
        repo.to_str().expect("UTF-8 temp paths").to_owned()
    };
    for (code, args) in git_error_cases(&s, &store, &repo) {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let failed = s.run(&args);
        assert_eq!(failed.code(), code, "{args:?}");
        raised.push(code);
    }
    // Last, as it damages the store: verify finds the blob gone.
    let blob = hash_object(ObjectKind::Blob, b"pub fn f() {}\n");
    fs::remove_file(Layout::new(Utf8PathBuf::from(&store)).object_path(blob)).unwrap();
    let corrupt = s.run(&["--repo", &store, "verify"]);
    assert_eq!(corrupt.code(), "E_STORE_CORRUPT");
    raised.push("E_STORE_CORRUPT");
    // A metadata file that does not parse, and one that does not open: each failure names the
    // file operation and the store file it failed on, relative to the store's root, and neither
    // message carries the store's path.
    let layout = Layout::new(Utf8PathBuf::from(&store));
    fs::write(layout.journal_ckpt(), [0; 3]).unwrap();
    let malformed = s.run(&["--repo", &store, "info"]);
    assert_eq!((malformed.code(), malformed.exit), ("E_STORE_CORRUPT", 1));
    assert_eq!(
        malformed["error"]["details"],
        json!({"fs_op": "read", "store_file": "meta/journal.ckpt"})
    );
    fs::remove_file(layout.journal_off()).unwrap();
    let missing = s.run(&["--repo", &store, "info"]);
    assert_eq!((missing.code(), missing.exit), ("E_IO", 1));
    assert_eq!(
        missing["error"]["details"],
        json!({"fs_op": "open", "store_file": "meta/journal.off"})
    );
    for failed in [&malformed, &missing] {
        let message = failed["error"]["message"].as_str().unwrap();
        assert!(!message.contains(&store), "{message}");
    }
    raised.push("E_IO");
    assert!(
        !fs::exists(MNT_STORE).unwrap_or(false),
        "nothing was made under /mnt"
    );

    // Each code is raised here or listed as not raised, never both.
    for code in ErrorCode::ALL {
        let code = code.as_str();
        let listed = NOT_RAISED.iter().any(|(c, _)| *c == code);
        assert_ne!(raised.contains(&code), listed, "{code}");
    }
    insta::assert_json_snapshot!(s.transcript());
}

/// The failing commands of [`error_codes_reach_the_cli`], each with the code it must raise,
/// against `store`, where main holds `src/lib.rs`, and `c2`, which main does not hold,
/// adds `src/main.rs`.
fn error_cases(s: &Session, store: &str, c2: &str) -> Vec<(&'static str, Vec<String>)> {
    let empty = s.file("empty.json", "[]");
    let conflict = s.file(
        "conflict.json",
        r#"[{"path": "src/lib.rs/x", "op": "put", "content": {"inline": "x"}}]"#,
    );
    let not_json = s.file("not-json.json", "{\"path\": ");
    let advance = s.file(
        "advance.json",
        r#"[{"path": "src/new.rs", "op": "put", "content": {"inline": "x"}}]"#,
    );
    let huge = format!("{}/huge.bin", s.tmp());
    fs::File::create(&huge)
        .unwrap()
        .set_len(swarmops_ops::content::MAX_CONTENT_BYTES + 1)
        .unwrap();
    let too_large = s.file(
        "too-large.json",
        &json!([{"path": "huge.bin", "op": "put", "content": {"file": huge}}]).to_string(),
    );
    let missing = format!("{}/missing", s.tmp());
    let on = |args: &[&str]| -> Vec<String> {
        ["--repo", store, "--as", AGENT]
            .iter()
            .chain(args)
            .map(|arg| (*arg).to_owned())
            .collect()
    };
    let commit = |changes: &str| on(&["commit", "-m", "x", "--changes", changes]);
    let anonymous = vec![
        "--repo".to_owned(),
        store.to_owned(),
        "commit".to_owned(),
        "-m".to_owned(),
        "x".to_owned(),
        "--changes".to_owned(),
        empty.clone(),
    ];
    vec![
        ("E_EXISTS", vec!["init".to_owned(), store.to_owned()]),
        (
            "E_NOT_A_REPO",
            vec!["--repo".to_owned(), missing, "info".to_owned()],
        ),
        ("E_IDENTITY_REQUIRED", anonymous),
        ("E_REV_NOT_FOUND", on(&["show", "release/none"])),
        ("E_PATH_NOT_FOUND", on(&["cat", "main:src/none.rs"])),
        ("E_IS_DIR", on(&["cat", "main:src"])),
        ("E_TOO_LARGE", commit(&too_large)),
        ("E_EMPTY_COMMIT", commit(&empty)),
        ("E_TYPE_CONFLICT", commit(&conflict)),
        (
            "E_CAS_MISMATCH",
            on(&["ref", "set", "main", c2, "--create"]),
        ),
        (
            "E_CAS_MISMATCH",
            on(&[
                "commit",
                "-m",
                "x",
                "--changes",
                &advance,
                "--parent",
                c2,
                "--advance",
                "main",
            ]),
        ),
        (
            "E_REF_NAME_INVALID",
            on(&["ref", "set", "feature/x", "main", "--create"]),
        ),
        ("E_INVALID_ARG", on(&["show", "not a rev"])),
        ("E_INVALID_ARG", on(&["log", "-n", "0"])),
        ("E_INVALID_ARG", commit(&not_json)),
        (
            "E_UNSUPPORTED_FS",
            vec!["init".to_owned(), MNT_STORE.to_owned()],
        ),
    ]
}

/// The failing `git import`s of [`error_codes_reach_the_cli`], each with the code it must raise,
/// against `store`, whose main holds a native changeset; `repo` gives the repository of a git
/// fixture by the fixture's name.
fn git_error_cases(
    s: &Session,
    store: &str,
    repo: &dyn Fn(&str) -> String,
) -> Vec<(&'static str, Vec<String>)> {
    let import = |args: &[&str]| -> Vec<String> {
        ["--repo", store, "--as", AGENT, "git", "import"]
            .iter()
            .chain(args)
            .map(|arg| (*arg).to_owned())
            .collect()
    };
    let missing = format!("{}/missing", s.tmp());
    let (refs, gitlinks) = (repo("refs"), repo("gitlinks"));
    vec![
        ("E_GIT_NOT_A_REPO", import(&[&missing])),
        (
            "E_GIT_REF_NOT_FOUND",
            import(&[&refs, "--ref", "no-such-branch"]),
        ),
        // main holds a changeset of no import.
        ("E_GIT_IMPORT_DIVERGED", import(&[&refs])),
        (
            "E_GIT_IMPORT_LOSSY",
            import(&[&gitlinks, "--into", "release/gitlinks"]),
        ),
        ("E_GIT_UNSUPPORTED", import(&[&repo("shallow")])),
        // A partial clone lacks every blob.
        (
            "E_GIT_READ",
            import(&[&repo("partial_clone"), "--into", "release/partial"]),
        ),
    ]
}

// ---------------------------------------------------------------------------------------------
// Running the binary.
// ---------------------------------------------------------------------------------------------

/// The binary, with none of this process's `SWARMOPS_*` variables, the order check on
/// (`SWARMOPS_TEST_ORDER_CHECK=1`; a test's own `env` still wins), and a deadline
/// (`support::RUN_TIMEOUT`): `output()` kills and reaps a run still going then, whose status
/// has no exit code.
fn swarmops() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_swarmops"));
    for var in support::swarmops_vars() {
        command.env_remove(var);
    }
    command.env(ORDER_CHECK_ENV, "1");
    command.timeout(support::RUN_TIMEOUT);
    command
}

/// What one run printed, and its exit code.
struct Output {
    exit: i32,
    stdout: String,
    stderr: String,
}

/// One run whose stdout was a JSON envelope.
struct Reply {
    exit: i32,
    /// The envelope, for reading fields.
    json: Value,
    /// The envelope as printed, for snapshots, which keep its key order.
    printed: Json,
}

impl Reply {
    /// The error's code; `""` on success.
    fn code(&self) -> &str {
        self.json["error"]["code"].as_str().unwrap_or_default()
    }
}

impl Index<&str> for Reply {
    type Output = Value;

    fn index(&self, key: &str) -> &Value {
        &self.json[key]
    }
}

/// A temp dir for stores and files, the transcript of the commands run in it, and the names
/// that stand for what changes from run to run.
struct Session {
    dir: TempDir,
    steps: Vec<Json>,
    names: Names,
}

impl Session {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("create a temp dir");
        let tmp = dir
            .path()
            .to_str()
            .expect("temp paths are UTF-8")
            .to_owned();
        let mut names = Names::default();
        names.add(&tmp, "[tmp]");
        Self {
            dir,
            steps: Vec::new(),
            names,
        }
    }

    /// The temp dir.
    fn tmp(&self) -> String {
        self.dir.path().to_str().unwrap().to_owned()
    }

    /// `<tmp>/store`, where most tests make their store.
    fn store(&self) -> String {
        format!("{}/store", self.tmp())
    }

    /// Writes `text` to `<tmp>/<name>`; its path.
    fn file(&self, name: &str, text: &str) -> String {
        let path = format!("{}/{name}", self.tmp());
        fs::write(&path, text).expect("write a test file");
        path
    }

    /// Names `value` `[name]` in the snapshot.
    fn name(&mut self, value: &str, name: &str) {
        self.names.add(value, &format!("[{name}]"));
    }

    /// Names the string at `pointer` in `reply` `[name]`; the string.
    fn name_at(&mut self, reply: &Reply, pointer: &str, name: &str) -> String {
        let value = reply
            .json
            .pointer(pointer)
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("no string at {pointer}: {}", reply.json))
            .to_owned();
        self.name(&value, name);
        value
    }

    /// Commits `content` at `path` as [`AGENT`] on `parent` (default: main, if it exists);
    /// the new changeset's id, named `[name]`.
    fn commit(
        &mut self,
        store: &str,
        parent: Option<&str>,
        path: &str,
        content: &str,
        name: &str,
    ) -> String {
        let changes = self.file(
            &format!("{name}.json"),
            &json!([{"path": path, "op": "put", "content": {"inline": content}}]).to_string(),
        );
        let mut args = vec![
            "--repo",
            store,
            "--as",
            AGENT,
            "commit",
            "-m",
            name,
            "--changes",
            &changes,
        ];
        if let Some(parent) = parent {
            args.extend(["--parent", parent]);
        }
        let reply = self.run(&args);
        assert_eq!(reply.exit, 0, "{}", reply.json);
        self.name_at(&reply, "/result/changeset", name)
    }

    fn run(&mut self, args: &[&str]) -> Reply {
        self.run_env(&[], args)
    }

    fn run_env(&mut self, env: &[(&str, &str)], args: &[&str]) -> Reply {
        self.step(env, args, None)
    }

    fn run_stdin(&mut self, stdin: &str, args: &[&str]) -> Reply {
        self.step(&[], args, Some(stdin))
    }

    /// Runs the binary with `args`, `env` set and `stdin` given; records nothing.
    fn run_raw(env: &[(&str, &str)], args: &[&str], stdin: Option<&str>) -> Output {
        let mut command = swarmops();
        command.args(args);
        for (key, value) in env {
            command.env(key, value);
        }
        if let Some(stdin) = stdin {
            command.write_stdin(stdin);
        }
        let output = command.output().expect("run swarmops");
        let exit = output.status.code().unwrap_or_else(|| {
            panic!(
                "swarmops {args:?} ended {} (past its {:?} deadline, so killed, or by a signal)\n\
                 --- stdout ---\n{}--- stderr ---\n{}",
                output.status,
                support::RUN_TIMEOUT,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            )
        });
        Output {
            exit,
            stdout: String::from_utf8(output.stdout).expect("stdout is UTF-8"),
            stderr: String::from_utf8(output.stderr).expect("stderr is UTF-8"),
        }
    }

    /// Runs the binary, checks its envelope ([`Session::check`]) and records the step.
    fn step(&mut self, env: &[(&str, &str)], args: &[&str], stdin: Option<&str>) -> Reply {
        let output = Session::run_raw(env, args, stdin);
        let json = check(args, &output);
        let mut step = vec![("args".to_owned(), Json::String(shell_words(args)))];
        if !env.is_empty() {
            let vars = env
                .iter()
                .map(|(key, value)| ((*key).to_owned(), Json::String((*value).to_owned())))
                .collect();
            step.push(("env".to_owned(), Json::Object(vars)));
        }
        if let Some(stdin) = stdin {
            let stdin = serde_json::from_str(stdin).unwrap_or(Json::String(stdin.to_owned()));
            step.push(("stdin".to_owned(), stdin));
        }
        step.push(("exit".to_owned(), Json::Number(output.exit.into())));
        let printed: Json = serde_json::from_str(&output.stdout).expect("checked above");
        step.push(("stdout".to_owned(), printed.clone()));
        self.steps.push(Json::Object(step));
        Reply {
            exit: output.exit,
            json,
            printed,
        }
    }

    /// The transcript so far, with what changes from run to run replaced by its name.
    fn transcript(&mut self) -> Json {
        let steps = self.steps.clone();
        self.names.redact(Json::Array(steps))
    }

    /// One reply's envelope as printed, redacted as [`Session::transcript`] does.
    fn redact(&mut self, reply: &Reply) -> Json {
        self.names.redact(reply.printed.clone())
    }

    /// A `--human` run as transcript text: the command, its exit code, what it printed.
    fn text_step(&mut self, env: &[(&str, &str)], args: &[&str], output: &Output) -> String {
        let vars = env.iter().fold(String::new(), |mut vars, (k, v)| {
            let _ = write!(vars, "{k}={v} ");
            vars
        });
        let text = format!(
            "$ {vars}swarmops {}\n[exit {}]\n{}\n",
            shell_words(args),
            output.exit,
            output.stdout
        );
        self.names.redact_str(&text)
    }
}

/// Checks one run against the envelope contract and returns the envelope: stdout holds exactly
/// one JSON document and stderr nothing; `ok` holds exactly when the exit code is 0, and then
/// `result` is there and `error` is not; otherwise `error` is there, with the `exit`, the
/// retry advice and the details object that its code's row of the table gives, and `result`
/// is not; `api` is 1; `meta` has `elapsed_ms`, `resolved` and `auth: "trust"`.
fn check(args: &[&str], output: &Output) -> Value {
    let shown = format!(
        "swarmops {args:?} exited {}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        output.exit, output.stdout, output.stderr
    );
    assert!(output.stderr.is_empty(), "nothing on stderr: {shown}");
    assert!(
        output.stdout.ends_with('\n') && output.stdout.lines().count() == 1,
        "one compact line: {shown}"
    );
    let json: Value = serde_json::from_str(&output.stdout)
        .unwrap_or_else(|e| panic!("stdout is one JSON document ({e}): {shown}"));
    assert_eq!(json["api"], 1, "{shown}");
    assert!(json["op"].is_string(), "{shown}");
    assert!(json["meta"]["elapsed_ms"].is_u64(), "{shown}");
    assert!(json["meta"]["resolved"].is_object(), "{shown}");
    assert_eq!(json["meta"]["auth"], "trust", "{shown}");
    if output.exit == 0 {
        assert_eq!(json["ok"], true, "{shown}");
        assert!(json.get("result").is_some(), "{shown}");
        assert!(json.get("error").is_none(), "{shown}");
    } else {
        assert_eq!(json["ok"], false, "{shown}");
        assert!(json.get("result").is_none(), "{shown}");
        let error = &json["error"];
        let code = ErrorCode::ALL
            .iter()
            .find(|code| error["code"] == code.as_str())
            .unwrap_or_else(|| panic!("a known code: {shown}"));
        assert_eq!(error["exit"], output.exit, "{shown}");
        assert_eq!(code.exit(), output.exit, "{shown}");
        assert_eq!(error["retryable"], code.retryable(), "{shown}");
        assert!(error["message"].is_string(), "{shown}");
        assert!(error["details"].is_object(), "{shown}");
        assert!(
            error["hint"].is_string() || error["hint"].is_null(),
            "{shown}"
        );
    }
    json
}

/// `args` as one line a shell would split back into them.
fn shell_words(args: &[&str]) -> String {
    args.iter()
        .map(|arg| {
            let plain = !arg.is_empty()
                && arg
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_./:=~[]@".contains(&b));
            if plain {
                (*arg).to_owned()
            } else {
                format!("'{}'", arg.replace('\'', r"'\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------------------------------------
// Redaction.
// ---------------------------------------------------------------------------------------------

/// What changes from run to run, and the stable names that replace it in snapshots.
#[derive(Default)]
struct Names {
    /// Each value a test named, and its name.
    named: Vec<(String, String)>,
    /// Hex runs of 32 or more digits no test named, in the order they were met: `[id<n>]`.
    found: Vec<String>,
}

impl Names {
    fn add(&mut self, value: &str, name: &str) {
        assert!(!value.is_empty(), "name {name} stands for nothing");
        self.named.push((value.to_owned(), name.to_owned()));
        // Longest first, so that an id is replaced before a prefix of it.
        self.named
            .sort_by_key(|(value, _)| std::cmp::Reverse(value.len()));
    }

    /// `json` with each string and key redacted ([`Names::redact_str`]) and each time key's
    /// number replaced by `[<key>]`.
    fn redact(&mut self, json: Json) -> Json {
        match json {
            Json::String(text) => Json::String(self.redact_str(&text)),
            Json::Array(items) => Json::Array(items.into_iter().map(|j| self.redact(j)).collect()),
            Json::Object(members) => {
                let mut renamed = false;
                let mut members: Vec<(String, Json)> = members
                    .into_iter()
                    .map(|(key, value)| {
                        let value = match value {
                            Json::Number(_) if TIME_KEYS.contains(&key.as_str()) => {
                                Json::String(format!("[{key}]"))
                            }
                            value => self.redact(value),
                        };
                        let name = self.redact_str(&key);
                        renamed |= name != key;
                        (name, value)
                    })
                    .collect();
                // A map keyed by ids (`meta.resolved`) is in the order of ids that change from
                // run to run; in their names' order it is the same every run. A struct's
                // fields are never ids, so they keep the order they were printed in.
                if renamed {
                    members.sort_by(|(a, _), (b, _)| a.cmp(b));
                }
                Json::Object(members)
            }
            other => other,
        }
    }

    /// `text` with each named value replaced by its name, then each other run of 32 or more
    /// lowercase hex digits by `[id<n>]`.
    fn redact_str(&mut self, text: &str) -> String {
        let mut text = text.to_owned();
        for (value, name) in &self.named {
            text = text.replace(value.as_str(), name);
        }
        let mut out = String::with_capacity(text.len());
        let mut run = String::new();
        for c in text.chars().chain(std::iter::once('\0')) {
            if c.is_ascii_digit() || ('a'..='f').contains(&c) {
                run.push(c);
                continue;
            }
            if run.len() >= 32 {
                let n = if let Some(at) = self.found.iter().position(|seen| *seen == run) {
                    at + 1
                } else {
                    self.found.push(run.clone());
                    self.found.len()
                };
                let _ = write!(out, "[id{n}]");
            } else {
                out.push_str(&run);
            }
            run.clear();
            if c != '\0' {
                out.push(c);
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------------------------
// JSON that keeps its key order.
// ---------------------------------------------------------------------------------------------

/// A JSON value that keeps each object's keys in the order they were written, so that a
/// snapshot shows the order the binary printed them in (`serde_json::Value` sorts keys).
#[derive(Debug, Clone, PartialEq)]
enum Json {
    Null,
    Bool(bool),
    Number(Number),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl<'de> Deserialize<'de> for Json {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct JsonVisitor;

        impl<'de> Visitor<'de> for JsonVisitor {
            type Value = Json;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a JSON value")
            }

            fn visit_unit<E>(self) -> Result<Json, E> {
                Ok(Json::Null)
            }

            fn visit_bool<E>(self, b: bool) -> Result<Json, E> {
                Ok(Json::Bool(b))
            }

            fn visit_i64<E>(self, n: i64) -> Result<Json, E> {
                Ok(Json::Number(n.into()))
            }

            fn visit_u64<E>(self, n: u64) -> Result<Json, E> {
                Ok(Json::Number(n.into()))
            }

            fn visit_f64<E: de::Error>(self, n: f64) -> Result<Json, E> {
                Number::from_f64(n)
                    .map(Json::Number)
                    .ok_or_else(|| E::custom("not a finite number"))
            }

            fn visit_str<E>(self, s: &str) -> Result<Json, E> {
                Ok(Json::String(s.to_owned()))
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Json, A::Error> {
                let mut items = Vec::new();
                while let Some(item) = seq.next_element()? {
                    items.push(item);
                }
                Ok(Json::Array(items))
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Json, A::Error> {
                let mut members = Vec::new();
                while let Some(member) = map.next_entry()? {
                    members.push(member);
                }
                Ok(Json::Object(members))
            }
        }

        deserializer.deserialize_any(JsonVisitor)
    }
}

impl Serialize for Json {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Json::Null => serializer.serialize_unit(),
            Json::Bool(b) => serializer.serialize_bool(*b),
            Json::Number(n) => n.serialize(serializer),
            Json::String(s) => serializer.serialize_str(s),
            Json::Array(items) => {
                let mut seq = serializer.serialize_seq(Some(items.len()))?;
                for item in items {
                    seq.serialize_element(item)?;
                }
                seq.end()
            }
            Json::Object(members) => {
                let mut map = serializer.serialize_map(Some(members.len()))?;
                for (key, value) in members {
                    map.serialize_entry(key, value)?;
                }
                map.end()
            }
        }
    }
}

/// `swarmops derive` (operations spec §1, storage spec §5's eager mode): the skip lists of new
/// changesets derived ahead of any reader, once or, with `--follow`, as they are committed.
mod derive {
    use std::path::Path;
    use std::thread;
    use std::time::{Duration, Instant};

    use serde_json::{Value, json};
    use swarmops_testkit::child::Child;

    use crate::support::{self, Store};

    /// The agent the commits act as.
    const WRITER: &str = "agent/worker-18";

    /// Whether `store` holds changeset `id`'s first-parent skip list, the value of the built-in
    /// eager derivation.
    fn has_list(store: &Store, id: &str) -> bool {
        let lists = Path::new(store.path()).join("meta/derived/first_parent_skip_list/1");
        lists.join(&id[..2]).join(&id[2..]).is_file()
    }

    /// Whether `store` holds `id`'s skip list by `deadline`, looking every 10 ms.
    fn listed_by(store: &Store, id: &str, deadline: Instant) -> bool {
        loop {
            if has_list(store, id) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// `swarmops --repo <store> derive --follow`, then `args`: a child that runs until it is
    /// killed, and is killed and reaped when dropped.
    fn follower(store: &Store, args: &[&str]) -> Child {
        let mut line = vec!["--repo", store.path(), "derive", "--follow"];
        line.extend(args);
        Child::spawn(&mut support::swarmops(&line, &[]))
    }

    /// Kills `child`, a follower of `store`, reaps it, and gives the `derived` count of each
    /// envelope it printed: one compact line each, a success of `derive` on that store, printed
    /// for a run that derived something, and none of whose values was stored already (a
    /// follower reads each frame once, and nothing else derives here). Nothing on stderr.
    fn rounds(child: &mut Child, store: &Store) -> Vec<u64> {
        let (output, killed) = child.finish(Instant::now());
        let stdout = String::from_utf8(output.stdout).expect("UTF-8 stdout");
        let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
        assert!(
            killed,
            "it follows until killed: {:?}\n{stdout}",
            output.status
        );
        assert_eq!(stderr, "", "nothing on stderr");
        let round = |line: &str| {
            let envelope: Value = serde_json::from_str(line).expect("one envelope a line");
            assert_eq!(envelope["ok"], true, "{line}");
            assert_eq!(envelope["op"], "derive", "{line}");
            assert_eq!(envelope["meta"]["repo"], store.path(), "{line}");
            let derived = envelope["result"]["derived"].as_u64().expect("a count");
            assert!(
                derived > 0,
                "only a run that derived something prints: {line}"
            );
            assert_eq!(
                envelope["result"]["cached"], 0,
                "each frame read once: {line}"
            );
            derived
        };
        stdout.lines().map(round).collect()
    }

    /// Once, `swarmops derive` derives the skip lists of what the journal records, up to 1,000
    /// frames, and says where to go on (`next`); after that frame nothing is left, and `next` is
    /// omitted; from the first frame again, the lists are found stored. It needs no identity.
    /// `--from-start` and `--interval-ms` without `--follow`, `--after` with `--from-start`, and
    /// an interval of 0 are usage errors; and a follower of a store that is not there fails at
    /// once, as any command does.
    #[test]
    fn derive_runs_once_and_says_where_to_go_on() {
        let store = Store::init();
        let derive = |args: &[&str]| store.read(&[&["derive"][..], args].concat()).ok();
        assert_eq!(derive(&[]), json!({"derived": 0, "cached": 0}));
        let c0 = store.commit(WRITER, "f", "0", None);
        let c1 = store.commit(WRITER, "f", "1", Some(&c0));

        assert_eq!(derive(&[]), json!({"derived": 2, "cached": 0, "next": 1}));
        assert!(has_list(&store, &c0) && has_list(&store, &c1));
        assert_eq!(
            derive(&["--after", "1"]),
            json!({"derived": 0, "cached": 0})
        );
        assert_eq!(derive(&[]), json!({"derived": 0, "cached": 2, "next": 1}));

        for refused in [
            &["derive", "--from-start"][..],
            &["derive", "--interval-ms", "10"],
            &["derive", "--follow", "--after", "1", "--from-start"],
            &["derive", "--follow", "--interval-ms", "0"],
        ] {
            let run = store.read(refused);
            assert_eq!(run.exit(), 2, "{run}");
            run.err("E_INVALID_ARG");
        }
        let gone = format!("{}/gone", store.tmp().to_str().expect("UTF-8 temp paths"));
        let missing = support::run(&["--repo", &gone, "derive", "--follow"], &[]);
        assert_eq!(missing.exit(), 6, "{missing}");
        missing.err("E_NOT_A_REPO");
    }

    /// A follower derives the skip lists of what is committed while it runs, and leaves to
    /// readers what was there when it started (it starts at the journal's end). Three commits
    /// come before it; then probes, one at a time, until it has derived one, so that it is past
    /// its start; then three more commits, whose lists are all there within 5 s. What it printed
    /// counts at most the probes and the three: never the three made before it.
    #[test]
    fn derive_follow_derives_commits_made_while_it_runs() {
        let store = Store::init();
        let mut parent: Option<String> = None;
        let mut commit = |content: &str| {
            let id = store.commit(WRITER, "f", content, parent.as_deref());
            parent = Some(id.clone());
            id
        };
        for i in 0..3 {
            commit(&format!("before {i}"));
        }
        let mut child = follower(&store, &["--interval-ms", "50"]);

        let ready_by = Instant::now() + Duration::from_secs(60);
        let mut probes = 0_u64;
        loop {
            probes += 1;
            let probe = commit(&format!("probe {probes}"));
            let wait = Instant::now() + Duration::from_millis(500);
            if listed_by(&store, &probe, wait) {
                break;
            }
            assert!(Instant::now() < ready_by, "{probes} probes, none derived");
        }
        let made: Vec<String> = (0..3).map(|i| commit(&format!("after {i}"))).collect();
        let by = Instant::now() + Duration::from_secs(5);
        for id in &made {
            assert!(listed_by(&store, id, by), "{id}'s list within 5 s");
        }

        let derived: u64 = rounds(&mut child, &store).iter().sum();
        assert!(
            derived <= probes + 3,
            "{derived} values derived for {probes} probes and 3 commits"
        );
    }

    /// With `--from-start` a follower first derives what the journal holds: three commits made
    /// before it starts are its first run, of 3 values; a fourth, made once their lists are
    /// there, is derived as it comes.
    #[test]
    fn derive_follow_from_the_start_derives_what_is_there_first() {
        let store = Store::init();
        let c0 = store.commit(WRITER, "f", "0", None);
        let c1 = store.commit(WRITER, "f", "1", Some(&c0));
        let c2 = store.commit(WRITER, "f", "2", Some(&c1));
        let mut child = follower(&store, &["--from-start", "--interval-ms", "50"]);

        let within = |seconds| Instant::now() + Duration::from_secs(seconds);
        assert!(listed_by(&store, &c2, within(60)), "the lists there before");
        let c3 = store.commit(WRITER, "f", "3", Some(&c2));
        assert!(listed_by(&store, &c3, within(5)), "a list as it comes");

        let rounds = rounds(&mut child, &store);
        assert_eq!(rounds.first(), Some(&3), "{rounds:?}");
        assert!(rounds.iter().sum::<u64>() <= 4, "{rounds:?}");
    }
}
