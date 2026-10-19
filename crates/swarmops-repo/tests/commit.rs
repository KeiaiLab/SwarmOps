//! Commits through `Repo::commit`: change lists turned into blobs, rebuilt trees and a
//! changeset whose delta lists what changed, the checks a request must pass before anything
//! is written, and the durability order (every object durable before the changeset's journal
//! record), after a crash at any filesystem call and with writers in several processes.
//!
//! Every test owns a fresh store (`tempfile::tempdir()`), so tests are independent and may
//! run in any order or in parallel. The crash sweep and the multi-process test run this test
//! binary again as their children (selected by [`CHILD_ROOT`]), on the real filesystem.

use std::fs;
#[cfg(feature = "test-hooks")]
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
#[cfg(feature = "test-hooks")]
use std::process::Output;
use std::sync::Arc;
#[cfg(feature = "test-hooks")]
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_core::{
    Changeset, ChangesetId, CodecError, DeltaEntry, DeltaOp, EntryKind, ObjectId, ObjectKind, Tree,
    TreeEntry, hash_object,
};
use swarmops_repo::{
    CasResult, Change, ChangeOp, CommitOutcome, CommitRequest, InsertOutcome, LocalRepo, RepoError,
};
#[cfg(feature = "test-hooks")]
use swarmops_store::{CRASH_AT_ENV, FailAt, FailAtFs, FailMode, InjectedError, ORDER_CHECK_ENV};
use swarmops_store::{
    ChangesetMeta, Journal, Layout, LimitKind, MetaStore, RealFs, Recovered, StoreError,
};
use swarmops_testkit::child::{CHILD_TIMEOUT, Child, this_test};
#[cfg(feature = "test-hooks")]
use swarmops_testkit::child::{SIGABRT, run};
use swarmops_testkit::faults::checked;
use swarmops_testkit::fs::FailingFs;
#[cfg(feature = "test-hooks")]
use swarmops_testkit::fs::{RecordingFs, copy_dir};
use swarmops_testkit::repo::{blob, delete, new_repo, path, put, put_as, signature};
use swarmops_testkit::test_name;

/// Set only in the children this file's tests spawn: the store root.
const CHILD_ROOT: &str = "SWARMOPS_TEST_REPO_COMMIT_CHILD_ROOT";

/// Set with [`CHILD_ROOT`] in the children of the multi-process test: the child's number.
const CHILD_NUMBER: &str = "SWARMOPS_TEST_REPO_COMMIT_CHILD_NUMBER";

/// How the multi-process test's children report to their parent, among the harness's output.
const REPORT: &str = "repo-child: ";

/// How many children the multi-process test spawns, and how many commits each makes.
const CHILDREN: u64 = 8;
const PER_CHILD: u64 = 4;

/// A request for `changes` on `parents` with the message "a test commit", recording the agent
/// in its metadata.
fn request(parents: &[ChangesetId], changes: Vec<Change>) -> CommitRequest {
    CommitRequest {
        metadata: vec![("agent".to_owned(), b"test/agent".to_vec())],
        ..swarmops_testkit::repo::request(parents, changes, "a test commit")
    }
}

#[track_caller]
fn commit(repo: &LocalRepo, parents: &[ChangesetId], changes: Vec<Change>) -> CommitOutcome {
    repo.commit(request(parents, changes)).expect("commit")
}

fn file_put(content: &[u8]) -> DeltaOp {
    DeltaOp::Put {
        kind: EntryKind::File,
        id: blob(content),
    }
}

/// The delta of changeset `id`, as (path, op) pairs; no entry carries a rename.
fn delta(repo: &LocalRepo, id: ChangesetId) -> Vec<(String, DeltaOp)> {
    let stored = repo
        .changeset(id)
        .unwrap()
        .expect("the changeset is stored");
    stored
        .changeset
        .delta
        .into_iter()
        .map(|entry| {
            assert_eq!(entry.renamed_from, None);
            (entry.path.to_string(), entry.op)
        })
        .collect()
}

/// The names and kinds of directory `dir`'s entries at changeset `id`.
fn listing(
    repo: &LocalRepo,
    id: ChangesetId,
    dir: Option<&str>,
) -> Option<Vec<(String, EntryKind)>> {
    let dir = dir.map(path);
    repo.list_tree(id, dir.as_ref())
        .unwrap()
        .map(|entries| entries.into_iter().map(|e| (e.name, e.kind)).collect())
}

fn names(entries: &[(&str, EntryKind)]) -> Vec<(String, EntryKind)> {
    entries
        .iter()
        .map(|&(name, kind)| (name.to_owned(), kind))
        .collect()
}

/// The content of file `p` at changeset `id`, if it is a file there.
fn content(repo: &LocalRepo, id: ChangesetId, p: &str) -> Option<Vec<u8>> {
    repo.read_blob_at(id, &path(p))
        .unwrap()
        .map(|(_, _, bytes)| bytes)
}

fn entry(name: &str, kind: EntryKind, id: ObjectId) -> TreeEntry {
    TreeEntry {
        name: name.to_owned(),
        kind,
        id,
    }
}

fn tree(entries: Vec<TreeEntry>) -> Tree {
    Tree::new(entries).expect("a valid tree")
}

/// Every object file of the store at `root`: the files in its fan-out directories.
fn object_files(root: &Utf8Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for fanout in fs::read_dir(root.join("objects")).unwrap() {
        let fanout = fanout.unwrap();
        if fanout.file_name() == "tmp" {
            continue;
        }
        for file in fs::read_dir(fanout.path()).unwrap() {
            files.push(file.unwrap().path());
        }
    }
    files.sort();
    files
}

/// How many frames the journal of the store at `root` lists.
fn journal_frames(root: &Utf8Path) -> u64 {
    Journal::open(&Layout::new(root), Arc::new(RealFs))
        .unwrap()
        .frame_count()
        .unwrap()
}

#[track_caller]
fn assert_type_conflict(got: Result<CommitOutcome, RepoError>, want: &str) {
    match got {
        Err(RepoError::TypeConflict { path: conflict }) => assert_eq!(conflict, path(want)),
        other => panic!("expected TypeConflict at {want}, got {other:?}"),
    }
}

#[test]
fn root_commit_from_change_list_builds_trees_and_delta() {
    let (_dir, _root, repo) = new_repo();
    // Out of order on purpose: the delta comes out sorted by path.
    let out = commit(
        &repo,
        &[],
        vec![
            put("src/x/y.txt", b"y\n"),
            put("a.rs", b"fn a() {}\n"),
            put("src/lib.rs", b"pub mod x;\n"),
        ],
    );
    assert_eq!((out.blobs_written, out.trees_written), (3, 3));
    assert_eq!(out.files_changed, 3);
    assert_eq!(out.insert, InsertOutcome::Inserted { seq: 0 });
    assert_eq!(out.warnings, Vec::<String>::new());

    // The trees as their canonical objects, bottom-up: the root holds a.rs and src.
    let x = tree(vec![entry("y.txt", EntryKind::File, blob(b"y\n").0)]);
    let src = tree(vec![
        entry("lib.rs", EntryKind::File, blob(b"pub mod x;\n").0),
        entry("x", EntryKind::Dir, x.id().0),
    ]);
    let root = tree(vec![
        entry("a.rs", EntryKind::File, blob(b"fn a() {}\n").0),
        entry("src", EntryKind::Dir, src.id().0),
    ]);
    assert_eq!(out.root, root.id());
    assert_eq!(
        listing(&repo, out.id, None),
        Some(names(&[("a.rs", EntryKind::File), ("src", EntryKind::Dir)]))
    );

    let stored = repo.changeset(out.id).unwrap().unwrap();
    assert_eq!((stored.id, stored.seq, stored.generation), (out.id, 0, 1));
    let cs = &stored.changeset;
    assert_eq!(cs.id().unwrap(), out.id, "the id is the hash of the bytes");
    assert_eq!(cs.root, out.root);
    assert_eq!(cs.parents, []);
    assert_eq!(
        delta(&repo, out.id),
        [
            ("a.rs".to_owned(), file_put(b"fn a() {}\n")),
            ("src/lib.rs".to_owned(), file_put(b"pub mod x;\n")),
            ("src/x/y.txt".to_owned(), file_put(b"y\n")),
        ]
    );
    assert_eq!((&cs.author, &cs.committer), (&signature(), &signature()));
    assert_eq!(cs.message, b"a test commit");
    assert_eq!(cs.extra_headers, []);
    assert_eq!(cs.metadata, [("agent".to_owned(), b"test/agent".to_vec())]);
}

