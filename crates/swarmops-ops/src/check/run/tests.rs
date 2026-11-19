//! `check run` in the directory form: selection, outcomes, the environment, the runs kept.

use std::ffi::OsString;

use camino::{Utf8Path, Utf8PathBuf};
use serde_json::Value;
use swarmops_api::{CheckOutcome, CheckStage, OpError};

use super::*;
use crate::check::log::{Kind, parse};

/// A directory holding `.swarmops/checks.toml` with `checks`, and a runs root beside it.
fn tree(checks: &str) -> (tempfile::TempDir, Utf8PathBuf, Utf8PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let base = Utf8PathBuf::from_path_buf(std::fs::canonicalize(tmp.path()).unwrap()).unwrap();
    let dir = base.join("tree");
    std::fs::create_dir_all(dir.join(".swarmops")).unwrap();
    std::fs::write(
        dir.join(".swarmops/checks.toml"),
        format!("version = 1\n{checks}"),
    )
    .unwrap();
    (tmp, dir, base.join("runs"))
}

/// The caller's environment of the tests: a few names, one of them secret-looking.
fn caller(name: &str) -> Option<OsString> {
    match name {
        "PATH" => Some(OsString::from("/usr/bin:/bin")),
        "HOME" => Some(OsString::from("/home/agent")),
        "FOO" => Some(OsString::from("bar")),
        "SWARMOPS_TEST_LEAK" => Some(OsString::from("1")),
        "SWARMOPS_CHECK_NAME" => Some(OsString::from("forged")),
        _ => None,
    }
}

/// Runs the `stage` checks `names` of `dir`, runs below `runs`, passing `pass`.
fn run(
    dir: &Utf8Path,
    runs: &Utf8Path,
    names: &[&str],
    pass: &[&str],
) -> Result<CheckRunResp, OpError> {
    let mut opts = RunOpts::new(&caller);
    opts.runs_root = Some(runs.to_owned());
    opts.pass = pass.iter().map(|p| (*p).to_owned()).collect();
    let names: Vec<String> = names.iter().map(|n| (*n).to_owned()).collect();
    run_dir(dir, CheckStage::BeforeLand, &names, &opts)
}

/// The run an `E_CHECK_FAILED` carries.
fn run_of(error: &OpError) -> CheckRunResp {
    assert_eq!(error.code, "E_CHECK_FAILED", "{error:?}");
    serde_json::from_value(error.details["run"].clone()).unwrap()
}

/// The names of the variables a check printed with `/usr/bin/env` into its log.
fn printed(log: &str) -> Vec<(String, String)> {
    let text = std::fs::read_to_string(log).unwrap();
    let mut vars: Vec<(String, String)> = (parse(&text).into_iter())
        .filter(|line| line.kind == Kind::Out)
        .filter_map(|line| {
            let (name, value) = line.text.split_once('=')?;
            Some((name.to_owned(), value.to_owned()))
        })
        .collect();
    vars.sort();
    vars
}

/// One passing and one failing check: both run, in name order; the run is `E_CHECK_FAILED`
/// naming the failing one with its exit code and tails, the passing one without tails.
#[test]
fn a_failing_check_fails_the_run_and_every_check_runs() {
    let (_tmp, dir, runs) = tree(
        "[check.ok]\nargv = [\"/usr/bin/true\"]\n\
         [check.bad]\nargv = [\"/bin/sh\", \"-c\", \"echo out; echo boom >&2; exit 3\"]\n",
    );
    let error = run(&dir, &runs, &[], &[]).unwrap_err();
    assert_eq!(
        error.message,
        "1 of 2 checks did not pass: bad failed (exit 3)"
    );
    let resp = run_of(&error);
    let names: Vec<(&str, CheckOutcome)> = resp
        .checks
        .iter()
        .map(|c| (c.name.as_str(), c.outcome))
        .collect();
    assert_eq!(
        names,
        [("bad", CheckOutcome::Failed), ("ok", CheckOutcome::Passed)]
    );
    assert_eq!(resp.failed, ["bad"]);
    let bad = &resp.checks[0];
    assert_eq!(
        (bad.exit_code, bad.stderr_tail.as_deref()),
        (Some(3), Some("boom\n"))
    );
    assert_eq!(bad.stdout_tail.as_deref(), Some("out\n"));
    assert_eq!(resp.checks[1].stdout_tail, None);
    assert!(resp.vouched && !resp.cancelled && resp.tree.is_none());
    assert!(error.hint.unwrap().contains("check log bad --dir"));

    // The record says the run ended, with the same result.
    let record: Value =
        serde_json::from_slice(&std::fs::read(Utf8Path::new(&resp.dir).join(RUN_FILE)).unwrap())
            .unwrap();
    assert_eq!(
        (&record["format"], &record["state"]),
        (&Value::from(1), &Value::from("ended"))
    );
    assert_eq!(record["result"], serde_json::to_value(&resp).unwrap());
}

