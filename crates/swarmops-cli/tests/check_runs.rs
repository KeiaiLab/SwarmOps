//! Run lines, `check runs` and `check log --failed` (M31-11, M31-12, M31-13) through the binary:
//! the line each check of a run leaves, where it goes, its rotation, and a failed append; the
//! lines read back filtered, newest first, a page at a time with a cursor, by the command and by
//! the MCP tool.
//!
//! Every fixture check is a program with its arguments: `sh -c …` where a script is wanted.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use swarmops_api::CheckRunLine;
use swarmops_ops::check::lines::{
    MAX_RUN_LINES_BYTES, RUN_LINES_DIR_ENV, RUN_LINES_FILE, RUN_LINES_OLDER_FILE,
};

use crate::check_run::{Tree, check, checked_lab, failed_run};
use crate::ws::{AGENT, Out, run_in};

/// The fixture the reader's unit tests read: twelve lines of three runs and four checks, two
/// more of keys already seen, then three lines that do not read.
const FIXTURE: &[u8] = include_bytes!("fixtures/check/run-lines.jsonl");

/// The fixture's three runs, oldest first.
const R1: &str = "0193f1e2-0000-7000-8000-0000000000a1";
const R2: &str = "0193f1e2-0000-7000-8000-0000000000a2";
const R3: &str = "0193f1e2-0000-7000-8000-0000000000a3";

/// A directory holding the fixture as its run lines, in a temp dir.
fn fixture_lines() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("lines");
    fs::create_dir(&dir).unwrap();
    fs::write(dir.join(RUN_LINES_FILE), FIXTURE).unwrap();
    (tmp, dir)
}

/// `check runs --run-lines <dir>` and `args`, run in `cwd`, which is no workspace.
fn runs(cwd: &Path, dir: &Path, args: &[&str]) -> Out {
    let mut line = vec!["check", "runs", "--run-lines", dir.to_str().unwrap()];
    line.extend_from_slice(args);
    run_in(cwd, &[], &line, None)
}

/// `(run, check)` of each line of a page.
fn named(page: &Value) -> Vec<(String, String)> {
    (page["runs"].as_array().unwrap().iter())
        .map(|line| {
            let text = |key: &str| line[key].as_str().unwrap().to_owned();
            (text("run"), text("check"))
        })
        .collect()
}

/// `pairs` as [`named`] gives them.
fn pairs(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    (pairs.iter())
        .map(|(run, check)| ((*run).to_owned(), (*check).to_owned()))
        .collect()
}

/// Three checks, in run order `bad` (two lines, exit 3), `ok` and `slow` (half a second).
const THREE: &str = "[check.bad]\nargv = [\"sh\", \"-c\", \"echo one; echo two >&2; exit 3\"]\n\
                     [check.ok]\nargv = [\"sh\", \"-c\", \"echo fine\"]\n\
                     [check.slow]\nargv = [\"sh\", \"-c\", \"sleep 0.5\"]\n";

/// The lines of `file`, each as the JSON object it is.
fn lines_of(file: &Path) -> Vec<Value> {
    let text = fs::read_to_string(file).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
    (text.lines())
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("{line}: {e}")))
        .collect()
}

/// The properties `swarmops schema CheckRunLine` names.
fn line_properties(tmp: &Path) -> BTreeSet<String> {
    let schema = run_in(tmp, &[], &["schema", "CheckRunLine"], None).ok();
    let properties = schema["CheckRunLine"]["properties"].as_object().unwrap();
    properties.keys().cloned().collect()
}

