//! M1 experiment: how `FsObjectStore::put_batch` scales with the number of fsyncs in flight.
//!
//! Every iteration writes 256 new 4 KiB objects into a store of its own: a fresh temporary
//! directory under `SWARMOPS_BENCH_DIR` (default `~/.local/state/swarmops-bench`, on the
//! native filesystem), with `objects/tmp/` and all 256 fan-out directories made beforehand,
//! as a created store has them. The payloads are pseudo-random, so zstd leaves them raw and
//! every object file holds a full 4 KiB, and no two objects of the whole run are the same, so
//! every put writes and syncs a file (an idempotent put of an existing object syncs nothing).
//! Only the store calls are timed; making the store and its payloads before, and removing it
//! after (then syncing the base directory, so that the next iteration's fsyncs do not carry
//! the removal), are not.
//!
//! - `put_flush/<threads>`: `put_batch` on a pool of 1, 8 or 32 threads
//!   ([`FsObjectStore::set_parallelism`]), then `flush`, whose directory fsyncs run on the same
//!   pool: what a commit's object writes cost.
//! - `put_only/<threads>`: `put_batch` alone: per object a temp file written and `sync_all`ed,
//!   then linked into place.
//! - `flush_only`: `flush` alone, after an untimed `put_batch`, on a pool of 32 threads: one
//!   directory fsync per fan-out directory the 256 objects touched (about 162), 32 at a time
//!   (one after another until Task 7c).
//! - `put_flush_busy/<threads>` and `flush_only_busy`: the same while another thread appends
//!   64 bytes to a file of its own every 100 µs, as some process always does on a swarm host.
//!   While any metadata is pending on the filesystem, ext4 turns every directory fsync into a
//!   journal commit of its own, and fsyncs made at once share commits; on an idle filesystem
//!   the first commit covers them all. The other variants see whatever the host does
//!   meanwhile.
//!
//! Throughput is reported in objects per second; docs/perf.md cites criterion's median.

mod common;

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use camino::Utf8PathBuf;
use criterion::{
    BenchmarkId, Criterion, SamplingMode, Throughput, criterion_group, criterion_main,
};
use swarmops_core::ObjectKind;
use swarmops_store::{FsObjectStore, Layout, ObjectStore, RealFs};
use tempfile::{NamedTempFile, TempDir};

use crate::common::{base_dir, mix};

/// Objects per iteration.
const OBJECTS: u64 = 256;

/// Bytes per object, in 8-byte words.
const WORDS: u64 = 512;

/// The pool sizes measured.
const THREADS: [usize; 3] = [1, 8, 32];

/// The next 256 payloads of the run: object `n` of the run holds `mix` of the words
/// `n × 512 ..`, so no two objects are alike.
fn new_payloads(next_object: &mut u64) -> Vec<(ObjectKind, Vec<u8>)> {
    let first = *next_object;
    *next_object += OBJECTS;
    (first..first + OBJECTS)
        .map(|n| {
            let payload = (n * WORDS..(n + 1) * WORDS)
                .flat_map(|word| mix(word).to_le_bytes())
                .collect();
            (ObjectKind::Blob, payload)
        })
        .collect()
}

/// A store in a fresh directory under `base`, its `put_batch` on `threads` threads, with
/// `objects/tmp/` and the 256 fan-out directories in place and synced.
fn fresh_store(base: &Path, threads: usize) -> (TempDir, FsObjectStore) {
    let dir = tempfile::tempdir_in(base).expect("create store dir");
    let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("UTF-8 store dir");
    let layout = Layout::new(root);
    fs::create_dir_all(layout.objects_tmp()).expect("create objects/tmp");
    for byte in 0..=u8::MAX {
        fs::create_dir(layout.objects_dir().join(format!("{byte:02x}"))).expect("fan-out dir");
    }
    File::open(layout.objects_dir())
        .and_then(|objects| objects.sync_all())
        .expect("sync objects/");
    let mut store = FsObjectStore::open(layout, Box::new(RealFs));
    store.set_parallelism(threads);
    (dir, store)
}

