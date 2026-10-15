//! `Repo::verify`: a clean store has no problems (temp files and stray entries under
//! `objects/` included); each kind of damage is reported as its own problem: object files
//! that do not hash to their ids or are missing, changesets whose parents are missing or
//! whose root is not their parent's root with their delta applied, metadata records that
//! disagree with each other or with their objects, sorted views that disagree with the
//! index columns, a corrupt journal, and ref files that are damaged or name no changeset.
//! Damage to the structure is found by deep and shallow checks alike; only a deep check
//! re-hashes blobs.
//!
//! Every test owns a fresh store (`tempfile::tempdir()`) and damages it by hand.

use std::fs;
use std::sync::Arc;
use std::time::Duration;

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_core::{
    BlobId, Changeset, ChangesetId, DeltaEntry, DeltaOp, EntryKind, ObjectId, ObjectKind, RepoPath,
    Signature, Tree, hash_object,
};
use swarmops_repo::{Change, ChangeOp, CommitRequest, LocalRepo, VerifyProblem, VerifyReport};
use swarmops_store::{
    ChangesetMeta, FsObjectStore, Index, Journal, Layout, MetaLock, MetaStore, ObjectStore, RealFs,
};
use tempfile::TempDir;

fn new_repo() -> (TempDir, Utf8PathBuf, LocalRepo) {
    let dir = tempfile::tempdir().expect("create a temp dir");
    let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).expect("temp dir is UTF-8");
    let repo = LocalRepo::create(&root, "test").expect("create the store");
    (dir, root, repo)
}

fn path(s: &str) -> RepoPath {
    RepoPath::new(s).expect("a valid repo path")
}

fn blob(content: &[u8]) -> BlobId {
    BlobId(hash_object(ObjectKind::Blob, content))
}

fn put(p: &str, content: &[u8]) -> Change {
    put_as(p, EntryKind::File, content)
}

fn put_as(p: &str, kind: EntryKind, content: &[u8]) -> Change {
    Change {
        path: path(p),
        op: ChangeOp::Put {
            kind,
            content: content.to_vec(),
        },
    }
}

fn delete(p: &str) -> Change {
    Change {
        path: path(p),
        op: ChangeOp::Delete,
    }
}

fn signature() -> Signature {
    Signature {
        name: b"test/agent".to_vec(),
        email: b"agent@test.swarmops.local".to_vec(),
        secs: 1_760_000_000,
        offset_secs: 0,
        negative_zero: false,
    }
}

#[track_caller]
fn commit(repo: &LocalRepo, parents: &[ChangesetId], changes: Vec<Change>) -> ChangesetId {
    let req = CommitRequest {
        parents: parents.to_vec(),
        changes,
        author: signature(),
        committer: signature(),
        message: b"a test commit".to_vec(),
        metadata: Vec::new(),
    };
    repo.commit(req).expect("commit").id
}

/// The file of object `id` in the store at `root`.
fn object_file(root: &Utf8Path, id: ObjectId) -> Utf8PathBuf {
    Layout::new(root).object_path(id)
}

/// How many object files the store at `root` holds: the 62-hex files of its fan-out
/// directories.
fn object_count(root: &Utf8Path) -> u64 {
    let mut count = 0;
    for byte in 0..=255u8 {
        for file in fs::read_dir(root.join(format!("objects/{byte:02x}"))).unwrap() {
            let name = file.unwrap().file_name().into_string().unwrap();
            if name.len() == 62 && name.bytes().all(|b| b.is_ascii_hexdigit()) {
                count += 1;
            }
        }
    }
    count
}

/// Compacts the index of the store at `root` under the writer lock, so that the sorted views
/// cover every record; returns how many they cover.
fn compact(root: &Utf8Path) -> u64 {
    let layout = Layout::new(root);
    let _lock = MetaLock::acquire(&layout, Duration::from_secs(10)).unwrap();
    Index::open(&layout, Arc::new(RealFs))
        .unwrap()
        .compact()
        .unwrap()
}

/// Asserts that verify finds exactly `want`, in any order, deep (`deep`) or shallow; returns
/// its report.
#[track_caller]
fn assert_problems_when(
    repo: &LocalRepo,
    deep: bool,
    mut want: Vec<VerifyProblem>,
) -> VerifyReport {
    let report = repo.verify(deep).unwrap();
    let mut got = report.problems.clone();
    let key = |p: &VerifyProblem| format!("{p:?}");
    got.sort_by_key(key);
    want.sort_by_key(key);
    assert_eq!(got, want, "deep: {deep}");
    report
}

