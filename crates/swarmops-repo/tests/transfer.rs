//! Objects from another store (the native sync design §2.3, §2.4, §2.6): blobs put as stored
//! bytes (`Repo::put_blobs`), changesets adopted with their ids and no ref moved
//! (`Repo::adopt`, checks V1 to V5), and the changesets a store lacks of another's history
//! (`RepoRead::missing`, the ancestry walk from every `have`).
//!
//! The history every test transfers, made in a source store of its own:
//!
//! ```text
//!   c0 ── c1 ─────┬── c3 ── c4      c3 merges c1 and c2
//!     └── c2 ─────┘
//! ```

use std::collections::HashSet;
use std::sync::Arc;

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_core::{
    BlobId, Changeset, ChangesetId, DeltaOp, EntryTarget, Hashed, ObjectId, ObjectKind, Tree,
    TreeId,
};
use swarmops_repo::{Depth, LocalRepo, RepoError, StoreError, StoreFormat};
use swarmops_store::RealFs;
use swarmops_testkit::faults::checked;
use swarmops_testkit::fs::RecordingFs;
use swarmops_testkit::repo::{commit, delete, new_repo_as, put, request};
use swarmops_testkit::store::layout_of;
use tempfile::TempDir;

/// The formats native sync takes: FORMAT 1 stores are refused before any byte (the design §4).
const FORMATS: [StoreFormat; 2] = [StoreFormat::V2, StoreFormat::V3];

/// The source store and its history, parents first: c0 to c4 of the module documentation.
fn source() -> (TempDir, LocalRepo, Vec<ChangesetId>) {
    let (dir, _root, repo) = new_repo_as(StoreFormat::V2);
    let c0 = commit(
        &repo,
        &[],
        vec![
            put("a.txt", b"a"),
            put("src/lib.rs", b"lib"),
            put("src/x/y.txt", b"y"),
        ],
    );
    let c1 = commit(
        &repo,
        &[c0],
        vec![
            put("src/lib.rs", b"lib 2"),
            delete("a.txt"),
            put("docs/b.md", b"b"),
        ],
    );
    let c2 = commit(&repo, &[c0], vec![put("c.txt", b"c")]);
    let merge = request(&[c1, c2], vec![put("c.txt", b"c")], "merge c2");
    let c3 = repo.commit(merge).unwrap().id;
    let c4 = commit(&repo, &[c3], vec![delete("src/x/y.txt")]);
    (dir, repo, vec![c0, c1, c2, c3, c4])
}

/// The changeset objects of `ids` in `repo`, in their order, and the blobs their deltas put,
/// each once.
fn objects_of(repo: &LocalRepo, ids: &[ChangesetId]) -> (Vec<Hashed>, Vec<Hashed>) {
    let mut changesets = Vec::new();
    let mut blobs = Vec::new();
    let mut seen = HashSet::new();
    for &id in ids {
        let (kind, payload) = repo.object(id.0).unwrap().expect("the source holds it");
        let changeset = Changeset::decode(&payload).unwrap();
        changesets.push(Hashed::new(kind, payload));
        for entry in &changeset.delta {
            let DeltaOp::Put { id: blob, .. } = entry.op else {
                continue;
            };
            if seen.insert(blob) {
                let (kind, payload) = repo.object(blob.0).unwrap().expect("its blob");
                blobs.push(Hashed::new(kind, payload));
            }
        }
    }
    (changesets, blobs)
}

/// An empty store of `format`: FORMAT 3's made by an upgrade, as no `init` makes one yet.
fn target(format: StoreFormat) -> (TempDir, Utf8PathBuf, LocalRepo) {
    let (dir, root, repo) = new_repo_as(StoreFormat::V2);
    if format == StoreFormat::V2 {
        return (dir, root, repo);
    }
    repo.upgrade().unwrap();
    drop(repo);
    let repo = LocalRepo::open(&root).unwrap();
    assert_eq!(repo.format(), format);
    (dir, root, repo)
}

/// How long the store's `journal.log` is: a frame appended makes it longer.
fn journal_len(root: &Utf8Path) -> u64 {
    std::fs::metadata(layout_of(root).journal_log())
        .unwrap()
        .len()
}