/// A run of three checks adds three lines that pass their schema check (M31-11): each line's
/// keys are properties of the published type, each reads as one, in run order, with the run's
/// id and directory and its check's outcome and duration.
#[test]
fn a_run_of_three_checks_adds_three_lines_that_pass_their_schema_check() {
    // 1. The run, its lines sent to a directory that is not there yet.
    let tree = Tree::new(THREE);
    let lines_dir = tree.tmp().join("lines");
    let run = failed_run(&tree.run(&["--run-lines", lines_dir.to_str().unwrap()], &[]));

    // 2. Three lines, each of the schema's keys and read as the type.
    let lines = lines_of(&lines_dir.join(RUN_LINES_FILE));
    assert_eq!(lines.len(), 3, "{lines:?}");
    let properties = line_properties(tree.tmp());
    for line in &lines {
        let keys: BTreeSet<String> = line.as_object().unwrap().keys().cloned().collect();
        assert!(
            keys.is_subset(&properties),
            "{keys:?} against {properties:?}"
        );
        let read: CheckRunLine = serde_json::from_value(line.clone()).unwrap();
        assert_eq!(serde_json::to_value(&read).unwrap(), *line, "read back");
        assert!(swarmops_ops::check::lines::valid(&read), "{read:?}");
    }

    // 3. In run order, each with its run and its check's result.
    let names: Vec<&str> = lines.iter().map(|l| l["check"].as_str().unwrap()).collect();
    assert_eq!(names, ["bad", "ok", "slow"]);
    for line in &lines {
        let of = check(&run, line["check"].as_str().unwrap());
        assert_eq!(
            (&line["run"], &line["run_dir"], &line["vouched"]),
            (&run["run"], &run["dir"], &json!(true))
        );
        assert_eq!(
            (&line["outcome"], &line["duration_ms"]),
            (&of["outcome"], &of["duration_ms"])
        );
        assert_eq!(line["definition_digest"], of["definition_digest"]);
        assert_eq!(
            (&line["attempt"], &line["stage"]),
            (&json!(1), &json!("before-land"))
        );
        assert!(
            line.get("tree").is_none() && line.get("workspace").is_none(),
            "{line}"
        );
    }
    assert_eq!(lines[0]["exit_code"], 3, "{lines:?}");
    assert!(
        lines[2]["wait_ms"].as_u64() >= lines[1]["wait_ms"].as_u64(),
        "{lines:?}"
    );

    // 4. The run's own directory holds no lines file: they went where the flag said.
    let keyed = PathBuf::from(run["dir"].as_str().unwrap());
    assert!(!keyed.parent().unwrap().join(RUN_LINES_FILE).exists());
}

/// The file rotates past its bound (M31-11): a file of 64 MiB and a byte gets the run's line
/// at its end and becomes the one older file; the next run starts a new file.
#[test]
fn the_file_rotates_past_its_bound() {
    // 1. A file one byte past the bound, sparse, ending a line.
    let tree = Tree::new("[check.ok]\nargv = [\"true\"]\n");
    let lines_dir = tree.tmp().join("lines");
    fs::create_dir(&lines_dir).unwrap();
    let current = lines_dir.join(RUN_LINES_FILE);
    let older = lines_dir.join(RUN_LINES_OLDER_FILE);
    let file = fs::File::create(&current).unwrap();
    file.set_len(MAX_RUN_LINES_BYTES).unwrap();
    drop(file);
    fs::OpenOptions::new()
        .append(true)
        .open(&current)
        .and_then(|mut file| std::io::Write::write_all(&mut file, b"\n"))
        .unwrap();
    let flag = ["--run-lines", lines_dir.to_str().unwrap()];

    // 2. One run: its line at the end of what is now the older file, no current file.
    let first = tree.run(&flag, &[]).ok();
    assert!(!current.exists(), "rotated");
    let text = fs::read(&older).unwrap();
    assert!(
        text.len() as u64 > MAX_RUN_LINES_BYTES + 1,
        "{} bytes",
        text.len()
    );
    let last = text[..text.len() - 1]
        .rsplit(|b| *b == b'\n')
        .next()
        .unwrap();
    let line: Value = serde_json::from_slice(last).unwrap();
    assert_eq!(line["run"], first["run"], "{line}");

    // 3. A second run starts a new file; the older one is as it was.
    let second = tree.run(&flag, &[]).ok();
    let lines = lines_of(&current);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0]["run"], second["run"]);
    assert_eq!(fs::metadata(&older).unwrap().len(), text.len() as u64);

    // 4. `check runs` reads both files: both runs' lines, newest first, and the long line of
    //    the sparse file skipped, never held.
    let page = runs(tree.tmp(), &lines_dir, &[]).ok();
    let got: Vec<&Value> = page["runs"].as_array().unwrap().iter().collect();
    assert_eq!(got.len(), 2, "{page}");
    assert_eq!(
        (&got[0]["run"], &got[1]["run"]),
        (&second["run"], &first["run"])
    );
    assert_eq!((&page["scanned"], &page["skipped"]), (&json!(3), &json!(1)));
}

