//! `so queue` (the M22 core design §6): the landing queue's commands run as the real binary on a
//! store whose `main` names a check, each envelope pinned as a golden.
//!
//! ```text
//!   so queue enqueue ──▶ entry durable ──▶ detached `so queue run main` (lane free)
//!                                              │ check in a workspace of its own
//!                                              ▼
//!   so queue wait ◀── poll, start a runner when none holds the lane ── main moved: landed
//! ```
//!
//! The check reads the candidate's tree: `bad` fails it, `slow` makes it sleep first.

use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::mcp::Names;
use crate::ws::{AGENT, Lab, Out};

/// The repository's check: fails on `bad`, sleeps [`SLOW_S`] seconds on `slow`.
const CHECK: &str = "if [ -e slow ]; then sleep 3; fi; if [ -e bad ]; then exit 1; fi; exit 0";

/// How long a slow check sleeps, in seconds.
const SLOW_S: u64 = 3;

/// The most a test waits for an entry to land.
const LAND_WITHIN: Duration = Duration::from_secs(60);

/// A store whose `main` names [`CHECK`] as its check.
pub(crate) fn checked_lab() -> Lab {
    let lab = Lab::new();
    let config = json!({"check": {"argv": ["sh", "-c", CHECK], "timeout_s": 60}});
    lab.publish(".swarmops/workspaces.json", &config.to_string());
    lab
}

/// `main`'s head.
fn main_of(lab: &Lab) -> String {
    let resolved = lab.so(&["resolve", "main"]).ok();
    resolved["id"].as_str().expect("an id").to_owned()
}

/// A change on `main`'s head putting `content` at `path`, on no ref: its changeset id.
pub(crate) fn change(lab: &Lab, path: &str, content: &str) -> String {
    let main = main_of(lab);
    lab.store.commit(AGENT, path, content, Some(&main))
}

/// `queue enqueue main --head <head>`, then `extra`.
fn enqueue(lab: &Lab, head: &str, extra: &[&str]) -> Out {
    let mut line = vec!["queue", "enqueue", "main", "--head", head];
    line.extend(extra);
    lab.so(&line)
}

