//! Loose object store: file format, idempotent and racing puts, verification on
//! read, directory-fsync batching, and the crash-injecting `FsOps`.
//!
//! Every test owns a fresh store root (`tempfile::tempdir()`), so tests are
//! independent and may run in any order or in parallel.

mod support;

use std::collections::BTreeSet;
use std::fs;
use std::io;
#[cfg(feature = "test-hooks")]
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Barrier, Condvar, Mutex};
use std::thread;
use std::time::Duration;

#[cfg(feature = "test-hooks")]
use camino::Utf8PathBuf;
use proptest::prelude::Rng;
use proptest::test_runner::{RngAlgorithm, TestRng};
use swarmops_core::{ObjectId, ObjectKind, hash_object};
#[cfg(feature = "test-hooks")]
use swarmops_store::{CRASH_AT_ENV, FS_CALLS_ENV};
use swarmops_store::{CrashAtFs, FsObjectStore, FsOps, Layout, ObjectStore, RealFs, StoreError};

#[cfg(feature = "test-hooks")]
use crate::support::child::{self, SIGABRT, this_test};
use crate::support::fs::{Call, Rendezvous, Watch, Watched};
use crate::support::new_root;

/// `"SOB1" | kind u8 | enc u8 | u16 rsvd | u64le ulen` (storage spec §2).
const HEADER_LEN: usize = 16;
const ENC_RAW: u8 = 0;
const ENC_ZSTD: u8 = 1;

/// Set only in the child that `crash_at_fs_aborts_child_at_call_k` spawns: the store root.
#[cfg(feature = "test-hooks")]
const CRASH_CHILD_ROOT: &str = "SWARMOPS_TEST_CRASH_CHILD_ROOT";

/// Set only in the children that `fs_calls_hook_reports_how_many_calls_a_process_made`
/// spawns: the store root.
#[cfg(feature = "test-hooks")]
const COUNT_CHILD_ROOT: &str = "SWARMOPS_TEST_COUNT_CHILD_ROOT";
#[cfg(feature = "test-hooks")]
const CRASH_PAYLOAD: &[u8] = b"this put must never become visible";

/// Set only in the children of `open_default_under_crash_at_puts_one_object_at_a_time`: the
/// store root.
#[cfg(feature = "test-hooks")]
const ONE_AT_A_TIME_ROOT: &str = "SWARMOPS_TEST_ONE_AT_A_TIME_ROOT";

fn real_store(layout: &Layout) -> FsObjectStore {
    FsObjectStore::open(layout.clone(), Box::new(RealFs))
}

/// Deterministic incompressible bytes.
fn random_bytes(len: usize) -> Vec<u8> {
    let mut out = vec![0; len];
    TestRng::deterministic_rng(RngAlgorithm::ChaCha).fill_bytes(&mut out);
    out
}

