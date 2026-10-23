//! git's objects converted into a changeset's fields, trees and blobs: every commit git wrote that
//! a changeset holds as it is converts without loss and renders back to its own id; each class of
//! what a changeset cannot hold is refused by name, or dropped or normalized and named once
//! allowed; and what no class covers is refused whatever is allowed.

use std::collections::{BTreeMap, HashMap};

use proptest::prelude::*;
use swarmops_core::{
    BlobId, EntryTarget, LeafKind, ObjectId, ObjectKind, Tree, TreeEntry, TreeId, hash_object,
};
use swarmops_git::{
    ConvertError, Converted, Converter, GitOid, GitSource, GixSource, LossyClass, LossySet,
    RawCommit, RawKind, RawTree, SourceError, commit_bytes, git_hash, tree_bytes,
};
use swarmops_store::MAX_OBJECT_LEN;
use swarmops_testkit::git::{self, GitFixture, Manifest};

use crate::support::MemSource;

/// The fixtures every commit of which git wrote and a changeset holds as it is, and how many
/// commits each holds; `many_parents`' `too-many`, a merge of 256 roots, aside.
const FAITHFUL: [(&str, usize); 10] = [
    ("headers", 6),
    ("linear", 5),
    ("loose_and_packed", 5),
    ("many_parents", 257),
    ("merges", 17),
    ("modes", 7),
    ("refs", 10),
    ("replace_ref", 4),
    ("roots", 8),
    ("signatures", 7),
];

/// Each fixture of one lossy class but `nonstandard`: the class, the place in its manifest of
/// the first commit that holds it, and the path its refusal names.
const LOSSY: [(&str, LossyClass, usize, &[u8]); 5] = [
    ("empty_trees", LossyClass::EmptyTrees, 1, b"a/empty"),
    ("gitlinks", LossyClass::Gitlinks, 1, b"lib/sub"),
    ("modes_odd", LossyClass::Modes, 1, b"group.txt"),
    ("paths", LossyClass::Paths, 1, b"caf\xe9.txt"),
    ("tree_order", LossyClass::TreeOrder, 1, b"a.txt"),
];

fn oid(hex: &str) -> GitOid {
    hex.parse().unwrap_or_else(|error| panic!("{hex}: {error}"))
}

fn manifest(fixture: &GitFixture) -> &Manifest {
    fixture
        .manifest
        .as_ref()
        .expect("the fixture has a manifest")
}

fn open(fixture: &GitFixture) -> GixSource {
    let repo = fixture
        .repo
        .as_deref()
        .expect("the fixture holds a repository");
    GixSource::open(repo).unwrap_or_else(|error| panic!("open: {error}"))
}

fn set(classes: &[LossyClass]) -> LossySet {
    classes.iter().copied().collect()
}

/// What conversions appended to their `out`, by id: the store the tests read converted trees and
/// blobs from.
#[derive(Default)]
struct Objects(HashMap<ObjectId, Vec<u8>>);

impl Objects {
    /// Takes what a conversion appended to `out`, which holds no object twice.
    fn take(&mut self, out: &mut Vec<(ObjectKind, Vec<u8>)>) {
        for (kind, bytes) in out.drain(..) {
            let id = hash_object(kind, &bytes);
            assert!(
                self.0.insert(id, bytes).is_none(),
                "{kind:?} {id} was appended twice"
            );
        }
    }

    fn tree(&self, id: TreeId) -> Tree {
        let bytes = self.0.get(&id.0);
        Tree::decode(bytes.unwrap_or_else(|| panic!("tree {id} was never appended"))).unwrap()
    }

    fn blob(&self, id: BlobId) -> &[u8] {
        let bytes = self.0.get(&id.0);
        bytes.unwrap_or_else(|| panic!("blob {id} was never appended"))
    }

    /// The git id `entry` renders to, from these objects alone: its blob's, or its subtree's.
    fn git_oid(&self, entry: &TreeEntry) -> GitOid {
        match entry.target {
            EntryTarget::Leaf(_, blob) => git_hash(RawKind::Blob, self.blob(blob)),
            EntryTarget::Dir(tree) => self.git_tree(tree),
        }
    }

    /// The git id of tree `id`'s rendering, its subtrees rendered too.
    fn git_tree(&self, id: TreeId) -> GitOid {
        let tree = self.tree(id);
        git_hash(
            RawKind::Tree,
            &tree_bytes(&tree, |entry| self.git_oid(entry)),
        )
    }

