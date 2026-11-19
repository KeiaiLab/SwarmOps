//! `check log`'s reader: which run it reads, how many lines, and how a log ends.

use std::ffi::OsString;
use std::io::Write as _;

use camino::Utf8PathBuf;
use swarmops_api::{CheckLogEnd, CheckLogStream, CheckOutcome, CheckStage};

use super::*;
use crate::check::run::{RunOpts, run_dir};
use swarmops_api::MAX_FAILED_LOGS;

/// A directory with `checks`, its runs below a root beside it: the directory, the runs of it.
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
    let root = base.join("runs");
    let runs = dir_runs(&dir, Some(&root)).unwrap();
    (tmp, dir, runs)
}

/// No variable of the caller's.
fn nothing(_: &str) -> Option<OsString> {
    None
}

/// Runs the checks of `dir`, its runs below the root above `runs`; the run's id.
fn ran(dir: &Utf8Path, runs: &Utf8Path) -> String {
    let mut opts = RunOpts::new(&nothing);
    opts.runs_root = runs.parent().map(Utf8Path::to_owned);
    let resp = match run_dir(dir, CheckStage::BeforeLand, &[], &opts) {
        Ok(resp) => resp,
        Err(e) => serde_json::from_value(e.details["run"].clone()).unwrap(),
    };
    resp.run
}

/// A finished check's log: every line, its start and end, its outcome and duration, its
/// streams; the newest run unless one is named; `--tail` the last lines.
#[test]
fn a_finished_log_reads_whole_with_its_end() {
    let (_tmp, dir, runs) =
        tree("[check.a]\nargv = [\"/bin/sh\", \"-c\", \"echo one; echo two >&2; exit 4\"]\n");
    let first = ran(&dir, &runs);
    let second = ran(&dir, &runs);
    let (_, resp) = Follower::start(&runs, "--dir .", "a", None, 1_000, false).unwrap();
    assert_eq!(resp.run, second);
    assert!(resp.ended);
    assert_eq!(
        (resp.ended_by, resp.outcome),
        (Some(CheckLogEnd::End), Some(CheckOutcome::Failed))
    );
    assert!(resp.duration_ms.is_some());
    let streams: Vec<CheckLogStream> = resp.lines.iter().map(|l| l.stream).collect();
    assert_eq!(streams[0], CheckLogStream::Run);
    assert!(streams.contains(&CheckLogStream::Out) && streams.contains(&CheckLogStream::Err));
    assert_eq!((resp.first_line, resp.total_lines), (1, 4));

    let (_, named) = Follower::start(&runs, "--dir .", "a", Some(&first), 2, false).unwrap();
    assert_eq!(named.run, first);
    assert_eq!((named.first_line, named.lines.len()), (3, 2));
    assert_eq!(named.lines[1].n, 4);
}

/// No run of the check, a run not there, and a tail out of its bounds are refused.
#[test]
fn a_run_that_is_not_there_is_refused() {
    let (_tmp, dir, runs) = tree("[check.a]\nargv = [\"/usr/bin/true\"]\n");
    let none = Follower::start(&runs, "--dir .", "a", None, 10, false).unwrap_err();
    assert_eq!(
        (none.code.as_str(), &none.details["reason"]),
        ("E_INVALID_ARG", &"run".into())
    );
    ran(&dir, &runs);
    let other = Follower::start(&runs, "--dir .", "b", None, 10, false).unwrap_err();
    assert_eq!(other.details["check"], "b");
    let gone = Follower::start(
        &runs,
        "--dir .",
        "a",
        Some("0193f1e2-0000-7000-8000-000000000000"),
        10,
        false,
    )
    .unwrap_err();
    assert_eq!(gone.details["reason"], "run");
    for tail in [0, MAX_CHECK_LOG_TAIL + 1] {
        assert_eq!(
            Follower::start(&runs, "--dir .", "a", None, tail, false)
                .unwrap_err()
                .details["reason"],
            "tail"
        );
    }
}

