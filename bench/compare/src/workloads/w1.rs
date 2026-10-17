//! W1: one writer, durable transactions back to back, each inserting one changeset (a 32-byte
//! id, its generation, a 32-byte delta hash, one 32-byte parent) and moving `main` to it by
//! compare-and-swap. Commits/s and the latency of each commit; then, the store closed, its size
//! per transaction in the shape that one commit per transaction leaves.

use std::path::Path;
use std::time::{Duration, Instant};

use super::Values;
use crate::data::{chain, changeset};
use crate::engines::{Engine, Main, Result, other};
use crate::measure::{Latency, dir_size, ms};

pub fn run(engine: &dyn Engine, base: &Path, warmup: u64, txns: u64) -> Result<Values> {
    let dir = tempfile::Builder::new()
        .prefix(&format!("w1-{}-", engine.name()))
        .tempdir_in(base)?;
    let (elapsed, mut latencies) = {
        let db = engine.create(dir.path())?;
        let mut session = db.session()?;
        let mut main = None;
        let mut commit = |n: u64| -> Result<()> {
            let cs = changeset(chain::W1, n, None);
            let committed = session.commit(&cs, Main::Expect(main), "w1")?;
            if committed.old != main {
                return Err(other("the compare-and-swap reported another old main"));
            }
            main = Some(cs.id);
            Ok(())
        };
        for n in 0..warmup {
            commit(n)?;
        }
        let mut latencies: Vec<Duration> = Vec::with_capacity(usize::try_from(txns).unwrap_or(0));
        let started = Instant::now();
        for n in warmup..warmup + txns {
            let t = Instant::now();
            commit(n)?;
            latencies.push(t.elapsed());
        }
        (started.elapsed(), latencies)
    };
    let size = dir_size(dir.path());
    let lat = Latency::of(&mut latencies);
    let total = (warmup + txns) as f64;
    Ok(Values::from([
        ("commits_per_s".into(), txns as f64 / elapsed.as_secs_f64()),
        ("p50_ms".into(), lat.p50_us / 1e3),
        ("p90_ms".into(), lat.p90_us / 1e3),
        ("p99_ms".into(), lat.p99_us / 1e3),
        ("p999_ms".into(), lat.p999_us / 1e3),
        ("max_ms".into(), lat.max_us / 1e3),
        ("mean_ms".into(), lat.mean_us / 1e3),
        ("elapsed_ms".into(), ms(elapsed)),
        ("bytes_per_txn".into(), size.allocated as f64 / total),
        (
            "apparent_bytes_per_txn".into(),
            size.apparent as f64 / total,
        ),
    ]))
}
