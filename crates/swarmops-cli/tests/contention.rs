//! 32 agents committing at once through the real `swarmops` binary: what a commit costs when
//! every process takes its turn on the store's `meta/lock` (storage spec, risk 1: 32 processes
//! × 1,000 commits on ext4). The store-level half of the measurement, 32 processes of bare
//! `MetaStore` transactions, is `swarmops-store`'s `tests/contention.rs`.
//!
//! Ignored, since it runs minutes of fsync'd commits: 8–9 minutes with the kernel-queue lock
//! on the loaded host that docs/perf.md "M1 (CLI level)" records. `just check-slow` runs it
//! unoptimized (one contention test at a time: `.config/nextest.toml`); `just perf` runs it
//! optimized and prints only its summary, as docs/perf.md cites it: `cargo nextest run
//! --locked --release -p swarmops-cli --run-ignored ignored-only --test all -E
//! 'test(/^contention::/)' --no-capture`.
//! [`COMMITS_VAR`] sets how many commits each child makes, for a quick check of the harness.
//!
//! The parent `init`s a store in a fresh temporary directory under `SWARMOPS_BENCH_DIR`
//! (default `~/.local/state/swarmops-bench`, on the native filesystem, as every measurement in
//! docs/perf.md), commits a base changeset and points main at it. It checks first that the
//! harness sees a `Busy` at all: one commit while the test itself holds `meta/lock` must fail
//! with `E_BUSY` (exit 4) once the store's 10 s lock timeout has passed.
//!
//! Then it runs this test binary again as [`CHILDREN`] children (selected by [`WORK_DIR`]),
//! each writing its stdout and stderr to files in the work directory. Each child says it is
//! ready and waits until the parent, once all are, says go; then it runs `swarmops commit`
//! [`COMMITS`] times in a row (or as [`COMMITS_VAR`] says), as agent `contention/agent-<n>`.
//! Each commit puts one small file, `agents/<n>.txt`, whose content names the commit, on
//! `--parent` the child's own previous changeset (the base for its first), so that no two
//! children's commits depend on each other. A commit that fails with `E_BUSY` (exit 4,
//! retryable) is counted and run again; anything else fails the child. The child prints one
//! report line: each commit's `meta.elapsed_ms` and its wall time from spawn to exit, the
//! `meta.elapsed_ms` of each `E_BUSY`, its total time from go to its last exit, and its last
//! changeset.
//!
//! The parent prints the summary, every line starting [`SUMMARY`]: the 1-minute load average
//! before, during and after the run, throughput, per-commit latency (nearest-rank
//! percentiles), `E_BUSY`s, the children's total times and the spread between the slowest and
//! the fastest; then what [`SOLO`] uncontended commits cost on the final store (and so roughly
//! how long the contended commits waited for the lock), and what `verify --deep` and
//! `log -n 5` (from main, moved to child 0's last changeset) return and cost. Only then does
//! it assert: every `E_BUSY` after the lock timeout, none at all, every commit made (as `info`
//! counts them), `verify --deep` clean, and the log as committed.
//!
//! The 0-`E_BUSY` assertion assumes a disk no more than about a third slower than this host's:
//! with the kernel-queue lock the longest wait measured here was about 500 lock passes (7.9 s
//! at 15.7 ms a pass, docs/perf.md "After 7c"), and a third more per pass takes that past the
//! 10 s timeout.
//!
//! Nothing in it waits forever, whatever the lock or the binary does. Every `swarmops` run goes
//! through the support runner, which kills one still running past its deadline and fails with
//! what it printed. A child gives up at its [`MAX_BUSY_IN_A_ROW`]th `E_BUSY` in a row, and past
//! [`CHILD_DEADLINE`]; the parent fails as soon as a child fails, and waits for the children
//! only until [`PARENT_DEADLINE`]; and whatever ends the test early, [`Children`] kills and
//! reaps every child still running. [`a_hung_child_is_killed_and_reported`] checks the parent's
//! deadline with a child told ([`HANG`]) to sleep past it.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use swarmops_store::{Layout, MetaLock};
use swarmops_testkit::test_name;

use crate::support;

/// How many children commit at once.
const CHILDREN: usize = 32;

/// How many commits each child makes unless [`COMMITS_VAR`] says otherwise: the storage
/// spec's scenario (risk 1).
const COMMITS: usize = 1_000;

/// Set to give each child another number of commits than [`COMMITS`].
const COMMITS_VAR: &str = "SWARMOPS_TEST_CLI_CONTENTION_COMMITS";

/// The measurement's name, which its children are run with.
const TEST: &str = "thirty_two_processes_thousand_commits_each";

/// The name of the test of the harness's deadline, which its children are run with.
const HUNG_TEST: &str = "a_hung_child_is_killed_and_reported";

