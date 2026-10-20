//! The git import cut short at each of its filesystem calls, and the import at scale (the M3
//! plan's Review Focus 3): an import crashed (`SWARMOPS_TEST_CRASH_AT`) or failed
//! (`SWARMOPS_TEST_FAIL_AT`) at any call leaves a store that verifies clean, and the same import
//! run again ends with the changesets, the target and the external ids an uninterrupted import
//! leaves; and 100,000 commits import in 13 frames within a memory budget. The sweeps drive the
//! store's test hooks, so `all.rs` holds this module only with the `test-hooks` feature, which the
//! workspace's gates turn on (`--all-features`).
//!
//! - A sweep imports `refs`' `newer`, six commits, into `main`, two changesets to a frame
//!   ([`request`]), onto two stores ([`Onto`]): a fresh one (three frames, then `main` created),
//!   and one that holds `older`, `newer`'s first three commits, with `main` at its tip (two frames
//!   from that base, then `main` moved on). Each frame's external ids follow it, and the
//!   metadata's flush, the target's move and the flush after it end the import.
//! - A dry run counts the import's filesystem calls, its store's opening included
//!   (`SWARMOPS_TEST_FS_CALLS`). Each run then imports in a child process, this test binary run
//!   again ([`import_as_child`]), on a copy of the sweep's store, its k-th call crashing or failing
//!   it. With a hook that counts calls set, the object store, the metadata store and the external
//!   id index run at parallelism 1, so that call k is the same call in every run. Every child
//!   runs under the store's order checker (`SWARMOPS_TEST_ORDER_CHECK=1`), and so does what the
//!   parent opens and imports after it ([`checked`]).
//! - The oracle. A crashed child aborts at its crash (one past its last call, it finishes). A
//!   failed child stops with a store failure, its committed frames' commits in the store and
//!   `main` not moved, or succeeds with every commit recorded (a failure past its last commit point
//!   is a warning); no error or warning of it names the store's path. Either way, the store it
//!   left verifies clean, and the same import, run again in the parent, succeeds and leaves
//!   `main`, the changesets and the external ids of the dry run's uninterrupted import, the store
//!   verifying clean again. After a crash, that import takes exactly the commits whose external
//!   ids the crash left out.
//! - The default gate takes every fifth call, with the last two, and fails them onto the fresh
//!   store alone; the ignored variants take every call, onto both. A failure sweep also fails
//!   every fifth call persistently, with `ENOSPC`.
//!
//! The git export's crash and failure sweeps at its git-side calls (the M8 plan's Review Focus
//! 3) are the module `export` below; `swarmops-cli`'s `tests/git.rs` sweeps its store calls.
//!
//! Each run owns its temp dirs, and the tests share nothing.

use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_core::ChangesetId;
use swarmops_core::bytes::to_u64;
use swarmops_git::import::DEFAULT_FRAME_CHANGESETS;
use swarmops_git::{GitOid, GixSource, ImportError, ImportReport, ImportRequest, import};
use swarmops_repo::LocalRepo;
use swarmops_store::external_ids::lower_hex;
use swarmops_store::{
    CRASH_AT_ENV, ExternalEntry, ExternalIds, FAIL_AT_ENV, FS_CALLS_ENV, Frame, Journal, Layout,
    ORDER_CHECK_ENV, RealFs, Record, SYSTEM_GIT,
};
use swarmops_testkit::child::{Child, SIGABRT, run, this_test};
use swarmops_testkit::faults::checked;
use swarmops_testkit::fs::{NoSyncFs, copy_dir};
use swarmops_testkit::git::{self, GitFixture};
use swarmops_testkit::test_name;
use tempfile::TempDir;

use crate::support::MemSource;

/// Set only in the children the sweeps spawn: the store the child imports into.
const STORE_ENV: &str = "SWARMOPS_TEST_GIT_SWEEP_STORE";

/// Set with [`STORE_ENV`]: the git repository the child imports from.
const SOURCE_ENV: &str = "SWARMOPS_TEST_GIT_SWEEP_SOURCE";

/// Set only in the child of the scale test: the store it creates and imports into.
const SCALE_ENV: &str = "SWARMOPS_TEST_GIT_SCALE_STORE";

/// How a child reports to its parent, among the test harness's output.
const REPORT: &str = "git-sweep-child: ";

/// Who moves the target in these tests.
const ACTOR: &str = "test/sweeps";

/// How many commits `newer` reaches.
const COMMITS: u64 = 6;

/// How many changesets a frame of the swept import holds: `newer` takes three frames.
const FRAME_CHANGESETS: usize = 2;

/// The default gate's share of a sweep's calls: every fifth.
const SAMPLED: u64 = 5;

/// Which of every [`SAMPLED`] calls a failure sweep fails persistently: those two after the ones
/// the default gate fails once.
const PERSISTENT_AT: u64 = 2;

/// How many runs of a sweep go at once, each with a child process and a store of its own.
const AT_ONCE: usize = 8;

/// The scale test's commits.
const SCALE_COMMITS: u64 = 100_000;

/// The most memory the scale test's process may hold resident at its peak: 1 GiB, in KiB.
const SCALE_PEAK_KIB: u64 = 1 << 20;

/// How long the scale test's child may run.
const SCALE_TIMEOUT: Duration = Duration::from_mins(30);

/// The import the sweeps cut short: `newer` into `main`, [`FRAME_CHANGESETS`] to a frame.
fn request() -> ImportRequest {
    let mut req = ImportRequest::new("newer", "main", ACTOR);
    req.frame_changesets = FRAME_CHANGESETS;
    req
}

/// `path` as a UTF-8 path, which every path these tests make is.
fn utf8(path: PathBuf) -> Utf8PathBuf {
    Utf8PathBuf::from_path_buf(path).expect("a UTF-8 path")
}

/// The repository at `path`, read through gix.
fn open(path: &Path) -> GixSource {
    GixSource::open(path).unwrap_or_else(|error| panic!("open {}: {error}", path.display()))
}

/// What a sweep's import starts from.
#[derive(Clone, Copy, Debug)]
enum Onto {
    /// A fresh store: the import makes three frames and creates `main`.
    Fresh,
    /// A store that holds `older`, `newer`'s first three commits, `main` at its tip: the import
    /// goes on from that base, makes two frames, and moves `main` on.
    Older,
}

impl Onto {
    /// How many of `newer`'s commits the store holds before the import.
    fn held(self) -> u64 {
        match self {
            Onto::Fresh => 0,
            Onto::Older => 3,
        }
    }

    /// What the import adds to the journal, frame by frame ([`shape`]).
    fn frames(self) -> &'static [&'static str] {
        match self {
            Onto::Fresh => &["changesets 2", "changesets 2", "changesets 2", "ref main"],
            Onto::Older => &["changesets 2", "changesets 1", "ref main"],
        }
    }
}

/// What a store holds of an import: how many changesets, what `main` names, and every git entry
/// of the external id index, by its key's hex, the commit's oid.
#[derive(Debug, PartialEq, Eq)]
struct State {
    changesets: u64,
    main: Option<ChangesetId>,
    entries: Vec<(String, ExternalEntry)>,
}

impl State {
    /// What the store at `root` holds, read under the order checker.
    fn of(root: &Utf8Path) -> State {
        let repo = LocalRepo::open_with_fs(root, checked(RealFs)).expect("open the store");
        let info = repo.info().expect("the store's counts");
        let ids = ExternalIds::new(Layout::new(root), checked(RealFs));
        let listed = ids.list(SYSTEM_GIT).expect("list the git external ids");
        let mut entries: Vec<(String, ExternalEntry)> = (listed.into_iter())
            .map(|(key, entry)| (lower_hex(&key), entry.expect("a whole entry")))
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        State {
            changesets: info.changesets,
            main: info.main,
            entries,
        }
    }
}

/// The journal's frames in the store at `root`, as a reader reads them.
fn frames(root: &Utf8Path) -> Vec<Frame> {
    let journal = Journal::open(&Layout::new(root), Arc::new(RealFs)).expect("open the journal");
    journal.tail_frames_from(0).expect("read the journal")
}

/// What `frame` records, in short: `changesets <n>`, `ref <name>` or `compacted`.
fn shape(frame: &Frame) -> String {
    match &frame.records[..] {
        [Record::RefMove { name, .. }] => format!("ref {name}"),
        [Record::Compacted { .. }] => "compacted".to_owned(),
        records
            if records
                .iter()
                .all(|r| matches!(r, Record::Changeset { .. })) =>
        {
            format!("changesets {}", records.len())
        }
        records => panic!("a frame of the import that records {records:?}"),
    }
}

