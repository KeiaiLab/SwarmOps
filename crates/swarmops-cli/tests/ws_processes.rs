//! `ws land --check` under contention (M5 design §12, acceptance 2), and the workspace
//! performance cases of §9 for `just perf`.
//!
//! Eight `so ws land --check` processes, each in a workspace of its own on one store, land onto
//! `main` at once; one is killed at a random moment and run again. Afterwards each confirmed
//! land is on `main` exactly once, and the tree each landed changeset holds is the tree its
//! check ran on (the check writes `$SWARMOPS_CHECK_TREE` to a log).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::Duration;

use serde_json::Value;
use swarmops_testkit::ws::Rng;

use crate::support;
use crate::ws::{AGENT, Lab, Out, inside, write};

/// The processes that land at once.
const PROCESSES: usize = 8;

/// The latest moment, in milliseconds, at which the killed process is killed.
const KILL_WITHIN_MS: usize = 600;

/// The check: it sleeps a little, so that the processes overlap and the target moves under
/// them, then appends the workspace and the tree it ran on to the log named by `$1`.
const CHECK_SCRIPT: &str =
    "sleep 0.15; printf '%s %s\\n' \"$SWARMOPS_WS\" \"$SWARMOPS_CHECK_TREE\" >> \"$1\"";

/// A process's workspace name.
fn name_of(i: usize) -> String {
    format!("p{i}")
}

/// The file the process `i` adds.
fn file_of(i: usize) -> String {
    format!("src/p{i}.rs")
}

/// The arguments of one `so ws land --check` run: the commit message, the check and its log.
fn land_args(i: usize, log: &Path) -> Vec<String> {
    let mut args: Vec<String> = ["--as", AGENT, "ws", "land", "-m"]
        .map(str::to_owned)
        .to_vec();
    args.push(format!("land {i}"));
    args.extend(["--check", "--max-attempts", "16", "--"].map(str::to_owned));
    args.extend(["sh", "-c", CHECK_SCRIPT, "sh"].map(str::to_owned));
    args.push(log.to_str().unwrap().to_owned());
    args
}

/// Starts `so ws land --check` in workspace `dir`, its output piped.
fn start(dir: &Path, i: usize, log: &Path) -> Child {
    let args = land_args(i, log);
    let mut command = support::swarmops(&args, &[]);
    command
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command.spawn().expect("start swarmops ws land")
}

/// What a run printed and how it ended.
fn finish(child: Child) -> Out {
    let done = child.wait_with_output().expect("wait for swarmops");
    Out {
        exit: done.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&done.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&done.stderr).into_owned(),
    }
}

/// The trees each workspace's checks ran on, in order: the log's lines `<workspace> <tree>`.
fn recorded(log: &Path) -> BTreeMap<String, Vec<String>> {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let mut trees: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for line in text.lines() {
        let (name, tree) = line.split_once(' ').expect("a log line: workspace, tree");
        trees
            .entry(name.to_owned())
            .or_default()
            .push(tree.to_owned());
    }
    trees
}

/// Whether each landed changeset's root is the tree the last check of its workspace ran on:
/// `landed` is `(workspace, root)`; the error names the first that differs.
fn trees_match(
    landed: &[(String, String)],
    recorded: &BTreeMap<String, Vec<String>>,
) -> Result<(), String> {
    for (name, root) in landed {
        let ran = recorded.get(name).and_then(|trees| trees.last());
        if ran != Some(root) {
            return Err(format!(
                "{name}: landed tree {root}, its check ran on {ran:?}"
            ));
        }
    }
    Ok(())
}

/// The first-parent history of `main`, newest first.
fn main_history(lab: &Lab) -> Vec<String> {
    let log = lab.store.read(&["log", "-n", "1000"]).ok();
    log["entries"]
        .as_array()
        .expect("log entries")
        .iter()
        .map(|entry| entry["id"].as_str().unwrap().to_owned())
        .collect()
}

/// The root tree of changeset `id`.
fn root_of(lab: &Lab, id: &str) -> String {
    let shown = lab.store.read(&["show", id]).ok();
    shown["root"].as_str().expect("a root").to_owned()
}

/// The changeset workspace `name` stands on after its land: its `head`.
fn head_of(lab: &Lab, name: &str) -> String {
    let info = lab.so(&["ws", "info", "--ws", name]).ok();
    info["head"].as_str().expect("a head").to_owned()
}

