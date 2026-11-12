//! Tags with git's tag object (the annotated tags design, slice T-3): an annotated tag is its
//! ref, the object's bytes as a blob, and a Tag fact naming both in one frame; a lightweight one
//! is its ref alone. A tag never moves, a create run again is applied already, a FORMAT 2 store
//! takes lightweight tags only, and a power loss leaves a tag whole or absent.
//!
//! Every test owns a fresh store (`tempfile::tempdir()`).

use std::fs;
use std::path::Path;
use std::sync::Arc;

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_core::{BlobId, ChangesetId, ObjectKind, hash_object};
use swarmops_repo::{
    Applied, GcRequest, LocalRepo, OpId, OpRecord, RepoError, StoreError, StoreFormat, TAG_BATCH,
    TagKind, TagMade, TagObject, TagRead,
};
use swarmops_store::{FsOps, Layout};
use swarmops_testkit::power::{Chooser, ImageRecordingFs, Keep, RecordedCall, Seeded, Timeline};
use swarmops_testkit::repo::{commit, new_repo_as, put};
use tempfile::TempDir;

/// Who makes the tags.
const ACTOR: &str = "test/tagger";

/// The tag every test makes.
const V1: &str = "tags/v1";

/// The feature a Tag fact needs.
const FEATURE: &str = "tag_object";

/// A tag object as git writes one: its head, a blank line, the message.
fn tag_bytes(message: &str) -> Vec<u8> {
    format!(
        "object 0123456789abcdef0123456789abcdef01234567\ntype commit\ntag v1\n\
         tagger A U Thor <a@example.com> 1700000000 +0000\n\n{message}\n"
    )
    .into_bytes()
}

/// The annotated tag object of `message`.
fn annotated(message: &str) -> TagObject {
    TagObject::Annotated(tag_bytes(message))
}

/// The id of the blob that holds `bytes`.
fn blob_of(bytes: &[u8]) -> BlobId {
    BlobId::new(hash_object(ObjectKind::Blob, bytes))
}

/// A FORMAT 3 store, made through `fs`: one of FORMAT 2 with two changesets, upgraded (no `init`
/// makes FORMAT 3 yet), and the two changesets.
fn v3_on(root: &Utf8Path, fs: Arc<dyn FsOps>) -> (LocalRepo, ChangesetId, ChangesetId) {
    let repo = LocalRepo::create_with_fs_as(root, "tags", StoreFormat::V2, Arc::clone(&fs))
        .expect("create the store");
    let t1 = commit(&repo, &[], vec![put("a.txt", b"a")]);
    let t2 = commit(&repo, &[t1], vec![put("b.txt", b"b")]);
    assert!(repo.upgrade().expect("upgrade").upgraded);
    drop(repo);
    let repo = LocalRepo::open_with_fs(root, fs).expect("open the store");
    assert_eq!(repo.format(), StoreFormat::V3);
    (repo, t1, t2)
}

/// [`v3_on`] on the real filesystem, in a temp dir of its own.
fn v3_repo() -> (TempDir, Utf8PathBuf, LocalRepo, ChangesetId, ChangesetId) {
    let dir = tempfile::tempdir().expect("a temp dir");
    let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).expect("UTF-8");
    let fs = Arc::new(swarmops_store::RealFs::new());
    let (repo, t1, t2) = v3_on(&root, fs);
    (dir, root, repo, t1, t2)
}

/// Tag `name` as the store reads it now.
fn read(repo: &LocalRepo, name: &str) -> Option<TagRead> {
    repo.snapshot().expect("a snapshot").tag(name).expect("tag")
}

/// What the tag `name` carries: its target and kind.
fn carried(repo: &LocalRepo, name: &str) -> Carried {
    read(repo, name).map(|tag| (tag.target, tag.kind))
}

/// What a tag carries: its target and kind; `None` for no tag.
type Carried = Option<(ChangesetId, TagKind)>;

/// Tag `v1` of the store at `root`, opened again, and the bytes of the blob `bytes` hash to if
/// the tag is there.
fn reopened(root: &Utf8Path, bytes: &[u8]) -> Result<(Carried, Option<Vec<u8>>), RepoError> {
    let repo = LocalRepo::open(root)?;
    let snapshot = repo.snapshot()?;
    let tag = snapshot.tag(V1)?.map(|tag| (tag.target, tag.kind));
    let held = match tag {
        Some(_) => Some(snapshot.blob(blob_of(bytes))?),
        None => None,
    };
    Ok((tag, held))
}

