//! FORMAT 3's view work and feature list under crashes and power loss (the FORMAT 3 design §2.5
//! rule 7, §2.6, §7): a view step stopped at any of its calls leaves the runs a reader reads, the
//! old manifest or the new one, and the next step finishes; a power loss during a step leaves no
//! manifest that names a missing run (W4′); a write that enables a feature, stopped at any of its
//! calls, leaves no record of a feature the list does not hold (W10).
//!
//! Every run of the swept code goes through the order checker. The stores are made with
//! `MetaStore::create_in` of a FORMAT 3 layout.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_core::{ChangesetId, ObjectKind, hash_object};
use swarmops_store::{
    CasResult, ChangesetMeta, CrashAtFs, Expect, Fact, FactKind, FailAt, FailAtFs, FailMode, FsOps,
    InjectedError, Layout, MetaStore, Step, StoreError, StoreFormat, supported_for_tests,
};
use swarmops_testkit::faults;
use swarmops_testkit::fs::NoSyncFs;
use swarmops_testkit::power::{ImageRecordingFs, Seeded, Timeline};
use swarmops_testkit::store::sweep::copy_of;
use swarmops_testkit::store::{new_root, real_fs, temp_dir};
use tempfile::TempDir;

/// The records the template's first run covers.
const FIRST_RUN: u64 = 2_000;

/// The records the template holds: past its run, a tail longer than `RUN_MIN`, so that the
/// swept step builds a run, merges it with the first and removes that one.
const RECORDS: u64 = 3_500;

/// The changesets every check looks up: the ends of both runs, and one past the last.
const SAMPLES: [u64; 4] = [0, FIRST_RUN - 1, FIRST_RUN, RECORDS - 1];

/// The seeds of the power-loss images taken at each cut: the least, the most, and random ones.
const SEEDS: std::ops::Range<u64> = 0..6;

/// The feature the feature sweep's fact is of.
const FEATURE: &str = "lineage";

/// The store's root in the power-loss recording's base directory.
const STORE: &str = "store";

/// What the power-loss recording marks once the commits are done, before the step.
const STEP_MARK: &str = "step";

/// Changeset `n`'s id.
fn id(n: u64) -> ChangesetId {
    ChangesetId::new(hash_object(ObjectKind::Changeset, &n.to_le_bytes()))
}

/// Changeset `n`, a root.
fn changeset(n: u64) -> ChangesetMeta {
    let delta_hash = hash_object(ObjectKind::Blob, &(n % 97).to_le_bytes()).0;
    ChangesetMeta::new(id(n), 0, 0, delta_hash, Vec::new())
}

/// The FORMAT 3 layout of the store at `root`.
fn layout(root: &Utf8Path) -> Layout {
    Layout::with_format(root, StoreFormat::V3)
}

/// Commits changesets `range` as one frame.
fn insert(store: &MetaStore, range: std::ops::Range<u64>) {
    let mut write = store.write().unwrap();
    write
        .insert_changesets_batch(range.map(changeset).collect())
        .unwrap();
    write.commit().unwrap();
}

/// A new FORMAT 3 store at `layout` on `fs`, its index synced from one thread.
fn create(layout: Layout, fs: Arc<dyn FsOps>) -> MetaStore {
    let mut store = MetaStore::create_in(layout, fs).unwrap();
    store.set_parallelism(1);
    store
}

/// A FORMAT 3 store whose manifest names one run of [`FIRST_RUN`] records, and whose
/// checkpoint passed [`RECORDS`]: a step builds the rest, merges and removes the first run.
fn template() -> (TempDir, Utf8PathBuf) {
    let (dir, root) = new_root();
    let store = create(layout(root.root()), real_fs());
    insert(&store, 0..FIRST_RUN);
    assert_eq!(store.step_views().unwrap(), Step::Done);
    insert(&store, FIRST_RUN..RECORDS);
    (dir, root.root().to_owned())
}

/// The bytes of the manifest of the store at `root`.
fn manifest(root: &Utf8Path) -> Vec<u8> {
    fs::read(layout(root).views_dir().join("MANIFEST")).unwrap()
}

/// The names of the run files of the store at `root`, in order.
fn run_files(root: &Utf8Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(layout(root).views_dir())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| Path::new(name).extension().is_some_and(|ext| ext == "run"))
        .collect();
    names.sort();
    names
}

/// Opens the store at `root` through `fs`, its index synced from one thread.
fn open(root: &Utf8Path, fs: Arc<dyn FsOps>) -> MetaStore {
    let mut store = MetaStore::open_in(layout(root), fs).unwrap();
    store.set_parallelism(1);
    store
}