/// How a child's import ended, as it reports it ([`REPORT`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ended {
    /// It succeeded, having recorded `imported` commits, with `warnings` warnings: steps past its
    /// last commit point that failed.
    Imported { imported: u64, warnings: usize },
    /// A store failure stopped it once its committed frames had recorded `imported` commits
    /// ([`ImportError::Store`]).
    Stopped { imported: u64 },
    /// A step after a frame's commit point failed, and it stopped after that frame, its frames
    /// having recorded `imported` commits ([`ImportError::PostCommit`]).
    PostCommit { imported: u64 },
    /// The store did not open, so nothing was imported.
    Unopened,
}

impl Ended {
    /// The line a child reports it in, after [`REPORT`].
    fn line(self) -> String {
        match self {
            Ended::Imported { imported, warnings } => format!("imported {imported} {warnings}"),
            Ended::Stopped { imported } => format!("stopped {imported}"),
            Ended::PostCommit { imported } => format!("post-commit {imported}"),
            Ended::Unopened => "unopened".to_owned(),
        }
    }

    /// What [`Ended::line`] made `line` of.
    fn parse(line: &str) -> Option<Ended> {
        let words: Vec<&str> = line.split(' ').collect();
        let number = |at: usize| -> Option<u64> { words.get(at)?.parse().ok() };
        Some(match words.first().copied()? {
            "imported" => Ended::Imported {
                imported: number(1)?,
                warnings: words.get(2)?.parse().ok()?,
            },
            "stopped" => Ended::Stopped {
                imported: number(1)?,
            },
            "post-commit" => Ended::PostCommit {
                imported: number(1)?,
            },
            "unopened" => Ended::Unopened,
            _ => return None,
        })
    }
}

/// When this process is a child a sweep spawned ([`STORE_ENV`] set): imports `newer` into the
/// store its parent named, on the filesystem the hooks its parent set ask for, reports how the
/// import ended ([`REPORT`]), and returns `true`.
fn import_as_child() -> bool {
    let Some(root) = std::env::var_os(STORE_ENV) else {
        return false;
    };
    let root = utf8(PathBuf::from(root));
    let source = PathBuf::from(std::env::var_os(SOURCE_ENV).expect("the source repository"));
    let source = open(&source);
    // A hook that counts calls is set: `open` hands the object store, the metadata store and the
    // external id index one filesystem that counts their calls, each at parallelism 1.
    let ended = match LocalRepo::open(&root) {
        Ok(repo) => import_into(&repo, &source, &root),
        Err(_) => Ended::Unopened,
    };
    println!("{REPORT}{}", ended.line());
    true
}

/// The child's import of `newer` from `source` into `repo`, whose root is `root`, and how it
/// ended. An outcome no injected store failure explains fails the child, and with it the sweep:
/// an error other than a store failure, and an error or a warning that names the store's path (a
/// store-facing message names a store file by its path in the store).
fn import_into(repo: &LocalRepo, source: &GixSource, root: &Utf8Path) -> Ended {
    let no_host_path = |text: &str| {
        assert!(
            !text.contains(root.as_str()),
            "the store's path in what the import said: {text}"
        );
    };
    match import(repo, source, &request()) {
        Ok(report) => {
            for warning in &report.warnings {
                no_host_path(warning);
            }
            Ended::Imported {
                imported: report.imported,
                warnings: report.warnings.len(),
            }
        }
        Err(error) => {
            no_host_path(&error.to_string());
            match error {
                ImportError::Store { imported, .. } => Ended::Stopped { imported },
                ImportError::PostCommit { imported, .. } => Ended::PostCommit { imported },
                other => panic!("an import whose store failed stopped otherwise: {other:?}"),
            }
        }
    }
}

/// Runs `test` again in a child process ([`import_as_child`]) that imports `newer` from `source`
/// into the store at `root`, with `hook`, a test hook's variable and value, set and every
/// filesystem call checked against the store's write-ordering rules; what it printed and how it
/// ended.
fn child(test: &str, root: &Utf8Path, source: &Path, hook: (&str, &str)) -> Output {
    run(this_test(&test_name!(test))
        .env(STORE_ENV, root)
        .env(SOURCE_ENV, source)
        .env(hook.0, hook.1)
        .env(ORDER_CHECK_ENV, "1"))
}

/// What the child that gave `output` reported of its import. A child that failed, or reported
/// nothing, fails the test with what it printed.
fn ended(output: &Output) -> Ended {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let reported = (stdout.lines())
        .find_map(|line| line.strip_prefix(REPORT))
        .and_then(Ended::parse);
    match reported {
        Some(ended) if output.status.success() => ended,
        _ => panic!(
            "a child that did not report how its import ended: {:?}\n--- stdout ---\n{stdout}\n\
             --- stderr ---\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr),
        ),
    }
}

/// A fresh copy of the store at `template`, in a temp dir of its own: faster than creating one,
/// which syncs a dozen times.
fn copy(template: &Utf8Path) -> (TempDir, Utf8PathBuf) {
    let dir = tempfile::tempdir().expect("a temp dir");
    let root = utf8(dir.path().join("store"));
    copy_dir(template.as_std_path(), root.as_std_path());
    (dir, root)
}

/// What a sweep stands on: the fixture, the store every run copies, the dry run's call count,
/// and what the uninterrupted import leaves.
struct Sweep {
    /// The test whose children import ([`import_as_child`]).
    test: &'static str,
    /// What the import starts from.
    onto: Onto,
    /// `refs`, which holds `source`; it lives as long as the sweep.
    _fixture: GitFixture,
    /// `refs`' repository.
    source: PathBuf,
    /// The temp dir that holds `template`.
    _template_dir: TempDir,
    /// The store every run copies.
    template: Utf8PathBuf,
    /// How many filesystem calls the uninterrupted import makes, its store's opening included.
    calls: u64,
    /// What the uninterrupted import leaves.
    whole: State,
}

impl Sweep {
    /// The ground of a sweep of `test` whose import starts from `onto`: the store (fresh, or
    /// holding `older`), and the dry run, a child that imports on a copy of it, counting its
    /// calls, under the order checker. That import must record `newer`'s other commits in frames
    /// of two, their external ids, and `main`'s move to the tip's changeset, and flush.
    fn new(test: &'static str, onto: Onto) -> Sweep {
        let fixture = git::fixture("refs");
        let source = fixture.repo.clone().expect("refs holds a repository");
        let template_dir = tempfile::tempdir().expect("a temp dir");
        let template = utf8(template_dir.path().join("store"));
        let repo = LocalRepo::create_with_fs(&template, "sweeps", checked(RealFs))
            .expect("create the store");
        if let Onto::Older = onto {
            let older = ImportRequest::new("older", "main", ACTOR);
            let report = import(&repo, &open(&source), &older).expect("import older");
            assert_eq!(report.imported, onto.held());
        }
        drop(repo);

        let (dir, root) = copy(&template);
        let file = dir.path().join("fs-calls");
        let file_name = file.to_str().expect("a UTF-8 temp path");
        let output = child(test, &root, &source, (FS_CALLS_ENV, file_name));
        let uninterrupted = Ended::Imported {
            imported: COMMITS - onto.held(),
            warnings: 0,
        };
        assert_eq!(ended(&output), uninterrupted, "the dry run onto {onto:?}");
        let calls = std::fs::read_to_string(&file).expect("the dry run's call count");
        let calls: u64 = calls.trim().parse().expect("a call count");
        let before = frames(&template).len();
        let added: Vec<String> = frames(&root)[before..].iter().map(shape).collect();
        assert_eq!(added, onto.frames(), "the dry run onto {onto:?}");
        let journal = Journal::open(&Layout::new(&root), Arc::new(RealFs)).expect("the journal");
        let end = std::fs::metadata(root.join("meta/journal.log")).expect("journal.log");
        assert_eq!(journal.ckpt().expect("the checkpoint"), end.len());
        let whole = State::of(&root);
        let recorded = (whole.changesets, to_u64(whole.entries.len()));
        assert_eq!(recorded, (COMMITS, COMMITS), "the dry run onto {onto:?}");
        assert!(whole.entries.iter().all(|(_, entry)| entry.faithful));
        assert!(
            calls > 100,
            "the import makes a hundred calls and more: {calls}"
        );
        Sweep {
            test,
            onto,
            _fixture: fixture,
            source,
            _template_dir: template_dir,
            template,
            calls,
            whole,
        }
    }

