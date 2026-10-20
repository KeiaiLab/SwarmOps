//! The three-way tree merge (storage spec §4; `Repo::merge_with`, `Repo::merge_trees_with`): the
//! paths ours changed since the base, each decided against what theirs holds there, returned as
//! data — the delta that turns theirs' root into the merged one, the contents the merge made, and
//! the conflicts. A fake content merger stands in where the content is not the point, and diff3
//! (`merge_content`) where it is.
//!
//! Every test owns a fresh store on the real filesystem without its flushes (`NoSyncFs`): a merge
//! writes nothing, and what the tests check is what it reads and what it returns.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use camino::Utf8PathBuf;
use proptest::prelude::*;
use swarmops_core::{
    ChangesetId, DeltaEntry, DeltaOp, EntryTarget, LeafKind, ObjectKind, RepoPath, TreeId,
    hash_object,
};
use swarmops_repo::merge::conflict::{ConflictHunk, ConflictKind, ContentMerge};
use swarmops_repo::merge::diff3::merge_content;
use swarmops_repo::merge::tree_diff::{Leaf, PathFilter, PathTriple, delta_of};
use swarmops_repo::merge::tree_merge::{FileConflict, PathPlan, entry_in, plan_path};
use swarmops_repo::{Change, ChangeOp, LocalRepo, MergeOutcome, RepoError};
use swarmops_store::FsOps;
use swarmops_testkit::fs::{Call, NoSyncFs, Watch, Watched};
use swarmops_testkit::repo::{blob, delete, path, put, put_as, request};
use tempfile::TempDir;

/// A fresh store in a temp dir of its own, on a filesystem that never syncs.
fn unsynced_repo() -> (TempDir, Utf8PathBuf, LocalRepo) {
    let dir = tempfile::tempdir().expect("create a temp dir");
    let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).expect("temp dir is UTF-8");
    let repo = LocalRepo::create_with_fs(&root, "test", Arc::new(NoSyncFs)).expect("create");
    (dir, root, repo)
}

/// Commits `changes` on `parents` (none: a root changeset); a list that changes nothing is
/// committed too, with the parent's tree. Its id.
#[track_caller]
fn commit_on(repo: &LocalRepo, parents: &[ChangesetId], changes: Vec<Change>) -> ChangesetId {
    let mut req = request(parents, changes, "a merge test commit");
    req.allow_empty = true;
    repo.commit(req).expect("commit").id
}

/// The base, ours and theirs of a merge: `base` committed as a root, then `ours` and `theirs`
/// each committed on it.
#[track_caller]
fn three(
    repo: &LocalRepo,
    base: Vec<Change>,
    ours: Vec<Change>,
    theirs: Vec<Change>,
) -> [ChangesetId; 3] {
    let base = commit_on(repo, &[], base);
    [
        base,
        commit_on(repo, &[base], ours),
        commit_on(repo, &[base], theirs),
    ]
}

/// Changeset `cs`'s root tree.
fn root_of(repo: &LocalRepo, cs: ChangesetId) -> TreeId {
    repo.changeset(cs).unwrap().unwrap().changeset.root
}

/// The changeset that commits `outcome` on `theirs`, as the merge's caller would: the contents
/// the merge made stored first, then its delta committed on theirs, each put naming its blob.
#[track_caller]
fn applied(repo: &LocalRepo, theirs: ChangesetId, outcome: &MergeOutcome) -> ChangesetId {
    for (id, content) in &outcome.new_blobs {
        let stored = repo.put_blob(content.clone()).unwrap();
        assert_eq!(stored.id, *id, "new_blobs keys each content by its blob id");
    }
    let changes = outcome
        .delta
        .iter()
        .map(|entry| Change {
            path: entry.path.clone(),
            op: match entry.op {
                DeltaOp::Put { kind, id } => ChangeOp::PutBlob { kind, id },
                DeltaOp::Delete => ChangeOp::Delete,
            },
        })
        .collect();
    commit_on(repo, &[theirs], changes)
}

/// Every leaf of changeset `cs`'s tree: its path, kind and content.
fn files_of(repo: &LocalRepo, cs: ChangesetId) -> BTreeMap<String, (LeafKind, Vec<u8>)> {
    let read = repo.snapshot().unwrap();
    let page = read
        .list_tree_depth(cs, None, u32::MAX, None, usize::MAX)
        .unwrap()
        .unwrap();
    page.entries
        .into_iter()
        .filter_map(|entry| match entry.target {
            EntryTarget::Leaf(kind, id) => {
                Some((entry.path.to_string(), (kind, read.blob(id).unwrap())))
            }
            EntryTarget::Dir(_) => None,
        })
        .collect()
}

/// Regular files with these paths and contents, as [`files_of`] gives them.
fn files(entries: &[(&str, &[u8])]) -> BTreeMap<String, (LeafKind, Vec<u8>)> {
    entries
        .iter()
        .map(|&(p, content)| (p.to_owned(), (LeafKind::File, content.to_vec())))
        .collect()
}

/// The leaf of `kind` that holds `content`.
fn leaf(kind: LeafKind, content: &[u8]) -> Leaf {
    Leaf {
        kind,
        blob: blob(content),
    }
}

/// The regular file that holds `content`.
fn file(content: &[u8]) -> Leaf {
    leaf(LeafKind::File, content)
}