/// One round: eight checked lands at once, one killed at a random moment and run again.
fn round(seed: u64) {
    // 1. A store, and eight workspaces, each with an edit adding a file of its own.
    let lab = Lab::new();
    let log = lab.store.tmp().join("checks.log");
    let dirs: Vec<PathBuf> = (0..PROCESSES)
        .map(|i| {
            let dir = lab.workspace(&name_of(i));
            write(&dir, &file_of(i), &format!("process {i}\n"));
            dir
        })
        .collect();
    let before = main_history(&lab);

    // 2. All eight started together; one killed after a random time.
    let mut rng = Rng::new(seed);
    let victim = rng.below(PROCESSES);
    let delay = u64::try_from(rng.below(KILL_WITHIN_MS)).unwrap();
    let mut children: Vec<Child> = dirs
        .iter()
        .enumerate()
        .map(|(i, dir)| start(dir, i, &log))
        .collect();
    std::thread::sleep(Duration::from_millis(delay));
    children[victim].kill().expect("kill the victim");
    let outs: Vec<Out> = children.into_iter().map(finish).collect();

    // 3. Every process but the victim ended: landed (exit 0), never half-way.
    for (i, out) in outs.iter().enumerate() {
        if i == victim {
            continue;
        }
        assert_eq!(out.exit, 0, "seed {seed}, process {i}: {out}");
        assert!(
            out.envelope()["result"]["land"]["landed"].is_string(),
            "{out}"
        );
    }

    // 4. The victim run again. A kill inside the land's own commit leaves a pending commit,
    //    which the land names as the call to run again first (`ws commit`); then the land.
    let mut again = finish(start(&dirs[victim], victim, &log));
    if again.exit == 2 && again.stdout.contains("cut short") {
        let msg = format!("land {victim}");
        let committed = inside(&dirs[victim], &["ws", "commit", "-m", &msg]);
        assert_eq!(committed.exit, 0, "seed {seed}: {committed}");
        again = finish(start(&dirs[victim], victim, &log));
    }
    let landed_already =
        again.exit != 0 && again.envelope()["error"]["code"] == "E_NOTHING_TO_LAND";
    assert!(again.exit == 0 || landed_already, "seed {seed}: {again}");

    // 5. Each workspace's landed changeset is on `main` once, and holds the tree its check ran
    //    on; `main` gained exactly the eight.
    let history = main_history(&lab);
    assert_eq!(
        history.len(),
        before.len() + PROCESSES,
        "seed {seed}: {history:?}"
    );
    let mut landed = Vec::new();
    for i in 0..PROCESSES {
        let name = name_of(i);
        let head = head_of(&lab, &name);
        let times = history.iter().filter(|id| **id == head).count();
        assert_eq!(
            times, 1,
            "seed {seed}, {name}: {head} is on main {times} times"
        );
        landed.push((name, root_of(&lab, &head)));
    }
    if let Err(forged) = trees_match(&landed, &recorded(&log)) {
        panic!("seed {seed}: {forged}");
    }

    // 6. The store is whole, and `main` holds all eight files.
    let verify = lab.store.read(&["verify", "--deep"]).ok();
    assert_eq!(verify["problems"], Value::Array(Vec::new()), "seed {seed}");
    for i in 0..PROCESSES {
        let file = format!("main:{}", file_of(i));
        let cat = lab.store.read(&["cat", &file]);
        assert_eq!(cat.exit(), 0, "seed {seed}: {file}: {cat}");
    }
}

/// The seed of a round: the clock, so each run kills at another moment; printed on failure by
/// the assertions.
fn fresh_seed() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(1, |d| {
            u64::try_from(d.as_nanos() % u128::from(u64::MAX)).unwrap_or(1)
        })
}

/// Eight `ws land --check` processes on one store, one killed at random: every land that was
/// confirmed is on `main` once, and each landed tree is the tree its check recorded.
#[test]
fn eight_checked_lands_land_once_each_with_the_checked_tree() {
    round(fresh_seed());
}

/// The same round, twelve times with other kill moments.
#[test]
#[ignore = "slow, ~60 s: twelve rounds of eight checked lands"]
fn twelve_rounds_of_checked_lands_land_once_each() {
    let seed = fresh_seed();
    for n in 0..12 {
        round(seed.wrapping_add(n));
    }
}

/// Self-check of the tree comparison: a landed tree that is not the one the check recorded (a
/// forged record) fails it, as does a workspace whose check never ran; the true record passes.
#[test]
fn the_tree_equality_check_fails_on_a_forged_recorded_tree() {
    let landed = vec![("p0".to_owned(), "aaa".to_owned())];
    let mut truth: BTreeMap<String, Vec<String>> = BTreeMap::new();
    truth.insert("p0".to_owned(), vec!["old".to_owned(), "aaa".to_owned()]);
    assert_eq!(trees_match(&landed, &truth), Ok(()));

    let mut forged = truth.clone();
    forged.insert("p0".to_owned(), vec!["bbb".to_owned()]);
    assert!(
        trees_match(&landed, &forged)
            .unwrap_err()
            .starts_with("p0:")
    );
    assert!(trees_match(&landed, &BTreeMap::new()).is_err());
}

/// The performance cases of the M5 design §9, for `just perf` (a release build, a quiet host):
/// each prints `ws-perf: ` lines and asserts only that its commands succeed, never a target; a
/// missed target is recorded in `docs/perf.md`. All are ignored in `just check`.
mod perf {
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::time::{Duration, Instant};

    use camino::Utf8Path;
    use serde_json::Value;
    use swarmops_api::{
        ChangeInput, ChangeOp, CommitReq, ContentInput, Identity, ModeInput, StoreOps,
    };
    use swarmops_ops::LocalOps;
    use swarmops_testkit::ws::{TREE_DIRS, TREE_FILES, content_of, path_of, rewrite_first};

    use crate::support::Store;
    use crate::ws::{AGENT, Out, inside, run_in};

    /// The changes in one seeding commit.
    const CHUNK: usize = 5_000;

    /// How many times a status is timed; the median counts.
    const ROUNDS: usize = 20;

    /// The size of a file of the 100 MiB tree: 10,000 of them make 100 MiB.
    const BIG_FILE: usize = 10 * 1024;

