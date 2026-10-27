//! Changesets recorded with the fields their caller gives (`Repo::commit_raw`,
//! `Repo::commit_raw_batch` and the `RawBatch` of `Repo::raw_batch`): a root changeset beside
//! `main`, parents in their order, signatures, headers, message and metadata kept verbatim, the
//! delta diffed from the first parent's root, many changesets in one journal frame, and every
//! refusal made before anything is written.
//!
//! Every test owns a fresh store (`tempfile::tempdir()`), written through the order checker
//! (`swarmops_testkit::faults::checked`) in a build with the test hooks: a frame appended before
//! every object its process linked is durable panics there (W5).

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_core::{
    Changeset, ChangesetId, CodecError, DeltaEntry, DeltaOp, LeafKind, ObjectKind, Signature, Tree,
    TreeEntry, TreeId, hash_object,
};
use swarmops_repo::{Depth, InsertOutcome, LocalRepo, RawChangeset, RawOutcome, RepoError};
use swarmops_store::{
    Frame, FsObjectStore, Journal, LimitKind, ObjectStore, RealFs, Record, StoreError, StoreFormat,
};
use swarmops_testkit::faults::checked;
use swarmops_testkit::fs::RecordingFs;
use swarmops_testkit::repo::{blob, commit, new_repo_as, path, put, signature};
use swarmops_testkit::store::layout_of;
use tempfile::TempDir;

/// A fresh store, written through the order checker in a build with the test hooks.
fn new_repo() -> (TempDir, Utf8PathBuf, LocalRepo) {
    let dir = tempfile::tempdir().expect("a temp dir");
    let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).expect("UTF-8 temp paths");
    let repo = LocalRepo::create_with_fs(&root, "test", checked(RealFs::new())).expect("create");
    (dir, root, repo)
}

/// Stores, with `put_objects`, the tree that holds `files` (each a path and a file's content),
/// its subtrees and its blobs; the tree's id.
fn store_tree(repo: &LocalRepo, files: &[(&str, &[u8])]) -> TreeId {
    let mut objects = Vec::new();
    let root = tree_of(files, &mut objects);
    repo.put_objects(objects).expect("put the tree");
    root
}

/// The tree that holds `files`, its subtrees and its blobs appended to `objects`.
fn tree_of(files: &[(&str, &[u8])], objects: &mut Vec<(ObjectKind, Vec<u8>)>) -> TreeId {
    let mut below: BTreeMap<&str, Vec<(&str, &[u8])>> = BTreeMap::new();
    let mut entries = BTreeMap::new();
    for &(file, content) in files {
        if let Some((dir, rest)) = file.split_once('/') {
            below.entry(dir).or_default().push((rest, content));
        } else {
            objects.push((ObjectKind::Blob, content.to_vec()));
            let entry = TreeEntry::leaf(file, LeafKind::File, blob(content));
            entries.insert(file.to_owned(), entry);
        }
    }
    for (dir, files) in below {
        let id = tree_of(&files, objects);
        entries.insert(dir.to_owned(), TreeEntry::dir(dir, id));
    }
    let tree = Tree::new(entries.into_values().collect()).expect("a valid tree");
    objects.push((ObjectKind::Tree, tree.encode()));
    tree.id()
}

/// A raw changeset of `root` on `parents`, by [`signature`], with `message` and no extra headers
/// or metadata.
fn raw(parents: &[ChangesetId], root: TreeId, message: &str) -> RawChangeset {
    RawChangeset::new(
        parents.to_vec(),
        root,
        signature(),
        signature(),
        message.as_bytes().to_vec(),
        Vec::new(),
        Vec::new(),
    )
}

/// The id of the changeset `raw` makes if its delta is `delta`: the test's own oracle.
fn expected_id(raw: &RawChangeset, delta: Vec<DeltaEntry>) -> ChangesetId {
    let changeset = Changeset::new(
        raw.parents.clone(),
        raw.root,
        delta,
        raw.author.clone(),
        raw.committer.clone(),
        raw.message.clone(),
        raw.extra_headers.clone(),
        raw.metadata.clone(),
    )
    .unwrap();
    changeset.id().expect("a canonical changeset")
}