    /// Every leaf under tree `id`, by path: its kind and its bytes.
    fn files(&self, id: TreeId) -> BTreeMap<String, (LeafKind, Vec<u8>)> {
        let mut files = BTreeMap::new();
        let mut trees = vec![(String::new(), id)];
        while let Some((dir, id)) = trees.pop() {
            for entry in self.tree(id).entries() {
                let path = if dir.is_empty() {
                    entry.name.clone()
                } else {
                    format!("{dir}/{}", entry.name)
                };
                match entry.target {
                    EntryTarget::Leaf(kind, blob) => {
                        files.insert(path, (kind, self.blob(blob).to_vec()));
                    }
                    EntryTarget::Dir(tree) => trees.push((path, tree)),
                }
            }
        }
        files
    }
}

/// Commits, each with what a converter made of it.
type Results = Vec<(GitOid, Result<Converted, ConvertError>)>;

/// Fixture `name`'s commits, parents first, each with what one converter allowing `allowed` made
/// of it; and the objects the conversions appended.
fn convert_fixture(name: &str, allowed: LossySet) -> (Results, Objects) {
    let fixture = git::fixture(name);
    let source = open(&fixture);
    let mut converter = Converter::new(&source, allowed);
    let (mut objects, mut out) = (Objects::default(), Vec::new());
    let results = manifest(&fixture)
        .commits
        .iter()
        .map(|commit| {
            let id = oid(&commit.oid);
            let result = converter.commit(id, &mut out);
            objects.take(&mut out);
            (id, result)
        })
        .collect();
    (results, objects)
}

#[test]
fn every_faithful_fixture_commit_converts_to_its_own_oid() {
    for (name, commits) in FAITHFUL {
        let mut checked = 0;
        let fixture = git::fixture(name);
        let manifest = manifest(&fixture);
        let source = open(&fixture);
        let mut converter = Converter::new(&source, LossySet::empty());
        let (mut objects, mut out) = (Objects::default(), Vec::new());
        // `more_than_255_parents_is_refused_naming_the_commit` takes it.
        let refused = manifest.refs.get("refs/heads/too-many");
        for commit in manifest.commits.iter().filter(|c| Some(&c.oid) != refused) {
            let id = oid(&commit.oid);
            let got = (converter.commit(id, &mut out)).unwrap_or_else(|e| panic!("{name}: {e}"));
            objects.take(&mut out);
            assert_eq!(got.oid, id);
            assert!(got.lossy.is_empty(), "{name}: {id} lost {:?}", got.lossy);
            let parents: Vec<GitOid> = commit.parents.iter().map(|parent| oid(parent)).collect();
            assert_eq!(got.parents, parents, "{name}: {id}'s parents");
            // The root, rendered from the objects appended alone, is the commit's own tree, and
            // the commit renders back to its own bytes.
            let tree = objects.git_tree(got.root);
            assert_eq!(tree, oid(&commit.tree), "{name}: {id}'s tree");
            let bytes = commit_bytes(
                tree,
                &got.parents,
                &got.author,
                &got.committer,
                &got.extra_headers,
                &got.message,
            );
            assert!(bytes == commit.raw, "{name}: {id} renders otherwise");
            assert_eq!(git_hash(RawKind::Commit, &bytes), id, "{name}");
            checked += 1;
        }
        assert_eq!(checked, commits, "{name}");
    }
}

#[test]
fn git_order_and_byte_order_both_hold() {
    let fixture = git::fixture("modes");
    let source = open(&fixture);
    let head = source.resolve("HEAD").unwrap();
    let mut out = Vec::new();
    let got = (Converter::new(&source, LossySet::empty()).commit(head, &mut out)).unwrap();
    let mut objects = Objects::default();
    objects.take(&mut out);
    let root = objects.tree(got.root);
    // The changeset's tree is in byte order: the directory `a` before the file `a.b`.
    let names: Vec<&[u8]> = root.entries().iter().map(|e| e.name.as_bytes()).collect();
    let place = |names: &[&[u8]], name: &[u8]| names.iter().position(|n| *n == name).unwrap();
    assert!(place(&names, b"a") < place(&names, b"a.b"), "{names:?}");
    // Its git rendering is in git's order, `a.b` before `a/`, and is the commit's own tree.
    let rendered = tree_bytes(&root, |entry| objects.git_oid(entry));
    let git_names: Vec<Vec<u8>> = (RawTree::parse(&rendered).unwrap().entries.into_iter())
        .map(|entry| entry.name)
        .collect();
    let git_names: Vec<&[u8]> = git_names.iter().map(Vec::as_slice).collect();
    assert!(place(&git_names, b"a.b") < place(&git_names, b"a"));
    let commit = RawCommit::parse(&source.read(head).unwrap().1).unwrap();
    assert_eq!(git_hash(RawKind::Tree, &rendered), commit.tree);
}

