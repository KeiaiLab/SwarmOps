//! The SARIF reader: the fixture, the mapping of levels, kinds and locations, and every refusal.

use swarmops_api::{CheckFindingLevel, CheckReportFault};

use super::*;
use crate::check::{MAX_FINDING_MESSAGE_BYTES, MAX_FINDING_TITLE_BYTES};

/// Three results of `error`, `warning` and `note`, one of kind `pass`, one with no level.
const FIXTURE: &[u8] = include_bytes!("../../../tests/fixtures/check/sarif.json");

/// The root of a check in `.` with no host directory.
const ROOT: Place<'static> = Place {
    dir: ".",
    host: None,
};

/// A finding's level, title, path, line and end line.
type Shown<'f> = (
    CheckFindingLevel,
    &'f str,
    Option<&'f str>,
    Option<u64>,
    Option<u64>,
);

/// What `json` reads as at the root, or the test fails.
fn read_ok(json: &str) -> SarifRead {
    read(json.as_bytes(), ROOT).unwrap_or_else(|e| panic!("{json}: {e:?}"))
}

/// A log of one run of `results`.
fn log(results: &str) -> String {
    format!(r#"{{"version":"2.1.0","runs":[{{"results":[{results}]}}]}}"#)
}

/// The fixture: a finding for each result of kind `fail`, in order, with its level, title,
/// message, group and location; the `pass` result counted and no finding; no level is `warning`.
#[test]
fn the_fixture_gives_a_finding_a_failing_result() {
    let read = read(FIXTURE, Place::new("crates/app", None)).unwrap();
    assert_eq!((read.results, read.found), (5, 4));
    let shown: Vec<Shown<'_>> = (read.findings.iter())
        .map(|f| {
            (
                f.level,
                f.title.as_str(),
                f.path.as_deref(),
                f.line,
                f.end_line,
            )
        })
        .collect();
    assert_eq!(
        shown,
        [
            (
                CheckFindingLevel::Error,
                "unused-import",
                Some("crates/app/src/lib.rs"),
                Some(3),
                Some(3)
            ),
            (
                CheckFindingLevel::Warning,
                "long-function",
                Some("crates/app/src/my mod.rs"),
                Some(10),
                Some(129)
            ),
            (
                CheckFindingLevel::Notice,
                "consider a doc comment",
                None,
                None,
                None
            ),
            (CheckFindingLevel::Warning, "no-level", None, None, None),
        ]
    );
    for finding in &read.findings {
        assert_eq!(finding.group.as_deref(), Some("fixture-lint"));
    }
    assert_eq!(
        read.findings[0].message.as_deref(),
        Some("the import of `fs` is never used\nremove it")
    );
}

/// Each level maps; a kind other than `fail` is counted and gives no finding; a result with
/// neither rule nor message is titled so; a run's tool may come after its results, and each
/// run's findings take its own tool.
#[test]
fn levels_kinds_and_tools_map() {
    let results = [
        r#"{"level":"error","ruleId":"a"}"#,
        r#"{"level":"warning","ruleId":"b"}"#,
        r#"{"level":"note","ruleId":"c"}"#,
        r#"{"level":"none","ruleId":"d"}"#,
        r#"{"ruleId":"e","kind":"fail"}"#,
        r#"{"kind":"open"}"#,
        r#"{"kind":"review"}"#,
        r#"{"kind":"informational"}"#,
        r#"{"kind":"notApplicable"}"#,
        "{}",
    ];
    let read = read_ok(&log(&results.join(",")));
    let shown: Vec<(CheckFindingLevel, &str)> = (read.findings.iter())
        .map(|f| (f.level, f.title.as_str()))
        .collect();
    assert_eq!(
        shown,
        [
            (CheckFindingLevel::Error, "a"),
            (CheckFindingLevel::Warning, "b"),
            (CheckFindingLevel::Notice, "c"),
            (CheckFindingLevel::Notice, "d"),
            (CheckFindingLevel::Warning, "e"),
            (CheckFindingLevel::Warning, "(no title)"),
        ]
    );
    assert_eq!((read.results, read.found), (10, 6));

    let two = r#"{"version":"2.1.0","runs":[
        {"results":[{"ruleId":"x"}],"tool":{"driver":{"name":"late"}}},
        {"tool":{"driver":{"name":"early"}},"results":[{"ruleId":"y"}]},
        {"results":[{"ruleId":"z"}]},
        {"results":null}]}"#;
    let read = read_ok(two);
    let groups: Vec<Option<&str>> = read.findings.iter().map(|f| f.group.as_deref()).collect();
    assert_eq!(groups, [Some("late"), Some("early"), None]);
}