/// The delta entry that puts a file holding `content` at `p`.
fn put_file(p: &str, content: &[u8]) -> DeltaEntry {
    DeltaEntry::new(
        path(p),
        DeltaOp::Put {
            kind: LeafKind::File,
            id: blob(content),
        },
        None,
    )
    .unwrap()
}

/// The delta of changeset `id`, as the store holds it.
fn delta(repo: &LocalRepo, id: ChangesetId) -> Vec<DeltaEntry> {
    let stored = repo
        .changeset(id)
        .expect("read")
        .expect("the changeset is stored");
    stored.changeset.delta
}

/// The journal's frames, as a reader reads them.
fn frames(root: &Utf8Path) -> Vec<Frame> {
    let journal =
        Journal::open(&layout_of(root), Arc::new(RealFs::new())).expect("open the journal");
    journal.tail_frames_from(0).expect("read the journal")
}

/// What the store at `root` holds that a write changes: its object files, by path, and the
/// bytes of `journal.log`.
fn on_disk(root: &Utf8Path) -> (Vec<PathBuf>, Vec<u8>) {
    let mut files = Vec::new();
    for fanout in fs::read_dir(root.join("objects")).expect("objects/") {
        let fanout = fanout.expect("a fan-out directory");
        if fanout.file_name() != "tmp" {
            for file in fs::read_dir(fanout.path()).expect("a fan-out directory") {
                files.push(file.expect("an object file").path());
            }
        }
    }
    files.sort();
    let log = fs::read(layout_of(root).journal_log()).expect("journal.log");
    (files, log)
}

/// `repo.commit_raw_batch(raws)`, which must be refused, leaving the store at `root` as it was:
/// its error.
#[track_caller]
fn refused(repo: &LocalRepo, root: &Utf8Path, raws: Vec<RawChangeset>) -> RepoError {
    let before = on_disk(root);
    let err = repo.commit_raw_batch(raws).expect_err("refused");
    assert_eq!(on_disk(root), before, "{err:?}: something was written");
    err
}

/// A changeset id no store holds, made from `seed`.
fn unknown(seed: &[u8]) -> ChangesetId {
    ChangesetId::new(hash_object(ObjectKind::Changeset, seed))
}

/// A raw changeset without parents is a root changeset whatever refs exist: `main` names another
/// root changeset and stays there. Its delta puts every file of its root.
#[test]
fn commit_raw_makes_a_root_changeset_while_main_exists() {
    let (_dir, _root, repo) = new_repo();
    let first = commit(&repo, &[], vec![put("a", b"a\n")]);
    repo.cas_ref("main", None, Some(first), "test/agent")
        .expect("main");
    let root = store_tree(&repo, &[("b", b"b\n"), ("d/e", b"e\n")]);
    let second = raw(&[], root, "a second root");
    let want = expected_id(
        &second,
        vec![put_file("b", b"b\n"), put_file("d/e", b"e\n")],
    );

    let outcome = repo.commit_raw(second).expect("commit_raw");
    let made = RawOutcome::new(want, InsertOutcome::Inserted { seq: 1 }, 2);
    assert_eq!(outcome.outcomes, [made]);
    assert_eq!(
        outcome.frame_seq,
        Some(2),
        "after the commit's frame and main's"
    );
    assert_eq!(outcome.warnings, Vec::<String>::new());
    let record = repo.changeset_meta(want).unwrap().expect("recorded");
    assert_eq!((record.generation, record.parents), (1, Vec::new()));
    let main = repo.get_ref("main").unwrap().map(|state| state.target);
    assert_eq!(main, Some(first), "main stays where it was");
    assert_eq!(repo.info().unwrap().changesets, 2);
    assert_eq!(repo.verify(Depth::Deep).unwrap().problems, []);
}