#[test]
fn each_lossy_class_is_refused_by_name() {
    for (name, class, first, path) in LOSSY {
        let (results, _) = convert_fixture(name, LossySet::empty());
        for (at, (id, result)) in results.into_iter().enumerate() {
            match result {
                Ok(_) => assert!(at < first, "{name}: commit {at} converted"),
                Err(ConvertError::Lossy {
                    commit,
                    class: refused,
                    path: named,
                    detail,
                }) => {
                    assert!(at >= first, "{name}: commit {at} refused: {detail}");
                    assert_eq!((commit, refused), (id, class), "{name}: {detail}");
                    if at == first {
                        assert!(named == path, "{name}: {}", named.escape_ascii());
                    }
                }
                Err(error) => panic!("{name}: commit {at}: {error}"),
            }
        }
    }
    // Every commit of `nonstandard` is, each for a reason of its own, naming no path.
    let (results, _) = convert_fixture("nonstandard", LossySet::empty());
    assert_eq!(results.len(), 5);
    for (id, result) in results {
        assert!(
            matches!(
                &result,
                Err(ConvertError::Lossy { commit, class: LossyClass::Nonstandard, path, .. })
                    if *commit == id && path.is_empty()
            ),
            "{id}: {result:?}"
        );
    }
}

/// What the head of the fixture of a lossy class keeps, once its class is allowed: its files,
/// each a file holding these bytes, a dropped path among none of them.
fn head_files(name: &str) -> Vec<(&'static str, &'static [u8])> {
    match name {
        "empty_trees" => vec![("a/keep.txt", b"keep\n")],
        "gitlinks" => vec![("lib/readme.txt", b"lib\n")],
        // The odd modes are files, and the zero-padded `040000` a directory.
        "modes_odd" => vec![
            ("group.txt", b"group\n"),
            ("padded/inner.txt", b"inner\n"),
            ("plain.txt", b"plain\n"),
            ("private.txt", b"private\n"),
        ],
        "paths" => vec![("plain.txt", b"plain\n")],
        // `dup.txt` twice: the first, `a.txt`'s blob, kept.
        "tree_order" => vec![("dup.txt", b"a\n")],
        "nonstandard" => vec![
            ("five.txt", b"five.txt\n"),
            ("four.txt", b"four.txt\n"),
            ("one.txt", b"one.txt\n"),
            ("three.txt", b"three.txt\n"),
            ("two.txt", b"two.txt\n"),
        ],
        _ => unreachable!("{name} is no fixture of a lossy class"),
    }
}

#[test]
fn each_lossy_class_when_allowed_converts_and_marks_git_lossy() {
    let nonstandard = ("nonstandard", LossyClass::Nonstandard, 0, &b""[..]);
    for (name, class, first, _) in LOSSY.into_iter().chain([nonstandard]) {
        let (results, objects) = convert_fixture(name, set(&[class]));
        let mut head = None;
        for (at, (id, result)) in results.into_iter().enumerate() {
            let got = result.unwrap_or_else(|error| panic!("{name}: commit {at}: {error}"));
            let lost = at >= first;
            let lossy = if lost {
                set(&[class])
            } else {
                LossySet::empty()
            };
            assert_eq!(got.lossy, lossy, "{name}: commit {at}");
            let mut metadata = vec![("git.oid".to_owned(), id.to_string().into_bytes())];
            if lost {
                metadata.insert(0, ("git.lossy".to_owned(), class.as_str().into()));
            }
            assert_eq!(got.metadata, metadata, "{name}: commit {at}");
            head = Some(got);
        }
        let head = head.expect("the fixture has commits");
        let files: Vec<(String, (LeafKind, Vec<u8>))> =
            objects.files(head.root).into_iter().collect();
        let expected: Vec<(String, (LeafKind, Vec<u8>))> = (head_files(name).into_iter())
            .map(|(path, bytes)| (path.to_owned(), (LeafKind::File, bytes.to_vec())))
            .collect();
        assert_eq!(files, expected, "{name}");
        // `nonstandard`'s last commit lists its parent twice: once in the changeset.
        if name == "nonstandard" {
            assert_eq!(head.parents.len(), 1);
        }
    }
}