/// Which of `ids` `repo` holds.
fn held(repo: &LocalRepo, ids: &[ChangesetId]) -> Vec<ChangesetId> {
    (ids.iter().copied())
        .filter(|&id| repo.changeset_meta(id).unwrap().is_some())
        .collect()
}

/// W5 for the changesets `ids`: each one's root tree, every tree below it and every blob they
/// name is in `repo`. The power-loss images' check: a frame never names a changeset whose
/// objects a power loss could take back.
fn whole(repo: &LocalRepo, ids: &[ChangesetId]) -> Result<(), String> {
    for &id in ids {
        let (_, payload) = (repo.object(id.0).map_err(|e| e.to_string())?)
            .ok_or_else(|| format!("the store lacks changeset {id}'s object"))?;
        let changeset = Changeset::decode(&payload).map_err(|e| e.to_string())?;
        let mut trees = vec![changeset.root];
        while let Some(tree) = trees.pop() {
            let tree = held_tree(repo, tree, id)?;
            for entry in tree.entries() {
                match entry.target {
                    EntryTarget::Dir(below) => trees.push(below),
                    EntryTarget::Leaf(_, blob) => held_blob(repo, blob, id)?,
                    other => return Err(format!("an unknown entry {other:?}")),
                }
            }
        }
    }
    Ok(())
}

/// Tree `id` of changeset `of`, which `repo` must hold.
fn held_tree(repo: &LocalRepo, id: TreeId, of: ChangesetId) -> Result<Tree, String> {
    match repo.object(id.0).map_err(|e| e.to_string())? {
        Some((ObjectKind::Tree, payload)) => Tree::decode(&payload).map_err(|e| e.to_string()),
        _ => Err(format!("changeset {of}: the store lacks tree {}", id.0)),
    }
}

/// Blob `id` of changeset `of`, which `repo` must hold.
fn held_blob(repo: &LocalRepo, id: BlobId, of: ChangesetId) -> Result<(), String> {
    match repo.object(id.0).map_err(|e| e.to_string())? {
        Some((ObjectKind::Blob, _)) => Ok(()),
        _ => Err(format!("changeset {of}: the store lacks blob {}", id.0)),
    }
}

/// Adopted into a store of each format, every changeset keeps its id, its generation and its
/// bytes, its trees are rebuilt whole, and the store verifies clean.
#[test]
fn adopt_keeps_every_id() {
    let (_src_dir, src, ids) = source();
    let (changesets, blobs) = objects_of(&src, &ids);
    for format in FORMATS {
        let (_dir, _root, repo) = target(format);
        let put = repo.put_blobs(blobs.clone()).unwrap();
        assert!(put.iter().all(|outcome| outcome.written), "{put:?}");

        let adopted = repo.adopt(changesets.clone()).unwrap();
        assert_eq!(adopted.adopted, ids, "{format:?}");
        assert_eq!(adopted.present, [], "{format:?}");
        assert_eq!(adopted.frames.len(), 1, "{format:?}: one frame");
        assert_eq!(adopted.warnings, Vec::<String>::new());

        for (&id, object) in ids.iter().zip(&changesets) {
            let theirs = src.changeset_meta(id).unwrap().unwrap();
            let ours = repo.changeset_meta(id).unwrap().expect("adopted");
            assert_eq!(ours.generation, theirs.generation, "{format:?} {id}");
            assert_eq!(ours.parents, theirs.parents, "{format:?} {id}");
            let bytes = repo.object(id.0).unwrap().unwrap();
            assert_eq!(bytes, (ObjectKind::Changeset, object.payload().to_vec()));
        }
        assert_eq!(whole(&repo, &ids), Ok(()), "{format:?}");
        assert_eq!(repo.verify(Depth::Deep).unwrap().problems, [], "{format:?}");
    }
}