/// A relative reference is percent-decoded and joined to the check's directory; a `file:` URI is
/// taken below the check's directory on the host only; `..`, another scheme, a query and a bad
/// escape give no path; the finding is kept, and a line only comes with a path; an end before
/// the start is left out.
#[test]
fn a_location_s_path_is_a_tree_path_or_none() {
    let place = Place::new("crates/app", Some("/work/tree/crates/app"));
    let with = |uri: &str| -> (Option<String>, Option<u64>) {
        let json = log(&format!(
            r#"{{"ruleId":"r","locations":[{{"physicalLocation":{{"artifactLocation":{{"uri":{uri:?}}},"region":{{"startLine":4,"endLine":2}}}}}}]}}"#
        ));
        let read = read(json.as_bytes(), place).unwrap();
        assert_eq!(read.findings.len(), 1, "{uri}");
        let finding = &read.findings[0];
        assert_eq!(finding.end_line, None, "{uri}");
        (finding.path.clone(), finding.line)
    };
    let found = |path: &str| (Some(path.to_owned()), Some(4));
    assert_eq!(with("src/a.rs"), found("crates/app/src/a.rs"));
    assert_eq!(with("./src/%C3%A9.rs"), found("crates/app/src/é.rs"));
    assert_eq!(
        with("file:///work/tree/crates/app/src/a.rs"),
        found("crates/app/src/a.rs")
    );
    assert_eq!(
        with("file://localhost/work/tree/crates/app/b.rs"),
        found("crates/app/b.rs")
    );
    assert_eq!(
        with("file:/work/tree/crates/app/c.rs"),
        found("crates/app/c.rs")
    );
    for none in [
        "file:///work/tree/crates/other/a.rs",
        "file:///work/tree/crates/apple/a.rs",
        "file://host/work/tree/crates/app/a.rs",
        "file:///work/tree/crates/app/../../x.rs",
        "https://example.com/a.rs",
        "../a.rs",
        "/etc/passwd",
        "src/a.rs?x=1",
        "src/a.rs#L3",
        "src/%zz.rs",
        "src/%ff.rs",
        "",
    ] {
        assert_eq!(with(none), (None, None), "{none}");
    }
    let rootless = log(
        r#"{"locations":[{"physicalLocation":{"artifactLocation":{"uri":"file:///work/tree/crates/app/a.rs"}}}]}"#,
    );
    let read = read(rootless.as_bytes(), Place::new("crates/app", None)).unwrap();
    assert_eq!(read.findings[0].path, None);
}

