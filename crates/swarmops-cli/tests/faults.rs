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
//! - No sweep here reaches the index's compaction: the commit template holds 31 frames
//!   (`support::sweep`), far short of `Index::COMPACT_THRESHOLD` (1,024 records). A compaction
//!   past that threshold under a power loss or a crash is the slow power-loss variant's cover
//!   (`power_loss.rs`, `..._at_every_cut`, which `just check-slow` and the weekly
//!   `just check-ci-slow` run); a failed call in one is the store's tests' (`meta.rs`: a failed
//!   sync before a compaction, a failed `Compacted` frame, an interrupted compaction finished).
//!
//! Each test owns its temp dirs, and the tests share nothing.

use std::io;
use std::os::unix::process::ExitStatusExt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::process::ExitStatus;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use swarmops_store::{FAIL_AT_ENV, StoreFormat};
use swarmops_testkit::child::SIGABRT;
use swarmops_testkit::per_format;

use crate::support::sweep::{
    NOW, SWEPT_PUTS, commit_template, dry_commit, dry_init, init_again, init_args, sweep,
    swept_commit,
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
/// The first is what each of the journal's three aborts says ([`ABORT_SITES`]).
const ABORTS: [&str; 2] = ["could not be cut back off it", "could not be taken back"];

/// How many double faults abort the process: the count the architecture page and the module
/// documentation of `swarmops mcp` give ([`ABORTS_DOCUMENTED`]).
const ABORT_COUNT: usize = 4;

/// Where the aborts are, and how many each source holds: the journal's failed frame whose cut
/// fails too, its cut that fails while unlisted frames are written afresh and its frames written
/// afresh that land elsewhere and cannot be cut back; and the `init` that cannot take its
/// `FORMAT` back. Each with what it says on stderr, one of [`ABORTS`].
const ABORT_SITES: [(&str, &str, usize); 2] = [
    (
        include_str!("../../swarmops-store/src/meta/journal.rs"),
        ABORTS[0],
        3,
    ),
    (
        include_str!("../../swarmops-repo/src/repo.rs"),
        ABORTS[1],
        1,
    ),
];

/// The sentences that count the aborts, each in the text that holds it.
const ABORTS_DOCUMENTED: [(&str, &str); 2] = [
    (
        include_str!("../../../docs/architecture.md"),
        "Four double faults abort the process",
    ),
    (include_str!("../src/mcp.rs"), "four documented aborts"),
];

/// The call that ends the process in an abort.
const ABORT_CALL: &str = "std::process::abort();";

/// How each abort's reason starts on stderr.
const FATAL: &str = "swarmops: fatal:";

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
/// error, and no warning of a run that succeeded, carries the store's path ([`holds_store_path`]:
/// with its leading `/` or without it, as a file named relative to `/` shows it), but where an
/// `init` that made the store says so: that message and its hint name the path the client gave.
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
            !holds_store_path(&result["warnings"].to_string(), store),
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
        !holds_store_path(&seen.to_string(), store),
        "a host path in the error: {run}"
    );
    Ended::Failed(error)
}

/// Whether `text` holds the store's path `store` (absolute), with its leading `/` or without it:
/// a store file named relative to `/`, not to the store's root, shows the host path so
/// (`tmp/…/store/FORMAT`), relative and free of `..`, so the check on `details.store_file`
/// alone passes it.
fn holds_store_path(text: &str, store: &str) -> bool {
    text.contains(store.trim_start_matches('/'))
}

/// A run made up for the oracles' own checks: one that printed `envelope` and `stderr`, and
/// ended as `status` says (a wait status: an exit code in its second byte, or a signal).
fn made_up(status: i32, envelope: &Value, stderr: &str) -> Run {
    Run {
        args: vec!["made-up".to_owned()],
        status: ExitStatus::from_raw(status),
        stdout: format!("{envelope}\n"),
        stderr: stderr.to_owned(),
    }
}

/// [`made_up`]: a run that failed with `error` (exit 1).
fn made_up_failure(mut error: Value) -> Run {
    error["exit"] = 1.into();
    made_up(1 << 8, &json!({"api": 1, "ok": false, "error": error}), "")
}

/// [`made_up`]: a run that succeeded with `result`.
fn made_up_success(result: &Value) -> Run {
    made_up(0, &json!({"api": 1, "ok": true, "result": result}), "")
}

