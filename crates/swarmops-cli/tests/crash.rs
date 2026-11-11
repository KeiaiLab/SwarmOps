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
//! - The commit template holds 31 frames (`support::sweep`), far short of
//!   `Index::COMPACT_THRESHOLD` (1,024 records). The compaction sweep has a template of its own
//!   that holds the threshold's records, so that its commit compacts: it crashes the index sync,
//!   the views written and renamed into place, the `Compacted` frame and the checkpoint, in
//!   `just check`. The slow power-loss variant (`power_loss.rs`, `..._at_every_cut`, its run
//!   ending in `Op::Bulk`) covers the same calls against a lost power.
//!
//! - A revert (the M18 design §4), a land of the inverse change, is swept as a land is.
//! - A group land (the M18 design §6.3) is swept as a land is: its chain lands whole or not at
//!   all.
//! - A ref transaction (the M18 design §7) is swept as a ref move: every ref moves or none does.
//! - A batch of lands (the M18 D2 design §3) is swept through the binary too: a land leads two
//!   requests that killed `land` runs queued, kept alive by this test's flocks, and crashes at
//!   each of its calls; every key then lands once.
//!
//! Each test owns its temp dirs, and the tests share nothing.

use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use camino::Utf8Path;
use serde_json::{Value, json};
use swarmops_core::{ObjectKind, Signature, Tree};
use swarmops_ops::NOW_ENV;
use swarmops_repo::{LocalRepo, RawChangeset};
use swarmops_store::{
    CRASH_AT_ENV, FS_CALLS_ENV, FsOps, Index, Journal, LEAD_WAIT_ENV, MetaLock, RealFs, Record,
    StoreFormat,
};

use swarmops_testkit::child::Child;
use swarmops_testkit::per_format;

use crate::support::sweep::{
    DryRun, NOW, SWEPT_PUTS, TEMPLATE_FRAMES, calls_file, commit_template, dry_commit, dry_init,
    init_again, init_args, reported_calls, sweep, swept_commit,
};
use crate::support::{Run, Store, agent, run, swarmops};

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
    let journal = Journal::open(&store.layout(), Arc::new(RealFs::new())).unwrap();
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

per_format! {
/// A commit crashed at each of its filesystem calls in turn, from the first to past the last
/// (k = 1..=N + 1, N counted by a dry run): after each crash the store verifies clean, and the
/// next commit succeeds and is shown. The commit's frame is the 32nd past the checkpoint, so
/// the sweep crashes its index sync and checkpoint too. Every run signs at [`NOW`], so every
/// run makes the same calls, in the same order: two dry runs, in different seconds of the
/// clock, must count the same calls and commit the same changeset.
#[ignore = "slow, ~20 s: ~90 crashed commits; just check-slow runs it"]
fn kill9_during_commit_store_verifies_and_next_commit_succeeds(format) {
    let template = commit_template(format);
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
}

/// How many changesets the compaction sweep's template holds: the index's compaction
/// threshold, so that the swept commit's record takes the index's tail past it.
const COMPACT_TEMPLATE: u64 = Index::COMPACT_THRESHOLD;

/// Where the index compaction of a store stands.
#[derive(Debug, PartialEq, Eq)]
struct Compaction {
    /// The whole frames in its `journal.log`.
    frames: usize,
    /// How many of them are `Compacted` frames.
    marked: usize,
    /// How many records the index's sorted views cover.
    sorted: u64,
}

/// Where the index compaction of `store` stands.
fn compaction(store: &Store) -> Compaction {
    let fs: Arc<dyn FsOps> = Arc::new(RealFs::new());
    let journal = Journal::open(&store.layout(), Arc::clone(&fs)).unwrap();
    let frames = journal.tail_frames_from(0).unwrap();
    let marked = (frames.iter())
        .filter(|frame| {
            (frame.records.iter()).any(|record| matches!(record, Record::Compacted { .. }))
        })
        .count();
    let index = Index::open(&store.layout(), fs).unwrap();
    Compaction {
        frames: frames.len(),
        marked,
        sorted: index.sorted_count(),
    }
}

/// A store of `format` for the compaction sweep: main at a first changeset, then
/// [`COMPACT_TEMPLATE`] - 1 root changesets in one raw batch frame, so that the index holds
/// [`COMPACT_TEMPLATE`] records, none compacted. Every command on it signs at [`NOW`].
fn compaction_template(format: StoreFormat) -> Store {
    let template = Store::init_as(format).with_env(&[(NOW_ENV, &NOW.to_string())]);
    let first = template.commit(&agent(0), "README.md", "# compaction\n", None);
    let create = ["ref", "set", "main", first.as_str(), "--create"];
    template.run_as(&agent(0), &create, &[]).ok();

    // 1. The batch's changesets share the empty root tree, each its own message.
    let repo = LocalRepo::open(Utf8Path::new(template.path())).expect("open the repository");
    let tree = Tree::empty_root();
    repo.put_objects(vec![(ObjectKind::Tree, tree.encode())])
        .expect("put the empty tree");
    let secs = i64::try_from(NOW).unwrap();
    let signature = Signature::new(b"bulk".to_vec(), b"bulk@test".to_vec(), secs, 0, false);
    let signature = signature.expect("a signature");
    let raws = (1..COMPACT_TEMPLATE)
        .map(|n| {
            let message = format!("bulk {n}").into_bytes();
            let (author, committer) = (signature.clone(), signature.clone());
            RawChangeset::new(
                Vec::new(),
                tree.id(),
                author,
                committer,
                message,
                vec![],
                vec![],
            )
        })
        .collect();
    repo.commit_raw_batch(raws).expect("the batch");
    drop(repo);

    // 2. Every record in the index's tail: the next one compacts.
    assert_eq!(template.info()["changesets"], COMPACT_TEMPLATE);
    let compacted = compaction(&template);
    assert_eq!(
        (compacted.marked, compacted.sorted),
        (0, 0),
        "the template compacted"
    );
    template
}

/// How far a run of the compaction sweep got before it ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompactCut {
    /// Before the commit point: no changeset.
    Uncommitted,
    /// Committed, the index's sorted views not in place.
    Unsorted,
    /// The views in place, no `Compacted` frame.
    Unmarked,
    /// A whole `Compacted` frame in the journal, the run crashed after it.
    Marked,
    /// The run finished.
    Finished,
}