/// A check of `.swarmops/checks.toml` gets the pinned variables and, of the caller's, only the
/// fixed list and what `--pass` names; a pinned name is never the caller's.
#[test]
fn a_check_gets_the_pinned_variables_and_only_those_listed() {
    let (_tmp, dir, runs) = tree("[check.env]\nargv = [\"/usr/bin/env\"]\n");
    let resp = run(&dir, &runs, &[], &[]).unwrap();
    assert_eq!(resp.inherited_env, ["HOME", "PATH"]);
    assert!(!resp.inherited_all);
    let vars = printed(resp.checks[0].log.as_deref().unwrap());
    let names: Vec<&str> = vars.iter().map(|(n, _)| n.as_str()).collect();
    let mut want = vec![
        "HOME",
        "PATH",
        env::ENV_ARTIFACTS,
        env::ENV_ATTEMPT,
        env::ENV_CHANGED,
        env::ENV_NAME,
        env::ENV_OUTPUTS,
        env::ENV_REPORT,
        env::ENV_RUN,
        env::ENV_STAGE,
        env::ENV_SUMMARY,
        env::ENV_TREE,
    ];
    want.sort_unstable();
    assert_eq!(names, want);
    let value = |name: &str| {
        vars.iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    };
    assert_eq!(value(env::ENV_NAME), Some("env"));
    assert_eq!(value(env::ENV_STAGE), Some("before-land"));
    assert_eq!(value(env::ENV_TREE), Some(""));
    assert_eq!(value(env::ENV_RUN), Some(resp.run.as_str()));
    let own = format!("{}/checks/env", resp.dir);
    assert_eq!(
        value(env::ENV_REPORT),
        Some(format!("{own}/report.jsonl").as_str())
    );
    assert!(Utf8Path::new(&own).join("artifacts").is_dir());
    assert!(!Utf8Path::new(&own).join("report.jsonl").exists());

    // With --pass FOO, FOO too; a name the caller has not set is left out.
    let resp = run(&dir, &runs, &[], &["FOO", "UNSET_HERE"]).unwrap();
    assert_eq!(resp.inherited_env, ["FOO", "HOME", "PATH"]);
    let vars = printed(resp.checks[0].log.as_deref().unwrap());
    assert!(vars.contains(&("FOO".to_owned(), "bar".to_owned())));
    assert!(
        !vars
            .iter()
            .any(|(n, _)| n == "SWARMOPS_TEST_LEAK" || n == "UNSET_HERE")
    );
}

/// `--pass` takes a variable's name, not a pinned one, at most 64 of them.
#[test]
fn a_bad_pass_is_refused_before_anything_runs() {
    let (_tmp, dir, runs) = tree("[check.ok]\nargv = [\"/usr/bin/true\"]\n");
    for bad in [
        "",
        "1A",
        "A-B",
        "SWARMOPS_CHECK_NAME",
        "SWARMOPS_WS",
        &"A".repeat(129),
    ] {
        let error = run(&dir, &runs, &[], &[bad]).unwrap_err();
        assert_eq!(
            (error.code.as_str(), &error.details["reason"]),
            ("E_INVALID_ARG", &Value::from("pass")),
            "{bad:?}"
        );
    }
    let many: Vec<String> = (0..65).map(|n| format!("V{n}")).collect();
    let many: Vec<&str> = many.iter().map(String::as_str).collect();
    assert_eq!(
        run(&dir, &runs, &[], &many).unwrap_err().details["reason"],
        "pass"
    );
    assert!(!runs.exists(), "nothing ran");
}

