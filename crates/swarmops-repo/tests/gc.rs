//! Tests of `Repo::gc` (the M6 design §8): what it removes, counted by kind, and that it
//! removes nothing else, which a range detector measures on the store's tree before and after;
//! a dry run that removes nothing and reports the same; a second run that finds nothing; the
//! objects no changeset or fact reaches counted and kept; a held maintenance lease refused at
//! once; in FORMAT 3, the view work stepped once `meta/lock` is released (the FORMAT 3 design
//! §2.5 rule 1, §3.3).
//!
//! Every test owns a fresh store (`tempfile::tempdir()`).

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::time::{Duration, Instant, SystemTime};

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_core::ObjectKind;
use swarmops_repo::{GcReport, GcRequest, LocalRepo, RepoError, StoreError};
use swarmops_store::{GC_TEMP_GRACE, LeftoverKind, MaintLease, StoreFormat};
use swarmops_testkit::repo::{commit, new_repo, new_repo_as, put};
use swarmops_testkit::store::layout_of;

/// A temp file this much past the grace.
const STALE: Duration = Duration::from_hours(1);

/// Writes `bytes` to `rel` under `root`, its parents made.
fn write(root: &Utf8Path, rel: &str, bytes: &[u8]) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

/// Sets the mtime of every regular file at or under `rel` to past the grace.
fn make_stale(root: &Utf8Path, rel: &str) {
    for path in tree(root).into_iter().filter(|path| path.starts_with(rel)) {
        let path = root.join(path);
        if fs::symlink_metadata(&path).unwrap().is_file() {
            let file = File::options().write(true).open(&path).unwrap();
            file.set_modified(SystemTime::now() - GC_TEMP_GRACE - STALE)
                .unwrap();
        }
    }
}

/// Every path under `root`, files and directories, relative to it: the range detector's view
/// of a store.
fn tree(root: &Utf8Path) -> BTreeSet<Utf8PathBuf> {
    let mut paths = BTreeSet::new();
    let mut stack = vec![root.to_owned()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = Utf8PathBuf::from_path_buf(entry.unwrap().path()).unwrap();
            if fs::symlink_metadata(&path).unwrap().is_dir() {
                stack.push(path.clone());
            }
            paths.insert(path.strip_prefix(root).unwrap().to_owned());
        }
    }
    paths
}

/// Whether `rel` lies in a class of leftover of the M6 design §8.2 and the FORMAT 3 design §3.3,
/// in a store of `format` (1, 2 or 3): restated here from the designs, not from the code under
/// test.
fn in_a_class(rel: &Utf8Path, format: u32) -> bool {
    let parts: Vec<&str> = rel.iter().collect();
    let residue_of = |stem: &str, refs: &str| {
        (parts.len() >= 2 && parts[0] == "meta")
            && (parts[1] == refs
                || [".log", ".off", ".ckpt"]
                    .iter()
                    .any(|ext| parts[1] == format!("{stem}{ext}")))
    };
    // FORMAT 3's own files, in a store an upgrade did not take to FORMAT 3.
    let format_3_files = matches!(
        parts.as_slice(),
        ["meta", "sorted3", ..]
            | ["meta", "journal3.ckpt" | "features" | "views.lock"]
            | ["meta", "idx", "facts.col"]
    );
    match parts.as_slice() {
        ["objects", "tmp", _] | ["meta", "derived", "tmp", _] | ["meta.tmp", ..] => true,
        // Below the built-ins' version, 1.
        [
            "meta",
            "derived",
            "first_parent_skip_list" | "last_modified",
            version,
            ..,
        ] => version.parse::<u32>().is_ok_and(|version| version < 1),
        // FORMAT 3: FORMAT 1's files, FORMAT 2's views, and the runs a view step replaces or
        // left, never FORMAT 2's journal, its poison or its ref files.
        ["meta", "sorted", ..] | ["meta", "sorted3", "tmp", ..] if format == 3 => true,
        ["meta", "sorted3", run] if format == 3 => Utf8Path::new(run).extension() == Some("run"),
        _ if format == 3 => residue_of("journal", "refs"),
        _ if format == 2 => residue_of("journal", "refs") || format_3_files,
        _ => residue_of("journal2", "refs2") || format_3_files,
    }
}

/// The paths `before` held that `after` does not, and that lie in no class of leftover.
fn out_of_range(
    before: &BTreeSet<Utf8PathBuf>,
    after: &BTreeSet<Utf8PathBuf>,
    format: u32,
) -> Vec<Utf8PathBuf> {
    before
        .difference(after)
        .filter(|rel| !in_a_class(rel, format))
        .cloned()
        .collect()
}

