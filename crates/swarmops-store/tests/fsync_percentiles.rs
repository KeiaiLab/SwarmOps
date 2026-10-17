//! True per-write fsync percentiles on this host (ignored: ~20 s of fsyncs).
//!
//! Run: `cargo nextest run -p swarmops-store --run-ignored all -E 'test(fsync_percentiles)' --no-capture`
//! Each variant does `N` independent 4 KiB writes in a fresh temporary directory
//! under `SWARMOPS_BENCH_DIR` (default `~/.local/state/swarmops-bench`) and
//! records every iteration's own duration; the table is what `docs/perf.md` cites.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::time::Instant;
use swarmops_testkit::measure::{base_dir, percentile};

const N: usize = 1000;
const PAYLOAD: [u8; 4096] = [0xA5; 4096];

fn write_payload(path: &Path) -> File {
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .expect("create file");
    f.write_all(&PAYLOAD).expect("write payload");
    f
}

fn measure(name: &str, base: &Path, mut step: impl FnMut(&Path, &File)) {
    let dir = tempfile::tempdir_in(base).expect("variant dir");
    let dir_handle = File::open(dir.path()).expect("open dir");
    let mut samples = Vec::with_capacity(N);
    for i in 0..N {
        let path = dir.path().join(format!("o{i}.tmp"));
        let t = Instant::now();
        step(&path, &dir_handle);
        samples.push(t.elapsed());
    }
    samples.sort();
    println!(
        "{name:28} n={N} p50={:>8} us p90={:>8} us p99={:>8} us max={:>8} us",
        percentile(&samples, 50).as_micros(),
        percentile(&samples, 90).as_micros(),
        percentile(&samples, 99).as_micros(),
        samples[samples.len() - 1].as_micros()
    );
}

#[test]
#[ignore = "measurement, ~20 s of fsyncs; prints the table docs/perf.md cites"]
fn fsync_percentiles() {
    let base = base_dir();
    measure("no_fsync", &base, |p, _| drop(write_payload(p)));
    measure("sync_data", &base, |p, _| {
        write_payload(p).sync_data().expect("sync_data");
    });
    measure("sync_all", &base, |p, _| {
        write_payload(p).sync_all().expect("sync_all");
    });
    measure("sync_all_rename_dir_fsync", &base, |p, dir| {
        write_payload(p).sync_all().expect("sync_all");
        fs::rename(p, p.with_extension("obj")).expect("rename");
        dir.sync_all().expect("dir fsync");
    });
}
