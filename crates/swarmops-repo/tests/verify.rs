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
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_core::{
    Changeset, ChangesetId, DeltaEntry, DeltaOp, LeafKind, ObjectId, ObjectKind, Tree, TreeId,
    hash_object,
};
use swarmops_repo::{LocalRepo, MismatchCause, RepoError, StoreError, VerifyProblem, VerifyReport};
use swarmops_store::{
    ChangesetMeta, FsObjectStore, FsOps, Index, Journal, Layout, MetaLock, MetaStore, ObjectStore,
    RealFs, Record,
};
use swarmops_testkit::fs::{Call, RecordingFs, Watch, Watched, When};
use swarmops_testkit::repo::{blob, commit, delete, new_repo, path, put, put_as, signature};
use swarmops_testkit::store::lying_zstd;
use tempfile::TempDir;

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

/// An index mismatch of record `seq`, as `cause` says.
fn mismatch(seq: u64, cause: MismatchCause) -> VerifyProblem {
    VerifyProblem::IndexMismatch { seq, cause }
}

/// A changeset by the test signature, with no changes of its own unless `delta` says.
fn changeset(parents: Vec<ChangesetId>, root: TreeId, delta: Vec<DeltaEntry>) -> Changeset {
    Changeset {
        parents,
        root,
        delta,
        author: signature(),
        committer: signature(),
        message: b"written by hand".to_vec(),
        extra_headers: Vec::new(),
        metadata: Vec::new(),
    }
}

/// Stores `cs`'s object in the store at `root`, durably, and records it in a write
/// transaction of its own with `parents` and `delta_hash` (which may not be the object's);
/// returns its id.
fn record_by_hand(
    root: &Utf8Path,
    cs: &Changeset,
    parents: Vec<ChangesetId>,
    delta_hash: [u8; 32],
) -> ChangesetId {
    let objects = FsObjectStore::open(Layout::new(root), Box::new(RealFs));
    let id = ChangesetId(
        objects
            .put(ObjectKind::Changeset, &cs.encode().unwrap())
            .unwrap(),
    );
    objects.flush().unwrap();
    let meta = MetaStore::open(root).unwrap();
    let mut write = meta.write().unwrap();
    write
        .insert_changeset(ChangesetMeta {
            id,
            seq: 0,
            generation: 0,
            delta_hash,
            parents,
        })
        .unwrap();
    write.commit().unwrap();
    id
}

/// Swaps entries `a` and `b` (each `width` bytes) of file `path`.
fn swap_entries(path: &Utf8Path, width: usize, a: usize, b: usize) {
    let mut bytes = fs::read(path).unwrap();
    let first = bytes[a * width..(a + 1) * width].to_vec();
    let second = bytes[b * width..(b + 1) * width].to_vec();
    bytes[a * width..(a + 1) * width].copy_from_slice(&second);
    bytes[b * width..(b + 1) * width].copy_from_slice(&first);
    fs::write(path, bytes).unwrap();
}