    /// The import run again on the store at `root` as a run left it (`what` names the run), all
    /// under the order checker, once the store verifies clean: it must succeed and leave `main`,
    /// the changesets and the external ids the uninterrupted import leaves, and the store must
    /// verify clean again. Its report. It runs without a flush to stable storage ([`NoSyncFs`]),
    /// its calls checked all the same: what it leaves the store holding is the subject here, not
    /// what a power loss would leave of it, and the run before it paid for its flushes.
    fn resume(&self, root: &Utf8Path, what: &str) -> ImportReport {
        let repo = LocalRepo::open_with_fs(root, checked(NoSyncFs))
            .unwrap_or_else(|error| panic!("{what}: open: {error}"));
        let verified = repo.verify(true).expect("verify");
        assert_eq!(verified.problems, [], "{what}: as the run left it");
        let report = import(&repo, &open(&self.source), &request())
            .unwrap_or_else(|error| panic!("{what}: the import again: {error}"));
        assert_eq!(Some(report.head), self.whole.main, "{what}");
        assert_eq!(State::of(root), self.whole, "{what}");
        let verified = repo.verify(true).expect("verify");
        assert_eq!(verified.problems, [], "{what}: after the import again");
        report
    }
}

/// Runs `run` on each of `items`, [`AT_ONCE`] at a time: as many threads each take the next item
/// as they finish one. The outcomes, in the order of `items`; a run that panics fails the sweep
/// with its panic.
fn runs<K: Sync, T: Send>(items: &[K], run: impl Fn(&K) -> T + Sync) -> Vec<T> {
    let next = AtomicUsize::new(0);
    let mut outcomes: Vec<(usize, T)> = thread::scope(|scope| {
        let workers: Vec<_> = (0..AT_ONCE)
            .map(|_| {
                scope.spawn(|| {
                    let mut done = Vec::new();
                    loop {
                        let at = next.fetch_add(1, Ordering::Relaxed);
                        let Some(item) = items.get(at) else {
                            break done;
                        };
                        done.push((at, run(item)));
                    }
                })
            })
            .collect();
        (workers.into_iter())
            .flat_map(|worker| {
                worker
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            })
            .collect()
    });
    outcomes.sort_by_key(|(at, _)| *at);
    outcomes.into_iter().map(|(_, outcome)| outcome).collect()
}

/// The calls of `1..=last` a sweep takes: every `every`-th, and the last two.
fn calls_taken(last: u64, every: u64) -> Vec<u64> {
    (1..=last)
        .filter(|&k| k % every == 0 || k + 1 >= last)
        .collect()
}

/// A class of a sweep's runs: its name, and whether a run's outcome is in it.
type Class<'a, T> = (&'static str, &'a dyn Fn(T) -> bool);

/// What a crashed run left, before the import ran again.
#[derive(Debug, Clone, Copy)]
struct Crashed {
    /// Whether the child finished: the crash was past its last call.
    finished: bool,
    /// How many changesets the store held.
    changesets: u64,
    /// How many git entries the external id index held.
    entries: u64,
    /// Whether `main` named the tip's changeset.
    moved: bool,
}

