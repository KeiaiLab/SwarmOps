//! Tests of the upgrade from FORMAT 1 to FORMAT 2 (the M6 design §3): an upgraded store reads as
//! it did, frames, changesets, refs (targets and versions) and reflogs, and takes FORMAT 2's ref
//! names; a FORMAT 2 store is left alone; FORMAT 1 readers of an upgraded store, a handle opened
//! before it among them, fail closed on the poisoned checkpoint; a held maintenance lease refuses
//! it at once; a crash before any of its calls leaves one whole format, FORMAT 1 (a rerun
//! completes) or FORMAT 2 (an open finishes it).
//!
//! Every test owns a fresh store root (`tempfile::tempdir()`).

use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use camino::Utf8Path;
use swarmops_store::meta::upgrade::{self, Upgraded};
use swarmops_store::{
    CrashAtFs, FailAt, FailAtFs, FailMode, FsOps, InjectedError, Journal, Layout, MaintLease,
    MetaLock, MetaStore, RealFs, Record, RefMoveEntry, RefState, StoreError, StoreFormat,
    UPGRADE_POISON, read_format,
};
use swarmops_testkit::faults;
use swarmops_testkit::fs::NoSyncFs;
use swarmops_testkit::store::sweep::{changeset, copy_of, id};
use swarmops_testkit::store::{new_root, real_fs};
use tempfile::TempDir;

/// Who the template's ref moves name.
const ACTOR: &str = "test/1";

/// How many frames the template's journal holds: the checkpoint moves at 32 frames, so that
/// some lie past it, unmaterialized in the checkpoint's sense.
const FRAMES: u64 = 40;

/// The refs the template moves.
const REFS: [&str; 2] = ["main", "release/a"];

/// A free-form branch: a FORMAT 2 name only.
const BRANCH: &str = "feature/login";

/// What a store holds, as its own format reads it: every frame's records, the changesets, the
/// refs and their reflogs.
#[derive(Debug, PartialEq, Eq)]
struct View {
    frames: Vec<(u64, Vec<Record>)>,
    changesets: u64,
    refs: Vec<(String, RefState)>,
    reflogs: Vec<Vec<RefMoveEntry>>,
}

/// The view of the store at `root`, in the format its `FORMAT` names.
fn view(root: &Utf8Path) -> View {
    let store = MetaStore::open(root).unwrap();
    let layout = Layout::with_format(root, store.format());
    let journal = Journal::open(&layout, real_fs()).unwrap();
    let frames = journal.frames_from(0, usize::MAX).unwrap();
    let read = store.read().unwrap();
    View {
        frames: frames
            .into_iter()
            .map(|frame| (frame.seq, frame.records))
            .collect(),
        changesets: read.count_changesets().unwrap(),
        refs: read.list_refs().unwrap(),
        reflogs: REFS
            .iter()
            .map(|name| read.reflog(name, usize::MAX).unwrap())
            .collect(),
    }
}

/// The format `FORMAT` of the store at `root` names.
fn format_of(root: &Utf8Path) -> StoreFormat {
    let layout = Layout::new(root);
    read_format(&layout, &swarmops_store::StoreFs::new(real_fs(), root)).unwrap()
}

/// A FORMAT 1 store of [`FRAMES`] frames, each a changeset that `main` moves to; `release/a`
/// moved twice among them, so that its version is 2; frames past the checkpoint.
fn template() -> (TempDir, Layout) {
    let (dir, layout) = new_root();
    MetaStore::create_in(layout.clone(), real_fs()).unwrap();
    std::fs::write(layout.format_file(), StoreFormat::V1.marker()).unwrap();
    let store = MetaStore::open(layout.root()).unwrap();
    for n in 0..FRAMES {
        let mut write = store.write().unwrap();
        write.insert_changeset(changeset(n)).unwrap();
        let main = (n > 0).then(|| id(n - 1));
        write.cas_ref("main", main, Some(id(n)), ACTOR).unwrap();
        if n == 3 || n == 5 {
            let release = (n == 5).then(|| id(3));
            write
                .cas_ref("release/a", release, Some(id(n)), ACTOR)
                .unwrap();
        }
        write.commit().unwrap();
    }
    let ckpt = Journal::open(&layout, real_fs()).unwrap().ckpt().unwrap();
    let log_len = std::fs::metadata(layout.journal_log()).unwrap().len();
    assert!(ckpt > 0 && ckpt < log_len, "frames past the checkpoint");
    (dir, layout)
}

