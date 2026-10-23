//! M1 experiment: does `Index::sync` gain from syncing its five files in parallel?
//!
//! `Index::sync` (every 32 transactions, and before a compaction) makes the five index files
//! durable with one `sync_data` each: one after another at parallelism 1, from five threads at
//! once at the store's default ([`Index::set_parallelism`]). Here five files in a fresh
//! temporary directory under `SWARMOPS_BENCH_DIR` (default `~/.local/state/swarmops-bench`, on
//! the native filesystem) each get 64 KiB of new data appended before every iteration
//! (untimed), as the index's appends grow its files, and the iteration makes them durable:
//!
//! - `serial`: `sync_data` of each file in turn, each opened for it, as `Index::sync` does at
//!   parallelism 1;
//! - `parallel`: the same from five threads at once (`std::thread::scope`; starting the
//!   threads is timed too);
//! - `one_file`: the `sync_data` of one such file alone, for scale;
//! - `index_sync/<threads>`: `Index::sync` itself at parallelism 1 and 5, on an index of its
//!   own that gets the 32 records of a checkpoint's frames appended before every iteration
//!   (untimed).
//!
//! Each variant runs in a directory of its own; docs/perf.md cites criterion's median.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use camino::Utf8PathBuf;
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use swarmops_core::{ChangesetId, ObjectKind, hash_object};
use swarmops_store::{ChangesetMeta, Index, Layout, RealFs};
use swarmops_testkit::measure::{base_dir, mix};
use tempfile::TempDir;

/// New bytes appended to every file before each iteration, in 8-byte words (64 KiB).
const WORDS: u64 = 8 * 1024;

/// The records appended to the index before each `index_sync` iteration: one checkpoint's.
const RECORDS: u64 = 32;

/// Files in a fresh directory, each kept open for appending.
struct Files {
    _dir: TempDir,
    paths: Vec<PathBuf>,
    appenders: Vec<File>,
    /// How many 64 KiB chunks have been appended to each file so far.
    chunks: u64,
}

impl Files {
    fn new(base: &Path, count: usize) -> Files {
        let dir = tempfile::tempdir_in(base).expect("create variant dir");
        let paths: Vec<PathBuf> = (0..count)
            .map(|i| dir.path().join(format!("col{i}")))
            .collect();
        let appenders = paths
            .iter()
            .map(|path| {
                OpenOptions::new()
                    .append(true)
                    .create_new(true)
                    .open(path)
                    .expect("create file")
            })
            .collect();
        File::open(dir.path())
            .and_then(|dir| dir.sync_all())
            .expect("sync the variant dir");
        Files {
            _dir: dir,
            paths,
            appenders,
            chunks: 0,
        }
    }

    /// Appends 64 KiB that no earlier call wrote to every file, leaving them dirty.
    fn dirty(&mut self) {
        for (file, appender) in (0..).zip(&mut self.appenders) {
            let first = (self.chunks * 8 + file) * WORDS;
            let chunk: Vec<u8> = (first..first + WORDS)
                .flat_map(|word| mix(word).to_le_bytes())
                .collect();
            appender.write_all(&chunk).expect("append");
        }
        self.chunks += 1;
    }
}

/// What `RealFs::sync_data` does, as `Index::sync` calls it per file.
fn sync_data(path: &Path) {
    File::open(path)
        .and_then(|file| file.sync_data())
        .expect("sync_data");
}

/// Runs `iters` iterations on `files`, each dirtying them (untimed) and then making them
/// durable with `sync`, and returns the time spent in `sync`.
fn run(files: &mut Files, iters: u64, sync: impl Fn(&[PathBuf])) -> Duration {
    let mut total = Duration::ZERO;
    for _ in 0..iters {
        files.dirty();
        let started = Instant::now();
        sync(&files.paths);
        total += started.elapsed();
    }
    total
}

fn fsync_fanout(c: &mut Criterion) {
    let base = base_dir();
    let mut group = c.benchmark_group("fsync_fanout");
    group
        .sample_size(30)
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(10));

    let mut files = Files::new(&base, 5);
    group.bench_function("serial", |b| {
        b.iter_custom(|iters| {
            run(&mut files, iters, |paths| {
                for path in paths {
                    sync_data(path);
                }
            })
        });
    });
    drop(files);

    let mut files = Files::new(&base, 5);
    group.bench_function("parallel", |b| {
        b.iter_custom(|iters| {
            run(&mut files, iters, |paths| {
                thread::scope(|scope| {
                    for path in paths {
                        scope.spawn(|| sync_data(path));
                    }
                });
            })
        });
    });
    drop(files);

    let mut files = Files::new(&base, 1);
    group.bench_function("one_file", |b| {
        b.iter_custom(|iters| run(&mut files, iters, |paths| sync_data(&paths[0])));
    });
    drop(files);

    for threads in [1, 5] {
        let dir = tempfile::tempdir_in(&base).expect("create variant dir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("UTF-8 dir");
        let mut index =
            Index::create(&Layout::new(root), Arc::new(RealFs::new())).expect("create index");
        index.set_parallelism(threads);
        let mut next = 0;
        group.bench_function(BenchmarkId::new("index_sync", threads), |b| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    for n in next..next + RECORDS {
                        index.append(&changeset(n)).expect("append");
                    }
                    next += RECORDS;
                    let started = Instant::now();
                    index.sync().expect("sync");
                    total += started.elapsed();
                }
                total
            });
        });
        drop(index);
        drop(dir);
    }
    group.finish();
}

/// Changeset `n` of a chain: the child of changeset `n - 1`.
fn changeset(n: u64) -> ChangesetMeta {
    let id = |n: u64| ChangesetId(hash_object(ObjectKind::Changeset, &n.to_le_bytes()));
    ChangesetMeta::new(
        id(n),
        0,
        n + 1,
        hash_object(ObjectKind::Blob, &n.to_le_bytes()).0,
        n.checked_sub(1).map(id).into_iter().collect(),
    )
}

criterion_group!(benches, fsync_fanout);
criterion_main!(benches);
