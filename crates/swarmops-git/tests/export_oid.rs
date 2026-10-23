//! Export oids (storage spec §5's `GitOid`, §7): the git commit each changeset exports to, derived
//! once and cached as `git_oid`. An imported changeset whose lineage SwarmOps did not change takes
//! its `git.oid` without a tree read; any other is rendered over its root's tree oid, memoized per
//! tree and blob (`git_tree_oid`, `git_blob_oid`): a native changeset with its own signatures and
//! SwarmOps' trailers, a lossy import and what is built on it anew. The oids it pins are git's
//! (SHA-1 over git's object bytes, worked out apart from SwarmOps). Every test owns a fresh store.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::thread;

use swarmops_core::{
    Changeset, ChangesetId, LeafKind, ObjectId, ObjectKind, Signature, Tree, TreeEntry,
};
use swarmops_git::{
    ExportOid, ExportReader, ExportedOid, GitOid, RawKind, TRAILERS, blob_oid, commit_bytes,
    export_message, git_hash, register, render_commit, tree_oid,
};
use swarmops_repo::{Change, LocalRepo, RepoError};
use swarmops_store::DerivedSlot;
use swarmops_testkit::fs::{NoSyncFs, RecordingFs};
use swarmops_testkit::repo::{blob, new_repo, put, put_as, request, signature};
use swarmops_testkit::store::temp_dir;
use tempfile::TempDir;

/// A store of its own with [`ExportOid`] registered, on a filesystem that records each call by
/// the path it acts on in the store (`"read objects/ab/cd…"`).
struct Recorded {
    _dir: TempDir,
    repo: LocalRepo,
    /// The calls made since it last ran.
    calls: Box<dyn Fn() -> Vec<String>>,
}

impl Recorded {
    fn new() -> Recorded {
        let (dir, tmp) = temp_dir();
        let root = tmp.join("store");
        let (fs, calls) = RecordingFs::new();
        let fs = fs.naming_paths_in(root.clone().into_std_path_buf());
        let mut repo = LocalRepo::create_with_fs(&root, "test", Arc::new(fs)).unwrap();
        register(&mut repo).unwrap();
        Recorded {
            _dir: dir,
            repo,
            calls: Box::new(calls),
        }
    }

    /// Forgets the calls made so far.
    fn forget(&self) {
        let _ = (self.calls)();
    }

    /// The objects read since the calls were last taken, by their paths in the store, each as
    /// often as it was read.
    fn object_reads(&self) -> Vec<String> {
        (self.calls)()
            .into_iter()
            .filter_map(|call| {
                let path = call.strip_prefix("read ")?;
                path.starts_with("objects/").then(|| path.to_owned())
            })
            .collect()
    }
}

/// A fresh store with [`ExportOid`] registered.
fn registered() -> (TempDir, LocalRepo) {
    let (dir, _, mut repo) = new_repo();
    register(&mut repo).unwrap();
    (dir, repo)
}

/// Commits `changes` on `parents` with `message` and `metadata` (its keys ascending), signed by
/// the test kit's signature: the changeset's id.
fn commit(
    repo: &LocalRepo,
    parents: &[ChangesetId],
    changes: Vec<Change>,
    message: &str,
    metadata: &[(&str, &str)],
) -> ChangesetId {
    let metadata = (metadata.iter())
        .map(|&(key, value)| (key.to_owned(), value.as_bytes().to_vec()))
        .collect();
    let mut req = request(parents, changes, message);
    req.metadata = metadata;
    repo.commit(req).unwrap().id
}

/// Changeset `id`, which `repo` holds.
fn changeset(repo: &LocalRepo, id: ChangesetId) -> Changeset {
    repo.changeset(id).unwrap().unwrap().changeset
}

/// Changeset `id`'s export oid.
fn value(repo: &LocalRepo, id: ChangesetId) -> ExportedOid {
    repo.derived::<ExportOid>(id, &()).unwrap()
}

/// A changeset no store holds, with `message` and `metadata` (its keys ascending), and its id.
fn unstored(message: &[u8], metadata: &[(&str, &[u8])]) -> (ChangesetId, Changeset) {
    let cs = Changeset::new(
        Vec::new(),
        Tree::empty_root().id(),
        Vec::new(),
        signature(),
        signature(),
        message.to_vec(),
        Vec::new(),
        (metadata.iter())
            .map(|&(key, value)| (key.to_owned(), value.to_vec()))
            .collect(),
    )
    .unwrap();
    (cs.id().unwrap(), cs)
}

