//! What SwarmOps' tests share: the doubles of the filesystem the stores write through, the
//! power-loss images, the child-process runner, and the builders of fresh stores, repositories
//! and runs of the binary. A dev-dependency of `swarmops-store`, `swarmops-repo`,
//! `swarmops-ops` and `swarmops-cli`, for their integration tests (and the operations' unit
//! tests), never a dependency of a product crate.
//!
//! - [`fs`]: [`FsOps`](swarmops_store::FsOps) doubles on real files, to record, fail or hold
//!   calls ([`fs::Watched`], [`fs::RecordingFs`], [`fs::FailingFs`]), skip every flush
//!   ([`fs::FastFs`]), or tell calls made at once from calls made one after another
//!   ([`fs::Rendezvous`]).
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
//!
//! It depends on no crate above `swarmops-repo`: the operations' unit tests use it, and a
//! dependency on `swarmops-ops` would build a second copy of the crate under test there. No
//! store unit test uses it, for the same reason. Its `test-hooks` feature (not a default: see
//! `Cargo.toml`) turns on the store's.

pub mod child;
pub mod cli;
pub mod faults;
pub mod fs;
pub mod measure;
pub mod power;
pub mod repo;
pub mod store;
