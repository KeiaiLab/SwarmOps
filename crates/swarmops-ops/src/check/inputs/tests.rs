//! A check's inputs against a tree: of a store, of a directory, and of a tree held in memory.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::PermissionsExt as _;

use camino::Utf8PathBuf;
use proptest::prelude::*;
use swarmops_api::{
    ChangeInput, ChangeOp, CommitReq, ContentInput, EntryKindOut, ErrorCode, Identity, ModeInput,
    OpError, ReadTreeReq, ResolveRevReq, StoreOps,
};

use super::{Entry, HostTree, Inputs, Resolved, StoreTree, Tree, parse, resolve, resolve_one};
use crate::LocalOps;
use crate::error;

/// The files of the fixture tree: a path, its content (a link's target), and what it is.
const FILES: [(&str, &str, ModeInput); 9] = [
    ("README.md", "hi\n", ModeInput::File),
    ("docs-old/x.md", "old\n", ModeInput::File),
    ("docs/a.md", "a\n", ModeInput::File),
    ("docs/api/b.md", "b\n", ModeInput::File),
    ("docs/latest", "a.md", ModeInput::Symlink),
    ("link", "docs", ModeInput::Symlink),
    ("scripts/run.sh", "#!/bin/sh\n", ModeInput::Exec),
    ("src/lib.rs", "//! lib\n", ModeInput::File),
    ("web/index.html", "<p>\n", ModeInput::File),
];

/// The inputs `patterns` read as, or the test fails.
#[track_caller]
fn inputs(patterns: &[&str]) -> Inputs {
    let owned: Vec<String> = patterns.iter().map(|p| (*p).to_owned()).collect();
    parse(&owned).unwrap_or_else(|fault| panic!("{patterns:?}: {fault:?}"))
}

/// A store whose main holds `files`, and the full id of its head.
fn store_with(files: &[(&str, &str, ModeInput)]) -> (tempfile::TempDir, LocalOps, String) {
    let dir = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
    let (ops, _) = LocalOps::init(&root, "test").unwrap();
    let who = Identity::parse("agent/test").unwrap();
    let changes = (files.iter())
        .map(|(path, content, mode)| {
            let mut change = ChangeInput::new((*path).to_owned(), ChangeOp::Put);
            change.content = Some(ContentInput::Inline((*content).to_owned()));
            change.mode = Some(*mode);
            change
        })
        .collect();
    let mut req = CommitReq::new(changes, "root".to_owned());
    req.advance = Some("main".to_owned());
    ops.commit(&who, req).unwrap();
    let id = ops.resolve_rev(ResolveRevReq::new("main")).unwrap().id;
    (dir, ops, id)
}