/// Parents in their own order (not their seqs'), signatures (a time before 1970, an odd offset,
/// a `-0000` committer, a name that is not UTF-8), extra headers in their order, metadata and a
/// message without a final line break are kept byte for byte: the stored object decodes to them.
#[test]
fn commit_raw_keeps_parents_signatures_headers_and_message_verbatim() {
    let (_dir, root_dir, repo) = new_repo();
    let older = commit(&repo, &[], vec![put("a", b"older")]);
    let newer = commit(&repo, &[], vec![put("a", b"newer")]);
    let root = store_tree(&repo, &[("a", b"merged")]);
    let gpgsig = b"-----BEGIN PGP SIGNATURE-----\n\nwsBcBAABCAAQBQJ\n-----END PGP SIGNATURE-----";
    let author = Signature::new(
        b"Ren\xe9 Fixture".to_vec(),
        b"rene@fixture.example".to_vec(),
        -86_400,
        19_800,
        false,
    )
    .unwrap();
    let committer = Signature::new(
        b"Committer".to_vec(),
        b"committer@fixture.example".to_vec(),
        1_700_000_060,
        0,
        true,
    )
    .unwrap();
    let message = b"merge newer into older\n\nno line break at the end".to_vec();
    let extra_headers = vec![
        (b"encoding".to_vec(), b"ISO-8859-1".to_vec()),
        (b"gpgsig".to_vec(), gpgsig.to_vec()),
        (b"x-custom".to_vec(), b"kept as it is".to_vec()),
    ];
    let metadata = vec![(
        "git.oid".to_owned(),
        b"0123456789abcdef0123456789abcdef01234567".to_vec(),
    )];
    let raw = RawChangeset::new(
        vec![newer, older],
        root,
        author,
        committer,
        message,
        extra_headers,
        metadata,
    );

    let outcome = repo.commit_raw(raw.clone()).expect("commit_raw");
    let id = outcome.outcomes[0].id;
    let objects = FsObjectStore::open(layout_of(&root_dir), Box::new(RealFs::new()));
    let (kind, payload) = objects.get(id.0).unwrap().expect("the changeset's object");
    assert_eq!(kind, ObjectKind::Changeset);
    let stored = Changeset::decode(&payload).expect("a canonical changeset");
    let want = Changeset::new(
        raw.parents,
        root,
        vec![put_file("a", b"merged")],
        raw.author,
        raw.committer,
        raw.message,
        raw.extra_headers,
        raw.metadata,
    )
    .unwrap();
    assert_eq!(stored, want);
    let record = repo.changeset_meta(id).unwrap().expect("recorded");
    assert_eq!(record.parents, [newer, older]);
}

/// A merge's delta is what its root changes from its first parent's root, whatever its other
/// parents hold: here only its second parent's `b`. A later changeset's delta deletes and
/// modifies; one whose root is its first parent's changes nothing. `verify`, which recomputes
/// each root from its first parent's root and its delta, finds nothing wrong.
#[test]
fn commit_raw_delta_is_the_tree_diff_from_the_first_parent() {
    let (_dir, _root, repo) = new_repo();
    let base = commit(&repo, &[], vec![put("a", b"a"), put("dir/x", b"x")]);
    let first = commit(&repo, &[base], vec![put("a", b"a, first")]);
    let second = commit(&repo, &[base], vec![put("b", b"b")]);
    let merged = store_tree(&repo, &[("a", b"a, first"), ("b", b"b"), ("dir/x", b"x")]);
    let merge = repo
        .commit_raw(raw(&[first, second], merged, "merge"))
        .unwrap();
    let merge = merge.outcomes[0];
    assert_eq!(delta(&repo, merge.id), [put_file("b", b"b")]);
    assert_eq!(merge.files_changed, 1);

    let later = store_tree(&repo, &[("b", b"b"), ("dir/x", b"x, later")]);
    let next = repo.commit_raw(raw(&[merge.id], later, "later")).unwrap();
    let next = next.outcomes[0];
    let deleted = DeltaEntry::new(path("a"), DeltaOp::Delete, None).unwrap();
    assert_eq!(
        delta(&repo, next.id),
        [deleted, put_file("dir/x", b"x, later")]
    );
    assert_eq!(next.files_changed, 2);

    let same = repo
        .commit_raw(raw(&[next.id], later, "the same tree"))
        .unwrap();
    let same = same.outcomes[0];
    assert_eq!((delta(&repo, same.id), same.files_changed), (Vec::new(), 0));
    assert_eq!(repo.verify(Depth::Deep).unwrap().problems, []);
}

