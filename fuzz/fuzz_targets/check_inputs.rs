//! A check's declared inputs (M30-3): the reader of its patterns never panics, refuses only at
//! a pattern the list holds, in one short line, and what it reads matches a path exactly when a
//! matcher written here from the grammar's words says so. The input is lines:
//!
//! ```text
//! <a pattern>\n … <a pattern>\n<a path>          (one line alone: a path and no pattern)
//!
//! accepted: at most 256 patterns of at most 4,096 bytes each, none twice, every segment a
//!           name a repository path may hold, `*`, or a last `**`; read the same in any order;
//!           no pattern is the whole tree, which every path matches
//! refused:  at the first pattern at fault, the ones before it accepted; a message of one line
//!           and at most 512 bytes
//! ```
#![no_main]

use libfuzzer_sys::fuzz_target;
use swarmops_ops::check::inputs;
use swarmops_ops::check::{MAX_CHECK_INPUTS, MAX_INPUT_PATTERN_BYTES};

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let mut lines: Vec<&str> = text.split('\n').collect();
    let path = lines.pop().unwrap_or_default();
    let patterns: Vec<String> = lines.into_iter().map(str::to_owned).collect();
    match inputs::parse(&patterns) {
        Ok(read) => {
            accepted(&patterns);
            assert_eq!(read.len(), patterns.len());
            assert_eq!(read.is_empty(), patterns.is_empty());
            let expected = patterns.is_empty() || patterns.iter().any(|p| matches(p, path));
            assert_eq!(read.matches(path), expected, "{patterns:?} {path:?}");

            // The order the patterns are given in is no part of what they are.
            let mut reversed = patterns.clone();
            reversed.reverse();
            assert_eq!(inputs::parse(&reversed).ok().as_ref(), Some(&read));
        }
        Err((at, what)) => {
            assert!(at < patterns.len(), "{at} of {}: {what}", patterns.len());
            assert!(!what.contains('\n'), "{what:?}");
            assert!(what.len() <= 512, "{} bytes", what.len());
            assert!(inputs::parse(&patterns[..at]).is_ok(), "{patterns:?}");
            assert!(inputs::parse(&patterns[..=at]).is_err(), "{patterns:?}");
        }
    }
});

/// `patterns` keep the rules of a check's `inputs`.
fn accepted(patterns: &[String]) {
    assert!(patterns.len() <= MAX_CHECK_INPUTS, "{}", patterns.len());
    for (at, pattern) in patterns.iter().enumerate() {
        assert!(pattern.len() <= MAX_INPUT_PATTERN_BYTES, "{pattern:?}");
        assert!(!patterns[..at].contains(pattern), "{pattern:?} twice");
        assert!(!pattern.contains('\0'), "{pattern:?}");
        let segments: Vec<&str> = pattern.split('/').collect();
        let last = segments.len() - 1;
        for (i, segment) in segments.iter().enumerate() {
            assert!(!matches!(*segment, "" | "." | ".."), "{pattern:?}");
            let wild = *segment == "*" || (*segment == "**" && i == last);
            assert!(wild || !segment.contains('*'), "{pattern:?}");
        }
    }
}

/// Whether `pattern` matches `path`, by the grammar's words.
fn matches(pattern: &str, path: &str) -> bool {
    let pattern: Vec<&str> = pattern.split('/').collect();
    let path: Vec<&str> = path.split('/').collect();
    below(&pattern, &path)
}

/// Whether the segments `pattern` match the segments `path`: both at their end, a last `**`
/// with one segment or more left, or a first segment that is `*` or the path's own, and the
/// rest matching the rest.
fn below(pattern: &[&str], path: &[&str]) -> bool {
    match (pattern, path) {
        ([], []) | (["**"], [_, ..]) => true,
        ([first, pattern @ ..], [name, path @ ..]) => {
            (*first == "*" || first == name) && below(pattern, path)
        }
        _ => false,
    }
}