/// The bytes of the FORMAT 3 journal of the store at `root`.
fn journal_len(root: &Utf8Path) -> u64 {
    let log = Layout::with_format(root, StoreFormat::V3).journal_log();
    fs::metadata(log).expect("the journal").len()
}

/// Whether the feature list of the store at `root` names `tag_object`.
fn lists_feature(root: &Utf8Path) -> bool {
    let file = Layout::with_format(root, StoreFormat::V3).features_file();
    fs::read_to_string(file).is_ok_and(|list| list.lines().any(|name| name == FEATURE))
}

/// Deletes tag `name`, which names `target`.
fn delete(repo: &LocalRepo, name: &str, target: ChangesetId) {
    repo.cas_ref(name, Some(target), None, ACTOR)
        .expect("delete the tag");
}

/// Whether `made` is the immutable tag `name`'s refusal.
fn immutable(made: &Result<TagMade, RepoError>, name: &str) -> bool {
    matches!(made, Err(RepoError::Store(StoreError::TagImmutable(n))) if n == name)
}

#[test]
fn an_annotated_tag_reads_back_its_bytes() {
    let (_dir, _root, repo, t1, _t2) = v3_repo();
    let bytes = tag_bytes("release 1");

    // 1. Made: a new tag, its blob the object's bytes.
    let made = repo
        .tag_create(V1, t1, annotated("release 1"), ACTOR)
        .unwrap();
    assert_eq!(made.applied, Applied::New);
    let tag = read(&repo, V1).expect("the tag");
    assert_eq!(tag.name, V1);
    assert_eq!(tag.target, t1);
    assert_eq!(
        tag.kind,
        TagKind::Annotated {
            blob: blob_of(&bytes)
        }
    );
    assert_eq!(
        repo.snapshot().unwrap().blob(blob_of(&bytes)).unwrap(),
        bytes
    );

    // 2. Listed among the tags, and not under another prefix.
    let snapshot = repo.snapshot().unwrap();
    let listed = snapshot.tags("tags/").unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, V1);
    assert_eq!(snapshot.tags("tags/w").unwrap(), []);
}

/// A lightweight create writes the ref's move alone, a frame as long as a raw create's, until
/// its name held a tag object: then its frame also holds a Lightweight fact, so that the old
/// object is not carried again.
#[test]
fn a_lightweight_tag_writes_no_fact_until_the_feature_is_on() {
    let (_dir, root, repo, t1, _t2) = v3_repo();

    // 1. A lightweight tag: the same frame length as a raw create of a name as long.
    let before = journal_len(&root);
    let made = repo.tag_create("tags/a", t1, TagObject::Lightweight, ACTOR);
    assert_eq!(made.unwrap().applied, Applied::New);
    let tagged = journal_len(&root) - before;
    let before = journal_len(&root);
    repo.cas_ref("tags/b", None, Some(t1), ACTOR).unwrap();
    assert_eq!(tagged, journal_len(&root) - before);
    assert_eq!(carried(&repo, "tags/a"), Some((t1, TagKind::Lightweight)));
    assert!(!lists_feature(&root), "no Tag fact, no feature");

    // 2. An annotated tag turns the feature on.
    repo.tag_create(V1, t1, annotated("one"), ACTOR).unwrap();
    assert!(lists_feature(&root));

    // 3. Deleted and made lightweight at the same target: a fact more than a raw create.
    delete(&repo, V1, t1);
    let before = journal_len(&root);
    let made = repo.tag_create(V1, t1, TagObject::Lightweight, ACTOR);
    assert_eq!(made.unwrap().applied, Applied::New);
    assert!(journal_len(&root) - before > tagged);
    assert_eq!(carried(&repo, V1), Some((t1, TagKind::Lightweight)));
}

