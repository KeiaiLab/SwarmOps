//! A check's log: its line format and its parse back, sections, bounds, and the dir key.

use serde_json::json;
use swarmops_api::CheckOutcome;
use swarmops_ws::run::{Output, Stream};

use super::*;

/// A log into memory, started.
fn log() -> LineLog<Vec<u8>> {
    LineLog::start(Vec::new(), &start(), 1_763_452_800_123).unwrap()
}

/// The start of a check `unit`.
fn start() -> Start {
    let mut start = Start::new("unit", "0193", "before-land");
    start.argv = vec!["cargo".to_owned(), "test".to_owned()];
    start
}

/// The lines `log` wrote once ended as `end`, parsed, and what it says.
fn ended(log: LineLog<Vec<u8>>, end: End) -> (String, Vec<Line>, Summary) {
    let (bytes, summary) = log.finish(end).unwrap();
    let text = String::from_utf8(bytes).unwrap();
    let lines = parse(&text);
    (text, lines, summary)
}

/// The kinds and texts of `lines`.
fn shown(lines: &[Line]) -> Vec<(&'static str, &str)> {
    lines
        .iter()
        .map(|l| (l.kind.as_str(), l.text.as_str()))
        .collect()
}

/// Every line has the format: a time of five places, three digits of milliseconds, its kind and
/// its text; a CRLF loses its `\r`, a last line without `\n` is written at the end, and bytes
/// that are not UTF-8 are replaced. Each stream keeps its own order, split however it comes.
#[test]
fn each_line_has_its_time_its_kind_and_its_text() {
    let mut log = log();
    log.write(Stream::Stdout, b"one\r\ntw").unwrap();
    log.write(Stream::Stderr, b"warn\xff\n").unwrap();
    log.write(Stream::Stdout, b"o\nlast").unwrap();
    let (text, lines, summary) = ended(log, End::new(CheckOutcome::Passed));

    // 1. The format: every line `    s.mmm kind text`.
    for line in text.lines() {
        let (time, rest) = line.split_at(9);
        assert!(time.starts_with("    "), "{line:?}");
        assert_eq!(time.as_bytes()[5], b'.', "{line:?}");
        assert!(
            rest.starts_with(" out ") || rest.starts_with(" err ") || rest.starts_with(" run ")
        );
    }
    assert_eq!(text.lines().count(), lines.len());

    // 2. The lines, in the order read.
    assert_eq!(
        shown(&lines)[1..lines.len() - 1],
        [
            ("out", "one"),
            ("err", "warn\u{fffd}"),
            ("out", "two"),
            ("out", "last")
        ]
    );
    assert!(lines.iter().all(|l| l.kind != Kind::Other), "{text}");
    let times: Vec<u64> = lines.iter().map(|l| l.at_ms.unwrap()).collect();
    assert!(times.windows(2).all(|w| w[0] <= w[1]), "{times:?}");
    assert_eq!(summary.dropped_bytes, 0);
    assert_eq!(summary.sections, []);
}

/// The first line is `start`, the wall clock given once; the last is `end`, with the outcome,
/// the exit code, the duration, the sections and the bytes dropped.
#[test]
fn the_runner_s_lines_open_and_close_the_log() {
    let mut log = log();
    log.write(Stream::Stdout, b"hi\n").unwrap();
    let mut end = End::new(CheckOutcome::Failed);
    end.exit_code = Some(101);
    let (_, lines, summary) = ended(log, end);
    let start = lines[0].event("start").unwrap();
    assert_eq!(
        serde_json::Value::Object(start),
        json!({"check": "unit", "run": "0193", "stage": "before-land", "tree": null,
               "attempt": 1, "argv": ["cargo", "test"], "dir": ".", "timeout_s": 900,
               "started_at_ms": 1_763_452_800_123_u64})
    );
    let end = lines.last().unwrap().event("end").unwrap();
    assert_eq!(end["outcome"], "failed");
    assert_eq!(end["exit_code"], 101);
    assert_eq!(end["duration_ms"], summary.duration_ms);
    assert_eq!(
        (&end["sections"], &end["dropped_bytes"]),
        (&json!([]), &json!(0))
    );
    assert!(end.get("signal").is_none());
    assert_eq!(lines[0].event("end"), None);
}