/// A directory of the host that holds `files`, each as what it is.
fn dir_with(files: &[(&str, &str, ModeInput)]) -> (tempfile::TempDir, Utf8PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(dir.path().join("tree")).unwrap();
    std::fs::create_dir(&root).unwrap();
    for (path, content, mode) in files {
        let at = root.join(path);
        std::fs::create_dir_all(at.parent().unwrap()).unwrap();
        if *mode == ModeInput::Symlink {
            std::os::unix::fs::symlink(content, &at).unwrap();
        } else {
            std::fs::write(&at, content).unwrap();
        }
        if *mode == ModeInput::Exec {
            std::fs::set_permissions(&at, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    (dir, root)
}

/// What `resolved` holds: each entry's path and kind, and whether more matched.
#[track_caller]
fn found(resolved: Resolved) -> (Vec<(String, EntryKindOut)>, bool) {
    match resolved {
        Resolved::Entries { entries, truncated } => (
            (entries.into_iter())
                .map(|entry| (entry.path, entry.kind))
                .collect(),
            truncated,
        ),
        Resolved::Whole => panic!("the whole tree, not entries"),
    }
}

/// The paths `patterns` match in `tree`, with no limit.
#[track_caller]
fn paths(tree: &dyn Tree, patterns: &[&str]) -> Vec<String> {
    let (entries, truncated) = found(resolve(tree, &inputs(patterns), usize::MAX).unwrap());
    assert!(!truncated, "{patterns:?}");
    entries.into_iter().map(|(path, _)| path).collect()
}

/// Patterns match files, executables and links, each once, in the bytewise order of their
/// paths; a directory named exactly matches nothing; a limit cuts the list and says so.
#[test]
fn patterns_resolve_to_the_entries_they_match() {
    let (_dir, ops, id) = store_with(&FILES);
    let tree = StoreTree::new(&ops, id.as_str());

    // 1. A file, an executable and a link, by what they are; `web` names no entry.
    let all = inputs(&["docs/**", "scripts/run.sh", "web", "docs/a.md"]);
    let resolved = resolve(&tree, &all, usize::MAX).unwrap();
    let Resolved::Entries { entries, truncated } = &resolved else {
        panic!("{resolved:?}");
    };
    assert!(!truncated, "{resolved:?}");
    assert!(
        entries.iter().all(|entry| entry.id.is_some()),
        "a stored entry has its blob's id: {entries:?}"
    );
    assert_ne!(entries[0].id, entries[1].id, "two contents, two blobs");
    assert_eq!(
        found(resolved).0,
        [
            ("docs/a.md".to_owned(), EntryKindOut::File),
            ("docs/api/b.md".to_owned(), EntryKindOut::File),
            ("docs/latest".to_owned(), EntryKindOut::Symlink),
            ("scripts/run.sh".to_owned(), EntryKindOut::Exec),
        ]
    );
    assert_eq!(paths(&tree, &["web"]), Vec::<String>::new());
    assert_eq!(paths(&tree, &["docs"]), Vec::<String>::new());
    assert_eq!(paths(&tree, &["docs/*"]), ["docs/a.md", "docs/latest"]);
    assert_eq!(paths(&tree, &["*"]), ["README.md", "link"]);

    // 2. In the order of the paths' bytes: `docs-old/x.md` before `docs/a.md`, though the
    //    directory `docs` sorts before `docs-old`.
    assert_eq!(
        paths(&tree, &["*/*"]),
        [
            "docs-old/x.md",
            "docs/a.md",
            "docs/latest",
            "scripts/run.sh",
            "src/lib.rs",
            "web/index.html",
        ]
    );
    assert_eq!(paths(&tree, &["**"]).len(), FILES.len());

    // 3. A limit: the first entries in that order, and whether more matched.
    let two = found(resolve(&tree, &inputs(&["*/*"]), 2).unwrap());
    let first: Vec<&str> = two.0.iter().map(|(path, _)| path.as_str()).collect();
    assert_eq!((first, two.1), (vec!["docs-old/x.md", "docs/a.md"], true));
    let six = found(resolve(&tree, &inputs(&["*/*"]), 6).unwrap());
    assert_eq!((six.0.len(), six.1), (6, false));
    let none = found(resolve(&tree, &inputs(&["*/*"]), 0).unwrap());
    assert_eq!(none, (Vec::new(), true));
    let nothing = found(resolve(&tree, &inputs(&["web"]), 0).unwrap());
    assert_eq!(nothing, (Vec::new(), false));
}

/// A directory the store gives over several pages is listed whole: each entry once, in the
/// store's order, whether a page holds one entry, a few, or the whole directory.
#[test]
fn a_directory_of_more_than_a_page_is_listed_whole() {
    let (_dir, ops, id) = store_with(&FILES);

    // 1. The store cuts the root's seven entries at a page of two, and hands back a cursor.
    let mut req = ReadTreeReq::new(id.as_str());
    req.limit = 2;
    let cut = ops.read_tree(req).unwrap();
    assert_eq!(cut.entries.len(), 2);
    assert!(cut.truncated, "a page of two does not hold the root");
    assert!(
        cut.cursor.is_some(),
        "a cut page names where the next begins"
    );

    // 2. Page by page, the root and the directories below it list as they do in one page.
    let whole = StoreTree::new(&ops, id.as_str());
    let root = [
        "README.md",
        "docs",
        "docs-old",
        "link",
        "scripts",
        "src",
        "web",
    ];
    for page in [1, 2, 3, 6, 7, 8] {
        let paged = StoreTree {
            store: &ops,
            id: id.clone(),
            page,
        };
        let listed = paged.list(None).unwrap();
        let names: Vec<&str> = listed.iter().map(|entry| entry.path.as_str()).collect();
        assert_eq!(names, root, "a page of {page}");
        assert_eq!(listed, whole.list(None).unwrap(), "a page of {page}");
        for dir in ["docs", "docs/api", "missing"] {
            let below = paged.list(Some(dir)).unwrap();
            assert_eq!(below, whole.list(Some(dir)).unwrap(), "{dir} by {page}");
        }
        let below: Vec<String> = (paged.list(Some("docs")).unwrap().into_iter())
            .map(|entry| entry.path)
            .collect();
        assert_eq!(below, ["docs/a.md", "docs/api", "docs/latest"], "by {page}");

        // 3. So a resolve over pages finds what one over a single page finds.
        let every = inputs(&["**"]);
        assert_eq!(
            resolve(&paged, &every, usize::MAX).unwrap(),
            resolve(&whole, &every, usize::MAX).unwrap(),
            "a page of {page}"
        );
        assert_eq!(paths(&paged, &["**"]).len(), FILES.len());
    }
}

/// A tree that fails the test when it is listed.
struct Unlisted;

impl Tree for Unlisted {
    fn list(&self, dir: Option<&str>) -> Result<Vec<Entry>, OpError> {
        panic!("{dir:?} listed");
    }
}

/// A check that declares no input has the whole tree for its input: every path matches, and
/// resolving lists nothing.
#[test]
fn no_pattern_is_the_whole_tree() {
    let none = inputs(&[]);
    assert!(none.is_empty(), "{none:?}");
    assert_eq!(none.len(), 0);
    assert!(none.matches("README.md"), "{none:?}");
    assert!(none.matches("docs/api/b.md"), "{none:?}");
    assert_eq!(resolve(&Unlisted, &none, 0).unwrap(), Resolved::Whole);

    let some = inputs(&["docs/**", "README.md"]);
    assert!(!some.is_empty(), "{some:?}");
    assert_eq!(some.len(), 2);
    assert!(some.matches("README.md"), "{some:?}");
    assert!(some.matches("docs/api/b.md"), "{some:?}");
    assert!(!some.matches("docs"), "{some:?}");
    assert!(!some.matches("src/lib.rs"), "{some:?}");
}

/// A link is an entry of its own, matched by its path; nothing is found through it, in a store
/// or in a directory, whether the walk comes down to it or starts below it.
#[test]
fn a_link_is_matched_and_not_followed() {
    let (_store, ops, id) = store_with(&FILES);
    let (_dir, root) = dir_with(&FILES);
    let stored = StoreTree::new(&ops, id.as_str());
    let host = HostTree::new(root.clone());
    assert!(
        std::fs::metadata(root.join("link/a.md")).is_ok(),
        "the host itself reads through the link"
    );
    for tree in [&stored as &dyn Tree, &host] {
        assert_eq!(paths(tree, &["link"]), ["link"]);
        assert_eq!(paths(tree, &["link/**"]), Vec::<String>::new());
        assert_eq!(paths(tree, &["link/a.md"]), Vec::<String>::new());
        assert_eq!(paths(tree, &["*/a.md"]), ["docs/a.md"]);
        assert_eq!(paths(tree, &["docs/latest/**"]), Vec::<String>::new());
        let below = inputs(&["link/**", "link/api/*"]);
        for at in 0..below.len() {
            let one = found(resolve_one(tree, &below, at, 1).unwrap());
            assert_eq!(one, (Vec::new(), false), "pattern {at}");
        }
        assert_eq!(tree.list(Some("link")).unwrap(), []);
        assert_eq!(tree.list(Some("link/api")).unwrap(), []);
        assert_eq!(tree.list(Some("docs/latest")).unwrap(), []);
        assert_eq!(tree.list(Some("docs/a.md")).unwrap(), []);
        assert_eq!(tree.list(Some("missing/dir")).unwrap(), []);
    }
}

/// The store's tree and a directory holding the same files resolve to the same paths and
/// kinds; the directory's entries have no id, and what a workspace keeps out of its tree
/// (`.swarmws` at the root, a `.git` directory) is no entry.
#[test]
fn the_store_and_a_directory_resolve_alike() {
    let (_store, ops, id) = store_with(&FILES);
    let (_dir, root) = dir_with(&FILES);
    for kept_out in [".swarmws/index.bin", ".git/config", "src/.git/config"] {
        let at = root.join(kept_out);
        std::fs::create_dir_all(at.parent().unwrap()).unwrap();
        std::fs::write(at, "x").unwrap();
    }
    std::fs::create_dir(root.join("empty")).unwrap();
    let stored = StoreTree::new(&ops, id.as_str());
    let host = HostTree::new(root);

    let cases: [&[&str]; 7] = [
        &["**"],
        &["*"],
        &["*/*"],
        &["docs/**", "scripts/*"],
        &["docs/api/b.md", "link", "web"],
        &["*/api/*", "src/**"],
        &[".git/**", ".swarmws/**", "src/.git/*", "empty", "empty/**"],
    ];
    for patterns in cases {
        let wanted = inputs(patterns);
        let in_store = resolve(&stored, &wanted, usize::MAX).unwrap();
        let on_host = resolve(&host, &wanted, usize::MAX).unwrap();
        let Resolved::Entries { entries, .. } = &on_host else {
            panic!("{on_host:?}");
        };
        assert!(
            entries.iter().all(|entry| entry.id.is_none()),
            "{entries:?}"
        );
        assert_eq!(found(on_host), found(in_store), "{patterns:?}");
    }
    assert_eq!(paths(&host, &["**"]).len(), FILES.len());
    assert_eq!(
        paths(&host, &[".git/**", "src/.git/*"]),
        Vec::<String>::new()
    );
    assert_eq!(host.list(Some(".swarmws")).unwrap(), []);
    assert_eq!(host.list(Some("src/.git")).unwrap(), []);
}

/// A tree that counts the directories listed below the root.
struct Counted<'t> {
    /// The tree read.
    tree: &'t dyn Tree,
    /// The directories listed so far, the root aside.
    below: Cell<usize>,
    /// Whether the root was listed.
    root: Cell<bool>,
}

impl Tree for Counted<'_> {
    fn list(&self, dir: Option<&str>) -> Result<Vec<Entry>, OpError> {
        match dir {
            Some(_) => self.below.set(self.below.get() + 1),
            None => self.root.set(true),
        }
        self.tree.list(dir)
    }
}

/// Of two patterns, the one that matches nothing is told from the one that matches: each is
/// asked for on its own, by its place in the sorted patterns, and only what is below the names
/// it starts with is listed.
#[test]
fn one_pattern_of_two_that_matches_nothing_is_named() {
    let (_dir, ops, id) = store_with(&FILES);
    let stored = StoreTree::new(&ops, id.as_str());
    let both = inputs(&["missing/**", "docs/**"]);

    // 1. Sorted: `docs/**` is pattern 0, `missing/**` pattern 1.
    let first = found(resolve_one(&stored, &both, 0, 1).unwrap());
    assert_eq!(
        first,
        (vec![("docs/a.md".to_owned(), EntryKindOut::File)], true)
    );
    let second = found(resolve_one(&stored, &both, 1, 1).unwrap());
    assert_eq!(second, (Vec::new(), false));
    let all = found(resolve_one(&stored, &both, 0, usize::MAX).unwrap());
    assert_eq!((all.0.len(), all.1), (3, false));

    // 2. A place past the last pattern is the caller's fault, not an empty answer.
    let error = resolve_one(&stored, &both, 2, 1).unwrap_err();
    assert_eq!(error.code, ErrorCode::Internal.as_str());

    // 3. Only below the names: `docs/api/*` lists `docs/api` alone, a pattern of names its
    //    last directory, and one that starts with a wildcard the root.
    let counted = |patterns: &[&str], at: usize| {
        let tree = Counted {
            tree: &stored,
            below: Cell::new(0),
            root: Cell::new(false),
        };
        let matched = found(resolve_one(&tree, &inputs(patterns), at, usize::MAX).unwrap());
        (matched.0.len(), tree.root.get(), tree.below.get())
    };
    assert_eq!(counted(&["docs/api/*"], 0), (1, false, 1));
    assert_eq!(counted(&["docs/api/b.md"], 0), (1, false, 1));
    assert_eq!(counted(&["README.md"], 0), (1, true, 0));
    assert_eq!(counted(&["*/api/*"], 0), (1, true, 6));
    assert_eq!(counted(&["docs/**"], 0), (3, false, 2));

    // 4. A whole resolve lists only the directories a pattern reaches below.
    let tree = Counted {
        tree: &stored,
        below: Cell::new(0),
        root: Cell::new(false),
    };
    assert_eq!(paths(&tree, &["docs/api/*", "src/*"]).len(), 2);
    assert_eq!((tree.root.get(), tree.below.get()), (true, 3));
}

/// A store's error other than a missing directory is the answer, not an empty tree.
#[test]
fn a_tree_that_cannot_be_read_is_an_error() {
    /// A tree whose every directory fails.
    struct Broken;
    impl Tree for Broken {
        fn list(&self, _dir: Option<&str>) -> Result<Vec<Entry>, OpError> {
            Err(error::error(ErrorCode::Io, "the tree cannot be read"))
        }
    }
    let wanted = inputs(&["docs/**"]);
    let error = resolve(&Broken, &wanted, 1).unwrap_err();
    assert_eq!(error.code, ErrorCode::Io.as_str());
    let error = resolve_one(&Broken, &wanted, 0, 1).unwrap_err();
    assert_eq!(error.code, ErrorCode::Io.as_str());

    // A changeset the store does not hold is the store's error, at the root.
    let (_dir, ops, _) = store_with(&FILES);
    let gone = StoreTree::new(&ops, "0".repeat(64));
    assert!(resolve(&gone, &wanted, 1).is_err(), "no such changeset");

    // A directory of the host that is not there is the host's error, not an empty tree: at the
    // root, and below it, where a walk of one pattern starts.
    let (_dir, root) = dir_with(&FILES);
    let nowhere = HostTree::new(root.join("nowhere"));
    for dir in [None, Some("docs")] {
        let error = nowhere.list(dir).unwrap_err();
        assert_eq!(error.code, ErrorCode::Io.as_str(), "{dir:?}");
        assert_eq!(error.details["path"], ".", "{dir:?}");
        assert_eq!(error.details["vouched"], true, "{dir:?}");
    }
    let error = resolve_one(&nowhere, &wanted, 0, 1).unwrap_err();
    assert_eq!(error.code, ErrorCode::Io.as_str());
}

/// A tree held in memory: its files by path, each with what it is.
struct MemTree {
    /// The files, executables and links.
    files: BTreeMap<String, EntryKindOut>,
}

impl Tree for MemTree {
    fn list(&self, dir: Option<&str>) -> Result<Vec<Entry>, OpError> {
        let below = dir.map_or_default(|dir| format!("{dir}/"));
        let mut seen = BTreeSet::new();
        let mut entries = Vec::new();
        for (path, kind) in &self.files {
            let Some(rest) = path.strip_prefix(&below) else {
                continue;
            };
            let (name, kind) = match rest.split_once('/') {
                Some((name, _)) => (name, EntryKindOut::Dir),
                None => (rest, *kind),
            };
            if seen.insert(name) {
                entries.push(Entry::new(format!("{below}{name}"), kind));
            }
        }

        // In the reverse of any order a walk could lean on.
        entries.reverse();
        Ok(entries)
    }
}

/// A path segment: of a few names that share prefixes, and bytes that sort around `/`.
fn name() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("a".to_owned()),
        Just("b".to_owned()),
        Just("a-b".to_owned()),
        Just("a.b".to_owned()),
        Just("a0".to_owned()),
        Just("ab".to_owned()),
    ]
}