/// The delta entry that puts `leaf` at `p`.
fn put_entry(p: &str, leaf: Leaf) -> DeltaEntry {
    DeltaEntry {
        path: path(p),
        op: DeltaOp::Put {
            kind: leaf.kind,
            id: leaf.blob,
        },
        renamed_from: None,
    }
}

/// The delta entry that deletes `p`.
fn delete_entry(p: &str) -> DeltaEntry {
    DeltaEntry {
        path: path(p),
        op: DeltaOp::Delete,
        renamed_from: None,
    }
}

/// The conflict at `p`, of `kind`, with the base's, ours' and theirs' leaves there.
fn conflict(
    p: &str,
    kind: ConflictKind,
    (base, ours, theirs): (Option<Leaf>, Option<Leaf>, Option<Leaf>),
) -> FileConflict {
    FileConflict {
        path: path(p),
        kind,
        base,
        ours,
        theirs,
    }
}

/// A content merger for the merges that must not call one.
fn no_content_merge(base: &[u8], ours: &[u8], theirs: &[u8]) -> ContentMerge {
    panic!("the content merger was called: base {base:?}, ours {ours:?}, theirs {theirs:?}")
}

/// A content merger that merges every file cleanly into `merged`, and records the base's, ours'
/// and theirs' contents of each call.
struct MergingTo {
    merged: Vec<u8>,
    calls: RefCell<Vec<[Vec<u8>; 3]>>,
}

impl MergingTo {
    fn new(merged: &[u8]) -> Self {
        Self {
            merged: merged.to_vec(),
            calls: RefCell::new(Vec::new()),
        }
    }

    fn merge(&self, base: &[u8], ours: &[u8], theirs: &[u8]) -> ContentMerge {
        let call = [base.to_vec(), ours.to_vec(), theirs.to_vec()];
        self.calls.borrow_mut().push(call);
        ContentMerge::Clean(self.merged.clone())
    }

    /// The calls so far, each as its three contents.
    fn calls(&self) -> Vec<[Vec<u8>; 3]> {
        self.calls.borrow().clone()
    }
}

/// Ours and theirs changed different paths: the delta is ours' changes (an edit, an addition, a
/// deletion), in path order, and committed on theirs it holds both sides' changes.
#[test]
fn disjoint_paths_merge_into_both() {
    let (_dir, _root, repo) = unsynced_repo();
    let [base, ours, theirs] = three(
        &repo,
        vec![
            put("README.md", b"v1\n"),
            put("docs/old.md", b"old\n"),
            put("src/a.rs", b"a1\n"),
            put("src/b.rs", b"b1\n"),
        ],
        vec![
            put("src/a.rs", b"a2\n"),
            put("src/new.rs", b"new\n"),
            delete("docs/old.md"),
        ],
        vec![
            put("README.md", b"v2\n"),
            put("src/b.rs", b"b2\n"),
            put("tests/t.rs", b"t\n"),
        ],
    );
    let outcome = repo
        .merge_with(base, ours, theirs, &no_content_merge)
        .unwrap();
    let delta = vec![
        delete_entry("docs/old.md"),
        put_entry("src/a.rs", file(b"a2\n")),
        put_entry("src/new.rs", file(b"new\n")),
    ];
    assert_eq!(
        outcome,
        MergeOutcome {
            delta,
            ..MergeOutcome::default()
        }
    );
    let merged = applied(&repo, theirs, &outcome);
    assert_eq!(
        files_of(&repo, merged),
        files(&[
            ("README.md", b"v2\n"),
            ("src/a.rs", b"a2\n"),
            ("src/b.rs", b"b2\n"),
            ("src/new.rs", b"new\n"),
            ("tests/t.rs", b"t\n"),
        ])
    );
}

/// A file both sides changed is merged by the content merger, called once, with the base's, ours'
/// and theirs' contents in that order. Clean bytes neither side holds are put as a new blob and the
/// path is auto-merged; clean bytes that are theirs add nothing; clean bytes that are ours put
/// ours' blob, which the store holds already.
#[test]
fn a_file_changed_on_both_sides_is_merged_by_content() {
    let (_dir, _root, repo) = unsynced_repo();
    let [base, ours, theirs] = three(
        &repo,
        vec![put("p", b"base\n"), put("q", b"q\n")],
        vec![put("p", b"ours\n")],
        vec![put("p", b"theirs\n"), put("q", b"q2\n")],
    );
    let to = MergingTo::new(b"merged\n");
    let outcome = repo
        .merge_with(base, ours, theirs, &|b, o, t| to.merge(b, o, t))
        .unwrap();
    let three_contents = [b"base\n".to_vec(), b"ours\n".to_vec(), b"theirs\n".to_vec()];
    assert_eq!(to.calls(), [three_contents]);
    assert_eq!(
        outcome,
        MergeOutcome {
            delta: vec![put_entry("p", file(b"merged\n"))],
            new_blobs: BTreeMap::from([(blob(b"merged\n"), b"merged\n".to_vec())]),
            conflicts: Vec::new(),
            auto_merged: vec![path("p")],
        }
    );
    let merged = applied(&repo, theirs, &outcome);
    assert_eq!(
        files_of(&repo, merged),
        files(&[("p", b"merged\n"), ("q", b"q2\n")])
    );

    // Merged to theirs' bytes: nothing to put.
    let to = MergingTo::new(b"theirs\n");
    let outcome = repo
        .merge_with(base, ours, theirs, &|b, o, t| to.merge(b, o, t))
        .unwrap();
    assert_eq!(outcome, MergeOutcome::default());
    // Merged to ours' bytes: ours' blob, no new one, nothing auto-merged.
    let to = MergingTo::new(b"ours\n");
    let outcome = repo
        .merge_with(base, ours, theirs, &|b, o, t| to.merge(b, o, t))
        .unwrap();
    assert_eq!(
        outcome,
        MergeOutcome {
            delta: vec![put_entry("p", file(b"ours\n"))],
            ..MergeOutcome::default()
        }
    );
}