/// The legacy check keeps the caller's whole environment, the pinned names set over it.
#[test]
fn the_legacy_check_inherits_everything() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = Utf8PathBuf::from_path_buf(std::fs::canonicalize(tmp.path()).unwrap()).unwrap();
    std::fs::create_dir_all(dir.join(".swarmops")).unwrap();
    std::fs::write(
        dir.join(".swarmops/workspaces.json"),
        r#"{"check": {"argv": ["/usr/bin/env"]}}"#,
    )
    .unwrap();
    let runs = dir.join("runs");
    let resp = run(&dir, &runs, &[], &[]).unwrap();
    assert_eq!(
        (resp.file.as_str(), resp.inherited_all),
        (".swarmops/workspaces.json", true)
    );
    assert_eq!(resp.inherited_env, Vec::<String>::new());
    let vars = printed(resp.checks[0].log.as_deref().unwrap());
    let value = |name: &str| vars.iter().find(|(n, _)| n == name).map(|(_, v)| v.clone());
    assert_eq!(value(env::ENV_NAME).as_deref(), Some("default"));
    // The process's own environment reaches it: this test's PATH, whatever it is.
    assert_eq!(value("PATH"), std::env::var("PATH").ok());
}

/// Names select checks of the stage; an unknown name, one of another stage, and a selection of
/// none are refused before anything runs; a name given twice runs once.
#[test]
fn names_select_checks_of_the_stage() {
    let (_tmp, dir, runs) = tree(
        "[check.a]\nargv = [\"/usr/bin/true\"]\n\
         [check.b]\nargv = [\"/usr/bin/true\"]\n\
         [check.m]\nargv = [\"/usr/bin/true\"]\nstages = [\"manual\"]\n",
    );
    let resp = run(&dir, &runs, &["b", "b"], &[]).unwrap();
    let names: Vec<&str> = resp.checks.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["b"]);
    let all = run(&dir, &runs, &[], &[]).unwrap();
    assert_eq!(all.checks.len(), 2);

    let unknown = run(&dir, &runs, &["zz"], &[]).unwrap_err();
    assert_eq!(
        (&unknown.details["reason"], &unknown.details["check"]),
        (&Value::from("check"), &Value::from("zz"))
    );
    let other = run(&dir, &runs, &["m"], &[]).unwrap_err();
    assert_eq!(other.details["reason"], "stage");
    assert_eq!(other.details["stages"], serde_json::json!(["manual"]));
    let mut opts = RunOpts::new(&caller);
    opts.runs_root = Some(runs.clone());
    let none = run_dir(&dir, CheckStage::Deploy, &[], &opts).unwrap_err();
    assert_eq!(
        (none.code.as_str(), &none.details["reason"]),
        ("E_INVALID_ARG", &Value::from("no_checks"))
    );
    let (_tmp2, empty, runs2) = tree("");
    std::fs::remove_file(empty.join(".swarmops/checks.toml")).unwrap();
    assert_eq!(
        run(&empty, &runs2, &[], &[]).unwrap_err().details["reason"],
        "no_checks"
    );
    assert!(!runs2.exists(), "nothing ran");
}

/// A program that does not start, and a directory that is not there, are `not_started`, each
/// with its reason; their logs hold the start and the end.
#[test]
fn a_check_that_cannot_start_is_not_started() {
    let (_tmp, dir, runs) = tree(
        "[check.gone]\nargv = [\"swarmops-no-such-program\"]\n\
         [check.nodir]\nargv = [\"/usr/bin/true\"]\ndir = \"missing\"\n",
    );
    let resp = run_of(&run(&dir, &runs, &[], &[]).unwrap_err());
    let gone = &resp.checks[0];
    assert_eq!(
        (gone.outcome, gone.not_started),
        (CheckOutcome::NotStarted, Some(CheckNotStarted::Program))
    );
    assert!(
        gone.message
            .as_deref()
            .unwrap()
            .contains("swarmops-no-such-program")
    );
    assert_eq!(gone.stdout_tail, None);
    let nodir = &resp.checks[1];
    assert_eq!(nodir.not_started, Some(CheckNotStarted::Dir));
    assert!(nodir.message.as_deref().unwrap().contains("missing"));
    let text = std::fs::read_to_string(gone.log.as_deref().unwrap()).unwrap();
    let lines = parse(&text);
    assert_eq!(lines.len(), 2, "{text}");
    assert!(lines[0].event("start").is_some() && lines[1].event("end").is_some());
}

