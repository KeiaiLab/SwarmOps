//! Many `swarmops` processes on one store (operations spec §0; storage spec §3): the M1
//! acceptance tests for concurrency, through the real binary. Processes that crash in the middle
//! of a write are `crash.rs`'s.
//!
//! - Every child is the binary itself (`CARGO_BIN_EXE_swarmops`) with its own `--repo`, `--as`
//!   and environment; children that must race start together behind a barrier
//!   ([`support::Racers`]).
//! - Every test ends with `verify --deep` through the binary, and reads its `problems`.
//!
//! Each test owns its temp dirs, and the tests share nothing.

mod support;

use std::any::Any;
use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use camino::{Utf8Path, Utf8PathBuf};
use serde_json::{Value, json};
use support::{BIN, Racers, Run, Store, agent, retry_busy, run, run_bin_within, run_within};
use swarmops_core::{EntryKind, ObjectKind, RepoPath, Signature, hash_object};
use swarmops_repo::{Change, ChangeOp, CommitRequest, LocalRepo};
use swarmops_store::{Layout, MetaLock};
use swarmops_testkit::fs::{Call, Watch, Watched};

/// How many children the commit tests race.
const COMMITTERS: usize = 8;

/// How many children race each round of the ref test.
const RACERS: usize = 16;

/// How many rounds the ref test races: the first creates main, each other one moves it.
const ROUNDS: u64 = 4;

// ---------------------------------------------------------------------------------------------
// Concurrent processes.
// ---------------------------------------------------------------------------------------------

/// The file that committer `n` puts, and its content.
fn own_file(n: usize) -> (String, String) {
    (
        format!("worker-{n}.txt"),
        format!("written by worker {n}\n"),
    )
}

/// Eight processes commit at once, each its own file: every commit returns, each changeset is
/// stored whole and numbered once, and the store verifies clean.
#[test]
fn eight_processes_commit_concurrently_all_persist() {
    let store = Store::init();
    let mut racers = Racers::new(store.tmp());
    for n in 0..COMMITTERS {
        let (path, content) = own_file(n);
        let changes = store.changes(&format!("worker-{n}"), &path, &content);
        let message = format!("worker {n}");
        let args = store.args(
            &agent(n),
            &["commit", "-m", &message, "--changes", &changes],
        );
        racers.spawn(&args, &[("SWARMOPS_RUN_ID", &format!("run-{n}"))]);
    }
    let ids: Vec<String> = racers
        .race()
        .iter()
        .map(|run| {
            let result = run.ok();
            assert_eq!(result["files_changed"], 1, "{run}");
            assert_eq!(result["blobs_written"], 1, "{run}");
            assert!(result.get("warnings").is_none(), "{run}");
            assert!(
                result.get("parent").is_none(),
                "no main yet, so a root: {run}"
            );
            result["changeset"].as_str().unwrap().to_owned()
        })
        .collect();
    let distinct: BTreeSet<&String> = ids.iter().collect();
    assert_eq!(distinct.len(), COMMITTERS, "{ids:?}");

    let mut seqs = Vec::new();
    for (n, id) in ids.iter().enumerate() {
        let shown = store.show(id);
        assert_eq!(shown["id"], id.as_str());
        let author = json!({"agent": agent(n), "run_id": format!("run-{n}")});
        assert_eq!(shown["author"], author, "{id}");
        let (path, content) = own_file(n);
        let changes = shown["changes"].as_array().unwrap();
        assert_eq!(changes.len(), 1, "{shown}");
        assert_eq!(
            (&changes[0]["path"], &changes[0]["op"]),
            (&json!(path), &json!("put"))
        );
        let cat = store.read(&["cat", &format!("{id}:{path}")]).ok();
        assert_eq!(cat["content"], content);
        seqs.push(shown["seq"].as_u64().unwrap());
    }
    seqs.sort_unstable();
    let all: Vec<u64> = (0..).take(COMMITTERS).collect();
    assert_eq!(seqs, all, "each changeset is numbered once");
    assert_eq!(store.info()["changesets"], COMMITTERS);
    store.assert_clean("after the commits");
}

