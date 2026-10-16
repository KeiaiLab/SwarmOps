//! What every measurement records beside its numbers: the 1-minute load average at its start,
//! its range during, and at its end (sampled every 2 s), and a raw `fdatasync` probe at its
//! start and end; plus the statistics and file-size helpers the workloads share.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;

/// The directory the stores are made in, created if missing: `SWARMOPS_BENCH_DIR`, or
/// `~/.local/state/swarmops-bench` (the native filesystem, unlike a `/tmp` that may be a
/// tmpfs), as the store's own benches use. A path under `/mnt` (a Windows drive on WSL) is
/// refused, as the store refuses it.
pub fn base_dir() -> PathBuf {
    let dir = std::env::var_os("SWARMOPS_BENCH_DIR").map_or_else(
        || {
            let home = std::env::var_os("HOME").expect("HOME is set");
            PathBuf::from(home).join(".local/state/swarmops-bench")
        },
        PathBuf::from,
    );
    fs::create_dir_all(&dir).expect("create the bench base dir");
    let utf8 = camino::Utf8Path::from_path(&dir).expect("a UTF-8 bench dir");
    assert!(
        swarmops_store::is_native_linux_fs(utf8),
        "{} is not on a native Linux filesystem",
        dir.display()
    );
    dir
}

/// The 1-minute load average now.
pub fn load1() -> f64 {
    fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|s| s.split_whitespace().next()?.parse().ok())
        .unwrap_or(f64::NAN)
}

/// The raw `fdatasync` probe: the median of `count` appends of 120 bytes to a fresh file in
/// `dir`, each followed by `sync_data` (as the M1 store-level runs took it).
pub fn probe(dir: &Path, count: usize) -> Duration {
    let path = dir.join(format!("probe-{}", std::process::id()));
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .expect("open the probe file");
    let mut times: Vec<Duration> = (0..count)
        .map(|_| {
            let started = Instant::now();
            file.write_all(&[0x70; 120]).expect("probe write");
            file.sync_data().expect("probe sync");
            started.elapsed()
        })
        .collect();
    drop(file);
    let _ = fs::remove_file(&path);
    times.sort_unstable();
    times[times.len() / 2]
}

/// The host as one measurement saw it.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Env {
    pub load_start: f64,
    pub load_min: f64,
    pub load_max: f64,
    pub load_end: f64,
    pub probe_start_ms: f64,
    pub probe_end_ms: f64,
}

impl Env {
    /// `12.1 (11.8–14.0)`: the load at the start, and its range during.
    pub fn load(&self) -> String {
        format!(
            "{:.1} ({:.1}–{:.1})",
            self.load_start, self.load_min, self.load_max
        )
    }

    /// `4.1 / 4.3 ms`: the probe at the start and at the end.
    pub fn probe(&self) -> String {
        format!("{:.1} / {:.1} ms", self.probe_start_ms, self.probe_end_ms)
    }
}

/// Runs `f`, recording the host around it: a probe of `probes` syncs in `dir` before and
/// after, the load sampled every 2 s from its start to its end.
pub fn recorded<T>(dir: &Path, probes: usize, f: impl FnOnce() -> T) -> (T, Env) {
    let probe_start = probe(dir, probes);
    let load_start = load1();
    let (stop, stopped) = mpsc::channel::<()>();
    let sampler = thread::spawn(move || {
        let (mut min, mut max) = (f64::INFINITY, f64::NEG_INFINITY);
        loop {
            let load = load1();
            min = min.min(load);
            max = max.max(load);
            match stopped.recv_timeout(Duration::from_secs(2)) {
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                _ => return (min, max),
            }
        }
    });
    let out = f();
    let load_end = load1();
    let _ = stop.send(());
    let (min, max) = sampler.join().expect("the load sampler");
    let probe_end = probe(dir, probes);
    let env = Env {
        load_start,
        load_min: min.min(load_start).min(load_end),
        load_max: max.max(load_start).max(load_end),
        load_end,
        probe_start_ms: ms(probe_start),
        probe_end_ms: ms(probe_end),
    };
    (out, env)
}

pub fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

pub fn us(d: Duration) -> f64 {
    d.as_secs_f64() * 1e6
}

/// Nearest-rank percentile, in per mille, of sorted samples.
pub fn percentile(sorted: &[Duration], per_mille: usize) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let rank = (sorted.len() * per_mille).div_ceil(1000).max(1);
    sorted[rank - 1]
}

/// The latency summary of one run's samples (sorted here).
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Latency {
    pub n: usize,
    pub mean_us: f64,
    pub p50_us: f64,
    pub p90_us: f64,
    pub p99_us: f64,
    pub p999_us: f64,
    pub max_us: f64,
}

impl Latency {
    pub fn of(samples: &mut [Duration]) -> Latency {
        samples.sort_unstable();
        let total: Duration = samples.iter().sum();
        let n = samples.len();
        Latency {
            n,
            mean_us: if n == 0 { 0.0 } else { us(total) / n as f64 },
            p50_us: us(percentile(samples, 500)),
            p90_us: us(percentile(samples, 900)),
            p99_us: us(percentile(samples, 990)),
            p999_us: us(percentile(samples, 999)),
            max_us: us(samples.last().copied().unwrap_or_default()),
        }
    }
}

/// A directory's size: the sum of its files' lengths (apparent) and of the blocks they
/// occupy (allocated, `du`'s measure), every file below it counted.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct Size {
    pub apparent: u64,
    pub allocated: u64,
    pub files: u64,
}

pub fn dir_size(dir: &Path) -> Size {
    let mut size = Size::default();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in fs::read_dir(&d).expect("read a store dir") {
            let entry = entry.expect("a dir entry");
            let meta = entry.metadata().expect("its metadata");
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                size.apparent += meta.len();
                size.allocated += meta.blocks() * 512;
                size.files += 1;
            }
        }
    }
    size
}

/// Copies directory `from` to `to` (created), recursively.
pub fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("create the copy");
    for entry in fs::read_dir(from).expect("read the original") {
        let entry = entry.expect("a dir entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("its type").is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).expect("copy a file");
        }
    }
    File::open(to)
        .and_then(|d| d.sync_all())
        .expect("sync the copy");
}

/// The median and range of `values` (sorted here); NaN for none.
pub fn median_range(values: &mut [f64]) -> (f64, f64, f64) {
    values.sort_unstable_by(f64::total_cmp);
    match values.len() {
        0 => (f64::NAN, f64::NAN, f64::NAN),
        n => {
            let median = if n.is_multiple_of(2) {
                f64::midpoint(values[n / 2 - 1], values[n / 2])
            } else {
                values[n / 2]
            };
            (median, values[0], values[n - 1])
        }
    }
}
