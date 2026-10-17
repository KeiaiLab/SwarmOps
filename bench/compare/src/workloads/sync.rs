//! The sync calls per commit of each engine, counted with the tracer (`crate::trace`): a child
//! process (`__sync`) creates a store, commits 32 untraced W1 transactions, then waits; the
//! tracer attaches and the child commits `n` more and exits. Run for two `n`, the difference
//! per transaction is what one commit costs, without the open, the warm-up and the close. The
//! pair runs once per run of the plan, as every measurement does.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use serde::Serialize;

use crate::engines::{Engine, Result, other};
use crate::trace::{self, Counts};

/// One engine's per-commit figures.
#[derive(Debug, Clone, Serialize)]
pub struct PerCommit {
    pub engine: String,
    /// The run of the plan it was counted in (1, 2, …).
    pub run: usize,
    pub txns: (u64, u64),
    pub syscalls: f64,
    pub written_bytes: f64,
    pub by_name: BTreeMap<String, f64>,
    pub syncs_by_target: BTreeMap<String, f64>,
}

impl PerCommit {
    pub fn syncs(&self) -> f64 {
        [
            "fsync",
            "fdatasync",
            "sync_file_range",
            "msync",
            "syncfs",
            "sync",
        ]
        .iter()
        .filter_map(|n| self.by_name.get(*n))
        .sum()
    }
}

fn traced(engine: &dyn Engine, base: &Path, txns: u64, exe: &Path) -> Result<Counts> {
    let dir = tempfile::Builder::new()
        .prefix(&format!("sync-{}-", engine.name()))
        .tempdir_in(base)?;
    let mut child = Command::new(exe)
        .arg("__sync")
        .arg(engine.name())
        .arg(dir.path())
        .arg(txns.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;
    let mut stdout = BufReader::new(child.stdout.take().expect("piped"));
    let mut line = String::new();
    stdout.read_line(&mut line)?;
    if line.trim() != "ready" {
        let _ = child.kill();
        let _ = child.wait();
        return Err(other(format!("__sync said {line:?}, not ready")));
    }
    let mut stdin = child.stdin.take().expect("piped");
    let root = trace::root_of(dir.path());
    // The tracer reaps the child; `child` is not waited for after this.
    let counts = trace::trace(child.id(), &root, || {
        let _ = stdin.write_all(b"go\n");
        let _ = stdin.flush();
    })
    .map_err(other)?;
    drop(stdin);
    line.clear();
    stdout.read_line(&mut line)?;
    if line.trim() != "done" {
        return Err(other(format!("__sync ended with {line:?}, not done")));
    }
    Ok(counts)
}

/// Run `run`'s count for `engine`.
pub fn per_commit(
    engine: &dyn Engine,
    run: usize,
    base: &Path,
    txns: (u64, u64),
    exe: &Path,
) -> Result<PerCommit> {
    let short = traced(engine, base, txns.0, exe)?;
    let long = traced(engine, base, txns.1, exe)?;
    let n = (txns.1 - txns.0) as f64;
    let diff = |x: u64, y: u64| (y as f64 - x as f64) / n;
    let per = |ma: &BTreeMap<String, u64>, mb: &BTreeMap<String, u64>| {
        let mut keys: Vec<&String> = ma.keys().chain(mb.keys()).collect();
        keys.sort_unstable();
        keys.dedup();
        keys.into_iter()
            .map(|k| {
                let (x, y) = (
                    ma.get(k).copied().unwrap_or(0),
                    mb.get(k).copied().unwrap_or(0),
                );
                (k.clone(), diff(x, y))
            })
            .collect::<BTreeMap<_, _>>()
    };
    Ok(PerCommit {
        engine: engine.name().to_owned(),
        run,
        txns,
        syscalls: diff(short.syscalls, long.syscalls),
        written_bytes: diff(short.written_bytes, long.written_bytes),
        by_name: per(&short.by_name, &long.by_name),
        syncs_by_target: per(&short.syncs_by_target, &long.syncs_by_target),
    })
}