    /// The files of the §9 create case.
    const CREATE_FILES: usize = 10_000;

    /// The changesets of the §9 land case.
    const LONG_HISTORY: usize = 12_000;

    /// The files a land changes in the update case.
    const LANDED_FILES: usize = 10;

    /// Commits `changes` on top of `parent` (the first one when none) and moves `main` there;
    /// the new changeset.
    fn put(ops: &LocalOps, parent: Option<String>, changes: Vec<ChangeInput>) -> String {
        let who = Identity::parse(AGENT).unwrap();
        let mut req = CommitReq::new(changes, "seed");
        req.parent = parent.map(Into::into);
        req.advance = Some("main".to_owned());
        ops.commit(&who, req).unwrap().changeset
    }

    /// One put of inline `text` at `path`.
    fn change(path: String, text: String) -> ChangeInput {
        let mut change = ChangeInput::new(path, ChangeOp::Put);
        change.content = Some(ContentInput::Inline(text));
        change.mode = Some(ModeInput::File);
        change
    }

    /// A store at `store` holding the synthetic tree of `files` files in `dirs` directories,
    /// `text(i)` in file `i`, `main` on it.
    fn seed(store: &Store, files: usize, dirs: usize, text: impl Fn(usize) -> String) -> LocalOps {
        let paths = |i| path_of(i, dirs);
        seed_with(store.path(), files, CHUNK, paths, text)
    }

    /// The store at `repo` holding `files` files, `path(i)` holding `text(i)`, committed
    /// `chunk` at a time, `main` on the last commit.
    fn seed_with(
        repo: &str,
        files: usize,
        chunk: usize,
        path: impl Fn(usize) -> String,
        text: impl Fn(usize) -> String,
    ) -> LocalOps {
        let ops = LocalOps::open(Utf8Path::new(repo)).unwrap();
        let mut head = None;
        for start in (0..files).step_by(chunk) {
            let end = files.min(start + chunk);
            let changes = (start..end).map(|i| change(path(i), text(i))).collect();
            head = Some(put(&ops, head, changes));
        }
        ops
    }

    /// The median of `times`.
    fn median(mut times: Vec<Duration>) -> Duration {
        times.sort();
        times[times.len() / 2]
    }

    /// Runs `args` in `dir` as the agent; what it printed and how long it took.
    fn timed(dir: &Path, args: &[&str]) -> (Out, Duration) {
        let started = Instant::now();
        let out = inside(dir, args);
        (out, started.elapsed())
    }

    /// Prints one result line.
    fn say(case: &str, took: Duration, note: &str) {
        println!("ws-perf: {case}: {:.1} ms {note}", took.as_secs_f64() * 1e3);
    }

    /// Makes workspace `name` of all of `store`'s `main` in `dir`; how long the binary took.
    fn create(store: &Store, dir: &Path, name: &str, extra: &[&str]) -> Duration {
        let mut flags = vec!["--all"];
        flags.extend(extra);
        create_at(store.path(), store.tmp(), dir, name, &flags)
    }

    /// Makes workspace `name` of the store at `repo` in `dir`, run in `cwd` with `flags`
    /// (`--all`, `--prefix`, `--base`); how long the binary took.
    fn create_at(repo: &str, cwd: &Path, dir: &Path, name: &str, flags: &[&str]) -> Duration {
        let path = dir.to_str().unwrap();
        let mut args = vec!["--repo", repo, "--as", AGENT];
        args.extend(["ws", "create", name, "--path", path]);
        args.extend(flags);
        let started = Instant::now();
        let made = run_in(cwd, &[], &args, None);
        let took = started.elapsed();
        made.ok();
        took
    }

