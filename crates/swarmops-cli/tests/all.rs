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
