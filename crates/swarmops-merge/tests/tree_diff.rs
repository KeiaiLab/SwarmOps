//! The tree diff (`swarmops_merge::tree_diff`): the leaf-level changes between two trees, read
//! through a `TreeRead` that is a map of trees built here and that logs every read; the
//! canonical delta of a diff; and the three-way path entries.
//!
//! A tree is drawn as a map of its leaves, path → leaf, and stored as the tree objects a commit
//! would make of it, so that the map is the oracle: what changed is what the two maps disagree on.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};

use proptest::prelude::*;
use swarmops_core::{
    BlobId, Changeset, DeltaEntry, DeltaOp, EntryTarget, LeafKind, ObjectKind, RepoPath, Signature,
    Tree, TreeEntry, TreeId, hash_object,
};
use swarmops_merge::tree_diff::{
    ChangeKind, Leaf, PathFilter, PathTriple, TreeChange, TreeRead, delta_of, diff_trees, three_way,
};

/// A tree as the map of its leaves: path → leaf. A path is a leaf or a directory, never both.
type Files = BTreeMap<RepoPath, Leaf>;

/// Trees by id, as a store holds them, with a log of every read.
#[derive(Default)]
struct Trees {
    by_id: HashMap<TreeId, Tree>,
    reads: RefCell<Vec<TreeId>>,
}

/// A read of a tree the map does not hold.
#[derive(Debug, PartialEq, Eq)]
struct NoSuchTree(TreeId);

impl TreeRead for Trees {
    type Error = NoSuchTree;

    fn tree(&self, id: TreeId) -> Result<Tree, NoSuchTree> {
        self.reads.borrow_mut().push(id);
        self.by_id.get(&id).cloned().ok_or(NoSuchTree(id))
    }
}

impl Trees {
    /// Stores the trees of `files` and returns its root: `None` for no files (the empty tree,
    /// which a diff never reads).
    fn add(&mut self, files: &Files) -> Option<TreeId> {
        let leaves: Vec<(&str, Leaf)> = files.iter().map(|(p, l)| (p.as_str(), *l)).collect();
        (!leaves.is_empty()).then(|| self.add_dir(&leaves))
    }

    /// Stores the tree of one directory, whose leaves `files` names by their paths below it,
    /// and the trees of its subdirectories.
    fn add_dir(&mut self, files: &[(&str, Leaf)]) -> TreeId {
        let mut below: BTreeMap<&str, Vec<(&str, Leaf)>> = BTreeMap::new();
        let mut entries = Vec::new();
        for &(path, leaf) in files {
            match path.split_once('/') {
                None => entries.push(TreeEntry::leaf(path, leaf.kind, leaf.blob)),
                Some((dir, rest)) => below.entry(dir).or_default().push((rest, leaf)),
            }
        }
        for (name, files) in below {
            let id = self.add_dir(&files);
            entries.push(TreeEntry::dir(name, id));
        }
        entries.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
        let tree = Tree::new(entries).unwrap();
        let id = tree.id();
        self.by_id.insert(id, tree);
        id
    }

    /// The tree of directory `dir` in the tree `root`, found without a logged read.
    fn dir_at(&self, root: TreeId, dir: &str) -> TreeId {
        dir.split('/').fold(root, |id, name| {
            let entry = self.by_id[&id].entries().iter().find(|e| e.name == name);
            match entry.map(|e| e.target) {
                Some(EntryTarget::Dir(tree)) => tree,
                other => panic!("{dir}: {name} is {other:?}, not a directory"),
            }
        })
    }

    /// The trees read since the last call, sorted; the log starts again empty.
    fn take_reads(&self) -> Vec<TreeId> {
        let mut reads = std::mem::take(&mut *self.reads.borrow_mut());
        reads.sort_unstable();
        reads
    }
}

fn blob(n: u8) -> BlobId {
    BlobId::new(hash_object(ObjectKind::Blob, &[n]))
}

fn file(n: u8) -> Leaf {
    Leaf::new(LeafKind::File, blob(n))
}

