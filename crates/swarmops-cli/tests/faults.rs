//! `swarmops` processes whose filesystem calls fail (storage spec §2 rule 3, §3; operations spec
//! §1): the error sweeps through the real binary. They drive the test hooks, so this test target
//! requires the `test-hooks` feature, which the workspace's gates turn on (`--all-features`).
//!
//! - A failure is `SWARMOPS_TEST_FAIL_AT=<k>:<mode>:<errno>`: the store's k-th filesystem call,
//!   counted as `SWARMOPS_TEST_CRASH_AT` counts them, fails with an injected I/O error, `once`
//!   (that call alone) or `persistent` (every mutating call from it on, reads aside). `eio` lets a
//!   call that flushes do its work and then fails it, as a failed writeback does; `enospc`, and
//!   `eio` on any other call, fails it before it does anything.
//! - A sweep fails each call of a dry run in turn (k = 1..=N, N counted with
//!   `SWARMOPS_TEST_FS_CALLS`), each run on a store of its own; commits sign at a fixed time
//!   (`SWARMOPS_TEST_NOW`), so that call k is the same call in every run. Every run is checked
//!   against the store's write-ordering rules (`SWARMOPS_TEST_ORDER_CHECK=1`, set by the test
//!   support).
//! - The oracle: a run that succeeds committed, and its change is visible and verifies clean
//!   once the fault clears; a run that fails either committed nothing or says that it did (a
//!   failure after the commit point is a warning in a successful response, and an `init` that
//!   made the store says so in `details.created`): never a commit behind a plain error. The store
//!   always opens again and takes the next commit. No run ends but with its envelope, save the
//!   documented aborts, which say why on stderr: a failed journal append whose cut back off the
//!   journal fails too, and a failed sync of the store root after `init` linked `FORMAT` whose
//!   taking `FORMAT` back fails too.
//!
//! Each test owns its temp dirs, and the tests share nothing.

mod support;

use std::fs;
use std::ops::RangeInclusive;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use serde_json::Value;
use support::{Run, Store, agent, run};
use swarmops_ops::NOW_ENV;
use swarmops_store::{FAIL_AT_ENV, FS_CALLS_ENV};

/// How many runs of a sweep go at once, each with a store of its own.
const AT_ONCE: usize = 8;

/// The time the sweeps sign at: November 2023, in Unix seconds.
const NOW: u64 = 1_700_000_000;

/// How many frames lie past the checkpoint in the commit sweep's template: the swept commit's
/// frame is the 32nd, so it also syncs the index and moves the checkpoint.
const TEMPLATE_FRAMES: u64 = 31;

/// The commit the sweep fails: files in new nested directories on main.
const SWEPT_PUTS: [(&str, &str); 3] = [
    ("src/lib.rs", "pub fn f() {}\n"),
    ("src/bin/tool.rs", "fn main() {}\n"),
    ("docs/notes.md", "# notes\n"),
];

/// The failure modes and errnos the commit sweep runs each call with.
const FAILURES: [&str; 4] = [
    "once:eio",
    "once:enospc",
    "persistent:eio",
    "persistent:enospc",
];

/// Those the init sweep runs each call with: most of `init`'s calls make directories, which
/// `EIO` and `ENOSPC` alike fail before they do anything, so a persistent `ENOSPC` adds nothing
/// that a persistent `EIO` and a single `ENOSPC` do not already cover.
const INIT_FAILURES: [&str; 3] = ["once:eio", "once:enospc", "persistent:eio"];

/// What the documented aborts say on stderr: a failed journal append whose cut failed too; a
/// failed sync of the store root after `init` linked `FORMAT`, whose taking back failed too.
const ABORTS: [&str; 2] = ["could not be cut back off it", "could not be taken back"];

/// Runs `run(k)` for every k of `ks`, [`AT_ONCE`] at a time, each on a thread of its own.
fn sweep(ks: RangeInclusive<u64>, run: impl Fn(u64) + Sync) {
    let ks: Vec<u64> = ks.collect();
    let run = &run;
    for batch in ks.chunks(AT_ONCE) {
        thread::scope(|scope| {
            let runs: Vec<_> = batch.iter().map(|&k| scope.spawn(move || run(k))).collect();
            for handle in runs {
                handle
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
            }
        });
    }
}

