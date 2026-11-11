//! Tests of `Repo::upgrade` (the M6 design §3, the FORMAT 3 design §3), which takes a FORMAT 1
//! repository through FORMAT 2 to FORMAT 3: an upgraded repository holds the history, refs and
//! reflogs it held, verifies clean and takes FORMAT 2's names; a second upgrade does nothing; a
//! handle opened before it fails closed and a reopen reads FORMAT 3; a held lease is refused at
//! once; a crash before any call of it leaves a repository that opens whole, and that the open
//! itself finishes once a poison is written, two opens at once included.
//!
//! Every test owns a fresh store (`tempfile::tempdir()`).

use std::io;
use std::sync::{Arc, Barrier};
use std::thread;

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_core::ChangesetId;
use swarmops_repo::{Depth, LocalRepo, RepoError, StoreError, UpgradeReport};
use swarmops_store::meta::upgrade;
use swarmops_store::{
    CrashAtFs, FailAt, FailAtFs, FailMode, FsOps, InjectedError, Layout, MaintLease, MetaStore,
    RealFs, RefMoveEntry, RefState, StoreFormat,
};
use swarmops_testkit::faults;
use swarmops_testkit::fs::{NoSyncFs, copy_dir};
use swarmops_testkit::repo::{commit, put};
use tempfile::TempDir;

/// Who the template's ref moves name.
const ACTOR: &str = "test/1";

/// The refs the template moves.
const REFS: [&str; 2] = ["main", "release/1"];

/// A free-form branch: a FORMAT 2 name only.
const BRANCH: &str = "feature/login";

/// What a repository holds: its history from `main`, its refs and their reflogs.
#[derive(Debug, PartialEq, Eq)]
struct View {
    log: Vec<ChangesetId>,
    refs: Vec<(String, RefState)>,
    reflogs: Vec<Vec<RefMoveEntry>>,
    changesets: u64,
}

/// The view of the repository at `root`, opened afresh.
fn view(root: &Utf8Path) -> View {
    let repo = LocalRepo::open(root).unwrap();
    let main = repo.get_ref("main").unwrap().unwrap().target;
    let log = repo.log_first_parent(main, usize::MAX).unwrap();
    let meta = MetaStore::open(root).unwrap();
    let read = meta.read().unwrap();
    View {
        log: log.into_iter().map(|cs| cs.id).collect(),
        refs: repo.list_refs().unwrap(),
        reflogs: REFS
            .iter()
            .map(|name| read.reflog(name, usize::MAX).unwrap())
            .collect(),
        changesets: repo.info().unwrap().changesets,
    }
}

/// Whether the repository at `root` verifies clean, to `depth`.
#[track_caller]
fn assert_verifies(root: &Utf8Path, depth: Depth, what: &str) {
    let report = LocalRepo::open(root).unwrap().verify(depth).unwrap();
    assert!(report.problems.is_empty(), "{what}: {:?}", report.problems);
}

/// A FORMAT 1 repository: five commits that `main` advances to, `release/1` made at the second
/// and moved to the fourth.
fn template() -> (TempDir, Utf8PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
    let repo = LocalRepo::create_as(&root, "test", StoreFormat::V1).unwrap();
    let mut parent: Option<ChangesetId> = None;
    for n in 0..5 {
        let parents: Vec<ChangesetId> = parent.into_iter().collect();
        let id = commit(
            &repo,
            &parents,
            vec![put("a.txt", format!("{n}\n").as_bytes())],
        );
        repo.cas_ref("main", parent, Some(id), ACTOR).unwrap();
        if n == 1 {
            repo.cas_ref("release/1", None, Some(id), ACTOR).unwrap();
        }
        if n == 3 {
            let at = repo.get_ref("release/1").unwrap().unwrap().target;
            repo.cas_ref("release/1", Some(at), Some(id), ACTOR)
                .unwrap();
        }
        parent = Some(id);
    }
    assert_eq!(repo.format(), StoreFormat::V1);
    (dir, root)
}

