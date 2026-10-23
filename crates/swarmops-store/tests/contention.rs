//! 32 writer processes on one native metadata store: how write transactions queue on
//! `meta/lock` (docs/perf.md, "M1 store level" and "M1 store fixes (7c)").
//!
//! Ignored, since it runs minutes of fsync'd transactions: `just check-slow` runs it, and
//! `cargo nextest run --release -p swarmops-store --run-ignored all --test all -E
//! 'test(/^contention::/)' --no-capture` prints its table, as docs/perf.md cites it.
//!
//! The test runs this test binary again as [`CHILDREN`] children (selected by [`WORK_DIR`])
//! on a store in a fresh temporary directory under `SWARMOPS_BENCH_DIR` (default
//! `~/.local/state/swarmops-bench`, on the native filesystem). Each child opens the store,
//! says it is ready, and waits until the parent, once all are ready, says go; then it runs
//! [`PER_CHILD`] write transactions back to back, as a writer that loops does, each inserting
//! the next changeset of a chain of its own and committing it, with the default 10 s lock
//! timeout: a `Busy` is counted and the transaction tried again. [`PAUSE`] puts a pause
//! between two transactions of a child, as an agent leaves at least a process start between
//! two commits. [`OBJECT_SYNCS`] has each child put and flush that many new objects of its
//! own before each transaction, outside the lock, as a CLI commit writes its objects before
//! its transaction takes the lock ([`Prelude`]); the parent reads it and hands it to every
//! child. For every transaction a child times
//!
//! - the transaction: from calling [`MetaStore::write`] until `commit` returns;
//! - the lock wait: until [`MetaStore::write`] returns, which is the wait for `meta/lock`
//!   plus the tail checks and replay under it; the uncontended `write`s the parent times once
//!   the children are done show what those cost alone.
//!
//! It prints one JSON line (its percentiles, `Busy` count and elapsed time) and writes its
//! samples to a file. The parent aggregates them and prints the table, with each
//! transaction's time in the lock once `write` returned (the transaction minus its lock
//! wait), and the uncontended `write`s' cost; then it checks the store as `verify` would
//! (every changeset there, each chain intact, one frame per transaction, and the seqs
//! contiguous in journal order) and that every child put the objects it was handed (its JSON
//! line gives their number, and `objects/` holds them all), prints what an uncontended
//! transaction costs, and asserts last that no child met `Busy`.
//!
//! Two tests run in the default gate, so that a run with [`OBJECT_SYNCS`] measures the object
//! syncs it says it adds: `object_syncs_prelude_writes_new_objects_and_flushes_them` checks the
//! prelude on a store whose filesystem calls are recorded (new objects each time, their fan-out
//! directories fsynced before `write` takes the lock, no call under it), and
//! `object_syncs_reach_every_child` runs the parent and its children as the measurement does,
//! at [`SMALL`]'s size, each child handed two objects to put before each transaction.
//!
//! The 0-`Busy` assertion assumes a disk no more than about a third slower than this host's:
//! with the kernel-queue lock a waiter can lose a few hundred releases in a row (docs/perf.md,
//! "M1 store fixes (7c)": up to 283–359 transactions, 5.2–8.9 s on a slow disk), and a third
//! more per transaction takes the longest wait past the 10 s timeout. `.config/nextest.toml`
//! runs this test and the CLI-level one (`swarmops-cli`'s `tests/contention.rs`) one after
//! the other, so that neither slows the other's.
//!
//! Nothing in it waits forever, whatever the lock does. A child gives up at its
//! [`MAX_BUSY_IN_A_ROW`]th `Busy` in a row, and past [`CHILD_DEADLINE`]; the parent fails the
//! test as soon as a child fails, and waits for the children only until [`PARENT_DEADLINE`]
//! ([`SMALL`]'s own in the default gate); and whatever ends the test early, [`Children`] kills
//! and reaps every child still running.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_core::bytes::to_u64;
use swarmops_core::{ChangesetId, ObjectId, ObjectKind, hash_object};
use swarmops_store::{
    ChangesetMeta, Frame, FsObjectStore, InsertOutcome, Journal, Layout, MetaStore, ObjectStore,
    RealFs, Record, StoreError,
};
use swarmops_testkit::child::test_args;
use swarmops_testkit::faults::checked;
use swarmops_testkit::fs::RecordingFs;
use swarmops_testkit::measure::{base_dir, mix, percentile};
use swarmops_testkit::store::new_root;
use swarmops_testkit::test_name;

