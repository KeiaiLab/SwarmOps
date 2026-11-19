//! Taking in a check's declared reports: where each is looked for, whether this run wrote it,
//! and how it is refused.

use std::fs;
use std::time::{Duration, SystemTime};

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_api::{CheckReportFault, CheckReportFormat, CheckReportOut, CheckReportState};

use super::*;

/// The nextest fixture, two failed cases.
const NEXTEST: &[u8] = include_bytes!("../../../tests/fixtures/check/nextest-junit.xml");

/// The SARIF fixture, four findings.
const SARIF: &[u8] = include_bytes!("../../../tests/fixtures/check/sarif.json");

/// A temporary check directory.
fn cwd() -> (tempfile::TempDir, Utf8PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = Utf8PathBuf::from_path_buf(fs::canonicalize(tmp.path()).unwrap()).unwrap();
    (tmp, cwd)
}

/// A JUnit report declared at `path`.
fn junit(path: &str) -> CheckReportOut {
    CheckReportOut::new(CheckReportFormat::Junit, path)
}

/// What a check that `ran` in `cwd`, declaring `reports`, leaves for its intake: its reports
/// looked at before, `ran` run, then taken in.
fn after(cwd: &Utf8Path, reports: &[CheckReportOut], ran: impl FnOnce()) -> Intake {
    let before = Before::look(cwd, reports);
    ran();
    let mut intake = Intake::default();
    take_declared(&mut intake, cwd, ".", reports, &before, &mut Budget::new());
    intake
}

/// Writes `bytes` at `path` below `cwd`, its directories made.
fn put(cwd: &Utf8Path, path: &str, bytes: &[u8]) {
    let at = cwd.join(path);
    fs::create_dir_all(at.parent().unwrap()).unwrap();
    fs::write(at, bytes).unwrap();
}

/// The one report's state and fault.
fn state(intake: &Intake) -> (CheckReportState, Option<CheckReportFault>) {
    (intake.reports[0].state, intake.reports[0].fault)
}

/// A report written by the run is read, its findings and counts taken; one not there is
/// `missing`; a file where a directory of its path should be leaves it `missing`.
#[test]
fn a_fresh_report_is_read_and_an_absent_one_is_missing() {
    let (_tmp, cwd) = cwd();
    let reports = [
        junit("out/junit.xml"),
        CheckReportOut::new(CheckReportFormat::Sarif, "out/a.sarif"),
        junit("none.xml"),
        junit("file/junit.xml"),
    ];
    put(&cwd, "file", b"a file");
    let intake = after(&cwd, &reports, || {
        put(&cwd, "out/junit.xml", NEXTEST);
        put(&cwd, "out/a.sarif", SARIF);
    });
    let states: Vec<CheckReportState> = intake.reports.iter().map(|r| r.state).collect();
    assert_eq!(
        states,
        [
            CheckReportState::Read,
            CheckReportState::Read,
            CheckReportState::Missing,
            CheckReportState::Missing
        ]
    );
    let junit = &intake.reports[0];
    assert_eq!(
        (junit.findings, junit.tests, junit.failed, junit.skipped),
        (Some(2), Some(3), Some(2), Some(0))
    );
    assert_eq!(junit.path, "out/junit.xml");
    let sarif = &intake.reports[1];
    assert_eq!((sarif.findings, sarif.results), (Some(4), Some(5)));
    assert_eq!(intake.total, 6);
    let indexes: Vec<u32> = intake.findings.iter().map(|f| f.report).collect();
    assert_eq!(indexes, [0, 0, 1, 1, 1, 1]);
}

/// A report there before the check and not written by it is `stale`, and not read; one
/// rewritten in place by the run is read.
#[test]
fn a_report_left_from_before_is_stale_and_a_rewritten_one_read() {
    let (_tmp, cwd) = cwd();
    let reports = [junit("junit.xml")];
    put(&cwd, "junit.xml", NEXTEST);
    let untouched = after(&cwd, &reports, || {});
    assert_eq!(state(&untouched), (CheckReportState::Stale, None));
    assert_eq!((untouched.total, untouched.reports[0].findings), (0, None));

    let rewritten = after(&cwd, &reports, || {
        std::thread::sleep(Duration::from_millis(20));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(cwd.join("junit.xml"))
            .unwrap();
        std::io::Write::write_all(&mut file, NEXTEST).unwrap();
    });
    assert_eq!(state(&rewritten), (CheckReportState::Read, None));
}

