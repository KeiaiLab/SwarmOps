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

/// The line `--version` prints: `swarmops <version>`, and `+<commit>` when the build set
/// `SWARMOPS_BUILD_COMMIT`, which this crate reads at compile time as the binaries do.
pub fn version_line() -> String {
    let version = env!("CARGO_PKG_VERSION");
    let Some(commit) = option_env!("SWARMOPS_BUILD_COMMIT") else {
        return format!("swarmops {version}\n");
    };
    format!("swarmops {version}+{commit}\n")
}

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
    use swarmops_store::StoreFormat;
    pub use swarmops_testkit::cli::sweep::{
        DryRun, NOW, SWEPT_PUTS, TEMPLATE_FRAMES, calls_file, dry_commit, init_again, init_args,
        reported_calls, sweep, swept_commit,
    };

    use super::{Store, Swarmops};

    /// A store of `format` for the commit sweeps
    /// (`swarmops_testkit::cli::sweep::commit_template`).
    pub fn commit_template(format: StoreFormat) -> Store {
        swarmops_testkit::cli::sweep::commit_template::<Swarmops>(format)
    }

    /// How many filesystem calls `init` of a store of `format` makes on a fresh path
    /// (`swarmops_testkit::cli::sweep::dry_init`).
    pub fn dry_init(format: StoreFormat) -> u64 {
        swarmops_testkit::cli::sweep::dry_init::<Swarmops>(format)
    }
}

/// Lowercase hex digits in an operation id's checksum: `<op>:<crc>`.
const CRC_DIGITS: usize = 8;

/// `text` with the checksum of each operation id (`4:9f3a01c2`) replaced by `[crc]`
/// (`4:[crc]`): the checksum covers the operation's times and ids, which change from run to run.
pub fn redact_crcs(text: &str) -> String {
    let bytes = text.as_bytes();
    let bounded = |b: Option<&u8>| !b.is_some_and(u8::is_ascii_alphanumeric);
    let mut out = String::with_capacity(text.len());
    let mut done = 0;
    for (colon, _) in text.match_indices(':') {
        // `<digits>:<8 hex digits>`, neither end inside a longer word.
        let digits = bytes[..colon]
            .iter()
            .rev()
            .take_while(|b| b.is_ascii_digit())
            .count();
        let crc = &bytes[colon + 1..];
        let hex = crc
            .iter()
            .take_while(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
            .count();
        let before = (colon - digits).checked_sub(1).map(|at| &bytes[at]);
        if digits == 0 || hex != CRC_DIGITS || !bounded(before) || !bounded(crc.get(hex)) {
            continue;
        }
        out.push_str(&text[done..=colon]);
        out.push_str("[crc]");
        done = colon + 1 + hex;
    }
    out.push_str(&text[done..]);
    out
}
