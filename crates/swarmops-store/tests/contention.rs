//! 32 writer processes on one native metadata store: how write transactions queue on
//! `meta/lock` (docs/perf.md, "M1 store level" and "M1 store fixes (7c)").
//!
//! Ignored, since it runs minutes of fsync'd transactions: `just check-slow` runs it, and
//! `cargo nextest run --release -p swarmops-store --run-ignored all --test contention
//! --no-capture` prints its table, as docs/perf.md cites it.
//!
//! The test runs this test binary again as [`CHILDREN`] children (selected by [`WORK_DIR`])
//! on a store in a fresh temporary directory under `SWARMOPS_BENCH_DIR` (default
//! `~/.local/state/swarmops-bench`, on the native filesystem). Each child opens the store,
//! says it is ready, and waits until the parent, once all are ready, says go; then it runs
//! [`PER_CHILD`] write transactions back to back, as a writer that loops does, each inserting
//! the next changeset of a chain of its own and committing it, with the default 10 s lock
//! timeout: a `Busy` is counted and the transaction tried again. [`PAUSE`] puts a pause
//! between two transactions of a child, as an agent leaves at least a process start between
//! two commits. For every transaction a child times
//!
//! - the transaction: from calling [`MetaStore::write`] until `commit` returns;
//! - the lock wait: until [`MetaStore::write`] returns, which is the wait for `meta/lock`
//!   plus the tail checks and replay under it; the uncontended `write`s the parent times once
//!   the children are done show what those cost alone.
//!
//! It prints one JSON line (its percentiles, `Busy` count and elapsed time) and writes its
//! samples to a file. The parent aggregates them and prints the table, and the uncontended
//! `write`s' cost; then it checks the store as `verify` would (every changeset there, each
//! chain intact, one frame per transaction, and the seqs contiguous in journal order), prints
//! what an uncontended transaction costs, and asserts last that no child met `Busy`.
//!
//! Nothing in it waits forever, whatever the lock does. A child gives up at its
//! [`MAX_BUSY_IN_A_ROW`]th `Busy` in a row, and past [`CHILD_DEADLINE`]; the parent fails the
//! test as soon as a child fails, and waits for the children only until [`PARENT_DEADLINE`];
//! and whatever ends the test early, [`Children`] kills and reaps every child still running.

#[path = "../benches/common/mod.rs"]
mod common;

use std::fmt::Write as _;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_core::{ChangesetId, ObjectKind, hash_object};
use swarmops_store::{
    ChangesetMeta, Frame, InsertOutcome, Journal, Layout, MetaStore, RealFs, Record, StoreError,
};

use crate::common::base_dir;

/// How many writer processes the test runs.
const CHILDREN: u64 = 32;

/// How many write transactions each child runs.
const PER_CHILD: u64 = 1_000;

/// Every child's transactions together.
const TOTAL: u64 = CHILDREN * PER_CHILD;

/// This test's name, which the children are run with.
const TEST: &str = "thirty_two_processes_thousand_transactions_each";

/// Set only in the children: the work directory, which holds the store (`store/`), the
/// barrier files (`ready-<n>`, `go`), the children's samples (`samples-<n>`) and their output
/// (`stdout-<n>`, `stderr-<n>`).
const WORK_DIR: &str = "SWARMOPS_TEST_CONTENTION_DIR";

/// Set with [`WORK_DIR`]: the child's number, from 0, which is also its chain's.
const CHILD_NUMBER: &str = "SWARMOPS_TEST_CONTENTION_CHILD";

/// Set to put a pause between two transactions of a child: its length in microseconds.
const PAUSE: &str = "SWARMOPS_TEST_CONTENTION_PAUSE_US";

/// How a child's summary line starts.
const SUMMARY: &str = "{\"child\":";

/// At how many `Busy` errors in a row (the transaction tried again after each) a child gives
/// up: 5 minutes at the default 10 s lock timeout.
const MAX_BUSY_IN_A_ROW: u64 = 30;

/// How long after the go a child may still start a transaction: well past the longest whole
/// run measured on this host (13.2 min, on a slow disk).
const CHILD_DEADLINE: Duration = Duration::from_mins(60);

/// How long after the go the parent waits for the children before it kills them: a child's
/// deadline, one more transaction that meets every `Busy` it may at 10 s each, and a minute.
const PARENT_DEADLINE: Duration =
    CHILD_DEADLINE.saturating_add(Duration::from_secs(MAX_BUSY_IN_A_ROW * 10 + 60));

