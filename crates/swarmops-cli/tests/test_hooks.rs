//! A `swarmops` built without the `test-hooks` feature ignores the test hooks (storage spec §8):
//! `SWARMOPS_TEST_CRASH_AT=<k>`, which aborts a build with the feature at its k-th filesystem
//! call, `SWARMOPS_TEST_FS_CALLS=<file>`, which has it write how many calls it made,
//! `SWARMOPS_TEST_FAIL_AT=<k>[:<mode>[:<errno>]]`, which fails its k-th call (and with
//! `persistent` every mutating call after it), `SWARMOPS_TEST_ORDER_CHECK=1`, which checks the
//! order of its writes (the test support sets it for every run), and
//! `SWARMOPS_TEST_NOW=<unix seconds>`, which fixes the time it signs changesets at.
//!
//! The test exists only in a build without the feature, as `cargo build --release` is. `just
//! check` and `scripts/check.sh` run it in a release build and require it to run
//! (`--no-tests=fail`), so a build that turns the feature on all the same (a stray default, say)
//! fails the gate rather than passing it with nothing tested. The crash tests (`crash.rs`) show
//! the hooks acting when the feature is on.

#![cfg(not(feature = "test-hooks"))]

mod support;

use std::time::{SystemTime, UNIX_EPOCH};

use support::{Store, agent, run};
use swarmops_ops::NOW_ENV;
use swarmops_store::{CRASH_AT_ENV, FAIL_AT_ENV, FS_CALLS_ENV};

/// The time the commit asks to be signed at: September 2001, in Unix seconds.
const NOW: u64 = 1_000_000_000;

#[test]
fn a_build_without_the_feature_ignores_the_test_hooks() {
    // SWARMOPS_TEST_CRASH_AT=1 would abort init at its first filesystem call.
    let store = Store::empty();
    let init = run(&["init", store.path()], &[(CRASH_AT_ENV, "1")]);
    assert_eq!(
        init.ok()["changesets"],
        0,
        "the crash hook is ignored: {init}"
    );

    // SWARMOPS_TEST_FS_CALLS, SWARMOPS_TEST_FAIL_AT and SWARMOPS_TEST_NOW, on a commit: the
    // failure hook would fail its first call and every mutating one after it.
    let calls = store.tmp().join("fs-calls");
    let now = NOW.to_string();
    let env = [
        (FS_CALLS_ENV, calls.to_str().unwrap()),
        (FAIL_AT_ENV, "1:persistent"),
        (NOW_ENV, now.as_str()),
    ];
    let changes = store.changes("hooks", "hooks.txt", "the test hooks\n");
    let before = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let commit = store.run_as(
        &agent(0),
        &["commit", "-m", "hooks", "--changes", &changes],
        &env,
    );
    let id = commit.ok()["changeset"].as_str().unwrap().to_owned(); // the failure hook is ignored
    let secs = store.show(&id)["signatures"]["committer"]["secs"]
        .as_u64()
        .unwrap();
    assert!(
        !calls.exists(),
        "the call-count hook is ignored: nothing written"
    );
    assert!(
        secs >= before,
        "signed at the clock's time, not SWARMOPS_TEST_NOW's: {secs} < {before}"
    );
    store.assert_clean("after the commit");
}
