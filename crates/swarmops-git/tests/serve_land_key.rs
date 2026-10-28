//! The land key of a push to `refs/land/<target>` (`swarmops_git::serve::land_key`): its shape,
//! a golden value, and its dependence on each of the identity, the target and the tip.

use swarmops_git::oid::GitOid;
use swarmops_git::serve::land_key::{LAND_KEY_PREFIX, land_key};

/// The pushed tip of the golden case.
const TIP: &str = "aa98cdaea2b176d63ae407fbe244e3212a1c122b";

/// Another tip.
const OTHER_TIP: &str = "02f0202f51e66e6c98d6f7e420d304d6c9a95615";

/// The pusher and the target of the golden case.
const IDENTITY: &str = "person/ada";
const TARGET: &str = "main";

/// The key of [`IDENTITY`] pushing [`TIP`] to `refs/land/main`: `gp1-` and the first 30 bytes of
/// BLAKE3(`swarmops:serve-land:v1\0person/ada\0main\0aa98…122b`), in hex. A change of it changes
/// every key a client's retry finds: a new prefix, never this one. Checked against the Python
/// `blake3` package, 2026-10-29.
const GOLDEN: &str = "gp1-18d3f175e20f3ab9ef8845468ee23088fd43b3312fcfb1a93a4c6c6290dc";

/// `hex` as an oid.
fn oid(hex: &str) -> GitOid {
    GitOid::from_hex(hex.as_bytes()).unwrap()
}

/// 64 characters: the prefix and 60 lowercase hex digits, within a land's key grammar.
#[test]
fn land_key_shape() {
    let key = land_key(IDENTITY, TARGET, oid(TIP));
    assert_eq!(key.len(), 64, "{key}");
    let hex = key.strip_prefix(LAND_KEY_PREFIX).expect("the prefix");
    assert_eq!(LAND_KEY_PREFIX, "gp1-");
    assert_eq!(hex.len(), 60, "{key}");
    assert!(
        hex.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "{key}"
    );
    assert!(
        key.bytes()
            .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase() || b == b'-'),
        "{key}"
    );
}

/// The same push gives the same key, run after run: the golden value.
#[test]
fn land_key_golden() {
    assert_eq!(land_key(IDENTITY, TARGET, oid(TIP)), GOLDEN);
    assert_eq!(land_key(IDENTITY, TARGET, oid(TIP)), GOLDEN);
}

/// Another identity, another target or another tip: another key. The parts are separated, so
/// moving a byte from one to the next changes the key too.
#[test]
fn land_key_differs_per_part() {
    let base = land_key(IDENTITY, TARGET, oid(TIP));
    let others = [
        land_key("person/bob", TARGET, oid(TIP)),
        land_key(IDENTITY, "release/1", oid(TIP)),
        land_key(IDENTITY, TARGET, oid(OTHER_TIP)),
        land_key("person/adam", "ain", oid(TIP)),
    ];
    for other in &others {
        assert_ne!(*other, base);
    }
    for (i, a) in others.iter().enumerate() {
        for b in &others[i + 1..] {
            assert_ne!(a, b);
        }
    }
}