#[test]
fn a_tag_recreated_lightweight_elsewhere_is_lightweight() {
    let (_dir, _root, repo, t1, t2) = v3_repo();
    repo.tag_create(V1, t1, annotated("one"), ACTOR).unwrap();
    delete(&repo, V1, t1);
    repo.tag_create(V1, t2, TagObject::Lightweight, ACTOR)
        .unwrap();
    assert_eq!(carried(&repo, V1), Some((t2, TagKind::Lightweight)));

    // Moved back by a raw delete and create at its first target: still no object.
    delete(&repo, V1, t2);
    repo.cas_ref(V1, None, Some(t1), ACTOR).unwrap();
    assert_eq!(carried(&repo, V1), Some((t1, TagKind::Lightweight)));
}

#[test]
fn a_tag_recreated_at_its_target_by_ref_set_keeps_its_object() {
    let (_dir, _root, repo, t1, t2) = v3_repo();
    let blob = blob_of(&tag_bytes("one"));
    repo.tag_create(V1, t1, annotated("one"), ACTOR).unwrap();

    // 1. Recreated at its target by a raw move: the object again.
    delete(&repo, V1, t1);
    assert_eq!(read(&repo, V1), None);
    repo.cas_ref(V1, None, Some(t1), ACTOR).unwrap();
    assert_eq!(carried(&repo, V1), Some((t1, TagKind::Annotated { blob })));

    // 2. Recreated elsewhere by a raw move: lightweight.
    delete(&repo, V1, t1);
    repo.cas_ref(V1, None, Some(t2), ACTOR).unwrap();
    assert_eq!(carried(&repo, V1), Some((t2, TagKind::Lightweight)));
}

#[test]
fn op_undo_of_a_tag_delete_brings_its_object_back() {
    let (_dir, _root, repo, t1, _t2) = v3_repo();
    let blob = blob_of(&tag_bytes("one"));
    repo.tag_create(V1, t1, annotated("one"), ACTOR).unwrap();
    delete(&repo, V1, t1);

    // The delete is the last operation; its undo makes the tag again.
    let (ops, _older) = repo.op_log(None, 1).unwrap();
    let op = &ops[0];
    assert!(matches!(
        &op.records[..],
        [OpRecord::RefMove { name, new: None, .. }] if name == V1
    ));
    repo.undo(OpId::new(op.seq, op.crc), ACTOR).unwrap();
    assert_eq!(carried(&repo, V1), Some((t1, TagKind::Annotated { blob })));
}

#[test]
fn a_tag_created_again_alike_is_already_applied() {
    let (_dir, root, repo, t1, _t2) = v3_repo();

    // 1. Annotated: the same bytes at the same target, nothing written.
    repo.tag_create(V1, t1, annotated("one"), ACTOR).unwrap();
    let before = journal_len(&root);
    let again = repo.tag_create(V1, t1, annotated("one"), ACTOR).unwrap();
    assert_eq!(again.applied, Applied::Already);
    assert_eq!(journal_len(&root), before);

    // 2. Lightweight: the same.
    repo.tag_create("tags/a", t1, TagObject::Lightweight, ACTOR)
        .unwrap();
    let before = journal_len(&root);
    let again = repo.tag_create("tags/a", t1, TagObject::Lightweight, ACTOR);
    assert_eq!(again.unwrap().applied, Applied::Already);
    assert_eq!(journal_len(&root), before);
}

#[test]
fn a_tag_created_again_otherwise_is_immutable() {
    let (_dir, root, repo, t1, t2) = v3_repo();
    let blob = blob_of(&tag_bytes("one"));
    repo.tag_create(V1, t1, annotated("one"), ACTOR).unwrap();
    let before = journal_len(&root);

    // Another target, with the same object or none: refused, nothing written.
    let made = repo.tag_create(V1, t2, annotated("one"), ACTOR);
    assert!(immutable(&made, V1), "{made:?}");
    let made = repo.tag_create(V1, t2, TagObject::Lightweight, ACTOR);
    assert!(immutable(&made, V1), "{made:?}");
    assert_eq!(journal_len(&root), before);
    assert_eq!(carried(&repo, V1), Some((t1, TagKind::Annotated { blob })));
}

