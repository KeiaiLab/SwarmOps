//! `check runs`: the filters, the order, the page and its cursor, the lines that do not read,
//! and the files.

use std::io::{BufReader, Cursor as Bytes};

use camino::Utf8PathBuf;
use swarmops_api::{CheckOutcome, CheckRunLine, CheckRunsReq, CheckStage, OpError};

use super::*;
use crate::check::lines::{RUN_LINES_FILE, RUN_LINES_OLDER_FILE};

/// The fixture the binary's tests copy: twelve lines of three runs and four checks, two more of
/// keys already seen, then a foreign object, a `not_run` line and a torn last line.
const FIXTURE: &[u8] =
    include_bytes!("../../../../swarmops-cli/tests/fixtures/check/run-lines.jsonl");

/// The fixture's three runs, oldest first.
const R1: &str = "0193f1e2-0000-7000-8000-0000000000a1";
const R2: &str = "0193f1e2-0000-7000-8000-0000000000a2";
const R3: &str = "0193f1e2-0000-7000-8000-0000000000a3";

/// When the fixture's first check started.
const T0: u64 = 1_700_000_000_000;

/// The fixture's lines, newest first, as `(run, check)`.
const NEWEST_FIRST: [(&str, &str); 12] = [
    (R3, "unit"),
    (R3, "test"),
    (R3, "lint"),
    (R3, "build"),
    (R2, "unit"),
    (R2, "test"),
    (R2, "lint"),
    (R2, "build"),
    (R1, "unit"),
    (R1, "test"),
    (R1, "lint"),
    (R1, "build"),
];

/// A request with no filter and the largest page.
fn all() -> CheckRunsReq {
    let mut req = CheckRunsReq::new("w");
    req.limit = MAX_CHECK_RUNS_LIMIT;
    req
}

/// The page `req` gives over `current` alone.
fn page_of(current: &[u8], req: &CheckRunsReq) -> Page {
    paged(&[], current, req)
}

/// The page `req` gives over the two files.
fn paged(older: &[u8], current: &[u8], req: &CheckRunsReq) -> Page {
    let (filter, limit) = Filter::of(req).unwrap();
    page(Bytes::new(older), Bytes::new(current), &filter, limit)
}

/// `(run, check)` of each line.
fn named(lines: &[CheckRunLine]) -> Vec<(&str, &str)> {
    (lines.iter())
        .map(|line| (line.run.as_str(), line.check.as_str()))
        .collect()
}

/// The refusal of `req`: its `details.reason`.
fn reason(req: &CheckRunsReq) -> String {
    let error: OpError = Filter::of(req).expect_err("refused");
    assert_eq!(error.code, "E_INVALID_ARG", "{error:?}");
    error.details["reason"].as_str().unwrap().to_owned()
}

/// A line of `run` and `check` at `at`.
fn line(at: u64, run: &str, check: &str) -> String {
    let line = CheckRunLine::new(at, run, check, CheckStage::BeforeLand, CheckOutcome::Passed);
    serde_json::to_string(&line).unwrap() + "\n"
}

