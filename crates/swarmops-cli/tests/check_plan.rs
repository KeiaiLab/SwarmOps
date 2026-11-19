//! `check plan` (M30-4, M30-5) through the binary: pinned plans for fixture changes, in each of
//! its forms; the same answer twice, byte for byte; and a plan that fails closed, each planted
//! fault giving `run` with its reason and each error no plan.
//!
//! ```text
//!   main:  .swarmops/checks.toml (fixtures/check/plan/target.toml)
//!            docs   inputs docs/**, README.md        before-land
//!            site   inputs web/**, docs/**           before-land, manual
//!            unit   no inputs: the whole tree        before-land
//!            slow   inputs crates/**                 manual
//!          README.md, docs/guide.md, src/lib.rs, web/app/page.tsx, crates/a/src/lib.rs
//! ```

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::SystemTime;

use serde_json::{Value, json};

use crate::mcp::Server;
use crate::ws::{AGENT, Lab, Out, inside, main_of, run_in, write};

/// The target's check file, and the same with one definition changed, each as its name says.
const TARGET: &str = include_str!("fixtures/check/plan/target.toml");
const RENAMED: &str = include_str!("fixtures/check/plan/renamed.toml");
const DELETED: &str = include_str!("fixtures/check/plan/deleted.toml");
const MODIFIED: &str = include_str!("fixtures/check/plan/modified.toml");
const NARROWED: &str = include_str!("fixtures/check/plan/narrowed.toml");
const ADDED: &str = include_str!("fixtures/check/plan/added.toml");

/// Five changed paths, one a line and NUL-ended, one of them with a space.
const CHANGED_TXT: &[u8] = include_bytes!("fixtures/check/plan/changed.txt");
const CHANGED_NUL: &[u8] = include_bytes!("fixtures/check/plan/changed.nul");

/// A check file whose one check declares an input that is no pattern, on line 9.
const REFUSED_INPUTS: &str = include_str!("fixtures/check/refused-inputs.toml");

/// A put of `content` at `path`, as `commit --changes` takes it.
fn put(path: &str, content: &str) -> Value {
    json!({"path": path, "op": "put", "content": {"inline": content}})
}

/// A delete of `path`.
fn delete(path: &str) -> Value {
    json!({"path": path, "op": "delete"})
}

/// Commits `changes` on `parent` as the lab's agent, and moves `main` there if asked; the
/// changeset.
fn commit(lab: &Lab, parent: &str, changes: &[Value], advance: bool) -> String {
    static FILES: AtomicUsize = AtomicUsize::new(0);
    let file = lab.beside(&format!(
        "changes-{}.json",
        FILES.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(&file, Value::from(changes.to_vec()).to_string()).unwrap();
    let mut line = vec![
        "commit",
        "-m",
        "change",
        "--changes",
        file.to_str().unwrap(),
        "--parent",
        parent,
    ];
    if advance {
        line.extend(["--advance", "main"]);
    }
    let committed = lab.so(&line);
    committed.ok()["changeset"].as_str().unwrap().to_owned()
}

/// The fixture: a lab whose `main` holds the target's check file and a file for each of its
/// checks' inputs; `main`'s head.
fn plan_lab() -> (Lab, String) {
    let lab = Lab::new();
    let first = main_of(&lab);
    let files = [
        put(".swarmops/checks.toml", TARGET),
        put("web/app/page.tsx", "page\n"),
        put("crates/a/src/lib.rs", "pub fn a() {}\n"),
    ];
    let main = commit(&lab, &first, &files, true);
    (lab, main)
}

/// `check plan`, then `args`, on the lab's store.
fn plan(lab: &Lab, args: &[&str]) -> Out {
    lab.so(&[&["check", "plan"][..], args].concat())
}

/// `check plan --dir dir`, then `args`, run beside the store with no store named, `stdin` on
/// its standard input.
fn plan_dir(lab: &Lab, dir: &Path, args: &[&str], stdin: Option<&str>) -> Out {
    let line = [&["check", "plan", "--dir", dir.to_str().unwrap()][..], args].concat();
    run_in(lab.store.tmp(), &[], &line, stdin)
}

/// A directory beside the lab's store that holds `checks` as its check file.
fn tree(lab: &Lab, checks: &str) -> PathBuf {
    let dir = lab.beside("tree");
    write(&dir, ".swarmops/checks.toml", checks);
    dir
}

/// The entry of check `name` in the plan `result`.
#[track_caller]
fn check<'r>(result: &'r Value, name: &str) -> &'r Value {
    let checks = result["checks"].as_array().expect("checks");
    (checks.iter())
        .find(|check| check["name"] == name)
        .unwrap_or_else(|| panic!("no check {name} in {result}"))
}

/// What the plan `result` says of each check: its name, verdict and reason.
fn verdicts(result: &Value) -> Vec<(String, String, String)> {
    let word = |value: &Value| value.as_str().unwrap().to_owned();
    (result["checks"].as_array().unwrap().iter())
        .map(|c| (word(&c["name"]), word(&c["verdict"]), word(&c["reason"])))
        .collect()
}

/// `(name, verdict, reason)`, owned, to compare with [`verdicts`].
fn says(name: &str, verdict: &str, reason: &str) -> (String, String, String) {
    (name.to_owned(), verdict.to_owned(), reason.to_owned())
}

/// Every check of the plan `result` is `run`, `widened`, for `cause`, and the plan says so.
#[track_caller]
fn assert_widened(result: &Value, cause: &str) {
    assert_eq!(result["widened"], cause, "{result}");
    let checks = result["checks"].as_array().unwrap();
    assert!(!checks.is_empty(), "{result}");
    for check in checks {
        assert_eq!(
            (&check["verdict"], &check["reason"], &check["widened"]),
            (&json!("run"), &json!("widened"), &json!(cause)),
            "{check}"
        );
    }
}

