//! Tests of gc of leftovers (the M6 design §8): each class of leftover listed and removed, and
//! what is not one kept; the poisoned checkpoint of an upgrade removed last, so that a gc cut
//! short finishes on the next run; the maintenance lease, never waited for, taken before
//! `meta/lock`.
//!
//! Every test owns a fresh store root (`tempfile::tempdir()`).

use std::fs::{self, File};
use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use camino::Utf8Path;
use swarmops_store::gc::{self, LeftoverKind};
use swarmops_store::{
    FsOps, GC_TEMP_GRACE, Layout, Leftover, MaintLease, MetaStore, RealFs, StoreError, StoreFormat,
    StoreFs, UPGRADE_POISON,
};
use swarmops_testkit::store::new_root;
use tempfile::TempDir;

/// A temp file past the grace by this much.
const STALE: Duration = Duration::from_hours(1);

/// A FORMAT 1 store root with `meta/` made and `FORMAT` written, and its layout.
fn new_store() -> (TempDir, Layout) {
    let (dir, layout) = new_root();
    MetaStore::create_in(layout.clone(), Arc::new(RealFs::new())).expect("create meta/");
    fs::write(layout.format_file(), StoreFormat::V1.marker()).unwrap();
    (dir, layout)
}

/// Writes `bytes` to `rel` under `root`, its parents made.
fn write(root: &Utf8Path, rel: &str, bytes: &[u8]) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

/// Sets the mtime of `rel` under `root` to `age` before now.
fn age(root: &Utf8Path, rel: &str, age: Duration) {
    let file = File::options().write(true).open(root.join(rel)).unwrap();
    file.set_modified(SystemTime::now() - age).unwrap();
}

/// The kind and the path of each leftover, in the order listed.
fn names(leftovers: &[Leftover]) -> Vec<(LeftoverKind, String)> {
    leftovers
        .iter()
        .map(|leftover| (leftover.kind, leftover.rel.to_string()))
        .collect()
}

/// The store's leftovers now, as this build's gc lists them with `derivations` registered.
fn listed(layout: &Layout, derivations: &[(&str, u32)]) -> Vec<Leftover> {
    gc::list_leftovers_of(layout, SystemTime::now(), derivations).unwrap()
}

/// Removes each of `leftovers` through the real filesystem.
fn remove_all(layout: &Layout, leftovers: &[Leftover]) {
    let fs = StoreFs::new(Arc::new(RealFs::new()), layout.root());
    for leftover in leftovers {
        gc::remove(&fs, leftover).unwrap();
    }
}

/// Whether `rel` is there under `root` (not following a symlink).
fn exists(root: &Utf8Path, rel: &str) -> bool {
    fs::symlink_metadata(root.join(rel)).is_ok()
}

/// A temp file of `objects/tmp/` or `meta/derived/tmp/` older than the grace is one; a younger
/// one, which a write in progress may hold, is kept. Removing it leaves nothing to list.
#[test]
fn temp_files_past_the_grace_are_removed_younger_ones_kept() {
    let (_dir, layout) = new_store();
    let root = layout.root();
    for (rel, old) in [
        ("objects/tmp/.tmpold", true),
        ("objects/tmp/.tmpnew", false),
        ("meta/derived/tmp/.tmpold", true),
        ("meta/derived/tmp/.tmpnew", false),
    ] {
        write(root, rel, b"12345");
        let by = if old { GC_TEMP_GRACE + STALE } else { STALE };
        age(root, rel, by);
    }

    let found = listed(&layout, &[]);
    assert_eq!(
        names(&found),
        [
            (LeftoverKind::ObjectTemp, "objects/tmp/.tmpold".to_owned()),
            (
                LeftoverKind::DerivedTemp,
                "meta/derived/tmp/.tmpold".to_owned()
            ),
        ]
    );
    assert!(found.iter().all(|leftover| leftover.bytes == 5));
    remove_all(&layout, &found);

    assert!(!exists(root, "objects/tmp/.tmpold"));
    assert!(!exists(root, "meta/derived/tmp/.tmpold"));
    assert!(exists(root, "objects/tmp/.tmpnew"));
    assert!(exists(root, "meta/derived/tmp/.tmpnew"));
    assert_eq!(names(&listed(&layout, &[])), []);
}

/// `meta.tmp/`, what a create that crashed staged, is one once `meta/` exists, removed with all
/// it holds; without `meta/` it is kept, since a create may be staging there.
#[test]
fn meta_staging_is_removed_only_beside_meta() {
    let (_dir, layout) = new_store();
    let root = layout.root();
    write(root, "meta.tmp/meta/journal.log", b"abc");
    write(root, "meta.tmp/meta/idx/ids.col", b"de");

    let found = listed(&layout, &[]);
    assert_eq!(
        names(&found),
        [(LeftoverKind::MetaStaging, "meta.tmp".to_owned())]
    );
    assert_eq!(found[0].bytes, 5);
    remove_all(&layout, &found);
    assert!(!exists(root, "meta.tmp"));

    let (_bare, bare) = new_root();
    write(bare.root(), "meta.tmp/meta/journal.log", b"abc");
    assert_eq!(names(&listed(&bare, &[])), []);
}

