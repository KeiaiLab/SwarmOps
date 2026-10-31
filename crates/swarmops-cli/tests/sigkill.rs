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
//! A kill counts wherever it hits, before the run wrote anything included: about 1 kill of a land
//! in 5 hit there, measured over 200 kills a format.
//!
//! The land variant (the M15 design §6) kills `land --key` instead and runs the same land again
//! with the same key: the key lands its change exactly once, whatever moment the kill hit.
//!
//! Each test owns its temp dir; the tests share nothing.

use std::collections::{BTreeMap, BTreeSet};
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

/// The ref the land test's last step lands onto.
const RELEASE: &str = "release/1";

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
    #[ignore = "slow, ~220 s per format, 310 s at load 270: 500 kills, each with verify --deep and log"]
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
    #[ignore = "slow, ~30 s per format, 180 s at load 900: 200 kills, each with verify --deep and a log"]
    fn sigkill_mid_land_200_times(format) {
        kill_lands(format, SLOW_LAND_KILLS);
    }
}
