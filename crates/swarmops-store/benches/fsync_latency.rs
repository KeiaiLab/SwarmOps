//! M0 durability experiment: what one durable object write costs on this host.
//!
//! Each variant runs in its own fresh temporary directory under
//! `SWARMOPS_BENCH_DIR` (default `~/.local/state/swarmops-bench`), so the base
//! directory is never deleted and a later variant never fsyncs a directory
//! bloated by an earlier one. Criterion reports per-sample means; for true
//! per-write percentiles see `tests/fsync_percentiles.rs`.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use swarmops_testkit::measure::base_dir;
use tempfile::TempDir;

const PAYLOAD: [u8; 4096] = [0xA5; 4096];

fn fresh_dir(base: &Path) -> TempDir {
    tempfile::tempdir_in(base).expect("create variant dir")
}

struct Counter {
    dir: PathBuf,
    n: u64,
}

impl Counter {
    fn next_path(&mut self) -> PathBuf {
        self.n += 1;
        self.dir.join(format!("obj-{}.tmp", self.n))
    }
}

fn write_payload(path: &Path) -> File {
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .expect("create file");
    f.write_all(&PAYLOAD).expect("write payload");
    f
}

fn fsync_latency(c: &mut Criterion) {
    let base = base_dir();
    let mut group = c.benchmark_group("fsync");
    group
        .sample_size(60)
        .measurement_time(Duration::from_secs(6))
        .warm_up_time(Duration::from_secs(1));

    let dir = fresh_dir(&base);
    let mut counter = Counter {
        dir: dir.path().to_path_buf(),
        n: 0,
    };
    group.bench_function("no_fsync", |b| {
        b.iter(|| {
            let path = counter.next_path();
            drop(write_payload(&path));
        });
    });
    drop(dir);

    let dir = fresh_dir(&base);
    let mut counter = Counter {
        dir: dir.path().to_path_buf(),
        n: 0,
    };
    group.bench_function("sync_data", |b| {
        b.iter(|| {
            let path = counter.next_path();
            let f = write_payload(&path);
            f.sync_data().expect("sync_data");
        });
    });
    drop(dir);

    let dir = fresh_dir(&base);
    let mut counter = Counter {
        dir: dir.path().to_path_buf(),
        n: 0,
    };
    group.bench_function("sync_all", |b| {
        b.iter(|| {
            let path = counter.next_path();
            let f = write_payload(&path);
            f.sync_all().expect("sync_all");
        });
    });
    drop(dir);

    let dir = fresh_dir(&base);
    let mut counter = Counter {
        dir: dir.path().to_path_buf(),
        n: 0,
    };
    let dir_handle = File::open(dir.path()).expect("open dir");
    group.bench_function("sync_all_rename_dir_fsync", |b| {
        b.iter(|| {
            let tmp = counter.next_path();
            let f = write_payload(&tmp);
            f.sync_all().expect("sync_all");
            let fin = tmp.with_extension("obj");
            fs::rename(&tmp, &fin).expect("rename");
            dir_handle.sync_all().expect("dir fsync");
        });
    });
    drop(dir_handle);
    drop(dir);
    group.finish();
}

criterion_group!(benches, fsync_latency);
criterion_main!(benches);