/// A stop flag set before the run starts runs nothing: every check `not_run`, without a log,
/// and the run `cancelled`.
#[test]
fn a_stopped_run_runs_nothing_more() {
    let (_tmp, dir, runs) = tree("[check.a]\nargv = [\"/usr/bin/true\"]\n");
    let stop = AtomicBool::new(true);
    let mut opts = RunOpts::new(&caller);
    opts.runs_root = Some(runs);
    opts.stop = Some(&stop);
    let error = run_dir(&dir, CheckStage::BeforeLand, &[], &opts).unwrap_err();
    assert_eq!(error.message, "the run was stopped: 1 not run");
    let resp = run_of(&error);
    assert!(resp.cancelled);
    assert_eq!(
        (resp.checks[0].outcome, &resp.checks[0].log),
        (CheckOutcome::NotRun, &None)
    );
}

/// A directory keeps its newest 20 runs: the 21st removes the oldest, but never one whose lock
/// is held. No runs root: refused, naming the variable.
#[test]
fn a_directory_keeps_20_runs() {
    let (_tmp, dir, runs) = tree("[check.a]\nargv = [\"/usr/bin/true\"]\n");
    let first = run(&dir, &runs, &[], &[]).unwrap();
    let keyed = Utf8Path::new(&first.dir).parent().unwrap().to_owned();
    let held_lock = File::open(Utf8Path::new(&first.dir).join(RUN_LOCK)).unwrap();
    held_lock.lock().unwrap();
    let count = || {
        (keyed.read_dir_utf8().unwrap().flatten())
            .filter(|e| e.file_type().unwrap().is_dir())
            .count()
    };
    for _ in 0..20 {
        run(&dir, &runs, &[], &[]).unwrap();
    }
    assert_eq!(count(), 21, "the oldest's lock is held: it stays");
    held_lock.unlock().unwrap();
    run(&dir, &runs, &[], &[]).unwrap();
    assert_eq!(count(), 20);
    assert!(!Utf8Path::new(&first.dir).exists());
    assert_eq!(
        std::fs::read_to_string(keyed.join("dir")).unwrap(),
        format!("{dir}\n")
    );

    let opts = RunOpts::new(&caller);
    let nowhere = run_dir(&dir, CheckStage::BeforeLand, &[], &opts).unwrap_err();
    assert!(nowhere.message.contains(CHECK_RUNS_DIR_ENV), "{nowhere:?}");
}

/// A lock that cannot be opened to write (another account's run, a read-only copy) is still tried
/// for the run that holds it, and one that cannot be opened at all is taken as held: either way
/// the run is not pruned. Running as root, which opens any file, the second half is passed over.
#[test]
fn a_lock_that_cannot_be_written_is_still_held() {
    use std::os::unix::fs::PermissionsExt as _;

    let (_tmp, dir, runs) = tree("[check.a]\nargv = [\"/usr/bin/true\"]\n");
    let first = run(&dir, &runs, &[], &[]).unwrap();
    let lock_path = Utf8Path::new(&first.dir).join(RUN_LOCK);
    let held_lock = File::open(&lock_path).unwrap();
    held_lock.lock().unwrap();
    let mode = |bits: u32| {
        std::fs::set_permissions(&lock_path, std::fs::Permissions::from_mode(bits)).unwrap();
    };

    // 1. Read-only: held, as it is.
    mode(0o444);
    assert!(held(&lock_path));
    held_lock.unlock().unwrap();
    assert!(!held(&lock_path));

    // 2. Unreadable: taken as held, and the run outlives the pruning of 20 runs after it.
    mode(0o000);
    if File::open(&lock_path).is_err() {
        assert!(held(&lock_path));
        for _ in 0..20 {
            run(&dir, &runs, &[], &[]).unwrap();
        }
        assert!(Utf8Path::new(&first.dir).exists());
    }
    mode(0o644);
}

