//! Canonical codec tests: byte layouts, golden vectors, round trips, rejection of
//! non-canonical input, and "garbage never panics".

use std::fs;
use std::path::PathBuf;

use proptest::prelude::*;
use swarmops_core::object::varint;
use swarmops_core::{
    BlobId, Changeset, ChangesetId, CodecError, DeltaEntry, DeltaOp, EntryKind, ObjectId,
    ObjectKind, RepoPath, Signature, Tree, TreeEntry, TreeId, hash_object,
};

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

fn read_golden(name: &str) -> (Vec<u8>, ObjectId) {
    let text = fs::read_to_string(golden_dir().join(format!("{name}.hex"))).unwrap();
    let mut lines = text.lines();
    let bytes = hex::decode(lines.next().unwrap().trim()).unwrap();
    let id = ObjectId::from_hex(lines.next().unwrap().trim()).unwrap();
    (bytes, id)
}

fn blob_id(content: &[u8]) -> BlobId {
    BlobId(hash_object(ObjectKind::Blob, content))
}

fn sig(name: &[u8], email: &[u8], secs: i64, offset_secs: i32, negative_zero: bool) -> Signature {
    Signature {
        name: name.to_vec(),
        email: email.to_vec(),
        secs,
        offset_secs,
        negative_zero,
    }
}

fn one_file_tree() -> Tree {
    Tree::new(vec![TreeEntry {
        name: "a.rs".into(),
        kind: EntryKind::File,
        id: blob_id(b"x").as_object_id(),
    }])
    .unwrap()
}

fn minimal_changeset() -> Changeset {
    Changeset {
        parents: vec![],
        root: Tree::empty_root().id(),
        delta: vec![],
        author: sig(b"a", b"a@b", 0, 0, false),
        committer: sig(b"a", b"a@b", 0, 0, false),
        message: b"m".to_vec(),
        extra_headers: vec![],
        metadata: vec![],
    }
}

fn full_changeset() -> Changeset {
    let p1 = ChangesetId(hash_object(ObjectKind::Changeset, b"p1"));
    let p2 = ChangesetId(hash_object(ObjectKind::Changeset, b"p2"));
    Changeset {
        parents: vec![p1, p2],
        root: one_file_tree().id(),
        delta: vec![
            DeltaEntry {
                path: RepoPath::new("a.rs").unwrap(),
                op: DeltaOp::Put {
                    kind: EntryKind::File,
                    id: blob_id(b"x"),
                },
                renamed_from: None,
            },
            DeltaEntry {
                path: RepoPath::new("bin/run").unwrap(),
                op: DeltaOp::Put {
                    kind: EntryKind::Exec,
                    id: blob_id(b"#!/bin/sh\n"),
                },
                renamed_from: Some(RepoPath::new("run.sh").unwrap()),
            },
            DeltaEntry {
                path: RepoPath::new("old.txt").unwrap(),
                op: DeltaOp::Delete,
                renamed_from: None,
            },
        ],
        author: sig(b"Ada", b"ada@example.com", 1_700_000_000, -32_400, false),
        committer: sig(
            b"claude-code/w1",
            b"w1@swarmops.local",
            1_700_000_001,
            0,
            true,
        ),
        message: b"feat: full\n\nbody\n".to_vec(),
        extra_headers: vec![(b"gpgsig".to_vec(), b"sig".to_vec())],
        metadata: vec![("agent".to_owned(), b"claude-code/w1".to_vec())],
    }
}

#[test]
fn varint_roundtrip_and_rejects_overlong() {
    for v in [0u64, 1, 127, 128, 300, 16_383, 16_384, u64::MAX] {
        let mut buf = Vec::new();
        varint::write(&mut buf, v);
        let mut pos = 0;
        assert_eq!(varint::read(&buf, &mut pos).unwrap(), v);
        assert_eq!(pos, buf.len());
    }
    let mut pos = 0;
    assert_eq!(
        varint::read(&[0xFF; 11], &mut pos).unwrap_err(),
        CodecError::Overlong
    );
    let mut pos = 0;
    assert_eq!(
        varint::read(&[0x80, 0x00], &mut pos).unwrap_err(),
        CodecError::Overlong
    );
    let mut pos = 0;
    assert_eq!(
        varint::read(&[0x80], &mut pos).unwrap_err(),
        CodecError::Truncated
    );
}

#[test]
fn tree_encode_matches_spec_layout() {
    let id = blob_id(b"x").as_object_id();
    let t = one_file_tree();
    let mut want = b"SWTR\x01\x01\x00\x04a.rs".to_vec();
    want.extend_from_slice(&id.0);
    assert_eq!(t.encode(), want);
    assert_eq!(t.id(), TreeId(hash_object(ObjectKind::Tree, &want)));
}

