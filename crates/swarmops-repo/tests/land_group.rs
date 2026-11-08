//! The group land (the M18 design §6.3): `Repo::land_group` plans changes in order onto a
//! target's head, as `Repo::land_check` does, writes the clean ones as a chain `L1 … Lk`, and
//! publishes the chain in one frame: k inserts and one move of the target, never made anew. A
//! target that moved makes the group plan again; a conflicting change is reported and passed
//! over; a group land that crashes at any filesystem call has landed all of its chain or none.
//!
//! The crash sweep runs this test binary again as its children (selected by [`CHILD_ROOT`]), on
//! the real filesystem.

use std::io;
#[cfg(feature = "test-hooks")]
use std::os::unix::process::ExitStatusExt;
#[cfg(feature = "test-hooks")]
use std::process::Output;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use camino::{Utf8Path, Utf8PathBuf};
#[cfg(feature = "test-hooks")]
use swarmops_core::ObjectId;
use swarmops_core::{ChangesetId, Signature};
use swarmops_repo::land::{LandGroup, LandGroupRequest, PlanItem, PlanOutcome};
use swarmops_repo::{Advance, Change, Depth, LocalRepo, OpRecord};
#[cfg(feature = "test-hooks")]
use swarmops_store::{CRASH_AT_ENV, ORDER_CHECK_ENV};
use swarmops_store::{Journal, RealFs};
#[cfg(feature = "test-hooks")]
use swarmops_testkit::child::{SIGABRT, run, this_test};
use swarmops_testkit::faults::checked;
#[cfg(feature = "test-hooks")]
use swarmops_testkit::fs::copy_dir;
use swarmops_testkit::fs::{Call, Watch, Watched};
use swarmops_testkit::repo::{path, put, request};
use swarmops_testkit::store::layout_of;
#[cfg(feature = "test-hooks")]
use swarmops_testkit::test_name;
use tempfile::TempDir;

/// Who makes the changes the tests land.
const ACTOR: &str = "test/agent";

/// Who lands.
const LANDER: &str = "test/lander";

/// The changes of the D4 group, and the one of them (from 0) that conflicts.
const TEN: usize = 10;
const FOURTH: usize = 3;

/// A fresh store, its writes checked against the store's ordering rules.
fn new_store() -> (TempDir, Utf8PathBuf, LocalRepo) {
    let dir = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
    let repo = LocalRepo::create_with_fs(&root, "test", checked(RealFs::new())).unwrap();
    (dir, root, repo)
}

/// The lander's signature: fixed, so that the same group makes the same chain.
fn lander() -> Signature {
    Signature::new(
        LANDER.as_bytes().to_vec(),
        b"lander@test.swarmops.local".to_vec(),
        1_760_000_100,
        0,
        false,
    )
    .unwrap()
}

/// Commits `changes` on `parents` by [`ACTOR`], moving `main` to it when `advance` says so.
#[track_caller]
fn commit(
    repo: &LocalRepo,
    parents: &[ChangesetId],
    changes: Vec<Change>,
    advance: Moves,
) -> ChangesetId {
    let mut req = request(parents, changes, "a test commit");
    req.metadata = vec![("agent".to_owned(), ACTOR.as_bytes().to_vec())];
    if advance == Moves::Main {
        req.advance = Some(Advance::new("main", ACTOR));
    }
    repo.commit(req).unwrap().id
}

/// Whether a commit moves `main`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Moves {
    Main,
    Nothing,
}

/// What `main` names.
fn main_head(repo: &LocalRepo) -> ChangesetId {
    repo.get_ref("main").unwrap().expect("main exists").target
}

/// What the file at `p` holds in changeset `id`'s tree.
fn content(repo: &LocalRepo, id: ChangesetId, p: &str) -> Vec<u8> {
    let found = repo.read_blob_at(id, &path(p)).unwrap();
    found.map(|(_, _, bytes)| bytes).expect("the file")
}

