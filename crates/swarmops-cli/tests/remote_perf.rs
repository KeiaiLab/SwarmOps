//! The git remotes' performance cases (M13), for the `perf` recipe. Written by the M13 plan's
//! task C11.
//!
//! The three targets of the roadmap's Topology 2 table, against the test kit's loopback server,
//! so the network does not count. Each case prints `remote-perf: ` lines and asserts only what
//! it ran (the commands succeed, the stores agree), never the target: a missed target is
//! recorded in `docs/perf.md`, not a failing test (the plan's decision 22). All are ignored in
//! `just check`; `just perf` runs them in a release build.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use swarmops_testkit::git;
use swarmops_testkit::remote::{GitServer, HttpAuth, HttpServer};
use tempfile::TempDir;

use crate::support::{Run, Store, run_within};

/// The agent the commands that need an identity run as.
const AGENT: &str = "agent/worker-17";

/// The repository every case serves.
const REPO: &str = "repo";

/// The remote's name in the store.
const ORIGIN: &str = "origin";

/// How long one command may take on the long history: a loaded host imports 12,000 commits in
/// minutes.
const SLOW_RUN: Duration = Duration::from_secs(3_600);

/// The long history of the roadmap's targets.
const LONG: usize = 12_000;

/// The short history the fetch and push targets compare against.
const SHORT: usize = 1_000;

/// How many one-commit fetches and pushes are timed on each history; the median counts.
const ROUNDS: usize = 5;

/// A served repository and the git environment the binary runs with.
struct Remote {
    /// The remote's side.
    server: GitServer,
    /// The server's smart HTTP.
    http: HttpServer,
    /// git's empty `HOME` for the product's children.
    _home: TempDir,
    /// The variables every run sets.
    env: Vec<(String, String)>,
}

impl Remote {
    /// [`REPO`] served with a linear `main` of `commits` commits (none: an empty repository).
    fn with_history(commits: usize) -> Remote {
        let server = GitServer::new();
        let bare = server.create(REPO);
        if commits > 0 {
            fast_import(&server, &bare, commits);
        }
        Remote::serve(server)
    }

    /// [`REPO`] served as a copy of the committed fixture repository at `source`.
    fn from_repository(source: &Path) -> Remote {
        let server = GitServer::new();
        let bare = server.bare(REPO);
        let mut clone = server.command();
        clone
            .args(["clone", "--quiet", "--bare"])
            .arg(source)
            .arg(&bare);
        succeed(&mut clone);
        let mut config = server.command();
        config.arg("--git-dir").arg(&bare);
        config.args(["config", "http.receivepack", "true"]);
        succeed(&mut config);
        Remote::serve(server)
    }

    /// `server`, served over HTTP with the environment the binary runs in.
    fn serve(server: GitServer) -> Remote {
        let http = server.http(HttpAuth::Anonymous);
        let home = tempfile::tempdir().expect("a temp HOME");
        let git = text(server.git_program());
        let git = if git.starts_with('/') {
            git
        } else {
            which(&git)
        };
        let env = vec![
            ("SWARMOPS_GIT".to_owned(), git),
            ("HOME".to_owned(), text(home.path())),
            ("GIT_CONFIG_NOSYSTEM".to_owned(), "1".to_owned()),
            ("GIT_CONFIG_GLOBAL".to_owned(), "/dev/null".to_owned()),
        ];
        Remote {
            server,
            http,
            _home: home,
            env,
        }
    }

    /// The repository's HTTP URL.
    fn url(&self) -> String {
        self.http.url(REPO)
    }

    /// A commit on `main` of the remote, as another person pushing.
    fn commit(&self, n: usize) {
        let path = format!("new{n}.txt");
        let body = format!("new {n}\n");
        self.server
            .commit_on(REPO, "main", &format!("remote {n}"), &[(&path, &body)]);
    }

    /// The variables every run sets.
    fn vars(&self) -> Vec<(&str, &str)> {
        let pairs = self.env.iter();
        pairs.map(|(k, v)| (k.as_str(), v.as_str())).collect()
    }

    /// A fresh store whose commands run with this remote's git environment.
    fn store(&self) -> Store {
        Store::init().with_env(&self.vars())
    }

