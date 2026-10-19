//! The workloads (W1–W6 of the M1.5 plan's Task 8) and what they share: the run plan, the
//! records they produce, the seeded stores, and the writer loop of W2.

pub mod breakdown;
pub mod sync;
pub mod w1;
pub mod w2;
pub mod w3;
pub mod w4;
pub mod w6;

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tempfile::TempDir;

use crate::data::{Id, chain, changeset};
use crate::engines::{Engine, Error, Main, Result, Session, other};
use crate::measure::Env;

/// How much of everything to run.
#[derive(Debug, Clone)]
pub struct Plan {
    pub quick: bool,
    pub runs: usize,
    /// Syncs per raw `fdatasync` probe.
    pub probes: usize,
    pub w1_warmup: u64,
    pub w1_txns: u64,
    /// The native commits the breakdown times per run (after W1's warm-up): 1,024 in the full
    /// plan, so that one compaction (every 1,024 records) falls among them.
    pub breakdown_txns: u64,
    pub w2_writers: u64,
    pub w2_txns: u64,
    /// Also run W2 on threads for the engines that run it on processes.
    pub w2_threads_too: bool,
    /// The W3 store sizes; the largest is W4's and W5's, the smallest W6's.
    pub sizes: Vec<u64>,
    pub lookups: usize,
    pub w4_opens: usize,
    pub w4_kills: usize,
    /// The kill comes this long (a seeded draw from the range, in ms) after the writer's
    /// first acknowledged commit.
    pub w4_kill_after_ms: (u64, u64),
    pub w6_idle_lookups: usize,
    pub w6_busy: Duration,
    /// The traced runs' transaction counts: the per-commit figure is their difference.
    pub sync_txns: (u64, u64),
}

impl Plan {
    pub fn full() -> Plan {
        Plan {
            quick: false,
            runs: 3,
            probes: 40,
            w1_warmup: 200,
            w1_txns: 10_000,
            breakdown_txns: 1_024,
            w2_writers: 32,
            w2_txns: 1_000,
            w2_threads_too: true,
            sizes: vec![100_000, 1_000_000],
            lookups: 20_000,
            w4_opens: 5,
            w4_kills: 3,
            w4_kill_after_ms: (100, 1_000),
            w6_idle_lookups: 20_000,
            w6_busy: Duration::from_secs(10),
            sync_txns: (64, 64 + 1_024),
        }
    }

    /// A smoke run of every workload, at sizes that keep it under 2 minutes once built.
    pub fn quick() -> Plan {
        Plan {
            quick: true,
            runs: 3,
            probes: 5,
            w1_warmup: 10,
            w1_txns: 50,
            breakdown_txns: 50,
            w2_writers: 32,
            w2_txns: 4,
            w2_threads_too: false,
            sizes: vec![5_000, 20_000],
            lookups: 1_000,
            w4_opens: 2,
            w4_kills: 1,
            w4_kill_after_ms: (20, 120),
            w6_idle_lookups: 1_000,
            w6_busy: Duration::from_millis(500),
            sync_txns: (16, 16 + 64),
        }
    }
}

/// One measurement: a workload (and its variant) on one engine, one run.
#[derive(Debug, Clone, Serialize)]
pub struct Record {
    pub workload: &'static str,
    pub variant: String,
    pub engine: String,
    pub run: usize,
    pub env: Env,
    pub values: BTreeMap<String, f64>,
}

/// The values of one measurement, by name.
pub type Values = BTreeMap<String, f64>;

/// `n` in thousands or millions, for variant names: `100k`, `1M`.
pub fn size_name(n: u64) -> String {
    if n >= 1_000_000 && n.is_multiple_of(1_000_000) {
        format!("{}M", n / 1_000_000)
    } else {
        format!("{}k", n / 1_000)
    }
}

/// What one W2 writer did, as a child process prints it (one JSON line) or a thread
/// returns it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriterResult {
    pub writer: u64,
    /// When its first transaction started and its last ended, in µs since the Unix epoch, so
    /// that the parent can tell the wall time of processes it did not time itself.
    pub start_unix_us: u64,
    pub end_unix_us: u64,
    /// Every transaction's latency (first attempt to commit, `Busy` retries included), µs.
    pub latencies_us: Vec<u64>,
    pub busy: u64,
    pub mismatched: u64,
}

pub fn unix_us() -> u64 {
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    u64::try_from(since.as_micros()).unwrap_or(u64::MAX)
}

/// W2's writer `w`: `txns` transactions, each inserting the next changeset of the writer's own
/// chain (rooted at `root`) and moving `main` to it, comparing against what `main` names inside
/// the transaction. A `Busy` is counted and the transaction tried again.
pub fn writer_loop(session: &mut dyn Session, w: u64, txns: u64, root: Id) -> Result<WriterResult> {
    let actor = format!("w2/writer-{w}");
    let mut latencies_us = Vec::with_capacity(usize::try_from(txns).unwrap_or(0));
    let (mut busy, mut mismatched) = (0, 0);
    let mut guess = None;
    let start_unix_us = unix_us();
    for n in 0..txns {
        let cs = changeset(chain::W2_WRITER + w, n, Some(root));
        let started = Instant::now();
        loop {
            match session.commit(&cs, Main::Current { guess }, &actor) {
                Ok(committed) => {
                    mismatched += u64::from(committed.mismatched);
                    guess = Some(cs.id);
                    break;
                }
                Err(Error::Busy(_)) => busy += 1,
                Err(e) => return Err(e),
            }
        }
        latencies_us.push(u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX));
    }
    Ok(WriterResult {
        writer: w,
        start_unix_us,
        end_unix_us: unix_us(),
        latencies_us,
        busy,
        mismatched,
    })
}

