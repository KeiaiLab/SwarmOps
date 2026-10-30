//! The text `--version` prints after the program's name: the package version, and the build's
//! commit when `SWARMOPS_BUILD_COMMIT` was set at compile time (umbrella spec §4).
//!
//! `option_env!` puts the variable in the crate's dep-info, so cargo rebuilds the crate when
//! it changes. Release builds leave it unset.

use std::sync::OnceLock;

/// The build's commit, from `SWARMOPS_BUILD_COMMIT` at compile time.
const BUILD_COMMIT: Option<&str> = option_env!("SWARMOPS_BUILD_COMMIT");

/// The fewest hex digits a commit may have: git's shortest abbreviation.
const MIN_COMMIT_LEN: usize = 7;

/// The most hex digits a commit may have: a full SHA-1 object name.
const MAX_COMMIT_LEN: usize = 40;

// A malformed value fails the build rather than ship a binary that names no real commit.
const _: () = assert!(
    commit_ok(BUILD_COMMIT),
    "SWARMOPS_BUILD_COMMIT must be 7 to 40 lowercase hex digits, such as `git rev-parse HEAD`"
);

/// Whether `commit` may stamp the version: absent, or 7 to 40 lowercase hex digits.
const fn commit_ok(commit: Option<&str>) -> bool {
    let Some(commit) = commit else {
        return true;
    };
    let digits = commit.as_bytes();
    if digits.len() < MIN_COMMIT_LEN || digits.len() > MAX_COMMIT_LEN {
        return false;
    }

    // `const fn` allows no iterators: walk the bytes by index.
    let mut i = 0;
    while i < digits.len() {
        if !matches!(digits[i], b'0'..=b'9' | b'a'..=b'f') {
            return false;
        }
        i += 1;
    }
    true
}

/// `version`, with `commit` as semver build metadata (`0.7.0+<commit>`) when there is one.
fn version_text(version: &str, commit: Option<&str>) -> String {
    let Some(commit) = commit else {
        return version.to_owned();
    };
    format!("{version}+{commit}")
}

/// This build's version text, which `--version` prints after `swarmops `.
pub(crate) fn version() -> &'static str {
    static TEXT: OnceLock<String> = OnceLock::new();
    TEXT.get_or_init(|| version_text(env!("CARGO_PKG_VERSION"), BUILD_COMMIT))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHORT: &str = "69e435e";
    const FULL: &str = "69e435e36a6781b907a1fa1a5bd69caeff4c6db0";

    /// No variable, or 7 to 40 lowercase hex digits.
    #[test]
    fn a_commit_is_7_to_40_lowercase_hex() {
        assert!(commit_ok(None));
        assert!(commit_ok(Some(SHORT)));
        assert!(commit_ok(Some(FULL)));
    }

    /// Anything else fails the build: too short or long, upper case, not hex, empty.
    #[test]
    fn other_commits_are_refused() {
        let long = format!("{FULL}0");
        let upper = FULL.to_uppercase();
        for commit in [
            "69e435",
            long.as_str(),
            upper.as_str(),
            "XYZ",
            "69e435g",
            "",
        ] {
            assert!(!commit_ok(Some(commit)), "{commit:?}");
        }
    }

    /// The commit is semver build metadata after a `+`; without one, the version alone.
    #[test]
    fn the_text_appends_the_commit() {
        assert_eq!(version_text("0.7.0", None), "0.7.0");
        assert_eq!(version_text("0.7.0", Some(FULL)), format!("0.7.0+{FULL}"));
    }

    /// This build's text: the package version, and the commit if one was set.
    #[test]
    fn this_build_has_its_text() {
        let pkg = env!("CARGO_PKG_VERSION");
        let expected = BUILD_COMMIT.map_or_else(|| pkg.to_owned(), |c| format!("{pkg}+{c}"));
        assert_eq!(version(), expected);
    }
}