/// The git oid of 40 hex digits.
fn oid(hex: &str) -> GitOid {
    hex.parse().unwrap()
}

/// The git oid of a blob of `content`.
fn git_blob(content: &[u8]) -> GitOid {
    git_hash(RawKind::Blob, content)
}

/// The git oid of a tree whose entries, in git's order, are `(mode, name, oid)`.
fn git_tree(entries: &[(&str, &str, GitOid)]) -> GitOid {
    let mut bytes = Vec::new();
    for (mode, name, oid) in entries {
        bytes.extend_from_slice(format!("{mode} {name}\0").as_bytes());
        bytes.extend_from_slice(&oid.0);
    }
    git_hash(RawKind::Tree, &bytes)
}

/// Where object `id` lives in a store: `objects/<2 hex>/<62 hex>`.
fn object_path(id: ObjectId) -> String {
    let hex = id.to_hex();
    format!("objects/{}/{}", &hex[..2], &hex[2..])
}

/// Where the value of derivation `name` (version 1, no key) about `subject` lives in a store.
fn derived_path(name: &str, subject: ObjectId) -> String {
    let hex = subject.to_hex();
    format!("meta/derived/{name}/1/{}/{}", &hex[..2], &hex[2..])
}

#[test]
fn a_native_changeset_renders_to_pinned_bytes() {
    let (_dir, repo) = registered();
    let worker = Signature::new(
        b"claude-code/worker-17".to_vec(),
        b"worker-17@claude-code.swarmops.local".to_vec(),
        1_700_000_000,
        0,
        false,
    )
    .unwrap();
    let squashed = "c".repeat(64);
    let mut req = request(&[], vec![put("a", b"a\n")], "Add a\n");
    req.author = worker.clone();
    req.committer = worker;
    req.metadata = vec![
        ("agent".to_owned(), b"claude-code/worker-17".to_vec()),
        ("land_key".to_owned(), b"k-1".to_vec()),
        ("run_id".to_owned(), b"r1".to_vec()),
        ("squashed_from".to_owned(), squashed.clone().into_bytes()),
    ];
    let id = repo.commit(req).unwrap().id;
    let cs = changeset(&repo, id);
    let read = repo.snapshot().unwrap();
    let reader = ExportReader::new(&read, repo.derived_store());
    // git's oids of the file `a\n` and of the tree of that one file.
    let a = blob_oid(&reader, blob(b"a\n")).unwrap();
    assert_eq!(a, oid("78981922613b2afb6025042ff6bd878ac1994e85"));
    let tree = tree_oid(&reader, cs.root).unwrap();
    assert_eq!(tree, oid("aaff74984cccd156a469afa7d9ab10e4777beb24"));
    // Its own signatures, no extra header, and a trailer block after a blank line: neither
    // `agent` nor `run_id` is a trailer.
    let expected = format!(
        "tree aaff74984cccd156a469afa7d9ab10e4777beb24\n\
         author claude-code/worker-17 <worker-17@claude-code.swarmops.local> 1700000000 +0000\n\
         committer claude-code/worker-17 <worker-17@claude-code.swarmops.local> 1700000000 +0000\n\
         \n\
         Add a\n\
         \n\
         SwarmOps-Changeset: {}\n\
         SwarmOps-Land-Key: k-1\n\
         SwarmOps-Squashed-From: {squashed}\n",
        id.0.to_hex()
    );
    let rendered = render_commit(id, &cs, &[], tree);
    assert_eq!(String::from_utf8(rendered.clone()).unwrap(), expected);
    let exported = value(&repo, id);
    let commit = git_hash(RawKind::Commit, &rendered);
    assert_eq!(exported, ExportedOid::new(commit.0, true));
    assert_eq!(exported.git_oid(), commit);
    // Pinned: the changeset's id, which the hash contract fixes, and the oid git gives these
    // bytes (git 2.43.0's `hash-object -t commit`, which `fsck --strict` accepts).
    let pinned = "038263fb99e3991842afb4918018810ed6345d9306c5d22ac82ea52b285e1290";
    assert_eq!(id.0.to_hex(), pinned);
    assert_eq!(commit, oid("289054922b69c2600fa2e17c8990bfaaa2654b1b"));
    // Stored as version 1 reads it back: the oid's 20 bytes, then `kept`.
    let slot = DerivedSlot::new("git_oid", 1, id.0, &[]);
    let stored = repo.derived_store().get(&slot).unwrap().unwrap();
    assert_eq!(stored, [&commit.0[..], &[1]].concat());
}

