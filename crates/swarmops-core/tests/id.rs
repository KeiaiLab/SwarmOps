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
