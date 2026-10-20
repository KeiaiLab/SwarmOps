//! `swarmops` processes that crash in the middle of a write (operations spec §0; storage spec
//! §2–§3): the M1 acceptance tests for crash consistency, through the real binary. They drive
//! the test hooks, so `all.rs` holds this module only with the `test-hooks` feature, which the
//! workspace's gates turn on (`--all-features`).
//!
//! - A crash is `SWARMOPS_TEST_CRASH_AT=k`: the store's k-th filesystem call aborts the
//!   process, as a `kill -9` at that exact point would (no destructor runs; what reached the
//!   kernel stays). The object store and the metadata store share one call count, objects are
//!   written one at a time then, and a sweep's commits sign at a fixed time
//!   (`SWARMOPS_TEST_NOW`), so that they write the same objects: every run of a command makes
//!   the same calls in the same order, and call k is the same call in every run. A dry run with
//!   `SWARMOPS_TEST_FS_CALLS=<file>` counts a command's calls first (twice, in two seconds of
//!   the clock, for the commit), and the sweep crashes the command at every one of them. The
//!   runner, the dry runs and the commit template are the error sweeps' too (`support::sweep`).
//! - Every run is checked against the store's write-ordering rules (`SWARMOPS_TEST_ORDER_CHECK=1`,
//!   which the test support sets for every run): a write that breaks one panics, and the run
//!   ends in `E_INTERNAL` instead of at its crash point.
//! - Every test ends with `verify --deep` through the binary, and reads its `problems`.
//!
//! Each test owns its temp dirs, and the tests share nothing.

use std::fs::{self, OpenOptions};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use swarmops_ops::NOW_ENV;
use swarmops_store::{CRASH_AT_ENV, FS_CALLS_ENV, Journal, RealFs};

use crate::support::sweep::{
    DryRun, NOW, SWEPT_PUTS, TEMPLATE_FRAMES, calls_file, commit_template, dry_commit, dry_init,
    init_again, reported_calls, sweep, swept_commit,
};
use crate::support::{Run, Store, agent, run};

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
    /// The crash left its frame whole in `journal.log` and not listed in `journal.off`, and the
    /// next commit listed it.
    unlisted: bool,
    /// The temp files the crash left in `objects/tmp/`.
    leftovers: usize,
}

/// How many whole frames the `journal.log` of `store` holds that its `journal.off` does not list:
/// what a crash between a frame's two appends leaves, which the next write lists.
fn unlisted_frames(store: &Store) -> u64 {
    let journal = Journal::open(&store.layout(), Arc::new(RealFs)).unwrap();
    let whole = u64::try_from(journal.tail_frames_from(0).unwrap().len()).unwrap();
    whole - journal.frame_count().unwrap()
}

/// One run of the commit sweep: the swept commit on a copy of `template`, its k-th filesystem
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
    let crash = swept_commit(&store, &env);
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

    // The next commit's write lists a frame the crash left whole and unlisted, and says so.
    let unlisted = unlisted_frames(&store);
    let next = format!("after a crash at call {k}\n");
    let changes = store.changes("after-crash", "after-crash.txt", &next);
    let args = [
        "commit",
        "-m",
        "after-crash.txt",
        "--changes",
        changes.as_str(),
    ];
    let run = store.run_as(&agent(2), &args, &[]);
    let result = run.ok();
    assert!(result.get("warnings").is_none(), "k={k}: {run}");
    let recovered = &run.envelope()["meta"]["recovered"];
    let listed = recovered["frames_listed"].as_u64().unwrap_or(0);
    assert_eq!(listed, unlisted, "k={k}: {run}");
    let fresh = result["changeset"].as_str().unwrap();
    let shown = store.show(fresh);
    assert_eq!(shown["changes"][0]["path"], "after-crash.txt", "k={k}");
    assert_eq!(store.info()["changesets"], changesets + 1, "k={k}");
    store.assert_clean(&format!("k={k}: after the next commit"));
    Crashed {
        finished,
        committed,
        unlisted: unlisted > 0,
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
        crashes.iter().any(|crash| crash.unlisted),
        "some crash came between the frame's journal.log append and its listing, which the next \
         commit made and reported (meta.recovered.frames_listed)"
    );
    assert!(
        crashes.iter().any(|crash| crash.leftovers > 0),
        "some crash left a temp file in objects/tmp, which verify passed over"
    );
}

