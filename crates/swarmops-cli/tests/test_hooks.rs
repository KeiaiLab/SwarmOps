//! The test hooks act only in a `swarmops` built with the `test-hooks` feature (storage spec
//! §8): `SWARMOPS_TEST_CRASH_AT=<k>` aborts it at its k-th filesystem call,
//! `SWARMOPS_TEST_FS_CALLS=<file>` has it write how many calls it made to `<file>`, and
//! `SWARMOPS_TEST_NOW=<unix seconds>` fixes the time it signs changesets at. Any other build,
//! `cargo build --release` among them, ignores all three.
//!
//! The test checks the build it is part of: the workspace's test run turns the feature on
//! (`--all-features`) and finds the hooks acting; `just check` runs it once more in a release
//! build, without the feature, and finds them ignored.

mod support;

use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

use support::{Store, agent, run};
use swarmops_ops::NOW_ENV;
use swarmops_store::{CRASH_AT_ENV, FS_CALLS_ENV};

/// Whether the `swarmops` under test honors the hooks: it is built with this test's features.
const HOOKS: bool = cfg!(feature = "test-hooks");

/// The time the commit asks to be signed at: September 2001, in Unix seconds.
const NOW: u64 = 1_000_000_000;

#[test]
fn the_test_hooks_act_only_in_a_build_with_the_feature() {
    // SWARMOPS_TEST_CRASH_AT=1: init's first filesystem call aborts it, before it makes anything.
    let store = Store::empty();
    let init = run(&["init", store.path()], &[(CRASH_AT_ENV, "1")]);
    if HOOKS {
        assert!(init.aborted(), "aborted at its first call: {init}");
        run(&["init", store.path()], &[]).ok();
    } else {
        assert_eq!(
            init.ok()["changesets"],
            0,
            "the crash hook is ignored: {init}"
        );
    }

    // SWARMOPS_TEST_FS_CALLS and SWARMOPS_TEST_NOW, on a commit.
    let calls = store.tmp().join("fs-calls");
    let now = NOW.to_string();
    let env = [
        (FS_CALLS_ENV, calls.to_str().unwrap()),
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
    let id = commit.ok()["changeset"].as_str().unwrap().to_owned();
    let secs = store.show(&id)["signatures"]["committer"]["secs"]
        .as_u64()
        .unwrap();
    if HOOKS {
        let counted = fs::read_to_string(&calls).unwrap();
        assert!(counted.parse::<u64>().unwrap() > 0, "{counted:?}");
        assert_eq!(secs, NOW, "signed at SWARMOPS_TEST_NOW's time");
    } else {
        assert!(
            !calls.exists(),
            "the call-count hook is ignored: nothing written"
        );
        assert!(
            secs >= before,
            "signed at the clock's time: {secs} < {before}"
        );
    }
    store.assert_clean("after the commit");
}