/// A tree of up to 24 files, none of them below another one's path.
fn files() -> impl Strategy<Value = BTreeMap<String, EntryKindOut>> {
    let kind = prop_oneof![
        Just(EntryKindOut::File),
        Just(EntryKindOut::Exec),
        Just(EntryKindOut::Symlink),
    ];
    let path = proptest::collection::vec(name(), 1..5).prop_map(|names| names.join("/"));
    proptest::collection::btree_map(path, kind, 0..24).prop_map(|files| {
        let paths: Vec<String> = files.keys().cloned().collect();
        (files.into_iter())
            .filter(|(path, _)| {
                let below = format!("{path}/");
                !paths.iter().any(|other| other.starts_with(&below))
            })
            .collect()
    })
}

/// A pattern: names and `*`s, maybe a last `**`.
fn pattern() -> impl Strategy<Value = String> {
    let segment = prop_oneof![3 => name(), 1 => Just("*".to_owned())];
    (proptest::collection::vec(segment, 0..4), any::<bool>()).prop_map(|(mut segments, deep)| {
        if deep || segments.is_empty() {
            segments.push("**".to_owned());
        }
        segments.join("/")
    })
}

proptest! {
    /// Resolving is the same as listing every file of the tree, in the order of the paths'
    /// bytes, and keeping those the inputs match: whatever the walk leaves unlisted held no
    /// match, a limit keeps the first of them, and one pattern alone resolves to its own.
    #[test]
    fn resolving_is_filtering_the_whole_listing_by_matches(
        files in files(),
        patterns in proptest::collection::btree_set(pattern(), 1..4),
        limit in 0usize..8,
    ) {
        let tree = MemTree { files };
        let patterns: Vec<String> = patterns.into_iter().collect();
        let wanted = parse(&patterns).unwrap();
        let listing = |inputs: &Inputs| -> Vec<(String, EntryKindOut)> {
            (tree.files.iter())
                .filter(|(path, _)| inputs.matches(path))
                .map(|(path, kind)| (path.clone(), *kind))
                .collect()
        };

        // 1. Every pattern together, whole and cut.
        let expected = listing(&wanted);
        let whole = found(resolve(&tree, &wanted, usize::MAX).unwrap());
        prop_assert_eq!(&whole, &(expected.clone(), false));
        let cut = found(resolve(&tree, &wanted, limit).unwrap());
        let kept: Vec<_> = expected.iter().take(limit).cloned().collect();
        prop_assert_eq!(cut, (kept, expected.len() > limit));

        // 2. Each pattern alone, by its place in the sorted patterns.
        for (at, pattern) in patterns.iter().enumerate() {
            let alone = parse(std::slice::from_ref(pattern)).unwrap();
            let one = found(resolve_one(&tree, &wanted, at, usize::MAX).unwrap());
            prop_assert_eq!(one, (listing(&alone), false), "{}", pattern);
        }
    }
}