/// Runs `args(store)` as `agent(1)` on a copy of `template` without a crash, counting its
/// calls (N), then on a fresh copy for each k of 1..=N + 1, its k-th call aborting it: each run
/// must end at its crash point (or, past N, finish), not in the order check's `E_INTERNAL`; the
/// store must then verify clean, and take a commit (its recovery checked too) that verifies
/// clean. `what` names the command in messages. Returns N.
fn sweep_checked(template: &Store, what: &str, args: impl Fn(&Store) -> Vec<String> + Sync) -> u64 {
    let dry = Store::copy_of(template);
    let file = calls_file(&dry);
    let dry_args = args(&dry);
    let dry_args: Vec<&str> = dry_args.iter().map(String::as_str).collect();
    let run = dry.run_as(
        &agent(1),
        &dry_args,
        &[(FS_CALLS_ENV, file.to_str().unwrap())],
    );
    run.ok();
    let calls = reported_calls(&file);
    sweep(1..=calls + 1, |k| {
        let store = Store::copy_of(template);
        let crash_at = k.to_string();
        let args = args(&store);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let run = store.run_as(&agent(1), &args, &[(CRASH_AT_ENV, &crash_at)]);
        if k <= calls {
            assert!(run.aborted(), "{what}, k={k} of {calls}: {run}");
        } else {
            run.ok();
        }
        store.assert_clean(&format!("{what}, k={k}: after the crash"));
        store.commit(
            &agent(2),
            "after.txt",
            &format!("after {what} k={k}\n"),
            None,
        );
        store.assert_clean(&format!("{what}, k={k}: after the next commit"));
    });
    calls
}

/// Every crash sweep runs under the order checker (`swarmops_store::OrderCheckFs`, rules W1 to
/// W7): the tests set `SWARMOPS_TEST_ORDER_CHECK=1` for every run of the binary, and the store's
/// and the repository's sweeps wrap their crashing child's filesystem in it, and the one their
/// parent recovers on, so a write that breaks a rule panics in the run crashed at any call, in a
/// dry run, or in what recovers from the crash. This sweep, in the default gate, crashes a root
/// commit on a fresh store, a ref move on top of it and an `op undo` of that move at each of
/// their calls; the slow sweeps (`kill9_during_*`) do the same for `init` and for a commit that
/// syncs the index and moves the checkpoint.
#[test]
fn order_check_rules_hold_in_every_crash_sweep() {
    let template = Store::init().with_env(&[(NOW_ENV, &NOW.to_string())]);
    let commit = |store: &Store| {
        let changes = store.changes_of("crash", &SWEPT_PUTS);
        ["commit", "-m", "checked", "--changes", &changes]
            .map(str::to_owned)
            .to_vec()
    };
    let calls = sweep_checked(&template, "a root commit", commit);
    assert!(calls > 20, "a commit makes dozens of calls: {calls}");

    let id = template.commit(&agent(0), "base.txt", "the base\n", None);
    let create = |_: &Store| {
        ["ref", "set", "main", &id, "--create"]
            .map(str::to_owned)
            .to_vec()
    };
    let calls = sweep_checked(&template, "a ref move", create);
    assert!(
        calls > 5,
        "a ref move writes its frame and its file: {calls}"
    );

    // The move made on the template, operation 1, and its undo swept: it deletes main again.
    let args = ["ref", "set", "main", id.as_str(), "--create"];
    template.run_as(&agent(0), &args, &[]).ok();
    let undo = |_: &Store| ["op", "undo", "1"].map(str::to_owned).to_vec();
    let calls = sweep_checked(&template, "an undo", undo);
    assert!(
        calls > 5,
        "an undo writes its frame and removes the ref's file: {calls}"
    );
}

/// One run of the init sweep: `init` on a fresh path, its k-th filesystem call (of the
/// `calls` a dry run counted) aborting it. Then either `info` says `E_NOT_A_REPO` and a second
/// `init` succeeds, or `info` succeeds (and a second `init` says `E_EXISTS`); either way the
/// store is fresh and verifies clean ([`init_again`]). Returns whether `info` found the store
/// right after the crash.
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
    init_again(&store, &format!("k={k}"))
}