#[test]
fn a_merge_exports_its_parents_in_order() {
    let (_dir, repo) = registered();
    let left = commit(&repo, &[], vec![put("l", b"l\n")], "left\n", &[]);
    let right = commit(&repo, &[], vec![put("r", b"r\n")], "right\n", &[]);
    let merge = commit(
        &repo,
        &[right, left],
        vec![put("l", b"l\n")],
        "merge\n",
        &[],
    );
    let (left, right, cs) = (
        value(&repo, left),
        value(&repo, right),
        changeset(&repo, merge),
    );
    let (left, right) = (left.git_oid(), right.git_oid());
    let read = repo.snapshot().unwrap();
    let reader = ExportReader::new(&read, repo.derived_store());
    let tree = tree_oid(&reader, cs.root).unwrap();
    let rendered = render_commit(merge, &cs, &[right, left], tree);
    let head = format!("tree {tree}\nparent {right}\nparent {left}\nauthor ");
    assert!(rendered.starts_with(head.as_bytes()));
    let commit = git_hash(RawKind::Commit, &rendered);
    assert_eq!(value(&repo, merge).git_oid(), commit);
}

#[test]
fn trailers_follow_git_rules() {
    // Each message, and what comes before the trailer block in its export.
    let cases = [
        // No final line break: one, then a blank line.
        ("Add a", "Add a\n\n"),
        ("Add a\n", "Add a\n\n"),
        // A last paragraph of trailers, which is not the first: SwarmOps' join it.
        (
            "Add a\n\nCo-Authored-By: X <x@y>\n",
            "Add a\n\nCo-Authored-By: X <x@y>\n",
        ),
        (
            "Add a\n\nCo-Authored-By: X <x@y>",
            "Add a\n\nCo-Authored-By: X <x@y>\n",
        ),
        (
            "Add a\n\nWhy.\n\nSigned-off-by: A <a@b>\nCo-Authored-By: X <x@y>\n",
            "Add a\n\nWhy.\n\nSigned-off-by: A <a@b>\nCo-Authored-By: X <x@y>\n",
        ),
        // The first paragraph is the title, which is never a trailer block; an empty one ends
        // at the first line.
        ("Fix: thing\n", "Fix: thing\n\n"),
        ("\nKey: v\n", "\nKey: v\n"),
        // A last paragraph that is not trailers alone: a blank line first.
        ("Add a\n\nWhy.\n", "Add a\n\nWhy.\n\n"),
        (
            "Add a\n\nCo-Authored-By: X <x@y>\nand more\n",
            "Add a\n\nCo-Authored-By: X <x@y>\nand more\n\n",
        ),
        ("Add a\n\n-Bad: x\n", "Add a\n\n-Bad: x\n\n"),
        ("Add a\n\nKey:x\n", "Add a\n\nKey:x\n\n"),
        ("Add a\n\nTwo words: x\n", "Add a\n\nTwo words: x\n\n"),
        // A blank last line: no paragraph after it.
        ("Add a\n\n", "Add a\n\n\n"),
        // An empty message: the block alone.
        ("", ""),
    ];
    for (message, before) in cases {
        let (id, cs) = unstored(message.as_bytes(), &[]);
        let expected = format!("{before}SwarmOps-Changeset: {}\n", id.0.to_hex());
        let got = String::from_utf8(export_message(id, &cs)).unwrap();
        assert_eq!(got, expected, "{message:?}");
    }
    // The three keys, in that order, whatever else the metadata holds; a line break in a value
    // continues on the next line after one space, as git writes it.
    assert_eq!(
        TRAILERS,
        [
            ("land_key", "SwarmOps-Land-Key"),
            ("cr", "SwarmOps-CR"),
            ("squashed_from", "SwarmOps-Squashed-From"),
        ]
    );
    let metadata: [(&str, &[u8]); 8] = [
        ("agent", b"agent/worker-17"),
        ("commit_key", b"c-1"),
        ("cr", b"cr-7"),
        ("land_key", b"k\n1"),
        ("lander", b"lander/1"),
        ("lander_run_id", b"r2"),
        ("run_id", b"r1"),
        ("squashed_from", b"aa bb"),
    ];
    let (id, cs) = unstored(b"Land\n", &metadata);
    let expected = format!(
        "Land\n\nSwarmOps-Changeset: {}\nSwarmOps-Land-Key: k\n 1\nSwarmOps-CR: cr-7\n\
         SwarmOps-Squashed-From: aa bb\n",
        id.0.to_hex()
    );
    let got = String::from_utf8(export_message(id, &cs)).unwrap();
    assert_eq!(got, expected);
}

