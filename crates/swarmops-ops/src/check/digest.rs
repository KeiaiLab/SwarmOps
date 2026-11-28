//! The digest of a check's definition: what `check list` gives as `definition_digest`.
//!
//! ```text
//! BLAKE3( "swarmops:check-definition:v0\0"
//!         ‖ for each field present, in tag order: tag (1 byte) ‖ u64le(len) ‖ bytes )
//!
//!   tag 1  argv       always:              u64le(count) ‖ for each: u64le(len) ‖ bytes
//!   tag 2  dir        unless "." (the root): its UTF-8 bytes
//!   tag 3  timeout_s  unless 900:          u64le(seconds)
//!   tag 4  reports    unless none:         u64le(count) ‖ for each, in declared order:
//!                                          u8(format: 1 junit, 2 sarif) ‖ u64le(len) ‖ path
//!   tag 5  inputs     unless none:         u64le(count) ‖ for each, sorted bytewise:
//!                                          u64le(len) ‖ pattern
//! ```
//!
//! The name is not hashed: a check renamed keeps its digest. A field at its default is left out,
//! so a key added to the file later, while left out, changes no digest. `v0` says the recipe is
//! provisional: it is no part of the hash contract, and a later release may replace it.

use swarmops_api::{CheckReportFormat, DEFAULT_CHECK_TIMEOUT_S};

use super::file::{ROOT_DIR, Report};

/// What every definition's hash starts with: its domain and version.
const DOMAIN: &[u8] = b"swarmops:check-definition:v0\0";

/// The tag of the program and its arguments.
const TAG_ARGV: u8 = 1;

/// The tag of the directory.
const TAG_DIR: u8 = 2;

/// The tag of the timeout.
const TAG_TIMEOUT: u8 = 3;

/// The tag of the reports.
const TAG_REPORTS: u8 = 4;

/// The tag of the inputs.
const TAG_INPUTS: u8 = 5;

/// What a check's definition is made of: the fields its digest names. A key the file gains
/// later is a field here, so that hashing it changes no caller but the one that fills it in.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Definition<'c> {
    /// The program and its arguments.
    pub(crate) argv: &'c [String],
    /// The directory it runs in.
    pub(crate) dir: &'c str,
    /// The most seconds it may run.
    pub(crate) timeout_s: u64,
    /// The reports it declares, in declared order.
    pub(crate) reports: &'c [Report],
    /// The path patterns of what it reads, in any order; none for the whole tree.
    pub(crate) inputs: &'c [String],
}

/// The digest of the definition `of`: 64 lowercase hex digits.
pub(crate) fn definition(of: &Definition<'_>) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(DOMAIN);

    // 1. argv, always: its count, then each string with its length.
    let mut value = length(of.argv.len()).to_vec();
    for arg in of.argv {
        value.extend_from_slice(&length(arg.len()));
        value.extend_from_slice(arg.as_bytes());
    }
    field(&mut hasher, TAG_ARGV, &value);

    // 2. dir and timeout_s, each unless at its default.
    if of.dir != ROOT_DIR {
        field(&mut hasher, TAG_DIR, of.dir.as_bytes());
    }
    if of.timeout_s != DEFAULT_CHECK_TIMEOUT_S {
        field(&mut hasher, TAG_TIMEOUT, &of.timeout_s.to_le_bytes());
    }

    // 3. reports, unless none: their count, then each one's format and path, in order.
    if !of.reports.is_empty() {
        let mut value = length(of.reports.len()).to_vec();
        for report in of.reports {
            value.push(match report.format {
                CheckReportFormat::Junit => 1,
                CheckReportFormat::Sarif => 2,
                _ => 0,
            });
            value.extend_from_slice(&length(report.path.len()));
            value.extend_from_slice(report.path.as_bytes());
        }
        field(&mut hasher, TAG_REPORTS, &value);
    }

    // 4. inputs, unless none: their count, then each pattern with its length, sorted here, so
    //    that the order they were given in changes nothing.
    if !of.inputs.is_empty() {
        let mut patterns: Vec<&str> = of.inputs.iter().map(String::as_str).collect();
        patterns.sort_unstable();
        let mut value = length(patterns.len()).to_vec();
        for pattern in patterns {
            value.extend_from_slice(&length(pattern.len()));
            value.extend_from_slice(pattern.as_bytes());
        }
        field(&mut hasher, TAG_INPUTS, &value);
    }
    hasher.finalize().to_hex().to_string()
}

/// Hashes one field: its tag, its value's length, its value.
fn field(hasher: &mut blake3::Hasher, tag: u8, value: &[u8]) {
    hasher.update(&[tag]);
    hasher.update(&length(value.len()));
    hasher.update(value);
}

/// `len` as eight little-endian bytes.
fn length(len: usize) -> [u8; 8] {
    u64::try_from(len).unwrap_or(u64::MAX).to_le_bytes()
}