/// V1: bytes that are not the changeset of the id they carry (another kind's object, made of a
/// changeset's bytes, or a tree) are refused, and nothing is recorded. A blob too: one that is
/// not the blob of its id, refused before any blob of the call is written.
#[test]
fn adopt_refuses_bytes_that_miss_their_id() {
    let (_src_dir, src, ids) = source();
    let (changesets, blobs) = objects_of(&src, &ids);
    let (_dir, root, repo) = target(StoreFormat::V2);
    repo.put_blobs(blobs).unwrap();
    let before = journal_len(&root);

    let as_blob = Hashed::new(ObjectKind::Blob, changesets[0].payload().to_vec());
    let tree = Hashed::new(ObjectKind::Tree, Tree::empty_root().encode());
    for bad in [as_blob, tree] {
        let id = bad.id();
        let refused = repo.adopt(vec![changesets[0].clone(), bad]);
        assert!(
            matches!(refused, Err(RepoError::Store(StoreError::Corrupt { id: at })) if at == id),
            "{refused:?}"
        );
    }
    assert_eq!(held(&repo, &ids), []);
    assert_eq!(journal_len(&root), before, "no frame");
}

/// A blob whose bytes are not the blob of the id it carries refuses the whole call: no blob of
/// it is written.
#[test]
fn put_blobs_refuses_bytes_that_miss_their_id_and_writes_none() {
    let (_dir, _root, repo) = target(StoreFormat::V2);
    let good = Hashed::new(ObjectKind::Blob, b"good".to_vec());
    let as_tree = Hashed::new(ObjectKind::Tree, Tree::empty_root().encode());
    let id = as_tree.id();

    let refused = repo.put_blobs(vec![good.clone(), as_tree]);
    assert!(
        matches!(refused, Err(RepoError::Store(StoreError::Corrupt { id: at })) if at == id),
        "{refused:?}"
    );
    assert!(!repo.contains_object(good.id()).unwrap(), "no blob written");
    assert!(!repo.contains_object(id).unwrap());
}

/// V5: a changeset whose delta, applied to its first parent's root, does not give its `root`
/// (here a forged delta that leaves a file out) is refused, and nothing is recorded.
#[test]
fn adopt_refuses_a_rebuilt_root_that_differs() {
    let (_src_dir, src, ids) = source();
    let (changesets, blobs) = objects_of(&src, &ids);
    let (_dir, _root, repo) = target(StoreFormat::V2);
    repo.put_blobs(blobs).unwrap();

    let mut forged = Changeset::decode(changesets[0].payload()).unwrap();
    forged.delta.pop();
    let (forged, _) = forged.encode_hashed().unwrap();
    let id = forged.id();

    let refused = repo.adopt(vec![forged]);
    assert!(
        matches!(refused, Err(RepoError::Store(StoreError::Corrupt { id: at })) if at == id),
        "{refused:?}"
    );
    assert_eq!(repo.info().unwrap().changesets, 0);
}

/// V3: a changeset whose parent the store lacks and the call does not name first is refused.
#[test]
fn adopt_refuses_an_unknown_parent() {
    let (_src_dir, src, ids) = source();
    let (changesets, blobs) = objects_of(&src, &ids);
    let (_dir, _root, repo) = target(StoreFormat::V2);
    repo.put_blobs(blobs).unwrap();

    let refused = repo.adopt(vec![changesets[1].clone()]);
    assert!(
        matches!(refused, Err(RepoError::UnknownParent(parent)) if parent == ids[0]),
        "{refused:?}"
    );
    // Parents after their child: the same.
    let refused = repo.adopt(vec![changesets[1].clone(), changesets[0].clone()]);
    assert!(
        matches!(refused, Err(RepoError::UnknownParent(parent)) if parent == ids[0]),
        "{refused:?}"
    );
    assert_eq!(repo.info().unwrap().changesets, 0);
}

/// V4: a changeset whose delta puts a blob the store lacks is refused, naming the path.
#[test]
fn adopt_refuses_a_missing_blob() {
    let (_src_dir, src, ids) = source();
    let (changesets, _blobs) = objects_of(&src, &ids);
    let (_dir, _root, repo) = target(StoreFormat::V2);

    let refused = repo.adopt(vec![changesets[0].clone()]);
    let Err(RepoError::MissingBlob { path, id }) = refused else {
        panic!("{refused:?}");
    };
    assert_eq!(path.as_str(), "a.txt");
    assert_eq!(id, swarmops_testkit::repo::blob(b"a"));
    assert_eq!(repo.info().unwrap().changesets, 0);
}

