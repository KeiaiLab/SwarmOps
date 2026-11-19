//! The run line: what it holds for each outcome in both forms, its bounds, where it goes, its
//! rotation, and a failed append.

use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt as _;

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_api::{CheckOutcome, CheckRunOut, CheckStage};

use super::*;

/// A run id with fixed bytes.
const RUN: &str = "0193a5b0-0000-7000-8000-000000000001";

/// A temp dir and its path.
fn scratch() -> (tempfile::TempDir, Utf8PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = Utf8PathBuf::from_path_buf(std::fs::canonicalize(tmp.path()).unwrap()).unwrap();
    (tmp, dir)
}

/// The run of the directory form, kept in `dir`.
fn dir_form(dir: &Utf8Path) -> Of<'_> {
    Of {
        run: RUN,
        stage: CheckStage::BeforeLand,
        vouched: true,
        tree: None,
        workspace: None,
        run_dir: dir,
    }
}

/// The lines of the current file of `dir`.
fn written(dir: &Utf8Path) -> Vec<CheckRunLine> {
    let text = std::fs::read_to_string(dir.join(RUN_LINES_FILE)).unwrap();
    (text.lines())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// A line holds its check's name, stage, wait, duration and outcome, with the run, the attempt,
/// the start and the run's directory, for each outcome that leaves one; the tree and the
/// workspace in the workspace form only; the exit code, the findings and the peak memory when
/// there are any.
#[test]
fn a_line_holds_its_check_s_run_in_both_forms() {
    // 1. The directory form, each outcome: vouched, no tree, no workspace.
    let dir = Utf8Path::new("/runs/key/0193a5b0-0000-7000-8000-000000000001");
    let outcomes = [
        CheckOutcome::Passed,
        CheckOutcome::Failed,
        CheckOutcome::TimedOut,
        CheckOutcome::Cancelled,
        CheckOutcome::NotStarted,
    ];
    for outcome in outcomes {
        let mut out = CheckRunOut::new("unit", outcome, "ab".repeat(32), 900);
        out.duration_ms = 40;
        let times = Times {
            at_ms: 1_700_000_000_000,
            wait_ms: 7,
            peak_rss_kib: None,
        };
        let line = line_of(&dir_form(dir), &out, &times);
        let mut want = CheckRunLine::new(
            1_700_000_000_000,
            RUN,
            "unit",
            CheckStage::BeforeLand,
            outcome,
        );
        want.vouched = true;
        want.definition_digest = "ab".repeat(32);
        want.wait_ms = 7;
        want.duration_ms = 40;
        want.run_dir = Some(dir.to_string());
        assert_eq!(line, want, "{outcome:?}");
        assert!(valid(&line), "{line:?}");
    }

    // 2. The workspace form, a failed check with findings and a peak: its tree and workspace.
    let tree = "cd".repeat(32);
    let of = Of {
        run: RUN,
        stage: CheckStage::Manual,
        vouched: false,
        tree: Some(&tree),
        workspace: Some("agent-1"),
        run_dir: dir,
    };
    let mut out = CheckRunOut::new("lint", CheckOutcome::Failed, "ef".repeat(32), 60);
    out.exit_code = Some(3);
    out.findings_total = 5;
    out.duration_ms = 1_200;
    let times = Times {
        at_ms: 2,
        wait_ms: 0,
        peak_rss_kib: Some(4_096),
    };
    let line = line_of(&of, &out, &times);
    assert_eq!(
        (
            line.tree.as_deref(),
            line.workspace.as_deref(),
            line.vouched
        ),
        (Some(tree.as_str()), Some("agent-1"), false)
    );
    assert_eq!(
        (line.stage, line.exit_code, line.findings, line.peak_rss_kib),
        (CheckStage::Manual, Some(3), 5, Some(4_096))
    );
    assert_eq!((line.attempt, line.labels.len()), (1, 0), "{line:?}");
    assert!(valid(&line), "{line:?}");
}

/// The longest line a run writes is under the 4 KiB one append may hold.
#[test]
fn the_longest_line_fits() {
    let dir = format!("/{}", "d".repeat(MAX_RUN_LINE_DIR_BYTES - 1));
    let tree = "f".repeat(64);
    let workspace = "w".repeat(64);
    let of = Of {
        run: RUN,
        stage: CheckStage::BeforeLand,
        vouched: false,
        tree: Some(&tree),
        workspace: Some(&workspace),
        run_dir: Utf8Path::new(&dir),
    };
    let mut out = CheckRunOut::new(
        "c".repeat(crate::check::MAX_CHECK_NAME_BYTES),
        CheckOutcome::NotStarted,
        "e".repeat(64),
        86_400,
    );
    out.exit_code = Some(i32::MIN);
    out.findings_total = u64::MAX;
    out.duration_ms = u64::MAX;
    let times = Times {
        at_ms: u64::MAX,
        wait_ms: u64::MAX,
        peak_rss_kib: Some(u64::MAX),
    };
    let line = line_of(&of, &out, &times);
    assert_eq!(
        line.run_dir.as_deref(),
        Some(dir.as_str()),
        "the directory is kept"
    );
    assert!(valid(&line), "{line:?}");
    let bytes = serde_json::to_vec(&line).unwrap().len() + 1;
    assert!(bytes < MAX_RUN_LINE_BYTES, "{bytes} bytes");
    assert_eq!(MAX_RUN_LINE_BYTES, 4 << 10, "one append's bound");
}

/// A run directory past 1,024 bytes is left out of the line, which is written all the same.
#[test]
fn a_run_directory_past_its_bound_is_left_out() {
    let (_tmp, lines) = scratch();
    let long = format!("/{}", "d".repeat(MAX_RUN_LINE_DIR_BYTES));
    let out = CheckRunOut::new("unit", CheckOutcome::Passed, "", 900);
    let times = Times {
        at_ms: 1,
        wait_ms: 0,
        peak_rss_kib: None,
    };
    let line = line_of(&dir_form(Utf8Path::new(&long)), &out, &times);
    assert_eq!(line.run_dir, None, "{} bytes", long.len());
    append_within(&lines, &line, MAX_RUN_LINES_BYTES).unwrap();
    assert_eq!(written(&lines), [line]);
}

/// Three lines past a bound of two: the older file holds them, and the next line starts a new
/// file.
#[test]
fn run_lines_rotate_past_the_bound() {
    let (_tmp, dir) = scratch();
    let line = CheckRunLine::new(1, RUN, "unit", CheckStage::BeforeLand, CheckOutcome::Passed);
    let line_len = serde_json::to_vec(&line).unwrap().len() as u64 + 1;
    for _ in 0..3 {
        append_within(&dir, &line, line_len * 2).unwrap();
    }
    assert!(!dir.join(RUN_LINES_FILE).exists(), "rotated");
    let older = std::fs::read_to_string(dir.join(RUN_LINES_OLDER_FILE)).unwrap();
    assert_eq!(older.lines().count(), 3, "{older}");
    append_within(&dir, &line, line_len * 2).unwrap();
    assert_eq!(written(&dir), [line], "a new file");
    assert_eq!(
        (RUN_LINES_FILE, RUN_LINES_OLDER_FILE),
        ("run-lines.jsonl", "run-lines.1.jsonl")
    );
    assert_eq!(MAX_RUN_LINES_BYTES, 64 << 20, "the op records' bound");
}

/// A directory that cannot be written gives one warning, the first time; later lines of the
/// run are not tried. A line past 4 KiB is refused the same way.
#[test]
fn a_failed_append_warns_once() {
    // 1. A directory that is a file.
    let (_tmp, dir) = scratch();
    let file = dir.join("a-file");
    std::fs::write(&file, "x").unwrap();
    let line = CheckRunLine::new(1, RUN, "unit", CheckStage::BeforeLand, CheckOutcome::Passed);
    let mut sink = Sink::resolve(Some(&file), &dir);
    let warning = sink.append(&line).expect("the first failure warns");
    assert!(warning.contains(file.as_str()), "{warning}");
    assert!(!warning.contains('\n'), "{warning}");
    assert_eq!(sink.append(&line), None, "said once");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "x", "untouched");

    // 2. A line too long for one append.
    let mut long = line.clone();
    long.labels = vec!["l".repeat(MAX_RUN_LINE_BYTES)];
    let mut sink = Sink::resolve(None, &dir);
    assert!(sink.append(&long).is_some(), "past 4 KiB");
    assert!(!dir.join(RUN_LINES_FILE).exists(), "nothing written");

    // 3. One that works says nothing, each time.
    let mut sink = Sink::resolve(None, &dir);
    assert_eq!((sink.append(&line), sink.append(&line)), (None, None));
    assert_eq!(written(&dir).len(), 2);
}