    /// Runs `args` with this remote's git environment.
    fn run(&self, args: &[&str]) -> Run {
        run_within(args, &self.vars(), SLOW_RUN)
    }

    /// `args` on `store` as [`AGENT`], with this remote's git environment and [`SLOW_RUN`] to end.
    fn on(&self, store: &Store, args: &[&str]) -> Run {
        let mut line = vec!["--repo", store.path(), "--as", AGENT];
        line.extend_from_slice(args);
        run_within(&line, &self.vars(), SLOW_RUN)
    }

    /// A store that has cloned the remote: the remote added, `main` fetched and set.
    fn cloned(&self) -> Store {
        let store = self.store();
        let url = self.url();
        let line = ["remote", "add", ORIGIN, url.as_str(), "--branch", "main"];
        store.read(&line).ok();
        let head = fetch(self, &store);
        let line = ["ref", "set", "main", head.as_str(), "--create"];
        store.run_as(AGENT, &line, &[]).ok();
        store
    }
}

/// `main` of `bare` made of `commits` commits, one file changed in each, by `git fast-import`.
fn fast_import(server: &GitServer, bare: &Path, commits: usize) {
    let mut stream = String::new();
    for n in 1..=commits {
        let when = 1_700_000_000 + n;
        let body = format!("commit {n}\n");
        let message = format!("c{n}");
        writeln!(stream, "commit refs/heads/main").expect("write");
        writeln!(stream, "mark :{n}").expect("write");
        writeln!(stream, "committer Perf <perf@example.com> {when} +0000").expect("write");
        writeln!(stream, "data {}\n{message}", message.len()).expect("write");
        if n > 1 {
            writeln!(stream, "from :{}", n - 1).expect("write");
        }
        // 64 files in rotation: trees stay small, every commit changes one.
        writeln!(stream, "M 100644 inline dir{}/f{}.txt", n % 8, n % 64).expect("write");
        writeln!(stream, "data {}\n{body}", body.len()).expect("write");
    }
    let mut command = server.command();
    command
        .arg("--git-dir")
        .arg(bare)
        .args(["fast-import", "--quiet"]);
    command.stdin(Stdio::piped());
    let mut child = command.spawn().expect("run git fast-import");
    let mut stdin = child.stdin.take().expect("a piped stdin");
    stdin
        .write_all(stream.as_bytes())
        .expect("feed fast-import");
    drop(stdin);
    assert!(child.wait().expect("git fast-import").success());
}

/// `command` run to its end; a failure fails the case.
fn succeed(command: &mut std::process::Command) {
    let output = command.output().expect("run git");
    assert!(output.status.success(), "{command:?}: {output:?}");
}

/// `path` as text.
fn text(path: &Path) -> String {
    path.to_str().expect("UTF-8 paths").to_owned()
}

/// The absolute path of the program `name` on `PATH`.
fn which(name: &str) -> String {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
        .map(|found| text(&found))
        .expect("git on PATH")
}

/// `fetch origin` on `store`: the changeset of the remote's head.
fn fetch(remote: &Remote, store: &Store) -> String {
    let fetched = remote.on(store, &["fetch", ORIGIN]).ok();
    fetched["head"]["changeset"]
        .as_str()
        .expect("a head")
        .to_owned()
}

/// The command's own `meta.elapsed_ms`, a succeeded run's.
fn elapsed_ms(run: &Run) -> u32 {
    run.ok();
    let envelope = run.envelope();
    let ms = envelope["meta"]["elapsed_ms"].as_u64().expect("elapsed_ms");
    u32::try_from(ms).expect("a small time")
}

/// The median of `times`.
fn median(mut times: Vec<u32>) -> u32 {
    times.sort_unstable();
    times[times.len() / 2]
}

/// The changeset `main` names in `store`.
fn main_at(store: &Store) -> String {
    let refs = store.read(&["ref", "list"]).ok();
    let listed = refs["refs"].as_array().expect("refs");
    let main = listed.iter().find(|entry| entry["name"] == "main");
    main.expect("main")["target"]
        .as_str()
        .expect("a target")
        .to_owned()
}