/// How many writer processes the measurement runs.
const CHILDREN: u64 = 32;

/// How many write transactions each of the measurement's children runs.
const PER_CHILD: u64 = 1_000;

/// The measurement: [`CHILDREN`] children of [`PER_CHILD`] transactions each.
const MEASUREMENT: Shape = Shape {
    test: "thirty_two_processes_thousand_transactions_each",
    children: CHILDREN,
    per_child: PER_CHILD,
    deadline: PARENT_DEADLINE,
};

/// The measurement's parent and children in the default gate: 2 children of 3 transactions
/// each.
const SMALL: Shape = Shape {
    test: "object_syncs_reach_every_child",
    children: 2,
    per_child: 3,
    deadline: Duration::from_mins(2),
};

/// Set only in the children: the work directory, which holds the store (`store/`), the
/// barrier files (`ready-<n>`, `go`), the children's samples (`samples-<n>`) and their output
/// (`stdout-<n>`, `stderr-<n>`).
const WORK_DIR: &str = "SWARMOPS_TEST_CONTENTION_DIR";

/// Set with [`WORK_DIR`]: the child's number, from 0, which is also its chain's.
const CHILD_NUMBER: &str = "SWARMOPS_TEST_CONTENTION_CHILD";

/// Set to put a pause between two transactions of a child: its length in microseconds.
const PAUSE: &str = "SWARMOPS_TEST_CONTENTION_PAUSE_US";

/// Set to make a child put and flush that many new objects before each transaction, outside
/// the lock ([`Prelude`]): other writers' object syncs. The measurement reads it, and the
/// parent sets it for every child it starts.
const OBJECT_SYNCS: &str = "SWARMOPS_TEST_CONTENTION_OBJECT_SYNCS";

/// The length of each blob a [`Prelude`] puts: 4 KiB.
const BLOB_LEN: usize = 4_096;

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

/// A run of the parent and its children: the test the children are run as, which runs
/// [`child`] with this shape; how many children it starts, and how many write transactions each
/// runs; and how long after the go the parent waits for them before it kills them.
#[derive(Clone, Copy)]
struct Shape {
    test: &'static str,
    children: u64,
    per_child: u64,
    deadline: Duration,
}

impl Shape {
    /// Every child's transactions together.
    const fn total(self) -> u64 {
        self.children * self.per_child
    }
}

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
    ChangesetMeta::new(id(chain, j), 0, 0, delta(chain, j), parents(chain, j))
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

/// The new objects a child puts and flushes before each transaction: [`OBJECT_SYNCS`], or
/// none.
fn object_syncs() -> u64 {
    std::env::var(OBJECT_SYNCS).map_or(0, |objects| {
        objects
            .parse()
            .expect("the object syncs are a whole number of objects")
    })
}

/// Blob `k` of the prelude of transaction `j` of chain `chain`: [`BLOB_LEN`] bytes that start
/// with the three numbers, so that no other prelude's blob is the same, and go on with
/// SplitMix64's scattered output, which zstd cannot shrink, so that the object file holds the
/// whole 4 KiB.
fn blob(chain: u64, j: u64, k: u64) -> Vec<u8> {
    let mut blob = Vec::with_capacity(BLOB_LEN);
    for n in [chain, j, k] {
        blob.extend_from_slice(&n.to_le_bytes());
    }
    let seed = mix(mix(mix(chain) ^ j) ^ k);
    let mut word = 0;
    while blob.len() < BLOB_LEN {
        blob.extend_from_slice(&mix(seed.wrapping_add(word)).to_le_bytes());
        word += 1;
    }
    blob.truncate(BLOB_LEN);
    blob
}

