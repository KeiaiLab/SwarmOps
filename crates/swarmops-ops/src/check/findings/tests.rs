//! The report file's reader, the cuts and the run's bound.

use swarmops_api::{CheckFindingLevel, CheckFindingOut};

use super::*;

/// The one finding `line` gives, or the test fails.
fn one(line: &str) -> CheckFindingOut {
    let read = read_report_file(line.as_bytes());
    assert_eq!(read.invalid, 0, "{read:?}");
    assert_eq!(read.findings.len(), 1, "{read:?}");
    read.findings[0].clone()
}

/// Why `line` is invalid, as the report's first invalid line says.
fn why(line: &[u8]) -> String {
    let read = read_report_file(line);
    assert!(read.findings.is_empty(), "{read:?}");
    assert_eq!(read.invalid, 1, "{read:?}");
    read.first_invalid.unwrap()
}

/// The fixture: a finding with every key, an unknown key on the second line (invalid, named), a
/// blank line, a finding with a path and no line.
#[test]
fn the_fixture_reads_in_line_order() {
    let read = read_report_file(include_bytes!(
        "../../../tests/fixtures/check/findings.jsonl"
    ));
    let mut first = CheckFindingOut::new(CheckFindingLevel::Error, "unused variable `x`");
    first.path = Some("crates/app/src/lib.rs".to_owned());
    first.line = Some(12);
    first.end_line = Some(14);
    first.message = Some("remove it or prefix it with _".to_owned());
    let mut last = CheckFindingOut::new(CheckFindingLevel::Notice, "coverage 81%");
    last.path = Some("crates/app".to_owned());
    assert_eq!(read.findings, [first, last]);
    assert_eq!((read.lines, read.blank, read.invalid), (4, 1, 1));
    assert_eq!(
        read.first_invalid.as_deref(),
        Some(r#"line 2: unknown key "group""#)
    );
    assert!(!read.cut);
}

/// Each level reads; a message is kept with its line breaks; an empty one is left out.
#[test]
fn each_key_reads() {
    for level in CheckFindingLevel::ALL {
        let line = format!(r#"{{"level":"{}","title":"t"}}"#, level.as_str());
        assert_eq!(one(&line), CheckFindingOut::new(level, "t"));
    }
    let finding = one(r#"{"title":"t","message":"a\nb","level":"warning"}"#);
    assert_eq!(finding.message.as_deref(), Some("a\nb"));
    let finding = one(r#"{"level":"notice","title":"t","message":""}"#);
    assert_eq!(finding.message, None);
    let finding = one(r#"{"level":"error","title":"t","path":"a/b","line":1,"end_line":1}"#);
    assert_eq!(
        (finding.path.as_deref(), finding.line, finding.end_line),
        (Some("a/b"), Some(1), Some(1))
    );
    let finding = one(&format!(
        r#"{{"level":"error","title":"t","path":"a","line":{MAX_FINDING_LINE},"end_line":{MAX_FINDING_LINE}}}"#
    ));
    assert_eq!(finding.end_line, Some(1 << 32));
}

/// Every kind of invalid line, with the words its first-invalid message gives.
#[test]
fn each_invalid_line_says_why() {
    let cases: &[(&[u8], &str)] = &[
        (b"not json", "line 1: not JSON"),
        (b"[1, 2]", "line 1: not an object"),
        (b"\"text\"", "line 1: not an object"),
        (
            br#"{"lvl":"error","title":"t"}"#,
            r#"line 1: unknown key "lvl""#,
        ),
        (br#"{"title":"t"}"#, "line 1: missing level"),
        (br#"{"level":"error"}"#, "line 1: missing title"),
        (
            br#"{"level":"info","title":"t"}"#,
            "line 1: level is not error, warning or notice",
        ),
        (
            br#"{"level":1,"title":"t"}"#,
            "line 1: level is not error, warning or notice",
        ),
        (
            br#"{"level":"error","title":2}"#,
            "line 1: title is not a string",
        ),
        (
            br#"{"level":"error","title":" \n "}"#,
            "line 1: title is empty",
        ),
        (
            br#"{"level":"error","title":"t","message":null}"#,
            "line 1: message is not a string",
        ),
        (
            br#"{"level":"error","title":"t","path":7}"#,
            "line 1: path is not a string",
        ),
        (
            br#"{"level":"error","title":"t","path":"/etc/passwd"}"#,
            "line 1: path is not a repository path",
        ),
        (
            br#"{"level":"error","title":"t","path":"a/../b"}"#,
            "line 1: path is not a repository path",
        ),
        (
            br#"{"level":"error","title":"t","line":3}"#,
            "line 1: line without path",
        ),
        (
            br#"{"level":"error","title":"t","path":"a","line":0}"#,
            "line 1: line is not a whole number from 1 to 4294967296",
        ),
        (
            br#"{"level":"error","title":"t","path":"a","line":4294967297}"#,
            "line 1: line is not a whole number from 1 to 4294967296",
        ),
        (
            br#"{"level":"error","title":"t","path":"a","line":2.5}"#,
            "line 1: line is not a whole number from 1 to 4294967296",
        ),
        (
            br#"{"level":"error","title":"t","path":"a","end_line":3}"#,
            "line 1: end_line without line",
        ),
        (
            br#"{"level":"error","title":"t","path":"a","line":3,"end_line":2}"#,
            "line 1: end_line is not a whole number from line to 4294967296",
        ),
        (
            b"{\"level\":\"error\",\"title\":\"\xff\"}",
            "line 1: not UTF-8",
        ),
    ];
    for (line, said) in cases {
        assert_eq!(why(line), *said, "{}", String::from_utf8_lossy(line));
    }
}

/// An unknown key's name is cut and its control characters escaped.
#[test]
fn an_unknown_key_is_shown_cut() {
    let key = format!("{}\\u001b", "k".repeat(40));
    let line = format!(r#"{{"level":"error","title":"t","{key}":1}}"#);
    let said = why(line.as_bytes());
    assert_eq!(said, format!("line 1: unknown key \"{}\"…", "k".repeat(32)));
    let line = r#"{"level":"error","title":"t","a\u001bb":1}"#;
    assert_eq!(why(line.as_bytes()), r#"line 1: unknown key "a\u{1b}b""#);
}

/// Blank lines, of spaces and tabs or nothing, count nowhere; CRLF reads as LF; the last line
/// needs no `\n`; the first invalid line is the one named, the others counted.
#[test]
fn blank_lines_crlf_and_the_first_invalid_line() {
    let text = "\r\n \t\n{\"level\":\"error\",\"title\":\"a\"}\r\nnope\n\n[]\n\
                {\"level\":\"notice\",\"title\":\"b\"}";
    let read = read_report_file(text.as_bytes());
    let titles: Vec<&str> = read.findings.iter().map(|f| f.title.as_str()).collect();
    assert_eq!(titles, ["a", "b"]);
    assert_eq!((read.lines, read.blank, read.invalid), (7, 3, 2));
    assert_eq!(read.first_invalid.as_deref(), Some("line 4: not JSON"));
    assert_eq!(read.found, 2);
    assert_eq!(read_report_file(b""), FileRead::default());
    assert_eq!(read_report_file(b"\n").lines, 1);
}

/// A title is one line: its line breaks are spaces, and it is trimmed.
#[test]
fn a_title_is_one_line() {
    let finding = one(r#"{"level":"error","title":" first\r\nsecond\nthird "}"#);
    assert_eq!(finding.title, "first  second third");
}

/// Titles and messages are cut at a character's end with `…`, within their bounds; a path past
/// its bound makes the line invalid.
#[test]
fn titles_and_messages_are_cut_at_a_character_s_end() {
    // 1. A title of 199 ASCII bytes and a 2-byte character: past 200, cut before it.
    let title = format!("{}é", "a".repeat(199));
    let finding = one(&format!(r#"{{"level":"error","title":"{title}"}}"#));
    assert_eq!(finding.title, format!("{}…", "a".repeat(197)));
    assert!(finding.title.len() <= MAX_FINDING_TITLE_BYTES);
    let exact = "é".repeat(100);
    assert_eq!(
        one(&format!(r#"{{"level":"error","title":"{exact}"}}"#)).title,
        exact
    );

    // 2. A message of 4 KiB of 3-byte characters and one more.
    let message = "€".repeat(1366);
    let finding = one(&format!(
        r#"{{"level":"error","title":"t","message":"{message}"}}"#
    ));
    let cut = finding.message.unwrap();
    assert!(cut.len() <= MAX_FINDING_MESSAGE_BYTES, "{}", cut.len());
    assert_eq!(cut, format!("{}…", "€".repeat(1364)));

    // 3. A path of 4 KiB reads; one byte more does not.
    let path = format!("{}/b", "a".repeat(MAX_FINDING_PATH_BYTES - 2));
    let finding = one(&format!(
        r#"{{"level":"error","title":"t","path":"{path}"}}"#
    ));
    assert_eq!(finding.path.as_deref(), Some(path.as_str()));
    let line = format!(r#"{{"level":"error","title":"t","path":"a{path}"}}"#);
    assert_eq!(why(line.as_bytes()), "line 1: path is longer than 4 KiB");
}

/// A line of 64 KiB reads; one of 64 KiB and one byte is invalid, and the next line reads.
#[test]
fn a_line_past_64_kib_is_invalid() {
    let finding = |len: usize| {
        let head = r#"{"level":"error","title":"t","message":""#;
        let pad = len - head.len() - 2;
        format!("{head}{}\"}}", "m".repeat(pad))
    };
    assert_eq!(
        finding(MAX_FINDING_LINE_BYTES).len(),
        MAX_FINDING_LINE_BYTES
    );
    one(&finding(MAX_FINDING_LINE_BYTES));
    let text = format!(
        "{}\n{{\"level\":\"notice\",\"title\":\"next\"}}\n",
        finding(MAX_FINDING_LINE_BYTES + 1)
    );
    let read = read_report_file(text.as_bytes());
    assert_eq!(
        read.first_invalid.as_deref(),
        Some("line 1: longer than 64 KiB")
    );
    assert_eq!(read.findings.len(), 1);
    assert_eq!(read.findings[0].title, "next");
}

/// Past the bound, the whole lines before it are read and the rest is not: `cut`.
#[test]
fn a_file_past_its_bound_is_read_to_it() {
    let line = "{\"level\":\"error\",\"title\":\"t\"}\n";
    let text = line.repeat(4);
    let read = read_bounded(text.as_bytes(), line.len() * 2 + 5, usize::MAX);
    assert!(read.cut);
    assert_eq!((read.findings.len(), read.lines, read.invalid), (2, 2, 0));
    let read = read_bounded(text.as_bytes(), line.len() * 2, usize::MAX);
    assert_eq!((read.findings.len(), read.cut), (2, true));
    let read = read_bounded(text.as_bytes(), text.len(), usize::MAX);
    assert_eq!((read.findings.len(), read.cut), (4, false));
    let read = read_bounded(text.as_bytes(), 5, usize::MAX);
    assert_eq!((read.lines, read.cut), (0, true));
}

/// The cuts: within their bound, at a character's boundary, `…` on the cut side.
#[test]
fn the_cuts_keep_their_bound() {
    assert_eq!(cut_end("abc", 3), "abc");
    assert_eq!(cut_end("abcd", 3), "…");
    assert_eq!(cut_end("abcdef", 5), "ab…");
    for limit in 3..12 {
        for text in ["aé€xyz€é", "€€€€€"] {
            assert!(cut_end(text, limit).len() <= limit);
        }
    }
    assert_eq!(message_of(""), None);
}

/// A reader keeps no more findings than it is asked to, and counts them all; the budget's room
/// is what the run can still return.
#[test]
fn a_reader_keeps_what_the_run_can_return() {
    let text = "{\"level\":\"error\",\"title\":\"t\"}\n".repeat(5);
    let read = read_bounded(text.as_bytes(), text.len(), 2);
    assert_eq!((read.findings.len(), read.found, read.lines), (2, 5, 5));
    let mut budget = Budget::bounded(3, 1_000);
    assert_eq!(budget.room(), 3);
    let finding = CheckFindingOut::new(CheckFindingLevel::Notice, "n");
    assert!(budget.take(&finding));
    assert_eq!(budget.room(), 2);
    budget.skip(4);
    assert_eq!((budget.room(), budget.omitted()), (0, 4));
    assert!(!budget.take(&finding));
}

/// Two checks of 6,000 and 5,000 findings: the first's 6,000 and the second's first 4,000 are
/// returned, its last 1,000 counted.
#[test]
fn the_run_returns_10000_findings() {
    let mut budget = Budget::new();
    let finding = CheckFindingOut::new(CheckFindingLevel::Warning, "w");
    let first = (0..6_000).filter(|_| budget.take(&finding)).count();
    let second = (0..5_000).filter(|_| budget.take(&finding)).count();
    assert_eq!((first, second), (6_000, 4_000));
    assert_eq!(budget.omitted(), 1_000);
}

/// A long message that would pass the byte bound stops the run's returns there: the smaller
/// findings after it are counted, not returned.
#[test]
fn the_byte_bound_stops_the_returns_there() {
    let mut budget = Budget::bounded(100, 100);
    let small = CheckFindingOut::new(CheckFindingLevel::Notice, "1234567890");
    let mut long = small.clone();
    long.message = Some("m".repeat(80));
    assert!(budget.take(&small));
    assert!(budget.take(&small));
    assert!(!budget.take(&long));
    assert!(!budget.take(&small));
    assert_eq!(budget.omitted(), 2);
    let mut exact = Budget::bounded(100, 10);
    assert!(exact.take(&small));
    assert!(!exact.take(&small));
}
