//! `swarmops` processes whose filesystem calls fail (storage spec §2 rule 3, §3; operations spec
//! §1): the error sweeps through the real binary. They drive the test hooks, so `all.rs` holds
//! this module only with the `test-hooks` feature, which the workspace's gates turn on
//! (`--all-features`).
//!
//! - A failure is `SWARMOPS_TEST_FAIL_AT=<k>:<mode>:<errno>`: the store's k-th filesystem call,
//!   counted as `SWARMOPS_TEST_CRASH_AT` counts them, fails with an injected I/O error, `once`
//!   (that call alone) or `persistent` (every mutating call from it on, reads aside). `eio` lets a
//!   call that flushes do its work and then fails it, as a failed writeback does; `enospc`, and
//!   `eio` on any other call, fails it before it does anything.
//! - A sweep fails each call of a dry run in turn (k = 1..=N, N counted with
//!   `SWARMOPS_TEST_FS_CALLS`), each run on a store of its own; commits sign at a fixed time
//!   (`SWARMOPS_TEST_NOW`), so that call k is the same call in every run. The runner, the dry
//!   runs and the commit template are the crash sweeps' too (`support::sweep`). Every run is
//!   checked against the store's write-ordering rules (`SWARMOPS_TEST_ORDER_CHECK=1`, set by the
//!   test support).
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

use std::io;
use std::os::unix::process::ExitStatusExt;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use swarmops_store::FAIL_AT_ENV;
use swarmops_testkit::child::SIGABRT;

use crate::support::sweep::{
    NOW, SWEPT_PUTS, commit_template, dry_commit, dry_init, init_again, sweep, swept_commit,
};
use crate::support::{self, Run, Store, agent, run};

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