/// Set only in the children: the work directory, which holds the store (`store/`), the
/// barrier files (`ready-<n>`, `go`), each child's change list (`changes-<n>.json`) and its
/// output (`stdout-<n>`, `stderr-<n>`).
const WORK_DIR: &str = "SWARMOPS_TEST_CLI_CONTENTION_DIR";

/// Set with [`WORK_DIR`]: the child's number, from 0.
const CHILD_NUMBER: &str = "SWARMOPS_TEST_CLI_CONTENTION_CHILD";

/// Set with [`WORK_DIR`]: the base changeset, every child's first parent.
const BASE: &str = "SWARMOPS_TEST_CLI_CONTENTION_BASE";

/// Set only by [`a_hung_child_is_killed_and_reported`]: the number of the child that sleeps
/// past every deadline after the go, as a hung child would.
const HANG: &str = "SWARMOPS_TEST_CLI_CONTENTION_HANG";

/// How a child's report line starts, among the test harness's own output.
const REPORT: &str = "contention-child: ";

/// How every line of the summary starts, so that `just perf` can pick them out.
const SUMMARY: &str = "contention: ";

/// How many uncontended commits the parent times once the children are done.
const SOLO: usize = 100;

/// The store's lock timeout (`MetaStore`'s default), after which a writer gets `E_BUSY`.
const LOCK_TIMEOUT_MS: u64 = 10_000;

/// At how many `E_BUSY` in a row (the commit run again after each) a child gives up: 5
/// minutes at the 10 s lock timeout, as the store-level test does.
const MAX_BUSY_IN_A_ROW: u64 = 30;

/// How long one `swarmops` run may take before the support runner kills it.
const RUN_TIMEOUT: Duration = support::RUN_TIMEOUT;

/// How long after the go a child may still start a commit: well past the longest whole run
/// measured on this host (14.8 minutes, with the polling lock on a loaded disk).
const CHILD_DEADLINE: Duration = Duration::from_mins(60);

/// How long after the go the parent waits for the children before it kills them: a child's
/// deadline, one more commit that meets every `E_BUSY` it may at 10 s each and then takes its
/// whole run deadline, and a minute.
const PARENT_DEADLINE: Duration = CHILD_DEADLINE.saturating_add(Duration::from_secs(
    MAX_BUSY_IN_A_ROW * 10 + RUN_TIMEOUT.as_secs() + 60,
));

/// How long a process waits at the barrier before it gives up.
const BARRIER_TIMEOUT: Duration = Duration::from_secs(120);

/// How often the parent samples the load average while the children run.
const LOAD_EVERY: Duration = Duration::from_secs(5);

#[test]
#[ignore = "measurement, minutes of fsync'd commits: 32 processes committing through the binary"]
fn thirty_two_processes_thousand_commits_each() {
    if let Some(env) = child_env() {
        child(&env);
        return;
    }
    let started = Instant::now();
    let commits = commits_per_child();
    let work = tempfile::tempdir_in(base_dir()).expect("create the work dir");
    let store = store_path(work.path());
    let base = create_store(work.path());
    let control = busy_control(work.path(), &base);

    let run = run_children(work.path(), &base);
    let busy = print_contention(&run, commits, control);
    let changesets = support::run_within(&["--repo", &store, "info"], &[], RUN_TIMEOUT);
    print_solo(work.path(), &base, &run);
    let verify = support::run_within(&["--repo", &store, "verify", "--deep"], &[], RUN_TIMEOUT);
    print_verify(&verify);
    let log = log_from_child_zero(&store, &base, &run);
    println!(
        "{SUMMARY}the whole test took {:.1} s",
        started.elapsed().as_secs_f64()
    );

    // Every E_BUSY must have come after the lock timeout: one sooner is a fault, not a wait.
    let early: Vec<u64> = run
        .reports
        .iter()
        .flatten()
        .flat_map(|report| report.busy_ms.iter().copied())
        .filter(|&ms| ms < LOCK_TIMEOUT_MS)
        .collect();
    assert!(
        early.is_empty(),
        "E_BUSY before the lock timeout: {early:?} ms"
    );
    let missing: Vec<&String> = run
        .reports
        .iter()
        .filter_map(|r| r.as_ref().err())
        .collect();
    assert!(
        missing.is_empty(),
        "children without a report: {missing:#?}"
    );
    assert_eq!(
        busy,
        0,
        "E_BUSY per child: {:?}",
        run.reports
            .iter()
            .flatten()
            .map(|report| report.busy_ms.len())
            .collect::<Vec<_>>()
    );
    for report in run.reports.iter().flatten() {
        assert_eq!(report.elapsed_ms.len(), commits, "child {}", report.child);
    }
    let expected = 1 + CHILDREN * commits;
    assert_eq!(
        changesets.ok()["changesets"].as_u64(),
        u64::try_from(expected).ok(),
        "{changesets}"
    );
    assert_eq!(verify.exit(), 0, "verify --deep: {verify}");
    assert_eq!(verify.ok()["problems"], json!([]), "{verify}");
    check_log(&log, &run, commits);
}

