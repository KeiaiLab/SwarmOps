//! Many `swarmops` processes on one store, and `swarmops` processes that crash in the middle
//! of a write (operations spec §0; storage spec §2–§3): the M1 acceptance tests for
//! concurrency and crash consistency, through the real binary.
//!
//! - Every child is the binary itself (`CARGO_BIN_EXE_swarmops`) with its own `--repo`, `--as`
//!   and environment; children that must race start together behind a barrier
//!   ([`support::Racers`]).
//! - A crash is `SWARMOPS_TEST_CRASH_AT=k`: the store's k-th filesystem call aborts the
//!   process, as a `kill -9` at that exact point would (no destructor runs; what reached the
//!   kernel stays). The object store and the metadata store share one call count, objects are
//!   written one at a time then, and a sweep's commits sign at a fixed time
//!   (`SWARMOPS_TEST_NOW`), so that they write the same objects: every run of a command makes
//!   the same calls in the same order, and call k is the same call in every run. A dry run with
//!   `SWARMOPS_TEST_FS_CALLS=<file>` counts a command's calls first (twice, in two seconds of
//!   the clock, for the commit), and the sweep crashes the command at every one of them.
//! - Every test ends with `verify --deep` through the binary, and reads its `problems`.
//!
//! Each test owns its temp dirs, and the tests share nothing.

mod support;

use std::any::Any;
use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::ops::RangeInclusive;
use std::os::unix::fs::MetadataExt;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use camino::{Utf8Path, Utf8PathBuf};
use serde_json::{Value, json};
use support::{Racers, Run, Store, agent, run, run_within};
use swarmops_core::{EntryKind, ObjectKind, RepoPath, Signature, hash_object};
use swarmops_ops::NOW_ENV;
use swarmops_repo::{Change, ChangeOp, CommitRequest, LocalRepo};
use swarmops_store::{CRASH_AT_ENV, FS_CALLS_ENV, FsOps, Layout, LinkOutcome, MetaLock, RealFs};

/// How many children the commit tests race.
const COMMITTERS: usize = 8;

/// How many children race each round of the ref test.
const RACERS: usize = 16;

/// How many rounds the ref test races: the first creates main, each other one moves it.
const ROUNDS: u64 = 4;

/// How many runs of a crash sweep go at once, each with a store of its own.
const AT_ONCE: usize = 8;

/// How many frames lie past the checkpoint in the commit sweep's template: the frame of the
/// commit the sweep crashes is the 32nd, so its transaction also syncs the index and moves the
/// checkpoint, and the sweep crashes those calls too.
const TEMPLATE_FRAMES: u64 = 31;