/// How `run` on the store at `store` ended, checked: an envelope, or the documented abort. A
/// filesystem failure (`E_IO`, `E_IO_TRANSIENT`) names the file operation that failed
/// (`details.fs_op`, one of the store's) and, where it was on a file of the store, that file
/// relative to the store's root (`details.store_file`: never absolute, never with `..`). No
/// error, and no warning of a run that succeeded, carries the store's path, but where an `init`
/// that made the store says so: that message and its hint name the path the client gave.
fn ended(run: &Run, store: &str) -> Ended {
    if run.aborted() {
        assert!(
            ABORTS.iter().any(|abort| run.stderr.contains(abort)),
            "an abort not documented: {run}"
        );
        return Ended::Aborted;
    }
    if run.exit() == 0 {
        let result = run.ok();
        assert!(
            !result["warnings"].to_string().contains(store),
            "a host path in a warning: {run}"
        );
        return Ended::Succeeded(result);
    }
    let mut envelope = run.envelope();
    let error = envelope["error"].take();
    assert_ne!(error["code"], "E_INTERNAL", "a panic: {run}");
    if error["code"] == "E_IO_TRANSIENT" {
        assert_eq!(error["details"]["committed"], false, "{run}");
        assert_eq!(error["retryable"], true, "{run}");
    }
    if error["code"] == "E_IO" || error["code"] == "E_IO_TRANSIENT" {
        let op = error["details"]["fs_op"].as_str();
        let ops = swarmops_store::FileOp::ALL.map(swarmops_store::FileOp::as_str);
        assert!(op.is_some_and(|op| ops.contains(&op)), "{run}");
        if let Some(file) = error["details"].get("store_file") {
            let file = camino::Utf8Path::new(file.as_str().unwrap_or("/"));
            let parent = camino::Utf8Component::ParentDir;
            assert!(
                file.is_relative() && !file.components().any(|part| part == parent),
                "{run}"
            );
        }
    }
    let mut seen = error.clone();
    if error["details"]["created"] == true {
        let made = format!("the store at {store} is made, but reading it back failed: ");
        let message = error["message"].as_str().unwrap_or_default();
        let read = message.strip_prefix(&made);
        seen["message"] = read.unwrap_or_else(|| panic!("{run}")).into();
        seen["hint"] = Value::Null;
    }
    assert!(
        !seen.to_string().contains(store),
        "a host path in the error: {run}"
    );
    Ended::Failed(error)
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
    let ended = ended(&run, store.path());
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
/// makes it, and with one, says `E_EXISTS`. Either way the store is fresh, and verifies clean
/// ([`init_again`]).
fn init_failing_at(k: u64, failure: &str) -> Ended {
    let store = Store::empty();
    let what = format!("init, call {k} failing {failure}");
    let fail_at = format!("{k}:{failure}");
    let init = run(&["init", store.path()], &[(FAIL_AT_ENV, fail_at.as_str())]);
    let ended = ended(&init, store.path());
    let whole = init_again(&store, &what);
    match &ended {
        Ended::Succeeded(result) => {
            assert!(whole, "{what}: it succeeded, but left no store: {init}");
            assert_eq!(result["changesets"], 0, "{what}: a fresh store");
        }
        Ended::Failed(error) => assert_eq!(
            whole,
            error["details"]["created"] == true,
            "{what}: the store is there exactly when the error says it was made: {init}"
        ),
        Ended::Aborted => {}
    }
    ended
}

/// Whether a run, failing as its first argument says, ended as its second says.
type EndedSo = fn(&str, &Ended) -> bool;

/// The outcomes the commit sweep exists to reach: a failed journal append, cut back off the
/// journal and reported as `E_IO_TRANSIENT` that did not commit; a failed listing of the
/// committed frame in `journal.off`, a success whose warning names it; and, under a persistent
/// failure, the documented abort of a failed append whose cut fails too.
const COMMIT_OUTCOMES: [(&str, EndedSo); 3] = [
    ("E_IO_TRANSIENT, not committed", |_, ended| {
        matches!(ended, Ended::Failed(error)
            if error["code"] == "E_IO_TRANSIENT" && error["details"]["committed"] == false)
    }),
    (
        "a success with a warning that names journal.off",
        |_, ended| matches!(ended, Ended::Succeeded(result) if warns_of(result, "journal.off")),
    ),
    (
        "a documented abort under a persistent failure",
        |failure, ended| failure.starts_with("persistent:") && matches!(ended, Ended::Aborted),
    ),
];

/// Whether one of the warnings of a successful `result` names `what`.
fn warns_of(result: &Value, what: &str) -> bool {
    result["warnings"].as_array().is_some_and(|warnings| {
        (warnings.iter()).any(|warning| warning.as_str().is_some_and(|w| w.contains(what)))
    })
}

/// Which of [`COMMIT_OUTCOMES`] no run of `ended` reached, each run with the failure it ran
/// with. A change to the commit path could take one out of the sweep unseen, every run still
/// ending honestly.
fn unreached(ended: &[(&str, Ended)]) -> Vec<&'static str> {
    COMMIT_OUTCOMES
        .into_iter()
        .filter(|(_, so)| !ended.iter().any(|(failure, ended)| so(failure, ended)))
        .map(|(what, _)| what)
        .collect()
}

/// An injected I/O error at any filesystem call of `init` and of a `commit` that moves the
/// checkpoint is reported honestly (Review Focus 3): each call of a dry run fails in turn, once
/// and persistently, with `EIO` and with `ENOSPC` ([`FAILURES`], [`INIT_FAILURES`]). A success
/// committed, visibly and cleanly; a failure committed nothing or says it did; the store always
/// opens again; no run panics, and none aborts but as documented ([`ABORTS`]). The dry run of
/// the commit asserts that it moves the checkpoint, so that the sweep fails the index sync and
/// the checkpoint's move too; and the commit sweep reaches each outcome it exists for
/// ([`COMMIT_OUTCOMES`]).
#[test]
#[ignore = "slow, ~3 min: ~1,300 failed runs of init and commit; just check-slow runs it"]
fn fail_at_every_call_reports_honestly() {
    let template = commit_template();
    let dry = dry_commit(&template);
    let mut ended = Vec::new();
    for failure in FAILURES {
        let runs = sweep(1..=dry.calls, |k| {
            commit_failing_at(&template, k, failure, &dry.changeset)
        });
        ended.extend(runs.into_iter().map(|run| (failure, run)));
    }
    let missing = unreached(&ended);
    assert!(
        missing.is_empty(),
        "no run of the commit sweep ended so: {missing:?}"
    );

    let calls = dry_init();
    let mut aborted = false;
    for failure in INIT_FAILURES {
        let runs = sweep(1..=calls, |k| init_failing_at(k, failure));
        aborted |= failure.starts_with("persistent:")
            && runs.iter().any(|run| matches!(run, Ended::Aborted));
    }
    assert!(
        aborted,
        "no init run ended in the documented abort: the root sync failed and FORMAT could not be taken back"
    );
}