/// A log whose runner is gone, its lock free and no `end` line, ends as `runner_gone`; a check
/// the run never started ends at once as `not_run`, with no log.
#[test]
fn a_log_ends_by_its_runner_going_or_its_record() {
    // 1. A run whose log lost its end line: as a killed runner leaves it.
    let (_tmp, dir, runs) = tree("[check.a]\nargv = [\"/usr/bin/true\"]\n");
    let id = ran(&dir, &runs);
    let log = runs.join(&id).join("logs/a.log");
    let text = std::fs::read_to_string(&log).unwrap();
    let cut: Vec<&str> = text.lines().filter(|l| !l.contains(" run end ")).collect();
    std::fs::write(&log, format!("{}\n", cut.join("\n"))).unwrap();
    let (_, resp) = Follower::start(&runs, "--dir .", "a", None, 10, false).unwrap();
    assert_eq!(
        (resp.ended, resp.ended_by, resp.outcome),
        (true, Some(CheckLogEnd::RunnerGone), None)
    );

    // 2. A check its run marked not_run: no log, ended at once.
    let record_path = runs.join(&id).join(RUN_FILE);
    let mut record: Value = serde_json::from_slice(&std::fs::read(&record_path).unwrap()).unwrap();
    record["result"]["checks"][0]["name"] = "b".into();
    record["result"]["checks"][0]["outcome"] = "not_run".into();
    std::fs::write(&record_path, serde_json::to_vec(&record).unwrap()).unwrap();
    let (_, resp) = Follower::start(&runs, "--dir .", "b", None, 10, false).unwrap();
    assert_eq!(
        (resp.ended_by, resp.outcome, &resp.log),
        (Some(CheckLogEnd::End), Some(CheckOutcome::NotRun), &None)
    );
}

/// A follower gives the lines written since its last look, whole lines only, and the end with
/// the last of them; a look that finds nothing gives nothing.
#[test]
fn a_follower_gives_what_was_written_since() {
    // 1. A run in progress: its record and lock, a log of two lines and half a third.
    let (_tmp, _dir, runs) = tree("[check.a]\nargv = [\"/usr/bin/true\"]\n");
    let run = runs.join("0193f1e2-0000-7000-8000-000000000001");
    std::fs::create_dir_all(run.join("logs")).unwrap();
    let mut result = CheckRunResp::new(
        true,
        "0193f1e2-0000-7000-8000-000000000001",
        CheckStage::BeforeLand,
        ".swarmops/checks.toml",
        run.as_str(),
    );
    result.checks = vec![swarmops_api::CheckRunOut::new(
        "a",
        CheckOutcome::NotRun,
        "ab",
        9,
    )];
    let record = serde_json::json!({"format": 1, "state": "running", "result": result});
    std::fs::write(run.join(RUN_FILE), record.to_string()).unwrap();
    let lock = std::fs::File::create(run.join(RUN_LOCK)).unwrap();
    lock.lock().unwrap();
    let log_path = run.join("logs/a.log");
    let mut log = std::fs::File::create(&log_path).unwrap();
    log.write_all(b"    0.001 run start {}\n    0.002 out one\n    0.003 out tw")
        .unwrap();

    // 2. Started: the whole lines; a look then: nothing new.
    let (mut follower, first) = Follower::start(&runs, "--dir .", "a", None, 1_000, true).unwrap();
    assert_eq!((first.lines.len(), first.ended), (2, false));
    assert!(follower.look().unwrap().is_none());

    // 3. The third line ends, a fourth comes: both, in order.
    log.write_all(b"o\n    0.004 err three\n").unwrap();
    let next = follower.look().unwrap().unwrap();
    let texts: Vec<&str> = next.lines.iter().map(|l| l.text.as_str()).collect();
    assert_eq!(
        (texts, next.first_line, next.ended),
        (vec!["two", "three"], 3, false)
    );

    // 4. The end line: the last look, ended.
    log.write_all(b"    0.005 run end {\"outcome\":\"passed\",\"duration_ms\":5}\n")
        .unwrap();
    let last = follower.look().unwrap().unwrap();
    assert_eq!(
        (last.ended, last.outcome, last.duration_ms),
        (true, Some(CheckOutcome::Passed), Some(5))
    );
    assert!(follower.done());
    drop(lock);
}

