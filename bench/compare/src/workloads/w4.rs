//! W4 on the largest seeded store (1,000,000 changesets):
//!
//! - open: a fresh process opens the store and looks up one changeset (`__open`); the open,
//!   the first lookup and the process from spawn to exit are timed. The files are in the page
//!   cache (dropping it needs root): this is a warm open. In a full run W3 has opened the store
//!   before, so this times a re-open; with `--only w4`, run 1's first trial is the first open
//!   after the load, which `open_max_ms` shows.
//! - recovery: a writer process (`__writer`) commits W1's transaction in a loop, printing each
//!   acknowledged commit; a seeded 0.1–1 s after its first, it is killed with SIGKILL, most
//!   likely inside a commit. A fresh process (`__recover`) then opens the store, commits one
//!   transaction (where most engines repair what the kill left) and checks that the last
//!   acknowledged changeset is there. A killed process loses nothing the kernel holds, so this
//!   is recovery from a crash of the process, not from a power loss.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::{Seed, Values};
use crate::data::{chain, hex, id, mix};
use crate::engines::{Engine, Result, other};
use crate::measure::{median_range, ms};

/// What `__open` prints.
#[derive(Debug, Serialize, Deserialize)]
pub struct Opened {
    pub open_us: u64,
    pub first_us: u64,
}

/// What `__recover` prints.
#[derive(Debug, Serialize, Deserialize)]
pub struct Recovered {
    pub open_us: u64,
    pub write_us: u64,
    pub acked_present: bool,
}

pub fn open(engine: &dyn Engine, seed: &Seed, trials: usize, exe: &Path) -> Result<Values> {
    let target = id(chain::SEED, seed.n / 2);
    let (mut opens, mut firsts, mut processes) = (vec![], vec![], vec![]);
    for _ in 0..trials {
        let started = Instant::now();
        let out = Command::new(exe)
            .arg("__open")
            .arg(engine.name())
            .arg(&seed.dir)
            .arg(hex(&target))
            .stderr(Stdio::inherit())
            .output()?;
        let process = started.elapsed();
        if !out.status.success() {
            return Err(other(format!("__open failed: {}", out.status)));
        }
        let opened: Opened = serde_json::from_slice(&out.stdout).map_err(other)?;
        opens.push(opened.open_us as f64 / 1e3);
        firsts.push(opened.first_us as f64 / 1e3);
        processes.push(ms(process));
    }
    let (open_ms, _, open_max) = median_range(&mut opens);
    let (first_ms, _, _) = median_range(&mut firsts);
    let (process_ms, _, _) = median_range(&mut processes);
    Ok(Values::from([
        ("open_ms".into(), open_ms),
        ("open_max_ms".into(), open_max),
        ("first_lookup_ms".into(), first_ms),
        ("open_first_ms".into(), open_ms + first_ms),
        ("process_ms".into(), process_ms),
        ("trials".into(), trials as f64),
    ]))
}

pub fn recover(
    engine: &dyn Engine,
    seed: &Seed,
    first_trial: u64,
    trials: usize,
    after_ms: (u64, u64),
    exe: &Path,
) -> Result<Values> {
    let root = id(chain::SEED, seed.n - 1);
    let (mut opens, mut writes, mut totals, mut acked, mut killed_after) =
        (vec![], vec![], vec![], vec![], vec![]);
    for trial in first_trial..first_trial + trials as u64 {
        let writer_chain = chain::W4_WRITER + trial;
        let mut child = Command::new(exe)
            .arg("__writer")
            .arg(engine.name())
            .arg(&seed.dir)
            .arg(writer_chain.to_string())
            .arg(hex(&root))
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let stdout = child.stdout.take().expect("piped");
        let (lines, received) = mpsc::channel();
        let reader = thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if lines.send(line).is_err() {
                    break;
                }
            }
        });
        // The first acknowledged commit: the store is open and writing.
        let first = received.recv_timeout(Duration::from_secs(600));
        if first.is_err() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(other("the writer never acknowledged a commit"));
        }
        let (lo, hi) = after_ms;
        let after = lo + mix(trial) % (hi - lo).max(1);
        thread::sleep(Duration::from_millis(after));
        child.kill()?;
        child.wait()?;
        let _ = reader.join();
        let last: u64 = std::iter::once(first.expect("checked"))
            .chain(received.try_iter())
            .filter_map(|l| l.strip_prefix("acked ")?.parse().ok())
            .max()
            .ok_or_else(|| other("no acknowledged commit"))?;
        let started = Instant::now();
        let out = Command::new(exe)
            .arg("__recover")
            .arg(engine.name())
            .arg(&seed.dir)
            .arg(writer_chain.to_string())
            .arg(last.to_string())
            .arg((chain::W4_RECOVER + trial).to_string())
            .stderr(Stdio::inherit())
            .output()?;
        let process = started.elapsed();
        if !out.status.success() {
            return Err(other(format!("__recover failed: {}", out.status)));
        }
        let recovered: Recovered = serde_json::from_slice(&out.stdout).map_err(other)?;
        if !recovered.acked_present {
            return Err(other(format!(
                "an acknowledged commit ({last}) is missing after the kill"
            )));
        }
        opens.push(recovered.open_us as f64 / 1e3);
        writes.push(recovered.write_us as f64 / 1e3);
        totals.push((recovered.open_us + recovered.write_us) as f64 / 1e3);
        acked.push((last + 1) as f64);
        killed_after.push(after as f64);
        let _ = process;
    }
    let (open_ms, _, _) = median_range(&mut opens);
    let (write_ms, _, _) = median_range(&mut writes);
    let (total_ms, _, total_max) = median_range(&mut totals);
    let (acked, _, _) = median_range(&mut acked);
    let (after, _, _) = median_range(&mut killed_after);
    Ok(Values::from([
        ("open_ms".into(), open_ms),
        ("write_ms".into(), write_ms),
        ("total_ms".into(), total_ms),
        ("total_max_ms".into(), total_max),
        ("acked".into(), acked),
        ("killed_after_ms".into(), after),
        ("trials".into(), trials as f64),
    ]))
}