/// The harness's own deadline: of two children, the one told to hang after the go is killed
/// at the parent's deadline and named in the failure, every child is reaped, and the other's
/// report stays.
#[test]
fn a_hung_child_is_killed_and_reported() {
    if let Some(env) = child_env() {
        child(&env);
        return;
    }
    let work = tempfile::tempdir().expect("create the work dir");
    let base = create_store(work.path());
    let mut children = spawn_children(
        work.path(),
        &base,
        HUNG_TEST,
        2,
        &[(COMMITS_VAR, "1"), (HANG, "1")],
    );
    wait_until_ready(work.path(), &mut children);
    fs::write(work.path().join("go"), b"").expect("say go");
    let pids = children.pids();
    // Child 0 makes its one commit and exits; child 1 sleeps on.
    let done = Instant::now() + RUN_TIMEOUT;
    while children.0[0].try_wait().expect("poll child 0").is_none() {
        assert!(Instant::now() < done, "child 0 never finished its commit");
        thread::sleep(Duration::from_millis(10));
    }
    let waited = Instant::now();
    let failure = wait_for_children(
        work.path(),
        &mut children,
        Instant::now() + Duration::from_secs(1),
    )
    .expect_err("child 1 sleeps past the deadline");
    assert!(
        waited.elapsed() < Duration::from_secs(30),
        "the wait ended at the deadline, not at the hung child's end"
    );
    assert!(
        failure.contains("children [1] still running at the parent's deadline"),
        "{failure}"
    );
    for pid in pids {
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "child process {pid} was killed and reaped"
        );
    }
    let zero = report_of(work.path(), 0).expect("child 0's report");
    assert_eq!(zero.elapsed_ms.len(), 1, "{zero:?}");
}

// ---------------------------------------------------------------------------------------------
// The children.
// ---------------------------------------------------------------------------------------------

/// What a child prints for the parent, as one JSON line after [`REPORT`].
#[derive(Debug, Serialize, Deserialize)]
struct Report {
    /// The child's number.
    child: usize,
    /// `meta.elapsed_ms` of each commit made, in order.
    elapsed_ms: Vec<u64>,
    /// The wall time of each commit made, from spawn to exit, in µs.
    wall_us: Vec<u64>,
    /// `meta.elapsed_ms` of each `E_BUSY`.
    busy_ms: Vec<u64>,
    /// From go to the last commit's exit, in µs.
    total_us: u64,
    /// The last changeset committed: the base if none was.
    head: String,
}

/// What a child runs with: its work directory, its number and the base changeset.
struct ChildEnv {
    work: PathBuf,
    number: usize,
    base: String,
}

/// This process's [`ChildEnv`], if it is a child.
fn child_env() -> Option<ChildEnv> {
    let work = std::env::var_os(WORK_DIR)?;
    let number = std::env::var(CHILD_NUMBER)
        .expect("a child has a number")
        .parse()
        .expect("a child's number is a number");
    let base = std::env::var(BASE).expect("a child has a base changeset");
    Some(ChildEnv {
        work: PathBuf::from(work),
        number,
        base,
    })
}

/// A child's run: [`COMMITS`] commits (or as [`COMMITS_VAR`] says), each on the one before,
/// then its report. Anything but a commit or an `E_BUSY` fails it, and so do
/// [`MAX_BUSY_IN_A_ROW`] `E_BUSY`s in a row and a commit not started by [`CHILD_DEADLINE`].
fn child(env: &ChildEnv) {
    let number = env.number;
    let store = store_path(&env.work);
    let changes = env.work.join(format!("changes-{number}.json"));
    let agent = format!("contention/agent-{number:02}");
    let path = format!("agents/{number:02}.txt");
    let commits = commits_per_child();
    let mut report = Report {
        child: number,
        elapsed_ms: Vec::with_capacity(commits),
        wall_us: Vec::with_capacity(commits),
        busy_ms: Vec::new(),
        total_us: 0,
        head: env.base.clone(),
    };
    fs::write(env.work.join(format!("ready-{number}")), b"").expect("say ready");
    wait_for(&env.work.join("go"));
    let started = Instant::now();
    let deadline = started + CHILD_DEADLINE;
    if std::env::var(HANG).ok() == Some(number.to_string()) {
        // A test of the harness: hang past every deadline, as a stuck child would.
        thread::sleep(PARENT_DEADLINE);
        return;
    }
    let mut in_a_row = 0;
    while report.elapsed_ms.len() < commits {
        let j = report.elapsed_ms.len();
        assert!(
            Instant::now() < deadline,
            "child {number}: commit {j} not started {CHILD_DEADLINE:?} after the go"
        );
        let what = format!("agent {number} commit {j}");
        let parent = report.head.clone();
        let (run, wall) = commit(&store, &changes, &agent, Some(&parent), &path, &what);
        match outcome(&run, Some(&parent), &format!("child {number}, commit {j}")) {
            Outcome::Committed { id, elapsed_ms } => {
                report.elapsed_ms.push(elapsed_ms);
                report.wall_us.push(micros(wall));
                report.head = id;
                in_a_row = 0;
            }
            Outcome::Busy { elapsed_ms } => {
                report.busy_ms.push(elapsed_ms);
                in_a_row += 1;
                assert!(
                    in_a_row < MAX_BUSY_IN_A_ROW,
                    "child {number}, commit {j}: E_BUSY {in_a_row} times in a row"
                );
            }
        }
    }
    report.total_us = micros(started.elapsed());
    let line = serde_json::to_string(&report).expect("a report");
    println!("{REPORT}{line}");
}