#[test]
fn second_commit_modifies_and_deletes() {
    let (_dir, _root, repo) = new_repo();
    let first = commit(
        &repo,
        &[],
        vec![
            put("a.rs", b"a"),
            put("src/lib.rs", b"v1"),
            put("src/x/y.txt", b"y"),
        ],
    );
    let second = commit(
        &repo,
        &[first.id],
        vec![put("src/lib.rs", b"v2"), delete("src/x/y.txt")],
    );
    // One new blob; src and the root rewritten; src/x is gone, not rewritten.
    assert_eq!((second.blobs_written, second.trees_written), (1, 2));
    assert_eq!(
        delta(&repo, second.id),
        [
            ("src/lib.rs".to_owned(), file_put(b"v2")),
            ("src/x/y.txt".to_owned(), DeltaOp::Delete),
        ]
    );
    assert_eq!(
        listing(&repo, second.id, Some("src")),
        Some(names(&[("lib.rs", EntryKind::File)]))
    );
    assert_eq!(listing(&repo, second.id, Some("src/x")), None);
    assert_eq!(
        content(&repo, second.id, "src/lib.rs"),
        Some(b"v2".to_vec())
    );
    assert_eq!(content(&repo, second.id, "src/x/y.txt"), None);
    assert_eq!(
        content(&repo, second.id, "a.rs"),
        Some(b"a".to_vec()),
        "an untouched file carries over"
    );
    // The first changeset still reads as it was.
    assert_eq!(content(&repo, first.id, "src/lib.rs"), Some(b"v1".to_vec()));
    assert_eq!(content(&repo, first.id, "src/x/y.txt"), Some(b"y".to_vec()));

    let stored = repo.changeset(second.id).unwrap().unwrap();
    assert_eq!((stored.seq, stored.generation), (1, 2));
    assert_eq!(stored.changeset.parents, [first.id]);
}

#[test]
fn delete_prunes_empty_directories() {
    let (_dir, _root, repo) = new_repo();
    let first = commit(
        &repo,
        &[],
        vec![
            put("a/b/c/d.txt", b"deep"),
            put("a/keep.txt", b"keep"),
            put("top.txt", b"top"),
        ],
    );
    // Deleting the only file of a/b/c prunes c, then b; a keeps keep.txt.
    let second = commit(&repo, &[first.id], vec![delete("a/b/c/d.txt")]);
    assert_eq!(second.trees_written, 2, "a and the root");
    assert_eq!(
        listing(&repo, second.id, Some("a")),
        Some(names(&[("keep.txt", EntryKind::File)]))
    );
    for gone in ["a/b", "a/b/c"] {
        assert_eq!(listing(&repo, second.id, Some(gone)), None, "{gone}");
    }

    // Deleting the rest leaves the empty root, the one tree that may be empty.
    let third = commit(
        &repo,
        &[second.id],
        vec![delete("a/keep.txt"), delete("top.txt")],
    );
    assert_eq!(third.root, Tree::empty_root().id());
    assert_eq!(third.trees_written, 1, "the empty root");
    assert_eq!(listing(&repo, third.id, None), Some(Vec::new()));
    assert_eq!(listing(&repo, third.id, Some("a")), None);

    // A commit on the empty root builds from nothing.
    let fourth = commit(&repo, &[third.id], vec![put("a/new.txt", b"new")]);
    assert_eq!(
        listing(&repo, fourth.id, None),
        Some(names(&[("a", EntryKind::Dir)]))
    );
    assert_eq!(
        content(&repo, fourth.id, "a/new.txt"),
        Some(b"new".to_vec())
    );
}

#[test]
fn commit_rejects_file_dir_type_conflict() {
    let (_dir, root, repo) = new_repo();
    let base = commit(
        &repo,
        &[],
        vec![put("a.rs", b"a"), put("src/lib.rs", b"lib")],
    );
    let objects = object_files(&root);

    assert_type_conflict(
        repo.commit(request(&[base.id], vec![put("a.rs/b", b"under a file")])),
        "a.rs/b",
    );
    assert_type_conflict(
        repo.commit(request(&[base.id], vec![put("src", b"over a directory")])),
        "src",
    );
    assert_type_conflict(repo.commit(request(&[base.id], vec![delete("src")])), "src");
    // One request may not make a path both a file and a directory, in either order.
    for changes in [
        vec![put("new", b"file"), put("new/inner", b"file")],
        vec![put("new/inner", b"file"), put("new", b"file")],
    ] {
        assert_type_conflict(repo.commit(request(&[base.id], changes)), "new/inner");
    }

    assert_eq!(object_files(&root), objects, "nothing was written");
    assert_eq!(repo.info().unwrap().changesets, 1);
}

#[test]
fn commit_rejects_dir_put_and_empty_changes() {
    let (_dir, root, repo) = new_repo();
    match repo.commit(request(
        &[],
        vec![put("a", b"a"), put_as("d", EntryKind::Dir, b"")],
    )) {
        Err(RepoError::DirPut { path: dir }) => assert_eq!(dir, path("d")),
        other => panic!("expected DirPut, got {other:?}"),
    }
    let empty = repo.commit(request(&[], Vec::new()));
    assert!(matches!(empty, Err(RepoError::EmptyChanges)), "{empty:?}");

    // A change list that changes nothing is empty too: a put of what the path holds already.
    let base = commit(&repo, &[], vec![put("a", b"a")]);
    let objects = object_files(&root);
    let same = repo.commit(request(&[base.id], vec![put("a", b"a")]));
    assert!(matches!(same, Err(RepoError::EmptyChanges)), "{same:?}");
    assert_eq!(object_files(&root), objects, "nothing was written");
    assert_eq!(repo.info().unwrap().changesets, 1);
}

#[test]
fn commit_rejects_a_changeset_it_cannot_encode() {
    let (_dir, root, repo) = new_repo();
    let objects = object_files(&root);
    let mut req = request(&[], vec![put("a", b"a")]);
    req.metadata = vec![
        ("run_id".to_owned(), b"r1".to_vec()),
        ("agent".to_owned(), b"test/agent".to_vec()),
    ];
    match repo.commit(req) {
        Err(RepoError::Codec(CodecError::UnsortedMetadata)) => {}
        other => panic!("expected UnsortedMetadata, got {other:?}"),
    }
    assert_eq!(object_files(&root), objects, "nothing was written");
    assert_eq!(repo.info().unwrap().changesets, 0);
}

/// The repository refuses a metadata key that ends in `:b64` itself, before anything is written,
/// as the operations do: `show` gives a value that is not UTF-8 in base64 under its key with that
/// suffix appended, so a writer below the API, such as the git import, must not store such a key
/// either. `check_metadata` is the rule every writer runs; it names the first key that breaks it.
#[test]
fn commit_refuses_a_reserved_metadata_key_too() {
    use swarmops_repo::{RESERVED_METADATA_KEY_SUFFIX, check_metadata};

    let entry = |key: &str| (key.to_owned(), b"v".to_vec());
    assert_eq!(RESERVED_METADATA_KEY_SUFFIX, ":b64");
    let refused = |keys: &[&str]| {
        let metadata: Vec<(String, Vec<u8>)> = keys.iter().map(|&key| entry(key)).collect();
        match check_metadata(&metadata) {
            Ok(()) => None,
            Err(RepoError::ReservedMetadataKey { key }) => Some(key),
            Err(other) => panic!("{keys:?}: {other:?}"),
        }
    };
    assert_eq!(refused(&[]), None);
    // The suffix counts at the end of a key only.
    assert_eq!(
        refused(&["agent", "b64", "x:b6", "x:b64:y", "x:b64y"]),
        None
    );
    assert_eq!(refused(&[":b64"]).as_deref(), Some(":b64"));
    assert_eq!(
        refused(&["agent", "note:b64", "z:b64"]).as_deref(),
        Some("note:b64")
    );

    let (_dir, root, repo) = new_repo();
    let objects = object_files(&root);
    let mut req = request(&[], vec![put("a", b"a")]);
    req.metadata.push(entry("x:b64"));
    let refusal = repo.commit(req.clone()).expect_err("a reserved key");
    assert!(
        matches!(&refusal, RepoError::ReservedMetadataKey { key } if key == "x:b64"),
        "{refusal:?}"
    );
    // The operations give this message with E_INVALID_ARG.
    assert_eq!(
        refusal.to_string(),
        r#"metadata key "x:b64" ends in ":b64", which show reserves"#
    );
    assert_eq!(object_files(&root), objects, "nothing was written");
    assert_eq!(journal_frames(&root), 0, "no transaction");
    assert_eq!(repo.info().unwrap().changesets, 0);

    // A key that holds the suffix without ending in it is a key like any other.
    req.metadata[1] = entry("x:b64:y");
    let out = repo.commit(req).unwrap();
    assert_eq!(out.insert, InsertOutcome::Inserted { seq: 0 });
}

