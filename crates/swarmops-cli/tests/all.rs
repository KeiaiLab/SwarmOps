//! The CLI's integration tests, in one test binary (one link): each file of `tests/` is a module
//! here, and its tests are named after it (`multiprocess::<test>`); `support` binds the binaries
//! under test. `crash.rs` and `faults.rs` drive the test hooks, so they are here only with the
//! `test-hooks` feature, which the workspace's gates turn on (`--all-features`). The release
//! proof, `test_hooks.rs`, is a binary of its own, built without the feature. Cargo builds no
//! other test target from `tests/` (`autotests = false` in `Cargo.toml`): a new test file is a
//! `mod` line here.

mod support;

mod cli;
mod contention;
#[cfg(feature = "test-hooks")]
mod crash;
#[cfg(feature = "test-hooks")]
mod faults;
mod mcp;
mod multiprocess;
mod power_loss;
mod so;

/// Every test file is a module here: with `autotests = false` Cargo builds no target from a file
/// of `tests/` that no `mod` line names, and its tests would never run (jj's
/// `no_forgotten_test_files`). Directories (`support/`, `snapshots/`, …) are passed over.
#[test]
fn every_test_file_is_a_module_of_all_rs() {
    // `all.rs` itself, and the release proof, a `[[test]]` target of its own.
    let own_targets = ["all", "test_hooks"];
    let declared: Vec<&str> = include_str!("all.rs")
        .lines()
        .filter_map(|line| line.strip_prefix("mod ")?.strip_suffix(';'))
        .collect();
    let tests = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    for entry in std::fs::read_dir(tests).unwrap() {
        let name = entry.unwrap().file_name().into_string().unwrap();
        let Some(module) = name.strip_suffix(".rs") else {
            continue;
        };
        assert!(
            own_targets.contains(&module) || declared.contains(&module),
            "no `mod` line of tests/all.rs names tests/{name}, so Cargo never builds it"
        );
    }
}
