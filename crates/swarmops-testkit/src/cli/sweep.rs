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
//!
//! - A sweep holds only if call k is the same call in every run. A run with
//!   `SWARMOPS_TEST_FS_TRACE` set lists its calls, and two lists show where two runs part:
//!   [`same_count`] for a dry run done twice, [`diverged`] for a dry run and a cut one.
//!
//! Each of them runs the [`Program`] its store's commands run.

use std::fs;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::thread;

use swarmops_store::{FS_CALLS_ENV, FS_TRACE_ENV, StoreFormat};

use super::{Program, Run, Store};

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

/// The lines of the trace a run wrote to `file` (`SWARMOPS_TEST_FS_TRACE`), one for each
/// filesystem call it counted; `None` if there is no such file.
fn trace_lines(file: &Path) -> Option<Vec<String>> {
    let text = fs::read_to_string(file).ok()?;
    Some(text.lines().map(str::to_owned).collect())
}

/// The first of lines 1 to `upto` that is not the same in `a` and in `b`: its number, then the
/// line of each, empty where the trace has ended.
fn first_difference(a: &[String], b: &[String], upto: u64) -> Option<(u64, String, String)> {
    let line = |trace: &[String], n: u64| {
        let at = usize::try_from(n - 1).ok()?;
        trace.get(at).cloned()
    };
    (1..=upto).find_map(|n| {
        let (in_a, in_b) = (line(a, n), line(b, n));
        (in_a != in_b).then(|| (n, in_a.unwrap_or_default(), in_b.unwrap_or_default()))
    })
}

/// Where a run cut at call `k` (crashed or failed there) parted from the dry run before it, by
/// their traces (`SWARMOPS_TEST_FS_TRACE`, set in both runs): the first of lines 1 to `k` that
/// differs, as its number, the dry run's line and the cut run's, the latter empty if the cut
/// run made fewer calls. `None` if the cut run made the dry run's first `k` calls: call `k` was
/// then the call the sweep meant. A sweep says this where a cut run did not end as it must.
///
/// # Panics
///
/// If the dry run left no trace: the test did not set the variable.
pub fn diverged(dry: &Path, cut: &Path, k: u64) -> Option<(u64, String, String)> {
    let Some(dry_lines) = trace_lines(dry) else {
        panic!(
            "{}: the dry run left no trace: set {FS_TRACE_ENV}",
            dry.display()
        )
    };
    let cut_lines = trace_lines(cut).unwrap_or_default();
    first_difference(&dry_lines, &cut_lines, k)
}

/// Checks that a dry run done twice made the same number of filesystem calls, `a` then `b`:
/// if it did not, call k is not the same call in every run, and a sweep of the calls proves
/// nothing. `dry_a` and `dry_b` are the traces of the two runs (`SWARMOPS_TEST_FS_TRACE`).
///
/// # Panics
///
/// If the counts differ, with the first line at which the traces do.
#[track_caller]
pub fn same_count(a: u64, b: u64, dry_a: &Path, dry_b: &Path) {
    if a == b {
        return;
    }
    let lines_a = trace_lines(dry_a).unwrap_or_default();
    let lines_b = trace_lines(dry_b).unwrap_or_default();
    let longest = u64::try_from(lines_a.len().max(lines_b.len())).unwrap_or(u64::MAX);
    let Some((n, in_a, in_b)) = first_difference(&lines_a, &lines_b, longest) else {
        panic!(
            "a dry run made {a} filesystem calls, and {b} when done again: call k is not the \
             same call in every run. The traces do not differ ({} lines each, of {} and {}): \
             set {FS_TRACE_ENV} in both runs",
            lines_a.len(),
            dry_a.display(),
            dry_b.display()
        )
    };
    panic!(
        "a dry run made {a} filesystem calls, and {b} when done again: call k is not the same \
         call in every run. The traces first differ at line {n}:\n  first:  {in_a:?}\n  again:  \
         {in_b:?}"
    );
}

/// The file a run on `store` writes its call count to.
pub fn calls_file<P: Program>(store: &Store<P>) -> PathBuf {
    store.tmp().join("fs-calls")
}