/// One run of the compaction sweep: [`swept_commit`] on a copy of `template`, its k-th call
/// aborting it. The store verifies clean and holds the dry run's changeset or none; sorted views
/// only if it holds it, a `Compacted` frame only with them. The next commit lands, `show` finds
/// it, and the views then cover the template's records, with one `Compacted` frame at most: a
/// crash after the views' renames leaves them in place, and the next writer adopts them.
fn crash_compaction_at(template: &Store, k: u64, dry: &DryRun) -> CompactCut {
    let store = Store::copy_of(template);
    let crash_at = k.to_string();
    let crash = swept_commit(&store, &[(CRASH_AT_ENV, crash_at.as_str())]);
    let finished = k > dry.calls;
    if finished {
        assert_eq!(crash.ok()["changeset"], dry.changeset.as_str(), "k={k}");
    } else {
        assert!(crash.aborted(), "k={k} of {}: {crash}", dry.calls);
    }
    store.assert_clean(&format!("k={k}: after the crash"));

    // 1. The changeset, and the compaction only past it.
    let changesets = store.info()["changesets"].as_u64().unwrap();
    let committed = match changesets - COMPACT_TEMPLATE {
        0 => false,
        1 => true,
        n => panic!("k={k}: {n} new changesets"),
    };
    let shown = store.read(&["show", &dry.changeset]);
    if committed {
        shown.ok();
    } else {
        shown.err("E_REV_NOT_FOUND");
    }
    let crashed = compaction(&store);
    let (sorted, marked) = (crashed.sorted > 0, crashed.marked > 0);
    assert!(
        committed || !sorted,
        "k={k}: views, not committed: {crashed:?}"
    );
    assert!(
        sorted || !marked,
        "k={k}: a Compacted frame, no views: {crashed:?}"
    );

    // 2. The next commit lands, and the index is compacted once.
    let next = store.commit(&agent(2), "after.txt", &format!("after k={k}\n"), None);
    assert_eq!(
        store.show(&next)["changes"][0]["path"],
        "after.txt",
        "k={k}"
    );
    assert_eq!(store.info()["changesets"], changesets + 1, "k={k}");
    let after = compaction(&store);
    assert!(
        after.sorted >= COMPACT_TEMPLATE,
        "k={k}: not compacted: {after:?}"
    );
    assert!(after.marked <= 1, "k={k}: compacted twice: {after:?}");
    store.assert_clean(&format!("k={k}: after the next commit"));
    match (finished, committed, sorted, marked) {
        (true, ..) => CompactCut::Finished,
        (_, false, ..) => CompactCut::Uncommitted,
        (_, _, false, _) => CompactCut::Unsorted,
        (_, _, _, false) => CompactCut::Unmarked,
        _ => CompactCut::Marked,
    }
}