/// A version directory of a registered derivation below its version is one; its version, a
/// later one, a name no derivation registers and a directory that names no version are kept.
#[test]
fn superseded_derived_versions_are_removed_unknown_names_kept() {
    let (_dir, layout) = new_store();
    let root = layout.root();
    let value = "ab/cdef";
    for version in ["last_modified/1", "last_modified/2", "last_modified/3"] {
        write(root, &format!("meta/derived/{version}/{value}"), b"v");
    }
    write(root, &format!("meta/derived/someone_elses/1/{value}"), b"v");
    write(
        root,
        &format!("meta/derived/last_modified/01/{value}"),
        b"v",
    );
    write(root, &format!("meta/derived/last_modified/x/{value}"), b"v");

    // Registered at 2: version 1 is superseded.
    let found = listed(&layout, &[("last_modified", 2)]);
    assert_eq!(
        names(&found),
        [(
            LeftoverKind::DerivedVersion,
            "meta/derived/last_modified/1".to_owned()
        )]
    );
    assert_eq!(found[0].bytes, 1);
    remove_all(&layout, &found);
    assert!(!exists(root, "meta/derived/last_modified/1"));
    for kept in ["last_modified/2", "last_modified/3", "someone_elses/1"] {
        assert!(exists(root, &format!("meta/derived/{kept}")), "{kept}");
    }
    assert!(exists(root, "meta/derived/last_modified/01"));
    assert!(exists(root, "meta/derived/last_modified/x"));

    // No derivation registered: no name is known, and none is a leftover.
    assert_eq!(
        names(&gc::list_leftovers(&layout, SystemTime::now()).unwrap()),
        []
    );
}

/// Lays out what an upgrade leaves of FORMAT 1 in the FORMAT 2 store at `root`: `FORMAT` says 2,
/// FORMAT 1's journal and ref files are still there, and its checkpoint is `ckpt`.
fn upgraded_leaving(root: &Utf8Path, ckpt: &[u8]) -> Layout {
    let layout = Layout::with_format(root, StoreFormat::V2);
    fs::create_dir_all(layout.meta_dir()).unwrap();
    fs::write(layout.format_file(), StoreFormat::V2.marker()).unwrap();
    write(root, "meta/journal.log", b"frames");
    write(root, "meta/journal.off", b"offsets!");
    write(root, "meta/refs/main", b"ref\n");
    write(root, "meta/refs/release/1", b"ref\n");
    write(root, "meta/journal.ckpt", ckpt);
    write(root, "meta/journal2.log", b"kept");
    layout
}

/// In a FORMAT 2 store whose FORMAT 1 checkpoint is the poison, FORMAT 1's files are the
/// upgrade's residue: `journal.log`, `journal.off` and `refs/` first, the poisoned checkpoint
/// last. With any other checkpoint they are kept; the store's own files always are.
#[test]
fn upgrade_residue_goes_poisoned_checkpoint_last() {
    let (dir, _) = new_root();
    let root = Utf8Path::from_path(dir.path()).unwrap();
    let layout = upgraded_leaving(root, UPGRADE_POISON);

    let found = listed(&layout, &[]);
    let residue = |rel: &str| (LeftoverKind::UpgradeResidue, rel.to_owned());
    assert_eq!(
        names(&found),
        [
            residue("meta/journal.log"),
            residue("meta/journal.off"),
            residue("meta/refs"),
            residue("meta/journal.ckpt"),
        ]
    );
    remove_all(&layout, &found);
    for gone in ["journal.log", "journal.off", "refs", "journal.ckpt"] {
        assert!(!exists(root, &format!("meta/{gone}")), "{gone}");
    }
    assert!(exists(root, "meta/journal2.log"));
    assert_eq!(names(&listed(&layout, &[])), []);

    // Not the poison (8 bytes: a FORMAT 1 checkpoint): nothing of it is touched.
    let (other, _) = new_root();
    let other = Utf8Path::from_path(other.path()).unwrap();
    let layout = upgraded_leaving(other, &[0; 8]);
    assert_eq!(names(&listed(&layout, &[])), []);
}

/// In a FORMAT 1 store, an upgrade that stopped before its poison left `journal2.*` and
/// `refs2/`: residue. Once the poison is written (an upgrade between its steps 5 and 6, which the
/// next open finishes) they are the store's next files, and kept.
#[test]
fn format_2_files_of_a_stopped_upgrade_go_only_without_the_poison() {
    let (_dir, layout) = new_store();
    let root = layout.root();
    for rel in [
        "journal2.log",
        "journal2.off",
        "journal2.ckpt",
        "refs2/0123",
    ] {
        write(root, &format!("meta/{rel}"), b"x");
    }

    let ckpt = fs::read(layout.journal_ckpt()).unwrap();
    fs::write(layout.journal_ckpt(), UPGRADE_POISON).unwrap();
    assert_eq!(names(&listed(&layout, &[])), []);
    fs::write(layout.journal_ckpt(), ckpt).unwrap();

    let found = listed(&layout, &[]);
    let residue = |rel: &str| (LeftoverKind::UpgradeResidue, rel.to_owned());
    assert_eq!(
        names(&found),
        [
            residue("meta/journal2.ckpt"),
            residue("meta/journal2.log"),
            residue("meta/journal2.off"),
            residue("meta/refs2"),
        ]
    );
    remove_all(&layout, &found);
    assert!(!exists(root, "meta/refs2"));
    assert!(exists(root, "meta/journal.log"));
    assert!(exists(root, "meta/refs"));
    MetaStore::open_in(layout.clone(), Arc::new(RealFs::new())).expect("the store still opens");
}

