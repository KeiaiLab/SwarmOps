//! The land key of a change native sync lands on an upstream (native sync design, its
//! corrections, "Land key"): [`SYNC_KEY_PREFIX`] and 60 hex digits of a BLAKE3 hash of the
//! target and the change's id. The upstream derives it from a land's `target` and `head`, so
//! one change lands once per target, whoever sends it.
//!
//! ```text
//! BLAKE3("swarmops:sync-land:v1\0" ‖ target ‖ \0 ‖ change id hex)
//!   └─ first 30 bytes, lowercase hex ─▶ "ss1-" + 60 hex = 64 characters
//! ```
//!
//! 64 characters of `[0-9a-z-]`: within a land's `idempotency_key` grammar (1 to 64 letters,
//! digits and `-`), the form `serve`'s `gp1-` key has. The bytes hashed are a contract: a change
//! of them is a new prefix.

use std::fmt::Write as _;

use swarmops_core::ChangesetId;

/// What every land key of native sync starts with.
pub(crate) const SYNC_KEY_PREFIX: &str = "ss1-";

/// What the hash of a land key starts with: its domain and version.
const DOMAIN: &[u8] = b"swarmops:sync-land:v1\0";

/// How many bytes of the hash a land key keeps: 60 hex digits.
const KEPT_BYTES: usize = 30;

/// The land key of `change` landed on `target`.
#[allow(dead_code, reason = "serve --stdio (N-3) and sync (N-5) call it")]
pub(crate) fn sync_key(target: &str, change: ChangesetId) -> String {
    // 1. The hash of which change lands where, the two parts ended by a NUL but the last.
    let mut hasher = blake3::Hasher::new();
    hasher.update(DOMAIN);
    hasher.update(target.as_bytes());
    hasher.update(b"\0");
    hasher.update(change.to_string().as_bytes());
    let hash = hasher.finalize();

    // 2. The prefix, then the kept bytes in lowercase hex.
    let mut key = String::with_capacity(SYNC_KEY_PREFIX.len() + 2 * KEPT_BYTES);
    key.push_str(SYNC_KEY_PREFIX);
    for byte in &hash.as_bytes()[..KEPT_BYTES] {
        // Writing to a String does not fail.
        let _ = write!(key, "{byte:02x}");
    }
    key
}

#[cfg(test)]
mod tests {
    use camino::Utf8PathBuf;
    use serde_json::json;
    use swarmops_api::{
        ChangeInput, ChangeOp, CommitReq, ContentInput, Identity, LandReq, LandStatus, Rev,
        StoreOps,
    };
    use swarmops_core::ObjectId;

    use super::*;
    use crate::LocalOps;

    /// A changeset id of `byte`s.
    fn change(byte: u8) -> ChangesetId {
        ChangesetId::new(ObjectId::new([byte; 32]))
    }

    /// Commits a put of `path` on `parent`, advancing `advance`; the changeset's id.
    fn put(
        ops: &LocalOps,
        who: &Identity,
        parent: Option<&str>,
        path: &str,
        advance: Option<&str>,
    ) -> String {
        let mut change_input = ChangeInput::new(path.to_owned(), ChangeOp::Put);
        change_input.content = Some(ContentInput::Inline(format!("{path}\n")));
        let mut req = CommitReq::new(vec![change_input], path.to_owned());
        req.parent = parent.map(Rev::from);
        req.advance = advance.map(str::to_owned);
        ops.commit(who, req).unwrap().changeset
    }

    /// The key is 64 characters of `[0-9a-z-]` after its prefix, and a land takes it as its
    /// idempotency key: landed once, then replayed.
    #[test]
    fn the_sync_key_fits_the_land_key_grammar() {
        // 1. Its form.
        let key = sync_key("main", change(1));
        assert_eq!(key.len(), 64);
        assert!(key.starts_with(SYNC_KEY_PREFIX), "{key}");
        let allowed = |b: u8| b.is_ascii_digit() || b.is_ascii_lowercase() || b == b'-';
        assert!(key.bytes().all(allowed), "{key}");

        // 2. A land of a change under its key, twice.
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
        let (ops, _) = LocalOps::init(&root, "acme").unwrap();
        let who = Identity::parse("agent/worker-17").unwrap();
        let base = put(&ops, &who, None, "a", Some("main"));
        let head = put(&ops, &who, Some(&base), "b", None);
        let key = sync_key("main", head.parse::<ObjectId>().unwrap().into());
        let land = || -> LandReq {
            let fields = json!({"head": head, "target": "main", "idempotency_key": key});
            serde_json::from_value(fields).unwrap()
        };
        let landed = ops.land(&who, land()).unwrap();
        assert_eq!(landed.status, LandStatus::Landed);
        let again = ops.land(&who, land()).unwrap();
        assert_eq!(
            (again.status, again.landed),
            (LandStatus::Replayed, landed.landed)
        );
    }

    /// One change has a key per target, and two changes on one target have two.
    #[test]
    fn the_sync_key_differs_by_target() {
        let main = sync_key("main", change(1));
        assert_ne!(main, sync_key("release/1", change(1)));
        assert_ne!(main, sync_key("main", change(2)));
        assert_eq!(main, sync_key("main", change(1)));
    }

    /// The bytes hashed are a contract: this key changes only with a new prefix.
    #[test]
    fn the_sync_key_is_pinned() {
        assert_eq!(sync_key("main", change(1)), PINNED);
    }

    /// `sync_key("main", 01…01)`, as the recipe of the module's drawing gives it (checked
    /// against a second BLAKE3 implementation).
    const PINNED: &str = "ss1-2375b311e9cc1dfa2dbab0c079cf11e53726bd5157009dffa64b4cabe511";
}
