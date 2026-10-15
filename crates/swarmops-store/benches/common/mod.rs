//! What the store's benches and measurement tests share. A module directory, not
//! `benches/common.rs`, so that Cargo does not take it for a bench target of its own; the
//! measurement tests include it with `#[path]`. Each includer uses what it needs.

#![allow(dead_code)]

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

/// The directory the measurements make their temporary directories in, created if missing:
/// `SWARMOPS_BENCH_DIR`, or `~/.local/state/swarmops-bench` (on the native filesystem, unlike
/// a `/tmp` that may be a tmpfs).
pub fn base_dir() -> PathBuf {
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
pub fn mix(n: u64) -> u64 {
    let mut z = n.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// Nearest-rank percentile over sorted samples.
pub fn percentile(sorted: &[Duration], percent: usize) -> Duration {
    let rank = (sorted.len() * percent).div_ceil(100).max(1);
    sorted[rank - 1]
}