/// Both sides edited the same line: with diff3 as the content merger, the path conflicts with
/// diff3's hunk, and the conflict names the base's, ours' and theirs' leaves. Nothing is put
/// there, while a path only ours changed is.
#[test]
fn a_content_conflict_carries_its_hunks() {
    let (_dir, _root, repo) = unsynced_repo();
    let [base, ours, theirs] = three(
        &repo,
        vec![put("src/lib.rs", b"a\nb\nc\n"), put("other", b"1\n")],
        vec![put("src/lib.rs", b"a\nB\nc\n"), put("other", b"2\n")],
        vec![put("src/lib.rs", b"a\nX\nc\n")],
    );
    let outcome = repo.merge_with(base, ours, theirs, &merge_content).unwrap();
    let hunk = ConflictHunk {
        base: 1..2,
        ours: 1..2,
        theirs: 1..2,
        base_text: b"b\n".to_vec(),
        ours_text: b"B\n".to_vec(),
        theirs_text: b"X\n".to_vec(),
    };
    let sides = (
        Some(file(b"a\nb\nc\n")),
        Some(file(b"a\nB\nc\n")),
        Some(file(b"a\nX\nc\n")),
    );
    assert_eq!(
        outcome,
        MergeOutcome {
            delta: vec![put_entry("other", file(b"2\n"))],
            conflicts: vec![conflict(
                "src/lib.rs",
                ConflictKind::Content { hunks: vec![hunk] },
                sides
            )],
            ..MergeOutcome::default()
        }
    );
}

/// Ours deleted a file theirs changed (`delete_modify`), and changed a file theirs deleted
/// (`modify_delete`): both conflict, each with the leaves it has on each side.
#[test]
fn delete_against_modify_and_modify_against_delete_conflict() {
    let (_dir, _root, repo) = unsynced_repo();
    let [base, ours, theirs] = three(
        &repo,
        vec![put("d", b"1\n"), put("m", b"1\n")],
        vec![delete("d"), put("m", b"2\n")],
        vec![put("d", b"2\n"), delete("m")],
    );
    let outcome = repo
        .merge_with(base, ours, theirs, &no_content_merge)
        .unwrap();
    let conflicts = vec![
        conflict(
            "d",
            ConflictKind::DeleteModify,
            (Some(file(b"1\n")), None, Some(file(b"2\n"))),
        ),
        conflict(
            "m",
            ConflictKind::ModifyDelete,
            (Some(file(b"1\n")), Some(file(b"2\n")), None),
        ),
    ];
    assert_eq!(
        outcome,
        MergeOutcome {
            conflicts,
            ..MergeOutcome::default()
        }
    );
}

/// Both sides added a file the base lacks: the same file is one change, nothing to put; different
/// contents are merged over an empty base, and diff3's conflict there is `add_add`.
#[test]
fn add_add_conflicts_unless_identical() {
    let (_dir, _root, repo) = unsynced_repo();
    let [base, ours, theirs] = three(
        &repo,
        vec![put("keep", b"k\n")],
        vec![put("both", b"ours\n"), put("same", b"x\n")],
        vec![put("both", b"theirs\n"), put("same", b"x\n")],
    );
    let outcome = repo.merge_with(base, ours, theirs, &merge_content).unwrap();
    let hunk = ConflictHunk {
        base: 0..0,
        ours: 0..1,
        theirs: 0..1,
        base_text: Vec::new(),
        ours_text: b"ours\n".to_vec(),
        theirs_text: b"theirs\n".to_vec(),
    };
    let sides = (None, Some(file(b"ours\n")), Some(file(b"theirs\n")));
    assert_eq!(
        outcome,
        MergeOutcome {
            conflicts: vec![conflict(
                "both",
                ConflictKind::AddAdd { hunks: vec![hunk] },
                sides
            )],
            ..MergeOutcome::default()
        }
    );
    // The content merger is called with an empty base.
    let to = MergingTo::new(b"both\n");
    let outcome = repo
        .merge_with(base, ours, theirs, &|b, o, t| to.merge(b, o, t))
        .unwrap();
    assert_eq!(
        to.calls(),
        [[Vec::new(), b"ours\n".to_vec(), b"theirs\n".to_vec()]]
    );
    assert_eq!(outcome.auto_merged, [path("both")]);
}

