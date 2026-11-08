//! The literal SIGKILL test (the M6 design, §9 crash tests): the binary commits on a store in a
//! temp dir on the real disk, and a real `kill -9` (`std::process::Child::kill`) ends it at a
//! random point. `crash.rs` aborts a run at a chosen filesystem call; this test picks no call,
//! only a moment, so it also covers what no hook marks (process start, parsing, output).
//!
//! ```text
//!   spawn `commit --advance main` ──┐
//!   wait a seeded delay < window    │  window: how long the last whole commit took here
//!   SIGKILL (testkit deadline) ─────┘
//!   verify --deep clean · every commit whose envelope was read is in `log` · next commit ok,
//!   timed: the next window
//! ```
//!
//! A kill counts wherever it hits, before the run wrote anything included.
//!
//! The land variant (the M15 design §6) kills `land --key` instead and runs the same land again
//! with the same key: the key lands its change exactly once, whatever moment the kill hit.
//!
//! The revert variant (the M18 design §4.3) kills `revert --key` of a change just made on main
//! and runs it again: the change is taken back exactly once, whatever moment the kill hit.
//!
//! The group variant (the M18 design §6.3) kills `land-group` of two changes and runs it again:
//! main holds each group's whole chain, never part of one, whatever moment the kill hit.
//!
//! The ref variant (the M16 design §2) kills `ref set` and `op undo` on `release/1`: after each
//! kill the ref names its old or its new target, the op log holds every operation once and in
//! order, and the same operation run again is recorded once.
//!
//! The batch variant (the M18 D2 design §5.1) lands eight changes at once, queued behind the
//! lock and then batched, and kills one of the eight landers, leader or waiter, at a random
//! moment of the batch; every key lands once after the lands run again.
//!
//! Two reader processes run beside every loop ([`Readers`]): `ref list`, then `show` and `cat`
//! of what each ref names. What a ref they read reaches is never missing (`E_REV_NOT_FOUND`,
//! `E_PATH_NOT_FOUND`, `E_STORE_CORRUPT`); the documented `E_IO` after a cut is counted apart.
//!
//! Each test owns its temp dir; the tests share nothing.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::Output;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::Value;
use swarmops_core::ObjectId;
use swarmops_store::{DEFAULT_INIT_FORMAT, LEAD_WAIT_ENV, MetaLock, StoreFormat};
use swarmops_testkit::child::Child;
use swarmops_testkit::per_format;
use swarmops_testkit::ws::Rng;

use crate::support::{Run, Store, agent, swarmops};

/// The signal `Child::kill` sends on Unix.
const SIGKILL: i32 = 9;

/// The kills the fast variant makes per format (`just check`).
const FAST_KILLS: usize = 10;

/// The kills the slow variant makes per format.
const SLOW_KILLS: usize = 500;

/// How many runs per kill a test may spend: a run that ends before its delay is no kill.
const RUNS_PER_KILL: usize = 4;

/// The seed of every test's delays; a failure prints it with the run's index.
const SEED: u64 = 0x5167_4b11;

/// The ref every commit advances.
const MAIN: &str = "main";

/// The ref the land test's last step lands onto.
const RELEASE: &str = "release/1";

/// How many reader processes run beside every kill loop.
const READERS: usize = 2;

/// How many violations a [`Tally`] keeps the message of.
const KEPT: usize = 5;

/// What readers counted, over all their passes.
#[derive(Debug, Default)]
struct Tally {
    /// Reads that succeeded.
    reads: usize,
    /// Reads that met the documented `E_IO` after a writer's cut (storage spec §3).
    io_after_cut: usize,
    /// Reads whose snapshot gave up on writers landing meanwhile (`E_BUSY`, retryable): neither
    /// a read nor a violation.
    busy: usize,
    /// Reads that failed otherwise: what a ref reached was missing, or any other error.
    violations: usize,
    /// The first [`KEPT`] violations.
    seen: Vec<String>,
}

impl Tally {
    /// Adds `other`'s counts to these.
    fn add(&mut self, other: Tally) {
        self.reads += other.reads;
        self.io_after_cut += other.io_after_cut;
        self.busy += other.busy;
        self.violations += other.violations;
        let room = KEPT.saturating_sub(self.seen.len());
        self.seen.extend(other.seen.into_iter().take(room));
    }

    /// Counts `run`; the result of a read that succeeded.
    fn judge(&mut self, run: &Run) -> Option<Value> {
        // 1. A read that succeeded.
        let envelope: Option<Value> = serde_json::from_str(run.stdout.trim()).ok();
        if run.status.success()
            && let Some(mut envelope) = envelope
        {
            self.reads += 1;
            return Some(envelope["result"].take());
        }

        // 2. The allowed failures, then every other one.
        let code = envelope.as_ref().and_then(|e| e["error"]["code"].as_str());
        match code {
            Some("E_IO") => self.io_after_cut += 1,
            Some("E_BUSY") => self.busy += 1,
            _ => {
                self.violations += 1;
                if self.seen.len() < KEPT {
                    self.seen.push(run.to_string());
                }
            }
        }
        None
    }
}

/// Reader processes beside a kill loop, until [`Readers::stop`].
struct Readers {
    /// Set to end the readers' loops.
    done: Arc<AtomicBool>,
    /// The reader threads, each spawning its reads one at a time.
    threads: Vec<JoinHandle<Tally>>,
}

impl Readers {
    /// Starts [`READERS`] readers on `store`, each making passes ([`Readers::pass`]) until
    /// stopped, one pass at least.
    fn start(store: &Store) -> Readers {
        let done = Arc::new(AtomicBool::new(false));
        let threads = (0..READERS)
            .map(|_| {
                let (path, done) = (store.path().to_owned(), Arc::clone(&done));
                thread::spawn(move || {
                    let mut tally = Tally::default();
                    loop {
                        tally.add(Readers::pass(&path));
                        if done.load(Ordering::Relaxed) {
                            return tally;
                        }
                    }
                })
            })
            .collect();
        Readers { done, threads }
    }

    /// One pass over the store at `path`: `ref list`, then `show` of each target it read and
    /// `cat` of the first file that changeset puts.
    fn pass(path: &str) -> Tally {
        let mut tally = Tally::default();
        let read = |args: &[&str]| crate::support::run(&[&["--repo", path], args].concat(), &[]);

        // 1. The refs; a ref read as absent is no read of it.
        let Some(listed) = tally.judge(&read(&["ref", "list"])) else {
            return tally;
        };
        let refs = listed["refs"].as_array().cloned().unwrap_or_default();
        let targets: BTreeSet<String> = (refs.iter())
            .filter_map(|entry| entry["target"].as_str().map(str::to_owned))
            .collect();

        // 2. What each target reaches: the changeset, and a file it puts.
        for target in &targets {
            let Some(shown) = tally.judge(&read(&["show", target])) else {
                continue;
            };
            let changes = shown["changes"].as_array().cloned().unwrap_or_default();
            let put = (changes.iter())
                .find(|change| change["op"] == "put")
                .and_then(|change| change["path"].as_str());
            if let Some(file) = put {
                tally.judge(&read(&["cat", &format!("{target}:{file}")]));
            }
        }
        tally
    }