/// Where lines go: the directory the flag names, made absolute; else the variable's, which must
/// be absolute; else the form's own place. A named directory that is not there is made, its
/// owner's alone; one that is there keeps its mode.
#[test]
fn lines_go_where_the_flag_or_the_variable_says_or_beside_the_runs() {
    // 1. The flag, relative to the working directory, wins over the variable.
    let (_tmp, dir) = scratch();
    let var = dir.join("from-var");
    let flag = named(
        Some(Utf8Path::new("rel/lines")),
        Some(OsStr::new(var.as_str())),
    )
    .unwrap();
    let cwd = Utf8PathBuf::from_path_buf(std::env::current_dir().unwrap()).unwrap();
    assert_eq!(flag, Some(cwd.join("rel/lines")));

    // 2. The variable alone; relative, it is refused with its reason and a hint.
    let from_var = named(None, Some(OsStr::new(var.as_str()))).unwrap();
    assert_eq!(from_var.as_deref(), Some(var.as_path()));
    let refused = named(None, Some(OsStr::new("rel"))).unwrap_err();
    assert_eq!(refused.code, "E_INVALID_ARG", "{refused:?}");
    assert_eq!(refused.details["reason"], "run_lines_dir", "{refused:?}");
    assert!(refused.message.contains(RUN_LINES_DIR_ENV), "{refused:?}");
    assert!(refused.hint.is_some(), "{refused:?}");
    assert_eq!(RUN_LINES_DIR_ENV, "SWARMOPS_CHECK_RUN_LINES_DIR");

    // 3. Neither: none named, and the sink is the form's own place.
    assert_eq!(named(None, None).unwrap(), None);
    let own = dir.join("own");
    let sink = Sink::resolve(None, &own);
    assert_eq!((sink.dir(), sink.is_named()), (own.as_path(), false));
    let sink = Sink::resolve(from_var.as_deref(), &own);
    assert_eq!((sink.dir(), sink.is_named()), (var.as_path(), true));

    // 4. A named directory that is not there is made, one level, mode 0700; then used as it is.
    let line = CheckRunLine::new(1, RUN, "unit", CheckStage::BeforeLand, CheckOutcome::Passed);
    let mut sink = Sink::resolve(Some(&var), &own);
    assert_eq!(sink.append(&line), None);
    let mode = |path: &Utf8Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&var), 0o700, "made its owner's alone");
    std::fs::set_permissions(&var, std::fs::Permissions::from_mode(0o750)).unwrap();
    assert_eq!(sink.append(&line), None);
    assert_eq!(mode(&var), 0o750, "kept as the operator set it");
    assert_eq!(written(&var).len(), 2);
    let deep = dir.join("no/such/parent");
    let mut sink = Sink::resolve(Some(&deep), &own);
    assert!(sink.append(&line).is_some(), "one level only");
    assert!(!dir.join("no").exists(), "no parent made");
}

