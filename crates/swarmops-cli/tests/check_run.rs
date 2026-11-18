//! `check run` (M31-1, M31-2, M31-3, M30-6) through the binary: outcomes, a timed-out check's
//! process group, the environment a check gets, sections in its log, signals, stages, the
//! workspace form and where runs go.
//!
//! Every fixture check is a program with its arguments: `sh -c …` where a script is wanted, since
//! a check may run a shell, but SwarmOps adds none.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal};
use serde_json::{Value, json};
use swarmops_testkit::child::Child;

use crate::support;
use crate::ws::{AGENT, Lab, Out, run_in, write};

/// A directory `tree` below a temp dir, holding `.swarmops/checks.toml` with `checks`, and a runs
/// directory beside it.
struct Tree {
    /// The temp dir.
    tmp: tempfile::TempDir,
}

impl Tree {
    /// A tree whose check file holds `version = 1` and `checks`.
    fn new(checks: &str) -> Tree {
        let tree = Tree {
            tmp: tempfile::tempdir().unwrap(),
        };
        write(
            &tree.dir(),
            ".swarmops/checks.toml",
            &format!("version = 1\n{checks}"),
        );
        tree
    }

    /// The tree's directory.
    fn dir(&self) -> PathBuf {
        self.tmp.path().join("tree")
    }

    /// Where its runs go.
    fn runs(&self) -> PathBuf {
        self.tmp.path().join("runs")
    }

    /// The temp dir.
    fn tmp(&self) -> &Path {
        self.tmp.path()
    }

    /// `check run --dir <tree>` and `args`, its runs below [`Tree::runs`], with `env` set.
    fn run(&self, args: &[&str], env: &[(&str, &str)]) -> Out {
        let runs = self.runs();
        let mut all = vec![("SWARMOPS_CHECK_RUNS_DIR", runs.to_str().unwrap())];
        all.extend_from_slice(env);
        let dir = self.dir();
        let mut line = vec!["check", "run", "--dir", dir.to_str().unwrap()];
        line.extend_from_slice(args);
        run_in(self.tmp(), &all, &line, None)
    }
}

/// The run an `E_CHECK_FAILED` carries in `details.run`.
fn failed_run(out: &Out) -> Value {
    let mut error = out.err("E_CHECK_FAILED", 1);
    error["details"]["run"].take()
}

/// Check `name` of `run`.
fn check<'r>(run: &'r Value, name: &str) -> &'r Value {
    (run["checks"].as_array().unwrap().iter())
        .find(|c| c["name"] == name)
        .unwrap_or_else(|| panic!("no check {name} in {run}"))
}

/// The lines of the log at `path`: `(kind, text)`, the time left out.
fn log_lines(path: &str) -> Vec<(String, String)> {
    let text = fs::read_to_string(path).unwrap();
    (text.lines())
        .map(|line| {
            let rest = line.trim_start().split_once(' ').unwrap().1;
            let (kind, text) = rest.split_once(' ').unwrap();
            (kind.to_owned(), text.to_owned())
        })
        .collect()
}

/// The variables a check that ran `/usr/bin/env` printed into the log at `path`, by name.
fn printed_env(path: &str) -> Vec<(String, String)> {
    let mut vars: Vec<(String, String)> = (log_lines(path).into_iter())
        .filter(|(kind, _)| kind == "out")
        .filter_map(|(_, text)| {
            let (name, value) = text.split_once('=')?;
            Some((name.to_owned(), value.to_owned()))
        })
        .collect();
    vars.sort();
    vars
}

/// The pid a script wrote into `file` below `dir`, waiting up to 60 s for it.
fn pid_in(dir: &Path, file: &str) -> Pid {
    let path = dir.join(file);
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Ok(text) = fs::read_to_string(&path)
            && let Ok(pid) = text.trim().parse()
        {
            return Pid::from_raw(pid).unwrap();
        }
        assert!(Instant::now() < deadline, "no pid in {}", path.display());
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Whether process group `group` is gone, waiting up to 10 s for it.
fn group_gone(group: Pid) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while rustix::process::test_kill_process_group(group).is_ok() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    rustix::process::test_kill_process_group(group).is_err()
}

