//! Canonical codec tests: byte layouts, golden vectors, round trips, rejection of
//! non-canonical input, and "garbage never panics".

use std::fs;
use std::path::PathBuf;

use proptest::prelude::*;
use swarmops_core::object::varint;
use swarmops_core::{
    BlobId, Changeset, ChangesetId, CodecError, DeltaEntry, DeltaOp, EntryKind, EntryTarget,
    LeafKind, ObjectId, ObjectKind, RepoPath, Signature, Tree, TreeEntry, TreeId, hash_object,
};

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

/// Golden file: line 1 payload hex, line 2 id hex, line 3 (changesets only) delta_hash hex.
fn read_golden(name: &str) -> (Vec<u8>, ObjectId, Option<[u8; 32]>) {
    let text = fs::read_to_string(golden_dir().join(format!("{name}.hex"))).unwrap();
    let mut lines = text.lines();
    let bytes = hex::decode(lines.next().unwrap().trim()).unwrap();
    let id = ObjectId::from_hex(lines.next().unwrap().trim()).unwrap();
    let delta_hash = lines.next().map(|l| {
        let mut out = [0u8; 32];
        hex::decode_to_slice(l.trim(), &mut out).unwrap();
        out
    });
    (bytes, id, delta_hash)
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
    Tree::new(vec![TreeEntry::leaf("a.rs", LeafKind::File, blob_id(b"x"))]).unwrap()
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
                    kind: LeafKind::File,
                    id: blob_id(b"x"),
                },
                renamed_from: None,
            },
            DeltaEntry {
                path: RepoPath::new("bin/run").unwrap(),
                op: DeltaOp::Put {
                    kind: LeafKind::Exec,
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

/// An entry's target is laid out as the `(kind, id)` pair it replaced: the kind byte, then the
/// blob's or the tree's id. A one-entry tree of each target encodes to
/// `SWTR | 01 | 01 | kind | 01 'a' | id`, has the id of those bytes and decodes back to the same
/// entry; the golden trees decode to the targets they were made of.
#[test]
fn a_tree_entry_target_round_trips_through_the_byte_layout() {
    let blob = blob_id(b"x");
    let dir = one_file_tree().id();
    for (target, kind, byte) in [
        (EntryTarget::Leaf(LeafKind::File, blob), EntryKind::File, 0),
        (EntryTarget::Leaf(LeafKind::Exec, blob), EntryKind::Exec, 1),
        (
            EntryTarget::Leaf(LeafKind::Symlink, blob),
            EntryKind::Symlink,
            2,
        ),
        (EntryTarget::Dir(dir), EntryKind::Dir, 3),
    ] {
        assert_eq!((target.kind(), target.kind().as_u8()), (kind, byte));
        let entry = TreeEntry {
            name: "a".into(),
            target,
        };
        let tree = Tree::new(vec![entry.clone()]).unwrap();
        let mut want = b"SWTR\x01\x01".to_vec();
        want.extend_from_slice(&[byte, 1, b'a']);
        want.extend_from_slice(&target.id().0);
        assert_eq!(tree.encode(), want, "{kind:?}");
        let id = TreeId(hash_object(ObjectKind::Tree, &want));
        assert_eq!(tree.id(), id, "{kind:?}");
        assert_eq!(Tree::decode(&want).unwrap().entries(), [entry], "{kind:?}");
    }
    assert_eq!(EntryTarget::Leaf(LeafKind::Exec, blob).id(), blob.0);
    assert_eq!(EntryTarget::Dir(dir).id(), dir.0);
    for (leaf, kind) in [
        (LeafKind::File, EntryKind::File),
        (LeafKind::Exec, EntryKind::Exec),
        (LeafKind::Symlink, EntryKind::Symlink),
    ] {
        assert_eq!(leaf.entry_kind(), kind);
        assert_eq!(leaf.as_u8(), kind.as_u8());
        assert_eq!(LeafKind::from_u8(kind.as_u8()), Ok(leaf));
    }
    let (bytes, _, _) = read_golden("one_file_tree");
    assert_eq!(
        Tree::decode(&bytes).unwrap().entries(),
        [TreeEntry::leaf("a.rs", LeafKind::File, blob)]
    );
    assert_eq!(
        Tree::decode(&bytes).unwrap().entries()[0].target,
        EntryTarget::Leaf(LeafKind::File, blob)
    );
    let (bytes, _, _) = read_golden("empty_tree");
    assert_eq!(Tree::decode(&bytes).unwrap(), Tree::empty_root());
}

/// Each accessor names the one variant it asks for and `None` for the other.
#[test]
fn the_accessors_name_one_variant() {
    let blob = blob_id(b"x");
    let dir = one_file_tree().id();

    // A tree entry's target: a leaf or a subdirectory.
    let leaf = EntryTarget::Leaf(LeafKind::Exec, blob);
    assert_eq!(leaf.leaf(), Some((LeafKind::Exec, blob)));
    assert_eq!(leaf.dir(), None);
    assert_eq!(EntryTarget::Dir(dir).leaf(), None);
    assert_eq!(EntryTarget::Dir(dir).dir(), Some(dir));

    // A delta's op: a put or a delete.
    let put = DeltaOp::Put {
        kind: LeafKind::Symlink,
        id: blob,
    };
    assert_eq!(put.put(), Some((LeafKind::Symlink, blob)));
    assert_eq!(DeltaOp::Delete.put(), None);
}

/// The constructors refuse what `encode` refuses, with the same error, and keep what they are
/// given.
#[test]
fn the_constructors_check_what_encode_checks() {
    let path = |p: &str| RepoPath::new(p).unwrap();
    let put = DeltaOp::Put {
        kind: LeafKind::File,
        id: blob_id(b"x"),
    };

    // A signature: `-0000` only with offset zero.
    assert_eq!(
        Signature::new(b"n".to_vec(), b"e".to_vec(), 0, 60, true).unwrap_err(),
        CodecError::BadSignature
    );
    let ok = Signature::new(b"n".to_vec(), b"e".to_vec(), 5, 0, true).unwrap();
    assert_eq!((ok.secs, ok.offset_secs, ok.negative_zero), (5, 0, true));

    // A delta entry: a rename only on a put, never from its own path.
    let refused = [
        DeltaEntry::new(path("a"), DeltaOp::Delete, Some(path("b"))),
        DeltaEntry::new(path("a"), put.clone(), Some(path("a"))),
    ];
    for entry in refused {
        assert_eq!(entry.unwrap_err(), CodecError::BadRename);
    }
    let renamed = DeltaEntry::new(path("a"), put.clone(), Some(path("b"))).unwrap();
    assert_eq!(renamed.renamed_from, Some(path("b")));

    // A changeset: what `validate` checks, here a delta out of order.
    let cs = minimal_changeset();
    let entry = |p: &str| DeltaEntry::new(path(p), put.clone(), None).unwrap();
    let unsorted = Changeset::new(
        cs.parents.clone(),
        cs.root,
        vec![entry("b"), entry("a")],
        cs.author.clone(),
        cs.committer.clone(),
        cs.message.clone(),
        cs.extra_headers.clone(),
        cs.metadata.clone(),
    );
    assert_eq!(unsorted.unwrap_err(), CodecError::Unsorted);
    let same = Changeset::new(
        cs.parents.clone(),
        cs.root,
        cs.delta.clone(),
        cs.author.clone(),
        cs.committer.clone(),
        cs.message.clone(),
        cs.extra_headers.clone(),
        cs.metadata.clone(),
    );
    assert_eq!(same.unwrap(), cs);

    // A tree entry: its name is checked where it becomes a tree.
    let target = EntryTarget::Leaf(LeafKind::File, blob_id(b"x"));
    let entry = TreeEntry::new("a/b", target);
    assert_eq!((entry.name.as_str(), entry.target), ("a/b", target));
    assert!(Tree::new(vec![entry]).is_err());
}

#[test]
fn tree_new_rejects_unsorted_duplicate_and_empty() {
    let e = |name: &str| TreeEntry::leaf(name, LeafKind::File, blob_id(b"x"));
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
        let (bytes, id, _) = read_golden(name);
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
        let (bytes, id, pinned_delta_hash) = read_golden(name);
        assert_eq!(cs.encode().unwrap(), bytes, "{name}: encoding drifted");
        assert_eq!(Changeset::decode(&bytes).unwrap(), cs, "{name}: decode");
        assert_eq!(cs.id().unwrap().as_object_id(), id, "{name}: id drifted");
        assert_eq!(
            cs.delta_hash().unwrap(),
            pinned_delta_hash.expect("golden line 3 pins delta_hash"),
            "{name}: delta_hash boundary drifted"
        );
    }
    assert_ne!(
        minimal_changeset().delta_hash().unwrap(),
        full_changeset().delta_hash().unwrap()
    );
}

/// `encode_hashed` gives the golden vectors' bytes, id and delta hash from one encoding: the hash
/// contract did not move.
#[test]
fn encode_hashed_gives_the_golden_vectors() {
    for (name, cs) in [
        ("minimal_changeset", minimal_changeset()),
        ("full_changeset", full_changeset()),
    ] {
        let (bytes, id, pinned_delta_hash) = read_golden(name);
        let (object, delta_hash) = cs.encode_hashed().unwrap();
        assert_eq!(object.kind(), ObjectKind::Changeset, "{name}");
        assert_eq!(
            object.payload(),
            bytes.as_slice(),
            "{name}: encoding drifted"
        );
        assert_eq!(object.id(), id, "{name}: id drifted");
        assert_eq!(
            Some(delta_hash),
            pinned_delta_hash,
            "{name}: delta_hash drifted"
        );
    }
}

/// `delta_hash` covers exactly the delta section (`varint m` + entries): the same
/// delta on a different base, with a different message or metadata, hashes the same;
/// a different delta hashes differently. The literal pins the section boundary
/// independently of the golden file.
#[test]
fn delta_hash_depends_only_on_delta_section() {
    let base = full_changeset();
    let expected = base.delta_hash().unwrap();
    assert_eq!(
        hex::encode(expected),
        "24cc96bb154ea7252fecce3a87c4fe6e799d784ad213ad5a79e9ba7ae05fccaf"
    );
    let mut other_base = base.clone();
    other_base.parents = vec![];
    other_base.root = Tree::empty_root().id();
    other_base.message = b"different".to_vec();
    other_base.metadata = vec![];
    other_base.extra_headers = vec![];
    other_base.author = sig(b"x", b"y", 1, 0, false);
    assert_eq!(other_base.delta_hash().unwrap(), expected);
    let mut other_delta = base.clone();
    other_delta.delta.pop();
    assert_ne!(other_delta.delta_hash().unwrap(), expected);
    assert_ne!(minimal_changeset().delta_hash().unwrap(), expected);
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
}

/// A delta put names a leaf, never a directory: its kind is a `LeafKind`, which cannot say
/// `Dir`, so no encoder writes one, and a put whose kind byte is 3 (`Dir`), or any byte past
/// the leaf kinds, does not decode.
#[test]
fn a_delta_put_of_a_directory_does_not_decode() {
    let mut cs = minimal_changeset();
    cs.delta = vec![DeltaEntry {
        path: RepoPath::new("a.rs").unwrap(),
        op: DeltaOp::Put {
            kind: LeafKind::File,
            id: blob_id(b"x"),
        },
        renamed_from: None,
    }];
    // Control: the put of a file is canonical and round-trips.
    let valid = cs.encode().unwrap();
    assert_eq!(Changeset::decode(&valid).unwrap(), cs);

    // The layout is `len|path | op | kind | id | has_rename`: a put's kind byte follows its op
    // byte (0).
    let put = |kind: u8| [len_prefixed("a.rs").as_slice(), &[0, kind]].concat();
    for byte in [EntryKind::Dir.as_u8(), 4, u8::MAX] {
        assert_eq!(LeafKind::from_u8(byte), Err(CodecError::BadKind(byte)));
        let tampered = splice_once(&valid, &put(LeafKind::File.as_u8()), &put(byte));
        assert_eq!(
            Changeset::decode(&tampered).unwrap_err(),
            CodecError::BadKind(byte),
            "kind byte {byte}"
        );
    }
}

/// `varint len | bytes` as the layout writes a string; every string used with it
/// here is shorter than 128 bytes, so the varint is a single byte.
fn len_prefixed(s: &str) -> Vec<u8> {
    let mut out = vec![u8::try_from(s.len()).unwrap()];
    out.extend_from_slice(s.as_bytes());
    out
}

/// Replaces the single occurrence of `from` in `bytes` with `to`. Panics when `from`
/// is absent or ambiguous, so a drifting layout cannot turn a test into a no-op.
fn splice_once(bytes: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    let mut hits = bytes
        .windows(from.len())
        .enumerate()
        .filter_map(|(at, window)| (window == from).then_some(at));
    let at = hits.next().expect("pattern present in the encoding");
    assert!(hits.next().is_none(), "pattern is unique in the encoding");
    [&bytes[..at], to, &bytes[at + from.len()..]].concat()
}

#[test]
fn changeset_rejects_delete_with_renamed_from() {
    let mut cs = minimal_changeset();
    cs.delta = vec![DeltaEntry {
        path: RepoPath::new("old.txt").unwrap(),
        op: DeltaOp::Delete,
        renamed_from: None,
    }];
    // Control: the plain delete is canonical and round-trips.
    let valid = cs.encode().unwrap();
    assert_eq!(Changeset::decode(&valid).unwrap(), cs);

    // Encoder: a deleted path has no content to have been renamed from.
    cs.delta[0].renamed_from = Some(RepoPath::new("was.txt").unwrap());
    assert_eq!(cs.encode().unwrap_err(), CodecError::BadRename);

    // Decoder: the same entry as raw bytes. The layout is `len|path | op | has_rename |
    // [len|from]`, so `old.txt, Delete, no rename` becomes `old.txt, Delete, from was.txt`.
    let path = len_prefixed("old.txt");
    let plain = [path.as_slice(), &[1, 0]].concat();
    let renamed = [path.as_slice(), &[1, 1], &len_prefixed("was.txt")].concat();
    let tampered = splice_once(&valid, &plain, &renamed);
    assert_eq!(
        Changeset::decode(&tampered).unwrap_err(),
        CodecError::BadRename
    );
}

#[test]
fn changeset_rejects_rename_from_self() {
    let path = RepoPath::new("a.rs").unwrap();
    let mut cs = minimal_changeset();
    cs.delta = vec![DeltaEntry {
        path: path.clone(),
        op: DeltaOp::Put {
            kind: LeafKind::File,
            id: blob_id(b"x"),
        },
        renamed_from: Some(RepoPath::new("b.rs").unwrap()),
    }];
    // Control: a rename from a different path is canonical and round-trips.
    let valid = cs.encode().unwrap();
    assert_eq!(Changeset::decode(&valid).unwrap(), cs);

    // Encoder: a path cannot be renamed from itself.
    cs.delta[0].renamed_from = Some(path);
    assert_eq!(cs.encode().unwrap_err(), CodecError::BadRename);

    // Decoder: point the rename source ("b.rs", the same length as "a.rs") at the
    // entry's own path.
    let tampered = splice_once(&valid, b"b.rs", b"a.rs");
    assert_eq!(
        Changeset::decode(&tampered).unwrap_err(),
        CodecError::BadRename
    );
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

/// One of the four targets an entry can name, each as likely as the others.
fn arb_target() -> impl Strategy<Value = EntryTarget> {
    prop_oneof![
        arb_id().prop_map(|id| EntryTarget::Leaf(LeafKind::File, BlobId(id))),
        arb_id().prop_map(|id| EntryTarget::Leaf(LeafKind::Exec, BlobId(id))),
        arb_id().prop_map(|id| EntryTarget::Leaf(LeafKind::Symlink, BlobId(id))),
        arb_id().prop_map(|id| EntryTarget::Dir(TreeId(id)))
    ]
}

fn arb_id() -> impl Strategy<Value = ObjectId> {
    any::<[u8; 32]>().prop_map(ObjectId)
}

fn arb_sorted_entries() -> impl Strategy<Value = Vec<TreeEntry>> {
    proptest::collection::vec((arb_name(), arb_target()), 1..6).prop_map(|v| {
        let mut entries: Vec<TreeEntry> = v
            .into_iter()
            .map(|(name, target)| TreeEntry { name, target })
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
        Just(LeafKind::File),
        Just(LeafKind::Exec),
        Just(LeafKind::Symlink)
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
            .map(|(path, op, from)| {
                // Canonical renames only: a `Put`, and never from its own path.
                let renamed_from = match op {
                    DeltaOp::Put { .. } => from.filter(|source| *source != path),
                    DeltaOp::Delete => None,
                };
                DeltaEntry {
                    path,
                    op,
                    renamed_from,
                }
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

/// `cs` made invalid in one of four ways, each an error `encode` reports: a delete that names a
/// rename source (`BadRename`), an author or a committer whose `-0000` has an offset
/// (`BadSignature`), or metadata keys out of order (`UnsortedMetadata`).
fn made_invalid(mut cs: Changeset, how: usize) -> Changeset {
    match how % 4 {
        0 => cs.delta.push(DeltaEntry {
            path: RepoPath::new("zz/deleted").unwrap(),
            op: DeltaOp::Delete,
            renamed_from: Some(RepoPath::new("zz/source").unwrap()),
        }),
        1 => {
            cs.author.negative_zero = true;
            cs.author.offset_secs = 60;
        }
        2 => {
            cs.committer.negative_zero = true;
            cs.committer.offset_secs = -60;
        }
        _ => {
            cs.metadata.push(("~".to_owned(), Vec::new()));
            cs.metadata.push(("a".to_owned(), Vec::new()));
        }
    }
    cs
}

proptest! {
    /// `encode_hashed` gives what `encode`, `id` and `delta_hash` give, which each encode the
    /// changeset again: its bytes as an object of kind `Changeset`, the id that hashes them, and
    /// the delta hash. A changeset made invalid is refused by all four with the same error.
    #[test]
    fn encode_hashed_agrees_with_encode_id_and_delta_hash(
        cs in arb_changeset(),
        how in 0..4_usize
    ) {
        let (object, delta_hash) = cs.encode_hashed().unwrap();
        prop_assert_eq!(object.kind(), ObjectKind::Changeset);
        let bytes = cs.encode().unwrap();
        prop_assert_eq!(object.payload(), bytes.as_slice());
        prop_assert_eq!(object.id(), cs.id().unwrap().0);
        prop_assert_eq!(delta_hash, cs.delta_hash().unwrap());

        let bad = made_invalid(cs, how);
        let refused = bad.encode().unwrap_err();
        prop_assert_eq!(bad.encode_hashed().unwrap_err(), refused);
        prop_assert_eq!(bad.id().unwrap_err(), refused);
        prop_assert_eq!(bad.delta_hash().unwrap_err(), refused);
    }
}

/// Regenerates the golden files from the fixed inputs above. Writes nothing unless
/// `SWARMOPS_REGEN_GOLDEN=1` is set, so `--run-ignored all` can never rewrite the pin.
/// Run on purpose, review the diff, commit; the regular golden tests then pin it.
#[test]
#[ignore = "writes tests/golden when SWARMOPS_REGEN_GOLDEN=1; run on purpose"]
fn regenerate_golden() {
    if std::env::var_os("SWARMOPS_REGEN_GOLDEN").is_none() {
        eprintln!("regenerate_golden: SWARMOPS_REGEN_GOLDEN not set; nothing written");
        return;
    }
    let trees: Vec<(&str, Tree)> = vec![
        ("empty_tree", Tree::empty_root()),
        ("one_file_tree", one_file_tree()),
    ];
    for (name, tree) in trees {
        let text = format!(
            "{}\n{}\n",
            hex::encode(tree.encode()),
            tree.id().as_object_id().to_hex()
        );
        fs::write(golden_dir().join(format!("{name}.hex")), text).unwrap();
    }
    let changesets: Vec<(&str, Changeset)> = vec![
        ("minimal_changeset", minimal_changeset()),
        ("full_changeset", full_changeset()),
    ];
    for (name, cs) in changesets {
        let text = format!(
            "{}\n{}\n{}\n",
            hex::encode(cs.encode().unwrap()),
            cs.id().unwrap().as_object_id().to_hex(),
            hex::encode(cs.delta_hash().unwrap())
        );
        fs::write(golden_dir().join(format!("{name}.hex")), text).unwrap();
    }
}
