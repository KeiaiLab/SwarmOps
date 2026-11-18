//! The binaries under test, bound for the test kit's runner (`swarmops_testkit::cli`): Cargo
//! tells only the CLI's own tests where they are (`CARGO_BIN_EXE_swarmops`, `CARGO_BIN_EXE_so`),
//! so this module binds them, and gives the runner's items the names the tests use. A module
//! directory, so that Cargo does not take it for a test target of its own.

#![allow(dead_code, unused_imports)] // Each test binary uses the part it needs.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use swarmops_testkit::cli::{self, Program};
pub use swarmops_testkit::cli::{
    RUN_TIMEOUT, Run, agent, isolate, isolation, retry_busy, run_bin, run_bin_within, swarmops_vars,
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

/// Makes the directory `path`, whose name is not UTF-8, for a test that runs [`BIN`] in it.
/// False on macOS when the volume refuses the name (`EILSEQ`, as APFS does): on such a volume
/// the test has no working directory to run in, and says on stderr that it was skipped. Any
/// other failure, and every failure on another OS, fails the test.
pub fn made_non_utf8_dir(path: &Path) -> bool {
    /// macOS's `EILSEQ`, "Illegal byte sequence".
    #[cfg(target_os = "macos")]
    const EILSEQ: i32 = 92;

    let made = std::fs::create_dir(path);
    #[cfg(target_os = "macos")]
    if matches!(&made, Err(e) if e.raw_os_error() == Some(EILSEQ)) {
        eprintln!("skipped: this volume refuses a name that is not UTF-8 (EILSEQ)");
        return false;
    }
    made.expect("make a directory whose name is not UTF-8");
    true
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

/// What the queue's crash, power-loss and SIGKILL tests share (the M22 core design §5, C8): a
/// store whose `main` names a check, and changes queued on it while no runner can take them.
pub mod queue {
    use serde_json::{Value, json};
    use swarmops_store::LaneLock;

    use super::{Store, agent};

    /// The target every queue test lands on.
    pub const TARGET: &str = "main";

    /// A store whose `main` holds only `.swarmops/workspaces.json`, naming `sh -c <check>` as
    /// the repository's check: `main`'s head.
    pub fn checked(store: &Store, check: &str) -> String {
        let config = json!({"check": {"argv": ["sh", "-c", check], "timeout_s": 600}});
        let base = store.commit(
            &agent(0),
            ".swarmops/workspaces.json",
            &config.to_string(),
            None,
        );
        let create = ["ref", "set", TARGET, base.as_str(), "--create"];
        store.run_as(&agent(0), &create, &[]).ok();
        base
    }

    /// `n` changes on `base`, each adding `change-<i>.txt`, enqueued onto [`TARGET`] in order
    /// while this process holds the target's lane, so that no enqueue starts a runner: their
    /// entry ids.
    pub fn enqueued(store: &Store, base: &str, n: usize) -> Vec<String> {
        // 1. The lane, held until every entry is durable.
        let lane = LaneLock::try_acquire(&store.layout(), TARGET).expect("take the lane");
        assert!(lane.is_some(), "no runner holds a fresh store's lane");

        // 2. Each change, then its entry.
        let mut entries = Vec::with_capacity(n);
        for i in 0..n {
            let path = format!("change-{i}.txt");
            let head = store.commit(&agent(1), &path, &format!("{i}\n"), Some(base));
            let args = ["queue", "enqueue", TARGET, "--head", head.as_str()];
            let queued = store.run_as(&agent(1), &args, &[]).ok();
            entries.push(queued["entry"]["id"].as_str().expect("an id").to_owned());
        }
        entries
    }

    /// Blocks until no runner holds [`TARGET`]'s lane, then frees it again: a runner a queue
    /// command started has ended.
    pub fn runner_gone(store: &Store) {
        let deadline = std::time::Instant::now() + super::RUN_TIMEOUT;
        loop {
            let lane = LaneLock::try_acquire(&store.layout(), TARGET).expect("try the lane");
            if lane.is_some() {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "a runner never ended");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// `queue status <TARGET>`'s entries.
    pub fn listed(store: &Store) -> Vec<Value> {
        let status = store.read(&["queue", "status", TARGET]).ok();
        status["entries"].as_array().expect("entries").clone()
    }

    /// Asserts that every entry of `entries` landed once: each listed as landed, the landed
    /// changesets distinct, and `main`'s history `base` and one changeset per entry. `what`
    /// says when.
    #[track_caller]
    pub fn landed_once(store: &Store, base: &str, entries: &[String], what: &str) {
        // 1. Each entry landed, on a changeset of its own.
        let listed = listed(store);
        let mut landed = Vec::new();
        for entry in entries {
            let found = listed.iter().find(|e| e["id"] == entry.as_str());
            let found = found.unwrap_or_else(|| panic!("{what}: {entry} not queued: {listed:?}"));
            assert_eq!(found["state"], "landed", "{what}: {found}");
            landed.push(found["landed"].as_str().expect("a landed id").to_owned());
        }
        let mut distinct = landed.clone();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(distinct.len(), landed.len(), "{what}: {landed:?}");

        // 2. Main: the base and one changeset per entry, none twice.
        let log = store.read(&["log", TARGET]).ok();
        let ids: Vec<&str> = (log["entries"].as_array().expect("entries").iter())
            .map(|e| e["id"].as_str().expect("an id"))
            .collect();
        assert_eq!(ids.len(), entries.len() + 1, "{what}: {ids:?}");
        assert_eq!(ids.last(), Some(&base), "{what}: {ids:?}");
        for id in &landed {
            assert!(
                ids.contains(&id.as_str()),
                "{what}: {id} not on main: {ids:?}"
            );
        }
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

/// [`redact_crcs`] rewrites a whole operation id alone: not a shorter or longer hex run, one
/// followed by a letter, an uppercase one, one with no operation number, or one inside a
/// longer word.
#[test]
fn redact_crcs_takes_whole_ids_only() {
    let cases = [
        ("op 4:9f3a01c2 done", "op 4:[crc] done"),
        ("1:00000000,22:abcdef01", "1:[crc],22:[crc]"),
        ("4:9f3a", "4:9f3a"),
        ("4:9f3a01c2a", "4:9f3a01c2a"),
        ("4:9f3a01c2z", "4:9f3a01c2z"),
        ("4:9F3A01C2", "4:9F3A01C2"),
        (":9f3a01c2", ":9f3a01c2"),
        ("x4:9f3a01c2", "x4:9f3a01c2"),
    ];
    for (text, redacted) in cases {
        assert_eq!(redact_crcs(text), redacted, "{text}");
    }
}