/// An upgrade of the store at `root` through the real filesystem, the order checker watching.
fn upgrade_checked(root: &Utf8Path) -> Result<Upgraded, StoreError> {
    upgrade::upgrade(&Layout::new(root), faults::checked(RealFs::new()))
}

/// The upgraded store holds every frame, changeset, ref (target and version) and reflog it held,
/// under FORMAT 2's files, and `FORMAT` says 2; it takes a write, and a FORMAT 2 name.
#[test]
fn an_upgraded_store_reads_as_it_did() {
    let (_dir, layout) = template();
    let root = layout.root();
    let before = view(root);
    assert_eq!(before.frames.len(), usize::try_from(FRAMES).unwrap());
    let release = before.refs.iter().find(|(name, _)| name == "release/a");
    assert_eq!(release.unwrap().1.version, 2, "{before:?}");

    let upgraded = upgrade_checked(root).unwrap();
    assert_eq!(
        upgraded,
        Upgraded::new(StoreFormat::V1, StoreFormat::V2, true)
    );
    assert_eq!(format_of(root), StoreFormat::V2);
    assert_eq!(view(root), before);
    let v2 = Layout::with_format(root, StoreFormat::V2);
    for file in [v2.journal_log(), v2.journal_off(), v2.journal_ckpt()] {
        assert!(file.is_file(), "{file}");
    }

    // A write after it, under the order checker: the frames past the checkpoint replay as
    // FORMAT 1's would, and a free-form branch is a name it takes.
    let store = MetaStore::open_with_fs(root, faults::checked(RealFs::new())).unwrap();
    assert_eq!(store.format(), StoreFormat::V2);
    let mut write = store.write().unwrap();
    write.insert_changeset(changeset(FRAMES)).unwrap();
    write
        .cas_ref(BRANCH, None, Some(id(FRAMES)), ACTOR)
        .unwrap();
    write.commit().unwrap();
    store.flush().unwrap();
    let read = store.read().unwrap();
    assert_eq!(read.get_ref(BRANCH).unwrap().unwrap().target, id(FRAMES));
    assert_eq!(read.count_changesets().unwrap(), FRAMES + 1);
    assert_eq!(read.list_refs().unwrap().len(), REFS.len() + 1);
}

/// A FORMAT 2 store is not upgraded: nothing written, `upgraded` false.
#[test]
fn a_format_2_store_is_left_alone() {
    let (_dir, layout) = template();
    let root = layout.root();
    upgrade_checked(root).unwrap();
    let before = view(root);

    let counter = Arc::new(CrashAtFs::new(RealFs::new(), 0));
    let again = upgrade::upgrade(&layout, Arc::clone(&counter) as Arc<dyn FsOps>).unwrap();
    assert_eq!(
        again,
        Upgraded::new(StoreFormat::V2, StoreFormat::V2, false)
    );
    assert_eq!(counter.calls(), 1, "the read of FORMAT, and no write");
    assert_eq!(view(root), before);
}

/// A FORMAT 1 reader of the upgraded store, one opened afresh with a FORMAT 1 layout (as an
/// older build opens it) or a handle opened before the upgrade, fails closed on the poisoned
/// checkpoint: `InvalidData`, for reads and writes alike.
#[test]
fn format_1_readers_of_an_upgraded_store_fail_closed() {
    let (_dir, layout) = template();
    let root = layout.root();
    let before = MetaStore::open(root).unwrap();
    upgrade_checked(root).unwrap();
    assert_eq!(
        std::fs::read(layout.journal_ckpt()).unwrap(),
        UPGRADE_POISON
    );

    let afresh = MetaStore::open_in(Layout::new(root), real_fs()).unwrap();
    for store in [&before, &afresh] {
        assert_invalid_data(store.read().map(drop));
        assert_invalid_data(store.write().map(drop));
    }
}