/// A copy of the repository at `template`, in a temp dir of its own.
fn copy_of(template: &Utf8Path) -> (TempDir, Utf8PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
    copy_dir(template.as_std_path(), root.as_std_path());
    (dir, root)
}

/// The upgraded repository holds what it held, verifies clean, says it is FORMAT 3, and takes a
/// free-form branch; a second upgrade does nothing.
#[test]
fn an_upgraded_repository_holds_what_it_held() {
    let (_dir, root) = template();
    let before = view(&root);
    assert_eq!(before.log.len(), 5);
    assert_eq!(before.refs[1].1.version, 2, "{before:?}");

    let report = LocalRepo::open(&root).unwrap().upgrade().unwrap();
    assert_eq!(
        report,
        UpgradeReport::new(StoreFormat::V1, StoreFormat::V3, true)
    );
    let repo = LocalRepo::open(&root).unwrap();
    assert_eq!(repo.format(), StoreFormat::V3);
    assert_eq!(repo.info().unwrap().store_version, 3);
    assert_eq!(view(&root), before);
    assert_verifies(&root, Depth::Deep, "upgraded");

    let main = repo.get_ref("main").unwrap().unwrap().target;
    repo.cas_ref(BRANCH, None, Some(main), ACTOR).unwrap();
    assert_eq!(repo.get_ref(BRANCH).unwrap().unwrap().target, main);
    assert_verifies(&root, Depth::Deep, "with a branch");

    let again = repo.upgrade().unwrap();
    assert_eq!(
        again,
        UpgradeReport::new(StoreFormat::V3, StoreFormat::V3, false)
    );
}

/// A handle opened before the upgrade reads the poisoned checkpoint: its next write and its next
/// read fail as damage (`InvalidData`, which the operations report as `E_STORE_CORRUPT`); a
/// reopen reads FORMAT 3 and writes. So does a handle opened on FORMAT 2.
#[test]
fn a_handle_opened_before_fails_closed_and_a_reopen_reads_format_3() {
    let (_dir, root) = template();
    let stale = LocalRepo::open(&root).unwrap();
    LocalRepo::open(&root).unwrap().upgrade().unwrap();

    let main = LocalRepo::open(&root)
        .unwrap()
        .get_ref("main")
        .unwrap()
        .unwrap()
        .target;
    assert_invalid_data(
        stale
            .cas_ref("release/2", None, Some(main), ACTOR)
            .map(drop),
    );
    assert_invalid_data(stale.get_ref("main").map(drop));

    let fresh = LocalRepo::open(&root).unwrap();
    assert_eq!(fresh.format(), StoreFormat::V3);
    fresh.cas_ref("release/2", None, Some(main), ACTOR).unwrap();

    // A FORMAT 2 handle, its store upgraded under it.
    let (_dir, root, v2) = swarmops_testkit::repo::new_repo_as(StoreFormat::V2);
    let c1 = commit(&v2, &[], vec![put("a.txt", b"a")]);
    v2.cas_ref("main", None, Some(c1), ACTOR).unwrap();
    LocalRepo::open(&root).unwrap().upgrade().unwrap();
    assert_invalid_data(v2.get_ref("main").map(drop));
    assert_invalid_data(v2.cas_ref(BRANCH, None, Some(c1), ACTOR).map(drop));
    assert_eq!(LocalRepo::open(&root).unwrap().format(), StoreFormat::V3);
}

/// `result` is a failed read of a file, of kind `InvalidData`.
#[track_caller]
fn assert_invalid_data(result: Result<(), RepoError>) {
    match result {
        Err(RepoError::Store(StoreError::Io(e))) => {
            assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");
        }
        other => panic!("not InvalidData: {other:?}"),
    }
}

/// A held maintenance lease refuses the upgrade at once; the repository stays FORMAT 1.
#[test]
fn a_held_lease_refuses_the_upgrade() {
    let (_dir, root) = template();
    let _held = MaintLease::try_acquire(&Layout::new(&root)).unwrap();
    let refused = LocalRepo::open(&root).unwrap().upgrade().unwrap_err();
    assert!(
        matches!(refused, RepoError::Store(StoreError::MaintenanceBusy)),
        "{refused:?}"
    );
    assert_eq!(LocalRepo::open(&root).unwrap().format(), StoreFormat::V1);
}