#[test]
fn allow_empty_commits_a_changeset_with_the_parent_root() {
    let (_dir, _root, repo) = new_repo();
    let base = commit(&repo, &[], vec![put("a", b"a")]);
    let empty = |parents: &[ChangesetId], changes| CommitRequest {
        allow_empty: true,
        message: b"nothing changes".to_vec(),
        ..request(parents, changes)
    };

    // No changes: the changeset keeps its parent's root, with an empty delta.
    let out = repo.commit(empty(&[base.id], Vec::new())).unwrap();
    assert_ne!(out.id, base.id);
    assert_eq!(out.root, base.root);
    assert_eq!((out.blobs_written, out.trees_written), (0, 0));
    assert_eq!(out.files_changed, 0);
    assert_eq!(out.insert, InsertOutcome::Inserted { seq: 1 });
    let stored = repo.changeset(out.id).unwrap().unwrap();
    assert_eq!(stored.changeset.parents, [base.id]);
    assert_eq!(stored.changeset.delta, []);
    assert_eq!(stored.changeset.message, b"nothing changes");
    assert_eq!(stored.generation, 2);
    assert_eq!(content(&repo, out.id, "a"), Some(b"a".to_vec()));

    // Changes that change nothing count as none.
    let same = repo.commit(empty(&[out.id], vec![put("a", b"a")])).unwrap();
    assert_eq!(same.root, base.root);
    assert_eq!(delta(&repo, same.id), []);

    // A root changeset of nothing has the empty tree, written so that it reads.
    let first = repo.commit(empty(&[], Vec::new())).unwrap();
    assert_eq!(first.root, Tree::empty_root().id());
    assert_eq!(first.trees_written, 1, "the empty tree was not stored yet");
    assert_eq!(listing(&repo, first.id, None), Some(Vec::new()));
    assert_eq!(repo.verify(true).unwrap().problems, []);
}

#[test]
fn commit_refuses_more_than_255_parents_before_writing() {
    let (_dir, root, repo) = new_repo();
    let base = commit(&repo, &[], vec![put("a", b"a")]);
    // 255 more changesets for parents, recorded straight in the metadata (no objects): a
    // journal record holds at most 255 parents, so 256 cannot be recorded.
    let others: Vec<ChangesetId> = (0..255_u32)
        .map(|n| ChangesetId(hash_object(ObjectKind::Changeset, &n.to_le_bytes())))
        .collect();
    let meta = MetaStore::open(&root).unwrap();
    let mut write = meta.write().unwrap();
    for (n, &id) in others.iter().enumerate() {
        let record = ChangesetMeta {
            id,
            seq: 0,
            generation: 0,
            delta_hash: [u8::try_from(n).unwrap(); 32],
            parents: Vec::new(),
        };
        write.insert_changeset(record).unwrap();
    }
    write.commit().unwrap();
    let objects = object_files(&root);

    let mut parents = vec![base.id];
    parents.extend(others);
    match repo.commit(request(&parents, vec![put("b", b"b")])) {
        Err(RepoError::Store(StoreError::Limit { what, max, got })) => {
            assert_eq!((what, max, got), (LimitKind::Parents, 255, 256));
        }
        other => panic!("expected Limit, got {other:?}"),
    }
    assert_eq!(object_files(&root), objects, "nothing was written");
}

#[test]
fn commit_root_matches_a_root_commit_of_the_same_files() {
    // However a tree got to its files, its root is the root of those files: a commit on a
    // parent gives the root a root commit of its final files gives.
    let (_dir, _root, repo) = new_repo();
    let base = commit(
        &repo,
        &[],
        vec![
            put("a.rs", b"a"),
            put("src/lib.rs", b"v1"),
            put("src/x/y.txt", b"y"),
            put("old/gone.txt", b"g"),
        ],
    );
    let child = commit(
        &repo,
        &[base.id],
        vec![
            put("src/lib.rs", b"v2"),
            delete("src/x/y.txt"),
            delete("old/gone.txt"),
            put("src/x/z.txt", b"z"),
            put_as("bin/run", EntryKind::Exec, b"#!/bin/sh\n"),
        ],
    );
    let (_other_dir, _other_root, fresh) = new_repo();
    let direct = commit(
        &fresh,
        &[],
        vec![
            put("a.rs", b"a"),
            put("src/lib.rs", b"v2"),
            put("src/x/z.txt", b"z"),
            put_as("bin/run", EntryKind::Exec, b"#!/bin/sh\n"),
        ],
    );
    assert_eq!(child.root, direct.root);
}

#[test]
fn commit_rejects_unknown_parent() {
    let (_dir, root, repo) = new_repo();
    let base = commit(&repo, &[], vec![put("a", b"a")]);
    let objects = object_files(&root);
    let unknown = ChangesetId(hash_object(ObjectKind::Changeset, b"never committed"));
    for parents in [vec![unknown], vec![base.id, unknown]] {
        match repo.commit(request(&parents, vec![put("b", b"b")])) {
            Err(RepoError::UnknownParent(parent)) => assert_eq!(parent, unknown),
            other => panic!("expected UnknownParent, got {other:?}"),
        }
    }
    // A parent listed twice is refused before anything is written, too.
    match repo.commit(request(&[base.id, base.id], vec![put("b", b"b")])) {
        Err(RepoError::Store(StoreError::DuplicateParent(parent))) => assert_eq!(parent, base.id),
        other => panic!("expected DuplicateParent, got {other:?}"),
    }
    assert_eq!(object_files(&root), objects, "nothing was written");
    assert_eq!(repo.info().unwrap().changesets, 1);
}

/// The store root of this process, if it is a child one of this file's tests spawned.
fn child_root() -> Option<Utf8PathBuf> {
    std::env::var(CHILD_ROOT).ok().map(Utf8PathBuf::from)
}

/// Runs test `test` again in a child process on the store at `root`, the child's k-th
/// filesystem call aborting it (`SWARMOPS_TEST_CRASH_AT=k`) and every call before it checked
/// against the store's write-ordering rules (`SWARMOPS_TEST_ORDER_CHECK=1`).
#[cfg(feature = "test-hooks")]
fn crash_child(test: &str, root: &Utf8Path, k: u64) -> Output {
    run(this_test(&test_name!(test))
        .env(CHILD_ROOT, root)
        .env(CRASH_AT_ENV, k.to_string())
        .env(ORDER_CHECK_ENV, "1"))
}

/// The commit the crash sweep's child makes: a root commit of three files in nested
/// directories, so that it writes blobs, trees and its changeset.
#[cfg(feature = "test-hooks")]
fn crash_request() -> CommitRequest {
    request(
        &[],
        vec![
            put("a.rs", b"a"),
            put("src/lib.rs", b"lib"),
            put("src/x/y.txt", b"y"),
        ],
    )
}

/// How many runs of the crash sweep go at once: each has a child process and a store of its
/// own.
#[cfg(feature = "test-hooks")]
const AT_ONCE: u64 = 8;

/// Runs `run(k)` for k = 1, 2, … [`AT_ONCE`] at a time, until a run reports that its child
/// finished; returns how many filesystem calls the child makes (the last k it crashed at).
/// Every run past that one must finish too.
#[cfg(feature = "test-hooks")]
fn sweep(run: impl Fn(u64) -> bool + Sync) -> u64 {
    let run = &run;
    let mut first = 1;
    loop {
        let finished: Vec<bool> = thread::scope(|scope| {
            let runs: Vec<_> = (first..first + AT_ONCE)
                .map(|k| scope.spawn(move || run(k)))
                .collect();
            runs.into_iter().map(|run| run.join().unwrap()).collect()
        });
        if let Some(at) = finished.iter().position(|&done| done) {
            assert!(
                finished[at..].iter().all(|&done| done),
                "a crash after the child's last call: {finished:?} from k={first}"
            );
            return first + u64::try_from(at).unwrap() - 1;
        }
        first += AT_ONCE;
        assert!(first < 1_000, "the child never finished");
    }
}

/// One run of the commit crash sweep: a child commits [`crash_request`] on a copy of
/// `template` until its k-th filesystem call aborts it. The store must then hold nothing or
/// the whole changeset `want`, verify clean, and take the commit again, which recovers from the
/// crash under the order checker ([`checked`]), as the child ran. Returns whether the child
/// finished (k was past its last call), and whether the changeset was committed.
#[cfg(feature = "test-hooks")]
fn commit_crashing_at(template: &Utf8Path, want: ChangesetId, k: u64) -> (bool, bool) {
    let dir = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
    // A fresh store from the template, faster than creating one (which syncs a dozen times).
    copy_dir(template.as_std_path(), root.as_std_path());
    let child = crash_child(
        "commit_never_inserts_record_before_objects_are_durable",
        &root,
        k,
    );
    let finished = child.status.success();
    assert!(
        finished || child.status.signal() == Some(SIGABRT),
        "k={k}: {:?}\n{}",
        child.status,
        String::from_utf8_lossy(&child.stderr)
    );

    // Either nothing is committed, or all of it is, every object there.
    let repo = LocalRepo::open_with_fs(&root, checked(RealFs)).unwrap();
    let committed = match repo.info().unwrap().changesets {
        0 => false,
        1 => true,
        n => panic!("k={k}: {n} changesets"),
    };
    assert!(
        committed || !finished,
        "k={k}: the commit returned but is not stored"
    );
    if committed {
        assert!(repo.changeset(want).unwrap().is_some(), "k={k}");
        for (file, bytes) in [
            ("a.rs", b"a".as_slice()),
            ("src/lib.rs", b"lib"),
            ("src/x/y.txt", b"y"),
        ] {
            assert_eq!(
                content(&repo, want, file).as_deref(),
                Some(bytes),
                "k={k}: {file}"
            );
        }
    }
    assert_eq!(repo.verify(true).unwrap().problems, [], "k={k}");

    // The next commit takes the store as the crash left it.
    let again = repo.commit(crash_request()).unwrap();
    assert_eq!(again.id, want, "k={k}");
    let expected = if committed {
        InsertOutcome::AlreadyPresent
    } else {
        InsertOutcome::Inserted { seq: 0 }
    };
    assert_eq!(again.insert, expected, "k={k}");
    assert_eq!(repo.verify(true).unwrap().problems, [], "k={k}");
    (finished, committed)
}