fn exec(n: u8) -> Leaf {
    Leaf::new(LeafKind::Exec, blob(n))
}

fn symlink(n: u8) -> Leaf {
    Leaf::new(LeafKind::Symlink, blob(n))
}

fn path(s: &str) -> RepoPath {
    RepoPath::new(s).unwrap()
}

fn files(leaves: &[(&str, Leaf)]) -> Files {
    leaves.iter().map(|&(p, leaf)| (path(p), leaf)).collect()
}

fn sorted(mut ids: Vec<TreeId>) -> Vec<TreeId> {
    ids.sort_unstable();
    ids
}

fn added(p: &str, new: Leaf) -> TreeChange {
    TreeChange::new(path(p), ChangeKind::Added { new })
}

fn deleted(p: &str, old: Leaf) -> TreeChange {
    TreeChange::new(path(p), ChangeKind::Deleted { old })
}

fn modified(p: &str, old: Leaf, new: Leaf) -> TreeChange {
    TreeChange::new(path(p), ChangeKind::Modified { old, new })
}

fn type_changed(p: &str, old: Leaf, new: Leaf) -> TreeChange {
    TreeChange::new(path(p), ChangeKind::TypeChanged { old, new })
}

fn put(p: &str, leaf: Leaf) -> DeltaEntry {
    let op = DeltaOp::Put {
        kind: leaf.kind,
        id: leaf.blob,
    };
    DeltaEntry::new(path(p), op, None).unwrap()
}

fn delete(p: &str) -> DeltaEntry {
    DeltaEntry::new(path(p), DeltaOp::Delete, None).unwrap()
}

/// The two trees stored, and their diff under `filter`.
fn diff(trees: &mut Trees, from: &Files, to: &Files, filter: &PathFilter) -> Vec<TreeChange> {
    let (from, to) = (trees.add(from), trees.add(to));
    diff_trees(trees, from, to, filter).unwrap()
}

/// What the diff of `from` and `to` must say: every path whose leaf differs, in path-byte order
/// (the maps' order), a modification when both leaves or neither are symlinks.
fn expected(from: &Files, to: &Files) -> Vec<TreeChange> {
    let paths: BTreeSet<&RepoPath> = from.keys().chain(to.keys()).collect();
    paths
        .into_iter()
        .filter_map(|p| {
            let kind = match (from.get(p).copied(), to.get(p).copied()) {
                (Some(old), Some(new)) if old == new => return None,
                (Some(old), Some(new))
                    if (old.kind == LeafKind::Symlink) != (new.kind == LeafKind::Symlink) =>
                {
                    ChangeKind::TypeChanged { old, new }
                }
                (Some(old), Some(new)) => ChangeKind::Modified { old, new },
                (Some(old), None) => ChangeKind::Deleted { old },
                (None, Some(new)) => ChangeKind::Added { new },
                (None, None) => unreachable!("the path is a key of one of the maps"),
            };
            Some(TreeChange::new(p.clone(), kind))
        })
        .collect()
}

/// Whether `path` equals one of `prefixes` or lies below one, worked out from the strings.
fn is_under(path: &RepoPath, prefixes: &[RepoPath]) -> bool {
    prefixes.iter().any(|prefix| {
        path.as_str()
            .strip_prefix(prefix.as_str())
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
    })
}

/// `files` with `delta` applied, checking that the delta is canonical: each delete names a
/// leaf `files` holds, each put changes what its path holds, and nothing is a rename.
fn applied(mut files: Files, delta: &[DeltaEntry]) -> Files {
    for entry in delta {
        assert_eq!(entry.renamed_from, None, "{entry:?}");
        match entry.op.put() {
            Some((kind, id)) => {
                let leaf = Leaf::new(kind, id);
                let old = files.insert(entry.path.clone(), leaf);
                assert_ne!(old, Some(leaf), "a put that changes nothing: {entry:?}");
            }
            None => {
                assert!(
                    files.remove(&entry.path).is_some(),
                    "a delete of no leaf: {entry:?}"
                );
            }
        }
    }
    files
}