/// The commit the sweep crashes: three files in new nested directories on main, so that it
/// reads main's tree and writes blobs, trees and its changeset.
const CRASH_PUTS: [(&str, &str); 3] = [
    ("src/lib.rs", "pub fn f() {}\n"),
    ("src/bin/tool.rs", "fn main() {}\n"),
    ("docs/notes.md", "# notes\n"),
];

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
/// then the winners, and its version counts the rounds.
#[test]
fn sixteen_processes_race_ref_set_exactly_one_wins_per_round() {
    let store = Store::init();
    let mut main: Option<String> = None;
    let mut winners = Vec::new();
    for round in 0..ROUNDS {
        // Each racer's changeset, on main as it is (none in the first round), made at once.
        let mut commits = Racers::new(store.tmp());
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
            commits.spawn(
                &store.args(&agent(racer), &args),
                &[("SWARMOPS_RUN_ID", &run_id)],
            );
        }
        let ids: Vec<String> = commits
            .race()
            .iter()
            .map(|run| run.ok()["changeset"].as_str().unwrap().to_owned())
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
// Crashes.
// ---------------------------------------------------------------------------------------------

/// The time the crash tests sign at: November 2023, in Unix seconds.
const NOW: u64 = 1_700_000_000;

/// `SWARMOPS_TEST_NOW=<unix seconds>`: a commit signs its changeset at that time, not the
/// clock's, so that the same commit made twice is the same changeset, whenever it runs; unset,
/// the time is the clock's.
#[test]
fn swarmops_test_now_fixes_the_signature_time() {
    let template = Store::init();
    let now = NOW.to_string();
    let ids: Vec<String> = (0..2)
        .map(|run| {
            let store = Store::copy_of(&template);
            let changes = store.changes("same", "same.txt", "the same change\n");
            let args = ["commit", "-m", "same", "--changes", &changes];
            let result = store.run_as(&agent(0), &args, &[(NOW_ENV, &now)]).ok();
            let id = result["changeset"].as_str().unwrap().to_owned();
            let shown = store.show(&id);
            assert_eq!(shown["created_at"], NOW * 1000, "run {run}");
            for signature in ["author", "committer"] {
                assert_eq!(shown["signatures"][signature]["secs"], NOW, "run {run}");
            }
            store.assert_clean(&format!("run {run}"));
            id
        })
        .collect();
    assert_eq!(ids[0], ids[1], "one commit, at one time: one changeset");

    let store = Store::copy_of(&template);
    let before = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let id = store.commit(&agent(0), "clock.txt", "the clock\n", None);
    let secs = store.show(&id)["signatures"]["committer"]["secs"]
        .as_u64()
        .unwrap();
    assert!(secs >= before, "unset, the clock's time: {secs} < {before}");
    store.assert_clean("signed at the clock's time");
}

/// Runs `run(k)` for every k of `ks`, [`AT_ONCE`] at a time, each on a thread of its own; the
/// outcomes, in the order of `ks`.
fn sweep<T: Send>(ks: RangeInclusive<u64>, run: impl Fn(u64) -> T + Sync) -> Vec<T> {
    let ks: Vec<u64> = ks.collect();
    let run = &run;
    let mut outcomes = Vec::with_capacity(ks.len());
    for batch in ks.chunks(AT_ONCE) {
        thread::scope(|scope| {
            let runs: Vec<_> = batch.iter().map(|&k| scope.spawn(move || run(k))).collect();
            for handle in runs {
                let outcome = handle
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
                outcomes.push(outcome);
            }
        });
    }
    outcomes
}

/// The number a dry run wrote to `file`: how many filesystem calls it made.
fn reported_calls(file: &Path) -> u64 {
    let text = fs::read_to_string(file).unwrap_or_else(|e| {
        panic!(
            "{}: the run reported no filesystem call count ({e})",
            file.display()
        )
    });
    text.trim()
        .parse()
        .unwrap_or_else(|e| panic!("{text:?} is not a call count: {e}"))
}

/// The file a run of `store`'s writes its call count to.
fn calls_file(store: &Store) -> PathBuf {
    store.tmp().join("fs-calls")
}

/// `meta/journal.ckpt` of `store`: how far the checkpoint is.
fn checkpoint(store: &Store) -> u64 {
    let layout = Layout::new(Utf8PathBuf::from(store.path()));
    let bytes = fs::read(layout.journal_ckpt()).unwrap();
    u64::from_le_bytes(bytes.try_into().expect("8 bytes"))
}

/// The length of `store`'s `meta/journal.log`.
fn journal_len(store: &Store) -> u64 {
    let layout = Layout::new(Utf8PathBuf::from(store.path()));
    fs::metadata(layout.journal_log()).unwrap().len()
}

/// A store for the commit sweep: main at a first changeset, then changesets on it, until
/// [`TEMPLATE_FRAMES`] frames lie past the checkpoint (which has passed none). Every command
/// on it, and on its copies, signs at [`NOW`], so the store and every commit on it are the same
/// in every run.
fn commit_template() -> Store {
    let store = Store::init().with_env(&[(NOW_ENV, &NOW.to_string())]);
    let first = store.commit(&agent(0), "README.md", "# crash test\n", None);
    let create = store.run_as(&agent(0), &["ref", "set", "main", &first, "--create"], &[]);
    create.ok();
    for n in 2..TEMPLATE_FRAMES {
        store.commit(
            &agent(0),
            &format!("history/{n}.txt"),
            &format!("{n}\n"),
            None,
        );
    }
    assert_eq!(checkpoint(&store), 0);
    store
}

/// Runs the commit the sweep crashes on `store`, with `env` set.
fn crash_commit(store: &Store, env: &[(&str, &str)]) -> Run {
    let changes = store.changes_of("crash", &CRASH_PUTS);
    store.run_as(
        &agent(1),
        &["commit", "-m", "crash", "--changes", &changes],
        env,
    )
}

/// What the crash commit does when nothing crashes it.
#[derive(Debug, PartialEq, Eq)]
struct DryRun {
    /// How many filesystem calls it makes.
    calls: u64,
    /// The changeset it commits.
    changeset: String,
}

/// Runs the crash commit on a copy of `template` without a crash.
fn dry_commit(template: &Store) -> DryRun {
    let store = Store::copy_of(template);
    let file = calls_file(&store);
    let result = crash_commit(&store, &[(FS_CALLS_ENV, file.to_str().unwrap())]).ok();
    assert_eq!(
        checkpoint(&store),
        journal_len(&store),
        "the 32nd frame past the checkpoint moves it to the journal's end"
    );
    DryRun {
        calls: reported_calls(&file),
        changeset: result["changeset"].as_str().unwrap().to_owned(),
    }
}

/// Waits until the clock's second changes: a run after it signs at another second, unless
/// something fixes the time.
fn wait_for_the_next_second() {
    let second = || {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    };
    let start = second();
    while second() == start {
        thread::sleep(Duration::from_millis(10));
    }
}

/// What one run of the commit sweep found.
struct Crashed {
    /// The commit ran to its end: k was past its last call.
    finished: bool,
    /// The store holds its changeset.
    committed: bool,
    /// The temp files the crash left in `objects/tmp/`.
    leftovers: usize,
}

/// One run of the commit sweep: the crash commit on a copy of `template`, its k-th filesystem
/// call aborting it. It makes the dry run's calls, in the same order, so it must abort when k is
/// one of them, and otherwise commit the dry run's changeset after the dry run's calls. The
/// store must then verify clean (the temp files a crash leaves in `objects/tmp/` are no
/// objects), hold the changeset or not, and take a fresh commit, which `show` finds.
fn crash_commit_at(template: &Store, k: u64, dry: &DryRun) -> Crashed {
    let store = Store::copy_of(template);
    let file = calls_file(&store);
    let crash_at = k.to_string();
    let env = [
        (CRASH_AT_ENV, crash_at.as_str()),
        (FS_CALLS_ENV, file.to_str().unwrap()),
    ];
    let crash = crash_commit(&store, &env);
    let finished = k > dry.calls;
    if finished {
        let result = crash.ok();
        assert_eq!(result["changeset"], dry.changeset.as_str(), "k={k}");
        let made = reported_calls(&file);
        assert_eq!(
            made, dry.calls,
            "k={k}: the dry run's calls, no more and no fewer"
        );
    } else {
        assert!(crash.aborted(), "k={k} of {} calls: {crash}", dry.calls);
        assert!(!file.exists(), "k={k}: a run that crashes reports nothing");
    }
    let leftovers = store.temp_files();
    store.assert_clean(&format!("k={k}: after the crash"));

    let before = TEMPLATE_FRAMES - 1; // one of the template's frames moved main
    let changesets = store.info()["changesets"].as_u64().unwrap();
    let new = changesets.checked_sub(before).unwrap_or_else(|| {
        panic!("k={k}: {changesets} changesets, fewer than the template's {before}")
    });
    let committed = match new {
        0 => false,
        1 => true,
        n => panic!("k={k}: {n} new changesets"),
    };
    assert!(
        committed || !finished,
        "k={k}: it returned, but is not stored"
    );
    // The changeset the store holds, if any, is the dry run's.
    let shown = store.read(&["show", &dry.changeset]);
    if committed {
        shown.ok();
    } else {
        shown.err("E_REV_NOT_FOUND");
    }

    let next = format!("after a crash at call {k}\n");
    let fresh = store.commit(&agent(2), "after-crash.txt", &next, None);
    let shown = store.show(&fresh);
    assert_eq!(shown["changes"][0]["path"], "after-crash.txt", "k={k}");
    assert_eq!(store.info()["changesets"], changesets + 1, "k={k}");
    store.assert_clean(&format!("k={k}: after the next commit"));
    Crashed {
        finished,
        committed,
        leftovers,
    }
}

/// A commit crashed at each of its filesystem calls in turn, from the first to past the last
/// (k = 1..=N + 1, N counted by a dry run): after each crash the store verifies clean, and the
/// next commit succeeds and is shown. The commit's frame is the 32nd past the checkpoint, so
/// the sweep crashes its index sync and checkpoint too. Every run signs at [`NOW`], so every
/// run makes the same calls, in the same order: two dry runs, in different seconds of the
/// clock, must count the same calls and commit the same changeset.
#[test]
#[ignore = "slow, ~20 s: ~90 crashed commits; just check-slow runs it"]
fn kill9_during_commit_store_verifies_and_next_commit_succeeds() {
    let template = commit_template();
    let dry = dry_commit(&template);
    wait_for_the_next_second();
    assert_eq!(
        dry_commit(&template),
        dry,
        "one call sequence for every run"
    );
    assert!(dry.calls > 30, "a commit makes dozens of calls: {dry:?}");

    let crashes = sweep(1..=dry.calls + 1, |k| crash_commit_at(&template, k, &dry));
    let after_commit_point = crashes
        .iter()
        .filter(|crash| crash.committed && !crash.finished)
        .count();
    assert!(
        after_commit_point > 0,
        "some crash came after the journal append: the metadata store's calls count too"
    );
    assert!(
        crashes.iter().any(|crash| !crash.committed),
        "some before it"
    );
    assert!(
        crashes.iter().any(|crash| crash.leftovers > 0),
        "some crash left a temp file in objects/tmp, which verify passed over"
    );
}

/// Runs `init` on a fresh path without a crash; how many filesystem calls it made.
fn dry_init() -> u64 {
    let store = Store::empty();
    let file = calls_file(&store);
    run(
        &["init", store.path()],
        &[(FS_CALLS_ENV, file.to_str().unwrap())],
    )
    .ok();
    reported_calls(&file)
}

/// One run of the init sweep: `init` on a fresh path, its k-th filesystem call (of the
/// `calls` a dry run counted) aborting it. Then either `info` says `E_NOT_A_REPO` and a second
/// `init` succeeds, or `info` succeeds (and a second `init` says `E_EXISTS`); either way the
/// store is fresh and verifies clean. Returns whether `info` found the store right after the
/// crash.
fn init_crashing_at(k: u64, calls: u64) -> bool {
    let store = Store::empty();
    let crash_at = k.to_string();
    let init = run(
        &["init", store.path()],
        &[(CRASH_AT_ENV, crash_at.as_str())],
    );
    if k <= calls {
        assert!(init.aborted(), "k={k}: {init}");
    } else {
        init.ok();
    }
    let info = store.read(&["info"]);
    let whole = info.exit() == 0;
    if whole {
        run(&["init", store.path()], &[]).err("E_EXISTS");
    } else {
        info.err("E_NOT_A_REPO");
        run(&["init", store.path()], &[]).ok();
    }
    let info = store.info();
    let fresh = (info["name"].as_str(), info["changesets"].as_u64());
    assert_eq!(fresh, (Some("store"), Some(0)), "k={k}");
    store.assert_clean(&format!("k={k}"));
    whole
}

/// `init` crashed at each of its filesystem calls in turn (`LocalRepo::create`, with
/// `MetaStore::create` inside it, then the first snapshot `init` reports from), from the first
/// to past the last: before `FORMAT` is linked there is no store and a second `init` makes it;
/// from then on the store is whole.
#[test]
#[ignore = "slow, ~25 s: ~300 crashed inits; just check-slow runs it"]
fn kill9_during_init_leaves_no_store_or_a_whole_one() {
    let calls = dry_init();
    assert!(
        calls > 256,
        "init makes 256 fan-out directories alone: {calls}"
    );
    let whole = sweep(1..=calls + 1, |k| init_crashing_at(k, calls));
    let first = whole
        .iter()
        .position(|&whole| whole)
        .expect("past the last call, init finishes");
    assert!(first > 256, "a store before its objects/ is made: {first}");
    assert!(
        whole[first..].iter().all(|&whole| whole),
        "once FORMAT is linked, the store is whole: {whole:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// A failed index sync (storage spec §3, the batched sync).
// ---------------------------------------------------------------------------------------------

/// The real filesystem, except that the first `sync_data` of a file named `name` fails: what
/// a writeback error looks like to the store, reported once, to the next sync after it.
struct FailFirstSync {
    name: &'static str,
    failed: AtomicBool,
}

impl FailFirstSync {
    fn of(name: &'static str) -> FailFirstSync {
        FailFirstSync {
            name,
            failed: AtomicBool::new(false),
        }
    }
}

impl FsOps for FailFirstSync {
    fn create_dir_all(&self, p: &Path) -> io::Result<()> {
        RealFs.create_dir_all(p)
    }

    fn write_durable_temp(&self, dir: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
        RealFs.write_durable_temp(dir, bytes)
    }

    fn link_noclobber(&self, from: &Path, to: &Path) -> io::Result<LinkOutcome> {
        RealFs.link_noclobber(from, to)
    }

    fn fsync_dir(&self, p: &Path) -> io::Result<()> {
        RealFs.fsync_dir(p)
    }

    fn read(&self, p: &Path) -> io::Result<Option<Vec<u8>>> {
        RealFs.read(p)
    }

    fn exists(&self, p: &Path) -> io::Result<bool> {
        RealFs.exists(p)
    }

    fn remove(&self, p: &Path) -> io::Result<()> {
        RealFs.remove(p)
    }

    fn append_durable(&self, path: &Path, bytes: &[u8]) -> io::Result<u64> {
        RealFs.append_durable(path, bytes)
    }

    fn file_len(&self, path: &Path) -> io::Result<u64> {
        RealFs.file_len(path)
    }

    fn truncate(&self, path: &Path, len: u64) -> io::Result<()> {
        RealFs.truncate(path, len)
    }

    fn atomic_replace(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        RealFs.atomic_replace(path, bytes)
    }

    fn sync_data(&self, path: &Path) -> io::Result<()> {
        let named = path.file_name().is_some_and(|name| name == self.name);
        if named && !self.failed.swap(true, Ordering::SeqCst) {
            return Err(io::Error::other(format!(
                "injected: the writeback of {} failed",
                self.name
            )));
        }
        RealFs.sync_data(path)
    }

    fn append(&self, path: &Path, bytes: &[u8]) -> io::Result<u64> {
        RealFs.append(path, bytes)
    }

    fn rename_durable(&self, from: &Path, to: &Path) -> io::Result<()> {
        RealFs.rename_durable(from, to)
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
    assert_eq!(checkpoint(&store), 0, "the checkpoint did not move");
    let id = outcome.id.to_string();
    assert_eq!(store.show(&id)["seq"], BEFORE);
    store.assert_clean("after the failed sync");

    store.commit(&agent(0), "next.txt", "next\n", None);
    assert_eq!(fs::metadata(&ids_col).unwrap().len(), (BEFORE + 2) * 32);
    assert_eq!(checkpoint(&store), journal_len(&store));
    assert_eq!(store.info()["changesets"], BEFORE + 2);
    assert_eq!(store.show(&id)["seq"], BEFORE);
    store.assert_clean("after the next commit");
}
