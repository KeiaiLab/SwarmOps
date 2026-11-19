//! Run lines, `check runs` and `check log --failed` (M31-11, M31-12, M31-13) through the binary:
//! the line each check of a run leaves, where it goes, its rotation, and a failed append.
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
use crate::ws::run_in;

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