/// What a child does before each transaction, outside the lock. With `blobs` > 0 (from
/// [`OBJECT_SYNCS`]) it puts that many new blobs ([`blob`]) into a [`FsObjectStore`] on the
/// store's own `objects/` and flushes it, as a CLI commit writes its objects before its write
/// transaction takes the lock: `put_batch` writes each to a temp file, `sync_all`s it and links
/// it into its fan-out directory, then `flush` fsyncs those directories, all at once on the
/// object store's pool. Those are the other writers' object syncs that the lock holder's two
/// journal syncs may queue behind on ext4's shared journal (docs/perf.md, "M1 — `just perf`
/// after all merges"). Nothing removes the objects. With 0 it does nothing, and the
/// transactions are M1's bare ones.
struct Prelude {
    /// The object store on the store's `objects/`.
    objects: FsObjectStore,
    /// How many blobs it puts before each transaction.
    blobs: u64,
}

impl Prelude {
    /// Puts the blobs of transaction `j` of chain `chain` and flushes them.
    fn run(&self, chain: u64, j: u64) {
        if self.blobs == 0 {
            return;
        }
        let batch = (0..self.blobs)
            .map(|k| (ObjectKind::Blob, blob(chain, j, k)))
            .collect();
        self.objects.put_batch(batch).expect("put the blobs");
        self.objects.flush().expect("flush the blobs");
    }
}

/// What one transaction took, as a child times it: see the module documentation.
struct Timed {
    /// From calling [`MetaStore::write`] until `commit` returned.
    transaction: Duration,
    /// Until [`MetaStore::write`] returned.
    wait: Duration,
    /// Whether the commit moved `journal.ckpt`.
    checkpointed: bool,
}