/// A store with a changeset, and one leftover of each class a FORMAT 1 store can hold, beside
/// what gc never removes: an old temp-like file in `git/`, `workspaces/` and `quarantine/`.
fn store_with_leftovers() -> (tempfile::TempDir, Utf8PathBuf, LocalRepo) {
    let (dir, root, repo) = new_repo_as(StoreFormat::V1);
    commit(&repo, &[], vec![put("a.txt", b"a")]);
    write(&root, "objects/tmp/.tmpold", b"12345");
    write(&root, "meta/derived/tmp/.tmpold", b"123");
    write(&root, "meta.tmp/meta/journal.log", b"1");
    // Below version 1, the built-in's version.
    write(&root, "meta/derived/last_modified/0/ab/cd", b"12");
    write(&root, "meta/journal2.log", b"1234");
    for kept in [
        "git/mirror.git/HEAD",
        "workspaces/w/file",
        "quarantine/objects/x",
        "meta/derived/someone_elses/0/ab/cd",
    ] {
        write(&root, kept, b"kept");
    }
    for stale in [
        "objects/tmp",
        "meta/derived/tmp",
        "git",
        "workspaces",
        "quarantine",
    ] {
        make_stale(&root, stale);
    }
    (dir, root, repo)
}

/// The counts `report` gives each kind.
fn counts(report: &GcReport) -> Vec<(LeftoverKind, u64)> {
    report.removed.clone()
}

/// gc removes one of each class, and reports them by kind with their bytes; the range detector
/// finds nothing removed outside the classes, and the store's objects, journal and refs whole.
#[test]
fn gc_removes_each_class_and_nothing_else() {
    let (_dir, root, repo) = store_with_leftovers();
    let before = tree(&root);

    let report = repo.gc(GcRequest::new(false)).unwrap();
    assert_eq!(
        counts(&report),
        [
            (LeftoverKind::ObjectTemp, 1),
            (LeftoverKind::DerivedTemp, 1),
            (LeftoverKind::MetaStaging, 1),
            (LeftoverKind::DerivedVersion, 1),
            (LeftoverKind::UpgradeResidue, 1),
        ]
    );
    assert_eq!(report.bytes, 5 + 3 + 1 + 2 + 4);
    assert_eq!(report.orphan_objects, 0);

    let after = tree(&root);
    assert_eq!(out_of_range(&before, &after, 1), Vec::<Utf8PathBuf>::new());
    for kept in [
        "git/mirror.git/HEAD",
        "workspaces/w/file",
        "quarantine/objects/x",
        "meta/derived/someone_elses/0/ab/cd",
    ] {
        assert!(after.contains(Utf8Path::new(kept)), "{kept}");
    }
    let removed: Vec<_> = before.difference(&after).collect();
    assert!(
        removed.contains(&&Utf8PathBuf::from("meta.tmp")),
        "{removed:?}"
    );

    let report = repo.verify(swarmops_repo::Depth::Deep).unwrap();
    assert!(report.problems.is_empty(), "{:?}", report.problems);
}

/// A dry run removes nothing, the tree is as it was, and it reports what the gc after it removes;
/// a second gc finds nothing.
#[test]
fn a_dry_run_removes_nothing_and_a_second_gc_finds_nothing() {
    let (_dir, root, repo) = store_with_leftovers();
    let before = tree(&root);

    let dry = repo.gc(GcRequest::new(true)).unwrap();
    // The lease file is made by the first lease taken: the one path gc adds.
    let mut after_dry = tree(&root);
    after_dry.remove(Utf8Path::new("meta/maint.lock"));
    assert_eq!(after_dry, before);

    let real = repo.gc(GcRequest::new(false)).unwrap();
    assert_eq!(dry, real);
    assert_eq!(dry.removed.len(), 5, "{dry:?}");

    let again = repo.gc(GcRequest::new(false)).unwrap();
    assert_eq!(again.removed, []);
    assert_eq!(again.bytes, 0);
}

/// An object file no changeset reaches is counted, and kept; one a changeset reaches is not.
#[test]
fn orphan_objects_are_counted_and_kept() {
    let (_dir, root, repo) = new_repo();
    commit(&repo, &[], vec![put("a.txt", b"a")]);
    assert_eq!(repo.gc(GcRequest::new(false)).unwrap().orphan_objects, 0);

    let ids = repo
        .put_objects(vec![(ObjectKind::Blob, b"no changeset names me".to_vec())])
        .unwrap();
    let report = repo.gc(GcRequest::new(false)).unwrap();
    assert_eq!(report.orphan_objects, 1);
    assert!(
        repo.contains_object(ids[0]).unwrap(),
        "the orphan was removed"
    );
    assert!(layout_of(&root).object_path(ids[0]).is_file());
}

