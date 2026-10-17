//! What the sweeps through the binary share: `crash.rs` crashes a command at each of its
//! filesystem calls in turn (`SWARMOPS_TEST_CRASH_AT`), and `faults.rs` fails each one
//! (`SWARMOPS_TEST_FAIL_AT`).
//!
//! - A dry run counts the command's calls first (`SWARMOPS_TEST_FS_CALLS`: [`dry_commit`],
//!   [`dry_init`]), and [`sweep`] then runs the command once for each call, [`AT_ONCE`] at a
//!   time, each run on a store of its own. A commit signs at [`NOW`] (`SWARMOPS_TEST_NOW`) in
//!   every run, so that it writes the same objects, and call k is the same call in every run.
//! - The commit sweeps run [`swept_commit`] on copies of [`commit_template`]. Its frame is the
//!   32nd past the checkpoint, so its transaction also syncs the index and moves the checkpoint,
//!   and the sweeps reach those calls too. [`dry_commit`] asserts that it does: a change to the
//!   checkpoint's batching or to the template fails both sweeps at their dry runs, rather than
//!   taking those calls out of them unseen.

use std::fs;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::thread;

use swarmops_ops::NOW_ENV;
use swarmops_store::FS_CALLS_ENV;

use super::{Run, Store, agent, run};

/// How many runs of a sweep go at once, each with a store of its own.
pub const AT_ONCE: usize = 8;

/// The time the sweeps sign at: November 2023, in Unix seconds.
pub const NOW: u64 = 1_700_000_000;

/// How many frames lie past the checkpoint in [`commit_template`]: the frame of
/// [`swept_commit`] is the 32nd, so its transaction also syncs the index and moves the
/// checkpoint, and the commit sweeps reach those calls too ([`dry_commit`] asserts it).
pub const TEMPLATE_FRAMES: u64 = 31;

/// What [`swept_commit`] puts: three files in new nested directories on main, so that it reads
/// main's tree and writes blobs, trees and its changeset.
pub const SWEPT_PUTS: [(&str, &str); 3] = [
    ("src/lib.rs", "pub fn f() {}\n"),
    ("src/bin/tool.rs", "fn main() {}\n"),
    ("docs/notes.md", "# notes\n"),
];

/// Runs `run(k)` for every k of `ks`, [`AT_ONCE`] at a time, each on a thread of its own; the
/// outcomes, in the order of `ks`. A run that panics fails the sweep with its panic.
pub fn sweep<T: Send>(ks: RangeInclusive<u64>, run: impl Fn(u64) -> T + Sync) -> Vec<T> {
    let ks: Vec<u64> = ks.collect();
    let run = &run;
    let mut outcomes = Vec::with_capacity(ks.len());
    for batch in ks.chunks(AT_ONCE) {
        thread::scope(|scope| {
            let runs: Vec<_> = batch.iter().map(|&k| scope.spawn(move || run(k))).collect();
            for handle in runs {
                let outcome = handle
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
                outcomes.push(outcome);
            }
        });
    }
    outcomes
}

/// The number a dry run wrote to `file`: how many filesystem calls it made.
pub fn reported_calls(file: &Path) -> u64 {
    let text = fs::read_to_string(file).unwrap_or_else(|e| {
        panic!(
            "{}: the run reported no filesystem call count ({e})",
            file.display()
        )
    });
    text.trim()
        .parse()
        .unwrap_or_else(|e| panic!("{text:?} is not a call count: {e}"))
}

/// The file a run on `store` writes its call count to.
pub fn calls_file(store: &Store) -> PathBuf {
    store.tmp().join("fs-calls")
}

/// A store for the commit sweeps: main at a first changeset, then changesets on it, until
/// [`TEMPLATE_FRAMES`] frames lie past the checkpoint (which has passed none). Every command
/// on it, and on its copies, signs at [`NOW`], so the store and every commit on it are the same
/// in every run.
pub fn commit_template() -> Store {
    let store = Store::init().with_env(&[(NOW_ENV, &NOW.to_string())]);
    let first = store.commit(&agent(0), "README.md", "# a sweep's template\n", None);
    let create = store.run_as(&agent(0), &["ref", "set", "main", &first, "--create"], &[]);
    create.ok();
    for n in 2..TEMPLATE_FRAMES {
        store.commit(
            &agent(0),
            &format!("history/{n}.txt"),
            &format!("{n}\n"),
            None,
        );
    }
    assert_eq!(store.checkpoint(), 0);
    store
}

/// Runs the commit the commit sweeps crash or fail on `store`, as `agent(1)`, with `env` set:
/// [`SWEPT_PUTS`], on main.
pub fn swept_commit(store: &Store, env: &[(&str, &str)]) -> Run {
    let changes = store.changes_of("swept", &SWEPT_PUTS);
    store.run_as(
        &agent(1),
        &["commit", "-m", "swept", "--changes", &changes],
        env,
    )
}

/// What [`swept_commit`] does on a copy of [`commit_template`] when nothing crashes or fails it.
#[derive(Debug, PartialEq, Eq)]
pub struct DryRun {
    /// How many filesystem calls it makes.
    pub calls: u64,
    /// The changeset it commits.
    pub changeset: String,
}

/// Runs [`swept_commit`] on a copy of `template` with nothing crashing or failing it, counting
/// its calls. It must make dozens of them, and move the checkpoint to the journal's end, so that
/// a sweep of its calls reaches the index sync and the checkpoint's move.
pub fn dry_commit(template: &Store) -> DryRun {
    let store = Store::copy_of(template);
    let file = calls_file(&store);
    let result = swept_commit(&store, &[(FS_CALLS_ENV, file.to_str().unwrap())]).ok();
    assert_eq!(
        store.checkpoint(),
        store.journal_len(),
        "the 32nd frame past the checkpoint moves it to the journal's end"
    );
    let dry = DryRun {
        calls: reported_calls(&file),
        changeset: result["changeset"].as_str().unwrap().to_owned(),
    };
    assert!(dry.calls > 30, "a commit makes dozens of calls: {dry:?}");
    dry
}

/// Runs `init` on a fresh path with nothing crashing or failing it; how many filesystem calls it
/// made: more than the 256 fan-out directories it makes alone.
pub fn dry_init() -> u64 {
    let store = Store::empty();
    let file = calls_file(&store);
    run(
        &["init", store.path()],
        &[(FS_CALLS_ENV, file.to_str().unwrap())],
    )
    .ok();
    let calls = reported_calls(&file);
    assert!(
        calls > 256,
        "init makes 256 fan-out directories alone: {calls}"
    );
    calls
}

/// What an `init` of `store` that a crash or an injected failure may have cut short left there,
/// the fault gone: whether `info` finds a store. With one, a second `init` says `E_EXISTS`;
/// without one, `info` says `E_NOT_A_REPO`, and a second `init` makes the store. Either way, the
/// store is then a fresh one, and verifies clean. `what` names the run in messages.
#[track_caller]
pub fn init_again(store: &Store, what: &str) -> bool {
    let info = store.read(&["info"]);
    let whole = info.exit() == 0;
    if whole {
        run(&["init", store.path()], &[]).err("E_EXISTS");
    } else {
        info.err("E_NOT_A_REPO");
        run(&["init", store.path()], &[]).ok();
    }
    let info = store.info();
    let fresh = (info["name"].as_str(), info["changesets"].as_u64());
    assert_eq!(fresh, (Some("store"), Some(0)), "{what}");
    store.assert_clean(what);
    whole
}
