//! W4 on the largest seeded store (1,000,000 changesets):
//!
//! - open: a fresh process opens the store and looks up one changeset (`__open`); the open,
//!   the first lookup and the process from spawn to exit are timed. The files are in the page
//!   cache (dropping it needs root): this is a warm open. Each seeded store's first open after
//!   its load is timed once at seeding, before W3 ([`first_open`]), so these are re-opens; for
//!   an engine whose first open settles what its load left (`fjall-unflushed`,
//!   [`super::opens_as_loaded`]), each trial opens a fresh copy of the store as the load left
//!   it, taken at seeding ([`Seed::as_loaded`]), so that its row is that store's.
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

use tempfile::TempDir;

use super::{Seed, Values, copy_store};
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

/// One open of the store in `dir`, a seed of `n` changesets, by a fresh process (`__open`) that
/// then looks one changeset up: what that process timed, and its own time from spawn to exit.
fn open_once(engine: &dyn Engine, dir: &Path, n: u64, exe: &Path) -> Result<(Opened, Duration)> {
    let target = id(chain::SEED, n / 2);
    let started = Instant::now();
    let out = Command::new(exe)
        .arg("__open")
        .arg(engine.name())
        .arg(dir)
        .arg(hex(&target))
        .stderr(Stdio::inherit())
        .output()?;
    let process = started.elapsed();
    if !out.status.success() {
        return Err(other(format!("__open failed: {}", out.status)));
    }
    let opened: Opened = serde_json::from_slice(&out.stdout).map_err(other)?;
    Ok((opened, process))
}

/// The first open of `seed`'s store after its load, timed as one trial of [`open`] is: once per
/// seeded store, before any workload opens it (`main.rs`).
pub fn first_open(engine: &dyn Engine, seed: &Seed, exe: &Path) -> Result<Values> {
    let (opened, process) = open_once(engine, &seed.dir, seed.n, exe)?;
    Ok(Values::from([
        ("first_open_ms".into(), opened.open_us as f64 / 1e3),
        ("first_lookup_ms".into(), opened.first_us as f64 / 1e3),
        ("process_ms".into(), ms(process)),
    ]))
}

pub fn open(engine: &dyn Engine, seed: &Seed, trials: usize, exe: &Path) -> Result<Values> {
    open_with(seed, trials, |dir| open_once(engine, dir, seed.n, exe))
}

/// [`open`], each trial's open made by `open_one` on the directory it is given.
fn open_with(
    seed: &Seed,
    trials: usize,
    mut open_one: impl FnMut(&Path) -> Result<(Opened, Duration)>,
) -> Result<Values> {
    let (mut opens, mut firsts, mut processes) = (vec![], vec![], vec![]);
    for _ in 0..trials {
        // A fresh copy of the store as its load left it, made and synced untimed, for an
        // engine W4 opens so.
        let copy = seed
            .as_loaded
            .as_ref()
            .map(|as_loaded| copy_store(as_loaded.path()))
            .transpose()?;
        let dir = copy.as_ref().map_or(seed.dir.as_path(), TempDir::path);
        let (opened, process) = open_one(dir)?;
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use super::{Opened, open_with};
    use crate::workloads::Seed;

    /// The files directly in `dir`, by name, with their bytes.
    fn files(dir: &Path) -> BTreeMap<String, Vec<u8>> {
        fs::read_dir(dir)
            .expect("a directory")
            .map(|entry| {
                let entry = entry.expect("an entry");
                let name = entry.file_name().to_string_lossy().into_owned();
                (name, fs::read(entry.path()).expect("a file"))
            })
            .collect()
    }

    /// A seeded store in a fresh directory in `work`, as its load left it: a journal.
    fn loaded(work: &Path) -> Seed {
        let keep = tempfile::tempdir_in(work).expect("a store dir");
        let dir = keep.path().to_path_buf();
        fs::write(dir.join("0.jnl"), b"what the load left").expect("write a journal");
        Seed {
            n: 1_000,
            dir,
            load_secs: 0.0,
            as_loaded: None,
            _keep: keep,
        }
    }

    /// What an open of an unflushed fjall store does to it: it replays the journal into a table
    /// and deletes the journal.
    fn settle(dir: &Path) {
        let _ = fs::remove_file(dir.join("0.jnl"));
        fs::write(dir.join("table"), b"settled").expect("write a table");
    }

    /// What `__open` prints, and its process's time.
    fn timed() -> (Opened, Duration) {
        let opened = Opened {
            open_us: 1_000,
            first_us: 10,
        };
        (opened, Duration::from_millis(5))
    }

    #[test]
    fn each_w4_open_of_a_store_kept_as_loaded_is_of_a_fresh_copy_of_it() {
        let work = tempfile::tempdir().expect("a work dir");
        let mut seed = loaded(work.path());
        seed.keep_as_loaded()
            .expect("copy the store as its load left it");
        let kept = seed
            .as_loaded
            .as_ref()
            .expect("the copy")
            .path()
            .to_path_buf();
        let as_loaded = files(&kept);
        // The store's first open, timed at seeding, has settled it since.
        settle(&seed.dir);
        let mut opened: Vec<(PathBuf, BTreeMap<String, Vec<u8>>)> = Vec::new();
        open_with(&seed, 3, |dir| {
            opened.push((dir.to_path_buf(), files(dir)));
            settle(dir);
            Ok(timed())
        })
        .expect("three opens");
        assert_eq!(opened.len(), 3);
        for (dir, held) in &opened {
            assert!(
                *dir != seed.dir && *dir != kept,
                "{}: opened in place",
                dir.display()
            );
            assert_eq!(
                *held,
                as_loaded,
                "{}: not the store as its load left it",
                dir.display()
            );
            assert!(
                !dir.exists(),
                "{}: the copy outlived its open",
                dir.display()
            );
        }
        assert_eq!(
            files(&kept),
            as_loaded,
            "the copy kept at seeding was opened"
        );

        // A store not kept as loaded is opened in place, every time.
        let settled = loaded(work.path());
        let mut dirs = Vec::new();
        open_with(&settled, 2, |dir| {
            dirs.push(dir.to_path_buf());
            Ok(timed())
        })
        .expect("two opens");
        assert_eq!(dirs, [settled.dir.clone(), settled.dir.clone()]);
    }
}
