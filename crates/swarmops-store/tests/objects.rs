//! Loose object store: file format, idempotent and racing puts, verification on
//! read, directory-fsync batching, and the crash-injecting `FsOps`.
//!
//! Every test owns a fresh store root (`tempfile::tempdir()`), so tests are
//! independent and may run in any order or in parallel.

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
use swarmops_store::{CRASH_AT_ENV, CrashAtFs, FS_CALLS_ENV, FsOps};
use swarmops_store::{FsMethod, FsObjectStore, Layout, ObjectStore, RealFs, StoreError};
#[cfg(feature = "test-hooks")]
use swarmops_testkit::child::{self, SIGABRT, this_test};
use swarmops_testkit::fs::{Call, Rendezvous, Watch, Watched};
use swarmops_testkit::store::{lying_zstd, new_root};
#[cfg(feature = "test-hooks")]
use swarmops_testkit::test_name;

/// `"SOB1" | kind u8 | enc u8 | u16 rsvd | u64le ulen` (storage spec §2).
const HEADER_LEN: usize = 16;
const ENC_RAW: u8 = 0;
const ENC_ZSTD: u8 = 1;

/// Set only in the child that `crash_at_fs_aborts_child_at_call_k` spawns: the store root.
#[cfg(feature = "test-hooks")]
const CRASH_CHILD_ROOT: &str = "SWARMOPS_TEST_CRASH_CHILD_ROOT";

