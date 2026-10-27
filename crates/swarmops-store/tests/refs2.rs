//! Tests of FORMAT 2 ref names and ref files (the M6 design); the M6 plan's task A1 writes them.
//! The ref files themselves are unit tests of `meta::refs`: a FORMAT 2 store opens only once its
//! journal is built.

use swarmops_core::{ChangesetId, ObjectId};
use swarmops_store::{RefKind, StoreError, StoreFormat, check_move, check_ref_name_in, ref_kind};

/// A changeset id made of `byte`.
fn cs(byte: u8) -> ChangesetId {
    ChangesetId::new(ObjectId::new([byte; 32]))
}

/// The FORMAT 2 grammar (the M6 design §2.5), one row per rule: `release/<x>` keeps FORMAT 1's
/// rule, so `release/a..b` stays a name.
#[test]
fn v2_grammar_accepts_and_refuses_by_rule() {
    let longest = format!("feature/{}", "x".repeat(255 - "feature/".len()));
    let accepted = [
        "main",
        "release/x",
        "release/a..b",
        "feature/x",
        "x",
        "Main",
        "a/b",
        "tags/v1",
        "tags/v1/rc.1",
        "remote/origin/main",
        &longest,
    ];
    for name in accepted {
        check_ref_name_in(StoreFormat::V2, name).unwrap_or_else(|e| panic!("{name:?}: {e}"));
    }

    let too_long = format!("{longest}x");
    let refused = [
        ".x",
        "x.",
        "x.lock",
        "a..b",
        "a//b",
        "/a",
        "a/",
        "main/x",
        "tags",
        "tags/",
        "remote/r",
        "remote/R/x",
        "release",
        "release/.x",
        "",
        &too_long,
    ];
    for name in refused {
        let refusal = check_ref_name_in(StoreFormat::V2, name);
        assert!(
            matches!(&refusal, Err(StoreError::RefName(n)) if n == name),
            "{name:?}: {refusal:?}"
        );
    }
}

/// A tag is created and deleted, never moved; every other kind moves freely.
#[test]
fn tags_are_created_and_deleted_never_moved() {
    check_move(StoreFormat::V2, "tags/v1", None, Some(cs(1))).unwrap();
    check_move(StoreFormat::V2, "tags/v1", Some(cs(1)), None).unwrap();
    let moved = check_move(StoreFormat::V2, "tags/v1", Some(cs(1)), Some(cs(2)));
    assert!(
        matches!(&moved, Err(StoreError::TagImmutable(n)) if n == "tags/v1"),
        "{moved:?}"
    );

    for name in ["main", "release/x", "feature/x", "remote/origin/main"] {
        check_move(StoreFormat::V2, name, Some(cs(1)), Some(cs(2)))
            .unwrap_or_else(|e| panic!("{name:?}: {e}"));
    }
    assert_eq!(ref_kind("tags/v1"), Some(RefKind::Tag));
}

/// `main` is never deleted, in either format (the v0.6.0 design D8); every other kind is
/// deleted freely, and `main` is created and moved as before.
#[test]
fn check_move_refuses_deleting_main_only() {
    for format in [StoreFormat::V1, StoreFormat::V2] {
        let deleted = check_move(format, "main", Some(cs(1)), None);
        assert!(
            matches!(&deleted, Err(StoreError::RefProtected(n)) if n == "main"),
            "{format:?}: {deleted:?}"
        );
        check_move(format, "release/1", Some(cs(1)), None)
            .unwrap_or_else(|e| panic!("{format:?}: release/1: {e}"));
        check_move(format, "main", None, Some(cs(1)))
            .unwrap_or_else(|e| panic!("{format:?}: create main: {e}"));
        check_move(format, "main", Some(cs(1)), Some(cs(2)))
            .unwrap_or_else(|e| panic!("{format:?}: move main: {e}"));
    }

    // FORMAT 2's other kinds.
    for name in ["tags/v1", "feature/x", "remote/origin/main"] {
        check_move(StoreFormat::V2, name, Some(cs(1)), None)
            .unwrap_or_else(|e| panic!("{name:?}: {e}"));
    }
}