#[test]
fn a_directory_left_empty_by_a_drop_is_dropped() {
    // `only/` holds nothing but a gitlink: with gitlinks allowed, and nothing else, it goes with
    // its gitlink, under that class alone.
    let (results, objects) = convert_fixture("gitlinks", set(&[LossyClass::Gitlinks]));
    let (_, head) = results.into_iter().last().expect("gitlinks has commits");
    let head = head.unwrap();
    assert_eq!(head.lossy, set(&[LossyClass::Gitlinks]));
    let root = objects.tree(head.root);
    let names: Vec<&str> = root.entries().iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["lib"]);

    // A directory whose subdirectories each drop empty, one for a gitlink and one as an empty
    // tree, is dropped with them: under both classes once allowed, as `empty_trees` otherwise.
    let mut source = MemSource::default();
    let empty = source.tree(&[]);
    let gitlink = source.tree(&[("160000", b"module", GitOid::new([0x22; 20]))]);
    let outer = source.tree(&[("40000", b"link", gitlink), ("40000", b"none", empty)]);
    let kept = source.blob(b"kept\n");
    let root = source.tree(&[("40000", b"outer", outer), ("100644", b"z.txt", kept)]);
    let commit = source.commit(root, &[], "a directory that drops empty\n");
    let mut out = Vec::new();
    let got = Converter::new(
        &source,
        set(&[LossyClass::Gitlinks, LossyClass::EmptyTrees]),
    )
    .commit(commit, &mut out)
    .unwrap();
    assert_eq!(
        got.lossy,
        set(&[LossyClass::EmptyTrees, LossyClass::Gitlinks])
    );
    let mut objects = Objects::default();
    objects.take(&mut out);
    let names: Vec<String> = objects.files(got.root).into_keys().collect();
    assert_eq!(names, ["z.txt"]);
    let refused = Converter::new(&source, set(&[LossyClass::Gitlinks])).commit(commit, &mut out);
    assert!(
        matches!(&refused, Err(ConvertError::Lossy { class: LossyClass::EmptyTrees, path, .. })
            if path == b"outer/none"),
        "{refused:?}"
    );
}

#[test]
fn more_than_255_parents_is_refused_naming_the_commit() {
    let fixture = git::fixture("many_parents");
    let too_many = oid(&manifest(&fixture).refs["refs/heads/too-many"]);
    let source = open(&fixture);
    for allowed in [LossySet::empty(), LossySet::all()] {
        let mut out = Vec::new();
        assert_eq!(
            Converter::new(&source, allowed).commit(too_many, &mut out),
            Err(ConvertError::Parents {
                commit: too_many,
                parents: 256
            })
        );
        assert!(out.is_empty(), "a refused commit appends nothing");
    }
}

#[test]
fn a_blob_past_the_object_limit_is_too_large_before_it_is_read() {
    let mut source = MemSource::default();
    // Its header claims one byte more than an object holds; reading it panics.
    let huge = GitOid::new([0x5a; 20]);
    source.claim(huge, RawKind::Blob, MAX_OBJECT_LEN + 1);
    let small = source.blob(b"small\n");
    let data = source.tree(&[
        ("100644", b"big.bin", huge),
        ("100644", b"small.txt", small),
    ]);
    let root = source.tree(&[("40000", b"data", data)]);
    let commit = source.commit(root, &[], "a blob past the object limit\n");
    for allowed in [LossySet::empty(), LossySet::all()] {
        let mut out = Vec::new();
        assert_eq!(
            Converter::new(&source, allowed).commit(commit, &mut out),
            Err(ConvertError::TooLarge {
                commit,
                path: b"data/big.bin".to_vec(),
                size: 536_870_913
            })
        );
        assert!(out.is_empty(), "a refused commit appends nothing");
    }
}