#[test]
fn tree_new_rejects_unsorted_duplicate_and_empty() {
    let e = |name: &str| TreeEntry {
        name: name.into(),
        kind: EntryKind::File,
        id: blob_id(b"x").as_object_id(),
    };
    assert_eq!(
        Tree::new(vec![e("b"), e("a")]).unwrap_err(),
        CodecError::Unsorted
    );
    assert_eq!(
        Tree::new(vec![e("a"), e("a")]).unwrap_err(),
        CodecError::Duplicate
    );
    assert_eq!(Tree::new(vec![]).unwrap_err(), CodecError::EmptyNonRootTree);
    assert_eq!(Tree::empty_root().entries(), &[] as &[TreeEntry]);
}

#[test]
fn tree_decode_rejects_unsorted_and_duplicate_names() {
    let id = blob_id(b"x").as_object_id();
    let entry = |name: &[u8]| {
        let mut v = vec![0x00, u8::try_from(name.len()).unwrap()];
        v.extend_from_slice(name);
        v.extend_from_slice(&id.0);
        v
    };
    let mut unsorted = b"SWTR\x01\x02".to_vec();
    unsorted.extend(entry(b"b"));
    unsorted.extend(entry(b"a"));
    assert_eq!(Tree::decode(&unsorted).unwrap_err(), CodecError::Unsorted);
    let mut dup = b"SWTR\x01\x02".to_vec();
    dup.extend(entry(b"a"));
    dup.extend(entry(b"a"));
    assert_eq!(Tree::decode(&dup).unwrap_err(), CodecError::Duplicate);
    let mut trailing = one_file_tree().encode();
    trailing.push(0);
    assert_eq!(
        Tree::decode(&trailing).unwrap_err(),
        CodecError::TrailingBytes
    );
}

#[test]
fn tree_golden_vectors() {
    for (name, tree) in [
        ("empty_tree", Tree::empty_root()),
        ("one_file_tree", one_file_tree()),
    ] {
        let (bytes, id) = read_golden(name);
        assert_eq!(tree.encode(), bytes, "{name}: encoding drifted");
        assert_eq!(Tree::decode(&bytes).unwrap(), tree, "{name}: decode");
        assert_eq!(tree.id().as_object_id(), id, "{name}: id drifted");
    }
}

#[test]
fn changeset_golden_vectors() {
    for (name, cs) in [
        ("minimal_changeset", minimal_changeset()),
        ("full_changeset", full_changeset()),
    ] {
        let (bytes, id) = read_golden(name);
        assert_eq!(cs.encode().unwrap(), bytes, "{name}: encoding drifted");
        assert_eq!(Changeset::decode(&bytes).unwrap(), cs, "{name}: decode");
        assert_eq!(cs.id().unwrap().as_object_id(), id, "{name}: id drifted");
        assert_eq!(
            cs.delta_hash().unwrap(),
            Changeset::decode(&bytes).unwrap().delta_hash().unwrap()
        );
    }
    assert_ne!(
        minimal_changeset().delta_hash().unwrap(),
        full_changeset().delta_hash().unwrap()
    );
}

#[test]
fn signature_roundtrip_negative_zero() {
    for (offset, neg) in [(-32_400, false), (0, true), (0, false), (19_800, false)] {
        let mut cs = minimal_changeset();
        cs.author = sig(b"n", b"e", -1, offset, neg);
        let back = Changeset::decode(&cs.encode().unwrap()).unwrap();
        assert_eq!(back.author.offset_secs, offset);
        assert_eq!(back.author.negative_zero, neg);
        assert_eq!(back, cs);
    }
    let mut bad = minimal_changeset();
    bad.author = sig(b"n", b"e", 0, 60, true);
    assert_eq!(bad.encode().unwrap_err(), CodecError::BadSignature);
}

#[test]
fn changeset_rejects_unsorted_delta_and_metadata() {
    let mut cs = full_changeset();
    cs.delta.swap(0, 2);
    assert_eq!(cs.encode().unwrap_err(), CodecError::Unsorted);
    let mut cs = full_changeset();
    cs.metadata = vec![("z".to_owned(), vec![]), ("a".to_owned(), vec![])];
    assert_eq!(cs.encode().unwrap_err(), CodecError::UnsortedMetadata);
    let mut cs = full_changeset();
    cs.delta[0].op = DeltaOp::Put {
        kind: EntryKind::Dir,
        id: blob_id(b"x"),
    };
    assert_eq!(cs.encode().unwrap_err(), CodecError::BadKind(3));
}

#[test]
fn decode_truncated_inputs_error_without_panic() {
    let bytes = full_changeset().encode().unwrap();
    for len in 0..bytes.len() {
        assert!(Changeset::decode(&bytes[..len]).is_err(), "prefix {len}");
    }
    let tree = one_file_tree().encode();
    for len in 0..tree.len() {
        assert!(Tree::decode(&tree[..len]).is_err(), "prefix {len}");
    }
}

fn arb_name() -> impl Strategy<Value = String> {
    "[a-z0-9._-]{1,8}".prop_filter("no dot components", |s| s != "." && s != "..")
}

fn arb_kind() -> impl Strategy<Value = EntryKind> {
    prop_oneof![
        Just(EntryKind::File),
        Just(EntryKind::Exec),
        Just(EntryKind::Symlink),
        Just(EntryKind::Dir)
    ]
}