/// Changeset `id`'s parents.
fn parents(repo: &LocalRepo, id: ChangesetId) -> Vec<ChangesetId> {
    let stored = repo.changeset(id).unwrap().expect("the changeset");
    stored.changeset.parents
}

/// How many frames the journal of the store at `root` lists.
fn frame_count(root: &Utf8Path) -> u64 {
    Journal::open(&layout_of(root), Arc::new(RealFs::new()))
        .unwrap()
        .frame_count()
        .unwrap()
}

/// A group land of `heads` onto `main` by [`lander`], each keyed `key-<n>` when `keys` says so.
fn group_of(heads: &[ChangesetId], keys: Keys) -> LandGroupRequest {
    let items = (heads.iter().enumerate())
        .map(|(n, &head)| {
            let mut item = PlanItem::new(head);
            if keys == Keys::Each {
                item.key = Some(format!("key-{n}"));
            }
            item
        })
        .collect();
    LandGroupRequest::new("main", items, lander())
}

/// Whether a group's changes carry keys.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Keys {
    Each,
    None,
}

/// Each item's outcome, by name.
fn outcomes(group: &LandGroup) -> Vec<&'static str> {
    (group.items.iter())
        .map(|item| match &item.outcome {
            PlanOutcome::Clean(_) => "clean",
            PlanOutcome::Conflict(_) => "conflict",
            PlanOutcome::Replayed(_) => "replayed",
            PlanOutcome::NothingToLand(_) => "nothing_to_land",
            other => panic!("an outcome the test does not expect: {other:?}"),
        })
        .collect()
}

/// A store whose `main` is `t0` and then a change of `f`, and ten changes on `t0`, each adding
/// a file of its own; the fourth also edits `f`'s line that `main` changed.
fn ten_changes() -> (TempDir, Utf8PathBuf, LocalRepo, Vec<ChangesetId>) {
    let (dir, root, repo) = new_store();
    let t0 = commit(&repo, &[], vec![put("f", b"1\n2\n3\n")], Moves::Main);
    let heads: Vec<ChangesetId> = (0..TEN)
        .map(|n| {
            let mut changes = vec![put(&format!("c/{n}"), format!("{n}\n").as_bytes())];
            if n == FOURTH {
                changes.push(put("f", b"1\nX\n3\n"));
            }
            commit(&repo, &[t0], changes, Moves::Nothing)
        })
        .collect();
    let t1 = main_head(&repo);
    commit(&repo, &[t1], vec![put("f", b"1\nT\n3\n")], Moves::Main);
    (dir, root, repo, heads)
}

/// D4: ten changes whose fourth conflicts land nine in one operation: its nine changesets, a
/// chain on the head the group read, and one move of `main`, to the ninth. The fourth's report
/// names the path; every file of the nine is on `main`.
#[test]
fn ten_changes_with_a_conflicting_fourth_land_nine_in_one_move() {
    let (_dir, _root, repo, heads) = ten_changes();
    let onto = main_head(&repo);

    // 1. Nine clean, the fourth a conflict on `f`.
    let group = repo.land_group(&group_of(&heads, Keys::None)).unwrap();
    let mut want = vec!["clean"; TEN];
    want[FOURTH] = "conflict";
    assert_eq!(outcomes(&group), want);
    let PlanOutcome::Conflict(conflicts) = &group.items[FOURTH].outcome else {
        unreachable!("checked above");
    };
    let paths: Vec<&str> = (conflicts.conflicts.iter())
        .map(|c| c.path.as_str())
        .collect();
    assert_eq!(paths, ["f"]);
    assert_eq!((group.onto, group.attempts), (onto, 1));

    // 2. The chain: each landed changeset's parent the one before, the first's the head read.
    let landed: Vec<ChangesetId> = group.items.iter().filter_map(|i| i.landed).collect();
    assert_eq!(landed.len(), TEN - 1);
    assert_eq!(group.items[FOURTH].landed, None);
    let mut parent = onto;
    for id in &landed {
        assert_eq!(parents(&repo, *id), [parent]);
        parent = *id;
    }
    let head = group.landed.as_ref().expect("the group landed").head;
    assert_eq!((head, main_head(&repo)), (parent, parent));

    // 3. One operation: nine inserts and one move of `main`, from the head read to the ninth.
    let (ops, _) = repo.op_log(None, 1).unwrap();
    assert_eq!(group.landed.as_ref().unwrap().op, ops[0].seq);
    let inserted: Vec<ChangesetId> = (ops[0].records.iter())
        .filter_map(|r| match r {
            OpRecord::Changeset { id, .. } => Some(*id),
            _ => None,
        })
        .collect();
    assert_eq!(inserted, landed);
    let moves: Vec<(&str, Option<ChangesetId>, Option<ChangesetId>)> = (ops[0].records.iter())
        .filter_map(|r| match r {
            OpRecord::RefMove { name, old, new, .. } => Some((name.as_str(), *old, *new)),
            _ => None,
        })
        .collect();
    assert_eq!(moves, [("main", Some(onto), Some(head))]);

    // 4. Every clean change's file is on `main`, and the store verifies clean.
    for n in (0..TEN).filter(|&n| n != FOURTH) {
        let file = content(&repo, head, &format!("c/{n}"));
        assert_eq!(file, format!("{n}\n").into_bytes());
    }
    assert_eq!(repo.verify(Depth::Deep).unwrap().problems, []);
}