/// Waits until `path` exists.
fn wait_for(path: &Path) {
    let deadline = Instant::now() + BARRIER_TIMEOUT;
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        thread::sleep(Duration::from_millis(1));
    }
}

// ---------------------------------------------------------------------------------------------
// Running the binary.
// ---------------------------------------------------------------------------------------------

/// The store in the work directory `work`.
fn store_path(work: &Path) -> String {
    work.join("store")
        .to_str()
        .expect("the work dir's path is UTF-8")
        .to_owned()
}

/// Runs the binary with `args` through the support runner (no `SWARMOPS_*` variable set, a
/// deadline of [`RUN_TIMEOUT`]); the run, and how long it took from spawn to exit.
fn timed(args: &[&str]) -> (support::Run, Duration) {
    let started = Instant::now();
    let run = support::run_within(args, &[], RUN_TIMEOUT);
    (run, started.elapsed())
}

/// Commits one small file, `path`, as `agent` on `parent` (with none, on main if it exists)
/// to `store`, through the change list file `changes`, which it rewrites: the file's content
/// and the message are both `what`.
fn commit(
    store: &str,
    changes: &Path,
    agent: &str,
    parent: Option<&str>,
    path: &str,
    what: &str,
) -> (support::Run, Duration) {
    let list = json!([{"path": path, "op": "put", "content": {"inline": format!("{what}\n")}}]);
    fs::write(changes, list.to_string()).expect("write a change list");
    let changes = changes.to_str().expect("the work dir's path is UTF-8");
    let mut args = vec![
        "--repo",
        store,
        "--as",
        agent,
        "commit",
        "-m",
        what,
        "--changes",
        changes,
    ];
    if let Some(parent) = parent {
        args.extend(["--parent", parent]);
    }
    timed(&args)
}

/// What a commit came to.
enum Outcome {
    /// It made changeset `id`, on the parent it was given.
    Committed { id: String, elapsed_ms: u64 },
    /// `E_BUSY` (exit 4, retryable): the store's lock stayed taken for the whole timeout.
    Busy { elapsed_ms: u64 },
}

/// What `run`, a commit on `parent` that `what` names, came to. Anything else (another error,
/// another parent, no `meta.elapsed_ms`, an envelope off the contract) fails the test, named
/// after `what`.
fn outcome(run: &support::Run, parent: Option<&str>, what: &str) -> Outcome {
    let envelope = run.envelope();
    let elapsed_ms = elapsed_ms(&envelope, what);
    match run.exit() {
        0 => {
            let result = &envelope["result"];
            assert_eq!(
                result["parent"].as_str(),
                parent,
                "{what}: committed on another parent: {run}"
            );
            let id = result["changeset"]
                .as_str()
                .unwrap_or_else(|| panic!("{what}: no changeset id: {run}"));
            Outcome::Committed {
                id: id.to_owned(),
                elapsed_ms,
            }
        }
        4 if envelope["error"]["code"] == "E_BUSY" => Outcome::Busy { elapsed_ms },
        _ => panic!("{what}: {run}"),
    }
}

/// The `meta.elapsed_ms` of `envelope`, which every envelope carries: its absence fails the
/// test, named after `what`.
fn elapsed_ms(envelope: &Value, what: &str) -> u64 {
    envelope["meta"]["elapsed_ms"]
        .as_u64()
        .unwrap_or_else(|| panic!("{what}: no meta.elapsed_ms in {envelope}"))
}

// ---------------------------------------------------------------------------------------------
// The parent.
// ---------------------------------------------------------------------------------------------