/// The directories a run makes are its owner's alone: the runs root, the directory's key
/// directory and the run's own, as a workspace's `.swarmws/` is, since a log holds whatever a
/// check printed.
#[test]
fn a_run_s_directories_are_its_owner_s_alone() {
    use std::os::unix::fs::PermissionsExt as _;

    let (_tmp, dir, runs) = tree("[check.a]\nargv = [\"/usr/bin/true\"]\n");
    let ran = run(&dir, &runs, &[], &[]).unwrap();
    let run_dir = Utf8Path::new(&ran.dir);
    let key = run_dir.parent().unwrap();
    for made in [
        &runs,
        key,
        run_dir,
        &run_dir.join("logs"),
        &run_dir.join("checks/a"),
    ] {
        let mode = std::fs::metadata(made).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, RUNS_DIR_MODE, "{made}: {mode:o}");
    }
}

/// A run of one directory removes the runs of another directory that is gone, unless one of them
/// still holds its lock; the runs of a directory that is there stay.
#[test]
fn the_runs_of_a_directory_that_is_gone_go() {
    // 1. Runs of three directories: one stays, one is removed, one is removed while running.
    let checks = "version = 1\n[check.a]\nargv = [\"/usr/bin/true\"]\n";
    let (_tmp, dir, runs) = tree("[check.a]\nargv = [\"/usr/bin/true\"]\n");
    let base = dir.parent().unwrap();
    let other = |name: &str| {
        let made = base.join(name);
        std::fs::create_dir_all(made.join(".swarmops")).unwrap();
        std::fs::write(made.join(".swarmops/checks.toml"), checks).unwrap();
        made
    };
    let (kept, gone, busy) = (other("kept"), other("gone"), other("busy"));
    let key_of = |dir: &Utf8Path| {
        let ran = run(dir, &runs, &[], &[]).unwrap();
        let key = Utf8Path::new(&ran.dir).parent().unwrap().to_owned();
        (key, Utf8Path::new(&ran.dir).join(RUN_LOCK))
    };
    let (kept_key, _) = key_of(&kept);
    let (gone_key, _) = key_of(&gone);
    let (busy_key, busy_lock) = key_of(&busy);
    std::fs::remove_dir_all(&gone).unwrap();
    std::fs::remove_dir_all(&busy).unwrap();
    let lock = File::open(&busy_lock).unwrap();
    lock.lock().unwrap();

    // 2. A run of the first: the gone directory's runs go; the others stay.
    run(&dir, &runs, &[], &[]).unwrap();
    assert!(kept_key.exists() && busy_key.exists());
    assert!(!gone_key.exists());

    // 3. Its lock let go, the next run removes the busy one's too.
    lock.unlock().unwrap();
    run(&dir, &runs, &[], &[]).unwrap();
    assert!(!busy_key.exists() && kept_key.exists());
}