/// Ours made a file executable and theirs changed its content: the merge keeps both, an
/// executable with theirs' content, without a content merge (only theirs changed the content);
/// and the other way round. `plan_path` decides it from the three leaves alone.
#[test]
fn a_kind_change_on_one_side_keeps_the_others_content() {
    let (_dir, _root, repo) = unsynced_repo();
    let [base, ours, theirs] = three(
        &repo,
        vec![put("run.sh", b"v1\n")],
        vec![put_as("run.sh", LeafKind::Exec, b"v1\n")],
        vec![put("run.sh", b"v2\n")],
    );
    let exec_v2 = leaf(LeafKind::Exec, b"v2\n");
    let outcome = repo
        .merge_with(base, ours, theirs, &no_content_merge)
        .unwrap();
    assert_eq!(
        outcome,
        MergeOutcome {
            delta: vec![put_entry("run.sh", exec_v2)],
            ..MergeOutcome::default()
        }
    );
    let triple = PathTriple {
        path: path("run.sh"),
        base: Some(file(b"v1\n")),
        ours: Some(leaf(LeafKind::Exec, b"v1\n")),
        theirs: Some(file(b"v2\n")),
    };
    assert_eq!(plan_path(&triple), PathPlan::Put(exec_v2));
    // Ours changed the content, theirs the kind.
    let swapped = repo
        .merge_with(base, theirs, ours, &no_content_merge)
        .unwrap();
    assert_eq!(
        swapped,
        MergeOutcome {
            delta: vec![put_entry("run.sh", exec_v2)],
            ..MergeOutcome::default()
        }
    );
}

/// Both sides changed a file's kind, differently: `mode`, with the two kinds, whatever the
/// contents; so do two additions of one path with different kinds.
#[test]
fn kinds_changed_differently_conflict_as_mode() {
    let (_dir, _root, repo) = unsynced_repo();
    let [base, ours, theirs] = three(
        &repo,
        vec![put("tool", b"1\n")],
        vec![
            put_as("tool", LeafKind::Exec, b"1\n"),
            put_as("new", LeafKind::Exec, b"n\n"),
        ],
        vec![
            put_as("tool", LeafKind::Symlink, b"2\n"),
            put("new", b"n\n"),
        ],
    );
    let outcome = repo
        .merge_with(base, ours, theirs, &no_content_merge)
        .unwrap();
    let conflicts = vec![
        conflict(
            "new",
            ConflictKind::Mode {
                ours: LeafKind::Exec,
                theirs: LeafKind::File,
            },
            (None, Some(leaf(LeafKind::Exec, b"n\n")), Some(file(b"n\n"))),
        ),
        conflict(
            "tool",
            ConflictKind::Mode {
                ours: LeafKind::Exec,
                theirs: LeafKind::Symlink,
            },
            (
                Some(file(b"1\n")),
                Some(leaf(LeafKind::Exec, b"1\n")),
                Some(leaf(LeafKind::Symlink, b"2\n")),
            ),
        ),
    ];
    assert_eq!(
        outcome,
        MergeOutcome {
            conflicts,
            ..MergeOutcome::default()
        }
    );
}

/// A file against a directory is `type_change`, at ours' path: ours puts `a/b` where theirs has
/// a file `a`; ours puts a file `d` where theirs has a directory `d`; ours deletes the file `e`,
/// holding nothing there, where theirs has made `e` a directory. Theirs' leaves stay as they are.
#[test]
fn a_file_against_a_directory_is_a_type_change() {
    let (_dir, _root, repo) = unsynced_repo();
    let [base, ours, theirs] = three(
        &repo,
        vec![put("e", b"1\n"), put("keep", b"k\n")],
        vec![put("a/b", b"ab\n"), put("d", b"d\n"), delete("e")],
        vec![
            put("a", b"a\n"),
            put("d/x", b"dx\n"),
            delete("e"),
            put("e/y", b"ey\n"),
        ],
    );
    let outcome = repo
        .merge_with(base, ours, theirs, &no_content_merge)
        .unwrap();
    let conflicts = vec![
        conflict(
            "a/b",
            ConflictKind::TypeChange,
            (None, Some(file(b"ab\n")), None),
        ),
        conflict(
            "d",
            ConflictKind::TypeChange,
            (None, Some(file(b"d\n")), None),
        ),
        conflict(
            "e",
            ConflictKind::TypeChange,
            (Some(file(b"1\n")), None, None),
        ),
    ];
    assert_eq!(
        outcome,
        MergeOutcome {
            conflicts,
            ..MergeOutcome::default()
        }
    );
}

/// A file and a directory that only ours swapped merge cleanly: ours turns the file `f` into a
/// directory and the directory `h` into a file, theirs leaves both alone, and the delta turns
/// theirs' tree into one with ours' swaps (the merge deletes theirs' file `f` and the leaves of
/// `h`). Where both sides made `f` a directory, their leaves below it merge.
#[test]
fn a_file_and_a_directory_swapped_by_one_side_merge_cleanly() {
    let (_dir, _root, repo) = unsynced_repo();
    let base_files = vec![
        put("f", b"f\n"),
        put("h/x", b"x\n"),
        put("h/y", b"y\n"),
        put("other", b"1\n"),
    ];
    let swaps = vec![
        delete("f"),
        put("f/g", b"g\n"),
        delete("h/x"),
        delete("h/y"),
        put("h", b"h\n"),
    ];
    let [base, ours, theirs] = three(&repo, base_files, swaps, vec![put("other", b"2\n")]);
    let outcome = repo
        .merge_with(base, ours, theirs, &no_content_merge)
        .unwrap();
    assert_eq!(outcome.conflicts, []);
    let merged = applied(&repo, theirs, &outcome);
    assert_eq!(
        files_of(&repo, merged),
        files(&[("f/g", b"g\n"), ("h", b"h\n"), ("other", b"2\n")])
    );

    // Theirs made `f` a directory too, with another leaf in it.
    let also = commit_on(&repo, &[base], vec![delete("f"), put("f/z", b"z\n")]);
    let outcome = repo
        .merge_with(base, ours, also, &no_content_merge)
        .unwrap();
    assert_eq!(outcome.conflicts, []);
    let merged = applied(&repo, also, &outcome);
    assert_eq!(
        files_of(&repo, merged),
        files(&[
            ("f/g", b"g\n"),
            ("f/z", b"z\n"),
            ("h", b"h\n"),
            ("other", b"1\n"),
        ])
    );
}

