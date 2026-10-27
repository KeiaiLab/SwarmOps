//! Tests of `Repo::gc` (the M6 design §8): what it removes, counted by kind, and that it
//! removes nothing else, which a range detector measures on the store's tree before and after;
//! a dry run that removes nothing and reports the same; a second run that finds nothing; the
//! objects no changeset reaches counted and kept; a held maintenance lease refused at once.
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

/// Whether `rel` lies in a class of leftover of the M6 design §8.2, in a store of `format`
/// (1 or 2): restated here from the design, not from the code under test.
fn in_a_class(rel: &Utf8Path, format: u32) -> bool {
    let parts: Vec<&str> = rel.iter().collect();
    let residue_of = |stem: &str, refs: &str| {
        (parts.len() >= 2 && parts[0] == "meta")
            && (parts[1] == refs
                || [".log", ".off", ".ckpt"]
                    .iter()
                    .any(|ext| parts[1] == format!("{stem}{ext}")))
    };
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
        _ if format == 2 => residue_of("journal", "refs"),
        _ => residue_of("journal2", "refs2"),
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