#[test]
#[cfg(feature = "test-hooks")]
fn commit_never_inserts_record_before_objects_are_durable() {
    if let Some(root) = child_root() {
        // SWARMOPS_TEST_CRASH_AT is set: open shares one CrashAtFs between the object store
        // and the metadata store and writes objects one at a time, so that call k is the
        // same call in every run.
        let repo = LocalRepo::open(&root).unwrap();
        repo.commit(crash_request()).unwrap();
        return;
    }
    let want = {
        let (_dir, _root, reference) = new_repo();
        reference.commit(crash_request()).unwrap().id
    };
    let (_template_dir, template, repo) = new_repo();
    drop(repo);
    let past_the_commit_point = AtomicBool::new(false);
    let calls = sweep(|k| {
        let (finished, committed) = commit_crashing_at(&template, want, k);
        if committed && !finished {
            past_the_commit_point.store(true, Ordering::Relaxed);
        }
        finished
    });
    // Opening and committing make dozens of filesystem calls; a sweep that ends at once
    // never crashed anything. And the metadata store's calls count too: some crash comes
    // after the changeset's journal frame, while it is materialized.
    assert!(calls > 20, "the child finished after {calls} calls");
    assert!(
        past_the_commit_point.load(Ordering::Relaxed),
        "no crash came after the commit point"
    );
}

#[test]
fn commit_is_idempotent_for_identical_request() {
    let (_dir, root, repo) = new_repo();
    let base = commit(&repo, &[], vec![put("a", b"a")]);
    let req = request(&[base.id], vec![put("a", b"a2"), put("b/c", b"c")]);
    let first = repo.commit(req.clone()).unwrap();
    assert_eq!(first.insert, InsertOutcome::Inserted { seq: 1 });
    let objects = object_files(&root);
    let frames = journal_frames(&root);

    let again = repo.commit(req).unwrap();
    assert_eq!((again.id, again.root), (first.id, first.root));
    assert_eq!(again.insert, InsertOutcome::AlreadyPresent);
    assert_eq!(
        (again.blobs_written, again.trees_written),
        (0, 0),
        "nothing new to write"
    );
    assert_eq!(again.warnings, Vec::<String>::new());
    assert_eq!(
        (first.files_changed, again.files_changed),
        (2, 2),
        "a replay counts its delta too"
    );
    assert_eq!(object_files(&root), objects);
    assert_eq!(journal_frames(&root), frames, "no new transaction");
    assert_eq!(repo.info().unwrap().changesets, 2);
}

/// Commits with `advance` (storage spec §4, `commit`): the changeset and its ref's move in one
/// transaction, so in one journal frame, recorded together or not at all, and only as a one-step
/// fast-forward from the first parent.
mod advance {
    use swarmops_repo::{Advance, Advanced};
    use swarmops_store::{JournalEntry, RefState};
    use swarmops_testkit::fs::RecordingFs;

    use super::*;

    /// Who the tests' moves are recorded for.
    const ACTOR: &str = "test/agent";

    /// A request for `changes` on `parents` ([`request`]) that advances ref `name`.
    fn advancing(parents: &[ChangesetId], changes: Vec<Change>, name: &str) -> CommitRequest {
        CommitRequest {
            advance: Some(Advance {
                name: name.to_owned(),
                actor: ACTOR.to_owned(),
            }),
            ..request(parents, changes)
        }
    }

    /// The move `advancing(.., name)` reports: from `old`, made now or before.
    fn moved(name: &str, old: Option<ChangesetId>, already_applied: bool) -> Advanced {
        Advanced {
            name: name.to_owned(),
            old,
            already_applied,
        }
    }

    /// A store with changeset `c0` (a file `a`) and main at it.
    fn with_main() -> (tempfile::TempDir, Utf8PathBuf, LocalRepo, ChangesetId) {
        let (dir, root, repo) = new_repo();
        let c0 = commit(&repo, &[], vec![put("a", b"a")]).id;
        repo.cas_ref("main", None, Some(c0), ACTOR).unwrap();
        (dir, root, repo, c0)
    }

    /// The records of the frames past the first `frames` of the store at `root`: each frame's
    /// seq, the record's kind and its key.
    fn records_past(root: &Utf8Path, frames: u64) -> Vec<(u64, &'static str, Vec<u8>)> {
        let meta = MetaStore::open(root).unwrap();
        let entries: Vec<JournalEntry> = meta
            .read()
            .unwrap()
            .journal_after(frames.checked_sub(1), 100)
            .unwrap();
        entries
            .into_iter()
            .map(|entry| (entry.seq, entry.kind, entry.key))
            .collect()
    }

    /// The error `got` holds, which must be `RefMoved` for ref `name`, naming `actual`.
    #[track_caller]
    fn assert_moved(
        got: Result<CommitOutcome, RepoError>,
        name: &str,
        actual: Option<ChangesetId>,
    ) {
        match got {
            Err(RepoError::RefMoved {
                name: moved,
                actual: found,
                ..
            }) => assert_eq!((moved.as_str(), found), (name, actual)),
            other => panic!("expected RefMoved for {name}, got {other:?}"),
        }
    }

    /// A commit with `advance` records its changeset and the ref's move in one new journal frame,
    /// the changeset first, under one seq: one append of `journal.log`, where the same work as a
    /// commit and then a compare-and-swap makes two.
    #[test]
    fn commit_advancing_is_one_frame() {
        let (_dir, root, repo, c0) = with_main();
        let version = repo.get_ref("main").unwrap().unwrap().version;
        drop(repo);
        let (fs, calls) = RecordingFs::new();
        let fs = fs.naming_paths_in(root.as_std_path().to_path_buf());
        let repo = LocalRepo::open_with_fs(&root, Arc::new(fs)).unwrap();
        let appends = |calls: Vec<String>| {
            (calls.iter())
                .filter(|call| *call == "append_durable meta/journal.log")
                .count()
        };
        let frames = journal_frames(&root);
        calls();

        let out = repo
            .commit(advancing(&[c0], vec![put("b", b"b")], "main"))
            .unwrap();
        assert_eq!(appends(calls()), 1, "one frame");
        assert_eq!(out.insert, InsertOutcome::Inserted { seq: 1 });
        assert_eq!(out.advanced, Some(moved("main", Some(c0), false)));
        assert_eq!(out.warnings, Vec::<String>::new());
        let main = RefState {
            target: out.id,
            version: version + 1,
        };
        assert_eq!(repo.get_ref("main").unwrap(), Some(main));
        assert_eq!(
            records_past(&root, frames),
            [
                (frames, "changeset", out.id.0.0.to_vec()),
                (frames, "ref_move", b"main".to_vec())
            ]
        );
        assert_eq!(journal_frames(&root), frames + 1);

        // The same work in two steps: two frames, two appends.
        let two = repo
            .commit(request(&[out.id], vec![put("c", b"c")]))
            .unwrap();
        repo.cas_ref("main", Some(out.id), Some(two.id), ACTOR)
            .unwrap();
        assert_eq!(appends(calls()), 2, "two frames");
    }

    /// A commit with `advance` whose ref moved meanwhile (main read at X, then moved X to Y)
    /// records nothing, neither its changeset nor the move: `RefMoved` with what main names. The
    /// store's changesets, its journal and main are as they were, and it verifies clean (the
    /// objects the commit flushed are unreferenced, which is no problem).
    #[test]
    fn a_commit_advancing_that_loses_records_nothing() {
        let (_dir, root, repo, x) = with_main();
        let y = commit(&repo, &[x], vec![put("a", b"y")]).id;
        repo.cas_ref("main", Some(x), Some(y), "test/other")
            .unwrap();
        let count = || {
            MetaStore::open(&root)
                .unwrap()
                .read()
                .unwrap()
                .count_changesets()
        };
        let before = (
            count().unwrap(),
            journal_frames(&root),
            repo.get_ref("main").unwrap(),
        );

        let lost = repo.commit(advancing(&[x], vec![put("b", b"b")], "main"));
        assert_moved(lost, "main", Some(y));
        let after = (
            count().unwrap(),
            journal_frames(&root),
            repo.get_ref("main").unwrap(),
        );
        assert_eq!(after, before, "nothing recorded");
        assert_eq!(repo.verify(true).unwrap().problems, []);
    }

