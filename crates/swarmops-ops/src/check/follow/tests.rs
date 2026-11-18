//! `check log`'s reader: which run it reads, how many lines, and how a log ends.

use std::ffi::OsString;
use std::io::Write as _;

use camino::Utf8PathBuf;
use swarmops_api::{CheckLogEnd, CheckLogStream, CheckOutcome, CheckStage};

use super::*;
use crate::check::run::{RunOpts, run_dir};

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
    let (_, resp) = Follower::start(&runs, "a", None, 1_000, false).unwrap();
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

    let (_, named) = Follower::start(&runs, "a", Some(&first), 2, false).unwrap();
    assert_eq!(named.run, first);
    assert_eq!((named.first_line, named.lines.len()), (3, 2));
    assert_eq!(named.lines[1].n, 4);
}

/// No run of the check, a run not there, and a tail out of its bounds are refused.
#[test]
fn a_run_that_is_not_there_is_refused() {
    let (_tmp, dir, runs) = tree("[check.a]\nargv = [\"/usr/bin/true\"]\n");
    let none = Follower::start(&runs, "a", None, 10, false).unwrap_err();
    assert_eq!(
        (none.code.as_str(), &none.details["reason"]),
        ("E_INVALID_ARG", &"run".into())
    );
    ran(&dir, &runs);
    let other = Follower::start(&runs, "b", None, 10, false).unwrap_err();
    assert_eq!(other.details["check"], "b");
    let gone = Follower::start(
        &runs,
        "a",
        Some("0193f1e2-0000-7000-8000-000000000000"),
        10,
        false,
    )
    .unwrap_err();
    assert_eq!(gone.details["reason"], "run");
    for tail in [0, MAX_CHECK_LOG_TAIL + 1] {
        assert_eq!(
            Follower::start(&runs, "a", None, tail, false)
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
    let (_, resp) = Follower::start(&runs, "a", None, 10, false).unwrap();
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
    let (_, resp) = Follower::start(&runs, "b", None, 10, false).unwrap();
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
    let (mut follower, first) = Follower::start(&runs, "a", None, 1_000, true).unwrap();
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
    let (_, resp) = Follower::start(&runs, "a", None, 5, false).unwrap();
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
    let linked = Follower::start(&runs, "a", None, 5, false).unwrap_err();
    assert_eq!(linked.code, "E_IO", "{linked:?}");
    std::fs::remove_file(&log).unwrap();
    let made = std::process::Command::new("mkfifo")
        .arg(&log)
        .status()
        .unwrap();
    assert!(made.success());
    let fifo = Follower::start(&runs, "a", None, 5, false).unwrap_err();
    assert_eq!(fifo.code, "E_IO", "{fifo:?}");
}
