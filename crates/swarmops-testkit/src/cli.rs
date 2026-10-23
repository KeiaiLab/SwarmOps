//! Running the real `swarmops` binary from the multi-process and crash tests: one run to its end,
//! children that race behind a start barrier, and a store in a temp dir of its own with the
//! commands the tests run on it.
//!
//! Only the CLI's own tests can name the binary: Cargo tells them alone where it is
//! (`CARGO_BIN_EXE_swarmops`, and `CARGO_BIN_EXE_so` for its short name). They bind it to a
//! [`Program`] of their own, and run [`Store`], [`Racers`] and the functions here with it.
//!
//! Every child runs with none of the developer's `SWARMOPS_*` variables, only those the test
//! sets, and `SWARMOPS_TEST_ORDER_CHECK=1` (unless the test sets it otherwise): in a build with
//! the `test-hooks` feature, as the gates build it, every write a child makes is checked against
//! the store's write-ordering rules, and a call that breaks one panics, which the child reports
//! as `E_INTERNAL`. Its envelope is checked against the contract before a test reads it: stdout
//! holds one JSON line, `ok` holds exactly when the exit code is 0, and an error's `exit` is the
//! exit code. Every child has a deadline ([`RUN_TIMEOUT`] unless a test gives another): one still
//! running then is killed and fails the test with what it printed. A child is reaped however
//! the test ends: its handle ([`crate::child::Child`]) kills and waits for it when it is dropped,
//! a panic's unwinding included. A setup command that meets `E_BUSY` may run again, as an agent's
//! would ([`retry_busy`]). What the crash and error sweeps share (their runner, dry runs and
//! commit template) is in [`sweep`].

pub mod sweep;

mod binary;

pub use binary::{
    BUSY_RETRIES, Program, RUN_TIMEOUT, Racers, Run, Store, agent, retry_busy, run, run_bin,
    run_bin_within, run_within, swarmops, swarmops_vars,
};