/// X1 (the design's Corrections, C.1): the same target with another object, annotated or
/// lightweight either way, is a move of the tag too.
#[test]
fn another_object_at_the_same_target_is_immutable() {
    let (_dir, root, repo, t1, _t2) = v3_repo();

    // 1. Annotated, then another object or none.
    repo.tag_create(V1, t1, annotated("one"), ACTOR).unwrap();
    let before = journal_len(&root);
    let made = repo.tag_create(V1, t1, annotated("two"), ACTOR);
    assert!(immutable(&made, V1), "{made:?}");
    let made = repo.tag_create(V1, t1, TagObject::Lightweight, ACTOR);
    assert!(immutable(&made, V1), "{made:?}");
    assert_eq!(journal_len(&root), before);

    // 2. Lightweight, then an object.
    repo.tag_create("tags/a", t1, TagObject::Lightweight, ACTOR)
        .unwrap();
    let made = repo.tag_create("tags/a", t1, annotated("one"), ACTOR);
    assert!(immutable(&made, "tags/a"), "{made:?}");
    assert_eq!(carried(&repo, "tags/a"), Some((t1, TagKind::Lightweight)));
}

/// X2: FORMAT 2 has no Tag fact, so an annotated create is refused before anything is written,
/// its blob included.
#[test]
fn a_format_2_store_refuses_an_annotated_tag() {
    let (_dir, _root, repo) = new_repo_as(StoreFormat::V2);
    let t1 = commit(&repo, &[], vec![put("a.txt", b"a")]);
    let made = repo.tag_create(V1, t1, annotated("one"), ACTOR);
    assert!(
        matches!(
            &made,
            Err(RepoError::Store(StoreError::FormatRequired { name, needs: 3, store: 2 }))
                if name == FEATURE
        ),
        "{made:?}"
    );
    assert_eq!(read(&repo, V1), None);
    assert!(!repo.has_blob(blob_of(&tag_bytes("one"))).unwrap());
}

/// X2: FORMAT 2 takes a lightweight tag; FORMAT 1 takes no tag at all.
#[test]
fn a_format_2_store_takes_a_lightweight_tag() {
    let (_dir, _root, repo) = new_repo_as(StoreFormat::V2);
    let t1 = commit(&repo, &[], vec![put("a.txt", b"a")]);
    let made = repo.tag_create(V1, t1, TagObject::Lightweight, ACTOR);
    assert_eq!(made.unwrap().applied, Applied::New);
    assert_eq!(carried(&repo, V1), Some((t1, TagKind::Lightweight)));

    let (_dir, _root, repo) = new_repo_as(StoreFormat::V1);
    let t1 = commit(&repo, &[], vec![put("a.txt", b"a")]);
    let made = repo.tag_create(V1, t1, TagObject::Lightweight, ACTOR);
    assert!(
        matches!(
            made,
            Err(RepoError::Store(StoreError::FormatRequired {
                needs: 2,
                ..
            }))
        ),
        "{made:?}"
    );
}

/// What a recording of a FORMAT 3 store's making and then `run` holds.
struct Recorded {
    /// The calls, `before` and `returned` marked around `run`.
    calls: Vec<RecordedCall>,
    timeline: Timeline,
    /// The store root, under `base`.
    root: Utf8PathBuf,
    /// The changesets [`v3_on`] made.
    t1: ChangesetId,
}

/// Makes a FORMAT 3 store through an [`ImageRecordingFs`], then runs `run` on it between the
/// marks `before` and `returned`.
fn record(base: &Path, run: impl FnOnce(&LocalRepo, ChangesetId)) -> Recorded {
    let root = Utf8PathBuf::from_path_buf(base.join("store")).expect("UTF-8");
    let fs = Arc::new(ImageRecordingFs::new());
    fs.create_dir_all(root.as_std_path()).unwrap();
    fs.fsync_dir(base).unwrap();
    let (repo, t1, _t2) = v3_on(&root, Arc::clone(&fs) as Arc<dyn FsOps>);
    fs.mark("before");
    run(&repo, t1);
    fs.mark("returned");
    drop(repo);
    let calls = fs.calls();
    let timeline = Timeline::new(base, &calls);
    Recorded {
        calls,
        timeline,
        root,
        t1,
    }
}