/// How many uncontended `write`s, and transactions, the parent times once the children are done.
const SOLO: u64 = 100;

/// How long a process waits at the barrier before it gives up.
const BARRIER_TIMEOUT: Duration = Duration::from_secs(120);

fn store_root(work: &Path) -> Utf8PathBuf {
    Utf8PathBuf::from_path_buf(work.join("store")).expect("UTF-8 work dir")
}

/// What changeset `j` of chain `chain` is named by.
fn key(chain: u64, j: u64) -> [u8; 16] {
    let mut key = [0; 16];
    key[..8].copy_from_slice(&chain.to_le_bytes());
    key[8..].copy_from_slice(&j.to_le_bytes());
    key
}

/// The id of changeset `j` of chain `chain`.
fn id(chain: u64, j: u64) -> ChangesetId {
    ChangesetId(hash_object(ObjectKind::Changeset, &key(chain, j)))
}

/// The delta hash of changeset `j` of chain `chain`.
fn delta(chain: u64, j: u64) -> [u8; 32] {
    hash_object(ObjectKind::Blob, &key(chain, j)).0
}

/// The parents of changeset `j` of chain `chain`: none for its first, else the one before.
fn parents(chain: u64, j: u64) -> Vec<ChangesetId> {
    j.checked_sub(1).map(|i| id(chain, i)).into_iter().collect()
}

/// Changeset `j` of chain `chain`, as a writer hands it to `insert_changeset`.
fn changeset(chain: u64, j: u64) -> ChangesetMeta {
    ChangesetMeta {
        id: id(chain, j),
        seq: 0,
        generation: 0,
        delta_hash: delta(chain, j),
        parents: parents(chain, j),
    }
}

/// Nearest-rank percentile over sorted samples.
fn percentile(sorted: &[Duration], percent: usize) -> Duration {
    let rank = (sorted.len() * percent).div_ceil(100).max(1);
    sorted[rank - 1]
}

/// `d` in milliseconds, with two decimals.
fn ms(d: Duration) -> String {
    format!("{:.2} ms", d.as_secs_f64() * 1e3)
}

/// p50, p99 and max of `samples`, sorting them.
fn spread(samples: &mut [Duration]) -> [Duration; 3] {
    samples.sort_unstable();
    [
        percentile(samples, 50),
        percentile(samples, 99),
        samples[samples.len() - 1],
    ]
}

/// Waits until `path` exists.
fn wait_for(path: &Path) {
    let deadline = Instant::now() + BARRIER_TIMEOUT;
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        thread::sleep(Duration::from_millis(1));
    }
}

/// The pause between two transactions of a child: [`PAUSE`], or none.
fn pause() -> Duration {
    std::env::var(PAUSE).map_or(Duration::ZERO, |micros| {
        Duration::from_micros(micros.parse().expect("the pause is in whole microseconds"))
    })
}

/// The work directory and number of this process, if it is a child.
fn child_env() -> Option<(PathBuf, u64)> {
    let work = std::env::var_os(WORK_DIR)?;
    let number = std::env::var(CHILD_NUMBER)
        .expect("a child has a number")
        .parse()
        .expect("a child's number is a u64");
    Some((PathBuf::from(work), number))
}

