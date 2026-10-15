//! 32 agents committing at once through the real `swarmops` binary: what a commit costs when
//! every process takes its turn on the store's `meta/lock` (storage spec, risk 1: 32 processes
//! × 1,000 commits on ext4). The store-level half of the measurement, 32 processes of bare
//! `MetaStore` transactions, is `swarmops-store`'s `tests/contention.rs`.
//!
//! Ignored, since it runs minutes of fsync'd commits. `just check-slow` runs it unoptimized,
//! with [`COMMITS`] commits per child: 250, the fallback from the spec's [`SPEC_COMMITS`],
//! which took 8.6 and 14.8 minutes on the loaded host that docs/perf.md "M1 (CLI level)"
//! records. `just perf` runs the spec's 32 × 1,000 optimized and prints only its summary, as
//! docs/perf.md cites it: `SWARMOPS_TEST_CLI_CONTENTION_COMMITS=1000 cargo nextest run
//! --release -p swarmops-cli --run-ignored all --test contention --no-capture`.
//! [`COMMITS_VAR`] sets how many commits each child makes.
//!
//! The parent `init`s a store in a fresh temporary directory under `SWARMOPS_BENCH_DIR`
//! (default `~/.local/state/swarmops-bench`, on the native filesystem, as every measurement in
//! docs/perf.md), commits a base changeset and points main at it. It checks first that the
//! harness sees a `Busy` at all: one commit while the test itself holds `meta/lock` must fail
//! with `E_BUSY` (exit 4) once the store's 10 s lock timeout has passed.
//!
//! Then it runs this test binary again as [`CHILDREN`] children (selected by [`WORK_DIR`]).
//! Each child says it is ready and waits until the parent, once all are, says go; then it runs
//! `swarmops commit` [`COMMITS`] times in a row (or as [`COMMITS_VAR`] says), as agent
//! `contention/agent-<n>`. Each commit puts one small file, `agents/<n>.txt`, whose content
//! names the commit, on `--parent` the child's own previous changeset (the base for its
//! first), so that no two children's commits depend on each other. A commit that fails with
//! `E_BUSY` (exit 4, retryable) is counted and run again; any other failure is recorded and
//! ends the child's run. The child prints one report line: each commit's `meta.elapsed_ms` and
//! its wall time from spawn to exit, the `meta.elapsed_ms` of each `E_BUSY`, its failure if it
//! had one, its total time from go to its last exit, and its last changeset.
//!
//! The parent prints the summary, every line starting [`SUMMARY`]: the 1-minute load average
//! before, during and after the run, throughput, per-commit latency percentiles, `E_BUSY`s
//! and failures, the children's total times and the spread between the slowest and the
//! fastest; then what [`SOLO`] uncontended commits cost on the final store (and so roughly
//! how long the contended commits waited for the lock), and what `verify --deep` and
//! `log -n 5` (from main, moved to child 0's last changeset) return and cost. Only then does
//! it assert: every `E_BUSY` after the lock timeout, none at all, no other failure, every
//! commit made (as `info` counts them), `verify --deep` clean, and the log as committed.

use std::fs;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use swarmops_store::{Layout, MetaLock};

/// How many children commit at once.
const CHILDREN: usize = 32;

/// How many commits each child makes in the scenario of the storage spec (risk 1).
const SPEC_COMMITS: usize = 1_000;

/// How many commits each child makes unless [`COMMITS_VAR`] says otherwise: the fallback
/// from [`SPEC_COMMITS`], whose run takes over 10 minutes on a loaded host.
const COMMITS: usize = 250;

/// Set to give each child another number of commits than [`COMMITS`].
const COMMITS_VAR: &str = "SWARMOPS_TEST_CLI_CONTENTION_COMMITS";

/// This test's name, which the children are run with.
const TEST: &str = "thirty_two_processes_thousand_commits_each";

/// Set only in the children: the work directory, which holds the store (`store/`) and the
/// barrier files (`ready-<n>`, `go`).
const WORK_DIR: &str = "SWARMOPS_TEST_CLI_CONTENTION_DIR";

/// Set with [`WORK_DIR`]: the child's number, from 0.
const CHILD_NUMBER: &str = "SWARMOPS_TEST_CLI_CONTENTION_CHILD";

