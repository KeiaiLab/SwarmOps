//! The process counters at the store's file boundary (the M21 design §2.1): object reads and
//! writes, journal writes and syncs, and the waits for and holds of `meta/lock`, counted where
//! the store makes the call.
//!
//! The counters are the process's: each test runs in a process of its own (nextest), and the
//! write queue's tests run their lead and their member as two children, so that each child's
//! counts are its own.

#[cfg(feature = "test-hooks")]
use std::fs::File;
#[cfg(feature = "test-hooks")]
use std::io;
#[cfg(feature = "test-hooks")]
use std::path::{Path, PathBuf};
#[cfg(feature = "test-hooks")]
use std::sync::Arc;
use std::thread;
use std::time::Duration;
#[cfg(feature = "test-hooks")]
use std::time::Instant;

use swarmops_core::{ChangesetId, ObjectKind, hash_object};
use swarmops_store::fs_ops::counters::{Counters, Counts};
use swarmops_store::{
    ChangesetMeta, FsObjectStore, MetaLock, MetaStore, MetaWrite, ObjectStore, RealFs, StoreError,
};
#[cfg(feature = "test-hooks")]
use swarmops_store::{FrameUse, FsOps, LinkOutcome, Planned, Serve, Turn};
#[cfg(feature = "test-hooks")]
use swarmops_testkit::child::{run, this_test};
use swarmops_testkit::store::new_root;
#[cfg(feature = "test-hooks")]
use swarmops_testkit::test_name;

const ACTOR: &str = "test/counters";

/// Set only in a queue test's child: its role, [`LEAD`] or [`MEMBER`].
const ROLE: &str = "SWARMOPS_TEST_COUNTERS_ROLE";

/// Set with [`ROLE`]: the store's root.
const ROOT: &str = "SWARMOPS_TEST_COUNTERS_ROOT";

/// The child that leads the queue.
const LEAD: &str = "lead";

/// The child whose request the lead answers.
const MEMBER: &str = "member";

/// How a child's counts line starts.
const COUNTS_LINE: &str = "counts:";

/// How long the parent waits for the lead to hold the lock.
const LEAD_DEADLINE: Duration = Duration::from_secs(60);

/// How long the lock is held while another waits for it.
const HOLD: Duration = Duration::from_millis(20);

/// How long the lead pauses at each crash point of its queue steps, under the lock: the last
/// comes after every answer is written, so the member reads its answer before the lock is free
/// (it looks every `ANSWER_POLL`, 1 ms).
#[cfg(feature = "test-hooks")]
const LEAD_PAUSE: Duration = Duration::from_millis(50);

/// The process's counts now.
fn now() -> Counts {
    Counters::process().snapshot()
}

/// Inserts changeset `name` and moves `main` from `old` to it.
fn advance(write: &mut MetaWrite<'_>, name: &[u8], old: Option<ChangesetId>) -> ChangesetId {
    let id = ChangesetId::new(hash_object(ObjectKind::Changeset, name));
    let delta = hash_object(ObjectKind::Blob, name).0;
    write
        .insert_changeset(ChangesetMeta::new(id, 0, 0, delta, Vec::new()))
        .unwrap();
    write.cas_ref("main", old, Some(id), ACTOR).unwrap();
    id
}

/// A put and a get move only the object counts; a commit's frame only the journal counts.
#[test]
fn counts_split_object_and_journal_calls() {
    let (_dir, layout) = new_root();
    let store = MetaStore::create(layout.root()).unwrap();
    let objects = FsObjectStore::open(layout.clone(), Box::new(RealFs::new()));

    // 1. One new object, flushed, then read back.
    let before = now();
    let id = objects.put(ObjectKind::Blob, b"counted").unwrap();
    objects.flush().unwrap();
    assert!(objects.get(id).unwrap().is_some());
    let moved = now().since(&before);
    assert_eq!(moved.object_writes, 1, "{moved:?}");
    assert_eq!(moved.object_reads, 1, "{moved:?}");
    assert_eq!(moved.journal_writes, 0, "{moved:?}");
    assert_eq!(moved.journal_syncs, 0, "{moved:?}");

    // 2. One committed frame.
    let before = now();
    let mut write = store.write().unwrap();
    advance(&mut write, b"one", None);
    write.commit().unwrap();
    let moved = now().since(&before);
    assert!(moved.journal_writes > 0, "{moved:?}");
    assert!(moved.journal_syncs > 0, "{moved:?}");
    assert_eq!(moved.object_writes, 0, "{moved:?}");
    assert_eq!(moved.object_reads, 0, "{moved:?}");
}