/// Commits `content` at `path` on `main` of `store` and moves `main` to it.
fn land(store: &Store, path: &str, content: &str) {
    let old = main_at(store);
    let new = store.commit(AGENT, path, content, Some(&old));
    let line = ["ref", "set", "main", new.as_str(), "--expect", old.as_str()];
    store.run_as(AGENT, &line, &[]).ok();
}

/// Times `work`.
fn timed(work: impl FnOnce()) -> Duration {
    let start = Instant::now();
    work();
    start.elapsed()
}

/// Times `so clone` of `remote`, and `git clone --bare` followed by `git import` of it, and
/// prints both and the ratio.
fn clone_cost(label: &str, remote: &Remote) {
    let dir = tempfile::tempdir().expect("a temp dir");
    let url = remote.url();

    // 1. The product's clone.
    let into = text(&dir.path().join("so"));
    let args = [
        "--as",
        AGENT,
        "clone",
        url.as_str(),
        "--into",
        into.as_str(),
    ];
    let mut cloned = None;
    let so = timed(|| cloned = Some(remote.run(&args)));
    let cloned = cloned.expect("ran").ok();

    // 2. Git's own clone, then today's import of the bare copy.
    let bare = dir.path().join("plain.git");
    let mut plain = remote.server.command();
    plain
        .args(["clone", "--quiet", "--bare", url.as_str()])
        .arg(&bare);
    let plain_clone = timed(|| succeed(&mut plain));
    let store = remote.store();
    let source = text(&bare);
    let mut imported = None;
    let import = timed(|| {
        let line = ["git", "import", source.as_str()];
        imported = Some(remote.on(&store, &line));
    });
    let imported = imported.expect("ran").ok();
    assert_eq!(
        cloned["imported"], imported["imported"],
        "both made the same changesets"
    );

    let baseline = plain_clone + import;
    println!(
        "remote-perf: clone {label}: so clone {:.3} s; git clone --bare {:.3} s + git import \
         {:.3} s = {:.3} s; ratio {:.2} (target at most 1.10)",
        so.as_secs_f64(),
        plain_clone.as_secs_f64(),
        import.as_secs_f64(),
        baseline.as_secs_f64(),
        so.as_secs_f64() / baseline.as_secs_f64()
    );
}

/// The medians, in ms, of [`ROUNDS`] one-commit fetches and then one-commit pushes on a history
/// of `commits`.
fn fetch_and_push(commits: usize) -> (u32, u32) {
    let remote = Remote::with_history(commits);
    let store = remote.cloned();

    // 1. Fetches: the remote gains a commit, the store fetches it and moves `main` to it.
    let mut got = Vec::new();
    for n in 0..ROUNDS {
        remote.commit(n);
        let fetched = remote.on(&store, &["fetch", ORIGIN]);
        got.push(elapsed_ms(&fetched));
        let head = fetched.ok()["head"]["changeset"]
            .as_str()
            .expect("a head")
            .to_owned();
        let old = main_at(&store);
        let line = [
            "ref",
            "set",
            "main",
            head.as_str(),
            "--expect",
            old.as_str(),
        ];
        store.run_as(AGENT, &line, &[]).ok();
    }

    // 2. Pushes: a landed commit goes out, fast-forward.
    let mut sent = Vec::new();
    for n in 0..ROUNDS {
        land(&store, &format!("local{n}.txt"), &format!("local {n}\n"));
        let pushed = remote.on(&store, &["push", ORIGIN]);
        sent.push(elapsed_ms(&pushed));
    }
    (median(got), median(sent))
}

/// Target 1 on the 12,000-commit fixture: `so clone` against `git clone --bare` and
/// `git import`.
#[test]
#[ignore = "slow, minutes: imports 12,000 commits twice, run by `just perf`"]
fn clone_cost_on_12000_commits() {
    clone_cost("12,000 commits", &Remote::with_history(LONG));
}

/// Target 1 on committed fixtures (the pilot's repository is timed by the pilot task).
#[test]
#[ignore = "slow, ~20 s: clones each committed fixture twice, run by `just perf`"]
fn clone_cost_on_committed_fixtures() {
    for name in ["linear", "merges", "roots"] {
        if git::skipped(name).is_some() {
            continue;
        }
        let fixture = git::fixture(name);
        let repo = fixture.repo.clone().expect("a repository");
        clone_cost(name, &Remote::from_repository(&repo));
    }
}

