//! A check's report file (M31-4): its reader never panics on any bytes, accounts for every line
//! it reads, and gives only findings within their rules. The first input byte picks the bound:
//!
//! ```text
//! byte & 1 == 0 ─▶ the rest read as a report file, under its 64 MiB bound
//! byte & 1 == 1 ─▶ the rest read under a bound of (byte >> 1) * 8 bytes, which cuts it
//! ```
//!
//! Every line read is a finding, an invalid line or a blank one; every finding has a title of one
//! line within its bound, a message within its bound, a repository path within its bound, a
//! `line` only with a path and an `end_line` only with a `line` and not before it; the first
//! invalid line is named exactly when there is one; reading twice gives the same.
#![no_main]

use libfuzzer_sys::fuzz_target;
use swarmops_core::RepoPath;
use swarmops_ops::check::findings::{self, FileRead, MAX_FINDING_LINE};
use swarmops_ops::check::{
    MAX_FINDING_MESSAGE_BYTES, MAX_FINDING_PATH_BYTES, MAX_FINDING_TITLE_BYTES,
};

fuzz_target!(|data: &[u8]| {
    let Some((&mode, rest)) = data.split_first() else {
        return;
    };
    let (read, again, within) = if mode & 1 == 0 {
        (
            findings::read_report_file(rest),
            findings::read_report_file(rest),
            rest,
        )
    } else {
        let limit = usize::from(mode >> 1) * 8;
        let read = findings::read_bounded(rest, limit, usize::MAX);
        assert_eq!(read.cut, rest.len() > limit);
        let within = if read.cut {
            let head = &rest[..limit];
            &head[..head
                .iter()
                .rposition(|&b| b == b'\n')
                .map_or(0, |at| at + 1)]
        } else {
            rest
        };
        (
            read,
            findings::read_bounded(rest, limit, usize::MAX),
            within,
        )
    };
    assert_eq!(read, again);
    holds(&read, within);
});

/// `read` keeps its rules, `within` being the bytes it read.
fn holds(read: &FileRead, within: &[u8]) {
    // 1. Every line accounted for: as many as `within` holds, the last one needing no `\n`.
    let segments = within.split(|&b| b == b'\n').count();
    let lines = if within.ends_with(b"\n") || within.is_empty() {
        segments - 1
    } else {
        segments
    };
    assert_eq!(read.lines, lines as u64);
    let found = read.findings.len() as u64;
    assert_eq!(found, read.found, "{read:?}");
    assert_eq!(found + read.invalid + read.blank, read.lines, "{read:?}");
    assert_eq!(read.first_invalid.is_some(), read.invalid > 0, "{read:?}");

    // 2. Each finding within its rules.
    for finding in &read.findings {
        let title = &finding.title;
        assert!(!title.trim().is_empty() && title.len() <= MAX_FINDING_TITLE_BYTES);
        assert!(!title.contains(['\n', '\r']), "{title:?}");
        if let Some(message) = &finding.message {
            assert!(!message.is_empty() && message.len() <= MAX_FINDING_MESSAGE_BYTES);
        }
        if let Some(path) = &finding.path {
            assert!(path.len() <= MAX_FINDING_PATH_BYTES);
            assert!(RepoPath::new(path.as_str()).is_ok(), "{path:?}");
        }
        assert!(
            finding.line.is_none() || finding.path.is_some(),
            "{finding:?}"
        );
        assert!(
            finding.end_line.is_none() || finding.line.is_some(),
            "{finding:?}"
        );
        if let Some(line) = finding.line {
            assert!((1..=MAX_FINDING_LINE).contains(&line));
        }
        if let (Some(line), Some(end)) = (finding.line, finding.end_line) {
            assert!(line <= end && end <= MAX_FINDING_LINE, "{finding:?}");
        }
        assert!(
            finding.group.is_none() && finding.report == 0,
            "{finding:?}"
        );
    }
}
