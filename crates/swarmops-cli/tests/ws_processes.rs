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
    use std::path::Path;
    use std::time::{Duration, Instant};

    use camino::Utf8Path;
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
        let ops = LocalOps::open(Utf8Path::new(store.path())).unwrap();
        let mut head = None;
        for start in (0..files).step_by(CHUNK) {
            let end = files.min(start + CHUNK);
            let changes = (start..end)
                .map(|i| change(path_of(i, dirs), text(i)))
                .collect();
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
        let path = dir.to_str().unwrap();
        let mut args = vec!["--repo", store.path(), "--as", AGENT];
        args.extend(["ws", "create", name, "--path", path, "--all"]);
        args.extend(extra);
        let started = Instant::now();
        let made = run_in(store.tmp(), &[], &args, None);
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
}