/// A group land is one frame, whatever its size: the journal lists one more.
#[test]
fn a_group_is_one_frame() {
    let (_dir, root, repo, heads) = ten_changes();
    let before = frame_count(&root);
    repo.land_group(&group_of(&heads, Keys::None)).unwrap();
    assert_eq!(frame_count(&root), before + 1);
}

/// A keyed group run again replays: every clean change of the first run is replayed, naming
/// the changeset that landed it, and nothing is recorded.
#[test]
fn a_group_rerun_with_its_keys_replays() {
    let (_dir, root, repo, heads) = ten_changes();
    let first = repo.land_group(&group_of(&heads, Keys::Each)).unwrap();
    let frames = frame_count(&root);
    let head = main_head(&repo);

    let again = repo.land_group(&group_of(&heads, Keys::Each)).unwrap();
    let mut want = vec!["replayed"; TEN];
    want[FOURTH] = "conflict";
    assert_eq!(outcomes(&again), want);
    assert!(again.landed.is_none(), "a group of replays lands nothing");
    for (first, again) in first.items.iter().zip(&again.items) {
        assert_eq!(first.landed, again.landed);
    }
    assert_eq!((frame_count(&root), main_head(&repo)), (frames, head));
}

/// A racer that acts once on the store, through a handle of its own, after the group's
/// snapshot: at its first flush of objects, or at its first read of one, as it plans.
struct Racer {
    root: Utf8PathBuf,
    when: When,
    raced: AtomicBool,
    act: Box<dyn Fn(&LocalRepo) + Send + Sync>,
}

/// When a [`Racer`] acts.
#[derive(Clone, Copy, PartialEq, Eq)]
enum When {
    /// At the group's first `fsync_dir` of `objects/`: its chain written, not published.
    Flush,
    /// At its first call under `objects/`: its snapshot taken, its plan begun, no lock held.
    Planning,
}

impl Racer {
    /// A handle on the store at `root` whose group meets `act` `when` says.
    fn open(
        root: &Utf8Path,
        when: When,
        act: impl Fn(&LocalRepo) + Send + Sync + 'static,
    ) -> LocalRepo {
        let racer = Racer {
            root: root.to_owned(),
            when,
            raced: AtomicBool::new(false),
            act: Box::new(act),
        };
        LocalRepo::open_with_fs(root, checked(Watched(racer, RealFs::new()))).unwrap()
    }
}