per_format! {
/// A commit whose record takes the index's tail past [`Index::COMPACT_THRESHOLD`], crashed at
/// each of its filesystem calls in turn (issue #16 item 1): the sweep crashes the index sync,
/// the sorted views written and renamed into place, the `Compacted` frame and the checkpoint.
/// After each crash the store verifies clean and the next commit lands. The sweep reaches a
/// crash before the commit point, one before the views are in place (committed, no views), one
/// between their renames and the `Compacted` frame, and one after that frame (not finished).
fn crash_sweep_crosses_the_compaction_threshold(format) {
    let template = compaction_template(format);

    // 1. The dry run: its calls counted; it appends its frame and a `Compacted` one.
    let store = Store::copy_of(&template);
    let file = calls_file(&store);
    let frames = compaction(&store).frames;
    let result = swept_commit(&store, &[(FS_CALLS_ENV, file.to_str().unwrap())]).ok();
    let compacted = Compaction {
        frames: frames + 2,
        marked: 1,
        sorted: COMPACT_TEMPLATE + 1,
    };
    assert_eq!(compaction(&store), compacted, "the dry run compacts");
    let dry = DryRun {
        calls: reported_calls(&file),
        changeset: result["changeset"].as_str().unwrap().to_owned(),
    };

    // 2. A crash at each call, and one past the last.
    let crashes = sweep(1..=dry.calls + 1, |k| crash_compaction_at(&template, k, &dry));
    let cuts = [
        CompactCut::Uncommitted,
        CompactCut::Unsorted,
        CompactCut::Unmarked,
        CompactCut::Marked,
        CompactCut::Finished,
    ];
    for cut in cuts {
        assert!(crashes.contains(&cut), "no run ended {cut:?}: {crashes:?}");
    }
    println!("compaction sweep {format:?}: {} calls", dry.calls);
}
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

per_format! {
/// Every crash sweep runs under the order checker (`swarmops_store::OrderCheckFs`, rules W1 to
/// W8): the tests set `SWARMOPS_TEST_ORDER_CHECK=1` for every run of the binary, and the store's
/// and the repository's sweeps wrap their crashing child's filesystem in it, and the one their
/// parent recovers on, so a write that breaks a rule panics in the run crashed at any call, in a
/// dry run, or in what recovers from the crash. This sweep, in the default gate, crashes a root
/// commit on a fresh store, a ref move on top of it and an `op undo` of that move at each of
/// their calls; the slow sweeps (`kill9_during_*`) do the same for `init` and for a commit that
/// syncs the index and moves the checkpoint.
fn order_check_rules_hold_in_every_crash_sweep(format) {
    let template = Store::init_as(format).with_env(&[(NOW_ENV, &NOW.to_string())]);
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

    // The move made on the template, operation 1, and its undo swept: it deletes the ref again.
    // `main` is never deleted, so the swept ref is a release ref.
    let args = ["ref", "set", "release/swept", id.as_str(), "--create"];
    template.run_as(&agent(0), &args, &[]).ok();
    let listed = template.read(&["op", "log", "-n", "1"]).ok();
    let op1 = listed["ops"][0]["id"].as_str().expect("an id").to_owned();
    assert!(op1.starts_with("1:"), "{listed}");
    let undo = |_: &Store| vec!["op".to_owned(), "undo".to_owned(), op1.clone()];
    let calls = sweep_checked(&template, "an undo", undo);
    assert!(
        calls > 5,
        "an undo writes its frame and removes the ref's file: {calls}"
    );
}
}

per_format! {
/// A land crashed at any filesystem call lands once or not at all (operations spec §3, §8),
/// through the binary: on a store where main names `t0` and an agent's commit on it waits to
/// land, the land, keyed, crashed at each call k of the N its dry run makes, leaves the store
/// verifying clean; and the same command run again ends with main at the dry run's landed
/// changeset, on `t0`, the only changeset on main past it: replayed where the crashed run had
/// published it, landed anew (onto `t0`, so main had stayed there) otherwise. Every run signs at
/// one fixed time, so every land makes the same changeset. Past N, the land finishes as the dry
/// run did. Both sides of the commit point are reached.
fn a_land_crashing_at_any_call_lands_once_through_the_binary(format) {
    const KEY: &str = "crash-land-1";
    let template = Store::init_as(format).with_env(&[(NOW_ENV, &NOW.to_string())]);
    let t0 = template.commit(&agent(0), "base.txt", "the base\n", None);
    let create = ["ref", "set", "main", t0.as_str(), "--create"];
    template.run_as(&agent(0), &create, &[]).ok();
    let head = template.commit(&agent(0), "change.txt", "the change\n", Some(&t0));
    let land = ["land", head.as_str(), "--key", KEY];

    let dry = Store::copy_of(&template);
    let file = calls_file(&dry);
    let result = dry.run_as(&agent(1), &land, &[(FS_CALLS_ENV, file.to_str().unwrap())]);
    let result = result.ok();
    assert_eq!(result["status"], "landed", "{result}");
    let landed = result["landed"].as_str().unwrap().to_owned();
    assert_eq!(dry.show(&landed)["land_key"], KEY);
    let calls = reported_calls(&file);
    assert!(calls > 20, "a land makes dozens of calls: {calls}");

    let published = sweep(1..=calls + 1, |k| {
        let store = Store::copy_of(&template);
        let crash_at = k.to_string();
        let run = store.run_as(&agent(1), &land, &[(CRASH_AT_ENV, &crash_at)]);
        if k <= calls {
            assert!(run.aborted(), "k={k} of {calls}: {run}");
        } else {
            assert_eq!(run.ok()["landed"], landed.as_str(), "k={k}");
        }
        store.assert_clean(&format!("k={k}: after the crash"));
        let again = store.run_as(&agent(1), &land, &[]).ok();
        assert_eq!(
            (&again["landed"], &again["rebased_onto"]),
            (&json!(landed), &json!(t0)),
            "k={k}: {again}"
        );
        let log = store.read(&["log"]).ok();
        let on_main: Vec<&str> = (log["entries"].as_array().unwrap().iter())
            .map(|entry| entry["id"].as_str().unwrap())
            .collect();
        assert_eq!(on_main, [landed.as_str(), t0.as_str()], "k={k}");
        store.assert_clean(&format!("k={k}: after the same land again"));
        match again["status"].as_str() {
            Some("replayed") => true,
            Some("landed") => false,
            _ => panic!("k={k}: {again}"),
        }
    });
    assert!(
        published.iter().any(|&replayed| replayed),
        "some crash came after the land's commit point: the same land again replayed it"
    );
    assert!(
        published.iter().any(|&replayed| !replayed),
        "some before it"
    );
}
}

/// Commits `content` at `path` on main's head as `agent`, moving main to it: the new changeset.
fn advance_main(store: &Store, agent: &str, path: &str, content: &str) -> String {
    let changes = store.changes(path, path, content);
    let args = [
        "commit",
        "-m",
        path,
        "--changes",
        &changes,
        "--advance",
        "main",
    ];
    let result = store.run_as(agent, &args, &[]).ok();
    result["changeset"]
        .as_str()
        .expect("a changeset")
        .to_owned()
}

per_format! {
/// A revert crashed at any filesystem call lands once or not at all (the M18 design §4.3),
/// through the binary: on a store whose main is `t0`, then A, then B, the keyed revert of A,
/// crashed at each call k of the N its dry run makes, leaves the store verifying clean; and the
/// same command run again ends with main at the dry run's landed changeset, on B, the only
/// changeset on main past it, recording A as what it reverted: replayed where the crashed run
/// had published it, landed anew otherwise. Both sides of the commit point are reached.
fn a_revert_crashing_at_any_call_lands_once_through_the_binary(format) {
    const KEY: &str = "crash-revert-1";
    let template = Store::init_as(format).with_env(&[(NOW_ENV, &NOW.to_string())]);
    let t0 = advance_main(&template, &agent(0), "base.txt", "the base\n");
    let a = advance_main(&template, &agent(0), "a.txt", "a\n");
    let b = advance_main(&template, &agent(0), "b.txt", "b\n");
    let revert = ["revert", a.as_str(), "--reason", "a was wrong", "--key", KEY];

    // 1. The dry run, its calls counted.
    let dry = Store::copy_of(&template);
    let file = calls_file(&dry);
    let result = dry.run_as(&agent(1), &revert, &[(FS_CALLS_ENV, file.to_str().unwrap())]);
    let result = result.ok();
    assert_eq!(result["land"]["status"], "landed", "{result}");
    let landed = result["land"]["landed"].as_str().unwrap().to_owned();
    assert_eq!(dry.show(&landed)["reverts"], json!([a]));
    let calls = reported_calls(&file);
    assert!(calls > 20, "a revert makes dozens of calls: {calls}");

    // 2. Each call crashed, then the same revert again.
    let published = sweep(1..=calls + 1, |k| {
        let store = Store::copy_of(&template);
        let crash_at = k.to_string();
        let run = store.run_as(&agent(1), &revert, &[(CRASH_AT_ENV, &crash_at)]);
        if k <= calls {
            assert!(run.aborted(), "k={k} of {calls}: {run}");
        } else {
            assert_eq!(run.ok()["land"]["landed"], landed.as_str(), "k={k}");
        }
        store.assert_clean(&format!("k={k}: after the crash"));
        let again = store.run_as(&agent(1), &revert, &[]).ok();
        let land = &again["land"];
        assert_eq!(
            (&land["landed"], &land["rebased_onto"], &again["reverted"]),
            (&json!(landed), &json!(b), &json!([a])),
            "k={k}: {again}"
        );
        let log = store.read(&["log"]).ok();
        let on_main: Vec<&str> = (log["entries"].as_array().unwrap().iter())
            .map(|entry| entry["id"].as_str().unwrap())
            .collect();
        assert_eq!(on_main, [&landed, &b, &a, &t0], "k={k}");
        store.assert_clean(&format!("k={k}: after the same revert again"));
        match land["status"].as_str() {
            Some("replayed") => true,
            Some("landed") => false,
            _ => panic!("k={k}: {again}"),
        }
    });
    assert!(
        published.iter().any(|&replayed| replayed),
        "some crash came after the revert's commit point: the same revert again replayed it"
    );
    assert!(
        published.iter().any(|&replayed| !replayed),
        "some before it"
    );
}
}

per_format! {
/// A group land crashed at any filesystem call lands its whole chain or none of it (the M18
/// design §6.3), through the binary: on a store whose main is `t0`, with two changes on it,
/// `land-group` of both, crashed at each call k of the N its dry run makes, leaves the store
/// verifying clean with main at `t0` or at the whole chain; and the same command run again ends
/// with main at the dry run's chain, both links on `t0`: landed anew where main had stayed, or
/// `E_NOTHING_TO_LAND` where the crashed run had published it. Both sides of the commit point
/// are reached.
fn a_group_land_crashing_at_any_call_lands_all_or_none_through_the_binary(format) {
    let template = Store::init_as(format).with_env(&[(NOW_ENV, &NOW.to_string())]);
    let t0 = advance_main(&template, &agent(0), "base.txt", "the base\n");
    let one = template.commit(&agent(0), "one.txt", "one\n", Some(&t0));
    let two = template.commit(&agent(0), "two.txt", "two\n", Some(&t0));
    let group = ["land-group", one.as_str(), two.as_str()];

    // 1. The dry run, its calls counted: a chain of two on `t0`.
    let dry = Store::copy_of(&template);
    let file = calls_file(&dry);
    let result = dry.run_as(&agent(1), &group, &[(FS_CALLS_ENV, file.to_str().unwrap())]);
    let result = result.ok();
    let chain: Vec<String> = (result["items"].as_array().unwrap().iter())
        .map(|item| item["landed"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(result["head"], chain[1].as_str(), "{result}");
    let calls = reported_calls(&file);
    assert!(calls > 20, "a group land makes dozens of calls: {calls}");

    // 2. Each call crashed: main at `t0` or the whole chain; then the same group again.
    let want = [chain[1].as_str(), chain[0].as_str(), t0.as_str()];
    let published = sweep(1..=calls + 1, |k| {
        let store = Store::copy_of(&template);
        let crash_at = k.to_string();
        let run = store.run_as(&agent(1), &group, &[(CRASH_AT_ENV, &crash_at)]);
        if k <= calls {
            assert!(run.aborted(), "k={k} of {calls}: {run}");
        } else {
            assert_eq!(run.ok()["head"], chain[1].as_str(), "k={k}");
        }
        store.assert_clean(&format!("k={k}: after the crash"));
        let main_at = |store: &Store| -> Vec<String> {
            let log = store.read(&["log"]).ok();
            (log["entries"].as_array().unwrap().iter())
                .map(|entry| entry["id"].as_str().unwrap().to_owned())
                .collect()
        };
        let crashed = main_at(&store);
        let landed = crashed.len() == want.len();
        assert!(landed || crashed == [t0.as_str()], "k={k}: main is part of a chain: {crashed:?}");
        let again = store.run_as(&agent(1), &group, &[]);
        if landed {
            again.err("E_NOTHING_TO_LAND");
        } else {
            assert_eq!(again.ok()["head"], chain[1].as_str(), "k={k}");
        }
        assert_eq!(main_at(&store), want, "k={k}");
        store.assert_clean(&format!("k={k}: after the same group again"));
        landed
    });
    assert!(
        published.iter().any(|&landed| landed),
        "some crash came after the group's commit point"
    );
    assert!(published.iter().any(|&landed| !landed), "some before it");
}
}

per_format! {
/// A ref transaction crashed at any filesystem call moves every ref or none (the M18 design
/// §7), through the binary: on a store whose main and `release/a` are at `t0`, `ref txn` of main
/// and `release/a` to `t1` and of a new `release/b` at `t1`, crashed at each call k of the N its
/// dry run makes, leaves the store verifying clean with every ref where it was or every ref
/// moved; and the same command run again moves them where none had moved, or is
/// `E_CAS_MISMATCH` where all had. Both sides of the commit point are reached.
fn a_txn_crashing_at_any_call_moves_all_or_none(format) {
    let template = Store::init_as(format).with_env(&[(NOW_ENV, &NOW.to_string())]);
    let t0 = advance_main(&template, &agent(0), "base.txt", "the base\n");
    let t1 = template.commit(&agent(0), "one.txt", "one\n", Some(&t0));
    let create = ["ref", "set", "release/a", t0.as_str(), "--create"];
    template.run_as(&agent(0), &create, &[]).ok();
    let moves = [
        format!("main={t0}:{t1}"),
        format!("release/a={t0}:{t1}"),
        format!("release/b=new:{t1}"),
    ];
    let mut txn = vec!["ref", "txn"];
    for spec in &moves {
        txn.extend(["--move", spec.as_str()]);
    }
    let refs = |store: &Store| -> Value { store.read(&["ref", "list"]).ok()["refs"].clone() };
    let targets = |store: &Store| -> Vec<(String, String)> {
        (refs(store).as_array().unwrap().iter())
            .map(|r| {
                let name = r["name"].as_str().unwrap().to_owned();
                (name, r["target"].as_str().unwrap().to_owned())
            })
            .collect()
    };
    let pair = |name: &str, at: &str| (name.to_owned(), at.to_owned());
    let before = vec![pair("main", &t0), pair("release/a", &t0)];
    let after = vec![pair("main", &t1), pair("release/a", &t1), pair("release/b", &t1)];

    // 1. The dry run, its calls counted: every ref moved.
    let dry = Store::copy_of(&template);
    let file = calls_file(&dry);
    let result = dry.run_as(&agent(1), &txn, &[(FS_CALLS_ENV, file.to_str().unwrap())]);
    assert_eq!(result.ok()["moves"].as_array().unwrap().len(), 3);
    assert_eq!(targets(&dry), after);
    let calls = reported_calls(&file);
    assert!(calls > 5, "a transaction makes several calls: {calls}");

    // 2. Each call crashed: every ref where it was, or every ref moved; then the same again.
    let moved = sweep(1..=calls + 1, |k| {
        let store = Store::copy_of(&template);
        let crash_at = k.to_string();
        let run = store.run_as(&agent(1), &txn, &[(CRASH_AT_ENV, &crash_at)]);
        if k <= calls {
            assert!(run.aborted(), "k={k} of {calls}: {run}");
        } else {
            run.ok();
        }
        store.assert_clean(&format!("k={k}: after the crash"));
        let crashed = targets(&store);
        let moved = crashed == after;
        assert!(moved || crashed == before, "k={k}: a mixed state: {crashed:?}");
        let again = store.run_as(&agent(1), &txn, &[]);
        if moved {
            again.err("E_CAS_MISMATCH");
        } else {
            again.ok();
        }
        assert_eq!(targets(&store), after, "k={k}");
        store.assert_clean(&format!("k={k}: after the same transaction again"));
        moved
    });
    assert!(
        moved.iter().any(|&moved| moved),
        "some crash came after the transaction's commit point"
    );
    assert!(moved.iter().any(|&moved| !moved), "some before it");
}
}

/// How many requests the batch sweep seeds behind its leader.
const SEEDS: usize = 2;

/// How long the batch sweep waits for a request to be queued.
const QUEUE_PATIENCE: Duration = Duration::from_secs(10);

/// The idempotency key of the batch sweep's land `n`: 0 the leader's, then the seeds'.
fn batch_key(n: usize) -> String {
    format!("crash-batch-{n}")
}

/// The names in the queue of `store`, sorted; none if it has no queue.
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

/// How many calls past its dry run's count the batch sweep crashes the leader at: a run's count
/// varies by one or so.
const CALLS_SLACK: u64 = 3;

/// Whether queue file `name` has extension `ext`: `req`, `com` or `ans`.
fn is_kind(name: &str, ext: &str) -> bool {
    !name.starts_with('.') && Path::new(name).extension().is_some_and(|e| e == ext)
}

/// The batch sweep's template: a store whose `main` is `t0`, a change on `t0` per land (the
/// leader's first), and each seed's request as a killed `land --key` queued it.
struct Seeded {
    template: Store,
    t0: String,
    heads: Vec<String>,
    /// Each seed's request file: its name and bytes.
    requests: Vec<(String, Vec<u8>)>,
}

/// The arguments of the batch sweep's land `n` on `store`, as `agent(n)`.
fn batch_land(store: &Store, seeded: &Seeded, n: usize) -> Vec<String> {
    let key = batch_key(n);
    store.args(&agent(n), &["land", &seeded.heads[n], "--key", &key])
}

/// Builds [`Seeded`] of `format`: each seed's land run behind the lock this test holds, its
/// request read once queued, the run killed and its request removed.
fn seeded_template(format: StoreFormat) -> Seeded {
    let now = NOW.to_string();
    let template = Store::init_as(format).with_env(&[(NOW_ENV, &now)]);
    let t0 = template.commit(&agent(0), "base.txt", "the base\n", None);
    let create = ["ref", "set", "main", t0.as_str(), "--create"];
    template.run_as(&agent(0), &create, &[]).ok();
    let heads = (0..=SEEDS)
        .map(|n| {
            let path = format!("change-{n}.txt");
            template.commit(&agent(0), &path, &format!("{n}\n"), Some(&t0))
        })
        .collect();
    let mut seeded = Seeded {
        template,
        t0,
        heads,
        requests: Vec::new(),
    };

    let queue = seeded.template.layout().queue_dir();
    for n in 1..=SEEDS {
        // 1. The lock held, the seed's land queued behind it.
        let lock = MetaLock::acquire(&seeded.template.layout(), QUEUE_PATIENCE).expect("the lock");
        let args = batch_land(&seeded.template, &seeded, n);
        let waiter = Child::spawn(&mut swarmops(&args, &[(NOW_ENV, &now)]));
        let limit = Instant::now() + QUEUE_PATIENCE;
        let name = loop {
            let found = queue_names(&seeded.template);
            if let Some(name) = found.into_iter().find(|name| is_kind(name, "req")) {
                break name;
            }
            assert!(Instant::now() < limit, "seed {n} never queued");
            thread::sleep(Duration::from_millis(1));
        };

        // 2. The seed killed, its request kept apart.
        drop(waiter);
        let path = queue.join(&name);
        let bytes = fs::read(path.as_std_path()).expect("the request");
        fs::remove_file(path.as_std_path()).expect("remove the request");
        seeded.requests.push((name, bytes));
        drop(lock);
    }
    seeded
}

/// What one run of the batch sweep left: whether the leader finished, and the queue's files
/// when it ended.
struct BatchRun {
    finished: bool,
    left: Vec<String>,
}

/// One run of the batch sweep on a copy of the template: the seeds' requests in its queue,
/// flocked by this test as their live waiters would, the leader's land run with `hooks` (the
/// lead-wait hook asking for the two). Then the seeds' flocks go, and each land runs again:
/// the store verifies clean after the crash and after, `main` holds each key once, in the
/// batch's order, and the next write's janitor empties the queue.
fn batch_run(seeded: &Seeded, what: &str, hooks: &[(&str, &str)]) -> BatchRun {
    let store = Store::copy_of(&seeded.template);

    // 1. The seeds, alive: their requests in place, flocked.
    let queue = store.layout().queue_dir();
    fs::create_dir_all(queue.as_std_path()).expect("the queue");
    let alive: Vec<File> = (seeded.requests.iter())
        .map(|(name, bytes)| {
            let path = queue.join(name);
            fs::write(path.as_std_path(), bytes).expect("a seed's request");
            let file = File::open(path.as_std_path()).expect("open a seed's request");
            file.lock().expect("a seed's flock");
            file
        })
        .collect();

    // 2. The leader.
    let seeds = SEEDS.to_string();
    let mut env = vec![(LEAD_WAIT_ENV, seeds.as_str())];
    env.extend_from_slice(hooks);
    let land = batch_land(&store, seeded, 0);
    let land: Vec<&str> = land.iter().map(String::as_str).collect();
    let led = run(&land, &env);
    let finished = !led.aborted();
    let left = queue_names(&store);
    drop(alive);
    if finished {
        assert_eq!(led.ok()["status"], "landed", "{what}: {led}");
    }
    store.assert_clean(&format!("{what}: after the leader"));

    // 3. Every land again: each replays one the batch committed, or lands.
    for n in 0..=SEEDS {
        let args = batch_land(&store, seeded, n);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let again = run(&args, &[]).ok();
        let status = again["status"].as_str();
        let want: &[&str] = if finished {
            &["replayed"]
        } else {
            &["landed", "replayed"]
        };
        assert!(
            status.is_some_and(|s| want.contains(&s)),
            "{what}: land {n} again: {again}"
        );
    }

    // 4. Each key once on main, in the batch's order; the queue empty.
    let log = store.read(&["log", "main"]).ok();
    let on_main: Vec<String> = (log["entries"].as_array().expect("log entries").iter())
        .map(|entry| entry["id"].as_str().expect("an id").to_owned())
        .collect();
    let keys: Vec<Value> = on_main
        .iter()
        .map(|id| store.show(id)["land_key"].clone())
        .collect();
    let want: Vec<Value> = (0..=SEEDS).rev().map(|n| json!(batch_key(n))).collect();
    assert_eq!(
        (&keys[..=SEEDS], on_main.last()),
        (&want[..], Some(&seeded.t0)),
        "{what}: main {on_main:?}"
    );
    assert_eq!(on_main.len(), SEEDS + 2, "{what}: main {on_main:?}");
    store.assert_clean(&format!("{what}: after the lands again"));

    // 5. A replay takes no turn: the next write's janitor empties the queue.
    store.commit(&agent(0), "after.txt", "after\n", Some(&seeded.t0));
    assert_eq!(
        queue_names(&store),
        Vec::<String>::new(),
        "{what}: the queue"
    );
    BatchRun { finished, left }
}

/// How many of the seeds `ran`'s leader answered.
fn answered(ran: &BatchRun) -> usize {
    ran.left.iter().filter(|name| is_kind(name, "ans")).count()
}

per_format! {
/// A batch led through the binary lands each member once whatever call crashes it (the M18 D2
/// design §5.1): the leader's `land --key`, `SWARMOPS_TEST_LEAD_WAIT=2`, leads the requests of
/// two waiting `land --key` runs (seeded, [`seeded_template`]) and crashes at each call k of
/// the N its dry run makes, and of [`CALLS_SLACK`] more. The leader's count of calls is not
/// the same in every run: on a loaded host one run made one call fewer than another (92 and
/// 93), so a run may finish a little before N or crash a little past it. After each, every
/// land run again leaves the three keys on `main` once, in the batch's order (the leader's,
/// then the seeds' oldest first), the store clean, and the queue empty after the next write.
/// The dry run answered both seeds (their answers stay: no waiter reads them); the sweep
/// crashes up to about N, finishes at its end, and reaches a crash that left claimed requests
/// (`.com`) and one that left an answer (`.ans`).
fn a_batch_led_through_the_binary_crashing_at_any_call_lands_each_once(format) {
    let seeded = seeded_template(format);

    // 1. The dry run: its calls counted; it answers both seeds.
    let file = calls_file(&seeded.template);
    let file = file.to_str().expect("a UTF-8 path").to_owned();
    let dry = batch_run(&seeded, "the dry run", &[(FS_CALLS_ENV, &file)]);
    assert!(dry.finished, "the dry run crashed");
    assert_eq!(answered(&dry), SEEDS, "the dry run left {:?}", dry.left);
    let calls = reported_calls(Path::new(&file));
    assert!(calls > 40, "a batch of three lands makes many calls: {calls}");

    // 2. A crash at each call, and a few past the dry run's count.
    let runs = sweep(1..=calls + CALLS_SLACK, |k| {
        let crash_at = k.to_string();
        let ran = batch_run(&seeded, &format!("k={k} of {calls}"), &[(CRASH_AT_ENV, &crash_at)]);
        (k, ran)
    });
    let first_finished = runs.iter().find(|(_, ran)| ran.finished).map(|&(k, _)| k);
    assert!(
        first_finished.is_some_and(|k| k + CALLS_SLACK > calls),
        "the first run to finish crashed at k={first_finished:?} of {calls}"
    );
    assert!(runs.last().is_some_and(|(_, ran)| ran.finished), "a crash after the end");
    let left_a = |suffix: &str| {
        (runs.iter())
            .filter(|(_, ran)| !ran.finished)
            .any(|(_, ran)| ran.left.iter().any(|name| is_kind(name, suffix)))
    };
    assert!(left_a("com"), "no crash left a claimed request");
    assert!(left_a("ans"), "no crash left an answer");
    println!("batch sweep {format:?}: {calls} calls, the first finished at {first_finished:?}");
}
}

/// One run of the init sweep: `init` of a store of `format` on a fresh path, its k-th filesystem
/// call (of the `calls` a dry run counted) aborting it. Then either `info` says `E_NOT_A_REPO` and a second
/// `init` succeeds, or `info` succeeds (and a second `init` says `E_EXISTS`); either way the
/// store is fresh and verifies clean ([`init_again`]). Returns whether `info` found the store
/// right after the crash.
fn init_crashing_at(format: StoreFormat, k: u64, calls: u64) -> bool {
    let store = Store::empty();
    let crash_at = k.to_string();
    let init = run(
        &init_args(format, store.path()),
        &[(CRASH_AT_ENV, crash_at.as_str())],
    );
    if k <= calls {
        assert!(init.aborted(), "k={k}: {init}");
    } else {
        init.ok();
    }
    init_again(&store, format, &format!("k={k}"))
}

per_format! {
/// `init` crashed at each of its filesystem calls in turn (`LocalRepo::create`, with
/// `MetaStore::create` inside it, then the first snapshot `init` reports from), from the first
/// to past the last: before `FORMAT` is linked there is no store and a second `init` makes it;
/// from then on the store is whole.
#[ignore = "slow, ~25 s: ~300 crashed inits; just check-slow runs it"]
fn kill9_during_init_leaves_no_store_or_a_whole_one(format) {
    let calls = dry_init(format);
    let whole = sweep(1..=calls + 1, |k| init_crashing_at(format, k, calls));
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

per_format! {
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
fn a_frame_journal_off_refuses_to_list_is_committed_with_a_warning(format) {
    let store = Store::init_as(format).with_env(&[(NOW_ENV, &NOW.to_string())]);
    let off = store.layout().journal_off().into_std_path_buf();
    // `meta/journal.off`, or FORMAT 2's `meta/journal2.off`.
    let off_file = format!("meta/{}", off.file_name().unwrap().to_str().unwrap());
    let off_append = format!("append_durable {off_file}");
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
            warning.contains("is committed") && warning.contains(&off_append),
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
            "store_file": off_file
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
}
