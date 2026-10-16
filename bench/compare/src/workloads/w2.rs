//! W2: 32 writers × 1,000 transactions on one store, W1's transaction each, `main` compared
//! against what it names inside the transaction (every writer moves it). Processes for the
//! engines that several processes may open at once (native, SQLite), threads in one process
//! otherwise (redb, fjall, sled); with `w2_threads_too`, the process engines on threads as well,
//! so the asymmetry can be measured. Throughput, latency (a `Busy` retry included), `Busy` and
//! other failures, sled's conflict reruns, and fairness: the spread of the writers' total times.

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Barrier;
use std::thread;
use std::time::{Duration, Instant};

use super::{Values, WriterResult, writer_loop};
use crate::data::{chain, changeset, hex, id};
use crate::engines::{Db, Engine, Result, other};
use crate::measure::Latency;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Processes,
    Threads,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Mode::Processes => "processes",
            Mode::Threads => "threads",
        }
    }
}

pub fn run(
    engine: &dyn Engine,
    base: &Path,
    writers: u64,
    txns: u64,
    mode: Mode,
    exe: &Path,
) -> Result<Values> {
    let dir = tempfile::Builder::new()
        .prefix(&format!("w2-{}-{}-", engine.name(), mode.name()))
        .tempdir_in(base)?;
    let root = changeset(chain::W2_ROOT, 0, None);
    let db = engine.create(dir.path())?;
    db.session()?.load(std::slice::from_ref(&root))?;
    let (results, failed, retries, db) = match mode {
        Mode::Processes => {
            drop(db);
            let (results, failed) = processes(engine, dir.path(), writers, txns, &root.id, exe)?;
            (results, failed, 0, engine.open(dir.path())?)
        }
        Mode::Threads => {
            let (results, failed) = threads(&*db, writers, txns, &root.id);
            let retries = db.session()?.retries();
            (results, failed, retries, db)
        }
    };
    // Every writer's last changeset is there.
    let mut session = db.session()?;
    for r in &results {
        let last = id(chain::W2_WRITER + r.writer, txns - 1);
        if session.get(&last)?.is_none() {
            return Err(other(format!(
                "writer {}'s last changeset is missing",
                r.writer
            )));
        }
    }
    drop(session);
    drop(db);
    Ok(summarize(&results, failed, retries, writers * txns))
}

fn processes(
    engine: &dyn Engine,
    dir: &Path,
    writers: u64,
    txns: u64,
    root: &[u8; 32],
    exe: &Path,
) -> Result<(Vec<WriterResult>, u64)> {
    let sync = tempfile::Builder::new()
        .prefix("w2-barrier-")
        .tempdir_in(dir.parent().unwrap_or(dir))?;
    let children = (0..writers)
        .map(|w| {
            Command::new(exe)
                .arg("__w2")
                .arg(engine.name())
                .arg(dir)
                .arg(w.to_string())
                .arg(txns.to_string())
                .arg(hex(root))
                .arg(sync.path())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
        })
        .collect::<std::io::Result<Vec<_>>>()?;
    // The barrier: every child has opened the store before any starts.
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        let ready = fs::read_dir(sync.path())?
            .filter(|e| {
                e.as_ref()
                    .is_ok_and(|e| e.file_name().to_string_lossy().starts_with("ready-"))
            })
            .count();
        if ready as u64 == writers {
            break;
        }
        if Instant::now() > deadline {
            return Err(other(format!(
                "only {ready} of {writers} writers got ready"
            )));
        }
        thread::sleep(Duration::from_millis(1));
    }
    fs::File::create(sync.path().join("go"))?;
    let mut results = Vec::new();
    let mut failed = 0;
    for child in children {
        let out = child.wait_with_output()?;
        let line = String::from_utf8_lossy(&out.stdout);
        match (out.status.success(), line.lines().last()) {
            (true, Some(line)) => results.push(
                serde_json::from_str(line).map_err(|e| other(format!("a writer's result: {e}")))?,
            ),
            _ => failed += 1,
        }
    }
    Ok((results, failed))
}

fn threads(db: &dyn Db, writers: u64, txns: u64, root: &[u8; 32]) -> (Vec<WriterResult>, u64) {
    let barrier = Barrier::new(usize::try_from(writers).unwrap_or(usize::MAX));
    thread::scope(|scope| {
        let handles: Vec<_> = (0..writers)
            .map(|w| {
                let barrier = &barrier;
                scope.spawn(move || {
                    let session = db.session();
                    barrier.wait();
                    writer_loop(&mut *session?, w, txns, *root)
                })
            })
            .collect();
        let mut results = Vec::new();
        let mut failed = 0;
        for handle in handles {
            match handle.join() {
                Ok(Ok(r)) => results.push(r),
                Ok(Err(e)) => {
                    eprintln!("w2: a writer failed: {e}");
                    failed += 1;
                }
                Err(_) => failed += 1,
            }
        }
        (results, failed)
    })
}

fn summarize(results: &[WriterResult], failed: u64, retries: u64, planned: u64) -> Values {
    let start = results.iter().map(|r| r.start_unix_us).min().unwrap_or(0);
    let end = results.iter().map(|r| r.end_unix_us).max().unwrap_or(0);
    let wall_s = end.saturating_sub(start) as f64 / 1e6;
    let mut latencies: Vec<Duration> = results
        .iter()
        .flat_map(|r| r.latencies_us.iter().map(|&us| Duration::from_micros(us)))
        .collect();
    let committed = latencies.len() as u64;
    let lat = Latency::of(&mut latencies);
    let totals: Vec<u64> = results
        .iter()
        .map(|r| r.end_unix_us.saturating_sub(r.start_unix_us))
        .collect();
    let spread = match (totals.iter().min(), totals.iter().max()) {
        (Some(&min), Some(&max)) if min > 0 => max as f64 / min as f64,
        _ => f64::NAN,
    };
    Values::from([
        ("txns_per_s".into(), committed as f64 / wall_s),
        ("wall_s".into(), wall_s),
        ("committed".into(), committed as f64),
        ("planned".into(), planned as f64),
        ("p50_ms".into(), lat.p50_us / 1e3),
        ("p90_ms".into(), lat.p90_us / 1e3),
        ("p99_ms".into(), lat.p99_us / 1e3),
        ("p999_ms".into(), lat.p999_us / 1e3),
        ("max_ms".into(), lat.max_us / 1e3),
        ("mean_ms".into(), lat.mean_us / 1e3),
        (
            "busy".into(),
            results.iter().map(|r| r.busy).sum::<u64>() as f64,
        ),
        (
            "mismatched".into(),
            results.iter().map(|r| r.mismatched).sum::<u64>() as f64,
        ),
        ("failed_writers".into(), failed as f64),
        ("conflict_reruns".into(), retries as f64),
        ("spread".into(), spread),
    ])
}
