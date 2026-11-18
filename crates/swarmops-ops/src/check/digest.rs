//! The digest of a check's definition: what `check list` gives as `definition_digest`.
//!
//! ```text
//! BLAKE3( "swarmops:check-definition:v0\0"
//!         ‖ for each field present, in tag order: tag (1 byte) ‖ u64le(len) ‖ bytes )
//!
//!   tag 1  argv       always:              u64le(count) ‖ for each: u64le(len) ‖ bytes
//!   tag 2  dir        unless "." (the root): its UTF-8 bytes
//!   tag 3  timeout_s  unless 900:          u64le(seconds)
//! ```
//!
//! The name is not hashed: a check renamed keeps its digest. A field at its default is left out,
//! so a key added to the file later, while left out, changes no digest. `v0` says the recipe is
//! provisional: it is no part of the hash contract, and a later release may replace it.

use swarmops_api::DEFAULT_CHECK_TIMEOUT_S;

use super::file::ROOT_DIR;

/// What every definition's hash starts with: its domain and version.
const DOMAIN: &[u8] = b"swarmops:check-definition:v0\0";

/// The tag of the program and its arguments.
const TAG_ARGV: u8 = 1;

/// The tag of the directory.
const TAG_DIR: u8 = 2;

/// The tag of the timeout.
const TAG_TIMEOUT: u8 = 3;

/// The digest of the definition `argv`, `dir`, `timeout_s`: 64 lowercase hex digits.
pub(crate) fn definition(argv: &[String], dir: &str, timeout_s: u64) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(DOMAIN);

    // 1. argv, always: its count, then each string with its length.
    let mut value = length(argv.len()).to_vec();
    for arg in argv {
        value.extend_from_slice(&length(arg.len()));
        value.extend_from_slice(arg.as_bytes());
    }
    field(&mut hasher, TAG_ARGV, &value);

    // 2. dir and timeout_s, each unless at its default.
    if dir != ROOT_DIR {
        field(&mut hasher, TAG_DIR, dir.as_bytes());
    }
    if timeout_s != DEFAULT_CHECK_TIMEOUT_S {
        field(&mut hasher, TAG_TIMEOUT, &timeout_s.to_le_bytes());
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
