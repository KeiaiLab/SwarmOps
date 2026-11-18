//! A check's log (M31-3): its writer never panics, writes only lines its reader reads back in
//! the format, holds its bounds and accounts for what it leaves out; its reader never panics on
//! any bytes, as a check writing into its own log could leave. The first input byte picks the
//! mode and the log's bound; the rest is the output:
//!
//! ```text
//! byte & 1 == 0 ─▶ the writer: the rest cut into chunks, each to stdout or stderr by a byte of
//!                  its own, into a LineLog bounded at 256..=16,383 bytes (byte >> 1 picks it);
//!                  then read back: start first, end last, every line in the format, sections
//!                  in order and within their bounds, a cut line at most once and only past
//!                  the bound, nothing dropped unless a line or the log was cut
//! byte & 1 == 1 ─▶ the reader: the rest parsed as a log, lines numbered from 1
//! ```
#![no_main]

use libfuzzer_sys::fuzz_target;
use swarmops_api::CheckOutcome;
use swarmops_ops::check::log::{
    self, End, Kind, LineLog, MAX_CHECK_LOG_LINE_BYTES, MAX_CHECK_SECTIONS,
    MAX_SECTION_TITLE_BYTES, Output, Start, Stream,
};

fuzz_target!(|data: &[u8]| {
    let Some((&mode, rest)) = data.split_first() else {
        return;
    };
    if mode & 1 == 1 {
        read(rest);
        return;
    }
    write(rest, 256 + u64::from(mode >> 1) * 128);
});

/// The reader alone: any text parses, every line numbered, a line of `other` kept whole.
fn read(bytes: &[u8]) {
    let text = String::from_utf8_lossy(bytes);
    let lines = log::parse(&text);
    assert_eq!(lines.len(), text.lines().count());
    for (at, line) in lines.iter().enumerate() {
        assert_eq!(line.n, at as u64 + 1);
        assert_eq!(line.at_ms.is_none(), line.kind == Kind::Other, "{line:?}");
    }
}

/// The writer, `bytes` cut into chunks: each chunk a length byte, a stream byte, its bytes.
fn write(bytes: &[u8], limit: u64) {
    // 1. The output, chunk by chunk, into a log bounded at `limit`.
    let start = Start::new(
        "fuzz",
        "0193f1e2-0000-7000-8000-000000000000",
        "before-land",
    );
    let mut writer = LineLog::bounded(Vec::new(), &start, 0, limit).expect("memory takes it");
    let mut rest = bytes;
    let mut markers = 0usize;
    while let [len, stream, tail @ ..] = rest {
        let take = usize::from(*len).min(tail.len());
        let stream = if stream & 1 == 0 {
            Stream::Stdout
        } else {
            Stream::Stderr
        };
        markers += tail[..take]
            .windows(17)
            .filter(|w| *w == b"swarmops:section ")
            .count();
        writer
            .write(stream, &tail[..take])
            .expect("memory takes it");
        rest = &tail[take..];
    }
    let (written, summary) = writer
        .finish(End::new(CheckOutcome::Passed))
        .expect("memory takes it");

    // 2. Read back: start first, end last, every line in the format.
    let text = String::from_utf8(written).expect("the log is UTF-8");
    let lines = log::parse(&text);
    assert!(lines.len() >= 2, "{text:?}");
    assert!(lines[0].event("start").is_some(), "{text:?}");
    let end = lines
        .last()
        .and_then(|line| line.event("end"))
        .expect("an end line");
    assert!(
        lines.iter().all(|line| line.kind != Kind::Other),
        "{text:?}"
    );
    assert!(text.ends_with('\n'));

    // 3. Sections: in order, numbered, within their bounds, the end line's the same.
    assert!(summary.sections.len() <= MAX_CHECK_SECTIONS.min(markers));
    for pair in summary.sections.windows(2) {
        assert!(pair[0].start_ms <= pair[1].start_ms, "{pair:?}");
        assert_eq!(
            pair[0].start_ms + pair[0].duration_ms,
            pair[1].start_ms,
            "{pair:?}"
        );
    }
    for section in &summary.sections {
        assert!(!section.title.is_empty() && section.title.len() <= MAX_SECTION_TITLE_BYTES);
    }
    assert_eq!(
        end["sections"].as_array().map(Vec::len),
        Some(summary.sections.len())
    );

    // 4. The bound: a cut line at most once, and nothing dropped without a cut or a long line.
    let cuts = lines
        .iter()
        .filter(|line| line.event("cut").is_some())
        .count();
    assert!(cuts <= 1, "{text:?}");
    for line in lines.iter().filter(|l| l.kind != Kind::Run) {
        assert!(line.text.len() <= MAX_CHECK_LOG_LINE_BYTES * 3, "{line:?}");
    }
    let over = bytes
        .split(|b| *b == b'\n')
        .any(|line| line.len() > MAX_CHECK_LOG_LINE_BYTES);
    if summary.dropped_bytes > 0 {
        assert!(
            cuts == 1 || over,
            "dropped {} with no cut: {text:?}",
            summary.dropped_bytes
        );
    }
    assert_eq!(end["dropped_bytes"].as_u64(), Some(summary.dropped_bytes));
}