/// The commit itself, its tree and a tree under it pass the blob's gate too, as their headers say,
/// before they are read: whole, since they are parsed. A header that says the most an object
/// holds lets the read go ahead; one byte more is refused, naming the object, whatever is allowed,
/// and the conversion appends nothing.
#[test]
fn a_commit_or_a_tree_past_the_object_limit_is_too_large_before_it_is_read() {
    let mut source = MemSource::default();
    let file = source.blob(b"file\n");
    let data = source.tree(&[("100644", b"file.txt", file)]);
    let root = source.tree(&[("40000", b"data", data)]);
    let commit = source.commit(root, &[], "a commit and trees at the object limit\n");
    for held in [commit, root, data] {
        source.state_length(held, MAX_OBJECT_LEN);
    }
    let mut out = Vec::new();
    let converted = Converter::new(&source, LossySet::empty()).commit(commit, &mut out);
    assert!(converted.expect("at the limit").lossy.is_empty());
    assert_eq!(out.len(), 3, "a blob and two trees");

    // Headers that claim one byte more; reading any of them panics.
    let (huge_commit, huge_root, huge_dir) = (
        GitOid::new([0x5c; 20]),
        GitOid::new([0x5d; 20]),
        GitOid::new([0x5e; 20]),
    );
    source.claim(huge_commit, RawKind::Commit, MAX_OBJECT_LEN + 1);
    source.claim(huge_root, RawKind::Tree, MAX_OBJECT_LEN + 1);
    source.claim(huge_dir, RawKind::Tree, MAX_OBJECT_LEN + 1);
    let on_huge_root = source.commit(huge_root, &[], "a root tree past the object limit\n");
    let holding = source.tree(&[("40000", b"huge", huge_dir), ("100644", b"small.txt", file)]);
    let under_it = source.commit(
        holding,
        &[],
        "a tree past the object limit under the root\n",
    );
    for (converted, oid, kind) in [
        (huge_commit, huge_commit, RawKind::Commit),
        (on_huge_root, huge_root, RawKind::Tree),
        (under_it, huge_dir, RawKind::Tree),
    ] {
        for allowed in [LossySet::empty(), LossySet::all()] {
            let mut out = Vec::new();
            let refused = SourceError::TooLarge {
                oid,
                kind,
                size: 536_870_913,
            };
            assert_eq!(
                Converter::new(&source, allowed).commit(converted, &mut out),
                Err(ConvertError::Source(refused)),
                "{kind} {oid}"
            );
            assert!(out.is_empty(), "a refused commit appends nothing");
        }
    }
}

#[test]
fn a_deep_tree_converts_without_recursion() {
    const DEPTH: usize = 10_000;
    // A stack of 256 KiB, which a conversion that recursed once a directory would overflow.
    let (got, made) = std::thread::Builder::new()
        .name("a deep tree".to_owned())
        .stack_size(256 * 1024)
        .spawn(|| {
            let mut source = MemSource::default();
            let file = source.blob(b"deep\n");
            let mut tree = source.tree(&[("100644", b"file.txt", file)]);
            for _ in 0..DEPTH {
                tree = source.tree(&[("40000", b"d", tree)]);
            }
            let commit = source.commit(tree, &[], "10,000 directories deep\n");
            let mut out = Vec::new();
            let got = Converter::new(&source, LossySet::empty()).commit(commit, &mut out);
            (got, out.len())
        })
        .unwrap()
        .join()
        .unwrap();
    // Without loss: it rendered back to its own id; one blob, and a tree for each directory.
    assert!(got.unwrap().lossy.is_empty());
    assert_eq!(made, 1 + DEPTH + 1);
}

#[test]
fn metadata_is_git_lossy_then_git_oid() {
    let mut source = MemSource::default();
    let kept = source.blob(b"kept\n");
    let empty = source.tree(&[]);
    let gitlink = GitOid::new([0x11; 20]);
    let lossy_root = source.tree(&[
        ("40000", b"empty", empty),
        ("100644", b"kept.txt", kept),
        ("160000", b"sub", gitlink),
    ]);
    let lossy = source.commit(lossy_root, &[], "a gitlink and an empty tree\n");
    let faithful_root = source.tree(&[("100644", b"kept.txt", kept)]);
    let faithful = source.commit(faithful_root, &[lossy], "nothing lost\n");
    let mut converter = Converter::new(&source, LossySet::all());
    let mut out = Vec::new();
    let first = converter.commit(lossy, &mut out).unwrap();
    // The classes lost, by name in order, then the commit's id: the keys ascend, as a
    // changeset's metadata keys must.
    assert_eq!(
        first.metadata,
        [
            ("git.lossy".to_owned(), b"empty_trees,gitlinks".to_vec()),
            ("git.oid".to_owned(), lossy.to_string().into_bytes()),
        ]
    );
    let second = converter.commit(faithful, &mut out).unwrap();
    assert_eq!(
        second.metadata,
        [("git.oid".to_owned(), faithful.to_string().into_bytes())]
    );
    // What the first dropped leaves the second's root: one tree, made once.
    assert_eq!(first.root, second.root);
    let mut objects = Objects::default();
    objects.take(&mut out);
    assert_eq!(objects.0.len(), 2);
}