/// The commit sweeps' dry run, in the default gate: the swept commit, on a copy of the
/// template, moves the checkpoint ([`dry_commit`] asserts it), so the commit sweeps of the slow
/// suite ([`fail_at_every_call_reports_honestly`], and `crash.rs`'s
/// `kill9_during_commit_store_verifies_and_next_commit_succeeds`) fail and crash the index sync
/// and the checkpoint's move too. A change to the checkpoint's batching or to the template fails
/// here at once, not in the slow suite alone.
#[test]
fn the_commit_sweeps_reach_the_index_sync_and_the_checkpoint() {
    dry_commit(&commit_template());
}

/// The commit sweep for a publish, `commit --advance main`: its changeset and main's move are
/// one journal frame, so whatever a run with an injected failure ends with, the store holds both
/// or neither.
mod advance {
    use serde_json::json;
    use swarmops_store::FS_CALLS_ENV;

    use super::*;
    use crate::support::sweep::{DryRun, calls_file, reported_calls};

    /// [`swept_commit`]'s change on `base`, main's head, advancing main, as `agent(1)` with `env`
    /// set: the commit the advancing sweep fails. The changeset is the swept commit's.
    fn swept_advance(store: &Store, base: &str, env: &[(&str, &str)]) -> Run {
        let changes = store.changes_of("swept", &SWEPT_PUTS);
        let args = [
            "commit",
            "-m",
            "swept",
            "--changes",
            &changes,
            "--parent",
            base,
            "--advance",
            "main",
        ];
        store.run_as(&agent(1), &args, env)
    }

    /// What main names in `store`, if it exists.
    fn main_of(store: &Store) -> Option<String> {
        let refs = store.read(&["ref", "list"]).ok();
        let main = (refs["refs"].as_array().unwrap().iter()).find(|r| r["name"] == "main");
        main.map(|r| r["target"].as_str().unwrap().to_owned())
    }

    /// [`swept_advance`] on a copy of `template` with nothing failing it, counting its calls;
    /// with main's head before it. It commits the swept commit's changeset and moves main to it,
    /// in the transaction that moves the checkpoint, and makes more calls than the swept commit
    /// (the ref file's), so a sweep of its calls reaches the move, the index sync and the
    /// checkpoint's move.
    fn dry_advance(template: &Store) -> (DryRun, String) {
        let plain = dry_commit(template);
        let store = Store::copy_of(template);
        let base = main_of(&store).expect("the template's main");
        let file = calls_file(&store);
        let env = [(FS_CALLS_ENV, file.to_str().unwrap())];
        let result = swept_advance(&store, &base, &env).ok();
        assert_eq!(
            store.checkpoint(),
            store.journal_len(),
            "the 32nd frame past the checkpoint moves it to the journal's end"
        );
        assert_eq!(result["changeset"], plain.changeset.as_str());
        assert_eq!(
            result["advanced"],
            json!({"name": "main", "old": base, "new": plain.changeset, "already_applied": false})
        );
        let dry = DryRun {
            calls: reported_calls(&file),
            changeset: plain.changeset.clone(),
        };
        assert!(
            dry.calls > plain.calls,
            "the move adds calls: {dry:?} {plain:?}"
        );
        (dry, base)
    }