/// Asserts that verify finds exactly `want`, deep and shallow alike: damage to the structure,
/// which a shallow check covers too. Returns the deep check's report.
#[track_caller]
fn assert_problems(repo: &LocalRepo, want: Vec<VerifyProblem>) -> VerifyReport {
    assert_problems_when(repo, false, want.clone());
    assert_problems_when(repo, true, want)
}

#[test]
fn verify_clean_store_zero_problems() {
    let (_dir, root, repo) = new_repo();
    for deep in [true, false] {
        assert_eq!(
            repo.verify(deep).unwrap(),
            VerifyReport {
                objects_checked: 0,
                changesets_checked: 0,
                problems: Vec::new(),
            }
        );
    }

    // A history with nested trees, a deletion that prunes, a merge, a commit that empties the
    // root, two refs and a compacted index.
    let c1 = commit(
        &repo,
        &[],
        vec![
            put("a.rs", b"a"),
            put("src/lib.rs", b"v1"),
            put("src/x/y.txt", b"y"),
        ],
    );
    let c2 = commit(
        &repo,
        &[c1],
        vec![put("src/lib.rs", b"v2"), delete("src/x/y.txt")],
    );
    let side = commit(
        &repo,
        &[c1],
        vec![put_as("bin/tool", EntryKind::Exec, b"#!/bin/sh\n")],
    );
    let merge = commit(&repo, &[c2, side], vec![put("merged.txt", b"m")]);
    let empty = commit(
        &repo,
        &[merge],
        vec![delete("a.rs"), delete("src/lib.rs"), delete("merged.txt")],
    );
    repo.cas_ref("main", None, Some(empty), "test/agent")
        .unwrap();
    repo.cas_ref("release/1.0", None, Some(c2), "test/agent")
        .unwrap();
    assert_eq!(compact(&root), 5);

    // What a crash leaves in objects/tmp/, and stray entries elsewhere under objects/, are no
    // objects: verify passes over them.
    fs::write(root.join("objects/tmp/.tmpLEFTOVER"), b"half an object").unwrap();
    fs::write(root.join("objects/ab/not-an-object"), b"junk").unwrap();
    fs::write(root.join("objects/stray"), b"junk").unwrap();
    fs::create_dir(root.join("objects/zz")).unwrap();

    let deep = repo.verify(true).unwrap();
    assert_eq!(deep.problems, []);
    assert_eq!(deep.changesets_checked, 5);
    assert_eq!(deep.objects_checked, object_count(&root));
    // Every object here is referenced, so a shallow check examines them all as well.
    assert_eq!(repo.verify(false).unwrap(), deep);
}

#[test]
fn verify_detects_corrupt_blob_and_missing_object() {
    let (_dir, root, repo) = new_repo();
    let c1 = commit(
        &repo,
        &[],
        vec![put("a.txt", b"aaa"), put("b/c.txt", b"ccc")],
    );
    let c2 = commit(&repo, &[c1], vec![put("d.txt", b"ddd")]);

    // One blob's file no longer hashes to its id; another's is gone. Both changesets
    // reference both; each problem is reported once, against the first.
    let corrupt = blob(b"aaa").0;
    let file = object_file(&root, corrupt);
    let mut bytes = fs::read(&file).unwrap();
    *bytes.last_mut().unwrap() ^= 0x01;
    fs::write(&file, bytes).unwrap();
    let missing = blob(b"ccc").0;
    fs::remove_file(object_file(&root, missing)).unwrap();

    let report = assert_problems_when(
        &repo,
        true,
        vec![
            VerifyProblem::CorruptObject(corrupt),
            VerifyProblem::MissingObject {
                referenced_by: c1,
                id: missing,
            },
        ],
    );
    assert_eq!(report.changesets_checked, 2);
    // A shallow check reads no blob: the corrupt one goes unseen, the missing one does not.
    let shallow = assert_problems_when(
        &repo,
        false,
        vec![VerifyProblem::MissingObject {
            referenced_by: c1,
            id: missing,
        }],
    );
    assert_eq!(shallow.changesets_checked, 2);

    // A missing tree hides what is below it, and is reported against the first changeset
    // that has it, too.
    let b_tree = repo
        .list_tree(c2, None)
        .unwrap()
        .unwrap()
        .into_iter()
        .find(|entry| entry.name == "b")
        .unwrap()
        .id;
    fs::remove_file(object_file(&root, b_tree)).unwrap();
    let lost_b = VerifyProblem::MissingObject {
        referenced_by: c1,
        id: b_tree,
    };
    assert_problems_when(&repo, false, vec![lost_b.clone()]);
    assert_problems_when(
        &repo,
        true,
        vec![VerifyProblem::CorruptObject(corrupt), lost_b.clone()],
    );

    // A changeset whose own object is gone.
    fs::remove_file(object_file(&root, c2.0)).unwrap();
    let lost_c2 = VerifyProblem::MissingObject {
        referenced_by: c2,
        id: c2.0,
    };
    assert_problems_when(&repo, false, vec![lost_b.clone(), lost_c2.clone()]);
    assert_problems_when(
        &repo,
        true,
        vec![VerifyProblem::CorruptObject(corrupt), lost_b, lost_c2],
    );
}