/// Target 2: a one-commit `fetch` and `push` on 12,000 commits against 1,000, by
/// `meta.elapsed_ms`.
#[test]
#[ignore = "slow, minutes: imports 13,000 commits, run by `just perf`"]
fn fetch_and_push_scale_with_history() {
    let (fetch_short, push_short) = fetch_and_push(SHORT);
    let (fetch_long, push_long) = fetch_and_push(LONG);
    let ratio = |long: u32, short: u32| f64::from(long) / f64::from(short.max(1));
    println!(
        "remote-perf: fetch of one commit: {SHORT} commits {fetch_short} ms, {LONG} commits \
         {fetch_long} ms; ratio {:.2} (target at most 1.50)",
        ratio(fetch_long, fetch_short)
    );
    println!(
        "remote-perf: push of one commit: {SHORT} commits {push_short} ms, {LONG} commits \
         {push_long} ms; ratio {:.2} (target at most 1.50)",
        ratio(push_long, push_short)
    );
}

/// Target 3: a `sync` of one land onto a remote that gained one commit, on 12,000 commits,
/// timed by the client.
#[test]
#[ignore = "slow, minutes: imports 12,000 commits, run by `just perf`"]
fn sync_of_one_land_on_12000_commits() {
    let remote = Remote::with_history(LONG);
    let store = remote.cloned();

    // 1. Both sides move: the remote gains a commit, the store lands one.
    remote.commit(0);
    land(&store, "local.txt", "local\n");

    // 2. The sync, timed by the client.
    let mut synced = None;
    let took = timed(|| synced = Some(remote.on(&store, &["sync", ORIGIN])));
    synced.expect("ran").ok();
    println!(
        "remote-perf: sync of one land: {:.3} s on {LONG} commits (target under 1 s)",
        took.as_secs_f64()
    );
}

/// The mirror's git directory inside a store, as the git crate lays it out.
const MIRROR: &str = "git/mirror.git";

/// The configuration the product's git children run with (the git crate's `CONFIG`), so that
/// git alone does the same durable work.
const GIT_CONFIG: [&str; 8] = [
    "core.fsync=all",
    "core.fsyncMethod=fsync",
    "gc.autoDetach=false",
    "protocol.version=2",
    "fetch.recurseSubmodules=false",
    "transfer.credentialsInUrl=die",
    "fetch.fsckObjects=true",
    "transfer.fsckObjects=true",
];

/// The product's `git fetch` options up to the URL (the git crate's `FETCH_FLAGS`).
const FETCH_FLAGS: [&str; 8] = [
    "fetch",
    "--no-tags",
    "--prune",
    "--no-write-fetch-head",
    "--no-auto-gc",
    "--no-recurse-submodules",
    "--no-show-forced-updates",
    "--end-of-options",
];

/// The product's refspecs for [`ORIGIN`], each forced.
const REFSPECS: [&str; 2] = [
    "+refs/heads/*:refs/remotes/origin/*",
    "+refs/tags/*:refs/remote-tags/origin/*",
];

/// The medians, in ms, of one fetch's phases.
struct Phases {
    /// The binary's `fetch`, by `meta.elapsed_ms`.
    so: u32,
    /// `git fetch` alone into a copy of the mirror.
    fetch: u32,
    /// `git gc --auto` alone on that copy.
    gc: u32,
}

impl Phases {
    /// What git's two phases leave of the binary's time: SwarmOps' own share.
    fn rest(&self) -> i32 {
        let rest = i64::from(self.so) - i64::from(self.fetch) - i64::from(self.gc);
        i32::try_from(rest).expect("a small time")
    }
}

/// `args` run as the product runs git: the server's binary and pinned environment, with
/// [`GIT_CONFIG`], on the git directory `dir`.
fn git_on(remote: &Remote, dir: &Path, args: &[&str]) -> std::process::Command {
    let mut command = remote.server.command();
    for pair in GIT_CONFIG {
        command.args(["-c", pair]);
    }
    command.arg("--git-dir").arg(dir).args(args);
    command
}