/// What the reader refuses in a line: a run that is no UUID, a name that is no check's, a tree
/// that is not 64 hex digits, `not_run`, a run directory that is relative or too long, a control
/// character in the workspace's name.
#[test]
fn a_line_s_values_are_checked() {
    let good = || {
        let mut line =
            CheckRunLine::new(1, RUN, "unit", CheckStage::BeforeLand, CheckOutcome::Passed);
        line.tree = Some("ab".repeat(32));
        line.workspace = Some("w".to_owned());
        line.run_dir = Some("/runs/r".to_owned());
        line
    };
    assert!(valid(&good()), "the good line");
    let broken: [fn(&mut CheckRunLine); 9] = [
        |line| line.run = "run".to_owned(),
        |line| line.run = RUN.to_uppercase(),
        |line| line.check = "Unit".to_owned(),
        |line| line.check = String::new(),
        |line| line.tree = Some("ab".repeat(31)),
        |line| line.tree = Some("AB".repeat(32)),
        |line| line.outcome = CheckOutcome::NotRun,
        |line| line.run_dir = Some("runs/r".to_owned()),
        |line| line.workspace = Some("w\n".to_owned()),
    ];
    for (i, change) in broken.iter().enumerate() {
        let mut line = good();
        change(&mut line);
        assert!(!valid(&line), "case {i}: {line:?}");
    }
    let mut long = good();
    long.run_dir = Some(format!("/{}", "d".repeat(MAX_RUN_LINE_DIR_BYTES)));
    assert!(!valid(&long), "a run directory past its bound");
}
