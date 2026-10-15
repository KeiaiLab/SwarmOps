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