/// The number a dry run wrote to `file`: how many filesystem calls it made.
fn reported_calls(file: &Path) -> u64 {
    let text = fs::read_to_string(file)
        .unwrap_or_else(|e| panic!("{}: no call count ({e})", file.display()));
    text.trim().parse().unwrap()
}

/// How a run with an injected failure ended.
#[derive(Debug)]
enum Ended {
    /// It succeeded, with this result.
    Succeeded(Value),
    /// It failed, with this error: never `E_INTERNAL`, a panic (the order checker's included).
    Failed(Value),
    /// It aborted, as documented ([`ABORTS`]).
    Aborted,
}

/// How `run` ended, checked: an envelope, or the documented abort.
fn ended(run: &Run) -> Ended {
    if run.aborted() {
        assert!(
            ABORTS.iter().any(|abort| run.stderr.contains(abort)),
            "an abort not documented: {run}"
        );
        return Ended::Aborted;
    }
    if run.exit() == 0 {
        return Ended::Succeeded(run.ok());
    }
    let mut envelope = run.envelope();
    let error = envelope["error"].take();
    assert_ne!(error["code"], "E_INTERNAL", "a panic: {run}");
    if error["code"] == "E_IO_TRANSIENT" {
        assert_eq!(error["details"]["committed"], false, "{run}");
        assert_eq!(error["retryable"], true, "{run}");
    }
    Ended::Failed(error)
}

/// A store for the commit sweep: main at a first changeset, then changesets on it, until
/// [`TEMPLATE_FRAMES`] frames lie past the checkpoint; every command on it, and on its copies,
/// signs at [`NOW`].
fn commit_template() -> Store {
    let store = Store::init().with_env(&[(NOW_ENV, &NOW.to_string())]);
    let first = store.commit(&agent(0), "README.md", "# error sweep\n", None);
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
    assert_eq!(store.checkpoint(), 0);
    store
}

/// Runs the swept commit on `store`, with `env` set.
fn swept_commit(store: &Store, env: &[(&str, &str)]) -> Run {
    let changes = store.changes_of("swept", &SWEPT_PUTS);
    store.run_as(
        &agent(1),
        &["commit", "-m", "swept", "--changes", &changes],
        env,
    )
}

/// One run of the commit sweep: the swept commit on a copy of `template`, failing as `failure`
/// says from call `k`. Then, the fault cleared: the store opens and verifies clean; it holds the
/// commit's changeset (`changeset`, the dry run's) exactly when the run succeeded (either, after
/// the documented abort); and it takes the next commit. Returns how the run ended.
fn commit_failing_at(template: &Store, k: u64, failure: &str, changeset: &str) -> Ended {
    let store = Store::copy_of(template);
    let what = format!("call {k} failing {failure}");
    let fail_at = format!("{k}:{failure}");
    let run = swept_commit(&store, &[(FAIL_AT_ENV, fail_at.as_str())]);
    let ended = ended(&run);
    if let Ended::Succeeded(result) = &ended {
        assert_eq!(result["changeset"], changeset, "{what}: {run}");
    }
    store.assert_clean(&format!("{what}: after the failed run"));
    let shown = store.read(&["show", changeset]);
    match ended {
        Ended::Succeeded(_) => {
            shown.ok();
        }
        Ended::Failed(_) => {
            shown.err("E_REV_NOT_FOUND");
        }
        Ended::Aborted => {}
    }
    let before = store.info()["changesets"].as_u64().unwrap();
    store.commit(&agent(2), "after.txt", &format!("after {what}\n"), None);
    assert_eq!(store.info()["changesets"], before + 1, "{what}");
    store.assert_clean(&format!("{what}: after the next commit"));
    ended
}

