//! A check's declared inputs (M30-3): the path patterns of its `inputs`, in the access file's
//! grammar (`Pattern`), from the tree's root.
//!
//! ```text
//!   inputs = ["web/**", "docs/*", "CHANGELOG.md"]
//!
//!   web/**         web/app/page.tsx, web/package.json      every depth below web
//!   docs/*         docs/guide.md                           not docs/status/M1/a.txt
//!   CHANGELOG.md   CHANGELOG.md                            that entry alone
//!
//!   no `inputs`    every path: the check takes the whole tree
//! ```
//!
//! A pattern names paths as they are, byte for byte, whatever the check's `dir`. A path matches
//! a check's inputs when any pattern does; its *first match* is the first one in declared order.

use super::{MAX_CHECK_INPUTS, MAX_INPUT_PATTERN_BYTES};
use crate::pattern::{DEEP, Pattern};

/// A check's inputs, read: its patterns in declared order. None is the whole tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inputs {
    /// The patterns, in declared order.
    patterns: Vec<Pattern>,
}

impl Inputs {
    /// The inputs `patterns` declare, or the index of the first one at fault and what is wrong
    /// with it: more than [`MAX_CHECK_INPUTS`] of them, one that is no pattern (`rule`), or
    /// one given twice (at the second). No pattern reads as the whole tree.
    pub fn parse(patterns: &[String]) -> Result<Inputs, (usize, String)> {
        let mut read = Vec::with_capacity(patterns.len().min(MAX_CHECK_INPUTS));
        for (at, text) in patterns.iter().enumerate() {
            if at == MAX_CHECK_INPUTS {
                return Err((at, format!("more than {MAX_CHECK_INPUTS} patterns")));
            }
            let pattern = rule(text).map_err(|what| (at, what))?;
            if patterns[..at].contains(text) {
                return Err((at, "given twice".to_owned()));
            }
            read.push(pattern);
        }
        Ok(Inputs { patterns: read })
    }

    /// Whether it holds no pattern: the check takes the whole tree.
    pub fn whole_tree(&self) -> bool {
        self.patterns.is_empty()
    }

    /// The index, in declared order, of the first pattern `path` matches. None when none
    /// does, and for the whole tree, which has no pattern to name.
    pub fn first_match(&self, path: &str) -> Option<usize> {
        (self.patterns.iter()).position(|pattern| pattern.matches(path))
    }

    /// Whether `path` is among the inputs: a pattern matches it, or there is none.
    pub fn matches(&self, path: &str) -> bool {
        self.whole_tree() || self.first_match(path).is_some()
    }
}

/// The pattern `text`, or what is wrong with it: more than [`MAX_INPUT_PATTERN_BYTES`] bytes, a
/// NUL, a segment `.` or `..` (which no repository path holds), or one of the grammar's own
/// faults: an empty segment, a `*` beside other bytes, a `**` before the last segment.
fn rule(text: &str) -> Result<Pattern, String> {
    if text.len() > MAX_INPUT_PATTERN_BYTES {
        return Err(format!(
            "of {} bytes, more than {MAX_INPUT_PATTERN_BYTES}",
            text.len()
        ));
    }
    if text.contains('\0') {
        return Err("not a pattern: a NUL byte".to_owned());
    }
    let mut segments = text.split('/');
    if segments
        .clone()
        .any(|segment| segment == "." || segment == "..")
    {
        return Err("not a pattern: a segment . or .., which no path holds".to_owned());
    }
    if let Some(pattern) = Pattern::parse(text) {
        return Ok(pattern);
    }

    // The grammar's own fault, named: the first that holds.
    let why = if segments.clone().any(str::is_empty) {
        "an empty segment"
    } else if segments.any(|segment| segment.contains('*') && segment != "*" && segment != DEEP) {
        "a * beside other bytes in a segment"
    } else {
        "a ** before the last segment"
    };
    Err(format!("not a pattern: {why}"))
}