/// The recs of the id view's first two entries, in the store at `root`.
fn first_two_recs(root: &Utf8Path) -> [u64; 2] {
    let index = Index::open(&Layout::new(root), Arc::new(RealFs)).unwrap();
    let view = index.ids_view();
    [view.rec_at(0).unwrap(), view.rec_at(1).unwrap()]
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
        vec![put_as("bin/tool", LeafKind::Exec, b"#!/bin/sh\n")],
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
        .target
        .id();
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

/// Rewrites the file of object `id` in the store at `root` as a zstd object whose header and
/// frame both record `size` bytes ([`lying_zstd`]): a lie that agrees with itself, so that the
/// header check passes and only decoding finds it out.
fn lie_about_its_size(root: &Utf8Path, id: ObjectId, size: u64) {
    let file = object_file(root, id);
    let mut bytes = fs::read(&file).unwrap();
    lying_zstd(&mut bytes, size);
    fs::write(&file, bytes).unwrap();
}

#[test]
fn verify_deep_lists_an_object_whose_zstd_frame_lies_about_its_size() {
    let (_dir, root, repo) = new_repo();
    let content = b"a line that compresses well\n".repeat(147); // 4,116 bytes
    commit(&repo, &[], vec![put("big.txt", &content)]);
    let lying = blob(&content).0;
    lie_about_its_size(&root, lying, 1 << 40);

    // Read whole, the file would claim a TiB: the deep check lists it as corrupt and goes on.
    assert_problems_when(&repo, true, vec![VerifyProblem::CorruptObject(lying)]);
    // A shallow check reads no blob.
    assert_problems_when(&repo, false, Vec::new());
}

/// A blob's file grown to 2 GiB, sparse behind its header (no disk; that much memory, were it
/// read), is longer than any object file: a deep check lists it as corrupt by its length, without
/// reading it, and goes on. The recorder fails a read of that file, should one come, so that no
/// run of this test reads gigabytes. A shallow check reads no blob.
#[test]
fn verify_deep_lists_an_object_file_of_huge_length_without_reading_it() {
    let (_dir, root, repo) = new_repo();
    let content = b"a blob whose file grows to 2 GiB\n";
    commit(&repo, &[], vec![put("huge.txt", content)]);
    drop(repo);
    let huge = blob(content).0;
    let file = object_file(&root, huge);
    (fs::OpenOptions::new().write(true).open(&file).unwrap())
        .set_len(2 << 30)
        .unwrap();

    let (recording, calls) = RecordingFs::new();
    let read = format!("read {}", file.file_name().unwrap());
    recording
        .failing()
        .set(Box::leak(read.clone().into_boxed_str()));
    let repo = LocalRepo::open_with_fs(&root, Arc::new(recording)).unwrap();
    assert_problems_when(&repo, true, vec![VerifyProblem::CorruptObject(huge)]);
    assert_problems_when(&repo, false, Vec::new());
    assert!(!calls().contains(&read), "the blob's file is never read");
}

/// A blob rewritten as a zstd object whose frame is of zstd's legacy v0.7 format, holding the
/// very content the blob's id names: the store writes no such frame and its zstd has no decoder
/// for one (storage spec §2 rule 1), so the deep check lists the blob as corrupt, where a zstd
/// built with the legacy decoders passed it as clean.
#[test]
fn verify_deep_lists_an_object_held_in_a_legacy_zstd_frame() {
    use swarmops_core::bytes::to_u64;
    use swarmops_testkit::store::{zstd_object, zstd_v07_frame};

    let (_dir, root, repo) = new_repo();
    let content = b"a blob that a frame of zstd's legacy v0.7 format holds in full\n";
    commit(&repo, &[], vec![put("legacy.txt", content)]);
    let legacy = blob(content).0;
    let file = object_file(&root, legacy);
    let mut bytes = fs::read(&file).unwrap();
    zstd_object(&mut bytes, to_u64(content.len()), &zstd_v07_frame(content));
    fs::write(&file, bytes).unwrap();

    assert_problems_when(&repo, true, vec![VerifyProblem::CorruptObject(legacy)]);
    // A shallow check reads no blob.
    assert_problems_when(&repo, false, Vec::new());
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
    let layout = Layout::new(&root);
    let bogus = ChangesetMeta {
        id: ChangesetId(hash_object(ObjectKind::Changeset, b"not in the journal")),
        seq: 0,
        generation: 1,
        delta_hash: [7; 32],
        parents: Vec::new(),
    };

    // Past the checkpoint (still at 0), the index's records are the journal's to vouch for:
    // one it does not, as a power loss can leave there, is counted by no handle, so verify
    // finds nothing; and the next writer cuts it and replays the journal's in its place.
    let replace_id = |at: usize| {
        let ids = layout.meta_path("idx/ids.col");
        let mut column = fs::read(&ids).unwrap();
        column[at * 32..at * 32 + 32].copy_from_slice(&bogus.id.0.0);
        fs::write(&ids, column).unwrap();
    };
    replace_id(1);
    assert_problems(&repo, Vec::new());
    assert_eq!(Index::open(&layout, Arc::new(RealFs)).unwrap().len(), 1);
    // Then move the checkpoint past both records: the writer's flush replays the record first.
    MetaStore::open(&root).unwrap().flush().unwrap();
    assert_eq!(Index::open(&layout, Arc::new(RealFs)).unwrap().len(), 2);
    assert_problems(&repo, Vec::new());

    // An index record that no journal frame holds: what a writer that went around the
    // journal would leave.
    {
        let _lock = MetaLock::acquire(&layout, Duration::from_secs(10)).unwrap();
        let mut index = Index::open(&layout, Arc::new(RealFs)).unwrap();
        assert_eq!(index.append(&bogus).unwrap(), 2);
        index.sync().unwrap();
    }
    assert_problems(&repo, vec![mismatch(2, MismatchCause::Journal)]);

    // Two records in each other's places.
    let ids = layout.meta_path("idx/ids.col");
    let mut column = fs::read(&ids).unwrap();
    column[..32].copy_from_slice(&c2.0.0);
    column[32..64].copy_from_slice(&c1.0.0);
    fs::write(&ids, column).unwrap();
    assert_problems(
        &repo,
        vec![
            mismatch(0, MismatchCause::Journal),
            mismatch(1, MismatchCause::Journal),
            mismatch(2, MismatchCause::Journal),
        ],
    );

    // The same record while changeset frames follow the checkpoint: past it, the index counts
    // only the records those frames vouch for, so no handle counts one that no frame holds,
    // past the journal's last changeset, and the next writer would cut it without a word. The
    // index files hold it all the same, which verify measures by their lengths before it reads
    // the journal.
    let (_dir, root, repo) = new_repo();
    let c1 = commit(&repo, &[], vec![put("f", b"1")]);
    commit(&repo, &[c1], vec![put("f", b"2")]);
    let layout = Layout::new(&root);
    {
        let _lock = MetaLock::acquire(&layout, Duration::from_secs(10)).unwrap();
        let mut index = Index::open(&layout, Arc::new(RealFs)).unwrap();
        assert_eq!(index.append(&bogus).unwrap(), 2);
        index.sync().unwrap();
    }
    let index = Index::open(&layout, Arc::new(RealFs)).unwrap();
    assert_eq!(index.len(), 2, "no handle counts it");
    assert_eq!(index.held().unwrap(), 3, "the files hold it");
    assert_problems(&repo, vec![mismatch(2, MismatchCause::Journal)]);
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
    assert_problems(&repo, vec![mismatch(1, MismatchCause::Journal)]);
}

/// A writer whose index sync fails cuts the index back to the checkpoint
/// ([`Index::truncate_to`]). A verify that opened the index before the cut, and counted records
/// the cut took, fails with an I/O error (`UnexpectedEof`) when it reads them, never with an
/// [`VerifyProblem::IndexMismatch`]; one that runs after the cut finds nothing wrong.
#[test]
fn verify_racing_the_cut_of_a_failed_index_sync_fails_with_an_io_error() {
    let (_dir, root, repo) = new_repo();
    let c1 = commit(&repo, &[], vec![put("f", b"1")]);
    let c2 = commit(&repo, &[c1], vec![put("f", b"2")]);
    let c3 = commit(&repo, &[c2], vec![put("f", b"3")]);
    // The checkpoint past records 0..=2; records 3 and 4 lie past it.
    MetaStore::open(&root).unwrap().flush().unwrap();
    let c4 = commit(&repo, &[c3], vec![put("f", b"4")]);
    commit(&repo, &[c4], vec![put("f", b"5")]);
    let layout = Layout::new(&root);
    let records = || Index::open(&layout, Arc::new(RealFs)).unwrap().len();
    assert_eq!(records(), 5);

    let (writer_fs, _calls) = RecordingFs::new();
    let fail = writer_fs.failing();
    let mut writer = MetaStore::open_with_fs(&root, Arc::new(writer_fs)).unwrap();
    writer.set_parallelism(1);
    fail.set("sync_data ids.col");
    // Verify opens the index (five records), then reads the ref files, `main` first, then the
    // journal: right before it reads `main`, the writer's flush fails its sync and cuts the
    // index back to the three records the checkpoint passed.
    let (verify_fs, _calls) = RecordingFs::new();
    let verify_fs = verify_fs.with_hook((
        "read main".to_owned(),
        When::Before,
        Box::new(move || {
            let cut = writer.flush();
            assert!(matches!(cut, Err(StoreError::Io(_))), "{cut:?}");
        }),
    ));
    let racing = LocalRepo::open_with_fs(&root, Arc::new(verify_fs)).unwrap();

    match racing.verify(false) {
        Err(RepoError::Store(StoreError::Io(e))) => {
            assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof, "{e}");
        }
        other => panic!("expected an I/O error from the stale index, got {other:?}"),
    }
    assert_eq!(records(), 3, "cut");
    assert_problems(&repo, Vec::new());
}