/// The `result` of `out`'s envelope as printed, byte for byte: what follows `"result":` up to
/// `,"meta":`, which the envelope writes after it.
fn result_text(out: &Out) -> String {
    assert_eq!(out.exit, 0, "{out}");
    let start = out.stdout.find(r#""result":"#).expect("a result") + r#""result":"#.len();
    let end = out.stdout.rfind(r#","meta":"#).expect("a meta");
    out.stdout[start..end].to_owned()
}

/// `out` failed with `code`: a non-zero exit, and an envelope with no `result`; the error.
#[track_caller]
fn no_plan(out: &Out, code: &str) -> Value {
    assert_ne!(out.exit, 0, "{out}");
    let envelope = out.envelope();
    assert_eq!(envelope["ok"], false, "{out}");
    assert!(
        envelope.get("result").is_none(),
        "an error is no plan: {out}"
    );
    assert_eq!(envelope["error"]["code"], code, "{out}");
    envelope["error"].clone()
}

/// The root tree of changeset `id`.
fn root_of(lab: &Lab, id: &str) -> Value {
    lab.so(&["show", id]).ok()["root"].clone()
}

/// Item 1 (B4-K8). Three changes on the fixture's target, each planned against it and asserted
/// field for field: a docs file; a crate file; both and a deleted web file.
#[test]
fn fixture_changes_give_pinned_plans() {
    let (lab, main) = plan_lab();

    // 1. A docs file: `docs` and `site` for their inputs, `unit` for having none.
    let docs = commit(&lab, &main, &[put("docs/guide.md", "more\n")], false);
    let result = plan(&lab, &[&docs]).ok();
    let digest = |name: &str| check(&result, name)["definition_digest"].clone();
    assert_eq!(
        result,
        json!({
            "vouched": false, "subject": "changeset", "stage": "before-land",
            "rev": docs, "tree": root_of(&lab, &docs), "target": "main", "onto": main,
            "base": main, "file": ".swarmops/checks.toml", "version": 1, "changed": 1,
            "checks": [
                {"name": "docs", "verdict": "run", "reason": "input_changed",
                 "path": "docs/guide.md", "pattern": "docs/**",
                 "definition_digest": digest("docs")},
                {"name": "site", "verdict": "run", "reason": "input_changed",
                 "path": "docs/guide.md", "pattern": "docs/**",
                 "definition_digest": digest("site")},
                {"name": "unit", "verdict": "run", "reason": "no_inputs",
                 "definition_digest": digest("unit")},
            ],
        })
    );
    let listed = lab.so(&["check", "list", &docs]).ok();
    for entry in listed["checks"].as_array().unwrap() {
        if entry["stages"]
            .as_array()
            .unwrap()
            .contains(&json!("before-land"))
        {
            let name = entry["name"].as_str().unwrap();
            assert_eq!(digest(name), entry["definition_digest"], "{name}");
        }
    }

    // 2. A crate file: nothing of `docs` or `site`; `unit` still runs.
    let krate = commit(
        &lab,
        &main,
        &[put("crates/a/src/lib.rs", "pub fn b() {}\n")],
        false,
    );
    let result = plan(&lab, &[&krate]).ok();
    assert_eq!(
        (&result["rev"], &result["base"], &result["changed"]),
        (&json!(krate), &json!(main), &json!(1))
    );
    assert_eq!(result["tree"], root_of(&lab, &krate));
    assert_eq!(
        verdicts(&result),
        [
            says("docs", "not_applicable", "inputs_unchanged"),
            says("site", "not_applicable", "inputs_unchanged"),
            says("unit", "run", "no_inputs"),
        ]
    );
    for name in ["docs", "site", "unit"] {
        let entry = check(&result, name);
        assert!(
            entry.get("path").is_none() && entry.get("pattern").is_none(),
            "{entry}"
        );
        assert!(
            entry.get("widened").is_none() && entry.get("definition").is_none(),
            "{entry}"
        );
    }
    assert!(result.get("widened").is_none(), "{result}");

    // 3. Both, and a deleted web file, over two commits: three paths in path order, the
    //    deleted file a changed path, the README's own pattern named.
    let first = commit(
        &lab,
        &main,
        &[
            put("README.md", "# more\n"),
            put("crates/a/src/lib.rs", "pub fn c() {}\n"),
        ],
        false,
    );
    let both = commit(&lab, &first, &[delete("web/app/page.tsx")], false);
    let result = plan(&lab, &[&both, "--target", "main"]).ok();
    assert_eq!(
        (
            &result["rev"],
            &result["onto"],
            &result["base"],
            &result["changed"]
        ),
        (&json!(both), &json!(main), &json!(main), &json!(3))
    );
    assert_eq!(result["tree"], root_of(&lab, &both));
    assert_eq!(
        verdicts(&result),
        [
            says("docs", "run", "input_changed"),
            says("site", "run", "input_changed"),
            says("unit", "run", "no_inputs"),
        ]
    );
    let docs = check(&result, "docs");
    assert_eq!(
        (&docs["path"], &docs["pattern"]),
        (&json!("README.md"), &json!("README.md"))
    );
    let site = check(&result, "site");
    assert_eq!(
        (&site["path"], &site["pattern"]),
        (&json!("web/app/page.tsx"), &json!("web/**"))
    );
}

/// Item 2 (B4-K9). Two runs give the same `result` byte for byte, in the changeset form, the
/// directory form and `--landed`; and two changesets of one tree on one target give the same
/// `checks`.
#[test]
fn two_runs_agree_byte_for_byte() {
    let (lab, main) = plan_lab();
    let change = commit(&lab, &main, &[put("docs/guide.md", "more\n")], false);
    for args in [&[change.as_str()][..], &[change.as_str(), "--landed"]] {
        let once = plan(&lab, args);
        let twice = plan(&lab, args);
        assert_eq!(result_text(&once), result_text(&twice), "{args:?}");
    }
    let dir = tree(&lab, TARGET);
    let list = lab.beside("changed.txt");
    fs::write(&list, CHANGED_TXT).unwrap();
    let args = ["--changed-from", list.to_str().unwrap()];
    let once = plan_dir(&lab, &dir, &args, None);
    let twice = plan_dir(&lab, &dir, &args, None);
    assert_eq!(result_text(&once), result_text(&twice));

    // Two changesets of one tree: another message, the same files.
    let same = commit(&lab, &main, &[put("docs/guide.md", "more\n")], false);
    let again = {
        let amended = commit(&lab, &main, &[put("docs/guide.md", "x\n")], false);
        commit(&lab, &amended, &[put("docs/guide.md", "more\n")], false)
    };
    assert_ne!(same, again);
    assert_eq!(root_of(&lab, &same), root_of(&lab, &again), "one tree");
    let (of_same, of_again) = (plan(&lab, &[&same]).ok(), plan(&lab, &[&again]).ok());
    assert_eq!(of_same["checks"], of_again["checks"]);
    assert_eq!(of_same["tree"], of_again["tree"]);
}

/// Item 3 (B4-K10). The directory form says the caller vouched, names no tree and opens no
/// store: it answers with `SWARMOPS_REPO` naming nothing, and its envelope names no store.
#[test]
fn the_directory_form_says_the_caller_vouched() {
    let lab = Lab::new();
    let dir = tree(&lab, TARGET);
    let line = [
        "check",
        "plan",
        "--dir",
        dir.to_str().unwrap(),
        "--changed",
        "web/app/page.tsx",
    ];
    let nowhere = [("SWARMOPS_REPO", "/nonexistent/store")];
    let out = run_in(lab.store.tmp(), &nowhere, &line, None);
    let envelope = out.envelope();
    assert_eq!(
        (out.exit, &envelope["op"]),
        (0, &json!("check_plan")),
        "{out}"
    );
    assert!(envelope["meta"].get("repo").is_none(), "{out}");
    let result = &envelope["result"];
    assert_eq!(
        (&result["vouched"], &result["subject"], &result["changed"]),
        (&json!(true), &json!("paths"), &json!(1))
    );
    for absent in [
        "tree",
        "rev",
        "target",
        "onto",
        "base",
        "workspace",
        "edits",
        "outcome",
    ] {
        assert!(result.get(absent).is_none(), "{absent}: {result}");
    }
    assert_eq!(
        verdicts(result),
        [
            says("docs", "not_applicable", "inputs_unchanged"),
            says("site", "run", "input_changed"),
            says("unit", "run", "no_inputs"),
        ]
    );

    // Its refusal of the check file says vouched too, and so does the store form's not.
    write(&dir, ".swarmops/checks.toml", REFUSED_INPUTS);
    let refused = plan_dir(&lab, &dir, &["--changed", "a"], None);
    let error = no_plan(&refused, "E_CHECK_FILE");
    assert_eq!(
        (refused.exit, &error["details"]["vouched"]),
        (1, &json!(true))
    );
    assert_eq!(error["details"]["line"], 9, "{error}");

    // A subject is one of three: none, or two of them, is a usage error.
    let none = run_in(lab.store.tmp(), &[], &["check", "plan"], None);
    assert_eq!(none.exit, 2, "{none}");
    let two = plan_dir(&lab, &dir, &["main"], None);
    assert_eq!(two.exit, 2, "{two}");
    let off = run_in(
        lab.store.tmp(),
        &[],
        &["check", "plan", "main", "--changed", "a"],
        None,
    );
    assert_eq!(off.exit, 2, "--changed without --dir: {off}");
    for flags in [
        &["--changed-from", "x"][..],
        &["--ws", "w1", "--changed", "a"],
        &["--ws", "w1", "--landed"],
        &["--ws", "w1", "--target", "main"],
        &["--dir", ".", "--landed"],
        &["--dir", ".", "--target", "main"],
        &["--dir", ".", "--nul"],
        &["--dir", ".", "--ws", "w1"],
    ] {
        let line = [&["check", "plan"][..], flags].concat();
        let refused = run_in(lab.store.tmp(), &[], &line, None);
        assert_eq!(refused.exit, 2, "{flags:?}: {refused}");
        assert!(refused.envelope().get("result").is_none(), "{flags:?}");
    }
}

/// Item 4 (B4-K11). The changed paths come from `--changed`, a file of lines, a NUL-ended file
/// and stdin, the flags' paths first; an empty list plans no change; a file that is not there is
/// an error and no plan.
#[test]
fn the_list_of_changed_paths() {
    let lab = Lab::new();
    let dir = tree(&lab, TARGET);
    let (lines, nuls) = (lab.beside("changed.txt"), lab.beside("changed.nul"));
    fs::write(&lines, CHANGED_TXT).unwrap();
    fs::write(&nuls, CHANGED_NUL).unwrap();
    let (lines, nuls) = (lines.to_str().unwrap(), nuls.to_str().unwrap());

    // 1. `--changed` twice.
    let flags = [
        "--changed",
        "web/app/page.tsx",
        "--changed",
        "docs/guide.md",
    ];
    let two = plan_dir(&lab, &dir, &flags, None).ok();
    assert_eq!(two["changed"], 2);
    assert_eq!(
        check(&two, "site")["path"],
        "web/app/page.tsx",
        "the caller's order"
    );
    assert_eq!(check(&two, "docs")["path"], "docs/guide.md");

    // 2. A file of lines, the same NUL-ended, and the lines on stdin: one plan.
    let of_lines = plan_dir(&lab, &dir, &["--changed-from", lines], None);
    let of_nuls = plan_dir(&lab, &dir, &["--changed-from", nuls, "--nul"], None);
    let text = String::from_utf8(CHANGED_TXT.to_vec()).unwrap();
    let of_stdin = plan_dir(&lab, &dir, &["--changed-from", "-"], Some(&text));
    assert_eq!(result_text(&of_lines), result_text(&of_nuls));
    assert_eq!(result_text(&of_lines), result_text(&of_stdin));
    let listed = of_lines.ok();
    assert_eq!(listed["changed"], 5);
    assert!(listed.get("widened").is_none(), "{listed}");
    assert_eq!(
        check(&listed, "docs")["path"],
        "docs/status/M1/a b.txt",
        "a path with a space"
    );

    // 3. Both flags: the flags' paths are read first.
    let both = plan_dir(
        &lab,
        &dir,
        &["--changed", "README.md", "--changed-from", lines],
        None,
    );
    let both = both.ok();
    assert_eq!(both["changed"], 6);
    assert_eq!(check(&both, "docs")["path"], "README.md");

    // 4. An empty list is a change of no path: nothing to run, and not a widened plan.
    let empty = lab.beside("empty.txt");
    fs::write(&empty, "").unwrap();
    let none = plan_dir(
        &lab,
        &dir,
        &["--changed-from", empty.to_str().unwrap()],
        None,
    )
    .ok();
    assert_eq!(none["changed"], 0);
    assert!(none.get("widened").is_none(), "{none}");
    for (_, verdict, reason) in verdicts(&none) {
        assert_eq!(
            (verdict.as_str(), reason.as_str()),
            ("not_applicable", "inputs_unchanged")
        );
    }

    // 5. A file that is not there: an error, vouched, naming the path as given; no plan.
    let missing = lab.beside("nothing.txt");
    let out = plan_dir(
        &lab,
        &dir,
        &["--changed-from", missing.to_str().unwrap()],
        None,
    );
    let error = no_plan(&out, "E_IO");
    assert_eq!(
        error["details"]["path"],
        missing.to_str().unwrap(),
        "{error}"
    );
    assert_eq!(error["details"]["vouched"], true, "{error}");
}

/// Item 5 (B4-K18 to B4-K23). Each of six planted faults gives `run` with its reason: the
/// changed paths unknown, cut and unreadable; the definition renamed, deleted, and not the
/// target's head's.
#[test]
fn each_of_six_planted_faults_gives_run_with_its_reason() {
    let (lab, main) = plan_lab();
    let dir = tree(&lab, TARGET);
    paths_unknown(&lab, &dir);
    paths_cut(&lab, &dir, &main);
    path_unreadable(&lab, &dir);
    definition_renamed(&lab, &main);
    definition_deleted(&lab, &main);
    definition_not_the_target_s(&lab, &main);
}

/// F1. The changed paths are not known: no shared history, and no path given.
fn paths_unknown(lab: &Lab, dir: &Path) {
    let root = lab.beside("root.json");
    let files = json!([
        put(".swarmops/checks.toml", TARGET),
        put("other.txt", "x\n")
    ]);
    fs::write(&root, files.to_string()).unwrap();
    let line = [
        "commit",
        "-m",
        "another root",
        "--changes",
        root.to_str().unwrap(),
    ];
    let other = lab.so(&[&line[..], &["--advance", "release/other"]].concat());
    let other = other.ok()["changeset"].as_str().unwrap().to_owned();
    let unrelated = plan(lab, &[&other]).ok();
    assert_widened(&unrelated, "paths_unknown");
    assert!(unrelated.get("base").is_none(), "{unrelated}");
    assert_widened(&plan_dir(lab, dir, &[], None).ok(), "paths_unknown");
}

/// F2. The changed paths are cut: 100,001 paths given, and a land-check item of 1,001.
fn paths_cut(lab: &Lab, dir: &Path, main: &str) {
    let many = (0..100_001).fold(String::new(), |mut list, i| {
        let _ = writeln!(list, "data/f{i}");
        list
    });
    let list = lab.beside("many.txt");
    fs::write(&list, &many).unwrap();
    let cut = plan_dir(lab, dir, &["--changed-from", list.to_str().unwrap()], None).ok();
    assert_widened(&cut, "paths_truncated");
    assert_eq!(cut["changed"], 100_000);
    fs::write(&list, &many[..many.len() - "data/f100000\n".len()]).unwrap();
    let whole = plan_dir(lab, dir, &["--changed-from", list.to_str().unwrap()], None).ok();
    assert_eq!(whole["changed"], 100_000);
    assert!(
        whole.get("widened").is_none(),
        "100,000 paths are read: {whole}"
    );
    let files: Vec<Value> = (0..1_001)
        .map(|i| put(&format!("data/f{i:04}"), "x\n"))
        .collect();
    let wide = commit(lab, main, &files, false);
    assert_widened(&plan(lab, &[&wide, "--landed"]).ok(), "paths_truncated");
    let unlanded = plan(lab, &[&wide]).ok();
    assert_eq!(
        unlanded["changed"], 1_001,
        "the changeset form reads them all"
    );
    assert!(unlanded.get("widened").is_none(), "{unlanded}");
}

/// F3. A changed path that does not read: `../x`, invalid UTF-8, a quoted line.
fn path_unreadable(lab: &Lab, dir: &Path) {
    assert_widened(
        &plan_dir(lab, dir, &["--changed", "../x"], None).ok(),
        "path_unreadable",
    );
    let bytes = lab.beside("bytes.txt");
    fs::write(&bytes, b"docs/a\xffb.md\n").unwrap();
    let from = ["--changed-from", bytes.to_str().unwrap()];
    assert_widened(&plan_dir(lab, dir, &from, None).ok(), "path_unreadable");
    let quoted = plan_dir(
        lab,
        dir,
        &["--changed-from", "-"],
        Some("\"docs/a b.md\"\n"),
    );
    assert_widened(&quoted.ok(), "path_unreadable");
    let unquoted = plan_dir(
        lab,
        dir,
        &["--changed-from", "-", "--nul"],
        Some("docs/a b.md\0"),
    );
    assert_eq!(check(&unquoted.ok(), "docs")["reason"], "input_changed");
}

/// F4. The definition renamed: run under the new name, `was` the old, which is listed too.
fn definition_renamed(lab: &Lab, main: &str) {
    let renamed = commit(lab, main, &[put(".swarmops/checks.toml", RENAMED)], false);
    let result = plan(lab, &[&renamed]).ok();
    let prose = check(&result, "prose");
    assert_eq!(
        (
            &prose["verdict"],
            &prose["reason"],
            &prose["definition"],
            &prose["was"]
        ),
        (
            &json!("run"),
            &json!("definition_changed"),
            &json!("renamed"),
            &json!("docs")
        )
    );
    assert_eq!(check(&result, "docs")["definition"], "deleted");
    assert_eq!(check(&result, "docs")["verdict"], "run");
}

/// F5. The definition deleted: listed, run, with the target's digest.
fn definition_deleted(lab: &Lab, main: &str) {
    let deleted = commit(lab, main, &[put(".swarmops/checks.toml", DELETED)], false);
    let result = plan(lab, &[&deleted]).ok();
    let site = check(&result, "site");
    assert_eq!(
        (&site["verdict"], &site["reason"], &site["definition"]),
        (
            &json!("run"),
            &json!("definition_changed"),
            &json!("deleted")
        )
    );
    let at_main = plan(lab, &["main"]).ok();
    assert_eq!(
        site["definition_digest"],
        check(&at_main, "site")["definition_digest"]
    );
}

/// F6. The definition differs from the target's head's: the candidate changed the argv; and,
/// apart, the target changed it since the base of a candidate that did not.
fn definition_not_the_target_s(lab: &Lab, main: &str) {
    let modified = commit(lab, main, &[put(".swarmops/checks.toml", MODIFIED)], false);
    let unit = check(&plan(lab, &[&modified]).ok(), "unit").clone();
    assert_eq!(
        (&unit["verdict"], &unit["reason"], &unit["definition"]),
        (
            &json!("run"),
            &json!("definition_changed"),
            &json!("modified")
        )
    );
    let aside = commit(lab, main, &[put("src/lib.rs", "pub fn g() {}\n")], false);
    assert_eq!(
        check(&plan(lab, &[&aside]).ok(), "unit")["reason"],
        "no_inputs"
    );
    let moved = commit(lab, main, &[put(".swarmops/checks.toml", MODIFIED)], true);
    let result = plan(lab, &[&aside]).ok();
    assert_eq!(
        (&result["onto"], &result["base"]),
        (&json!(moved), &json!(main))
    );
    let unit = check(&result, "unit");
    assert_eq!(
        (&unit["verdict"], &unit["reason"], &unit["definition"]),
        (
            &json!("run"),
            &json!("definition_changed"),
            &json!("modified")
        )
    );
}

/// Item 6 (B4-K24). A candidate that narrows its own inputs still plans that check as `run`:
/// the same change without the rewrite runs it for its input, and a crate-only change on the
/// unchanged file does not need it, so the test would fail if narrowing were honoured.
#[test]
fn a_candidate_that_narrows_its_own_inputs_still_runs_the_check() {
    let (lab, main) = plan_lab();
    let narrowed = commit(
        &lab,
        &main,
        &[
            put("docs/x.md", "x\n"),
            put(".swarmops/checks.toml", NARROWED),
        ],
        false,
    );
    let site = check(&plan(&lab, &[&narrowed]).ok(), "site").clone();
    assert_eq!(
        (&site["verdict"], &site["reason"], &site["definition"]),
        (
            &json!("run"),
            &json!("definition_changed"),
            &json!("modified")
        )
    );

    let unchanged = commit(&lab, &main, &[put("docs/x.md", "x\n")], false);
    let site = check(&plan(&lab, &[&unchanged]).ok(), "site").clone();
    assert_eq!(
        (&site["verdict"], &site["reason"]),
        (&json!("run"), &json!("input_changed"))
    );
    assert_eq!(
        (&site["path"], &site["pattern"]),
        (&json!("docs/x.md"), &json!("docs/**"))
    );

    let krate = commit(
        &lab,
        &main,
        &[put("crates/a/src/lib.rs", "pub fn n() {}\n")],
        false,
    );
    let site = check(&plan(&lab, &[&krate]).ok(), "site").clone();
    assert_eq!(site["verdict"], "not_applicable");

    // The directory form has no target to compare with: there the narrowed file is the
    // caller's word, and a changed check file runs everything.
    let dir = tree(&lab, NARROWED);
    let blind = plan_dir(&lab, &dir, &["--changed", "docs/x.md"], None).ok();
    assert_eq!(check(&blind, "site")["verdict"], "not_applicable");
    let seen = [
        "--changed",
        "docs/x.md",
        "--changed",
        ".swarmops/checks.toml",
    ];
    assert_widened(
        &plan_dir(&lab, &dir, &seen, None).ok(),
        "check_file_changed",
    );
}

/// Item 7 (B4-K26). Beside the six: a check added; the target's file refused, which widens the
/// plan with the candidate's checks all `run`; the directory form with the check file among
/// its paths.
#[test]
fn what_cannot_be_compared_runs() {
    let (lab, main) = plan_lab();
    let added = commit(&lab, &main, &[put(".swarmops/checks.toml", ADDED)], false);
    let result = plan(&lab, &[&added, "--stage", "before-land"]).ok();
    let lint = check(&result, "lint");
    assert_eq!(
        (&lint["verdict"], &lint["reason"], &lint["definition"]),
        (&json!("run"), &json!("definition_changed"), &json!("added"))
    );
    assert!(lint.get("was").is_none(), "{lint}");

    let dir = tree(&lab, TARGET);
    for path in [
        ".swarmops/checks.toml",
        ".swarmops/workspaces.json",
        ".swarmops",
    ] {
        let seen = plan_dir(
            &lab,
            &dir,
            &["--changed", "docs/guide.md", "--changed", path],
            None,
        );
        assert_widened(&seen.ok(), "check_file_changed");
    }

    // The target's check file stops reading: a change made before is planned all `run`.
    let change = commit(&lab, &main, &[put("src/lib.rs", "pub fn g() {}\n")], false);
    commit(
        &lab,
        &main,
        &[put(".swarmops/checks.toml", "version = 2\n")],
        true,
    );
    let result = plan(&lab, &[&change]).ok();
    assert_widened(&result, "target_unread");
    assert_eq!(
        result["checks"].as_array().unwrap().len(),
        3,
        "the candidate's checks"
    );
    assert_eq!(result["changed"], 1);
}

/// Item 8 (B4-K27). An error is no plan: the candidate's check file refused, in both forms; a
/// revision and a target that name nothing. Each is a non-zero exit, a code and no `result`.
/// A candidate that deletes its check file is not an error, and not an empty plan: the
/// target's checks are listed, deleted.
#[test]
fn an_error_is_no_plan() {
    let (lab, main) = plan_lab();
    let broken = commit(
        &lab,
        &main,
        &[put(".swarmops/checks.toml", REFUSED_INPUTS)],
        false,
    );
    let refused = plan(&lab, &[&broken]);
    let error = no_plan(&refused, "E_CHECK_FILE");
    assert_eq!(
        (refused.exit, &error["details"]["vouched"]),
        (1, &json!(false))
    );
    assert_eq!(error["details"]["key"], "check.site.inputs", "{error}");
    let dir = tree(&lab, REFUSED_INPUTS);
    let refused = plan_dir(&lab, &dir, &["--changed", "docs/guide.md"], None);
    assert_eq!(
        no_plan(&refused, "E_CHECK_FILE")["details"]["vouched"],
        true
    );
    no_plan(&plan(&lab, &[&broken, "--landed"]), "E_CHECK_FILE");

    no_plan(&plan(&lab, &["nothing"]), "E_REV_NOT_FOUND");
    let nowhere = plan(&lab, &["main", "--target", "release/none"]);
    assert!(!nowhere.envelope()["error"]["code"].is_null(), "{nowhere}");
    assert!(
        nowhere.envelope().get("result").is_none() && nowhere.exit != 0,
        "{nowhere}"
    );
    no_plan(
        &plan(&lab, &["--ws", "no-such-workspace"]),
        "E_WS_NOT_FOUND",
    );
    let missing = lab.beside("no-dir");
    no_plan(&plan_dir(&lab, &missing, &[], None), "E_INVALID_ARG");

    let gone = commit(&lab, &main, &[delete(".swarmops/checks.toml")], false);
    let result = plan(&lab, &[&gone]).ok();
    assert!(result.get("file").is_none(), "{result}");
    let names: Vec<&str> = (result["checks"].as_array().unwrap().iter())
        .map(|check| check["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["docs", "site", "unit"], "not an empty plan");
    for entry in result["checks"].as_array().unwrap() {
        assert_eq!(
            (&entry["verdict"], &entry["definition"]),
            (&json!("run"), &json!("deleted")),
            "{entry}"
        );
    }
}

/// Item 9 (B4-K12). A workspace's committed chain and its edits are planned together; after a
/// discard the head's tree is named; an uncommitted edit of the check file is compared; a
/// rebasing workspace is refused.
#[test]
fn a_workspace_s_edits_are_planned() {
    // 1. One commit touching docs, and an uncommitted edit of a crate file.
    let (lab, main) = plan_lab();
    let dir = lab.workspace("w1");
    write(&dir, "docs/guide.md", "mine\n");
    let committed = inside(&dir, &["ws", "commit", "-m", "docs"]).ok();
    let head = committed["changeset"].as_str().unwrap().to_owned();
    write(&dir, "crates/a/src/lib.rs", "pub fn edited() {}\n");
    let manual = ["--ws", "w1", "--stage", "manual"];
    let result = plan(&lab, &manual).ok();
    assert_eq!(
        (&result["subject"], &result["vouched"], &result["workspace"]),
        (&json!("workspace"), &json!(false), &json!("w1"))
    );
    assert_eq!(
        (
            &result["rev"],
            &result["base"],
            &result["onto"],
            &result["target"]
        ),
        (&json!(head), &json!(main), &json!(main), &json!("main"))
    );
    assert_eq!(
        (&result["edits"], &result["changed"]),
        (&json!(1), &json!(2))
    );
    assert!(
        result.get("tree").is_none(),
        "an edit: no tree was planned: {result}"
    );
    assert_eq!(
        verdicts(&result),
        [
            says("site", "run", "input_changed"),
            says("slow", "run", "input_changed")
        ]
    );
    assert_eq!(check(&result, "site")["path"], "docs/guide.md");
    assert_eq!(check(&result, "slow")["path"], "crates/a/src/lib.rs");

    // By its directory, the same plan.
    let by_path = plan(
        &lab,
        &["--ws-path", dir.to_str().unwrap(), "--stage", "manual"],
    );
    assert_eq!(by_path.ok(), result);

    // 2. The edit discarded: the head's tree, and the crate check not needed.
    let discarded = inside(&dir, &["ws", "discard"]);
    assert_eq!(discarded.exit, 0, "{discarded}");
    let result = plan(&lab, &manual).ok();
    assert_eq!(
        (&result["edits"], &result["changed"]),
        (&json!(0), &json!(1))
    );
    assert_eq!(result["tree"], root_of(&lab, &head));
    assert_eq!(
        verdicts(&result),
        [
            says("site", "run", "input_changed"),
            says("slow", "not_applicable", "inputs_unchanged")
        ]
    );
    assert_eq!(
        result["checks"],
        plan(&lab, &[&head, "--stage", "manual"]).ok()["checks"]
    );

    // 3. An uncommitted edit of the check file itself is compared with the target's head.
    write(&dir, ".swarmops/checks.toml", NARROWED);
    let result = plan(&lab, &manual).ok();
    let site = check(&result, "site");
    assert_eq!(
        (&site["verdict"], &site["reason"], &site["definition"]),
        (
            &json!("run"),
            &json!("definition_changed"),
            &json!("modified")
        )
    );
    inside(&dir, &["ws", "discard"]).ok();

    // 4. A rebasing workspace is refused, as `check run` refuses it.
    commit(&lab, &main, &[put("docs/guide.md", "theirs\n")], true);
    let stopped = inside(&dir, &["ws", "rebase"]);
    assert_eq!(
        stopped.envelope()["error"]["code"],
        "E_CONFLICT",
        "{stopped}"
    );
    let refused = plan(&lab, &manual);
    let error = no_plan(&refused, "E_UNRESOLVED_CONFLICTS");
    assert!(
        error["hint"]
            .as_str()
            .unwrap()
            .contains("ws rebase --continue"),
        "{error}"
    );
}

/// Item 10 (B4-K13). `--landed` on a clean change plans the paths and the tree `land-check`
/// gives for it; on a change the target already holds, nothing to land; on a conflicting one,
/// `E_CONFLICT` with the report, and no plan.
#[test]
fn a_land_check_item_is_planned() {
    let (lab, main) = plan_lab();
    let change = commit(&lab, &main, &[put("docs/guide.md", "ours\n")], false);
    let onto = commit(&lab, &main, &[put("src/lib.rs", "pub fn t() {}\n")], true);

    // 1. Clean: the item's root and paths, on the target's new head.
    let item = lab.so(&["land-check", &change]).ok();
    assert_eq!(item["items"][0]["outcome"], "clean", "{item}");
    let result = plan(&lab, &[&change, "--landed"]).ok();
    assert_eq!(
        (
            &result["subject"],
            &result["outcome"],
            &result["rev"],
            &result["onto"]
        ),
        (
            &json!("landed"),
            &json!("clean"),
            &json!(change),
            &json!(onto)
        )
    );
    assert_eq!(result["tree"], item["items"][0]["root"]);
    assert_ne!(
        result["tree"],
        root_of(&lab, &change),
        "the landed tree, not the candidate's"
    );
    let paths = item["items"][0]["paths"].as_array().unwrap();
    assert_eq!(result["changed"], paths.len());
    assert_eq!(check(&result, "docs")["path"], paths[0]["path"]);
    assert!(result.get("base").is_none(), "{result}");
    assert_eq!(
        verdicts(&result),
        [
            says("docs", "run", "input_changed"),
            says("site", "run", "input_changed"),
            says("unit", "run", "no_inputs"),
        ]
    );

    // 2. The change landed: the target holds it, and nothing is left to land.
    let landed = lab.so(&["land", &change, "-m", "land it"]);
    assert_eq!(landed.exit, 0, "{landed}");
    let result = plan(&lab, &[&change, "--landed"]).ok();
    assert_eq!(
        (&result["outcome"], &result["changed"]),
        (&json!("nothing_to_land"), &json!(0))
    );
    assert!(
        result.get("tree").is_none() && result.get("widened").is_none(),
        "{result}"
    );
    for (_, verdict, _) in verdicts(&result) {
        assert_eq!(verdict, "not_applicable");
    }

    // 3. A conflict: the report, and no plan.
    let theirs = main_of(&lab);
    let conflicting = commit(&lab, &main, &[put("docs/guide.md", "another\n")], false);
    let refused = plan(&lab, &[&conflicting, "--landed"]);
    let error = no_plan(&refused, "E_CONFLICT");
    assert_eq!(refused.exit, 3, "{refused}");
    assert_eq!(error["details"]["onto"], theirs.as_str(), "{error}");
    assert_eq!(
        error["details"]["report"]["files"][0]["path"], "docs/guide.md",
        "{error}"
    );
    assert!(
        error["hint"]
            .as_str()
            .unwrap()
            .starts_with("swarmops land-check "),
        "{error}"
    );

    // `--base` is a land's: clap refuses it without `--landed`.
    let off = plan(&lab, &[&conflicting, "--base", &main]);
    assert_eq!(off.exit, 2, "{off}");
}

/// Item 11 (B4-K15). A plan is of one stage: a check of two stages is in both plans, a
/// manual-only one in that plan alone; `--check` filters, and refuses a name of another stage
/// (`stage`) and one no check has (`check`).
#[test]
fn a_plan_is_of_one_stage() {
    let (lab, main) = plan_lab();
    let change = commit(
        &lab,
        &main,
        &[put("crates/a/src/lib.rs", "pub fn s() {}\n")],
        false,
    );
    let names = |result: &Value| -> Vec<String> {
        verdicts(result)
            .into_iter()
            .map(|(name, _, _)| name)
            .collect()
    };
    let before = plan(&lab, &[&change]).ok();
    assert_eq!(before["stage"], "before-land");
    assert_eq!(names(&before), ["docs", "site", "unit"]);
    let manual = plan(&lab, &[&change, "--stage", "manual"]).ok();
    assert_eq!(manual["stage"], "manual");
    assert_eq!(names(&manual), ["site", "slow"]);
    assert_eq!(check(&manual, "slow")["reason"], "input_changed");
    let deploy = plan(&lab, &[&change, "--stage", "deploy"]).ok();
    assert_eq!(
        deploy["checks"],
        json!([]),
        "no check of the stage: a plan of none, exit 0"
    );

    // `--check`, repeatable, keeps those checks, sorted.
    let two = plan(&lab, &[&change, "--check", "unit", "--check", "docs"]).ok();
    assert_eq!(names(&two), ["docs", "unit"]);
    assert_eq!(check(&two, "unit"), check(&before, "unit"));

    // A check of another stage, and a name no check has: refused as `check run` refuses them.
    let other = plan(&lab, &[&change, "--check", "slow"]);
    let error = other.err("E_INVALID_ARG", 2);
    assert_eq!(
        (&error["details"]["reason"], &error["details"]["stages"]),
        (&json!("stage"), &json!(["manual"])),
        "{error}"
    );
    assert_eq!(error["hint"], "give the check's stage: --stage manual");
    let unknown = plan(&lab, &[&change, "--check", "nope"]).err("E_INVALID_ARG", 2);
    assert_eq!(
        (&unknown["details"]["reason"], &unknown["details"]["check"]),
        (&json!("check"), &json!("nope")),
        "{unknown}"
    );
    let dir = tree(&lab, TARGET);
    let in_dir = plan_dir(&lab, &dir, &["--check", "slow"], None).err("E_INVALID_ARG", 2);
    assert_eq!(in_dir["details"]["reason"], "stage", "{in_dir}");
}

/// Every file below `root`: its path, size, modification time and bytes.
fn files_below(root: &Path) -> Vec<(PathBuf, u64, SystemTime, Vec<u8>)> {
    let mut found = Vec::new();
    let mut dirs = vec![root.to_owned()];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let meta = fs::symlink_metadata(&path).unwrap();
            if meta.is_dir() {
                dirs.push(path);
            } else if meta.is_file() {
                let bytes = fs::read(&path).unwrap();
                found.push((path, meta.len(), meta.modified().unwrap(), bytes));
            }
        }
    }
    found.sort();
    found
}

/// Item 12 (B4-K16). A plan takes no writer lock and writes nothing to the store: every file
/// below the store's root is as it was after the three store forms; and a plan answers while
/// the check of `ws land --check` runs, whose command it is.
#[test]
fn a_plan_takes_no_lock_and_writes_nothing() {
    // 1. A change, a workspace with a commit and an edit, and the store as it stands.
    let (lab, main) = plan_lab();
    let change = commit(&lab, &main, &[put("docs/guide.md", "more\n")], false);
    let dir = lab.workspace("w1");
    write(&dir, "docs/guide.md", "mine\n");
    inside(&dir, &["ws", "commit", "-m", "docs"]).ok();
    write(&dir, "src/lib.rs", "pub fn edited() {}\n");
    let root = Path::new(lab.path());
    let before = files_below(root);
    assert!(before.len() > 3, "the store's files");

    // 2. Each store form, and the store untouched after each.
    for args in [
        &[change.as_str()][..],
        &[change.as_str(), "--landed"],
        &["--ws", "w1"],
        &[change.as_str(), "--stage", "manual", "--check", "site"],
    ] {
        let planned = plan(&lab, args);
        assert_eq!(planned.exit, 0, "{args:?}: {planned}");
        let after = files_below(root);
        assert_eq!(
            after.len(),
            before.len(),
            "{args:?}: a file made or removed"
        );
        for (was, is) in before.iter().zip(&after) {
            assert_eq!((&was.0, was.1, was.2), (&is.0, is.1, is.2), "{args:?}");
            assert!(was.3 == is.3, "{args:?}: {} changed", was.0.display());
        }
    }

    // 3. A checked land whose check is two plans: of the workspace being landed, and of main.
    //    The land holds what it holds while its check runs, and both plans answer.
    inside(&dir, &["ws", "commit", "-m", "the edit"]).ok();
    let bin = crate::support::BIN;
    let (of_ws, of_main) = (lab.beside("plan-ws.json"), lab.beside("plan-main.json"));
    let script = format!(
        "{bin} --repo {store} check plan --ws w1 > {ws} && \
         {bin} --repo {store} check plan main --landed > {main}",
        store = lab.path(),
        ws = of_ws.display(),
        main = of_main.display(),
    );
    let landed = inside(
        &dir,
        &[
            "ws", "land", "-m", "land", "--check", "--", "sh", "-c", &script,
        ],
    );
    assert_eq!(landed.exit, 0, "the check's exit is the plans': {landed}");
    for file in [of_ws, of_main] {
        let said: Value = serde_json::from_str(&fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(
            (&said["ok"], &said["op"]),
            (&json!(true), &json!("check_plan")),
            "{said}"
        );
    }
}

/// Item 13 (B4-K17). The tool `check_plan` answers as the command does, for a changeset and
/// for a land: the same `result`.
#[test]
fn the_tool_answers_as_the_command() {
    let (lab, main) = plan_lab();
    let change = commit(&lab, &main, &[put("docs/guide.md", "more\n")], false);
    commit(&lab, &main, &[put("src/lib.rs", "pub fn t() {}\n")], true);
    let mut server = Server::start(&["--repo", lab.path(), "--as", AGENT], &[]);
    server.initialize();

    // 1. A changeset, with its target, stage and checks named.
    let said = server.call("check_plan", json!({"rev": change}));
    assert_eq!(said.envelope["result"], plan(&lab, &[&change]).ok());
    let request = json!({"rev": change, "target": "main", "stage": "manual", "checks": ["site"]});
    let said = server.call("check_plan", request);
    let line = [
        change.as_str(),
        "--target",
        "main",
        "--stage",
        "manual",
        "--check",
        "site",
    ];
    assert_eq!(said.envelope["result"], plan(&lab, &line).ok());
    assert_eq!(
        said.envelope["result"]["checks"].as_array().unwrap().len(),
        1
    );

    // 2. The change as it would land.
    let said = server.call("check_plan", json!({"rev": change, "landed": true}));
    assert_eq!(
        said.envelope["result"],
        plan(&lab, &[&change, "--landed"]).ok()
    );
    assert_eq!(said.envelope["result"]["outcome"], "clean");

    // 3. The tool's own refusal: a base with no land.
    let refused = server.call("check_plan", json!({"rev": change, "base": main}));
    assert_eq!(
        refused.envelope["error"]["code"], "E_INVALID_ARG",
        "{}",
        refused.envelope
    );
    assert_eq!(refused.envelope["error"]["details"]["reason"], "base");
    assert!(
        refused.envelope.get("result").is_none(),
        "{}",
        refused.envelope
    );
    server.finish_clean();
}