/// A wait for a held lock and each hold are counted: two waits, the second as long as the
/// first holder kept it at least, and both holds.
#[test]
fn a_lock_wait_and_hold_are_counted() {
    let (_dir, layout) = new_root();
    MetaStore::create(layout.root()).unwrap();
    let before = now();

    // 1. One holder lets go after HOLD; the other waits for it.
    let held = MetaLock::acquire(&layout, Duration::ZERO).unwrap();
    let holder = thread::spawn(move || {
        thread::sleep(HOLD);
        drop(held);
    });
    let second = MetaLock::acquire(&layout, Duration::from_secs(5)).unwrap();
    holder.join().unwrap();
    drop(second);

    // 2. What was counted.
    let moved = now().since(&before);
    let hold_us = u64::try_from(HOLD.as_micros()).unwrap();
    assert_eq!(moved.lock_waits, 2, "{moved:?}");
    assert!(moved.lock_wait_us >= hold_us / 2, "{moved:?}");
    assert!(moved.lock_hold_us >= hold_us, "{moved:?}");
    assert_eq!(moved.lock_timeouts, 0, "{moved:?}");
}

/// An acquire that gives up is a timeout; the wait is counted as well.
#[test]
fn a_timeout_is_counted() {
    let (_dir, layout) = new_root();
    MetaStore::create(layout.root()).unwrap();
    let _held = MetaLock::acquire(&layout, Duration::ZERO).unwrap();
    let before = now();
    let given_up = MetaLock::acquire(&layout, Duration::from_millis(10));
    assert!(matches!(given_up, Err(StoreError::Busy)));
    let moved = now().since(&before);
    assert_eq!(moved.lock_timeouts, 1, "{moved:?}");
    assert_eq!(moved.lock_waits, 1, "{moved:?}");
    assert!(moved.lock_wait_us > 0, "{moved:?}");
    assert_eq!(moved.lock_hold_us, 0, "{moved:?}");
}

/// `since` is the difference of each field; nothing moved is all zeros.
#[test]
fn counts_since_subtracts() {
    let (_dir, layout) = new_root();
    let objects = FsObjectStore::open(layout, Box::new(RealFs::new()));
    let first = now();
    objects.put(ObjectKind::Blob, b"since").unwrap();
    let second = now();
    assert_eq!(second.since(&second), Counts::default());
    let moved = second.since(&first);
    assert_eq!(
        moved.object_writes,
        second.object_writes - first.object_writes
    );
    assert_eq!(moved.object_writes, 1);
    assert_eq!(moved.object_reads, second.object_reads - first.object_reads);
    assert_eq!(first.since(&second), Counts::default(), "never below zero");
}

/// A member whose request a lead answers waits once and never holds the lock.
#[cfg(feature = "test-hooks")]
#[test]
fn a_queued_member_counts_one_wait_and_no_hold() {
    let test = test_name!("a_queued_member_counts_one_wait_and_no_hold");
    let Some([_lead, member]) = queue_pair(&test) else {
        return;
    };
    assert_eq!(member.lock_waits, 1, "{member:?}");
    assert!(member.lock_wait_us > 0, "{member:?}");
    assert_eq!(member.lock_hold_us, 0, "{member:?}");
    assert_eq!(member.lock_timeouts, 0, "{member:?}");
}

/// A lead counts one wait, the try that found the lock free, and its hold.
#[cfg(feature = "test-hooks")]
#[test]
fn a_lead_counts_one_wait_and_its_hold() {
    let test = test_name!("a_lead_counts_one_wait_and_its_hold");
    let Some([lead, _member]) = queue_pair(&test) else {
        return;
    };
    assert_eq!(lead.lock_waits, 1, "{lead:?}");
    assert!(lead.lock_hold_us > 0, "{lead:?}");
    assert_eq!(lead.lock_timeouts, 0, "{lead:?}");
}

/// The lead's and the member's counts, each from a child running `test` again; `None` in a
/// child, which runs its role instead.
#[cfg(feature = "test-hooks")]
fn queue_pair(test: &str) -> Option<[Counts; 2]> {
    // 1. A child runs its role and is done.
    if let Ok(role) = std::env::var(ROLE) {
        let root = std::env::var(ROOT).expect("a child has a root");
        child(&role, Path::new(&root));
        return None;
    }

    // 2. The lead first, holding the lock while it waits for one request.
    let (_dir, layout) = new_root();
    MetaStore::create(layout.root()).unwrap();
    let spawn = |role: &str| {
        let mut command = this_test(test);
        command
            .env(ROLE, role)
            .env(ROOT, layout.root().as_str())
            .env(swarmops_store::fs_ops::LEAD_WAIT_ENV, "1");
        thread::spawn(move || run(&mut command))
    };
    let lead = spawn(LEAD);
    wait_until_held(layout.meta_lock().as_std_path());

    // 3. Then the member, which the lead answers.
    let member = spawn(MEMBER);
    let counts = [lead, member].map(|child| {
        let output = child.join().unwrap();
        let printed = String::from_utf8_lossy(&output.stdout).into_owned();
        assert!(output.status.success(), "{output:?}");
        parse_counts(&printed)
    });
    Some(counts)
}