/// The run lines beside the runs of `resp`'s directory, in file order.
fn run_lines(resp: &CheckRunResp) -> Vec<swarmops_api::CheckRunLine> {
    let keyed = Utf8Path::new(&resp.dir).parent().unwrap();
    let text = std::fs::read_to_string(keyed.join(lines::RUN_LINES_FILE)).unwrap();
    (text.lines())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// A run of three checks leaves three lines beside the directory's runs, in run order: each
/// with the run's id and directory, its check's outcome and duration, a wait no shorter than
/// the one before, and a start at or after it.
#[test]
fn three_checks_leave_three_lines_in_run_order() {
    let (_tmp, dir, runs) = tree(
        "[check.a]\nargv = [\"/bin/sh\", \"-c\", \"sleep 0.05\"]\n\
         [check.b]\nargv = [\"/bin/sh\", \"-c\", \"exit 3\"]\n\
         [check.c]\nargv = [\"/usr/bin/true\"]\n",
    );
    let resp = run_of(&run(&dir, &runs, &[], &[]).unwrap_err());
    let lines = run_lines(&resp);
    let names: Vec<&str> = lines.iter().map(|line| line.check.as_str()).collect();
    assert_eq!(names, ["a", "b", "c"]);
    for (line, out) in lines.iter().zip(&resp.checks) {
        assert_eq!(
            (line.run.as_str(), line.run_dir.as_deref()),
            (resp.run.as_str(), Some(resp.dir.as_str()))
        );
        assert_eq!(
            (line.outcome, line.duration_ms, line.exit_code),
            (out.outcome, out.duration_ms, out.exit_code)
        );
        assert_eq!(line.definition_digest, out.definition_digest);
        assert_eq!((line.attempt, line.stage), (1, CheckStage::BeforeLand));
        assert!(line.vouched && line.tree.is_none() && line.workspace.is_none());
        assert!(lines::valid(line), "{line:?}");
    }
    assert!(lines[0].wait_ms <= lines[1].wait_ms && lines[1].wait_ms <= lines[2].wait_ms);
    assert!(lines[1].wait_ms >= 50, "b waited for a: {lines:?}");
    assert!(lines[0].at <= lines[1].at && lines[1].at <= lines[2].at);

    // The line's start is its log's.
    let log = std::fs::read_to_string(resp.checks[0].log.as_deref().unwrap()).unwrap();
    let start = parse(&log)[0].event("start").unwrap();
    assert_eq!(start["started_at_ms"], lines[0].at, "{log}");

    // A second run adds its lines after them; a directory the caller names gets them instead.
    let elsewhere = runs.join("elsewhere");
    let mut opts = RunOpts::new(&caller);
    opts.runs_root = Some(runs.clone());
    opts.lines = Some(elsewhere.clone());
    let again = run_of(&run_dir(&dir, CheckStage::BeforeLand, &[], &opts).unwrap_err());
    assert_eq!(
        run_lines(&resp).len(),
        3,
        "the directory's own file is as it was"
    );
    let text = std::fs::read_to_string(elsewhere.join(lines::RUN_LINES_FILE)).unwrap();
    assert_eq!(text.lines().count(), 3, "{text}");
    assert!(text.contains(&again.run), "{text}");
}

/// A check that a stop cut short leaves a `cancelled` line, and the checks never reached none;
/// a program that does not start leaves a `not_started` line of no duration.
#[test]
fn a_check_not_reached_leaves_no_line() {
    // 1. `a` is stopped once it runs; `b` is never reached.
    let (_tmp, dir, runs) = tree(
        "[check.a]\nargv = [\"/bin/sh\", \"-c\", \"touch started; sleep 60\"]\n\
         [check.b]\nargv = [\"/usr/bin/true\"]\n",
    );
    let stop = AtomicBool::new(false);
    let mut opts = RunOpts::new(&caller);
    opts.runs_root = Some(runs.clone());
    opts.stop = Some(&stop);
    let started = dir.join("started");
    let error = std::thread::scope(|scope| {
        scope.spawn(|| {
            let deadline = Instant::now() + Duration::from_secs(60);
            while !started.exists() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            stop.store(true, Ordering::SeqCst);
        });
        run_dir(&dir, CheckStage::BeforeLand, &[], &opts).unwrap_err()
    });
    let resp = run_of(&error);
    assert_eq!(
        (resp.checks[0].outcome, resp.checks[1].outcome),
        (CheckOutcome::Cancelled, CheckOutcome::NotRun)
    );
    let lines = run_lines(&resp);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(
        (lines[0].check.as_str(), lines[0].outcome),
        ("a", CheckOutcome::Cancelled)
    );

    // 2. A program that is not there: `not_started`, duration 0, no exit code.
    let (_tmp, dir, runs) = tree("[check.gone]\nargv = [\"swarmops-no-such-program\"]\n");
    let resp = run_of(&run(&dir, &runs, &[], &[]).unwrap_err());
    let lines = run_lines(&resp);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(
        (lines[0].outcome, lines[0].duration_ms, lines[0].exit_code),
        (CheckOutcome::NotStarted, 0, None)
    );
    assert!(lines[0].at > 0, "when it was tried: {lines:?}");

    // 3. A stop before the first check: no line, no file.
    let (_tmp, dir, runs) = tree("[check.a]\nargv = [\"/usr/bin/true\"]\n");
    let stop = AtomicBool::new(true);
    let mut opts = RunOpts::new(&caller);
    opts.runs_root = Some(runs);
    opts.stop = Some(&stop);
    let resp = run_of(&run_dir(&dir, CheckStage::BeforeLand, &[], &opts).unwrap_err());
    let keyed = Utf8Path::new(&resp.dir).parent().unwrap();
    assert!(!keyed.join(lines::RUN_LINES_FILE).exists(), "no line");
}

/// Lines that cannot be appended change nothing a run answers: its outcomes, its record and its
/// error are those of a run whose lines were written.
#[test]
fn a_failed_append_changes_no_answer() {
    let (_tmp, dir, runs) = tree(
        "[check.bad]\nargv = [\"/bin/sh\", \"-c\", \"exit 3\"]\n\
         [check.ok]\nargv = [\"/usr/bin/true\"]\n",
    );
    let file = dir.join("a-file");
    std::fs::write(&file, "x").unwrap();
    let mut opts = RunOpts::new(&caller);
    opts.runs_root = Some(runs.clone());
    opts.lines = Some(file.clone());
    let unwritten = run_dir(&dir, CheckStage::BeforeLand, &[], &opts).unwrap_err();
    let written = run(&dir, &runs, &[], &[]).unwrap_err();
    assert_eq!(unwritten.message, written.message);
    assert_eq!(unwritten.hint.is_some(), written.hint.is_some());
    let (a, b) = (run_of(&unwritten), run_of(&written));
    let outcomes = |resp: &CheckRunResp| -> Vec<(String, CheckOutcome, Option<i32>)> {
        (resp.checks.iter())
            .map(|c| (c.name.clone(), c.outcome, c.exit_code))
            .collect()
    };
    assert_eq!(outcomes(&a), outcomes(&b));
    assert_eq!((a.failed, a.cancelled), (b.failed, b.cancelled));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "x", "untouched");
}