/// Set only in the children that `crash_at_k_aborts_before_call_k_runs` spawns: the file they
/// append to.
#[cfg(feature = "test-hooks")]
const APPENDS_FILE: &str = "SWARMOPS_TEST_CRASH_APPENDS_FILE";

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
    FsObjectStore::open(layout.clone(), Box::new(RealFs::new()))
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
impl Watch for Calls {
    fn before(&self, call: &Call<'_>) -> io::Result<()> {
        match call.method {
            FsMethod::WriteDurableTemp => {
                self.write_durable_temp.fetch_add(1, Ordering::SeqCst);
            }
            FsMethod::LinkNoclobber => {
                self.link_noclobber.fetch_add(1, Ordering::SeqCst);
            }
            FsMethod::FsyncDir => {
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
    let fs = Watched(Arc::clone(&calls), RealFs::new());
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
fn a_raw_object_is_read_back_in_the_buffer_it_was_read_into() {
    let (_root, layout) = new_root();
    let store = real_store(&layout);
    let payload = random_bytes(4096);
    let id = store.put(ObjectKind::Blob, &payload).unwrap();
    assert_eq!(
        fs::read(layout.object_path(id)).unwrap()[5],
        ENC_RAW,
        "incompressible bytes are stored raw"
    );
    let (kind, got) = store.get(id).unwrap().expect("stored");
    assert_eq!((kind, &got), (ObjectKind::Blob, &payload));
    // The read buffer held the whole file: drained of its header in place, it keeps room for
    // the header still. A copy of the payload would have been allocated at the payload's length.
    assert!(
        got.capacity() >= got.len() + HEADER_LEN,
        "a payload of {} bytes in a buffer of {}",
        got.len(),
        got.capacity()
    );
}

#[test]
fn a_zstd_frame_that_records_a_huge_size_is_corrupt_without_allocating_it() {
    let (_root, layout) = new_root();
    let store = real_store(&layout);
    let id = store.put(ObjectKind::Blob, &[b'a'; 4096]).unwrap();
    // Each lie agrees with itself (header and frame record the same size), and the frame makes
    // no output at all: no read may reserve the size it claims before the output says otherwise.
    // A size past the object limit (512 MiB) is refused by the header alone, as `BadHeader`,
    // before any decoding; one within it is decoded and found out by its output, as `Corrupt`.
    let past_the_limit: [Damage; 2] = [
        ("2^40 bytes, which aborted the process", id, |file| {
            lying_zstd(file, 1 << 40);
        }),
        ("2^63 bytes, which panicked", id, |file| {
            lying_zstd(file, 1 << 63);
        }),
    ];
    for (what, id, damage) in past_the_limit {
        let got = get_damaged(&store, &layout, id, damage);
        assert!(
            matches!(got, Err(StoreError::BadHeader { id: bad }) if bad == id),
            "{what}: {got:?}"
        );
    }
    let within_it: [Damage; 2] = [
        (
            "64 MiB + 1, the first size decoded as a stream",
            id,
            |file| {
                lying_zstd(file, (64 << 20) + 1);
            },
        ),
        ("64 MiB, the largest size decoded in one pass", id, |file| {
            lying_zstd(file, 64 << 20);
        }),
    ];
    for (what, id, damage) in within_it {
        let got = get_damaged(&store, &layout, id, damage);
        assert!(
            matches!(got, Err(StoreError::Corrupt { id: bad }) if bad == id),
            "{what}: {got:?}"
        );
    }
}

/// A payload longer than the object limit is refused before it is hashed and before any
/// filesystem call (storage spec §2 rule 1): by `put`, and by `put_batch` where it follows two
/// payloads that fit, of which nothing is written either. Each oversized payload is a `vec!` of
/// zeros, whose pages stay untouched until something reads them. `put_hashed_batch`, which a
/// commit writes its objects through, refuses the same batch hashed by its caller.
#[test]
fn put_refuses_a_payload_past_the_object_limit_without_a_filesystem_call() {
    use swarmops_core::Hashed;
    use swarmops_store::{LimitKind, MAX_OBJECT_LEN};
    use swarmops_testkit::fs::RecordingFs;

    let (_root, layout) = new_root();
    let (fs, calls) = RecordingFs::new();
    let store = FsObjectStore::open(layout.clone(), Box::new(fs));
    let past = || vec![0_u8; usize::try_from(MAX_OBJECT_LEN + 1).unwrap()];
    let refused = |got: Result<(), StoreError>| match got {
        Err(StoreError::Limit { what, max, got }) => (what, max, got),
        other => panic!("not a limit: {other:?}"),
    };
    let limit = (LimitKind::ObjectBytes, 536_870_912, 536_870_913);

    assert_eq!(
        refused(store.put(ObjectKind::Blob, &past()).map(|_| ())),
        limit
    );
    let batch = vec![
        (ObjectKind::Blob, b"one".to_vec()),
        (ObjectKind::Tree, b"two".to_vec()),
        (ObjectKind::Blob, past()),
    ];
    assert_eq!(refused(store.put_batch(batch).map(|_| ())), limit);
    let hashed = vec![
        Hashed::new(ObjectKind::Blob, b"one".to_vec()),
        Hashed::new(ObjectKind::Tree, b"two".to_vec()),
        Hashed::new(ObjectKind::Blob, past()),
    ];
    assert_eq!(refused(store.put_hashed_batch(hashed).map(|_| ())), limit);
    assert_eq!(calls(), Vec::<String>::new(), "no filesystem call at all");
    assert_eq!(object_files(&layout), Vec::<PathBuf>::new());
}

/// An object file whose header records more than the object limit is `BadHeader` before
/// anything is allocated for its payload, raw or zstd (storage spec §2 rule 1): a raw header
/// that claims `MAX_OBJECT_LEN + 1` bytes over a 16-byte body, and a zstd header and frame that
/// both claim 2^40 bytes (the same lie within the limit is decoded, and found `Corrupt`).
#[test]
fn a_header_claiming_more_than_the_object_limit_is_bad_header() {
    use swarmops_store::MAX_OBJECT_LEN;

    let (_root, layout) = new_root();
    let store = real_store(&layout);
    let raw = store.put(ObjectKind::Blob, &[7; 16]).unwrap();
    let zstd = store.put(ObjectKind::Blob, &[b'a'; 4096]).unwrap();
    assert_eq!(fs::read(layout.object_path(raw)).unwrap()[5], ENC_RAW);
    assert_eq!(fs::read(layout.object_path(zstd)).unwrap()[5], ENC_ZSTD);

    let cases: [Damage; 2] = [
        ("a raw header one byte past the limit", raw, |file| {
            file[8..16].copy_from_slice(&(MAX_OBJECT_LEN + 1).to_le_bytes());
        }),
        (
            "a zstd header and frame that claim 2^40 bytes",
            zstd,
            |file| {
                lying_zstd(file, 1 << 40);
            },
        ),
    ];
    for (what, id, damage) in cases {
        let got = get_damaged(&store, &layout, id, damage);
        assert!(
            matches!(got, Err(StoreError::BadHeader { id: bad }) if bad == id),
            "{what}: {got:?}"
        );
    }
}

/// An object file longer than any this store writes (its 16-byte header and a raw payload of
/// `MAX_OBJECT_LEN` bytes: a zstd body is always shorter than its payload) is `BadHeader` by its
/// length alone, measured before the store makes any `FsOps` call, so that neither `get` nor
/// `read_chunks` reads it into memory (storage spec §2 rule 1). Each file here is sparse past the
/// raw object it replaces, costing no disk: 2 GiB behind the header it had, 2 GiB behind a header
/// that records the length of its body, and a byte past the bound behind one that records the most
/// an object holds. A file of the bound's own length passes the measure and is read. The recorder
/// fails each read of the object's file, should one come, so that no run of this test reads a
/// gigabyte.
#[test]
fn an_object_file_longer_than_any_the_store_writes_is_bad_header_before_it_is_read() {
    use std::os::unix::fs::FileExt;

    use swarmops_store::{FileOp, MAX_OBJECT_LEN};
    use swarmops_testkit::fs::RecordingFs;

    let (_root, layout) = new_root();
    let (recording, calls) = RecordingFs::new();
    let failing = recording.failing();
    let store = FsObjectStore::open(layout.clone(), Box::new(recording));
    let id = store.put(ObjectKind::Blob, &[7; 100]).unwrap();
    let path = layout.object_path(id);
    assert_eq!(fs::read(&path).unwrap()[5], ENC_RAW);
    let read = format!("read {}", path.file_name().unwrap());
    failing.set(Box::leak(read.clone().into_boxed_str()));
    calls();

    // The object's file, made `len` bytes long, its header recording a payload of `ulen` bytes.
    let plant = |len: u64, ulen: u64| {
        let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(len).unwrap();
        file.write_all_at(&ulen.to_le_bytes(), 8).unwrap();
    };
    let bound = 16 + MAX_OBJECT_LEN;
    let too_long = [
        ("2 GiB behind the header it had", 2 << 30, 100),
        (
            "2 GiB behind a header of its body's length",
            2 << 30,
            (2 << 30) - 16,
        ),
        ("a byte past the bound", bound + 1, MAX_OBJECT_LEN),
    ];
    for (what, len, ulen) in too_long {
        plant(len, ulen);
        let got = store.get(id);
        assert!(
            matches!(got, Err(StoreError::BadHeader { id: bad }) if bad == id),
            "get, {what}: {got:?}"
        );
        let mut pieces = 0;
        let got = store.read_chunks(id, &mut |_| pieces += 1);
        assert!(
            matches!(got, Err(StoreError::BadHeader { id: bad }) if bad == id),
            "read_chunks, {what}: {got:?}"
        );
        assert_eq!(pieces, 0, "read_chunks, {what}: no piece is visited");
        assert_eq!(calls(), Vec::<String>::new(), "{what}: never read");
    }

    // The bound is a length an object file can have: both read the file, which the recorder fails.
    plant(bound, MAX_OBJECT_LEN);
    let rel = path.strip_prefix(layout.root()).unwrap();
    let failed_read = |got: Option<StoreError>| match got {
        Some(StoreError::Io(failure)) => {
            assert_eq!(failure.op, FileOp::Fs(FsMethod::Read));
            assert_eq!(failure.file.as_deref(), Some(rel));
        }
        other => panic!("expected the read's injected failure, got {other:?}"),
    };
    failed_read(store.get(id).err());
    failed_read(store.read_chunks(id, &mut |_| ()).err());
    assert_eq!(calls(), [read.clone(), read]);
}

/// One byte past the largest payload decoded in one pass, 64 MiB, `get` decodes a zstd object as
/// a stream, into a buffer that grows only as the output arrives: past the payload's length once
/// its last byte arrives. A payload of 64 MiB it decodes in one pass, into a buffer of the size
/// the file records. So the two sizes pin the cap (with a cap of 1 GiB, both would be read in
/// one pass), and the stream round-trips a real object. About 0.4 s and 140 MB at most.
#[test]
fn a_zstd_object_past_the_cap_round_trips_through_the_store() {
    let (_root, layout) = new_root();
    let store = real_store(&layout);
    for (len, streamed) in [((64 << 20) + 1, true), (64 << 20, false)] {
        let payload = vec![0; len];
        let id = store.put(ObjectKind::Blob, &payload).unwrap();
        let file = fs::read(layout.object_path(id)).unwrap();
        assert_eq!(file[5], ENC_ZSTD);
        assert!(
            file.len() < 64 << 10,
            "{} bytes of zstd for {len} zeros",
            file.len()
        );
        let (kind, got) = store.get(id).unwrap().expect("stored");
        assert!(
            kind == ObjectKind::Blob && got == payload,
            "the {len} bytes come back"
        );
        assert_eq!(
            got.capacity() > got.len(),
            streamed,
            "{len} bytes in a buffer of {}",
            got.capacity()
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
        Err(StoreError::Io(failure)) => {
            let fanout = failing[0].strip_prefix(layout.root()).unwrap();
            assert_eq!(
                (failure.op, failure.file.as_deref()),
                (
                    swarmops_store::FileOp::Fs(FsMethod::FsyncDir),
                    camino::Utf8Path::from_path(fanout)
                ),
                "the first failure in path order is the one reported, by its directory in the \
                 store: {failure}"
            );
            assert_eq!(
                failure.source.to_string(),
                format!("injected: fsync_dir {}", failing[0].display()),
                "the double's own error, as it came"
            );
        }
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
    let fs = Watched(
        ParkFsyncs {
            parked: parked_tx,
            gate: Arc::clone(&gate),
        },
        RealFs::new(),
    );
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

/// A fan-out directory that cannot be listed (a regular file in its place: `ENOTDIR`) fails
/// `list` with the listing's failure (`read_dir`), named by its place in the store, never by
/// the store's root. Root-safe: no file mode is involved.
#[test]
fn list_names_a_fanout_directory_it_cannot_list() {
    use camino::Utf8Path;
    use swarmops_store::FileOp;

    let (_root, layout) = new_root();
    let store = real_store(&layout);
    fs::create_dir_all(layout.objects_dir()).unwrap();
    fs::write(layout.objects_dir().join("ab"), b"not a directory").unwrap();
    match store.list() {
        Err(StoreError::Io(failure)) => {
            assert_eq!(
                (failure.op, failure.file.as_deref(), failure.kind()),
                (
                    FileOp::ReadDir,
                    Some(Utf8Path::new("objects/ab")),
                    io::ErrorKind::NotADirectory
                ),
                "{failure}"
            );
            let text = failure.to_string();
            assert!(!text.contains(layout.root().as_str()), "{text}");
        }
        other => panic!("expected the listing's failure, got {other:?}"),
    }
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
    // With its fan-out dir in place, a new object's put makes four FsOps calls: exists, exists
    // of the fan-out dir, write_durable_temp, link_noclobber; and the batch's first new object
    // one more before its temp file, create_dir_all objects/tmp, made once a batch. One object
    // at a time, call 1 + 4 × 20 + 3 comes in the middle of the 21st object's put, after 20
    // whole ones: the first 20, in every run.
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
            this_test(&test_name!(TEST))
                .env(ONE_AT_A_TIME_ROOT, layout.root())
                .env(CRASH_AT_ENV, (1 + 4 * whole + 3).to_string()),
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
        this_test(&test_name!("crash_at_fs_aborts_child_at_call_k"))
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

/// A [`CrashAtFs`] aborts at call k before that call runs, and not before: a child that makes
/// three appends to one file, crashed at k = 1, 2 and 3, leaves the first k - 1 of them, and at
/// k = 4, past its last call, ends normally with all three.
#[test]
#[cfg(feature = "test-hooks")]
fn crash_at_k_aborts_before_call_k_runs() {
    if let Some(file) = std::env::var_os(APPENDS_FILE) {
        let k = std::env::var(CRASH_AT_ENV).unwrap().parse().unwrap();
        let fs = CrashAtFs::new(RealFs::new(), k);
        for append in [b"1", b"2", b"3"] {
            fs.append(Path::new(&file), append).unwrap();
        }
        return;
    }
    for k in 1..=4_u64 {
        let (_root, layout) = new_root();
        let file = layout.root().join("appends");
        let child = child::run(
            this_test(&test_name!("crash_at_k_aborts_before_call_k_runs"))
                .env(APPENDS_FILE, &file)
                .env(CRASH_AT_ENV, k.to_string()),
        );
        let ended_as_expected = if k <= 3 {
            child.status.signal() == Some(SIGABRT)
        } else {
            child.status.success()
        };
        assert!(
            ended_as_expected,
            "k={k}: {:?}\n{}",
            child.status,
            String::from_utf8_lossy(&child.stderr)
        );
        let held = fs::read(&file).unwrap_or_default();
        let appended = usize::try_from(k - 1).unwrap().min(3);
        assert_eq!(held, b"123"[..appended], "k={k}: the appends before call k");
    }
}

/// A [`CrashAtFs`] counts every call, and with a report file writes the count there when it
/// is dropped.
#[test]
#[cfg(feature = "test-hooks")]
fn crash_at_fs_counts_its_calls_and_reports_them_when_dropped() {
    let (_root, layout) = new_root();
    let report = layout.root().join("calls").into_std_path_buf();
    let fs = CrashAtFs::new(RealFs::new(), 0).reporting_calls_to(report.clone());
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
    let counted = Arc::new(CrashAtFs::new(RealFs::new(), 0));
    let mut store = FsObjectStore::open(layout, Box::new(Arc::clone(&counted)));
    store.set_parallelism(1);
    store.put(ObjectKind::Blob, CRASH_PAYLOAD).unwrap();
    store.flush().unwrap();
    let calls = counted.calls();
    assert!(calls > 3, "a put makes several calls: {calls}");

    for crash_at in [None, Some(calls)] {
        let (_root, layout) = new_root();
        let report = layout.root().join("calls").into_std_path_buf();
        let mut child = this_test(&test_name!(
            "fs_calls_hook_reports_how_many_calls_a_process_made"
        ));
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

/// Puts of objects their caller hashed ([`ObjectStore::put_hashed_batch`], storage spec §8): each
/// object's file placed once, the outcome saying whether this call placed it, and `objects/tmp`
/// made at most once a batch.
mod hashed {
    use std::os::unix::fs::MetadataExt;

    use swarmops_core::Hashed;
    use swarmops_store::PutOutcome;
    use swarmops_testkit::fs::RecordingFs;

    use super::*;

    /// One object of `kind` for each of `payloads`, hashed.
    fn hashed(kind: ObjectKind, payloads: &[&[u8]]) -> Vec<Hashed> {
        payloads
            .iter()
            .map(|payload| Hashed::new(kind, payload.to_vec()))
            .collect()
    }

    /// The outcome of a put of `kind`/`payload` that placed its file (`written`) or did not.
    fn outcome(kind: ObjectKind, payload: &[u8], written: bool) -> PutOutcome {
        PutOutcome::new(hash_object(kind, payload), written)
    }

    #[test]
    fn put_hashed_batch_reports_what_it_placed() {
        let (_root, layout) = new_root();
        let mut store = real_store(&layout);
        store.set_parallelism(4);
        let three: [&[u8]; 3] = [b"one", b"two", b"three"];
        let placed = store
            .put_hashed_batch(hashed(ObjectKind::Blob, &three))
            .unwrap();
        let want = |written| three.map(|p| outcome(ObjectKind::Blob, p, written));
        assert_eq!(placed, want(true), "three new objects, in input order");
        let again = store
            .put_hashed_batch(hashed(ObjectKind::Blob, &three))
            .unwrap();
        assert_eq!(again, want(false), "the same three again: none placed");

        // A batch of new and stored objects, of each kind: only the new ones are placed.
        let mut mixed = hashed(ObjectKind::Blob, &[b"two", b"four"]);
        mixed.extend(hashed(ObjectKind::Tree, &[b"one", b"a tree"]));
        mixed.push(Hashed::new(ObjectKind::Changeset, b"a changeset".to_vec()));
        let placed = store.put_hashed_batch(mixed.clone()).unwrap();
        assert_eq!(
            placed,
            [
                outcome(ObjectKind::Blob, b"two", false),
                outcome(ObjectKind::Blob, b"four", true),
                outcome(ObjectKind::Tree, b"one", true),
                outcome(ObjectKind::Tree, b"a tree", true),
                outcome(ObjectKind::Changeset, b"a changeset", true),
            ],
            "a tree of the same bytes as a stored blob is another object"
        );
        for (item, put) in mixed.into_iter().zip(placed) {
            let stored = store.get(put.id).unwrap();
            assert_eq!(stored, Some((item.kind(), item.into_payload())), "{put:?}");
        }
        assert_eq!(store.put_hashed_batch(Vec::new()).unwrap(), []);
    }

    #[test]
    fn racing_put_hashed_batches_place_each_object_once() {
        const THREADS: usize = 8;
        let (_root, layout) = new_root();
        let store = real_store(&layout);
        let payload = random_bytes(1 << 20);
        let barrier = Barrier::new(THREADS);
        let outcomes: Vec<PutOutcome> = thread::scope(|s| {
            let workers: Vec<_> = (0..THREADS)
                .map(|_| {
                    s.spawn(|| {
                        let item = Hashed::new(ObjectKind::Blob, payload.clone());
                        barrier.wait();
                        store.put_hashed_batch(vec![item]).unwrap()
                    })
                })
                .collect();
            (workers.into_iter())
                .flat_map(|worker| worker.join().unwrap())
                .collect()
        });

        let id = hash_object(ObjectKind::Blob, &payload);
        assert!(outcomes.iter().all(|o| o.id == id), "{outcomes:?}");
        let placed = outcomes.iter().filter(|o| o.written).count();
        assert_eq!(
            placed, 1,
            "exactly one thread placed the file: {outcomes:?}"
        );
        let file = layout.object_path(id).into_std_path_buf();
        assert_eq!(
            object_files(&layout),
            std::slice::from_ref(&file),
            "one object file"
        );
        assert_eq!(
            fs::metadata(&file).unwrap().nlink(),
            1,
            "no temp file names it"
        );
        assert_eq!(
            files_in(layout.objects_tmp().as_std_path()),
            Vec::<PathBuf>::new(),
            "every temp file is removed, the losers' included"
        );
    }

    #[test]
    fn put_hashed_batch_makes_objects_tmp_at_most_once() {
        let (_root, layout) = new_root();
        let (fs, calls) = RecordingFs::new();
        let fs = fs.naming_paths_in(layout.root().as_std_path().to_path_buf());
        let mut store = FsObjectStore::open(layout.clone(), Box::new(fs));
        store.set_parallelism(4);
        let made = |calls: Vec<String>| {
            (calls.iter())
                .filter(|call| *call == "create_dir_all objects/tmp")
                .count()
        };
        let first: [&[u8]; 3] = [b"first 0", b"first 1", b"first 2"];

        assert!(!layout.objects_tmp().exists(), "a fresh root has none");
        let placed = store
            .put_hashed_batch(hashed(ObjectKind::Blob, &first))
            .unwrap();
        assert!(placed.iter().all(|o| o.written), "{placed:?}");
        assert_eq!(made(calls()), 1, "made once for three new objects");
        assert!(layout.objects_tmp().is_dir());

        store
            .put_hashed_batch(hashed(ObjectKind::Blob, &first))
            .unwrap();
        assert_eq!(made(calls()), 0, "a batch of stored objects writes nothing");

        let second: [&[u8]; 3] = [b"second 0", b"second 1", b"second 2"];
        store
            .put_hashed_batch(hashed(ObjectKind::Blob, &second))
            .unwrap();
        assert_eq!(made(calls()), 1, "once a batch, not once a store");
    }
}

/// An object's kind and length from its file's header alone ([`ObjectStore::stat`]), and an
/// object a caller relies on without writing it ([`ObjectStore::retain`]).
mod stat {
    use swarmops_store::{FileOp, ObjectStat};
    use swarmops_testkit::fs::RecordingFs;

    use super::*;

    /// A store of `layout` on a [`RecordingFs`] that names each call by its path in the store,
    /// and the function that returns the calls made since it last ran.
    fn recording_store(layout: &Layout) -> (FsObjectStore, impl Fn() -> Vec<String>) {
        let (fs, calls) = RecordingFs::new();
        let fs = fs.naming_paths_in(layout.root().as_std_path().to_path_buf());
        (FsObjectStore::open(layout.clone(), Box::new(fs)), calls)
    }

    /// Applies `damage` to the file of `id`, stats `id`, and restores the file.
    fn stat_damaged(
        store: &FsObjectStore,
        layout: &Layout,
        id: ObjectId,
        damage: fn(&mut Vec<u8>),
    ) -> Result<Option<ObjectStat>, StoreError> {
        let path = layout.object_path(id);
        let pristine = fs::read(&path).unwrap();
        let mut damaged = pristine.clone();
        damage(&mut damaged);
        fs::write(&path, &damaged).unwrap();
        let got = store.stat(id);
        fs::write(&path, &pristine).unwrap();
        got
    }

    /// A damage that leaves the header whole, and the length `stat` reports through it.
    type PastTheHeader = (&'static str, fn(&mut Vec<u8>), u64);

    /// What `stat` gives for a stored object of `kind` whose header records `size`.
    fn stat_of(kind: ObjectKind, size: u64) -> ObjectStat {
        ObjectStat::new(kind, size)
    }

    #[test]
    fn stat_reads_the_header_only() {
        let (_root, layout) = new_root();
        let (store, calls) = recording_store(&layout);
        let large = store
            .put(ObjectKind::Blob, &random_bytes(10 << 20))
            .unwrap();
        let zstd = store.put(ObjectKind::Blob, &[b'a'; 4096]).unwrap();
        let tree = store.put(ObjectKind::Tree, b"a tree's bytes").unwrap();
        let raw = store.put(ObjectKind::Changeset, &[7; 100]).unwrap();
        assert_eq!(fs::read(layout.object_path(large)).unwrap()[5], ENC_RAW);
        assert_eq!(fs::read(layout.object_path(zstd)).unwrap()[5], ENC_ZSTD);
        calls();

        let stat = |id| store.stat(id).unwrap();
        assert_eq!(stat(large), Some(stat_of(ObjectKind::Blob, 10 << 20)));
        assert_eq!(
            stat(zstd),
            Some(stat_of(ObjectKind::Blob, 4096)),
            "the payload's length, not the file's"
        );
        assert_eq!(stat(tree), Some(stat_of(ObjectKind::Tree, 14)));
        assert_eq!(stat(raw), Some(stat_of(ObjectKind::Changeset, 100)));
        assert_eq!(stat(hash_object(ObjectKind::Blob, b"never stored")), None);
        assert_eq!(
            calls(),
            Vec::<String>::new(),
            "no FsOps call: the header is read through std::fs"
        );

        // A header this store does not write is `BadHeader`, as `get` finds it.
        let cases: [Damage; 6] = [
            ("bad magic", raw, |file| file[0] = b'X'),
            ("unknown kind", raw, |file| file[4] = 3),
            ("unknown encoding", raw, |file| file[5] = 2),
            ("nonzero reserved bytes", raw, |file| file[7] = 1),
            ("header cut short", raw, |file| {
                file.truncate(HEADER_LEN - 1);
            }),
            ("an empty file", raw, Vec::clear),
        ];
        for (what, id, damage) in cases {
            let got = stat_damaged(&store, &layout, id, damage);
            assert!(
                matches!(got, Err(StoreError::BadHeader { id: bad }) if bad == id),
                "{what}: {got:?}"
            );
        }
        // Nothing past the header is read: the payload's damage is `get`'s to find, and the
        // length is the header's own, however large, since nothing is allocated by it.
        let past_the_header: [PastTheHeader; 2] = [
            (
                "raw payload cut short",
                |file| {
                    file.pop();
                },
                100,
            ),
            (
                "a length no payload has",
                |file| file[8..16].copy_from_slice(&u64::MAX.to_le_bytes()),
                u64::MAX,
            ),
        ];
        for (what, damage, size) in past_the_header {
            let got = stat_damaged(&store, &layout, raw, damage);
            let want = stat_of(ObjectKind::Changeset, size);
            assert_eq!(got.unwrap(), Some(want), "{what}");
        }

        // A path that is no regular file fails as `get` fails, named by its place in the store.
        let path = layout.object_path(raw);
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        match store.stat(raw) {
            Err(StoreError::Io(failure)) => {
                let file = path.strip_prefix(layout.root()).unwrap();
                assert_eq!(
                    (failure.op, failure.file.as_deref(), failure.kind()),
                    (FileOp::Open, Some(file), io::ErrorKind::InvalidData),
                    "{failure}"
                );
            }
            other => panic!("expected an I/O failure, got {other:?}"),
        }
        assert_eq!(calls(), Vec::<String>::new(), "still no FsOps call");
    }

    #[test]
    fn retain_makes_the_next_flush_sync_the_fan_out() {
        let (_root, layout) = new_root();
        // Another writer, in another process say, linked the object and has not flushed yet.
        let id = real_store(&layout)
            .put(ObjectKind::Blob, b"stored elsewhere")
            .unwrap();
        let fanout = format!("fsync_dir objects/{:02x}", id.0[0]);
        let (store, calls) = recording_store(&layout);

        assert_eq!(
            store.retain(id).unwrap(),
            Some(stat_of(ObjectKind::Blob, 16)),
            "what stat gives"
        );
        assert_eq!(calls(), Vec::<String>::new(), "retain makes no FsOps call");
        store.flush().unwrap();
        assert_eq!(
            calls(),
            [fanout],
            "one fsync, of the object's fan-out directory"
        );
        store.flush().unwrap();
        assert_eq!(calls(), Vec::<String>::new(), "the flush took it");

        // An object that is not stored, or whose header is damaged, leaves nothing to flush.
        let missing = hash_object(ObjectKind::Blob, b"never stored");
        assert_eq!(store.retain(missing).unwrap(), None);
        let path = layout.object_path(id);
        let mut file = fs::read(&path).unwrap();
        file[0] = b'X';
        fs::write(&path, &file).unwrap();
        assert!(matches!(store.retain(id), Err(StoreError::BadHeader { id: bad }) if bad == id));
        store.flush().unwrap();
        assert_eq!(calls(), Vec::<String>::new(), "nothing to fsync");
    }
}

/// `read_chunks`: an object's payload handed over in pieces as it decodes, and verified whole, as
/// `get` verifies it (storage spec §2 rule 1, §8).
mod read_chunks {
    use swarmops_store::ObjectStat;

    use super::*;

    /// What `read_chunks` of an object returns, and the pieces it handed its visitor, in order.
    type Visited = (Result<Option<ObjectStat>, StoreError>, Vec<Vec<u8>>);

    /// The lines of [`numbered_lines`] in a payload of 240,000 bytes.
    const LINES: u32 = 20_000;

    /// `count` lines of 12 bytes, `line 000000\n` numbered from 0: text that zstd shrinks.
    fn numbered_lines(count: u32) -> Vec<u8> {
        (0..count)
            .flat_map(|n| format!("line {n:06}\n").into_bytes())
            .collect()
    }

    fn visit(store: &FsObjectStore, id: ObjectId) -> Visited {
        let mut pieces = Vec::new();
        let got = store.read_chunks(id, &mut |piece: &[u8]| pieces.push(piece.to_vec()));
        (got, pieces)
    }

    /// Applies `damage` to the file of `id`, reads `id` in pieces, and restores the file.
    fn visit_damaged(
        store: &FsObjectStore,
        layout: &Layout,
        id: ObjectId,
        damage: fn(&mut Vec<u8>),
    ) -> Visited {
        let path = layout.object_path(id);
        let pristine = fs::read(&path).unwrap();
        let mut damaged = pristine.clone();
        damage(&mut damaged);
        fs::write(&path, &damaged).unwrap();
        let got = visit(store, id);
        fs::write(&path, &pristine).unwrap();
        got
    }

    /// Objects of each kind, raw and zstd, come back in pieces that concatenate to what `get`
    /// returns, with their kind and payload length: a raw payload in one piece (none when it is
    /// empty), from the buffer its file was read into; a zstd payload as its stream decodes, so a
    /// megabyte of it in more than one. An object that is not stored is `None`, nothing visited.
    #[test]
    fn read_chunks_visits_the_payload_in_order() {
        let (_root, layout) = new_root();
        let store = real_store(&layout);
        let cases = [
            (ObjectKind::Blob, random_bytes(1 << 20), ENC_RAW),
            (ObjectKind::Blob, numbered_lines(100_000), ENC_ZSTD),
            (ObjectKind::Tree, b"a tree's bytes".to_vec(), ENC_RAW),
            (ObjectKind::Changeset, vec![b'c'; 4096], ENC_ZSTD),
            (ObjectKind::Blob, Vec::new(), ENC_RAW),
        ];
        for (kind, payload, enc) in cases {
            let id = store.put(kind, &payload).unwrap();
            let size = u64::try_from(payload.len()).unwrap();
            let what = format!("{kind:?} of {size} bytes");
            assert_eq!(fs::read(layout.object_path(id)).unwrap()[5], enc, "{what}");
            let (got, pieces) = visit(&store, id);
            assert_eq!(got.unwrap(), Some(ObjectStat::new(kind, size)), "{what}");
            let whole = store.get(id).unwrap().expect("stored").1;
            assert!(
                pieces.concat() == whole,
                "{what}: the pieces are the payload"
            );
            assert!(whole == payload, "{what}");
            if enc == ENC_RAW {
                assert_eq!(pieces.len(), usize::from(size > 0), "{what}");
            } else if size > 1 << 20 {
                assert!(pieces.len() > 1, "{what}: {} piece(s)", pieces.len());
            }
        }

        let missing = hash_object(ObjectKind::Blob, b"never stored");
        let (got, pieces) = visit(&store, missing);
        assert_eq!((got.unwrap(), pieces.len()), (None, 0));
    }

    /// A damaged object fails as `get` fails it, wherever the damage lies: a header this store
    /// does not write (one past the object limit among them) is `BadHeader` before any piece is
    /// visited, and bytes that are not the object's are `Corrupt`, found once the pieces before the
    /// damage have gone by (the caller drops what it kept of them).
    #[test]
    fn read_chunks_verifies_the_whole_payload() {
        use swarmops_store::MAX_OBJECT_LEN;

        let (_root, layout) = new_root();
        let store = real_store(&layout);
        let raw = store
            .put(ObjectKind::Blob, &random_bytes(200 << 10))
            .unwrap();
        let zstd = store.put(ObjectKind::Blob, &numbered_lines(LINES)).unwrap();
        assert_eq!(fs::read(layout.object_path(raw)).unwrap()[5], ENC_RAW);
        assert_eq!(fs::read(layout.object_path(zstd)).unwrap()[5], ENC_ZSTD);

        let corrupt: [Damage; 6] = [
            ("raw payload byte flipped near its end", raw, |file| {
                file[HEADER_LEN + (190 << 10)] ^= 0x01;
            }),
            ("kind byte changed to another valid kind", raw, |file| {
                file[4] = 1;
            }),
            ("zstd payload byte flipped mid-file", zstd, |file| {
                let middle = file.len() / 2;
                file[middle] ^= 0x01;
            }),
            ("zstd checksum flipped", zstd, |file| {
                *file.last_mut().unwrap() ^= 0x01;
            }),
            ("zstd frame cut short", zstd, |file| {
                file.pop();
            }),
            (
                "a frame that records 4 KiB and makes nothing",
                zstd,
                |file| {
                    lying_zstd(file, 4096);
                },
            ),
        ];
        for (what, id, damage) in corrupt {
            let (got, _) = visit_damaged(&store, &layout, id, damage);
            assert!(
                matches!(got, Err(StoreError::Corrupt { id: bad }) if bad == id),
                "{what}: {got:?}"
            );
        }

        let bad_header: [Damage; 7] = [
            ("bad magic", raw, |file| {
                file[0] = b'X';
            }),
            ("header cut short", raw, |file| {
                file.truncate(HEADER_LEN - 1);
            }),
            ("raw payload cut short", raw, |file| {
                file.pop();
            }),
            ("zstd ulen below the frame size", zstd, |file| {
                let size = u64::from(LINES) * 12 - 1;
                file[8..16].copy_from_slice(&size.to_le_bytes());
            }),
            ("zstd ulen above the frame size", zstd, |file| {
                let size = u64::from(LINES) * 12 + 1;
                file[8..16].copy_from_slice(&size.to_le_bytes());
            }),
            // The object limit's cases, as `get` refuses them.
            ("a raw header one byte past the object limit", raw, |file| {
                file[8..16].copy_from_slice(&(MAX_OBJECT_LEN + 1).to_le_bytes());
            }),
            (
                "a zstd header and frame that claim 2^40 bytes",
                zstd,
                |file| {
                    lying_zstd(file, 1 << 40);
                },
            ),
        ];
        for (what, id, damage) in bad_header {
            let (got, pieces) = visit_damaged(&store, &layout, id, damage);
            assert!(
                matches!(got, Err(StoreError::BadHeader { id: bad }) if bad == id),
                "{what}: {got:?}"
            );
            assert_eq!(pieces.len(), 0, "{what}: no piece is visited");
        }
    }
}

/// zstd's legacy formats (v0.1 to v0.7, from before zstd 1.0) are no object's encoding: this
/// store writes frames of the current format only, and builds zstd without the legacy decoders
/// (`default-features = false`; storage spec §2 rule 1), some 25,000 lines of C that a crafted
/// object file would otherwise reach before its payload is hashed. A zstd built with them
/// (its default `legacy` feature) reads both object files below back as valid: they hold the very
/// payload their id names.
mod legacy_zstd {
    use swarmops_core::bytes::to_u64;
    use swarmops_store::ObjectStat;
    use swarmops_testkit::store::{zstd_object, zstd_v07_frame};

    use super::*;

    /// What every object file here holds, whatever its frames: the payload of the object's id.
    const PAYLOAD: &[u8] = b"an object whose zstd body holds a frame of the legacy v0.7 format\n";

    /// The bytes of [`PAYLOAD`] that the two-frame body holds in a frame of the current format.
    const HEAD: usize = 24;

    /// The object file of [`PAYLOAD`] with one frame of the v0.7 format for its zstd body.
    fn one_legacy_frame(file: &mut Vec<u8>) {
        zstd_object(file, to_u64(PAYLOAD.len()), &zstd_v07_frame(PAYLOAD));
    }

    /// The object file of [`PAYLOAD`] with a frame of the current format, as zstd writes one, then
    /// a frame of the v0.7 format for its zstd body: zstd decodes a body frame by frame, so a
    /// check of the first frame's magic alone would not refuse it.
    fn a_legacy_frame_second(file: &mut Vec<u8>) {
        let (head, tail) = PAYLOAD.split_at(HEAD);
        let mut body = zstd::bulk::compress(head, 3).unwrap();
        body.extend(zstd_v07_frame(tail));
        zstd_object(file, to_u64(PAYLOAD.len()), &body);
    }

    /// Applies `damage` to the file of `id`, reads `id` in pieces, and restores the file.
    fn read_chunks_damaged(
        store: &FsObjectStore,
        layout: &Layout,
        id: ObjectId,
        damage: fn(&mut Vec<u8>),
    ) -> Result<Option<ObjectStat>, StoreError> {
        let path = layout.object_path(id);
        let pristine = fs::read(&path).unwrap();
        let mut damaged = pristine.clone();
        damage(&mut damaged);
        fs::write(&path, &damaged).unwrap();
        let got = store.read_chunks(id, &mut |_| {});
        fs::write(&path, &pristine).unwrap();
        got
    }

    /// A legacy frame anywhere in an object's body makes the object `Corrupt`, for `get` and for
    /// `read_chunks` alike, although the frame holds the payload the object's id names: zstd no
    /// longer finds the frame's size, so the read stops before decoding anything.
    #[test]
    fn an_object_in_a_legacy_zstd_frame_is_corrupt() {
        let (_root, layout) = new_root();
        let store = real_store(&layout);
        let id = store.put(ObjectKind::Blob, PAYLOAD).unwrap();
        let cases: [Damage; 2] = [
            ("one v0.7 frame", id, one_legacy_frame),
            (
                "a frame of the current format, then a v0.7 frame",
                id,
                a_legacy_frame_second,
            ),
        ];
        for (what, id, damage) in cases {
            let got = get_damaged(&store, &layout, id, damage);
            assert!(
                matches!(got, Err(StoreError::Corrupt { id: bad }) if bad == id),
                "get, {what}: {got:?}"
            );
            let got = read_chunks_damaged(&store, &layout, id, damage);
            assert!(
                matches!(got, Err(StoreError::Corrupt { id: bad }) if bad == id),
                "read_chunks, {what}: {got:?}"
            );
        }
        // Each read put the object's own file back.
        let stored = store.get(id).unwrap();
        assert_eq!(stored, Some((ObjectKind::Blob, PAYLOAD.to_vec())));
    }
}
