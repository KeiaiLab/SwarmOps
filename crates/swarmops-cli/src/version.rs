//! What a build says of itself. The text `--version` prints after the program's name: the
//! package version, and the build's commit when `SWARMOPS_BUILD_COMMIT` was set at compile time
//! (umbrella spec §4). And whether the build is a release's ([`is_release`]): one compiled with
//! `SWARMOPS_BUILD_CHANNEL=release`, which the project's release workflow sets and nothing else
//! does.
//!
//! `option_env!` puts each variable in the crate's dep-info, so cargo rebuilds the crate when
//! one changes. A release's build sets the channel and leaves the commit unset; a build from a
//! clone sets the commit or neither. The channel changes nothing `--version` prints.
//!
//! A build with the `test-hooks` feature reads two variables more, at run time, for the tests of
//! `update`: `SWARMOPS_TEST_VERSION` and `SWARMOPS_TEST_UPDATE_RELEASE`. A build without the
//! feature ignores both.

use std::sync::OnceLock;

/// The build's commit, from `SWARMOPS_BUILD_COMMIT` at compile time.
const BUILD_COMMIT: Option<&str> = option_env!("SWARMOPS_BUILD_COMMIT");

/// The fewest hex digits a commit may have: the floor of git's default abbreviation
/// (`core.abbrev` = `auto`), which scales with the object count but never goes below 7.
const MIN_COMMIT_LEN: usize = 7;

/// The most hex digits a commit may have: a full SHA-1 object name.
const MAX_COMMIT_LEN: usize = 40;

// A malformed value fails the build rather than ship a binary that names no real commit.
const _: () = assert!(
    commit_ok(BUILD_COMMIT),
    "SWARMOPS_BUILD_COMMIT must be 7 to 40 lowercase hex digits, such as `git rev-parse HEAD`"
);

/// The build's channel, from `SWARMOPS_BUILD_CHANNEL` at compile time.
const BUILD_CHANNEL: Option<&str> = option_env!("SWARMOPS_BUILD_CHANNEL");

/// The one channel there is: a release's binaries, built by the project's release workflow.
const RELEASE_CHANNEL: &str = "release";

// Any other value fails the build: a word this build does not know must not pass for no channel.
const _: () = assert!(
    channel_ok(BUILD_CHANNEL),
    "SWARMOPS_BUILD_CHANNEL must be `release`, or unset"
);

// A release is named by its version alone, and a build that names its commit is from a clone.
const _: () = assert!(
    BUILD_CHANNEL.is_none() || BUILD_COMMIT.is_none(),
    "SWARMOPS_BUILD_CHANNEL and SWARMOPS_BUILD_COMMIT cannot both be set"
);

/// The variable that gives [`version`] its text, in a build with the `test-hooks` feature: a
/// version (`0.0.1`, `1.2.3-rc.1`), so that a test has a build older than the one it was built
/// as. Any other value is ignored.
#[cfg(feature = "test-hooks")]
pub(crate) const TEST_VERSION_ENV: &str = "SWARMOPS_TEST_VERSION";

/// The variable that makes a build with the `test-hooks` feature a release's ([`is_release`])
/// when it is `1`: no test builds with the channel set.
#[cfg(feature = "test-hooks")]
pub(crate) const TEST_UPDATE_RELEASE_ENV: &str = "SWARMOPS_TEST_UPDATE_RELEASE";

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