/// Where lines go: with no flag and no variable, beside the directory's runs, or in the
/// workspace's state, with its tree and name; the variable alone names the directory; the flag
/// wins over it; a relative variable is refused with its reason.
#[test]
fn lines_go_beside_the_runs_or_where_the_caller_says() {
    // 1. The directory form, nothing named: beside the directory's runs.
    let tree = Tree::new("[check.ok]\nargv = [\"true\"]\n");
    let run = tree.run(&[], &[]).ok();
    let keyed = PathBuf::from(run["dir"].as_str().unwrap());
    let keyed = keyed.parent().unwrap();
    assert!(keyed.starts_with(tree.runs()), "{}", keyed.display());
    let lines = lines_of(&keyed.join(RUN_LINES_FILE));
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(
        (&lines[0]["run"], &lines[0]["vouched"]),
        (&run["run"], &json!(true))
    );

    // 2. The variable alone names the directory; the flag wins over it.
    let from_var = tree.tmp().join("from-var");
    let from_flag = tree.tmp().join("from-flag");
    let var = [(RUN_LINES_DIR_ENV, from_var.to_str().unwrap())];
    let by_var = tree.run(&[], &var).ok();
    let lines = lines_of(&from_var.join(RUN_LINES_FILE));
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0]["run"], by_var["run"]);
    let by_flag = tree.run(&["--run-lines", "from-flag"], &var).ok();
    let lines = lines_of(&from_flag.join(RUN_LINES_FILE));
    assert_eq!(
        lines[0]["run"], by_flag["run"],
        "relative to the working directory"
    );
    assert_eq!(
        lines_of(&from_var.join(RUN_LINES_FILE)).len(),
        1,
        "the flag won"
    );
    assert_eq!(
        lines_of(&keyed.join(RUN_LINES_FILE)).len(),
        1,
        "nothing more beside the runs"
    );

    // 3. A relative variable is refused, and nothing is made or run.
    let refused = tree.run(&[], &[(RUN_LINES_DIR_ENV, "rel")]);
    let error = refused.err("E_INVALID_ARG", 2);
    assert_eq!(error["details"]["reason"], "run_lines_dir", "{error}");
    assert!(
        error["hint"].as_str().unwrap().contains(RUN_LINES_DIR_ENV),
        "{error}"
    );
    assert!(!tree.tmp().join("rel").exists());

    // 4. The workspace form: in the workspace's state, with its tree and name, not vouched.
    let (lab, ws) = checked_lab();
    let run = lab.so(&["check", "run", "--ws", "w1"]).ok();
    let lines = lines_of(&ws.join(".swarmws/check-runs").join(RUN_LINES_FILE));
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(
        (
            &lines[0]["tree"],
            &lines[0]["workspace"],
            &lines[0]["vouched"]
        ),
        (&run["tree"], &json!("w1"), &json!(false))
    );
    assert_eq!(
        (&lines[0]["run"], &lines[0]["run_dir"]),
        (&run["run"], &run["dir"])
    );
}

/// A failed append changes no answer: with `--run-lines` naming a file, the envelope and the
/// exit code are those of the same run without it (ids, times and the run's path aside), and stderr
/// holds one warning.
#[test]
fn a_failed_append_changes_no_answer() {
    // 1. The same run twice: its lines unwritable, then written.
    let tree = Tree::new(THREE);
    let file = tree.tmp().join("a-file");
    fs::write(&file, "x").unwrap();
    let unwritten = tree.run(&["--run-lines", file.to_str().unwrap()], &[]);
    let written = tree.run(&[], &[]);

    // 2. The exit code and the envelope, but for what differs between any two runs.
    assert_eq!(unwritten.exit, written.exit, "{unwritten}");
    let shape = |out: &crate::ws::Out| -> Value {
        let mut envelope = out.envelope();
        envelope["meta"]["elapsed_ms"] = Value::Null;
        let run = envelope["error"]["details"]["run"].as_object_mut().unwrap();
        let (id, dir) = (run["run"].take(), run["dir"].take());
        let (id, dir) = (
            id.as_str().unwrap().to_owned(),
            dir.as_str().unwrap().to_owned(),
        );
        run.insert("duration_ms".to_owned(), Value::Null);
        for check in run["checks"].as_array_mut().unwrap() {
            check["duration_ms"] = Value::Null;
            check["log"] = Value::from(check["log"].as_str().unwrap().replace(&dir, "<dir>"));
        }
        let text = envelope.to_string().replace(&id, "<run>");
        serde_json::from_str(&text).unwrap()
    };
    assert_eq!(shape(&unwritten), shape(&written));

    // 3. One warning, on stderr; the file untouched; no warning when the lines were written.
    let warnings: Vec<&str> = (unwritten.stderr.lines())
        .filter(|line| line.starts_with("warning: run lines:"))
        .collect();
    assert_eq!(warnings.len(), 1, "{unwritten}");
    assert!(warnings[0].contains(file.to_str().unwrap()), "{unwritten}");
    assert_eq!(fs::read_to_string(&file).unwrap(), "x");
    assert!(!written.stderr.contains("warning: run lines:"), "{written}");
}