/// The change `change` read the other way: what the diff of the two trees swapped says.
fn reversed(change: &TreeChange) -> TreeChange {
    let kind = match change.kind {
        ChangeKind::Added { new } => ChangeKind::Deleted { old: new },
        ChangeKind::Deleted { old } => ChangeKind::Added { new: old },
        ChangeKind::Modified { old, new } => ChangeKind::Modified { old: new, new: old },
        ChangeKind::TypeChanged { old, new } => ChangeKind::TypeChanged { old: new, new: old },
        _ => panic!("a variant this test does not know"),
    };
    TreeChange::new(change.path.clone(), kind)
}

fn leaf() -> impl Strategy<Value = Leaf> {
    let kinds = prop::sample::select(vec![LeafKind::File, LeafKind::Exec, LeafKind::Symlink]);
    (kinds, 0..3_u8).prop_map(|(kind, n)| Leaf::new(kind, blob(n)))
}

/// A path of one to three components from a few names, among them names that sort between a
/// directory and its leaves by bytes (`a-c` and `a.d` between `a` and `a/…`).
fn repo_path() -> impl Strategy<Value = RepoPath> {
    let name = prop::sample::select(vec!["a", "b", "a-c", "a.d"]);
    prop::collection::vec(name, 1..4).prop_map(|names| path(&names.join("/")))
}

/// Puts (`Some`) and deletes (`None`) of paths.
type Edits = Vec<(RepoPath, Option<Leaf>)>;

fn edits(most: usize) -> impl Strategy<Value = Edits> {
    prop::collection::vec((repo_path(), prop::option::of(leaf())), 0..most)
}

/// `files` with `edits` made in order, as commits would make them: a put drops a leaf above
/// its path and everything below it (a file becomes a directory, or a directory a file), a
/// delete drops the path and everything below it.
fn edited(mut files: Files, edits: &Edits) -> Files {
    for (at, leaf) in edits {
        let below = format!("{at}/");
        files.retain(|p, _| !p.as_str().starts_with(&below));
        files.remove(at);
        if let Some(leaf) = leaf {
            let mut above = at.parent();
            while let Some(dir) = above {
                files.remove(&dir);
                above = dir.parent();
            }
            files.insert(at.clone(), *leaf);
        }
    }
    files
}

/// A tree drawn as a few puts and deletes.
fn tree() -> impl Strategy<Value = Files> {
    edits(16).prop_map(|edits| edited(Files::new(), &edits))
}

/// Two trees: one, and the same one edited (they share most subtrees); or two drawn apart.
fn two_trees() -> impl Strategy<Value = (Files, Files)> {
    prop_oneof![
        (tree(), edits(8)).prop_map(|(from, edits)| {
            let to = edited(from.clone(), &edits);
            (from, to)
        }),
        (tree(), tree()),
    ]
}