    /// The same request made again, the ref at its changeset already, is a replay: the insert
    /// finds the changeset, the move is `already_applied`, and nothing is written, no frame and
    /// no new version of the ref.
    #[test]
    fn an_identical_advancing_commit_is_a_replay() {
        let (_dir, root, repo, c0) = with_main();
        let req = advancing(&[c0], vec![put("b", b"b")], "main");
        let first = repo.commit(req.clone()).unwrap();
        assert_eq!(first.advanced, Some(moved("main", Some(c0), false)));
        let (frames, main) = (journal_frames(&root), repo.get_ref("main").unwrap());

        let again = repo.commit(req).unwrap();
        assert_eq!(again.id, first.id);
        assert_eq!(again.insert, InsertOutcome::AlreadyPresent);
        assert_eq!(again.advanced, Some(moved("main", Some(c0), true)));
        assert_eq!(again.warnings, Vec::<String>::new());
        assert_eq!(journal_frames(&root), frames, "no new frame");
        assert_eq!(repo.get_ref("main").unwrap(), main, "main's version stays");
    }

    /// A root commit with `advance` creates the ref, in the changeset's frame. Once the ref
    /// exists, another root commit that advances it records nothing.
    #[test]
    fn a_root_commit_advancing_creates_the_ref() {
        let (_dir, root, repo) = new_repo();
        let out = repo
            .commit(advancing(&[], vec![put("a", b"a")], "main"))
            .unwrap();
        assert_eq!(out.insert, InsertOutcome::Inserted { seq: 0 });
        assert_eq!(out.advanced, Some(moved("main", None, false)));
        let main = RefState {
            target: out.id,
            version: 1,
        };
        assert_eq!(repo.get_ref("main").unwrap(), Some(main));
        assert_eq!(journal_frames(&root), 1, "one frame for both");

        let other = repo.commit(advancing(&[], vec![put("b", b"b")], "main"));
        assert_moved(other, "main", Some(out.id));
        assert_eq!(journal_frames(&root), 1, "nothing recorded");
        assert_eq!(repo.info().unwrap().changesets, 1);
        assert_eq!(repo.get_ref("main").unwrap(), Some(main));
    }

    /// A changeset the store holds already, committed again with `advance`, moves the ref: the
    /// insert finds the changeset, and the new frame holds the move alone.
    #[test]
    fn an_advance_of_a_present_changeset_moves_the_ref() {
        let (_dir, root, repo, c0) = with_main();
        let plain = request(&[c0], vec![put("b", b"b")]);
        let c1 = repo.commit(plain.clone()).unwrap().id;
        let frames = journal_frames(&root);

        let advance = advancing(&[c0], vec![put("b", b"b")], "main").advance;
        let out = repo.commit(CommitRequest { advance, ..plain }).unwrap();
        assert_eq!(out.id, c1);
        assert_eq!(out.insert, InsertOutcome::AlreadyPresent);
        assert_eq!(out.advanced, Some(moved("main", Some(c0), false)));
        let main = repo.get_ref("main").unwrap().map(|state| state.target);
        assert_eq!(main, Some(c1));
        assert_eq!(
            records_past(&root, frames),
            [(frames, "ref_move", b"main".to_vec())]
        );
    }

    /// A ref name that is not `main` or `release/<x>` (at most 255 bytes) is refused before
    /// anything is written: no object file, no frame.
    #[test]
    fn an_advance_with_a_bad_ref_name_writes_nothing() {
        let (_dir, root, repo, c0) = with_main();
        let objects = object_files(&root);
        let frames = journal_frames(&root);
        let long = format!("release/{}", "x".repeat(248));
        for name in [
            "feature/x",
            "release/",
            "release/.x",
            "Main",
            "",
            long.as_str(),
        ] {
            match repo.commit(advancing(&[c0], vec![put("new", b"new")], name)) {
                Err(RepoError::RefName(refused)) => assert_eq!(refused, name),
                other => panic!("{name:?}: expected RefName, got {other:?}"),
            }
        }
        assert_eq!(object_files(&root), objects, "no object was put");
        assert_eq!(journal_frames(&root), frames);
    }

    /// The commit the crash sweep's child makes: [`crash_request`]'s files, on main's head `base`,
    /// advancing main.
    #[cfg(feature = "test-hooks")]
    fn advancing_crash_request(base: ChangesetId) -> CommitRequest {
        let files = crash_request().changes;
        advancing(&[base], files, "main")
    }

    /// One run of the advancing crash sweep: a child commits [`advancing_crash_request`] on a
    /// copy of `template` (main at `base`) until its k-th filesystem call aborts it. The changeset
    /// `want` must then be in the store exactly when main names it, the store must verify clean,
    /// and the same request made again must end with both. Returns whether the child finished,
    /// and whether the changeset was committed.
    #[cfg(feature = "test-hooks")]
    fn advancing_commit_crashing_at(
        template: &Utf8Path,
        base: ChangesetId,
        want: ChangesetId,
        k: u64,
    ) -> (bool, bool) {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
        copy_dir(template.as_std_path(), root.as_std_path());
        let child = crash_child(
            "advance::an_advancing_commit_crashing_at_any_call_leaves_both_or_neither",
            &root,
            k,
        );
        let finished = child.status.success();
        assert!(
            finished || child.status.signal() == Some(SIGABRT),
            "k={k}: {:?}\n{}",
            child.status,
            String::from_utf8_lossy(&child.stderr)
        );

        // Both or neither: the changeset is there exactly when main names it.
        let repo = LocalRepo::open(&root).unwrap();
        let committed = repo.changeset(want).unwrap().is_some();
        let main = repo.get_ref("main").unwrap().map(|state| state.target);
        let expected = if committed { want } else { base };
        assert_eq!(main, Some(expected), "k={k}: committed {committed}");
        assert!(
            committed || !finished,
            "k={k}: the commit returned but is not stored"
        );
        assert_eq!(repo.verify(true).unwrap().problems, [], "k={k}");

        // The same request made again ends with both.
        let again = repo.commit(advancing_crash_request(base)).unwrap();
        assert_eq!(again.id, want, "k={k}");
        assert_eq!(
            again.advanced,
            Some(moved("main", Some(base), committed)),
            "k={k}"
        );
        let main = repo.get_ref("main").unwrap().map(|state| state.target);
        assert_eq!(main, Some(want), "k={k}");
        assert_eq!(repo.verify(true).unwrap().problems, [], "k={k}");
        (finished, committed)
    }

    /// A commit with `advance` that crashes at any filesystem call leaves its changeset and the
    /// ref's move both recorded or neither, never one: for every k, the changeset is in the store
    /// exactly when main names it, the store verifies clean, and the request made again ends
    /// with both. Some crash comes after the commit point. The two-step flow (a commit, then a
    /// compare-and-swap) fails this at every k between its two frames.
    #[test]
    #[cfg(feature = "test-hooks")]
    fn an_advancing_commit_crashing_at_any_call_leaves_both_or_neither() {
        if let Some(root) = child_root() {
            // SWARMOPS_TEST_CRASH_AT is set: `open` shares one CrashAtFs between the stores
            // and writes one object at a time, so call k is the same call in every run.
            let repo = LocalRepo::open(&root).unwrap();
            let base = repo.get_ref("main").unwrap().expect("main").target;
            repo.commit(advancing_crash_request(base)).unwrap();
            return;
        }
        let (_template_dir, template, repo, base) = with_main();
        drop(repo);
        let want = {
            let dir = tempfile::tempdir().unwrap();
            let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
            copy_dir(template.as_std_path(), root.as_std_path());
            let repo = LocalRepo::open(&root).unwrap();
            repo.commit(advancing_crash_request(base)).unwrap().id
        };
        let past_the_commit_point = AtomicBool::new(false);
        let calls = sweep(|k| {
            let (finished, committed) = advancing_commit_crashing_at(&template, base, want, k);
            if committed && !finished {
                past_the_commit_point.store(true, Ordering::Relaxed);
            }
            finished
        });
        assert!(calls > 20, "the child finished after {calls} calls");
        assert!(
            past_the_commit_point.load(Ordering::Relaxed),
            "no crash came after the commit point"
        );
    }
}

#[test]
fn commit_rejects_duplicate_paths_and_deletes_of_missing_paths() {
    let (_dir, root, repo) = new_repo();
    let base = commit(
        &repo,
        &[],
        vec![put("a.rs", b"a"), put("src/lib.rs", b"lib")],
    );
    let objects = object_files(&root);
    for (changes, twice) in [
        (vec![put("x", b"1"), put("x", b"2")], "x"),
        (vec![put("a.rs", b"new"), delete("a.rs")], "a.rs"),
    ] {
        match repo.commit(request(&[base.id], changes)) {
            Err(RepoError::DuplicatePath { path: dup }) => assert_eq!(dup, path(twice)),
            other => panic!("expected DuplicatePath at {twice}, got {other:?}"),
        }
    }
    for missing in ["nope", "src/nope", "a.rs/under-a-file", "deep/er/path"] {
        match repo.commit(request(&[base.id], vec![delete(missing)])) {
            Err(RepoError::PathNotFound { path: gone }) => assert_eq!(gone, path(missing)),
            other => panic!("expected PathNotFound at {missing}, got {other:?}"),
        }
    }
    // A root commit has nothing to delete.
    match repo.commit(request(&[], vec![put("b", b"b"), delete("a.rs")])) {
        Err(RepoError::PathNotFound { path: gone }) => assert_eq!(gone, path("a.rs")),
        other => panic!("expected PathNotFound, got {other:?}"),
    }
    assert_eq!(object_files(&root), objects, "nothing was written");
    assert_eq!(repo.info().unwrap().changesets, 1);
}

