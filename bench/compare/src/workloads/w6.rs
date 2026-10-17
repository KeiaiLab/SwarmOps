//! W6: a reader's latency while a writer commits continuously, on a copy of the smallest
//! seeded store (100,000 changesets). Reader and writer are two threads of one process, each
//! with a session of its own (SQLite: two connections). The reader looks up a changeset by id
//! in a read transaction of its own each time, as W3's `fresh`; first with the writer idle,
//! then for a fixed time while the writer runs W1's durable transaction back to back.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use super::{Seed, Values};
use crate::data::{chain, changeset, id, mix};
use crate::engines::{Engine, Main, Result, other};
use crate::measure::{Latency, copy_dir};

pub fn run(
    engine: &dyn Engine,
    seed: &Seed,
    base: &Path,
    run: u64,
    idle_lookups: usize,
    busy: Duration,
) -> Result<Values> {
    let dir = tempfile::Builder::new()
        .prefix(&format!("w6-{}-", engine.name()))
        .tempdir_in(base)?;
    let store = dir.path().join("store");
    copy_dir(&seed.dir, &store);
    let db = engine.open(&store)?;
    let n = seed.n;
    let ids: Vec<_> = (0..idle_lookups as u64)
        .map(|i| id(chain::SEED, mix(i ^ 0x5eed) % n))
        .collect();
    let mut reader = db.session()?;
    for id in &ids {
        reader.get(id)?;
    }
    let mut idle = Vec::with_capacity(ids.len());
    for id in &ids {
        let t = Instant::now();
        let found = reader.get(id)?;
        idle.push(t.elapsed());
        if found.is_none() {
            return Err(other("w6: a seeded changeset is missing"));
        }
    }
    let stop = AtomicBool::new(false);
    let (busy_lat, writer) = thread::scope(|scope| {
        let writer = scope.spawn(|| -> Result<(u64, Vec<Duration>, Duration)> {
            let mut session = db.session()?;
            let root = id(chain::SEED, n - 1);
            let mut main = Some(root);
            let mut latencies = Vec::new();
            let started = Instant::now();
            let mut k = 0;
            while !stop.load(Ordering::Relaxed) {
                let cs = changeset(chain::W6_WRITER + run, k, Some(root));
                let t = Instant::now();
                session.commit(&cs, Main::Expect(main), "w6")?;
                latencies.push(t.elapsed());
                main = Some(cs.id);
                k += 1;
            }
            Ok((k, latencies, started.elapsed()))
        });
        let mut latencies = Vec::new();
        let started = Instant::now();
        let mut i = 0;
        let mut failed = None;
        while started.elapsed() < busy {
            let t = Instant::now();
            match reader.get(&ids[i % ids.len()]) {
                Ok(Some(_)) => latencies.push(t.elapsed()),
                Ok(None) => {
                    failed = Some(other("w6: a seeded changeset is missing under the writer"));
                    break;
                }
                Err(e) => {
                    failed = Some(e);
                    break;
                }
            }
            i += 1;
        }
        stop.store(true, Ordering::Relaxed);
        let writer = writer.join().map_err(|_| other("the w6 writer panicked"));
        match failed {
            Some(e) => (Err(e), writer),
            None => (Ok(latencies), writer),
        }
    });
    let mut busy_lat = busy_lat?;
    let (commits, mut writer_lat, writer_time) = writer??;
    let idle = Latency::of(&mut idle);
    let busy = Latency::of(&mut busy_lat);
    let commits_lat = Latency::of(&mut writer_lat);
    Ok(Values::from([
        ("idle_p50_us".into(), idle.p50_us),
        ("idle_p99_us".into(), idle.p99_us),
        ("idle_max_us".into(), idle.max_us),
        ("busy_p50_us".into(), busy.p50_us),
        ("busy_p99_us".into(), busy.p99_us),
        ("busy_p999_us".into(), busy.p999_us),
        ("busy_max_us".into(), busy.max_us),
        ("busy_lookups".into(), busy.n as f64),
        (
            "writer_commits_per_s".into(),
            commits as f64 / writer_time.as_secs_f64(),
        ),
        ("writer_p50_ms".into(), commits_lat.p50_us / 1e3),
        ("writer_p99_ms".into(), commits_lat.p99_us / 1e3),
    ]))
}