/// One run of the init sweep: `init` on a fresh path, failing as `failure` says from call `k`.
/// Then, the fault cleared: a store is there, whole, exactly when the run succeeded or said it
/// made one (`details.created`; either after the documented abort); without one, a second `init`
/// makes it, and with one, says `E_EXISTS`. Either way the store opens and verifies clean.
fn init_failing_at(k: u64, failure: &str) -> Ended {
    let store = Store::empty();
    let what = format!("init, call {k} failing {failure}");
    let fail_at = format!("{k}:{failure}");
    let init = run(&["init", store.path()], &[(FAIL_AT_ENV, fail_at.as_str())]);
    let ended = ended(&init);
    let info = store.read(&["info"]);
    let whole = info.exit() == 0;
    match &ended {
        Ended::Succeeded(result) => {
            assert!(whole, "{what}: it succeeded, but {info}");
            assert_eq!(result["changesets"], 0, "{what}: a fresh store");
        }
        Ended::Failed(error) => assert_eq!(
            whole,
            error["details"]["created"] == true,
            "{what}: the store is there exactly when the error says it was made: {init}\n{info}"
        ),
        Ended::Aborted => {}
    }
    if whole {
        run(&["init", store.path()], &[]).err("E_EXISTS");
    } else {
        info.err("E_NOT_A_REPO");
        run(&["init", store.path()], &[]).ok();
    }
    let info = store.info();
    assert_eq!(info["name"], "store", "{what}");
    store.assert_clean(&what);
    ended
}

/// An injected I/O error at any filesystem call of `init` and of a `commit` that moves the
/// checkpoint is reported honestly (Review Focus 3): each call of a dry run fails in turn, once
/// and persistently, with `EIO` and with `ENOSPC` ([`FAILURES`], [`INIT_FAILURES`]). A success
/// committed, visibly and cleanly; a failure committed nothing or says it did; the store always
/// opens again; no run panics, and none aborts but as documented ([`ABORTS`]).
#[test]
#[ignore = "slow, ~3 min: ~1,300 failed runs of init and commit; just check-slow runs it"]
fn fail_at_every_call_reports_honestly() {
    let template = commit_template();
    let dry = Store::copy_of(&template);
    let file = dry.tmp().join("fs-calls");
    let result = swept_commit(&dry, &[(FS_CALLS_ENV, file.to_str().unwrap())]).ok();
    let changeset = result["changeset"].as_str().unwrap().to_owned();
    let calls = reported_calls(&file);
    assert!(calls > 30, "a commit makes dozens of calls: {calls}");
    for failure in FAILURES {
        sweep(1..=calls, |k| {
            commit_failing_at(&template, k, failure, &changeset);
        });
    }

    let empty = Store::empty();
    let file = empty.tmp().join("fs-calls");
    run(
        &["init", empty.path()],
        &[(FS_CALLS_ENV, file.to_str().unwrap())],
    )
    .ok();
    let calls = reported_calls(&file);
    assert!(
        calls > 256,
        "init makes 256 fan-out directories alone: {calls}"
    );
    for failure in INIT_FAILURES {
        sweep(1..=calls, |k| {
            init_failing_at(k, failure);
        });
    }
}

/// `init` failing once with `EIO` at each of its last calls: the sync of the store root that
/// makes `FORMAT` durable (its work done, then failing, as a failed writeback does), and the reads
/// of the new store it reports from. An `init` whose root sync fails takes `FORMAT` back (no
/// store, and a second `init` makes it); one whose reads fail says it made the store
/// (`details.created`). In the default gate; [`fail_at_every_call_reports_honestly`] sweeps
/// every call.
#[test]
fn an_init_failing_at_its_last_calls_reports_honestly() {
    let empty = Store::empty();
    let file = empty.tmp().join("fs-calls");
    run(
        &["init", empty.path()],
        &[(FS_CALLS_ENV, file.to_str().unwrap())],
    )
    .ok();
    let calls = reported_calls(&file);
    let (created, taken_back) = (AtomicUsize::new(0), AtomicUsize::new(0));
    sweep(calls - 7..=calls, |k| {
        if let Ended::Failed(error) = init_failing_at(k, "once:eio") {
            let counter = if error["details"]["created"] == true {
                &created
            } else {
                &taken_back
            };
            counter.fetch_add(1, Ordering::Relaxed);
        }
    });
    assert!(
        created.load(Ordering::Relaxed) > 0,
        "a read after the store was made failed"
    );
    assert!(
        taken_back.load(Ordering::Relaxed) > 0,
        "the root's sync after FORMAT failed"
    );
}