#[test]
fn commit_replaces_a_file_with_a_directory_and_back() {
    let (_dir, _root, repo) = new_repo();
    let base = commit(&repo, &[], vec![put("node", b"a file")]);
    // Deletes apply before puts, so one request can turn a file into a directory...
    let dir = commit(
        &repo,
        &[base.id],
        vec![put("node/child", b"inside"), delete("node")],
    );
    assert_eq!(
        delta(&repo, dir.id),
        [
            ("node".to_owned(), DeltaOp::Delete),
            ("node/child".to_owned(), file_put(b"inside")),
        ]
    );
    assert_eq!(
        listing(&repo, dir.id, Some("node")),
        Some(names(&[("child", EntryKind::File)]))
    );
    // ...and a directory back into a file, once its last file goes.
    let file = commit(
        &repo,
        &[dir.id],
        vec![put("node", b"a file again"), delete("node/child")],
    );
    assert_eq!(
        delta(&repo, file.id),
        [
            ("node".to_owned(), file_put(b"a file again")),
            ("node/child".to_owned(), DeltaOp::Delete),
        ]
    );
    assert_eq!(
        content(&repo, file.id, "node"),
        Some(b"a file again".to_vec())
    );
    assert_eq!(listing(&repo, file.id, Some("node")), None);
}

#[test]
fn commit_records_only_what_changes() {
    let (_dir, _root, repo) = new_repo();
    let script = b"#!/bin/sh\necho hi\n";
    let base = commit(
        &repo,
        &[],
        vec![put("same.txt", b"same"), put("tool", script)],
    );
    // A put of what a path holds already is left out of the delta; a new kind alone is a
    // change; two files with the same bytes share one blob.
    let out = commit(
        &repo,
        &[base.id],
        vec![
            put("same.txt", b"same"),
            put_as("tool", EntryKind::Exec, script),
            put("new.txt", b"new"),
            put("twin.txt", b"new"),
        ],
    );
    assert_eq!(
        delta(&repo, out.id),
        [
            ("new.txt".to_owned(), file_put(b"new")),
            (
                "tool".to_owned(),
                DeltaOp::Put {
                    kind: EntryKind::Exec,
                    id: blob(script),
                }
            ),
            ("twin.txt".to_owned(), file_put(b"new")),
        ]
    );
    assert_eq!(
        (out.blobs_written, out.trees_written),
        (1, 1),
        "one blob for both new files (the script's is stored already), and the root"
    );
    assert_eq!(
        out.files_changed, 3,
        "four changes, but the put of what same.txt holds already is not one"
    );
    assert_eq!(
        repo.read_blob_at(out.id, &path("tool")).unwrap(),
        Some((EntryKind::Exec, blob(script), script.to_vec()))
    );
}

#[test]
fn a_failed_object_flush_fails_the_commit_and_inserts_nothing() {
    let (_dir, root, repo) = new_repo();
    drop(repo);
    let objects = root.join("objects").into_std_path_buf();
    // The fan-out directories' fsyncs fail: the objects' names would not survive a power
    // loss, so the changeset must not be recorded.
    let fs = FailingFs::new(move |method, p| {
        method == "fsync_dir" && p.parent() == Some(objects.as_path())
    });
    let repo = LocalRepo::open_with_fs(&root, Arc::new(fs)).unwrap();
    match repo.commit(request(&[], vec![put("a", b"a")])) {
        Err(RepoError::Store(StoreError::Io(e))) => {
            assert!(e.to_string().contains("injected"), "{e}");
        }
        other => panic!("expected the flush's error, got {other:?}"),
    }
    assert_eq!(journal_frames(&root), 0, "no transaction");
    let fresh = LocalRepo::open(&root).unwrap();
    assert_eq!(fresh.info().unwrap().changesets, 0);
    assert_eq!(fresh.verify(true).unwrap().problems, []);
}

/// `flush_metadata` makes the metadata's projections durable at once: a commit moves the
/// checkpoint only every 32 frames, so after two commits both frames lie past it; the flush syncs
/// the index and moves the checkpoint to the end of the journal, and appends no frame. A second
/// flush, with no frame past the checkpoint, changes nothing.
#[test]
fn flush_metadata_moves_the_checkpoint_to_the_journal_end() {
    let (_dir, root, repo) = new_repo();
    let ckpt = || {
        Journal::open(&Layout::new(&root), Arc::new(RealFs))
            .unwrap()
            .ckpt()
            .unwrap()
    };
    let log_len = || fs::metadata(root.join("meta/journal.log")).unwrap().len();
    let first = commit(&repo, &[], vec![put("a", b"1")]);
    commit(&repo, &[first.id], vec![put("a", b"2")]);
    let end = log_len();
    assert_eq!(journal_frames(&root), 2);
    assert!(ckpt() < end, "both frames lie past the checkpoint");

    repo.flush_metadata().unwrap();
    assert_eq!(ckpt(), end);
    assert_eq!(
        (journal_frames(&root), log_len()),
        (2, end),
        "no frame appended"
    );

    repo.flush_metadata().unwrap();
    assert_eq!(ckpt(), end);
    assert_eq!((journal_frames(&root), log_len()), (2, end));
}

/// An object `put_objects` stored is durable by name once the handle next flushes its objects,
/// which a commit does before its journal frame whether or not its changeset names the object: so
/// the git import can put a commit's blobs and trees first and record the changeset after them
/// (storage spec §2 rule 2). The put alone syncs no directory.
#[test]
fn put_objects_are_made_durable_by_the_next_commit() {
    use swarmops_testkit::fs::RecordingFs;

    let (_dir, root, repo) = new_repo();
    drop(repo);
    let (fs, calls) = RecordingFs::new();
    let fs = fs.naming_paths_in(root.as_std_path().to_path_buf());
    let repo = LocalRepo::open_with_fs(&root, Arc::new(fs)).unwrap();
    calls();

    let payload = b"put before the commit that makes it durable\n";
    let ids = repo
        .put_objects(vec![(ObjectKind::Blob, payload.to_vec())])
        .unwrap();
    assert_eq!(ids, [blob(payload).0]);
    let dir = fanout(ids[0]);
    let fsync = format!("fsync_dir {dir}");
    let putting = calls();
    assert!(
        !putting.iter().any(|call| call.starts_with("fsync_dir")),
        "{putting:?}"
    );

    // A commit none of whose own objects lies in that directory.
    repo.commit(a_commit_outside(&dir)).unwrap();
    let committing = calls();
    let at = |call: &str| {
        committing
            .iter()
            .position(|made| made == call)
            .unwrap_or_else(|| panic!("no {call:?} among {committing:?}"))
    };
    assert!(
        at(&fsync) < at("append_durable meta/journal.log"),
        "{committing:?}"
    );
}

/// The fan-out directory of object `id`, as a path in the store: `objects/<xx>`.
fn fanout(id: ObjectId) -> String {
    format!("objects/{:02x}", id.0[0])
}

/// What a root commit of `req`, one put of a file at the root, writes: its blob, its root tree
/// and the changeset, built as `Repo::commit` builds them (which
/// `root_commit_from_change_list_builds_trees_and_delta` pins).
fn root_commit_objects(req: &CommitRequest) -> [ObjectId; 3] {
    let [
        Change {
            path: file,
            op: ChangeOp::Put { kind, content },
        },
    ] = &req.changes[..]
    else {
        panic!("a request of one put: {:?}", req.changes);
    };
    let blob = blob(content);
    let root = tree(vec![entry(file.as_str(), *kind, blob.0)]).id();
    let changeset = Changeset {
        parents: Vec::new(),
        root,
        delta: vec![DeltaEntry {
            path: file.clone(),
            op: DeltaOp::Put {
                kind: *kind,
                id: blob,
            },
            renamed_from: None,
        }],
        author: req.author.clone(),
        committer: req.committer.clone(),
        message: req.message.clone(),
        extra_headers: Vec::new(),
        metadata: req.metadata.clone(),
    };
    [blob.0, root.0, changeset.id().unwrap().0]
}

/// A root commit of one file whose three objects lie in one fan-out directory, and that
/// directory: the file's content is picked so that its blob and the root tree share one (about
/// one candidate in 256 does, so a match turns up long before 10,000), then the message so that
/// the changeset joins them. Its flush has that one directory to fsync, which it does on the
/// calling thread: a store that writes its objects in parallel makes that fsync at the same call
/// in every run.
fn a_commit_in_one_fanout_dir() -> (CommitRequest, String) {
    let share_a_dir = |req: &CommitRequest, a: usize, b: usize| {
        let objects = root_commit_objects(req);
        fanout(objects[a]) == fanout(objects[b])
    };
    let base = (0..10_000_u32)
        .map(|i| {
            let content = format!("one fan-out directory {i}\n");
            request(&[], vec![put("one.txt", content.as_bytes())])
        })
        .find(|req| share_a_dir(req, 0, 1))
        .expect("about one content in 256 fits");
    let req = (0..10_000_u32)
        .map(|i| CommitRequest {
            message: format!("a test commit, take {i}").into_bytes(),
            ..base.clone()
        })
        .find(|req| share_a_dir(req, 0, 2))
        .expect("about one message in 256 fits");
    let dir = fanout(root_commit_objects(&req)[0]);
    (req, dir)
}