#[test]
fn an_imported_changeset_keeps_its_message() {
    let git_oid: &[u8] = b"0123456789abcdef0123456789abcdef01234567";
    let messages = [
        "Add a",
        "Add a\n",
        "",
        "Fix: thing\n\nCo-Authored-By: X <x@y>\n",
    ];
    for message in messages {
        let metadata: [(&str, &[u8]); 2] = [("git.oid", git_oid), ("land_key", b"k")];
        let (id, cs) = unstored(message.as_bytes(), &metadata);
        assert_eq!(export_message(id, &cs), message.as_bytes(), "{message:?}");
    }
    // Its commit holds its headers and its signatures as the import found them.
    let (_, mut cs) = unstored(b"Add a", &[("git.oid", git_oid)]);
    cs.extra_headers = vec![
        (b"encoding".to_vec(), b"ISO-8859-1".to_vec()),
        (
            b"gpgsig".to_vec(),
            b"-----BEGIN-----\n\nabc\n-----END-----".to_vec(),
        ),
    ];
    cs.author.offset_secs = 19_800;
    let id = cs.id().unwrap();
    let (tree, parents) = (
        GitOid::new([7; 20]),
        [GitOid::new([1; 20]), GitOid::new([2; 20])],
    );
    let headers = &cs.extra_headers;
    let commit = commit_bytes(tree, &parents, &cs.author, &cs.committer, headers, b"Add a");
    assert_eq!(render_commit(id, &cs, &parents, tree), commit);
}

#[test]
fn an_unchanged_import_takes_its_git_oid_without_reading_a_tree() {
    let store = Recorded::new();
    let mut chain: Vec<ChangesetId> = Vec::new();
    for i in 1..=5_u8 {
        let git_oid = format!("{i:040x}");
        let parents: Vec<ChangesetId> = chain.last().copied().into_iter().collect();
        let changes = vec![put(&format!("f{i}"), &[i])];
        let metadata = [("git.oid", git_oid.as_str())];
        chain.push(commit(
            &store.repo,
            &parents,
            changes,
            "imported\n",
            &metadata,
        ));
    }
    store.forget();
    let head = *chain.last().unwrap();
    let expected = ExportedOid::new(oid(&format!("{:040x}", 5)).0, true);
    assert_eq!(value(&store.repo, head), expected);
    // The five changesets, and no tree or blob.
    let read: BTreeSet<String> = store.object_reads().into_iter().collect();
    let changesets: BTreeSet<String> = chain.iter().map(|id| object_path(id.0)).collect();
    assert_eq!(read, changesets);
}

#[test]
fn a_git_oid_not_in_lowercase_hex_is_rendered() {
    let (_dir, repo) = registered();
    // The commit git makes of the file `a`, which the changesets below are.
    let tree = git_tree(&[("100644", "a", git_blob(b"a\n"))]);
    let fields = commit_bytes(tree, &[], &signature(), &signature(), &[], b"a\n");
    let made = git_hash(RawKind::Commit, &fields).to_string();
    // As the import records it: its own commit, without a tree read.
    let faithful = [("git.oid", made.as_str())];
    let faithful = commit(&repo, &[], vec![put("a", b"a\n")], "a\n", &faithful);
    let kept = ExportedOid::new(oid(&made).0, true);
    assert_eq!(value(&repo, faithful), kept);
    // Upper case, 39 digits, or no hex: rendered, to the same commit, which is not its `git.oid`.
    let others = [made.to_uppercase(), made[1..].to_owned(), "g".repeat(40)];
    for other in others {
        let metadata = [("git.oid", other.as_str())];
        let id = commit(&repo, &[], vec![put("a", b"a\n")], "a\n", &metadata);
        let expected = ExportedOid::new(oid(&made).0, false);
        assert_eq!(value(&repo, id), expected, "{other}");
    }
}