/// A held lease, another gc's, repair's or upgrade's, refuses gc at once.
#[test]
fn a_held_lease_refuses_gc_at_once() {
    let (_dir, root, repo) = store_with_leftovers();
    let held = MaintLease::try_acquire(&layout_of(&root)).unwrap();
    let before = tree(&root);

    let asked = Instant::now();
    let refused = repo.gc(GcRequest::new(false));
    assert!(
        matches!(refused, Err(RepoError::Store(StoreError::MaintenanceBusy))),
        "{refused:?}"
    );
    assert!(asked.elapsed() < Duration::from_secs(1), "it waited");
    assert!(tree(&root) == before, "a refused gc removed something");

    drop(held);
    repo.gc(GcRequest::new(false)).expect("free once dropped");
}

/// The detector catches a gc that removes outside the classes: a fake gc that removes an
/// object file, and one that removes FORMAT 1's journal in a FORMAT 1 store.
#[test]
fn the_range_detector_catches_a_gc_that_removes_an_object() {
    let (_dir, root, repo) = new_repo_as(StoreFormat::V1);
    let id = commit(&repo, &[], vec![put("a.txt", b"a")]);
    let before = tree(&root);

    let object = layout_of(&root).object_path(id.0);
    fs::remove_file(&object).unwrap();
    let caught = out_of_range(&before, &tree(&root), 1);
    let rel = object.strip_prefix(&root).unwrap().to_owned();
    assert_eq!(caught, [rel]);

    fs::remove_file(root.join("meta/journal.off")).unwrap();
    let caught = out_of_range(&before, &tree(&root), 1);
    assert!(
        caught.contains(&Utf8PathBuf::from("meta/journal.off")),
        "{caught:?}"
    );
}

/// gc steps the view work a FORMAT 3 store's commits left below the backlog bound: once it ran,
/// the runs cover what the checkpoint passed (the FORMAT 3 design's amendment of 2026-11-11,
/// G6). `init` makes no FORMAT 3 store yet, so a FORMAT 2 repository's metadata is swapped for
/// a FORMAT 3 one; a dry run steps nothing.
#[test]
fn gc_catches_up_an_unstepped_tail() {
    use std::sync::Arc;

    use swarmops_core::{ChangesetId, hash_object};
    use swarmops_store::{ChangesetMeta, Layout, MetaStore, RealFs};

    // 1. The FORMAT 3 metadata, and 3,000 changesets in two frames: no step below the bound.
    let (_dir, root, repo) = new_repo_as(StoreFormat::V2);
    drop(repo);
    fs::remove_dir_all(root.join("meta")).unwrap();
    let layout = Layout::with_format(&root, StoreFormat::V3);
    let meta = MetaStore::create_in(layout.clone(), Arc::new(RealFs::new())).unwrap();
    fs::write(layout.format_file(), StoreFormat::V3.marker()).unwrap();
    for range in [0..1_500, 1_500..3_000] {
        let mut write = meta.write().unwrap();
        let changesets = range.map(|n: u64| {
            let id = ChangesetId::new(hash_object(ObjectKind::Changeset, &n.to_le_bytes()));
            let delta = hash_object(ObjectKind::Blob, &n.to_le_bytes()).0;
            ChangesetMeta::new(id, 0, 0, delta, Vec::new())
        });
        write.insert_changesets_batch(changesets.collect()).unwrap();
        write.commit().unwrap();
    }
    let run = layout
        .views_dir()
        .join("c-0000000000000000-0000000000000bb8.run");
    assert!(!fs::exists(&run).unwrap(), "no run before gc");

    // 2. A dry run builds nothing; a gc builds the run of all 3,000.
    let repo = LocalRepo::open(&root).unwrap();
    repo.gc(GcRequest::new(true)).unwrap();
    assert!(!fs::exists(&run).unwrap(), "a dry run writes nothing");
    repo.gc(GcRequest::new(false)).unwrap();
    assert!(fs::exists(&run).unwrap(), "gc built the run");
}

/// A FORMAT 3 repository: one of FORMAT 2 with a changeset, upgraded (no `init` makes FORMAT 3
/// yet).
fn new_v3_repo() -> (tempfile::TempDir, Utf8PathBuf, LocalRepo) {
    let (dir, root, repo) = new_repo_as(StoreFormat::V2);
    commit(&repo, &[], vec![put("a.txt", b"a")]);
    assert!(repo.upgrade().unwrap().upgraded);
    let repo = LocalRepo::open(&root).unwrap();
    assert_eq!(repo.format(), StoreFormat::V3);
    (dir, root, repo)
}

