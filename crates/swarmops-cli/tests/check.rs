//! `check list` (M30-1, M30-2) through the binary: the same tree gives the same answer byte for
//! byte, a tree with no file lists nothing, the directory form says the caller vouched, the
//! one check `default` is the command `ws land --check` runs, and no link is followed.

use std::fs;
use std::path::Path;

use serde_json::{Value, json};

use crate::ws::{Lab, Out, inside, main_of, run_in, write};

/// The plan's fixture, and the same file with a key version 1 does not take on line 11.
const FIXTURE: &str = include_str!("fixtures/check/checks.toml");
const REFUSED: &str = include_str!("fixtures/check/refused.toml");

/// The `result` of `out`'s envelope as printed, byte for byte: what follows `"result":` up to
/// `,"meta":`, which the envelope writes after it.
fn result_text(out: &Out) -> String {
    assert_eq!(out.exit, 0, "{out}");
    let start = out.stdout.find(r#""result":"#).expect("a result") + r#""result":"#.len();
    let end = out.stdout.rfind(r#","meta":"#).expect("a meta");
    out.stdout[start..end].to_owned()
}

/// Commits `changes` (a JSON array) on main's head, as the lab's agent, and moves main there; the
/// changeset.
fn publish_changes(lab: &Lab, changes: &Value) -> String {
    let file = lab.beside("changes.json");
    fs::write(&file, changes.to_string()).unwrap();
    let before = main_of(lab);
    let line = ["commit", "-m", "c", "--changes", file.to_str().unwrap()];
    let committed = lab.so(&[&line[..], &["--parent", &before, "--advance", "main"]].concat());
    committed.ok()["changeset"].as_str().unwrap().to_owned()
}

/// `check list --dir dir`, run beside the store with no store named.
fn list_dir(lab: &Lab, dir: &Path) -> Out {
    run_in(
        lab.store.tmp(),
        &[],
        &["check", "list", "--dir", dir.to_str().unwrap()],
        None,
    )
}

/// Two changesets with the same root tree give the same `result`, byte for byte; so do two runs
/// of the directory form over the same files.
#[test]
fn the_same_tree_gives_the_same_answer() {
    let lab = Lab::new();
    lab.publish(".swarmops/checks.toml", FIXTURE);
    let first = main_of(&lab);
    lab.publish("README.md", "# changed\n");
    lab.publish("README.md", "# acme\n");
    let again = main_of(&lab);
    assert_ne!(first, again, "two changesets");

    let at_first = lab.so(&["check", "list", &first]);
    let at_again = lab.so(&["check", "list", &again]);
    assert_eq!(result_text(&at_first), result_text(&at_again));
    assert_eq!(at_first.ok()["checks"].as_array().unwrap().len(), 3);

    let dir = lab.beside("tree");
    write(&dir, ".swarmops/checks.toml", FIXTURE);
    let once = list_dir(&lab, &dir);
    let twice = list_dir(&lab, &dir);
    assert_eq!(result_text(&once), result_text(&twice));
    assert_eq!(once.ok()["checks"], at_first.ok()["checks"]);
}

/// A tree with no check file, in the store or in a directory, lists no check and exits 0.
#[test]
fn no_file_lists_nothing() {
    let lab = Lab::new();
    let stored = lab.so(&["check", "list"]).ok();
    assert_eq!(stored["checks"], json!([]));
    assert_eq!(stored["vouched"], false);
    assert!(stored.get("file").is_none(), "{stored}");

    let dir = lab.beside("empty");
    fs::create_dir_all(&dir).unwrap();
    let listed = list_dir(&lab, &dir).ok();
    assert_eq!(listed, json!({"vouched": true, "checks": []}));
}

/// `vouched` is true in every answer of the directory form, a refusal's details included, and
/// false in every answer of the store form.
#[test]
fn the_directory_form_says_the_caller_vouched() {
    let lab = Lab::new();
    let dir = lab.beside("tree");
    write(&dir, ".swarmops/checks.toml", FIXTURE);
    assert_eq!(list_dir(&lab, &dir).ok()["vouched"], true);
    write(&dir, ".swarmops/checks.toml", REFUSED);
    let refused = list_dir(&lab, &dir).err("E_CHECK_FILE", 1);
    assert_eq!(refused["details"]["vouched"], true, "{refused}");
    assert_eq!(refused["details"]["line"], 11, "{refused}");

    lab.publish(".swarmops/checks.toml", FIXTURE);
    assert_eq!(lab.so(&["check", "list"]).ok()["vouched"], false);
    lab.publish(".swarmops/checks.toml", REFUSED);
    let refused = lab.so(&["check", "list"]).err("E_CHECK_FILE", 1);
    assert_eq!(refused["details"]["vouched"], false, "{refused}");
}

/// The directory form opens no store: a store named that does not exist changes nothing, and
/// the envelope names none. Both forms at once is a usage error.
#[test]
fn the_directory_form_opens_no_store() {
    let lab = Lab::new();
    let dir = lab.beside("tree");
    write(&dir, ".swarmops/checks.toml", FIXTURE);
    let line = [
        "--repo",
        "/nonexistent/store",
        "--as",
        "x",
        "check",
        "list",
        "--dir",
    ];
    let named = run_in(
        lab.store.tmp(),
        &[("SWARMOPS_REPO", "/nonexistent/other")],
        &[&line[..], &[dir.to_str().unwrap()]].concat(),
        None,
    );
    assert_eq!(named.ok()["checks"].as_array().unwrap().len(), 3);
    assert!(named.envelope()["meta"].get("repo").is_none(), "{named}");

    let both = lab.so(&["check", "list", "main", "--dir", "."]);
    let error = both.err("E_INVALID_ARG", 2);
    assert!(
        error["message"].as_str().unwrap().contains("--dir"),
        "{error}"
    );
    let nowhere = list_dir(&lab, &lab.beside("missing"));
    assert_eq!(nowhere.err("E_INVALID_ARG", 2)["details"]["reason"], "dir");
}

/// A symbolic link at `.swarmops/checks.toml` is refused, `not_a_file`, in the tree and in a
/// directory: its target is never read.
#[test]
fn a_link_is_not_a_check_file() {
    let lab = Lab::new();
    publish_changes(
        &lab,
        &json!([
            {"path": "other.toml", "op": "put", "content": {"inline": FIXTURE}},
            {"path": ".swarmops/checks.toml", "op": "put", "mode": "symlink",
             "content": {"inline": "../other.toml"}},
        ]),
    );
    let stored = lab.so(&["check", "list"]).err("E_CHECK_FILE", 1);
    assert_eq!(stored["details"]["reason"], "not_a_file", "{stored}");

    let dir = lab.beside("tree");
    write(&dir, "other.toml", FIXTURE);
    fs::create_dir_all(dir.join(".swarmops")).unwrap();
    std::os::unix::fs::symlink("../other.toml", dir.join(".swarmops/checks.toml")).unwrap();
    let linked = list_dir(&lab, &dir).err("E_CHECK_FILE", 1);
    assert_eq!(linked["details"]["reason"], "not_a_file", "{linked}");
    assert_eq!(linked["details"]["vouched"], true, "{linked}");
}

/// A `workspaces.json` to publish, made from where its check is to leave its mark.
type Config = fn(&Path) -> String;

/// `check.argv` of `workspaces.json` still reads as the one check `default`, and `check list`
/// reads the file as `ws land --check` does: where it lists `default`, the land runs that argv;
/// where it lists none, the land has no command to run; where it refuses the file, so does the
/// land. Each case lands from a workspace made before the file was published.
#[test]
fn the_legacy_check_is_what_a_checked_land_runs() {
    let ran = |lab: &Lab| lab.beside("ran.txt");
    let cases: [(&str, Config); 5] = [
        ("valid", |ran| {
            let script = format!("echo checked > {}", ran.display());
            json!({"check": {"argv": ["sh", "-c", script]}}).to_string()
        }),
        ("empty", |_| r#"{"check": {"argv": []}}"#.to_owned()),
        ("no check", |_| r#"{"prefixes": []}"#.to_owned()),
        ("not json", |_| "{\"check\": ".to_owned()),
        ("argv a string", |_| {
            r#"{"check": {"argv": "make"}}"#.to_owned()
        }),
    ];
    for (case, config) in cases {
        // 1. A workspace with an edit, then the file at main.
        let lab = Lab::new();
        let dir = lab.workspace("w1");
        write(&dir, "src/new.rs", "pub fn g() {}\n");
        let config = config(&ran(&lab));
        lab.publish(".swarmops/workspaces.json", &config);

        // 2. What each says.
        let listed = lab.so(&["check", "list"]);
        let landed = inside(&dir, &["ws", "land", "-m", "land", "--check"]);
        match case {
            "valid" => {
                let checks = listed.ok()["checks"].clone();
                let argv = &checks[0]["argv"];
                assert_eq!(checks[0]["name"], "default", "{case}");
                let given: Value = serde_json::from_str(&config).unwrap();
                assert_eq!(argv, &given["check"]["argv"], "{case}");
                assert_eq!(landed.exit, 0, "{case}: {landed}");
                let said = fs::read_to_string(ran(&lab)).expect("the check ran");
                assert_eq!(said, "checked\n", "{case}");
            }
            "empty" | "no check" => {
                assert_eq!(listed.ok()["checks"], json!([]), "{case}");
                let error = landed.err("E_INVALID_ARG", 2);
                assert_eq!(error["details"]["missing"], "check", "{case}: {error}");
            }
            _ => {
                let refused = listed.err("E_CHECK_FILE", 1);
                assert_eq!(refused["details"]["file"], ".swarmops/workspaces.json");
                let error = landed.err("E_INVALID_ARG", 2);
                assert!(
                    error["message"]
                        .as_str()
                        .unwrap()
                        .contains("workspaces.json"),
                    "{case}: {error}"
                );
            }
        }
        assert!(
            !ran(&lab).exists() || case == "valid",
            "{case}: nothing ran"
        );
    }
}