impl Watch for Racer {
    fn before(&self, call: &Call<'_>) -> io::Result<()> {
        // 1. Where the call is.
        let at = call.path().strip_prefix(&self.root).unwrap_or(call.path());
        let under = |dir: &str| at.components().next().is_some_and(|c| c.as_os_str() == dir);

        // 2. The moment, once.
        let now = match self.when {
            When::Flush => call.method == "fsync_dir" && under("objects"),
            When::Planning => under("objects"),
        };
        if !now || self.raced.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let other = LocalRepo::open_with_fs(&self.root, checked(RealFs::new())).unwrap();
        (self.act)(&other);
        Ok(())
    }
}

/// Moves `main` on `other` by a change of a file of its own.
fn move_main(other: &LocalRepo) {
    let head = main_head(other);
    commit(other, &[head], vec![put("race", b"r\n")], Moves::Main);
}

/// A group whose target moves after its plan publishes nothing of that plan: it plans again on
/// the moved head and lands its chain there, in its second attempt.
#[test]
fn a_group_on_a_moved_target_plans_again() {
    let (_dir, root, repo, heads) = ten_changes();
    let onto = main_head(&repo);
    drop(repo);
    let repo = Racer::open(&root, When::Flush, move_main);

    let group = repo.land_group(&group_of(&heads, Keys::None)).unwrap();
    assert_eq!(
        parents(&repo, group.onto),
        [onto],
        "the group landed on the racer's head"
    );
    assert_eq!(group.attempts, 2);
    let landed: Vec<ChangesetId> = group.items.iter().filter_map(|i| i.landed).collect();
    assert_eq!(parents(&repo, landed[0]), [group.onto]);
    assert_eq!(main_head(&repo), *landed.last().unwrap());
    assert_eq!(content(&repo, main_head(&repo), "race"), b"r\n");
    assert_eq!(repo.verify(Depth::Deep).unwrap().problems, []);
}

/// A key of the group that a land on another ref records after the group's plan is refused
/// under the lock, as a land's is (the M16 design §6): `KeyReused`, naming that ref, and
/// nothing of the group is recorded, though `main` did not move.
#[test]
fn a_group_whose_key_lands_elsewhere_meanwhile_records_nothing() {
    let (_dir, root, repo, heads) = ten_changes();
    let onto = main_head(&repo);
    repo.cas_ref("release/x", None, Some(onto), ACTOR).unwrap();
    let frames = frame_count(&root);
    drop(repo);
    let first = heads[0];
    let repo = Racer::open(&root, When::Flush, move |other| {
        let mut land = swarmops_repo::land::LandRequest::new(first, "release/x", lander());
        land.key = Some("key-0".to_owned());
        other.land(land).unwrap();
    });

    let refused = repo.land_group(&group_of(&heads, Keys::Each)).unwrap_err();
    let swarmops_repo::RepoError::KeyReused { key, target, .. } = refused else {
        panic!("not a key reused: {refused:?}");
    };
    assert_eq!((key.as_str(), target.as_str()), ("key-0", "release/x"));
    assert_eq!(main_head(&repo), onto);
    assert_ne!(repo.get_ref("release/x").unwrap().unwrap().target, onto);
    assert_eq!(
        frame_count(&root),
        frames + 1,
        "only the racer's land is recorded"
    );
}

/// A group whose changes all conflict, or have nothing to land, lands nothing and records
/// nothing: each item's outcome says why.
#[test]
fn a_group_with_nothing_clean_lands_nothing() {
    let (_dir, root, repo, heads) = ten_changes();
    let frames = frame_count(&root);
    let head = main_head(&repo);
    let group = repo
        .land_group(&group_of(&[heads[FOURTH], head], Keys::None))
        .unwrap();
    assert_eq!(outcomes(&group), ["conflict", "nothing_to_land"]);
    assert!(group.landed.is_none());
    assert_eq!((frame_count(&root), main_head(&repo)), (frames, head));
}

