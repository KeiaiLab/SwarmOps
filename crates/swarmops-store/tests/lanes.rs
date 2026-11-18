//! The land queue's lane lock, `meta/lanes/<key>.lock` (the M22 core design I-4 and §5
//! takeover): one runner per target, and a new holder kills only the check process groups its
//! dead predecessor left behind.

use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use swarmops_store::meta::lanes::LaneGroup;
use swarmops_store::{LaneLock, Layout, MetaStore};
use swarmops_testkit::store::new_root;

/// How long a killed check group may take to be reaped.
const REAP_WITHIN: Duration = Duration::from_secs(10);

/// While a lane is held its target admits no second holder, another target does, and dropping
/// the guard frees the first.
#[test]
fn a_lane_admits_one_holder_per_target() {
    let (_dir, layout) = store();
    let layout = &layout;

    // 1. The first holder of `main` takes it; a second is refused while it lives.
    let held = LaneLock::try_acquire(layout, "main").unwrap();
    assert!(held.is_some(), "a free lane is taken");
    let second = LaneLock::try_acquire(layout, "main").unwrap();
    assert!(second.is_none(), "a held lane admits no second holder");

    // 2. Another target's lane is its own.
    let other = LaneLock::try_acquire(layout, "release/1").unwrap();
    assert!(other.is_some(), "another target's lane is free");

    // 3. Dropping the guard frees `main`.
    drop(held);
    let again = LaneLock::try_acquire(layout, "main").unwrap();
    assert!(again.is_some(), "a dropped lane is free again");
}

/// A dead runner's `.groups` lines name its check groups: the next holder kills those of this
/// boot whose leader still has the recorded start time, leaves alone a line of another boot or
/// of a pid reused since (another start time), and clears the file.
#[test]
fn takeover_kills_only_its_dead_runners_groups() {
    let (_dir, layout) = store();
    let layout = &layout;

    // 1. Three check groups: one the dead runner ran, one of another boot, one whose pid was
    //    reused (its start time differs from the line's).
    let mut ours = sleeper();
    let mut other_boot = sleeper();
    let mut reused = sleeper();
    let ours_line = group_of(&ours, 0);
    let boot_line = group_of(&other_boot, 1);
    let boot_line = LaneGroup::new(
        boot_line.pgid(),
        boot_line.starttime(),
        "00000000-0000-0000-0000-000000000000",
        1,
    );
    let reused_line = group_of(&reused, 2);
    let reused_line = LaneGroup::new(
        reused_line.pgid(),
        reused_line.starttime() + 1,
        reused_line.boot_id(),
        2,
    );

    // 2. The runner records them, then dies holding the lane: its guard dropped without a
    //    clear, as the kernel drops a dead process's flock.
    let runner = LaneLock::try_acquire(layout, "main").unwrap().unwrap();
    let lines = [ours_line, boot_line, reused_line];
    runner.write_groups(&lines).unwrap();
    assert_eq!(runner.groups().unwrap(), lines);
    drop(runner);

    // 3. The next holder kills only the group of this boot and start time, and clears the file.
    let next = LaneLock::try_acquire(layout, "main").unwrap().unwrap();
    assert_eq!(next.groups().unwrap(), Vec::<LaneGroup>::new(), "cleared");
    let status = reap(&mut ours);
    assert_eq!(
        status,
        Some(libc::SIGKILL),
        "its dead runner's group is killed"
    );
    thread::sleep(Duration::from_millis(200));
    assert!(
        other_boot.try_wait().unwrap().is_none(),
        "another boot's line is left"
    );
    assert!(reused.try_wait().unwrap().is_none(), "a reused pid is left");

    // 4. The test's own children, ended by their handles.
    for child in [&mut other_boot, &mut reused] {
        child.kill().unwrap();
        child.wait().unwrap();
    }
}

/// A fresh metadata store, `meta/` made, no lane yet.
fn store() -> (tempfile::TempDir, Layout) {
    let (dir, layout) = new_root();
    MetaStore::create(layout.root()).unwrap();
    (dir, layout)
}

/// A `sleep` leading a process group of its own, as a check runs.
fn sleeper() -> Child {
    Command::new("sleep")
        .arg("60")
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

/// The `.groups` line of `child`'s group in `slot`, read from this boot.
fn group_of(child: &Child, slot: u16) -> LaneGroup {
    LaneGroup::of(child.id(), slot).expect("a live group reads")
}

/// The signal that ended `child`, waiting up to [`REAP_WITHIN`]; `None` if it still runs.
fn reap(child: &mut Child) -> Option<i32> {
    let deadline = Instant::now() + REAP_WITHIN;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            return status.signal();
        }
        thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
    None
}

/// Lane files are runners' IPC, no metadata: a store holding a lane and its `.groups` checks
/// clean.
#[test]
fn verify_passes_over_the_lanes() {
    let (_dir, layout) = store();
    let lane = LaneLock::try_acquire(&layout, "main").unwrap().unwrap();
    lane.write_groups(&[LaneGroup::new(4_242, 7, "boot", 0)])
        .unwrap();
    let check = MetaStore::open(layout.root()).unwrap().check().unwrap();
    assert!(check.problems.is_empty(), "{:?}", check.problems);
    assert!(check.meta_corrupt.is_empty(), "{:?}", check.meta_corrupt);
}