proptest! {
    /// The diff is what the two maps disagree on; its delta turns one into the other; a tree
    /// diffed with itself is empty and reads nothing; and the diff the other way round is the
    /// same changes reversed.
    #[test]
    fn the_diff_applied_to_from_gives_to((from, to) in two_trees()) {
        let mut trees = Trees::default();
        let (a, b) = (trees.add(&from), trees.add(&to));

        let forward = diff_trees(&trees, a, b, &PathFilter::all()).unwrap();
        prop_assert_eq!(&forward, &expected(&from, &to));
        prop_assert_eq!(applied(from.clone(), &delta_of(&forward)), to);

        let backward = diff_trees(&trees, b, a, &PathFilter::all()).unwrap();
        prop_assert_eq!(backward, forward.iter().map(reversed).collect::<Vec<_>>());

        trees.take_reads();
        prop_assert_eq!(diff_trees(&trees, a, a, &PathFilter::all()).unwrap(), Vec::new());
        prop_assert_eq!(trees.take_reads(), Vec::new());
    }

    /// A diff under some prefixes is the whole diff's changes at the paths that equal or lie
    /// below one of them, and `PathFilter::matches` says the same of every path.
    #[test]
    fn a_filtered_diff_is_the_diff_at_the_paths_it_matches(
        (from, to) in two_trees(),
        prefixes in prop::collection::vec(repo_path(), 0..4),
    ) {
        let mut trees = Trees::default();
        let filter = PathFilter::under(prefixes.clone());
        let want: Vec<TreeChange> = expected(&from, &to)
            .into_iter()
            .filter(|change| is_under(&change.path, &prefixes))
            .collect();
        prop_assert_eq!(diff(&mut trees, &from, &to, &filter), want);
        for p in from.keys().chain(to.keys()) {
            prop_assert_eq!(filter.matches(p), is_under(p, &prefixes), "{}", p);
        }
    }

    /// Three-way path entries are, at every path base → ours or base → theirs changes (within
    /// the filter), the leaf each of the three trees holds there.
    #[test]
    fn three_way_gives_the_three_leaves_at_every_changed_path(
        base in tree(),
        ours_edits in edits(6),
        theirs_edits in edits(6),
        prefixes in prop::option::of(prop::collection::vec(repo_path(), 0..3)),
    ) {
        let ours = edited(base.clone(), &ours_edits);
        let theirs = edited(base.clone(), &theirs_edits);
        let mut trees = Trees::default();
        let (b, o, t) = (trees.add(&base), trees.add(&ours), trees.add(&theirs));
        let filter = prefixes.clone().map_or_else(PathFilter::all, PathFilter::under);

        let paths: BTreeSet<&RepoPath> =
            base.keys().chain(ours.keys()).chain(theirs.keys()).collect();
        let want: Vec<PathTriple> = paths
            .into_iter()
            .filter(|p| prefixes.as_ref().is_none_or(|prefixes| is_under(p, prefixes)))
            .filter_map(|p| {
                let at = |files: &Files| files.get(p).copied();
                let (base, ours, theirs) = (at(&base), at(&ours), at(&theirs));
                let triple = PathTriple::new(p.clone(), base, ours, theirs);
                (ours != base || theirs != base).then_some(triple)
            })
            .collect();
        prop_assert_eq!(three_way(&trees, b, o, t, &filter).unwrap(), want);
    }
}

#[test]
fn diff_trees_never_reads_a_subtree_both_sides_share() {
    let from = files(&[
        ("a/b/c", file(1)),
        ("a/b/d", file(1)),
        ("a/e/f", file(1)),
        ("g/h/i", file(1)),
        ("j", file(1)),
    ]);
    let mut to = from.clone();
    to.insert(path("a/b/c"), file(2));
    let mut trees = Trees::default();
    let (x, y) = (trees.add(&from).unwrap(), trees.add(&to).unwrap());

    let changes = diff_trees(&trees, Some(x), Some(y), &PathFilter::all()).unwrap();
    assert_eq!(changes, [modified("a/b/c", file(1), file(2))]);
    // Two trees a level, those on the changed leaf's path: never `a/e`, `g` or `g/h`.
    let on_the_path = vec![
        x,
        y,
        trees.dir_at(x, "a"),
        trees.dir_at(y, "a"),
        trees.dir_at(x, "a/b"),
        trees.dir_at(y, "a/b"),
    ];
    assert_eq!(trees.take_reads(), sorted(on_the_path));

    // A root diffed with itself is not read at all.
    let same = diff_trees(&trees, Some(x), Some(x), &PathFilter::all()).unwrap();
    assert_eq!(same, Vec::new());
    assert_eq!(trees.take_reads(), Vec::new());
}

#[test]
fn a_file_that_becomes_a_directory_is_a_delete_and_puts() {
    let from = files(&[("a", file(1))]);
    let to = files(&[("a/x", file(1)), ("a/y", file(2))]);
    let mut trees = Trees::default();

    let changes = diff(&mut trees, &from, &to, &PathFilter::all());
    assert_eq!(
        changes,
        [
            deleted("a", file(1)),
            added("a/x", file(1)),
            added("a/y", file(2)),
        ]
    );
    assert_eq!(
        delta_of(&changes),
        [delete("a"), put("a/x", file(1)), put("a/y", file(2))]
    );

    // And back: the directory's leaves are deleted, the file put.
    assert_eq!(
        diff(&mut trees, &to, &from, &PathFilter::all()),
        [
            added("a", file(1)),
            deleted("a/x", file(1)),
            deleted("a/y", file(2)),
        ]
    );
}

