//! GitHub's documented limits, checked before a push sends anything (M13): a file over 100 MiB
//! and a push over 2 GB are refused locally with their path or size, and a file from 50 MiB is
//! warned about. They apply to `github.com` and its subdomains only, and are not configurable.
//!
//! The values are GitHub's, as read on 2026-10-22 in "About large files on GitHub" (the file
//! limits, in MiB) and "Repository limits" (the push limit, written "2GB" without saying decimal
//! or binary; the smaller, 2 × 10^9 bytes, is taken, so that no push GitHub refuses is sent).
//!
//! Under the `test-hooks` feature, `SWARMOPS_TEST_GITHUB_HOSTS` (a comma-separated list of
//! `host[:port]`) adds hosts that count as GitHub's, for the loopback server of the tests; without
//! the feature it is ignored.

use crate::transport::NewObjects;

/// GitHub's host; its subdomains (`ssh.github.com`) count as it.
const GITHUB: &str = "github.com";

/// The variable that adds hosts under the `test-hooks` feature: `host[:port]`, comma-separated.
#[cfg(feature = "test-hooks")]
const TEST_HOSTS: &str = "SWARMOPS_TEST_GITHUB_HOSTS";

/// A file GitHub refuses: larger than 100 MiB ("About large files on GitHub").
pub const GITHUB_FILE_REFUSED: u64 = 100 * 1024 * 1024;

/// A file git warns about when it is pushed to GitHub: larger than 50 MiB ("About large files on
/// GitHub").
pub const GITHUB_FILE_WARNED: u64 = 50 * 1024 * 1024;

/// A push GitHub refuses: larger than 2 GB ("Repository limits"), read as 2 × 10^9 bytes.
pub const GITHUB_PUSH_REFUSED: u64 = 2_000_000_000;

/// Which limit a push broke (`E_TOO_LARGE`, `details.scope`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitScope {
    /// A file over [`GITHUB_FILE_REFUSED`]: `remote_file`.
    RemoteFile,
    /// A push over [`GITHUB_PUSH_REFUSED`]: `remote_push`.
    RemotePush,
}

impl LimitScope {
    /// The scope as `details.scope` writes it.
    pub fn as_str(self) -> &'static str {
        match self {
            LimitScope::RemoteFile => "remote_file",
            LimitScope::RemotePush => "remote_push",
        }
    }
}

/// A push past one of GitHub's limits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LimitExceeded {
    /// Which limit.
    pub scope: LimitScope,
    /// The file's path, for [`LimitScope::RemoteFile`].
    pub path: Option<String>,
    /// The file's or the push's size, in bytes.
    pub size: u64,
    /// The limit, in bytes.
    pub limit: u64,
}

impl LimitExceeded {
    /// A `size` past `limit` of `scope`, at `path` for a file.
    pub fn new(scope: LimitScope, path: Option<String>, size: u64, limit: u64) -> LimitExceeded {
        LimitExceeded {
            scope,
            path,
            size,
            limit,
        }
    }
}

/// Whether the limits apply to a remote on `host` (without user or port): `github.com` and
/// its subdomains, in any case. Under the `test-hooks` feature, also each host
/// `SWARMOPS_TEST_GITHUB_HOSTS` names (its port, if any, is not compared).
pub fn applies_to(host: &str) -> bool {
    let host = host.to_ascii_lowercase();

    // GitHub's own host, or a name under it.
    let under = host
        .strip_suffix(GITHUB)
        .is_some_and(|rest| rest.is_empty() || (rest.ends_with('.') && rest.len() > 1));
    under || test_host(&host)
}

/// Whether `SWARMOPS_TEST_GITHUB_HOSTS` names `host` (the `test-hooks` feature only).
#[cfg(feature = "test-hooks")]
fn test_host(host: &str) -> bool {
    let Some(listed) = std::env::var_os(TEST_HOSTS) else {
        return false;
    };
    let listed = listed.to_string_lossy().to_ascii_lowercase();

    // Each `host[:port]` entry, by its host.
    listed
        .split(',')
        .map(|entry| entry.trim().split(':').next().unwrap_or_default())
        .any(|entry| !entry.is_empty() && entry == host)
}

/// Without the `test-hooks` feature no host is added.
#[cfg(not(feature = "test-hooks"))]
fn test_host(_host: &str) -> bool {
    false
}

/// Checks what a push would add against the limits: the warnings for the files over 50 MiB,
/// or the first limit it breaks, a file over 100 MiB before a push over 2 GB (the file is what
/// can be acted on).
pub fn check(objects: &NewObjects) -> Result<Vec<String>, LimitExceeded> {
    // 1. Each file: refused over 100 MiB, warned about over 50 MiB.
    let mut warnings = Vec::new();
    for (_, size, path) in &objects.blobs {
        if *size > GITHUB_FILE_REFUSED {
            return Err(LimitExceeded {
                scope: LimitScope::RemoteFile,
                path: Some(path.clone()),
                size: *size,
                limit: GITHUB_FILE_REFUSED,
            });
        }
        if *size > GITHUB_FILE_WARNED {
            warnings.push(format!(
                "github_large_file: {path} is {size} bytes, more than the {GITHUB_FILE_WARNED} \
                 GitHub warns about"
            ));
        }
    }

    // 2. The push as a whole, by its objects' bytes on disk: an upper bound of the pack.
    if objects.disk_usage > GITHUB_PUSH_REFUSED {
        return Err(LimitExceeded {
            scope: LimitScope::RemotePush,
            path: None,
            size: objects.disk_usage,
            limit: GITHUB_PUSH_REFUSED,
        });
    }
    Ok(warnings)
}