/// In a FORMAT 3 store, gc removes FORMAT 2's views and nothing out of the classes; the detector
/// flags a fake gc that removes `journal2.log`, which FORMAT 3 shares with FORMAT 2, or the poison
/// `journal2.ckpt`.
#[test]
fn gc_scope_detector_flags_a_fake_gc_removing_journal2_log() {
    let (_dir, root, repo) = new_v3_repo();
    let before = tree(&root);

    // 1. The real gc: FORMAT 2's views, and only what lies in a class.
    let report = repo.gc(GcRequest::new(false)).unwrap();
    assert_eq!(counts(&report), [(LeftoverKind::UpgradeResidue, 1)]);
    let after = tree(&root);
    assert_eq!(out_of_range(&before, &after, 3), Vec::<Utf8PathBuf>::new());
    assert!(!after.contains(Utf8Path::new("meta/sorted")));

    // 2. Fakes: each removal is flagged.
    for rel in [
        "meta/journal2.log",
        "meta/journal2.ckpt",
        "meta/journal3.ckpt",
    ] {
        fs::remove_file(root.join(rel)).unwrap();
        let caught = out_of_range(&after, &tree(&root), 3);
        assert!(
            caught.contains(&Utf8PathBuf::from(rel)),
            "{rel}: {caught:?}"
        );
    }
}

/// gc steps the view work only once it released `meta/lock`: when the step writes its run, the
/// lock is free (the FORMAT 3 design §2.5 rule 1, lock order rule 8).
#[test]
fn gc_steps_the_views_outside_the_lock() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU8, Ordering};

    use swarmops_store::{ChangesetMeta, MetaLock, MetaStore};
    use swarmops_testkit::fs::{RecordingFs, When};

    /// What the probe saw of `meta/lock` as the step wrote its run.
    const UNSEEN: u8 = 0;
    const FREE: u8 = 1;
    const HELD: u8 = 2;

    // 1. A FORMAT 3 store with more than a run's worth of changesets past its runs.
    let (_dir, root, repo) = new_v3_repo();
    drop(repo);
    let meta = MetaStore::open(&root).unwrap();
    let mut write = meta.write().unwrap();
    let changesets = (1..=2_000_u64).map(|n| {
        let id = swarmops_core::ChangesetId::new(swarmops_core::hash_object(
            ObjectKind::Changeset,
            &n.to_le_bytes(),
        ));
        let delta = swarmops_core::hash_object(ObjectKind::Blob, &n.to_le_bytes()).0;
        ChangesetMeta::new(id, 0, 0, delta, Vec::new())
    });
    write.insert_changesets_batch(changesets.collect()).unwrap();
    write.commit().unwrap();
    meta.flush().unwrap();

    // 2. A gc whose filesystem, as the step writes its run, tries meta/lock: FREE if it was.
    let seen = Arc::new(AtomicU8::new(UNSEEN));
    let layout = layout_of(&root);
    let run = "append meta/sorted3/tmp/c-0000000000000000-00000000000007d1.run";
    let probe = {
        let seen = Arc::clone(&seen);
        let layout = layout.clone();
        Box::new(move || {
            let free = MetaLock::acquire(&layout, Duration::ZERO).is_ok();
            seen.store(if free { FREE } else { HELD }, Ordering::SeqCst);
        })
    };
    let (recording, _calls) = RecordingFs::new();
    let recording = recording
        .naming_paths_in(root.clone().into_std_path_buf())
        .with_hook((run.to_owned(), When::Before, probe));
    let repo = LocalRepo::open_with_fs(&root, Arc::new(recording)).unwrap();
    repo.gc(GcRequest::new(false)).unwrap();
    assert_eq!(
        seen.load(Ordering::SeqCst),
        FREE,
        "the step ran under meta/lock or not at all"
    );
}

/// A blob that only a fact names is a root of gc's count: it is no orphan (the FORMAT 3 design
/// §2.8); a blob nothing names is one.
#[cfg(feature = "test-hooks")]
#[test]
fn a_fact_named_blob_is_not_an_orphan() {
    use swarmops_store::{Expect, Fact, FactKind, MetaStore, supported_for_tests};

    let _features = supported_for_tests(&["review"]);
    let (_dir, root, repo) = new_v3_repo();
    let named = repo
        .put_objects(vec![
            (ObjectKind::Blob, b"why the change".to_vec()),
            (ObjectKind::Blob, b"its title".to_vec()),
        ])
        .unwrap();
    assert_eq!(repo.gc(GcRequest::new(false)).unwrap().orphan_objects, 2);

    // A Review Open naming both: its intent and its title.
    let (intent, title) = (named[0].0, named[1].0);
    let body = [&[0; 12][..], &[4], b"main", &[1; 32], &intent, &title].concat();
    let open = Fact::new(
        FactKind::Review,
        0,
        [7; 32],
        [6; 32],
        1,
        "test/1".to_owned(),
        body,
    );
    let meta = MetaStore::open(&root).unwrap();
    let mut write = meta.write().unwrap();
    write.put_fact(open.unwrap(), Expect::Any).unwrap();
    write.commit().unwrap();

    let report = repo.gc(GcRequest::new(false)).unwrap();
    assert_eq!(report.orphan_objects, 0);
    assert!(repo.contains_object(named[0]).unwrap());
}