/// [`made_up`]: a run that aborted, saying `stderr`.
fn made_up_abort(stderr: &str) -> Run {
    made_up(SIGABRT, &Value::Null, stderr)
}

/// [`ended`]'s own check, on runs made up for it ([`made_up`]): it takes a success, a failure
/// that names its file relative to the store's root, the failure of an `init` that made the
/// store and names the path the client gave, and a documented abort; and it refuses each thing
/// it exists to refuse, one at a time: an abort not documented, a panic, a transient failure
/// that committed or is not retryable, a filesystem failure without its operation or with a file
/// outside the store's root, and the store's path in an error or in a warning, with its leading
/// `/` or without it ([`holds_store_path`]). A sweep whose oracle lost one of these checks would
/// stay green; this test does not.
#[test]
fn the_fault_oracle_refuses_each_thing_it_checks() {
    const STORE: &str = "/tmp/sweep/store";
    let op = swarmops_store::FileOp::ALL[0].as_str();
    let failing = |code: &str, details: Value| {
        let retryable = code == "E_IO_TRANSIENT";
        json!({"code": code, "retryable": retryable, "message": "no", "details": details})
    };
    let transient = |details: Value| failing("E_IO_TRANSIENT", details);
    let io = |details: Value| failing("E_IO", details);
    let made = |read: &str| {
        let message = format!("the store at {STORE} is made, but reading it back failed: {read}");
        let details = json!({"created": true, "fs_op": op});
        json!({"code": "E_IO", "message": message, "hint": STORE, "details": details})
    };
    let not_retryable = json!({"committed": false, "fs_op": op});
    let not_retryable =
        json!({"code": "E_IO_TRANSIENT", "retryable": false, "details": not_retryable});

    // 1. What it takes.
    let success = made_up_success(&json!({"changeset": "c", "warnings": ["meta/journal.off"]}));
    assert!(matches!(ended(&success, STORE), Ended::Succeeded(_)));
    for error in [
        transient(json!({"committed": false, "fs_op": op, "store_file": "meta/journal.log"})),
        io(json!({"fs_op": op})),
        made("a read failed"),
    ] {
        let failure = made_up_failure(error);
        assert!(
            matches!(ended(&failure, STORE), Ended::Failed(_)),
            "{failure}"
        );
    }
    for abort in ABORTS {
        let aborted = made_up_abort(&format!("{FATAL} the frame {abort}: aborting"));
        assert!(
            matches!(ended(&aborted, STORE), Ended::Aborted),
            "{aborted}"
        );
    }

    // 2. What it refuses.
    let refused = [
        (
            "an abort that is not documented",
            made_up_abort("it gave up"),
        ),
        (
            "a panic",
            made_up_failure(json!({"code": "E_INTERNAL", "message": "a panic"})),
        ),
        (
            "a transient failure that committed",
            made_up_failure(transient(json!({"committed": true, "fs_op": op}))),
        ),
        (
            "a transient failure that is not retryable",
            made_up_failure(not_retryable),
        ),
        (
            "a filesystem failure without its operation",
            made_up_failure(io(json!({}))),
        ),
        (
            "a filesystem failure of no operation of the store's",
            made_up_failure(io(json!({"fs_op": "no_such_operation"}))),
        ),
        (
            "an absolute store file",
            made_up_failure(io(json!({"fs_op": op, "store_file": "/meta/journal.log"}))),
        ),
        (
            "a store file above the store's root",
            made_up_failure(io(json!({"fs_op": op, "store_file": "../store/FORMAT"}))),
        ),
        (
            "the store's path in a message",
            made_up_failure(json!({"code": "E_EXISTS", "message": format!("{STORE} is there")})),
        ),
        (
            "the store's path without its leading slash, as a store file",
            made_up_failure(io(
                json!({"fs_op": op, "store_file": "tmp/sweep/store/FORMAT"}),
            )),
        ),
        (
            "the store's path in a warning",
            made_up_success(&json!({"warnings": [format!("{STORE}/meta/journal.off failed")]})),
        ),
        (
            "the store's path in what an init that made the store read back",
            made_up_failure(made(&format!("{STORE}/FORMAT"))),
        ),
    ];
    for (what, run) in refused {
        let judged = catch_unwind(AssertUnwindSafe(|| ended(&run, STORE)));
        assert!(judged.is_err(), "{what} was taken: {run}");
    }

    // 3. The path check itself, with the leading `/` and without it.
    assert!(holds_store_path("open /tmp/sweep/store/FORMAT", STORE));
    assert!(holds_store_path("tmp/sweep/store/FORMAT", STORE));
    assert!(!holds_store_path("meta/journal.log", STORE));
}