/// One run of a crash sweep: the import in a child on a copy of the sweep's store, its k-th call
/// aborting it, then the store checked and the import run again ([`Sweep::resume`]). Its walk
/// stops at the first commits the index names, so it takes exactly the commits whose external ids
/// the crash left out, unless `main` moved.
fn crash_at(sweep: &Sweep, k: u64) -> Crashed {
    let (_dir, root) = copy(&sweep.template);
    let at = k.to_string();
    let output = child(sweep.test, &root, &sweep.source, (CRASH_AT_ENV, &at));
    let what = format!(
        "onto {:?}, crashed at call {k} of {}",
        sweep.onto, sweep.calls
    );
    let finished = k > sweep.calls;
    if finished {
        let whole = Ended::Imported {
            imported: COMMITS - sweep.onto.held(),
            warnings: 0,
        };
        assert_eq!(ended(&output), whole, "{what}");
    } else {
        assert_eq!(
            output.status.signal(),
            Some(SIGABRT),
            "{what}: {:?}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let left = State::of(&root);
    let moved = left.main == sweep.whole.main;
    let entries = to_u64(left.entries.len());
    let resumed = sweep.resume(&root, &what);
    let expected = if moved {
        (0, false)
    } else {
        (COMMITS - entries, true)
    };
    let took = (resumed.imported, resumed.ref_moved);
    assert_eq!(took, expected, "{what}: it left {left:?}");
    Crashed {
        finished,
        changesets: left.changesets,
        entries,
        moved,
    }
}

/// The crash sweep of `test` at every `every`-th call of the import onto each store, and its
/// last two. What the crashes left must hold a crash between two frames, one past a frame's
/// commit point and before its external ids (W8's window), one past the last frame and before
/// `main` moved, and one past the move's commit point: else the sweep missed what it is for.
/// Prints how many runs it made, and how many crashes fell in each of those windows.
fn crash_sweep(test: &'static str, every: u64) {
    for onto in [Onto::Fresh, Onto::Older] {
        let sweep = Sweep::new(test, onto);
        let ks = calls_taken(sweep.calls + 1, every);
        let crashes = runs(&ks, |&k| crash_at(&sweep, k));
        let held = onto.held();
        let windows: [Class<'_, Crashed>; 4] = [
            ("between two frames", &|c| {
                held < c.changesets && c.changesets < COMMITS
            }),
            (
                "between a frame's commit point and its external ids",
                &|c| c.entries < c.changesets,
            ),
            ("after the last frame, before the move", &|c| {
                c.entries == COMMITS && !c.moved
            }),
            ("after the move's commit point", &|c| c.moved && !c.finished),
        ];
        let counts: Vec<String> = (windows.iter())
            .map(|(what, left)| {
                let inside = crashes.iter().filter(|&&c| left(c)).count();
                assert!(inside > 0, "onto {onto:?}, no crash {what}: {crashes:?}");
                format!("{inside} {what}")
            })
            .collect();
        println!(
            "crash sweep onto {onto:?}: {} runs over {} calls; {}",
            ks.len(),
            sweep.calls,
            counts.join(", ")
        );
    }
}

/// One run of a failure sweep: the import in a child on a copy of the sweep's store, its k-th
/// call failing as `failure` says (`<mode>:<errno>`). The child must stop with a store failure,
/// the store holding its committed frames' commits and no more, `main` not moved, and, after a
/// failure past a frame's commit point, none of that frame's external ids: it writes nothing
/// after a failed write. Or it succeeds, every commit recorded, saying what failed in a warning:
/// a failed call is never passed over in silence. Then the store is checked and the import run
/// again ([`Sweep::resume`]).
fn fail_at(sweep: &Sweep, k: u64, failure: &str) -> Ended {
    let (_dir, root) = copy(&sweep.template);
    let spec = format!("{k}:{failure}");
    let output = child(sweep.test, &root, &sweep.source, (FAIL_AT_ENV, &spec));
    let what = format!(
        "onto {:?}, call {k} of {} failing {failure}",
        sweep.onto, sweep.calls
    );
    let ended = ended(&output);
    let left = State::of(&root);
    let held = sweep.onto.held();
    match ended {
        Ended::Imported { imported, warnings } => {
            assert_eq!(imported, COMMITS - held, "{what}");
            assert_eq!(
                left, sweep.whole,
                "{what}: it succeeded, so it recorded all"
            );
            assert!(warnings > 0, "{what}: it succeeded, and said nothing of it");
        }
        Ended::Stopped { imported } | Ended::PostCommit { imported } => {
            let committed = (left.changesets, left.main == sweep.whole.main);
            assert_eq!(committed, (held + imported, false), "{what}: {ended:?}");
        }
        Ended::Unopened => assert_eq!(left.changesets, held, "{what}"),
    }
    if let Ended::PostCommit { imported } = ended {
        let entries = to_u64(left.entries.len());
        assert!(
            entries < held + imported,
            "{what}: the external ids of the frame it stopped after: {left:?}"
        );
    }
    sweep.resume(&root, &what);
    ended
}

/// The failure sweep of `test`: the import onto each of `starts` failed once with `EIO` at every
/// `every`-th call and the last two, and persistently with `ENOSPC` at every fifth
/// ([`PERSISTENT_AT`]). Among what the failures ended in must be a failure between two frames,
/// one after a frame's commit point (`PostCommit`), and a success with a warning, a failure past
/// the import's last commit point. Prints how many runs it made, and how they ended.
fn failure_sweep(test: &'static str, every: u64, starts: &[Onto]) {
    for &onto in starts {
        let sweep = Sweep::new(test, onto);
        let once = calls_taken(sweep.calls, every).into_iter();
        let once = once.map(|k| (k, "once:eio"));
        let persistent = (1..=sweep.calls)
            .filter(|k| k % SAMPLED == PERSISTENT_AT)
            .map(|k| (k, "persistent:enospc"));
        let failures: Vec<(u64, &str)> = once.chain(persistent).collect();
        let ended = runs(&failures, |&(k, failure)| fail_at(&sweep, k, failure));
        let new = COMMITS - onto.held();
        let classes: [Class<'_, Ended>; 3] = [
            (
                "stopped between two frames",
                &|e| matches!(e, Ended::Stopped { imported } if 0 < imported && imported < new),
            ),
            ("stopped after a frame's commit point", &|e| {
                matches!(e, Ended::PostCommit { .. })
            }),
            ("succeeded with a warning", &|e| {
                matches!(e, Ended::Imported { .. })
            }),
        ];
        let counts: Vec<String> = (classes.iter())
            .map(|(what, class)| {
                let failed = ended.iter().filter(|&&e| class(e)).count();
                assert!(failed > 0, "onto {onto:?}, no run {what}: {ended:?}");
                format!("{failed} {what}")
            })
            .collect();
        println!(
            "failure sweep onto {onto:?}: {} runs over {} calls; {}",
            failures.len(),
            sweep.calls,
            counts.join(", ")
        );
    }
}

/// An import crashed at any filesystem call (every fifth here; every one in the ignored variant
/// below) leaves a store that verifies clean, and the same import run again takes exactly what
/// the crash left out, ending where an uninterrupted import ends.
#[test]
fn import_crashing_at_any_call_resumes_to_the_same_state() {
    if import_as_child() {
        return;
    }
    crash_sweep(
        "import_crashing_at_any_call_resumes_to_the_same_state",
        SAMPLED,
    );
}

/// [`import_crashing_at_any_call_resumes_to_the_same_state`] at every call.
#[test]
#[ignore = "slow, ~90 s: an import crashed at each of its calls"]
fn import_crashing_at_every_call_resumes_to_the_same_state() {
    if import_as_child() {
        return;
    }
    crash_sweep("import_crashing_at_every_call_resumes_to_the_same_state", 1);
}

/// An import whose filesystem call fails (every fifth here, once and persistently, onto the fresh
/// store; every one onto both in the ignored variant below) stops with a store failure, its
/// committed frames recorded, or succeeds with everything recorded and a warning; the same import
/// run again ends where an uninterrupted one ends.
#[test]
fn import_failing_at_any_call_stops_and_resumes() {
    if import_as_child() {
        return;
    }
    failure_sweep(
        "import_failing_at_any_call_stops_and_resumes",
        SAMPLED,
        &[Onto::Fresh],
    );
}

/// [`import_failing_at_any_call_stops_and_resumes`] at every call, onto both stores.
#[test]
#[ignore = "slow, ~2 min: an import failed at each of its calls"]
fn import_failing_at_every_call_stops_and_resumes() {
    if import_as_child() {
        return;
    }
    failure_sweep(
        "import_failing_at_every_call_stops_and_resumes",
        1,
        &[Onto::Fresh, Onto::Older],
    );
}

/// The peak resident memory of this process (`VmHWM` in `/proc/self/status`), in KiB.
fn peak_resident_kib() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").expect("/proc/self/status");
    (status.lines())
        .find_map(|line| line.strip_prefix("VmHWM:"))
        .and_then(|value| value.trim().strip_suffix("kB")?.trim().parse().ok())
        .expect("VmHWM in /proc/self/status")
}

/// The scale test's child: a [`MemSource`] of [`SCALE_COMMITS`] linear commits that each change
/// one file, imported with the default request into a fresh store at `root` that never flushes
/// to stable storage ([`NoSyncFs`]: what the store holds is the subject, not what survives a
/// power loss). Reports the commits imported, the import's wall time in milliseconds and the
/// process's peak resident memory in KiB, the source's included.
fn import_at_scale(root: &Utf8Path) {
    let mut source = MemSource::default();
    let mut parent: Option<GitOid> = None;
    for n in 1..=SCALE_COMMITS {
        let blob = source.blob(format!("{n}\n").as_bytes());
        let tree = source.tree(&[("100644", b"count.txt", blob)]);
        let message = format!("count to {n}\n");
        parent = Some(source.commit(tree, parent.as_slice(), &message));
    }
    let tip = parent.expect("commits");
    let repo = LocalRepo::create_with_fs(root, "scale", Arc::new(NoSyncFs)).expect("create");
    let req = ImportRequest::new(tip.to_string(), "main", ACTOR);
    let started = Instant::now();
    let report = import(&repo, &source, &req).expect("the import");
    let wall = started.elapsed().as_millis();
    let peak = peak_resident_kib();
    println!("{REPORT}scale {} {wall} {peak}", report.imported);
}

/// 100,000 commits import in 13 frames of at most 8,192 changesets, the index compacting at
/// most once a frame and once more, and the importing process's peak resident memory stays under
/// 1 GiB. The import runs in a child of its own, so that the peak is its own; the test prints the
/// wall time.
#[test]
#[ignore = "slow, ~2 min: 100,000 synthetic commits"]
fn a_hundred_thousand_commit_import_stays_within_its_budget() {
    if let Some(root) = std::env::var_os(SCALE_ENV) {
        import_at_scale(&utf8(PathBuf::from(root)));
        return;
    }
    let dir = tempfile::tempdir().expect("a temp dir");
    let root = utf8(dir.path().join("store"));
    let test = test_name!("a_hundred_thousand_commit_import_stays_within_its_budget");
    let mut command = this_test(&test);
    command.env(SCALE_ENV, &root);
    let output = Child::spawn(&mut command).wait(Instant::now() + SCALE_TIMEOUT);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let reported: Option<Vec<u64>> = (stdout.lines())
        .find_map(|line| line.strip_prefix(REPORT)?.strip_prefix("scale "))
        .and_then(|line| line.split(' ').map(|n| n.parse().ok()).collect());
    let (true, Some(&[imported, wall_ms, peak_kib])) =
        (output.status.success(), reported.as_deref())
    else {
        panic!(
            "the scale child failed, or reported nothing: {:?}\n--- stdout ---\n{stdout}\n\
             --- stderr ---\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr),
        );
    };
    assert_eq!(imported, SCALE_COMMITS);

    let journal = frames(&root);
    let sizes: Vec<usize> = (journal.iter())
        .map(|frame| {
            let changesets = (frame.records.iter())
                .filter(|record| matches!(record, Record::Changeset { .. }))
                .count();
            assert!(changesets == 0 || changesets == frame.records.len());
            changesets
        })
        .filter(|&changesets| changesets > 0)
        .collect();
    let full = usize::try_from(SCALE_COMMITS).expect("fits") / DEFAULT_FRAME_CHANGESETS;
    let mut expected = vec![DEFAULT_FRAME_CHANGESETS; full];
    expected.push(usize::try_from(SCALE_COMMITS).expect("fits") % DEFAULT_FRAME_CHANGESETS);
    assert_eq!(sizes, expected, "13 frames: 12 of 8,192 and one of 1,696");
    let compactions = (journal.iter())
        .filter(|frame| matches!(frame.records[..], [Record::Compacted { .. }]))
        .count();
    assert!(
        compactions <= sizes.len() + 1,
        "{compactions} compactions for {} frames",
        sizes.len()
    );
    assert!(
        peak_kib < SCALE_PEAK_KIB,
        "a peak of {peak_kib} KiB resident, past 1 GiB"
    );
    println!(
        "scale: {imported} commits imported in {wall_ms} ms, {} changeset frames, {compactions} \
         compactions, a peak of {peak_kib} KiB resident",
        sizes.len()
    );
}

/// The git export cut short at each of its git-side calls (the M8 plan's Review Focus 3): an export
/// whose process dies before any write, sync or move of its destination, or whose destination fails
/// one of them, leaves the branch where it was, and the same export run again ends with the branch,
/// the objects and the external id entries an uninterrupted export leaves.
///
/// - A sweep exports `main` from a store that imported `linear` and holds three native changesets
///   on it, the third deleting a file and adding an executable and a symlink ([`template`]), onto
///   two destinations ([`Onto`]): an empty repository, which takes `linear`'s history too, and
///   `linear`'s own repository, its objects in a pack and `main` in `packed-refs`, which takes the
///   three native commits on its tip.
/// - The crash sweep exports in a child process, this test binary run again ([`export_as_child`]),
///   through [`Crashing`], a [`GixSink`] that aborts the process right before its k-th mutating
///   call (`write_object`, `sync`, `cas_branch`, counted by a dry run), or inside the branch's move,
///   between its lock and its rename, leaving the lock as git's lock protocol leaves it. The parent
///   then finds the branch where it was, or at the head's export with every object it reaches
///   reading back; runs the export again (after removing the lock a crash in the move left, once
///   the export has said so, as the error's hint says); and checks that it took exactly what the
///   crash left out, and ended where the uninterrupted export ended.
/// - The failure sweep fails each mutating call of a [`MemSink`] in turn ([`MemSink::fail_at`]):
///   the export stops at that call with its error, the branch where it was, and makes no call
///   after it; the same export run again on the same sink ends where an uninterrupted one ends, and
///   no power loss after any call of the two exports leaves the branch naming an object it took.
/// - The store's side is the subject of `swarmops-cli`'s `tests/git.rs`, whose sweeps crash and
///   fail `git export` at the store's calls. Here every store opens under the order checker
///   ([`checked`]), without its flushes ([`NoSyncFs`]): a crash of the process keeps what it wrote.
mod export {
    use std::collections::{BTreeMap, BTreeSet};
    use std::fs::{self, OpenOptions};
    use std::io::Write as _;
    use std::os::unix::process::ExitStatusExt;
    use std::path::{Path, PathBuf};
    use std::process::Output;

    use camino::Utf8Path;
    use swarmops_core::{ChangesetId, LeafKind};
    use swarmops_git::{
        BranchUpdate, ExportError, ExportReport, ExportRequest, GitOid, GitSink, GitSource,
        GixSink, GixSource, ImportRequest, ObjectFormat, RawCommit, RawKind, RawTree, SinkError,
        Stored, export, import, register,
    };
    use swarmops_repo::{Advance, Change, LocalRepo};
    use swarmops_testkit::child::{SIGABRT, run, this_test};
    use swarmops_testkit::faults::checked;
    use swarmops_testkit::fs::{NoSyncFs, copy_dir};
    use swarmops_testkit::git::{self, EmptyRepository, GitFixture};
    use swarmops_testkit::repo::{delete, put, put_as, request};
    use swarmops_testkit::test_name;
    use tempfile::TempDir;

    use super::{Class, SAMPLED, State, runs, utf8};
    use crate::support::{Keep, MemSink, reaches_only_kept};

    /// Set only in the children of the crash sweep: the store the child exports from.
    const STORE_ENV: &str = "SWARMOPS_TEST_GIT_EXPORT_SWEEP_STORE";

    /// Set with [`STORE_ENV`]: the repository the child exports into.
    const DESTINATION_ENV: &str = "SWARMOPS_TEST_GIT_EXPORT_SWEEP_DESTINATION";

    /// Set with [`STORE_ENV`]: where the child's sink crashes it ([`Crash::env`]).
    const CRASH_ENV: &str = "SWARMOPS_TEST_GIT_EXPORT_SWEEP_CRASH";

    /// How a child reports to its parent, among the test harness's output.
    const REPORT: &str = "git-export-sweep-child: ";

    /// Who moves the store's refs in these tests.
    const ACTOR: &str = "test/export-sweeps";

    /// The branch every export writes, and the store ref it exports.
    const MAIN: &str = "main";

    /// The export the sweeps cut short: `main` into the branch `main`.
    fn request_main() -> ExportRequest {
        ExportRequest::new(MAIN, MAIN)
    }

    /// What a sweep's export writes into.
    #[derive(Clone, Copy, Debug)]
    enum Onto {
        /// An empty repository: the export writes `linear`'s history and the three native commits
        /// on it, and creates the branch.
        Empty,
        /// `linear`'s own repository, its objects in a pack and `main` in `packed-refs`: the
        /// export trusts them, writes the three native commits on its tip, and moves the branch.
        Fixture,
    }

    /// The store at `root`, opened under the order checker without its flushes, with
    /// [`swarmops_git::ExportOid`] registered: what every export here reads.
    fn exporter(root: &Utf8Path) -> LocalRepo {
        let mut repo = LocalRepo::open_with_fs(root, checked(NoSyncFs))
            .unwrap_or_else(|error| panic!("open the store: {error}"));
        register(&mut repo).expect("register ExportOid");
        repo
    }

    /// The repository at `dir`, written through gix.
    fn gix(dir: &Path) -> GixSink {
        GixSink::open(dir).unwrap_or_else(|error| panic!("open {}: {error}", dir.display()))
    }

    /// Commits `changes` on `main`'s head and moves `main` there, in one frame.
    fn commit_on_main(repo: &LocalRepo, changes: Vec<Change>, message: &str) -> ChangesetId {
        let head = repo
            .get_ref(MAIN)
            .expect("read main")
            .map(|state| state.target);
        let mut req = request(head.as_slice(), changes, message);
        req.advance = Some(Advance {
            name: MAIN.to_owned(),
            actor: ACTOR.to_owned(),
        });
        repo.commit(req).expect("commit on main").id
    }

    /// Makes the store every run of a sweep copies, at `root`: `linear`'s `main`, imported from
    /// the repository at `source`, then three native changesets on it, `main` at the third; the
    /// second's message ends in a trailer, and the third deletes a file and adds an executable and
    /// a symlink.
    fn template(root: &Utf8Path, source: &Path) {
        let repo = LocalRepo::create_with_fs(root, "export-sweeps", checked(NoSyncFs))
            .expect("create the store");
        let source = GixSource::open(source).expect("open linear");
        let imported = import(&repo, &source, &ImportRequest::new(MAIN, MAIN, ACTOR));
        assert_eq!(imported.expect("import linear").imported, 5);
        commit_on_main(
            &repo,
            vec![put("notes/one.txt", b"one\n")],
            "Add notes/one.txt\n",
        );
        commit_on_main(
            &repo,
            vec![
                put("notes/one.txt", b"one, again\n"),
                put("notes/two.txt", b"two\n"),
            ],
            "Change notes/one.txt, add notes/two.txt\n\nCo-Authored-By: A Person <a@example.com>\n",
        );
        commit_on_main(
            &repo,
            vec![
                delete("notes/two.txt"),
                put_as("run.sh", LeafKind::Exec, b"#!/bin/sh\necho run\n"),
                put_as("link", LeafKind::Symlink, b"notes/one.txt"),
            ],
            "Remove notes/two.txt; add run.sh and link\n",
        );
    }

    /// The loose object files of the repository at `dir`: those of its fan-out directories, by
    /// their paths under `objects/`.
    fn loose_files(dir: &Path) -> BTreeSet<PathBuf> {
        let objects = dir.join("objects");
        let mut files = BTreeSet::new();
        for fan_out in fs::read_dir(&objects).expect("objects/") {
            let fan_out = fan_out.expect("an entry of objects/");
            if fan_out.file_name().len() != 2 {
                continue;
            }
            for file in fs::read_dir(fan_out.path()).expect("a fan-out directory") {
                let path = file.expect("a loose object").path();
                files.insert(
                    path.strip_prefix(&objects)
                        .expect("under objects/")
                        .to_owned(),
                );
            }
        }
        files
    }

    /// The path under `objects/` of `oid`'s loose file.
    fn loose_name(oid: GitOid) -> PathBuf {
        let hex = oid.to_string();
        Path::new(&hex[..2]).join(&hex[2..])
    }

    /// Every object the commit `tip` reaches in the repository at `dir`, read through the
    /// import's reader, which checks each against its id: by id, its kind and its bytes.
    fn reachable(dir: &Path, tip: GitOid) -> BTreeMap<GitOid, (RawKind, Vec<u8>)> {
        let source = GixSource::open(dir).expect("open the destination");
        let mut objects = BTreeMap::new();
        let mut stack = vec![tip];
        while let Some(oid) = stack.pop() {
            if objects.contains_key(&oid) {
                continue;
            }
            let (kind, bytes) = (source.read(oid))
                .unwrap_or_else(|error| panic!("{oid}, which {tip} reaches: {error}"));
            match kind {
                RawKind::Commit => {
                    let commit = RawCommit::parse(&bytes).expect("a commit");
                    stack.extend(commit.parents.iter().copied());
                    stack.push(commit.tree);
                }
                RawKind::Tree => {
                    let tree = RawTree::parse(&bytes).expect("a tree");
                    stack.extend(tree.entries.iter().map(|entry| entry.oid));
                }
                RawKind::Blob | RawKind::Tag => {}
            }
            objects.insert(oid, (kind, bytes));
        }
        objects
    }

    /// Where a crash sweep's sink ([`Crashing`]) aborts the export.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Crash {
        /// Right before its k-th mutating call, counted from 1.
        Before(u64),
        /// Inside the branch's move, between taking its lock and renaming the lock onto the ref.
        InTheMove,
        /// Nowhere: the export runs to its end.
        Never,
    }

    impl Crash {
        /// The crash as [`CRASH_ENV`] gives it to a child.
        fn env(self) -> String {
            match self {
                Crash::Before(k) => k.to_string(),
                Crash::InTheMove => "move".to_owned(),
                Crash::Never => "never".to_owned(),
            }
        }

        /// What [`Crash::env`] made `value` of.
        fn parse(value: &str) -> Crash {
            match value {
                "move" => Crash::InTheMove,
                "never" => Crash::Never,
                k => Crash::Before(k.parse().expect("a call number")),
            }
        }
    }

    /// A [`GixSink`] that aborts the process where [`Crash`] says, as a `kill -9` there would:
    /// nothing of the sink's runs past that point, and what it wrote before stays. Its reads go
    /// to the sink as they are; its mutating calls are counted.
    struct Crashing {
        /// The sink.
        sink: GixSink,
        /// The repository's git directory, where the lock of a move goes.
        dir: PathBuf,
        /// Where it crashes.
        crash: Crash,
        /// How many mutating calls it was made.
        calls: u64,
    }

    impl Crashing {
        /// The repository at `dir`, crashing where `crash` says.
        fn open(dir: &Path, crash: Crash) -> Crashing {
            Crashing {
                sink: gix(dir),
                dir: dir.to_owned(),
                crash,
                calls: 0,
            }
        }

        /// Counts a mutating call, and aborts the process if the sweep crashes right before it.
        fn call(&mut self) {
            self.calls += 1;
            if self.crash == Crash::Before(self.calls) {
                std::process::abort();
            }
        }
    }

    impl GitSink for Crashing {
        fn object_format(&self) -> ObjectFormat {
            self.sink.object_format()
        }

        fn stored(&self, oid: GitOid) -> Result<Stored, SinkError> {
            self.sink.stored(oid)
        }

        fn read(&self, oid: GitOid) -> Result<(RawKind, Vec<u8>), SinkError> {
            self.sink.read(oid)
        }

        fn write_object(&mut self, kind: RawKind, bytes: &[u8]) -> Result<GitOid, SinkError> {
            self.call();
            self.sink.write_object(kind, bytes)
        }

        fn sync(&mut self, oids: &[GitOid]) -> Result<(), SinkError> {
            self.call();
            self.sink.sync(oids)
        }

        fn branch(&self, name: &str) -> Result<Option<GitOid>, SinkError> {
            self.sink.branch(name)
        }

        fn checked_out(&self) -> Result<Option<String>, SinkError> {
            self.sink.checked_out()
        }

        fn cas_branch(
            &mut self,
            name: &str,
            expected: Option<GitOid>,
            new: GitOid,
        ) -> Result<BranchUpdate, SinkError> {
            self.call();
            if self.crash == Crash::InTheMove {
                // What `GixSink`'s move does before its rename, git's lock protocol: the lock
                // created exclusively, the new value written to it and synced.
                let lock = self.dir.join(format!("refs/heads/{name}.lock"));
                let mut file = (OpenOptions::new().write(true).create_new(true))
                    .open(&lock)
                    .expect("take the branch's lock");
                (file.write_all(format!("{new}\n").as_bytes())).expect("write the lock");
                file.sync_all().expect("sync the lock");
                std::process::abort();
            }
            self.sink.cas_branch(name, expected, new)
        }
    }

    /// When this process is a child the crash sweep spawned ([`STORE_ENV`] set): exports `main`
    /// from the store its parent named into the repository it named, through a [`Crashing`] sink
    /// that crashes where its parent said, reports how many commits it wrote and how many
    /// mutating calls it made of its sink ([`REPORT`]: when it does not crash), and returns
    /// `true`. An export that fails fails the child, and with it the sweep.
    fn export_as_child() -> bool {
        let Some(root) = std::env::var_os(STORE_ENV) else {
            return false;
        };
        let root = utf8(PathBuf::from(root));
        let destination = std::env::var_os(DESTINATION_ENV).expect("the destination");
        let crash = Crash::parse(&std::env::var(CRASH_ENV).expect("where to crash"));
        let repo = exporter(&root);
        let mut sink = Crashing::open(Path::new(&destination), crash);
        let report = (export(&repo, &mut sink, &request_main()))
            .unwrap_or_else(|error| panic!("the child's export: {error}"));
        println!("{REPORT}exported {} {}", report.commits_written, sink.calls);
        true
    }

    /// What a child reported: the commits its export wrote and the mutating calls it made. A
    /// child that failed, or reported nothing, fails the test with what it printed.
    fn reported(output: &Output) -> (u64, u64) {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let line =
            (stdout.lines()).find_map(|line| line.strip_prefix(REPORT)?.strip_prefix("exported "));
        let numbers: Option<Vec<u64>> =
            line.and_then(|line| line.split(' ').map(|n| n.parse().ok()).collect());
        match numbers.as_deref() {
            Some(&[commits, calls]) if output.status.success() => (commits, calls),
            _ => panic!(
                "a child that did not report its export: {:?}\n--- stdout ---\n{stdout}\n\
                 --- stderr ---\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr),
            ),
        }
    }

    /// What an uninterrupted export did, and left.
    struct Whole {
        /// Its report.
        report: ExportReport,
        /// The commits it wrote: the export oids of those of the head's history the destination
        /// lacked.
        commits: BTreeSet<GitOid>,
        /// The destination's loose object files after it ([`loose_files`]).
        files: BTreeSet<PathBuf>,
        /// Every object the branch reaches after it ([`reachable`]).
        objects: BTreeMap<GitOid, (RawKind, Vec<u8>)>,
        /// What the store holds after it.
        state: State,
    }

    /// What a sweep stands on: the fixture, the store and the destination every run copies, and
    /// what the uninterrupted export makes of them.
    struct Sweep {
        /// The test whose children export ([`export_as_child`]).
        test: &'static str,
        /// What the export writes into.
        onto: Onto,
        /// `linear`, the store's source, whose repository is the [`Onto::Fixture`] destination.
        _fixture: GitFixture,
        /// The empty repository that is the [`Onto::Empty`] destination.
        _empty: EmptyRepository,
        /// The temp dir that holds `store`.
        _store_dir: TempDir,
        /// The store every run copies ([`template`]).
        store: camino::Utf8PathBuf,
        /// The repository every run copies.
        destination: PathBuf,
        /// What the destination's branch names before the export.
        old: Option<GitOid>,
        /// The destination's loose object files before the export.
        files: BTreeSet<PathBuf>,
        /// How many mutating calls the uninterrupted export makes of its sink.
        calls: u64,
        /// What the uninterrupted export did, and left.
        whole: Whole,
    }

    impl Sweep {
        /// The ground of a sweep of `test` onto `onto`: the store, and the dry run, an
        /// uninterrupted export into a copy of the destination through a [`Crashing`] sink that
        /// never crashes, which counts its calls. It must write the three native commits (and
        /// `linear`'s five into an empty repository), sync once and move the branch once.
        fn new(test: &'static str, onto: Onto) -> Sweep {
            let fixture = git::fixture("linear");
            let source = fixture.repo.clone().expect("linear holds a repository");
            let empty = git::empty_repository();
            let store_dir = tempfile::tempdir().expect("a temp dir");
            let store = utf8(store_dir.path().join("store"));
            template(&store, &source);
            let destination = match onto {
                Onto::Empty => empty.dir.clone(),
                Onto::Fixture => source.clone(),
            };
            let old = gix(&destination).branch(MAIN).expect("read the branch");
            let files = loose_files(&destination);

            let (_dir, root, copied) = copy_both(&store, &destination);
            let repo = exporter(&root);
            let mut sink = Crashing::open(&copied, Crash::Never);
            let report = export(&repo, &mut sink, &request_main()).expect("the dry run");
            let calls = sink.calls;
            let new_commits = match onto {
                Onto::Empty => 8,
                Onto::Fixture => 3,
            };
            assert_eq!(
                report.commits_written, new_commits,
                "the dry run onto {onto:?}"
            );
            assert_eq!(
                (
                    report.old,
                    report.branch_moved,
                    report.objects_checked,
                    report.rewritten
                ),
                (old, true, 0, 0),
                "the dry run onto {onto:?}"
            );
            assert_eq!(
                calls,
                report.objects_written + 2,
                "the dry run onto {onto:?}: a write per object, one sync, one move"
            );
            let objects = reachable(&copied, report.head_oid);
            let files_after = loose_files(&copied);
            let commits: BTreeSet<GitOid> = (objects.iter())
                .filter(|(oid, (kind, _))| {
                    *kind == RawKind::Commit && files_after.contains(&loose_name(**oid))
                })
                .map(|(oid, _)| *oid)
                .collect();
            assert_eq!(commits.len(), usize::try_from(new_commits).expect("fits"));
            let whole = Whole {
                commits,
                files: files_after,
                objects,
                state: State::of(&root),
                report,
            };
            Sweep {
                test,
                onto,
                _fixture: fixture,
                _empty: empty,
                _store_dir: store_dir,
                store,
                destination,
                old,
                files,
                calls,
                whole,
            }
        }

        /// The crash points of a sweep that takes every `every`-th call: those calls, and the
        /// last two (the sync and the move) and past them, then the move's inside.
        fn crashes(&self, every: u64) -> Vec<Crash> {
            let calls = self.calls;
            let before = (1..=calls).filter(|&k| k % every == 0 || k + 1 >= calls);
            (before.map(Crash::Before))
                .chain([Crash::InTheMove, Crash::Never])
                .collect()
        }
    }

    /// Copies of the store at `store` and of the repository at `destination`, in a temp dir of
    /// their own: the temp dir, the store's root and the repository's directory.
    fn copy_both(store: &Utf8Path, destination: &Path) -> (TempDir, camino::Utf8PathBuf, PathBuf) {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = utf8(dir.path().join("store"));
        copy_dir(store.as_std_path(), root.as_std_path());
        let copied = dir.path().join("repo.git");
        copy_dir(destination, &copied);
        (dir, root, copied)
    }

    /// What one run of the crash sweep left, before the export ran again.
    #[derive(Clone, Copy, Debug)]
    struct Crashed {
        /// Where it crashed.
        crash: Crash,
        /// How many of the uninterrupted export's loose objects it left.
        objects: usize,
        /// Whether the store held the native commits' external id entries.
        recorded: bool,
        /// Whether it left the branch's lock.
        locked: bool,
        /// Whether the branch named the head's export.
        moved: bool,
    }

    /// One run of the crash sweep: the export in a child on copies of the sweep's store and
    /// destination, crashing as `crash` says; then what it left checked, and the export run again
    /// ([`resume`]).
    fn crash_at(sweep: &Sweep, crash: Crash) -> Crashed {
        let (_dir, root, destination) = copy_both(&sweep.store, &sweep.destination);
        let output = run(this_test(&test_name!(sweep.test))
            .env(STORE_ENV, &root)
            .env(DESTINATION_ENV, &destination)
            .env(CRASH_ENV, crash.env()));
        let what = format!("onto {:?}, {crash:?} of {} calls", sweep.onto, sweep.calls);
        if crash == Crash::Never {
            let whole = (sweep.whole.report.commits_written, sweep.calls);
            assert_eq!(reported(&output), whole, "{what}");
        } else {
            assert_eq!(
                output.status.signal(),
                Some(SIGABRT),
                "{what}: {:?}\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let head = sweep.whole.report.head_oid;
        let branch = gix(&destination).branch(MAIN).expect("read the branch");
        let moved = branch == Some(head);
        assert!(
            moved || branch == sweep.old,
            "{what}: the branch names {branch:?}"
        );
        if moved {
            assert_eq!(reachable(&destination, head), sweep.whole.objects, "{what}");
        }
        let left: BTreeSet<PathBuf> = (loose_files(&destination).difference(&sweep.files))
            .cloned()
            .collect();
        let state = State::of(&root);
        let crashed = Crashed {
            crash,
            objects: left.len(),
            recorded: state.entries == sweep.whole.state.entries,
            locked: destination.join("refs/heads/main.lock").exists(),
            moved,
        };
        resume(sweep, &root, &destination, &left, &what);
        crashed
    }

    /// The export run again on the store at `root` and the repository at `destination`, as a run
    /// of `sweep` left them, `left` the loose objects it wrote (`what` names the run): it must
    /// take exactly what that run left out, and end with the branch, the objects and the store's
    /// external id entries the uninterrupted export ended with. A lock the run left is reported,
    /// and the branch left as it is, until it is removed.
    fn resume(
        sweep: &Sweep,
        root: &Utf8Path,
        destination: &Path,
        left: &BTreeSet<PathBuf>,
        what: &str,
    ) {
        let repo = exporter(root);
        let head = sweep.whole.report.head_oid;
        let moved = gix(destination).branch(MAIN).expect("read the branch") == Some(head);
        let lock = destination.join("refs/heads/main.lock");
        if lock.exists() {
            let error = export(&repo, &mut gix(destination), &request_main())
                .expect_err("the lock the crash left stops the export");
            assert!(
                matches!(&error, ExportError::Sink(SinkError::RefLocked { lock })
                    if lock == "refs/heads/main.lock"),
                "{what}: {error}"
            );
            assert_eq!(gix(destination).branch(MAIN), Ok(sweep.old), "{what}");
            // As the error's hint says: no git process runs.
            fs::remove_file(&lock).expect("remove the lock");
        }
        let report = (export(&repo, &mut gix(destination), &request_main()))
            .unwrap_or_else(|error| panic!("{what}: the export again: {error}"));
        let whole = &sweep.whole.report;
        assert_eq!((report.head, report.head_oid), (whole.head, head), "{what}");
        let took = (
            report.branch_moved,
            report.commits_written,
            report.objects_written,
            report.objects_checked,
        );
        let expected = if moved {
            (false, 0, 0, 0)
        } else {
            let objects = u64::try_from(left.len()).expect("fits");
            let commits = (sweep.whole.commits.iter())
                .filter(|commit| left.contains(&loose_name(**commit)))
                .count();
            let commits = u64::try_from(commits).expect("fits");
            (
                true,
                whole.commits_written - commits,
                whole.objects_written - objects,
                whole.objects_checked + objects,
            )
        };
        assert_eq!(
            took,
            expected,
            "{what}: it left {} loose objects",
            left.len()
        );
        assert_eq!(gix(destination).branch(MAIN), Ok(Some(head)), "{what}");
        assert!(!lock.exists(), "{what}: a lock left");
        assert_eq!(loose_files(destination), sweep.whole.files, "{what}");
        assert_eq!(reachable(destination, head), sweep.whole.objects, "{what}");
        assert_eq!(State::of(root), sweep.whole.state, "{what}");
        let verified = repo.verify(true).expect("verify");
        assert_eq!(verified.problems, [], "{what}: after the export again");
    }

    /// The crash sweep of `test` onto each destination, at every `every`-th call of its sink and
    /// the last two, inside the move and past it. What the crashes left must hold a crash among
    /// the object writes, one with every object written and nothing recorded (at the sync), one
    /// with the external id entries recorded and the branch not moved (at the move), the lock a
    /// crash in the move leaves, and the branch moved: else the sweep missed what it is for.
    /// Prints how many runs it made, and how many crashes fell in each of those windows.
    fn crash_sweep(test: &'static str, every: u64) {
        for onto in [Onto::Empty, Onto::Fixture] {
            let sweep = Sweep::new(test, onto);
            let points = sweep.crashes(every);
            let crashed = runs(&points, |&crash| crash_at(&sweep, crash));
            let written = sweep.whole.files.len() - sweep.files.len();
            let windows: [Class<'_, Crashed>; 5] = [
                ("among the object writes", &|c| {
                    0 < c.objects && c.objects < written
                }),
                ("with every object written and nothing recorded", &|c| {
                    c.objects == written && !c.recorded && !c.moved
                }),
                ("with the entries recorded and the branch not moved", &|c| {
                    c.recorded && !c.locked && !c.moved
                }),
                ("inside the move, its lock left", &|c| {
                    c.crash == Crash::InTheMove && c.locked
                }),
                ("past the move", &|c| c.crash == Crash::Never && c.moved),
            ];
            let counts: Vec<String> = (windows.iter())
                .map(|(what, inside)| {
                    let count = crashed.iter().filter(|&&c| inside(c)).count();
                    assert!(count > 0, "onto {onto:?}, no crash {what}: {crashed:?}");
                    format!("{count} {what}")
                })
                .collect();
            println!(
                "export crash sweep onto {onto:?}: {} runs over {} calls; {}",
                points.len(),
                sweep.calls,
                counts.join(", ")
            );
        }
    }

    /// An export crashed right before any call that writes its destination (every fifth here,
    /// with the sync, the move, the move's inside and past it; every one in the ignored variant
    /// below) leaves the branch where it was, and the same export run again takes exactly what the
    /// crash left out, ending where an uninterrupted export ends.
    #[test]
    fn export_crashing_at_any_sink_call_resumes() {
        if export_as_child() {
            return;
        }
        crash_sweep("export_crashing_at_any_sink_call_resumes", SAMPLED);
    }

    /// [`export_crashing_at_any_sink_call_resumes`] at every call.
    #[test]
    #[ignore = "slow, ~15 s: an export crashed at each of its 54 git-side calls"]
    fn export_crashing_at_every_sink_call_resumes() {
        if export_as_child() {
            return;
        }
        crash_sweep("export_crashing_at_every_sink_call_resumes", 1);
    }

    /// The sink a failure sweep's runs start from: empty, or holding `linear`'s objects packed,
    /// its `main` at `linear`'s tip.
    struct Start {
        /// What the export writes into.
        onto: Onto,
        /// `linear`'s objects, each its kind and its bytes: what the [`Onto::Fixture`] sink holds
        /// packed.
        packed: Vec<(RawKind, Vec<u8>)>,
        /// What `main` names in `linear`'s repository.
        tip: GitOid,
    }

    impl Start {
        /// A fresh sink to start from.
        fn sink(&self) -> MemSink {
            match self.onto {
                Onto::Empty => MemSink::default(),
                Onto::Fixture => {
                    let mut sink = MemSink::with_packed(self.packed.iter().cloned());
                    sink.set_branch(MAIN, self.tip);
                    sink
                }
            }
        }

        /// What the branch names before the export.
        fn old(&self) -> Option<GitOid> {
            match self.onto {
                Onto::Empty => None,
                Onto::Fixture => Some(self.tip),
            }
        }
    }

    /// What a failure sweep stands on: the store every run copies, the sink every run starts from,
    /// and what the uninterrupted export makes of them.
    struct MemSweep {
        /// `linear`, the store's source.
        _fixture: GitFixture,
        /// The temp dir that holds `store`.
        _store_dir: TempDir,
        /// The store every run copies ([`template`]).
        store: camino::Utf8PathBuf,
        /// The sink every run starts from.
        start: Start,
        /// How many mutating calls the uninterrupted export makes of its sink.
        calls: usize,
        /// What the uninterrupted export did.
        report: ExportReport,
        /// What a power loss after its last call leaves of its sink, keeping no object it did not
        /// sync.
        image: crate::support::MemImage,
        /// What the store holds after it.
        state: State,
    }

    impl MemSweep {
        /// The ground of a failure sweep onto `onto`: the store, and the uninterrupted export into
        /// a fresh sink, on a copy of it.
        fn new(onto: Onto) -> MemSweep {
            let fixture = git::fixture("linear");
            let source = fixture.repo.clone().expect("linear holds a repository");
            let store_dir = tempfile::tempdir().expect("a temp dir");
            let store = utf8(store_dir.path().join("store"));
            template(&store, &source);
            let manifest = fixture.manifest.as_ref().expect("linear's manifest");
            let tip: GitOid = manifest.refs["refs/heads/main"].parse().expect("an oid");
            let reader = GixSource::open(&source).expect("open linear");
            let oids = (manifest.commits.iter().map(|commit| commit.oid.as_str()))
                .chain(manifest.trees.keys().map(String::as_str))
                .chain(manifest.blobs.keys().map(String::as_str));
            let packed = oids
                .map(|oid| {
                    reader
                        .read(oid.parse().expect("an oid"))
                        .expect("read linear")
                })
                .collect();
            let start = Start { onto, packed, tip };

            let (_dir, root) = super::copy(&store);
            let mut sink = start.sink();
            let report = export(&exporter(&root), &mut sink, &request_main()).expect("the dry run");
            assert!(report.branch_moved, "the dry run onto {onto:?}");
            let calls = sink.log().len();
            MemSweep {
                _fixture: fixture,
                _store_dir: store_dir,
                store,
                start,
                calls,
                report,
                image: sink.image(calls, Keep::None),
                state: State::of(&root),
            }
        }
    }

    /// One run of the failure sweep: the export into the sweep's sink, its `k`-th mutating call
    /// failing. It must stop at that call with the sink's error, make no call after it, leave the
    /// branch where it was, and have recorded the native commits' external id entries exactly when
    /// the call that failed is the move (they are recorded once the objects are durable, before
    /// the branch moves); past the last call, it succeeds as the uninterrupted export does. Then
    /// the same export runs again on the same sink, its failure spent: it must write what the
    /// first did not, read back what it did, and end with the durable objects, the branch and the
    /// entries of the uninterrupted export; and no power loss after any call of the two exports
    /// leaves the branch naming an object it took ([`reaches_only_kept`]).
    fn fail_at(sweep: &MemSweep, k: usize) {
        let what = format!(
            "onto {:?}, call {k} of {} failing",
            sweep.start.onto, sweep.calls
        );
        let (_dir, root) = super::copy(&sweep.store);
        let repo = exporter(&root);
        let mut sink = sweep.start.sink();
        sink.fail_at(k);
        let first = export(&repo, &mut sink, &request_main());
        let failed = k <= sweep.calls;
        match &first {
            Err(ExportError::Sink(SinkError::Write { .. })) if failed => {}
            Ok(report) if !failed => {
                assert_eq!(
                    report.commits_written, sweep.report.commits_written,
                    "{what}"
                );
            }
            other => panic!("{what}: {other:?}"),
        }
        let logged = sink.log().len();
        assert_eq!(
            logged,
            k.min(sweep.calls + 1) - 1,
            "{what}: a call after the failure"
        );
        let branch = sink.branch(MAIN).expect("the branch");
        let expected = if failed {
            sweep.start.old()
        } else {
            Some(sweep.report.head_oid)
        };
        assert_eq!(branch, expected, "{what}");
        let recorded = State::of(&root).entries == sweep.state.entries;
        assert_eq!(
            recorded,
            k >= sweep.calls,
            "{what}: the entries, recorded before the move"
        );

        let report = (export(&repo, &mut sink, &request_main()))
            .unwrap_or_else(|error| panic!("{what}: the export again: {error}"));
        let written = u64::try_from(logged.min(sweep.calls - 2)).expect("fits");
        let whole = &sweep.report;
        let took = (
            report.branch_moved,
            report.objects_written,
            report.objects_checked,
        );
        let expected = if failed {
            (
                true,
                whole.objects_written - written,
                whole.objects_checked + written,
            )
        } else {
            (false, 0, 0)
        };
        assert_eq!(took, expected, "{what}: the export again");
        let end = sink.log().len();
        assert_eq!(
            sink.image(end, Keep::None),
            sweep.image,
            "{what}: the durable objects"
        );
        assert_eq!(State::of(&root), sweep.state, "{what}: the store");
        let keeps = [Keep::None, Keep::All]
            .into_iter()
            .chain((2..=9).map(Keep::Seeded));
        for keep in keeps {
            for after in 0..=end {
                if let Err(why) = reaches_only_kept(&sink.image(after, keep), MAIN) {
                    panic!("{what}: after {after} of {end} calls, keeping {keep:?}: {why}");
                }
            }
        }
    }

    /// An export whose destination fails any of its mutating calls stops there, with that error,
    /// the branch where it was; the same export run again ends where an uninterrupted export ends,
    /// and the branch never names an object a power loss can take, before or after the failure.
    /// Every call, onto both sinks: they live in memory.
    #[test]
    fn export_failing_at_any_sink_call_stops_and_resumes() {
        for onto in [Onto::Empty, Onto::Fixture] {
            let sweep = MemSweep::new(onto);
            let ks: Vec<usize> = (1..=sweep.calls + 1).collect();
            runs(&ks, |&k| fail_at(&sweep, k));
            println!(
                "export failure sweep onto {onto:?}: {} runs over {} calls",
                ks.len(),
                sweep.calls
            );
        }
    }
}