/// On Linux, a check that lasts leaves the peak of its group's resident memory.
#[cfg(target_os = "linux")]
#[test]
fn a_check_that_lasts_leaves_its_peak_memory() {
    let tree = Tree::new(THREE);
    let run = failed_run(&tree.run(&[], &[]));
    let keyed = PathBuf::from(run["dir"].as_str().unwrap());
    let lines = lines_of(&keyed.parent().unwrap().join(RUN_LINES_FILE));
    let slow = lines.iter().find(|line| line["check"] == "slow").unwrap();
    assert!(
        slow["peak_rss_kib"].as_u64().is_some_and(|kib| kib > 0),
        "{slow}"
    );
}

/// Filters over a fixture file give pinned pages (M31-12): each of the filters, and two
/// together, name the lines they should, newest first; the lines that do not read are counted.
#[test]
fn filters_over_a_fixture_file_give_pinned_pages() {
    // 1. No filter: the twelve lines, newest first by key, from the directory named.
    let (tmp, dir) = fixture_lines();
    let plain = runs(tmp.path(), &dir, &[]);
    assert_eq!(plain.envelope()["op"], "check_runs", "{plain}");
    let whole = plain.ok();
    let checks = ["unit", "test", "lint", "build"];
    let newest_first: Vec<(&str, &str)> = [R3, R2, R1]
        .iter()
        .flat_map(|run| checks.iter().map(move |check| (*run, *check)))
        .collect();
    assert_eq!(named(&whole), pairs(&newest_first));
    assert_eq!(
        (&whole["source"], &whole["vouched"], &whole["dir"]),
        (
            &json!("run_lines"),
            &json!(true),
            &json!(dir.to_str().unwrap())
        )
    );
    assert_eq!(
        (&whole["scanned"], &whole["skipped"]),
        (&json!(17), &json!(3))
    );
    assert!(whole.get("cursor").is_none(), "{whole}");

    // 2. A line as the file holds it, field for field.
    let build = &whole["runs"][11];
    let first: Value =
        serde_json::from_slice(FIXTURE.split(|b| *b == b'\n').next().unwrap()).unwrap();
    assert_eq!(*build, first);
    assert_eq!(
        (&build["peak_rss_kib"], &build["wait_ms"]),
        (&json!(20_480), &json!(0))
    );

    // 3. Each filter.
    let of = |args: &[&str]| named(&runs(tmp.path(), &dir, args).ok());
    assert_eq!(
        of(&["--check", "lint"]),
        pairs(&[(R3, "lint"), (R2, "lint"), (R1, "lint")])
    );
    assert_eq!(of(&["--check", "lint", "--check", "unit"]).len(), 6);
    assert_eq!(
        of(&["--tree", "bbbbbbbbbbbb"]),
        pairs(&[(R2, "unit"), (R2, "test"), (R2, "lint"), (R2, "build")])
    );
    assert_eq!(
        of(&["--outcome", "failed"]),
        pairs(&[(R3, "test"), (R1, "lint")])
    );
    assert_eq!(
        of(&[
            "--outcome",
            "timed_out",
            "--outcome",
            "cancelled",
            "--outcome",
            "not_started"
        ]),
        pairs(&[(R3, "build"), (R2, "unit"), (R1, "test")])
    );
    assert_eq!(of(&["--run", R1]), pairs(&newest_first[8..]));
    assert_eq!(
        of(&["--since", "1700000100000", "--until", "1700000200000"]),
        pairs(&newest_first[4..8])
    );

    // 4. Two together.
    assert_eq!(
        of(&["--check", "test", "--check", "lint", "--outcome", "passed"]),
        pairs(&[(R3, "lint"), (R2, "test"), (R2, "lint")])
    );

    // 5. What does not read as a filter: its reason; `not_run` is no outcome of a line.
    let reason = |args: &[&str]| {
        let error = runs(tmp.path(), &dir, args).err("E_INVALID_ARG", 2);
        error["details"]["reason"].as_str().unwrap().to_owned()
    };
    assert_eq!(reason(&["--limit", "0"]), "limit");
    assert_eq!(reason(&["-n", "1001"]), "limit");
    assert_eq!(reason(&["--tree", "abc"]), "tree");
    assert_eq!(reason(&["--since", "2", "--until", "1"]), "time");
    assert_eq!(reason(&["--check", "Lint"]), "check");
    assert_eq!(reason(&["--run", "nope"]), "run");
    let not_run = runs(tmp.path(), &dir, &["--outcome", "not_run"]);
    assert_eq!(not_run.exit, 2, "{not_run}");
    assert!(
        not_run.stderr.contains("not_run") || not_run.stdout.contains("not_run"),
        "{not_run}"
    );
}