/// A child: [`PER_CHILD`] write transactions on chain `number`, timed; see the module
/// documentation.
fn child(work: &Path, number: u64) {
    let store = MetaStore::open(&store_root(work)).expect("open the store");
    fs::write(work.join(format!("ready-{number}")), b"").expect("say ready");
    wait_for(&work.join("go"));
    let pause = pause();
    let started = Instant::now();
    let deadline = started + CHILD_DEADLINE;
    let capacity = usize::try_from(PER_CHILD).expect("PER_CHILD fits in usize");
    let (mut transactions, mut waits) =
        (Vec::with_capacity(capacity), Vec::with_capacity(capacity));
    let (mut busy, mut checkpoints) = (0_u64, 0_u64);
    for j in 0..PER_CHILD {
        if j > 0 && !pause.is_zero() {
            thread::sleep(pause);
        }
        assert!(
            Instant::now() < deadline,
            "child {number}: transaction {j} not started {CHILD_DEADLINE:?} after the go"
        );
        let meta = changeset(number, j);
        let begun = Instant::now();
        let mut in_a_row = 0;
        let mut write = loop {
            match store.write() {
                Ok(write) => break write,
                Err(StoreError::Busy) => {
                    busy += 1;
                    in_a_row += 1;
                    assert!(
                        in_a_row < MAX_BUSY_IN_A_ROW,
                        "child {number}, transaction {j}: Busy {in_a_row} times in a row"
                    );
                }
                Err(err) => panic!("child {number}, transaction {j}: {err}"),
            }
        };
        let waited = begun.elapsed();
        let outcome = write.insert_changeset(meta).expect("insert");
        assert!(
            matches!(outcome, InsertOutcome::Inserted { .. }),
            "child {number}, transaction {j}: {outcome:?}"
        );
        let info = write.commit().expect("commit");
        transactions.push(begun.elapsed());
        waits.push(waited);
        checkpoints += u64::from(info.checkpointed);
    }
    let elapsed = started.elapsed();

    let mut samples = String::new();
    for (transaction, wait) in transactions.iter().zip(&waits) {
        writeln!(samples, "{} {}", transaction.as_micros(), wait.as_micros()).expect("format");
    }
    fs::write(work.join(format!("samples-{number}")), samples).expect("write the samples");
    let [txn_p50, txn_p99, txn_max] = spread(&mut transactions).map(|d| d.as_micros());
    let [lock_p50, lock_p99, lock_max] = spread(&mut waits).map(|d| d.as_micros());
    println!(
        "{SUMMARY}{number},\"transactions\":{PER_CHILD},\"pause_us\":{},\"busy\":{busy},\
         \"checkpoints\":{checkpoints},\"elapsed_us\":{},\"txn_p50_us\":{txn_p50},\
         \"txn_p99_us\":{txn_p99},\"txn_max_us\":{txn_max},\"lock_p50_us\":{lock_p50},\
         \"lock_p99_us\":{lock_p99},\"lock_max_us\":{lock_max}}}",
        pause.as_micros(),
        elapsed.as_micros()
    );
}

/// The value of the integer field `name` in the flat JSON object `line`.
fn field(line: &str, name: &str) -> u64 {
    let key = format!("\"{name}\":");
    let start = line
        .find(&key)
        .unwrap_or_else(|| panic!("no {name} in {line}"))
        + key.len();
    let value = &line[start..];
    let end = value.find([',', '}']).unwrap_or(value.len());
    value[..end]
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("{name} in {line} is not an integer"))
}

/// The children, killed and reaped if still running when this drops: whatever ends the test
/// early (a panic, a failed spawn, a missed deadline), no child outlives it.
struct Children(Vec<Child>);