/// A log is read up to [`READ_LIMIT`] bytes and no further, as a stream of which only the lines
/// given are kept: a file longer than any run writes (a check may write its own log's path)
/// counts the lines within the limit, and `--tail` gives the last of those.
#[test]
fn a_log_is_read_no_further_than_its_limit() {
    let (_tmp, dir, runs) = tree("[check.a]\nargv = [\"/usr/bin/true\"]\n");
    let id = ran(&dir, &runs);
    let log = runs.join(&id).join("logs/a.log");
    let line = format!("    0.001 out {}\n", "x".repeat(1_024 - 15));
    assert_eq!(line.len(), 1_024);
    let within = usize::try_from(READ_LIMIT / 1_024).unwrap();
    let mut file = std::fs::File::create(&log).unwrap();
    let block = line.repeat(1_024);
    for _ in 0..=(within / 1_024) {
        file.write_all(block.as_bytes()).unwrap();
    }
    drop(file);
    let (_, resp) = Follower::start(&runs, "--dir .", "a", None, 5, false).unwrap();
    assert_eq!(resp.total_lines, u64::try_from(within).unwrap());
    assert_eq!(resp.lines.len(), 5);
    assert_eq!(resp.lines[4].n, resp.total_lines);
}

/// A log that is not a regular file, as a check could put at its own log's path, is `E_IO` at
/// once: a link (to `/dev/zero`, which would never end) is not followed, and a FIFO does not
/// block the open.
#[test]
fn a_log_that_is_not_a_regular_file_is_refused() {
    let (_tmp, dir, runs) = tree("[check.a]\nargv = [\"/usr/bin/true\"]\n");
    let id = ran(&dir, &runs);
    let log = runs.join(&id).join("logs/a.log");
    std::fs::remove_file(&log).unwrap();
    std::os::unix::fs::symlink("/dev/zero", &log).unwrap();
    let linked = Follower::start(&runs, "--dir .", "a", None, 5, false).unwrap_err();
    assert_eq!(linked.code, "E_IO", "{linked:?}");
    std::fs::remove_file(&log).unwrap();
    let made = std::process::Command::new("mkfifo")
        .arg(&log)
        .status()
        .unwrap();
    assert!(made.success());
    let fifo = Follower::start(&runs, "--dir .", "a", None, 5, false).unwrap_err();
    assert_eq!(fifo.code, "E_IO", "{fifo:?}");
}

/// The run of `dir` with its result, whatever its checks did.
fn ran_whole(dir: &Utf8Path, runs: &Utf8Path) -> CheckRunResp {
    let mut opts = RunOpts::new(&nothing);
    opts.runs_root = runs.parent().map(Utf8Path::to_owned);
    match run_dir(dir, CheckStage::BeforeLand, &[], &opts) {
        Ok(resp) => resp,
        Err(e) => serde_json::from_value(e.details["run"].clone()).unwrap(),
    }
}

/// `count` checks `c01`, `c02`, … that each print their name and exit 1, and `ok`, which passes.
fn failing(count: usize) -> String {
    let ok = "[check.ok]\nargv = [\"/bin/sh\", \"-c\", \"echo fine\"]\n".to_owned();
    let failed = (1..=count).map(|i| {
        format!(
            "[check.c{i:02}]\nargv = [\"/bin/sh\", \"-c\", \"echo c{i:02} one; echo c{i:02} two; \
             exit 1\"]\n"
        )
    });
    std::iter::once(ok).chain(failed).collect()
}

