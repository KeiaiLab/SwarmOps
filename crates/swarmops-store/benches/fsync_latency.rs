//! M0 durability experiment: what one durable object write costs on this host.
//!
//! Each iteration writes a fresh 4 KiB file under `SWARMOPS_BENCH_DIR`
//! (default `~/.local/state/swarmops-bench`) and then does the variant's
//! durability step. The directory is removed at the end.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};

const PAYLOAD: [u8; 4096] = [0xA5; 4096];

fn bench_dir() -> PathBuf {
    let dir = std::env::var_os("SWARMOPS_BENCH_DIR").map_or_else(
        || {
            let home = std::env::var_os("HOME").expect("HOME is set");
            PathBuf::from(home).join(".local/state/swarmops-bench")
        },
        PathBuf::from,
    );
    fs::create_dir_all(&dir).expect("create bench dir");
    dir
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
    let dir = bench_dir();
    let mut group = c.benchmark_group("fsync");
    group
        .sample_size(60)
        .measurement_time(Duration::from_secs(6))
        .warm_up_time(Duration::from_secs(1));

    let mut counter = Counter {
        dir: dir.clone(),
        n: 0,
    };
    group.bench_function("no_fsync", |b| {
        b.iter(|| {
            let path = counter.next_path();
            drop(write_payload(&path));
        });
    });

    let mut counter = Counter {
        dir: dir.clone(),
        n: 1_000_000,
    };
    group.bench_function("sync_data", |b| {
        b.iter(|| {
            let path = counter.next_path();
            let f = write_payload(&path);
            f.sync_data().expect("sync_data");
        });
    });

    let mut counter = Counter {
        dir: dir.clone(),
        n: 2_000_000,
    };
    group.bench_function("sync_all", |b| {
        b.iter(|| {
            let path = counter.next_path();
            let f = write_payload(&path);
            f.sync_all().expect("sync_all");
        });
    });

    let mut counter = Counter {
        dir: dir.clone(),
        n: 3_000_000,
    };
    let dir_handle = File::open(&dir).expect("open dir");
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
    group.finish();
    drop(dir_handle);
    fs::remove_dir_all(&dir).expect("remove bench dir");
}

criterion_group!(benches, fsync_latency);
criterion_main!(benches);