/// Another writer on the filesystem until dropped: a thread appending 64 bytes to a file of its
/// own under `base` every 100 µs, emptying it at 1 MiB, so that metadata is always pending.
struct BusyWriter {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    _file: NamedTempFile,
}

impl BusyWriter {
    fn start(base: &Path) -> BusyWriter {
        let file = NamedTempFile::new_in(base).expect("create the busy writer's file");
        let mut appender = OpenOptions::new()
            .append(true)
            .open(file.path())
            .expect("open the busy writer's file");
        let stop = Arc::new(AtomicBool::new(false));
        let thread = thread::spawn({
            let stop = Arc::clone(&stop);
            move || {
                let mut len = 0;
                while !stop.load(Ordering::Relaxed) {
                    appender.write_all(&[0x5a; 64]).expect("busy append");
                    len += 64;
                    if len >= 1 << 20 {
                        appender.set_len(0).expect("empty the busy writer's file");
                        len = 0;
                    }
                    thread::sleep(Duration::from_micros(100));
                }
            }
        });
        BusyWriter {
            stop,
            thread: Some(thread),
            _file: file,
        }
    }
}

impl Drop for BusyWriter {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            thread.join().expect("the busy writer panicked");
        }
    }
}

/// What an iteration times.
#[derive(Clone, Copy)]
enum Timed {
    PutFlush,
    PutOnly,
    FlushOnly,
}

/// How long `f` takes.
fn time(f: impl FnOnce()) -> Duration {
    let started = Instant::now();
    f();
    started.elapsed()
}

/// Runs `iters` iterations, each on a store of its own with a pool of `threads`, and returns
/// the time spent in what `timed` names.
fn run(base: &Path, threads: usize, timed: Timed, iters: u64, next_object: &mut u64) -> Duration {
    let mut total = Duration::ZERO;
    for _ in 0..iters {
        let (dir, store) = fresh_store(base, threads);
        let items = new_payloads(next_object);
        let put = |items| {
            let ids = store.put_batch(items).expect("put_batch");
            assert_eq!(u64::try_from(ids.len()), Ok(OBJECTS));
        };
        let flush = || store.flush().expect("flush");
        total += match timed {
            Timed::PutFlush => time(|| {
                put(items);
                flush();
            }),
            Timed::PutOnly => time(|| put(items)),
            Timed::FlushOnly => {
                put(items);
                time(flush)
            }
        };
        drop(store);
        drop(dir);
        File::open(base)
            .and_then(|base| base.sync_all())
            .expect("sync the bench base dir");
    }
    total
}

fn put_batch(c: &mut Criterion) {
    let base = base_dir();
    let mut next_object = 0;
    let mut group = c.benchmark_group("put_batch");
    group
        .sampling_mode(SamplingMode::Flat)
        .sample_size(10)
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(15))
        .throughput(Throughput::Elements(OBJECTS));
    for (name, timed) in [("put_flush", Timed::PutFlush), ("put_only", Timed::PutOnly)] {
        for threads in THREADS {
            group.bench_function(BenchmarkId::new(name, threads), |b| {
                b.iter_custom(|iters| run(&base, threads, timed, iters, &mut next_object));
            });
        }
    }
    group.bench_function("flush_only", |b| {
        b.iter_custom(|iters| run(&base, 32, Timed::FlushOnly, iters, &mut next_object));
    });
    for threads in THREADS {
        let busy = BusyWriter::start(&base);
        group.bench_function(BenchmarkId::new("put_flush_busy", threads), |b| {
            b.iter_custom(|iters| run(&base, threads, Timed::PutFlush, iters, &mut next_object));
        });
        drop(busy);
    }
    let busy = BusyWriter::start(&base);
    group.bench_function("flush_only_busy", |b| {
        b.iter_custom(|iters| run(&base, 32, Timed::FlushOnly, iters, &mut next_object));
    });
    drop(busy);
    group.finish();
}

criterion_group!(benches, put_batch);
criterion_main!(benches);