/// One failed check: its log alone, as `check log NAME` gives it, with its name in `failed`
/// and no `more`; `tail` bounds its lines.
#[test]
fn the_failed_logs_of_one_failure_are_that_log_alone() {
    let (_tmp, dir, runs) = tree(&failing(1));
    let id = ran(&dir, &runs);
    let failed = failed_logs(&runs, "--dir .", None, 1_000).unwrap();
    let (_, mut alone) = Follower::start(&runs, "--dir .", "c01", Some(&id), 1_000, false).unwrap();
    assert_eq!(failed.failed, ["c01"]);
    assert!(
        failed.more.is_empty() && failed.logs_omitted == 0,
        "{failed:?}"
    );
    alone.failed = vec!["c01".to_owned()];
    assert_eq!(failed, alone, "the log `check log c01` gives");
    assert!(failed.lines.iter().all(|line| !line.text.contains("fine")));

    // The run named, and a tail of one line.
    let last = failed_logs(&runs, "--dir .", Some(&id), 1).unwrap();
    assert_eq!((last.run.as_str(), last.lines.len()), (id.as_str(), 1));
    assert_eq!(last.first_line, last.total_lines);
}

/// Several failures: the first at the top, the others in `more`, in run order, each bounded by
/// `tail`; the newest run unless one is named.
#[test]
fn the_failed_logs_of_several_come_in_run_order() {
    let (_tmp, dir, runs) = tree(&failing(3));
    let first = ran(&dir, &runs);
    let second = ran(&dir, &runs);
    let failed = failed_logs(&runs, "--dir .", None, 2).unwrap();
    assert_eq!(failed.run, second, "the newest run");
    assert_eq!(failed.failed, ["c01", "c02", "c03"]);
    assert_eq!(failed.check, "c01");
    let more: Vec<&str> = failed.more.iter().map(|log| log.check.as_str()).collect();
    assert_eq!(more, ["c02", "c03"]);
    assert_eq!(failed.logs_omitted, 0);
    assert_eq!(failed.lines.len(), 2, "tail 2");
    for log in &failed.more {
        assert_eq!((log.lines.len(), log.run.as_str()), (2, second.as_str()));
        assert_eq!(log.outcome, Some(CheckOutcome::Failed));
        let (_, alone) =
            Follower::start(&runs, "--dir .", &log.check, Some(&second), 2, false).unwrap();
        assert_eq!(*log, alone.into(), "as `check log` gives it");
    }
    assert_eq!(
        failed_logs(&runs, "--dir .", Some(&first), 2).unwrap().run,
        first
    );
}

/// At most 20 logs, and 100,000 lines over all of them: the rest are named and counted.
#[test]
fn the_failed_logs_are_bounded_and_the_rest_counted() {
    // 1. 21 failures: 20 logs, one omitted, all 21 named.
    let (_tmp, dir, runs) = tree(&failing(21));
    ran(&dir, &runs);
    let failed = failed_logs(&runs, "--dir .", None, 5).unwrap();
    assert_eq!(failed.failed.len(), 21);
    assert_eq!(
        (failed.more.len() + 1, failed.logs_omitted),
        (MAX_FAILED_LOGS, 1)
    );
    assert_eq!(failed.more.last().unwrap().check, "c20");
    assert_eq!(MAX_FAILED_LOGS, 20);

    // 2. The lines bound cuts the count: a tail of 100,000 is one log, of 50,000 two, of
    //    30,000 three.
    for (tail, logs) in [
        (MAX_CHECK_LOG_TAIL, 1),
        (50_000, 2),
        (30_000, 3),
        (5_001, 19),
    ] {
        let failed = failed_logs(&runs, "--dir .", None, tail).unwrap();
        assert_eq!(failed.more.len() + 1, logs, "tail {tail}");
        assert_eq!(failed.logs_omitted, 21 - logs as u64, "tail {tail}");
        assert_eq!(failed.failed.len(), 21, "tail {tail}");
    }
    for tail in [0, MAX_CHECK_LOG_TAIL + 1] {
        let refused = failed_logs(&runs, "--dir .", None, tail).unwrap_err();
        assert_eq!(refused.details["reason"], "tail", "{refused:?}");
    }
}

