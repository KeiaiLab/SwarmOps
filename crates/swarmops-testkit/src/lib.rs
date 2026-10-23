//! What SwarmOps' tests share: the doubles of the filesystem the stores write through, the
//! power-loss images, the child-process runner, and the builders of fresh stores, repositories
//! and runs of the binary. A dev-dependency of `swarmops-store`, `swarmops-repo`,
//! `swarmops-ops` and `swarmops-cli`, for their integration tests (and the operations' unit
//! tests), never a dependency of a product crate.
//!
//! - [`fs`]: [`FsOps`](swarmops_store::FsOps) doubles on real files, to record, fail or hold
//!   calls over any filesystem ([`fs::Watched`], [`fs::RecordingFs`], [`fs::FailingFs`]), skip
//!   every flush while failing where the real filesystem fails ([`fs::NoSyncFs`]), or tell calls
//!   made at once from calls made one after another ([`fs::Rendezvous`]); each names a call's
//!   method with [`FsMethod`](swarmops_store::FsMethod).
//! - [`faults`]: Linux's fsync-failure semantics, modelled ([`faults::FsyncModelFs`]), and a
//!   filesystem under the store's order checker ([`faults::checked`]).
//! - [`power`]: what a power loss can leave of a recorded run ([`power::ImageRecordingFs`],
//!   [`power::Timeline`], [`power::Image`]).
//! - [`child`]: this test binary run again as a child process, with a deadline, killed and
//!   reaped however the test ends, by the name the harness knows the test by
//!   ([`test_name!`]).
//! - [`store`], [`repo`]: fresh store roots, repositories, and the changes and commit requests
//!   the tests make.
//! - [`cli`]: runs of the real `swarmops` binary, their envelopes checked against the contract,
//!   racers behind a barrier, a store with the commands the tests run on it, and the crash and
//!   error sweeps' template ([`cli::sweep`]).
//! - [`measure`]: what the store's benches and measurement tests share.
//! - [`git`]: the git fixtures: repositories real git made from the scripts in
//!   `git-fixtures/scripts/`, committed as archives with their manifests, extracted for a test
//!   ([`git::fixture`]) and rebuilt only on purpose (`just git-fixtures`).
//! - [`landing`]: what the landing measurements share: workloads, the record of a land, the
//!   invariants every run keeps, and a run's statistics.
//! - [`remote`]: the git remotes' test double (M13), git's own server on loopback and through
//!   an ssh stand-in.
//! - [`ws`]: the workspaces' synthetic trees and edit driver (M5).
//!
//! It depends on no crate above `swarmops-repo`: the operations' unit tests use it, and a
//! dependency on `swarmops-ops` would build a second copy of the crate under test there. No
//! store unit test uses it, for the same reason. Its `test-hooks` feature (not a default: see
//! `Cargo.toml`) turns on the store's. Its default features, `cli`, `git` and `repo`, hold
//! [`cli`] (and its `serde_json`), [`git`] (and `tar`, `base64`, `serde` and `serde_json`) and
//! [`repo`] and [`landing`] (and `swarmops-repo`): without them, as the fuzz crate takes it, the
//! rest depends on nothing above `swarmops-store`.

// Test support: the guide lets test code panic on assertions, so the two lints `just check`
// turns on for library code stay off here.
#![allow(clippy::unwrap_used, clippy::missing_assert_message)]

pub mod child;
#[cfg(feature = "cli")]
pub mod cli;
pub mod faults;
pub mod fs;
#[cfg(feature = "git")]
pub mod git;
#[cfg(feature = "repo")]
pub mod landing;
pub mod measure;
pub mod power;
pub mod remote;
#[cfg(feature = "repo")]
pub mod repo;
pub mod store;
pub mod ws;
