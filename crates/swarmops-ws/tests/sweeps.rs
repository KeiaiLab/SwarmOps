//! The crash sweeps of the workspace operations, under the test hooks (M5 design §2.4, W10):
//! a child process aborted at each call of the host layer (`CrashAtHost`) and of the durable
//! files' layer (`CrashAtFs`) of an update, a commit, a rebase, a land and a checked land.
//!
//! Afterwards, in each case: the materialisation the child left is finished by the next open,
//! the same operation again ends what it began (the pending record is resumable), the store
//! verifies deep, the index's status equals the `--full` status, the registry is consistent, and
//! the store and the workspace hold what a run without a crash leaves (so each change is made
//! once). The write-order rules WS1 to WS3 are checked on the uncut run's events, and each
//! detector is proved on a trace with its rule broken.
//!
//! The sweeps of an update, a commit and a rebase cut at every call. The land's two are sampled
//! in the gate (every third host call, every durable call) and cut at every call in their
//! `#[ignore]`d twins.

use crate::fixture::sweep::{
    self, Event, Scenario, check_order, intent_removal, intent_write, pending_write, reordered,
    state_write, store_call, tree_write,
};

/// The sweep of `scenario` at every `stride`-th host call and every durable call.
fn sweep(test: &str, scenario: Scenario, stride: u64) {
    sweep::run(test, scenario, stride);
}

/// An update killed at any call is finished by the next open, and the update again is a no-op.
#[test]
fn a_killed_update_finishes_on_the_next_open() {
    let test = swarmops_testkit::test_name!("a_killed_update_finishes_on_the_next_open");
    sweep(&test, Scenario::Update, 1);
}

/// An update killed after a file was written and before its executable bit was set is finished
/// by the next open, not refused as an edit of that file.
#[test]
fn an_update_killed_before_the_mode_finishes_on_the_next_open() {
    sweep::update_cut_before_the_mode();
}

/// A commit killed at any call replays the same changeset, once.
#[test]
fn a_killed_commit_replays_once() {
    let test = swarmops_testkit::test_name!("a_killed_commit_replays_once");
    sweep(&test, Scenario::Commit, 1);
}

/// A rebase killed at any call is finished by the same rebase again.
#[test]
fn a_killed_rebase_finishes() {
    let test = swarmops_testkit::test_name!("a_killed_rebase_finishes");
    sweep(&test, Scenario::Rebase, 1);
}

/// A land killed at any call lands once.
#[test]
fn a_killed_land_lands_once_sampled() {
    let test = swarmops_testkit::test_name!("a_killed_land_lands_once_sampled");
    sweep(&test, Scenario::Land, 3);
}

/// A checked land killed at any call lands once, the tree it checked.
#[test]
#[cfg(unix)]
fn a_killed_checked_land_lands_once_sampled() {
    let test = swarmops_testkit::test_name!("a_killed_checked_land_lands_once_sampled");
    sweep(&test, Scenario::LandChecked, 3);
}

/// The land sweep at every call.
#[test]
#[ignore = "slow, ~90 s: a child process per host and fs call of a land"]
fn a_killed_land_lands_once() {
    let test = swarmops_testkit::test_name!("a_killed_land_lands_once");
    sweep(&test, Scenario::Land, 1);
}

/// The checked-land sweep at every call.
#[test]
#[cfg(unix)]
#[ignore = "slow, ~90 s: a child process per host and fs call of a checked land"]
fn a_killed_checked_land_lands_once() {
    let test = swarmops_testkit::test_name!("a_killed_checked_land_lands_once");
    sweep(&test, Scenario::LandChecked, 1);
}

/// The events of the uncut run of every scenario keep the write-order rules WS1 to WS3.
#[test]
fn the_operations_keep_the_write_order() {
    for scenario in [
        Scenario::Update,
        Scenario::Commit,
        Scenario::Rebase,
        Scenario::Land,
    ] {
        let events = sweep::reference(scenario).events;
        assert!(!events.is_empty(), "{scenario:?}");
        if let Err(rule) = check_order(&events) {
            panic!("{scenario:?}: {rule}\n{events:#?}");
        }
    }
}

/// Self-check of the detectors: the commit's and the land's events with WS1 reversed (the
/// pending record after the store call), WS2 reversed (`workspace.json` before it) and WS3
/// reversed (the intent after the first write of the tree, or removed before the state) each fail the checker, naming the
/// rule; the unbroken events pass.
#[test]
fn each_order_detector_finds_its_rule_broken() {
    // 1. WS1 and WS2 on a commit's events: the pending record after the store call; the state
    //    before it.
    let commit = sweep::reference(Scenario::Commit).events;
    assert_eq!(check_order(&commit), Ok(()));
    let late_pending = reordered(&commit, pending_write, store_call);
    assert!(check_order(&late_pending).unwrap_err().starts_with("WS1"));
    let mut early_state = commit.clone();
    let at = early_state.iter().position(state_write).unwrap();
    let state = early_state.remove(at);
    let first = early_state.iter().position(store_call).unwrap();
    early_state.insert(first, state);
    assert!(check_order(&early_state).unwrap_err().starts_with("WS2"));

    // 2. WS3 on an update's: the intent after the first write of the tree; its removal
    //    before the state.
    let update = sweep::reference(Scenario::Update).events;
    assert_eq!(check_order(&update), Ok(()));
    let late_intent = reordered(&update, intent_write, tree_write);
    assert!(check_order(&late_intent).unwrap_err().starts_with("WS3"));
    let early_removal = reordered(&update, intent_removal, |e| matches!(e, Event::Host(..)));
    assert!(check_order(&early_removal).unwrap_err().starts_with("WS3"));
}