/// A group with nothing clean decides so under the lock, as a land's replay is: a target that
/// moved after the snapshot the plan read makes it plan again, on the moved head.
#[test]
fn a_group_with_nothing_clean_settles_under_the_lock() {
    let (_dir, root, repo, heads) = ten_changes();
    let head = main_head(&repo);
    drop(repo);
    let repo = Racer::open(&root, When::Planning, move_main);

    let group = repo
        .land_group(&group_of(&[heads[FOURTH], head], Keys::None))
        .unwrap();
    assert_eq!(outcomes(&group), ["conflict", "nothing_to_land"]);
    assert_eq!(group.attempts, 2, "planned again on the moved head");
    assert_eq!(parents(&repo, group.onto), [head]);
    assert_eq!(main_head(&repo), group.onto);
}

// ---- The crash sweep ----

/// Set only in the children the sweep spawns: the store root.
#[cfg(feature = "test-hooks")]
const CHILD_ROOT: &str = "SWARMOPS_TEST_REPO_LAND_GROUP_CHILD_ROOT";

/// Set with [`CHILD_ROOT`]: the heads the child lands, full ids, a space between each two.
#[cfg(feature = "test-hooks")]
const CHILD_HEADS: &str = "SWARMOPS_TEST_REPO_LAND_GROUP_CHILD_HEADS";

/// How a child of the sweep reports how its group ended, among the harness's output.
#[cfg(feature = "test-hooks")]
const REPORT: &str = "land-group-child: ";

/// How many runs of the sweep go at once: each has a child process and a store of its own.
#[cfg(feature = "test-hooks")]
const AT_ONCE: u64 = 8;

/// The sweep's group: three changes on `t0`, each adding a file of its own, keyed.
#[cfg(feature = "test-hooks")]
const SWEPT: usize = 3;

/// What the sweep lands: on a store whose `main` is `t0`, the group of `heads`; `want` is the
/// chain it makes.
#[cfg(feature = "test-hooks")]
#[derive(Debug, Clone)]
struct Swept {
    t0: ChangesetId,
    heads: Vec<ChangesetId>,
    want: Vec<ChangesetId>,
}

/// The store and the heads this process lands, if it is a child the sweep spawned.
#[cfg(feature = "test-hooks")]
fn child() -> Option<(Utf8PathBuf, Vec<ChangesetId>)> {
    let root = std::env::var(CHILD_ROOT).ok()?;
    let heads = std::env::var(CHILD_HEADS).expect("the heads to land");
    let heads = (heads.split(' '))
        .map(|hex| ChangesetId::new(ObjectId::from_hex(hex).expect("a full id")))
        .collect();
    Some((Utf8PathBuf::from(root), heads))
}

/// A fresh copy of `template`, in a temp dir of its own.
#[cfg(feature = "test-hooks")]
fn copy_of(template: &Utf8Path) -> (TempDir, Utf8PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
    copy_dir(template.as_std_path(), root.as_std_path());
    (dir, root)
}

/// The sweep's template, and the chain its group makes, landed on a copy.
#[cfg(feature = "test-hooks")]
fn swept_template() -> (TempDir, Utf8PathBuf, Swept) {
    let (dir, template, repo) = new_store();
    let t0 = commit(&repo, &[], vec![put("f", b"f\n")], Moves::Main);
    let heads: Vec<ChangesetId> = (0..SWEPT)
        .map(|n| {
            let file = put(&format!("c/{n}"), format!("{n}\n").as_bytes());
            commit(&repo, &[t0], vec![file], Moves::Nothing)
        })
        .collect();
    drop(repo);
    let (_copy, root) = copy_of(&template);
    let repo = LocalRepo::open_with_fs(&root, checked(RealFs::new())).unwrap();
    let group = repo.land_group(&group_of(&heads, Keys::Each)).unwrap();
    let want = group.items.iter().filter_map(|i| i.landed).collect();
    (dir, template, Swept { t0, heads, want })
}