/// A fixture of one passing and one failing check exits with `E_CHECK_FAILED` and lists both
/// outcomes (M31-1's acceptance): both ran, in name order; the failing one with its exit code and
/// tails, the passing one without tails; both logs end with their `end` line.
#[test]
fn a_passing_and_a_failing_check_both_run_and_fail_the_run() {
    let tree = Tree::new(
        "[check.ok]\nargv = [\"true\"]\n\
         [check.bad]\nargv = [\"sh\", \"-c\", \"echo boom >&2; exit 3\"]\n",
    );
    let out = tree.run(&[], &[]);
    let run = failed_run(&out);
    let outcomes: Vec<(&str, &str)> = (run["checks"].as_array().unwrap().iter())
        .map(|c| (c["name"].as_str().unwrap(), c["outcome"].as_str().unwrap()))
        .collect();
    assert_eq!(outcomes, [("bad", "failed"), ("ok", "passed")]);
    assert_eq!(run["failed"], json!(["bad"]));
    let bad = check(&run, "bad");
    assert_eq!(
        (&bad["exit_code"], &bad["stderr_tail"]),
        (&json!(3), &json!("boom\n"))
    );
    let ok = check(&run, "ok");
    assert!(
        ok.get("stdout_tail").is_none() && ok.get("stderr_tail").is_none(),
        "{ok}"
    );
    for c in [bad, ok] {
        let lines = log_lines(c["log"].as_str().unwrap());
        assert!(lines[0].1.starts_with("start "), "{lines:?}");
        assert!(lines.last().unwrap().1.starts_with("end "), "{lines:?}");
    }
    assert_eq!(run["vouched"], true);
    assert!(run.get("tree").is_none());
}

/// A check past its timeout is `timed_out`, and its process group is gone, the sleeper it
/// started among it (M31-1's acceptance).
#[test]
fn a_timed_out_check_s_group_is_gone() {
    let tree = Tree::new(
        "[check.slow]\nargv = [\"sh\", \"-c\", \"echo $$ > group; sleep 600 & echo $! > sleeper; \
         wait\"]\ntimeout_s = 1\n",
    );
    let started = Instant::now();
    let run = failed_run(&tree.run(&[], &[]));
    assert!(started.elapsed() < Duration::from_secs(60));
    let slow = check(&run, "slow");
    assert_eq!(slow["outcome"], "timed_out");
    assert!(slow.get("exit_code").is_none(), "{slow}");
    assert!(group_gone(pid_in(&tree.dir(), "group")));
    assert!(rustix::process::test_kill_process(pid_in(&tree.dir(), "sleeper")).is_err());
}

/// A check that prints its environment shows only the pinned names, and of the caller's
/// variables only the fixed list it has set (M31-2's acceptance): `FOO` and a `SWARMOPS_` name
/// the caller set do not reach it; with `--pass FOO`, `FOO` does, and `inherited_env` says so.
#[test]
fn a_check_sees_only_the_pinned_names_and_those_listed() {
    // 1. The pinned names and the fixed list the test's environment has.
    let tree = Tree::new("[check.env]\nargv = [\"/usr/bin/env\"]\n");
    let caller = [("FOO", "bar"), ("SWARMOPS_TEST_LEAK", "1")];
    let run = tree.run(&[], &caller).ok();
    let fixed: Vec<&str> = ([
        "HOME", "LANG", "LC_ALL", "LC_CTYPE", "LOGNAME", "PATH", "TMPDIR", "TZ", "USER",
    ])
    .into_iter()
    .filter(|name| std::env::var_os(name).is_some_and(|v| !v.is_empty()))
    .collect();
    assert_eq!(run["inherited_env"], json!(fixed));
    let pinned = [
        "SWARMOPS_CHECK_ARTIFACTS",
        "SWARMOPS_CHECK_ATTEMPT",
        "SWARMOPS_CHECK_CHANGED",
        "SWARMOPS_CHECK_NAME",
        "SWARMOPS_CHECK_OUTPUTS",
        "SWARMOPS_CHECK_REPORT",
        "SWARMOPS_CHECK_RUN",
        "SWARMOPS_CHECK_STAGE",
        "SWARMOPS_CHECK_SUMMARY",
        "SWARMOPS_CHECK_TREE",
    ];
    let mut want: Vec<&str> = fixed.iter().copied().chain(pinned).collect();
    want.sort_unstable();
    let log = check(&run, "env")["log"].as_str().unwrap().to_owned();
    let vars = printed_env(&log);
    let names: Vec<&str> = vars.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, want);
    let value = |vars: &[(String, String)], name: &str| {
        vars.iter().find(|(n, _)| n == name).map(|(_, v)| v.clone())
    };
    assert_eq!(value(&vars, "SWARMOPS_CHECK_NAME").as_deref(), Some("env"));
    assert_eq!(value(&vars, "SWARMOPS_CHECK_TREE").as_deref(), Some(""));

    // 2. With --pass FOO: FOO too, and nothing else more.
    let run = tree.run(&["--pass", "FOO"], &caller).ok();
    assert!(
        run["inherited_env"]
            .as_array()
            .unwrap()
            .contains(&json!("FOO"))
    );
    let vars = printed_env(check(&run, "env")["log"].as_str().unwrap());
    assert_eq!(value(&vars, "FOO").as_deref(), Some("bar"));
    assert_eq!(value(&vars, "SWARMOPS_TEST_LEAK"), None);
    assert_eq!(vars.len(), want.len() + 1);
}