/// The whole fixture: twelve lines newest first by their key, whatever the file's order, each
/// key once (the first read stands), three lines skipped, no cursor.
#[test]
fn the_fixture_reads_newest_first_by_key() {
    let whole = page_of(FIXTURE, &all());
    assert_eq!(named(&whole.runs), NEWEST_FIRST);
    assert_eq!((whole.scanned, whole.skipped), (17, 3));
    assert_eq!(whole.cursor, None);

    // The file holds R1's `unit` before its `test`, which started first.
    let file = String::from_utf8_lossy(FIXTURE);
    let of_unit = file.find(r#""check":"unit""#).unwrap();
    let of_test = file.find(r#""check":"test""#).unwrap();
    assert!(of_unit < of_test, "the fixture's swapped lines");

    // A key seen twice is the first read: R1's `lint` took 800 ms, though a later line of the
    // same key says 801.
    let lint = &whole.runs[10];
    assert_eq!(
        (lint.outcome, lint.duration_ms),
        (CheckOutcome::Failed, 800)
    );
    assert_eq!(
        file.matches(&format!(
            r#""at":{},"run":"{R1}","check":"lint""#,
            T0 + 1_200
        ))
        .count(),
        2
    );
}

/// Each filter alone, and two together.
#[test]
fn each_filter_gives_its_lines() {
    // 1. A check, and two.
    let mut req = all();
    req.checks = vec!["lint".to_owned()];
    assert_eq!(
        named(&page_of(FIXTURE, &req).runs),
        [(R3, "lint"), (R2, "lint"), (R1, "lint")]
    );
    req.checks.push("unit".to_owned());
    assert_eq!(page_of(FIXTURE, &req).runs.len(), 6);

    // 2. A tree, by a prefix of 12 digits and by all 64: a line of no tree never matches.
    let mut req = all();
    req.tree = Some("bbbbbbbbbbbb".to_owned());
    let of_tree = page_of(FIXTURE, &req);
    assert_eq!(of_tree.runs.len(), 4, "{:?}", named(&of_tree.runs));
    assert!(of_tree.runs.iter().all(|line| line.run == R2));
    req.tree = Some(format!("aaaaaaaaaaaa{}", "0".repeat(52)));
    assert!(
        page_of(FIXTURE, &req)
            .runs
            .iter()
            .all(|line| line.run == R1)
    );
    req.tree = Some("aaaaaaaaaaaa1".to_owned());
    assert_eq!(page_of(FIXTURE, &req).runs.len(), 0, "no such tree");

    // 3. An outcome, and two.
    let mut req = all();
    req.outcomes = vec![CheckOutcome::Failed];
    assert_eq!(
        named(&page_of(FIXTURE, &req).runs),
        [(R3, "test"), (R1, "lint")]
    );
    req.outcomes = vec![
        CheckOutcome::TimedOut,
        CheckOutcome::Cancelled,
        CheckOutcome::NotStarted,
    ];
    assert_eq!(
        named(&page_of(FIXTURE, &req).runs),
        [(R3, "build"), (R2, "unit"), (R1, "test")]
    );

    // 4. A run.
    let mut req = all();
    req.run = Some(R3.to_owned());
    assert_eq!(named(&page_of(FIXTURE, &req).runs), NEWEST_FIRST[..4]);

    // 5. Two together: the checks named, of the outcome named.
    let mut req = all();
    req.checks = vec!["test".to_owned(), "lint".to_owned()];
    req.outcomes = vec![CheckOutcome::Passed];
    assert_eq!(
        named(&page_of(FIXTURE, &req).runs),
        [(R3, "lint"), (R2, "test"), (R2, "lint")]
    );
    assert_eq!(
        page_of(FIXTURE, &req).skipped,
        3,
        "skipped whatever matches"
    );
}

/// A window holds the lines that started at or after `since` and before `until`.
#[test]
fn a_window_takes_its_start_and_leaves_its_end() {
    let mut req = all();
    req.since = Some(T0 + 100_000);
    req.until = Some(T0 + 200_000);
    assert_eq!(
        named(&page_of(FIXTURE, &req).runs),
        NEWEST_FIRST[4..8],
        "R2 alone"
    );
    req.until = Some(T0 + 200_001);
    assert_eq!(
        page_of(FIXTURE, &req).runs.len(),
        5,
        "R3's first, at the end"
    );
    req.since = Some(T0 + 100_001);
    assert_eq!(
        page_of(FIXTURE, &req).runs.len(),
        4,
        "R2's first, before the start"
    );
    req.until = req.since;
    assert_eq!(page_of(FIXTURE, &req).runs.len(), 0, "an empty window");
    let mut only = all();
    only.until = Some(T0 + 1);
    assert_eq!(named(&page_of(FIXTURE, &only).runs), [(R1, "build")]);
}

/// The order is the key's, whichever file holds a line and wherever in it; a key in both files
/// is one line.
#[test]
fn the_order_is_the_key_s_across_both_files() {
    let (a, b, c) = (
        line(1, R1, "unit"),
        line(2, R1, "lint"),
        line(3, R2, "unit"),
    );
    let newest_first = [(R2, "unit"), (R1, "lint"), (R1, "unit")];
    let files = [
        (format!("{a}{b}"), c.clone()),
        (c.clone(), format!("{b}{a}")),
        (format!("{c}{a}"), format!("{b}{a}{c}")),
        (String::new(), format!("{c}{b}{a}")),
    ];
    for (older, current) in &files {
        let got = paged(older.as_bytes(), current.as_bytes(), &all());
        assert_eq!(named(&got.runs), newest_first, "{older:?} then {current:?}");
    }

    // Equal times: by run, then by check, then by attempt.
    let ties = [line(5, R1, "b"), line(5, R2, "a"), line(5, R1, "a")].concat();
    let got = page_of(ties.as_bytes(), &all());
    assert_eq!(named(&got.runs), [(R2, "a"), (R1, "b"), (R1, "a")]);
    let mut second = CheckRunLine::new(5, R1, "a", CheckStage::BeforeLand, CheckOutcome::Failed);
    second.attempt = 2;
    let both = ties + &serde_json::to_string(&second).unwrap() + "\n";
    let got = page_of(both.as_bytes(), &all());
    assert_eq!(got.runs.len(), 4, "another attempt is another line");
    assert_eq!((got.runs[2].attempt, got.runs[3].attempt), (2, 1));
}

/// A cursor is its line's key, written and read back; one that does not parse is refused.
#[test]
fn a_cursor_reads_back_and_a_bad_one_is_refused() {
    // 1. The round trip, and the form.
    let first = {
        let mut req = all();
        req.limit = 1;
        page_of(FIXTURE, &req)
    };
    let text = first.cursor.clone().expect("more match");
    assert_eq!(text, format!("r1.{}.{R3}.1.unit", T0 + 203_000));
    let cursor = Cursor::parse(&text).unwrap();
    assert_eq!(cursor.to_string(), text);
    assert_eq!(Cursor::parse(&cursor.to_string()).unwrap(), cursor);

    // 2. Each refusal.
    let bad = [
        String::new(),
        "r1".to_owned(),
        format!("r2.1.{R1}.1.unit"),
        format!("r1.x.{R1}.1.unit"),
        format!("r1.+1.{R1}.1.unit"),
        format!("r1.1.{R1}.1.unit.more"),
        format!("r1.1.{R1}.unit"),
        "r1.1.not-a-run.1.unit".to_owned(),
        format!("r1.1.{}.1.unit", R1.to_uppercase()),
        format!("r1.1.{R1}.-1.unit"),
        format!("r1.1.{R1}.1.Unit"),
        format!("r1.1.{R1}.1."),
        format!("r1.99999999999999999999999.{R1}.1.unit"),
        format!("r1.1.{R1}.1.{}", "u".repeat(300)),
    ];
    for text in &bad {
        let mut req = all();
        req.cursor = Some(text.clone());
        assert_eq!(reason(&req), "cursor", "{text:?}");
    }
}

/// Pages of 1, 2 and 5 walked to the end give the whole, in order, nothing twice and nothing
/// lost; only the last page has no cursor.
#[test]
fn pages_walked_to_the_end_are_the_whole() {
    for limit in [1, 2, 5, 12, 13] {
        let mut req = all();
        req.limit = limit;
        let mut walked = Vec::new();
        let mut pages = 0;
        loop {
            let got = page_of(FIXTURE, &req);
            assert!(got.runs.len() <= limit, "limit {limit}");
            walked.extend(got.runs);
            pages += 1;
            assert!(pages <= 12, "limit {limit}: a cursor that does not move on");
            let Some(cursor) = got.cursor else { break };
            req.cursor = Some(cursor);
        }
        assert_eq!(named(&walked), NEWEST_FIRST, "limit {limit}");
        assert_eq!(
            pages,
            12_usize.div_ceil(limit),
            "limit {limit}: no empty last page"
        );
    }
}

/// Lines appended between two pages, and a rotation between them, repeat and drop nothing that
/// is still kept: the second page is what it would have been.
#[test]
fn appends_and_a_rotation_between_pages_change_no_later_page() {
    // 1. The first page of four, and the second as it is with nothing changed.
    let mut req = all();
    req.limit = 4;
    let first = page_of(FIXTURE, &req);
    req.cursor.clone_from(&first.cursor);
    let second = page_of(FIXTURE, &req);
    assert_eq!(named(&second.runs), NEWEST_FIRST[4..8]);

    // 2. A newer run appends its lines (the fixture's torn line ended first): the same page.
    let run4 = "0193f1e2-0000-7000-8000-0000000000a4";
    let appended = [FIXTURE, b"\n", line(T0 + 500_000, run4, "build").as_bytes()].concat();
    let after = page_of(&appended, &req);
    assert_eq!(
        after.runs, second.runs,
        "appended lines come on a first page only"
    );
    assert_eq!(after.cursor, second.cursor);
    let mut fresh = all();
    fresh.limit = 4;
    assert_eq!(
        page_of(&appended, &fresh).runs[0].run,
        run4,
        "a first page has them"
    );

    // 3. A rotation: the current file is now the older one, and new lines start another.
    let rotated = paged(FIXTURE, line(T0 + 500_000, run4, "build").as_bytes(), &req);
    assert_eq!(rotated.runs, second.runs, "nothing twice, nothing lost");
    assert_eq!(rotated.cursor, second.cursor);

    // 4. The lines the cursor names rotated away: the page goes on below where they were.
    let kept: Vec<u8> = (FIXTURE.split(|b| *b == b'\n'))
        .filter(|line| !String::from_utf8_lossy(line).contains(R3))
        .flat_map(|line| [line, b"\n"].concat())
        .collect();
    assert_eq!(named(&page_of(&kept, &req).runs), NEWEST_FIRST[4..8]);
}

/// A limit, a tree, a window, a check, an outcome or a run that does not read is refused with
/// its reason.
#[test]
fn a_filter_that_does_not_read_is_refused_with_its_reason() {
    let refused = |change: fn(&mut CheckRunsReq)| {
        let mut req = all();
        change(&mut req);
        reason(&req)
    };
    assert_eq!(refused(|req| req.limit = 0), "limit");
    assert_eq!(refused(|req| req.limit = MAX_CHECK_RUNS_LIMIT + 1), "limit");
    assert_eq!(
        refused(|req| req.tree = Some("abcdef01234".to_owned())),
        "tree"
    );
    assert_eq!(
        refused(|req| req.tree = Some("ABCDEF012345".to_owned())),
        "tree"
    );
    assert_eq!(refused(|req| req.tree = Some("a".repeat(65))), "tree");
    assert_eq!(
        refused(|req| req.tree = Some("abcdef01234g".to_owned())),
        "tree"
    );
    assert_eq!(
        refused(|req| {
            req.since = Some(2);
            req.until = Some(1);
        }),
        "time"
    );
    assert_eq!(refused(|req| req.checks = vec!["Lint".to_owned()]), "check");
    assert_eq!(
        refused(|req| req.outcomes = vec![CheckOutcome::NotRun]),
        "outcome"
    );
    assert_eq!(refused(|req| req.run = Some("run".to_owned())), "run");
    assert_eq!(
        (DEFAULT_CHECK_RUNS_LIMIT, MAX_CHECK_RUNS_LIMIT),
        (100, 1_000)
    );
}

/// Each kind of line that does not read is skipped and counted: one past 4 KiB, one that is not
/// UTF-8, not JSON, not a run line, one whose values break a rule, and a last line with no end.
#[test]
fn lines_that_do_not_read_are_skipped_and_counted() {
    let good = line(1, R1, "unit");
    let long = format!("{{\"pad\":\"{}\"}}\n", "x".repeat(5 << 10));
    let bad_run = line(2, "run", "unit");
    let relative = good.replace("\"outcome\"", "\"run_dir\":\"rel/dir\",\"outcome\"");
    let kinds: [(&str, Vec<u8>); 8] = [
        ("past 4 KiB", long.clone().into_bytes()),
        ("not UTF-8", b"{\"at\":1,\"run\":\"\xff\"}\n".to_vec()),
        ("not JSON", b"not json\n".to_vec()),
        ("an empty line", b"\n".to_vec()),
        ("a foreign object", b"{\"at\":1,\"op\":\"land\"}\n".to_vec()),
        ("a run that is no id", bad_run.into_bytes()),
        ("a relative run directory", relative.into_bytes()),
        (
            "a torn last line",
            good.trim_end().as_bytes()[..40].to_vec(),
        ),
    ];
    for (what, bytes) in &kinds {
        let file = [good.as_bytes(), bytes].concat();
        let got = page_of(&file, &all());
        assert_eq!(
            (got.runs.len(), got.scanned, got.skipped),
            (1, 2, 1),
            "{what}"
        );
    }

    // A line of exactly the bound reads; one byte more does not. Lines after a long one read.
    let fits = |len: usize| {
        let mut line =
            CheckRunLine::new(1, R1, "unit", CheckStage::BeforeLand, CheckOutcome::Passed);
        let base = serde_json::to_string(&line).unwrap().len() + r#","labels":[""]"#.len() + 1;
        line.labels = vec!["l".repeat(len - base)];
        serde_json::to_string(&line).unwrap() + "\n"
    };
    let (at, over) = (fits(MAX_RUN_LINE_BYTES), fits(MAX_RUN_LINE_BYTES + 1));
    assert_eq!(
        (at.len(), over.len()),
        (MAX_RUN_LINE_BYTES, MAX_RUN_LINE_BYTES + 1)
    );
    assert_eq!(page_of(at.as_bytes(), &all()).skipped, 0, "at the bound");
    assert_eq!(
        page_of(over.as_bytes(), &all()).skipped,
        1,
        "a byte past it"
    );
    let after = [long.as_bytes(), good.as_bytes()].concat();
    let got = page_of(&after, &all());
    assert_eq!(
        (got.runs.len(), got.skipped),
        (1, 1),
        "the line after a long one"
    );
}

/// A long line is skipped without being held: the reader, which fails a debug build if it ever
/// holds more than a line's bound, reads a line of 8 MiB through a small buffer.
#[test]
fn a_long_line_is_never_held() {
    let mut file = vec![b'x'; 8 << 20];
    file.push(b'\n');
    file.extend_from_slice(line(1, R1, "unit").as_bytes());
    let (filter, limit) = Filter::of(&all()).unwrap();
    for capacity in [1, 7, 4_096, 64 << 10] {
        let current = BufReader::with_capacity(capacity, Bytes::new(&file));
        let got = page(Bytes::new(&[][..]), current, &filter, limit);
        assert_eq!(
            (got.runs.len(), got.scanned, got.skipped),
            (1, 2, 1),
            "{capacity}"
        );
    }
}

/// The files of a directory: neither there is an empty page; both are read, the older first;
/// one that is no regular file is `E_IO` naming it; a lines directory that is a file is refused.
#[test]
fn the_files_of_a_directory_are_read() {
    // 1. No directory, then an empty one: an empty page.
    let tmp = tempfile::tempdir().unwrap();
    let base = Utf8PathBuf::from_path_buf(std::fs::canonicalize(tmp.path()).unwrap()).unwrap();
    let dir = base.join("lines");
    let none = runs_at(&dir, true, &all()).unwrap();
    assert_eq!((none.runs.len(), none.scanned, &none.cursor), (0, 0, &None));
    assert_eq!((none.dir.as_str(), none.vouched), (dir.as_str(), true));
    assert!(!dir.exists(), "reading makes nothing");
    std::fs::create_dir(&dir).unwrap();
    assert_eq!(runs_at(&dir, false, &all()).unwrap().runs.len(), 0);

    // 2. Both files, and the answer's fields.
    std::fs::write(dir.join(RUN_LINES_OLDER_FILE), line(1, R1, "unit")).unwrap();
    std::fs::write(dir.join(RUN_LINES_FILE), FIXTURE).unwrap();
    let mut req = all();
    req.limit = 12;
    let both = runs_at(&dir, false, &req).unwrap();
    assert_eq!((both.runs.len(), both.scanned, both.skipped), (12, 18, 3));
    assert_eq!(
        both.cursor.as_deref(),
        Some(format!("r1.{T0}.{R1}.1.build").as_str())
    );
    assert_eq!(both.source, swarmops_api::CheckRunsSource::RunLines);
    assert!(!both.vouched);

    // 3. A file that is a link, or a directory: E_IO, naming it.
    let current = dir.join(RUN_LINES_FILE);
    std::fs::remove_file(&current).unwrap();
    std::os::unix::fs::symlink(dir.join(RUN_LINES_OLDER_FILE), &current).unwrap();
    let linked = runs_at(&dir, false, &all()).unwrap_err();
    assert_eq!(
        (linked.code.as_str(), &linked.details["path"]),
        ("E_IO", &current.as_str().into())
    );
    std::fs::remove_file(&current).unwrap();
    std::fs::create_dir(&current).unwrap();
    let a_dir = runs_at(&dir, false, &all()).unwrap_err();
    assert_eq!(a_dir.code, "E_IO", "{a_dir:?}");

    // 4. A lines directory that is a file; a request that does not read, before any file.
    let file = base.join("a-file");
    std::fs::write(&file, "x").unwrap();
    let refused = runs_at(&file, true, &all()).unwrap_err();
    assert_eq!(refused.details["reason"], "run_lines_dir", "{refused:?}");
    let mut bad = all();
    bad.limit = 0;
    assert_eq!(
        runs_at(&dir, false, &bad).unwrap_err().details["reason"],
        "limit"
    );
}
