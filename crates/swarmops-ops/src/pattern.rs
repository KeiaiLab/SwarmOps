//! Name patterns: the grammar `access.toml` writes its agents and refs in, and a check its
//! `inputs`.
//!
//! ```text
//!   pattern   = segment *( "/" segment )
//!   segment   = literal | "*" | "**"        "**" only as the last one
//!
//!   feature/*    feature/a          not feature/a/b, not feature
//!   feature/**   feature/a, feature/a/b            not feature
//!   main         main               not mainline, not main/x
//! ```
//!
//! A literal segment is compared byte for byte: no case folding, no normalisation. A `*` beside
//! other bytes in a segment is no pattern.

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

    /// Whether a name below the directory `dir` (its segments, from the root) could match: the
    /// pattern's first segments match the directory's, and it has more segments or ends in `**`.
    /// So `web/*/x` could match below `web` and `web/app`, not below `docs` or `web/app/x`.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "a check's inputs are its first caller")
    )]
    pub(crate) fn could_match_below(&self, dir: &[&str]) -> bool {
        // 1. A trailing `**` takes every depth once the segments before it match.
        if let Some((last, head)) = self.segments.split_last()
            && last == DEEP
        {
            let shared = head.len().min(dir.len());
            return segments_match(&head[..shared], &dir[..shared]);
        }

        // 2. Otherwise the pattern is longer than the directory, and starts as it does.
        self.segments.len() > dir.len() && segments_match(&self.segments[..dir.len()], dir)
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
        Pattern::parse(pattern).unwrap().matches(name)
    }

    /// Whether `pattern` could match a name below the directory `dir`.
    fn below(pattern: &str, dir: &str) -> bool {
        let dir: Vec<&str> = dir.split('/').collect();
        Pattern::parse(pattern).unwrap().could_match_below(&dir)
    }

    /// An exact pattern matches its name alone.
    #[test]
    fn exact_name() {
        assert!(matches("main", "main"));
        assert!(!matches("main", "mainline"));
        assert!(!matches("main", "main/x"));
        assert!(!matches("main", "Main"));
    }

    /// `*` matches one segment.
    #[test]
    fn star_is_one_segment() {
        assert!(matches("release/*", "release/2"));
        assert!(!matches("release/*", "release/2/x"));
        assert!(!matches("release/*", "release"));
        assert!(matches("*/k.rs", "a/k.rs"));
        assert!(matches("*", "k.rs"));
        assert!(!matches("*", "a/k.rs"));
    }

    /// A trailing `**` matches every depth below, not the name above it.
    #[test]
    fn trailing_double_star_is_every_depth() {
        assert!(matches("feature/**", "feature/a"));
        assert!(matches("feature/**", "feature/a/b/c"));
        assert!(!matches("feature/**", "feature"));
        assert!(!matches("feature/**", "features/a"));
        assert!(matches("**", "a"));
        assert!(matches("**", "a/b"));
    }

    /// A `**` before the end, a `*` beside other bytes and an empty segment are no pattern.
    #[test]
    fn bad_patterns_are_none() {
        for pattern in ["a/**/b", "fe*", "a//b", "", "/a", "a/", "**/a", "a/***"] {
            assert_eq!(Pattern::parse(pattern), None, "{pattern:?}");
        }
    }

    /// A literal prefix reaches below the directories it names, and no other.
    #[test]
    fn a_literal_prefix_reaches_below_its_directories() {
        assert!(below("web/app/page.tsx", "web"));
        assert!(below("web/app/page.tsx", "web/app"));
        assert!(!below("web/app/page.tsx", "docs"));
        assert!(!below("web/app/page.tsx", "web/lib"));
    }

    /// `*` stands for any one directory on the way.
    #[test]
    fn a_star_reaches_below_any_one_directory() {
        assert!(below("a/*/k.rs", "a"));
        assert!(below("a/*/k.rs", "a/x"));
        assert!(!below("a/*/k.rs", "b"));
        assert!(below("*/k.rs", "anything"));
    }

    /// A trailing `**` reaches every depth below the segments before it.
    #[test]
    fn a_trailing_double_star_reaches_every_depth() {
        assert!(below("docs/**", "docs"));
        assert!(below("docs/**", "docs/status/M1"));
        assert!(!below("docs/**", "web"));
        assert!(below("**", "web/app"));
        assert!(below("a/b/**", "a"));
        assert!(!below("a/b/**", "c"));
    }

    /// A pattern no longer than the directory names nothing below it.
    #[test]
    fn a_pattern_no_longer_than_the_directory_reaches_nothing() {
        assert!(!below("docs", "docs"));
        assert!(!below("docs/*", "docs/status"));
        assert!(!below("a/*/k.rs", "a/x/k.rs"));
        assert!(!below("*", "a"));
    }
}
