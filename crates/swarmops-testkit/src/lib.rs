//! What SwarmOps' tests share: the doubles of the filesystem the stores write through, the
//! power-loss images, the child-process runner, and the builders of fresh stores, repositories
//! and runs of the binary. A dev-dependency of `swarmops-store`, `swarmops-repo`,
//! `swarmops-ops` and `swarmops-cli`, for their integration tests (and the operations' unit
//! tests), never a dependency of a product crate.
//!
//! - [`differential`]: one scripted git session against `swarmops serve` and against git's own
//!   server, compared step by step apart from the listed differences (the M17 design §2).
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
//! - [`formats`]: a test run once per store format ([`per_format!`]).
//! - [`store`], [`repo`]: fresh store roots, repositories, and the changes and commit requests
//!   the tests make.
//! - [`cli`]: runs of the real `swarmops` binary, their envelopes checked against the contract,
//!   racers behind a barrier, a store with the commands the tests run on it, and the crash and
//!   error sweeps' template ([`cli::sweep`]).
//! - [`measure`]: what the store's benches and measurement tests share.
//! - [`openmetrics`]: a reader of the OpenMetrics text format, for the text `health` prints.
//! - [`git`]: the git fixtures: repositories real git made from the scripts in
//!   `git-fixtures/scripts/`, committed as archives with their manifests, extracted for a test
//!   ([`git::fixture`]) and rebuilt only on purpose (`just git-fixtures`).
//! - [`landing`]: what the landing measurements share: workloads, the record of a land, the
//!   invariants every run keeps, and a run's statistics.
//! - [`remote`]: the git remotes' test double (M13), git's own server on loopback and through
//!   an ssh stand-in.
//! - [`serve`]: `swarmops serve`'s test site (M14), a root of stores served through an ssh
//!   forced-command stand-in and as a CGI script behind [`remote`]'s loopback server.
//! - [`accounting`]: what a served connection cost the server, CPU and peak memory, read off the
//!   ssh stand-in (M17).
//! - [`served_load`]: git clients that push their own branch and fetch the trunk on a schedule,
//!   each command's outcome read off git's output, for served traffic under load (M17).
//! - [`sync_load`]: landers through the binary beside a syncer, the remote's lag read once a
//!   second, and the lag's last-minute over first-minute ratio (the M18 design §2.4).
//! - [`ws`]: the workspaces' synthetic trees and edit driver (M5).
//! - [`schema_check`]: a JSON value checked against its type's JSON Schema, the subset
//!   `swarmops_api::schema_for_all` emits (M21).
//! - [`bulk`]: the scale fixtures, stores of 1,000 and 1,000,000 changesets built once, in parts,
//!   and the gate (`SWARMOPS_SCALE=1`) of the cases that use them (M21).
//!
//! It depends on no crate above `swarmops-repo`: the operations' unit tests use it, and a
//! dependency on `swarmops-ops` would build a second copy of the crate under test there. No
//! store unit test uses it, for the same reason. Its `test-hooks` feature (not a default: see
//! `Cargo.toml`) turns on the store's. Its default features, `cli`, `git` and `repo`, hold
//! [`cli`] (and its `serde_json`), [`git`] (and `tar`, `base64`, `serde` and `serde_json`) and
//! [`repo`] and [`landing`] (and `swarmops-repo`): without them, as the fuzz crate takes it, the
//! rest depends on nothing above `swarmops-store`.

// Test support: the guide lets test code panic on assertions, so the two lints `just check`
// turns on for library code stay off here. Its types are built by tests only, so they need not
// be `#[non_exhaustive]`.
#![allow(
    clippy::unwrap_used,
    clippy::missing_assert_message,
    clippy::exhaustive_enums,
    clippy::exhaustive_structs
)]

#[cfg(feature = "cli")]
pub mod accounting;
#[cfg(feature = "repo")]
pub mod bulk;
pub mod child;
#[cfg(feature = "cli")]
pub mod cli;
#[cfg(feature = "cli")]
pub mod differential;
pub mod faults;
pub mod formats;
pub mod fs;
#[cfg(feature = "git")]
pub mod git;
#[cfg(feature = "repo")]
pub mod landing;
pub mod measure;
pub mod openmetrics;
pub mod power;
pub mod remote;
#[cfg(feature = "repo")]
pub mod repo;
#[cfg(feature = "cli")]
pub mod schema_check;
#[cfg(feature = "cli")]
pub mod serve;
#[cfg(all(feature = "cli", feature = "repo"))]
pub mod served_load;
pub mod store;
#[cfg(all(feature = "cli", feature = "repo"))]
pub mod sync_load;
pub mod ws;