/// A cursor resumes (M31-12): three pages of four are the one page of a hundred, the last with
/// no cursor; lines a run appends between two pages change no later page; a mangled cursor is
/// refused.
#[test]
fn a_cursor_resumes() {
    // 1. The whole, then its first page of four.
    let (tmp, dir) = fixture_lines();
    let whole = runs(tmp.path(), &dir, &["--limit", "100"]).ok();
    let first = runs(tmp.path(), &dir, &["--limit", "4"]).ok();
    let cursor = first["cursor"].as_str().expect("more match").to_owned();

    // 2. A run appends its lines to the same directory between the two pages.
    let tree = Tree::new(THREE);
    let ran = failed_run(&tree.run(&["--run-lines", dir.to_str().unwrap()], &[]));
    let text = fs::read_to_string(dir.join(RUN_LINES_FILE)).unwrap();
    assert_eq!(
        text.lines().count(),
        17 + 2,
        "the first appended line ends the torn one"
    );

    // 3. The second and third pages are what they would have been.
    let second = runs(tmp.path(), &dir, &["--limit", "4", "--cursor", &cursor]).ok();
    let cursor = second["cursor"].as_str().expect("more match").to_owned();
    let third = runs(tmp.path(), &dir, &["-n", "4", "--cursor", &cursor]).ok();
    assert!(third.get("cursor").is_none(), "the last page: {third}");
    let joined: Vec<Value> = [&first, &second, &third]
        .iter()
        .flat_map(|page| page["runs"].as_array().unwrap().clone())
        .collect();
    assert_eq!(Value::from(joined), whole["runs"]);

    // 4. A first page now starts with the run that appended.
    let fresh = runs(tmp.path(), &dir, &["--limit", "4"]).ok();
    assert_eq!(fresh["runs"][0]["run"], ran["run"], "{fresh}");
    assert_eq!(fresh["runs"][3]["run"], R3, "{fresh}");

    // 5. A mangled cursor is refused with its reason.
    let mangled = format!("{cursor}.x");
    for bad in [mangled.as_str(), "r1", "0"] {
        let error = runs(tmp.path(), &dir, &["--cursor", bad]).err("E_INVALID_ARG", 2);
        assert_eq!(error["details"]["reason"], "cursor", "{bad}: {error}");
    }
}