#[test]
fn a_lossy_import_and_its_descendants_render_anew() {
    let store = Recorded::new();
    let repo = &store.repo;
    let [root_git, lossy_git, above_git] =
        ['1', '2', '3'].map(|digit| digit.to_string().repeat(40));
    // An import: a faithful root, a lossy child, and a faithful child of that.
    let root = [("git.oid", root_git.as_str())];
    let root = commit(repo, &[], vec![put("a", b"1\n")], "one\n", &root);
    let lossy = [("git.lossy", "gitlinks"), ("git.oid", lossy_git.as_str())];
    let lossy = commit(repo, &[root], vec![put("a", b"2\n")], "two\n", &lossy);
    let above = [("git.oid", above_git.as_str())];
    let above = commit(repo, &[lossy], vec![put("a", b"3\n")], "three\n", &above);
    let [root_tree, lossy_tree, above_tree] =
        [root, lossy, above].map(|id| object_path(changeset(repo, id).root.0));
    store.forget();
    let (lossy_oid, above_oid) = (value(repo, lossy), value(repo, above));
    // Both rendered, their trees read: new commits, not their `git.oid`. The root is not.
    let read: BTreeSet<String> = store.object_reads().into_iter().collect();
    for (exported, recorded, tree) in [
        (lossy_oid, &lossy_git, lossy_tree),
        (above_oid, &above_git, above_tree),
    ] {
        assert!(!exported.kept, "{recorded}");
        assert_ne!(exported.git_oid(), oid(recorded));
        assert!(read.contains(&tree), "{tree} is not read");
    }
    assert!(!read.contains(&root_tree), "the root's tree is read");
    // The lossy one is its fields over its root and its parent's `git.oid`, its message as is.
    let tree = git_tree(&[("100644", "a", git_blob(b"2\n"))]);
    let parent = [oid(&root_git)];
    let fields = commit_bytes(tree, &parent, &signature(), &signature(), &[], b"two\n");
    assert_eq!(lossy_oid.git_oid(), git_hash(RawKind::Commit, &fields));
    rendered_on_a_native_parent(&store, above, above_oid);
}

/// The rest of [`a_lossy_import_and_its_descendants_render_anew`], on `above` (its export oid
/// `above_oid`), an import rendered anew: a native child of it is rendered, and kept; a commit
/// made in git on that one's export, imported, is rendered (its parent is native), its tree read
/// once, and kept, its `git.oid` its value; the same commit under another `git.oid` exports to
/// that rendering, and is not kept.
fn rendered_on_a_native_parent(store: &Recorded, above: ChangesetId, above_oid: ExportedOid) {
    let repo = &store.repo;
    let native = commit(repo, &[above], vec![put("a", b"4\n")], "four\n", &[]);
    let native_oid = value(repo, native);
    assert!(native_oid.kept);
    // Rendered on its parent's new commit.
    let cs = changeset(repo, native);
    let tree = git_tree(&[("100644", "a", git_blob(b"4\n"))]);
    let rendered = render_commit(native, &cs, &[above_oid.git_oid()], tree);
    assert_eq!(native_oid.git_oid(), git_hash(RawKind::Commit, &rendered));
    // What git makes of a commit of `b` on the native one's export.
    let tree = git_tree(&[
        ("100644", "a", git_blob(b"4\n")),
        ("100644", "b", git_blob(b"5\n")),
    ]);
    let parent = [native_oid.git_oid()];
    let fields = commit_bytes(tree, &parent, &signature(), &signature(), &[], b"five\n");
    let made = git_hash(RawKind::Commit, &fields);
    let hex = made.to_string();
    let built = [("git.oid", hex.as_str())];
    let built = commit(repo, &[native], vec![put("b", b"5\n")], "five\n", &built);
    let root = object_path(changeset(repo, built).root.0);
    store.forget();
    let expected = ExportedOid::new(made.0, true);
    assert_eq!(value(repo, built), expected);
    let reads = store.object_reads();
    assert_eq!(
        reads.iter().filter(|path| **path == root).count(),
        1,
        "{reads:#?}"
    );
    // The export oid is derived by recomputing its root tree from the store's own bytes, never a
    // memo a store file could have placed: `a`'s blob is read for its git oid (M8 security fix).
    assert!(reads.contains(&object_path(blob(b"4\n").0)), "{reads:#?}");
    // The same commit under another `git.oid`: exported to that rendering, and not kept.
    let six = "6".repeat(40);
    let other = [("git.oid", six.as_str())];
    let other = commit(repo, &[native], vec![put("b", b"5\n")], "five\n", &other);
    let expected = ExportedOid::new(made.0, false);
    assert_eq!(value(repo, other), expected);
}