/// A root commit of one file none of whose objects lies in fan-out directory `dir`.
fn a_commit_outside(dir: &str) -> CommitRequest {
    (0..10_000_u32)
        .map(|i| {
            let content = format!("elsewhere {i}\n");
            request(&[], vec![put("two.txt", content.as_bytes())])
        })
        .find(|req| root_commit_objects(req).iter().all(|&id| fanout(id) != dir))
        .expect("most candidates fit")
}

/// A [`RecordingFs`] naming each call `"<method> <path in the store at root>"`, and the function
/// that returns the calls made since it last ran.
#[cfg(feature = "test-hooks")]
fn recording_in(root: &Utf8Path) -> (RecordingFs, impl Fn() -> Vec<String>) {
    let (fs, calls) = RecordingFs::new();
    (fs.naming_paths_in(root.as_std_path().to_path_buf()), calls)
}

/// Asserts that `calls` has call `first`, and has it before call `then`.
#[cfg(feature = "test-hooks")]
#[track_caller]
fn assert_called_before(calls: &[String], first: &str, then: &str) {
    let at = |call: &str| {
        calls
            .iter()
            .position(|made| made == call)
            .unwrap_or_else(|| panic!("no {call:?} among {calls:?}"))
    };
    assert!(at(first) < at(then), "{first:?} after {then:?}: {calls:?}");
}

/// A fan-out fsync that fails before it runs (`ENOSPC` strikes before the call, as a directory
/// that cannot be opened fails it: `EMFILE`, `ENOMEM`, `EACCES`) fails the commit, and the
/// directory stays queued until a later flush fsyncs it. That is what the object store's re-queue
/// is for: a failed fsync, one that ran (an `EIO` strikes after it), is fail-stop on the
/// filesystems the store supports (storage spec §2 rule 5), so no later sync could succeed after
/// it, as the second and third commits here do. A characterization test: any change that
/// swallows a flush error, or drops `put()`'s re-touch or the re-queue, fails it.
#[test]
#[cfg(feature = "test-hooks")]
fn a_failed_fanout_fsync_fails_the_commit_and_a_later_flush_syncs_the_directory() {
    let (first, dir) = a_commit_in_one_fanout_dir();
    let fsync = format!("fsync_dir {dir}");
    let frame = "append_durable meta/journal.log";

    // A dry run on a twin store: how many calls the open and the commit make before the flush's
    // one fsync, which is then call k.
    let k = {
        let (_dir, root, repo) = new_repo();
        drop(repo);
        let (fs, calls) = recording_in(&root);
        let repo = LocalRepo::open_with_fs(&root, checked(fs)).unwrap();
        repo.commit(first.clone()).unwrap();
        let calls = calls();
        let fsyncs: Vec<&String> = calls
            .iter()
            .filter(|call| call.starts_with("fsync_dir objects"))
            .collect();
        assert_eq!(fsyncs, [&fsync], "the flush fsyncs the one directory");
        u64::try_from(calls.iter().position(|call| *call == fsync).unwrap()).unwrap() + 1
    };

    let (_dir, root, repo) = new_repo();
    drop(repo);
    let (recording, calls) = recording_in(&root);
    let enospc = FailAt {
        at: k,
        mode: FailMode::Once,
        error: InjectedError::Enospc,
    };
    let repo = LocalRepo::open_with_fs(&root, checked(FailAtFs::new(recording, enospc))).unwrap();
    let journal = || {
        let len = |file: &str| fs::metadata(root.join("meta").join(file)).unwrap().len();
        (len("journal.log"), len("journal.off"))
    };
    let before = journal();
    calls();

    // 1. The commit fails with the fsync's error (E_IO), and nothing is recorded.
    match repo.commit(first.clone()) {
        Err(RepoError::Store(StoreError::Io(e))) => {
            assert_eq!(e.raw_os_error(), Some(28), "ENOSPC: {e}");
        }
        other => panic!("expected the flush's error, got {other:?}"),
    }
    let failed = calls();
    let linked = format!("link_noclobber {dir}/");
    assert_eq!(
        failed
            .iter()
            .filter(|call| call.starts_with(&linked))
            .count(),
        3,
        "the three objects were linked into {dir}: {failed:?}"
    );
    assert!(!failed.contains(&fsync), "the fsync never ran: {failed:?}");
    assert_eq!(journal(), before, "no frame, no listing");
    let info = repo.info().unwrap();
    assert_eq!((info.changesets, info.main), (0, None));

    // 2. A commit whose objects all lie in other fan-out directories succeeds on the same
    // handle, and its flush fsyncs the first commit's directory too: the re-queue.
    let second = a_commit_outside(&dir);
    let out = repo.commit(second.clone()).unwrap();
    assert_eq!(out.insert, InsertOutcome::Inserted { seq: 0 });
    let objects = root_commit_objects(&second);
    assert_eq!((objects[1], objects[2]), (out.root.0, out.id.0));
    assert!(
        objects.iter().all(|&id| fanout(id) != dir),
        "{objects:?} lie outside {dir}, so only the re-queue can fsync it"
    );
    assert_called_before(&calls(), &fsync, frame);

    // 3. The first commit made again succeeds: its objects are there already, and `put()`'s
    // re-touch queues their directory again, so that it is fsynced before the frame names them.
    let again = repo.commit(first).unwrap();
    assert_eq!(again.insert, InsertOutcome::Inserted { seq: 1 });
    assert_eq!((again.blobs_written, again.trees_written), (0, 0));
    let made = calls();
    assert!(
        !made.iter().any(|call| call.starts_with("link_noclobber")),
        "nothing was linked again: {made:?}"
    );
    assert_called_before(&made, &fsync, frame);

    // 4. Every call ran under the order checker, which judged W5 at both frames.
    assert_eq!(repo.verify(true).unwrap().problems, []);
}

/// A fan-out directory whose fsync fails every time fails every later commit on the handle,
/// even one whose objects all lie elsewhere, because the re-queue retries it on every flush:
/// the stickiness storage spec §2 rule 5 assumes of a failed directory fsync. The failure is
/// that one directory's (`FailAtFs`'s `persistent` mode would fail every later mutating call,
/// and with it any store's next commit at its first write), and nothing is inserted.
#[test]
fn a_persistent_eio_on_a_fanout_fsync_fails_every_later_commit_on_the_handle() {
    let (first, dir) = a_commit_in_one_fanout_dir();
    let second = a_commit_outside(&dir);
    let (_dir, root, repo) = new_repo();
    drop(repo);
    let failing = root.join(&dir).into_std_path_buf();
    let fs = FailingFs::new(move |method, p| method == "fsync_dir" && p == failing.as_path());
    let repo = LocalRepo::open_with_fs(&root, checked(fs)).unwrap();
    let commits = [
        ("the first commit", &first),
        ("a commit whose objects lie elsewhere", &second),
        ("that commit again", &second),
        ("the first commit again", &first),
    ];
    for (what, req) in commits {
        match repo.commit(req.clone()) {
            Err(RepoError::Store(StoreError::Io(e))) => {
                assert!(e.to_string().ends_with(&dir), "{what}: {e}");
            }
            other => panic!("{what}: expected the fsync's error, got {other:?}"),
        }
    }
    assert_eq!(journal_frames(&root), 0, "no transaction");
    let fresh = LocalRepo::open(&root).unwrap();
    assert_eq!(fresh.info().unwrap().changesets, 0);
    assert_eq!(fresh.verify(true).unwrap().problems, []);
}

#[test]
fn a_failure_after_the_commit_point_is_a_warning() {
    let (_dir, root, repo) = new_repo();
    drop(repo);
    // Materializing the frame into the index fails after the frame is durable.
    let fs = FailingFs::new(|method, p| method == "append" && p.ends_with("idx/ids.col"));
    let repo = LocalRepo::open_with_fs(&root, Arc::new(fs)).unwrap();
    let out = repo.commit(request(&[], vec![put("a", b"a")])).unwrap();
    assert_eq!(out.insert, InsertOutcome::Inserted { seq: 0 });
    let [warning] = &out.warnings[..] else {
        panic!("expected one warning, got {:?}", out.warnings);
    };
    assert!(warning.contains("frame 0"), "{warning}");
    assert!(warning.contains("injected"), "{warning}");

    // The changeset is committed: another handle sees it, and its next write repairs the
    // index.
    let fresh = LocalRepo::open(&root).unwrap();
    let stored = fresh.changeset(out.id).unwrap().expect("committed");
    assert_eq!(stored.seq, 0);
    let next = fresh
        .commit(request(&[out.id], vec![put("b", b"b")]))
        .unwrap();
    assert_eq!(next.insert, InsertOutcome::Inserted { seq: 1 });
    assert_eq!(next.warnings, Vec::<String>::new());
    assert_eq!(fresh.verify(true).unwrap().problems, []);
}