/// Whether the store holds the swept group, checked: `main` is the chain's last and its chain
/// down to `t0` is the whole chain, or `main` is `t0` and no changeset of the chain is in the
/// index. `what` names the run.
#[cfg(feature = "test-hooks")]
fn swept_landed(repo: &LocalRepo, swept: &Swept, what: &str) -> bool {
    let main = main_head(repo);
    if main == swept.t0 {
        for id in &swept.want {
            assert!(
                repo.changeset(*id).unwrap().is_none(),
                "{what}: {id} indexed"
            );
        }
        return false;
    }
    let mut chain = Vec::new();
    let mut at = main;
    while at != swept.t0 {
        chain.push(at);
        at = parents(repo, at)[0];
    }
    chain.reverse();
    assert_eq!(chain, swept.want, "{what}: main is not the whole chain");
    true
}

/// One run of the sweep: a child lands the group on a copy of `template` until its k-th
/// filesystem call aborts it. The group is then on `main` whole or not at all, the store
/// verifies clean, and the same group again ends with it whole on `main`. Returns whether the
/// child finished, and whether the crash left the group on `main`.
#[cfg(feature = "test-hooks")]
fn group_crashing_at(template: &Utf8Path, swept: &Swept, k: u64) -> (bool, bool) {
    let (_dir, root) = copy_of(template);
    let heads: Vec<String> = swept.heads.iter().map(ToString::to_string).collect();
    let crash_at = k.to_string();
    let child = group_child(&root, &heads.join(" "), &crash_at);
    let finished = child.status.success();
    assert!(
        finished || child.status.signal() == Some(SIGABRT),
        "k={k}: {:?}\n{}",
        child.status,
        String::from_utf8_lossy(&child.stderr)
    );

    // 1. All or none, and a clean store.
    let what = format!("k={k}");
    let repo = LocalRepo::open_with_fs(&root, checked(RealFs::new())).unwrap();
    let landed = swept_landed(&repo, swept, &what);
    assert!(
        landed || !finished,
        "{what}: the group returned but is not on main"
    );
    assert_eq!(repo.verify(Depth::Deep).unwrap().problems, [], "{what}");

    // 2. The same group again: replayed where it landed, landed whole where it did not.
    let again = repo
        .land_group(&group_of(&swept.heads, Keys::Each))
        .unwrap();
    let ids: Vec<ChangesetId> = again.items.iter().filter_map(|i| i.landed).collect();
    assert_eq!(ids, swept.want, "{what}: again");
    assert_eq!(again.landed.is_some(), !landed, "{what}: again");
    assert!(swept_landed(&repo, swept, &what), "{what}: landed again");
    (finished, landed)
}

/// The child of one run: this test again, on `root`, with `heads`, crashing at call `crash_at`.
#[cfg(feature = "test-hooks")]
fn group_child(root: &Utf8Path, heads: &str, crash_at: &str) -> Output {
    let test = "a_group_land_crashing_at_any_call_lands_all_or_none";
    let mut command = this_test(&test_name!(test));
    command
        .env(CHILD_ROOT, root)
        .env(CHILD_HEADS, heads)
        .env(ORDER_CHECK_ENV, "1")
        .env(CRASH_AT_ENV, crash_at);
    run(&mut command)
}

