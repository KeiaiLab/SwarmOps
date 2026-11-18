//! A check's JUnit report (M31-5): its reader never panics on any bytes, refuses with a fault of
//! its closed set or gives counts that hold together and findings within their rules, and never
//! reads a document that opens with a document type. The first input byte picks the check's
//! directory (even: the root, odd: `crates/app`); the rest is the report:
//!
//! ```text
//! read: failed + skipped ≤ tests; a finding per failed case, level error; each title one line,
//!       not empty, within 200 bytes; a group one line within 200; a message within 4 KiB; a path
//!       a repository path below the check's directory, a line only with it, from 1 to 2^32
//!       kept: keeping one finding keeps the first, with the same counts
//!       a document that opens with <!DOCTYPE (after a declaration) is never read
//! the same bytes read the same twice
//! ```
#![no_main]

use libfuzzer_sys::fuzz_target;
use swarmops_api::{CheckFindingLevel, CheckFindingOut};
use swarmops_core::RepoPath;
use swarmops_ops::check::findings::MAX_FINDING_LINE;
use swarmops_ops::check::junit;
use swarmops_ops::check::{
    MAX_FINDING_GROUP_BYTES, MAX_FINDING_MESSAGE_BYTES, MAX_FINDING_PATH_BYTES,
    MAX_FINDING_TITLE_BYTES,
};

fuzz_target!(|data: &[u8]| {
    let Some((&mode, report)) = data.split_first() else {
        return;
    };
    let dir = if mode & 1 == 0 { "." } else { "crates/app" };
    let read = junit::read(report, dir);
    assert_eq!(read, junit::read(report, dir));
    let Ok(read) = read else {
        return;
    };

    // 1. The counts, and a finding per failed case.
    assert!(read.failed + read.skipped <= read.tests, "{read:?}");
    assert_eq!(read.findings.len() as u64, read.failed, "{read:?}");
    let kept = junit::read_keeping(report, dir, 1).expect("it read once");
    assert_eq!(
        (kept.tests, kept.failed, kept.skipped),
        (read.tests, read.failed, read.skipped)
    );
    assert_eq!(
        kept.findings.as_slice(),
        &read.findings[..read.findings.len().min(1)]
    );

    // 2. Each finding within its rules.
    for finding in &read.findings {
        assert_eq!(finding.level, CheckFindingLevel::Error);
        within(finding, dir);
    }

    // 3. A document type, which could define entities, is never read.
    let text = String::from_utf8_lossy(report);
    let mut head = text.trim_start_matches('\u{feff}').trim_start();
    if head.starts_with("<?xml")
        && let Some(end) = head.find("?>")
    {
        head = head[end + 2..].trim_start();
    }
    assert!(!head.starts_with("<!DOCTYPE"), "a document type was read");
});

/// `finding`, of a check in `dir`, keeps every rule of a finding.
fn within(finding: &CheckFindingOut, dir: &str) {
    let title = &finding.title;
    assert!(
        !title.trim().is_empty() && title.len() <= MAX_FINDING_TITLE_BYTES,
        "{title:?}"
    );
    assert!(!title.contains(['\n', '\r']), "{title:?}");
    if let Some(group) = &finding.group {
        assert!(
            !group.is_empty() && group.len() <= MAX_FINDING_GROUP_BYTES,
            "{group:?}"
        );
        assert!(!group.contains(['\n', '\r']), "{group:?}");
    }
    if let Some(message) = &finding.message {
        assert!(!message.is_empty() && message.len() <= MAX_FINDING_MESSAGE_BYTES);
    }
    if let Some(path) = &finding.path {
        assert!(path.len() <= MAX_FINDING_PATH_BYTES);
        assert!(RepoPath::new(path.as_str()).is_ok(), "{path:?}");
        assert!(
            dir == "." || path.starts_with(&format!("{dir}/")),
            "{path:?}"
        );
    }
    assert!(
        finding.line.is_none() || finding.path.is_some(),
        "{finding:?}"
    );
    if let Some(line) = finding.line {
        assert!((1..=MAX_FINDING_LINE).contains(&line), "{finding:?}");
    }
    assert!(finding.end_line.is_none(), "{finding:?}");
    assert_eq!(finding.report, 0);
}
