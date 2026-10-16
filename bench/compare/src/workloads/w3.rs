//! W3: lookups on a seeded store of 100,000 and of 1,000,000 changesets, warm (one untimed
//! pass of every kind first; the files in the page cache), each timed on its own:
//!
//! - on one open read transaction (snapshot): by id (`get`), by id prefix (the first 6 bytes,
//!   12 hex digits: `prefix`), by delta hash through the secondary index (`delta`), and by an
//!   id the store does not hold (`miss`: what every insert asks first);
//! - by id, each in a read transaction of its own (`fresh`): what a CLI command pays, which
//!   opens one or two snapshots.
//!
//! Every answer is checked: the right generation, the one id, the one changeset.

use std::path::Path;
use std::time::{Duration, Instant};

use super::{Seed, Values};
use crate::data::{chain, delta_hash, id, mix};
use crate::engines::{Engine, Result, other};
use crate::measure::{Latency, Size, dir_size};

pub fn run(engine: &dyn Engine, seed: &Seed, lookups: usize) -> Result<Values> {
    let db = engine.open(&seed.dir)?;
    let mut session = db.session()?;
    let targets: Vec<u64> = (0..lookups as u64).map(|i| mix(i) % seed.n).collect();
    let ids: Vec<_> = targets.iter().map(|&t| id(chain::SEED, t)).collect();
    let dhs: Vec<_> = targets
        .iter()
        .map(|&t| delta_hash(chain::SEED, t))
        .collect();
    let misses: Vec<_> = (0..lookups as u64).map(|i| id(u64::MAX, i)).collect();
    let prefix = |id: &[u8; 32]| -> [u8; 6] { id[..6].try_into().expect("6 bytes") };

    // Warm-up: every kind once, untimed.
    {
        let snapshot = session.snapshot()?;
        for (i, id) in ids.iter().enumerate() {
            snapshot.get(id)?;
            snapshot.prefix(prefix(id))?;
            snapshot.by_delta(&dhs[i])?;
            snapshot.get(&misses[i])?;
        }
    }
    for id in &ids {
        session.get(id)?;
    }

    let (mut get, mut pre, mut delta, mut miss) = (vec![], vec![], vec![], vec![]);
    {
        let snapshot = session.snapshot()?;
        for (i, id) in ids.iter().enumerate() {
            let t = Instant::now();
            let found = snapshot.get(id)?;
            get.push(t.elapsed());
            if found != Some(targets[i] + 1) {
                return Err(other(format!(
                    "get: generation {found:?}, not {}",
                    targets[i] + 1
                )));
            }
        }
        for id in &ids {
            let t = Instant::now();
            let found = snapshot.prefix(prefix(id))?;
            pre.push(t.elapsed());
            if !found.contains(id) {
                return Err(other("prefix: the id is missing"));
            }
        }
        for (i, id) in ids.iter().enumerate() {
            let t = Instant::now();
            let found = snapshot.by_delta(&dhs[i])?;
            delta.push(t.elapsed());
            if found != [*id] {
                return Err(other(format!("delta: {} ids, not the one", found.len())));
            }
        }
        for id in &misses {
            let t = Instant::now();
            let found = snapshot.get(id)?;
            miss.push(t.elapsed());
            if found.is_some() {
                return Err(other("miss: found an id the store does not hold"));
            }
        }
    }
    let mut fresh = Vec::with_capacity(lookups);
    for (i, id) in ids.iter().enumerate() {
        let t = Instant::now();
        let found = session.get(id)?;
        fresh.push(t.elapsed());
        if found != Some(targets[i] + 1) {
            return Err(other("fresh get: wrong generation"));
        }
    }
    let mut values = Values::new();
    for (name, samples) in [
        ("get", &mut get),
        ("prefix", &mut pre),
        ("delta", &mut delta),
        ("miss", &mut miss),
        ("fresh", &mut fresh),
    ] {
        put(&mut values, name, samples);
    }
    Ok(values)
}

fn put(values: &mut Values, name: &str, samples: &mut [Duration]) {
    let lat = Latency::of(samples);
    values.insert(format!("{name}_p50_us"), lat.p50_us);
    values.insert(format!("{name}_p99_us"), lat.p99_us);
    values.insert(format!("{name}_max_us"), lat.max_us);
    values.insert(format!("{name}_mean_us"), lat.mean_us);
}

/// W5: a seeded store's size on disk, closed, and its largest files.
pub fn size(dir: &Path) -> (Size, Vec<(String, u64)>) {
    let size = dir_size(dir);
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        if let Ok(entries) = std::fs::read_dir(&d) {
            for entry in entries.flatten() {
                let path = entry.path();
                match entry.metadata() {
                    Ok(meta) if meta.is_dir() => stack.push(path),
                    Ok(meta) => {
                        use std::os::unix::fs::MetadataExt;
                        let rel = path.strip_prefix(dir).unwrap_or(&path);
                        files.push((rel.display().to_string(), meta.blocks() * 512));
                    }
                    Err(_) => {}
                }
            }
        }
    }
    files.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    (size, files)
}