#[test]
fn verify_detects_dangling_ref() {
    let (_dir, root, repo) = new_repo();
    let c1 = commit(&repo, &[], vec![put("f", b"1")]);
    repo.cas_ref("main", None, Some(c1), "test/agent").unwrap();
    repo.cas_ref("release/ok", None, Some(c1), "test/agent")
        .unwrap();

    // A ref file naming no changeset, and a file in refs/ that is no ref file at all.
    let refs = Layout::new(&root).meta_path("refs/release");
    let nowhere = ChangesetId(hash_object(ObjectKind::Changeset, b"never committed"));
    fs::write(refs.join("gone"), format!("{nowhere}\n1\n")).unwrap();
    fs::write(refs.join("broken"), b"not a ref file\n").unwrap();
    assert!(
        MetaStore::open(&root).unwrap().read().is_err(),
        "a damaged ref file fails every snapshot"
    );

    assert_problems(
        &repo,
        vec![
            VerifyProblem::CorruptRef("release/broken".to_owned()),
            VerifyProblem::DanglingRef("release/gone".to_owned()),
        ],
    );
}

#[test]
fn verify_detects_index_journal_mismatch() {
    let (_dir, root, repo) = new_repo();
    let c1 = commit(&repo, &[], vec![put("f", b"1")]);
    let c2 = commit(&repo, &[c1], vec![put("f", b"2")]);

    // An index record that no journal frame holds: what a writer that went around the
    // journal would leave.
    let layout = Layout::new(&root);
    {
        let _lock = MetaLock::acquire(&layout, Duration::from_secs(10)).unwrap();
        let mut index = Index::open(&layout, Arc::new(RealFs)).unwrap();
        let bogus = ChangesetMeta {
            id: ChangesetId(hash_object(ObjectKind::Changeset, b"not in the journal")),
            seq: 0,
            generation: 1,
            delta_hash: [7; 32],
            parents: Vec::new(),
        };
        assert_eq!(index.append(&bogus).unwrap(), 2);
        index.sync().unwrap();
    }
    assert_problems(&repo, vec![VerifyProblem::IndexMismatch { seq: 2 }]);

    // Two records in each other's places.
    let ids = layout.meta_path("idx/ids.col");
    let mut column = fs::read(&ids).unwrap();
    column[..32].copy_from_slice(&c2.0.0);
    column[32..64].copy_from_slice(&c1.0.0);
    fs::write(&ids, column).unwrap();
    assert_problems(
        &repo,
        vec![
            VerifyProblem::IndexMismatch { seq: 0 },
            VerifyProblem::IndexMismatch { seq: 1 },
            VerifyProblem::IndexMismatch { seq: 2 },
        ],
    );
}

#[test]
fn verify_detects_index_records_lost_below_the_checkpoint() {
    let (_dir, root, repo) = new_repo();
    let c1 = commit(&repo, &[], vec![put("f", b"1")]);
    commit(&repo, &[c1], vec![put("f", b"2")]);
    // Move the checkpoint past both records: the index must hold them from now on.
    MetaStore::open(&root).unwrap().flush().unwrap();
    // A power loss after a lying fsync could take the last record back.
    let ids = Layout::new(&root).meta_path("idx/ids.col");
    let column = fs::read(&ids).unwrap();
    fs::write(&ids, &column[..32]).unwrap();
    assert_problems(&repo, vec![VerifyProblem::IndexMismatch { seq: 1 }]);
}