/// `result` is a failed read of a file, of kind `InvalidData`.
#[track_caller]
fn assert_invalid_data(result: Result<(), StoreError>) {
    match result {
        Err(StoreError::Io(e)) => assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}"),
        other => panic!("not InvalidData: {other:?}"),
    }
}

/// A held maintenance lease refuses the upgrade at once, and nothing is written.
#[test]
fn a_held_lease_refuses_the_upgrade_at_once() {
    let (_dir, layout) = template();
    let root = layout.root();
    let before = view(root);
    let _held = MaintLease::try_acquire(&layout).unwrap();

    let started = Instant::now();
    let refused = upgrade_checked(root).unwrap_err();
    assert!(
        matches!(refused, StoreError::MaintenanceBusy),
        "{refused:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "never waited for"
    );
    assert_eq!(format_of(root), StoreFormat::V1);
    assert!(
        !Layout::with_format(root, StoreFormat::V2)
            .journal_log()
            .exists()
    );
    assert_eq!(view(root), before);
}

/// A FORMAT 1 journal that does not read (a byte of a frame's payload flipped) is not upgraded:
/// the error is the reader's, nothing is poisoned and `FORMAT` says 1.
#[test]
fn a_damaged_journal_is_not_upgraded() {
    let (_dir, layout) = template();
    let root = layout.root();
    let journal = Journal::open(&layout, real_fs()).unwrap();
    let middle = journal.frames_from(FRAMES / 2, 1).unwrap()[0].offset;
    let mut bytes = std::fs::read(layout.journal_log()).unwrap();
    let at = usize::try_from(middle).unwrap() + 12;
    bytes[at] ^= 0xff;
    std::fs::write(layout.journal_log(), bytes).unwrap();

    let refused = upgrade_checked(root).unwrap_err();
    assert!(
        matches!(refused, StoreError::JournalCorrupt { offset } if offset == middle),
        "{refused:?}"
    );
    assert_eq!(format_of(root), StoreFormat::V1);
    assert_ne!(
        std::fs::read(layout.journal_ckpt()).unwrap(),
        UPGRADE_POISON
    );
}

/// On a FORMAT 1 store that no upgrade poisoned there is nothing to finish, and looking costs no
/// call through the filesystem seam.
#[test]
fn an_unpoisoned_store_has_nothing_to_finish() {
    let (_dir, layout) = template();
    let counter = Arc::new(CrashAtFs::new(RealFs::new(), 0));
    let finished =
        upgrade::finish_if_poisoned(&layout, Arc::clone(&counter) as Arc<dyn FsOps>).unwrap();
    assert!(!finished);
    assert_eq!(counter.calls(), 0);
    assert_eq!(format_of(layout.root()), StoreFormat::V1);
}