/// Records every call by name ([`Call::named`]), and cuts the last entry off `journal.off` right
/// after its `nth` measurement (`file_len journal.off`), as a writer whose listing of its frame
/// failed cuts the entry back off (storage spec §2 rule 3 (b)); 0 cuts nothing.
struct CutAfterMeasuring {
    nth: u64,
    measured: AtomicU64,
    calls: Mutex<Vec<String>>,
}

impl Watch for CutAfterMeasuring {
    fn before(&self, call: &Call<'_>) -> io::Result<()> {
        self.calls.lock().unwrap().push(call.named());
        Ok(())
    }

    fn after(&self, call: &Call<'_>) {
        if call.named() == "file_len journal.off"
            && self.measured.fetch_add(1, Ordering::SeqCst) + 1 == self.nth
        {
            let off = call.path();
            let listed = fs::metadata(off).unwrap().len();
            let file = fs::OpenOptions::new().write(true).open(off).unwrap();
            file.set_len(listed - 8).unwrap();
        }
    }
}

/// A writer may cut the last entry off `journal.off` while verify reads the journal: an entry
/// whose sync failed (storage spec §2 rule 3 (b)), its frame whole in `journal.log` and committed.
/// Whichever of verify's measurements of `journal.off` the cut follows, verify measures it again
/// before it reads on, and reports no damage. (Its read of the frames past the last listed one,
/// and its check of the checkpoint, used to report the journal corrupt.)
#[test]
fn verify_reads_journal_off_again_when_a_writer_cuts_an_entry_it_measured() {
    let (_dir, root, repo) = new_repo();
    let c1 = commit(&repo, &[], vec![put("f", b"1")]);
    let c2 = commit(&repo, &[c1], vec![put("f", b"2")]);
    repo.cas_ref("main", None, Some(c2), "test/agent").unwrap();
    let off = Layout::new(&root).journal_off();
    let entries = fs::read(&off).unwrap();
    let verify_cutting_after = |nth| {
        fs::write(&off, &entries).unwrap();
        let cutting = Arc::new(Watched(
            CutAfterMeasuring {
                nth,
                measured: AtomicU64::new(0),
                calls: Mutex::new(Vec::new()),
            },
            RealFs,
        ));
        let racing = LocalRepo::open_with_fs(&root, Arc::clone(&cutting) as Arc<dyn FsOps>);
        let report = racing.unwrap().verify(false).unwrap();
        let calls = std::mem::take(&mut *cutting.calls.lock().unwrap());
        (report, calls)
    };
    let measured = |calls: &[String]| -> Vec<usize> {
        (calls.iter().enumerate())
            .filter(|(_, call)| *call == "file_len journal.off")
            .map(|(at, _)| at)
            .collect()
    };
    let (report, calls) = verify_cutting_after(0);
    assert_eq!(report.problems, []);
    let measurements = measured(&calls).len();
    assert!(measurements >= 3, "{calls:?}");
    for nth in 1..=measurements {
        let (report, calls) = verify_cutting_after(u64::try_from(nth).unwrap());
        assert_eq!(report.problems, [], "the cut after measurement {nth}");
        let cut_after = measured(&calls)[nth - 1];
        assert_eq!(
            calls[cut_after + 1],
            "file_len journal.off",
            "measured again after measurement {nth}: {calls:?}"
        );
        if nth == 1 {
            let read_again = [
                "file_len journal.off",
                "file_len journal.off",
                "file_len journal.log",
            ];
            assert_eq!(calls[cut_after..cut_after + 3], read_again, "{calls:?}");
        }
    }
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
        recs.map(|seq| mismatch(seq, MismatchCause::SortedView))
            .to_vec(),
    );
}

#[test]
fn verify_checks_the_order_of_a_sorted_view() {
    let (_dir, root, repo) = new_repo();
    let c1 = commit(&repo, &[], vec![put("f", b"1")]);
    let c2 = commit(&repo, &[c1], vec![put("f", b"2")]);
    commit(&repo, &[c2], vec![put("f", b"3")]);
    assert_eq!(compact(&root), 3);
    // Swap the first two entries whole, key and rec: each still names its record by its key,
    // but they are out of order, which the entry now second shows.
    let [rec_first, _] = first_two_recs(&root);
    let layout = Layout::new(&root);
    swap_entries(&layout.meta_path("sorted/ids.key"), 32, 0, 1);
    swap_entries(&layout.meta_path("sorted/ids.rec"), 8, 0, 1);
    assert_problems(&repo, vec![mismatch(rec_first, MismatchCause::SortedView)]);
}

#[test]
fn verify_checks_each_record_is_named_once_in_a_sorted_view() {
    let (_dir, root, repo) = new_repo();
    let c1 = commit(&repo, &[], vec![put("f", b"1")]);
    let c2 = commit(&repo, &[c1], vec![put("f", b"2")]);
    commit(&repo, &[c2], vec![put("f", b"3")]);
    assert_eq!(compact(&root), 3);
    // The second entry names the first entry's record again: under its own key, so that is
    // wrong, and the record it named before is named by no entry at all.
    let [first, second] = first_two_recs(&root);
    let recs = Layout::new(&root).meta_path("sorted/ids.rec");
    let mut view = fs::read(&recs).unwrap();
    view[8..16].copy_from_slice(&first.to_le_bytes());
    fs::write(&recs, view).unwrap();
    assert_problems(
        &repo,
        vec![
            mismatch(first, MismatchCause::SortedView),
            mismatch(second, MismatchCause::SortedView),
        ],
    );
}