#[test]
fn a_file_that_becomes_a_symlink_is_a_type_change() {
    let from = files(&[("f", file(1)), ("s", symlink(1)), ("x", exec(1))]);
    let to = files(&[("f", symlink(1)), ("s", exec(1)), ("x", symlink(2))]);
    let mut trees = Trees::default();

    assert_eq!(
        diff(&mut trees, &from, &to, &PathFilter::all()),
        [
            type_changed("f", file(1), symlink(1)),
            type_changed("s", symlink(1), exec(1)),
            type_changed("x", exec(1), symlink(2)),
        ]
    );

    // A symlink that comes to point elsewhere stays a symlink: a modification.
    let moved = files(&[("f", file(1)), ("s", symlink(2)), ("x", exec(1))]);
    assert_eq!(
        diff(&mut trees, &from, &moved, &PathFilter::all()),
        [modified("s", symlink(1), symlink(2))]
    );
}

#[test]
fn an_exec_bit_change_is_a_modification() {
    let from = files(&[("build.sh", file(1)), ("run.sh", file(1))]);
    let to = files(&[("build.sh", exec(2)), ("run.sh", exec(1))]);
    let mut trees = Trees::default();

    let changes = diff(&mut trees, &from, &to, &PathFilter::all());
    assert_eq!(
        changes,
        [
            modified("build.sh", file(1), exec(2)),
            modified("run.sh", file(1), exec(1)),
        ]
    );
    assert_eq!(
        delta_of(&changes),
        [put("build.sh", exec(2)), put("run.sh", exec(1))]
    );
}

#[test]
fn a_filter_reads_only_what_it_reaches() {
    let from = files(&[
        ("README", file(1)),
        ("docs/x.md", file(1)),
        ("src/a.rs", file(1)),
        ("src/lib/b.rs", file(1)),
        ("src-old/y", file(1)),
        ("srcs", file(1)),
    ]);
    let to = files(&[
        ("README", file(2)),
        ("docs/x.md", file(2)),
        ("src/a.rs", file(2)),
        ("src/new.rs", file(1)),
        ("src-old/y", file(2)),
        ("srcs", file(2)),
    ]);
    let mut trees = Trees::default();
    let (x, y) = (trees.add(&from).unwrap(), trees.add(&to).unwrap());
    let roots = vec![x, y];
    let src = vec![x, y, trees.dir_at(x, "src"), trees.dir_at(y, "src")];
    let diff_under = |prefixes: &[&str]| {
        let filter = PathFilter::under(prefixes.iter().map(|p| path(p)).collect());
        diff_trees(&trees, Some(x), Some(y), &filter).unwrap()
    };

    // Under `src`: the roots and the trees of `src`, never `docs` or `src-old`; the changes
    // below `src` only.
    assert_eq!(
        diff_under(&["src"]),
        [
            modified("src/a.rs", file(1), file(2)),
            deleted("src/lib/b.rs", file(1)),
            added("src/new.rs", file(1)),
        ]
    );
    let mut src_and_lib = src.clone();
    src_and_lib.push(trees.dir_at(x, "src/lib"));
    assert_eq!(trees.take_reads(), sorted(src_and_lib));

    // A prefix that is a file matches that file.
    assert_eq!(diff_under(&["srcs"]), [modified("srcs", file(1), file(2))]);
    assert_eq!(trees.take_reads(), sorted(roots.clone()));
    assert_eq!(
        diff_under(&["src/a.rs"]),
        [modified("src/a.rs", file(1), file(2))]
    );
    assert_eq!(trees.take_reads(), sorted(src));

    // Prefixes that overlap are their union; a prefix that names nothing matches nothing.
    assert_eq!(
        diff_under(&["src/lib", "README", "src", "nothing/here"]),
        [
            modified("README", file(1), file(2)),
            modified("src/a.rs", file(1), file(2)),
            deleted("src/lib/b.rs", file(1)),
            added("src/new.rs", file(1)),
        ]
    );
    trees.take_reads();
    assert_eq!(diff_under(&["nothing/here"]), Vec::new());
    assert_eq!(trees.take_reads(), sorted(roots));

    // No prefix at all: nothing matches, and nothing is read.
    assert_eq!(diff_under(&[]), Vec::new());
    assert_eq!(trees.take_reads(), Vec::new());
}