/// Polls `queue status --entry <entry>` until the entry's state is `state`.
pub(crate) fn wait_for_state(lab: &Lab, entry: &str, state: &str) {
    let deadline = Instant::now() + LAND_WITHIN;
    loop {
        let status = lab.so(&["queue", "status", "--entry", entry]).ok();
        if status["entries"][0]["state"] == state {
            return;
        }
        assert!(Instant::now() < deadline, "{entry} never {state}: {status}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// `out`'s envelope with what changes from run to run named by `names`, and `elapsed_ms`
/// pinned.
fn golden(names: &mut Names, out: &Out) -> Value {
    names.redact(out.envelope())
}

/// The landing queue's commands each print their envelope (K24): an enqueue with a key, a
/// second one withdrawn while the first is checked, the first waited for until it lands, then
/// both listed.
#[test]
fn queue_commands_golden() {
    // 1. Two changes, the first slow to check.
    let lab = checked_lab();
    let mut names = Names::default();
    names.add(lab.store.tmp().to_str().expect("UTF-8"), "[tmp]");
    names.add(&main_of(&lab), "[main]");
    let first = change(&lab, "slow", "1\n");
    let second = change(&lab, "second.txt", "2\n");
    names.add(&first, "[first]");
    names.add(&second, "[second]");

    // 2. The first queued with a key; once it is checked, the second queued and withdrawn.
    let queued = enqueue(&lab, &first, &["--key", "k-1"]);
    let entry = queued.ok()["entry"]["id"].as_str().unwrap().to_owned();
    names.add(&entry, "[entry1]");
    wait_for_state(&lab, &entry, "checking");
    let other = enqueue(&lab, &second, &[]);
    let other_id = other.ok()["entry"]["id"].as_str().unwrap().to_owned();
    names.add(&other_id, "[entry2]");
    let withdrawn = lab.so(&["queue", "withdraw", &other_id]);

    // 3. The first waited for, then both listed.
    let waited = lab.so(&["queue", "wait", &entry, "--timeout", "60"]);
    let landed = waited.ok()["landed"].as_str().unwrap().to_owned();
    names.add(&landed, "[landed]");
    let status = lab.so(&["queue", "status", "main"]);
    let steps: Vec<Value> = [&queued, &other, &withdrawn, &waited, &status]
        .iter()
        .map(|out| golden(&mut names, out))
        .collect();
    insta::assert_json_snapshot!(steps);
}

/// `queue wait` past its timeout with the entry still being checked: `E_QUEUE_WAIT_TIMEOUT`,
/// exit 4, retryable; waiting again sees it land.
#[test]
fn queue_wait_times_out_retryable() {
    let lab = checked_lab();
    let head = change(&lab, "slow", "1\n");
    let entry = enqueue(&lab, &head, &[]).ok()["entry"]["id"]
        .as_str()
        .unwrap()
        .to_owned();

    // 1. One second is shorter than the check.
    let started = Instant::now();
    let out = lab.so(&["queue", "wait", &entry, "--timeout", "1"]);
    let error = out.err("E_QUEUE_WAIT_TIMEOUT", 4);
    assert!(started.elapsed() < Duration::from_secs(SLOW_S), "{out}");
    assert_eq!(error["retryable"], true, "{out}");
    assert_eq!(error["details"]["entry"], entry.as_str(), "{out}");
    assert!(
        ["waiting", "checking"].contains(&error["details"]["state"].as_str().unwrap()),
        "{out}"
    );

    // 2. Again, long enough.
    let again = lab.so(&["queue", "wait", &entry, "--timeout", "60"]);
    assert_eq!(again.ok()["state"], "landed", "{again}");
}

/// `queue wait` on an entry whose check fails ends as soon as it is removed, with the removal's
/// error (reason `check_failed`), not at its timeout and not as a retryable timeout.
#[test]
fn queue_wait_reports_removal() {
    let lab = checked_lab();
    let head = change(&lab, "bad", "1\n");
    let entry = enqueue(&lab, &head, &[]).ok()["entry"]["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let started = Instant::now();
    let out = lab.so(&["queue", "wait", &entry, "--timeout", "60"]);
    assert!(started.elapsed() < LAND_WITHIN, "{out}");
    let envelope = out.envelope();
    assert_eq!(envelope["ok"], false, "{out}");
    assert_ne!(envelope["error"]["code"], "E_QUEUE_WAIT_TIMEOUT", "{out}");
    assert_ne!(out.exit, 4, "{out}");
    assert_eq!(
        envelope["error"]["details"]["reason"], "check_failed",
        "{out}"
    );
    assert_eq!(
        envelope["error"]["details"]["entry"],
        entry.as_str(),
        "{out}"
    );
}

/// A second `queue run` while a runner holds the lane exits 0 at once, its check still running.
#[test]
fn a_second_runner_exits_at_once() {
    // 1. An entry whose check sleeps: the runner the enqueue started holds the lane.
    let lab = checked_lab();
    let head = change(&lab, "slow", "1\n");
    let entry = enqueue(&lab, &head, &[]).ok()["entry"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    wait_for_state(&lab, &entry, "checking");

    // 2. Another runner: done well before the check is.
    let started = Instant::now();
    let out = lab.so(&["queue", "run", "main"]);
    assert_eq!(out.exit, 0, "{out}");
    assert!(started.elapsed() < Duration::from_secs(SLOW_S), "{out}");
    assert_eq!(out.ok()["entries"][0]["state"], "checking", "{out}");

    // 3. The first runner lands it.
    let waited = lab.so(&["queue", "wait", &entry, "--timeout", "60"]);
    assert_eq!(waited.ok()["state"], "landed", "{waited}");
}

/// `queue enqueue` alone lands its entry (K26): no `run`, no `wait`, no daemon; only reads of
/// `main` follow, which start nothing.
#[test]
fn an_enqueued_entry_lands_without_a_driver() {
    let lab = checked_lab();
    let head = change(&lab, "landed.txt", "1\n");
    enqueue(&lab, &head, &[]).ok();

    let deadline = Instant::now() + LAND_WITHIN;
    loop {
        let read = lab.so(&["cat", "main:landed.txt"]);
        if read.exit == 0 {
            assert_eq!(read.ok()["content"], "1\n", "{read}");
            break;
        }
        assert!(Instant::now() < deadline, "never landed: {read}");
        std::thread::sleep(Duration::from_millis(100));
    }
    lab.store.assert_clean("after the land");
}