/// A group land that crashes at any filesystem call lands all of its chain or none: for every
/// k, `main` is the whole chain on `t0` or `t0`, no changeset of the chain is indexed without
/// the move, the store verifies clean, and the same group again ends with the chain whole on
/// `main`. Some crash comes after the commit point.
#[test]
#[cfg(feature = "test-hooks")]
fn a_group_land_crashing_at_any_call_lands_all_or_none() {
    if let Some((root, heads)) = child() {
        let landed =
            LocalRepo::open(&root).and_then(|r| r.land_group(&group_of(&heads, Keys::Each)));
        match landed {
            Ok(group) => println!("{REPORT}landed {:?}", group.landed.map(|l| l.head)),
            Err(e) => println!("{REPORT}failed {e:?}"),
        }
        return;
    }
    let (_template_dir, template, swept) = swept_template();
    let template = &template;
    let swept = &swept;
    let mut past_the_commit_point = false;
    let mut first = 1;
    loop {
        // 1. A round of runs at once, k from `first` on.
        let runs: Vec<(bool, bool)> = thread::scope(|scope| {
            let runs: Vec<_> = (first..first + AT_ONCE)
                .map(|k| scope.spawn(move || group_crashing_at(template, swept, k)))
                .collect();
            runs.into_iter().map(|run| run.join().unwrap()).collect()
        });
        past_the_commit_point |= runs.iter().any(|&(finished, landed)| landed && !finished);

        // 2. Done once a child finished: every later one finishes too.
        if let Some(at) = runs.iter().position(|&(finished, _)| finished) {
            assert!(
                runs[at..].iter().all(|&(finished, _)| finished),
                "a crash after the end"
            );
            let calls = first + u64::try_from(at).unwrap() - 1;
            assert!(calls > 20, "the child finished after {calls} calls");
            break;
        }
        first += AT_ONCE;
        assert!(first < 2_000, "the child never finished");
    }
    assert!(
        past_the_commit_point,
        "no crash came after the commit point"
    );
}

/// Group lands and lands at once, each through a handle of its own: every change lands once,
/// each group's chain whole in one operation, and the store verifies clean. Waiting groups are
/// planned by whichever holder leads the write queue.
#[test]
fn groups_and_lands_at_once_land_each_once() {
    const GROUPS: usize = 4;
    const EACH: usize = 3;
    let (_dir, root, repo) = new_store();
    let t0 = commit(&repo, &[], vec![put("f", b"f\n")], Moves::Main);
    let heads: Vec<Vec<ChangesetId>> = (0..GROUPS)
        .map(|g| {
            (0..EACH)
                .map(|n| {
                    let file = put(&format!("g{g}/{n}"), b"x\n");
                    commit(&repo, &[t0], vec![file], Moves::Nothing)
                })
                .collect()
        })
        .collect();
    let single = commit(&repo, &[t0], vec![put("single", b"s\n")], Moves::Nothing);

    // 1. Every group and one land, each on its own thread and handle.
    let groups: Vec<LandGroup> = thread::scope(|scope| {
        let land = scope.spawn(|| {
            let mine = LocalRepo::open_with_fs(&root, checked(RealFs::new())).unwrap();
            let mut req = swarmops_repo::land::LandRequest::new(single, "main", lander());
            req.max_attempts = 32;
            mine.land(req).unwrap();
        });
        let runs: Vec<_> = (heads.iter())
            .map(|heads| {
                let root = &root;
                scope.spawn(move || {
                    let mine = LocalRepo::open_with_fs(root, checked(RealFs::new())).unwrap();
                    let mut req = group_of(heads, Keys::None);
                    req.max_attempts = 32;
                    mine.land_group(&req).unwrap()
                })
            })
            .collect();
        land.join().unwrap();
        runs.into_iter().map(|run| run.join().unwrap()).collect()
    });

    // 2. Each group one operation of its whole chain; every file on `main`.
    let (ops, _) = repo.op_log(None, 64).unwrap();
    for group in &groups {
        let landed: Vec<ChangesetId> = group.items.iter().filter_map(|i| i.landed).collect();
        assert_eq!(landed.len(), EACH);
        let op = ops
            .iter()
            .find(|op| op.seq == group.landed.as_ref().unwrap().op);
        let inserted: Vec<ChangesetId> = (op.expect("the group's operation").records.iter())
            .filter_map(|r| match r {
                OpRecord::Changeset { id, .. } => Some(*id),
                _ => None,
            })
            .collect();
        assert_eq!(inserted, landed);
    }
    let head = main_head(&repo);
    for g in 0..GROUPS {
        for n in 0..EACH {
            assert_eq!(content(&repo, head, &format!("g{g}/{n}")), b"x\n");
        }
    }
    assert_eq!(content(&repo, head, "single"), b"s\n");
    assert_eq!(repo.verify(Depth::Deep).unwrap().problems, []);
}