#[test]
fn tree_and_blob_oids_are_memoized_per_object_id() {
    let store = Recorded::new();
    let repo = &store.repo;
    let files = vec![put("a", b"a\n"), put("d/b", b"b\n")];
    let first = commit(repo, &[], files, "first\n", &[]);
    let second = commit(repo, &[first], vec![put("c", b"c\n")], "second\n", &[]);
    let (one, two) = (changeset(repo, first).root, changeset(repo, second).root);
    let dir = Tree::new(vec![TreeEntry::leaf("b", LeafKind::File, blob(b"b\n"))]).unwrap();
    let read = repo.snapshot().unwrap();
    let reader = ExportReader::new(&read, repo.derived_store());
    store.forget();
    let dir_oid = git_tree(&[("100644", "b", git_blob(b"b\n"))]);
    let expected = git_tree(&[("100644", "a", git_blob(b"a\n")), ("40000", "d", dir_oid)]);
    assert_eq!(tree_oid(&reader, one).unwrap(), expected);
    // Each tree's oid and each blob's is a file of the derived store, its object's id the slot's.
    let calls = (store.calls)();
    let memos = [
        ("git_tree_oid", one.0),
        ("git_tree_oid", dir.id().0),
        ("git_blob_oid", blob(b"a\n").0),
        ("git_blob_oid", blob(b"b\n").0),
    ];
    for (name, subject) in memos {
        let link = format!("link_noclobber {}", derived_path(name, subject));
        assert!(calls.contains(&link), "no {link} in {calls:#?}");
    }
    let slot = DerivedSlot::new("git_tree_oid", 1, one.0, &[]);
    assert_eq!(
        repo.derived_store().get(&slot).unwrap(),
        Some(expected.0.to_vec())
    );
    // Asked again, it reads no object.
    store.forget();
    assert_eq!(tree_oid(&reader, one).unwrap(), expected);
    assert_eq!(store.object_reads(), Vec::<String>::new());
    // The second root shares `a` and `d` with the first: only it and `c` are read.
    let expected = git_tree(&[
        ("100644", "a", git_blob(b"a\n")),
        ("100644", "c", git_blob(b"c\n")),
        ("40000", "d", dir_oid),
    ]);
    assert_eq!(tree_oid(&reader, two).unwrap(), expected);
    let read: BTreeSet<String> = store.object_reads().into_iter().collect();
    let wanted = BTreeSet::from([object_path(two.0), object_path(blob(b"c\n").0)]);
    assert_eq!(read, wanted);
}

#[test]
fn exec_and_symlink_entries_render_as_git_does() {
    let (_dir, repo) = registered();
    let run = put_as("run", LeafKind::Exec, b"#!/bin/sh\n");
    let run = changeset(&repo, commit(&repo, &[], vec![run], "run\n", &[])).root;
    let link = put_as("link", LeafKind::Symlink, b"a");
    let link = changeset(&repo, commit(&repo, &[], vec![link], "link\n", &[])).root;
    let read = repo.snapshot().unwrap();
    let reader = ExportReader::new(&read, repo.derived_store());
    // git's oid of a tree of one executable file, `100755 run`.
    let run = tree_oid(&reader, run).unwrap();
    assert_eq!(run, oid("6f2c44ec570e49318e3d2f293795575fcf8c1f01"));
    // A symlink is `120000` over the blob of its target: git's oid of `link` -> `a`.
    let link = tree_oid(&reader, link).unwrap();
    assert_eq!(link, oid("a0b83842cd6ce649cd0dadcbf82a810f054668d2"));
}