/// Waits until another process holds the flock on the lock file at `path`, looking with a
/// flock of its own (no store call, so the parent's counts stay as they are).
#[cfg(feature = "test-hooks")]
fn wait_until_held(path: &Path) {
    let deadline = Instant::now() + LEAD_DEADLINE;
    while Instant::now() < deadline {
        let file = File::open(path).unwrap();
        if file.try_lock().is_err() {
            return;
        }
        drop(file);
        thread::sleep(Duration::from_millis(1));
    }
    panic!("the lead did not take the lock within {LEAD_DEADLINE:?}");
}

/// A queue child: the lead plans its own request and the member's and commits; the member's
/// turn is answered. Either prints its counts.
#[cfg(feature = "test-hooks")]
fn child(role: &str, root: &Path) {
    let root = camino::Utf8Path::from_path(root).unwrap();
    let store = match role {
        LEAD => MetaStore::open_with_fs(root, Arc::new(PausingFs(RealFs::new()))).unwrap(),
        _ => MetaStore::open(root).unwrap(),
    };
    let before = now();
    let turn = store.take_turn(role.as_bytes(), Serve::Queue).unwrap();
    match (role, turn) {
        (LEAD, Turn::Lead(mut lead)) => {
            assert_eq!(lead.requests().len(), 2, "the member's request is gathered");
            let rids: Vec<_> = lead.requests().iter().map(|r| r.rid.clone()).collect();
            assert!(lead.may_plan(), "the member is granted");
            advance(lead.write(), b"lead", None);
            let plans = vec![
                Planned::new(rids[0].clone(), FrameUse::Frame(0), b"lead".to_vec()),
                Planned::new(rids[1].clone(), FrameUse::NoFrame, b"member".to_vec()),
            ];
            assert_eq!(lead.claim(plans).unwrap(), 2);
            lead.commit().unwrap();
        }
        (MEMBER, Turn::Answered(answer)) => assert_eq!(answer.body, b"member"),
        (role, _) => panic!("{role}: the other turn"),
    }
    print_counts(&now().since(&before));
}

/// Prints `counts` as a line [`parse_counts`] reads.
#[cfg(feature = "test-hooks")]
fn print_counts(counts: &Counts) {
    println!(
        "{COUNTS_LINE} {} {} {} {}",
        counts.lock_waits, counts.lock_wait_us, counts.lock_hold_us, counts.lock_timeouts
    );
}

/// The lock figures of the counts line in `printed`, the rest zero.
#[cfg(feature = "test-hooks")]
fn parse_counts(printed: &str) -> Counts {
    let line = printed
        .lines()
        .find_map(|line| line.strip_prefix(COUNTS_LINE))
        .unwrap_or_else(|| panic!("no counts line in {printed}"));
    let figures: Vec<u64> = line
        .split_whitespace()
        .map(|figure| figure.parse().unwrap())
        .collect();
    let [waits, wait_us, hold_us, timeouts] = figures[..] else {
        panic!("not four figures: {line}");
    };
    let mut counts = Counts::default();
    counts.lock_waits = waits;
    counts.lock_wait_us = wait_us;
    counts.lock_hold_us = hold_us;
    counts.lock_timeouts = timeouts;
    counts
}

/// [`RealFs`], pausing [`LEAD_PAUSE`] at each crash point of the write queue's steps.
#[cfg(feature = "test-hooks")]
struct PausingFs(RealFs);

#[cfg(feature = "test-hooks")]
impl FsOps for PausingFs {
    fn create_dir_all(&self, p: &Path) -> io::Result<()> {
        self.0.create_dir_all(p)
    }

    fn write_durable_temp(&self, dir: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
        self.0.write_durable_temp(dir, bytes)
    }

    fn link_noclobber(&self, from: &Path, to: &Path) -> io::Result<LinkOutcome> {
        self.0.link_noclobber(from, to)
    }

    fn fsync_dir(&self, p: &Path) -> io::Result<()> {
        self.0.fsync_dir(p)
    }

    fn read(&self, p: &Path) -> io::Result<Option<Vec<u8>>> {
        self.0.read(p)
    }

    fn exists(&self, p: &Path) -> io::Result<bool> {
        self.0.exists(p)
    }

    fn remove(&self, p: &Path) -> io::Result<()> {
        self.0.remove(p)
    }

    fn append_durable(&self, path: &Path, bytes: &[u8]) -> io::Result<u64> {
        self.0.append_durable(path, bytes)
    }

    fn file_len(&self, path: &Path) -> io::Result<u64> {
        self.0.file_len(path)
    }

    fn truncate(&self, path: &Path, len: u64) -> io::Result<()> {
        self.0.truncate(path, len)
    }

    fn atomic_replace(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        self.0.atomic_replace(path, bytes)
    }

    fn sync_data(&self, path: &Path) -> io::Result<()> {
        self.0.sync_data(path)
    }

    fn append(&self, path: &Path, bytes: &[u8]) -> io::Result<u64> {
        self.0.append(path, bytes)
    }

    fn rename_durable(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.0.rename_durable(from, to)
    }

    fn crash_point(&self) {
        thread::sleep(LEAD_PAUSE);
    }
}
