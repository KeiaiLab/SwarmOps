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
use swarmops_testkit::store::layout_of;

/// The agent most mutations act as.
const AGENT: &str = "agent/worker-17";

/// The keys whose numbers are times or durations, replaced by `[<key>]` in snapshots.
const TIME_KEYS: &[&str] = &["elapsed_ms", "created_at", "secs", "at"];

/// The store's access file for `serve`'s refusals: `person/bob` is named and granted nothing.
const ACCESS: &str = "version = 1\n\n[[grant]]\nwho = [\"person/bob\"]\n";

/// A fetch of the store `store`, as git over ssh asks `serve` for it.
const UPLOAD_STORE: &str = "git-upload-pack 'store'";

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
    let bad = resolve(&mut s, "main~x");
    assert_eq!((bad.exit, bad.code()), (2, "E_INVALID_ARG"));
    insta::assert_json_snapshot!(s.transcript());
}

/// `E_AMBIGUOUS_ID` (exit 1) through the binary: two index records whose ids share their first
/// 12 hex digits, written straight through the metadata store (no operation makes such twins),
/// and `show` of that prefix. The golden pins its message, details and hint.
#[test]
fn prefix_twins_golden() {
    use swarmops_core::{ChangesetId, ObjectId};
    use swarmops_store::{ChangesetMeta, MetaStore};

    /// The byte both twins' ids repeat: their 12-digit prefix is `abababababab`.
    const TWIN_BYTE: u8 = 0xab;

    let mut s = Session::new();
    let store = s.store();
    let init = s.run(&["init", &store]);
    s.name_at(&init, "/result/repo_id", "repo_id");

    // 1. The twins: ids `ab…ab01` and `ab…ab02`, each with a distinct root tree.
    let meta = MetaStore::open(&Utf8PathBuf::from(&store)).expect("open the metadata store");
    let mut write = meta.write().expect("take the store's lock");
    for last in [1u8, 2] {
        let mut id = [TWIN_BYTE; 32];
        id[31] = last;
        let id = ChangesetId::new(ObjectId::new(id));
        s.name(&id.to_string(), &format!("twin{last}"));
        write
            .insert_changeset(ChangesetMeta::new(id, 0, 0, [last; 32], Vec::new()))
            .expect("insert a twin");
    }
    write.commit().expect("commit the twins");
    drop(meta);

    // 2. Their shared prefix names neither: ambiguous.
    let shown = s.run(&["--repo", &store, "show", "abababababab"]);
    assert_eq!((shown.exit, shown.code()), (1, "E_AMBIGUOUS_ID"));
    assert_eq!(shown["error"]["details"]["rev"], "abababababab");
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
            {"name": "main", "kind": "main", "target": c2, "version": 2},
            {"name": "release/1.0", "kind": "release", "target": c1, "version": 1},
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
    let journal = layout_of(&store).journal_log();
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
        json!([{"name": "main", "kind": "main", "target": c2, "version": 2}])
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

/// `commit --key UUID` through the binary: the same command run again replays the first run's
/// changeset (`replayed: true`, nothing written, `info` counting one changeset), also with
/// `SWARMOPS_TEST_NOW` moving the clock in between, since the key's time signs the commit
/// (`show`'s `created_at` is the key's, to the second, and its `commit_key` the key); with
/// `--advance`, the run again finds the ref's head carrying the key (`already_applied`); and a
/// key that is not a uuid v7 is `E_INVALID_ARG`, exit 2, naming it, and records nothing.
#[test]
fn commit_with_a_key_twice_through_the_binary() {
    use swarmops_ops::NOW_ENV;

    let mut s = Session::new();
    let store = s.store();
    s.run(&["init", &store]);
    let list = |content: &str| {
        json!([{"path": "f.txt", "op": "put", "content": {"inline": content}}]).to_string()
    };
    let (first_changes, second_changes) =
        (s.file("1.json", &list("1")), s.file("2.json", &list("2")));
    // uuid v7s whose time is 2001-09-09T01:46:40.999Z.
    let (key, advance_key) = (
        "00e8d4a5-13e7-7a1c-9b2d-3e4f5a6b7c8d",
        "00e8d4a5-13e7-7a1c-9b2d-3e4f5a6b7c8e",
    );
    let keyed = [
        "--repo",
        &store,
        "--as",
        AGENT,
        "commit",
        "-m",
        "keyed",
        "--changes",
        &first_changes,
        "--key",
        key,
    ];

    let first = s.run(&keyed);
    assert_eq!(first.exit, 0, "{}", first.json);
    assert_eq!(first["result"]["replayed"], false);
    let c1 = s.name_at(&first, "/result/changeset", "c1");
    let again = s.run_env(&[(NOW_ENV, "2000000000")], &keyed);
    assert_eq!(again.exit, 0, "{}", again.json);
    assert_eq!(again["result"]["changeset"], c1.as_str());
    assert_eq!(again["result"]["replayed"], true);
    assert_eq!(again["result"]["blobs_written"], 0);
    let info = s.run(&["--repo", &store, "info"]);
    assert_eq!(info["result"]["changesets"], 1);
    let shown = s.run(&["--repo", &store, "show", &c1]);
    assert_eq!(shown["result"]["created_at"], 1_000_000_000_000_u64);
    assert_eq!(shown["result"]["metadata"]["commit_key"], key);

    s.run(&[
        "--repo", &store, "--as", AGENT, "ref", "set", "main", &c1, "--create",
    ]);
    let advancing = [
        "--repo",
        &store,
        "--as",
        AGENT,
        "commit",
        "-m",
        "advancing",
        "--changes",
        &second_changes,
        "--key",
        advance_key,
        "--advance",
        "main",
    ];
    let published = s.run(&advancing);
    let c2 = s.name_at(&published, "/result/changeset", "c2");
    assert_eq!(published["result"]["advanced"]["already_applied"], false);
    let replayed = s.run(&advancing);
    assert_eq!(replayed.exit, 0, "{}", replayed.json);
    assert_eq!(
        replayed["result"]["advanced"],
        json!({"name": "main", "old": c1, "new": c2, "already_applied": true})
    );
    assert_eq!(replayed["result"]["replayed"], true);
    assert_eq!(replayed["meta"]["resolved"], json!({"main": c2}));

    let refused = s.run(&[
        "--repo",
        &store,
        "--as",
        AGENT,
        "commit",
        "-m",
        "x",
        "--changes",
        &first_changes,
        "--key",
        "retry-1",
    ]);
    assert_eq!((refused.exit, refused.code()), (2, "E_INVALID_ARG"));
    assert_eq!(refused["error"]["details"]["idempotency_key"], "retry-1");
    let info = s.run(&["--repo", &store, "info"]);
    assert_eq!(info["result"]["changesets"], 2);
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

/// A `journal.off` zero-filled far past its entries (1 TiB here, a sparse file, which costs no
/// disk) is damage, more zeroed entries at its end than a power loss leaves: `so log` and
/// `so ref set` fail with `E_STORE_CORRUPT` (exit 1) at once (the store looks at no more than
/// the last 4 KiB of `journal.off` to find where its zeroed entries start), and change nothing.
/// The message names no host path. Each run has a deadline, so that a regression fails the test
/// instead of hanging it.
#[test]
fn a_journal_off_zero_filled_far_past_its_entries_fails_reads_and_writes_at_once() {
    use std::time::Duration;

    use crate::support::{SO, Store, run_bin_within};

    let store = Store::init();
    let c1 = store.commit(AGENT, "a.txt", "a\n", None);
    store
        .run_as(AGENT, &["ref", "set", "main", &c1, "--create"], &[])
        .ok();
    let layout = store.layout();
    let len = |path: Utf8PathBuf| fs::metadata(path).unwrap().len();
    let log_len = len(layout.journal_log());
    fs::OpenOptions::new()
        .write(true)
        .open(layout.journal_off())
        .unwrap()
        .set_len(1 << 40)
        .unwrap();
    let path = store.path();
    let release = ["--as", AGENT, "ref", "set", "release", &c1, "--create"];
    for args in [&["log"][..], &release] {
        let args = [&["--repo", path][..], args].concat();
        let run = run_bin_within(SO, &args, &[], "", Duration::from_secs(30));
        assert_eq!(run.exit(), 1, "{run}");
        let error = run.err("E_STORE_CORRUPT");
        assert_eq!(
            (&error["message"], &error["hint"]),
            (
                &json!("the metadata journal is corrupt at offset 0"),
                &json!("swarmops verify")
            ),
            "{run}"
        );
    }
    assert_eq!(
        (len(layout.journal_off()), len(layout.journal_log())),
        (1 << 40, log_len),
        "nothing changed"
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
    let version = support::version_line();
    for (args, start) in [
        (&["--help"][..], "Source control for coding-agent swarms"),
        (&["commit", "--help"], "Record a changeset"),
        (&["--version"], version.as_str()),
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

/// A file of 1 TiB (a hole: no disk) placed where an external id entry of 44 bytes would be is a
/// damaged entry, read no further than a byte past an entry: `verify` exits 1 with
/// `E_STORE_CORRUPT`, the entry an `external_id_corrupt` finding by its system and key, and its
/// message names the entry, not the store's path (storage spec §7).
#[test]
fn verify_reports_an_external_id_entry_of_huge_length_as_corrupt() {
    let mut s = Session::new();
    let store = s.store();
    s.run(&["init", &store]);
    let key = "ab".repeat(20);
    let entry = Layout::new(Utf8PathBuf::from(&store)).external_id_path("git", &[0xab; 20]);
    fs::create_dir_all(entry.parent().unwrap()).unwrap();
    fs::File::create(&entry).unwrap().set_len(1 << 40).unwrap();

    let damaged = s.run(&["--repo", &store, "verify"]);
    assert_eq!(
        (damaged.exit, damaged.code()),
        (1, "E_STORE_CORRUPT"),
        "{}",
        damaged.json
    );
    let details = &damaged["error"]["details"];
    let problem = format!("the entry of external id {key} of system git is corrupt");
    assert_eq!(details["problems"], json!([problem]), "{details}");
    assert_eq!(
        details["findings"],
        json!([{"kind": "external_id_corrupt", "system": "git", "key": key}]),
        "{details}"
    );
    let message = damaged["error"]["message"].as_str().unwrap();
    assert_eq!(message, format!("verify found a problem: {problem}"));
    assert!(!message.contains(&store), "{message}");
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

/// The codes of [`NOT_RAISED`] no golden pins: no operation raises them yet.
const UNPINNED: &[&str] = &["E_REF_NOT_FOUND", "E_WS_STALE"];

/// Codes [`error_codes_reach_the_cli`] does not raise, and why: each names the test whose golden
/// pins it, but those of [`UNPINNED`].
const NOT_RAISED: &[(&str, &str)] = &[
    (
        "E_AMBIGUOUS_ID",
        "needs two changesets sharing a 12-digit prefix: prefix_twins_golden pins it",
    ),
    (
        "E_UNKNOWN_PARENT",
        "a parent is resolved first (E_REV_NOT_FOUND): swarmops-ops's \
         error::tests::unknown_parent_golden pins its error object",
    ),
    ("E_REF_NOT_FOUND", "no operation raises it"),
    (
        "E_IO_TRANSIENT",
        "needs a journal append that fails: faults::fault_codes_golden pins it",
    ),
    (
        "E_BUSY",
        "needs the store's lock held for its 10 s wait: the slow \
         op_log_behind_a_held_lock_is_e_busy pins it",
    ),
    (
        "E_INTERNAL",
        "a caught panic, from a test hook: faults::fault_codes_golden pins it",
    ),
    (
        "E_CAS_RETRY_EXHAUSTED",
        "needs main moved under every attempt: remote_sync::cas_retry_exhausted_golden pins it",
    ),
    (
        "E_MAINTENANCE_BUSY",
        "needs the maintenance lease held: \
         format2::a_held_maintenance_lease_is_busy_through_the_binary pins it",
    ),
    (
        "E_TAG_IMMUTABLE",
        "needs a FORMAT 2 store, and this session's is FORMAT 1: \
         format2::a_format_2_store_holds_a_branch_and_a_tag pins it",
    ),
    (
        "E_REMOTE_DIVERGED",
        "needs a remote: remote_acceptance::remote_error_codes_golden pins it",
    ),
    (
        "E_REMOTE_REJECTED",
        "needs a remote: remote_acceptance::remote_error_codes_golden pins it",
    ),
    (
        "E_REMOTE_UNREACHABLE",
        "needs a remote: remote_acceptance::remote_error_codes_golden pins it",
    ),
    (
        "E_REMOTE_NOT_FOUND",
        "needs remotes.toml: remote_acceptance::remote_error_codes_golden pins it",
    ),
    (
        "E_WS_STALE",
        "raised by the M15 workspace refusals, pinned when they land",
    ),
];

/// Every error code M1 can raise reaches the CLI with its exit code (operations spec §8: each
/// `E_` code at least once), and the golden snapshot pins each one's message, details and hint.
#[test]
fn error_codes_reach_the_cli() {
    check_not_raised();
    let mut s = Session::new();
    let store = s.store();
    // FORMAT 1: it takes no branch (E_FORMAT_REQUIRED), and its files are damaged by name.
    let init = s.run(&["init", "--format", "1", &store]);
    s.name_at(&init, "/result/repo_id", "repo_id");
    let c1 = s.commit(&store, None, "src/lib.rs", "pub fn f() {}\n", "c1");
    s.run(&[
        "--repo", &store, "--as", AGENT, "ref", "set", "main", &c1, "--create",
    ]);
    // On main's head, but main stays where it is: a commit without --advance moves no ref.
    let c2 = s.commit(&store, None, "src/main.rs", "fn main() {}\n", "c2");
    let op0 = Session::op_id(&store, 0);

    let mut raised = Vec::new();
    for (code, args) in error_cases(&s, &store, [&c1, &c2], &op0) {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let failed = s.run(&args);
        assert_eq!(failed.code(), code, "{args:?}");
        raised.push(code);
    }
    // `git import`'s, from git fixtures, each named after its fixture in the transcript; then
    // `git export`'s, into `refs`' repository and into an empty one whose branch is locked.
    let fixtures = git_fixtures(&mut s);
    let repo = |name: &str| fixture_repo(&fixtures, name);
    let locked = swarmops_testkit::git::empty_repository();
    let exports = git_export_error_cases(&mut s, &store, &repo("refs"), &locked);
    for (code, args) in git_error_cases(&s, &store, &repo)
        .into_iter()
        .chain(exports)
    {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let failed = s.run(&args);
        assert_eq!(failed.code(), code, "{args:?}");
        raised.push(code);
    }
    for (code, args) in land_cases(&mut s, &store, &c1) {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        assert_eq!(s.run(&args).code(), code, "{args:?}");
        raised.push(code);
    }
    // The workspaces' (M5): each raised through `so ws`, the lock one with the lock held here.
    let (ws_cases, locked) = ws_cases(&mut s, &store, &c1);
    for (code, args) in ws_cases {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        assert_eq!(s.run(&args).code(), code, "{args:?}");
        raised.push(code);
    }
    {
        let held = swarmops_ws::lock::acquire(&locked.dir).expect("take the workspace's lock");
        let args: Vec<&str> = locked.args.iter().map(String::as_str).collect();
        let busy = s.run(&args);
        assert_eq!((busy.code(), busy.exit), ("E_WS_LOCKED", 4), "{args:?}");
        raised.push("E_WS_LOCKED");
        drop(held);
    }
    // `serve`'s (M14), refused before git starts: a caller no grant of the access file names,
    // and one it names without a read.
    let access = format!("{store}/access.toml");
    fs::write(&access, ACCESS).unwrap();
    let ssh = [("SSH_ORIGINAL_COMMAND", UPLOAD_STORE)];
    let root = s.tmp();
    for (code, who) in [("E_AUTH", AGENT), ("E_POLICY_DENIED", "person/bob")] {
        let refused = s.serve(&ssh, &["serve", "--root", &root, "--as", who]);
        assert_eq!(refused.code(), code, "{who}");
        let message = refused["error"]["message"].as_str().unwrap();
        assert!(!message.contains(&root), "no host path: {message}");
        raised.push(code);
    }
    fs::remove_file(&access).unwrap();

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

/// Checks [`NOT_RAISED`]: each code listed is one the table has, and each reason names the test
/// that pins its code, but those of [`UNPINNED`].
fn check_not_raised() {
    for (code, why) in NOT_RAISED {
        let known = ErrorCode::ALL.iter().any(|known| known.as_str() == *code);
        assert!(known, "{code} is not an error code");
        let pinned = why.contains(" pins it");
        assert_eq!(pinned, !UNPINNED.contains(code), "{code}: {why}");
    }
}

/// The failing commands of [`error_codes_reach_the_cli`], each with the code it must raise,
/// against `store`, where main holds `src/lib.rs`, and `c2`, which main does not hold,
/// adds `src/main.rs`; `op0` is the id of operation 0, the first commit.
fn error_cases(
    s: &Session,
    store: &str,
    [c1, c2]: [&str; 2],
    op0: &str,
) -> Vec<(&'static str, Vec<String>)> {
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
            on(&["ref", "set", "main/x", "main", "--create"]),
        ),
        // A branch, which only a FORMAT 2 store takes.
        (
            "E_FORMAT_REQUIRED",
            on(&["ref", "set", "feature/x", "main", "--create"]),
        ),
        ("E_INVALID_ARG", on(&["show", "not a rev"])),
        ("E_INVALID_ARG", on(&["log", "-n", "0"])),
        ("E_INVALID_ARG", commit(&not_json)),
        (
            "E_UNSUPPORTED_FS",
            vec!["init".to_owned(), MNT_STORE.to_owned()],
        ),
        // main names c1: a delete that expects it is refused all the same.
        (
            "E_REF_PROTECTED",
            on(&["ref", "delete", "main", "--expect", c1]),
        ),
        // Operation 0 is the first commit, which moved no ref; the store has three.
        ("E_NOTHING_TO_UNDO", on(&["op", "undo", op0])),
        ("E_OP_NOT_FOUND", on(&["op", "undo", "99:00000000"])),
        // An undo takes an operation's id, never its bare number.
        ("E_INVALID_ARG", on(&["op", "undo", "0"])),
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

/// The failing `git export`s of [`error_codes_reach_the_cli`], each with the code it must raise,
/// from `store`, whose main holds a native changeset, run without an identity, which an export
/// does not need: into `refs`, the `refs` fixture's repository, whose main is a history of its
/// own, and into `locked`, an empty repository, named `[locked]` in the transcript, whose
/// branch's lock file this writes.
fn git_export_error_cases(
    s: &mut Session,
    store: &str,
    refs: &str,
    locked: &swarmops_testkit::git::EmptyRepository,
) -> Vec<(&'static str, Vec<String>)> {
    let locked_path = locked.dir.to_str().expect("UTF-8 temp paths");
    s.name(locked_path, "locked");
    fs::write(locked.dir.join("refs/heads/main.lock"), "").unwrap();
    let export = |args: &[&str]| -> Vec<String> {
        ["--repo", store, "git", "export"]
            .iter()
            .chain(args)
            .map(|arg| (*arg).to_owned())
            .collect()
    };
    vec![
        // main's head has no export in what the branch holds.
        ("E_GIT_EXPORT_DIVERGED", export(&[refs])),
        // The objects are written and made durable, and the lock stops the branch's move.
        ("E_GIT_WRITE", export(&[locked_path])),
    ]
}

/// The git fixtures that the failing `git import`s and `git export`s of
/// [`error_codes_reach_the_cli`] read, each named after itself in `s`'s transcript.
fn git_fixtures(s: &mut Session) -> [(&'static str, swarmops_testkit::git::GitFixture); 4] {
    ["refs", "gitlinks", "shallow", "partial_clone"].map(|name| {
        let fixture = swarmops_testkit::git::fixture(name);
        s.name(fixture.dir.to_str().expect("UTF-8 temp paths"), name);
        (name, fixture)
    })
}

/// The path of the repository of fixture `name`, one of `fixtures`.
fn fixture_repo(fixtures: &[(&str, swarmops_testkit::git::GitFixture)], name: &str) -> String {
    let (_, fixture) = (fixtures.iter().find(|(named, _)| *named == name))
        .unwrap_or_else(|| panic!("no fixture {name}"));
    let repo = fixture
        .repo
        .as_deref()
        .expect("the fixture holds a repository");
    repo.to_str().expect("UTF-8 temp paths").to_owned()
}

/// The failing lands of [`error_codes_reach_the_cli`], each with the code it must raise, against
/// `store`, where main holds `c1`: a land of main's own head, which is on main already; and a land
/// of a change to `src/lib.rs` onto `release/conflict`, made here on `c1` with another change to
/// the same line.
fn land_cases(s: &mut Session, store: &str, c1: &str) -> Vec<(&'static str, Vec<String>)> {
    let ours = s.commit(store, Some(c1), "src/lib.rs", "pub fn ours() {}\n", "ours");
    let theirs = s.commit(
        store,
        Some(c1),
        "src/lib.rs",
        "pub fn theirs() {}\n",
        "theirs",
    );
    let create = [
        "--repo",
        store,
        "--as",
        AGENT,
        "ref",
        "set",
        "release/conflict",
        theirs.as_str(),
        "--create",
    ];
    assert_eq!(s.run(&create).exit, 0);
    let on = |args: &[&str]| -> Vec<String> {
        ["--repo", store, "--as", AGENT]
            .iter()
            .chain(args)
            .map(|arg| (*arg).to_owned())
            .collect()
    };
    vec![
        ("E_NOTHING_TO_LAND", on(&["land", "main"])),
        (
            "E_CONFLICT",
            on(&["land", &ours, "--target", "release/conflict"]),
        ),
    ]
}

/// The workspace whose lock the test holds, and the command that then meets it.
struct Locked {
    dir: camino::Utf8PathBuf,
    args: Vec<String>,
}

/// w2, a workspace on a ref of its own which then moves on the file w2 edits: a rebase of it
/// conflicts, and the workspace stands rebasing with markers in the file.
fn ws_conflict(s: &mut Session, store: &str, c1: &str, tmp: &str) {
    let on = |args: &[&str]| -> Vec<String> {
        ["--repo", store, "--as", AGENT]
            .iter()
            .chain(args)
            .map(|arg| (*arg).to_owned())
            .collect()
    };
    let w2 = format!("{tmp}/w2");
    let mut ok = |args: &[&str]| {
        let line = on(args);
        let line: Vec<&str> = line.iter().map(String::as_str).collect();
        let done = s.run(&line);
        assert_eq!(done.exit, 0, "{args:?}: {}", done.json);
    };
    ok(&["ref", "set", "release/ws", c1, "--create"]);
    ok(&[
        "ws",
        "create",
        "w2",
        "--path",
        &w2,
        "--base",
        "release/ws",
        "--all",
    ]);
    fs::write(format!("{w2}/src/lib.rs"), "pub fn mine() {}\n").unwrap();
    ok(&["ws", "commit", "--ws", "w2", "-m", "mine"]);
    let moved_on = json!([{"path": "src/lib.rs", "op": "put", "content": {"inline": "pub fn moved_on() {}\n"}}]);
    let changes = s.file("ws-moved-on.json", &moved_on.to_string());
    let mut ok = |args: &[&str]| {
        let line = on(args);
        let line: Vec<&str> = line.iter().map(String::as_str).collect();
        let done = s.run(&line);
        assert_eq!(done.exit, 0, "{args:?}: {}", done.json);
    };
    ok(&[
        "commit",
        "-m",
        "moved on",
        "--parent",
        c1,
        "--changes",
        &changes,
        "--advance",
        "release/ws",
    ]);
    let conflict = {
        let line = on(&["ws", "rebase", "--ws", "w2", "--onto", "release/ws"]);
        let line: Vec<&str> = line.iter().map(String::as_str).collect();
        s.run(&line)
    };
    assert_eq!(
        (conflict.code(), conflict.exit),
        ("E_CONFLICT", 3),
        "{}",
        conflict.json
    );
}

/// The failing commands of the workspaces (M5), each with the code it must raise, and the
/// command that meets a held lock. The workspaces they need are made here, `main` of `store`
/// being `c1`; two goldens of `E_INVALID_ARG` (H5's refusals, `details.missing` `rebase` and
/// `check`) are recorded on the way.
fn ws_cases(s: &mut Session, store: &str, c1: &str) -> (Vec<(&'static str, Vec<String>)>, Locked) {
    let tmp = s.tmp();
    let on = |args: &[&str]| -> Vec<String> {
        ["--repo", store, "--as", AGENT]
            .iter()
            .chain(args)
            .map(|arg| (*arg).to_owned())
            .collect()
    };

    // 1. w1: a workspace of `src`, dirty; w3: clean with one commit, never rebased; w7: a
    //    workspace of `src` with a file off it.
    let w1 = format!("{tmp}/w1");
    let w3 = format!("{tmp}/w3");
    let w7 = format!("{tmp}/w7");
    let mut ok = |args: &[&str]| {
        let line = on(args);
        let line: Vec<&str> = line.iter().map(String::as_str).collect();
        let done = s.run(&line);
        assert_eq!(done.exit, 0, "{args:?}: {}", done.json);
    };
    ok(&["ws", "create", "w1", "--path", &w1, "--prefix", "src"]);
    fs::write(format!("{w1}/src/lib.rs"), "pub fn edited() {}\n").unwrap();
    ok(&["ws", "create", "w3", "--path", &w3, "--all"]);
    fs::write(format!("{w3}/third.txt"), "third\n").unwrap();
    ok(&["ws", "commit", "--ws", "w3", "-m", "third"]);
    ok(&["ws", "create", "w7", "--path", &w7, "--prefix", "src"]);
    fs::write(format!("{w7}/notes.txt"), "off the prefixes\n").unwrap();

    ws_conflict(s, store, c1, &tmp);

    let mut cases = vec![
        (
            "E_WS_EXISTS",
            on(&["ws", "create", "w1", "--path", &format!("{tmp}/elsewhere")]),
        ),
        ("E_WS_NOT_FOUND", on(&["ws", "info", "--ws", "nope"])),
        ("E_WS_DIRTY", on(&["ws", "delete", "w1"])),
        (
            "E_PATH_NOT_EMPTY",
            on(&["ws", "create", "w4", "--path", &format!("{tmp}/store")]),
        ),
        (
            "E_UNRESOLVED_CONFLICTS",
            on(&[
                "ws",
                "rebase",
                "--ws",
                "w2",
                "--onto",
                "release/ws",
                "--continue",
            ]),
        ),
        (
            "E_MARKERS_PRESENT",
            on(&["ws", "resolve", "--ws", "w2", "src/lib.rs"]),
        ),
        (
            "E_INVALID_ARG",
            on(&["ws", "land", "--ws", "w3", "--mode", "exact"]),
        ),
        (
            "E_INVALID_ARG",
            on(&["ws", "land", "--ws", "w3", "--check"]),
        ),
        (
            "E_CHECK_FAILED",
            on(&["ws", "land", "--ws", "w3", "--check", "--", "false"]),
        ),
        (
            "E_WS_OUTSIDE_PREFIXES",
            on(&["ws", "commit", "--ws", "w7", "-m", "notes"]),
        ),
    ];

    // 3. A changeset with a top-level `.swarmws`, the workspace's own directory: a workspace of
    //    it is unportable on every platform (`reserved_name`).
    let reserved = s.commit(store, Some(c1), ".swarmws/x", "x\n", "reserved");
    let path = format!("{tmp}/w6");
    cases.push((
        "E_WS_UNPORTABLE",
        on(&[
            "ws", "create", "w6", "--path", &path, "--base", &reserved, "--all",
        ]),
    ));

    // 4. On Windows, a changeset whose path the platform cannot hold: a workspace of it is
    //    unportable. Written for that lane; the volume here holds the name.
    #[cfg(windows)]
    {
        let reserved = s.commit(store, Some(c1), "con.txt", "x\n", "reserved");
        let path = format!("{tmp}/w5");
        let args = on(&[
            "ws", "create", "w5", "--path", &path, "--base", &reserved, "--all",
        ]);
        cases.push(("E_WS_UNPORTABLE", args));
    }
    let locked = Locked {
        dir: camino::Utf8PathBuf::from(&w1),
        args: on(&["ws", "discard", "--ws", "w1"]),
    };
    (cases, locked)
}

/// `op log` lists the store's operations newest first, each with its kind, the changesets it
/// recorded and the refs it moved: here a commit, a ref's creation, a publish (`commit
/// --advance`) and an undo. A page holds at most `-n` operations below `--before`, and says when
/// older ones follow; `--max-changesets` caps the changesets listed, and `changesets_total`
/// counts them all.
#[test]
fn op_log_pages_with_before() {
    let mut s = Session::new();
    let store = s.store();
    s.run(&["init", &store]);
    let c1 = s.commit(&store, None, "a.txt", "a\n", "c1"); // op 0
    let on = |args: &[&'static str]| -> Vec<String> {
        ["--repo", store.as_str(), "--as", AGENT]
            .iter()
            .copied()
            .chain(args.iter().copied())
            .map(str::to_owned)
            .collect()
    };
    let run = |s: &mut Session, args: Vec<String>| {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let reply = s.run(&args);
        assert_eq!(reply.exit, 0, "{args:?}: {}", reply.json);
        reply
    };
    let mut create = on(&["ref", "set", "main"]);
    create.extend([c1.clone(), "--create".to_owned()]);
    run(&mut s, create); // op 1
    let list = json!([{"path": "b.txt", "op": "put", "content": {"inline": "b\n"}}]);
    let changes = s.file("c2.json", &list.to_string());
    let mut publish = on(&["commit", "-m", "c2\n\nthe body"]);
    publish
        .extend(["--changes", &changes, "--parent", &c1, "--advance", "main"].map(str::to_owned));
    let published = run(&mut s, publish); // op 2
    let c2 = s.name_at(&published, "/result/changeset", "c2");
    let op2 = Session::op_id(&store, 2);
    let mut undo = on(&["op", "undo"]);
    undo.push(op2.clone());
    run(&mut s, undo); // op 3

    let all = run(&mut s, on(&["op", "log"]));
    assert_eq!(all["op"], "op_log");
    assert_eq!(all["result"]["truncated"], false);
    let ops = all["result"]["ops"].as_array().unwrap();
    let field = |key: &str| -> Vec<Value> { ops.iter().map(|op| op[key].clone()).collect() };
    assert_eq!(field("op"), [3, 2, 1, 0].map(Value::from));
    for op in ops {
        let id = op["id"].as_str().unwrap();
        assert!(id.starts_with(&format!("{}:", op["op"])), "{op}");
    }
    let kinds = ["ref_move", "publish", "ref_move", "commit"];
    assert_eq!(field("kind"), kinds.map(Value::from));
    assert_eq!(field("changesets_total"), [0, 1, 0, 1].map(Value::from));
    let at = ops[1]["ref_moves"][0]["at"].as_i64().unwrap();
    assert!(at > 1_700_000_000, "a time of the clock's: {at}");
    let created_at = &ops[1]["changesets"][0]["created_at"];
    assert_eq!(
        ops[1],
        json!({
            "op": 2,
            "id": op2,
            "kind": "publish",
            "changesets": [{"id": c2, "agent": AGENT, "created_at": created_at, "title": "c2"}],
            "changesets_total": 1,
            "ref_moves": [{"name": "main", "old": c1, "new": c2, "actor": AGENT, "at": at}],
        })
    );
    let undo_move = &ops[0]["ref_moves"][0];
    assert_eq!(
        (&undo_move["old"], &undo_move["new"]),
        (&json!(c2), &json!(c1))
    );
    assert!(ops[2]["ref_moves"][0].get("old").is_none(), "a creation");
    assert_eq!(ops[3]["changesets"][0]["id"], c1.as_str());

    // A page at a time, below --before.
    let page = |s: &mut Session, args: &[&'static str]| -> (Vec<u64>, bool) {
        let mut line = on(&["op", "log"]);
        line.extend(args.iter().map(|arg| (*arg).to_owned()));
        let reply = run(s, line);
        let ops = reply["result"]["ops"].as_array().unwrap();
        let seqs = ops.iter().map(|op| op["op"].as_u64().unwrap()).collect();
        (seqs, reply["result"]["truncated"] == true)
    };
    assert_eq!(page(&mut s, &["-n", "2"]), (vec![3, 2], true));
    assert_eq!(page(&mut s, &["--before", "2", "-n", "1"]), (vec![1], true));
    assert_eq!(page(&mut s, &["--before", "2"]), (vec![1, 0], false));
    assert_eq!(page(&mut s, &["--before", "0"]), (vec![], false));
    assert_eq!(page(&mut s, &["--before", "99"]), (vec![3, 2, 1, 0], false));

    // --max-changesets 0 lists none, counts them, and still tells a publish.
    let mut capped = on(&["op", "log", "--before", "3", "-n", "1"]);
    capped.extend(["--max-changesets", "0"].map(str::to_owned));
    let capped = run(&mut s, capped);
    let only = &capped["result"]["ops"][0];
    assert_eq!(only["changesets"], json!([]));
    assert_eq!(
        (&only["changesets_total"], &only["kind"]),
        (&json!(1), &json!("publish"))
    );

    // A page of no operations is refused, as log's is.
    let args = ["--repo", store.as_str(), "op", "log", "-n", "0"];
    let zero = s.run(&args);
    assert_eq!((zero.exit, zero.code()), (2, "E_INVALID_ARG"));
}

/// `op log` waits for the store's lock as a write does: behind a lock held for the whole of the
/// store's 10 s timeout it is `E_BUSY` (exit 4, retryable), never a lock-free listing.
#[test]
#[ignore = "slow, ~10 s: waits out the store's 10 s lock timeout; just check-slow runs it"]
fn op_log_behind_a_held_lock_is_e_busy() {
    use std::time::Duration;

    use swarmops_store::MetaLock;

    use crate::support::Store;

    let store = Store::init();
    store.commit(AGENT, "a.txt", "a\n", None);
    let held = MetaLock::acquire(&store.layout(), Duration::from_secs(1)).expect("take meta/lock");
    let listed = store.read(&["op", "log"]);
    drop(held);
    assert!(listed.busy(), "{listed}");
    let error = listed.err("E_BUSY");
    assert_eq!(error["retryable"], true, "{listed}");
    assert!(
        error["hint"].as_str().unwrap().starts_with("retry"),
        "{listed}"
    );

    // The golden: the envelope without its `meta`, whose `elapsed_ms` is the wait.
    let mut envelope = listed.envelope();
    let meta = envelope.as_object_mut().expect("an object").remove("meta");
    assert!(meta.is_some(), "{listed}");
    insta::assert_json_snapshot!(envelope);
}

/// `op undo` through the binary takes the id `op log` prints and moves the refs that operation
/// moved back, in an operation of its own for the caller's agent, whose id the response gives
/// and another undo takes back; run again, it is a replay that writes nothing. An operation whose
/// ref was moved since is `E_CAS_MISMATCH` (exit 4) naming the ref, what it names and the
/// operation, with nothing written; one that moved no ref is `E_NOTHING_TO_UNDO`; without an
/// identity, an undo is refused as every mutation is.
#[test]
fn op_undo_end_to_end() {
    const UNDOER: &str = "agent/worker-18";
    let mut s = Session::new();
    let store = s.store();
    s.run(&["init", &store]);
    let c1 = s.commit(&store, None, "a.txt", "1\n", "c1"); // op 0
    let c2 = s.commit(&store, Some(&c1), "a.txt", "2\n", "c2"); // op 1
    let set = |s: &mut Session, new: &str, from: &[&str]| {
        let mut args = vec![
            "--repo",
            store.as_str(),
            "--as",
            AGENT,
            "ref",
            "set",
            "main",
            new,
        ];
        args.extend(from);
        assert_eq!(s.run(&args).exit, 0);
    };
    set(&mut s, &c1, &["--create"]); // op 2
    set(&mut s, &c2, &["--expect", &c1]); // op 3
    let undo = |s: &mut Session, op: &str| {
        s.run(&["--repo", store.as_str(), "--as", UNDOER, "op", "undo", op])
    };
    let main = |s: &mut Session| {
        let refs = s.run(&["--repo", store.as_str(), "ref", "list"]);
        refs["result"]["refs"][0]["target"].clone()
    };

    let op3 = Session::op_id(&store, 3);
    let undone = undo(&mut s, &op3);
    assert_eq!(
        (undone.exit, &undone["op"]),
        (0, &json!("undo")),
        "{}",
        undone.json
    );
    let at = undone["result"]["ref_moves"][0]["at"].clone();
    let op4 = Session::op_id(&store, 4);
    assert_eq!(
        undone["result"],
        json!({
            "undone": 3,
            "op": 4,
            "id": op4,
            "ref_moves": [{"name": "main", "old": c2, "new": c1, "actor": UNDOER, "at": at}],
            "already_applied": false,
        })
    );
    assert!(undone["meta"].get("recovered").is_none(), "{}", undone.json);
    assert_eq!(main(&mut s), c1.as_str());
    let again = undo(&mut s, &op3);
    assert_eq!(
        again["result"],
        json!({"undone": 3, "ref_moves": [], "already_applied": true})
    );
    let listed = s.run(&["--repo", store.as_str(), "op", "log", "-n", "1"]);
    let newest = &listed["result"]["ops"][0];
    assert_eq!(
        (&newest["op"], &newest["kind"]),
        (&json!(4), &json!("ref_move"))
    );
    assert_eq!(newest["ref_moves"], undone["result"]["ref_moves"]);
    // The undo is an operation of its own, and undoing it moves main forward again.
    let redone = undo(&mut s, &op4);
    assert_eq!(redone["result"]["op"], 5);
    assert_eq!(main(&mut s), c2.as_str());

    // Moved since: main names c3 now, neither what operation 3 set nor what it found.
    let c3 = s.commit(&store, Some(&c2), "a.txt", "3\n", "c3"); // op 6
    set(&mut s, &c3, &["--expect", &c2]); // op 7
    let blocked = undo(&mut s, &op3);
    assert_eq!((blocked.exit, blocked.code()), (4, "E_CAS_MISMATCH"));
    assert_eq!(
        blocked["error"]["details"],
        json!({"name": "main", "actual": c3, "op": 3})
    );
    let message = blocked["error"]["message"].as_str().unwrap();
    assert!(message.contains("newest first"), "{message}");
    assert_eq!(blocked["error"]["hint"], "swarmops op log");
    assert_eq!(main(&mut s), c3.as_str());
    let listed = s.run(&["--repo", store.as_str(), "op", "log", "-n", "1"]);
    assert_eq!(listed["result"]["ops"][0]["op"], 7, "nothing written");

    let op6 = Session::op_id(&store, 6);
    let nothing = undo(&mut s, &op6);
    assert_eq!((nothing.exit, nothing.code()), (1, "E_NOTHING_TO_UNDO"));
    assert_eq!(nothing["error"]["details"], json!({"op": 6}));
    let op7 = Session::op_id(&store, 7);
    let anonymous = s.run(&["--repo", store.as_str(), "op", "undo", &op7]);
    assert_eq!(
        (anonymous.exit, anonymous.code()),
        (5, "E_IDENTITY_REQUIRED")
    );
    assert_eq!(main(&mut s), c3.as_str());
    assert_eq!(
        s.run(&["--repo", store.as_str(), "verify", "--deep"]).exit,
        0
    );
}

/// `op undo` refuses an id it cannot trust, writing nothing: one past the last operation or
/// whose crc is not the frame's at its number is `E_OP_NOT_FOUND` (exit 6) with the reason, a
/// bare number `E_INVALID_ARG` (exit 2) naming it.
#[test]
fn op_undo_refuses_bad_ids() {
    let mut s = Session::new();
    let store = s.store();
    s.run(&["init", &store]);
    let c1 = s.commit(&store, None, "a.txt", "1\n", "c1"); // op 0
    let created = s.run(&[
        "--repo",
        &store,
        "--as",
        AGENT,
        "ref",
        "set",
        "release/r",
        &c1,
        "--create",
    ]); // op 1
    assert_eq!(created.exit, 0, "{}", created.json);
    let undo = |s: &mut Session, id: &str| {
        s.run(&["--repo", store.as_str(), "--as", AGENT, "op", "undo", id])
    };

    // Past the last operation.
    let missing = undo(&mut s, "99:00000000");
    assert_eq!((missing.exit, missing.code()), (6, "E_OP_NOT_FOUND"));
    assert_eq!(
        missing["error"]["details"],
        json!({"op": 99, "id": "99:00000000", "last": 1, "reason": "past_end"})
    );

    // The right number with another crc: the message never names the real id.
    let op1 = Session::op_id(&store, 1);
    let mistyped = if op1.ends_with('0') {
        "1:00000001"
    } else {
        "1:00000000"
    };
    let other = undo(&mut s, mistyped);
    assert_eq!((other.exit, other.code()), (6, "E_OP_NOT_FOUND"));
    assert_eq!(other["error"]["details"]["reason"], "checksum");
    let message = other["error"]["message"].as_str().unwrap();
    assert!(!message.contains(&op1), "{message}");
    let said = format!(
        "there is no operation {mistyped}: the operation at its number has another crc, so the \
         one named was taken back and its number went to another, or the id is mistyped (the \
         last is 1)"
    );
    assert_eq!(message, said);

    // A bare number, and an upper-case crc: the CLI passes the id on as given, and the
    // operation refuses it.
    for id in ["1", "1:9F3A01C2"] {
        let refused = undo(&mut s, id);
        assert_eq!((refused.exit, refused.code()), (2, "E_INVALID_ARG"), "{id}");
        assert_eq!(refused["error"]["details"], json!({"id": id}));
    }

    let listed = s.run(&["--repo", store.as_str(), "op", "log", "-n", "1"]);
    assert_eq!(listed["result"]["ops"][0]["op"], 1, "nothing written");
}

/// The agent whose publish main gains while a change waits to land, in the land tests.
const OTHER: &str = "agent/worker-18";

/// Publishes `content` at `path` on `parent` as [`OTHER`], moving main to it (`commit
/// --advance`); the changeset's id, named `[name]`.
fn publish_as_other(
    s: &mut Session,
    store: &str,
    parent: &str,
    (path, content): (&str, &str),
    name: &str,
) -> String {
    let changes = s.file(
        &format!("{name}.json"),
        &json!([{"path": path, "op": "put", "content": {"inline": content}}]).to_string(),
    );
    let args = [
        "--repo",
        store,
        "--as",
        OTHER,
        "commit",
        "-m",
        name,
        "--changes",
        changes.as_str(),
        "--parent",
        parent,
        "--advance",
        "main",
    ];
    let published = s.run(&args);
    assert_eq!(published.exit, 0, "{}", published.json);
    s.name_at(&published, "/result/changeset", name)
}

/// What main names, read through `ref list`.
fn main_target(s: &mut Session, store: &str) -> Value {
    let refs = s.run(&["--repo", store, "ref", "list"]);
    let main = (refs["result"]["refs"].as_array().unwrap().iter()).find(|r| r["name"] == "main");
    main.map_or(Value::Null, |r| r["target"].clone())
}

/// `land` publishes a change committed on an older head: it merges the change three-way onto
/// main's head as one new changeset whose parent is that head, and moves main to it in one
/// transaction, which `op log` calls a land; both sides' edits of one file merge
/// (`auto_merged`). `meta.resolved` names what the head and main named; `show` names the lander
/// and the chain; and `--human` prints one line.
#[test]
fn land_merges_a_commit_onto_main_end_to_end() {
    let mut s = Session::new();
    let store = s.store();
    s.run(&["init", &store]);
    let t0 = s.commit(&store, None, "a.txt", "1\n2\n3\n4\n5\n", "t0");
    s.run(&[
        "--repo", &store, "--as", AGENT, "ref", "set", "main", &t0, "--create",
    ]);
    let head = s.commit(&store, Some(&t0), "a.txt", "one\n2\n3\n4\n5\n", "head");
    let theirs = publish_as_other(
        &mut s,
        &store,
        &t0,
        ("a.txt", "1\n2\n3\n4\nfive\n"),
        "theirs",
    );

    let landed = s.run(&["--repo", &store, "--as", AGENT, "land", &head]);
    assert_eq!(
        (landed.exit, &landed["op"]),
        (0, &json!("land")),
        "{}",
        landed.json
    );
    let id = landed["result"]["landed"].as_str().unwrap().to_owned();
    assert_eq!(
        landed["result"],
        json!({
            "status": "landed",
            "landed": id,
            "rebased_onto": theirs,
            "target": "main",
            "base": t0,
            "attempts": 1,
            "relocated": false,
            "files_changed": 1,
            "squashed_from": [head],
            "auto_merged": ["a.txt"],
        })
    );
    let resolved = landed["meta"]["resolved"].as_object().unwrap();
    assert_eq!(resolved.len(), 2, "{}", landed.json);
    assert_eq!(
        (&resolved[&head], &resolved["main"]),
        (&json!(head), &json!(theirs))
    );
    assert_eq!(main_target(&mut s, &store), id.as_str());
    let cat = s.run(&["--repo", &store, "cat", "main:a.txt"]);
    assert_eq!(cat["result"]["content"], "one\n2\n3\n4\nfive\n");
    let shown = s.run(&["--repo", &store, "show", &id]);
    assert_eq!(shown["result"]["parents"], json!([theirs]));
    assert_eq!(shown["result"]["lander"], json!({"agent": AGENT}));
    assert_eq!(shown["result"]["squashed_from"], json!([head]));
    assert!(shown["result"].get("land_key").is_none(), "{}", shown.json);
    let newest = s.run(&["--repo", &store, "op", "log", "-n", "1"]);
    assert_eq!(
        newest["result"]["ops"][0]["kind"], "land",
        "{}",
        newest.json
    );

    let next = s.commit(&store, Some(&id), "b.txt", "b\n", "next");
    let args: [&str; 7] = ["--human", "--repo", &store, "--as", AGENT, "land", &next];
    let human = Session::run_raw(&[], &args, None);
    assert_eq!(human.exit, 0, "{}", human.stdout);
    let again = main_target(&mut s, &store);
    let again = again.as_str().unwrap();
    assert_eq!(
        human.stdout,
        format!("landed {again} on main (onto {id}, attempt 1)\n")
    );
    assert_eq!(s.run(&["--repo", &store, "verify", "--deep"]).exit, 0);
}

/// A land whose change conflicts with what main gained exits 3 with `E_CONFLICT`, not
/// retryable, and the conflict report in `details.report`; it records nothing.
/// `--max-conflict-bytes` bounds the report's hunk texts, and `--human` prints each conflicting
/// path with its kind, one a line.
#[test]
fn a_conflicting_land_exits_3_with_the_report() {
    let mut s = Session::new();
    let store = s.store();
    s.run(&["init", &store]);
    let t0 = s.commit(&store, None, "f.txt", "1\n2\n3\n", "t0");
    s.run(&[
        "--repo", &store, "--as", AGENT, "ref", "set", "main", &t0, "--create",
    ]);
    let head = s.commit(&store, Some(&t0), "f.txt", "1\nours\n3\n", "head");
    let theirs = publish_as_other(&mut s, &store, &t0, ("f.txt", "1\ntheirs\n3\n"), "theirs");

    let land: [&str; 6] = ["--repo", &store, "--as", AGENT, "land", &head];
    let conflict = s.run(&land);
    assert_eq!(
        (conflict.exit, conflict.code()),
        (3, "E_CONFLICT"),
        "{}",
        conflict.json
    );
    assert_eq!(conflict["error"]["retryable"], false);
    let blob = |text: &str| hash_object(ObjectKind::Blob, text.as_bytes()).to_hex();
    let side = |line: u64, text: &str| json!({"first": line, "last": line, "text": text});
    let report = json!({
        "base": t0,
        "ours": head,
        "theirs": theirs,
        "files": [{
            "path": "f.txt",
            "kind": "content",
            "base_blob": blob("1\n2\n3\n"),
            "ours_blob": blob("1\nours\n3\n"),
            "theirs_blob": blob("1\ntheirs\n3\n"),
            "hunks": [{"id": 0, "base": side(2, "2\n"), "ours": side(2, "ours\n"),
                       "theirs": side(2, "theirs\n")}],
            "theirs_changeset": theirs,
            "theirs_agent": OTHER,
        }],
        "kinds": ["content"],
        "truncated": false,
    });
    assert_eq!(conflict["error"]["details"], json!({ "report": report }));
    let hint = format!(
        "swarmops cat {theirs}:f.txt, then swarmops land {head} --resolve <file> (resolutions over \
         MCP) with each path's resolution and against = {theirs}"
    );
    assert_eq!(conflict["error"]["hint"], hint.as_str());
    assert_eq!(
        main_target(&mut s, &store),
        theirs.as_str(),
        "nothing recorded"
    );

    let cut = s.run(&[&land[..], &["--max-conflict-bytes", "0"][..]].concat());
    let cut = &cut["error"]["details"]["report"];
    assert_eq!(cut["truncated"], true);
    assert_eq!(
        cut["files"][0]["hunks"][0]["ours"],
        json!({"first": 2, "last": 2})
    );

    let human = Session::run_raw(&[], &[&["--human"][..], &land[..]].concat(), None);
    assert_eq!(human.exit, 3, "{}", human.stdout);
    let message = conflict["error"]["message"].as_str().unwrap();
    assert_eq!(
        human.stdout,
        format!("error: E_CONFLICT: {message}\nconflict: content f.txt\nhint: {hint}\n")
    );
}

/// A land run again with its `--key` finds its changeset on main and replays it: the same
/// result but `status` `replayed`, and nothing written (`op log` as it was); `--human` prints
/// `replayed <id>`. Run again without the key, it finds the change on main already:
/// `E_NOTHING_TO_LAND` (exit 1), `reason` `no_changes`.
#[test]
fn a_replayed_land_reports_replayed() {
    let mut s = Session::new();
    let store = s.store();
    s.run(&["init", &store]);
    let t0 = s.commit(&store, None, "a.txt", "a\n", "t0");
    s.run(&[
        "--repo", &store, "--as", AGENT, "ref", "set", "main", &t0, "--create",
    ]);
    let head = s.commit(&store, Some(&t0), "b.txt", "b\n", "head");
    let land: [&str; 8] = [
        "--repo", &store, "--as", AGENT, "land", &head, "--key", "land-7",
    ];
    let first = s.run(&land);
    assert_eq!(first.exit, 0, "{}", first.json);
    let newest = |s: &mut Session| {
        let listed = s.run(&["--repo", &store, "op", "log", "-n", "1"]);
        listed["result"]["ops"][0]["op"].clone()
    };
    let before = newest(&mut s);

    let again = s.run(&land);
    let mut replayed = first["result"].clone();
    replayed["status"] = json!("replayed");
    assert_eq!(again["result"], replayed, "{}", again.json);
    assert_eq!(newest(&mut s), before, "a replay writes nothing");
    let human = Session::run_raw(&[], &[&["--human"][..], &land[..]].concat(), None);
    let id = first["result"]["landed"].as_str().unwrap();
    assert_eq!((human.exit, human.stdout), (0, format!("replayed {id}\n")));

    let unkeyed = s.run(&land[..6]);
    assert_eq!((unkeyed.exit, unkeyed.code()), (1, "E_NOTHING_TO_LAND"));
    assert_eq!(unkeyed["error"]["details"], json!({"reason": "no_changes"}));
    assert_eq!(newest(&mut s), before);
}

/// A land that conflicts is resolved without a checkout: from the report of its `E_CONFLICT`
/// (exit 3), resolutions against the report's `theirs`, read by the client from a file
/// (`--resolve FILE`) or from stdin (`--resolve -`). Resolving one of two conflicting files exits
/// 3 again with the other; a `res.json` that resolves both lands, with the resolved contents. A
/// resolution file that cannot be read, or holds no resolutions, is `E_INVALID_ARG` (exit 2)
/// naming it.
#[test]
fn a_conflicting_land_resolved_with_a_file_lands() {
    let mut s = Session::new();
    let store = s.store();
    s.run(&["init", &store]);
    let files = json!([
        {"path": "f.txt", "op": "put", "content": {"inline": "1\n2\n3\n"}},
        {"path": "g.txt", "op": "put", "content": {"inline": "1\n2\n3\n"}},
    ]);
    let t0 = s.file("t0.json", &files.to_string());
    let base = s.run(&[
        "--repo",
        &store,
        "--as",
        AGENT,
        "commit",
        "-m",
        "t0",
        "--changes",
        &t0,
        "--advance",
        "main",
    ]);
    let t0 = s.name_at(&base, "/result/changeset", "t0");
    let ours = json!([
        {"path": "f.txt", "op": "put", "content": {"inline": "1\nours\n3\n"}},
        {"path": "g.txt", "op": "put", "content": {"inline": "1\nours\n3\n"}},
    ]);
    let ours = s.file("ours.json", &ours.to_string());
    let made = s.run(&[
        "--repo",
        &store,
        "--as",
        AGENT,
        "commit",
        "-m",
        "head",
        "--changes",
        &ours,
        "--parent",
        &t0,
    ]);
    let head = s.name_at(&made, "/result/changeset", "head");
    let theirs = publish_as_other(&mut s, &store, &t0, ("f.txt", "1\ntheirs\n3\n"), "x");
    let theirs = publish_as_other(
        &mut s,
        &store,
        &theirs,
        ("g.txt", "1\ntheirs\n3\n"),
        "theirs",
    );

    let land: [&str; 6] = ["--repo", &store, "--as", AGENT, "land", &head];
    let conflict = s.run(&land);
    assert_eq!((conflict.exit, conflict.code()), (3, "E_CONFLICT"));
    let report = &conflict["error"]["details"]["report"];
    assert_eq!(report["theirs"], theirs.as_str());
    let resolve = |files: Value| json!({"against": report["theirs"], "files": files}).to_string();

    let one = resolve(json!({"f.txt": {"take": "ours"}}));
    let partly = s.run_stdin(&one, &[&land[..], &["--resolve", "-"][..]].concat());
    assert_eq!(
        (partly.exit, partly.code()),
        (3, "E_CONFLICT"),
        "{}",
        partly.json
    );
    let left = &partly["error"]["details"]["report"];
    assert_eq!(left["files"].as_array().unwrap().len(), 1, "{left}");
    assert_eq!(left["files"][0]["path"], "g.txt");
    assert!(left.get("stale").is_none(), "{left}");

    let both = resolve(json!({
        "f.txt": {"take": "ours"},
        "g.txt": {"content": {"inline": "1\nours and theirs\n3\n"}},
    }));
    let res = s.file("res.json", &both);
    let landed = s.run(&[&land[..], &["--resolve", &res][..]].concat());
    assert_eq!(landed.exit, 0, "{}", landed.json);
    assert_eq!(landed["result"]["rebased_onto"], theirs.as_str());
    assert_eq!(landed["result"]["files_changed"], 2);
    for (path, content) in [
        ("f.txt", "1\nours\n3\n"),
        ("g.txt", "1\nours and theirs\n3\n"),
    ] {
        let cat = s.run(&["--repo", &store, "cat", &format!("main:{path}")]);
        assert_eq!(cat["result"]["content"], content, "{path}");
    }

    let missing = format!("{}/missing.json", s.tmp());
    let unread = s.run(&[&land[..], &["--resolve", &missing][..]].concat());
    assert_eq!((unread.exit, unread.code()), (2, "E_INVALID_ARG"));
    assert_eq!(unread["error"]["details"], json!({"resolve": missing}));
    let not_resolutions = s.run_stdin("[]", &[&land[..], &["--resolve", "-"][..]].concat());
    assert_eq!(
        (not_resolutions.exit, not_resolutions.code()),
        (2, "E_INVALID_ARG")
    );
    assert_eq!(not_resolutions["error"]["details"], json!({"resolve": "-"}));
    let hint = not_resolutions["error"]["hint"].as_str().unwrap();
    assert!(hint.contains("swarmops schema ResolutionsInput"), "{hint}");
}

// ---------------------------------------------------------------------------------------------
// Golden steps of the commands the tests above show no success of.
/// `blob put -` reads its stdin whatever it is, a socket among them, which no path opens: the
/// bytes a peer sends before it shuts its end are stored, as from a pipe.
#[test]
fn blob_put_reads_a_socket_stdin() {
    use std::io::Write as _;
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use std::process::Stdio;

    let s = Session::new();
    let store = s.store();
    let init = Session::run_raw(&[], &["init", &store], None);
    assert_eq!(init.exit, 0, "{}", init.stderr);
    let content = b"through a socket\n";
    let (mut ours, theirs) = UnixStream::pair().unwrap();
    ours.write_all(content).unwrap();
    ours.shutdown(std::net::Shutdown::Write).unwrap();

    let output = support::swarmops(&["--repo", &store, "--as", AGENT, "blob", "put", "-"], &[])
        .stdin(Stdio::from(OwnedFd::from(theirs)))
        .output()
        .unwrap();
    let printed: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{printed}");
    let id = hash_object(ObjectKind::Blob, content).to_hex();
    assert_eq!(
        printed["result"],
        json!({"blob": id, "size": content.len(), "written": true})
    );
}

// ---------------------------------------------------------------------------------------------

/// `blob put` stores a file's bytes, or stdin's, as a blob before a commit names it, and says
/// whether it wrote them; `blob has` lists which of the ids given the store holds.
#[test]
fn blob_put_has_golden() {
    let mut s = Session::new();
    let store = s.store();
    let init = s.run(&["init", &store]);
    s.name_at(&init, "/result/repo_id", "repo_id");

    // 1. A file's bytes are written; the same bytes from stdin are there already.
    let content = "named by id\n";
    let file = s.file("payload.txt", content);
    let put = s.run(&["--repo", &store, "--as", AGENT, "blob", "put", &file]);
    assert_eq!(put.exit, 0, "{}", put.json);
    let blob = s.name_at(&put, "/result/blob", "blob");
    let again = s.run_stdin(
        content,
        &["--repo", &store, "--as", AGENT, "blob", "put", "-"],
    );
    assert_eq!(again["result"]["written"], false, "{}", again.json);

    // 2. Of two ids, the stored one is present; it needs no identity.
    let missing = hash_object(ObjectKind::Blob, b"never stored\n").to_hex();
    s.name(&missing, "missing");
    let has = s.run(&["--repo", &store, "blob", "has", &blob, &missing]);
    assert_eq!(has["result"], json!({"present": [blob]}));

    insta::assert_json_snapshot!(s.transcript());
}

/// `op log` lists the store's operations newest first, with the changesets each recorded and
/// the refs it moved; `op undo` moves a move back, in an operation of its own.
#[test]
fn op_log_undo_golden() {
    let mut s = Session::new();
    let store = s.store();
    let init = s.run(&["init", &store]);
    s.name_at(&init, "/result/repo_id", "repo_id");

    // 1. Four operations: a commit, main's creation, a commit, main's move to it.
    let c1 = s.commit(&store, None, "a.txt", "a\n", "c1");
    let create = [
        "--repo", &store, "--as", AGENT, "ref", "set", "main", &c1, "--create",
    ];
    assert_eq!(s.run(&create).exit, 0);
    let c2 = s.commit(&store, Some(&c1), "b.txt", "b\n", "c2");
    let mv = [
        "--repo", &store, "--as", AGENT, "ref", "set", "main", &c2, "--expect", &c1,
    ];
    assert_eq!(s.run(&mv).exit, 0);

    // 2. The log, then the undo of the move (operation 3), then the newest operation.
    let log = s.run(&["--repo", &store, "op", "log"]);
    assert_eq!(log["result"]["ops"][0]["op"], 3, "{}", log.json);
    let op3 = log["result"]["ops"][0]["id"].as_str().unwrap().to_owned();
    let undo = s.run(&["--repo", &store, "--as", AGENT, "op", "undo", &op3]);
    assert_eq!(undo.exit, 0, "{}", undo.json);
    let newest = s.run(&["--repo", &store, "op", "log", "-n", "1"]);
    assert_eq!(newest["result"]["ops"][0]["op"], 4, "{}", newest.json);

    insta::assert_json_snapshot!(s.transcript());
}

/// `derive` derives the eager values of what the journal records and says where to go on
/// (`next`); after that frame nothing is left.
#[test]
fn derive_golden() {
    let mut s = Session::new();
    let store = s.store();
    let init = s.run(&["init", &store]);
    s.name_at(&init, "/result/repo_id", "repo_id");
    let c1 = s.commit(&store, None, "f.txt", "1\n", "c1");
    s.commit(&store, Some(&c1), "f.txt", "2\n", "c2");

    // Once from the first frame, then after the frame it gave as next.
    let first = s.run(&["--repo", &store, "derive"]);
    assert_eq!(
        first["result"],
        json!({"derived": 2, "cached": 0, "next": 1})
    );
    let rest = s.run(&["--repo", &store, "derive", "--after", "1"]);
    assert_eq!(rest["result"], json!({"derived": 0, "cached": 0}));

    insta::assert_json_snapshot!(s.transcript());
}

/// `land`, each way it ends: a change merged onto what main gained since, with `--key`; the
/// same land again, a replay; a land that conflicts, exit 3 with the report; and that land
/// again with `--resolve`, a file of resolutions against the report's `theirs`.
#[test]
fn land_golden() {
    let mut s = Session::new();
    let store = s.store();
    let init = s.run(&["init", &store]);
    s.name_at(&init, "/result/repo_id", "repo_id");

    // 1. main at t0; a change on t0, and another agent's publish on main since.
    let t0 = s.file(
        "t0.json",
        &json!([{"path": "f.txt", "op": "put", "content": {"inline": "1\n2\n3\n"}}]).to_string(),
    );
    let base = s.run(&[
        "--repo",
        &store,
        "--as",
        AGENT,
        "commit",
        "-m",
        "t0",
        "--changes",
        &t0,
        "--advance",
        "main",
    ]);
    let t0 = s.name_at(&base, "/result/changeset", "t0");
    let head = s.commit(&store, Some(&t0), "b.txt", "b\n", "head");
    publish_as_other(&mut s, &store, &t0, ("c.txt", "c\n"), "theirs");

    // 2. The land, then the same land again.
    let land: [&str; 8] = [
        "--repo", &store, "--as", AGENT, "land", &head, "--key", "land-7",
    ];
    let landed = s.run(&land);
    assert_eq!(landed.exit, 0, "{}", landed.json);
    let l1 = s.name_at(&landed, "/result/landed", "landed");
    let replayed = s.run(&land);
    assert_eq!(
        replayed["result"]["status"], "replayed",
        "{}",
        replayed.json
    );

    // 3. A change to f.txt's line 2, which another agent changed on main too: a conflict.
    let ours = s.commit(&store, Some(&l1), "f.txt", "1\nours\n3\n", "ours");
    let theirs = publish_as_other(&mut s, &store, &l1, ("f.txt", "1\ntheirs\n3\n"), "theirs2");
    let conflict: [&str; 6] = ["--repo", &store, "--as", AGENT, "land", &ours];
    let refused = s.run(&conflict);
    assert_eq!((refused.exit, refused.code()), (3, "E_CONFLICT"));

    // 4. Resolved from a file: the line both sides changed, as one.
    let resolutions = json!({
        "against": theirs,
        "files": {"f.txt": {"content": {"inline": "1\nours and theirs\n3\n"}}},
    });
    let res = s.file("res.json", &resolutions.to_string());
    let resolved = s.run(&[&conflict[..], &["--resolve", &res][..]].concat());
    assert_eq!(resolved.exit, 0, "{}", resolved.json);
    assert_eq!(resolved["result"]["rebased_onto"], theirs.as_str());
    s.name_at(&resolved, "/result/landed", "resolved");

    insta::assert_json_snapshot!(s.transcript());
}

/// A land's conflicts other than `content`: a file the change deleted and main changed
/// (`delete_modify`), one the change changed and main deleted (`modify_delete`), and a file
/// holding a NUL both changed (`binary`). Each names its blobs, and none has hunks.
#[test]
fn land_conflict_kinds_golden() {
    let mut s = Session::new();
    let store = s.store();
    let init = s.run(&["init", &store]);
    s.name_at(&init, "/result/repo_id", "repo_id");

    // 1. main at t0, holding the three files.
    let t0 = s.file(
        "t0.json",
        &json!([
            {"path": "gone.txt", "op": "put", "content": {"inline": "gone\n"}},
            {"path": "kept.txt", "op": "put", "content": {"inline": "kept\n"}},
            {"path": "logo.bin", "op": "put", "content": {"base64": "AAE="}},
        ])
        .to_string(),
    );
    let base = s.run(&[
        "--repo",
        &store,
        "--as",
        AGENT,
        "commit",
        "-m",
        "t0",
        "--changes",
        &t0,
        "--advance",
        "main",
    ]);
    let t0 = s.name_at(&base, "/result/changeset", "t0");

    // 2. The change: gone.txt deleted, kept.txt and logo.bin changed.
    let ours = s.file(
        "ours.json",
        &json!([
            {"path": "gone.txt", "op": "delete"},
            {"path": "kept.txt", "op": "put", "content": {"inline": "ours\n"}},
            {"path": "logo.bin", "op": "put", "content": {"base64": "AAI="}},
        ])
        .to_string(),
    );
    let change = s.run(&[
        "--repo",
        &store,
        "--as",
        AGENT,
        "commit",
        "-m",
        "ours",
        "--changes",
        &ours,
        "--parent",
        &t0,
    ]);
    let ours = s.name_at(&change, "/result/changeset", "ours");

    // 3. Another agent on main: gone.txt and logo.bin changed, kept.txt deleted.
    let theirs = s.file(
        "theirs.json",
        &json!([
            {"path": "gone.txt", "op": "put", "content": {"inline": "theirs\n"}},
            {"path": "kept.txt", "op": "delete"},
            {"path": "logo.bin", "op": "put", "content": {"base64": "AAM="}},
        ])
        .to_string(),
    );
    let published = s.run(&[
        "--repo",
        &store,
        "--as",
        OTHER,
        "commit",
        "-m",
        "theirs",
        "--changes",
        &theirs,
        "--parent",
        &t0,
        "--advance",
        "main",
    ]);
    s.name_at(&published, "/result/changeset", "theirs");

    // 4. The land conflicts on all three files.
    let refused = s.run(&["--repo", &store, "--as", AGENT, "land", &ours]);
    assert_eq!((refused.exit, refused.code()), (3, "E_CONFLICT"));
    let kinds = &refused["error"]["details"]["report"]["kinds"];
    assert_eq!(
        *kinds,
        json!(["binary", "delete_modify", "modify_delete"]),
        "{}",
        refused.json
    );

    insta::assert_json_snapshot!(s.transcript());
}

/// `git export` writes main's history into a git repository, made here without git, and
/// `git import` brings that repository's history into a second store; each run again writes
/// only what is new.
#[test]
fn git_export_import_golden() {
    let mut s = Session::new();
    let store = s.store();
    let init = s.run(&["init", &store]);
    s.name_at(&init, "/result/repo_id", "repo_id");
    let git = swarmops_testkit::git::empty_repository();
    let repo = git.dir.to_str().expect("UTF-8 temp paths").to_owned();
    s.name(&repo, "git");

    // 1. main at c1, exported; then main at c2, exported again: c2 alone is written.
    let c1 = s.commit(&store, None, "README.md", "# acme\n", "c1");
    let create = [
        "--repo", &store, "--as", AGENT, "ref", "set", "main", &c1, "--create",
    ];
    assert_eq!(s.run(&create).exit, 0);
    let first = s.run(&["--repo", &store, "git", "export", &repo]);
    assert_eq!(first.exit, 0, "{}", first.json);
    let c2 = s.commit(&store, Some(&c1), "src/lib.rs", "pub fn f() {}\n", "c2");
    let mv = [
        "--repo", &store, "--as", AGENT, "ref", "set", "main", &c2, "--expect", &c1,
    ];
    assert_eq!(s.run(&mv).exit, 0);
    let second = s.run(&["--repo", &store, "git", "export", &repo]);
    assert_eq!(second.exit, 0, "{}", second.json);

    // 2. A second store imports the repository; run again, it writes nothing.
    let copy = format!("{}/copy", s.tmp());
    let made = s.run(&["init", &copy]);
    s.name_at(&made, "/result/repo_id", "repo_id:copy");
    let import = ["--repo", &copy, "--as", AGENT, "git", "import", &repo];
    let imported = s.run(&import);
    assert_eq!(imported.exit, 0, "{}", imported.json);
    s.name_at(&imported, "/result/head_changeset", "imported");
    let again = s.run(&import);
    assert_eq!(again.exit, 0, "{}", again.json);

    insta::assert_json_snapshot!(s.transcript());
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

    /// The id `op log` lists for operation `op` of `store`; records nothing.
    fn op_id(store: &str, op: u64) -> String {
        let before = (op + 1).to_string();
        let args = ["--repo", store, "op", "log", "--before", &before, "-n", "1"];
        let output = Session::run_raw(&[], &args, None);
        let listed: Value = serde_json::from_str(&output.stdout).expect("an envelope");
        let id = listed
            .pointer("/result/ops/0/id")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("no operation {op}: {listed}"))
            .to_owned();
        assert!(id.starts_with(&format!("{op}:")), "{listed}");
        id
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

    /// Runs `serve` with `env` and `args`, refused before git starts: one `ERR` pkt-line on
    /// stdout, which git shows as `remote error:`, and the envelope on stderr. Checks both
    /// ([`check`]) and records the step, the envelope as its `stderr`.
    fn serve(&mut self, env: &[(&str, &str)], args: &[&str]) -> Reply {
        let output = Session::run_raw(env, args, None);

        // 1. stdout: one pkt-line, its four hex digits counting themselves, then the envelope.
        let len = output
            .stdout
            .get(..4)
            .and_then(|digits| usize::from_str_radix(digits, 16).ok());
        assert_eq!(len, Some(output.stdout.len()), "one pkt-line: {args:?}");
        assert!(output.stdout[4..].starts_with("ERR "), "{args:?}");
        let envelope = Output {
            exit: output.exit,
            stdout: output.stderr.clone(),
            stderr: String::new(),
        };
        let json = check(args, &envelope);
        let code = json["error"]["code"].as_str().unwrap_or_default();
        let message = json["error"]["message"].as_str().unwrap_or_default();
        assert_eq!(output.stdout[8..], format!("{code}: {message}"), "{args:?}");

        // 2. The step, its stdout the pkt-line as text.
        let mut step = vec![("args".to_owned(), Json::String(shell_words(args)))];
        let vars = env
            .iter()
            .map(|(key, value)| ((*key).to_owned(), Json::String((*value).to_owned())))
            .collect();
        step.push(("env".to_owned(), Json::Object(vars)));
        step.push(("exit".to_owned(), Json::Number(output.exit.into())));
        step.push(("stdout".to_owned(), Json::String(output.stdout)));
        let printed: Json = serde_json::from_str(&output.stderr).expect("checked above");
        step.push(("stderr".to_owned(), printed.clone()));
        self.steps.push(Json::Object(step));
        Reply {
            exit: output.exit,
            json,
            printed,
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

    /// `text` with each named value replaced by its name, each other operation id's checksum by
    /// `[crc]`, then each other run of 32 or more lowercase hex digits by `[id<n>]`.
    fn redact_str(&mut self, text: &str) -> String {
        let mut text = text.to_owned();
        for (value, name) in &self.named {
            text = text.replace(value.as_str(), name);
        }
        let text = support::redact_crcs(&text);
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
    /// `--after` past the journal's last frame is refused, with `--follow` too, at once.
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

        // A frame past the journal's last is refused, once or followed: no run said it.
        for past in [
            &["derive", "--after", "2"][..],
            &["derive", "--follow", "--after", "2"],
        ] {
            let run = store.read(past);
            assert_eq!(run.exit(), 2, "{run}");
            let error = run.err("E_INVALID_ARG");
            assert_eq!(error["details"]["after"], 2, "{run}");
        }

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

/// Crafted derived values (storage spec §5, `meta/derived/`): a value file someone placed in a
/// store, whole and with its trailer right (a put of any bytes writes one), holding a value no
/// derivation gives. `verify --deep` reports each such value as a `derived_mismatch` finding,
/// naming the derivation, the changeset and the key, where it found the store clean before; a
/// plain `verify` reads no derived value. `history` and `log --path` refuse a last touch whose
/// changeset does not touch the path, and `resolve` a skip list whose jump disagrees with the list
/// one level down, as `E_STORE_CORRUPT` naming the derivation and the changeset, where they took
/// the value as valid before. Removing the file has the value derived again.
mod crafted_derived_values {
    use std::fs;
    use std::io;
    use std::sync::Arc;

    use camino::Utf8PathBuf;
    use serde_json::{Value, json};
    use swarmops_core::{ChangesetId, ObjectId};
    use swarmops_repo::{FirstParentSkipList, LocalRepo};
    use swarmops_store::{DerivedSlot, DerivedStore, RealFs};
    use swarmops_testkit::fs::NoSyncFs;
    use swarmops_testkit::repo::{put, request};

    use crate::support::Store;

    /// The agent the commits act as.
    const WRITER: &str = "agent/worker-19";

    /// Changeset `hex`, by its id.
    fn id(hex: &str) -> ChangesetId {
        ChangesetId::new(ObjectId::from_hex(hex).expect("a changeset id"))
    }

    /// Places `value` in `slot` of `store`'s derived values in place of whatever file is there,
    /// as a put writes it, trailer and all: what anyone with the store's files can write.
    fn plant(store: &Store, slot: &DerivedSlot<'_>, value: &[u8]) {
        let layout = store.layout();
        match fs::remove_file(layout.derived_path(slot)) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => panic!("remove the value file: {e}"),
        }
        let derived = DerivedStore::open(layout, Arc::new(RealFs::new()));
        assert!(derived.put(slot, value).expect("plant a value"), "planted");
    }

    /// The `changeset` of each entry of `history`'s result, or the `id` of each of `log`'s
    /// (`key`).
    fn ids(result: &Value, key: &str) -> Vec<String> {
        let entries = result["entries"].as_array().expect("entries");
        let id = |entry: &Value| entry[key].as_str().expect("an id").to_owned();
        entries.iter().map(id).collect()
    }

    /// Asserts that `refused` failed with `E_STORE_CORRUPT`, naming derivation `name` and
    /// changeset `cs`, its hint a deep verify.
    #[track_caller]
    fn refused(refused: &crate::support::Run, name: &str, cs: &str) {
        let error = refused.err("E_STORE_CORRUPT");
        assert_eq!(error["details"]["derivation"], name, "{error}");
        assert_eq!(error["details"]["changeset"], cs, "{error}");
        assert_eq!(error["hint"], "swarmops verify --deep", "{error}");
    }

    /// What `verify --deep` finds: no finding for a clean store, else its findings.
    fn findings(store: &Store) -> Value {
        let verify = store.read(&["verify", "--deep"]);
        match verify.exit() {
            0 => verify.ok()["findings"].take(),
            _ => verify.err("E_STORE_CORRUPT")["details"]["findings"].take(),
        }
    }

    /// `c2` leaves `a` alone, so the last changeset to touch it from `c2` is `c1`. A file placed
    /// in `c2`'s slot under the path `a` that says `c2` touched it last is reported by
    /// `verify --deep`, with the path key's hex; a plain `verify` passes it; and `history` and
    /// `log --path` refuse it, `c2`'s delta not touching `a`. Without the file, the history is
    /// right and the store clean again.
    #[test]
    fn a_crafted_last_touch_is_caught() {
        let store = Store::init();
        let c1 = store.commit(WRITER, "a", "1", None);
        let c2 = store.commit(WRITER, "b", "1", Some(&c1));
        let main = ["ref", "set", "main", &c2, "--create"];
        store.run_as(WRITER, &main, &[]).ok();
        let history = || store.read(&["history", "main:a"]);
        let log = || store.read(&["log", "--path", "a"]);
        assert_eq!(ids(&history().ok(), "changeset"), [c1.as_str()]);
        assert_eq!(findings(&store), json!([]));

        // The path key `a`, as postcard writes a string; the value `Some(c2)`.
        let key = [&[1_u8][..], b"a"].concat();
        let slot = DerivedSlot::new("last_modified", 1, id(&c2).0, &key);
        plant(&store, &slot, &[&[1_u8][..], &id(&c2).0.0].concat());

        let finding = json!({
            "kind": "derived_mismatch",
            "changeset": c2,
            "derivation": "last_modified",
            "key": "0161",
        });
        assert_eq!(findings(&store), json!([finding]));
        assert_eq!(store.read(&["verify"]).ok()["findings"], json!([]));
        refused(&history(), "last_modified", &c2);
        refused(&log(), "last_modified", &c2);

        fs::remove_file(store.layout().derived_path(&slot)).unwrap();
        assert_eq!(ids(&history().ok(), "changeset"), [c1.as_str()]);
        assert_eq!(ids(&log().ok(), "id"), [c1.as_str()]);
        assert_eq!(findings(&store), json!([]));
    }

    /// A first-parent chain of 1,101 changesets on `main` and a side branch of 40 off its 11th,
    /// committed through the library (nothing synced) into `store`: the chain's ids, its root
    /// first, and the side branch's.
    fn chain_and_side_branch(store: &Store) -> (Vec<ChangesetId>, Vec<ChangesetId>) {
        let root = Utf8PathBuf::from(store.path());
        let repo = LocalRepo::open_with_fs(&root, Arc::new(NoSyncFs)).unwrap();
        let commit = |parent: Option<ChangesetId>, message: &str| {
            let parents: Vec<ChangesetId> = parent.into_iter().collect();
            let changes = vec![put("f", message.as_bytes())];
            repo.commit(request(&parents, changes, message)).unwrap().id
        };
        let mut main: Vec<ChangesetId> = Vec::new();
        for i in 0..1_101 {
            main.push(commit(main.last().copied(), &format!("main {i}")));
        }
        let mut side: Vec<ChangesetId> = Vec::new();
        for i in 0..40 {
            let parent = side.last().copied().unwrap_or(main[10]);
            side.push(commit(Some(parent), &format!("side {i}")));
        }
        repo.cas_ref("main", None, main.last().copied(), WRITER)
            .unwrap();
        (main, side)
    }

    /// `main~1025`, past the 1,024 steps walked one at a time, lifts along the head's skip list:
    /// its entry 10, 1,024 steps back, then one step. A list placed for the head whose entry 10
    /// names a changeset of an old side branch (whose generation is low enough for the jump) is
    /// reported by `verify --deep`, without a key; and `resolve`, which named a changeset main's
    /// history never held with it, refuses it: the jump disagrees with entry 9 of the list of the
    /// head's entry 9, the derivation's own rule one level down.
    #[test]
    fn a_crafted_skip_list_is_caught() {
        let store = Store::init();
        let (main, side) = chain_and_side_branch(&store);
        let head = main[1_100];
        let resolve = || store.read(&["resolve", "main~1025"]);
        assert_eq!(resolve().ok()["id"], main[75].to_string().as_str());
        assert_eq!(findings(&store), json!([]));

        let root = Utf8PathBuf::from(store.path());
        let repo = LocalRepo::open(&root).unwrap();
        let mut list = repo
            .derived::<FirstParentSkipList>(head, &())
            .unwrap()
            .into_vec();
        drop(repo);
        assert_eq!((list.len(), list[10]), (11, main[76]), "the head's list");
        list[10] = side[30];
        // A skip list as postcard writes it: the number of ids, then each id's 32 bytes.
        let mut value = vec![u8::try_from(list.len()).unwrap()];
        for entry in &list {
            value.extend(entry.0.0);
        }
        let slot = DerivedSlot::new("first_parent_skip_list", 1, head.0, &[]);
        plant(&store, &slot, &value);

        let finding = json!({
            "kind": "derived_mismatch",
            "changeset": head.to_string(),
            "derivation": "first_parent_skip_list",
        });
        assert_eq!(findings(&store), json!([finding]));
        let refusal = resolve();
        let side_29 = side[29].to_string();
        assert_eq!(refusal.exit(), 1, "side[29] is {side_29}: {refusal}");
        refused(&refusal, "first_parent_skip_list", &head.to_string());

        fs::remove_file(store.layout().derived_path(&slot)).unwrap();
        assert_eq!(resolve().ok()["id"], main[75].to_string().as_str());
        assert_eq!(findings(&store), json!([]));
    }
}