/// `init` crashed at each of its filesystem calls in turn (`LocalRepo::create`, with
/// `MetaStore::create` inside it, then the first snapshot `init` reports from), from the first
/// to past the last: before `FORMAT` is linked there is no store and a second `init` makes it;
/// from then on the store is whole.
#[test]
#[ignore = "slow, ~25 s: ~300 crashed inits; just check-slow runs it"]
fn kill9_during_init_leaves_no_store_or_a_whole_one() {
    let calls = dry_init();
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

/// Makes the file at `path` refuse writes (`read_only`), or take them again.
fn set_read_only(path: &Path, read_only: bool) {
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(if read_only { 0o444 } else { 0o644 });
    fs::set_permissions(path, permissions).unwrap();
}

/// `meta.recovered` of a write whose recovery listed one frame, replayed `records` changesets
/// and wrote `refs` ref files, and repaired nothing else.
fn listed_one(records: u64, refs: u64) -> Value {
    json!({
        "frames_listed": 1,
        "frames_rewritten": 0,
        "off_bytes_dropped": 0,
        "index_records_dropped": 0,
        "views_rebuilt": false,
        "records_replayed": records,
        "refs_replayed": refs,
    })
}

/// A commit, then a ref move, whose journal frame is durable in `journal.log` but whose listing
/// in `journal.off` fails (the file made read-only) has committed: the frame is its commit point.
/// Each exits 0 with a warning that names the append that failed (`append_durable
/// meta/journal.off`, relative to the store's root, never with the store's own path), and
/// readers see it already; while `journal.off` refuses writes, the next write fails, unable to
/// list the frame, with `E_IO` (`committed: false`: its transaction never began), not retryable,
/// since running it again meets the same refusal until the file's mode changes (ruling M16-R8),
/// its `details` naming the same append and its hint what to fix, and reports no recovery.
/// Once `journal.off` takes writes, the same commit run again finds its changeset (the store
/// holds it once), the same move finds it applied, each reporting in `meta.recovered` the
/// listing its write made and what it replayed, not as a warning; and the store verifies clean.
#[test]
fn a_frame_journal_off_refuses_to_list_is_committed_with_a_warning() {
    let store = Store::init().with_env(&[(NOW_ENV, &NOW.to_string())]);
    let off = store.layout().journal_off().into_std_path_buf();
    set_read_only(&off, true);
    if OpenOptions::new().append(true).open(&off).is_ok() {
        // Root writes to a read-only file all the same: nothing here can fail the listing.
        eprintln!("skipped: journal.off takes writes despite its mode (running as root?)");
        return;
    }
    let committed_with_a_warning = |run: &Run| {
        let result = run.ok();
        let warnings = result["warnings"].as_array().cloned().unwrap_or_default();
        assert_eq!(warnings.len(), 1, "{run}");
        let warning = warnings[0].as_str().unwrap();
        assert!(
            warning.contains("is committed") && warning.contains("append_durable meta/journal.off"),
            "{run}"
        );
        assert!(!warning.contains(store.path()), "{run}");
        result
    };

    let changes = store.changes("listing", "listing.txt", "committed all the same\n");
    let commit = ["commit", "-m", "listing", "--changes", changes.as_str()];
    let first = committed_with_a_warning(&store.run_as(&agent(0), &commit, &[]));
    let id = first["changeset"].as_str().unwrap().to_owned();
    assert_eq!(store.info()["changesets"], 1, "readers see its frame");
    let other = store.changes("other", "other.txt", "another change\n");
    let blocked = ["commit", "-m", "other", "--changes", other.as_str()];
    let blocked = store.run_as(&agent(0), &blocked, &[]);
    let error = blocked.err("E_IO");
    assert_eq!((blocked.exit(), &error["retryable"]), (1, &json!(false)));
    assert_eq!(
        error["details"],
        serde_json::json!({
            "committed": false,
            "fs_op": "append_durable",
            "store_file": "meta/journal.off"
        }),
        "{error}"
    );
    let hint = error["hint"].as_str().unwrap_or_default();
    assert!(
        hint.contains("writable"),
        "the hint says what to fix: {error}"
    );
    assert!(
        !error.to_string().contains(store.path()),
        "no host path: {error}"
    );
    let reported = blocked.envelope()["meta"].get("recovered").cloned();
    assert_eq!(
        reported, None,
        "a write that failed reports nothing: {blocked}"
    );
    set_read_only(&off, false);
    let again = store.run_as(&agent(0), &commit, &[]);
    let recovered = again.envelope()["meta"]["recovered"].clone();
    assert_eq!(recovered, listed_one(1, 0), "{again}");
    let again = again.ok();
    assert_eq!(again["changeset"], id.as_str(), "{again}");
    assert_eq!(
        again["blobs_written"], 0,
        "the first run wrote them: {again}"
    );
    assert!(again.get("warnings").is_none(), "{again}");
    assert_eq!(store.info()["changesets"], 1, "one changeset, found again");

    set_read_only(&off, true);
    let create = ["ref", "set", "main", id.as_str(), "--create"];
    let moved = committed_with_a_warning(&store.run_as(&agent(0), &create, &[]));
    assert_eq!(moved["already_applied"], false, "{moved}");
    assert_eq!(store.info()["main"], id.as_str(), "readers see the move");
    set_read_only(&off, false);
    let again = store.run_as(&agent(0), &create, &[]);
    let recovered = again.envelope()["meta"]["recovered"].clone();
    assert_eq!(recovered, listed_one(0, 1), "{again}");
    let again = again.ok();
    assert_eq!(again["already_applied"], true, "{again}");
    assert!(again.get("warnings").is_none(), "{again}");
    assert_eq!(store.info()["main"], id.as_str());
    store.assert_clean("after the listings that failed");
}