/// A directory or a workspace with no run yet gives an empty page, exit 0; a lines file that is
/// a link is `E_IO`; a named directory that is a file is refused; the directory form reads the
/// lines beside its runs.
#[test]
fn no_lines_is_an_empty_page_and_a_file_that_is_none_is_refused() {
    // 1. A directory never run: an empty page, vouched, nothing made.
    let tree = Tree::new(THREE);
    let dir = tree.dir();
    let line = ["check", "runs", "--dir", dir.to_str().unwrap()];
    let runs_root = tree.runs();
    let env = [("SWARMOPS_CHECK_RUNS_DIR", runs_root.to_str().unwrap())];
    let none = run_in(tree.tmp(), &env, &line, None).ok();
    assert_eq!(
        (&none["runs"], &none["scanned"], &none["vouched"]),
        (&json!([]), &json!(0), &json!(true))
    );
    assert!(!runs_root.exists(), "reading makes nothing");

    // 2. After a run: its three lines, newest first, from beside its runs.
    let ran = failed_run(&tree.run(&[], &[]));
    let page = run_in(tree.tmp(), &env, &line, None).ok();
    assert_eq!(
        named(&page)
            .iter()
            .map(|(_, check)| check.as_str())
            .collect::<Vec<_>>(),
        ["slow", "ok", "bad"]
    );
    let keyed = PathBuf::from(ran["dir"].as_str().unwrap());
    assert_eq!(page["dir"], keyed.parent().unwrap().to_str().unwrap());
    let failed = run_in(
        tree.tmp(),
        &env,
        &[&line[..], &["--outcome", "failed"]].concat(),
        None,
    )
    .ok();
    assert_eq!(named(&failed).len(), 1, "{failed}");

    // 3. The lines file a link: E_IO, naming it. A named directory that is a file: refused.
    let (tmp, lines) = fixture_lines();
    let file = lines.join(RUN_LINES_FILE);
    fs::rename(&file, lines.join("elsewhere")).unwrap();
    std::os::unix::fs::symlink(lines.join("elsewhere"), &file).unwrap();
    let linked = runs(tmp.path(), &lines, &[]).err("E_IO", 1);
    assert_eq!(
        linked["details"]["path"],
        file.to_str().unwrap(),
        "{linked}"
    );
    let refused = runs(tmp.path(), &lines.join("elsewhere"), &[]).err("E_INVALID_ARG", 2);
    assert_eq!(refused["details"]["reason"], "run_lines_dir", "{refused}");
}

/// The workspace form and the tool: `check runs --ws` reads the workspace's own lines, not
/// vouched, and the MCP tool `check_runs` answers as the command does, filters and cursor
/// included.
#[test]
fn the_tool_check_runs_answers_as_the_command_does() {
    // 1. Two runs of a workspace's checks.
    let (lab, ws) = checked_lab();
    let first = lab.so(&["check", "run", "--ws", "w1"]).ok();
    let second = lab.so(&["check", "run", "--ws", "w1"]).ok();

    // 2. The command: both runs' lines, newest first, from the workspace's state.
    let page = lab.so(&["check", "runs", "--ws", "w1"]).ok();
    let state = ws.join(".swarmws/check-runs");
    assert_eq!(
        (&page["vouched"], &page["dir"]),
        (&json!(false), &json!(state.to_str().unwrap()))
    );
    let got: Vec<&Value> = page["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|line| &line["run"])
        .collect();
    assert_eq!(got, [&second["run"], &first["run"]]);
    assert_eq!(page["runs"][0]["tree"], second["tree"]);

    // 3. The tool, with no filter, with filters, and with a cursor.
    let mut server = crate::mcp::Server::start(&["--repo", lab.path(), "--as", AGENT], &[]);
    server.initialize();
    let tool = server.ok("check_runs", json!({"workspace": "w1"}));
    assert_eq!(tool["result"], page);
    let run = first["run"].as_str().unwrap();
    let tree = &first["tree"].as_str().unwrap()[..12];
    let filtered = lab
        .so(&[
            "check",
            "runs",
            "--ws",
            "w1",
            "--run",
            run,
            "--check",
            "env",
            "--tree",
            tree,
            "--outcome",
            "passed",
        ])
        .ok();
    assert_eq!(filtered["runs"].as_array().unwrap().len(), 1, "{filtered}");
    let arguments = json!({"workspace": "w1", "run": run, "checks": ["env"], "tree": tree, "outcomes": ["passed"]});
    assert_eq!(server.ok("check_runs", arguments)["result"], filtered);
    let one = lab
        .so(&["check", "runs", "--ws", "w1", "--limit", "1"])
        .ok();
    let cursor = one["cursor"].as_str().expect("one more");
    let next = lab
        .so(&["check", "runs", "--ws", "w1", "-n", "1", "--cursor", cursor])
        .ok();
    assert_eq!(next["runs"][0]["run"], first["run"]);
    let by_tool = server.ok(
        "check_runs",
        json!({"workspace": "w1", "limit": 1, "cursor": cursor}),
    );
    assert_eq!(by_tool["result"], next);

    // 4. An outcome a line never has, and a workspace the store does not know.
    let call = server.call(
        "check_runs",
        json!({"workspace": "w1", "outcomes": ["not_run"]}),
    );
    assert_eq!(
        call.envelope["error"]["details"]["reason"], "outcome",
        "{:?}",
        call.envelope
    );
    let unknown = lab.so(&["check", "runs", "--ws", "nope"]);
    assert_ne!(unknown.exit, 0, "{unknown}");
}