/// An adopt records changesets and moves no ref: the refs are what they were.
#[test]
fn adopt_moves_no_ref() {
    let (_src_dir, src, ids) = source();
    let (changesets, blobs) = objects_of(&src, &ids);
    for format in FORMATS {
        let (_dir, _root, repo) = target(format);
        let refs = repo.list_refs().unwrap();
        repo.put_blobs(blobs.clone()).unwrap();
        repo.adopt(changesets.clone()).unwrap();
        assert_eq!(repo.list_refs().unwrap(), refs, "{format:?}");
        assert_eq!(repo.info().unwrap().main, None, "{format:?}");
    }
}

/// Changesets the store holds are present, and a call of nothing new appends no frame; a call
/// of some new ones records only those.
#[test]
fn adopt_again_writes_no_frame() {
    let (_src_dir, src, ids) = source();
    let (changesets, blobs) = objects_of(&src, &ids);
    let (_dir, root, repo) = target(StoreFormat::V2);
    repo.put_blobs(blobs).unwrap();

    let first = repo.adopt(changesets[..2].to_vec()).unwrap();
    assert_eq!(first.adopted, ids[..2]);
    let rest = repo.adopt(changesets.clone()).unwrap();
    assert_eq!(rest.adopted, ids[2..]);
    assert_eq!(rest.present, ids[..2]);
    assert_eq!(rest.frames.len(), 1);

    let before = journal_len(&root);
    let again = repo.adopt(changesets.clone()).unwrap();
    assert_eq!(again.adopted, []);
    assert_eq!(again.present, ids);
    assert_eq!(again.frames, Vec::<u64>::new());
    assert_eq!(journal_len(&root), before, "no frame");

    // Named twice in one call: recorded once.
    let (_dir2, _root2, twice) = target(StoreFormat::V2);
    twice.put_blobs(objects_of(&src, &ids).1).unwrap();
    let doubled = vec![changesets[0].clone(), changesets[0].clone()];
    let once = twice.adopt(doubled).unwrap();
    assert_eq!((once.adopted, once.present), (vec![ids[0]], vec![ids[0]]));
}

/// W5: the trees an adopt rebuilds are linked and their directories synced before the journal
/// frame that records their changesets, under the store's order checker.
#[test]
fn adopt_writes_trees_before_the_frame() {
    let (_src_dir, src, ids) = source();
    let (changesets, blobs) = objects_of(&src, &ids);
    let (_dir, root, repo) = target(StoreFormat::V2);
    drop(repo);
    let (fs, calls) = RecordingFs::new();
    let fs = fs.naming_paths_in(root.as_std_path().to_path_buf());
    let repo = LocalRepo::open_with_fs(&root, checked(fs)).unwrap();
    repo.put_blobs(blobs).unwrap();
    calls();

    repo.adopt(changesets).unwrap();
    let adopting = calls();
    let layout = layout_of(&root);
    let relative = |path: Utf8PathBuf| path.strip_prefix(&root).unwrap().to_string();
    let at = |call: &str| {
        adopting
            .iter()
            .position(|made| made == call)
            .unwrap_or_else(|| panic!("no {call:?} among {adopting:?}"))
    };
    let frame = at(&format!(
        "append_durable {}",
        relative(layout.journal_log())
    ));
    for &id in &ids {
        let root_tree = repo.changeset(id).unwrap().unwrap().changeset.root;
        let file = relative(layout.object_path(root_tree.0));
        let dir = file.rsplit_once('/').unwrap().0.to_owned();
        let linked = at(&format!("link_noclobber {file}"));
        let synced = (adopting.iter().enumerate())
            .position(|(n, made)| n > linked && *made == format!("fsync_dir {dir}"))
            .unwrap_or_else(|| panic!("{dir} never synced after {file}: {adopting:?}"));
        assert!(linked < synced && synced < frame, "{id}: {adopting:?}");
    }
}