/// A copy that keeps an old modification time (as `cp -p` makes) is `stale`, whether it
/// replaced a file or made a new one; one a second old is still this run's.
#[test]
fn a_copy_that_keeps_an_old_time_is_stale() {
    let (_tmp, cwd) = cwd();
    let reports = [junit("junit.xml")];
    let set_mtime = |ago: Duration| {
        let file = fs::File::options()
            .write(true)
            .open(cwd.join("junit.xml"))
            .unwrap();
        let then = SystemTime::now() - ago;
        file.set_times(fs::FileTimes::new().set_modified(then))
            .unwrap();
    };
    let copied = after(&cwd, &reports, || {
        assert!(!cwd.join("junit.xml").exists());
        put(&cwd, "junit.xml", NEXTEST);
        set_mtime(Duration::from_secs(3_600));
    });
    assert_eq!(state(&copied), (CheckReportState::Stale, None));
    put(&cwd, "junit.xml", NEXTEST);
    let over = after(&cwd, &reports, || {
        std::thread::sleep(Duration::from_millis(20));
        put(&cwd, "junit.xml", NEXTEST);
        set_mtime(Duration::from_secs(3_600));
    });
    assert_eq!(state(&over), (CheckReportState::Stale, None));
    let coarse = after(&cwd, &reports, || {
        put(&cwd, "junit.xml", NEXTEST);
        set_mtime(Duration::from_millis(500));
    });
    assert_eq!(state(&coarse), (CheckReportState::Read, None));
}

/// A link as the report, and a link as a directory of its path, are refused `not_a_file`, and
/// what they name is never read: the target is made unreadable, which a read through the link
/// would report as `io`.
#[test]
fn a_link_at_any_component_is_refused_and_not_followed() {
    use std::os::unix::fs::PermissionsExt as _;
    let (_tmp, cwd) = cwd();
    let (_outside_tmp, outside) = super::tests::cwd();
    put(&outside, "secret/junit.xml", NEXTEST);
    let secret = outside.join("secret/junit.xml");
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o000)).unwrap();
    let reports = [junit("linked.xml"), junit("dir/junit.xml")];
    let intake = after(&cwd, &reports, || {
        std::os::unix::fs::symlink(&secret, cwd.join("linked.xml")).unwrap();
        std::os::unix::fs::symlink(outside.join("secret"), cwd.join("dir")).unwrap();
    });
    for report in &intake.reports {
        assert_eq!(
            (report.state, report.fault),
            (CheckReportState::Refused, Some(CheckReportFault::NotAFile)),
            "{report:?}"
        );
    }
    assert_eq!(intake.total, 0);
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
}

/// A FIFO and a directory at the report's path are refused `not_a_file`, without waiting on the
/// FIFO.
#[test]
fn a_fifo_or_a_directory_is_refused_without_blocking() {
    let (_tmp, cwd) = cwd();
    let reports = [junit("fifo.xml"), junit("dir.xml")];
    let intake = after(&cwd, &reports, || {
        let made = std::process::Command::new("mkfifo")
            .arg(cwd.join("fifo.xml"))
            .status()
            .unwrap();
        assert!(made.success(), "mkfifo");
        fs::create_dir(cwd.join("dir.xml")).unwrap();
    });
    for report in &intake.reports {
        assert_eq!(
            (report.state, report.fault),
            (CheckReportState::Refused, Some(CheckReportFault::NotAFile)),
            "{report:?}"
        );
    }
}

/// A report past 64 MiB is refused `too_large`, and nothing of it parsed; one that does not
/// read as its format is refused with the reader's fault and words.
#[test]
fn a_report_past_its_bound_or_not_of_its_format_is_refused() {
    let (_tmp, cwd) = cwd();
    let reports = [
        junit("big.xml"),
        junit("bad.xml"),
        CheckReportOut::new(CheckReportFormat::Sarif, "old.sarif"),
    ];
    let intake = after(&cwd, &reports, || {
        let big = fs::File::create(cwd.join("big.xml")).unwrap();
        big.set_len(MAX_REPORT_BYTES + 1).unwrap();
        put(&cwd, "bad.xml", b"<report/>");
        put(&cwd, "old.sarif", br#"{"version":"2.0.0","runs":[]}"#);
    });
    let shown: Vec<(CheckReportState, Option<CheckReportFault>, Option<&str>)> =
        (intake.reports.iter())
            .map(|r| (r.state, r.fault, r.message.as_deref()))
            .collect();
    assert_eq!(
        shown,
        [
            (
                CheckReportState::Refused,
                Some(CheckReportFault::TooLarge),
                Some("past 64 MiB: not read")
            ),
            (
                CheckReportState::Refused,
                Some(CheckReportFault::Shape),
                Some("a root element <report>, not <testsuites> or <testsuite>")
            ),
            (
                CheckReportState::Refused,
                Some(CheckReportFault::Shape),
                Some("version \"2.0.0\", not 2.1.0")
            ),
        ]
    );
}

/// The run's bound holds across reports: a run with room for one finding keeps one, and counts
/// the rest of both reports as left out.
#[test]
fn the_run_s_bound_holds_across_reports() {
    let (_tmp, cwd) = cwd();
    let reports = [junit("a.xml"), junit("b.xml")];
    let before = Before::look(&cwd, &reports);
    put(&cwd, "a.xml", NEXTEST);
    put(&cwd, "b.xml", NEXTEST);
    let mut budget = Budget::bounded(1, 1 << 20);
    let mut intake = Intake::default();
    take_declared(&mut intake, &cwd, ".", &reports, &before, &mut budget);
    assert_eq!((intake.findings.len(), intake.total), (1, 4));
    assert_eq!(budget.omitted(), 3);
}

/// The time a report may be older than the check's start is a second.
#[test]
fn the_slack_is_a_second() {
    assert_eq!(REPORT_FRESH_SLACK, Duration::from_secs(1));
}
