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
use swarmops_store::Layout;
use tempfile::TempDir;

use crate::support;

/// The agent most mutations act as.
const AGENT: &str = "agent/worker-17";

/// The keys whose numbers are times or durations, replaced by `[<key>]` in snapshots.
const TIME_KEYS: &[&str] = &["elapsed_ms", "created_at", "secs"];

/// A store path under `/mnt/`, where WSL mounts Windows drives: `init` refuses it by its name,
/// so it need not exist, and nothing is made there.
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

/// cat: text, base64, a file at an older revision, a symlink, a truncated read. A rev is
/// split from its path at the first ':', so a path may hold more.
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
    let colon = s.run(&["--repo", store, "cat", "main:no:such:file"]);
    assert_eq!(colon["error"]["details"]["path"], "no:such:file");
}

/// ls (the root without a path or with an empty one, a directory with or without a trailing
/// `/`, pages), show (by ref and by prefix) and log (from main, a page at a time).
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
/// `details`; a clean store is a success envelope with the report as `result`.
#[test]
fn verify_problems_exit_1_with_the_report() {
    let mut s = Session::new();
    let store = s.store();
    s.run(&["init", &store]);
    s.commit(&store, None, "f.txt", "content", "c1");
    for deep in [false, true] {
        let mut args = vec!["--repo", store.as_str(), "verify"];
        if deep {
            args.push("--deep");
        }
        let clean = s.run(&args);
        assert_eq!(clean.exit, 0);
        assert_eq!(clean["result"]["problems"], json!([]));
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
        assert!(
            problems[0].as_str().unwrap().contains(&blob.to_hex()),
            "{problems:?}"
        );
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
        ("E_IO", "needs a failing filesystem"),
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
    // On main's head, but main stays where it is: a commit moves no ref.
    let c2 = s.commit(&store, None, "src/main.rs", "fn main() {}\n", "c2");

    let mut raised = Vec::new();
    for (code, args) in error_cases(&s, &store, &c2) {
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

// ---------------------------------------------------------------------------------------------
// Running the binary.
// ---------------------------------------------------------------------------------------------

/// The binary, with none of this process's `SWARMOPS_*` variables set.
fn swarmops() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_swarmops"));
    for var in support::swarmops_vars() {
        command.env_remove(var);
    }
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
        Output {
            exit: output.status.code().expect("swarmops exited, not killed"),
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