/// Times `command` to its end, in ms; a failure fails the case.
fn timed_ms(command: &mut std::process::Command) -> u32 {
    let took = timed(|| succeed(command));
    u32::try_from(took.as_millis()).expect("a small time")
}

/// [`ROUNDS`] one-commit fetches on `store`, the remote's commits numbered from `first`: each
/// round copies the mirror (`cp -a`), then times the binary's `fetch`, and `git fetch` and
/// `git gc --auto` alone on the copy.
fn phases(remote: &Remote, store: &Store, first: usize) -> Phases {
    let mirror = Path::new(store.path()).join(MIRROR);
    let url = remote.url();
    let (mut so, mut fetch, mut gc) = (Vec::new(), Vec::new(), Vec::new());
    for n in first..first + ROUNDS {
        remote.commit(n);

        // 1. The copy holds what the mirror held before the binary's fetch.
        let dir = tempfile::tempdir().expect("a temp dir");
        let copy = dir.path().join("copy.git");
        let mut cp = std::process::Command::new("cp");
        cp.arg("-a").arg(&mirror).arg(&copy);
        succeed(&mut cp);

        // 2. The binary's fetch, then git's two phases of it alone.
        so.push(elapsed_ms(&remote.on(store, &["fetch", ORIGIN])));
        let mut args = FETCH_FLAGS.to_vec();
        args.push(url.as_str());
        args.extend(REFSPECS);
        fetch.push(timed_ms(&mut git_on(remote, &copy, &args)));
        let auto = ["gc", "--auto", "--quiet"];
        gc.push(timed_ms(&mut git_on(remote, &copy, &auto)));
    }
    Phases {
        so: median(so),
        fetch: median(fetch),
        gc: median(gc),
    }
}

/// Prints `phases` of `label`.
fn print_phases(label: &str, phases: &Phases) {
    println!(
        "remote-perf: fetch phases {label}: so fetch {} ms; git fetch {} ms + git gc --auto {} \
         ms; rest (SwarmOps) {} ms",
        phases.so,
        phases.fetch,
        phases.gc,
        phases.rest()
    );
}

/// The phases of a one-commit fetch on `commits`, without and then with a commit-graph on the
/// server and the mirror.
fn fetch_phases(commits: usize) -> (Phases, Phases) {
    let remote = Remote::with_history(commits);
    let store = remote.cloned();
    let plain = phases(&remote, &store, 0);

    // A commit-graph on both sides; each round's copy takes the mirror's.
    let write = ["commit-graph", "write", "--reachable"];
    let bare = remote.server.bare(REPO);
    succeed(&mut git_on(&remote, &bare, &write));
    let mirror = Path::new(store.path()).join(MIRROR);
    succeed(&mut git_on(&remote, &mirror, &write));
    let graph = phases(&remote, &store, ROUNDS);
    (plain, graph)
}

/// G17 fetch's evidence: the one-commit fetch on 1,000 and 12,000 commits split into git's
/// `fetch` and `gc --auto` and the rest, SwarmOps' own, with and without a commit-graph.
#[test]
#[ignore = "slow, minutes: imports 13,000 commits, times 20 fetches three ways, run by `just perf`"]
fn fetch_phases_scale_with_history() {
    let (short, short_graph) = fetch_phases(SHORT);
    let (long, long_graph) = fetch_phases(LONG);
    let ratio = |long: f64, short: f64| long / short.max(1.0);
    for (label, short, long) in [
        ("without commit-graph", &short, &long),
        ("with commit-graph", &short_graph, &long_graph),
    ] {
        print_phases(&format!("{SHORT} commits, {label}"), short);
        print_phases(&format!("{LONG} commits, {label}"), long);
        println!(
            "remote-perf: fetch phases ratio {label}: so fetch {:.2}, git fetch {:.2}, \
             git gc --auto {:.2}, rest {:.2}",
            ratio(f64::from(long.so), f64::from(short.so)),
            ratio(f64::from(long.fetch), f64::from(short.fetch)),
            ratio(f64::from(long.gc), f64::from(short.gc)),
            ratio(f64::from(long.rest()), f64::from(short.rest()))
        );
    }
}