/// The entries directly inside `dir`, sorted; empty if `dir` does not exist.
fn files_in(dir: &Path) -> Vec<PathBuf> {
    match fs::read_dir(dir) {
        Ok(entries) => {
            let mut files: Vec<PathBuf> = entries.map(|e| e.expect("dir entry").path()).collect();
            files.sort();
            files
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(e) => panic!("read {}: {e}", dir.display()),
    }
}

/// Every visible object: the files in each `objects/<2hex>/` directory (not `objects/tmp`).
fn object_files(layout: &Layout) -> Vec<PathBuf> {
    let tmp = layout.objects_tmp().into_std_path_buf();
    files_in(layout.objects_dir().as_std_path())
        .into_iter()
        .filter(|dir| *dir != tmp)
        .flat_map(|dir| files_in(&dir))
        .collect()
}

/// What the store asked of a [`counting_store`]'s filesystem, and how it answers.
#[derive(Default)]
struct Calls {
    write_durable_temp: AtomicUsize,
    link_noclobber: AtomicUsize,
    fsynced_dirs: Mutex<Vec<PathBuf>>,
    /// Directories whose fsync fails, with an injected error that names the directory.
    failing_dirs: Mutex<Vec<PathBuf>>,
    /// Counts the directory fsyncs in flight (those that do not fail).
    dir_fsyncs: Rendezvous,
}

impl Calls {
    fn temps(&self) -> usize {
        self.write_durable_temp.load(Ordering::SeqCst)
    }

    fn links(&self) -> usize {
        self.link_noclobber.load(Ordering::SeqCst)
    }

    fn fsynced(&self) -> Vec<PathBuf> {
        self.fsynced_dirs.lock().unwrap().clone()
    }
}

/// Counts the calls the store makes: the real filesystem watched by them is a
/// [`counting_store`]'s.
impl Watch for Arc<Calls> {
    fn before(&self, call: &Call<'_>) -> io::Result<()> {
        match call.method {
            "write_durable_temp" => {
                self.write_durable_temp.fetch_add(1, Ordering::SeqCst);
            }
            "link_noclobber" => {
                self.link_noclobber.fetch_add(1, Ordering::SeqCst);
            }
            "fsync_dir" => {
                let p = call.path();
                self.fsynced_dirs.lock().unwrap().push(p.to_path_buf());
                if self.failing_dirs.lock().unwrap().iter().any(|dir| dir == p) {
                    return Err(io::Error::other(format!(
                        "injected: fsync_dir {}",
                        p.display()
                    )));
                }
                self.dir_fsyncs.arrive();
            }
            _ => {}
        }
        Ok(())
    }

    fn after(&self, call: &Call<'_>) {
        if call.method == "fsync_dir" {
            self.dir_fsyncs.depart();
        }
    }
}

/// A one-way gate: `wait` blocks until `open` has been called.
#[derive(Default)]
struct Gate {
    is_open: Mutex<bool>,
    opened: Condvar,
}

impl Gate {
    fn wait(&self) {
        let mut is_open = self.is_open.lock().unwrap();
        while !*is_open {
            is_open = self.opened.wait(is_open).unwrap();
        }
    }

    fn open(&self) {
        *self.is_open.lock().unwrap() = true;
        self.opened.notify_all();
    }
}

/// What makes the real filesystem's `fsync_dir` report its directory on `parked`, then wait
/// for `gate`.
struct ParkFsyncs {
    parked: mpsc::Sender<PathBuf>,
    gate: Arc<Gate>,
}

impl Watch for ParkFsyncs {
    fn before(&self, call: &Call<'_>) -> io::Result<()> {
        if call.method == "fsync_dir" {
            // A send error would only mean the test stopped listening.
            let _ = self.parked.send(call.path().to_path_buf());
            self.gate.wait();
        }
        Ok(())
    }
}

fn counting_store(layout: &Layout) -> (FsObjectStore, Arc<Calls>) {
    let calls = Arc::new(Calls::default());
    let fs = Watched(Arc::clone(&calls));
    (FsObjectStore::open(layout.clone(), Box::new(fs)), calls)
}

/// Three blob payloads whose objects land in two fan-out dirs: `a` and `b` share one, `c` has its own.
fn three_blobs_in_two_fanout_dirs() -> [Vec<u8>; 3] {
    let payload = |i: u32| format!("payload {i}").into_bytes();
    let fanout = |p: &[u8]| hash_object(ObjectKind::Blob, p).0[0];
    let a = payload(0);
    // Deterministic inputs; a 1-in-256 match turns up long before 10,000 candidates.
    let b = (1..10_000)
        .map(payload)
        .find(|p| fanout(p) == fanout(&a))
        .unwrap();
    let c = (1..10_000)
        .map(payload)
        .find(|p| fanout(p) != fanout(&a))
        .unwrap();
    [a, b, c]
}

/// A named edit of one stored object's file.
type Damage = (&'static str, ObjectId, fn(&mut Vec<u8>));

/// Applies `damage` to the file of `id`, reads `id` back, and restores the file.
fn get_damaged(
    store: &FsObjectStore,
    layout: &Layout,
    id: ObjectId,
    damage: fn(&mut Vec<u8>),
) -> Result<Option<(ObjectKind, Vec<u8>)>, StoreError> {
    let path = layout.object_path(id);
    let pristine = fs::read(&path).unwrap();
    let mut damaged = pristine.clone();
    damage(&mut damaged);
    fs::write(&path, &damaged).unwrap();
    let got = store.get(id);
    fs::write(&path, &pristine).unwrap();
    got
}

fn fanout_dir(layout: &Layout, blob: &[u8]) -> PathBuf {
    let object = layout.object_path(hash_object(ObjectKind::Blob, blob));
    object.parent().unwrap().as_std_path().to_path_buf()
}

#[test]
fn put_get_roundtrip_each_kind() {
    let (_root, layout) = new_root();
    let store = real_store(&layout);
    let random = random_bytes(4096);
    let kinds = [
        (ObjectKind::Blob, 0),
        (ObjectKind::Tree, 1),
        (ObjectKind::Changeset, 2),
    ];
    for (kind, kind_byte) in kinds {
        for payload in [b"".as_slice(), b"x".as_slice(), random.as_slice()] {
            let id = store.put(kind, payload).unwrap();
            assert_eq!(id, hash_object(kind, payload), "the id is the core hash");
            assert_eq!(store.get(id).unwrap(), Some((kind, payload.to_vec())));
            assert!(store.contains(id).unwrap());
            let file = fs::read(layout.object_path(id)).expect("object file at object_path(id)");
            assert_eq!(&file[0..4], b"SOB1");
            assert_eq!(file[4], kind_byte, "kind byte");
            let ulen = u64::try_from(payload.len()).unwrap().to_le_bytes();
            assert_eq!(&file[8..16], &ulen, "ulen");
        }
    }
    let never_stored = hash_object(ObjectKind::Blob, b"never stored");
    assert_eq!(store.get(never_stored).unwrap(), None);
    assert!(!store.contains(never_stored).unwrap());
}

#[test]
fn small_payload_stored_raw_large_payload_zstd() {
    let (_root, layout) = new_root();
    let store = real_store(&layout);
    // Puts a blob, checks it reads back, and returns (enc byte, file length).
    let stored = |payload: &[u8]| {
        let id = store.put(ObjectKind::Blob, payload).unwrap();
        assert_eq!(
            store.get(id).unwrap(),
            Some((ObjectKind::Blob, payload.to_vec()))
        );
        let file = fs::read(layout.object_path(id)).unwrap();
        (file[5], file.len())
    };

    assert_eq!(stored(&[b'a'; 100]).0, ENC_RAW, "100 B is stored raw");
    let large = vec![b'a'; 64 * 1024];
    let (enc, file_len) = stored(&large);
    assert_eq!(enc, ENC_ZSTD, "64 KiB of one byte is stored zstd");
    assert!(
        file_len < large.len(),
        "the zstd file ({file_len} B) is smaller than the payload"
    );

    assert_eq!(
        stored(&[b'b'; 127]).0,
        ENC_RAW,
        "below 128 B nothing is compressed"
    );
    assert_eq!(
        stored(&[b'b'; 128]).0,
        ENC_ZSTD,
        "from 128 B compressible payloads are zstd"
    );
    assert_eq!(
        stored(&random_bytes(4096)).0,
        ENC_RAW,
        "incompressible payloads stay raw"
    );
}

#[test]
fn put_is_idempotent_and_skips_rewrite() {
    let (_root, layout) = new_root();
    let (store, calls) = counting_store(&layout);
    let first = store.put(ObjectKind::Blob, b"same bytes").unwrap();
    assert_eq!((calls.temps(), calls.links()), (1, 1));

    let second = store.put(ObjectKind::Blob, b"same bytes").unwrap();
    assert_eq!(second, first);
    assert_eq!(
        (calls.temps(), calls.links()),
        (1, 1),
        "the second put writes no temp file and links nothing"
    );

    let batch = vec![(ObjectKind::Blob, b"same bytes".to_vec())];
    assert_eq!(store.put_batch(batch).unwrap(), vec![first]);
    assert_eq!(calls.temps(), 1, "put_batch skips existing objects too");
    assert_eq!(
        object_files(&layout),
        vec![layout.object_path(first).into_std_path_buf()]
    );
}

#[test]
fn concurrent_identical_puts_converge() {
    const THREADS: usize = 16;
    let (_root, layout) = new_root();
    let store = real_store(&layout);
    let payload = random_bytes(1 << 20);
    let barrier = Barrier::new(THREADS);
    let ids: Vec<ObjectId> = thread::scope(|s| {
        let workers: Vec<_> = (0..THREADS)
            .map(|_| {
                s.spawn(|| {
                    barrier.wait();
                    store.put(ObjectKind::Blob, &payload).unwrap()
                })
            })
            .collect();
        workers.into_iter().map(|w| w.join().unwrap()).collect()
    });

    let id = hash_object(ObjectKind::Blob, &payload);
    assert!(
        ids.iter().all(|got| *got == id),
        "every thread gets the same id"
    );
    assert_eq!(
        object_files(&layout),
        vec![layout.object_path(id).into_std_path_buf()],
        "exactly one object file"
    );
    assert_eq!(
        files_in(layout.objects_tmp().as_std_path()),
        Vec::<PathBuf>::new(),
        "every temp file is removed, the losers' included"
    );
    assert_eq!(store.get(id).unwrap(), Some((ObjectKind::Blob, payload)));
}

#[test]
fn corrupted_object_is_detected_on_get() {
    let (_root, layout) = new_root();
    let store = real_store(&layout);
    let raw = store.put(ObjectKind::Blob, &[7; 100]).unwrap();
    let zstd = store.put(ObjectKind::Blob, &vec![b'a'; 64 * 1024]).unwrap();
    assert_eq!(fs::read(layout.object_path(raw)).unwrap()[5], ENC_RAW);
    assert_eq!(fs::read(layout.object_path(zstd)).unwrap()[5], ENC_ZSTD);

    let cases: [Damage; 3] = [
        ("raw payload byte flipped", raw, |file| {
            file[HEADER_LEN + 50] ^= 0x01;
        }),
        ("zstd payload byte flipped", zstd, |file| {
            *file.last_mut().unwrap() ^= 0x01;
        }),
        ("kind byte changed to another valid kind", raw, |file| {
            file[4] = 1;
        }),
    ];
    for (what, id, damage) in cases {
        let got = get_damaged(&store, &layout, id, damage);
        assert!(
            matches!(got, Err(StoreError::Corrupt { id: bad }) if bad == id),
            "{what}: {got:?}"
        );
    }
}

#[test]
fn damaged_header_is_reported_as_bad_header() {
    let (_root, layout) = new_root();
    let store = real_store(&layout);
    let raw = store.put(ObjectKind::Blob, &[7; 100]).unwrap();
    let zstd = store.put(ObjectKind::Blob, &[b'a'; 4096]).unwrap();
    assert_eq!(fs::read(layout.object_path(raw)).unwrap()[5], ENC_RAW);
    assert_eq!(fs::read(layout.object_path(zstd)).unwrap()[5], ENC_ZSTD);

    let cases: [Damage; 9] = [
        ("bad magic", raw, |file| {
            file[0] = b'X';
        }),
        ("unknown kind", raw, |file| {
            file[4] = 3;
        }),
        ("unknown encoding", raw, |file| {
            file[5] = 2;
        }),
        ("nonzero reserved bytes", raw, |file| {
            file[6] = 1;
        }),
        ("header cut short", raw, |file| {
            file.truncate(HEADER_LEN - 1);
        }),
        ("raw ulen larger than the payload", raw, |file| {
            file[8] = 101;
        }),
        ("raw payload cut short", raw, |file| {
            file.pop();
        }),
        ("zstd ulen below the frame size", zstd, |file| {
            file[8..16].copy_from_slice(&4095_u64.to_le_bytes());
        }),
        ("zstd ulen above the frame size", zstd, |file| {
            file[8..16].copy_from_slice(&4097_u64.to_le_bytes());
        }),
    ];
    for (what, id, damage) in cases {
        let got = get_damaged(&store, &layout, id, damage);
        assert!(
            matches!(got, Err(StoreError::BadHeader { id: bad }) if bad == id),
            "{what}: {got:?}"
        );
    }
}

#[test]
fn flush_fsyncs_each_touched_dir_once() {
    let (_root, layout) = new_root();
    let (store, calls) = counting_store(&layout);
    let [a, b, c] = three_blobs_in_two_fanout_dirs();
    // Steady state: both fan-out dirs exist already (creating one also costs an
    // `objects/` fsync, see `flush_fsyncs_objects_dir_for_a_new_fanout_dir`).
    for payload in [&a, &c] {
        fs::create_dir_all(fanout_dir(&layout, payload)).unwrap();
    }
    for payload in [&a, &b, &c] {
        store.put(ObjectKind::Blob, payload).unwrap();
    }
    assert_eq!(
        calls.fsynced(),
        Vec::<PathBuf>::new(),
        "put leaves directory fsyncs to flush"
    );

    store.flush().unwrap();
    let mut fsynced = calls.fsynced();
    fsynced.sort();
    let mut expected = vec![fanout_dir(&layout, &a), fanout_dir(&layout, &c)];
    expected.sort();
    assert_eq!(
        fsynced, expected,
        "each touched fan-out dir exactly once; objects/ and objects/tmp never"
    );

    store.flush().unwrap();
    assert_eq!(calls.fsynced().len(), 2, "flush clears the touched set");
}

#[test]
fn flush_fsyncs_objects_dir_for_a_new_fanout_dir() {
    let (_root, layout) = new_root();
    let (store, calls) = counting_store(&layout);
    let [a, b, _] = three_blobs_in_two_fanout_dirs();
    let fanout = fanout_dir(&layout, &a);

    store.put(ObjectKind::Blob, &a).unwrap(); // creates the fan-out dir
    store.flush().unwrap();
    let mut first = calls.fsynced();
    first.sort();
    assert_eq!(
        first,
        vec![layout.objects_dir().into_std_path_buf(), fanout.clone()],
        "the new fan-out dir's own entry in objects/ is made durable too"
    );

    store.put(ObjectKind::Blob, &b).unwrap(); // same fan-out dir, which now exists
    store.flush().unwrap();
    assert_eq!(
        calls.fsynced()[2..],
        [fanout],
        "an existing fan-out dir needs only its own fsync"
    );
}

/// `count` blob payloads whose objects land in `count` different fan-out dirs.
fn blobs_in_distinct_fanout_dirs(layout: &Layout, count: usize) -> Vec<Vec<u8>> {
    let mut dirs = BTreeSet::new();
    (0..)
        .map(|i: u32| format!("spread {i}").into_bytes())
        .filter(|blob| dirs.insert(fanout_dir(layout, blob)))
        .take(count)
        .collect()
}

/// Puts `blobs` into `store` of `layout`. Every fan-out dir they land in exists beforehand
/// except the first blob's, which the put creates, so that `objects/` joins the touched set.
/// Returns the directories a flush must fsync, in path order: `objects/` first.
fn put_creating_one_fanout_dir(
    store: &FsObjectStore,
    layout: &Layout,
    blobs: &[Vec<u8>],
) -> Vec<PathBuf> {
    for blob in &blobs[1..] {
        fs::create_dir_all(fanout_dir(layout, blob)).unwrap();
    }
    let items = blobs.iter().map(|blob| (ObjectKind::Blob, blob.clone()));
    store.put_batch(items.collect()).unwrap();
    let mut dirs: Vec<PathBuf> = blobs.iter().map(|blob| fanout_dir(layout, blob)).collect();
    dirs.push(layout.objects_dir().into_std_path_buf());
    dirs.sort();
    dirs
}

#[test]
fn flush_at_parallelism_1_fsyncs_the_dirs_one_after_another_in_path_order() {
    let (_root, layout) = new_root();
    let (mut store, calls) = counting_store(&layout);
    store.set_parallelism(1);
    let blobs = blobs_in_distinct_fanout_dirs(&layout, 4);
    let dirs = put_creating_one_fanout_dir(&store, &layout, &blobs);
    assert_eq!(dirs[0], layout.objects_dir().as_std_path());
    store.flush().unwrap();
    assert_eq!(
        calls.fsynced(),
        dirs,
        "objects/, then each fan-out dir, in path order"
    );
    assert_eq!(calls.dir_fsyncs.most(), 1, "one at a time");
}

#[test]
fn flush_at_parallelism_8_fsyncs_the_same_dirs_at_once_and_any_failure_fails_it() {
    let (_root, layout) = new_root();
    let (mut store, calls) = counting_store(&layout);
    store.set_parallelism(8);
    let blobs = blobs_in_distinct_fanout_dirs(&layout, 17);
    let dirs = put_creating_one_fanout_dir(&store, &layout, &blobs);
    // Two dirs fail: the first fan-out dir (which one at a time would come right after
    // objects/) and a later one. Each of the others waits until eight are in flight, which
    // only fsyncs made at once can be.
    let failing = [dirs[1].clone(), dirs[9].clone()];
    calls.failing_dirs.lock().unwrap().extend(failing.clone());
    calls.dir_fsyncs.arm(8);
    match store.flush() {
        Err(StoreError::Io(e)) => assert_eq!(
            e.to_string(),
            format!("injected: fsync_dir {}", failing[0].display()),
            "the first failure in path order is the one reported"
        ),
        other => panic!("expected the injected error, got {other:?}"),
    }
    assert_eq!(
        calls.dir_fsyncs.most(),
        8,
        "eight fsyncs were in flight together"
    );
    let mut fsynced = calls.fsynced();
    fsynced.sort();
    assert_eq!(
        fsynced, dirs,
        "every dir's fsync was made, the failing ones too"
    );

    // The failed dirs stay touched, and only they: the next flush retries them.
    calls.dir_fsyncs.arm(0);
    calls.failing_dirs.lock().unwrap().clear();
    store.flush().unwrap();
    let mut retried = calls.fsynced().split_off(dirs.len());
    retried.sort();
    assert_eq!(retried, failing);
    store.flush().unwrap();
    assert_eq!(
        calls.fsynced().len(),
        dirs.len() + 2,
        "nothing left to flush"
    );
}

#[test]
fn concurrent_flush_waits_for_the_one_in_progress() {
    let (_root, layout) = new_root();
    let gate = Arc::new(Gate::default());
    let (parked_tx, parked_rx) = mpsc::channel();
    let fs = Watched(ParkFsyncs {
        parked: parked_tx,
        gate: Arc::clone(&gate),
    });
    let store = FsObjectStore::open(layout.clone(), Box::new(fs));
    // Two puts (two callers, say) into one existing fan-out dir: the touched set is {F}.
    let [a, b, _] = three_blobs_in_two_fanout_dirs();
    let fanout = fanout_dir(&layout, &a);
    fs::create_dir_all(&fanout).unwrap();
    store.put(ObjectKind::Blob, &a).unwrap();
    store.put(ObjectKind::Blob, &b).unwrap();

    // Assertions wait until the gate is open and both flushes are joined, so a failure
    // cannot leave the first flush parked forever.
    let (parked, early) = thread::scope(|s| {
        let store = &store;
        let first = s.spawn(move || store.flush());
        let parked = parked_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the first flush reaches fsync_dir");
        let (done_tx, done_rx) = mpsc::channel();
        let second = s.spawn(move || {
            let flushed = store.flush();
            let _ = done_tx.send(());
            flushed
        });
        let early = done_rx.recv_timeout(Duration::from_millis(200));
        gate.open();
        first.join().unwrap().unwrap();
        second.join().unwrap().unwrap();
        (parked, early)
    });
    assert_eq!(parked, fanout, "the first flush is fsyncing F");
    assert_eq!(
        early,
        Err(RecvTimeoutError::Timeout),
        "the second flush returned while the first was still parked in fsync_dir(F)"
    );
}

#[test]
fn flush_covers_objects_that_already_existed() {
    let (_root, layout) = new_root();
    // Another process linked this object into an existing fan-out dir but has not
    // fsynced that dir yet.
    fs::create_dir_all(fanout_dir(&layout, b"stored elsewhere")).unwrap();
    let id = real_store(&layout)
        .put(ObjectKind::Blob, b"stored elsewhere")
        .unwrap();

    let (store, calls) = counting_store(&layout);
    assert_eq!(
        store.put(ObjectKind::Blob, b"stored elsewhere").unwrap(),
        id
    );
    assert_eq!(calls.temps(), 0, "the existing object is not rewritten");
    store.flush().unwrap();
    assert_eq!(
        calls.fsynced(),
        vec![fanout_dir(&layout, b"stored elsewhere")],
        "the caller relies on the object, so its directory is made durable"
    );
}

#[test]
fn put_batch_returns_ids_in_input_order() {
    let (_root, layout) = new_root();
    let mut store = real_store(&layout);
    store.set_parallelism(4);
    let kinds = [ObjectKind::Blob, ObjectKind::Tree, ObjectKind::Changeset];
    let mut items: Vec<(ObjectKind, Vec<u8>)> = (0..64)
        .map(|i| (kinds[i % 3], format!("batch item {i}").into_bytes()))
        .collect();
    items.push(items[0].clone()); // the same object twice in one batch

    let ids = store.put_batch(items.clone()).unwrap();
    let expected: Vec<ObjectId> = items
        .iter()
        .map(|(kind, payload)| hash_object(*kind, payload))
        .collect();
    assert_eq!(ids, expected);
    for ((kind, payload), id) in items.into_iter().zip(ids) {
        assert_eq!(store.get(id).unwrap(), Some((kind, payload)));
    }
    assert_eq!(object_files(&layout).len(), 64);
    assert_eq!(store.put_batch(Vec::new()).unwrap(), Vec::<ObjectId>::new());
}

#[test]
fn list_returns_every_stored_object_in_id_order() {
    let (_root, layout) = new_root();
    let store = real_store(&layout);
    assert_eq!(
        store.list().unwrap(),
        [],
        "a store never written to lists nothing"
    );
    let mut ids: Vec<ObjectId> = [b"one".as_slice(), b"two", b"three"]
        .into_iter()
        .map(|payload| store.put(ObjectKind::Blob, payload).unwrap())
        .collect();
    ids.push(store.put(ObjectKind::Tree, b"a tree's bytes").unwrap());
    store.flush().unwrap();

    // Only the fan-out directories hold objects: temp files, stray entries and names this
    // store never writes are passed over.
    fs::write(layout.objects_tmp().join("leftover"), b"x").unwrap();
    let fanout = layout.object_path(ids[0]).parent().unwrap().to_owned();
    fs::write(fanout.join("not-an-object"), b"x").unwrap();
    fs::write(fanout.join("A".repeat(62)), b"x").unwrap();
    fs::create_dir(fanout.join("c".repeat(62))).unwrap();
    fs::write(layout.objects_dir().join("stray"), b"x").unwrap();
    fs::create_dir(layout.objects_dir().join("zz")).unwrap();
    fs::write(layout.objects_dir().join("zz").join("b".repeat(62)), b"x").unwrap();
    // Listing reads no object: a corrupt one is listed all the same (get finds it out).
    let corrupt = layout.object_path(ids[1]);
    let mut bytes = fs::read(&corrupt).unwrap();
    *bytes.last_mut().unwrap() ^= 0x01;
    fs::write(&corrupt, bytes).unwrap();

    ids.sort();
    assert_eq!(store.list().unwrap(), ids);
}

/// The batch the children of `open_default_under_crash_at_puts_one_object_at_a_time` put.
#[cfg(feature = "test-hooks")]
fn one_at_a_time_batch() -> Vec<(ObjectKind, Vec<u8>)> {
    (0..64)
        .map(|i| (ObjectKind::Blob, format!("one at a time {i}").into_bytes()))
        .collect()
}

#[test]
#[cfg(feature = "test-hooks")]
fn open_default_under_crash_at_puts_one_object_at_a_time() {
    const TEST: &str = "open_default_under_crash_at_puts_one_object_at_a_time";
    if let Some(root) = std::env::var_os(ONE_AT_A_TIME_ROOT) {
        let root = Utf8PathBuf::from_path_buf(root.into()).expect("UTF-8 store root");
        let store = FsObjectStore::open_default(Layout::new(root));
        let outcome = store.put_batch(one_at_a_time_batch());
        panic!("the crash hook did not fire: put_batch returned {outcome:?}");
    }
    // With its fan-out dir in place, a new object's put makes five FsOps calls: exists,
    // create_dir_all objects/tmp, exists of the fan-out dir, write_durable_temp,
    // link_noclobber. One object at a time, call 5 × 20 + 3 comes in the middle of the 21st
    // object's put, after 20 whole ones: the first 20, in every run.
    let whole = 20;
    let want: BTreeSet<ObjectId> = one_at_a_time_batch()[..whole]
        .iter()
        .map(|(kind, payload)| hash_object(*kind, payload))
        .collect();
    for run in 0..2 {
        let (_root, layout) = new_root();
        fs::create_dir_all(layout.objects_tmp()).unwrap();
        for byte in 0..=u8::MAX {
            fs::create_dir_all(layout.objects_dir().join(format!("{byte:02x}"))).unwrap();
        }
        let child = child::run(
            this_test(TEST)
                .env(ONE_AT_A_TIME_ROOT, layout.root())
                .env(CRASH_AT_ENV, (5 * whole + 3).to_string()),
        );
        assert_eq!(
            child.status.signal(),
            Some(SIGABRT),
            "run {run}: {:?}\n{}",
            child.status,
            String::from_utf8_lossy(&child.stderr),
        );
        let stored: BTreeSet<ObjectId> = real_store(&layout).list().unwrap().into_iter().collect();
        assert_eq!(
            stored, want,
            "run {run}: the first {whole} objects, and only they"
        );
    }
}

#[test]
#[cfg(feature = "test-hooks")]
fn crash_at_fs_aborts_child_at_call_k() {
    if let Some(root) = std::env::var_os(CRASH_CHILD_ROOT) {
        // Child: SWARMOPS_TEST_CRASH_AT is set, so `open_default` wraps RealFs in
        // CrashAtFs and this put aborts the process on its second FsOps call.
        let root = Utf8PathBuf::from_path_buf(root.into()).expect("UTF-8 store root");
        let store = FsObjectStore::open_default(Layout::new(root));
        let outcome = store.put(ObjectKind::Blob, CRASH_PAYLOAD);
        panic!("the crash hook did not fire: put returned {outcome:?}");
    }

    let (_root, layout) = new_root();
    let child = child::run(
        this_test("crash_at_fs_aborts_child_at_call_k")
            .env(CRASH_CHILD_ROOT, layout.root())
            .env(CRASH_AT_ENV, "2"),
    );
    assert_eq!(
        child.status.signal(),
        Some(SIGABRT),
        "the child must die by SIGABRT, got {:?}\n--- child stdout ---\n{}\n--- child stderr ---\n{}",
        child.status,
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr),
    );
    assert_eq!(
        object_files(&layout),
        Vec::<PathBuf>::new(),
        "no object file is visible after the crash"
    );
}

/// A [`CrashAtFs`] counts every call, and with a report file writes the count there when it
/// is dropped.
#[test]
fn crash_at_fs_counts_its_calls_and_reports_them_when_dropped() {
    let (_root, layout) = new_root();
    let report = layout.root().join("calls").into_std_path_buf();
    let fs = CrashAtFs::new(RealFs, 0).reporting_calls_to(report.clone());
    let dir = layout.root().join("d").into_std_path_buf();
    fs.create_dir_all(&dir).unwrap();
    assert!(fs.exists(&dir).unwrap());
    assert_eq!(fs.read(&dir.join("none")).unwrap(), None);
    assert_eq!(fs.calls(), 3);
    assert!(!report.exists(), "the count is written when it is dropped");
    drop(fs);
    assert_eq!(fs::read_to_string(&report).unwrap(), "3");
}

/// `SWARMOPS_TEST_FS_CALLS=<file>`: the object store `open_default` gives counts the calls of
/// the process and writes how many it made to the file when it is dropped, a crash test's dry
/// run; with `SWARMOPS_TEST_CRASH_AT=<k>` set too, it aborts at call k and writes nothing.
#[test]
#[cfg(feature = "test-hooks")]
fn fs_calls_hook_reports_how_many_calls_a_process_made() {
    if let Some(root) = std::env::var_os(COUNT_CHILD_ROOT) {
        let root = Utf8PathBuf::from_path_buf(root.into()).expect("UTF-8 store root");
        let mut store = FsObjectStore::open_default(Layout::new(root));
        // One directory fsync at a time, in path order: the k-th call is the same call.
        store.set_parallelism(1);
        store.put(ObjectKind::Blob, CRASH_PAYLOAD).unwrap();
        store.flush().unwrap();
        return;
    }
    // The same put and flush in this process, counted.
    let (_root, layout) = new_root();
    let counted = Arc::new(CrashAtFs::new(RealFs, 0));
    let mut store = FsObjectStore::open(layout, Box::new(Arc::clone(&counted)));
    store.set_parallelism(1);
    store.put(ObjectKind::Blob, CRASH_PAYLOAD).unwrap();
    store.flush().unwrap();
    let calls = counted.calls();
    assert!(calls > 3, "a put makes several calls: {calls}");

    for crash_at in [None, Some(calls)] {
        let (_root, layout) = new_root();
        let report = layout.root().join("calls").into_std_path_buf();
        let mut child = this_test("fs_calls_hook_reports_how_many_calls_a_process_made");
        child
            .env(COUNT_CHILD_ROOT, layout.root())
            .env(FS_CALLS_ENV, &report)
            .env_remove(CRASH_AT_ENV);
        if let Some(k) = crash_at {
            child.env(CRASH_AT_ENV, k.to_string());
        }
        let output = child::run(&mut child);
        let shown = format!(
            "{:?}\n--- child stderr ---\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        if crash_at.is_some() {
            assert_eq!(output.status.signal(), Some(SIGABRT), "{shown}");
            assert!(!report.exists(), "a run that crashes reports nothing");
        } else {
            assert!(output.status.success(), "{shown}");
            assert_eq!(fs::read_to_string(&report).unwrap(), calls.to_string());
        }
    }
}