/// A crash before each call of the upgrade (the call and every write after it fail before they
/// do anything, the order checker watching, on a filesystem that never syncs: durability is not
/// the subject here): before the poison the store is FORMAT 1, whole, and
/// a second upgrade completes; between the poison and `FORMAT`, an open finishes it; after it, it
/// is FORMAT 2. Each ends as the store was, in FORMAT 2.
#[test]
fn a_crash_at_any_call_leaves_one_whole_format() {
    let (_template_dir, template) = template();
    let template = template.root();
    let before = view(template);
    let calls = {
        let (_dir, root) = copy_of(template);
        let counter = Arc::new(CrashAtFs::new(NoSyncFs, 0));
        upgrade::upgrade(&Layout::new(&root), Arc::clone(&counter) as Arc<dyn FsOps>).unwrap();
        counter.calls()
    };
    assert!(calls > 10, "{calls} calls");

    let mut seen = [0_u32; 3]; // crashes left FORMAT 1, poisoned FORMAT 1, FORMAT 2
    for k in 1..=calls {
        let (_dir, root) = copy_of(template);
        let layout = Layout::new(&root);
        let fail = FailAt::new(k, FailMode::Persistent, InjectedError::Enospc);
        let crashed = upgrade::upgrade(&layout, faults::checked(FailAtFs::new(NoSyncFs, fail)));
        assert!(crashed.is_err(), "call {k} of {calls}: {crashed:?}");

        let poisoned = std::fs::read(layout.journal_ckpt()).unwrap() == UPGRADE_POISON;
        match (format_of(&root), poisoned) {
            (StoreFormat::V1, false) => {
                seen[0] += 1;
                assert_eq!(view(&root), before, "call {k}: FORMAT 1, whole");
                let rerun = upgrade::upgrade(&layout, faults::checked(NoSyncFs)).unwrap();
                assert!(rerun.upgraded, "call {k}");
            }
            (StoreFormat::V1, true) => {
                seen[1] += 1;
                let finished =
                    upgrade::finish_if_poisoned(&layout, faults::checked(NoSyncFs)).unwrap();
                assert!(finished, "call {k}");
            }
            (format, _) => {
                seen[2] += 1;
                assert_eq!(format, StoreFormat::V2, "call {k}");
            }
        }
        assert_eq!(format_of(&root), StoreFormat::V2, "call {k}");
        assert_eq!(view(&root), before, "call {k}: FORMAT 2, whole");
    }
    assert!(seen[0] > 0 && seen[1] > 0, "{seen:?}");
}

/// The upgrade's rewrite of a large journal, measured against the 10 s a writer waits for
/// `meta/lock` (the M6 plan's risk R-1): 200,000 changesets, one frame each, as a store of
/// single commits holds them.
#[test]
#[ignore = "slow, ~12 s: a journal of 200,000 frames built, then upgraded"]
fn the_upgrade_of_a_large_journal_is_measured() {
    const CHANGESETS: u64 = 200_000;
    const LOCK_TIMEOUT: Duration = Duration::from_secs(10);

    let (_dir, layout) = new_root();
    MetaStore::create_in(layout.clone(), real_fs()).unwrap();
    std::fs::write(layout.format_file(), StoreFormat::V1.marker()).unwrap();
    let built = Instant::now();

    // 1. The frames, appended straight to the journal under the lock: a transaction's own
    //    recovery and index append, 200,000 times over, would take minutes.
    let lock = MetaLock::acquire(&layout, LOCK_TIMEOUT).unwrap();
    let journal = Journal::open(&layout, Arc::new(NoSyncFs)).unwrap();
    for n in 0..CHANGESETS {
        let meta = changeset(n);
        let records = [
            Record::Changeset {
                id: meta.id,
                generation: 1,
                delta_hash: meta.delta_hash,
                parents: Vec::new(),
            },
            Record::RefMove {
                name: "main".to_owned(),
                old: (n > 0).then(|| id(n - 1)),
                new: Some(id(n)),
                actor: ACTOR.to_owned(),
                ts: 0,
            },
        ];
        journal.append_frame(&records).unwrap();
    }
    drop((journal, lock));

    // 2. The index and the ref file brought up to date with them, and the checkpoint moved.
    MetaStore::open_with_fs(layout.root(), Arc::new(NoSyncFs))
        .unwrap()
        .flush()
        .unwrap();
    let log_len = std::fs::metadata(layout.journal_log()).unwrap().len();
    eprintln!(
        "built {CHANGESETS} frames ({log_len} B) in {:.1} s",
        built.elapsed().as_secs_f64()
    );

    let started = Instant::now();
    let upgraded = upgrade::upgrade(&layout, real_fs()).unwrap();
    let took = started.elapsed();
    eprintln!(
        "upgrade of {CHANGESETS} frames: {:.2} s (meta/lock held throughout; a writer waits {} s)",
        took.as_secs_f64(),
        LOCK_TIMEOUT.as_secs()
    );
    assert!(upgraded.upgraded);
    assert!(took < LOCK_TIMEOUT, "{took:?}");
    let read = MetaStore::open(layout.root()).unwrap();
    assert_eq!(read.read().unwrap().count_changesets().unwrap(), CHANGESETS);
}