/// No failed check, a running run and no run at all are refused, each with what to do.
#[test]
fn no_failed_check_is_refused() {
    // 1. No run yet, and a run that is not there.
    let (_tmp, dir, runs) = tree("[check.ok]\nargv = [\"/usr/bin/true\"]\n");
    let none = failed_logs(&runs, "--dir .", None, 10).unwrap_err();
    assert_eq!(none.details["reason"], "run", "{none:?}");
    assert!(none.hint.unwrap().contains("check run --dir ."));
    let gone = "0193f1e2-0000-7000-8000-000000000000";
    let missing = failed_logs(&runs, "--dir .", Some(gone), 10).unwrap_err();
    assert_eq!(
        (&missing.details["reason"], &missing.details["run"]),
        (&"run".into(), &gone.into())
    );
    assert_eq!(
        failed_logs(&runs, "--dir .", Some("nope"), 10)
            .unwrap_err()
            .details["reason"],
        "run"
    );

    // 2. A green run: none failed, the run named, not running.
    let id = ran(&dir, &runs);
    let green = failed_logs(&runs, "--dir .", None, 10).unwrap_err();
    assert_eq!(green.code, "E_INVALID_ARG", "{green:?}");
    assert_eq!(
        (
            &green.details["reason"],
            &green.details["run"],
            &green.details["running"]
        ),
        (&"none_failed".into(), &id.as_str().into(), &false.into())
    );
    assert!(
        green
            .hint
            .unwrap()
            .contains(&format!("check log NAME --dir . --run {id}"))
    );

    // 3. A run still running: its record lists every check `not_run`, which is left out, so
    //    none failed so far; the answer says it runs.
    let run_dir = runs.join(&id);
    let record = std::fs::read_to_string(run_dir.join(RUN_FILE)).unwrap();
    let mut record: Value = serde_json::from_str(&record).unwrap();
    record["state"] = "running".into();
    record["result"]["checks"][0]["outcome"] = "not_run".into();
    std::fs::write(run_dir.join(RUN_FILE), record.to_string()).unwrap();
    let lock = File::open(run_dir.join(RUN_LOCK)).unwrap();
    lock.lock().unwrap();
    let running = failed_logs(&runs, "--dir .", None, 10).unwrap_err();
    assert_eq!(
        (&running.details["reason"], &running.details["running"]),
        (&"none_failed".into(), &true.into())
    );
    assert!(running.hint.unwrap().contains("wait"));

    //    One that has failed already is given, the one not started left out.
    let mut two = record.clone();
    let mut bad = two["result"]["checks"][0].clone();
    bad["name"] = "bad".into();
    bad["outcome"] = "failed".into();
    two["result"]["checks"] = Value::Array(vec![bad, record["result"]["checks"][0].clone()]);
    std::fs::write(run_dir.join(RUN_FILE), two.to_string()).unwrap();
    let so_far = failed_logs(&runs, "--dir .", None, 10).unwrap();
    assert_eq!(so_far.failed, ["bad"]);
    std::fs::write(run_dir.join(RUN_FILE), record.to_string()).unwrap();
    lock.unlock().unwrap();

    // 4. The same record once the run ended: a check never run, which has no log, did not
    //    pass.
    record["state"] = "ended".into();
    std::fs::write(run_dir.join(RUN_FILE), record.to_string()).unwrap();
    std::fs::remove_file(run_dir.join("logs/ok.log")).unwrap();
    let ended = failed_logs(&runs, "--dir .", None, 10).unwrap();
    assert_eq!(ended.failed, ["ok"]);
    assert_eq!(
        (ended.outcome, ended.lines.len()),
        (Some(CheckOutcome::NotRun), 0)
    );
}