/// The one check of a `check.argv` fixture still sees its caller's variable (M31-2's
/// acceptance), with the pinned names beside it.
#[test]
fn a_check_argv_check_keeps_its_caller_s_environment() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("tree");
    write(
        &dir,
        ".swarmops/workspaces.json",
        r#"{"check": {"argv": ["/usr/bin/env"]}}"#,
    );
    let runs = tmp.path().join("runs");
    let env = [
        ("SWARMOPS_CHECK_RUNS_DIR", runs.to_str().unwrap()),
        ("FOO", "bar"),
    ];
    let line = ["check", "run", "--dir", dir.to_str().unwrap()];
    let run = run_in(tmp.path(), &env, &line, None).ok();
    assert_eq!(
        (&run["file"], &run["inherited_all"]),
        (&json!(".swarmops/workspaces.json"), &json!(true))
    );
    assert_eq!(run["inherited_env"], json!([]));
    let vars = printed_env(check(&run, "default")["log"].as_str().unwrap());
    assert!(
        vars.contains(&("FOO".to_owned(), "bar".to_owned())),
        "{vars:?}"
    );
    assert!(vars.contains(&("SWARMOPS_CHECK_NAME".to_owned(), "default".to_owned())));
}

/// The log of a fixture check holds its sections in order with durations (M31-3's acceptance):
/// a marker on stdout and one on stderr, written as `run section` lines between `start` and
/// `end`, never as output; the `end` line and the envelope give both, in order, each with its
/// start and its duration. The two streams are read apart, so the script waits between them:
/// each stream's own order is kept, not the order of lines across them.
#[test]
fn a_check_s_log_holds_its_sections_in_order() {
    let tree = Tree::new(
        "[check.sections]\nargv = [\"sh\", \"-c\", \"echo swarmops:section build; echo a; \
         sleep 0.3; echo swarmops:section test >&2; sleep 0.3; echo b; exit 0\"]\n",
    );
    let run = tree.run(&[], &[]).ok();
    let sections = check(&run, "sections");
    let lines = log_lines(sections["log"].as_str().unwrap());
    let kinds: Vec<(&str, &str)> = lines
        .iter()
        .map(|(k, t)| (k.as_str(), t.as_str()))
        .collect();
    assert!(kinds[0].1.starts_with("start "), "{kinds:?}");
    assert_eq!(
        kinds[1..kinds.len() - 1],
        [
            ("run", r#"section {"n":1,"title":"build"}"#),
            ("out", "a"),
            ("run", r#"section {"n":2,"title":"test"}"#),
            ("out", "b"),
        ]
    );
    let end: Value =
        serde_json::from_str(kinds.last().unwrap().1.strip_prefix("end ").unwrap()).unwrap();
    let given = &sections["sections"];
    let titles: Vec<&str> = (given.as_array().unwrap().iter())
        .map(|s| s["title"].as_str().unwrap())
        .collect();
    assert_eq!(titles, ["build", "test"]);
    let starts: Vec<u64> = (given.as_array().unwrap().iter())
        .map(|s| s["start_ms"].as_u64().unwrap())
        .collect();
    assert!(starts[0] < starts[1], "{given}");
    for (at, section) in given.as_array().unwrap().iter().enumerate() {
        assert!(section["duration_ms"].is_u64(), "{section}");
        assert_eq!(end["sections"][at]["title"], section["title"]);
        assert_eq!(end["sections"][at]["start_ms"], section["start_ms"]);
        assert_eq!(end["sections"][at]["duration_ms"], section["duration_ms"]);
    }
}

/// A program that does not exist does not start: `not_started`, `program`, its name in the
/// message.
#[test]
fn a_missing_program_is_not_started() {
    let tree = Tree::new("[check.gone]\nargv = [\"swarmops-no-such-program\"]\n");
    let run = failed_run(&tree.run(&[], &[]));
    let gone = check(&run, "gone");
    assert_eq!(
        (&gone["outcome"], &gone["not_started"]),
        (&json!("not_started"), &json!("program"))
    );
    assert!(
        gone["message"]
            .as_str()
            .unwrap()
            .contains("swarmops-no-such-program")
    );
}

/// SIGINT, then SIGTERM, to `swarmops check run` while its first check sleeps: that check's
/// group is killed and it is `cancelled`, the second is `not_run` with no log, the run is
/// `E_CHECK_FAILED` with `cancelled`.
#[test]
fn a_signal_stops_the_run() {
    for signal in [Signal::INT, Signal::TERM] {
        // 1. Two checks, the first a sleeper writing its group.
        let tree = Tree::new(
            "[check.a]\nargv = [\"sh\", \"-c\", \"echo $$ > group; sleep 600\"]\n\
             [check.b]\nargv = [\"true\"]\n",
        );
        let runs = tree.runs();
        let dir = tree.dir();
        let line = ["check", "run", "--dir", dir.to_str().unwrap()];
        let mut command = support::swarmops(
            &line,
            &[("SWARMOPS_CHECK_RUNS_DIR", runs.to_str().unwrap())],
        );
        command.current_dir(tree.tmp());
        let mut child = Child::spawn(&mut command);

        // 2. The signal, once the first check runs.
        let group = pid_in(&dir, "group");
        let pid = Pid::from_raw(i32::try_from(child.id()).unwrap()).unwrap();
        rustix::process::kill_process(pid, signal).unwrap();
        let (output, killed) = child.finish(Instant::now() + Duration::from_secs(60));
        assert!(!killed, "the run ended by itself");

        // 3. The answer, and the group gone.
        let out = Out {
            exit: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8(output.stdout).unwrap(),
            stderr: String::from_utf8(output.stderr).unwrap(),
        };
        let run = failed_run(&out);
        assert_eq!(run["cancelled"], true, "{out}");
        assert_eq!(check(&run, "a")["outcome"], "cancelled");
        let b = check(&run, "b");
        assert_eq!(b["outcome"], "not_run");
        assert!(b.get("log").is_none(), "{b}");
        assert!(group_gone(group), "{signal:?}");
        let error = out.envelope();
        assert_eq!(
            error["error"]["message"],
            "the run was stopped: a cancelled, 1 not run"
        );
    }
}

/// Stages (M30-6): a check of two stages runs under each; a check of one runs under that one
/// only; names select; a name not defined, a name of another stage and a stage with no check
/// are refused, and nothing runs; an unknown stage in the file is refused at its line.
#[test]
fn a_check_of_two_stages_runs_in_both() {
    // 1. docs of two stages, lint of before-land.
    let tree = Tree::new(
        "[check.docs]\nargv = [\"true\"]\nstages = [\"before-land\", \"after-land\"]\n\
         [check.lint]\nargv = [\"true\"]\n",
    );
    let names = |run: &Value| -> Vec<String> {
        (run["checks"].as_array().unwrap().iter())
            .map(|c| c["name"].as_str().unwrap().to_owned())
            .collect()
    };
    let before = tree.run(&[], &[]).ok();
    assert_eq!(names(&before), ["docs", "lint"]);
    assert_eq!(before["stage"], "before-land");
    let after = tree.run(&["--stage", "after-land"], &[]).ok();
    assert_eq!(
        (names(&after), &after["stage"]),
        (vec!["docs".to_owned()], &json!("after-land"))
    );
    assert_eq!(names(&tree.run(&["lint"], &[]).ok()), ["lint"]);

    // 2. Refusals, nothing run.
    let unknown = tree.run(&["nope"], &[]).err("E_INVALID_ARG", 2);
    assert_eq!(unknown["details"]["reason"], "check");
    let other = tree
        .run(&["--stage", "after-land", "lint"], &[])
        .err("E_INVALID_ARG", 2);
    assert_eq!(
        (&other["details"]["reason"], &other["details"]["stages"]),
        (&json!("stage"), &json!(["before-land"]))
    );
    let none = tree
        .run(&["--stage", "deploy"], &[])
        .err("E_INVALID_ARG", 2);
    assert_eq!(none["details"]["reason"], "no_checks");

    // 3. An unknown stage in the file: refused at the item's line.
    let refused = include_str!("fixtures/check/refused-stage.toml");
    write(&tree.dir(), ".swarmops/checks.toml", refused);
    let error = tree.run(&[], &[]).err("E_CHECK_FILE", 1);
    assert_eq!(
        (&error["details"]["line"], &error["details"]["key"]),
        (&json!(8), &json!("check.unit.stages"))
    );
}

/// The directory form writes its runs below `SWARMOPS_CHECK_RUNS_DIR` only, else below the
/// state directory of the environment it is given; never into the tree, nor into a home or state
/// directory it is not given.
#[test]
fn runs_go_below_the_runs_directory_or_the_state_directory() {
    let tree = Tree::new("[check.ok]\nargv = [\"true\"]\n");
    let home = tree.tmp().join("home");
    let state = tree.tmp().join("state");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&state).unwrap();
    let host = [
        ("HOME", home.to_str().unwrap()),
        ("XDG_STATE_HOME", state.to_str().unwrap()),
    ];

    // 1. The variable: the run there, the home and state directories left empty.
    let run = tree.run(&[], &host).ok();
    let dir = PathBuf::from(run["dir"].as_str().unwrap());
    assert!(dir.starts_with(tree.runs()), "{}", dir.display());
    assert!(dir.join("run.json").is_file());
    assert_eq!(fs::read_dir(&home).unwrap().count(), 0);
    assert_eq!(fs::read_dir(&state).unwrap().count(), 0);
    let held: Vec<_> = fs::read_dir(tree.dir())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(held, [".swarmops"], "nothing written into the tree");

    // 2. The variable empty: the state directory of the environment given.
    let mut unset = host.to_vec();
    unset.push(("SWARMOPS_CHECK_RUNS_DIR", ""));
    let dir = tree.dir();
    let line = ["check", "run", "--dir", dir.to_str().unwrap()];
    let run = run_in(tree.tmp(), &unset, &line, None).ok();
    let kept = PathBuf::from(run["dir"].as_str().unwrap());
    let roots = [
        state.join("swarmops/check-runs"),
        home.join("Library/Application Support/swarmops/check-runs"),
    ];
    assert!(
        roots.iter().any(|root| kept.starts_with(root)),
        "{}",
        kept.display()
    );
}

/// What a check that runs the binary is passed (`--pass`), so that it stays isolated as the
/// test's own runs are: no login, no curl, no automatic update, the shared memo keys.
const ISOLATED: [&str; 8] = [
    "--pass",
    "SWARMOPS_AUTO_UPDATE",
    "--pass",
    "SWARMOPS_AUTH_FILE",
    "--pass",
    "SWARMOPS_CURL",
    "--pass",
    "SWARMOPS_MEMO_KEY_DIR",
];

/// A store whose main holds a check file of four checks (`env`; `writes`, which writes the store,
/// and `locked`, which commits in its workspace, both `manual`; `docs`, in `docs/`, `deploy`), and
/// a workspace `w1` of the whole tree; the workspace's directory.
fn checked_lab() -> (Lab, PathBuf) {
    let lab = Lab::new();
    let bin = env!("CARGO_BIN_EXE_swarmops");
    let store = lab.path().to_owned();
    let main = crate::ws::main_of(&lab);
    let quoted = |args: &[&str]| -> String {
        let items: Vec<String> = args.iter().map(|a| format!("{a:?}")).collect();
        format!("[{}]", items.join(", "))
    };
    let writes = quoted(&[
        bin,
        "--repo",
        &store,
        "--as",
        AGENT,
        "ref",
        "set",
        "release/probe",
        &main,
        "--create",
    ]);
    let locked = quoted(&[
        bin,
        "--as",
        AGENT,
        "ws",
        "commit",
        "-m",
        "x",
        "--status-after",
        "none",
    ]);
    let checks = format!(
        "version = 1\n[check.env]\nargv = [\"/usr/bin/env\"]\n\
         [check.writes]\nargv = {writes}\nstages = [\"manual\"]\n\
         [check.locked]\nargv = {locked}\nstages = [\"manual\"]\n\
         [check.docs]\nargv = [\"true\"]\ndir = \"docs\"\nstages = [\"deploy\"]\n"
    );
    lab.publish(".swarmops/checks.toml", &checks);
    let ws = lab.workspace("w1");
    (lab, ws)
}

/// The workspace form (M31-1): a clean workspace's checks run on the tree its files stand for,
/// read from the store; `SWARMOPS_WS` and the tree reach the check; an edit is `E_WS_DIRTY` and
/// starts nothing; a check may write the store (no writer lock is held) but not run a `ws`
/// mutation of its workspace (its lock is held); a sparse workspace without a check's directory
/// gives `not_started`.
#[test]
fn the_workspace_form_runs_on_the_tree_and_holds_the_workspace() {
    // 1. The checks on main, and a workspace of the whole tree.
    let (lab, ws) = checked_lab();
    let pass = ISOLATED;

    // 2. Clean: the env check sees the workspace and the tree, which the result names.
    let run = lab.so(&["check", "run", "--ws", "w1"]).ok();
    assert_eq!(
        (&run["vouched"], &run["workspace"]),
        (&json!(false), &json!("w1"))
    );
    let shown = lab.so(&["show", run["changeset"].as_str().unwrap()]).ok();
    assert_eq!(run["tree"], shown["root"]);
    assert!(
        PathBuf::from(run["dir"].as_str().unwrap()).starts_with(ws.join(".swarmws/check-runs"))
    );
    let vars = printed_env(check(&run, "env")["log"].as_str().unwrap());
    let value = |name: &str| vars.iter().find(|(n, _)| n == name).map(|(_, v)| v.clone());
    assert_eq!(value("SWARMOPS_WS").as_deref(), Some("w1"));
    assert_eq!(
        value("SWARMOPS_CHECK_TREE"),
        run["tree"].as_str().map(str::to_owned)
    );

    // 3. A check writes the store: the run holds no writer lock.
    let mut line = vec!["check", "run", "--ws", "w1", "--stage", "manual", "writes"];
    line.extend(&pass);
    let wrote = lab.so(&line).ok();
    assert_eq!(check(&wrote, "writes")["outcome"], "passed", "{wrote}");
    let refs = lab.so(&["ref", "list"]).ok().to_string();
    assert!(refs.contains("release/probe"), "{refs}");

    // 4. A check runs a mutation of its workspace: the lock is held.
    let mut line = vec!["check", "run", "--ws", "w1", "--stage", "manual", "locked"];
    line.extend(&pass);
    let run = failed_run(&lab.so(&line));
    let locked = check(&run, "locked");
    assert!(
        locked["stdout_tail"]
            .as_str()
            .unwrap()
            .contains("E_WS_LOCKED"),
        "{locked}"
    );

    // 5. An edit: E_WS_DIRTY, nothing run.
    let before = fs::read_dir(ws.join(".swarmws/check-runs"))
        .unwrap()
        .count();
    write(&ws, "README.md", "# edited\n");
    let dirty = lab.so(&["check", "run", "--ws", "w1"]).err("E_WS_DIRTY", 1);
    assert_eq!(dirty["details"]["paths"], json!(["README.md"]));
    assert_eq!(
        fs::read_dir(ws.join(".swarmws/check-runs"))
            .unwrap()
            .count(),
        before
    );

    // 6. A sparse workspace without docs/: its check is not started.
    let sparse = lab.beside("w2");
    let made = lab.so(&[
        "ws",
        "create",
        "w2",
        "--path",
        sparse.to_str().unwrap(),
        "--prefix",
        "src",
    ]);
    made.ok();
    let run = failed_run(&lab.so(&["check", "run", "--ws", "w2", "--stage", "deploy"]));
    assert_eq!(check(&run, "docs")["not_started"], "dir");
}