/// An upgrade that stopped between FORMAT 3's poison and `FORMAT` (its last call failed) is
/// finished by the next opens, no upgrade called: two at once both open FORMAT 3.
#[test]
fn opens_finish_an_upgrade_stopped_after_its_poison() {
    let (_dir, root) = template();
    let before = view(&root);
    let layout = Layout::new(&root);
    let calls = upgrade_calls(&root);
    let fail = FailAt::new(calls, FailMode::Persistent, InjectedError::Enospc);
    upgrade::upgrade(&layout, Arc::new(FailAtFs::new(RealFs::new(), fail))).unwrap_err();
    assert_eq!(
        std::fs::read(layout.format_file()).unwrap(),
        StoreFormat::V2.marker()
    );

    let barrier = Barrier::new(2);
    let formats: Vec<StoreFormat> = thread::scope(|scope| {
        let opens: Vec<_> = (0..2)
            .map(|_| {
                scope.spawn(|| {
                    barrier.wait();
                    LocalRepo::open(&root).unwrap().format()
                })
            })
            .collect();
        opens.into_iter().map(|open| open.join().unwrap()).collect()
    });
    assert_eq!(formats, [StoreFormat::V3; 2]);
    assert_eq!(
        std::fs::read(layout.format_file()).unwrap(),
        StoreFormat::V3.marker()
    );
    assert_eq!(view(&root), before);
    assert_verifies(&root, Depth::Deep, "finished by the opens");
}

/// How many calls through the filesystem seam an upgrade of a copy of the repository at
/// `template` makes.
fn upgrade_calls(template: &Utf8Path) -> u64 {
    let (_dir, root) = copy_of(template);
    let counter = Arc::new(CrashAtFs::new(RealFs::new(), 0));
    upgrade::upgrade(&Layout::new(&root), Arc::clone(&counter) as Arc<dyn FsOps>).unwrap();
    counter.calls()
}

/// A crash before each call of the upgrade (it and every write after it fail before they do
/// anything, the order checker watching, on a filesystem that never syncs: durability is not the
/// subject here): the repository opens whole, in the format before a poison and in the next one
/// from it on, the open finishing what is left; one short of FORMAT 3 is upgraded again. Each
/// ends FORMAT 3, holding what it held, and verifies clean.
#[test]
fn a_crash_at_any_call_leaves_a_repository_that_opens_whole() {
    let (_dir, template) = template();
    let template = template.as_path();
    let calls = upgrade_calls(template);
    let before = view(template);
    let mut short = [0; 2]; // crashes that left FORMAT 1, FORMAT 2
    for k in 1..=calls {
        let (_dir, root) = copy_of(template);
        let fail = FailAt::new(k, FailMode::Persistent, InjectedError::Enospc);
        let fs = faults::checked(FailAtFs::new(NoSyncFs, fail));
        upgrade::upgrade(&Layout::new(&root), fs).unwrap_err();

        let format = LocalRepo::open(&root).unwrap().format();
        if format != StoreFormat::V3 {
            let at = usize::try_from(format.version() - 1).unwrap();
            short[at] += 1;
            assert_eq!(view(&root), before, "call {k}: {format:?}, whole");
            assert_verifies(&root, Depth::Shallow, &format!("call {k}: {format:?}"));
            let rerun = upgrade::upgrade(&Layout::new(&root), faults::checked(NoSyncFs));
            assert!(rerun.unwrap().upgraded, "call {k}");
        }
        assert_eq!(
            LocalRepo::open(&root).unwrap().format(),
            StoreFormat::V3,
            "call {k}"
        );
        assert_eq!(view(&root), before, "call {k}: FORMAT 3, whole");
        assert_verifies(&root, Depth::Shallow, &format!("call {k}: FORMAT 3"));
    }
    let total: u64 = short.iter().sum();
    assert!(
        short.iter().all(|&n| n > 0) && total < calls,
        "{short:?} of {calls} left short of FORMAT 3"
    );
}
