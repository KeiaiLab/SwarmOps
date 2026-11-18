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