    /// Ends the readers after their current pass; what they counted.
    fn stop(mut self) -> Tally {
        self.done.store(true, Ordering::Relaxed);
        let mut tally = Tally::default();
        for thread in std::mem::take(&mut self.threads) {
            let counted = thread
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
            tally.add(counted);
        }
        tally
    }
}

impl Drop for Readers {
    /// A test that failed before [`Readers::stop`] still ends and reaps its readers.
    fn drop(&mut self) {
        self.done.store(true, Ordering::Relaxed);
        for thread in std::mem::take(&mut self.threads) {
            let _ = thread.join();
        }
    }
}

/// Stops `readers` and asserts they met no violation; `what` names the loop.
fn assert_read_clean(readers: Readers, what: &str) {
    let tally = readers.stop();
    println!("{what}: readers {tally:?}");
    assert_eq!(tally.violations, 0, "{what}: {:#?}", tally.seen);
}

/// How one killed run ended.
enum Ended {
    /// SIGKILL hit it; its envelope was read if it printed one whole.
    Killed(Option<String>),
    /// It exited before the delay, with this changeset.
    Exited(String),
}

/// The changeset of `stdout` if it holds one whole, successful envelope.
fn committed(stdout: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(stdout).ok()?;
    if !text.ends_with('\n') {
        return None;
    }

    let envelope: Value = serde_json::from_str(text).ok()?;
    assert_eq!(envelope["ok"], true, "a commit's envelope: {text}");
    let id = envelope["result"]["changeset"].as_str();
    Some(id.expect("a changeset id").to_owned())
}

/// The arguments of the `n`-th commit on `store`: one new file, `main` advanced.
fn commit_args(store: &Store, n: usize) -> Vec<String> {
    let name = format!("kill-{n}");
    let changes = store.changes(&name, &format!("{name}.txt"), &format!("{n}\n"));
    let args = [
        "commit",
        "-m",
        &name,
        "--changes",
        &changes,
        "--advance",
        MAIN,
    ];
    store.args(&agent(0), &args)
}

/// Runs the `n`-th commit and SIGKILLs it `delay` after its spawn, unless it ended before.
fn kill_after(store: &Store, n: usize, delay: Duration) -> Ended {
    let mut child = Child::spawn(&mut swarmops(&commit_args(store, n), &[]));
    let (output, _) = child.finish(Instant::now() + delay);
    let Output { status, stdout, .. } = &output;

    // 1. A signal ends only a killed run; the kill may land after the envelope was printed.
    if status.signal() == Some(SIGKILL) {
        return Ended::Killed(committed(stdout));
    }

    // 2. A run that beat its delay must have committed.
    assert!(
        status.success(),
        "run {n} ended {status} unkilled: {output:?}"
    );
    let id = committed(stdout).unwrap_or_else(|| panic!("run {n}: no envelope: {output:?}"));
    Ended::Exited(id)
}

/// The ids `log` lists from `main`, all of them.
fn logged(store: &Store, at_least: usize) -> BTreeSet<String> {
    let limit = (at_least + 1).to_string();
    let log = store.read(&["log", MAIN, "-n", &limit]).ok();
    let entries = log["entries"].as_array().expect("log entries");
    assert!(log.get("cursor").is_none(), "one page holds the log: {log}");
    entries
        .iter()
        .map(|entry| entry["id"].as_str().expect("an id").to_owned())
        .collect()
}

/// Commits on `main` to its end: the changeset, and the commit's wall time in microseconds, the
/// window of the next kill's delay.
fn commit(store: &Store, n: usize) -> (String, u64) {
    let args = commit_args(store, n);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let start = Instant::now();
    let result = crate::support::run(&args, &[]).ok();
    let window = u64::try_from(start.elapsed().as_micros()).expect("a short commit");
    let id = result["changeset"].as_str().expect("a changeset id");
    (id.to_owned(), window)
}

/// `kills` SIGKILLs of `commit --advance` on a store of `format`, each followed by the checks.
fn kill_commits(format: StoreFormat, kills: usize) {
    let store = Store::init_as(format);
    let mut read = BTreeSet::new();
    let mut rng = Rng::new(SEED ^ u64::from(format.version()));

    // 1. The window: one whole commit's wall time here, so most delays land inside a run. Each
    //    round's last commit times it again, so it follows the host's load as it rises or falls.
    let (first, mut window) = commit(&store, 0);
    read.insert(first);
    let readers = Readers::start(&store);

    let mut killed = 0;
    let mut runs = 0;
    while killed < kills {
        runs += 1;
        assert!(
            runs <= kills * RUNS_PER_KILL,
            "{killed} kills in {runs} runs"
        );
        let n = 2 * runs;
        let delay = Duration::from_micros(rng.next_u64() % window);
        let what = format!("{format:?} seed {SEED:#x} run {runs} delay {delay:?}");

        // 2. Kill a commit; keep the id of every envelope read.
        match kill_after(&store, n - 1, delay) {
            Ended::Killed(id) => {
                killed += 1;
                read.extend(id);
            }
            Ended::Exited(id) => {
                read.insert(id);
            }
        }

        // 3. The store verifies clean and lists every commit an envelope reported.
        store.assert_clean(&what);
        let log = logged(&store, 2 * runs + 1);
        let lost: Vec<&String> = read.difference(&log).collect();
        assert!(lost.is_empty(), "{what}: reported, not in log: {lost:?}");

        // 4. The next commit succeeds; its wall time is the next window.
        let (id, took) = commit(&store, n);
        read.insert(id);
        window = took;
    }

    println!("{format:?}: {killed} kills in {runs} runs, last window {window} us");
    assert_read_clean(readers, &format!("{format:?} commits"));
    store.assert_clean("after the last commit");
}

per_format! {
    /// The fast variant: [`FAST_KILLS`] kills per format.
    fn sigkill_mid_commit_keeps_the_store(format) {
        kill_commits(format, FAST_KILLS);
    }
}

per_format! {
    /// The slow variant: [`SLOW_KILLS`] kills per format.
    #[ignore = "slow, ~600 s: 500 kills per format, each with verify --deep and log; \
                218-310 s idle, 577-599 s in a loaded check-slow"]
    fn sigkill_mid_commit_500_times(format) {
        kill_commits(format, SLOW_KILLS);
    }
}

/// The kills the fast land variant makes per format (`just check`).
const FAST_LAND_KILLS: usize = 8;

/// The kills the slow land variant makes per format.
const SLOW_LAND_KILLS: usize = 200;

/// The seed of the land test's delays, apart from the commit test's.
const LAND_SEED: u64 = 0x1a4d_5167;

/// How many lands per kill the land test may spend. A land that ends before its delay is no kill,
/// and shrinks the window the delays are drawn from ([`WINDOW_SHRINK`]), so a loaded host that
/// slows the lands after the first still ends in the kills asked for.
const LAND_RUNS_PER_KILL: usize = 16;

/// A land that ended before its delay cuts the window by this fraction (1 in 8).
const WINDOW_SHRINK: u64 = 8;

/// The entries `log` is asked for beyond the `n + 2` `main` holds after land `n`: a surplus
/// changeset shows in the log, and fails the count.
const LOG_SURPLUS: usize = 2;

/// The idempotency key of the `n`-th land.
fn land_key(n: usize) -> String {
    format!("kill-land-{n}")
}

