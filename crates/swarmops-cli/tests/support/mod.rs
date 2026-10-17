//! The binaries under test, bound for the test kit's runner (`swarmops_testkit::cli`): Cargo
//! tells only the CLI's own tests where they are (`CARGO_BIN_EXE_swarmops`, `CARGO_BIN_EXE_so`),
//! so this module binds them, and gives the runner's items the names the tests use. A module
//! directory, so that Cargo does not take it for a test target of its own.

#![allow(dead_code, unused_imports)] // Each test binary uses the part it needs.

use std::process::Command;
use std::time::Duration;

use swarmops_testkit::cli::{self, Program};
pub use swarmops_testkit::cli::{
    RUN_TIMEOUT, Run, agent, retry_busy, run_bin, run_bin_within, swarmops_vars,
};

/// The binary under test.
pub const BIN: &str = env!("CARGO_BIN_EXE_swarmops");

/// The binary under test by its short name, `so`: the same program.
pub const SO: &str = env!("CARGO_BIN_EXE_so");

/// The program the test kit runs: [`BIN`], whose commits sign at `SWARMOPS_TEST_NOW`'s time in a
/// build with the test hooks.
pub enum Swarmops {}

impl Program for Swarmops {
    const BIN: &'static str = BIN;
    const NOW_ENV: &'static str = swarmops_ops::NOW_ENV;
}

/// A store in a temp dir of its own, and the commands the tests run on it, through [`BIN`].
pub type Store = cli::Store<Swarmops>;

/// Children of [`BIN`] that start together.
pub type Racers = cli::Racers<Swarmops>;

/// [`BIN`] with `args`, the order check on, `env` set, and none of the other `SWARMOPS_*`
/// variables of this process's environment.
pub fn swarmops<S: AsRef<str>>(args: &[S], env: &[(&str, &str)]) -> Command {
    cli::swarmops::<Swarmops, S>(args, env)
}

/// Runs [`BIN`] with `args` and `env` to its end, within [`RUN_TIMEOUT`].
pub fn run<S: AsRef<str>>(args: &[S], env: &[(&str, &str)]) -> Run {
    cli::run::<Swarmops, S>(args, env)
}

/// Runs [`BIN`] with `args` and `env` to its end; one still running after `timeout` is killed
/// and fails the test.
pub fn run_within<S: AsRef<str>>(args: &[S], env: &[(&str, &str)], timeout: Duration) -> Run {
    cli::run_within::<Swarmops, S>(args, env, timeout)
}

/// What the crash and error sweeps share (`swarmops_testkit::cli::sweep`), through [`BIN`].
pub mod sweep {
    pub use swarmops_testkit::cli::sweep::{
        DryRun, NOW, SWEPT_PUTS, TEMPLATE_FRAMES, calls_file, dry_commit, init_again,
        reported_calls, sweep, swept_commit,
    };

    use super::{Store, Swarmops};

    /// A store for the commit sweeps (`swarmops_testkit::cli::sweep::commit_template`).
    pub fn commit_template() -> Store {
        swarmops_testkit::cli::sweep::commit_template::<Swarmops>()
    }

    /// How many filesystem calls `init` makes on a fresh path
    /// (`swarmops_testkit::cli::sweep::dry_init`).
    pub fn dry_init() -> u64 {
        swarmops_testkit::cli::sweep::dry_init::<Swarmops>()
    }
}