/// Only the first location is taken: a later one of any shape is passed over, not held.
#[test]
fn only_the_first_location_is_read() {
    let json = log(
        r#"{"ruleId":"r","locations":[{"physicalLocation":{"artifactLocation":{"uri":"src/a.rs"},"region":{"startLine":4}}},7]}"#,
    );
    let read = read_ok(&json);
    let finding = &read.findings[0];
    assert_eq!(
        (finding.path.as_deref(), finding.line),
        (Some("src/a.rs"), Some(4))
    );
    let none = read_ok(&log(r#"{"ruleId":"r","locations":[]}"#));
    assert_eq!(none.findings[0].path, None);
}

/// The title is the rule, else the message's first line that is not blank; the message is cut
/// to 4 KiB.
#[test]
fn the_title_is_the_rule_or_the_message_s_first_line() {
    let long = "m".repeat(5_000);
    let read = read_ok(&log(&format!(
        r#"{{"message":{{"text":"\n  first line\nsecond"}}}},{{"ruleId":"","message":{{"text":"{long}"}}}}"#
    )));
    assert_eq!(read.findings[0].title, "first line");
    assert_eq!(read.findings[1].title.len(), MAX_FINDING_TITLE_BYTES);
    let message = read.findings[1].message.as_deref().unwrap();
    assert_eq!(message.len(), MAX_FINDING_MESSAGE_BYTES);
    assert!(message.ends_with('…'));
}

/// Each fault, refused with its words.
#[test]
fn each_fault_is_refused() {
    let deep = format!(
        r#"{{"version":"2.1.0","runs":[],"x":{}{}}}"#,
        "[".repeat(MAX_REPORT_DEPTH),
        "]".repeat(MAX_REPORT_DEPTH)
    );
    let fits = format!(
        r#"{{"version":"2.1.0","runs":[],"x":{}{}}}"#,
        "[".repeat(MAX_REPORT_DEPTH - 1),
        "]".repeat(MAX_REPORT_DEPTH - 1)
    );
    assert_eq!(read_ok(&fits).results, 0);
    let quoted = format!(
        r#"{{"version":"2.1.0","runs":[],"x":"{}"}}"#,
        "[{".repeat(100)
    );
    assert_eq!(read_ok(&quoted).results, 0);
    let cases: Vec<(Vec<u8>, CheckReportFault, &str)> = vec![
        (
            br#"{"version":"2.0.0","runs":[]}"#.to_vec(),
            CheckReportFault::Shape,
            "version \"2.0.0\", not 2.1.0",
        ),
        (
            br#"{"runs":[]}"#.to_vec(),
            CheckReportFault::Shape,
            "no version: a SARIF log names 2.1.0",
        ),
        (
            br#"{"version":"2.1.0"}"#.to_vec(),
            CheckReportFault::Shape,
            "no runs: a SARIF log holds an array of runs",
        ),
        (
            br#"{"version":"2.1.0","runs":{}}"#.to_vec(),
            CheckReportFault::Shape,
            "not a SARIF log: a value of the wrong type, at line 1 column 26",
        ),
        (
            br#"{"version":"2.1.0","runs":[{"results":[{"level":5}]}]}"#.to_vec(),
            CheckReportFault::Shape,
            "not a SARIF log: a value of the wrong type, at line 1 column 49",
        ),
        (
            b"[]".to_vec(),
            CheckReportFault::Shape,
            "not a SARIF log: a value of the wrong type, at line 1 column 0",
        ),
        (
            deep.into_bytes(),
            CheckReportFault::TooDeep,
            "arrays and objects nested past 64",
        ),
        (
            b"<testsuites/>".to_vec(),
            CheckReportFault::Syntax,
            "not JSON, at line 1 column 1",
        ),
        (
            br#"{"version":"2.1.0","runs":["#.to_vec(),
            CheckReportFault::Syntax,
            "not JSON: it ends before its last value, at line 1 column 27",
        ),
        (
            br#"{"version":"2.1.0","runs":[]} x"#.to_vec(),
            CheckReportFault::Syntax,
            "not JSON, at line 1 column 31",
        ),
        (
            [&[0xFE, 0xFF][..], b"{}"].concat(),
            CheckReportFault::NotText,
            "a UTF-16 byte order mark: only UTF-8 is read",
        ),
        (
            b"{\"version\":\"\xff\"}".to_vec(),
            CheckReportFault::NotText,
            "not UTF-8, at byte 12",
        ),
    ];
    for (bytes, fault, words) in cases {
        let got = read(&bytes, ROOT).map(|_| ()).unwrap_err();
        assert_eq!(
            (got.0, got.1.as_str()),
            (fault, words),
            "{}",
            String::from_utf8_lossy(&bytes)
        );
    }
}

/// A reader asked to keep one finding keeps one and counts them all; a UTF-8 mark reads.
#[test]
fn a_reader_keeps_what_it_is_asked_to() {
    let kept = read_keeping(FIXTURE, ROOT, 1).unwrap();
    assert_eq!((kept.findings.len(), kept.found, kept.results), (1, 4, 5));
    let marked = [&b"\xEF\xBB\xBF"[..], br#"{"version":"2.1.0","runs":[]}"#].concat();
    assert_eq!(read(&marked, ROOT).unwrap(), SarifRead::default());
}