/// The changeset a land's `stdout` reports, if it holds one whole, successful envelope.
fn landed(stdout: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(stdout).ok()?;
    if !text.ends_with('\n') {
        return None;
    }

    let envelope: Value = serde_json::from_str(text).ok()?;
    assert_eq!(envelope["ok"], true, "a land's envelope: {text}");
    let id = envelope["result"]["landed"].as_str();
    Some(id.expect("a landed changeset").to_owned())
}

/// Commits a new file off `main` without moving it; the changeset to land.
fn change_off_main(store: &Store, n: usize) -> String {
    let path = format!("land-{n}.txt");
    store.commit(&agent(0), &path, &format!("{n}\n"), None)
}

/// The arguments of the land of `head` under the `n`-th key.
fn land_args(store: &Store, head: &str, n: usize) -> Vec<String> {
    let key = land_key(n);
    store.args(&agent(0), &["land", head, "--key", &key])
}

/// Runs the land of `head` under the `n`-th key and SIGKILLs it `delay` after its spawn, unless it
/// ended before.
fn kill_land_after(store: &Store, head: &str, n: usize, delay: Duration) -> Ended {
    let mut child = Child::spawn(&mut swarmops(&land_args(store, head, n), &[]));
    let (output, _) = child.finish(Instant::now() + delay);
    let Output { status, stdout, .. } = &output;

    // 1. A signal ends only a killed run; the kill may land after the envelope was printed.
    if status.signal() == Some(SIGKILL) {
        return Ended::Killed(landed(stdout));
    }

    // 2. A run that beat its delay must have landed.
    assert!(
        status.success(),
        "land {n} ended {status} unkilled: {output:?}"
    );
    let id = landed(stdout).unwrap_or_else(|| panic!("land {n}: no envelope: {output:?}"));
    Ended::Exited(id)
}

/// What `main` names: the first entry of its log.
fn main_head(store: &Store) -> String {
    let log = store.read(&["log", MAIN, "-n", "1"]).ok();
    log["entries"][0]["id"].as_str().expect("an id").to_owned()
}

/// The land key of changeset `id`, as `show` reports it; `None` for a changeset no keyed land
/// made.
fn key_of(store: &Store, id: &str) -> Option<String> {
    let shown = store.show(id);
    shown["land_key"].as_str().map(str::to_owned)
}

/// Lands the `n`-th change again under its key, as an agent retries a land that died: the
/// changeset, which a land made now or a replay found.
fn land_again(store: &Store, head: &str, n: usize) -> String {
    let args = land_args(store, head, n);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let landed = crate::support::run(&args, &[]).ok();
    let status = landed["status"].as_str().expect("a land's status");
    assert!(
        matches!(status, "landed" | "replayed"),
        "land {n}: {landed}"
    );
    landed["landed"]
        .as_str()
        .expect("a landed changeset")
        .to_owned()
}

/// `kills` SIGKILLs of `land --key` on a store of `format`, each followed by the same land again
/// and the checks: the store verifies clean, `main`'s log holds each key's changeset exactly once
/// (and the base), and a reported land is the one `main` holds. Ends with the first key given for
/// another change, then for a land onto another target: `E_INVALID_ARG` each, no ref moved. Every
/// SIGKILL counts, one before the land wrote anything included (the module documentation).
fn kill_lands(format: StoreFormat, kills: usize) {
    let store = Store::init_as(format);
    let mut rng = Rng::new(LAND_SEED ^ u64::from(format.version()));
    let mut keys: BTreeMap<String, Option<String>> = BTreeMap::new();

    // 1. The base, and the first land, whose wall time is the window: most delays land inside a
    // run.
    let base = store.commit(&agent(0), "base.txt", "base\n", None);
    let create = ["ref", "set", MAIN, &base, "--create"];
    store.run_as(&agent(0), &create, &[]).ok();
    let first = change_off_main(&store, 0);
    let start = Instant::now();
    let landed = land_again(&store, &first, 0);
    let mut window = u64::try_from(start.elapsed().as_micros()).expect("a short land");
    keys.insert(landed, Some(land_key(0)));
    keys.insert(base.clone(), None);
    let readers = Readers::start(&store);

    let mut killed = 0;
    let mut n = 0;
    while killed < kills {
        n += 1;
        assert!(
            n <= kills * LAND_RUNS_PER_KILL,
            "{killed} kills in {n} lands"
        );
        let head = change_off_main(&store, n);
        let delay = Duration::from_micros(rng.next_u64() % window);
        let what = format!("{format:?} seed {LAND_SEED:#x} land {n} delay {delay:?}");

        // 2. Kill a land, then land it again under the same key.
        let reported = match kill_land_after(&store, &head, n, delay) {
            Ended::Killed(reported) => {
                killed += 1;
                reported
            }
            Ended::Exited(id) => {
                window -= window / WINDOW_SHRINK;
                Some(id)
            }
        };
        let again = land_again(&store, &head, n);
        if let Some(reported) = reported {
            assert_eq!(reported, again, "{what}: the reported land is the one kept");
        }

        // 3. The store verifies clean; main holds the base and one land per key so far.
        store.assert_clean(&what);
        let log = store
            .read(&["log", MAIN, "-n", &(n + 2 + LOG_SURPLUS).to_string()])
            .ok();
        let on_main: Vec<&str> = (log["entries"].as_array().expect("log entries").iter())
            .map(|entry| entry["id"].as_str().expect("an id"))
            .collect();
        assert_eq!(on_main.len(), n + 2, "{what}: main's log: {on_main:?}");
        assert!(
            on_main.contains(&again.as_str()),
            "{what}: {again} not on main"
        );
        for id in on_main {
            keys.entry(id.to_owned())
                .or_insert_with(|| key_of(&store, id));
        }
        let lands = keys
            .values()
            .flatten()
            .filter(|&k| *k == land_key(n))
            .count();
        assert_eq!(lands, 1, "{what}: changesets with key {n}");
    }

    // 4. A key given for another change is refused, and writes nothing. The change starts at the
    // base, so that the land's walk reaches the first key's changeset, which is a key's bound.
    let before = main_head(&store);
    let other = store.commit(&agent(0), "other.txt", "other\n", Some(&base));
    let args = ["land", other.as_str(), "--key", &land_key(0)];
    let refused = store.run_as(&agent(0), &args, &[]).err("E_INVALID_ARG");
    assert_eq!(refused["details"]["key"], land_key(0).as_str());
    assert_eq!(refused["details"]["target"], MAIN);
    assert_eq!(main_head(&store), before, "main did not move");

    // 5. The same key for a land onto another target, `release/1` at the base: refused, naming
    // `main`, and neither ref moves.
    store
        .run_as(&agent(0), &["ref", "set", RELEASE, &base, "--create"], &[])
        .ok();
    let refs = || store.read(&["ref", "list"]).ok();
    let before = refs();
    let args = ["land", other.as_str(), "--key", &land_key(0)];
    let args: Vec<&str> = args.into_iter().chain(["--target", RELEASE]).collect();
    let refused = store.run_as(&agent(0), &args, &[]).err("E_INVALID_ARG");
    assert_eq!(refused["details"]["target"], MAIN);
    assert_eq!(refs(), before, "no ref moved");

    println!("{format:?}: {killed} kills in {n} lands, window {window} us");
    assert_read_clean(readers, &format!("{format:?} lands"));
    store.assert_clean("after the last land");
}

