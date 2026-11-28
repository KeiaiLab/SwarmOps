//! Lock waits that give up gather no thread: a hundred of them on one `meta/lock` cost their
//! process one helper thread, and none once the lock is let go.
//!
//! The proof is the process's own count of helpers started and ended
//! ([`swarmops_store::fs_ops::counters`]), which reads no clock. On Linux the kernel's list of
//! this process's threads, `/proc/self/task`, is read beside it. That list is the whole
//! process's, which is why this file is a test binary of its own with one test (see
//! `Cargo.toml`): nothing else in the process starts a thread or ends one while it runs.

use std::fs::{self, File};
#[cfg(target_os = "linux")]
use std::thread;
use std::time::Duration;
#[cfg(target_os = "linux")]
use std::time::Instant;

use swarmops_store::flock_wait::{self, LockKey};
use swarmops_store::fs_ops::counters::{Counters, Counts};
use swarmops_store::{MetaLock, StoreError};
use swarmops_testkit::store::new_root;

/// The waits that give up.
const WAITS: u64 = 100;

/// How many threads this process has now, as the kernel lists them.
#[cfg(target_os = "linux")]
fn threads() -> usize {
    fs::read_dir("/proc/self/task")
        .expect("list this process's threads")
        .count()
}

/// Waits up to 10 s until [`threads`] is `want`, and returns what it was last: the kernel can
/// go on listing a thread for an instant after its join has returned.
#[cfg(target_os = "linux")]
fn wait_for_threads(want: usize) -> usize {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let now = threads();
        if now == want || Instant::now() >= deadline {
            return now;
        }
        thread::sleep(Duration::from_millis(1));
    }
}

/// The helpers the process started and ended since `before`.
fn helpers_since(before: &Counts) -> (u64, u64) {
    let moved = Counters::process().snapshot().since(before);
    (moved.flock_helpers_started, moved.flock_helpers_ended)
}

#[test]
fn a_hundred_waits_that_give_up_leave_one_helper_then_none() {
    let (_root, layout) = new_root();
    fs::create_dir_all(layout.meta_dir()).expect("create meta/");
    let held = MetaLock::acquire(&layout, Duration::ZERO).expect("the lock is free");
    let lock_file = File::open(layout.meta_lock()).expect("open the lock file");
    let key = LockKey::of(&lock_file).expect("the lock file's key");
    let before = Counters::process().snapshot();
    #[cfg(target_os = "linux")]
    let threads_before = threads();

    // 1. A hundred waits, each given up: the lock is this test's, so each can only be busy.
    for wait in 0..WAITS {
        let got = MetaLock::acquire(&layout, Duration::from_millis(1));
        assert!(matches!(got, Err(StoreError::Busy)), "wait {wait}: {got:?}");
    }
    let gave_up = Counters::process().snapshot().since(&before).lock_timeouts;
    assert_eq!(gave_up, WAITS, "every wait gave up");
    assert_eq!(helpers_since(&before), (1, 0), "one helper, still there");
    assert_eq!(flock_wait::waiting(&key), Some(0), "and no claim on it");
    // A started thread is listed at once.
    #[cfg(target_os = "linux")]
    assert_eq!(threads(), threads_before + 1, "one thread more");

    // 2. The holder lets go: the helper takes the lock nobody claims, lets it go and ends.
    drop(held);
    flock_wait::settled(&key);
    assert_eq!(helpers_since(&before), (1, 1), "the helper ended");
    assert_eq!(flock_wait::waiting(&key), None);
    MetaLock::acquire(&layout, Duration::ZERO).expect("the lock is free again");
    // The counts above are the proof; this only looks at where the process ended up.
    #[cfg(target_os = "linux")]
    assert_eq!(
        wait_for_threads(threads_before),
        threads_before,
        "the helper's thread is gone"
    );
}