fn arb_id() -> impl Strategy<Value = ObjectId> {
    any::<[u8; 32]>().prop_map(ObjectId)
}

fn arb_sorted_entries() -> impl Strategy<Value = Vec<TreeEntry>> {
    proptest::collection::vec((arb_name(), arb_kind(), arb_id()), 1..6).prop_map(|v| {
        let mut entries: Vec<TreeEntry> = v
            .into_iter()
            .map(|(name, kind, id)| TreeEntry { name, kind, id })
            .collect();
        entries.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
        entries.dedup_by(|a, b| a.name == b.name);
        entries
    })
}

fn arb_repo_path() -> impl Strategy<Value = RepoPath> {
    proptest::collection::vec(arb_name(), 1..4).prop_map(|c| RepoPath::new(c.join("/")).unwrap())
}

fn arb_signature() -> impl Strategy<Value = Signature> {
    (
        proptest::collection::vec(any::<u8>(), 0..8),
        proptest::collection::vec(any::<u8>(), 0..8),
        any::<i64>(),
        -50_400i32..=50_400,
        any::<bool>(),
    )
        .prop_map(|(name, email, secs, offset_secs, neg)| Signature {
            name,
            email,
            secs,
            offset_secs,
            negative_zero: offset_secs == 0 && neg,
        })
}

fn arb_delta() -> impl Strategy<Value = Vec<DeltaEntry>> {
    let leaf_kind = prop_oneof![
        Just(EntryKind::File),
        Just(EntryKind::Exec),
        Just(EntryKind::Symlink)
    ];
    let op = prop_oneof![
        (leaf_kind, arb_id()).prop_map(|(kind, id)| DeltaOp::Put {
            kind,
            id: BlobId(id)
        }),
        Just(DeltaOp::Delete)
    ];
    proptest::collection::vec(
        (arb_repo_path(), op, proptest::option::of(arb_repo_path())),
        0..5,
    )
    .prop_map(|v| {
        let mut delta: Vec<DeltaEntry> = v
            .into_iter()
            .map(|(path, op, renamed_from)| DeltaEntry {
                path,
                op,
                renamed_from,
            })
            .collect();
        delta.sort_by(|a, b| a.path.as_str().as_bytes().cmp(b.path.as_str().as_bytes()));
        delta.dedup_by(|a, b| a.path == b.path);
        delta
    })
}

fn arb_changeset() -> impl Strategy<Value = Changeset> {
    (
        proptest::collection::vec(arb_id().prop_map(ChangesetId), 0..3),
        arb_id().prop_map(TreeId),
        arb_delta(),
        arb_signature(),
        arb_signature(),
        proptest::collection::vec(any::<u8>(), 0..32),
        proptest::collection::vec(
            (
                proptest::collection::vec(any::<u8>(), 1..5),
                proptest::collection::vec(any::<u8>(), 0..8),
            ),
            0..3,
        ),
        proptest::collection::btree_map(
            arb_name(),
            proptest::collection::vec(any::<u8>(), 0..8),
            0..3,
        ),
    )
        .prop_map(
            |(parents, root, delta, author, committer, message, extra_headers, metadata)| {
                Changeset {
                    parents,
                    root,
                    delta,
                    author,
                    committer,
                    message,
                    extra_headers,
                    metadata: metadata.into_iter().collect(),
                }
            },
        )
}

proptest! {
    #[test]
    fn tree_roundtrip(entries in arb_sorted_entries()) {
        let t = Tree::new(entries).unwrap();
        prop_assert_eq!(Tree::decode(&t.encode()).unwrap(), t);
    }

    #[test]
    fn changeset_roundtrip(cs in arb_changeset()) {
        let bytes = cs.encode().unwrap();
        prop_assert_eq!(Changeset::decode(&bytes).unwrap(), cs);
    }

    #[test]
    fn decode_garbage_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
        let _ = Tree::decode(&bytes);
        let _ = Changeset::decode(&bytes);
    }
}

/// Regenerates the golden files from the fixed inputs above. Run once, commit the
/// output; the regular golden tests then pin it.
#[test]
#[ignore = "writes tests/golden; run on purpose"]
fn regenerate_golden() {
    let items: Vec<(&str, Vec<u8>, ObjectId)> = vec![
        (
            "empty_tree",
            Tree::empty_root().encode(),
            Tree::empty_root().id().as_object_id(),
        ),
        (
            "one_file_tree",
            one_file_tree().encode(),
            one_file_tree().id().as_object_id(),
        ),
        (
            "minimal_changeset",
            minimal_changeset().encode().unwrap(),
            minimal_changeset().id().unwrap().as_object_id(),
        ),
        (
            "full_changeset",
            full_changeset().encode().unwrap(),
            full_changeset().id().unwrap().as_object_id(),
        ),
    ];
    for (name, bytes, id) in items {
        let text = format!("{}\n{}\n", hex::encode(&bytes), id.to_hex());
        fs::write(golden_dir().join(format!("{name}.hex")), text).unwrap();
    }
}
