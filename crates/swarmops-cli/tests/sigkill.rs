//! The literal SIGKILL test (the M6 design, §9 crash tests): the binary commits on a store in a
//! temp dir on the real disk, and a real `kill -9` (`std::process::Child::kill`) ends it at a
//! random point. `crash.rs` aborts a run at a chosen filesystem call; this test picks no call,
//! only a moment, so it also covers what no hook marks (process start, parsing, output).
//!
//! ```text
//!   spawn `commit --advance main` ──┐
//!   wait a seeded delay < window    │  window: how long one commit takes here
//!   SIGKILL (testkit deadline) ─────┘
//!   verify --deep clean · every commit whose envelope was read is in `log` · next commit ok
//! ```
//!
//! Each test owns its temp dir; the tests share nothing.

use std::collections::BTreeSet;
use std::os::unix::process::ExitStatusExt;
use std::process::Output;
use std::time::{Duration, Instant};

use serde_json::Value;
use swarmops_store::StoreFormat;
use swarmops_testkit::child::Child;
use swarmops_testkit::per_format;
use swarmops_testkit::ws::Rng;

use crate::support::{Store, agent, swarmops};

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

/// Commits on `main` to its end; the changeset.
fn commit(store: &Store, n: usize) -> String {
    let args = commit_args(store, n);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = crate::support::run(&args, &[]).ok();
    result["changeset"]
        .as_str()
        .expect("a changeset id")
        .to_owned()
}

/// `kills` SIGKILLs of `commit --advance` on a store of `format`, each followed by the checks.
fn kill_commits(format: StoreFormat, kills: usize) {
    let store = Store::init_as(format);
    let mut read = BTreeSet::new();
    let mut rng = Rng::new(SEED ^ u64::from(format.version()));

    // 1. The window: one whole commit's wall time here, so most delays land inside a run.
    let start = Instant::now();
    read.insert(commit(&store, 0));
    let window = u64::try_from(start.elapsed().as_micros()).expect("a short commit");

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

        // 4. The next commit succeeds.
        read.insert(commit(&store, n));
    }

    println!("{format:?}: {killed} kills in {runs} runs, window {window} us");
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
    #[ignore = "slow, ~200 s per format: 500 kills, each with verify --deep and log"]
    fn sigkill_mid_commit_500_times(format) {
        kill_commits(format, SLOW_KILLS);
    }
}