/// How many filesystem calls `run` makes on a copy of the store at `template`, opened on a
/// counter (its open's calls among them).
fn calls_of(template: &Utf8Path, run: impl FnOnce(&MetaStore)) -> u64 {
    let (_dir, root) = copy_of(template);
    let counter = Arc::new(CrashAtFs::new(NoSyncFs, 0));
    let store = open(&root, Arc::clone(&counter) as Arc<dyn FsOps>);
    run(&store);
    counter.calls()
}

/// The store at `root` finds each of [`SAMPLES`] and no changeset past them.
#[track_caller]
fn assert_finds(store: &MetaStore, what: &str) {
    let read = store.read().unwrap();
    for n in SAMPLES {
        assert!(read.changeset(id(n)).unwrap().is_some(), "{what}: {n}");
    }
    assert_eq!(read.changeset(id(RECORDS)).unwrap(), None, "{what}");
}

/// A view step stopped at any of its calls (a persistent `ENOSPC` from there on, as a crash
/// leaves the files) leaves the old manifest or the new one, never another; a reader finds the
/// same changesets; and the next step finishes with the manifest and the run files a step that
/// was never stopped leaves.
#[test]
fn views_step_crash_at_every_call() {
    let (_template_dir, template) = template();
    let before = manifest(&template);

    // 1. What the whole step leaves, and the calls it takes past the open.
    let (_done_dir, done) = copy_of(&template);
    assert_eq!(
        open(&done, faults::checked(NoSyncFs)).step_views().unwrap(),
        Step::Done
    );
    let (after, after_runs) = (manifest(&done), run_files(&done));
    assert_ne!(before, after, "the step publishes a manifest");
    let run = |to: u64| format!("c-{:016x}-{to:016x}.run", 0);
    assert_eq!(run_files(&template), [run(FIRST_RUN)]);
    assert_eq!(after_runs, [run(RECORDS)], "merged, the first removed");
    let opened = calls_of(&template, |_| {});
    let stepped = calls_of(&template, |store| {
        store.step_views().unwrap();
    });
    assert!(
        stepped > opened + 5,
        "{opened} calls to open, {stepped} with the step"
    );

    // 2. Stopped at each of its calls.
    for k in opened + 1..=stepped {
        let (_dir, root) = copy_of(&template);
        let fail = FailAt::new(k, FailMode::Persistent, InjectedError::Enospc);
        let failing = open(&root, faults::checked(FailAtFs::new(NoSyncFs, fail)));
        let outcome = failing.step_views();
        assert!(outcome.is_err(), "call {k}: {outcome:?}");
        drop(failing);

        // A reader sees one manifest or the other, and finds the same changesets.
        let left = manifest(&root);
        assert!(
            left == before || left == after,
            "call {k}: another manifest"
        );
        let reader = open(&root, faults::checked(NoSyncFs));
        assert_finds(&reader, &format!("call {k}, stopped"));

        // The next step finishes.
        assert_eq!(reader.step_views().unwrap(), Step::Done, "call {k}");
        assert_eq!(manifest(&root), after, "call {k}: the manifest");
        assert_eq!(run_files(&root), after_runs, "call {k}: the run files");
        assert_finds(&reader, &format!("call {k}, stepped"));
    }
}

/// The manifest an image holds names only run files the image holds, each with the header its
/// entry names (W4′); `None` if the image holds no manifest it decodes as the design's table.
fn missing_runs(root: &Path) -> Option<Vec<String>> {
    let views = root.join("meta/sorted3");
    let bytes = fs::read(views.join("MANIFEST")).ok()?;
    let count = usize::try_from(u32::from_le_bytes(bytes.get(16..20)?.try_into().ok()?)).ok()?;
    let mut missing = Vec::new();
    for entry in bytes.get(20..20 + 24 * count)?.chunks(24) {
        let letter = if entry[0] == 0 { 'c' } else { 'f' };
        let from = u64::from_le_bytes(entry[8..16].try_into().unwrap());
        let to = u64::from_le_bytes(entry[16..24].try_into().unwrap());
        let name = format!("{letter}-{from:016x}-{to:016x}.run");
        let header = fs::read(views.join(&name)).ok();
        let whole = header.is_some_and(|run| {
            run.get(..4) == Some(b"SWRN")
                && run.get(5) == Some(&entry[0])
                && run.get(8..24) == Some(&entry[8..24])
        });
        if !whole {
            missing.push(name);
        }
    }
    Some(missing)
}