per_format! {
    /// The fast land variant: [`FAST_LAND_KILLS`] kills per format.
    fn sigkill_mid_land_keeps_one_land_per_key(format) {
        kill_lands(format, FAST_LAND_KILLS);
    }
}

per_format! {
    /// The slow land variant: [`SLOW_LAND_KILLS`] kills per format.
    #[ignore = "slow, ~115 s: 200 kills per format, each with verify --deep and a log; \
                ~30 s idle, 112-113 s loaded"]
    fn sigkill_mid_land_200_times(format) {
        kill_lands(format, SLOW_LAND_KILLS);
    }
}

/// The kills the slow group variant makes per format.
const SLOW_GROUP_KILLS: usize = 100;

/// The seed of the group test's delays, apart from the other tests'.
const GROUP_SEED: u64 = 0x6a0d_5167;

/// The changes each group of the group test lands.
const GROUP_SIZE: usize = 2;

/// The heads of the `n`-th group: [`GROUP_SIZE`] new files off `main`.
fn group_heads(store: &Store, n: usize) -> Vec<String> {
    (0..GROUP_SIZE)
        .map(|at| {
            let path = format!("group-{n}-{at}.txt");
            store.commit(&agent(0), &path, &format!("{n} {at}\n"), None)
        })
        .collect()
}

/// The arguments of the group land of `heads`.
fn group_args(store: &Store, heads: &[String]) -> Vec<String> {
    let mut args = vec!["land-group"];
    args.extend(heads.iter().map(String::as_str));
    store.args(&agent(0), &args)
}

/// The head a group land's `stdout` reports, if it holds one whole, successful envelope.
fn grouped(stdout: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(stdout).ok()?;
    if !text.ends_with('\n') {
        return None;
    }

    let envelope: Value = serde_json::from_str(text).ok()?;
    assert_eq!(envelope["ok"], true, "a group land's envelope: {text}");
    let id = envelope["result"]["head"].as_str();
    Some(id.expect("a group's head").to_owned())
}

/// Runs the group land of `heads` and SIGKILLs it `delay` after its spawn, unless it ended
/// before.
fn kill_group_after(store: &Store, heads: &[String], n: usize, delay: Duration) -> Ended {
    let mut child = Child::spawn(&mut swarmops(&group_args(store, heads), &[]));
    let (output, _) = child.finish(Instant::now() + delay);
    let Output { status, stdout, .. } = &output;

    // 1. A signal ends only a killed run; the kill may land after the envelope was printed.
    if status.signal() == Some(SIGKILL) {
        return Ended::Killed(grouped(stdout));
    }

    // 2. A run that beat its delay must have landed.
    assert!(
        status.success(),
        "group {n} ended {status} unkilled: {output:?}"
    );
    let id = grouped(stdout).unwrap_or_else(|| panic!("group {n}: no envelope: {output:?}"));
    Ended::Exited(id)
}

/// Lands the group of `heads` again, as an agent retries one that died: the chain's head, which
/// the land made now, or `main`'s when the group had landed and nothing is left to land.
fn group_again(store: &Store, heads: &[String]) -> String {
    let args = group_args(store, heads);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let run = crate::support::run(&args, &[]);
    if run.status.success() {
        return run.ok()["head"]
            .as_str()
            .expect("a group's head")
            .to_owned();
    }
    run.err("E_NOTHING_TO_LAND");
    main_head(store)
}

/// `kills` SIGKILLs of `land-group` on a store of `format`, each followed by the same group again
/// and the checks: the store verifies clean, `main`'s log holds the base and every group's whole
/// chain, never part of one, and a reported group is the one `main` holds.
fn kill_groups(format: StoreFormat, kills: usize) {
    let store = Store::init_as(format);
    let mut rng = Rng::new(GROUP_SEED ^ u64::from(format.version()));

    // 1. The base, and the first group, whose wall time is the window.
    let base = store.commit(&agent(0), "base.txt", "base\n", None);
    let create = ["ref", "set", MAIN, &base, "--create"];
    store.run_as(&agent(0), &create, &[]).ok();
    let first = group_heads(&store, 0);
    let start = Instant::now();
    group_again(&store, &first);
    let mut window = u64::try_from(start.elapsed().as_micros()).expect("a short group land");
    let readers = Readers::start(&store);

    let mut killed = 0;
    let mut n = 0;
    while killed < kills {
        n += 1;
        assert!(
            n <= kills * LAND_RUNS_PER_KILL,
            "{killed} kills in {n} groups"
        );
        let heads = group_heads(&store, n);
        let delay = Duration::from_micros(rng.next_u64() % window);
        let what = format!("{format:?} seed {GROUP_SEED:#x} group {n} delay {delay:?}");

        // 2. Kill a group land, then land the group again.
        let reported = match kill_group_after(&store, &heads, n, delay) {
            Ended::Killed(reported) => {
                killed += 1;
                reported
            }
            Ended::Exited(id) => {
                window -= window / WINDOW_SHRINK;
                Some(id)
            }
        };
        let again = group_again(&store, &heads);
        if let Some(reported) = reported {
            assert_eq!(
                reported, again,
                "{what}: the reported group is the one kept"
            );
        }

        // 3. The store verifies clean; main holds the base and every group's whole chain.
        store.assert_clean(&what);
        let want = 1 + (n + 1) * GROUP_SIZE;
        let log = store
            .read(&["log", MAIN, "-n", &(want + LOG_SURPLUS).to_string()])
            .ok();
        let on_main = log["entries"].as_array().expect("log entries");
        assert_eq!(on_main.len(), want, "{what}: main's log: {on_main:?}");
        assert_eq!(
            on_main[0]["id"],
            again.as_str(),
            "{what}: main is the group's head"
        );
    }

    println!("{format:?}: {killed} kills in {n} groups, window {window} us");
    assert_read_clean(readers, &format!("{format:?} groups"));
    store.assert_clean("after the last group");
}

per_format! {
    /// The slow group variant (the M18 design §6.3): [`SLOW_GROUP_KILLS`] kills per format.
    #[ignore = "slow, ~40 s: 100 kills per format, each with verify --deep and a log; \
                37 s at load 6-10"]
    fn sigkill_mid_group_land_100_times(format) {
        kill_groups(format, SLOW_GROUP_KILLS);
    }
}

/// The kills the slow revert variant makes per format.
const SLOW_REVERT_KILLS: usize = 200;

/// The seed of the revert test's delays, apart from the other tests'.
const REVERT_SEED: u64 = 0x2e7e_2711;

/// The idempotency key of the `n`-th revert.
fn revert_key(n: usize) -> String {
    format!("kill-revert-{n}")
}

/// Commits a new file on main's head and moves main to it: the changeset to revert.
fn change_on_main(store: &Store, n: usize) -> String {
    let path = format!("revert-{n}.txt");
    let changes = store.changes(&path, &path, &format!("{n}\n"));
    let args = [
        "commit",
        "-m",
        &path,
        "--changes",
        &changes,
        "--advance",
        MAIN,
    ];
    let result = store.run_as(&agent(0), &args, &[]).ok();
    result["changeset"]
        .as_str()
        .expect("a changeset")
        .to_owned()
}

/// The arguments of the revert of `changeset` under the `n`-th key.
fn revert_args(store: &Store, changeset: &str, n: usize) -> Vec<String> {
    let key = revert_key(n);
    let args = ["revert", changeset, "--reason", "killed", "--key", &key];
    store.args(&agent(0), &args)
}