#[test]
fn verify_fails_on_a_ref_file_it_cannot_read() {
    // A ref file that is no ref file is damage verify reports (CorruptRef), a directory where
    // the file should be among them (not a regular file); one that cannot be read at all is an
    // error it cannot get past, which names the read that failed and the ref file, by its place
    // in the store.
    use swarmops_store::{FileOp, FsMethod};

    let (_dir, root, repo) = new_repo();
    let c1 = commit(&repo, &[], vec![put("f", b"1")]);
    repo.cas_ref("release/a", None, Some(c1), "test/agent")
        .unwrap();
    let b = Layout::new(&root).meta_path("refs/release/b");
    fs::create_dir(&b).unwrap();
    assert_problems(
        &repo,
        vec![VerifyProblem::CorruptRef("release/b".to_owned())],
    );
    fs::remove_dir(&b).unwrap();
    std::os::unix::fs::symlink("b", &b).unwrap(); // a loop: a read of it fails with ELOOP
    for deep in [true, false] {
        match repo.verify(deep) {
            Err(RepoError::Store(StoreError::Io(e))) => {
                assert_eq!(e.raw_os_error(), Some(40), "ELOOP: {e}");
                assert_eq!(
                    (e.op, e.file.as_deref()),
                    (
                        FileOp::Fs(FsMethod::Read),
                        Some(Utf8Path::new("meta/refs/release/b"))
                    ),
                    "{e}"
                );
            }
            other => panic!("deep: {deep}: expected an I/O error, got {other:?}"),
        }
    }
}

/// A store for the damaged-metadata tests: two changesets, `main` on the second, a ref
/// `release/gone` that names no changeset, and the first blob corrupt.
fn store_with_refs_and_a_corrupt_blob() -> (TempDir, Utf8PathBuf, LocalRepo) {
    let (dir, root, repo) = new_repo();
    let c1 = commit(&repo, &[], vec![put("f", b"1")]);
    let c2 = commit(&repo, &[c1], vec![put("g", b"2")]);
    repo.cas_ref("main", None, Some(c2), "test/agent").unwrap();
    let release = Layout::new(&root).meta_path("refs/release");
    fs::create_dir_all(&release).unwrap();
    let nowhere = ChangesetId(hash_object(ObjectKind::Changeset, b"never committed"));
    fs::write(release.join("gone"), format!("{nowhere}\n1\n")).unwrap();
    let file = object_file(&root, blob(b"1").0);
    let mut bytes = fs::read(&file).unwrap();
    *bytes.last_mut().unwrap() ^= 0x01;
    fs::write(&file, bytes).unwrap();
    (dir, root, repo)
}

#[test]
fn verify_reports_damaged_metadata_files_and_checks_on() {
    // Whatever metadata file is damaged, verify reports it and checks the rest: it still finds
    // the corrupt blob, and the dangling ref (not main, whose changeset it knows).
    let meta = |what: &str| VerifyProblem::MetaCorrupt(what.to_owned());
    let dangling = VerifyProblem::DanglingRef("release/gone".to_owned());
    let corrupt = VerifyProblem::CorruptObject(blob(b"1").0);
    let journal_gone = |file: &str| {
        format!(
            "meta/{file} is missing: the journal is not checked, and the index is checked as it is"
        )
    };
    let index_damage = "the changeset index does not open (a sorted view file ends in a partial \
                        entry): it is not checked";
    let cases: [(&str, String); 4] = [
        (
            "journal.ckpt",
            "meta/journal.ckpt does not hold a u64le offset: the records the checkpoint passed \
             are not checked against the index"
                .to_owned(),
        ),
        ("journal.log", journal_gone("journal.log")),
        ("journal.off", journal_gone("journal.off")),
        (
            "sorted/ids.rec",
            format!("{index_damage}, and the journal's changesets are"),
        ),
    ];
    for (damage, what) in cases {
        let (_dir, root, repo) = store_with_refs_and_a_corrupt_blob();
        damage_meta(&root, damage);
        let report = repo.verify(true).unwrap();
        assert_eq!(
            report.problems,
            [meta(&what), dangling.clone(), corrupt.clone()],
            "{damage}"
        );
        assert_eq!(report.changesets_checked, 2, "{damage}");
    }

    // With neither the journal nor the index, no changeset is known: no ref's target can be
    // checked, which the report says, rather than calling every ref dangling.
    let (_dir, root, repo) = store_with_refs_and_a_corrupt_blob();
    damage_meta(&root, "journal.log");
    damage_meta(&root, "sorted/ids.rec");
    let report = repo.verify(true).unwrap();
    assert_eq!(
        report.problems,
        [
            meta("meta/journal.log is missing: the journal is not checked"),
            meta(&format!(
                "{index_damage}; with no journal either, no changeset is known, so neither \
                 changesets nor the targets of refs are checked"
            )),
            corrupt,
        ]
    );
    assert_eq!(report.changesets_checked, 0);
    assert_eq!(
        report.problems[0].to_string(),
        "damaged metadata: meta/journal.log is missing: the journal is not checked"
    );
}

#[test]
fn verify_reports_a_checkpoint_that_is_not_a_frame_boundary() {
    // Every snapshot and write transaction reads the frames past the checkpoint: a checkpoint
    // inside a frame, or past the log's end, fails them all with JournalCorrupt
    // (E_STORE_CORRUPT), so verify must report it (it reported nothing). One at a frame
    // boundary (0, a frame's start, the end of the last frame) is sound, and nothing is reported.
    let (_dir, root, repo) = store_with_refs_and_a_corrupt_blob();
    let dangling = VerifyProblem::DanglingRef("release/gone".to_owned());
    let corrupt = VerifyProblem::CorruptObject(blob(b"1").0);
    let layout = Layout::new(&root);
    let journal = Journal::open(&layout, Arc::new(RealFs)).unwrap();
    let frames = journal.frames_from(0, usize::MAX).unwrap();
    assert_eq!(frames.len(), 3, "c1, c2 and the ref move");
    let end = fs::metadata(layout.journal_log()).unwrap().len();
    for boundary in [0, frames[1].offset, frames[2].offset, end] {
        journal.set_ckpt(boundary).unwrap();
        MetaStore::open(&root).unwrap().read().unwrap();
        let report = repo.verify(true).unwrap();
        assert_eq!(
            report.problems,
            [dangling.clone(), corrupt.clone()],
            "ckpt {boundary}: no false report"
        );
        assert_eq!(report.changesets_checked, 2, "ckpt {boundary}");
    }
    for bogus in [frames[1].offset - 1, frames[1].offset + 1, end + 8] {
        journal.set_ckpt(bogus).unwrap();
        let read = MetaStore::open(&root).unwrap().read().map(|_| ());
        assert!(
            matches!(read, Err(StoreError::JournalCorrupt { offset }) if offset == bogus),
            "ckpt {bogus}: {read:?}"
        );
        let report = repo.verify(true).unwrap();
        assert_eq!(
            report.problems,
            [
                VerifyProblem::MetaCorrupt(format!(
                    "meta/journal.ckpt holds {bogus}, which is not a frame boundary: reads and \
                     writes fail on it, and the records the checkpoint passed are not checked \
                     against the index"
                )),
                dangling.clone(),
                corrupt.clone()
            ],
            "ckpt {bogus}"
        );
        assert_eq!(report.changesets_checked, 2, "ckpt {bogus}");
    }
}