#[test]
fn times_before_1970_and_negative_zero_are_kept() {
    let (results, _) = convert_fixture("signatures", LossySet::empty());
    let authors: Vec<_> = (results.into_iter())
        .map(|(_, result)| result.unwrap().author)
        .collect();
    let before_1970 = authors
        .iter()
        .find(|a| a.secs < 0)
        .expect("a time before 1970");
    assert_eq!((before_1970.secs, before_1970.offset_secs), (-86_400, 0));
    assert!(!before_1970.negative_zero);
    let minus_zero = authors.iter().find(|a| a.negative_zero).expect("-0000");
    assert_eq!(minus_zero.offset_secs, 0);
    // A Latin-1 name, as its bytes.
    assert!(authors.iter().any(|a| a.name == b"Ren\xe9 Fixture"));
}

#[test]
fn a_corrupt_source_object_is_a_source_error_not_a_lossy_class() {
    let fixture = git::fixture("corrupt_object");
    let manifest = manifest(&fixture);
    let (first, second) = (&manifest.commits[0], &manifest.commits[1]);
    let damaged = (RawTree::parse(&manifest.trees[&second.tree])
        .unwrap()
        .entries
        .into_iter())
    .find(|entry| entry.name == b"damaged.txt")
    .expect("the second commit adds damaged.txt")
    .oid;
    let source = open(&fixture);
    for allowed in [LossySet::empty(), LossySet::all()] {
        let mut converter = Converter::new(&source, allowed);
        let mut out = Vec::new();
        converter.commit(oid(&first.oid), &mut out).unwrap();
        let got = converter.commit(oid(&second.oid), &mut out);
        assert!(
            matches!(&got, Err(ConvertError::Source(SourceError::Corrupt { oid, .. })) if *oid == damaged),
            "{got:?}"
        );
    }
    // A partial clone lacks its blobs: what it lacks is the source's `Missing`.
    let (results, _) = convert_fixture("partial_clone", LossySet::all());
    for (id, result) in results {
        assert!(
            matches!(
                result,
                Err(ConvertError::Source(SourceError::Missing { .. }))
            ),
            "{id}: {result:?}"
        );
    }
}

#[test]
fn modes_map_by_their_file_type() {
    let mut source = MemSource::default();
    let blob = source.blob(b"x\n");
    let inner = source.tree(&[("100644", b"inner.txt", blob)]);
    // Each a mode git does not write, of a file, a symlink, a directory or a gitlink.
    let root = source.tree(&[
        ("100664", b"a", blob),
        ("100775", b"b", blob),
        ("0100644", b"c", blob),
        ("120777", b"d", blob),
        ("40755", b"e", inner),
        ("0160000", b"f", GitOid::new([0x33; 20])),
    ]);
    let commit = source.commit(root, &[], "modes git does not write\n");
    let mut out = Vec::new();
    let allowed = set(&[LossyClass::Modes, LossyClass::Gitlinks]);
    let got = Converter::new(&source, allowed)
        .commit(commit, &mut out)
        .unwrap();
    assert_eq!(got.lossy, allowed);
    let mut objects = Objects::default();
    objects.take(&mut out);
    let kinds: Vec<(String, LeafKind)> = (objects.files(got.root).into_iter())
        .map(|(path, (kind, _))| (path, kind))
        .collect();
    let expected = [
        ("a", LeafKind::File),
        ("b", LeafKind::Exec),
        ("c", LeafKind::File),
        ("d", LeafKind::Symlink),
        ("e/inner.txt", LeafKind::File),
    ];
    assert_eq!(kinds, expected.map(|(path, kind)| (path.to_owned(), kind)));
    // A zero-padded directory, the commit's one oddity, is refused naming it unless allowed.
    let root = source.tree(&[("040000", b"padded", inner)]);
    let commit = source.commit(root, &[], "a zero-padded directory\n");
    let refused = Converter::new(&source, LossySet::empty()).commit(commit, &mut out);
    assert!(
        matches!(&refused, Err(ConvertError::Lossy { class: LossyClass::Modes, path, .. })
            if path == b"padded"),
        "{refused:?}"
    );
    let allowed = set(&[LossyClass::Modes]);
    let got = Converter::new(&source, allowed)
        .commit(commit, &mut out)
        .unwrap();
    assert_eq!(got.lossy, allowed);
    // A mode of no file, directory, symlink or gitlink: refused whatever is allowed.
    for mode in ["60000", "20644", "100"] {
        let root = source.tree(&[("100644", b"a", blob), (mode, b"dev", blob)]);
        let commit = source.commit(root, &[], "a device\n");
        assert_eq!(
            Converter::new(&source, LossySet::all()).commit(commit, &mut out),
            Err(ConvertError::Mode {
                commit,
                path: b"dev".to_vec(),
                mode: mode.as_bytes().to_vec()
            })
        );
    }
}