    /// §9 rows 1, 2, 4 and 5 on the 50,000-file tree: the status of nothing changed and of 100
    /// files rewritten in the racy window, the update after a land of 10 files, and the
    /// commit of 1, 10 and 100 changed files.
    #[test]
    #[ignore = "slow, perf: seeds 50,000 files and times the binary"]
    fn perf_tree_of_50000_files() {
        // 1. The store, and a workspace of all of it.
        let store = Store::init();
        let ops = seed(&store, TREE_FILES, TREE_DIRS, |i| content_of(i, 0));
        let dir = store.tmp().join("big");
        let took = create(&store, &dir, "big", &[]);
        say("create 50,000 files", took, "");

        // 2. Row 1: nothing changed, a warm index; the median of 20.
        inside(&dir, &["ws", "status"]).ok();
        let times: Vec<Duration> = (0..ROUNDS)
            .map(|_| timed(&dir, &["ws", "status"]).1)
            .collect();
        say(
            "status 50,000 files, nothing changed",
            median(times),
            "(target < 100 ms)",
        );

        // 3. Row 2: 100 files rewritten with the same size, straight after a status.
        rewrite_first(&dir, TREE_DIRS, 100, 1).unwrap();
        let times: Vec<Duration> = (0..ROUNDS)
            .map(|_| timed(&dir, &["ws", "status"]).1)
            .collect();
        say("status after 100 edits", median(times), "(target < 150 ms)");

        // 3a. Those edits committed, untimed: each round below commits only its own k files.
        let edits = ["ws", "commit", "-m", "edits", "--status-after", "none"];
        inside(&dir, &edits).ok();

        // 4. Row 5: a commit of k changed files, by wall time and by the binary's own meta.
        for (round, k) in [1_usize, 10, 100].into_iter().enumerate() {
            let version = u32::try_from(round + 2).unwrap();
            rewrite_first(&dir, TREE_DIRS, k, version + 10).unwrap();
            let (out, wall) = timed(
                &dir,
                &["ws", "commit", "-m", "perf", "--status-after", "none"],
            );
            let elapsed = out.envelope()["meta"]["elapsed_ms"].clone();
            out.ok();
            say(
                &format!("commit of {k} changed files"),
                wall,
                &format!("(meta.elapsed_ms {elapsed})"),
            );
        }

        // 5. Row 4: a land of 10 files by someone else, then this workspace's update.
        let head = ops_head(&ops);
        let changes = (0..LANDED_FILES)
            .map(|i| change(path_of(i + 1_000, TREE_DIRS), content_of(i + 1_000, 99)))
            .collect();
        put(&ops, Some(head), changes);
        let (out, took) = timed(&dir, &["ws", "rebase", "--status-after", "none"]);
        out.ok();
        say("update after a land of 10 files", took, "(target < 100 ms)");

        // 6. The same update as a median of 20: each round a fresh land of the same 10 paths
        //    with new bytes, then the update, which must rebase onto that land.
        let update = ["ws", "rebase", "--status-after", "none"];
        let times: Vec<Duration> = (0..ROUNDS)
            .map(|round| {
                let version = 100 + u32::try_from(round).unwrap();
                let changes = (0..LANDED_FILES)
                    .map(|i| {
                        change(
                            path_of(i + 1_000, TREE_DIRS),
                            content_of(i + 1_000, version),
                        )
                    })
                    .collect();
                let landed = put(&ops, Some(ops_head(&ops)), changes);
                let (out, took) = timed(&dir, &update);
                let done = out.ok();
                assert_eq!(done["status"], "rebased", "round {round}: {done}");
                assert_eq!(done["onto"], landed.as_str(), "round {round}: {done}");
                took
            })
            .collect();
        say(
            &format!("update after a land of {LANDED_FILES} files, median of {ROUNDS}"),
            median(times),
            "(target < 100 ms)",
        );
    }

    /// The workspaces created, and the lands made, for the percentiles of G9's open and land.
    const SAMPLES: usize = 20;

    /// The nearest-rank `percent` percentile of `times`, which is not empty.
    fn percentile(mut times: Vec<Duration>, percent: usize) -> Duration {
        times.sort();
        let rank = (times.len() * percent).div_ceil(100).max(1);
        times[rank - 1]
    }

    /// Prints the p50 and p99 of `times` for `case`.
    fn spread(case: &str, times: &[Duration], note: &str) {
        let (p50, p99) = (
            percentile(times.to_vec(), 50),
            percentile(times.to_vec(), 99),
        );
        println!(
            "ws-perf: {case}: p50 {:.1} ms p99 {:.1} ms n {} {note}",
            p50.as_secs_f64() * 1e3,
            p99.as_secs_f64() * 1e3,
            times.len()
        );
    }

    /// G9's open and land latencies, as percentiles: [`SAMPLES`] workspaces of all of a
    /// 10,000-file tree (`ws create --all`), then [`SAMPLES`] lands in a row from one of them,
    /// each of one changed file (`ws land`), every land a fast-forward of `main`.
    #[test]
    #[ignore = "slow, perf: seeds 10,000 files and times 20 creates and 20 lands"]
    fn perf_open_and_land_percentiles() {
        // 1. The store, and the creates.
        let store = Store::init();
        seed(&store, CREATE_FILES, TREE_DIRS, |i| content_of(i, 0));
        let opens: Vec<Duration> = (0..SAMPLES)
            .map(|n| {
                let name = format!("o{n}");
                let flags = ["--status-after", "none"];
                create(&store, &store.tmp().join(&name), &name, &flags)
            })
            .collect();
        spread("create 10,000 files", &opens, "(target < 3 s, for 100 MiB)");

        // 2. The lands, one changed file each, from the first workspace.
        let dir = store.tmp().join("o0");
        let lands: Vec<Duration> = (0..SAMPLES)
            .map(|n| {
                let version = u32::try_from(n + 1).unwrap();
                rewrite_first(&dir, TREE_DIRS, 1, version).unwrap();
                let args = ["ws", "land", "-m", "perf", "--status-after", "none"];
                let (out, took) = timed(&dir, &args);
                out.ok();
                took
            })
            .collect();
        spread("land of 1 changed file", &lands, "");
    }

    /// The head of `main` in `ops`.
    fn ops_head(ops: &LocalOps) -> String {
        let req = swarmops_api::ResolveRevReq::new("main");
        ops.resolve_rev(req).unwrap().id
    }

    /// §9 row 3: a create of 10,000 files, 100 MiB, in a warm store; the median of three.
    #[test]
    #[ignore = "slow, perf: seeds 100 MiB and times the binary"]
    fn perf_create_of_10000_files() {
        let store = Store::init();
        seed(&store, CREATE_FILES, TREE_DIRS, |i| {
            let mut text = content_of(i, 0);
            text.push_str(&"x".repeat(BIG_FILE - text.len()));
            text
        });
        let times: Vec<Duration> = (0..3)
            .map(|n| {
                create(
                    &store,
                    &store.tmp().join(format!("c{n}")),
                    &format!("c{n}"),
                    &[],
                )
            })
            .collect();
        say(
            "create 10,000 files, 100 MiB",
            median(times),
            "(target < 3 s)",
        );
    }