/// The changeset a revert's `stdout` reports landed, if it holds one whole, successful envelope.
fn reverted(stdout: &[u8]) -> Option<String> {
    let result = result_of(stdout)?;
    let id = result["land"]["landed"].as_str();
    Some(id.expect("a landed changeset").to_owned())
}

/// Runs the revert of `changeset` under the `n`-th key and SIGKILLs it `delay` after its spawn,
/// unless it ended before.
fn kill_revert_after(store: &Store, changeset: &str, n: usize, delay: Duration) -> Ended {
    let mut child = Child::spawn(&mut swarmops(&revert_args(store, changeset, n), &[]));
    let (output, _) = child.finish(Instant::now() + delay);
    let Output { status, stdout, .. } = &output;

    // 1. A signal ends only a killed run; the kill may land after the envelope was printed.
    if status.signal() == Some(SIGKILL) {
        return Ended::Killed(reverted(stdout));
    }

    // 2. A run that beat its delay must have landed.
    assert!(
        status.success(),
        "revert {n} ended {status} unkilled: {output:?}"
    );
    let id = reverted(stdout).unwrap_or_else(|| panic!("revert {n}: no envelope: {output:?}"));
    Ended::Exited(id)
}

/// Reverts the `n`-th change again under its key, as an agent retries a revert that died: the
/// landed changeset, which the revert made now or a replay found.
fn revert_again(store: &Store, changeset: &str, n: usize) -> String {
    let args = revert_args(store, changeset, n);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = crate::support::run(&args, &[]).ok();
    let status = result["land"]["status"].as_str().expect("a land's status");
    assert!(
        matches!(status, "landed" | "replayed"),
        "revert {n}: {result}"
    );
    result["land"]["landed"]
        .as_str()
        .expect("a landed changeset")
        .to_owned()
}

/// `kills` SIGKILLs of `revert --key` on a store of `format` (the M18 design §4.3), each of a
/// change made on main just before, then the same revert again and the checks: the store
/// verifies clean, main's log holds the base and, per round, the change and one revert of it,
/// the reported revert is the one main holds, the key is on one changeset, and main no longer
/// holds the change's file. Every SIGKILL counts (the module documentation).
fn kill_reverts(format: StoreFormat, kills: usize) {
    let store = Store::init_as(format);
    let mut rng = Rng::new(REVERT_SEED ^ u64::from(format.version()));
    let mut keys: BTreeMap<String, Option<String>> = BTreeMap::new();

    // 1. The base, and the first revert, whose wall time is the window.
    let base = store.commit(&agent(0), "base.txt", "base\n", None);
    let create = ["ref", "set", MAIN, &base, "--create"];
    store.run_as(&agent(0), &create, &[]).ok();
    let first = change_on_main(&store, 0);
    let start = Instant::now();
    revert_again(&store, &first, 0);
    let mut window = u64::try_from(start.elapsed().as_micros()).expect("a short revert");
    let readers = Readers::start(&store);

    let mut killed = 0;
    let mut n = 0;
    while killed < kills {
        n += 1;
        assert!(
            n <= kills * LAND_RUNS_PER_KILL,
            "{killed} kills in {n} reverts"
        );
        let change = change_on_main(&store, n);
        let delay = Duration::from_micros(rng.next_u64() % window);
        let what = format!("{format:?} seed {REVERT_SEED:#x} revert {n} delay {delay:?}");

        // 2. Kill a revert, then run it again under the same key.
        let reported = match kill_revert_after(&store, &change, n, delay) {
            Ended::Killed(reported) => {
                killed += 1;
                reported
            }
            Ended::Exited(id) => {
                window -= window / WINDOW_SHRINK;
                Some(id)
            }
        };
        let again = revert_again(&store, &change, n);
        if let Some(reported) = reported {
            assert_eq!(
                reported, again,
                "{what}: the reported revert is the one kept"
            );
        }

        // 3. The store verifies clean; main holds the base, then a change and its revert per
        //    round; the key is on one changeset; the change's file is gone.
        store.assert_clean(&what);
        let rounds = 2 * (n + 1);
        let log = store
            .read(&["log", MAIN, "-n", &(rounds + 1 + LOG_SURPLUS).to_string()])
            .ok();
        let on_main: Vec<&str> = (log["entries"].as_array().expect("log entries").iter())
            .map(|entry| entry["id"].as_str().expect("an id"))
            .collect();
        assert_eq!(on_main.len(), rounds + 1, "{what}: main's log: {on_main:?}");
        assert_eq!(on_main[0], again, "{what}: {again} is not main's head");
        for id in on_main {
            keys.entry(id.to_owned())
                .or_insert_with(|| key_of(&store, id));
        }
        let reverts = keys
            .values()
            .flatten()
            .filter(|&k| *k == revert_key(n))
            .count();
        assert_eq!(reverts, 1, "{what}: changesets with key {n}");
        let file = format!("{MAIN}:revert-{n}.txt");
        store.read(&["cat", &file]).err("E_PATH_NOT_FOUND");
    }

    println!("{format:?}: {killed} kills in {n} reverts, window {window} us");
    assert_read_clean(readers, &format!("{format:?} reverts"));
    store.assert_clean("after the last revert");
}

per_format! {
    /// The slow revert variant: [`SLOW_REVERT_KILLS`] kills per format.
    #[ignore = "slow, ~30 s: 200 kills per format, each with verify --deep, a log and a cat; \
                29-30 s at load 9"]
    fn sigkill_mid_revert_200_times(format) {
        kill_reverts(format, SLOW_REVERT_KILLS);
    }
}

/// How many landers each round of the batch variant starts.
const BATCH_LANDERS: usize = 8;

/// The kills the fast batch variant makes, one per round (`just check`).
const FAST_BATCH_KILLS: usize = 3;

/// The kills the slow batch variant makes, one per round.
const SLOW_BATCH_KILLS: usize = 200;

/// The seed of the batch test's victims and delays, apart from the other tests'.
const BATCH_SEED: u64 = 0xba7c_4e11;

/// How many rounds per kill the batch test may spend: a victim that ends before its delay is no
/// kill, and shrinks the window ([`WINDOW_SHRINK`]).
const BATCH_ROUNDS_PER_KILL: usize = 4;

/// What every lander of a round asks of the lead-wait hook: one other request waiting. The
/// round queues all eight before any of them leads, so the hook holds no batch back; a leader
/// left alone after a kill waits the hook's 10 s at most, and its waiters may end `E_BUSY`.
const BATCH_LEAD_WAIT: &str = "1";

/// How long a round waits for its landers to queue, and for those it does not kill to end.
const BATCH_PATIENCE: Duration = Duration::from_secs(60);

/// The idempotency key of the batch test's `n`-th land.
fn batch_key(n: usize) -> String {
    format!("kill-batch-{n}")
}

/// The arguments of the batch test's land of `head` under its `n`-th key.
fn batch_land_args(store: &Store, head: &str, n: usize) -> Vec<String> {
    let key = batch_key(n);
    store.args(&agent(0), &["land", head, "--key", &key])
}

