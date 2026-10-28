//! The land key of a push to `refs/land/<target>` (M14 design §0.6): [`LAND_KEY_PREFIX`] and 60
//! hex digits of a BLAKE3 hash of the identity, the target and the pushed tip, so that the same
//! push again replays its land instead of landing twice.

use crate::oid::GitOid;

/// What every land key of `serve` starts with.
pub const LAND_KEY_PREFIX: &str = "gp1-";

/// The land key of `identity` pushing `tip` to `refs/land/<target>`. Empty until its body is
/// built: an empty key is no land key, so nothing lands with it.
pub fn land_key(identity: &str, target: &str, tip: GitOid) -> String {
    let _ = (identity, target, tip);
    String::new()
}