    /// §9 row 6: the overhead of `ws land --check` with `true` as the check, against a store
    /// of 12,000 changesets: the checked land's time less the unchecked land's.
    #[test]
    #[ignore = "slow, perf: seeds 12,000 changesets and times the binary"]
    fn perf_checked_land_overhead_on_12000_changesets() {
        // 1. The history: one changeset after another, a small tree.
        let store = Store::init();
        let ops = LocalOps::open(Utf8Path::new(store.path())).unwrap();
        let mut head = None;
        for i in 0..LONG_HISTORY {
            let changes = vec![change(format!("src/f{}.rs", i % 100), content_of(i, 0))];
            head = Some(put(&ops, head, changes));
        }

        // 2. Two workspaces, each with an edit to a file of its own.
        let (plain, checked) = (store.tmp().join("plain"), store.tmp().join("checked"));
        create(&store, &plain, "plain", &["--status-after", "none"]);
        create(&store, &checked, "checked", &["--status-after", "none"]);
        std::fs::write(plain.join("src/own-plain.rs"), "p\n").unwrap();
        std::fs::write(checked.join("src/own-checked.rs"), "c\n").unwrap();

        // 3. The unchecked land, then the checked one (rebased onto what the first landed).
        let (out, base) = timed(
            &plain,
            &["ws", "land", "-m", "plain", "--status-after", "none"],
        );
        out.ok();
        let args = [
            "ws",
            "land",
            "-m",
            "checked",
            "--check",
            "--status-after",
            "none",
            "--",
            "true",
        ];
        let (out, with_check) = timed(&checked, &args);
        out.ok();
        say("land, no check", base, "");
        say("land --check true", with_check, "");
        say(
            "check overhead",
            with_check.saturating_sub(base),
            "(target < 1000 ms)",
        );
    }

    // The M19 design §8.4: large trees and many workspaces. Each case seeds a store of its own
    // under `SWARMOPS_BENCH_DIR` (a native filesystem, unlike a tmpfs /tmp) and is sized to
    // finish within half the `ci-perf` profile's 55-minute kill.

    /// The files of the sparse cases' tree.
    const SPARSE_TREE: usize = 1_000_000;

    /// The files under one top directory of the sparse tree: one `--prefix`.
    const PREFIX_FILES: usize = 10_000;

    /// The files under one `p/<nnnn>` directory of a prefix.
    const LEAF_FILES: usize = 100;

    /// The changes in one seeding commit of the large trees: 20 for the sparse tree.
    const BIG_CHUNK: usize = 50_000;

    /// The files of B36's status case, and its directories (20 files each, as the 50,000-file
    /// tree's).
    const LARGE_TREE: usize = 500_000;
    const LARGE_DIRS: usize = 25_000;

    /// The workspaces A31 creates at once.
    const AT_ONCE: usize = 50;

    /// 28A1's two histories: the lands between a workspace's base and the target.
    const NEAR_LANDS: usize = 10;
    const FAR_LANDS: usize = 1_000;

    /// The paths every land of 28A1 changes between them; each land changes a tenth.
    const LANDED_PATHS: usize = 100;

    /// How many workspaces 28A1 updates for each history; the median counts.
    const UPDATE_ROUNDS: usize = 5;

    /// A store of its own in a temp dir under `SWARMOPS_BENCH_DIR` (`<tmp>/store`).
    struct BenchStore {
        /// The temp dir: the store and the workspaces.
        dir: tempfile::TempDir,
        /// The store's path.
        path: String,
    }

    impl BenchStore {
        /// A fresh store, made with `swarmops init`.
        fn init() -> BenchStore {
            let base = swarmops_testkit::measure::base_dir();
            let dir = tempfile::tempdir_in(base).expect("make a bench temp dir");
            let path = format!("{}/store", dir.path().to_str().unwrap());
            crate::support::run(&["init", path.as_str()], &[]).ok();
            BenchStore { dir, path }
        }

        /// The store's path.
        fn path(&self) -> &str {
            &self.path
        }

        /// The temp dir.
        fn tmp(&self) -> &Path {
            self.dir.path()
        }

        /// Makes workspace `name` in `<tmp>/<name>` with `flags`; how long the binary took.
        fn create(&self, name: &str, flags: &[&str]) -> Duration {
            create_at(self.path(), self.tmp(), &self.tmp().join(name), name, flags)
        }
    }

    /// File `i` of the sparse tree: `t<top>/p/<leaf>/f<i>.txt`, each top directory one prefix
    /// of [`PREFIX_FILES`] files in 100 leaves of [`LEAF_FILES`] (`t00/p/0000/f0000000.txt`).
    fn sparse_path(i: usize) -> String {
        let top = i / PREFIX_FILES;
        let leaf = (i % PREFIX_FILES) / LEAF_FILES;
        format!("t{top:02}/p/{leaf:04}/f{i:07}.txt")
    }

    /// Prints how long the seed of `files` (as written: `1,000,000`) took.
    fn seeded(files: &str, started: Instant) {
        say(&format!("seed {files} files"), started.elapsed(), "");
    }