/// A missing `have` the store lacks is passed over; every one it holds prunes the walk: what
/// `want` reaches and none of them does, generation descending, then id ascending.
#[test]
fn missing_is_pruned_by_every_have() {
    let (_dir, repo, ids) = source();
    let [c0, c1, c2, c3, c4] = ids[..] else {
        unreachable!()
    };
    let read = repo.snapshot().unwrap();
    let missing = |have: &[ChangesetId], want| read.missing(have, want, 100, None).unwrap();
    let lacked = ChangesetId::new(ObjectId::new([7; 32]));
    let by_id = |a: ChangesetId, b: ChangesetId| if a < b { [a, b] } else { [b, a] };
    let [low, high] = by_id(c1, c2);

    assert_eq!(missing(&[], c4).changesets, [c4, c3, low, high, c0]);
    assert_eq!(missing(&[c1], c4).changesets, [c4, c3, c2]);
    assert_eq!(missing(&[c2], c4).changesets, [c4, c3, c1]);
    assert_eq!(missing(&[c1, c2], c4).changesets, [c4, c3]);
    assert_eq!(missing(&[lacked, c3], c4).changesets, [c4]);
    assert_eq!(missing(&[lacked], c4).changesets.len(), 5);
    assert_eq!(missing(&[c4], c4).changesets, []);
    assert_eq!(missing(&[c4], c1).changesets, []);
    assert_eq!(missing(&[], c4).next, None);
    assert!(matches!(
        read.missing(&[], lacked, 100, None),
        Err(RepoError::NotFound(_))
    ));
}

/// Paged one changeset at a time, `missing` gives the whole answer once, each id once, and its
/// last page says no more follow.
#[test]
fn missing_pages_and_never_repeats_an_id() {
    let (_dir, repo, ids) = source();
    let read = repo.snapshot().unwrap();
    let whole = read.missing(&[], ids[4], 100, None).unwrap().changesets;

    let mut paged = Vec::new();
    let mut after = None;
    loop {
        let page = read.missing(&[], ids[4], 2, after).unwrap();
        assert!(page.changesets.len() <= 2, "{page:?}");
        paged.extend(page.changesets);
        let Some(next) = page.next else {
            break;
        };
        after = Some(next);
    }
    assert_eq!(paged, whole);
    let unique: HashSet<_> = paged.iter().collect();
    assert_eq!(unique.len(), paged.len(), "{paged:?}");
}

/// The walk of one `from` gives the answers it gave before it took several: for every pair,
/// `from..to` is `missing` of `to` with the one `have` `from`.
#[test]
fn ancestry_answers_are_unchanged() {
    let (_dir, repo, ids) = source();
    let read = repo.snapshot().unwrap();
    for &from in &ids {
        for &to in &ids {
            let ancestry = read.ancestry(from, to, 100, None).unwrap();
            let entries: Vec<_> = ancestry.entries.iter().map(|entry| entry.id).collect();
            let missing = read.missing(&[from], to, 100, None).unwrap();
            assert_eq!(entries, missing.changesets, "{from}..{to}");
        }
    }
}

#[cfg(feature = "test-hooks")]
mod hooks {
    //! What needs the test hooks: the crash sweep of an adopt, and its power-loss images.

    use std::os::unix::process::ExitStatusExt;
    use std::process::Output;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;

    use swarmops_store::{CRASH_AT_ENV, FsOps, ORDER_CHECK_ENV};
    use swarmops_testkit::child::{SIGABRT, run, this_test};
    use swarmops_testkit::fs::copy_dir;
    use swarmops_testkit::power::{Image, ImageRecordingFs, Seeded, Timeline};
    use swarmops_testkit::test_name;

    use super::*;

    /// Set only in the children of the crash sweep: the store root.
    const CHILD_ROOT: &str = "SWARMOPS_TEST_REPO_TRANSFER_CHILD_ROOT";

    /// Set with [`CHILD_ROOT`]: the source store's root.
    const CHILD_SOURCE: &str = "SWARMOPS_TEST_REPO_TRANSFER_CHILD_SOURCE";

    /// Set with [`CHILD_ROOT`]: the history's ids, parents first, joined by commas.
    const CHILD_IDS: &str = "SWARMOPS_TEST_REPO_TRANSFER_CHILD_IDS";

    /// How many runs of the crash sweep go at once.
    const AT_ONCE: u64 = 8;