    /// One run of the advancing sweep: [`swept_advance`] on a copy of `template`, failing as
    /// `failure` says from call `k`. Then, the fault cleared: the store verifies clean, and holds
    /// the commit's changeset (`changeset`, the dry run's) exactly when main names it, main at
    /// `base` otherwise; both when the run succeeded, a warning or not, and neither when it
    /// failed (either, after the documented abort); and the same publish made again ends with
    /// both. Returns how the run ended.
    fn advance_failing_at(
        template: &Store,
        k: u64,
        failure: &str,
        dry: &(DryRun, String),
    ) -> Ended {
        let (DryRun { changeset, .. }, base) = dry;
        let store = Store::copy_of(template);
        let what = format!("advance, call {k} failing {failure}");
        let fail_at = format!("{k}:{failure}");
        let run = swept_advance(&store, base, &[(FAIL_AT_ENV, fail_at.as_str())]);
        let ended = ended(&run, store.path());
        if let Ended::Succeeded(result) = &ended {
            assert_eq!(
                result["advanced"]["new"],
                changeset.as_str(),
                "{what}: {run}"
            );
        }
        store.assert_clean(&format!("{what}: after the failed run"));
        let present = store.read(&["show", changeset]).exit() == 0;
        let expected = if present { changeset } else { base };
        assert_eq!(
            main_of(&store).as_ref(),
            Some(expected),
            "{what}: the changeset is there exactly when main names it: {run}"
        );
        match ended {
            Ended::Succeeded(_) => assert!(present, "{what}: a success records both: {run}"),
            Ended::Failed(_) => assert!(!present, "{what}: a failure records neither: {run}"),
            Ended::Aborted => {}
        }
        let again = swept_advance(&store, base, &[]).ok();
        assert_eq!(again["advanced"]["new"], changeset.as_str(), "{what}");
        assert_eq!(again["advanced"]["already_applied"], present, "{what}");
        assert_eq!(main_of(&store).as_ref(), Some(changeset), "{what}");
        store.assert_clean(&format!("{what}: after the same publish again"));
        ended
    }

    /// An injected I/O error at any filesystem call of a `commit --advance` leaves its changeset
    /// and main's move both recorded or neither (Review Focus 3): each call of the dry run fails
    /// in turn, once and persistently, with `EIO` and with `ENOSPC` ([`FAILURES`]). A success
    /// records both, a warning or not; `E_IO_TRANSIENT` with `committed: false`, as every other
    /// failure, neither; the documented aborts either. No run panics, and the sweep reaches each
    /// outcome the commit sweep exists for ([`COMMIT_OUTCOMES`]).
    #[test]
    #[ignore = "slow, ~2 min: ~500 failed runs of commit --advance; just check-slow runs it"]
    fn fail_at_every_call_of_an_advancing_commit_reports_honestly() {
        let template = commit_template();
        let dry = dry_advance(&template);
        let mut ended = Vec::new();
        for failure in FAILURES {
            let runs = sweep(1..=dry.0.calls, |k| {
                advance_failing_at(&template, k, failure, &dry)
            });
            ended.extend(runs.into_iter().map(|run| (failure, run)));
        }
        let missing = unreached(&ended);
        assert!(
            missing.is_empty(),
            "no run of the advancing sweep ended so: {missing:?}"
        );
    }

    /// The advancing sweep's dry run, in the default gate ([`dry_advance`] asserts what it
    /// does): a change to the publish, the template or the checkpoint's batching that would take
    /// the move, the index sync or the checkpoint out of the slow sweep fails here at once.
    #[test]
    fn the_advancing_commit_sweep_reaches_the_move_and_the_checkpoint() {
        dry_advance(&commit_template());
    }
}

