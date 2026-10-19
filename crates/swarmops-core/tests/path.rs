use swarmops_core::{PathError, RepoPath};

#[test]
fn repo_path_accepts_nested_and_orders_by_bytes() {
    let a = RepoPath::new("a/b.rs").unwrap();
    let b = RepoPath::new("a/b.rs~").unwrap();
    assert!(a < b);
    assert_eq!(a.parent().unwrap().as_str(), "a");
    assert_eq!(a.file_name(), "b.rs");
    assert_eq!(a.components().collect::<Vec<_>>(), vec!["a", "b.rs"]);
    assert!(RepoPath::new("top.txt").unwrap().parent().is_none());
}

#[test]
fn repo_path_rejects_invalid() {
    for (s, e) in [
        ("", PathError::Empty),
        ("a//b", PathError::EmptyComponent),
        ("./a", PathError::DotComponent),
        ("a/../b", PathError::DotComponent),
        ("/a", PathError::LeadingOrTrailingSlash),
        ("a/", PathError::LeadingOrTrailingSlash),
        ("a\0b", PathError::Nul),
    ] {
        assert_eq!(RepoPath::new(s).unwrap_err(), e, "{s:?}");
    }
}

#[test]
fn repo_path_does_not_normalize_unicode() {
    let nfc = RepoPath::new("caf\u{e9}.txt").unwrap();
    let nfd = RepoPath::new("cafe\u{301}.txt").unwrap();
    assert_ne!(nfc, nfd);
}

/// The bytes of a tree that holds one regular file named `name`, as the layout writes them
/// (`"SWTR" 01 | 01 | kind 00 | varint len | name | id`), whatever the name: a decoder must
/// check what no encoder would write. The names here are under 128 bytes, so the length is one
/// byte.
fn one_file_tree_bytes(name: &str, id: [u8; 32]) -> Vec<u8> {
    let mut bytes = b"SWTR\x01\x01\x00".to_vec();
    bytes.push(u8::try_from(name.len()).unwrap());
    bytes.extend_from_slice(name.as_bytes());
    bytes.extend_from_slice(&id);
    bytes
}

/// A tree entry's name is one path component: a name with a `/` in it is refused as such, by
/// `Tree::new` and by `Tree::decode` alike (it was `EmptyComponent`, which says something else).
#[test]
fn a_tree_name_with_a_slash_is_refused_as_a_slash() {
    use swarmops_core::{BlobId, CodecError, LeafKind, ObjectKind, Tree, TreeEntry, hash_object};

    let blob = BlobId(hash_object(ObjectKind::Blob, b"x"));
    let slash = Err(CodecError::BadPath(PathError::Slash));
    for name in ["a/b", "/", "a/", "/a"] {
        let made = Tree::new(vec![TreeEntry::leaf(name, LeafKind::File, blob)]);
        assert_eq!(made.map(drop), slash, "new {name:?}");
        let decoded = Tree::decode(&one_file_tree_bytes(name, blob.0.0));
        assert_eq!(decoded.map(drop), slash, "decode {name:?}");
    }
}

/// Every other name a tree entry may not have gives the error it gave when the check built a
/// `RepoPath` of it, through `Tree::new` and `Tree::decode` alike; a plain name passes both.
#[test]
fn tree_names_are_checked_as_before() {
    use swarmops_core::{BlobId, CodecError, LeafKind, ObjectKind, Tree, TreeEntry, hash_object};

    let blob = BlobId(hash_object(ObjectKind::Blob, b"x"));
    for (name, want) in [
        ("", Err(CodecError::BadPath(PathError::Empty))),
        (".", Err(CodecError::BadPath(PathError::DotComponent))),
        ("..", Err(CodecError::BadPath(PathError::DotComponent))),
        ("a\0b", Err(CodecError::BadPath(PathError::Nul))),
        ("\0", Err(CodecError::BadPath(PathError::Nul))),
        ("ok", Ok(())),
        ("...", Ok(())),
        (".hidden", Ok(())),
    ] {
        let made = Tree::new(vec![TreeEntry::leaf(name, LeafKind::File, blob)]);
        assert_eq!(made.map(drop), want, "new {name:?}");
        let decoded = Tree::decode(&one_file_tree_bytes(name, blob.0.0));
        assert_eq!(decoded.map(drop), want, "decode {name:?}");
    }
}