/// Three chained raw changesets in one call are one journal frame of three changeset records,
/// in the batch's order, each naming the one before as its parent (whose root the batch gives
/// the delta), with generations 1 to 3.
#[test]
fn commit_raw_batch_is_one_frame_with_parents_from_the_batch() {
    let (_dir, root_dir, repo) = new_repo();
    let mut raws = Vec::new();
    let mut ids: Vec<ChangesetId> = Vec::new();
    for n in 1..=3 {
        let content = format!("version {n}\n");
        let root = store_tree(&repo, &[("f", content.as_bytes())]);
        let parents: Vec<ChangesetId> = ids.last().copied().into_iter().collect();
        let next = raw(&parents, root, &format!("raw {n}"));
        ids.push(expected_id(&next, vec![put_file("f", content.as_bytes())]));
        raws.push(next);
    }

    let outcome = repo.commit_raw_batch(raws).expect("commit_raw_batch");
    let inserted: Vec<RawOutcome> = (ids.iter().zip(0..))
        .map(|(&id, seq)| RawOutcome::new(id, InsertOutcome::Inserted { seq }, 1))
        .collect();
    assert_eq!(outcome.outcomes, inserted);
    let frames = frames(&root_dir);
    let [frame] = &frames[..] else {
        panic!("one frame: {frames:?}");
    };
    assert_eq!(outcome.frame_seq, Some(frame.seq));
    let recorded: Vec<ChangesetId> = (frame.records.iter())
        .map(|record| match record {
            Record::Changeset { id, .. } => *id,
            other => panic!("a changeset record: {other:?}"),
        })
        .collect();
    assert_eq!(recorded, ids);
    let generation = |id| {
        repo.changeset_meta(id)
            .unwrap()
            .expect("recorded")
            .generation
    };
    assert_eq!(
        ids.iter().map(|&id| generation(id)).collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert_eq!(repo.verify(Depth::Deep).unwrap().problems, []);
}

/// The same raw changeset again is the changeset the store holds: `AlreadyPresent`, and no frame
/// is appended. A batch that names one twice records it once, its second `AlreadyPresent`.
#[test]
fn commit_raw_of_a_changeset_the_store_holds_is_already_present() {
    let (_dir, root_dir, repo) = new_repo();
    let root = store_tree(&repo, &[("f", b"f")]);
    let once = repo.commit_raw(raw(&[], root, "once")).expect("commit_raw");
    let [made] = once.outcomes[..] else {
        panic!("one outcome: {once:?}");
    };
    assert_eq!(made.insert, InsertOutcome::Inserted { seq: 0 });
    let log = fs::read(layout_of(&root_dir).journal_log()).unwrap();

    let again = repo.commit_raw(raw(&[], root, "once")).expect("commit_raw");
    let present = RawOutcome::new(made.id, InsertOutcome::AlreadyPresent, made.files_changed);
    assert_eq!(again.outcomes, [present]);
    assert_eq!(again.frame_seq, None, "no frame");
    assert_eq!(fs::read(layout_of(&root_dir).journal_log()).unwrap(), log);

    let twice = vec![raw(&[], root, "twice"), raw(&[], root, "twice")];
    let twice = repo.commit_raw_batch(twice).expect("commit_raw_batch");
    let [first, second] = twice.outcomes[..] else {
        panic!("two outcomes: {twice:?}");
    };
    assert_eq!(first.id, second.id);
    assert_eq!(first.insert, InsertOutcome::Inserted { seq: 1 });
    assert_eq!(second.insert, InsertOutcome::AlreadyPresent);
    let frames = frames(&root_dir);
    let last = frames.last().expect("a frame");
    assert_eq!((twice.frame_seq, last.records.len()), (Some(last.seq), 1));
}

/// What a raw changeset must be is checked before anything is written: at most 255 parents, none
/// listed twice, metadata keys ascending and none ending in `:b64`, a root the store holds as a
/// tree, and parents the store holds. Each refusal leaves the object files and `journal.log` as
/// they were.
#[test]
fn commit_raw_refuses_before_writing() {
    let (_dir, root_dir, repo) = new_repo();
    let parent = commit(&repo, &[], vec![put("a", b"a")]);
    let root = store_tree(&repo, &[("b", b"b")]);
    let good = || raw(&[parent], root, "refused");
    let refuse = |raw: RawChangeset| refused(&repo, &root_dir, vec![raw]);

    let many = (0..256_u32).map(|n| unknown(&n.to_le_bytes())).collect();
    let mut raw = good();
    raw.parents = many;
    let err = refuse(raw);
    let limit = |e: &RepoError| match e {
        RepoError::Store(StoreError::Limit { what, max, got }) => Some((*what, *max, *got)),
        _ => None,
    };
    assert_eq!(limit(&err), Some((LimitKind::Parents, 255, 256)), "{err:?}");
    let mut raw = good();
    raw.parents = vec![parent, parent];
    let err = refuse(raw);
    assert!(matches!(err, RepoError::Store(StoreError::DuplicateParent(p)) if p == parent));
    let metadata = vec![("note:b64".to_owned(), b"x".to_vec())];
    let mut raw = good();
    raw.metadata = metadata;
    let err = refuse(raw);
    assert!(matches!(&err, RepoError::ReservedMetadataKey { key } if key == "note:b64"));
    let metadata = vec![("b".to_owned(), Vec::new()), ("a".to_owned(), Vec::new())];
    let mut raw = good();
    raw.metadata = metadata;
    let err = refuse(raw);
    assert!(
        matches!(err, RepoError::Codec(CodecError::UnsortedMetadata)),
        "{err:?}"
    );
    let unstored = TreeId::new(hash_object(ObjectKind::Tree, b"never stored"));
    let mut raw = good();
    raw.root = unstored;
    let err = refuse(raw);
    let missing = |e: &RepoError| match e {
        RepoError::MissingObject { id, kind } => Some((*id, *kind)),
        _ => None,
    };
    assert_eq!(
        missing(&err),
        Some((unstored.0, ObjectKind::Tree)),
        "{err:?}"
    );
    // A blob the store holds, named as the root.
    let not_a_tree = TreeId::new(blob(b"b").0);
    let mut raw = good();
    raw.root = not_a_tree;
    let err = refuse(raw);
    assert_eq!(
        missing(&err),
        Some((not_a_tree.0, ObjectKind::Tree)),
        "{err:?}"
    );
    let stranger = unknown(b"never committed");
    let mut raw = good();
    raw.parents = vec![stranger];
    let err = refuse(raw);
    assert!(
        matches!(err, RepoError::UnknownParent(p) if p == stranger),
        "{err:?}"
    );
}

/// A batch is checked whole before any of it is written: a raw refused refuses the batch, the
/// raws before it unwritten too; a parent later in the batch is not one it holds yet; and a
/// batch holds at most the 65,535 changesets of one frame.
#[test]
fn commit_raw_batch_refuses_the_whole_batch_before_writing() {
    let (_dir, root_dir, repo) = new_repo();
    let root = store_tree(&repo, &[("f", b"f")]);
    let first = raw(&[], root, "first");
    let first_id = expected_id(&first, vec![put_file("f", b"f")]);
    let stranger = unknown(b"never committed");

    let orphan = raw(&[stranger], root, "orphan");
    let err = refused(&repo, &root_dir, vec![first.clone(), orphan]);
    assert!(
        matches!(err, RepoError::UnknownParent(p) if p == stranger),
        "{err:?}"
    );
    let child = raw(&[first_id], root, "a child before its parent");
    let err = refused(&repo, &root_dir, vec![child, first.clone()]);
    assert!(
        matches!(err, RepoError::UnknownParent(p) if p == first_id),
        "{err:?}"
    );
    let err = refused(&repo, &root_dir, vec![first; 65_536]);
    assert!(
        matches!(
            err,
            RepoError::Store(StoreError::Limit {
                what: LimitKind::Records,
                max: 65_535,
                got: 65_536
            })
        ),
        "{err:?}"
    );
}

/// A batch built a changeset at a time gives each one's id as it is pushed, writing nothing, so
/// that the next can name it as a parent; its commit writes them all as one frame. A push that is
/// refused adds nothing, and an empty batch commits nothing.
#[test]
fn a_raw_batch_names_each_changeset_before_it_writes_anything() {
    let (_dir, root_dir, repo) = new_repo();
    let roots: Vec<TreeId> = (1..=3)
        .map(|n| store_tree(&repo, &[("f", format!("{n}").as_bytes())]))
        .collect();
    let before = on_disk(&root_dir);
    let mut batch = repo.raw_batch();
    let mut ids: Vec<ChangesetId> = Vec::new();
    for (n, &root) in roots.iter().enumerate() {
        let parents: Vec<ChangesetId> = ids.last().copied().into_iter().collect();
        let pushed = batch.push(raw(&parents, root, &format!("pushed {n}")));
        ids.push(pushed.expect("push"));
    }
    let stranger = unknown(b"never committed");
    let err = batch
        .push(raw(&[stranger], roots[0], "refused"))
        .expect_err("an unknown parent");
    assert!(
        matches!(err, RepoError::UnknownParent(p) if p == stranger),
        "{err:?}"
    );
    assert_eq!(batch.len(), 3);
    assert_eq!(
        on_disk(&root_dir),
        before,
        "nothing written before the commit"
    );

    let outcome = batch.commit().expect("commit the batch");
    let committed: Vec<ChangesetId> = outcome.outcomes.iter().map(|made| made.id).collect();
    assert_eq!(committed, ids);
    assert_eq!(frames(&root_dir).len(), 1);
    assert_eq!(repo.verify(Depth::Deep).unwrap().problems, []);

    let before = on_disk(&root_dir);
    let empty = repo.raw_batch();
    assert!(empty.is_empty());
    let nothing = empty.commit().expect("commit an empty batch");
    assert_eq!((nothing.outcomes, nothing.frame_seq), (Vec::new(), None));
    assert_eq!(on_disk(&root_dir), before, "an empty batch writes nothing");
}

/// The root a batch names may have been stored by another handle on the store that has not
/// flushed it, so that its name is not durable yet: the batch's flush syncs the root's fan-out
/// directory before the frame that records the changeset, as it does for objects of its own.
#[test]
fn a_root_another_handle_stored_is_synced_before_the_frame() {
    let (_dir, root_dir, repo) = new_repo_as(StoreFormat::V1);
    drop(repo);
    let other = LocalRepo::open(&root_dir).expect("open");
    let root = store_tree(&other, &[("f", b"stored by another handle")]);
    let (fs, calls) = RecordingFs::new();
    let fs = fs.naming_paths_in(root_dir.clone().into_std_path_buf());
    let repo = LocalRepo::open_with_fs(&root_dir, Arc::new(fs)).expect("open");
    calls();

    let outcome = repo
        .commit_raw(raw(&[], root, "a root it did not put"))
        .unwrap();
    let layout = layout_of(&root_dir);
    let fanout = |id| {
        let file = layout.object_path(id);
        let dir = file.parent().expect("a fan-out directory");
        format!(
            "fsync_dir {}",
            dir.strip_prefix(&root_dir).expect("in the store")
        )
    };
    let (root_fanout, changeset_fanout) = (fanout(root.0), fanout(outcome.outcomes[0].id.0));
    assert_ne!(
        root_fanout, changeset_fanout,
        "the test needs the two apart"
    );
    let calls = calls();
    let at = |call: &str| calls.iter().position(|made| made == call);
    let synced = at(&root_fanout).unwrap_or_else(|| panic!("{root_fanout}: {calls:#?}"));
    let frame = at("append_durable meta/journal.log").expect("the frame");
    assert!(synced < frame, "{calls:#?}");
}
