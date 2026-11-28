//! Name patterns: the one grammar `access.toml` reads its callers and refs in, and a check file
//! its checks' `inputs`.
//!
//! ```text
//!   pattern = segment ( "/" segment )*        segment = a name | "*" | "**" (the last one only)
//!
//!   feature/*    feature/a      yes     feature/a/b   no      feature   no
//!   feature/**   feature/a      yes     feature/a/b   yes     feature   no
//! ```
//!
//! A `*` is one whole segment, never a part of one; a `**` is every depth below, at least one
//! segment. What a name segment may hold is its reader's rule, not this module's.

/// A pattern's segment that matches any one segment.
pub(crate) const ONE: &str = "*";

/// A pattern's last segment that matches every depth below.
pub(crate) const DEEP: &str = "**";

/// A name pattern: `/`-separated segments, each exact or `*`, the last one possibly `**`.
///
/// `feature/*` matches `feature/a`, not `feature/a/b`; `feature/**` matches both, not `feature`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Pattern {
    /// The pattern's segments.
    segments: Vec<String>,
}

impl Pattern {
    /// The pattern `text`, or `None` if a segment is empty, holds a `*` beside other bytes, or
    /// is a `**` that is not the last one.
    pub(crate) fn parse(text: &str) -> Option<Pattern> {
        let segments: Vec<String> = text.split('/').map(str::to_owned).collect();
        let last = segments.len() - 1;
        for (i, segment) in segments.iter().enumerate() {
            let wild = segment == ONE || (segment == DEEP && i == last);
            if segment.is_empty() || (segment.contains('*') && !wild) {
                return None;
            }
        }
        Some(Pattern { segments })
    }

    /// Whether `name` matches.
    pub(crate) fn matches(&self, name: &str) -> bool {
        let names: Vec<&str> = name.split('/').collect();

        // 1. A trailing `**`: the segments before it, then at least one below.
        if let Some((last, head)) = self.segments.split_last()
            && last == DEEP
        {
            return names.len() > head.len() && segments_match(head, &names[..head.len()]);
        }

        // 2. Otherwise segment for segment.
        names.len() == self.segments.len() && segments_match(&self.segments, &names)
    }
}

/// Whether each name segment matches its pattern segment, exactly or by `*`.
pub(crate) fn segments_match(patterns: &[String], names: &[&str]) -> bool {
    patterns
        .iter()
        .zip(names)
        .all(|(pattern, name)| pattern == ONE || pattern == name)
}

#[cfg(test)]
mod tests {
    use super::Pattern;

    /// Whether the pattern `pattern` matches `name`.
    fn matches(pattern: &str, name: &str) -> bool {
        Pattern::parse(pattern)
            .unwrap_or_else(|| panic!("{pattern:?} is a pattern"))
            .matches(name)
    }

    /// A name matches itself alone, and `*` one whole segment: never none, never two.
    #[test]
    fn a_pattern_matches_segment_for_segment() {
        assert!(matches("main", "main"));
        assert!(!matches("main", "mainline"));
        assert!(!matches("main", "main/x"));
        assert!(matches("docs/*", "docs/a.md"));
        assert!(!matches("docs/*", "docs/a/b.md"));
        assert!(!matches("docs/*", "docs"));
        assert!(matches("*/Cargo.toml", "fuzz/Cargo.toml"));
        assert!(!matches("*/Cargo.toml", "Cargo.toml"));
        assert!(!matches("*/Cargo.toml", "crates/app/Cargo.toml"));
        assert!(matches("*", "README.md"));
        assert!(!matches("*", "docs/a.md"));
    }

    /// A last `**` is every depth below what is before it, at least one segment; alone, every
    /// name.
    #[test]
    fn a_trailing_double_star_is_every_depth_below() {
        assert!(matches("docs/**", "docs/a.md"));
        assert!(matches("docs/**", "docs/a/b/c.md"));
        assert!(!matches("docs/**", "docs"));
        assert!(!matches("docs/**", "docsy/a.md"));
        assert!(matches("crates/*/src/**", "crates/app/src/lib.rs"));
        assert!(!matches("crates/*/src/**", "crates/app/tests/all.rs"));
        assert!(matches("**", "README.md"));
        assert!(matches("**", "docs/a/b.md"));
    }

    /// A `**` before the end, a `*` beside other bytes and an empty segment are no pattern.
    #[test]
    fn what_is_no_pattern_is_refused() {
        for text in [
            "a/**/b", "**/b", "fe*", "*.rs", "a//b", "/a", "a/", "", "***",
        ] {
            assert_eq!(Pattern::parse(text), None, "{text:?}");
        }
    }
}