/// Eight processes commit the same file at once: its blob and the one root tree are written
/// by whichever process links them first, the others find them there or lose the no-clobber
/// link. One file holds the blob, linked once (no temp file still names it), no temp file is
/// left behind, and every changeset reads it back.
#[test]
fn blob_write_idempotent_across_processes() {
    let store = Store::init();
    let content = "the same bytes from every process\n".repeat(64);
    let changes = store.changes("shared", "shared.txt", &content);
    let mut racers = Racers::new(store.tmp());
    for n in 0..COMMITTERS {
        let args = store.args(&agent(n), &["commit", "-m", "same", "--changes", &changes]);
        racers.spawn(&args, &[("SWARMOPS_RUN_ID", &format!("run-{n}"))]);
    }
    let results: Vec<Value> = racers.race().iter().map(Run::ok).collect();
    let ids: BTreeSet<&str> = results
        .iter()
        .map(|result| result["changeset"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids.len(),
        COMMITTERS,
        "the authors differ, so the changesets do"
    );
    let roots: BTreeSet<&str> = results
        .iter()
        .map(|result| result["root"].as_str().unwrap())
        .collect();
    assert_eq!(roots.len(), 1, "one root tree: {roots:?}");
    let written: u64 = results
        .iter()
        .map(|result| result["blobs_written"].as_u64().unwrap())
        .sum();
    assert!(written >= 1, "some process wrote the blob");

    let blob = hash_object(ObjectKind::Blob, content.as_bytes());
    let file = Layout::new(Utf8PathBuf::from(store.path())).object_path(blob);
    let meta = fs::metadata(&file).unwrap();
    assert!(meta.is_file(), "{file}");
    assert_eq!(meta.nlink(), 1, "no temp file still links to {file}");
    assert_eq!(
        store.object_files(),
        2 + COMMITTERS,
        "the blob, the tree and the changesets, each once"
    );
    assert_eq!(store.temp_files(), 0, "every temp file was removed");
    for id in &ids {
        let cat = store.read(&["cat", &format!("{id}:shared.txt")]).ok();
        assert_eq!(cat["blob"], blob.to_hex(), "{id}");
        assert_eq!(cat["content"], content.as_str(), "{id}");
    }
    store.assert_clean("after the commits");
}

/// Sixteen processes race to move main, each to a changeset of its own, from what main names
/// now: in the first round they race to create it (`--create`), in each later round to move it
/// from the last winner (`--expect <its id>`). Every round exactly one wins (exit 0), and every
/// other one exits 4 with `E_CAS_MISMATCH`, whose `actual` is the winner. main's history is
/// then the winners, and its version counts the rounds. The racers' commits are the setup of
/// each round: one that meets `E_BUSY` (a lock wait a loaded host stretched past its 10 s) runs
/// again, as an agent's would ([`retry_busy`]); the ref moves, the race itself, never do.
#[test]
fn sixteen_processes_race_ref_set_exactly_one_wins_per_round() {
    let store = Store::init();
    let mut main: Option<String> = None;
    let mut winners = Vec::new();
    for round in 0..ROUNDS {
        // Each racer's changeset, on main as it is (none in the first round), made at once.
        let mut commits = Racers::new(store.tmp());
        let mut setups = Vec::with_capacity(RACERS);
        for racer in 0..RACERS {
            let changes = store.changes(
                &format!("round-{round}-racer-{racer}"),
                &format!("racer-{racer}.txt"),
                &format!("round {round}\n"),
            );
            let mut args = vec!["commit", "-m", "mine", "--changes", &changes];
            if let Some(main) = &main {
                args.extend(["--parent", main]);
            }
            let run_id = format!("round-{round}-racer-{racer}");
            let args = store.args(&agent(racer), &args);
            commits.spawn(&args, &[("SWARMOPS_RUN_ID", &run_id)]);
            setups.push((args, run_id));
        }
        let ids: Vec<String> = commits
            .race()
            .into_iter()
            .zip(&setups)
            .map(|(first, (args, run_id))| {
                let made = retry_busy(first, || run(args, &[("SWARMOPS_RUN_ID", run_id)]));
                made.ok()["changeset"].as_str().unwrap().to_owned()
            })
            .collect();

        let mut moves = Racers::new(store.tmp());
        for (racer, id) in ids.iter().enumerate() {
            let mut args = vec!["ref", "set", "main", id];
            match &main {
                Some(old) => args.extend(["--expect", old]),
                None => args.push("--create"),
            }
            moves.spawn(&store.args(&agent(racer), &args), &[]);
        }
        let runs = moves.race();
        let won: Vec<usize> = (0..RACERS).filter(|&r| runs[r].exit() == 0).collect();
        let [winner] = won[..] else {
            let shown: Vec<String> = runs.iter().map(ToString::to_string).collect();
            panic!(
                "round {round}: {} winners {won:?}:\n{}",
                won.len(),
                shown.join("\n")
            );
        };
        let new = ids[winner].as_str();
        let mut want = json!({"name": "main", "new": new, "already_applied": false});
        if let Some(old) = &main {
            want["old"] = json!(old);
        }
        assert_eq!(runs[winner].ok(), want, "round {round}");
        for (racer, run) in runs
            .iter()
            .enumerate()
            .filter(|&(racer, _)| racer != winner)
        {
            let error = run.err("E_CAS_MISMATCH");
            assert_eq!(
                (run.exit(), &error["retryable"]),
                (4, &json!(true)),
                "{run}"
            );
            let details = json!({"name": "main", "actual": new});
            assert_eq!(error["details"], details, "round {round}, racer {racer}");
        }
        let refs = store.read(&["ref", "list"]).ok();
        let main_ref = json!([{"name": "main", "target": new, "version": round + 1}]);
        assert_eq!(refs["refs"], main_ref, "round {round}");
        winners.push(new.to_owned());
        main = Some(new.to_owned());
    }

    let log = store.read(&["log"]).ok();
    let history: Vec<&str> = log["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["id"].as_str().unwrap())
        .collect();
    let newest_first: Vec<&str> = winners.iter().rev().map(String::as_str).collect();
    assert_eq!(
        history, newest_first,
        "main's first parents are the winners"
    );
    let racers = u64::try_from(RACERS).unwrap();
    assert_eq!(store.info()["changesets"], racers * ROUNDS);
    store.assert_clean("after the races");
}

// ---------------------------------------------------------------------------------------------
// The harness's own deadlines.
// ---------------------------------------------------------------------------------------------

/// The message of a panic that `catch_unwind` caught.
fn panic_message(panic: Box<dyn Any + Send>) -> String {
    match panic.downcast::<String>() {
        Ok(message) => *message,
        Err(panic) => panic.downcast_ref::<&str>().map_or_else(
            || "a panic without a message".to_owned(),
            |m| (*m).to_owned(),
        ),
    }
}

/// Holds `store`'s writer lock, as a writer in another process would: a commit then waits, for
/// as long as the lock's timeout (10 s), for the lock.
fn hold_the_lock(store: &Store) -> MetaLock {
    let layout = Layout::new(Utf8PathBuf::from(store.path()));
    MetaLock::acquire(&layout, Duration::from_secs(10)).unwrap()
}

/// A run still going at its deadline is killed (SIGKILL), reaped, and fails the test with what
/// it printed; at its deadline, not at the end of its wait.
#[test]
fn a_run_past_its_deadline_is_killed_and_reported() {
    let store = Store::init();
    let lock = hold_the_lock(&store);
    let changes = store.changes("waits", "waits.txt", "waits for the lock\n");
    let args = store.args(&agent(0), &["commit", "-m", "waits", "--changes", &changes]);
    let started = Instant::now();
    let caught = panic::catch_unwind(|| run_within(&args, &[], Duration::from_millis(300)));
    let message = panic_message(caught.expect_err("a run past its deadline fails the test"));
    assert!(message.contains("past its deadline"), "{message}");
    assert!(message.contains("signal: 9"), "killed: {message}");
    let waited = started.elapsed();
    assert!(
        waited < Duration::from_secs(5),
        "killed at its deadline: {waited:?}"
    );
    drop(lock);
    store.assert_clean("after a commit killed while it waited for the lock");
}

/// A run is killed at its deadline however much of its input the child leaves unread: the input
/// is written while the deadline runs, not before it starts. Here the child, a commit waiting
/// for the lock, reads none of it, and it is more than a pipe holds (64 KiB on Linux), so a
/// write of all of it would block until the child ends.
#[test]
fn a_run_whose_input_goes_unread_is_killed_at_its_deadline() {
    let store = Store::init();
    let lock = hold_the_lock(&store);
    let changes = store.changes("waits", "waits.txt", "waits for the lock\n");
    let args = store.args(&agent(0), &["commit", "-m", "waits", "--changes", &changes]);
    let input = "x".repeat(1 << 20);
    let started = Instant::now();
    let caught =
        panic::catch_unwind(|| run_bin_within(BIN, &args, &[], &input, Duration::from_millis(300)));
    let message = panic_message(caught.expect_err("a run past its deadline fails the test"));
    assert!(message.contains("past its deadline"), "{message}");
    assert!(message.contains("signal: 9"), "killed: {message}");
    let waited = started.elapsed();
    assert!(
        waited < Duration::from_secs(5),
        "killed at its deadline: {waited:?}"
    );
    drop(lock);
    store.assert_clean("after a commit killed while it waited for the lock");
}

/// Racers still going at their deadline are killed and reaped, every one of them, though the
/// test fails at the first: none is left running, nor unreaped.
#[test]
fn racers_past_their_deadline_are_killed_and_reaped() {
    let store = Store::init();
    let lock = hold_the_lock(&store);
    let mut racers = Racers::new(store.tmp());
    for n in 0..3 {
        let changes = store.changes(&format!("waits-{n}"), &format!("{n}.txt"), "waits\n");
        let args = store.args(&agent(n), &["commit", "-m", "waits", "--changes", &changes]);
        racers.spawn(&args, &[]);
    }
    let pids = racers.pids();
    let started = Instant::now();
    let caught = panic::catch_unwind(AssertUnwindSafe(|| {
        racers.race_within(Duration::from_millis(300))
    }));
    let message = panic_message(caught.expect_err("racers past their deadline fail the test"));
    assert!(message.contains("past its deadline"), "{message}");
    let waited = started.elapsed();
    assert!(
        waited < Duration::from_secs(5),
        "killed at their deadline: {waited:?}"
    );
    for pid in pids {
        let proc = PathBuf::from(format!("/proc/{pid}"));
        assert!(!proc.exists(), "child {pid} is gone, reaped");
    }
    drop(lock);
    store.assert_clean("after commits killed while they waited for the lock");
}

// ---------------------------------------------------------------------------------------------
// A failed index sync (storage spec §3, the batched sync).
// ---------------------------------------------------------------------------------------------

/// What makes the real filesystem's first `sync_data` of a file named `name` fail: what a
/// writeback error looks like to the store, reported once, to the next sync after it.
struct FailFirstSync {
    name: &'static str,
    failed: AtomicBool,
}

impl FailFirstSync {
    fn of(name: &'static str) -> Watched<FailFirstSync> {
        Watched(FailFirstSync {
            name,
            failed: AtomicBool::new(false),
        })
    }
}

impl Watch for FailFirstSync {
    fn before(&self, call: &Call<'_>) -> io::Result<()> {
        let named = call
            .path()
            .file_name()
            .is_some_and(|name| name == self.name);
        if call.method == "sync_data" && named && !self.failed.swap(true, Ordering::SeqCst) {
            return Err(io::Error::other(format!(
                "injected: the writeback of {} failed",
                self.name
            )));
        }
        Ok(())
    }
}

/// A commit of one file, as the CLI makes it.
fn one_file_commit(path: &str, content: &str) -> CommitRequest {
    let who = agent(3);
    let signature = Signature {
        name: who.clone().into_bytes(),
        email: b"worker-3@test-swarm.swarmops.local".to_vec(),
        secs: 1_700_000_000,
        offset_secs: 0,
        negative_zero: false,
    };
    CommitRequest {
        parents: Vec::new(),
        changes: vec![Change {
            path: RepoPath::new(path).unwrap(),
            op: ChangeOp::Put {
                kind: EntryKind::File,
                content: content.as_bytes().to_vec(),
            },
        }],
        author: signature.clone(),
        committer: signature,
        message: b"in process".to_vec(),
        metadata: vec![("agent".to_owned(), who.into_bytes())],
        allow_empty: false,
    }
}

/// The 32nd commit past the checkpoint fails its batched index sync (made in this process
/// through a filesystem whose first sync of `ids.col` fails): it is committed all the same, and
/// its transaction cuts the index back to the records the checkpoint has passed, none here.
/// The store verifies clean, the changeset is found through the journal, and the next commit
/// through the binary appends the records again, syncs them and moves the checkpoint.
#[test]
fn the_next_commit_repairs_an_index_cut_back_after_a_failed_sync() {
    // Frames 0..=30 through the binary, past the checkpoint, which has passed none.
    const BEFORE: u64 = 31;
    let store = Store::init();
    for n in 0..BEFORE {
        store.commit(&agent(0), &format!("f{n}.txt"), &format!("{n}\n"), None);
    }
    let layout = Layout::new(Utf8PathBuf::from(store.path()));
    let ids_col = layout.meta_path("idx/ids.col");
    assert_eq!(fs::metadata(&ids_col).unwrap().len(), BEFORE * 32);

    let failing = Arc::new(FailFirstSync::of("ids.col"));
    let repo = LocalRepo::open_with_fs(Utf8Path::new(store.path()), Arc::clone(&failing) as _);
    let outcome = repo
        .unwrap()
        .commit(one_file_commit("in-process.txt", "x\n"))
        .unwrap();
    assert!(
        failing.failed.load(Ordering::SeqCst),
        "the sync of ids.col failed"
    );
    assert_eq!(outcome.warnings.len(), 1, "{:?}", outcome.warnings);
    assert!(
        outcome.warnings[0].contains("injected"),
        "{}",
        outcome.warnings[0]
    );
    assert_eq!(
        fs::metadata(&ids_col).unwrap().len(),
        0,
        "cut back to the checkpoint"
    );
    assert_eq!(store.checkpoint(), 0, "the checkpoint did not move");
    let id = outcome.id.to_string();
    assert_eq!(store.show(&id)["seq"], BEFORE);
    store.assert_clean("after the failed sync");

    store.commit(&agent(0), "next.txt", "next\n", None);
    assert_eq!(fs::metadata(&ids_col).unwrap().len(), (BEFORE + 2) * 32);
    assert_eq!(store.checkpoint(), store.journal_len());
    assert_eq!(store.info()["changesets"], BEFORE + 2);
    assert_eq!(store.show(&id)["seq"], BEFORE);
    store.assert_clean("after the next commit");
}