/// A symlink both sides changed is not merged by lines: `binary`, without calling the content
/// merger; so is a file one side made a symlink while both changed its content.
#[test]
fn a_symlink_changed_on_both_sides_is_binary() {
    let (_dir, _root, repo) = unsynced_repo();
    let symlink = |target: &[u8]| leaf(LeafKind::Symlink, target);
    let [base, ours, theirs] = three(
        &repo,
        vec![put_as("link", LeafKind::Symlink, b"t1"), put("p", b"1\n")],
        vec![
            put_as("link", LeafKind::Symlink, b"t2"),
            put_as("p", LeafKind::Symlink, b"t"),
        ],
        vec![put_as("link", LeafKind::Symlink, b"t3"), put("p", b"2\n")],
    );
    let outcome = repo
        .merge_with(base, ours, theirs, &no_content_merge)
        .unwrap();
    let conflicts = vec![
        conflict(
            "link",
            ConflictKind::Binary,
            (
                Some(symlink(b"t1")),
                Some(symlink(b"t2")),
                Some(symlink(b"t3")),
            ),
        ),
        conflict(
            "p",
            ConflictKind::Binary,
            (Some(file(b"1\n")), Some(symlink(b"t")), Some(file(b"2\n"))),
        ),
    ];
    assert_eq!(
        outcome,
        MergeOutcome {
            conflicts,
            ..MergeOutcome::default()
        }
    );
}