/// How many changesets one seeding transaction inserts (the native store's frame holds at
/// most 65,535 records).
const SEED_BATCH: u64 = 50_000;

/// A store of `n` changesets (chain `SEED`, each the child of the one before), bulk-loaded in
/// transactions of 50,000 (then `main` moved to the last), settled and closed: what W3–W6 read.
/// Kept for the whole invocation.
pub struct Seed {
    pub n: u64,
    pub dir: PathBuf,
    /// How long the load took (not a workload: for the record).
    pub load_secs: f64,
    /// For an engine W4 opens as the load left it ([`opens_as_loaded`]): a copy of the store
    /// taken at seeding, before anything opened it ([`Seed::keep_as_loaded`]), which W4 copies
    /// again for each of its opens.
    pub as_loaded: Option<TempDir>,
    _keep: tempfile::TempDir,
}

impl Seed {
    /// Keeps a copy of the store as it is now in [`Seed::as_loaded`].
    pub fn keep_as_loaded(&mut self) -> Result<()> {
        self.as_loaded = Some(copy_store(&self.dir)?);
        Ok(())
    }
}

/// A copy of the store in `dir`, in a fresh directory beside it, every file and directory of it
/// synced, so that an open of the copy finds the files as `dir` holds them, on disk. A store of
/// these engines holds only files and directories; anything else is an error. Removed when
/// dropped.
pub fn copy_store(dir: &Path) -> Result<TempDir> {
    let parent = dir
        .parent()
        .ok_or_else(|| other("a store dir has a parent"))?;
    let name = dir.file_name().map_or_else(
        || "store".into(),
        |name| name.to_string_lossy().into_owned(),
    );
    let copy = tempfile::Builder::new()
        .prefix(&format!("{name}-copy-"))
        .tempdir_in(parent)?;
    copy_tree(dir, copy.path())?;
    Ok(copy)
}

/// Copies what directory `from` holds into the empty directory `to`, syncing every file it
/// writes and every directory it fills.
fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let (path, target) = (entry.path(), to.join(entry.file_name()));
        let kind = entry.file_type()?;
        if kind.is_dir() {
            fs::create_dir(&target)?;
            copy_tree(&path, &target)?;
        } else if kind.is_file() {
            fs::copy(&path, &target)?;
            File::open(&target)?.sync_all()?;
        } else {
            return Err(other(format!(
                "{} is neither a file nor a directory",
                path.display()
            )));
        }
    }
    File::open(to)?.sync_all()?;
    Ok(())
}

pub fn seed(engine: &dyn Engine, n: u64, base: &Path) -> Result<Seed> {
    let keep = tempfile::Builder::new()
        .prefix(&format!("seed-{}-{}-", engine.name(), size_name(n)))
        .tempdir_in(base)?;
    let dir = keep.path().to_path_buf();
    let started = Instant::now();
    {
        let db = engine.create(&dir)?;
        let mut session = db.session()?;
        let mut from = 0;
        while from < n {
            let to = (from + SEED_BATCH).min(n);
            let batch: Vec<_> = (from..to)
                .map(|i| changeset(chain::SEED, i, None))
                .collect();
            session.load(&batch)?;
            from = to;
        }
        drop(session);
        db.settle()?;
    }
    Ok(Seed {
        n,
        dir,
        load_secs: started.elapsed().as_secs_f64(),
        as_loaded: None,
        _keep: keep,
    })
}

/// The engines whose files are another's: SQLite `NORMAL` reads the same files as SQLite
/// `FULL` (`synchronous` is a connection setting), so the read-only workloads skip it.
pub fn same_files_as(engine: &str) -> Option<&'static str> {
    (engine == "sqlite-normal").then_some("sqlite")
}

/// The engines whose writes are another's: `fjall-unflushed` commits as `fjall` does and
/// differs only in how a seeded store is settled, so the write workloads (the sync count, W1,
/// W2, W6) skip it.
pub fn same_writes_as(engine: &str) -> Option<&'static str> {
    (engine == "fjall-unflushed").then_some("fjall")
}

/// The engines W4 opens as the load left them, each open on a fresh copy of the store taken at
/// seeding ([`Seed::as_loaded`]): `fjall-unflushed`, whose first open settles what its load left
/// (it replays every journal, flushes the memtables it recovers and deletes the sealed
/// journals), so that any later open of the store itself times the settled store, as `fjall`'s
/// row does.
pub fn opens_as_loaded(engine: &str) -> bool {
    engine == "fjall-unflushed"
}