/// A gc cut short after it removed `journal.log` (the removal of `journal.off` fails) leaves the
/// poisoned checkpoint, so the next gc finds the rest and removes it.
#[cfg(feature = "test-hooks")]
#[test]
fn a_gc_cut_after_journal_log_finishes_on_the_next_run() {
    use swarmops_store::{FailAt, FailAtFs, FailMode, InjectedError};

    let (dir, _) = new_root();
    let root = Utf8Path::from_path(dir.path()).unwrap();
    let layout = upgraded_leaving(root, UPGRADE_POISON);

    // A file's removal is two calls, the remove and its directory's fsync: call 3 is the
    // removal of journal.off.
    let fail = FailAt::new(3, FailMode::Once, InjectedError::Eio);
    let failing: Arc<dyn FsOps> = Arc::new(FailAtFs::new(RealFs::new(), fail));
    let fs = StoreFs::new(failing, root);
    let found = listed(&layout, &[]);
    let cut = found
        .iter()
        .try_for_each(|leftover| gc::remove(&fs, leftover));
    assert!(matches!(cut, Err(StoreError::Io(_))), "{cut:?}");
    assert!(!exists(root, "meta/journal.log"));
    assert_eq!(
        fs::read(root.join("meta/journal.ckpt")).unwrap(),
        UPGRADE_POISON
    );

    let rest = listed(&layout, &[]);
    let residue = |rel: &str| (LeftoverKind::UpgradeResidue, rel.to_owned());
    assert_eq!(
        names(&rest),
        [
            residue("meta/journal.off"),
            residue("meta/refs"),
            residue("meta/journal.ckpt"),
        ]
    );
    remove_all(&layout, &rest);
    assert_eq!(names(&listed(&layout, &[])), []);
}

/// A removal of a leftover gone already succeeds: a second gc finds nothing, and a removal
/// raced by another finds nothing to do.
#[test]
fn removing_twice_is_removing_once() {
    let (_dir, layout) = new_store();
    write(layout.root(), "meta.tmp/meta/x", b"x");
    let found = listed(&layout, &[]);
    remove_all(&layout, &found);
    remove_all(&layout, &found);
    assert_eq!(names(&listed(&layout, &[])), []);
}

/// A held lease refuses another at once, in this process too, with `MaintenanceBusy`; dropped,
/// it is free again.
#[test]
fn a_held_lease_refuses_another_at_once() {
    let (_dir, layout) = new_store();
    let held = MaintLease::try_acquire(&layout).unwrap();

    let asked = Instant::now();
    let refused = MaintLease::try_acquire(&layout);
    assert!(
        matches!(refused, Err(StoreError::MaintenanceBusy)),
        "{refused:?}"
    );
    assert!(asked.elapsed() < Duration::from_secs(1), "it waited");

    drop(held);
    MaintLease::try_acquire(&layout).expect("free once dropped");
}

/// A lease file that is no regular file is refused, unopened, as `meta/lock` is.
#[test]
fn a_lease_file_that_is_no_regular_file_is_refused() {
    let (_dir, layout) = new_store();
    fs::create_dir(layout.meta_path("maint.lock")).unwrap();
    let Err(StoreError::Io(failure)) = MaintLease::try_acquire(&layout) else {
        panic!("a directory taken as the lease file");
    };
    assert_eq!(failure.source.kind(), std::io::ErrorKind::InvalidData);
    assert_eq!(
        failure.file.as_deref(),
        Some(Utf8Path::new("meta/maint.lock"))
    );
}

/// The order is the lease, then `meta/lock`: taken so, both are held at once; a lease asked for
/// while a `meta/lock` taken under one is held is the reverse order, and panics.
#[test]
fn the_lease_comes_before_meta_lock() {
    let (_dir, layout) = new_store();
    let lease = MaintLease::try_acquire(&layout).unwrap();
    let meta = lease.meta_lock(&layout, Duration::ZERO).unwrap();

    drop(lease);
    let reversed = panic::catch_unwind(AssertUnwindSafe(|| MaintLease::try_acquire(&layout)));
    let message = reversed.expect_err("the reverse order was taken");
    let message = message.downcast_ref::<String>().expect("a formatted panic");
    assert!(message.contains("lock order"), "{message}");

    drop(meta);
    let lease = MaintLease::try_acquire(&layout).expect("in order again");
    drop(lease.meta_lock(&layout, Duration::ZERO).unwrap());
}