/// The documented abort of a commit (a failed journal append whose cut back off the journal
/// fails too, [`ABORTS`]) ends the process by `std::process::abort` even when its reason cannot
/// be written: here the run's stderr is a pipe nobody reads (its read end dropped before the
/// run, so every write to it fails with `EPIPE`), and the run must still abort, not panic at
/// the write and report `E_INTERNAL` (which `swarmops mcp`, catching a panic per call, would
/// serve on after, against storage spec §2 rule 3). The aborting call is found by running the
/// commit with a persistent `EIO` from each call in turn, cheaply, without the sweep's oracle;
/// the first run that aborts is then run again without a reader.
#[test]
fn an_abort_whose_reason_cannot_be_written_still_aborts() {
    let template = commit_template();
    let dry = dry_commit(&template);
    let fail_at = |k: u64| format!("{k}:persistent:eio");
    let aborting = sweep(1..=dry.calls, |k| {
        let store = Store::copy_of(&template);
        let fail_at = fail_at(k);
        swept_commit(&store, &[(FAIL_AT_ENV, fail_at.as_str())])
            .aborted()
            .then_some(k)
    });
    let k = aborting
        .into_iter()
        .flatten()
        .next()
        .expect("a persistent EIO at the journal append aborts, as the sweep documents");

    let store = Store::copy_of(&template);
    let changes = store.changes_of("swept", &SWEPT_PUTS);
    let args = store.args(&agent(1), &["commit", "-m", "swept", "--changes", &changes]);
    let now = NOW.to_string();
    let fail_at = fail_at(k);
    let env = [
        (swarmops_ops::NOW_ENV, now.as_str()),
        (FAIL_AT_ENV, fail_at.as_str()),
    ];
    let mut command = support::swarmops(&args, &env);
    let (reader, writer) = io::pipe().expect("a pipe");
    drop(reader);
    command.stderr(writer);
    let mut child = command.spawn().expect("run swarmops");
    let deadline = Instant::now() + support::RUN_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait().expect("look whether the run ended") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("call {k} failing persistently: the run was still going at the deadline");
        }
        thread::sleep(Duration::from_millis(1));
    };
    let output = child.wait_with_output().expect("the run's output");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        status.signal(),
        Some(SIGABRT),
        "call {k} failing persistently, stderr unwritable: {status}; stdout: {stdout}"
    );
    assert_eq!(stdout, "", "an aborted run prints no envelope");
    store.assert_clean(&format!("call {k} failing persistently: after the abort"));
}

/// `init` failing at each of its last eight calls: the write of `FORMAT`'s temp file, its link,
/// the sync of the store root that makes the link durable (its work done, then failing, as a
/// failed writeback does), and the five reads of the new store it reports from. Failing once
/// with `EIO`, each of them fails the `init`: the first three leave no store (an `init` whose
/// root sync fails takes `FORMAT` back, and a second `init` makes the store), and the five reads
/// fail after the store is made (`details.created`). The window's shape is pinned exactly: an
/// `init` that ignored the failed root sync would report success there, and a change to
/// `repo_info`'s FsOps reads moves the boundary. Failing persistently from the root's sync on,
/// the taking back fails too, and the process aborts, as documented ([`ABORTS`]; `init` appends
/// to no journal, so it has no other abort). In the default gate;
/// [`fail_at_every_call_reports_honestly`] sweeps every call.
#[test]
fn an_init_failing_at_its_last_calls_reports_honestly() {
    let calls = dry_init();
    let created = sweep(calls - 7..=calls, |k| {
        match init_failing_at(k, "once:eio") {
            Ended::Failed(error) => Some(error["details"]["created"] == true),
            Ended::Succeeded(_) | Ended::Aborted => None,
        }
    });
    assert_eq!(
        created,
        [
            Some(false),
            Some(false),
            Some(false),
            Some(true),
            Some(true),
            Some(true),
            Some(true),
            Some(true)
        ],
        "init's last calls: the temp write, the link and the root's sync (whose take-back \
         leaves no store, and the error says nothing was made), then the five reads of the \
         report, each failing after the store is made (created: true); none succeeds, none \
         aborts under a transient failure"
    );
    let root_sync = calls - 5;
    assert!(
        matches!(init_failing_at(root_sync, "persistent:eio"), Ended::Aborted),
        "a persistent failure at the root's sync, whose take-back fails too, aborts as documented"
    );
}