/// The names in `store`'s queue; none if it has no queue.
fn queue_names(store: &Store) -> Vec<String> {
    let Ok(entries) = fs::read_dir(store.layout().queue_dir().as_std_path()) else {
        return Vec::new();
    };
    let mut names: Vec<String> = (entries.flatten())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// How a lander of a round ended.
enum Landed {
    /// It landed: the changeset.
    Exited(String),
    /// It gave up waiting for the lock (`E_BUSY`), recording nothing.
    Busy,
    /// SIGKILL hit it; its changeset if it printed its envelope whole.
    Killed(Option<String>),
}

/// How the `n`-th lander's run ended, `output` its run; `victim` if the test killed it.
fn landed_as(n: usize, output: &Output, victim: bool) -> Landed {
    let Output { status, stdout, .. } = output;

    // 1. A signal ends only the victim.
    if status.signal() == Some(SIGKILL) {
        assert!(victim, "lander {n} killed: {output:?}");
        return Landed::Killed(landed(stdout));
    }

    // 2. A busy run recorded nothing; any other failure fails the test.
    if !status.success() {
        let envelope: Option<Value> = serde_json::from_slice(stdout).ok();
        let code = envelope.as_ref().map(|e| e["error"]["code"].clone());
        assert_eq!(code, Some("E_BUSY".into()), "lander {n}: {output:?}");
        return Landed::Busy;
    }
    let id = landed(stdout).unwrap_or_else(|| panic!("lander {n}: no envelope: {output:?}"));
    Landed::Exited(id)
}

/// Whether queue file `name` has extension `ext`: `req`, `com` or `ans`.
fn is_kind(name: &str, ext: &str) -> bool {
    !name.starts_with('.') && Path::new(name).extension().is_some_and(|e| e == ext)
}

/// The batch variant's state across its rounds.
struct Batches {
    format: StoreFormat,
    store: Store,
    rng: Rng,
    /// Each changeset on `main` seen so far, and its land key.
    keys: BTreeMap<String, Option<String>>,
    /// The delays' bound, in microseconds: set by the first round, unkilled.
    window: Option<u64>,
    /// How many lands the rounds made so far.
    lands: usize,
    killed: usize,
    rounds: usize,
    /// Landers that ended `E_BUSY`.
    busy: usize,
    /// Rounds whose victim left a claimed request (`.com`).
    claimed_left: usize,
}

impl Batches {
    /// A store of `format` whose `main` names a base commit.
    fn new(format: StoreFormat) -> Batches {
        let store = Store::init_as(format);
        let base = store.commit(&agent(0), "base.txt", "base\n", None);
        let create = ["ref", "set", MAIN, &base, "--create"];
        store.run_as(&agent(0), &create, &[]).ok();
        Batches {
            format,
            store,
            rng: Rng::new(BATCH_SEED ^ u64::from(format.version())),
            keys: BTreeMap::from([(base, None)]),
            window: None,
            lands: 0,
            killed: 0,
            rounds: 0,
            busy: 0,
            claimed_left: 0,
        }
    }

    /// One round: [`BATCH_LANDERS`] changes off `main`, their lands queued behind the lock this
    /// test holds, the lock let go and one lander, chosen at random, SIGKILLed a random delay
    /// within the window later. The changes and how each land ended.
    fn round(&mut self) -> (Vec<String>, Vec<Landed>, String) {
        let store = &self.store;
        let first = self.lands;

        // 1. The round's changes, and their lands queued behind the lock.
        let heads: Vec<String> = (first..first + BATCH_LANDERS)
            .map(|n| change_off_main(store, n))
            .collect();
        let lock = MetaLock::acquire(&store.layout(), BATCH_PATIENCE).expect("the lock");
        let mut landers: Vec<Child> = (heads.iter().zip(first..))
            .map(|(head, n)| {
                let args = batch_land_args(store, head, n);
                Child::spawn(&mut swarmops(&args, &[(LEAD_WAIT_ENV, BATCH_LEAD_WAIT)]))
            })
            .collect();
        let limit = Instant::now() + BATCH_PATIENCE;
        let queued = || {
            queue_names(store)
                .iter()
                .filter(|n| is_kind(n, "req"))
                .count()
        };
        while queued() < BATCH_LANDERS {
            assert!(
                Instant::now() < limit,
                "round {}: never queued",
                self.rounds
            );
            thread::sleep(Duration::from_millis(1));
        }

        // 2. The lock let go; the victim waited for until its delay, then the rest.
        let victim = self.rng.below(BATCH_LANDERS);
        let delay = (self.window).map(|w| Duration::from_micros(self.rng.next_u64() % w));
        let what = format!(
            "{:?} seed {BATCH_SEED:#x} round {} victim {victim} delay {delay:?}",
            self.format, self.rounds
        );
        drop(lock);
        let start = Instant::now();
        let (output, _) = landers[victim].finish(start + delay.unwrap_or(BATCH_PATIENCE));
        let victim_end = landed_as(first + victim, &output, true);
        let mut ends = Vec::with_capacity(BATCH_LANDERS);
        for (at, lander) in landers.iter_mut().enumerate() {
            if at == victim {
                continue;
            }
            let (output, overdue) = lander.finish(start + BATCH_PATIENCE);
            assert!(!overdue, "{what}: lander {at} past its deadline");
            ends.push(landed_as(first + at, &output, false));
        }
        ends.insert(victim, victim_end);
        let took = u64::try_from(start.elapsed().as_micros()).expect("a short round");
        let left = queue_names(store);
        self.claimed_left += usize::from(left.iter().any(|name| is_kind(name, "com")));

        // 3. The kill counted, or the window set or shrunk.
        match (&ends[victim], self.window) {
            (Landed::Killed(_), _) => self.killed += 1,
            (_, None) => self.window = Some(took),
            (_, Some(w)) => self.window = Some(w - w / WINDOW_SHRINK),
        }
        (heads, ends, what)
    }

    /// Every land of the round again under its key, then the checks: a reported land is the one
    /// kept and an `E_BUSY` one lands anew; `main` holds the base and each key once; the store
    /// verifies clean; the next write's janitor empties the queue.
    fn check(&mut self, heads: &[String], ends: &[Landed], what: &str) {
        let store = &self.store;
        let first = self.lands;

        // 1. Every land again.
        for (at, end) in ends.iter().enumerate() {
            let args = batch_land_args(store, &heads[at], first + at);
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let result = crate::support::run(&args, &[]).ok();
            let again = result["landed"].as_str().expect("a landed changeset");
            match end {
                Landed::Exited(id) | Landed::Killed(Some(id)) => {
                    assert_eq!(id, again, "{what}: lander {at}: the reported land is kept");
                }
                Landed::Busy => {
                    // An `E_BUSY` land was in no frame: it lands now, no replay.
                    assert_eq!(result["status"], "landed", "{what}: lander {at}: {result}");
                    self.busy += 1;
                }
                Landed::Killed(None) => {}
            }
        }
        self.lands += BATCH_LANDERS;

        // 2. Main holds the base and each key once; the store verifies clean.
        store.assert_clean(what);
        let asked = (self.lands + 1 + LOG_SURPLUS).to_string();
        let log = store.read(&["log", MAIN, "-n", &asked]).ok();
        let on_main: Vec<&str> = (log["entries"].as_array().expect("log entries").iter())
            .map(|entry| entry["id"].as_str().expect("an id"))
            .collect();
        assert_eq!(
            on_main.len(),
            self.lands + 1,
            "{what}: main's log: {on_main:?}"
        );
        for id in on_main {
            (self.keys)
                .entry(id.to_owned())
                .or_insert_with(|| key_of(store, id));
        }
        for n in first..self.lands {
            let key = batch_key(n);
            let count = self.keys.values().flatten().filter(|&k| *k == key).count();
            assert_eq!(count, 1, "{what}: changesets with key {key}");
        }

        // 3. A replay takes no turn: the next write's janitor empties the queue.
        let round = self.rounds.to_string();
        store.commit(&agent(0), "janitor.txt", &round, None);
        assert_eq!(
            queue_names(store),
            Vec::<String>::new(),
            "{what}: the queue"
        );
    }
}

/// Rounds of the batch variant on a store of `format` until `kills` landers were killed
/// ([`Batches::round`], [`Batches::check`]). Returns in how many rounds the killed lander left
/// a claimed request (`.com`): a member killed after its leader claimed it.
fn kill_batches(format: StoreFormat, kills: usize) -> usize {
    let mut batches = Batches::new(format);
    while batches.killed < kills {
        batches.rounds += 1;
        let Batches { killed, rounds, .. } = batches;
        assert!(
            rounds <= 1 + kills * BATCH_ROUNDS_PER_KILL,
            "{killed} kills in {rounds} rounds"
        );
        let (heads, ends, what) = batches.round();
        batches.check(&heads, &ends, &what);
    }
    let Batches {
        killed,
        rounds,
        busy,
        claimed_left,
        window,
        ..
    } = batches;
    println!(
        "{format:?}: {killed} kills in {rounds} rounds of {BATCH_LANDERS} lands, {busy} busy, \
         {claimed_left} left a claimed request, window {window:?} us"
    );
    claimed_left
}

/// The fast batch variant: [`FAST_BATCH_KILLS`] kills, one per round of [`BATCH_LANDERS`]
/// lands, on a store of the default format.
#[test]
fn sigkill_mid_batch_keeps_one_land_per_key() {
    kill_batches(DEFAULT_INIT_FORMAT, FAST_BATCH_KILLS);
}

/// The slow batch variant: [`SLOW_BATCH_KILLS`] kills, one per round of [`BATCH_LANDERS`] lands, on
/// a store of the default format; some kill hits a member after its claim.
#[test]
#[ignore = "slow, ~220 s: 200 kills in ~270 rounds of 8 batched lands, each with the lands \
            again, verify --deep and a log; estimated from the fast variant, 0.6 s a round"]
fn sigkill_mid_batch_200_times() {
    let claimed_left = kill_batches(DEFAULT_INIT_FORMAT, SLOW_BATCH_KILLS);
    assert!(claimed_left > 0, "no kill hit a claimed member");
}

/// The kills of each of `ref set` and `op undo` the fast ref variant makes per format
/// (`just check`).
const FAST_REF_KILLS: usize = 8;

/// The kills of each the slow ref variant makes per format.
const SLOW_REF_KILLS: usize = 200;

/// The seed of the ref test's delays, apart from the other tests'.
const REF_SEED: u64 = 0x4ef5_e7d0;

/// The newest operations the ref test compares with its model after each step: the round's two
/// and the two before them.
const TAIL: usize = 4;

/// How a killed run of any command ended.
enum Cut {
    /// SIGKILL hit it; its result, if it printed one whole, successful envelope.
    Killed(Option<Value>),
    /// It exited before the delay, with a whole, successful envelope.
    Exited,
}

/// The result of `stdout` if it holds one whole, successful envelope.
fn result_of(stdout: &[u8]) -> Option<Value> {
    let text = std::str::from_utf8(stdout).ok()?;
    if !text.ends_with('\n') {
        return None;
    }

    let mut envelope: Value = serde_json::from_str(text).ok()?;
    assert_eq!(envelope["ok"], true, "an envelope: {text}");
    Some(envelope["result"].take())
}

/// Runs `args` and SIGKILLs the run `delay` after its spawn, unless it ended before.
fn kill_run(args: &[String], delay: Duration) -> Cut {
    let mut child = Child::spawn(&mut swarmops(args, &[]));
    let (output, _) = child.finish(Instant::now() + delay);
    let Output { status, stdout, .. } = &output;

    // 1. A signal ends only a killed run; the kill may land after the envelope was printed.
    if status.signal() == Some(SIGKILL) {
        return Cut::Killed(result_of(stdout));
    }

    // 2. A run that beat its delay must have succeeded.
    assert!(
        status.success(),
        "{args:?} ended {status} unkilled: {output:?}"
    );
    assert!(result_of(stdout).is_some(), "no envelope: {output:?}");
    Cut::Exited
}

/// What `release/1` names.
fn release_head(store: &Store) -> String {
    let listed = store.read(&["ref", "list"]).ok();
    let refs = listed["refs"].as_array().expect("a ref list");
    let release = refs.iter().find(|entry| entry["name"] == RELEASE);
    let target = release.and_then(|entry| entry["target"].as_str());
    target.expect("release/1 exists").to_owned()
}

/// One operation as the ref test models it: each ref it moved and where to, in its order.
fn moves_of(op: &Value) -> String {
    let moves = op["ref_moves"].as_array().expect("an op's ref moves");
    let named: Vec<String> = (moves.iter())
        .map(|moved| format!("{}={}", moved["name"], moved["new"]))
        .collect();
    named.join(",")
}

/// The newest [`TAIL`] operations, oldest first, and the newest one's id. The operations are
/// numbered one after another from 0, so the newest one's number is how many there are, less
/// one: the op log holds no other.
fn ops_tail(store: &Store) -> (Vec<(u64, String)>, String) {
    let listed = store.read(&["op", "log", "-n", &TAIL.to_string()]).ok();
    let ops = listed["ops"].as_array().expect("an op list");
    let mut tail: Vec<(u64, String)> = (ops.iter())
        .map(|op| (op["op"].as_u64().expect("an op number"), moves_of(op)))
        .collect();
    tail.reverse();
    let newest = ops[0]["id"].as_str().expect("an op id").to_owned();
    (tail, newest)
}

/// Asserts that the op log ends as `model` does, once and in order, and holds no other
/// operation; `what` says when.
#[track_caller]
fn assert_ops(store: &Store, model: &[String], what: &str) {
    let (tail, _) = ops_tail(store);
    let from = model.len().saturating_sub(TAIL);
    let expected: Vec<(u64, String)> = (from..model.len())
        .map(|n| (u64::try_from(n).expect("a short log"), model[n].clone()))
        .collect();
    assert_eq!(tail, expected, "{what}: the op log's tail");
}

/// The model's line for a move of `release/1` to `target`.
fn release_move(target: &str) -> String {
    format!("{RELEASE:?}={target:?}")
}

/// Where the ref test's kills stand.
struct RefKills {
    /// Kills of `ref set` and of `op undo` so far.
    set: usize,
    undo: usize,
    /// The runs of each that made their move before they ended, killed or not.
    set_made: usize,
    undo_made: usize,
    /// The windows the delays of each are drawn from, in microseconds.
    set_window: u64,
    undo_window: u64,
}

/// The ref test's first move of `release/1` by `set`, and its undo, run whole and timed: the
/// model of the op log after them, and the first windows.
fn first_round(store: &Store, set: &[String]) -> (Vec<String>, RefKills) {
    // 1. The move and its undo, each timed.
    let start = Instant::now();
    crate::support::run(set, &[]).ok();
    let set_window = u64::try_from(start.elapsed().as_micros()).expect("a short move");
    let (_, moved) = ops_tail(store);
    let start = Instant::now();
    store.run_as(&agent(0), &["op", "undo", &moved], &[]).ok();
    let undo_window = u64::try_from(start.elapsed().as_micros()).expect("a short undo");

    // 2. The model: the setup's operations before the tail stay unmodelled (empty).
    let (tail, _) = ops_tail(store);
    let mut model: Vec<String> = Vec::new();
    model.resize(
        usize::try_from(tail[0].0).expect("a short log"),
        String::new(),
    );
    model.extend(tail.into_iter().map(|(_, moves)| moves));
    let cuts = RefKills {
        set: 0,
        undo: 0,
        set_made: 0,
        undo_made: 0,
        set_window,
        undo_window,
    };
    (model, cuts)
}

/// `kills` SIGKILLs each of `ref set` and `op undo` on `release/1` of a store of `format`,
/// readers beside them. Each round moves `release/1` from `a` to `b` and undoes the move: it
/// kills the move, checks, runs the move again, then kills the undo, checks, runs the undo
/// again. Each check: the ref names its old or its new target (the new one if the killed run's
/// envelope was read), the op log is the model with the killed operation once or not at all,
/// `verify --deep` finds nothing wrong; the run again succeeds, a replay exactly when the killed
/// run had made its move, and the op log holds the operation once.
fn kill_ref_moves(format: StoreFormat, kills: usize) {
    let store = Store::init_as(format);
    let mut rng = Rng::new(REF_SEED ^ u64::from(format.version()));
    let me = agent(0);

    // 1. Two changesets on main's history, `release/1` at the first.
    let a = store.commit(&me, "a.txt", "a\n", None);
    store
        .run_as(&me, &["ref", "set", MAIN, &a, "--create"], &[])
        .ok();
    let b = store.commit(&me, "b.txt", "b\n", Some(&a));
    store
        .run_as(&me, &["ref", "set", RELEASE, &a, "--create"], &[])
        .ok();
    let set = store.args(&me, &["ref", "set", RELEASE, &b, "--expect", &a]);

    // 2. A first whole move and undo, timed: the first windows.
    let (mut model, mut cuts) = first_round(&store, &set);
    let readers = Readers::start(&store);

    let mut rounds = 0;
    while cuts.set < kills || cuts.undo < kills {
        rounds += 1;
        assert!(
            rounds <= kills * LAND_RUNS_PER_KILL,
            "{} and {} kills in {rounds} rounds",
            cuts.set,
            cuts.undo
        );
        let what = format!("{format:?} seed {REF_SEED:#x} round {rounds}");

        // 3. Kill the move; release/1 names a or b, b if the envelope was read.
        let delay = Duration::from_micros(rng.next_u64() % cuts.set_window);
        let acked = match kill_run(&set, delay) {
            Cut::Killed(result) => {
                cuts.set += 1;
                result.is_some()
            }
            Cut::Exited => {
                cuts.set_window -= cuts.set_window / WINDOW_SHRINK;
                true
            }
        };
        let head = release_head(&store);
        assert!(head == a || head == b, "{what}: release/1 at {head}");
        assert!(!acked || head == b, "{what}: an acknowledged move kept");
        let made = head == b;
        cuts.set_made += usize::from(made);
        let mut seen = model.clone();
        seen.extend(made.then(|| release_move(&b)));
        assert_ops(&store, &seen, &format!("{what}, the move killed"));
        store.assert_clean(&format!("{what}, the move killed"));

        // 4. The move again: a replay exactly when the killed run made it; recorded once.
        let again = crate::support::run(&set, &[]).ok();
        assert_eq!(again["already_applied"], made, "{what}: {again}");
        model.push(release_move(&b));
        assert_ops(&store, &model, &format!("{what}, the move again"));
        let (_, moved) = ops_tail(&store);
        let undo = store.args(&me, &["op", "undo", &moved]);

        // 5. Kill the undo; release/1 names b or a, a if the envelope was read.
        let delay = Duration::from_micros(rng.next_u64() % cuts.undo_window);
        let acked = match kill_run(&undo, delay) {
            Cut::Killed(result) => {
                cuts.undo += 1;
                result.is_some()
            }
            Cut::Exited => {
                cuts.undo_window -= cuts.undo_window / WINDOW_SHRINK;
                true
            }
        };
        let head = release_head(&store);
        assert!(head == a || head == b, "{what}: release/1 at {head}");
        assert!(!acked || head == a, "{what}: an acknowledged undo kept");
        let undone = head == a;
        cuts.undo_made += usize::from(undone);
        let mut seen = model.clone();
        seen.extend(undone.then(|| release_move(&a)));
        assert_ops(&store, &seen, &format!("{what}, the undo killed"));
        store.assert_clean(&format!("{what}, the undo killed"));

        // 6. The undo again: a replay exactly when the killed run made it; recorded once.
        let again = crate::support::run(&undo, &[]).ok();
        assert_eq!(again["already_applied"], undone, "{what}: {again}");
        model.push(release_move(&a));
        assert_ops(&store, &model, &format!("{what}, the undo again"));
    }

    println!(
        "{format:?}: {} and {} kills in {rounds} rounds ({} and {} made), windows {} and {} us",
        cuts.set, cuts.undo, cuts.set_made, cuts.undo_made, cuts.set_window, cuts.undo_window
    );
    assert_read_clean(readers, &format!("{format:?} ref moves"));
    store.assert_clean("after the last undo");
}

per_format! {
    /// The fast ref variant: [`FAST_REF_KILLS`] kills of each per format.
    fn sigkill_mid_ref_move_keeps_the_store(format) {
        kill_ref_moves(format, FAST_REF_KILLS);
    }
}

per_format! {
    /// The slow ref variant: [`SLOW_REF_KILLS`] kills of each per format.
    #[ignore = "slow, ~230 s: 200 kills per format of ref set and op undo each, every one with verify --deep and the op log"]
    fn sigkill_mid_ref_move_200_times(format) {
        kill_ref_moves(format, SLOW_REF_KILLS);
    }
}

/// The readers' own check: one pass over a store whose one reachable blob file was removed
/// counts exactly one violation, the `cat` of that file; started readers count it too.
#[test]
fn readers_flag_a_missing_object() {
    let store = Store::init();
    let id = store.commit(&agent(0), "a.txt", "a\n", None);
    store
        .run_as(&agent(0), &["ref", "set", MAIN, &id, "--create"], &[])
        .ok();

    // 1. Remove the file's blob, which `main` reaches.
    let read = store.read(&["cat", &format!("{id}:a.txt")]).ok();
    let blob = read["blob"].as_str().expect("a blob id");
    let blob = ObjectId::from_str(blob).expect("a hex object id");
    fs::remove_file(store.layout().object_path(blob)).expect("remove the blob");

    // 2. One pass: `ref list` and `show` read, the `cat` is the violation.
    let tally = Readers::pass(store.path());
    assert_eq!(tally.violations, 1, "{tally:?}");
    assert_eq!(tally.reads, 2, "{tally:?}");

    // 3. The started readers make at least one pass each.
    let tally = Readers::start(&store).stop();
    assert!(tally.violations >= READERS, "{tally:?}");
}