#[test]
fn a_ref_move_whose_file_write_fails_is_applied_with_a_warning() {
    let (_dir, root, repo) = new_repo();
    let base = commit(&repo, &[], vec![put("a", b"a")]);
    drop(repo);
    // Writing the ref file fails after the move's journal frame is durable.
    let fs = FailingFs::new(|method, p| method == "atomic_replace" && p.ends_with("refs/main"));
    let repo = LocalRepo::open_with_fs(&root, Arc::new(fs)).unwrap();
    let out = repo
        .cas_ref("main", None, Some(base.id), "test/agent")
        .unwrap();
    assert_eq!(out.result, CasResult::Applied { old: None });
    let [warning] = &out.warnings[..] else {
        panic!("expected one warning, got {:?}", out.warnings);
    };
    assert!(warning.contains("frame 1"), "{warning}");
    assert!(warning.contains("injected"), "{warning}");

    // The move is committed: another handle sees it, and its next write writes the file.
    let ref_file = root.join("meta/refs/main");
    assert!(!ref_file.exists());
    let fresh = LocalRepo::open(&root).unwrap();
    let main = fresh.get_ref("main").unwrap().map(|state| state.target);
    assert_eq!(main, Some(base.id));
    fresh
        .commit(request(&[base.id], vec![put("b", b"b")]))
        .unwrap();
    assert_eq!(
        fs::read_to_string(&ref_file).unwrap(),
        format!("{}\n1\n", base.id)
    );
    assert_eq!(fresh.verify(true).unwrap().problems, []);
}

/// The repository at `root`, its listings in `journal.off` failing: a commit or a ref move made
/// through it commits, its frame whole and durable in `journal.log` but not listed, with one
/// warning, and nothing materialized.
fn listing_fails(root: &Utf8Path) -> LocalRepo {
    let fs = FailingFs::new(|method, p| method == "append_durable" && p.ends_with("journal.off"));
    LocalRepo::open_with_fs(root, Arc::new(fs)).unwrap()
}

/// Every outcome of a commit and of a ref move carries what its write's recovery repaired
/// first: here, the frame of a commit or a move whose listing in `journal.off` failed (committed,
/// with a warning), which the write lists, and whose changeset or ref file it replays. The
/// report comes with `Inserted` and with `AlreadyPresent` (the same request again, the retry an
/// agent makes after the warning), with `Applied` and with `Mismatch`, and with the `Applied` of
/// a move to the target it expects, which commits nothing; none of them warns.
#[test]
fn commit_and_cas_outcomes_carry_their_write_s_recovery() {
    let listed_and_replayed = Recovered {
        frames_listed: 1,
        records_replayed: 1,
        ..Recovered::default()
    };
    for again in [false, true] {
        let (_dir, root, repo) = new_repo();
        drop(repo);
        let first = request(&[], vec![put("a", b"a")]);
        let failed = listing_fails(&root).commit(first.clone()).unwrap();
        assert_eq!(failed.warnings.len(), 1, "{failed:?}");
        let repo = LocalRepo::open(&root).unwrap();
        let req = if again {
            first
        } else {
            request(&[failed.id], vec![put("b", b"b")])
        };
        let out = repo.commit(req).unwrap();
        let want = if again {
            InsertOutcome::AlreadyPresent
        } else {
            InsertOutcome::Inserted { seq: 1 }
        };
        assert_eq!(out.insert, want);
        assert_eq!(out.recovered, listed_and_replayed, "{want:?}");
        assert_eq!(out.warnings, Vec::<String>::new(), "{want:?}");
    }

    // main created at c1 through a move whose listing failed (its ref file never written), then
    // a move whose write lists that frame and writes the file.
    let listed_and_written = Recovered {
        frames_listed: 1,
        refs_replayed: 1,
        ..Recovered::default()
    };
    let moves = |c1: ChangesetId, c2: ChangesetId| {
        [
            (Some(c1), Some(c2), CasResult::Applied { old: Some(c1) }),
            (Some(c2), Some(c1), CasResult::Mismatch { actual: Some(c1) }),
            (Some(c1), Some(c1), CasResult::Applied { old: Some(c1) }),
        ]
    };
    for at in 0..3 {
        let (_dir, root, repo) = new_repo();
        let c1 = commit(&repo, &[], vec![put("a", b"a")]).id;
        let c2 = commit(&repo, &[c1], vec![put("b", b"b")]).id;
        drop(repo);
        let created = listing_fails(&root)
            .cas_ref("main", None, Some(c1), "test/agent")
            .unwrap();
        assert_eq!(created.result, CasResult::Applied { old: None });
        assert_eq!(created.warnings.len(), 1, "{created:?}");
        let (expected, new, want) = moves(c1, c2)[at];
        let repo = LocalRepo::open(&root).unwrap();
        let log = Layout::new(&root).journal_log();
        let written = fs::metadata(&log).unwrap().len();
        let out = repo.cas_ref("main", expected, new, "test/agent").unwrap();
        assert_eq!(out.result, want, "{expected:?} to {new:?}");
        assert_eq!(out.recovered, listed_and_written, "{expected:?} to {new:?}");
        assert_eq!(
            out.warnings,
            Vec::<String>::new(),
            "{expected:?} to {new:?}"
        );
        let grew = fs::metadata(&log).unwrap().len() > written;
        assert_eq!(
            grew,
            at == 0,
            "only the move that moves main writes a frame"
        );
        let main = repo.get_ref("main").unwrap().map(|state| state.target);
        assert_eq!(main, if at == 0 { Some(c2) } else { Some(c1) });
    }
}

/// Waits until the parent says go, so that the children commit at the same time.
fn wait_for_go(root: &Utf8Path) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !root.join("go").exists() {
        assert!(Instant::now() < deadline, "the parent never said go");
        thread::sleep(Duration::from_millis(1));
    }
}

/// A child of `commits_from_many_processes_all_land`: a chain of [`PER_CHILD`] commits off
/// `main`, each adding a file of its own; every child also puts the same `shared.txt`, so
/// that identical objects race between processes too. Reports the chain's head.
fn commit_child(root: &Utf8Path, number: u64) {
    let repo = LocalRepo::open(root).unwrap();
    let mut parent = repo.resolve("main").unwrap();
    wait_for_go(root);
    for j in 0..PER_CHILD {
        let own = format!("child-{number}/file-{j}.txt");
        let changes = vec![
            put(&own, format!("{number}/{j}").as_bytes()),
            put("shared.txt", b"the same bytes in every process"),
        ];
        parent = repo.commit(request(&[parent], changes)).unwrap().id;
    }
    println!("{REPORT}{parent}");
}

#[test]
fn commits_from_many_processes_all_land() {
    if let Some(root) = child_root() {
        let number = std::env::var(CHILD_NUMBER).unwrap().parse().unwrap();
        commit_child(&root, number);
        return;
    }
    let (_dir, root, repo) = new_repo();
    let base = commit(&repo, &[], vec![put("README", b"base")]);
    repo.cas_ref("main", None, Some(base.id), "test/parent")
        .unwrap();

    let children: Vec<Child> = (0..CHILDREN)
        .map(|number| {
            Child::spawn(
                this_test(&test_name!("commits_from_many_processes_all_land"))
                    .env(CHILD_ROOT, &root)
                    .env(CHILD_NUMBER, number.to_string()),
            )
        })
        .collect();
    fs::write(root.join("go"), b"").unwrap();
    let deadline = Instant::now() + CHILD_TIMEOUT;
    let heads: Vec<ChangesetId> = children
        .into_iter()
        .enumerate()
        .map(|(number, child)| {
            let output = child.wait(deadline);
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success(),
                "child {number}: {:?}\n{stdout}\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            let head = stdout
                .lines()
                .find_map(|line| line.strip_prefix(REPORT))
                .expect("the child reports its head");
            ChangesetId(ObjectId::from_hex(head).unwrap())
        })
        .collect();

    assert_eq!(repo.info().unwrap().changesets, 1 + CHILDREN * PER_CHILD);
    for (number, &head) in heads.iter().enumerate() {
        let log = repo.log_first_parent(head, 100).unwrap();
        assert_eq!(
            log.len(),
            usize::try_from(PER_CHILD + 1).unwrap(),
            "child {number}"
        );
        assert_eq!(log.last().map(|cs| cs.id), Some(base.id), "child {number}");
        for j in 0..PER_CHILD {
            assert_eq!(
                content(&repo, head, &format!("child-{number}/file-{j}.txt")),
                Some(format!("{number}/{j}").into_bytes()),
            );
        }
        assert_eq!(
            content(&repo, head, "shared.txt"),
            Some(b"the same bytes in every process".to_vec())
        );
    }
    assert_eq!(repo.verify(true).unwrap().problems, []);
}