/// A run found through its run lines: the newest line of the run names its directory, which is
/// read as any other; a run the lines do not hold, a line with no directory, a directory of
/// another name and a directory that is gone are refused, each with its reason.
#[test]
fn a_run_is_found_through_its_lines() {
    // 1. A run whose lines went to a directory of their own.
    let (_tmp, dir, runs) = tree(&failing(1));
    let lines_dir = runs.parent().unwrap().join("lines");
    let mut opts = RunOpts::new(&nothing);
    opts.runs_root = runs.parent().map(Utf8Path::to_owned);
    opts.lines = Some(lines_dir.clone());
    let error = run_dir(&dir, CheckStage::BeforeLand, &[], &opts).unwrap_err();
    let resp: CheckRunResp = serde_json::from_value(error.details["run"].clone()).unwrap();

    // 2. Found: the runs' directory, from which both forms of the log read.
    let found = run_through_lines(&lines_dir, &resp.run).unwrap();
    assert_eq!(found, runs);
    let through = failed_logs(&found, "", Some(&resp.run), 1_000).unwrap();
    assert_eq!(
        through,
        failed_logs(&runs, "", Some(&resp.run), 1_000).unwrap()
    );
    assert_eq!(through.failed, ["c01"]);

    // 3. A run the lines do not hold, and an id that is none.
    let other = "0193f1e2-0000-7000-8000-000000000000";
    let unknown = run_through_lines(&lines_dir, other).unwrap_err();
    assert_eq!(
        (&unknown.details["reason"], &unknown.details["run"]),
        (&"run".into(), &other.into())
    );
    assert_eq!(
        run_through_lines(&lines_dir, "nope").unwrap_err().details["reason"],
        "run"
    );
    let nowhere = run_through_lines(&lines_dir.join("none"), &resp.run).unwrap_err();
    assert_eq!(nowhere.details["reason"], "run", "{nowhere:?}");

    // 4. A line with no run directory; one whose directory is of another name.
    let file = lines_dir.join(crate::check::lines::RUN_LINES_FILE);
    let text = std::fs::read_to_string(&file).unwrap();
    let rewrite = |change: &dyn Fn(&mut Value)| {
        let rewritten: Vec<String> = (text.lines())
            .map(|line| {
                let mut line: Value = serde_json::from_str(line).unwrap();
                change(&mut line);
                line.to_string()
            })
            .collect();
        std::fs::write(&file, rewritten.join("\n") + "\n").unwrap();
    };
    rewrite(&|line| {
        line.as_object_mut().unwrap().remove("run_dir");
    });
    let no_dir = run_through_lines(&lines_dir, &resp.run).unwrap_err();
    assert_eq!(no_dir.details["reason"], "run_dir", "{no_dir:?}");
    let hint = no_dir.hint.unwrap();
    assert!(hint.contains("--dir") && hint.contains("--ws"), "{hint}");
    rewrite(&|line| line["run_dir"] = runs.join(other).as_str().into());
    let renamed = run_through_lines(&lines_dir, &resp.run).unwrap_err();
    assert_eq!(renamed.details["reason"], "run_dir", "{renamed:?}");

    // 5. The run's directory gone, past the runs kept: the names the lines give for it.
    std::fs::write(&file, &text).unwrap();
    std::fs::remove_dir_all(runs.join(&resp.run)).unwrap();
    let gone = run_through_lines(&lines_dir, &resp.run).unwrap_err();
    assert_eq!(gone.code, "E_INVALID_ARG", "{gone:?}");
    assert_eq!(
        (&gone.details["reason"], &gone.details["failed"]),
        (&"run_gone".into(), &serde_json::json!(["c01"]))
    );
    assert!(gone.hint.unwrap().contains("20"), "the runs kept");
}

/// `newest_run` is the run of the greatest id whose record reads, running or ended.
#[test]
fn the_newest_run_is_the_last_started() {
    let (_tmp, dir, runs) = tree(&failing(1));
    assert!(newest_run(&runs).unwrap().is_none(), "no run yet");
    let first = ran_whole(&dir, &runs);
    let second = ran_whole(&dir, &runs);
    assert!(first.run < second.run);
    assert_eq!(newest_run(&runs).unwrap().unwrap().id, second.run);
    std::fs::write(runs.join(&second.run).join(RUN_FILE), "not json").unwrap();
    assert_eq!(
        newest_run(&runs).unwrap().unwrap().id,
        first.run,
        "a record that reads"
    );
}