#[test]
fn a_deep_tree_renders_without_recursion() {
    const DEPTH: usize = 10_000;
    let (_dir, tmp) = temp_dir();
    let root = tmp.join("store");
    // A filesystem that never syncs, for the 10,002 objects.
    let repo = LocalRepo::create_with_fs(&root, "test", Arc::new(NoSyncFs)).unwrap();
    let content = b"deep\n";
    let mut objects = vec![(ObjectKind::Blob, content.to_vec())];
    let file = TreeEntry::leaf("file.txt", LeafKind::File, blob(content));
    let mut tree = Tree::new(vec![file]).unwrap();
    let mut expected = git_tree(&[("100644", "file.txt", git_blob(content))]);
    for _ in 0..DEPTH {
        let below = tree.id();
        objects.push((ObjectKind::Tree, tree.encode()));
        tree = Tree::new(vec![TreeEntry::dir("d", below)]).unwrap();
        expected = git_tree(&[("40000", "d", expected)]);
    }
    let top = tree.id();
    objects.push((ObjectKind::Tree, tree.encode()));
    repo.put_objects(objects).unwrap();
    // A stack of 256 KiB, which a walk that recursed once a directory would overflow.
    let got = thread::scope(|scope| {
        thread::Builder::new()
            .name("a deep tree".to_owned())
            .stack_size(256 * 1024)
            .spawn_scoped(scope, || {
                let read = repo.snapshot().unwrap();
                let reader = ExportReader::new(&read, repo.derived_store());
                tree_oid(&reader, top)
            })
            .unwrap()
            .join()
            .unwrap()
    });
    assert_eq!(got.unwrap(), expected);
}

#[test]
fn the_export_oid_ignores_refs_and_the_clock() {
    let store = Recorded::new();
    let repo = &store.repo;
    let first = commit(repo, &[], vec![put("a", b"a\n")], "first\n", &[]);
    let land_key = [("land_key", "k-1")];
    let second = commit(
        repo,
        &[first],
        vec![put("b", b"b\n")],
        "second\n",
        &land_key,
    );
    let derived = value(repo, second);
    // `main` moved, and commits made on the changeset and beside it.
    repo.cas_ref("main", None, Some(first), "test").unwrap();
    let after = commit(repo, &[second], vec![put("c", b"c\n")], "after\n", &[]);
    commit(repo, &[], vec![put("x", b"x\n")], "beside\n", &[]);
    repo.cas_ref("main", Some(first), Some(after), "test")
        .unwrap();
    // The same value, read back from its one file: derived once.
    store.forget();
    assert_eq!(value(repo, second), derived);
    let file = format!("read {}", derived_path("git_oid", second.0));
    assert_eq!((store.calls)(), [file]);
    // Another store, which made the other changeset first and moves no ref: the same value.
    let (_dir, twin) = registered();
    commit(&twin, &[], vec![put("x", b"x\n")], "beside\n", &[]);
    let first_again = commit(&twin, &[], vec![put("a", b"a\n")], "first\n", &[]);
    let changes = vec![put("b", b"b\n")];
    let again = commit(&twin, &[first_again], changes, "second\n", &land_key);
    assert_eq!(again, second);
    assert_eq!(value(&twin, again), derived);
}

#[test]
fn export_oids_are_derived_once_registered() {
    let (_dir, _, mut repo) = new_repo();
    let id = commit(&repo, &[], vec![put("a", b"a\n")], "a\n", &[]);
    let Err(RepoError::Derived { name, .. }) = repo.derived::<ExportOid>(id, &()) else {
        panic!("an export oid before ExportOid is registered");
    };
    assert_eq!(name, "git_oid");
    register(&mut repo).unwrap();
    assert!(value(&repo, id).kept);
    // Once per handle.
    let Err(RepoError::Derived { name, .. }) = register(&mut repo) else {
        panic!("ExportOid registered twice");
    };
    assert_eq!(name, "git_oid");
}