#[test]
fn an_object_of_another_kind_than_named_is_malformed() {
    let mut source = MemSource::default();
    let blob = source.blob(b"x\n");
    let tree = source.tree(&[("100644", b"x.txt", blob)]);
    let unparsed = source.put(RawKind::Commit, b"no tree line\n".to_vec());
    let cases = [
        // A file's entry naming a tree, and a directory's naming a blob.
        (source.tree(&[("100644", b"file", tree)]), tree),
        (source.tree(&[("40000", b"dir", blob)]), blob),
    ];
    for (root, named) in cases {
        let commit = source.commit(root, &[], "an entry of the wrong kind\n");
        let got = Converter::new(&source, LossySet::all()).commit(commit, &mut Vec::new());
        assert!(
            matches!(&got, Err(ConvertError::Malformed { oid, .. }) if *oid == named),
            "{got:?}"
        );
    }
    // A commit's id naming a tree, and bytes that are no commit.
    for named in [tree, unparsed] {
        let got = Converter::new(&source, LossySet::all()).commit(named, &mut Vec::new());
        assert!(
            matches!(&got, Err(ConvertError::Malformed { oid, .. }) if *oid == named),
            "{got:?}"
        );
    }
}

#[test]
fn a_commit_that_does_not_render_back_is_a_bug() {
    // A source that reads one tree's id as another tree's bytes: the commit converts without a
    // lossy class, yet renders back to another id, which only the fidelity check notices.
    let mut source = MemSource::default();
    let blob = source.blob(b"x\n");
    let named = source.tree(&[("100644", b"a.txt", blob)]);
    let other = source.tree(&[("100644", b"b.txt", blob)]);
    let (_, bytes) = source.read(other).unwrap();
    source.forge(named, RawKind::Tree, bytes);
    let commit = source.commit(named, &[], "a tree read as another\n");
    let mut out = Vec::new();
    let got = Converter::new(&source, LossySet::all()).commit(commit, &mut out);
    assert_eq!(got, Err(ConvertError::Bug { commit }));
    assert!(out.is_empty(), "a refused commit appends nothing");
}

#[test]
fn a_refused_commit_leaves_out_and_the_converter_as_they_were() {
    let mut source = MemSource::default();
    let blob = source.blob(b"shared\n");
    let refused_root = source.tree(&[
        ("100644", b"a.txt", blob),
        ("160000", b"sub", GitOid::new([0x44; 20])),
    ]);
    let refused = source.commit(refused_root, &[], "a gitlink\n");
    let root = source.tree(&[("100644", b"a.txt", blob)]);
    let commit = source.commit(root, &[refused], "the same blob, no gitlink\n");
    let mut converter = Converter::new(&source, LossySet::empty());
    let mut out = Vec::new();
    assert!(converter.commit(refused, &mut out).is_err());
    assert!(out.is_empty(), "a refused commit appends nothing");
    // The blob the refused commit read is appended with the next commit that names it.
    converter.commit(commit, &mut out).unwrap();
    let mut objects = Objects::default();
    objects.take(&mut out);
    assert_eq!(objects.0.len(), 2, "the blob and the root");
}