/// Creates the store in `work`, with a base changeset that main names; the base's id.
fn create_store(work: &Path) -> String {
    let store = store_path(work);
    support::run_within(&["init", &store, "--name", "contention"], &[], RUN_TIMEOUT).ok();
    // No main yet, so a commit without a parent is a root commit.
    let changes = work.join("changes-base.json");
    let what = "contention base";
    let (base, _) = commit(
        &store,
        &changes,
        "contention/harness",
        None,
        "README.md",
        what,
    );
    let Outcome::Committed { id, .. } = outcome(&base, None, "the base commit") else {
        panic!("the base commit: {base}");
    };
    let main = [
        "--repo",
        store.as_str(),
        "--as",
        "contention/harness",
        "ref",
        "set",
        "main",
        &id,
        "--create",
    ];
    support::run_within(&main, &[], RUN_TIMEOUT).ok();
    id
}

/// Checks that the harness sees a `Busy`: a commit while this process holds `meta/lock` must
/// get `E_BUSY` (exit 4) after the store's lock timeout. Returns its `meta.elapsed_ms`.
fn busy_control(work: &Path, base: &str) -> u64 {
    let store = store_path(work);
    let layout = Layout::new(Utf8PathBuf::from(store.as_str()));
    let held = MetaLock::acquire(&layout, Duration::from_secs(1)).expect("take meta/lock");
    let changes = work.join("changes-control.json");
    let (run, _) = commit(
        &store,
        &changes,
        "contention/control",
        Some(base),
        "control.txt",
        "the lock is held",
    );
    drop(held);
    let Outcome::Busy { elapsed_ms } = outcome(&run, Some(base), "the control commit") else {
        panic!("a commit against a held lock committed: {run}");
    };
    assert!(elapsed_ms >= LOCK_TIMEOUT_MS, "{run}");
    println!(
        "{SUMMARY}control: a commit while the test held meta/lock got E_BUSY (exit 4) after \
         {elapsed_ms} ms"
    );
    elapsed_ms
}

/// The children, killed and reaped if still running when this drops: whatever ends the test
/// early (a panic in the barrier wait, a failed spawn, a missed deadline), no child outlives
/// it.
struct Children(Vec<Child>);

impl Children {
    /// The children's process ids.
    fn pids(&self) -> Vec<u32> {
        self.0.iter().map(Child::id).collect()
    }