#[test]
fn verify_reports_a_lock_file_that_is_not_a_regular_file() {
    // Every write refuses a `meta/lock` that is not a regular file, as damage (E_STORE_CORRUPT,
    // whose hint is to run verify), so verify must name it too; it only looks at the file, without
    // opening it or following a symlink, and checks the rest as before. A lock file that is
    // missing is no damage: the next write makes it.
    use std::os::unix::fs::symlink;
    use std::process::Command;

    let lock_text = "meta/lock is not a regular file: every write fails on it until it is \
                     removed, and the next write makes a new one";
    let dangling = VerifyProblem::DanglingRef("release/gone".to_owned());
    let corrupt = VerifyProblem::CorruptObject(blob(b"1").0);
    for what in ["a FIFO", "a symlink to a regular file", "a directory"] {
        let (_dir, root, repo) = store_with_refs_and_a_corrupt_blob();
        let lock = Layout::new(&root).meta_lock();
        fs::remove_file(&lock).unwrap();
        match what {
            "a FIFO" => {
                let made = Command::new("mkfifo").arg(&lock).status();
                assert!(made.expect("run mkfifo").success(), "mkfifo");
            }
            "a symlink to a regular file" => {
                let regular = root.join("regular");
                fs::write(&regular, b"").unwrap();
                symlink(regular, &lock).unwrap();
            }
            _ => fs::create_dir(&lock).unwrap(),
        }
        let report = repo.verify(true).unwrap();
        assert_eq!(
            report.problems,
            [
                VerifyProblem::MetaCorrupt(lock_text.to_owned()),
                dangling.clone(),
                corrupt.clone()
            ],
            "{what}"
        );
        assert_eq!(report.changesets_checked, 2, "{what}");
    }

    let (_dir, root, repo) = store_with_refs_and_a_corrupt_blob();
    fs::remove_file(Layout::new(&root).meta_lock()).unwrap();
    let report = repo.verify(true).unwrap();
    assert_eq!(report.problems, [dangling, corrupt], "a missing lock file");
}

/// Damages metadata file `file` of the store at `root`: `journal.ckpt` malformed, a journal
/// file removed, or a sorted view (compacted first) cut inside an entry.
fn damage_meta(root: &Utf8Path, file: &str) {
    let path = Layout::new(root).meta_path(file);
    match file {
        "journal.ckpt" => fs::write(&path, b"bad").unwrap(),
        "sorted/ids.rec" => {
            compact(root);
            let bytes = fs::read(&path).unwrap();
            fs::write(&path, &bytes[..bytes.len() - 1]).unwrap();
        }
        _ => fs::remove_file(&path).unwrap(),
    }
}

