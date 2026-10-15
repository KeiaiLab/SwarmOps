//! `meta/lock` under contention: a writer that gives up leaves no lock behind, and 32 writers
//! in one process take turns through the kernel's flock queue without meeting `Busy`.
//!
//! The basic acquire, timeout and release tests are in `journal.rs`. Every test here owns a
//! fresh store root (`tempfile::tempdir()`), so tests are independent and may run in any order
//! or in parallel. The abandoned-waiter test watches the kernel's own lock table,
//! `/proc/locks`, which lists the holder of each `flock` lock and every waiter blocked on it.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::Barrier;
use std::thread;
use std::time::{Duration, Instant};

use camino::Utf8PathBuf;
use swarmops_store::{Layout, MetaLock, StoreError};
use tempfile::TempDir;

/// A fresh store root with its `meta/` directory, and its layout; the directory lives as long
/// as the `TempDir`.
fn new_meta() -> (TempDir, Layout) {
    let dir = tempfile::tempdir().expect("create store root");
    let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("temp dir is UTF-8");
    let layout = Layout::new(root);
    fs::create_dir_all(layout.meta_dir()).expect("create meta/");
    (dir, layout)
}

/// The `flock` locks on `path`'s file as `/proc/locks` lists them: how many are held, and how
/// many waiters are blocked on them (its `->` lines). A line names its file as
/// `<major>:<minor>:<inode>`, the device numbers in hex.
fn flocks_on(path: &Path) -> (usize, usize) {
    let meta = fs::metadata(path).expect("stat the lock file");
    let dev = meta.dev();
    // glibc's major()/minor() of a Linux dev_t.
    let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff);
    let minor = (dev & 0xff) | ((dev >> 12) & !0xff);
    let file = format!("{major:02x}:{minor:02x}:{}", meta.ino());
    let table = fs::read_to_string("/proc/locks").expect("read /proc/locks");
    let (mut held, mut waiting) = (0, 0);
    for line in table.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let (blocked, rest) = match fields.get(1) {
            Some(&"->") => (true, &fields[2..]),
            _ => (false, fields.get(1..).unwrap_or_default()),
        };
        if rest.first() == Some(&"FLOCK") && rest.get(4) == Some(&file.as_str()) {
            if blocked {
                waiting += 1;
            } else {
                held += 1;
            }
        }
    }
    (held, waiting)
}

/// Waits up to 10 s until `flocks_on(path)` is `want`, and returns what it was last.
fn wait_for_flocks(path: &Path, want: (usize, usize)) -> (usize, usize) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let now = flocks_on(path);
        if now == want || Instant::now() >= deadline {
            return now;
        }
        thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn abandoned_waiter_releases_the_lock_it_acquires_late() {
    let (_root, layout) = new_meta();
    let lock_file = layout.meta_lock().into_std_path_buf();
    let held = MetaLock::acquire(&layout, Duration::ZERO).unwrap();
    assert_eq!(flocks_on(&lock_file), (1, 0), "one holder, nobody waiting");

    let started = Instant::now();
    let got = MetaLock::acquire(&layout, Duration::from_millis(100));
    let waited = started.elapsed();
    assert!(matches!(got, Err(StoreError::Busy)), "{got:?}");
    assert!(
        waited >= Duration::from_millis(100),
        "gave up after {waited:?}"
    );
    // The writer gave up, but its wait goes on in the kernel's queue: a blocked flock cannot be
    // called off.
    assert_eq!(
        wait_for_flocks(&lock_file, (1, 1)),
        (1, 1),
        "the abandoned waiter is still queued behind the holder"
    );

    // Once the holder lets go, the abandoned waiter gets the lock, finds that nobody wants it
    // any more, and lets it go at once: no lock is left, and no waiter.
    drop(held);
    assert_eq!(
        wait_for_flocks(&lock_file, (0, 0)),
        (0, 0),
        "the abandoned waiter kept the lock it got"
    );
    let again = MetaLock::acquire(&layout, Duration::ZERO);
    assert!(again.is_ok(), "the lock is free again: {again:?}");
}

/// What one of [`thirty_two_threads_take_turns_without_busy`]'s threads saw: the wait of each of
/// its turns, and how many times the lock timed out (it then asks again).
struct Turns {
    waits: Vec<Duration>,
    busy: u64,
}

/// Nearest-rank percentile over sorted samples.
fn percentile(sorted: &[Duration], percent: usize) -> Duration {
    let rank = (sorted.len() * percent).div_ceil(100).max(1);
    sorted[rank - 1]
}

#[test]
fn thirty_two_threads_take_turns_without_busy() {
    const THREADS: usize = 32;
    const TURNS: usize = 16;
    /// How long a turn holds the lock: a little less than a transaction's two journal fsyncs.
    const HOLD: Duration = Duration::from_millis(2);
    let (_root, layout) = new_meta();
    let start = Barrier::new(THREADS);
    let turns: Vec<Turns> = thread::scope(|scope| {
        let threads: Vec<_> = (0..THREADS)
            .map(|_| {
                scope.spawn(|| {
                    start.wait();
                    let mut turns = Turns {
                        waits: Vec::with_capacity(TURNS),
                        busy: 0,
                    };
                    for _ in 0..TURNS {
                        // Back to back, as a writer that loops: the default 10 s timeout, and
                        // a `Busy` counted and asked again.
                        let asked = Instant::now();
                        let lock = loop {
                            match MetaLock::acquire(&layout, Duration::from_secs(10)) {
                                Ok(lock) => break lock,
                                Err(StoreError::Busy) => turns.busy += 1,
                                Err(e) => panic!("acquire: {e}"),
                            }
                        };
                        turns.waits.push(asked.elapsed());
                        thread::sleep(HOLD);
                        drop(lock);
                    }
                    turns
                })
            })
            .collect();
        threads.into_iter().map(|t| t.join().unwrap()).collect()
    });

    let busy: u64 = turns.iter().map(|t| t.busy).sum();
    let mut waits: Vec<Duration> = turns.iter().flat_map(|t| t.waits.clone()).collect();
    waits.sort_unstable();
    let worst = turns
        .iter()
        .map(|t| t.waits.iter().max().copied().unwrap_or_default())
        .max()
        .unwrap_or_default();
    println!(
        "{THREADS} threads x {TURNS} turns of {HOLD:?}: lock wait p50 {:?} p99 {:?} max {worst:?}, \
         Busy {busy}",
        percentile(&waits, 50),
        percentile(&waits, 99),
    );
    assert_eq!(waits.len(), THREADS * TURNS);
    assert_eq!(
        busy, 0,
        "no turn waited out the 10 s timeout (max wait {worst:?})"
    );
}
