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
//!
//! The M13 plan's task C1 froze these items; task C7 writes their bodies, which until then
//! panic with the task that writes them.

use crate::transport::NewObjects;

/// The task that writes this module's bodies.
const PLANNED: &str = "planned in M13 (C7)";

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

/// Whether the limits apply to a remote on `host`: `github.com` and its subdomains.
pub fn applies_to(_host: &str) -> bool {
    unimplemented!("{PLANNED}")
}

/// Checks what a push would add against the limits: the warnings for the files from 50 MiB,
/// or the first limit it breaks.
pub fn check(_objects: &NewObjects) -> Result<Vec<String>, LimitExceeded> {
    unimplemented!("{PLANNED}")
}