    /// The store's object files: every file under `objects/`, `objects/tmp` apart.
    fn object_count(repo: &str) -> usize {
        let objects = swarmops_testkit::store::layout_of(repo).objects_dir();
        let mut count = 0;
        for fanout in std::fs::read_dir(objects).unwrap() {
            let fanout = fanout.unwrap();
            if fanout.file_name() == "tmp" {
                continue;
            }
            count += std::fs::read_dir(fanout.path()).unwrap().count();
        }
        count
    }

    /// The files below `root`, `.swarmws/` apart: each path's bytes and permission bits.
    fn tree_of(root: &Path) -> BTreeMap<String, (Vec<u8>, u32)> {
        use std::os::unix::fs::PermissionsExt;

        let mut files = BTreeMap::new();
        let mut dirs = vec![root.to_path_buf()];
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                let name = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_owned();
                if name == ".swarmws" {
                    continue;
                }
                let meta = std::fs::symlink_metadata(&path).unwrap();
                if meta.is_dir() {
                    dirs.push(path);
                    continue;
                }
                let mode = meta.permissions().mode() & 0o777;
                files.insert(name, (std::fs::read(&path).unwrap(), mode));
            }
        }
        files
    }

    /// The first path whose bytes or mode differ between two trees, or that one lacks.
    fn first_difference(
        a: &BTreeMap<String, (Vec<u8>, u32)>,
        b: &BTreeMap<String, (Vec<u8>, u32)>,
    ) -> Option<String> {
        let names = a.keys().chain(b.keys());
        names
            .into_iter()
            .find(|name| a.get(*name) != b.get(*name))
            .cloned()
    }

    /// 28A1's equality: workspace `dir` holds the files of a fresh workspace of `flags` at its
    /// own head, byte for byte and mode for mode, and the status of both is empty.
    fn equals_a_fresh_one(store: &BenchStore, dir: &Path, fresh: &str, flags: &[&str]) {
        // 1. A fresh workspace at the updated one's head, with the same prefixes.
        let head = inside(dir, &["ws", "info"]).ok()["head"]
            .as_str()
            .unwrap()
            .to_owned();
        let mut with_base = vec!["--base", head.as_str(), "--status-after", "none"];
        with_base.extend(flags);
        store.create(fresh, &with_base);

        // 2. Same files, and nothing changed in either.
        let other = store.tmp().join(fresh);
        let differs = first_difference(&tree_of(dir), &tree_of(&other));
        assert_eq!(differs, None, "{} against {fresh}", dir.display());
        for root in [dir, other.as_path()] {
            let status = inside(root, &["ws", "status"]).ok();
            assert_eq!(status["changes"], Value::Array(Vec::new()), "{status}");
        }
    }

    /// The disk a directory takes: the allocated blocks of everything below it.
    fn disk_of(root: &Path) -> u64 {
        use std::os::unix::fs::MetadataExt;

        let mut bytes = 0;
        let mut dirs = vec![root.to_path_buf()];
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                let meta = std::fs::symlink_metadata(&path).unwrap();
                bytes += meta.blocks() * 512;
                if meta.is_dir() {
                    dirs.push(path);
                }
            }
        }
        bytes
    }

    /// A28: a sparse create of one 10,000-file prefix from a 1,000,000-file tree (the median
    /// of three), then that workspace's status, a 1-file commit and the update after a land of
    /// 10 files under the prefix, which leaves the files of a fresh workspace at its head.
    #[test]
    #[ignore = "slow, ~372 s: perf, seeds 1,000,000 files and times a sparse workspace"]
    fn perf_sparse_workspace_of_1000000_files() {
        // 1. The tree, in 20 commits of 50,000 files.
        let store = BenchStore::init();
        let started = Instant::now();
        let ops = seed_with(store.path(), SPARSE_TREE, BIG_CHUNK, sparse_path, |i| {
            content_of(i, 0)
        });
        seeded("1,000,000", started);

        // 2. Three sparse creates of the prefix `t00`; the first is the one measured on.
        let sparse = ["--prefix", "t00"];
        let times: Vec<Duration> = (0..3)
            .map(|n| store.create(&format!("s{n}"), &sparse))
            .collect();
        say(
            "sparse create of 10,000 of 1,000,000 files",
            median(times),
            "(no target; G9's whole tree < 3 s)",
        );
        let dir = store.tmp().join("s0");

        // 3. Its status, nothing changed, warm; the median of 20.
        inside(&dir, &["ws", "status"]).ok();
        let times: Vec<Duration> = (0..ROUNDS)
            .map(|_| timed(&dir, &["ws", "status"]).1)
            .collect();
        say(
            "sparse status, nothing changed",
            median(times),
            "(no target; G9's whole tree < 100 ms)",
        );

        // 4. A commit of 1 changed file.
        std::fs::write(dir.join(sparse_path(0)), content_of(0, 1)).unwrap();
        let commit = ["ws", "commit", "-m", "perf", "--status-after", "none"];
        let (out, took) = timed(&dir, &commit);
        let elapsed = out.envelope()["meta"]["elapsed_ms"].clone();
        out.ok();
        say(
            "sparse commit of 1 changed file",
            took,
            &format!("(meta.elapsed_ms {elapsed}; no target; G9's whole tree G2 plus hashing)"),
        );

        // 5. A land of 10 files under the prefix by someone else, then the update.
        let changes = (0..LANDED_FILES)
            .map(|i| change(sparse_path(i + 5_000), content_of(i + 5_000, 99)))
            .collect();
        put(&ops, Some(ops_head(&ops)), changes);
        let (out, took) = timed(&dir, &["ws", "rebase", "--status-after", "none"]);
        out.ok();
        say(
            "sparse update after a land of 10 files",
            took,
            "(no target; G9's whole tree < 100 ms)",
        );

        // 6. The updated workspace is a fresh one at its head.
        equals_a_fresh_one(&store, &dir, "fresh", &sparse);
    }

    /// B36: the status of a workspace of 500,000 files, nothing changed and after 100 files
    /// rewritten in the racy window, each the median of 20.
    #[test]
    #[ignore = "slow, ~333 s: perf, seeds 500,000 files and times their status"]
    fn perf_status_of_500000_files() {
        // 1. The tree, in 10 commits of 50,000 files, and a workspace of all of it.
        let store = BenchStore::init();
        let started = Instant::now();
        let paths = |i| path_of(i, LARGE_DIRS);
        seed_with(store.path(), LARGE_TREE, BIG_CHUNK, paths, |i| {
            content_of(i, 0)
        });
        seeded("500,000", started);
        let took = store.create("large", &["--all", "--status-after", "none"]);
        say("create 500,000 files", took, "");
        let dir = store.tmp().join("large");

        // 2. Nothing changed, a warm index.
        inside(&dir, &["ws", "status"]).ok();
        let times: Vec<Duration> = (0..ROUNDS)
            .map(|_| timed(&dir, &["ws", "status"]).1)
            .collect();
        say(
            "status 500,000 files, nothing changed",
            median(times),
            "(target < 1 s)",
        );

        // 3. 100 files rewritten with the same size, straight after a status.
        rewrite_first(&dir, LARGE_DIRS, 100, 1).unwrap();
        let times: Vec<Duration> = (0..ROUNDS)
            .map(|_| {
                let (out, took) = timed(&dir, &["ws", "status"]);
                let changes = out.ok()["changes"].as_array().unwrap().len();
                assert_eq!(changes, 100, "the 100 edits");
                took
            })
            .collect();
        say(
            "status 500,000 files after 100 edits",
            median(times),
            "(target < 1 s)",
        );
    }

    /// A31: 50 creates of all of a 10,000-file tree at once: each create's time, the store's
    /// objects before and after (the same: a create writes none), the disk each workspace adds.
    #[test]
    #[ignore = "slow, ~20 s: perf, seeds 10,000 files and runs 50 creates at once"]
    fn perf_fifty_creates_at_once() {
        // 1. The tree, and its objects.
        let store = BenchStore::init();
        seed_with(
            store.path(),
            CREATE_FILES,
            CHUNK,
            |i| path_of(i, TREE_DIRS),
            |i| content_of(i, 0),
        );
        let before = object_count(store.path());

        // 2. The creates, released together; each timed from its start to its exit.
        let gate = std::sync::Barrier::new(AT_ONCE);
        let flags = ["--all", "--status-after", "none"];
        let times: Vec<Duration> = std::thread::scope(|scope| {
            let runs: Vec<_> = (0..AT_ONCE)
                .map(|n| {
                    let (store, gate) = (&store, &gate);
                    scope.spawn(move || {
                        gate.wait();
                        store.create(&format!("w{n}"), &flags)
                    })
                })
                .collect();
            runs.into_iter().map(|run| run.join().unwrap()).collect()
        });
        spread(
            "50 creates of 10,000 files at once",
            &times,
            "(target < 3 s each)",
        );
        let slowest = times.iter().max().unwrap();
        say("slowest of 50 creates at once", *slowest, "(target < 3 s)");

        // 3. No object added (I9), and the disk each workspace takes.
        assert_eq!(
            object_count(store.path()),
            before,
            "objects after 50 creates"
        );
        let mut disk: Vec<u64> = (0..AT_ONCE)
            .map(|n| disk_of(&store.tmp().join(format!("w{n}"))))
            .collect();
        disk.sort_unstable();
        println!(
            "ws-perf: disk per workspace: median {} KiB max {} KiB objects {before}",
            disk[AT_ONCE / 2] / 1024,
            disk[AT_ONCE - 1] / 1024,
        );
    }

    /// One land of 28A1: a tenth of the [`LANDED_PATHS`], in turn, at version `n + 1`.
    fn land_tenth(ops: &LocalOps, n: usize) {
        let tenth = LANDED_PATHS / 10;
        let first = (n % 10) * tenth;
        let version = u32::try_from(n + 1).unwrap();
        let changes = (first..first + tenth)
            .map(|i| {
                change(
                    path_of(i + 1_000, TREE_DIRS),
                    content_of(i + 1_000, version),
                )
            })
            .collect();
        put(ops, Some(ops_head(ops)), changes);
    }

    /// Updates the workspaces `<prefix>0..` (`ws rebase`); the median time.
    fn update_all(store: &BenchStore, prefix: &str) -> Duration {
        let times = (0..UPDATE_ROUNDS)
            .map(|r| {
                let dir = store.tmp().join(format!("{prefix}{r}"));
                let (out, took) = timed(&dir, &["ws", "rebase", "--status-after", "none"]);
                out.ok();
                took
            })
            .collect();
        median(times)
    }

    /// 28A1: the update of a 50,000-file workspace across 1,000 lands against across 10, the
    /// same 100 paths changed by both histories (the median of 5 each), and each updated
    /// workspace equal to a fresh one at its head.
    #[test]
    #[ignore = "slow, ~140 s: perf, seeds 50,000 files, 1,000 lands and 10 workspaces"]
    fn perf_update_across_1000_lands() {
        // 1. The tree, and five workspaces for each history, all at its first head.
        let store = BenchStore::init();
        let ops = seed_with(
            store.path(),
            TREE_FILES,
            CHUNK,
            |i| path_of(i, TREE_DIRS),
            |i| content_of(i, 0),
        );
        let flags = ["--all", "--status-after", "none"];
        for r in 0..UPDATE_ROUNDS {
            for name in [format!("near{r}"), format!("far{r}")] {
                store.create(&name, &flags);

                // A warm index, as the whole-tree update's: no row in the racy window.
                inside(&store.tmp().join(&name), &["ws", "status"]).ok();
            }
        }

        // 2. 10 lands, every one of the 100 paths changed once; the near workspaces' update.
        for n in 0..NEAR_LANDS {
            land_tenth(&ops, n);
        }
        let near = update_all(&store, "near");
        say("update across 10 lands, 100 paths", near, "");

        // 3. 990 lands more over the same paths; the far workspaces' update.
        for n in NEAR_LANDS..FAR_LANDS {
            land_tenth(&ops, n);
        }
        let far = update_all(&store, "far");
        say("update across 1,000 lands, 100 paths", far, "");
        println!(
            "ws-perf: update ratio 1,000 lands / 10 lands: {:.2} (target <= 1.5)",
            far.as_secs_f64() / near.as_secs_f64()
        );

        // 4. Each history's updated workspace is a fresh one at its head.
        let all = ["--all"];
        equals_a_fresh_one(&store, &store.tmp().join("near0"), "fresh-near", &all);
        equals_a_fresh_one(&store, &store.tmp().join("far0"), "fresh-far", &all);
    }

    /// The sparse tree's layout: a prefix holds exactly 10,000 files in 100 leaves of 100, and
    /// the next file starts the next prefix.
    #[test]
    fn a_sparse_prefix_holds_ten_thousand_files() {
        assert_eq!(sparse_path(0), "t00/p/0000/f0000000.txt");
        assert_eq!(sparse_path(9_999), "t00/p/0099/f0009999.txt");
        assert_eq!(sparse_path(10_000), "t01/p/0000/f0010000.txt");
        assert_eq!(sparse_path(SPARSE_TREE - 1), "t99/p/0099/f0999999.txt");
        let in_first = (0..SPARSE_TREE)
            .filter(|i| sparse_path(*i).starts_with("t00/"))
            .count();
        assert_eq!(in_first, PREFIX_FILES);
    }

    /// Self-check of the tree comparison: equal trees pass, and a changed byte, a changed mode
    /// or a missing file is named; `.swarmws/` is not compared.
    #[test]
    fn the_tree_comparison_names_a_changed_byte_or_mode() {
        use std::os::unix::fs::PermissionsExt;

        // 1. Two equal trees, one with a `.swarmws/` of its own.
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        for root in [a.path(), b.path()] {
            std::fs::create_dir_all(root.join("d")).unwrap();
            std::fs::write(root.join("d/f.txt"), "one\n").unwrap();
        }
        std::fs::create_dir(a.path().join(".swarmws")).unwrap();
        std::fs::write(a.path().join(".swarmws/index"), "x").unwrap();
        assert_eq!(
            first_difference(&tree_of(a.path()), &tree_of(b.path())),
            None
        );

        // 2. A byte, then the mode, then a file only one side has.
        std::fs::write(b.path().join("d/f.txt"), "two\n").unwrap();
        let named = first_difference(&tree_of(a.path()), &tree_of(b.path()));
        assert_eq!(named.as_deref(), Some("d/f.txt"));
        std::fs::write(b.path().join("d/f.txt"), "one\n").unwrap();
        let exec = std::fs::Permissions::from_mode(0o755);
        std::fs::set_permissions(b.path().join("d/f.txt"), exec).unwrap();
        let named = first_difference(&tree_of(a.path()), &tree_of(b.path()));
        assert_eq!(named.as_deref(), Some("d/f.txt"));
        std::fs::write(a.path().join("d/g.txt"), "g\n").unwrap();
        let named = first_difference(&tree_of(a.path()), &tree_of(b.path()));
        assert!(named.is_some());
    }

    /// Self-check of the object count: a commit of a new file adds objects (its blob, its
    /// trees and its changeset), and a workspace create adds none.
    #[test]
    fn the_object_count_grows_with_a_commit_only() {
        let store = Store::init();
        let empty = object_count(store.path());
        seed(&store, 3, 1, |i| content_of(i, 0));
        let seeded = object_count(store.path());
        assert!(seeded > empty, "{empty} then {seeded}");
        create(
            &store,
            &store.tmp().join("w"),
            "w",
            &["--status-after", "none"],
        );
        assert_eq!(object_count(store.path()), seeded);
    }
}