/// Transaction `j` of chain `chain` on `store`, as child `chain` runs it: first `prelude`,
/// outside the lock; then a write transaction that inserts changeset `j` of the chain and
/// commits, tried again after each `Busy` (counted in `busy`).
fn transaction(store: &MetaStore, prelude: &Prelude, chain: u64, j: u64, busy: &mut u64) -> Timed {
    prelude.run(chain, j);
    let meta = changeset(chain, j);
    let begun = Instant::now();
    let mut in_a_row = 0;
    let mut write = loop {
        match store.write() {
            Ok(write) => break write,
            Err(StoreError::Busy) => {
                *busy += 1;
                in_a_row += 1;
                assert!(
                    in_a_row < MAX_BUSY_IN_A_ROW,
                    "child {chain}, transaction {j}: Busy {in_a_row} times in a row"
                );
            }
            Err(err) => panic!("child {chain}, transaction {j}: {err}"),
        }
    };
    let wait = begun.elapsed();
    let outcome = write.insert_changeset(meta).expect("insert");
    assert!(
        matches!(outcome, InsertOutcome::Inserted { .. }),
        "child {chain}, transaction {j}: {outcome:?}"
    );
    let info = write.commit().expect("commit");
    Timed {
        transaction: begun.elapsed(),
        wait,
        checkpointed: info.checkpointed,
    }
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

/// A child: `per_child` write transactions on chain `number`, timed; see the module
/// documentation.
fn child(work: &Path, number: u64, per_child: u64) {
    let root = store_root(work);
    let store = MetaStore::open(&root).expect("open the store");
    let prelude = Prelude {
        objects: FsObjectStore::open(Layout::new(root), Box::new(RealFs::new())),
        blobs: object_syncs(),
    };
    fs::write(work.join(format!("ready-{number}")), b"").expect("say ready");
    wait_for(&work.join("go"));
    let pause = pause();
    let started = Instant::now();
    let deadline = started + CHILD_DEADLINE;
    let capacity = usize::try_from(per_child).expect("per_child fits in usize");
    let (mut transactions, mut waits) =
        (Vec::with_capacity(capacity), Vec::with_capacity(capacity));
    let (mut busy, mut checkpoints) = (0_u64, 0_u64);
    for j in 0..per_child {
        if j > 0 && !pause.is_zero() {
            thread::sleep(pause);
        }
        assert!(
            Instant::now() < deadline,
            "child {number}: transaction {j} not started {CHILD_DEADLINE:?} after the go"
        );
        let timed = transaction(&store, &prelude, number, j, &mut busy);
        transactions.push(timed.transaction);
        waits.push(timed.wait);
        checkpoints += u64::from(timed.checkpointed);
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
        "{SUMMARY}{number},\"transactions\":{per_child},\"pause_us\":{},\"object_syncs\":{},\
         \"busy\":{busy},\"checkpoints\":{checkpoints},\"elapsed_us\":{},\
         \"txn_p50_us\":{txn_p50},\"txn_p99_us\":{txn_p99},\"txn_max_us\":{txn_max},\
         \"lock_p50_us\":{lock_p50},\"lock_p99_us\":{lock_p99},\"lock_max_us\":{lock_max}}}",
        pause.as_micros(),
        prelude.blobs,
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

/// Spawns `shape`'s children on the store in `work`, each handed `objects` ([`OBJECT_SYNCS`])
/// and writing its stdout and stderr to files there.
fn spawn_children(work: &Path, shape: Shape, objects: u64) -> Children {
    let exe = std::env::current_exe().expect("this test binary");
    let mut children = Children(Vec::new());
    for number in 0..shape.children {
        let file = |stream| {
            File::create(output_file(work, stream, number)).expect("create a child's output file")
        };
        let child = Command::new(&exe)
            .args(test_args(&test_name!(shape.test)))
            .env(WORK_DIR, work)
            .env(CHILD_NUMBER, number.to_string())
            .env(OBJECT_SYNCS, objects.to_string())
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
    let count = to_u64(children.0.len());
    while !(0..count).all(ready) {
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
/// with its output, and so do children still running `deadline` after the go (`started`); the
/// guard then kills the rest.
fn wait_for_children(work: &Path, children: &mut Children, started: Instant, deadline: Duration) {
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
            started.elapsed() < deadline,
            "children {running:?} still running {deadline:?} after the go"
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
fn samples(work: &Path, children: u64) -> (Vec<Duration>, Vec<Duration>) {
    let (mut transactions, mut waits) = (Vec::new(), Vec::new());
    for number in 0..children {
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

/// Runs `shape`'s children on the store in `work`, each handed `objects`, lets them go together
/// once all are ready, and returns their summary lines and the time from the go until the last
/// one exited.
fn run_children(work: &Path, shape: Shape, objects: u64) -> (Vec<String>, Duration) {
    let mut children = spawn_children(work, shape, objects);
    wait_until_ready(work, &mut children);
    let started = Instant::now();
    fs::write(work.join("go"), b"").expect("say go");
    wait_for_children(work, &mut children, started, shape.deadline);
    let wall = started.elapsed();
    let lines = (0..shape.children)
        .map(|number| summary(work, number))
        .collect();
    (lines, wall)
}

/// Prints what the children measured, aggregated, and returns how many `Busy` errors they
/// met. Besides the transaction and the lock wait, it gives each transaction's time in the
/// lock once `write` returned (the transaction minus its lock wait): the insert and the
/// commit, the two journal syncs included, which is what [`OBJECT_SYNCS`] may slow. The children
/// are `shape`'s, each handed `objects`.
fn print_contention(
    work: &Path,
    shape: Shape,
    objects: u64,
    lines: &[String],
    wall: Duration,
) -> u64 {
    let busy = lines.iter().map(|line| field(line, "busy")).sum();
    let elapsed = |line: &String| Duration::from_micros(field(line, "elapsed_us"));
    let fastest = lines.iter().map(elapsed).min().expect("children");
    let slowest = lines.iter().map(elapsed).max().expect("children");
    let (mut transactions, mut waits) = samples(work, shape.children);
    assert_eq!(
        transactions.len(),
        usize::try_from(shape.total()).expect("fits")
    );
    // Paired sample by sample, before the sorts below.
    let mut in_lock: Vec<Duration> = transactions
        .iter()
        .zip(&waits)
        .map(|(transaction, wait)| transaction.saturating_sub(*wait))
        .collect();
    let [txn_p50, txn_p99, txn_max] = spread(&mut transactions);
    let [lock_p50, lock_p99, lock_max] = spread(&mut waits);
    let [held_p50, held_p99, held_max] = spread(&mut in_lock);
    let total = f64::from(u32::try_from(shape.total()).expect("the total fits in u32"));
    println!(
        "{} processes x {} write transactions of one changeset, {} us apart, \
         {objects} new objects put and flushed before each:",
        shape.children,
        shape.per_child,
        pause().as_micros(),
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
        "  in lock (transaction minus lock wait): p50 {} p99 {} max {}",
        ms(held_p50),
        ms(held_p99),
        ms(held_max)
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
        let journal = Journal::open(&layout, Arc::new(RealFs::new())).expect("open the journal");
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
/// chain intact. The children are `shape`'s.
fn check_store(store: &MetaStore, shape: Shape, frames: &[Frame]) {
    let read = store.read().expect("a snapshot");
    assert_eq!(read.count_changesets().expect("count"), shape.total());
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
    assert_eq!(seq, shape.total(), "a frame per transaction");
    for chain in 0..shape.children {
        for j in 0..shape.per_child {
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

/// Checks that every one of `shape`'s children put the `objects` new objects it was handed
/// before each of its transactions: its summary line gives that number, and the store at
/// `root` holds them all, `objects` for each transaction.
fn check_object_syncs(root: &Utf8Path, shape: Shape, objects: u64, lines: &[String]) {
    for (number, line) in lines.iter().enumerate() {
        assert_eq!(
            field(line, "object_syncs"),
            objects,
            "child {number} was handed {objects} objects a transaction: {line}"
        );
    }
    let stored = FsObjectStore::open(Layout::new(root), Box::new(RealFs::new()))
        .list()
        .expect("list the objects");
    assert_eq!(
        to_u64(stored.len()),
        shape.total() * objects,
        "the objects the children put"
    );
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
        child(&work, number, MEASUREMENT.per_child);
        return;
    }
    let objects = object_syncs();
    let work = tempfile::tempdir_in(base_dir()).expect("create the work dir");
    let root = store_root(work.path());
    fs::create_dir(&root).expect("create the store root");
    let store = MetaStore::create(&root).expect("create the store");

    let (lines, wall) = run_children(work.path(), MEASUREMENT, objects);
    let busy = print_contention(work.path(), MEASUREMENT, objects, &lines, wall);
    let journal = JournalShape::read(&root);
    print_uncontended_write(&store);
    check_store(&store, MEASUREMENT, &journal.frames);
    check_object_syncs(&root, MEASUREMENT, objects, &lines);
    assert!(
        journal.past_ckpt < 32,
        "{} frames past the checkpoint",
        journal.past_ckpt
    );
    print_uncontended_transaction(&store);
    // Last, so that a run that meets `Busy` still checks the store and prints everything.
    assert_eq!(busy, 0, "Busy errors");
}

#[test]
fn object_syncs_reach_every_child() {
    const OBJECTS: u64 = 2;
    if let Some((work, number)) = child_env() {
        child(&work, number, SMALL.per_child);
        return;
    }
    let work = tempfile::tempdir().expect("create the work dir");
    let root = store_root(work.path());
    fs::create_dir(&root).expect("create the store root");
    let store = MetaStore::create(&root).expect("create the store");

    let (lines, wall) = run_children(work.path(), SMALL, OBJECTS);
    print_contention(work.path(), SMALL, OBJECTS, &lines, wall);
    let journal = JournalShape::read(&root);
    check_store(&store, SMALL, &journal.frames);
    check_object_syncs(&root, SMALL, OBJECTS, &lines);
}

#[test]
fn object_syncs_prelude_writes_new_objects_and_flushes_them() {
    const BLOBS: u64 = 4;
    const TRANSACTIONS: u64 = 3;
    let (dir, layout) = new_root();
    let root = layout.root().to_owned();
    MetaStore::create(&root).expect("create the store");
    let (recorder, calls) = RecordingFs::new();
    let recorder = Arc::new(recorder.naming_paths_in(dir.path().to_path_buf()));
    // One filesystem for both stores, so that the order checker (in a build with the test
    // hooks) judges the objects' syncs and the frames' together.
    let shared = checked(Arc::clone(&recorder));
    let store = MetaStore::open_with_fs(&root, Arc::clone(&shared)).expect("open the store");
    let prelude = Prelude {
        objects: FsObjectStore::open(layout.clone(), Box::new(Arc::clone(&shared))),
        blobs: BLOBS,
    };
    calls();
    let (mut busy, mut temps) = (0, 0);
    for j in 0..TRANSACTIONS {
        transaction(&store, &prelude, 0, j, &mut busy);
        let made = calls();
        // `write` takes `meta/lock` (through std::fs) before its first call in `meta/`, and
        // `commit` lets it go once the last one returns.
        let start = made
            .iter()
            .position(|call| path(call).starts_with("meta/"))
            .unwrap_or_else(|| panic!("transaction {j} made no call in meta/: {made:?}"));
        let (before, locked) = made.split_at(start);
        assert!(
            before.iter().all(|call| in_objects(call)),
            "transaction {j}: calls before the lock other than the prelude's: {before:?}"
        );
        assert!(
            !locked.iter().any(|call| in_objects(call)),
            "transaction {j}: a prelude call under the lock: {locked:?}"
        );
        assert!(
            locked.contains(&"append_durable meta/journal.log".to_owned()),
            "transaction {j} wrote no frame: {locked:?}"
        );
        let count =
            |method: &str| to_u64(before.iter().filter(|call| name(call) == method).count());
        assert_eq!(
            count("write_durable_temp"),
            BLOBS,
            "transaction {j}: {before:?}"
        );
        assert_eq!(
            count("link_noclobber"),
            BLOBS,
            "transaction {j}: {before:?}"
        );
        temps += count("write_durable_temp");
        // Every fan-out directory an object was linked into is fsynced after the link, and
        // `objects/` after a fan-out directory is made in it, all before the lock.
        for (at, call) in before.iter().enumerate() {
            let synced = |dir: &str| before[at..].contains(&format!("fsync_dir {dir}"));
            match name(call) {
                "link_noclobber" => {
                    let fanout = path(call).rsplit_once('/').expect("a fan-out dir").0;
                    assert!(
                        synced(fanout),
                        "transaction {j}: {fanout} unsynced: {before:?}"
                    );
                }
                "create_dir_all" if path(call) != "objects/tmp" => {
                    assert!(
                        synced("objects"),
                        "transaction {j}: objects unsynced: {before:?}"
                    );
                }
                _ => {}
            }
        }
    }
    assert_eq!((busy, temps), (0, TRANSACTIONS * BLOBS));
    let expected: BTreeSet<ObjectId> = (0..TRANSACTIONS)
        .flat_map(|j| (0..BLOBS).map(move |k| hash_object(ObjectKind::Blob, &blob(0, j, k))))
        .collect();
    assert_eq!(
        to_u64(expected.len()),
        TRANSACTIONS * BLOBS,
        "every blob is new"
    );
    let listed: BTreeSet<ObjectId> = prelude.objects.list().expect("list").into_iter().collect();
    assert_eq!(listed, expected);
    for id in &listed {
        let stored = fs::metadata(layout.object_path(*id))
            .expect("an object file")
            .len();
        assert!(
            stored > to_u64(BLOB_LEN),
            "{id:?} is stored whole, uncompressed"
        );
    }
    let journal = Journal::open(&layout, Arc::new(RealFs::new())).expect("open the journal");
    let frames = journal
        .frames_from(0, usize::MAX)
        .expect("read the journal");
    assert_eq!(
        to_u64(frames.len()),
        TRANSACTIONS,
        "a frame per transaction"
    );
}

/// The method a call recorded as `"<method> <path>"` is of.
fn name(call: &str) -> &str {
    call.split_once(' ').map_or(call, |(method, _)| method)
}

/// The path, in the store root, that a call recorded as `"<method> <path>"` acts on.
fn path(call: &str) -> &str {
    call.split_once(' ').map_or("", |(_, path)| path)
}

/// Whether a call recorded as `"<method> <path>"` acts on `objects/` or a path in it.
fn in_objects(call: &str) -> bool {
    let path = path(call);
    path == "objects" || path.starts_with("objects/")
}