    /// The power-loss sweep's seeds in the gate: 0 keeps nothing not durable, 1 everything.
    const SEEDS: [u64; 2] = [0, 1];

    /// The slow sweep's: those, and eight more that pick at random.
    const SEEDS_10: [u64; 10] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9];

    /// How many changesets the power-loss run's first adopt takes; the second takes the rest.
    const FIRST_BATCH: usize = 2;

    /// The source and the history a crash sweep's child adopts.
    struct Source {
        /// The source store's root.
        root: Utf8PathBuf,
        /// The history, parents first.
        ids: Vec<ChangesetId>,
        /// Their objects, in the same order.
        changesets: Vec<Hashed>,
    }

    /// Runs the crash sweep's test again in a child on the store at `root`, its k-th
    /// filesystem call aborting it, every call before it under the order checker.
    fn crash_child(root: &Utf8Path, source: &Source, k: u64) -> Output {
        let test = "adopt_crash_at_every_call_leaves_none_or_all";
        let ids: Vec<String> = source.ids.iter().map(ToString::to_string).collect();
        run(this_test(&test_name!(test))
            .env(CHILD_ROOT, root)
            .env(CHILD_SOURCE, &source.root)
            .env(CHILD_IDS, ids.join(","))
            .env(CRASH_AT_ENV, k.to_string())
            .env(ORDER_CHECK_ENV, "1"))
    }

    /// The crash sweep's child: adopts the history of the source store [`CHILD_SOURCE`] names,
    /// which it opens on the real filesystem, into the store at `root`, which it opens as the
    /// test hooks say.
    fn adopt_as_child(root: &Utf8Path) {
        let source = std::env::var(CHILD_SOURCE).unwrap();
        let src = LocalRepo::open_with_fs(Utf8Path::new(&source), Arc::new(RealFs::new()));
        let ids: Vec<ChangesetId> = (std::env::var(CHILD_IDS).unwrap().split(','))
            .map(|id| ChangesetId::new(id.parse().unwrap()))
            .collect();
        let (changesets, _blobs) = objects_of(&src.unwrap(), &ids);
        let repo = LocalRepo::open(root).unwrap();
        repo.adopt(changesets).unwrap();
    }

    /// Runs `run(k)` for k = 1, 2, … [`AT_ONCE`] at a time, until a run reports that its child
    /// finished; returns the last k a child crashed at. Every run past that one finishes too.
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
            assert!(first < 2_000, "the child never finished");
        }
    }

    /// One run of the sweep: a child adopts the history on a copy of `template`, which holds
    /// its blobs, until its k-th call aborts it. The store then holds none of the history or
    /// all of it, verifies clean, and takes the same adopt again, which leaves all of it.
    /// Returns whether the child finished, and whether the history was recorded.
    fn adopt_crashing_at(template: &Utf8Path, source: &Source, k: u64) -> (bool, bool) {
        let ids = &source.ids;
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
        copy_dir(template.as_std_path(), root.as_std_path());

        let child = crash_child(&root, source, k);
        let finished = child.status.success();
        assert!(
            finished || child.status.signal() == Some(SIGABRT),
            "k={k}: {:?}\n{}",
            child.status,
            String::from_utf8_lossy(&child.stderr)
        );

        // 1. None or all, all once the child returned, and clean.
        let repo = LocalRepo::open_with_fs(&root, checked(RealFs::new())).unwrap();
        let recorded = held(&repo, ids);
        assert!(
            recorded.is_empty() || recorded == *ids,
            "k={k}: {recorded:?}"
        );
        let adopted = !recorded.is_empty();
        assert!(
            adopted || !finished,
            "k={k}: the adopt returned, nothing held"
        );
        assert_eq!(whole(&repo, &recorded), Ok(()), "k={k}");
        assert_eq!(repo.verify(Depth::Deep).unwrap().problems, [], "k={k}");

        // 2. The same adopt again leaves all of it.
        let again = repo.adopt(source.changesets.clone()).unwrap();
        let expected = if adopted {
            (vec![], ids.clone())
        } else {
            (ids.clone(), vec![])
        };
        assert_eq!((again.adopted, again.present), expected, "k={k}");
        assert_eq!(repo.verify(Depth::Deep).unwrap().problems, [], "k={k}");
        (finished, adopted)
    }

    /// An adopt that crashes at any filesystem call leaves every changeset or none, in a
    /// store of each format; some crash comes after the commit point.
    #[test]
    fn adopt_crash_at_every_call_leaves_none_or_all() {
        if let Some(root) = std::env::var_os(CHILD_ROOT) {
            adopt_as_child(&Utf8PathBuf::from_path_buf(root.into()).unwrap());
            return;
        }
        let (src_dir, src, ids) = source();
        let (changesets, blobs) = objects_of(&src, &ids);
        let root = Utf8PathBuf::from_path_buf(src_dir.path().join("store")).unwrap();
        let source = Source {
            root,
            ids,
            changesets,
        };
        for format in FORMATS {
            let (_template_dir, template, repo) = target(format);
            repo.put_blobs(blobs.clone()).unwrap();
            drop(repo);
            let past_the_commit_point = AtomicBool::new(false);
            let calls = sweep(|k| {
                let (finished, adopted) = adopt_crashing_at(&template, &source, k);
                if adopted && !finished {
                    past_the_commit_point.store(true, Ordering::Relaxed);
                }
                finished
            });
            assert!(
                calls > 20,
                "{format:?}: the child finished after {calls} calls"
            );
            assert!(
                past_the_commit_point.load(Ordering::Relaxed),
                "{format:?}: no crash came after the commit point"
            );
        }
    }

    /// A recorded run: blobs put and the history adopted in two calls, on a store of its own.
    struct Recorded {
        /// The temp dir the run's store lay in.
        _dir: TempDir,
        /// The run's steps.
        timeline: Timeline,
        /// The changesets of each adopt call, in their order.
        batches: [Vec<ChangesetId>; 2],
    }

    /// Records the run of [`Recorded`] on a store of `format`.
    fn record(format: StoreFormat) -> Recorded {
        let (_src_dir, src, ids) = source();
        let (changesets, blobs) = objects_of(&src, &ids);
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let root = Utf8PathBuf::from_path_buf(base.join("store")).unwrap();
        let fs = Arc::new(ImageRecordingFs::new());
        let shared: Arc<dyn FsOps> = Arc::clone(&fs) as Arc<dyn FsOps>;

        // 1. The store, FORMAT 3's by an upgrade.
        let mut repo =
            LocalRepo::create_with_fs_as(&root, "power", StoreFormat::V2, Arc::clone(&shared))
                .unwrap();
        if format == StoreFormat::V3 {
            repo.upgrade().unwrap();
            drop(repo);
            repo = LocalRepo::open_with_fs(&root, Arc::clone(&shared)).unwrap();
        }
        fs.mark("adopt");

        // 2. The blobs, then the history in two adopts.
        repo.put_blobs(blobs).unwrap();
        let (first, rest) = changesets.split_at(FIRST_BATCH);
        repo.adopt(first.to_vec()).unwrap();
        fs.mark("adopted 1");
        repo.adopt(rest.to_vec()).unwrap();
        fs.mark("adopted 2");
        drop(repo);

        let (one, two) = ids.split_at(FIRST_BATCH);
        Recorded {
            timeline: Timeline::new(&base, &fs.calls()),
            batches: [one.to_vec(), two.to_vec()],
            _dir: dir,
        }
    }

    /// The image check of the store at `root`, `acked` of the run's adopts having returned
    /// before the power failed: it opens, verifies deep clean, holds each batch whole or not at
    /// all, the second only with the first and every acknowledged one, and no frame names a
    /// changeset whose trees or blobs are missing (W5).
    fn check_store(root: &Utf8Path, rec: &Recorded, acked: usize) -> Result<(), String> {
        let repo = LocalRepo::open_with_fs(root, Arc::new(RealFs::new()))
            .map_err(|e| format!("open: {e}"))?;
        let report = repo
            .verify(Depth::Deep)
            .map_err(|e| format!("verify: {e}"))?;
        if !report.problems.is_empty() {
            return Err(format!("verify --deep: {:?}", report.problems));
        }
        let mut whole_before = true;
        for (n, batch) in rec.batches.iter().enumerate() {
            let found = held(&repo, batch);
            let all = found.len() == batch.len();
            if !(found.is_empty() || all) {
                return Err(format!("batch {n}: {} of {}", found.len(), batch.len()));
            }
            if all && !whole_before {
                return Err(format!("batch {n} without the one before"));
            }
            if n < acked && !all {
                return Err(format!("batch {n} returned, then lost"));
            }
            whole(&repo, &found)?;
            whole_before = all;
        }
        Ok(())
    }

    /// [`check_store`] of `image`, written to a temp dir.
    fn check_image(image: &Image, rec: &Recorded, acked: usize) -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        image.materialize(dir.path()).map_err(|e| e.to_string())?;
        let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
        check_store(&root, rec, acked)
    }

    /// How many of the run's adopts had returned after the first `cut` steps.
    fn acked(rec: &Recorded, cut: usize) -> usize {
        ["adopted 1", "adopted 2"]
            .iter()
            .filter(|mark| cut >= rec.timeline.mark(mark))
            .count()
    }

    /// The images of a power loss after any step of the adopts of [`record`]'s run on a store
    /// of each format, for each of `seeds`: each must pass [`check_store`].
    fn sweep_images(seeds: &[u64]) {
        for format in FORMATS {
            let rec = record(format);
            let mut seen = HashSet::new();
            let mut held_some = false;
            for cut in rec.timeline.mark("adopt")..=rec.timeline.steps() {
                for &seed in seeds {
                    let image = rec.timeline.image(cut, &mut Seeded::new(seed, cut));
                    if !seen.insert(image.digest()) {
                        continue;
                    }
                    check_image(&image, &rec, acked(&rec, cut)).unwrap_or_else(|why| {
                        panic!(
                            "{format:?} cut {cut} seed {seed}: {why}\n{}",
                            image.describe()
                        )
                    });
                }
                held_some |= acked(&rec, cut) > 0;
            }
            assert!(held_some, "{format:?}: no cut after an adopt returned");
            assert!(seen.len() > 10, "{format:?}: {} images", seen.len());
        }
    }

    /// A power loss after any step of an adopt, the disk keeping nothing not durable (seed 0)
    /// or everything (seed 1), leaves a store that opens, verifies clean and holds each adopt's
    /// batch whole or not at all, every one that returned whole, never without the trees and
    /// blobs its changesets name (W5): in stores of format 2 and 3.
    #[test]
    fn adopt_power_loss_images_hold_whole_batches_or_none() {
        sweep_images(&SEEDS);
    }

    /// [`adopt_power_loss_images_hold_whole_batches_or_none`] for ten seeds: nothing kept,
    /// everything kept, and eight at random.
    #[test]
    #[ignore = "slow, ~60 s: images of every cut of two adopts x 10 seeds x 2 formats"]
    fn adopt_power_loss_images_hold_whole_batches_or_none_10_seeds() {
        sweep_images(&SEEDS_10);
    }

    /// The image check's own check: an image whose frame names a changeset with a tree the
    /// store lacks, as an adopt that framed before its trees were durable would leave, fails it.
    #[test]
    fn an_adopt_that_frames_before_its_trees_fails_the_image_check() {
        let rec = record(StoreFormat::V2);
        let last = rec.timeline.steps();
        let image = rec.timeline.image(last, &mut Seeded::new(1, last));
        let dir = tempfile::tempdir().unwrap();
        image.materialize(dir.path()).unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
        assert_eq!(check_store(&root, &rec, 2), Ok(()));

        // The newest changeset's root tree taken back, its frame kept.
        let newest = *rec.batches[1].last().unwrap();
        let repo = LocalRepo::open(&root).unwrap();
        let tree = repo.changeset(newest).unwrap().unwrap().changeset.root;
        drop(repo);
        std::fs::remove_file(layout_of(&root).object_path(tree.0)).unwrap();

        let repo = LocalRepo::open(&root).unwrap();
        let found = whole(&repo, &[newest]);
        assert!(
            found.as_ref().is_err_and(|why| why.contains("lacks tree")),
            "{found:?}"
        );
        assert!(check_store(&root, &rec, 2).is_err());
    }
}