#[test]
fn paths_sort_by_bytes_not_by_tree_order() {
    // In tree order `a/b` comes first (the root's entry `a` sorts before `a-c` and `a.d`); by
    // bytes `-` (0x2d) and `.` (0x2e) sort before `/` (0x2f).
    let from = files(&[
        ("a/b", file(1)),
        ("a-c", file(1)),
        ("a.d/e", file(1)),
        ("b/x", file(1)),
    ]);
    let to = files(&[
        ("a/b", file(2)),
        ("a-c", file(2)),
        ("a.d/e", file(2)),
        ("b/x", file(2)),
    ]);
    let mut trees = Trees::default();

    let changes = diff(&mut trees, &from, &to, &PathFilter::all());
    let paths: Vec<&str> = changes.iter().map(|c| c.path.as_str()).collect();
    assert_eq!(paths, ["a-c", "a.d/e", "a/b", "b/x"]);
}

#[test]
fn delta_of_a_diff_is_canonical() {
    let from = files(&[("a", file(1)), ("a-c/x", file(1)), ("d/e", file(1))]);
    let to = files(&[
        ("a/x", file(1)),
        ("a/y", exec(2)),
        ("a-c/x", file(2)),
        ("d", symlink(3)),
    ]);
    let mut trees = Trees::default();
    let changes = diff(&mut trees, &from, &to, &PathFilter::all());
    let delta = delta_of(&changes);
    assert_eq!(
        delta,
        [
            delete("a"),
            put("a-c/x", file(2)),
            put("a/x", file(1)),
            put("a/y", exec(2)),
            put("d", symlink(3)),
            delete("d/e"),
        ]
    );

    // A changeset whose delta it is encodes: the codec's order rules accept it.
    let signature = Signature::new(
        b"agent".to_vec(),
        b"agent@example.com".to_vec(),
        0,
        0,
        false,
    )
    .unwrap();
    let changeset = Changeset::new(
        vec![],
        trees.add(&to).unwrap(),
        delta,
        signature.clone(),
        signature,
        b"m".to_vec(),
        vec![],
        vec![],
    )
    .unwrap();
    let bytes = changeset.encode().unwrap();
    assert_eq!(Changeset::decode(&bytes).unwrap(), changeset);
}

#[test]
fn three_way_lists_each_side_at_each_changed_path() {
    let base = files(&[("a", file(1)), ("b", file(1)), ("c", file(1))]);
    let ours = files(&[
        ("a", file(2)),
        ("b", file(1)),
        ("c", file(1)),
        ("d", file(1)),
    ]);
    let theirs = files(&[("a", file(1)), ("b", file(3))]);
    let mut trees = Trees::default();
    let (b, o, t) = (trees.add(&base), trees.add(&ours), trees.add(&theirs));

    let triple = |p: &str, base: Option<Leaf>, ours: Option<Leaf>, theirs: Option<Leaf>| {
        PathTriple::new(path(p), base, ours, theirs)
    };
    assert_eq!(
        three_way(&trees, b, o, t, &PathFilter::all()).unwrap(),
        [
            triple("a", Some(file(1)), Some(file(2)), Some(file(1))),
            triple("b", Some(file(1)), Some(file(1)), Some(file(3))),
            triple("c", Some(file(1)), Some(file(1)), None),
            triple("d", None, Some(file(1)), None),
        ]
    );
}