/// The index of each call after the mark `before` that appends to `path`.
fn appends_to(rec: &Recorded, path: &Path) -> Vec<usize> {
    let from = rec.timeline.mark("before");
    (rec.calls.iter().enumerate())
        .filter(|&(at, _)| rec.timeline.steps_of(at).start >= from)
        .filter(
            |(_, call)| matches!(call, RecordedCall::AppendDurable { path: p, .. } if p == path),
        )
        .map(|(at, _)| at)
        .collect()
}

/// W10: the first Tag fact's frame is appended only after the feature list naming `tag_object`
/// replaced the old one; the next tag's frame replaces no list.
#[test]
fn the_feature_is_listed_before_the_first_tag_fact() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let rec = record(&base, |repo, t1| {
        repo.tag_create(V1, t1, annotated("one"), ACTOR).unwrap();
        repo.tag_create("tags/v2", t1, annotated("two"), ACTOR)
            .unwrap();
    });
    let layout = Layout::with_format(&rec.root, StoreFormat::V3);
    let features = layout.features_file().into_std_path_buf();
    let log = layout.journal_log().into_std_path_buf();

    // 1. Each replace of the list that names the feature, and the frames' appends.
    let listed: Vec<usize> = (rec.calls.iter().enumerate())
        .filter(|(_, call)| {
            matches!(call, RecordedCall::AtomicReplace { path, bytes }
                if *path == features && bytes.ends_with(b"tag_object\n"))
        })
        .map(|(at, _)| at)
        .collect();
    let frames = appends_to(&rec, &log);

    // 2. One replace, before the first tag's frame; the second frame after it.
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(frames.len(), 2, "{frames:?}");
    assert!(listed[0] < frames[0], "{listed:?} {frames:?}");
    assert!(rec.timeline.steps_of(listed[0]).start >= rec.timeline.mark("before"));
}

/// The directory the upgrade writes its first run in before it renames the run into
/// `sorted3/` (`rename_durable`, which syncs `sorted3/`). The power model takes a rename back
/// when the source's entry was never synced, and no step syncs `tmp/`: so every image of seed 0
/// loses the upgrade's run, which the manifest names, whatever runs after the upgrade. Not this
/// slice's: the tag create writes nothing there.
const RUNS_TMP: &str = "store/meta/sorted3/tmp";

/// [`Seeded`], but for the changes to [`RUNS_TMP`], which it keeps: the store as the upgrade
/// left it, so that the sweep judges the tag create alone.
struct Settled(Seeded);

impl Chooser for Settled {
    fn entries(&mut self, dir: &Path, uncovered: usize) -> usize {
        if dir == Path::new(RUNS_TMP) {
            return uncovered;
        }
        self.0.entries(dir, uncovered)
    }

    fn data(&mut self, file: &Path) -> Keep {
        self.0.data(file)
    }
}

/// The images of a power failure after every step of an annotated tag's create, for the seeds
/// 0 (the least the disk can hold), 1 (the most) and 2, 3 (random): each holds no `tags/v1`, or
/// holds it annotated with the exact bytes, never lightweight; once the create returned, the
/// latter.
#[test]
fn every_crash_image_of_a_tag_create_is_none_or_whole() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let bytes = tag_bytes("one");
    let rec = record(&base, |repo, t1| {
        repo.tag_create(V1, t1, annotated("one"), ACTOR).unwrap();
    });
    let whole = Some((
        rec.t1,
        TagKind::Annotated {
            blob: blob_of(&bytes),
        },
    ));
    let from = rec.timeline.mark("before");
    let returned = rec.timeline.mark("returned");

    // 1. Every cut, every seed: reopened, the tag is absent or whole.
    let mut failures = Vec::new();
    let mut seen = (false, false);
    for cut in from..=rec.timeline.steps() {
        for seed in 0..4 {
            let image = rec
                .timeline
                .image(cut, &mut Settled(Seeded::new(seed, cut)));
            let out = tempfile::tempdir().unwrap();
            image.materialize(out.path()).unwrap();
            let root = Utf8PathBuf::from_path_buf(out.path().join("store")).unwrap();
            let found = reopened(&root, &bytes);
            match found {
                Ok((None, _)) if cut < returned => seen.0 = true,
                Ok((tag, Some(held))) if tag == whole && held == bytes => seen.1 = true,
                other => failures.push(format!("cut {cut}, seed {seed}: {other:?}")),
            }
        }
    }

    // 2. None failed, and the sweep saw both sides of the commit point.
    assert!(failures.is_empty(), "{failures:#?}");
    assert_eq!(seen, (true, true));
}