/// A store of `format` for the commit sweeps: main at a first changeset, then changesets on
/// it, until [`TEMPLATE_FRAMES`] frames lie past the checkpoint (which has passed none). Every
/// command on it, and on its copies, signs at [`NOW`] ([`Program::NOW_ENV`]), so the store and
/// every commit on it are the same in every run.
pub fn commit_template<P: Program>(format: StoreFormat) -> Store<P> {
    let store = Store::<P>::init_as(format).with_env(&[(P::NOW_ENV, &NOW.to_string())]);
    let first = store.commit(
        &super::agent(0),
        "README.md",
        "# a sweep's template\n",
        None,
    );
    let create = store.run_as(
        &super::agent(0),
        &["ref", "set", "main", &first, "--create"],
        &[],
    );
    create.ok();
    for n in 2..TEMPLATE_FRAMES {
        store.commit(
            &super::agent(0),
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
pub fn swept_commit<P: Program>(store: &Store<P>, env: &[(&str, &str)]) -> Run {
    let changes = store.changes_of("swept", &SWEPT_PUTS);
    store.run_as(
        &super::agent(1),
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
pub fn dry_commit<P: Program>(template: &Store<P>) -> DryRun {
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

/// The arguments of an `init` of a store of `format` at `path`.
pub fn init_args(format: StoreFormat, path: &str) -> Vec<String> {
    let version = format.version().to_string();
    ["init", "--format", &version, path]
        .map(str::to_owned)
        .to_vec()
}

/// Runs `init` of a store of `format` on a fresh path with nothing crashing or failing it; how
/// many filesystem calls it made: more than the 256 fan-out directories it makes alone.
pub fn dry_init<P: Program>(format: StoreFormat) -> u64 {
    let store = Store::<P>::empty();
    let file = calls_file(&store);
    super::run::<P, _>(
        &init_args(format, store.path()),
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

/// What an `init` of a store of `format` at `store` that a crash or an injected failure may have
/// cut short left there, the fault gone: whether `info` finds a store. With one, a second `init`
/// says `E_EXISTS`; without one, `info` says `E_NOT_A_REPO`, and a second `init` makes the
/// store. Either way, the store is then a fresh one of `format`, and verifies clean. `what`
/// names the run in messages.
#[track_caller]
pub fn init_again<P: Program>(store: &Store<P>, format: StoreFormat, what: &str) -> bool {
    let info = store.read(&["info"]);
    let whole = info.exit() == 0;
    let init = init_args(format, store.path());
    if whole {
        super::run::<P, _>(&init, &[]).err("E_EXISTS");
    } else {
        info.err("E_NOT_A_REPO");
        super::run::<P, _>(&init, &[]).ok();
    }
    let info = store.info();
    let fresh = (
        info["name"].as_str(),
        info["changesets"].as_u64(),
        info["store_version"].as_u64(),
    );
    let made = (Some("store"), Some(0), Some(u64::from(format.version())));
    assert_eq!(fresh, made, "{what}");
    store.assert_clean(what);
    whole
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::{diverged, same_count};

    /// A trace file in `dir` named `name`, holding `lines`.
    fn trace(dir: &Path, name: &str, lines: &[&str]) -> PathBuf {
        let file = dir.join(name);
        fs::write(&file, lines.join("\n") + "\n").unwrap();
        file
    }

    const DRY: [&str; 3] = [
        "1 read meta/journal.ckpt",
        "2 exists meta/refs/main",
        "3 append meta/journal.log",
    ];

    /// Two traces that differ at line 2: `diverged` reports line 2 and both lines once the cut
    /// reaches it, and nothing for a cut before it.
    #[test]
    fn diverged_reports_the_first_differing_line() {
        let dir = tempfile::tempdir().unwrap();
        let dry = trace(dir.path(), "dry", &DRY);
        let cut = trace(dir.path(), "cut", &[DRY[0], "2 file_len meta/journal.log"]);
        let second = (
            2,
            DRY[1].to_owned(),
            "2 file_len meta/journal.log".to_owned(),
        );
        assert_eq!(diverged(&dry, &cut, 1), None);
        assert_eq!(diverged(&dry, &cut, 2), Some(second.clone()));
        assert_eq!(diverged(&dry, &cut, 3), Some(second));

        // The same calls up to the cut: nothing, whatever follows.
        let same = trace(dir.path(), "same", &DRY[..2]);
        assert_eq!(diverged(&dry, &same, 2), None);

        // A cut run that ended early, or wrote no trace at all, differs where it ends.
        let ended = (3, DRY[2].to_owned(), String::new());
        assert_eq!(diverged(&dry, &same, 3), Some(ended));
        let none = (1, DRY[0].to_owned(), String::new());
        assert_eq!(diverged(&dry, &dir.path().join("missing"), 3), Some(none));
    }

    /// A dry run without a trace is a mistake in the test, not "no divergence".
    #[test]
    #[should_panic(expected = "the dry run left no trace: set SWARMOPS_TEST_FS_TRACE")]
    fn diverged_refuses_a_dry_run_without_a_trace() {
        let dir = tempfile::tempdir().unwrap();
        let cut = trace(dir.path(), "cut", &DRY);
        let _ = diverged(&dir.path().join("missing"), &cut, 3);
    }

    /// Equal counts pass, with no trace read at all.
    #[test]
    fn same_count_passes_equal_counts() {
        same_count(3, 3, Path::new("/no/such/trace"), Path::new("/nor/this"));
    }

    /// Counts that differ panic with both counts and the first line the traces differ at.
    #[test]
    #[should_panic(
        expected = "a dry run made 3 filesystem calls, and 4 when done again: call k is not the \
                    same call in every run. The traces first differ at line 2:\n  first:  \
                    \"2 exists meta/refs/main\"\n  again:  \"2 exists meta/features\""
    )]
    fn same_count_panics_with_the_first_differing_line() {
        let dir = tempfile::tempdir().unwrap();
        let first = trace(dir.path(), "first", &DRY);
        let again = [
            DRY[0],
            "2 exists meta/features",
            "3 exists meta/refs/main",
            "4 append meta/journal.log",
        ];
        let again = trace(dir.path(), "again", &again);
        same_count(3, 4, &first, &again);
    }

    /// Counts that differ with nothing traced still panic, and say what to set.
    #[test]
    #[should_panic(expected = "The traces do not differ (0 lines each")]
    fn same_count_panics_without_traces_too() {
        same_count(3, 4, Path::new("/no/such/trace"), Path::new("/nor/this"));
    }

    /// The selftest of the sweeps' determinism check: a run with one more branch (a call the
    /// other run does not make), traced by a real `CrashAtFs`, is the line `diverged` names,
    /// and the count `same_count` refuses.
    #[cfg(feature = "test-hooks")]
    mod traced {
        use swarmops_store::{CrashAtFs, FsOps, RealFs};

        use super::*;

        /// Whether a run takes the branch that makes one more call.
        #[derive(Clone, Copy, PartialEq, Eq)]
        enum Branch {
            Plain,
            Extra,
        }

        /// A run of three calls, four with the extra branch, on a store of its own in `dir`,
        /// traced to the file it returns with its call count.
        fn run(dir: &Path, name: &str, branch: Branch) -> (u64, PathBuf) {
            let root = dir.join(name).join("store");
            let file = dir.join(format!("{name}.trace"));
            let fs = CrashAtFs::new(RealFs::new(), 0).tracing_calls_to(&file);
            fs.create_dir_all(&root.join("meta")).unwrap();
            if branch == Branch::Extra {
                fs.exists(&root.join("meta/features")).unwrap();
            }
            fs.append(&root.join("meta/journal.log"), b"frame").unwrap();
            fs.sync_data(&root.join("meta/journal.log")).unwrap();
            (fs.calls(), file)
        }

        #[test]
        fn sweep_names_the_extra_branch_of_a_run() {
            let dir = tempfile::tempdir().unwrap();
            let (calls, dry) = run(dir.path(), "dry", Branch::Plain);
            let (again, dry_again) = run(dir.path(), "again", Branch::Plain);
            let (more, cut) = run(dir.path(), "cut", Branch::Extra);

            // Two plain runs on two stores: the same count, the same lines.
            assert_eq!((calls, again), (3, 3));
            same_count(calls, again, &dry, &dry_again);
            assert_eq!(diverged(&dry, &dry_again, calls), None);

            // The extra branch is line 2.
            assert_eq!(more, 4);
            let extra = (
                2,
                "2 append meta/journal.log".to_owned(),
                "2 exists meta/features".to_owned(),
            );
            assert_eq!(diverged(&dry, &cut, calls), Some(extra));
            let refused = std::panic::catch_unwind(|| same_count(calls, more, &dry, &cut));
            let panic = refused.expect_err("3 calls are not 4");
            let said = panic.downcast_ref::<String>().expect("a formatted panic");
            assert!(
                said.contains("line 2:\n  first:  \"2 append meta/journal.log\""),
                "{said}"
            );
        }
    }
}