impl Drop for Children {
    fn drop(&mut self) {
        for child in &mut self.0 {
            // A child that has exited already is reaped (or was), and is not signalled.
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// The file in `work` that child `number`'s `stream` ("stdout" or "stderr") goes to.
fn output_file(work: &Path, stream: &str, number: u64) -> PathBuf {
    work.join(format!("{stream}-{number}"))
}

/// What child `number` wrote to `stream`.
fn output(work: &Path, stream: &str, number: u64) -> String {
    fs::read_to_string(output_file(work, stream, number))
        .unwrap_or_else(|e| format!("({stream} of child {number} unreadable: {e})"))
}

/// Spawns the children on the store in `work`, each writing its stdout and stderr to files
/// there.
fn spawn_children(work: &Path) -> Children {
    let exe = std::env::current_exe().expect("this test binary");
    let mut children = Children(Vec::new());
    for number in 0..CHILDREN {
        let file = |stream| {
            File::create(output_file(work, stream, number)).expect("create a child's output file")
        };
        let child = Command::new(&exe)
            .args(["--exact", TEST, "--ignored", "--nocapture"])
            .env(WORK_DIR, work)
            .env(CHILD_NUMBER, number.to_string())
            .stdout(file("stdout"))
            .stderr(file("stderr"))
            .spawn()
            .expect("spawn a child");
        children.0.push(child);
    }
    children
}

/// Waits until every child is ready; a child that exits first fails the test with its
/// stderr.
fn wait_until_ready(work: &Path, children: &mut Children) {
    let deadline = Instant::now() + BARRIER_TIMEOUT;
    let ready = |number| work.join(format!("ready-{number}")).exists();
    while !(0..CHILDREN).all(ready) {
        for (number, child) in (0..).zip(&mut children.0) {
            if let Some(status) = child.try_wait().expect("poll a child") {
                let stderr = output(work, "stderr", number);
                panic!("child {number} exited before it was ready: {status}\n{stderr}");
            }
        }
        assert!(Instant::now() < deadline, "the children never got ready");
        thread::sleep(Duration::from_millis(1));
    }
}

/// Waits until every child has exited cleanly. A child that fails fails the test at once,
/// with its output, and so do children still running at `deadline`; the guard then kills the
/// rest.
fn wait_for_children(work: &Path, children: &mut Children, deadline: Instant) {
    loop {
        let mut running = Vec::new();
        for (number, child) in (0..).zip(&mut children.0) {
            match child.try_wait().expect("poll a child") {
                None => running.push(number),
                Some(status) if status.success() => {}
                Some(status) => panic!(
                    "child {number}: {status}\n--- stdout ---\n{}\n--- stderr ---\n{}",
                    output(work, "stdout", number),
                    output(work, "stderr", number),
                ),
            }
        }
        if running.is_empty() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "children {running:?} still running {PARENT_DEADLINE:?} after the go"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

/// The summary line child `number` printed.
fn summary(work: &Path, number: u64) -> String {
    let stdout = output(work, "stdout", number);
    stdout
        .lines()
        .find(|line| line.starts_with(SUMMARY))
        .unwrap_or_else(|| panic!("child {number} printed no summary:\n{stdout}"))
        .to_owned()
}

/// Every child's samples: the transactions' times and the lock waits.
fn samples(work: &Path) -> (Vec<Duration>, Vec<Duration>) {
    let (mut transactions, mut waits) = (Vec::new(), Vec::new());
    for number in 0..CHILDREN {
        let text = fs::read_to_string(work.join(format!("samples-{number}"))).expect("samples");
        for line in text.lines() {
            let (transaction, wait) = line.split_once(' ').expect("two samples a line");
            let micros = |s: &str| Duration::from_micros(s.parse().expect("microseconds"));
            transactions.push(micros(transaction));
            waits.push(micros(wait));
        }
    }
    (transactions, waits)
}

/// Runs the children on the store in `work`, lets them go together once all are ready, and
/// returns their summary lines and the time from the go until the last one exited.
fn run_children(work: &Path) -> (Vec<String>, Duration) {
    let mut children = spawn_children(work);
    wait_until_ready(work, &mut children);
    let started = Instant::now();
    fs::write(work.join("go"), b"").expect("say go");
    wait_for_children(work, &mut children, started + PARENT_DEADLINE);
    let wall = started.elapsed();
    let lines = (0..CHILDREN).map(|number| summary(work, number)).collect();
    (lines, wall)
}

/// Prints what the children measured, aggregated, and returns how many `Busy` errors they
/// met.
fn print_contention(work: &Path, lines: &[String], wall: Duration) -> u64 {
    let busy = lines.iter().map(|line| field(line, "busy")).sum();
    let elapsed = |line: &String| Duration::from_micros(field(line, "elapsed_us"));
    let fastest = lines.iter().map(elapsed).min().expect("children");
    let slowest = lines.iter().map(elapsed).max().expect("children");
    let (mut transactions, mut waits) = samples(work);
    assert_eq!(transactions.len(), usize::try_from(TOTAL).expect("fits"));
    let [txn_p50, txn_p99, txn_max] = spread(&mut transactions);
    let [lock_p50, lock_p99, lock_max] = spread(&mut waits);
    let total = f64::from(u32::try_from(TOTAL).expect("TOTAL fits in u32"));
    println!(
        "{CHILDREN} processes x {PER_CHILD} write transactions of one changeset, {} us apart:",
        pause().as_micros()
    );
    println!(
        "  wall {:.1} s, {:.1} transactions/s, Busy {busy}",
        wall.as_secs_f64(),
        total / wall.as_secs_f64()
    );
    println!(
        "  transaction: p50 {} p99 {} max {}",
        ms(txn_p50),
        ms(txn_p99),
        ms(txn_max)
    );
    println!(
        "  lock wait (until write() returns): p50 {} p99 {} max {}",
        ms(lock_p50),
        ms(lock_p99),
        ms(lock_max)
    );
    println!(
        "  child total time: min {:.1} s, max {:.1} s, spread {:.2}x",
        fastest.as_secs_f64(),
        slowest.as_secs_f64(),
        slowest.as_secs_f64() / fastest.as_secs_f64()
    );
    for line in lines {
        println!("  {line}");
    }
    busy
}

/// The journal at the end: every frame, and how many lie past `journal.ckpt`.
struct JournalShape {
    frames: Vec<Frame>,
    past_ckpt: usize,
}

impl JournalShape {
    /// Reads the journal of the store at `root` and prints its shape.
    fn read(root: &Utf8Path) -> JournalShape {
        let layout = Layout::new(root);
        let journal = Journal::open(&layout, Arc::new(RealFs)).expect("open the journal");
        let frames = journal
            .frames_from(0, usize::MAX)
            .expect("read the journal");
        let ckpt = journal.ckpt().expect("read journal.ckpt");
        let past_ckpt = frames.iter().filter(|frame| frame.offset >= ckpt).count();
        let compacted = frames
            .iter()
            .filter(|frame| matches!(frame.records[..], [Record::Compacted { .. }]))
            .count();
        let log_len = fs::metadata(layout.journal_log())
            .expect("journal.log")
            .len();
        println!(
            "  journal: {} frames ({compacted} Compacted), {past_ckpt} past journal.ckpt, \
             journal.log {log_len} bytes",
            frames.len()
        );
        JournalShape { frames, past_ckpt }
    }
}

/// Checks the store as `verify` would, through a snapshot: every changeset there, each in the
/// frame of a transaction of its own with the seqs contiguous in journal order, and every
/// chain intact.
fn check_store(store: &MetaStore, frames: &[Frame]) {
    let read = store.read().expect("a snapshot");
    assert_eq!(read.count_changesets().expect("count"), TOTAL);
    let mut seq = 0;
    for frame in frames {
        match &frame.records[..] {
            [Record::Changeset { id, .. }] => {
                let got = read.changeset(*id).expect("find").expect("in the snapshot");
                assert_eq!(got.seq, seq, "frame {}", frame.seq);
                seq += 1;
            }
            [Record::Compacted { .. }] => {}
            records => panic!("frame {} holds {records:?}", frame.seq),
        }
    }
    assert_eq!(seq, TOTAL, "a frame per transaction");
    for chain in 0..CHILDREN {
        for j in 0..PER_CHILD {
            let got = read
                .changeset(id(chain, j))
                .expect("find")
                .unwrap_or_else(|| panic!("chain {chain} lacks changeset {j}"));
            assert_eq!(
                (got.generation, got.delta_hash, got.parents),
                (j + 1, delta(chain, j), parents(chain, j)),
                "chain {chain}, changeset {j}"
            );
        }
    }
}

/// Times `f` [`SOLO`] times and returns its p50 and p99.
fn solo(mut f: impl FnMut(u64)) -> [Duration; 2] {
    let mut times: Vec<Duration> = (0..SOLO)
        .map(|j| {
            let begun = Instant::now();
            f(j);
            begun.elapsed()
        })
        .collect();
    let [p50, p99, _] = spread(&mut times);
    [p50, p99]
}

/// Prints, for scale, what `write` alone takes on `store` with no other writer: the lock, the
/// tail checks and replay, which every lock wait the children measured includes. It commits
/// nothing.
fn print_uncontended_write(store: &MetaStore) {
    let [p50, p99] = solo(|_| drop(store.write().expect("write")));
    println!("  uncontended write(): p50 {} p99 {}", ms(p50), ms(p99));
}

/// Prints, for scale, what a whole transaction takes on `store` with no other writer, adding a
/// chain of [`SOLO`] changesets.
fn print_uncontended_transaction(store: &MetaStore) {
    let [p50, p99] = solo(|j| {
        let mut write = store.write().expect("write");
        write
            .insert_changeset(changeset(CHILDREN, j))
            .expect("insert");
        write.commit().expect("commit");
    });
    println!("  uncontended transaction: p50 {} p99 {}", ms(p50), ms(p99));
}

#[test]
#[ignore = "measurement, 5-15 min (fsync-bound): 32 processes x 1,000 fsync'd write transactions; prints the table docs/perf.md cites"]
fn thirty_two_processes_thousand_transactions_each() {
    if let Some((work, number)) = child_env() {
        child(&work, number);
        return;
    }
    let work = tempfile::tempdir_in(base_dir()).expect("create the work dir");
    let root = store_root(work.path());
    fs::create_dir(&root).expect("create the store root");
    let store = MetaStore::create(&root).expect("create the store");

    let (lines, wall) = run_children(work.path());
    let busy = print_contention(work.path(), &lines, wall);
    let journal = JournalShape::read(&root);
    print_uncontended_write(&store);
    check_store(&store, &journal.frames);
    assert!(
        journal.past_ckpt < 32,
        "{} frames past the checkpoint",
        journal.past_ckpt
    );
    print_uncontended_transaction(&store);
    // Last, so that a run that meets `Busy` still checks the store and prints everything.
    assert_eq!(busy, 0, "Busy errors");
}
