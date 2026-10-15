//! M1 experiment: would `Index::sync` gain from syncing its five files in parallel?
//!
//! `Index::sync` (every 32 transactions, and before a compaction) makes the five index files
//! durable with one `sync_data` each, one after another. Here five files in a fresh temporary
//! directory under `SWARMOPS_BENCH_DIR` (default `~/.local/state/swarmops-bench`, on the
//! native filesystem) each get 64 KiB of new data appended before every iteration (untimed),
//! as the index's appends grow its files, and the iteration makes them durable:
//!
//! - `serial`: `sync_data` of each file in turn, each opened for it, as `Index::sync` does;
//! - `parallel`: the same from five threads at once (`std::thread::scope`; starting the
//!   threads is timed too);
//! - `one_file`: the `sync_data` of one such file alone, for scale.
//!
//! Each variant runs in a directory of its own; docs/perf.md cites criterion's median.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};
use tempfile::TempDir;

/// New bytes appended to every file before each iteration, in 8-byte words (64 KiB).
const WORDS: u64 = 8 * 1024;

fn base_dir() -> PathBuf {
    let dir = std::env::var_os("SWARMOPS_BENCH_DIR").map_or_else(
        || {
            let home = std::env::var_os("HOME").expect("HOME is set");
            PathBuf::from(home).join(".local/state/swarmops-bench")
        },
        PathBuf::from,
    );
    fs::create_dir_all(&dir).expect("create bench base dir");
    dir
}

/// SplitMix64's output function: a bijection on `u64` that scatters its input.
fn mix(n: u64) -> u64 {
    let mut z = n.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

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
    group.finish();
}

criterion_group!(benches, fsync_fanout);
criterion_main!(benches);