    /// Kills every child still running, and reaps them all.
    fn kill_all(&mut self) {
        for child in &mut self.0 {
            // A child that has exited already is reaped (or was), and is not signalled.
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for Children {
    fn drop(&mut self) {
        self.kill_all();
    }
}

/// The file in `work` that child `number`'s `stream` ("stdout" or "stderr") goes to.
fn output_file(work: &Path, stream: &str, number: usize) -> PathBuf {
    work.join(format!("{stream}-{number}"))
}

/// What child `number` wrote to `stream`.
fn output(work: &Path, stream: &str, number: usize) -> String {
    fs::read_to_string(output_file(work, stream, number))
        .unwrap_or_else(|e| format!("({stream} of child {number} unreadable: {e})"))
}

/// The output of children `numbers`, for a failure message.
fn outputs(work: &Path, numbers: &[usize]) -> String {
    numbers
        .iter()
        .map(|&number| {
            format!(
                "--- child {number} stdout ---\n{}\n--- child {number} stderr ---\n{}",
                output(work, "stdout", number),
                output(work, "stderr", number)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Spawns `count` children of test `test` on the store in `work`, from `base`, with `env` set
/// besides the child's own variables; each writes its stdout and stderr to files there.
fn spawn_children(
    work: &Path,
    base: &str,
    test: &str,
    count: usize,
    env: &[(&str, &str)],
) -> Children {
    let exe = std::env::current_exe().expect("this test binary");
    let mut children = Children(Vec::with_capacity(count));
    for number in 0..count {
        let file = |stream| {
            File::create(output_file(work, stream, number)).expect("create a child's output file")
        };
        let child = Command::new(&exe)
            .args([
                "--exact",
                &test_name!(test),
                "--include-ignored",
                "--nocapture",
            ])
            .env(WORK_DIR, work)
            .env(CHILD_NUMBER, number.to_string())
            .env(BASE, base)
            .envs(env.iter().copied())
            .stdout(file("stdout"))
            .stderr(file("stderr"))
            .spawn()
            .expect("spawn a child");
        children.0.push(child);
    }
    children
}

/// Waits until every child is ready; fails at once, with its output, if one exits first.
fn wait_until_ready(work: &Path, children: &mut Children) {
    let deadline = Instant::now() + BARRIER_TIMEOUT;
    let count = children.0.len();
    let ready = |number: usize| work.join(format!("ready-{number}")).exists();
    while !(0..count).all(ready) {
        for (number, child) in children.0.iter_mut().enumerate() {
            if let Some(status) = child.try_wait().expect("poll a child") {
                panic!(
                    "child {number} exited before it was ready ({status}):\n{}",
                    outputs(work, &[number])
                );
            }
        }
        assert!(Instant::now() < deadline, "the children never got ready");
        thread::sleep(Duration::from_millis(1));
    }
}

/// Waits until every child has exited cleanly, until `deadline`. A child that fails, or
/// children still running at the deadline, end the wait at once: every child still running is
/// killed and reaped, and the failure is returned with the output of the children it names.
fn wait_for_children(
    work: &Path,
    children: &mut Children,
    deadline: Instant,
) -> Result<(), String> {
    loop {
        let statuses: Vec<Option<ExitStatus>> = children
            .0
            .iter_mut()
            .map(|child| child.try_wait().expect("poll a child"))
            .collect();
        let failed = statuses
            .iter()
            .enumerate()
            .find_map(|(number, status)| status.filter(|s| !s.success()).map(|s| (number, s)));
        if let Some((number, status)) = failed {
            children.kill_all();
            return Err(format!(
                "child {number} failed ({status}); every other child killed and reaped:\n{}",
                outputs(work, &[number])
            ));
        }
        let running: Vec<usize> = (0..statuses.len())
            .filter(|&number| statuses[number].is_none())
            .collect();
        if running.is_empty() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            children.kill_all();
            return Err(format!(
                "children {running:?} still running at the parent's deadline, killed and \
                 reaped:\n{}",
                outputs(work, &running)
            ));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// Child `number`'s report, from its stdout file.
fn report_of(work: &Path, number: usize) -> Result<Report, String> {
    let stdout = output(work, "stdout", number);
    let line = stdout
        .lines()
        .find_map(|line| line.strip_prefix(REPORT))
        .ok_or_else(|| {
            format!(
                "child {number} printed no report:\n{}",
                outputs(work, &[number])
            )
        })?;
    serde_json::from_str(line).map_err(|e| format!("child {number}'s report: {e}"))
}

/// What the children did.
struct Contention {
    /// Each child's report, or why there is none.
    reports: Vec<Result<Report, String>>,
    /// From go until the last child exited.
    wall: Duration,
    /// The 1-minute load average before go, its lowest and highest during the run, and after.
    load: [f64; 4],
}

/// Runs [`CHILDREN`] children on the store in `work`, from `base`, lets them go together
/// once all are ready, and collects their reports; a child that fails, or one still running at
/// [`PARENT_DEADLINE`], fails the test (every child killed and reaped first).
fn run_children(work: &Path, base: &str) -> Contention {
    let mut children = spawn_children(work, base, TEST, CHILDREN, &[]);
    wait_until_ready(work, &mut children);
    let before = load_average();
    let sampler = LoadSampler::start();
    let go = Instant::now();
    fs::write(work.join("go"), b"").expect("say go");
    let waited = wait_for_children(work, &mut children, go + PARENT_DEADLINE);
    let wall = go.elapsed();
    let (low, high) = sampler.stop();
    if let Err(failure) = waited {
        panic!("{failure}");
    }
    let after = load_average();
    Contention {
        reports: (0..CHILDREN)
            .map(|number| report_of(work, number))
            .collect(),
        wall,
        load: [before, low, high, after],
    }
}

/// Prints what the children measured, and returns how many `E_BUSY`s they met.
fn print_contention(run: &Contention, commits: usize, control_ms: u64) -> usize {
    let reports: Vec<&Report> = run.reports.iter().flatten().collect();
    let mut elapsed: Vec<u64> = reports
        .iter()
        .flat_map(|report| report.elapsed_ms.iter().copied())
        .collect();
    let mut wall: Vec<u64> = reports
        .iter()
        .flat_map(|report| report.wall_us.iter().copied())
        .collect();
    elapsed.sort_unstable();
    wall.sort_unstable();
    let busy: usize = reports.iter().map(|report| report.busy_ms.len()).sum();
    let [before, low, high, after] = run.load;
    println!(
        "{SUMMARY}{CHILDREN} children x {commits} commits of one small file each, each child on \
         its own chain (--parent its previous changeset)"
    );
    println!(
        "{SUMMARY}load (1-minute average): {before:.2} before, {low:.2}-{high:.2} during, \
         {after:.2} after"
    );
    println!(
        "{SUMMARY}{} commits in {:.1} s: {:.1} commits/s",
        elapsed.len(),
        run.wall.as_secs_f64(),
        per_second(elapsed.len(), run.wall)
    );
    if !elapsed.is_empty() {
        println!(
            "{SUMMARY}commit meta.elapsed_ms: mean {} p50 {} p90 {} p99 {} p99.9 {} max {}",
            mean(&elapsed),
            permille(&elapsed, 500),
            permille(&elapsed, 900),
            permille(&elapsed, 990),
            permille(&elapsed, 999),
            elapsed[elapsed.len() - 1]
        );
        println!(
            "{SUMMARY}commit wall time (spawn to exit): p50 {} p99 {} max {}",
            ms(permille(&wall, 500)),
            ms(permille(&wall, 990)),
            ms(wall[wall.len() - 1])
        );
    }
    let busy_at: Vec<String> = reports
        .iter()
        .flat_map(|report| {
            let child = report.child;
            report
                .busy_ms
                .iter()
                .map(move |ms| format!("child {child} after {ms} ms"))
        })
        .collect();
    println!(
        "{SUMMARY}E_BUSY {busy}{}, children without a report {} (control: E_BUSY after \
         {control_ms} ms)",
        if busy_at.is_empty() {
            String::new()
        } else {
            format!(" ({})", busy_at.join(", "))
        },
        run.reports.len() - reports.len()
    );
    print_fairness(&reports);
    busy
}

/// Prints each child's total time and the spread between the slowest and the fastest, and
/// how many children met a slow commit.
fn print_fairness(reports: &[&Report]) {
    let Some(fastest) = reports.iter().min_by_key(|report| report.total_us) else {
        return;
    };
    let slowest = reports
        .iter()
        .max_by_key(|report| report.total_us)
        .expect("a report");
    println!(
        "{SUMMARY}child total time: min {:.1} s (child {}), max {:.1} s (child {}), spread {:.2}x",
        secs(fastest.total_us),
        fastest.child,
        secs(slowest.total_us),
        slowest.child,
        ratio(slowest.total_us, fastest.total_us)
    );
    let over = |ms: u64| {
        reports
            .iter()
            .filter(|report| report.elapsed_ms.iter().any(|&e| e > ms))
            .count()
    };
    println!(
        "{SUMMARY}children with a commit over 1 s: {}, over 5 s: {}",
        over(1_000),
        over(5_000)
    );
}

/// Times [`SOLO`] commits in a row on the final store, uncontended, and prints them and the
/// lock wait they put the contended commits' at.
fn print_solo(work: &Path, base: &str, contention: &Contention) {
    let store = store_path(work);
    let changes = work.join("changes-solo.json");
    let mut parent = base.to_owned();
    let mut elapsed = Vec::with_capacity(SOLO);
    let mut wall = Vec::with_capacity(SOLO);
    for j in 0..SOLO {
        let what = format!("solo commit {j}");
        let (run, took) = commit(
            &store,
            &changes,
            "contention/solo",
            Some(&parent),
            "solo.txt",
            &what,
        );
        match outcome(&run, Some(&parent), &what) {
            Outcome::Committed { id, elapsed_ms } => {
                elapsed.push(elapsed_ms);
                wall.push(micros(took));
                parent = id;
            }
            Outcome::Busy { .. } => panic!("{what}, alone on the store: {run}"),
        }
    }
    elapsed.sort_unstable();
    wall.sort_unstable();
    let p50 = permille(&elapsed, 500);
    println!(
        "{SUMMARY}{SOLO} uncontended commits after: meta.elapsed_ms p50 {p50} p99 {} max {}; \
         wall p50 {}",
        permille(&elapsed, 990),
        elapsed[SOLO - 1],
        ms(permille(&wall, 500))
    );
    let mut waits: Vec<u64> = contention
        .reports
        .iter()
        .flatten()
        .flat_map(|report| report.elapsed_ms.iter().map(|&e| e.saturating_sub(p50)))
        .collect();
    waits.sort_unstable();
    if !waits.is_empty() {
        println!(
            "{SUMMARY}lock wait, estimated as meta.elapsed_ms - {p50} ms: mean {} p50 {} p99 {} \
             max {} ms",
            mean(&waits),
            permille(&waits, 500),
            permille(&waits, 990),
            waits[waits.len() - 1]
        );
    }
}

/// Prints what `verify --deep` found and what it cost.
fn print_verify(verify: &support::Run) {
    let envelope = verify.envelope();
    let report = match verify.exit() {
        0 => &envelope["result"],
        _ => &envelope["error"]["details"],
    };
    println!(
        "{SUMMARY}verify --deep: exit {}, {} problems, {} objects and {} changesets checked in {} ms",
        verify.exit(),
        report["problems"].as_array().map_or(0, Vec::len),
        report["objects_checked"],
        report["changesets_checked"],
        elapsed_ms(&envelope, "verify --deep")
    );
}

/// Moves main from `base` to child 0's last changeset and lists its last five commits.
fn log_from_child_zero(store: &str, base: &str, run: &Contention) -> support::Run {
    let head = match run.reports.first() {
        Some(Ok(report)) => report.head.clone(),
        _ => base.to_owned(),
    };
    let moved = [
        "--repo",
        store,
        "--as",
        "contention/harness",
        "ref",
        "set",
        "main",
        &head,
        "--expect",
        base,
    ];
    support::run_within(&moved, &[], RUN_TIMEOUT).ok();
    let log = support::run_within(&["--repo", store, "log", "-n", "5"], &[], RUN_TIMEOUT);
    let envelope = log.envelope();
    println!(
        "{SUMMARY}log -n 5 from main (child 0's last changeset): exit {}, {} entries in {} ms",
        log.exit(),
        envelope["result"]["entries"].as_array().map_or(0, Vec::len),
        elapsed_ms(&envelope, "log -n 5")
    );
    log
}

/// Checks that `log` lists child 0's last five commits, newest first.
fn check_log(log: &support::Run, run: &Contention, commits: usize) {
    let result = log.ok();
    let Some(Ok(zero)) = run.reports.first() else {
        panic!("child 0 gave no report");
    };
    let entries = result["entries"].as_array().expect("log entries");
    assert_eq!(entries.len(), 5.min(commits + 1), "{log}");
    assert_eq!(entries[0]["id"], zero.head.as_str());
    for (entry, j) in entries.iter().zip((0..commits).rev()) {
        assert_eq!(entry["title"], format!("agent 0 commit {j}"));
    }
}

// ---------------------------------------------------------------------------------------------
// Numbers.
// ---------------------------------------------------------------------------------------------

/// Samples the 1-minute load average every [`LOAD_EVERY`] on a thread of its own.
struct LoadSampler {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<(f64, f64)>,
}

impl LoadSampler {
    fn start() -> LoadSampler {
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            let (mut low, mut high) = (f64::INFINITY, f64::NEG_INFINITY);
            let mut next = Instant::now();
            while !stopped.load(Ordering::Acquire) {
                if Instant::now() >= next {
                    let load = load_average();
                    (low, high) = (low.min(load), high.max(load));
                    next += LOAD_EVERY;
                }
                thread::sleep(Duration::from_millis(100));
            }
            (low, high)
        });
        LoadSampler { stop, thread }
    }

    /// The lowest and the highest load sampled.
    fn stop(self) -> (f64, f64) {
        self.stop.store(true, Ordering::Release);
        self.thread.join().expect("the load sampler")
    }
}

/// The 1-minute load average, as `uptime` gives it; NaN if `/proc/loadavg` cannot be read.
fn load_average() -> f64 {
    fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|text| text.split_whitespace().next()?.parse().ok())
        .unwrap_or(f64::NAN)
}

/// The base directory of the stores measured on: `SWARMOPS_BENCH_DIR`, by default
/// `~/.local/state/swarmops-bench`.
fn base_dir() -> PathBuf {
    let dir = std::env::var_os("SWARMOPS_BENCH_DIR").map_or_else(
        || {
            let home = std::env::var_os("HOME").expect("HOME is set");
            PathBuf::from(home).join(".local/state/swarmops-bench")
        },
        PathBuf::from,
    );
    fs::create_dir_all(&dir).expect("create the bench base dir");
    dir
}

/// How many commits each child makes: [`COMMITS`], or what [`COMMITS_VAR`] says.
fn commits_per_child() -> usize {
    std::env::var(COMMITS_VAR).map_or(COMMITS, |n| {
        n.parse()
            .expect("SWARMOPS_TEST_CLI_CONTENTION_COMMITS is a number")
    })
}

/// The mean of `values`, which is not empty, rounded down.
fn mean(values: &[u64]) -> u64 {
    values.iter().sum::<u64>() / u64::try_from(values.len()).expect("fits")
}

/// The nearest-rank `per_mille` / 1000 percentile of `sorted`, which is not empty.
fn permille(sorted: &[u64], per_mille: usize) -> u64 {
    let rank = (sorted.len() * per_mille).div_ceil(1000).max(1);
    sorted[rank - 1]
}

fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

/// `us` microseconds as milliseconds with one decimal.
fn ms(us: u64) -> String {
    format!("{:.1} ms", Duration::from_micros(us).as_secs_f64() * 1e3)
}

fn secs(us: u64) -> f64 {
    Duration::from_micros(us).as_secs_f64()
}

fn ratio(us: u64, of: u64) -> f64 {
    secs(us) / secs(of)
}

/// `count` per second of `wall`.
fn per_second(count: usize, wall: Duration) -> f64 {
    f64::from(u32::try_from(count).unwrap_or(u32::MAX)) / wall.as_secs_f64()
}