/// Whether `channel` may stamp the build: absent, or [`RELEASE_CHANNEL`].
const fn channel_ok(channel: Option<&str>) -> bool {
    let Some(channel) = channel else {
        return true;
    };
    let (given, known) = (channel.as_bytes(), RELEASE_CHANNEL.as_bytes());
    if given.len() != known.len() {
        return false;
    }

    // `const fn` compares no strings: walk the bytes by index.
    let mut i = 0;
    while i < given.len() {
        if given[i] != known[i] {
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

/// This build's version text, which `--version` prints after `swarmops `. In a build with the
/// `test-hooks` feature, `SWARMOPS_TEST_VERSION`'s version instead, when it names one.
pub(crate) fn version() -> &'static str {
    static TEXT: OnceLock<String> = OnceLock::new();
    TEXT.get_or_init(|| {
        test_version().unwrap_or_else(|| version_text(env!("CARGO_PKG_VERSION"), BUILD_COMMIT))
    })
}

/// Whether this build is a release's: compiled with `SWARMOPS_BUILD_CHANNEL=release`. A build
/// from a clone is none, whatever version it prints. In a build with the `test-hooks` feature,
/// also when `SWARMOPS_TEST_UPDATE_RELEASE` is `1`.
pub(crate) fn is_release() -> bool {
    BUILD_CHANNEL.is_some() || test_release()
}

/// [`TEST_VERSION_ENV`]'s version, if the variable holds one.
#[cfg(feature = "test-hooks")]
fn test_version() -> Option<String> {
    hooked_version(std::env::var_os(TEST_VERSION_ENV))
}

/// In a build without the `test-hooks` feature, [`version`] is the build's own.
#[cfg(not(feature = "test-hooks"))]
fn test_version() -> Option<String> {
    None
}

/// Whether [`TEST_UPDATE_RELEASE_ENV`] says this build is a release's.
#[cfg(feature = "test-hooks")]
fn test_release() -> bool {
    hooked_release(std::env::var_os(TEST_UPDATE_RELEASE_ENV))
}

/// In a build without the `test-hooks` feature, only the channel makes a release's build.
#[cfg(not(feature = "test-hooks"))]
fn test_release() -> bool {
    false
}

/// The version `value` holds, as [`TEST_VERSION_ENV`]'s: one `update` reads as a version
/// ([`swarmops_ops::update::Version::parse`]), which is the installer's rule for `--version`.
/// `None` for anything else.
#[cfg(feature = "test-hooks")]
fn hooked_version(value: Option<std::ffi::OsString>) -> Option<String> {
    let value = value?.into_string().ok()?;
    swarmops_ops::update::Version::parse(&value).map(|_| value)
}

/// Whether `value`, as [`TEST_UPDATE_RELEASE_ENV`]'s, makes the build a release's: `1` alone.
#[cfg(feature = "test-hooks")]
fn hooked_release(value: Option<std::ffi::OsString>) -> bool {
    value.is_some_and(|value| value == "1")
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

    /// No variable, or `release`; any other word fails the build, a release's in another case
    /// or with a space among them.
    #[test]
    fn a_channel_is_release_or_none() {
        assert!(channel_ok(None));
        assert!(channel_ok(Some(RELEASE_CHANNEL)));
        assert!(channel_ok(Some("release")));
        for channel in [
            "", "Release", "release ", "releas", "releases", "nightly", "1",
        ] {
            assert!(!channel_ok(Some(channel)), "{channel:?}");
        }
    }

    /// This build is a release's exactly when it was compiled with the channel, which the test
    /// reads itself (scripts/check-stamped.py builds it both ways); no gate's build sets the
    /// hook, which would make it one too.
    #[test]
    fn this_build_knows_its_channel() {
        let channel = option_env!("SWARMOPS_BUILD_CHANNEL");
        assert_eq!(BUILD_CHANNEL, channel);
        if channel.is_some() {
            assert!(is_release(), "built with the channel");
            assert_eq!(option_env!("SWARMOPS_BUILD_COMMIT"), None, "and no commit");
        } else if !test_release() {
            assert!(!is_release(), "built without the channel");
        }
    }

    /// The version hook takes a version and nothing else, so that `--version` stays one line
    /// of the form the installer reads.
    #[cfg(feature = "test-hooks")]
    #[test]
    fn the_version_hook_takes_a_version_alone() {
        let hooked = |value: &str| hooked_version(Some(value.into()));
        for version in ["0.0.1", "10.20.30", "1.2.3-rc.1", "1.2.3-a-b.7"] {
            assert_eq!(hooked(version).as_deref(), Some(version));
        }
        for other in [
            "",
            "1",
            "1.2",
            "1.2.3.4",
            "v1.2.3",
            "1.2.x",
            "1..3",
            "1.2.3-",
            "1.2.3+abc",
            "1.2.3 ",
            "1.2.3\n",
            "1.2.3-rc 1",
            "1.2.3-\u{e9}",
        ] {
            assert_eq!(hooked(other), None, "{other:?}");
        }
        assert_eq!(hooked_version(None), None);
    }

    /// The release hook is `1`, and no other value.
    #[cfg(feature = "test-hooks")]
    #[test]
    fn the_release_hook_is_1_alone() {
        assert!(hooked_release(Some("1".into())));
        for other in ["", "0", "true", "on", "11", " 1"] {
            assert!(!hooked_release(Some(other.into())), "{other:?}");
        }
        assert!(!hooked_release(None));
    }

    /// The commit is semver build metadata after a `+`; without one, the version alone.
    #[test]
    fn the_text_appends_the_commit() {
        assert_eq!(version_text("0.7.0", None), "0.7.0");
        assert_eq!(version_text("0.7.0", Some(FULL)), format!("0.7.0+{FULL}"));
    }

    /// This build's text: the package version, and the commit if one was set. The test reads
    /// the variable itself, so a module that stops reading it fails in a stamped build. (Where
    /// the version hook is set, the text is the hook's: no gate's build sets it.)
    #[test]
    fn this_build_has_its_text() {
        let pkg = env!("CARGO_PKG_VERSION");
        let commit = option_env!("SWARMOPS_BUILD_COMMIT");
        let expected = commit.map_or_else(|| pkg.to_owned(), |c| format!("{pkg}+{c}"));
        assert_eq!(version(), test_version().unwrap_or(expected));
    }

    /// clap prints this text, not the package version it takes by default: the same string,
    /// so the check holds in an unstamped build too.
    #[test]
    fn clap_shows_this_text() {
        use clap::CommandFactory;

        let shown = crate::cli::Cli::command().get_version().map(str::as_ptr);
        assert_eq!(
            shown,
            Some(version().as_ptr()),
            "clap shows version()'s text: the same pointer"
        );
    }
}
