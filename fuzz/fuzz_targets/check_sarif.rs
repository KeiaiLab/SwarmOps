//! A check's SARIF report (M31-5): its reader never panics on any bytes, refuses with a fault of
//! its closed set or gives counts that hold together and findings within their rules. The first
//! input byte picks where the check runs (even: the root, odd: `crates/app`, with `/work/tree/
//! crates/app` as its directory on the host); the rest is the report:
//!
//! ```text
//! read: findings ≤ results, one per result of kind fail; each title one line, not empty, within
//!       200 bytes; a group one line within 200; a message within 4 KiB; a path a repository
//!       path below the check's directory; a line only with it, an end line only with a line
//!       and not before it, both from 1 to 2^32
//!       kept: keeping one finding keeps the first, with the same counts
//! the same bytes read the same twice
//! ```
#![no_main]

use libfuzzer_sys::fuzz_target;
use swarmops_core::RepoPath;
use swarmops_ops::check::findings::MAX_FINDING_LINE;
use swarmops_ops::check::sarif::{self, Place};
use swarmops_ops::check::{
    MAX_FINDING_GROUP_BYTES, MAX_FINDING_MESSAGE_BYTES, MAX_FINDING_PATH_BYTES,
    MAX_FINDING_TITLE_BYTES,
};

fuzz_target!(|data: &[u8]| {
    let Some((&mode, report)) = data.split_first() else {
        return;
    };
    let place = if mode & 1 == 0 {
        Place::new(".", None)
    } else {
        Place::new("crates/app", Some("/work/tree/crates/app"))
    };
    let read = sarif::read(report, place);
    assert_eq!(read, sarif::read(report, place));
    let Ok(read) = read else {
        return;
    };

    // 1. The counts.
    assert!(read.found <= read.results, "{read:?}");
    assert_eq!(read.findings.len() as u64, read.found, "{read:?}");
    let kept = sarif::read_keeping(report, place, 1).expect("it read once");
    assert_eq!((kept.found, kept.results), (read.found, read.results));
    assert_eq!(
        kept.findings.as_slice(),
        &read.findings[..read.findings.len().min(1)]
    );

    // 2. Each finding within its rules.
    for finding in &read.findings {
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
        }
        if let Some(message) = &finding.message {
            assert!(!message.is_empty() && message.len() <= MAX_FINDING_MESSAGE_BYTES);
        }
        if let Some(path) = &finding.path {
            assert!(path.len() <= MAX_FINDING_PATH_BYTES);
            assert!(RepoPath::new(path.as_str()).is_ok(), "{path:?}");
            let dir = place.dir;
            assert!(
                dir == "." || path.starts_with(&format!("{dir}/")),
                "{path:?}"
            );
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
            assert!((1..=MAX_FINDING_LINE).contains(&line), "{finding:?}");
        }
        if let (Some(line), Some(end)) = (finding.line, finding.end_line) {
            assert!(line <= end && end <= MAX_FINDING_LINE, "{finding:?}");
        }
        assert_eq!(finding.report, 0);
    }
});