/// `source` as its messages are printed: its words a space apart, without the `\\` that ends a
/// line inside a string literal to continue it on the next.
fn as_printed(source: &str) -> String {
    let words: Vec<&str> = (source.split_whitespace())
        .filter(|word| *word != "\\")
        .collect();
    words.join(" ")
}

/// The documented aborts are four ([`ABORT_COUNT`]): the sources hold that many abort calls and
/// as many reasons written to stderr, each saying what the sweeps know it by ([`ABORTS`]), and
/// the texts that count the aborts say four ([`ABORTS_DOCUMENTED`]). An abort added or removed
/// fails here until the count, the texts and the sweeps' list are changed with it.
#[test]
fn the_documented_aborts_are_four() {
    let mut aborts = 0;
    for (source, says, count) in ABORT_SITES {
        let source = as_printed(source);
        let found = (
            source.matches(ABORT_CALL).count(),
            source.matches(FATAL).count(),
            source.matches(says).count(),
        );
        assert_eq!(found, (count, count, count), "aborts that say {says:?}");
        aborts += count;
    }
    assert_eq!(aborts, ABORT_COUNT);
    for (text, sentence) in ABORTS_DOCUMENTED {
        assert!(
            as_printed(text).contains(sentence),
            "no text says {sentence:?}"
        );
    }
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

/// One run of the init sweep: `init` of a store of `format` on a fresh path, failing as
/// `failure` says from call `k`.
/// Then, the fault cleared: a store is there, whole, exactly when the run succeeded or said it
/// made one (`details.created`; either after the documented abort); without one, a second `init`
/// makes it, and with one, says `E_EXISTS`. Either way the store is fresh, and verifies clean
/// ([`init_again`]).
fn init_failing_at(format: StoreFormat, k: u64, failure: &str) -> Ended {
    let store = Store::empty();
    let what = format!("init, call {k} failing {failure}");
    let fail_at = format!("{k}:{failure}");
    let init = run(
        &init_args(format, store.path()),
        &[(FAIL_AT_ENV, fail_at.as_str())],
    );
    let ended = ended(&init, store.path());
    let whole = init_again(&store, format, &what);
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
/// committed frame in `journal.off` (FORMAT 2: `journal2.off`), a success whose warning names
/// it; and, under a persistent
/// failure, the documented abort of a failed append whose cut fails too.
const COMMIT_OUTCOMES: [(&str, EndedSo); 3] = [
    ("E_IO_TRANSIENT, not committed", |_, ended| {
        matches!(ended, Ended::Failed(error)
            if error["code"] == "E_IO_TRANSIENT" && error["details"]["committed"] == false)
    }),
    (
        "a success with a warning that names journal.off or journal2.off",
        |_, ended| {
            matches!(ended, Ended::Succeeded(result)
                if warns_of(result, "journal.off") || warns_of(result, "journal2.off"))
        },
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

per_format! {
/// An injected I/O error at any filesystem call of `init` and of a `commit` that moves the
/// checkpoint is reported honestly (Review Focus 3): each call of a dry run fails in turn, once
/// and persistently, with `EIO` and with `ENOSPC` ([`FAILURES`], [`INIT_FAILURES`]). A success
/// committed, visibly and cleanly; a failure committed nothing or says it did; the store always
/// opens again; no run panics, and none aborts but as documented ([`ABORTS`]). The dry run of
/// the commit asserts that it moves the checkpoint, so that the sweep fails the index sync and
/// the checkpoint's move too; and the commit sweep reaches each outcome it exists for
/// ([`COMMIT_OUTCOMES`]).
#[ignore = "slow, ~3 min: ~1,300 failed runs of init and commit; just check-slow runs it"]
fn fail_at_every_call_reports_honestly(format) {
    let template = commit_template(format);
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

    let calls = dry_init(format);
    let mut aborted = false;
    for failure in INIT_FAILURES {
        let runs = sweep(1..=calls, |k| init_failing_at(format, k, failure));
        aborted |= failure.starts_with("persistent:")
            && runs.iter().any(|run| matches!(run, Ended::Aborted));
    }
    assert!(
        aborted,
        "no init run ended in the documented abort: the root sync failed and FORMAT could not be taken back"
    );
}
}

per_format! {
/// The commit sweeps' dry run, in the default gate: the swept commit, on a copy of the
/// template, moves the checkpoint ([`dry_commit`] asserts it), so the commit sweeps of the slow
/// suite ([`fail_at_every_call_reports_honestly`], and `crash.rs`'s
/// `kill9_during_commit_store_verifies_and_next_commit_succeeds`) fail and crash the index sync
/// and the checkpoint's move too. A change to the checkpoint's batching or to the template fails
/// here at once, not in the slow suite alone.
fn the_commit_sweeps_reach_the_index_sync_and_the_checkpoint(format) {
    dry_commit(&commit_template(format));
}
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
    /// `failure` says from call `k`. Then, the fault cleared, the store holds both or neither
    /// ([`both_or_neither`]). Returns how the run ended.
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
        both_or_neither(&store, &ended, dry, &format!("{what}: {run}"));
        ended
    }

    /// What the advancing sweep holds `store` to after a run that ended as `ended`, the fault
    /// cleared; `what` names the run. The store verifies clean, and holds the commit's changeset
    /// (the dry run's, `dry`) exactly when main names it, main at its base otherwise; both when
    /// the run succeeded, a warning or not, and neither when it failed (either, after the
    /// documented abort); and the same publish made again ends with both.
    fn both_or_neither(store: &Store, ended: &Ended, dry: &(DryRun, String), what: &str) {
        let (DryRun { changeset, .. }, base) = dry;
        store.assert_clean(&format!("after the failed run: {what}"));
        let present = store.read(&["show", changeset]).exit() == 0;
        let expected = if present { changeset } else { base };
        assert_eq!(
            main_of(store).as_ref(),
            Some(expected),
            "the changeset is there exactly when main names it: {what}"
        );
        match ended {
            Ended::Succeeded(_) => assert!(present, "a success records both: {what}"),
            Ended::Failed(_) => assert!(!present, "a failure records neither: {what}"),
            Ended::Aborted => {}
        }
        let again = swept_advance(store, base, &[]).ok();
        assert_eq!(again["advanced"]["new"], changeset.as_str(), "{what}");
        assert_eq!(again["advanced"]["already_applied"], present, "{what}");
        assert_eq!(main_of(store).as_ref(), Some(changeset), "{what}");
        store.assert_clean(&format!("after the same publish again: {what}"));
    }

    /// [`both_or_neither`]'s own check, on stores made to break it: the swept commit made
    /// without its `--advance` leaves the changeset without main's move, which the oracle
    /// refuses however the run ended; it refuses a success that left neither and a failure that
    /// left both; and it takes a failure with neither and a success with both.
    #[test]
    fn the_advance_oracle_refuses_a_changeset_without_its_move() {
        let template = commit_template(swarmops_store::DEFAULT_INIT_FORMAT);
        let dry = dry_advance(&template);
        let succeeded = || Ended::Succeeded(Value::Null);
        let failed = || Ended::Failed(Value::Null);
        let refuses = |store: &Store, ended: Ended| {
            let judge = || both_or_neither(store, &ended, &dry, "a made-up run");
            catch_unwind(AssertUnwindSafe(judge)).is_err()
        };

        // 1. The changeset without the move: refused behind a success and behind a failure.
        let half = Store::copy_of(&template);
        let committed = swept_commit(&half, &[]).ok();
        assert_eq!(committed["changeset"], dry.0.changeset.as_str());
        assert!(refuses(&half, succeeded()), "half a publish, a success");
        assert!(refuses(&half, failed()), "half a publish, a failure");

        // 2. Neither behind a success, both behind a failure: refused.
        let neither = Store::copy_of(&template);
        assert!(refuses(&neither, succeeded()), "a success with neither");
        let both = Store::copy_of(&template);
        swept_advance(&both, &dry.1, &[]).ok();
        assert!(refuses(&both, failed()), "a failure with both");

        // 3. Neither behind a failure, both behind a success: taken.
        both_or_neither(&neither, &failed(), &dry, "a failure with neither");
        both_or_neither(&both, &succeeded(), &dry, "a success with both");
    }

    per_format! {
    /// An injected I/O error at any filesystem call of a `commit --advance` leaves its changeset
    /// and main's move both recorded or neither (Review Focus 3): each call of the dry run fails
    /// in turn, once and persistently, with `EIO` and with `ENOSPC` ([`FAILURES`]). A success
    /// records both, a warning or not; `E_IO_TRANSIENT` with `committed: false`, as every other
    /// failure, neither; the documented aborts either. No run panics, and the sweep reaches each
    /// outcome the commit sweep exists for ([`COMMIT_OUTCOMES`]).
    #[ignore = "slow, ~15 s: ~500 failed runs of commit --advance; just check-slow runs it"]
    fn fail_at_every_call_of_an_advancing_commit_reports_honestly(format) {
        let template = commit_template(format);
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
    }

    per_format! {
    /// The advancing sweep's dry run, in the default gate ([`dry_advance`] asserts what it
    /// does): a change to the publish, the template or the checkpoint's batching that would take
    /// the move, the index sync or the checkpoint out of the slow sweep fails here at once.
    fn the_advancing_commit_sweep_reaches_the_move_and_the_checkpoint(format) {
        dry_advance(&commit_template(format));
    }
    }
}

/// The failure sweep of a land, `land --key` of a commit onto main: its changeset and main's move
/// are one journal frame, so whatever a run with an injected failure ends with, main names the
/// landed changeset or what it named before, and the same land made again ends with main at the
/// landed changeset.
mod land {
    use serde_json::json;
    use swarmops_store::FS_CALLS_ENV;

    use super::*;
    use crate::support::sweep::{calls_file, reported_calls};

    /// The land's idempotency key.
    const KEY: &str = "fault-land-1";

    /// What the land the sweep fails does with nothing failing it, on a copy of the commit
    /// template.
    struct DryLand {
        /// How many filesystem calls it makes.
        calls: u64,
        /// The changeset it lands: the template's newest commit, a child of main's head.
        head: String,
        /// What main names before it.
        base: String,
        /// The changeset it makes on main.
        landed: String,
    }

    /// The land of `head` the sweep fails, keyed [`KEY`], as `agent(1)` with `env` set.
    fn swept_land(store: &Store, head: &str, env: &[(&str, &str)]) -> Run {
        store.run_as(&agent(1), &["land", head, "--key", KEY], env)
    }

    /// What main names in `store`.
    fn main_of(store: &Store) -> String {
        let resolved = store.read(&["resolve", "main"]).ok();
        resolved["id"].as_str().unwrap().to_owned()
    }

    /// [`swept_land`] of the template's newest commit on a copy of `template`, with nothing
    /// failing it, counting its calls. Its frame is the 32nd past the checkpoint, so it lands the
    /// commit on main in the transaction that syncs the index and moves the checkpoint, and a
    /// sweep of its calls reaches the publish, the index sync and the checkpoint's move.
    fn dry_land(template: &Store) -> DryLand {
        let store = Store::copy_of(template);
        let newest = store.read(&["op", "log", "-n", "1"]).ok();
        let head = newest["ops"][0]["changesets"][0]["id"].as_str().unwrap();
        let head = head.to_owned();
        let base = main_of(&store);
        let file = calls_file(&store);
        let env = [(FS_CALLS_ENV, file.to_str().unwrap())];
        let result = swept_land(&store, &head, &env).ok();
        assert_eq!(
            store.checkpoint(),
            store.journal_len(),
            "the 32nd frame past the checkpoint moves it to the journal's end"
        );
        assert_eq!(
            (&result["status"], &result["rebased_onto"]),
            (&json!("landed"), &json!(base)),
            "{result}"
        );
        let landed = result["landed"].as_str().unwrap().to_owned();
        assert_eq!(main_of(&store), landed);
        let dry = DryLand {
            calls: reported_calls(&file),
            head,
            base,
            landed,
        };
        assert!(
            dry.calls > 30,
            "a land makes dozens of calls: {}",
            dry.calls
        );
        dry
    }

    /// One run of the land sweep: [`swept_land`] on a copy of `template`, failing as `failure`
    /// says from call `k`. Then, the fault cleared: the store verifies clean, and main names the
    /// landed changeset (the dry run's) or what it named before: the first when the run
    /// succeeded, a warning or not, the second when it failed (either, after the documented
    /// abort); and the same land made again ends with main at the landed changeset, replayed
    /// where it had landed. Returns how the run ended.
    fn land_failing_at(template: &Store, k: u64, failure: &str, dry: &DryLand) -> Ended {
        let store = Store::copy_of(template);
        let what = format!("land, call {k} failing {failure}");
        let fail_at = format!("{k}:{failure}");
        let run = swept_land(&store, &dry.head, &[(FAIL_AT_ENV, fail_at.as_str())]);
        let ended = ended(&run, store.path());
        if let Ended::Succeeded(result) = &ended {
            assert_eq!(result["landed"], dry.landed.as_str(), "{what}: {run}");
        }
        store.assert_clean(&format!("{what}: after the failed run"));
        let main = main_of(&store);
        let landed = main == dry.landed;
        assert!(
            landed || main == dry.base,
            "{what}: main names {main}: {run}"
        );
        match ended {
            Ended::Succeeded(_) => assert!(landed, "{what}: a success has landed: {run}"),
            Ended::Failed(_) => assert!(!landed, "{what}: a failure has not: {run}"),
            Ended::Aborted => {}
        }
        let again = swept_land(&store, &dry.head, &[]).ok();
        let status = if landed { "replayed" } else { "landed" };
        assert_eq!(
            (&again["status"], &again["landed"]),
            (&json!(status), &json!(dry.landed)),
            "{what}"
        );
        assert_eq!(main_of(&store), dry.landed, "{what}");
        store.assert_clean(&format!("{what}: after the same land again"));
        ended
    }

    per_format! {
    /// An injected I/O error at any filesystem call of a `land` leaves main at the landed
    /// changeset or where it was (operations spec §3; Review Focus 1): each call of the dry run
    /// fails in turn, once and persistently, with `EIO` and with `ENOSPC` ([`FAILURES`]). A
    /// success has landed, a warning or not; `E_IO_TRANSIENT` with `committed: false`, as every
    /// other failure, has not; the documented aborts either. The same land again always ends
    /// landed. No run panics, and the sweep reaches each outcome the commit sweep exists for
    /// ([`COMMIT_OUTCOMES`]).
    #[ignore = "slow, ~10 s: ~200 failed runs of land, each checked twice; just check-slow runs it"]
    fn fail_at_every_call_of_a_land_reports_honestly(format) {
        let template = commit_template(format);
        let dry = dry_land(&template);
        let mut ended = Vec::new();
        for failure in FAILURES {
            let runs = sweep(1..=dry.calls, |k| {
                land_failing_at(&template, k, failure, &dry)
            });
            ended.extend(runs.into_iter().map(|run| (failure, run)));
        }
        let missing = unreached(&ended);
        assert!(
            missing.is_empty(),
            "no run of the land sweep ended so: {missing:?}"
        );
    }
    }

    per_format! {
    /// The land sweep's dry run, in the default gate ([`dry_land`] asserts what it does): a
    /// change to the land, the template or the checkpoint's batching that would take the publish,
    /// the index sync or the checkpoint out of the slow sweep fails here at once.
    fn the_land_sweep_reaches_the_publish_and_the_checkpoint(format) {
        dry_land(&commit_template(format));
    }
    }
}

per_format! {
/// The documented abort of a commit (a failed journal append whose cut back off the journal
/// fails too, [`ABORTS`]) ends the process by `std::process::abort` even when its reason cannot
/// be written: here the run's stderr is a pipe nobody reads (its read end dropped before the
/// run, so every write to it fails with `EPIPE`), and the run must still abort, not panic at
/// the write and report `E_INTERNAL` (which `swarmops mcp`, catching a panic per call, would
/// serve on after, against storage spec §2 rule 3). The aborting call is found by running the
/// commit with a persistent `EIO` from each call in turn, cheaply, without the sweep's oracle;
/// the first run that aborts is then run again without a reader.
fn an_abort_whose_reason_cannot_be_written_still_aborts(format) {
    let template = commit_template(format);
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
}

per_format! {
/// `init` failing at each of its last calls: the write of `FORMAT`'s temp file, its link, the
/// sync of the store root that makes the link durable (its work done, then failing, as a failed
/// writeback does), and the reads of the new store it reports from (five in FORMAT 1, four in
/// FORMAT 2: [`report_reads`]). Failing once with `EIO`, each of them fails the `init`: the
/// first three leave no store (an `init` whose root sync fails takes `FORMAT` back, and a second
/// `init` makes the store), and the reads fail after the store is made (`details.created`). The window's shape is pinned exactly: an
/// `init` that ignored the failed root sync would report success there, and a change to
/// `repo_info`'s FsOps reads moves the boundary. Failing persistently from the root's sync on,
/// the taking back fails too, and the process aborts, as documented ([`ABORTS`]; `init` appends
/// to no journal, so it has no other abort). In the default gate;
/// [`fail_at_every_call_reports_honestly`] sweeps every call.
fn an_init_failing_at_its_last_calls_reports_honestly(format) {
    let calls = dry_init(format);
    let reads = report_reads(format);
    let created = sweep(calls - 2 - reads..=calls, |k| {
        match init_failing_at(format, k, "once:eio") {
            Ended::Failed(error) => Some(error["details"]["created"] == true),
            Ended::Succeeded(_) | Ended::Aborted => None,
        }
    });
    let mut window = vec![Some(false); 3];
    window.extend(std::iter::repeat_n(Some(true), usize::try_from(reads).unwrap()));
    assert_eq!(
        created,
        window,
        "init's last calls: the temp write, the link and the root's sync (whose take-back \
         leaves no store, and the error says nothing was made), then the {reads} reads of the \
         report, each failing after the store is made (created: true); none succeeds, none \
         aborts under a transient failure"
    );
    let root_sync = calls - reads;
    assert!(
        matches!(init_failing_at(format, root_sync, "persistent:eio"), Ended::Aborted),
        "a persistent failure at the root's sync, whose take-back fails too, aborts as documented"
    );
}
}

/// How many FsOps reads `init`'s report of a fresh store of `format` makes: FORMAT 2 and 3 read
/// one file fewer.
fn report_reads(format: StoreFormat) -> u64 {
    match format {
        StoreFormat::V1 => 5,
        StoreFormat::V2 | StoreFormat::V3 => 4,
        other => panic!("no read count for {other:?}"),
    }
}

/// The golden envelopes of the two codes only a test hook raises through the binary: the first
/// call of the swept commit whose single `EIO` gives `E_IO_TRANSIENT` (exit 4, retryable,
/// `committed: false`), every call swept and the first such run kept; and `E_INTERNAL` (exit 1),
/// the panic of a malformed `SWARMOPS_TEST_FAIL_AT` caught into an envelope whose message names
/// no host path. Each snapshot is the envelope without its `meta`.
#[test]
fn fault_codes_golden() {
    let template = commit_template(swarmops_store::DEFAULT_INIT_FORMAT);
    let dry = dry_commit(&template);

    // 1. E_IO_TRANSIENT: of the runs failing each call once, the first reported so.
    let runs = sweep(1..=dry.calls, |k| {
        let store = Store::copy_of(&template);
        let fail_at = format!("{k}:once:eio");
        let run = swept_commit(&store, &[(FAIL_AT_ENV, fail_at.as_str())]);
        let transient = run.exit() != 0 && run.envelope()["error"]["code"] == "E_IO_TRANSIENT";
        transient.then_some(run)
    });
    let transient = runs
        .into_iter()
        .flatten()
        .next()
        .expect("a call whose transient failure is E_IO_TRANSIENT");
    let error = transient.err("E_IO_TRANSIENT");
    assert_eq!(
        (&error["exit"], &error["details"]["committed"]),
        (&4.into(), &false.into()),
        "{transient}"
    );
    insta::assert_json_snapshot!("io_transient", golden(&transient));

    // 2. E_INTERNAL: a value the hook cannot parse panics as the store opens.
    let store = Store::copy_of(&template);
    let internal = run(&["--repo", store.path(), "info"], &[(FAIL_AT_ENV, "bogus")]);
    let error = internal.err("E_INTERNAL");
    assert_eq!(internal.exit(), 1, "{internal}");
    let message = error["message"].as_str().unwrap_or_default();
    assert!(!holds_store_path(message, store.path()), "{internal}");
    insta::assert_json_snapshot!("internal", golden(&internal));
}

/// `run`'s envelope without its `meta`: what a golden snapshot pins.
fn golden(run: &Run) -> Value {
    let mut envelope = run.envelope();
    let meta = envelope.as_object_mut().expect("an object").remove("meta");
    assert!(meta.is_some(), "{run}");
    envelope
}