/// Two checks that did not pass: the hint names `check log --failed` and the run, after the
/// first one's findings when it has any; one failed check keeps the hint it had.
#[test]
fn two_failed_checks_are_hinted_to_their_logs_together() {
    // 1. Two failures, the first without findings.
    let (_tmp, dir, runs) = tree(
        "[check.a]\nargv = [\"/bin/sh\", \"-c\", \"exit 1\"]\n\
         [check.b]\nargv = [\"/bin/sh\", \"-c\", \"exit 2\"]\n\
         [check.c]\nargv = [\"/usr/bin/true\"]\n",
    );
    let error = run(&dir, &runs, &[], &[]).unwrap_err();
    let resp = run_of(&error);
    assert_eq!(
        error.hint.as_deref(),
        Some(
            format!(
                "read their logs: swarmops check log --failed --dir {dir} --run {}",
                resp.run
            )
            .as_str()
        )
    );

    // 2. Two failures, the first with a finding.
    let (_tmp, dir, runs) = tree(
        "[check.a]\nargv = [\"/bin/sh\", \"-c\", \"echo '{\\\"level\\\":\\\"error\\\",\\\"title\\\":\\\"t\\\"}' > \\\"$SWARMOPS_CHECK_REPORT\\\"; exit 1\"]\n\
         [check.b]\nargv = [\"/bin/sh\", \"-c\", \"exit 2\"]\n",
    );
    let error = run(&dir, &runs, &[], &[]).unwrap_err();
    let resp = run_of(&error);
    assert_eq!(resp.checks[0].findings_total, 1, "{resp:?}");
    assert_eq!(
        error.hint.as_deref(),
        Some(
            format!(
                "read its findings in details.run.checks[0].findings, then the logs: swarmops \
                 check log --failed --dir {dir} --run {}",
                resp.run
            )
            .as_str()
        )
    );

    // 3. One failure: the hint names its log, as before.
    let (_tmp, dir, runs) = tree(
        "[check.a]\nargv = [\"/usr/bin/true\"]\n\
         [check.b]\nargv = [\"/bin/sh\", \"-c\", \"exit 2\"]\n",
    );
    let error = run(&dir, &runs, &[], &[]).unwrap_err();
    let resp = run_of(&error);
    assert_eq!(
        error.hint.as_deref(),
        Some(
            format!(
                "read its log: swarmops check log b --dir {dir} --run {}",
                resp.run
            )
            .as_str()
        )
    );
}