/// A batch of [`TAG_BATCH`] + 1 tags is two frames; a process killed after the first leaves its
/// tags whole, and the batch run again takes the rest once.
#[test]
fn a_tag_batch_killed_between_frames_resumes() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let batch = |t1| -> Vec<(String, ChangesetId, TagObject)> {
        (0..65)
            .map(|n| (format!("tags/v{n}"), t1, annotated(&format!("tag {n}"))))
            .collect()
    };
    let rec = record(&base, |repo, t1| {
        let made = repo.tags_create(&batch(t1), ACTOR).unwrap();
        assert!(made.iter().all(Result::is_ok));
    });

    // 1. The image a kill leaves just before the second frame's append: 64 tags, whole.
    let log = Layout::with_format(&rec.root, StoreFormat::V3).journal_log();
    let frames = appends_to(&rec, log.as_std_path());
    assert_eq!(frames.len(), 2, "{frames:?}");
    let cut = rec.timeline.steps_of(frames[1]).start;
    let image = rec.timeline.image(cut, &mut Seeded::new(1, cut));
    let out = tempfile::tempdir().unwrap();
    image.materialize(out.path()).unwrap();
    let root = Utf8PathBuf::from_path_buf(out.path().join("store")).unwrap();
    let repo = LocalRepo::open(&root).unwrap();
    let tags = repo.snapshot().unwrap().tags("tags/").unwrap();
    assert_eq!(tags.len(), TAG_BATCH);
    assert!(
        tags.iter()
            .all(|tag| matches!(tag.kind, TagKind::Annotated { .. }))
    );

    // 2. Run again: the first frame's tags applied already, the last one new.
    let made = repo.tags_create(&batch(rec.t1), ACTOR).unwrap();
    let applied: Vec<Applied> = made.into_iter().map(|made| made.unwrap().applied).collect();
    assert_eq!(applied[..64], [Applied::Already; 64]);
    assert_eq!(applied[64..], [Applied::New]);
    for n in [0, 64] {
        let blob = blob_of(&tag_bytes(&format!("tag {n}")));
        let name = format!("tags/v{n}");
        assert_eq!(
            carried(&repo, &name),
            Some((rec.t1, TagKind::Annotated { blob }))
        );
    }
}

/// One frame moves at most as many refs as a `ref txn` (64, the design's Corrections C.2): 65
/// tags are two operations, of 64 and 1 moves. A batch's bad name fails alone.
#[test]
fn a_tag_batch_is_at_most_a_ref_txn() {
    let (_dir, _root, repo, t1, _t2) = v3_repo();
    let mut batch: Vec<(String, ChangesetId, TagObject)> = (0..65)
        .map(|n| (format!("tags/v{n}"), t1, TagObject::Lightweight))
        .collect();
    batch.push(("main".to_owned(), t1, TagObject::Lightweight));
    let made = repo.tags_create(&batch, ACTOR).unwrap();
    assert!(made[..65].iter().all(Result::is_ok));
    assert!(
        matches!(&made[65], Err(RepoError::RefName(name)) if name == "main"),
        "{:?}",
        made[65]
    );

    // The last two operations: 64 moves, then 1.
    let (ops, _older) = repo.op_log(None, 2).unwrap();
    let moves: Vec<usize> = ops.iter().map(|op| op.records.len()).collect();
    assert_eq!(moves, [1, 64], "newest first");
}

/// gc counts the blob a Tag fact names as reached: no orphan more than before the tag; a blob
/// nothing names is one (the count's self-check).
#[test]
fn gc_counts_no_tag_object_as_unreached() {
    let (_dir, _root, repo, t1, _t2) = v3_repo();
    let orphans = || repo.gc(GcRequest::new(false)).unwrap().orphan_objects;
    let before = orphans();
    repo.tag_create(V1, t1, annotated("one"), ACTOR).unwrap();
    assert_eq!(orphans(), before);
    assert!(repo.has_blob(blob_of(&tag_bytes("one"))).unwrap());

    repo.put_blob(b"named by nothing".to_vec()).unwrap();
    assert_eq!(orphans(), before + 1);
}
