use swarmops_core::{IdError, ObjectId, ObjectKind, hash_object};

#[test]
fn object_id_equals_blake3_of_domain_len_and_payload() {
    let payload = b"hello";
    let mut h = blake3::Hasher::new();
    h.update(b"swarmops:blob:v1\0");
    h.update(&u64::try_from(payload.len()).unwrap().to_le_bytes());
    h.update(payload);
    assert_eq!(
        hash_object(ObjectKind::Blob, payload).0,
        *h.finalize().as_bytes()
    );
}

#[test]
fn same_payload_different_kind_gives_different_id() {
    assert_ne!(
        hash_object(ObjectKind::Blob, b"x"),
        hash_object(ObjectKind::Tree, b"x")
    );
    assert_ne!(
        hash_object(ObjectKind::Tree, b"x"),
        hash_object(ObjectKind::Changeset, b"x")
    );
}

#[test]
fn object_id_hex_parse_cases() {
    let id = hash_object(ObjectKind::Blob, b"x");
    assert_eq!(ObjectId::from_hex(&id.to_hex()).unwrap(), id);
    assert_eq!(ObjectId::from_hex(&id.to_hex().to_uppercase()).unwrap(), id);
    assert_eq!(id.to_hex().len(), 64);
    assert!(
        id.to_hex()
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    );
    assert_eq!(id.to_string(), id.to_hex());
    assert!(matches!(ObjectId::from_hex("abc"), Err(IdError::Length(3))));
    assert!(matches!(
        ObjectId::from_hex(&"zz".repeat(32)),
        Err(IdError::NonHex)
    ));
}

/// `Display` and `Debug` print what they printed before they wrote from a stack buffer: the
/// lowercase hex, inside `ObjectId::new(…)` for `Debug`, with formatting flags such as a width
/// ignored; the typed ids print the same. A guard: it passes before and after.
#[test]
fn display_and_debug_are_unchanged() {
    use swarmops_core::{BlobId, ChangesetId, TreeId};

    for id in [
        hash_object(ObjectKind::Blob, b"x"),
        ObjectId::new([0; 32]),
        ObjectId::new([0xff; 32]),
    ] {
        let hex = id.to_hex();
        assert_eq!(format!("{id}"), hex);
        assert_eq!(format!("{id:?}"), format!("ObjectId({hex})"));
        assert_eq!(format!("{id:>70}"), hex, "no padding");
        assert_eq!(format!("{id:<70}|"), format!("{hex}|"), "no padding");
        assert_eq!(format!("{id:.8}"), hex, "no truncation");
        assert_eq!(format!("{}", BlobId::new(id)), hex);
        assert_eq!(format!("{}", TreeId::new(id)), hex);
        assert_eq!(format!("{}", ChangesetId::new(id)), hex);
        assert_eq!(
            format!("{:?}", BlobId::new(id)),
            format!("BlobId(ObjectId({hex}))")
        );
    }
}

/// `Hashed::new` hashes its payload once, as `hash_object` does, and keeps it: for each kind the
/// id is the hash of the payload it carries, and `into_payload` hands back the very `Vec` it was
/// given, not a copy.
#[test]
fn hashed_new_is_hash_object() {
    use swarmops_core::Hashed;

    for kind in [ObjectKind::Blob, ObjectKind::Tree, ObjectKind::Changeset] {
        for payload in [Vec::new(), b"x".to_vec(), vec![7; 4096]] {
            let hashed = Hashed::new(kind, payload.clone());
            assert_eq!(hashed.id(), hash_object(kind, &payload), "{kind:?}");
            assert_eq!(hashed.kind(), kind);
            assert_eq!(hashed.payload(), payload.as_slice());
        }
        let payload = vec![7; 4096];
        let at = payload.as_ptr();
        let kept = Hashed::new(kind, payload).into_payload();
        assert_eq!(
            kept.as_ptr(),
            at,
            "{kind:?}: the payload is kept, not copied"
        );
        assert_eq!(kept, [7; 4096]);
    }
}

/// An object file's header writes a kind as the byte `tag` gives: 0 blob, 1 tree, 2 changeset,
/// the bytes the store wrote before the tag moved here. `from_tag` reads them back and no other.
#[test]
fn object_kind_tag_round_trips() {
    let kinds = [
        (ObjectKind::Blob, 0),
        (ObjectKind::Tree, 1),
        (ObjectKind::Changeset, 2),
    ];
    for (kind, byte) in kinds {
        assert_eq!(kind.tag(), byte, "{kind:?}");
        assert_eq!(ObjectKind::from_tag(byte), Some(kind));
    }

    // Every other byte names no kind.
    for byte in 3..=u8::MAX {
        assert_eq!(ObjectKind::from_tag(byte), None, "{byte}");
    }
}

/// The constructors wrap what they are given: an id's digest, a typed id's untyped one.
#[test]
fn id_constructors_wrap_their_input() {
    use swarmops_core::{BlobId, ChangesetId, TreeId};

    let id = ObjectId::new([7; 32]);
    assert_eq!(id.0, [7; 32]);
    assert_eq!(ObjectId::from([7; 32]), id);
    assert_eq!(BlobId::new(id).as_object_id(), id);
    assert_eq!(TreeId::new(id).as_object_id(), id);
    assert_eq!(ChangesetId::new(id).as_object_id(), id);
}
