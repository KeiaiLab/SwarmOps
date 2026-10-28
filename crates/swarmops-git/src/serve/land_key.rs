//! The land key of a push to `refs/land/<target>` (M14 design §0.6): [`LAND_KEY_PREFIX`] and 60
//! hex digits of a BLAKE3 hash of the identity, the target and the pushed tip, so that the same
//! push again replays its land instead of landing twice.
//!
//! ```text
//! BLAKE3("swarmops:serve-land:v1\0" ‖ identity ‖ \0 ‖ target ‖ \0 ‖ tip hex)
//!   └─ first 30 bytes, lowercase hex ─▶ "gp1-" + 60 hex = 64 characters
//! ```
//!
//! 64 characters of `[0-9a-z-]`: within a land's `idempotency_key` grammar (1 to 64 letters,
//! digits and `-`). The bytes hashed are a contract: a change of them is a new prefix.

use std::fmt::Write as _;

use crate::oid::GitOid;

/// What every land key of `serve` starts with.
pub const LAND_KEY_PREFIX: &str = "gp1-";

/// What the hash of a land key starts with: its domain and version.
const DOMAIN: &[u8] = b"swarmops:serve-land:v1\0";

/// How many bytes of the hash a land key keeps: 60 hex digits.
const KEPT_BYTES: usize = 30;

/// The land key of `identity` pushing `tip` to `refs/land/<target>`.
pub fn land_key(identity: &str, target: &str, tip: GitOid) -> String {
    // 1. The hash of who pushed what where, each part ended by a NUL but the last.
    let mut hasher = blake3::Hasher::new();
    hasher.update(DOMAIN);
    hasher.update(identity.as_bytes());
    hasher.update(b"\0");
    hasher.update(target.as_bytes());
    hasher.update(b"\0");
    hasher.update(tip.to_string().as_bytes());
    let hash = hasher.finalize();

    // 2. The prefix, then the kept bytes in lowercase hex.
    let mut key = String::with_capacity(LAND_KEY_PREFIX.len() + 2 * KEPT_BYTES);
    key.push_str(LAND_KEY_PREFIX);
    for byte in &hash.as_bytes()[..KEPT_BYTES] {
        // Writing to a String does not fail.
        let _ = write!(key, "{byte:02x}");
    }
    key
}