#[test]
fn verify_problem_messages_name_the_cause_and_the_remedy() {
    for (cause, text) in [
        (
            MismatchCause::Journal,
            "changeset record 3: the index disagrees with the journal; rebuild the index from \
             the journal",
        ),
        (
            MismatchCause::SortedView,
            "changeset record 3: a sorted view disagrees with the index columns; rebuild the \
             sorted views from the columns",
        ),
        (
            MismatchCause::Object,
            "changeset record 3: its metadata record (the journal's, or the index's where the \
             journal cannot be read) disagrees with its object (parents, delta hash) or with its \
             parents' generations; derive the record from the object again",
        ),
    ] {
        assert_eq!(mismatch(3, cause).to_string(), text, "{cause:?}");
    }
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
fn verify_reports_a_listed_frame_cut_short() {
    let (_dir, root, repo) = new_repo();
    let c1 = commit(&repo, &[], vec![put("f", b"1")]);
    commit(&repo, &[c1], vec![put("f", b"2")]);
    let layout = Layout::new(&root);
    let frames = Journal::open(&layout, Arc::new(RealFs))
        .unwrap()
        .frames_from(0, 10)
        .unwrap();
    assert_eq!(frames.len(), 2);

    // journal.off lists the last frame, so it was whole and durable before that: journal.log
    // ending inside it (or right at its start) is damage, which the journal's readers and its
    // recovery report too.
    let log = layout.journal_log();
    let end = fs::metadata(&log).unwrap().len();
    for cut in [end - 1, frames[1].offset + 3, frames[1].offset] {
        fs::OpenOptions::new()
            .write(true)
            .open(&log)
            .unwrap()
            .set_len(cut)
            .unwrap();
        assert_problems(
            &repo,
            vec![VerifyProblem::JournalCorrupt {
                offset: frames[1].offset,
            }],
        );
    }
}

#[test]
fn verify_detects_missing_parents_root_mismatches_and_metadata_drift() {
    let (_dir, root, repo) = new_repo();
    let base = commit(&repo, &[], vec![put("a", b"a")]);
    let base_root = repo.changeset(base).unwrap().unwrap().changeset.root;

    // A changeset whose delta puts b, but whose root is its parent's: applying the delta to
    // the parent's root does not give its root.
    let put_b = vec![DeltaEntry {
        path: path("b"),
        op: DeltaOp::Put {
            kind: LeafKind::File,
            id: blob(b"b"),
        },
        renamed_from: None,
    }];
    let wrong = changeset(vec![base], base_root, put_b);
    let wrong_root = record_by_hand(&root, &wrong, vec![base], wrong.delta_hash().unwrap());

    // A changeset whose object names a parent the store does not hold, recorded without it:
    // the parent is missing, and the record disagrees with the object.
    let empty_root = Tree::empty_root();
    FsObjectStore::open(Layout::new(&root), Box::new(RealFs))
        .put(ObjectKind::Tree, &empty_root.encode())
        .unwrap();
    let nowhere = ChangesetId(hash_object(ObjectKind::Changeset, b"never committed"));
    let orphan = changeset(vec![nowhere], empty_root.id(), Vec::new());
    let drifted = record_by_hand(&root, &orphan, Vec::new(), orphan.delta_hash().unwrap());

    assert_problems(
        &repo,
        vec![
            VerifyProblem::RootMismatch(wrong_root),
            VerifyProblem::MissingParent {
                child: drifted,
                parent: nowhere,
            },
            mismatch(2, MismatchCause::Object),
        ],
    );
}

#[test]
fn verify_detects_generation_and_delta_hash_drift() {
    // Each field on its own: a delta hash the object does not have, a generation the index
    // holds wrong, and a generation the journal recorded wrong.
    let (_dir, root, repo) = new_repo();
    let base = commit(&repo, &[], vec![put("a", b"a")]);
    let base_root = repo.changeset(base).unwrap().unwrap().changeset.root;
    let empty = changeset(vec![base], base_root, Vec::new());
    record_by_hand(&root, &empty, vec![base], [9; 32]);
    assert_problems(&repo, vec![mismatch(1, MismatchCause::Object)]);

    let (_dir, root, repo) = new_repo();
    let c1 = commit(&repo, &[], vec![put("f", b"1")]);
    commit(&repo, &[c1], vec![put("f", b"2")]);
    // Below the checkpoint, where the index holds its records for good (past it, the journal
    // vouches for them: `verify_detects_index_journal_mismatch`).
    MetaStore::open(&root).unwrap().flush().unwrap();
    let generations = Layout::new(&root).meta_path("idx/gen.col");
    let mut column = fs::read(&generations).unwrap();
    column[8..16].copy_from_slice(&9_u64.to_le_bytes());
    fs::write(&generations, column).unwrap();
    assert_problems(&repo, vec![mismatch(1, MismatchCause::Journal)]);

    // A frame a writer appended (and died before it was materialized) with a generation
    // that is not 1 + its parent's.
    let (_dir, root, repo) = new_repo();
    let base = commit(&repo, &[], vec![put("a", b"a")]);
    let base_root = repo.changeset(base).unwrap().unwrap().changeset.root;
    let empty = changeset(vec![base], base_root, Vec::new());
    let objects = FsObjectStore::open(Layout::new(&root), Box::new(RealFs));
    let id = ChangesetId(
        objects
            .put(ObjectKind::Changeset, &empty.encode().unwrap())
            .unwrap(),
    );
    objects.flush().unwrap();
    let layout = Layout::new(&root);
    {
        let _lock = MetaLock::acquire(&layout, Duration::from_secs(10)).unwrap();
        let journal = Journal::open(&layout, Arc::new(RealFs)).unwrap();
        journal.truncate_torn_tail().unwrap();
        journal
            .append_frame(&[Record::Changeset {
                id,
                generation: 7,
                delta_hash: empty.delta_hash().unwrap(),
                parents: vec![base],
            }])
            .unwrap();
    }
    assert_problems(&repo, vec![mismatch(1, MismatchCause::Object)]);
}

/// The external id index (storage spec §7), checked after the changesets: an entry whose file is
/// damaged, one that names a changeset the store does not hold, and one of system `git` keyed by
/// another oid than its changeset's `git.oid` are each a problem of their own, listed by system
/// and key; a sound entry, one whose changeset has no `git.oid` (a native one), one of another
/// system, and a name under the index that is no entry's are none.
#[test]
fn verify_reports_external_ids_that_are_damaged_dangling_or_mismatched() {
    use swarmops_store::external_ids::lower_hex;
    use swarmops_store::{ExternalEntry, SYSTEM_GIT};
    use swarmops_testkit::repo::request;

    let (_dir, root, repo) = new_repo();
    let oid = |n: u8| vec![n; 20];
    let mut imported = request(&[], vec![put("f", b"1")], "imported from git");
    imported.metadata = vec![("git.oid".to_owned(), lower_hex(&oid(0xaa)).into_bytes())];
    let imported = repo.commit(imported).unwrap().id;
    let native = commit(&repo, &[imported], vec![put("f", b"2")]);
    let missing = ChangesetId(hash_object(ObjectKind::Changeset, b"in no store"));
    let entry = |changeset| ExternalEntry {
        changeset,
        faithful: true,
    };
    let sound = [(oid(0xaa), entry(imported)), (oid(0xee), entry(native))];
    assert_eq!(
        repo.record_external_ids(SYSTEM_GIT, &sound)
            .unwrap()
            .written,
        2
    );
    assert_eq!(
        repo.external_id(SYSTEM_GIT, &oid(0xaa)).unwrap(),
        Some(entry(imported))
    );
    assert_eq!(repo.external_id(SYSTEM_GIT, &oid(0x11)).unwrap(), None);
    repo.record_external_ids("swarmops-0a1b", &[(vec![0xbb; 32], entry(native))])
        .unwrap();
    assert_problems(&repo, Vec::new());

    // Keyed by another oid than its changeset's, naming a changeset in no store, and one whose
    // CRC is then damaged; and stray names beside the entries.
    let unsound = [
        (oid(0xbb), entry(imported)),
        (oid(0xcc), entry(missing)),
        (oid(0xdd), entry(native)),
    ];
    assert_eq!(
        repo.record_external_ids(SYSTEM_GIT, &unsound)
            .unwrap()
            .written,
        3
    );
    let layout = Layout::new(&root);
    let damaged = layout.external_id_path(SYSTEM_GIT, &oid(0xdd));
    let mut bytes = fs::read(&damaged).unwrap();
    bytes[43] ^= 0x01;
    fs::write(&damaged, bytes).unwrap();
    let git = layout.external_ids_dir().join("git");
    fs::write(git.join("aa/stray"), b"junk").unwrap();
    fs::create_dir(git.join("zz")).unwrap();
    fs::write(layout.external_ids_dir().join("stray"), b"junk").unwrap();

    let key = |n: u8| lower_hex(&oid(n));
    let want = vec![
        VerifyProblem::ExternalIdMismatch {
            system: "git".to_owned(),
            key: key(0xbb),
            changeset: imported,
        },
        VerifyProblem::ExternalIdDangling {
            system: "git".to_owned(),
            key: key(0xcc),
            changeset: missing,
        },
        VerifyProblem::ExternalIdCorrupt {
            system: "git".to_owned(),
            key: key(0xdd),
        },
    ];
    let report = assert_problems(&repo, want.clone());
    assert_eq!(
        report.problems, want,
        "after the changesets', by system and key"
    );
    let lines: Vec<String> = want.iter().map(ToString::to_string).collect();
    assert_eq!(
        lines,
        [
            format!(
                "external id {} of system git names changeset {imported}, whose git.oid is \
                 another oid",
                key(0xbb)
            ),
            format!(
                "external id {} of system git names changeset {missing}, which the store does \
                 not hold",
                key(0xcc)
            ),
            format!(
                "the entry of external id {} of system git is corrupt",
                key(0xdd)
            ),
        ]
    );
}

/// What a deep verify finds in the derived store (storage spec §5): every value it holds of the
/// two built-in derivations, checked against the changesets' objects.
mod derived_values {
    use swarmops_core::RepoPath;
    use swarmops_repo::{Derivation, FirstParentSkipList, LastModified, LastTouch, PathKey};
    use swarmops_store::DerivedSlot;
    use swarmops_testkit::fs::NoSyncFs;

    use super::*;

    /// A history whose derived values the tests check, committed through a filesystem that never
    /// syncs (`NoSyncFs`): `c0` puts `a` and `d/x`, `c1` on it `b`, `c2` on that `a`; `s1`, a side
    /// branch off `c0`, puts `a` and `d/y`; `merge` of `c2` and `s1` puts `e`; and on it, `quiet`
    /// (the merge first), 70 changesets that put only `q`.
    struct History {
        _dir: TempDir,
        root: Utf8PathBuf,
        repo: LocalRepo,
        c0: ChangesetId,
        c1: ChangesetId,
        c2: ChangesetId,
        s1: ChangesetId,
        merge: ChangesetId,
        quiet: Vec<ChangesetId>,
    }

    impl History {
        /// The history, with the values the engine derives for it: every changeset's skip list,
        /// and the last touches of paths that `path_history` chases from the last changeset and
        /// from the side branch's, among them the memos of a walk over the 70 that leave `a`
        /// alone.
        fn derived() -> History {
            let dir = tempfile::tempdir().unwrap();
            let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
            let repo = LocalRepo::create_with_fs(&root, "test", Arc::new(NoSyncFs)).unwrap();
            let c0 = commit(&repo, &[], vec![put("a", b"0"), put("d/x", b"0")]);
            let c1 = commit(&repo, &[c0], vec![put("b", b"1")]);
            let c2 = commit(&repo, &[c1], vec![put("a", b"2")]);
            let s1 = commit(&repo, &[c0], vec![put("a", b"s"), put("d/y", b"s")]);
            let merge = commit(&repo, &[c2, s1], vec![put("e", b"m")]);
            let mut quiet = vec![merge];
            for i in 0..70_u32 {
                let parent = *quiet.last().unwrap();
                quiet.push(commit(&repo, &[parent], vec![put("q", &i.to_le_bytes())]));
            }
            let head = *quiet.last().unwrap();
            for &cs in [c0, c1, c2, s1].iter().chain(&quiet) {
                repo.derived::<FirstParentSkipList>(cs, &()).unwrap();
            }
            let read = repo.snapshot().unwrap();
            for p in ["a", "b", "d", "d/x", "e", "q", "never"] {
                read.path_history(head, &path(p), 100).unwrap();
                read.path_history(s1, &path(p), 100).unwrap();
            }
            assert_eq!(read.last_modified(head, &path("a")).unwrap(), Some(c2));
            assert_eq!(read.last_modified(s1, &path("b")).unwrap(), None);
            drop(read);
            History {
                _dir: dir,
                root,
                repo,
                c0,
                c1,
                c2,
                s1,
                merge,
                quiet,
            }
        }

        /// Places `value` in `slot` in place of the file there, if any, as a put writes it,
        /// trailer and all.
        fn place(&self, slot: &DerivedSlot<'_>, value: &[u8]) {
            let file = Layout::new(&self.root).derived_path(slot);
            if file.exists() {
                fs::remove_file(&file).unwrap();
            }
            assert!(
                self.repo.derived_store().put(slot, value).unwrap(),
                "{slot:?}"
            );
        }
    }

    /// The key of `p`'s values of `last_modified`: its path's postcard bytes.
    fn path_key(p: &str) -> Vec<u8> {
        postcard::to_stdvec(&PathKey(path(p))).unwrap()
    }

    /// The slot of `cs`'s value of `last_modified` under `key`.
    fn touch_slot(cs: ChangesetId, key: &[u8]) -> DerivedSlot<'_> {
        DerivedSlot {
            name: LastModified::NAME,
            version: LastModified::VERSION,
            subject: cs.0,
            key,
        }
    }

    /// The slot of `cs`'s skip list.
    fn list_slot(cs: ChangesetId) -> DerivedSlot<'static> {
        DerivedSlot {
            name: FirstParentSkipList::NAME,
            version: FirstParentSkipList::VERSION,
            subject: cs.0,
            key: &[],
        }
    }

    /// A value of `last_modified`, as postcard writes it.
    fn touch(cs: Option<ChangesetId>) -> Vec<u8> {
        postcard::to_stdvec(&LastTouch(cs)).unwrap()
    }

    /// A skip list of `ids`, as postcard writes it: their number, then each one's 32 bytes.
    fn list(ids: &[ChangesetId]) -> Vec<u8> {
        let mut bytes = vec![u8::try_from(ids.len()).unwrap()];
        for id in ids {
            bytes.extend(id.0.0);
        }
        bytes
    }

    /// The values the engine derives are clean, deep and shallow. Each value placed over them that
    /// is not the one the objects give, whole and with its trailer right, is a `DerivedMismatch`
    /// of a deep check (by derivation, changeset and key), which a shallow one does not read: a
    /// last touch from a merge's second parent, a none where a changeset touched the path, an
    /// older toucher in the memo the walk from the last changeset left, a changeset the store
    /// does not hold, and bytes that are no value; a skip list with an entry on a side branch at
    /// the right distance from the root (entry 0, entry 1), cut short, with an entry on the chain
    /// at the wrong distance, an entry too many, empty for a changeset with a parent, naming a
    /// changeset the store does not hold, and bytes that are no list. Values placed where the
    /// engine stored none that are the ones the objects give are no problem, a last touch of none
    /// on the side branch among them; nor are a torn file, a value about a changeset the store
    /// does not hold, a key that is no path, and a skip list under a key: no reader takes them as
    /// valid. Verify removes none of the files.
    #[test]
    fn verify_deep_checks_derived_values_against_the_objects() {
        let h = History::derived();
        let quiet = &h.quiet;
        let head = *quiet.last().unwrap();
        assert_problems(&h.repo, Vec::new());
        let missing = ChangesetId(hash_object(ObjectKind::Changeset, b"in no store"));
        let keys = ["a", "b", "d", "e", "q"].map(path_key);
        let [key_a, key_b, key_d, key_e, key_q] = keys.each_ref().map(Vec::as_slice);
        let layout = Layout::new(&h.root);
        assert!(
            layout.derived_path(&touch_slot(head, key_a)).exists(),
            "a memo"
        );
        let wrong: Vec<(DerivedSlot<'_>, Vec<u8>)> = vec![
            (touch_slot(h.merge, key_a), touch(Some(h.s1))),
            (touch_slot(h.c1, key_a), touch(None)),
            (touch_slot(head, key_a), touch(Some(h.c0))),
            (touch_slot(h.c2, key_b), touch(Some(missing))),
            (touch_slot(h.c0, key_d), vec![9, 9]),
            (list_slot(h.c2), list(&[h.s1, h.c0])),
            (list_slot(h.merge), list(&[h.c2, h.s1])),
            (list_slot(quiet[10]), list(&[quiet[9], quiet[8], quiet[6]])),
            (
                list_slot(quiet[20]),
                list(&[quiet[19], quiet[18], quiet[16], quiet[12], quiet[5]]),
            ),
            (list_slot(h.c1), list(&[h.c0, h.c0])),
            (list_slot(quiet[1]), list(&[])),
            (list_slot(quiet[2]), list(&[quiet[1], missing, h.c1])),
            (list_slot(quiet[3]), vec![2, 0]),
        ];
        for (slot, value) in &wrong {
            h.place(slot, value);
        }
        h.place(&touch_slot(h.s1, key_b), &touch(None));
        h.place(&touch_slot(h.s1, key_d), &touch(Some(h.s1)));
        h.place(&touch_slot(quiet[30], key_a), &touch(Some(h.c2)));
        h.place(&touch_slot(quiet[30], key_q), &touch(Some(quiet[30])));
        h.place(&touch_slot(quiet[40], key_e), &touch(Some(h.merge)));
        let torn = layout.derived_path(&list_slot(quiet[4]));
        let mut bytes = fs::read(&torn).unwrap();
        *bytes.last_mut().unwrap() ^= 0x01;
        fs::write(&torn, bytes).unwrap();
        h.place(&list_slot(missing), &list(&[h.c0]));
        let no_path: &[u8] = &[4, b'a', b'/', b'/', b'b'];
        assert!(RepoPath::new("a//b").is_err());
        h.place(&touch_slot(h.c2, no_path), &touch(Some(h.c0)));
        let keyed_list = DerivedSlot {
            key: b"k",
            ..list_slot(h.c2)
        };
        h.place(&keyed_list, &list(&[h.c0]));

        let mut want: Vec<(&str, ChangesetId, Vec<u8>)> = (wrong.iter())
            .map(|(slot, _)| (slot.name, ChangesetId(slot.subject), slot.key.to_vec()))
            .collect();
        want.sort_unstable();
        let want: Vec<VerifyProblem> = (want.into_iter())
            .map(|(name, changeset, key)| VerifyProblem::DerivedMismatch {
                name: name.to_owned(),
                changeset,
                key,
            })
            .collect();
        assert_eq!(
            h.repo.verify(true).unwrap().problems,
            want,
            "in their order"
        );
        assert_problems_when(&h.repo, false, Vec::new());
        for (slot, _) in &wrong {
            assert!(layout.derived_path(slot).exists(), "verify removes nothing");
        }
        assert!(torn.exists(), "nor a torn file");
    }

    /// A `DerivedMismatch` line names the derivation, the changeset and the key (a path for
    /// `last_modified`, hex for another key, nothing for none), and the remedy.
    #[test]
    fn a_derived_mismatch_line_names_the_value_and_the_remedy() {
        let cs = ChangesetId(hash_object(ObjectKind::Changeset, b"a changeset"));
        let line = |name: &str, key: &[u8]| {
            VerifyProblem::DerivedMismatch {
                name: name.to_owned(),
                changeset: cs,
                key: key.to_vec(),
            }
            .to_string()
        };
        assert_eq!(
            line(LastModified::NAME, &path_key("src/lib.rs")),
            format!(
                "the derived value last_modified of changeset {cs} under path src/lib.rs is not \
                 the one the store's objects give; remove its file under \
                 meta/derived/last_modified/, or all of meta/derived/, to have it derived again"
            )
        );
        assert_eq!(
            line(FirstParentSkipList::NAME, &[]),
            format!(
                "the derived value first_parent_skip_list of changeset {cs} is not the one the \
                 store's objects give; remove its file under meta/derived/first_parent_skip_list/, \
                 or all of meta/derived/, to have it derived again"
            )
        );
        assert!(line("other", &[0xab, 0x01]).contains(" under key ab01 "));
    }
}
