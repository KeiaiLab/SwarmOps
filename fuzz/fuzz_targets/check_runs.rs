//! The run lines' reader (M31-12): `check runs` never panics on any bytes of its two files,
//! gives only lines a run writes, newest first, within the page and below the cursor, and its
//! pages walked to the end are the whole. The input:
//!
//! ```text
//! <byte: the page's limit, 1 + byte % 7><a cursor, or nothing>\n
//! <the older file's bytes>\0\n<the current file's bytes>      (no \0\n: the current file alone)
//! ```
//!
//! A cursor parses or is refused, and one that parses reads back from what it writes. A page
//! holds at most the limit, each line valid, strictly descending by key, each below the cursor's
//! key, with a cursor only when it is full; it counts no fewer lines read than skipped; reading
//! twice gives the same. From no cursor, the pages of the limit walked to the end are, in order,
//! the one page of 1,000, when that page is the whole: nothing twice, nothing lost.
#![no_main]

use std::io::Cursor as Bytes;

use libfuzzer_sys::fuzz_target;
use swarmops_api::{CheckRunLine, CheckRunsReq, MAX_CHECK_RUNS_LIMIT};
use swarmops_ops::check::lines;
use swarmops_ops::check::runs::{Cursor, Filter, Page, page};

/// The most limits a page of the target takes.
const LIMITS: u8 = 7;

/// What orders the lines.
type Key = (u64, String, String, u32);

fuzz_target!(|data: &[u8]| {
    // 1. The limit, the cursor's text, and the two files.
    let Some((&first, rest)) = data.split_first() else {
        return;
    };
    let limit = usize::from(first % LIMITS) + 1;
    let (head, files) = match rest.iter().position(|&b| b == b'\n') {
        Some(end) => (&rest[..end], &rest[end + 1..]),
        None => (rest, &[][..]),
    };
    let (older, current) = match files.windows(2).position(|pair| pair == b"\0\n") {
        Some(at) => (&files[..at], &files[at + 2..]),
        None => (&[][..], files),
    };

    // 2. The cursor: refused, or read back from what it writes.
    let text = String::from_utf8_lossy(head);
    let below = if text.is_empty() {
        None
    } else {
        match Cursor::parse(&text) {
            Ok(cursor) => {
                let written = cursor.to_string();
                assert_eq!(Cursor::parse(&written).ok(), Some(cursor), "{written}");
                Some(written)
            }
            Err(refused) => {
                assert_eq!(refused.details["reason"], "cursor", "{refused:?}");
                return;
            }
        }
    };

    // 3. One page from the cursor: within its rules, and the same twice.
    let read = read_page(older, current, limit, below.as_deref());
    assert_eq!(read, read_page(older, current, limit, below.as_deref()));
    holds(&read, limit, below.as_deref());

    // 4. From no cursor, the pages walked to the end are the one page of 1,000.
    let whole = read_page(older, current, MAX_CHECK_RUNS_LIMIT, None);
    holds(&whole, MAX_CHECK_RUNS_LIMIT, None);
    if whole.cursor.is_some() {
        return;
    }
    let mut walked: Vec<CheckRunLine> = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let next = read_page(older, current, limit, cursor.as_deref());
        holds(&next, limit, cursor.as_deref());
        assert_eq!((next.scanned, next.skipped), (whole.scanned, whole.skipped));
        walked.extend(next.runs);
        assert!(
            walked.len() <= whole.runs.len(),
            "a line twice, or one never there"
        );
        cursor = next.cursor;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(walked, whole.runs, "limit {limit}");
});

/// The page of `limit` lines of the two files below `cursor`, with no filter.
fn read_page(older: &[u8], current: &[u8], limit: usize, cursor: Option<&str>) -> Page {
    let mut req = CheckRunsReq::new("w");
    req.limit = limit;
    req.cursor = cursor.map(str::to_owned);
    let (filter, limit) = Filter::of(&req).expect("a limit of 1 to 1,000 and a cursor that parsed");
    page(Bytes::new(older), Bytes::new(current), &filter, limit)
}

/// `read`, a page of at most `limit` lines below `cursor`, keeps its rules.
fn holds(read: &Page, limit: usize, cursor: Option<&str>) {
    // 1. Within the page; a cursor only on a full page, naming its last line.
    assert!(read.runs.len() <= limit, "{} of {limit}", read.runs.len());
    assert!(read.scanned >= read.skipped, "{read:?}");
    match (&read.cursor, read.runs.last()) {
        (Some(next), Some(last)) => {
            assert_eq!(read.runs.len(), limit, "a cursor on a page not full");
            assert_eq!(key_of_cursor(next), key_of(last), "{next}");
        }
        (Some(next), None) => panic!("a cursor {next} on an empty page"),
        (None, _) => {}
    }

    // 2. Each line one a run writes, strictly newer than the next, and below the cursor.
    let below = cursor.map(key_of_cursor);
    for line in &read.runs {
        assert!(lines::valid(line), "{line:?}");
        assert!(below.as_ref().is_none_or(|below| key_of(line) < *below));
    }
    for pair in read.runs.windows(2) {
        assert!(key_of(&pair[0]) > key_of(&pair[1]), "{pair:?}");
    }
}

/// The key of `line`.
fn key_of(line: &CheckRunLine) -> Key {
    (line.at, line.run.clone(), line.check.clone(), line.attempt)
}

/// The key the cursor `text` names: `r1.<at>.<run>.<attempt>.<check>`, as it is written.
fn key_of_cursor(text: &str) -> Key {
    let parts: Vec<&str> = text.split('.').collect();
    let [_, at, run, attempt, check] = parts[..] else {
        panic!("a cursor of other parts: {text}");
    };
    (
        at.parse().expect("its time"),
        run.to_owned(),
        check.to_owned(),
        attempt.parse().expect("its attempt"),
    )
}