/// A power loss at any point of a view step, keeping any part of what was not durable, leaves a
/// manifest that names only runs the disk holds (W4′), and a store that finds every committed
/// changeset and whose next step finishes.
#[test]
fn power_loss_images_name_no_missing_run() {
    // 1. The run, recorded from an empty base: the store, its commits, a step, then the step
    //    the images are taken in.
    let (base_dir, base) = temp_dir();
    let recorder = Arc::new(ImageRecordingFs::new());
    let root = base.join(STORE);
    // The root's entry durable first: a metadata store's create syncs what it makes under it.
    recorder.create_dir_all(root.as_std_path()).unwrap();
    recorder.fsync_dir(base.as_std_path()).unwrap();
    let store = create(layout(&root), Arc::clone(&recorder) as Arc<dyn FsOps>);
    insert(&store, 0..FIRST_RUN);
    store.step_views().unwrap();
    insert(&store, FIRST_RUN..RECORDS);
    recorder.mark(STEP_MARK);
    assert_eq!(store.step_views().unwrap(), Step::Done);
    drop(store);
    let timeline = Timeline::new(base.as_std_path(), &recorder.calls());
    drop(base_dir);

    // 2. Every image from the step's start on.
    let from = timeline.mark(STEP_MARK);
    let mut images = 0;
    let mut failures = Vec::new();
    for cut in from..=timeline.steps() {
        for seed in SEEDS {
            let image = timeline.image(cut, &mut Seeded::new(seed, cut));
            let (_dir, at) = temp_dir();
            image.materialize(at.as_std_path()).unwrap();
            images += 1;
            let root = at.join(STORE);
            match missing_runs(root.as_std_path()) {
                Some(missing) if missing.is_empty() => {}
                other => {
                    failures.push(format!("cut {cut}, seed {seed}: {other:?}"));
                    continue;
                }
            }
            let store = open(&root, real_fs());
            assert_finds(&store, &format!("cut {cut}, seed {seed}"));
            assert_eq!(store.step_views().unwrap(), Step::Done);
            assert_finds(&store, &format!("cut {cut}, seed {seed}, stepped"));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {images} images name a missing run: {failures:?}",
        failures.len()
    );
    assert!(images > 10, "{images} images");
}

/// A lineage fact: its kind's feature is [`FEATURE`].
fn lineage() -> Fact {
    Fact::new(
        FactKind::Lineage,
        1,
        [7; 32],
        [8; 32],
        9,
        "test/1".to_owned(),
        vec![3; 32],
    )
    .unwrap()
}

/// Commits [`lineage`] in a frame of its own; whether it applied.
fn put(store: &MetaStore) -> Result<bool, StoreError> {
    let mut write = store.write()?;
    let applied = write.put_fact(lineage(), Expect::Any)?;
    write.commit()?;
    Ok(matches!(applied, CasResult::Applied { .. }))
}

/// A write whose fact needs a feature the list lacks, stopped at any of its calls (the list's
/// replace among them), leaves no journal record of a feature the list does not hold (W10): the
/// store checks clean, and the same write then succeeds.
#[test]
fn feature_enable_crash_leaves_no_unlisted_record() {
    let _guard = supported_for_tests(&[FEATURE]);
    let (_template_dir, template) = {
        let (dir, root) = new_root();
        let store = create(layout(root.root()), real_fs());
        insert(&store, 0..10);
        (dir, root.root().to_owned())
    };
    let opened = calls_of(&template, |_| {});
    let written = calls_of(&template, |store| assert!(put(store).unwrap()));
    assert!(
        written > opened + 3,
        "{opened} calls to open, {written} with the write"
    );

    let mut listed = 0;
    for k in opened + 1..=written {
        // 1. The write stopped at call k.
        let (_dir, root) = copy_of(&template);
        let fail = FailAt::new(k, FailMode::Persistent, InjectedError::Enospc);
        let failing = open(&root, faults::checked(FailAtFs::new(NoSyncFs, fail)));
        assert!(put(&failing).is_err(), "call {k}");
        drop(failing);

        // 2. No record of an unlisted feature: the store checks clean.
        let store = open(&root, faults::checked(NoSyncFs));
        let checked = store.check().unwrap();
        assert_eq!(checked.problems, Vec::<String>::new(), "call {k}");
        let list = fs::read(layout(&root).features_file()).unwrap_or_default();
        if list.ends_with(b"\nlineage\n") {
            listed += 1;
        }

        // 3. The write, again, succeeds.
        assert!(put(&store).unwrap(), "call {k}");
        assert_eq!(store.check().unwrap().problems, Vec::<String>::new());
        let found = store.read().unwrap().facts_by_subject([7; 32]).unwrap();
        assert!(!found.is_empty(), "call {k}");
    }
    assert!(listed > 0, "some stop came after the list's replace");
}