#[test]
fn verify_checks_the_sorted_views_against_the_columns() {
    let (_dir, root, repo) = new_repo();
    let c1 = commit(&repo, &[], vec![put("f", b"1")]);
    let c2 = commit(&repo, &[c1], vec![put("f", b"2")]);
    commit(&repo, &[c2], vec![put("f", b"3")]);
    assert_eq!(compact(&root), 3);
    assert_problems(&repo, Vec::new());

    // Swap the first two keys of the id view: each now stands at the other's record, and out
    // of order.
    let layout = Layout::new(&root);
    let recs = {
        let index = Index::open(&layout, Arc::new(RealFs)).unwrap();
        let view = index.ids_view();
        [view.rec_at(0).unwrap(), view.rec_at(1).unwrap()]
    };
    let keys = layout.meta_path("sorted/ids.key");
    let mut view = fs::read(&keys).unwrap();
    let (first, rest) = view.split_at_mut(32);
    first.swap_with_slice(&mut rest[..32]);
    fs::write(&keys, view).unwrap();
    assert_problems(
        &repo,
        recs.map(|seq| VerifyProblem::IndexMismatch { seq })
            .to_vec(),
    );
}

#[test]
fn verify_reports_a_corrupt_journal_frame() {
    let (_dir, root, repo) = new_repo();
    let c1 = commit(&repo, &[], vec![put("f", b"1")]);
    commit(&repo, &[c1], vec![put("f", b"2")]);
    let layout = Layout::new(&root);
    let frames = Journal::open(&layout, Arc::new(RealFs))
        .unwrap()
        .frames_from(0, 10)
        .unwrap();
    assert_eq!(frames.len(), 2);

    // Flip a byte inside the second frame's payload: it no longer matches its CRC. The first
    // frame is still compared with the index; the second changeset is checked from the index.
    let log = layout.journal_log();
    let mut bytes = fs::read(&log).unwrap();
    let at = usize::try_from(frames[1].offset).unwrap() + 12;
    bytes[at] ^= 0xff;
    fs::write(&log, bytes).unwrap();
    let report = assert_problems(
        &repo,
        vec![VerifyProblem::JournalCorrupt {
            offset: frames[1].offset,
        }],
    );
    assert_eq!(report.changesets_checked, 2);
}

#[test]
fn verify_detects_missing_parents_root_mismatches_and_metadata_drift() {
    let (_dir, root, repo) = new_repo();
    let base = commit(&repo, &[], vec![put("a", b"a")]);
    let base_root = repo.changeset(base).unwrap().unwrap().changeset.root;
    let objects = FsObjectStore::open(Layout::new(&root), Box::new(RealFs));
    let changeset = |parents: Vec<ChangesetId>, root, delta| Changeset {
        parents,
        root,
        delta,
        author: signature(),
        committer: signature(),
        message: b"written by hand".to_vec(),
        extra_headers: Vec::new(),
        metadata: Vec::new(),
    };
    // Stores `cs` and records it with `parents` (which may not be the object's).
    let record = |cs: &Changeset, parents: Vec<ChangesetId>| {
        let id = ChangesetId(
            objects
                .put(ObjectKind::Changeset, &cs.encode().unwrap())
                .unwrap(),
        );
        objects.flush().unwrap();
        let meta = MetaStore::open(&root).unwrap();
        let mut write = meta.write().unwrap();
        write
            .insert_changeset(ChangesetMeta {
                id,
                seq: 0,
                generation: 0,
                delta_hash: cs.delta_hash().unwrap(),
                parents,
            })
            .unwrap();
        write.commit().unwrap();
        id
    };

    // A changeset whose delta puts b, but whose root is its parent's: applying the delta to
    // the parent's root does not give its root.
    let put_b = vec![DeltaEntry {
        path: path("b"),
        op: DeltaOp::Put {
            kind: EntryKind::File,
            id: blob(b"b"),
        },
        renamed_from: None,
    }];
    let wrong_root = record(&changeset(vec![base], base_root, put_b), vec![base]);

    // A changeset whose object names a parent the store does not hold, recorded without it:
    // the parent is missing, and the metadata disagrees with the object.
    let empty_root = Tree::empty_root();
    objects.put(ObjectKind::Tree, &empty_root.encode()).unwrap();
    let nowhere = ChangesetId(hash_object(ObjectKind::Changeset, b"never committed"));
    let drifted = record(
        &changeset(vec![nowhere], empty_root.id(), Vec::new()),
        Vec::new(),
    );

    assert_problems(
        &repo,
        vec![
            VerifyProblem::RootMismatch(wrong_root),
            VerifyProblem::MissingParent {
                child: drifted,
                parent: nowhere,
            },
            VerifyProblem::IndexMismatch { seq: 2 },
        ],
    );
}
