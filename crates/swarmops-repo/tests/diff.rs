//! A diff of two changesets, read in one snapshot (storage spec §4, `RepoRead::diff`): the
//! leaf-level changes between their root trees, with the snapshot as the trees' reader.

use swarmops_core::{ChangesetId, LeafKind, ObjectKind, TreeId, hash_object};
use swarmops_repo::merge::tree_diff::{
    ChangeKind, Leaf, PathFilter, TreeChange, TreeRead, diff_trees,
};
use swarmops_repo::{LocalRepo, RepoError};
use swarmops_testkit::repo::{blob, commit, delete, new_repo, path, put, put_as};
use tempfile::TempDir;

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

/// The change of the leaf at `p`.
fn change(p: &str, kind: ChangeKind) -> TreeChange {
    TreeChange {
        path: path(p),
        kind,
    }
}

/// Three changesets in a row on a fresh store, c1 ← c2 ← c3: c2 changes a file's content and
/// another's exec bit, turns a symlink into a file, deletes a file and adds one; c3 adds one
/// more. The store, their ids, and the changes from c1 to c2 in path order.
fn three_changesets() -> (TempDir, LocalRepo, [ChangesetId; 3], Vec<TreeChange>) {
    let (dir, _root, repo) = new_repo();
    let c1 = commit(
        &repo,
        &[],
        vec![
            put("README.md", b"# demo\n"),
            put("bin/run", b"#!/bin/sh\n"),
            put_as("latest", LeafKind::Symlink, b"README.md"),
            put("src/lib.rs", b"pub fn f() {}\n"),
            put("src/old.rs", b"old\n"),
        ],
    );
    let c2 = commit(
        &repo,
        &[c1],
        vec![
            put("README.md", b"# demo\n\nv2\n"),
            put_as("bin/run", LeafKind::Exec, b"#!/bin/sh\n"),
            put("latest", b"README.md"),
            delete("src/old.rs"),
            put("src/new.rs", b"new\n"),
        ],
    );
    let c3 = commit(&repo, &[c2], vec![put("docs/guide.md", b"guide\n")]);
    let c1_to_c2 = vec![
        change(
            "README.md",
            ChangeKind::Modified {
                old: file(b"# demo\n"),
                new: file(b"# demo\n\nv2\n"),
            },
        ),
        change(
            "bin/run",
            ChangeKind::Modified {
                old: file(b"#!/bin/sh\n"),
                new: leaf(LeafKind::Exec, b"#!/bin/sh\n"),
            },
        ),
        change(
            "latest",
            ChangeKind::TypeChanged {
                old: leaf(LeafKind::Symlink, b"README.md"),
                new: file(b"README.md"),
            },
        ),
        change(
            "src/new.rs",
            ChangeKind::Added {
                new: file(b"new\n"),
            },
        ),
        change(
            "src/old.rs",
            ChangeKind::Deleted {
                old: file(b"old\n"),
            },
        ),
    ];
    (dir, repo, [c1, c2, c3], c1_to_c2)
}

/// `RepoRead::diff(x, y, filter)` is `diff_trees` of the two changesets' roots, read through the
/// snapshot: every kind of change a leaf can go through (its content, its exec bit, a symlink
/// that becomes a file, a deletion, an addition), in path order; changesets that are not
/// neighbours, either way round; a filter; a changeset against itself; and a changeset the
/// snapshot does not hold, which is `NotFound`. The snapshot reads a tree by its id, verified,
/// and a tree the store lacks is `MissingObject`.
#[test]
fn diff_between_two_changesets_is_their_roots_diff() {
    let (_dir, repo, [c1, c2, c3], c1_to_c2) = three_changesets();
    let read = repo.snapshot().unwrap();
    let all = PathFilter::all();
    assert_eq!(read.diff(c1, c2, &all).unwrap(), c1_to_c2);
    // The roots' diff, with the snapshot as the trees' reader.
    let root = |cs: ChangesetId| read.changeset(cs).unwrap().unwrap().changeset.root;
    assert_eq!(
        diff_trees(&read, Some(root(c1)), Some(root(c2)), &all).unwrap(),
        c1_to_c2
    );
    // Two changesets apart: c2's changes and c3's, in path order.
    let mut c1_to_c3 = c1_to_c2.clone();
    c1_to_c3.insert(
        2,
        change(
            "docs/guide.md",
            ChangeKind::Added {
                new: file(b"guide\n"),
            },
        ),
    );
    assert_eq!(read.diff(c1, c3, &all).unwrap(), c1_to_c3);
    // Within a filter only; backwards, each change turned around; nothing between a changeset
    // and itself.
    let src = PathFilter::under(vec![path("src")]);
    assert_eq!(read.diff(c1, c2, &src).unwrap(), c1_to_c2[3..]);
    assert_eq!(
        read.diff(c2, c1, &src).unwrap(),
        [
            change(
                "src/new.rs",
                ChangeKind::Deleted {
                    old: file(b"new\n")
                }
            ),
            change(
                "src/old.rs",
                ChangeKind::Added {
                    new: file(b"old\n")
                }
            ),
        ]
    );
    assert_eq!(read.diff(c3, c3, &all).unwrap(), Vec::<TreeChange>::new());

    // A changeset the snapshot does not hold, on either side.
    let unknown = ChangesetId(hash_object(ObjectKind::Changeset, b"never committed"));
    for (from, to) in [(unknown, c2), (c1, unknown)] {
        match read.diff(from, to, &all) {
            Err(RepoError::NotFound(missing)) => assert_eq!(missing, unknown.to_string()),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    // A tree by its id, as `TreeRead`: the snapshot's verified read.
    assert_eq!(
        TreeRead::tree(&read, root(c1)).unwrap(),
        read.tree(root(c1)).unwrap()
    );
    let missing = TreeId(hash_object(ObjectKind::Tree, b"never stored"));
    match TreeRead::tree(&read, missing) {
        Err(RepoError::MissingObject { id, kind }) => {
            assert_eq!((id, kind), (missing.0, ObjectKind::Tree));
        }
        other => panic!("expected MissingObject, got {other:?}"),
    }
}