/// Set with [`WORK_DIR`]: the base changeset, every child's first parent.
const BASE: &str = "SWARMOPS_TEST_CLI_CONTENTION_BASE";

/// How a child's report line starts, among the test harness's own output.
const REPORT: &str = "contention-child: ";

/// How every line of the summary starts, so that `just perf` can pick them out.
const SUMMARY: &str = "contention: ";

/// How many uncontended commits the parent times once the children are done.
const SOLO: usize = 100;

/// The store's lock timeout (`MetaStore`'s default), after which a writer gets `E_BUSY`.
const LOCK_TIMEOUT_MS: u64 = 10_000;

/// How long a process waits at the barrier before it gives up.
const BARRIER_TIMEOUT: Duration = Duration::from_secs(120);

/// How often the parent samples the load average while the children run.
const LOAD_EVERY: Duration = Duration::from_secs(5);

/// The variables the binary reads. Every run removes them, so that the developer's
/// environment never leaks in.
const VARS: &[&str] = &[
    "SWARMOPS_REPO",
    "SWARMOPS_AGENT",
    "SWARMOPS_RUN_ID",
    "SWARMOPS_FORMAT",
    "SWARMOPS_TEST_CRASH_AT",
];

#[test]
#[ignore = "measurement, minutes of fsync'd commits: 32 processes committing through the binary"]
fn thirty_two_processes_thousand_commits_each() {
    if let Some((work, number, base)) = child_env() {
        child(&work, number, &base);
        return;
    }
    let started = Instant::now();
    let commits = commits_per_child();
    let work = tempfile::tempdir_in(base_dir()).expect("create the work dir");
    let store = work.path().join("store");
    let store = store
        .to_str()
        .expect("the work dir's path is UTF-8")
        .to_owned();
    let base = create_store(&store);
    let control = busy_control(&store, &base);

    let run = run_children(work.path(), &base);
    let busy = print_contention(&run, commits, control);
    let changesets = swarmops(&["--repo", &store, "info"], None);
    let solo = print_solo(&store, &base, &run);
    let verify = swarmops(&["--repo", &store, "verify", "--deep"], None);
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
    let crashed: Vec<&String> = run
        .reports
        .iter()
        .filter_map(|r| r.as_ref().err())
        .collect();
    assert!(
        crashed.is_empty(),
        "children without a report: {crashed:#?}"
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
    let failures: Vec<&String> = run
        .reports
        .iter()
        .flatten()
        .filter_map(|report| report.failure.as_ref())
        .chain(&solo)
        .collect();
    assert!(failures.is_empty(), "failed commands: {failures:#?}");
    for report in run.reports.iter().flatten() {
        assert_eq!(report.elapsed_ms.len(), commits, "child {}", report.child);
    }
    let expected = 1 + CHILDREN * commits;
    assert_eq!(changesets.exit, 0, "{}", changesets.shown());
    assert_eq!(
        changesets.envelope["result"]["changesets"].as_u64(),
        u64::try_from(expected).ok(),
        "{}",
        changesets.shown()
    );
    assert_eq!(verify.exit, 0, "verify --deep: {}", verify.shown());
    assert_eq!(verify.envelope["result"]["problems"], json!([]));
    check_log(&log, &run, commits);
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
    /// The failure that ended the run, if one did.
    failure: Option<String>,
    /// From go to the last commit's exit, in µs.
    total_us: u64,
    /// The last changeset committed: the base if none was.
    head: String,
}

/// The work directory, number and base changeset of this process, if it is a child.
fn child_env() -> Option<(PathBuf, usize, String)> {
    let work = std::env::var_os(WORK_DIR)?;
    let number = std::env::var(CHILD_NUMBER)
        .expect("a child has a number")
        .parse()
        .expect("a child's number is a number");
    let base = std::env::var(BASE).expect("a child has a base changeset");
    Some((PathBuf::from(work), number, base))
}

/// A child's run: [`COMMITS`] commits (or as [`COMMITS_VAR`] says), each on the one before,
/// then its report.
fn child(work: &Path, number: usize, base: &str) {
    let store = work.join("store");
    let store = store.to_str().expect("UTF-8");
    let agent = format!("contention/agent-{number:02}");
    let path = format!("agents/{number:02}.txt");
    let commits = commits_per_child();
    let mut report = Report {
        child: number,
        elapsed_ms: Vec::with_capacity(commits),
        wall_us: Vec::with_capacity(commits),
        busy_ms: Vec::new(),
        failure: None,
        total_us: 0,
        head: base.to_owned(),
    };
    fs::write(work.join(format!("ready-{number}")), b"").expect("say ready");
    wait_for(&work.join("go"));
    let started = Instant::now();
    while report.elapsed_ms.len() < commits {
        let j = report.elapsed_ms.len();
        let what = format!("agent {number} commit {j}");
        let run = commit(store, &agent, Some(report.head.as_str()), &path, &what);
        match outcome(&run, &report.head) {
            Outcome::Committed(id) => {
                report.elapsed_ms.push(run.elapsed_ms());
                report.wall_us.push(micros(run.wall));
                report.head = id;
            }
            Outcome::Busy => report.busy_ms.push(run.elapsed_ms()),
            Outcome::Failed(why) => {
                report.failure = Some(format!("child {number}, commit {j}: {why}"));
                break;
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

/// One run of the binary.
struct Run {
    exit: i32,
    /// The envelope it printed; `null` if stdout did not hold one.
    envelope: Value,
    /// What it printed, for messages.
    stdout: String,
    stderr: String,
    /// From spawn to exit.
    wall: Duration,
}

impl Run {
    /// The envelope's `meta.elapsed_ms`; `u64::MAX` if it has none.
    fn elapsed_ms(&self) -> u64 {
        self.envelope["meta"]["elapsed_ms"]
            .as_u64()
            .unwrap_or(u64::MAX)
    }

    /// The run as a failure message gives it.
    fn shown(&self) -> String {
        format!(
            "exit {}, stdout {:?}, stderr {:?}",
            self.exit,
            self.stdout.trim_end(),
            self.stderr.trim_end()
        )
    }
}

/// Runs the binary with `args`, `stdin` on its standard input, and none of the variables it
/// reads set.
fn swarmops(args: &[&str], stdin: Option<&str>) -> Run {
    let mut command = Command::new(env!("CARGO_BIN_EXE_swarmops"));
    command
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for var in VARS {
        command.env_remove(var);
    }
    let started = Instant::now();
    let mut child = command.spawn().expect("run swarmops");
    if let Some(stdin) = stdin {
        let mut pipe = child.stdin.take().expect("stdin is piped");
        // A process that exits before it reads its input says why in its envelope.
        let _ = pipe.write_all(stdin.as_bytes());
    }
    let output = child.wait_with_output().expect("wait for swarmops");
    let wall = started.elapsed();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    Run {
        // A signal leaves no exit code: -1, which no envelope gives.
        exit: output.status.code().unwrap_or(-1),
        envelope: serde_json::from_str(&stdout).unwrap_or(Value::Null),
        stdout,
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        wall,
    }
}

/// Commits one small file, `path`, as `agent` on `parent` (with none, on main if it exists)
/// to `store`: the file's content and the message are both `what`.
fn commit(store: &str, agent: &str, parent: Option<&str>, path: &str, what: &str) -> Run {
    let changes = json!([{"path": path, "op": "put", "content": {"inline": format!("{what}\n")}}]);
    let mut args = vec![
        "--repo",
        store,
        "--as",
        agent,
        "commit",
        "-m",
        what,
        "--changes",
        "-",
    ];
    if let Some(parent) = parent {
        args.extend(["--parent", parent]);
    }
    swarmops(&args, Some(&changes.to_string()))
}

/// What a commit came to.
enum Outcome {
    /// It made this changeset, on the parent it was given.
    Committed(String),
    /// `E_BUSY` (exit 4, retryable): the store's lock stayed taken for the whole timeout.
    Busy,
    /// Anything else, as the failure message gives it.
    Failed(String),
}

/// What `run`, a commit on `parent`, came to.
fn outcome(run: &Run, parent: &str) -> Outcome {
    let result = &run.envelope["result"];
    match run.exit {
        0 if result["parent"] == parent => match result["changeset"].as_str() {
            Some(id) => Outcome::Committed(id.to_owned()),
            None => Outcome::Failed(format!("no changeset id: {}", run.shown())),
        },
        0 => Outcome::Failed(format!("not on parent {parent}: {}", run.shown())),
        4 if run.envelope["error"]["code"] == "E_BUSY" => Outcome::Busy,
        _ => Outcome::Failed(run.shown()),
    }
}

// ---------------------------------------------------------------------------------------------
// The parent.
// ---------------------------------------------------------------------------------------------

/// Creates the store at `store`, with a base changeset that main names; the base's id.
fn create_store(store: &str) -> String {
    let init = swarmops(&["init", store, "--name", "contention"], None);
    assert_eq!(init.exit, 0, "init: {}", init.shown());
    // No main yet, so a commit without a parent is a root commit.
    let base = commit(
        store,
        "contention/harness",
        None,
        "README.md",
        "contention base",
    );
    assert_eq!(base.exit, 0, "the base commit: {}", base.shown());
    let id = base.envelope["result"]["changeset"]
        .as_str()
        .expect("a changeset id")
        .to_owned();
    let main = swarmops(
        &[
            "--repo",
            store,
            "--as",
            "contention/harness",
            "ref",
            "set",
            "main",
            &id,
            "--create",
        ],
        None,
    );
    assert_eq!(main.exit, 0, "ref set main: {}", main.shown());
    id
}

/// Checks that the harness sees a `Busy`: a commit while this process holds `meta/lock` must
/// get `E_BUSY` (exit 4) after the store's lock timeout. Returns its `meta.elapsed_ms`.
fn busy_control(store: &str, base: &str) -> u64 {
    let layout = Layout::new(Utf8PathBuf::from(store));
    let held = MetaLock::acquire(&layout, Duration::from_secs(1)).expect("take meta/lock");
    let run = commit(
        store,
        "contention/control",
        Some(base),
        "control.txt",
        "the lock is held",
    );
    drop(held);
    assert!(
        matches!(outcome(&run, base), Outcome::Busy),
        "a commit against a held lock: {}",
        run.shown()
    );
    assert!(run.elapsed_ms() >= LOCK_TIMEOUT_MS, "{}", run.shown());
    println!(
        "{SUMMARY}control: a commit while the test held meta/lock got E_BUSY (exit 4) after {} ms",
        run.elapsed_ms()
    );
    run.elapsed_ms()
}

/// What the children did.
struct Contention {
    /// Each child's report, or what it printed instead of one.
    reports: Vec<Result<Report, String>>,
    /// From go until the last child exited.
    wall: Duration,
    /// The 1-minute load average before go, its lowest and highest during the run, and after.
    load: [f64; 4],
}

/// Runs the children on the store in `work`, from `base`, and collects their reports.
fn run_children(work: &Path, base: &str) -> Contention {
    let exe = std::env::current_exe().expect("this test binary");
    let mut children: Vec<Child> = (0..CHILDREN)
        .map(|number| {
            Command::new(&exe)
                .args(["--exact", TEST, "--ignored", "--nocapture"])
                .env(WORK_DIR, work)
                .env(CHILD_NUMBER, number.to_string())
                .env(BASE, base)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn a child")
        })
        .collect();
    wait_until_ready(work, &mut children);
    let before = load_average();
    let sampler = LoadSampler::start();
    let go = Instant::now();
    fs::write(work.join("go"), b"").expect("say go");
    let outputs: Vec<Output> = children
        .into_iter()
        .map(|child| child.wait_with_output().expect("wait for a child"))
        .collect();
    let wall = go.elapsed();
    let (low, high) = sampler.stop();
    let after = load_average();
    Contention {
        reports: outputs.iter().enumerate().map(report_of).collect(),
        wall,
        load: [before, low, high, after],
    }
}

/// Waits until every child is ready; fails at once, with what it printed, if one exits first.
fn wait_until_ready(work: &Path, children: &mut [Child]) {
    let deadline = Instant::now() + BARRIER_TIMEOUT;
    loop {
        let ready = (0..CHILDREN)
            .filter(|number| work.join(format!("ready-{number}")).exists())
            .count();
        if ready == CHILDREN {
            return;
        }
        for (number, child) in children.iter_mut().enumerate() {
            if let Some(status) = child.try_wait().expect("poll a child") {
                let mut stderr = String::new();
                if let Some(mut pipe) = child.stderr.take() {
                    let _ = pipe.read_to_string(&mut stderr);
                }
                panic!("child {number} exited before go ({status}): {stderr}");
            }
        }
        assert!(
            Instant::now() < deadline,
            "{ready} of {CHILDREN} children ready"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

/// Child `number`'s report, from its output.
fn report_of((number, output): (usize, &Output)) -> Result<Report, String> {
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .find_map(|line| line.strip_prefix(REPORT))
        .ok_or_else(|| {
            format!(
                "child {number} ({}) printed no report:\n{stdout}\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            )
        })
        .and_then(|line| serde_json::from_str(line).map_err(|e| format!("child {number}: {e}")))
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
    let failed = reports
        .iter()
        .filter(|report| report.failure.is_some())
        .count();
    let [before, low, high, after] = run.load;
    println!(
        "{SUMMARY}{CHILDREN} children x {commits} commits of one small file each, each child on \
         its own chain (--parent its previous changeset){}",
        if commits < SPEC_COMMITS {
            format!(
                "; the fallback from the spec's {CHILDREN} x {SPEC_COMMITS}, which takes over \
                 10 minutes on a loaded host ({COMMITS_VAR}={SPEC_COMMITS} runs it)"
            )
        } else {
            String::new()
        }
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
        "{SUMMARY}E_BUSY {busy}{}, children failed {failed}, children without a report {} \
         (control: E_BUSY after {control_ms} ms)",
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

/// Times [`SOLO`] commits in a row on the final store, uncontended, prints them and the lock
/// wait they put the contended commits' at, and returns any failure.
fn print_solo(store: &str, base: &str, contention: &Contention) -> Option<String> {
    let mut parent = base.to_owned();
    let mut elapsed = Vec::with_capacity(SOLO);
    let mut wall = Vec::with_capacity(SOLO);
    for j in 0..SOLO {
        let what = format!("solo commit {j}");
        let run = commit(
            store,
            "contention/solo",
            Some(parent.as_str()),
            "solo.txt",
            &what,
        );
        match outcome(&run, &parent) {
            Outcome::Committed(id) => {
                elapsed.push(run.elapsed_ms());
                wall.push(micros(run.wall));
                parent = id;
            }
            Outcome::Busy | Outcome::Failed(_) => {
                return Some(format!("solo commit {j}: {}", run.shown()));
            }
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
    None
}

/// Prints what `verify --deep` found and what it cost.
fn print_verify(verify: &Run) {
    let result = &verify.envelope["result"];
    println!(
        "{SUMMARY}verify --deep: exit {}, {} problems, {} objects and {} changesets checked in {} ms",
        verify.exit,
        result["problems"].as_array().map_or(0, Vec::len),
        result["objects_checked"],
        result["changesets_checked"],
        verify.elapsed_ms()
    );
}

/// Moves main from `base` to child 0's last changeset and lists its last five commits.
fn log_from_child_zero(store: &str, base: &str, run: &Contention) -> Run {
    let head = match run.reports.first() {
        Some(Ok(report)) => report.head.clone(),
        _ => base.to_owned(),
    };
    let moved = swarmops(
        &[
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
        ],
        None,
    );
    assert!(
        moved.exit == 0 || head == base,
        "ref set main: {}",
        moved.shown()
    );
    let log = swarmops(&["--repo", store, "log", "-n", "5"], None);
    println!(
        "{SUMMARY}log -n 5 from main (child 0's last changeset): exit {}, {} entries in {} ms",
        log.exit,
        log.envelope["result"]["entries"]
            .as_array()
            .map_or(0, Vec::len),
        log.elapsed_ms()
    );
    log
}

/// Checks that `log` lists child 0's last five commits, newest first.
fn check_log(log: &Run, run: &Contention, commits: usize) {
    assert_eq!(log.exit, 0, "log -n 5: {}", log.shown());
    let Some(Ok(zero)) = run.reports.first() else {
        panic!("child 0 gave no report");
    };
    let entries = log.envelope["result"]["entries"]
        .as_array()
        .expect("log entries");
    assert_eq!(entries.len(), 5.min(commits + 1), "{}", log.shown());
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