/// A symlink against a file is `binary` where both sides changed the path, one its kind and the
/// other its target or content (git's "distinct types" conflict), never a leaf of one side's kind
/// holding the other side's bytes: ours made the file `f1` a symlink to its text while theirs
/// edited the text (merged apart, a symlink whose target is a file's text), and `f2` is the same
/// with the sides swapped; ours made the symlink `l1` a regular file of its target while theirs
/// pointed it elsewhere (merged apart, a file holding a link target), and `l2` is swapped; `l3`
/// and `l4` are `l1` and `l2` with both sides' bytes alike. `f3` (a symlink on theirs) and `l5`
/// (a symlink in the base only) are contents both sides changed, never merged by lines. The
/// content merger is never called. Where both sides give the path one kind, kind and content
/// still merge apart, a symlink on the way included: ours made the symlink `k1` a file of other
/// bytes, theirs a file of its target; ours made the file `k2` a symlink elsewhere, theirs a
/// symlink to its text. git 2.43 merges these two cleanly, as here, and conflicts on the others.
#[test]
fn a_symlink_against_a_file_changed_on_both_sides_is_binary() {
    use LeafKind::{File, Symlink};
    /// A path's base, ours and theirs: each a kind and the bytes it holds.
    type Sides = [(LeafKind, &'static [u8]); 3];
    let lines: &[u8] = b"c2 line one\nc2 line two\n";
    let binary: [(&str, Sides); 8] = [
        ("f1", [(File, b"c1\n"), (Symlink, b"c1\n"), (File, lines)]),
        ("f2", [(File, b"c1\n"), (File, lines), (Symlink, b"c1\n")]),
        ("f3", [(File, b"1\n"), (File, b"2\n"), (Symlink, b"t")]),
        ("l1", [(Symlink, b"t1"), (File, b"t1"), (Symlink, b"t2")]),
        ("l2", [(Symlink, b"t1"), (Symlink, b"t2"), (File, b"t1")]),
        ("l3", [(Symlink, b"t1"), (File, b"t2"), (Symlink, b"t2")]),
        ("l4", [(Symlink, b"t1"), (Symlink, b"t2"), (File, b"t2")]),
        ("l5", [(Symlink, b"t"), (File, b"x\n"), (File, b"y\n")]),
    ];
    // Both sides give the path one kind: ours' leaf is put.
    let clean: [(&str, Sides); 2] = [
        ("k1", [(Symlink, b"t1"), (File, b"x"), (File, b"t1")]),
        ("k2", [(File, b"a"), (Symlink, b"b"), (Symlink, b"a")]),
    ];
    let triple = |p: &str, sides: &Sides| {
        let [base, ours, theirs] = sides.map(|(kind, bytes)| Some(leaf(kind, bytes)));
        PathTriple {
            path: path(p),
            base,
            ours,
            theirs,
        }
    };
    let mut wrong = Vec::new();
    let mut want = MergeOutcome::default();
    for (p, sides) in &binary {
        let t = triple(p, sides);
        let plan = plan_path(&t);
        if plan != PathPlan::Conflict(ConflictKind::Binary) {
            wrong.push(format!("{p}: {plan:?}, not binary"));
        }
        let leaves = (t.base, t.ours, t.theirs);
        want.conflicts
            .push(conflict(p, ConflictKind::Binary, leaves));
    }
    for (p, sides) in &clean {
        let t = triple(p, sides);
        let (plan, ours) = (plan_path(&t), t.ours.unwrap());
        if plan != PathPlan::Put(ours) {
            wrong.push(format!("{p}: {plan:?}, not ours' leaf"));
        }
        want.delta.push(put_entry(p, ours));
    }
    assert!(wrong.is_empty(), "plan_path:\n{}", wrong.join("\n"));

    let (_dir, _root, repo) = unsynced_repo();
    let side = |at: usize| -> Vec<Change> {
        let all = binary.iter().chain(&clean);
        all.map(|(p, sides)| put_as(p, sides[at].0, sides[at].1))
            .collect()
    };
    let [base, ours, theirs] = three(&repo, side(0), side(1), side(2));
    let outcome = repo
        .merge_with(base, ours, theirs, &no_content_merge)
        .unwrap();
    assert_eq!(outcome, want);
}

/// Ours deleted the only file of the directory `d`, theirs added another file to it: no conflict,
/// the delta deletes ours' file, and `d` keeps theirs' file.
#[test]
fn a_deleted_directory_keeps_what_theirs_added_under_it() {
    let (_dir, _root, repo) = unsynced_repo();
    let [base, ours, theirs] = three(
        &repo,
        vec![put("d/x", b"x\n")],
        vec![delete("d/x")],
        vec![put("d/z", b"z\n")],
    );
    let outcome = repo
        .merge_with(base, ours, theirs, &no_content_merge)
        .unwrap();
    assert_eq!(
        outcome,
        MergeOutcome {
            delta: vec![delete_entry("d/x")],
            ..MergeOutcome::default()
        }
    );
    let merged = applied(&repo, theirs, &outcome);
    assert_eq!(files_of(&repo, merged), files(&[("d/z", b"z\n")]));
}

/// Counts the reads of object files: the calls of `read` on a path below `objects/`.
#[derive(Default)]
struct ObjectReads(AtomicUsize);

impl Watch for ObjectReads {
    fn before(&self, call: &Call<'_>) -> io::Result<()> {
        let object = call.path().components().any(|c| c.as_os_str() == "objects");
        if call.method == "read" && object {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }
}

/// A merge reads the paths ours changed and nothing else: against a theirs that changed 1,000
/// files in 100 directories, the directory of ours' file among them, a one-file change of ours at
/// depth 3 reads at most 3 × 3 + 3 objects through `merge_with` (each of the three trees on its
/// path, once on each side, and the three changesets), and at most 3 × 3 through
/// `merge_trees_with`, which takes the roots.
#[test]
fn only_the_paths_ours_changed_are_read() {
    let (_dir, root, repo) = unsynced_repo();
    let base = commit_on(
        &repo,
        &[],
        vec![
            put("README.md", b"r\n"),
            put("src/core/keep.rs", b"k\n"),
            put("src/core/lib.rs", b"fn a() {}\n"),
        ],
    );
    let ours = commit_on(&repo, &[base], vec![put("src/core/lib.rs", b"fn b() {}\n")]);
    let mut many: Vec<Change> = (0..99)
        .flat_map(|d| (0..10).map(move |f| (d, f)))
        .map(|(d, f)| {
            put(
                &format!("gen/d{d:02}/f{f}"),
                format!("{d} {f}\n").as_bytes(),
            )
        })
        .collect();
    many.extend((0..10).map(|f| put(&format!("src/core/t{f}.rs"), format!("{f}\n").as_bytes())));
    assert_eq!(many.len(), 1000);
    let theirs = commit_on(&repo, &[base], many);
    let roots = [base, ours, theirs].map(|cs| root_of(&repo, cs));

    let reads = Arc::new(ObjectReads::default());
    let fs: Arc<dyn FsOps> = Arc::new(Watched(Arc::clone(&reads), NoSyncFs));
    let watched = LocalRepo::open_with_fs(&root, fs).unwrap();
    reads.0.store(0, Ordering::SeqCst);
    let depth = 3;
    let outcome = watched
        .merge_with(base, ours, theirs, &no_content_merge)
        .unwrap();
    assert_eq!(
        outcome,
        MergeOutcome {
            delta: vec![put_entry("src/core/lib.rs", file(b"fn b() {}\n"))],
            ..MergeOutcome::default()
        }
    );
    let read = reads.0.swap(0, Ordering::SeqCst);
    assert!(read <= 3 * depth + 3, "merge_with read {read} objects");

    let lib = [path("src/core/lib.rs")];
    let [b, o, t] = roots;
    let again = watched
        .merge_trees_with(b, o, t, &lib, &no_content_merge)
        .unwrap();
    assert_eq!(again, outcome);
    let read = reads.0.swap(0, Ordering::SeqCst);
    assert!(read <= 3 * depth, "merge_trees_with read {read} objects");
}

/// `merge_trees_with` merges the paths it is given and no other: a change of ours outside them is
/// not in the delta. A changeset the store does not hold is `NotFound` for `merge_with`.
#[test]
fn merge_trees_with_merges_only_the_paths_it_is_given() {
    let (_dir, _root, repo) = unsynced_repo();
    let [base, ours, theirs] = three(
        &repo,
        vec![put("a", b"1\n"), put("b", b"1\n")],
        vec![put("a", b"2\n"), put("b", b"2\n")],
        vec![put("c", b"c\n")],
    );
    let [b, o, t] = [base, ours, theirs].map(|cs| root_of(&repo, cs));
    let only_a = repo
        .merge_trees_with(b, o, t, &[path("a")], &no_content_merge)
        .unwrap();
    assert_eq!(
        only_a,
        MergeOutcome {
            delta: vec![put_entry("a", file(b"2\n"))],
            ..MergeOutcome::default()
        }
    );
    let nothing = repo
        .merge_trees_with(b, o, t, &[], &no_content_merge)
        .unwrap();
    assert_eq!(nothing, MergeOutcome::default());

    let unknown = ChangesetId(hash_object(ObjectKind::Changeset, b"never committed"));
    for (b, o, t) in [
        (unknown, ours, theirs),
        (base, unknown, theirs),
        (base, ours, unknown),
    ] {
        match repo.merge_with(b, o, t, &no_content_merge) {
            Err(RepoError::NotFound(missing)) => assert_eq!(missing, unknown.to_string()),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }
}

/// `entry_in` is what a tree holds at a path, from its root: a leaf, a directory, or nothing —
/// also below a file, and below a directory that is not there.
#[test]
fn entry_in_says_what_a_tree_holds_at_a_path() {
    let (_dir, _root, repo) = unsynced_repo();
    let cs = commit_on(
        &repo,
        &[],
        vec![put("a/b/c", b"c\n"), put_as("x", LeafKind::Exec, b"x\n")],
    );
    let read = repo.snapshot().unwrap();
    let root = root_of(&repo, cs);
    let at = |p: &str| entry_in(&read, root, &path(p)).unwrap();
    assert_eq!(
        at("a/b/c"),
        Some(EntryTarget::Leaf(LeafKind::File, blob(b"c\n")))
    );
    assert_eq!(
        at("x"),
        Some(EntryTarget::Leaf(LeafKind::Exec, blob(b"x\n")))
    );
    let Some(EntryTarget::Dir(b)) = at("a/b") else {
        panic!("a/b is a directory");
    };
    assert_eq!(
        read.tree(b).unwrap().entries()[0].name,
        "c",
        "the directory's tree"
    );
    assert_eq!(at("x/y"), None, "below a file");
    assert_eq!(at("a/z/c"), None, "below a missing directory");
    assert_eq!(at("q"), None);
}

/// A tree drawn as the map of its leaves: path → kind and content, one of three one-byte texts.
type Files = BTreeMap<RepoPath, (LeafKind, u8)>;

/// The content of a drawn leaf: `0`, `1` or `2`.
fn text(n: u8) -> [u8; 1] {
    [b'0' + n]
}

/// A drawn leaf as a tree entry names it.
fn drawn((kind, n): (LeafKind, u8)) -> Leaf {
    leaf(kind, &text(n))
}

/// `files` committed as a root changeset: its id.
fn commit_files(repo: &LocalRepo, files: &Files) -> ChangesetId {
    let changes = files
        .iter()
        .map(|(p, &(kind, n))| put_as(p.as_str(), kind, &text(n)))
        .collect();
    commit_on(repo, &[], changes)
}

fn drawn_leaf() -> impl Strategy<Value = (LeafKind, u8)> {
    let kinds = prop::sample::select(vec![LeafKind::File, LeafKind::Exec, LeafKind::Symlink]);
    (kinds, 0..3_u8)
}

/// A path of one to three components from a few names, among them names that sort between a
/// directory and its leaves by bytes (`a-c` and `a.d` between `a` and `a/…`).
fn drawn_path() -> impl Strategy<Value = RepoPath> {
    let name = prop::sample::select(vec!["a", "b", "a-c", "a.d"]);
    prop::collection::vec(name, 1..4).prop_map(|names| path(&names.join("/")))
}

/// Puts (`Some`) and deletes (`None`) of paths.
type Edits = Vec<(RepoPath, Option<(LeafKind, u8)>)>;

fn edits(most: usize) -> impl Strategy<Value = Edits> {
    prop::collection::vec((drawn_path(), prop::option::of(drawn_leaf())), 0..most)
}

/// `files` with `edits` made in order, as commits would make them: a put drops a leaf above its
/// path and everything below it (a file becomes a directory, or a directory a file), a delete
/// drops the path and everything below it.
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
    edits(12).prop_map(|edits| edited(Files::new(), &edits))
}

/// Two trees: one, and the same one edited (they share most subtrees); or two drawn apart.
fn two_trees() -> impl Strategy<Value = (Files, Files)> {
    prop_oneof![
        (tree(), edits(6)).prop_map(|(from, edits)| {
            let to = edited(from.clone(), &edits);
            (from, to)
        }),
        (tree(), tree()),
    ]
}

/// The identities of a three-way merge, over drawn trees: `merge(b, a, a)` and `merge(b, b, x)`
/// put nothing and conflict nowhere, and `merge(b, x, b)` is the delta from `b` to `x`, which
/// committed on `b` gives `x`'s tree — files that became directories and back included. None of
/// them merges a content.
#[test]
fn tree_merge_identities_hold() {
    let (_dir, _root, repo) = unsynced_repo();
    proptest!(|((b, x) in two_trees(), a in tree())| {
        let [b, x, a] = [&b, &x, &a].map(|files| commit_files(&repo, files));
        let merge = |base, ours, theirs| repo.merge_with(base, ours, theirs, &no_content_merge);
        prop_assert_eq!(merge(b, a, a).unwrap(), MergeOutcome::default());
        prop_assert_eq!(merge(b, b, x).unwrap(), MergeOutcome::default());
        let outcome = merge(b, x, b).unwrap();
        let read = repo.snapshot().unwrap();
        let delta = delta_of(&read.diff(b, x, &PathFilter::all()).unwrap());
        let want = MergeOutcome { delta, ..MergeOutcome::default() };
        prop_assert_eq!(&outcome, &want);
        prop_assert_eq!(root_of(&repo, applied(&repo, b, &outcome)), root_of(&repo, x));
    });
}

/// What a merge of three drawn trees must make, path by path, with diff3 as the content merger
/// (drawn contents are one line each, so two different changes of one always conflict): the
/// merged leaves, and the paths that conflict.
struct Expected {
    merged: Files,
    conflicts: BTreeSet<RepoPath>,
}

impl Expected {
    fn of(base: &Files, ours: &Files, theirs: &Files) -> Self {
        let mut expected = Self {
            merged: Files::new(),
            conflicts: BTreeSet::new(),
        };
        let paths: BTreeSet<&RepoPath> = base
            .keys()
            .chain(ours.keys())
            .chain(theirs.keys())
            .collect();
        for p in paths {
            let at = |files: &Files| files.get(p).copied();
            match one_path(at(base), at(ours), at(theirs)) {
                Ok(Some(leaf)) => {
                    expected.merged.insert(p.clone(), leaf);
                }
                Ok(None) => {}
                Err(Conflicts) => {
                    expected.conflicts.insert(p.clone());
                }
            }
            // Ours deleted the file, holding nothing below it, where theirs made it a directory.
            let has_below = |files: &Files| {
                let below = format!("{p}/");
                files.keys().any(|q| q.as_str().starts_with(&below))
            };
            if base.contains_key(p)
                && !ours.contains_key(p)
                && !theirs.contains_key(p)
                && !has_below(ours)
                && has_below(theirs)
            {
                expected.conflicts.insert(p.clone());
            }
        }
        expected
    }

    /// Whether the merge is clean: no path conflicts, and no merged leaf lies below another.
    fn clean(&self) -> bool {
        let below_a_leaf = self.merged.keys().any(|p| {
            let mut above = p.parent();
            while let Some(dir) = above {
                if self.merged.contains_key(&dir) {
                    return true;
                }
                above = dir.parent();
            }
            false
        });
        self.conflicts.is_empty() && !below_a_leaf
    }
}

/// A path whose two sides' changes conflict.
struct Conflicts;

/// What one path merges to from its three leaves: the leaf it holds after (`None`: none), or
/// [`Conflicts`]. Kind and content are taken apart, each from the side that changed it, but a
/// symlink on one side only conflicts: one side changed the kind, the other the bytes.
fn one_path(
    base: Option<(LeafKind, u8)>,
    ours: Option<(LeafKind, u8)>,
    theirs: Option<(LeafKind, u8)>,
) -> Result<Option<(LeafKind, u8)>, Conflicts> {
    if ours == base {
        return Ok(theirs);
    }
    if theirs == base || theirs == ours {
        return Ok(ours);
    }
    let (Some(ours), Some(theirs)) = (ours, theirs) else {
        return Err(Conflicts);
    };
    if ours.0 != theirs.0 && [ours.0, theirs.0].contains(&LeafKind::Symlink) {
        return Err(Conflicts);
    }
    let kind = changed_by_one(base.map(|b| b.0), ours.0, theirs.0).ok_or(Conflicts)?;
    let content = changed_by_one(base.map(|b| b.1), ours.1, theirs.1).ok_or(Conflicts)?;
    Ok(Some((kind, content)))
}

/// One property of a leaf both sides changed (its kind, or its content), merged: the side's that
/// changed it, or the change both made alike; `None` when both changed it, differently.
fn changed_by_one<T: PartialEq + Copy>(base: Option<T>, ours: T, theirs: T) -> Option<T> {
    if Some(ours) == base || ours == theirs {
        Some(theirs)
    } else if Some(theirs) == base {
        Some(ours)
    } else {
        None
    }
}

/// A merge of drawn trees keeps what each side changed, or conflicts: it conflicts exactly when a
/// path conflicts on its own (at least there) or the merged leaves are no tree (a file against a
/// directory), and a clean merge's delta, committed on theirs, gives the merged leaves' tree.
#[test]
fn a_merge_keeps_what_each_side_changed_or_conflicts() {
    let (_dir, _root, repo) = unsynced_repo();
    proptest!(|(base in tree(), ours_edits in edits(6), theirs_edits in edits(6))| {
        let ours = edited(base.clone(), &ours_edits);
        let theirs = edited(base.clone(), &theirs_edits);
        let [b, o, t] = [&base, &ours, &theirs].map(|files| commit_files(&repo, files));
        let outcome = repo.merge_with(b, o, t, &merge_content).unwrap();
        let expected = Expected::of(&base, &ours, &theirs);
        if outcome.conflicts.is_empty() {
            prop_assert!(expected.clean(), "a clean merge where {:?} conflict", expected.conflicts);
            let merged = applied(&repo, t, &outcome);
            prop_assert_eq!(root_of(&repo, merged), root_of(&repo, commit_files(&repo, &expected.merged)));
        } else {
            prop_assert!(!expected.clean(), "conflicts {:?} in a clean merge", outcome.conflicts);
            let reported: BTreeSet<&RepoPath> = outcome.conflicts.iter().map(|c| &c.path).collect();
            for p in &expected.conflicts {
                prop_assert!(reported.contains(p), "{} conflicts, unreported", p);
            }
        }
        // The leaves a conflict names are the drawn ones.
        for c in &outcome.conflicts {
            let at = |files: &Files| files.get(&c.path).copied().map(drawn);
            prop_assert_eq!((c.base, c.ours, c.theirs), (at(&base), at(&ours), at(&theirs)));
        }
    });
}