/// A marker on either stream opens a section, written as a `run section` line and not as
/// output; the sections come back in order with their start and duration, each until the next
/// one, the last until the output ends. An empty title is an ordinary line, a long one is cut.
#[test]
fn markers_open_sections_in_order() {
    let mut log = log();
    log.write(Stream::Stdout, b"before\nswarmops:section build\na\n")
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    log.write(Stream::Stderr, b"swarmops:section  test  \r\nb\n")
        .unwrap();
    log.write(Stream::Stdout, b"swarmops:section \n").unwrap();
    let wide = format!("swarmops:section {}\n", "é".repeat(150));
    log.write(Stream::Stdout, wide.as_bytes()).unwrap();
    let (_, lines, summary) = ended(log, End::new(CheckOutcome::Passed));

    // 1. The lines: markers as `run section`, the empty one as output.
    let kinds = shown(&lines);
    assert_eq!(kinds[1], ("out", "before"));
    assert_eq!(kinds[2], ("run", r#"section {"n":1,"title":"build"}"#));
    assert_eq!(kinds[3], ("out", "a"));
    assert_eq!(kinds[4], ("run", r#"section {"n":2,"title":"test"}"#));
    assert_eq!(kinds[5], ("err", "b"));
    assert_eq!(kinds[6], ("out", "swarmops:section "));

    // 2. The sections: in order, each with its start and duration, the long title cut.
    let titles: Vec<&str> = summary.sections.iter().map(|s| s.title.as_str()).collect();
    assert_eq!(titles[..2], ["build", "test"]);
    assert!(titles[2].ends_with('…') && titles[2].len() <= MAX_SECTION_TITLE_BYTES);
    let s = &summary.sections;
    assert!(s[0].start_ms < s[1].start_ms, "{s:?}");
    assert_eq!(s[0].start_ms + s[0].duration_ms, s[1].start_ms, "{s:?}");
    assert_eq!(
        s[2].start_ms + s[2].duration_ms,
        summary.duration_ms,
        "{s:?}"
    );
    let end = lines.last().unwrap().event("end").unwrap();
    assert_eq!(
        end["sections"][1],
        json!({"n": 2, "title": "test",
        "start_ms": s[1].start_ms, "duration_ms": s[1].duration_ms})
    );
}

/// Past 1,000 sections a marker is an ordinary line.
#[test]
fn at_most_1000_sections() {
    let mut log = log();
    for n in 0..=MAX_CHECK_SECTIONS {
        log.write(
            Stream::Stdout,
            format!("swarmops:section s{n}\n").as_bytes(),
        )
        .unwrap();
    }
    let (_, lines, summary) = ended(log, End::new(CheckOutcome::Passed));
    assert_eq!(summary.sections.len(), MAX_CHECK_SECTIONS);
    let last = &lines[lines.len() - 2];
    assert_eq!(
        (last.kind, last.text.as_str()),
        (Kind::Out, "swarmops:section s1000")
    );
}

/// A line past 16 KiB keeps its first 16 KiB; the rest is counted as dropped.
#[test]
fn a_long_line_is_cut_and_counted() {
    let mut log = log();
    let wide = vec![b'x'; MAX_CHECK_LOG_LINE_BYTES + 100];
    log.write(Stream::Stdout, &wide[..10_000]).unwrap();
    log.write(Stream::Stdout, &wide[10_000..]).unwrap();
    log.write(Stream::Stdout, b"\nnext\n").unwrap();
    let (_, lines, summary) = ended(log, End::new(CheckOutcome::Passed));
    assert_eq!(lines[1].text.len(), MAX_CHECK_LOG_LINE_BYTES);
    assert_eq!(lines[2].text, "next");
    assert_eq!(summary.dropped_bytes, 100);
}

/// Past the log's bound: one `cut` line, later output counted and not written, the `end` line
/// still written with the count.
#[test]
fn the_log_s_bound_cuts_once_and_counts() {
    let mut log = LineLog::bounded(Vec::new(), &start(), 0, 400).unwrap();
    for n in 0..20 {
        log.write(
            Stream::Stdout,
            format!("line {n:02} of output\n").as_bytes(),
        )
        .unwrap();
    }
    let (text, lines, summary) = ended(log, End::new(CheckOutcome::Passed));
    let cuts: Vec<&Line> = lines.iter().filter(|l| l.event("cut").is_some()).collect();
    assert_eq!(cuts.len(), 1, "{text}");
    assert_eq!(cuts[0].event("cut").unwrap()["limit"], 400);
    let kept = lines.iter().filter(|l| l.kind == Kind::Out).count();
    assert!(kept < 20, "{text}");
    assert_eq!(
        summary.dropped_bytes,
        (20 - kept as u64) * "line 00 of output\n".len() as u64
    );
    let end = lines.last().unwrap().event("end").unwrap();
    assert_eq!(end["dropped_bytes"], summary.dropped_bytes);
}

/// A line not in the format, as a check could write into its own log, reads as `other`, whole.
#[test]
fn a_line_out_of_format_reads_as_other() {
    let text =
        "    0.001 out fine\nnot a line\n    0.002 bad kind\n0.1 out short\n   12.345 err \n";
    let lines = parse(text);
    let kinds: Vec<Kind> = lines.iter().map(|l| l.kind).collect();
    assert_eq!(
        kinds,
        [Kind::Out, Kind::Other, Kind::Other, Kind::Other, Kind::Err]
    );
    assert_eq!(
        (lines[1].at_ms, lines[1].text.as_str()),
        (None, "not a line")
    );
    assert_eq!((lines[4].at_ms, lines[4].text.as_str()), (Some(12_345), ""));
    assert_eq!(lines[4].n, 5);
    let wide = parse("123456.789 run end {}");
    assert_eq!(wide[0].at_ms, Some(123_456_789));
}

/// Two spellings of one directory, canonicalized, give one key of 32 hex digits; another
/// directory another.
#[test]
fn the_dir_key_is_the_canonical_path_s() {
    let tmp = tempfile::tempdir().unwrap();
    let root = camino::Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
    std::fs::create_dir_all(root.join("a/b")).unwrap();
    let canonical = |p: &camino::Utf8Path| {
        camino::Utf8PathBuf::from_path_buf(std::fs::canonicalize(p).unwrap()).unwrap()
    };
    let one = dir_key(&canonical(&root.join("a")));
    assert_eq!(one, dir_key(&canonical(&root.join("a/b/.."))));
    assert_eq!(one, dir_key(&canonical(&root.join("a/./"))));
    assert_ne!(one, dir_key(&canonical(&root.join("a/b"))));
    assert_eq!(one.len(), 32);
    assert!(
        one.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    );
}