#[test]
fn each_object_is_appended_once_and_remembered_ones_never() {
    let mut source = MemSource::default();
    let (one, two, three) = (
        source.blob(b"1\n"),
        source.blob(b"2\n"),
        source.blob(b"3\n"),
    );
    let dir = source.tree(&[("100644", b"two.txt", two)]);
    let first_root = source.tree(&[("40000", b"dir", dir), ("100644", b"one.txt", one)]);
    let first = source.commit(first_root, &[], "first\n");
    let second_root = source.tree(&[
        ("40000", b"dir", dir),
        ("100644", b"one.txt", one),
        ("100644", b"three.txt", three),
    ]);
    let second = source.commit(second_root, &[first], "second\n");
    let (mut objects, mut out) = (Objects::default(), Vec::new());
    let mut converter = Converter::new(&source, LossySet::empty());
    let got = converter.commit(first, &mut out).unwrap();
    assert_eq!(out.len(), 4, "two blobs and two trees");
    objects.take(&mut out);
    converter.commit(second, &mut out).unwrap();
    assert_eq!(out.len(), 2, "the new blob and the new root");
    objects.take(&mut out);

    // Seeded with what a store holds already, a converter appends only what is new.
    let dir_id = match objects.tree(got.root).entries()[0].target {
        EntryTarget::Dir(id) => id,
        EntryTarget::Leaf(..) => unreachable!("`dir` sorts before `one.txt`"),
    };
    let mut seeded = Converter::new(&source, LossySet::empty());
    seeded.remember_tree(dir, dir_id);
    seeded.remember_blob(one, BlobId(hash_object(ObjectKind::Blob, b"1\n")));
    seeded.commit(second, &mut out).unwrap();
    let appended: Vec<ObjectId> = out
        .iter()
        .map(|(kind, bytes)| hash_object(*kind, bytes))
        .collect();
    assert_eq!(appended.len(), 2, "three.txt's blob and the root");
    assert!(appended.contains(&hash_object(ObjectKind::Blob, b"3\n")));
}

/// A node of a tree the property test makes: a leaf of a mode git writes, or a directory.
#[derive(Clone, Debug)]
enum Node {
    Leaf(&'static str, Vec<u8>),
    Dir(BTreeMap<String, Node>),
}

/// Names around `/` in git's order: `-` and `.` sort before it, `0` and `a` after it, `é` past
/// ASCII.
fn arb_name() -> impl Strategy<Value = String> {
    "[-.0aé]{1,3}".prop_filter("a path component", |name| name != "." && name != "..")
}

fn arb_dir() -> impl Strategy<Value = BTreeMap<String, Node>> {
    let mode = prop_oneof![Just("100644"), Just("100755"), Just("120000")];
    let leaf = (mode, proptest::collection::vec(any::<u8>(), 0..6))
        .prop_map(|(mode, bytes)| Node::Leaf(mode, bytes));
    let node = leaf.prop_recursive(4, 24, 4, |inner| {
        proptest::collection::btree_map(arb_name(), inner, 1..4).prop_map(Node::Dir)
    });
    proptest::collection::btree_map(arb_name(), node, 0..5)
}

/// `dir` as git trees in `source`, each in git's order; the root's id. Its leaves go to `files`.
fn build(
    source: &mut MemSource,
    dir: &BTreeMap<String, Node>,
    path: &str,
    files: &mut BTreeMap<String, (LeafKind, Vec<u8>)>,
) -> GitOid {
    let mut entries: Vec<(&str, Vec<u8>, GitOid, Vec<u8>)> = Vec::new();
    for (name, node) in dir {
        let at = if path.is_empty() {
            name.clone()
        } else {
            format!("{path}/{name}")
        };
        let (mode, oid, key) = match node {
            Node::Leaf(mode, bytes) => {
                let kind = match *mode {
                    "100644" => LeafKind::File,
                    "100755" => LeafKind::Exec,
                    _ => LeafKind::Symlink,
                };
                files.insert(at, (kind, bytes.clone()));
                (*mode, source.blob(bytes), name.clone().into_bytes())
            }
            // git compares a directory's name as if it ended in `/`.
            Node::Dir(dir) => (
                "40000",
                build(source, dir, &at, files),
                format!("{name}/").into_bytes(),
            ),
        };
        entries.push((mode, name.clone().into_bytes(), oid, key));
    }
    entries.sort_by(|a, b| a.3.cmp(&b.3));
    let entries: Vec<(&str, &[u8], GitOid)> = (entries.iter())
        .map(|(mode, name, oid, _)| (*mode, name.as_slice(), *oid))
        .collect();
    source.tree(&entries)
}

proptest! {
    #[test]
    fn trees_git_wrote_convert_without_loss(dir in arb_dir()) {
        let mut source = MemSource::default();
        let mut files = BTreeMap::new();
        let root = build(&mut source, &dir, "", &mut files);
        let commit = source.commit(root, &[], "a tree git wrote\n");
        let mut out = Vec::new();
        let got = Converter::new(&source, LossySet::empty()).commit(commit, &mut out);
        let got = got.map_err(|error| TestCaseError::fail(error.to_string()))?;
        prop_assert!(got.lossy.is_empty());
        let mut objects = Objects::default();
        objects.take(&mut out);
        prop_assert_eq!(objects.files(got.root), files);
        prop_assert_eq!(objects.git_tree(got.root), root);
    }
}
