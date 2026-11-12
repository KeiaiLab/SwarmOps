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

/// Target 3's ratio (the M18 design §2.3): the same sync of one land on 1,000 commits, which the
/// 12,000-commit sync may take at most 1.5 times.
#[test]
#[ignore = "slow, ~160 s: imports 1,000 commits, run by `just perf`"]
fn sync_of_one_land_on_1000_commits() {
    let remote = Remote::with_history(SHORT);
    let store = remote.cloned();

    // 1. Both sides move: the remote gains a commit, the store lands one.
    remote.commit(0);
    land(&store, "local.txt", "local\n");

    // 2. The sync, timed by the client.
    let mut synced = None;
    let took = timed(|| synced = Some(remote.on(&store, &["sync", ORIGIN])));
    synced.expect("ran").ok();
    println!(
        "remote-perf: sync of one land: {:.3} s on {SHORT} commits (the {LONG}-commit sync at \
         most 1.5 times this)",
        took.as_secs_f64()
    );
}

/// The mirror's git directory inside a store, as the git crate lays it out.
const MIRROR: &str = "git/mirror.git";

/// The configuration the product's git children run with (the git crate's `CONFIG`), so that
/// git alone does the same durable work.
const GIT_CONFIG: [&str; 9] = [
    "core.fsync=all",
    "core.fsyncMethod=fsync",
    "gc.autoDetach=false",
    "protocol.version=2",
    "fetch.recurseSubmodules=false",
    "transfer.credentialsInUrl=die",
    "fetch.fsckObjects=true",
    "transfer.fsckObjects=true",
    "fetch.writeCommitGraph=true",
];

/// Keys of the product's configuration that name the mirror or the remote, which the baseline
/// sets apart (`--git-dir`, the URL), or not at all.
const PLACE_KEYS: [&str; 2] = ["core.hooksPath", "http."];

/// [`GIT_CONFIG`] is the configuration of the product's `git fetch`, past [`PLACE_KEYS`]: a
/// stand-in git keeps the fetch child's `GIT_CONFIG_*` variables, then runs the real git.
#[cfg(unix)]
#[test]
fn git_config_is_the_products() {
    use std::os::unix::fs::PermissionsExt as _;

    let remote = Remote::with_history(1);
    let dir = tempfile::tempdir().expect("a temp dir");
    let dump = dir.path().join("config");

    // 1. The stand-in: the fetch child's configuration variables kept, then the real git.
    let vars = remote.vars();
    let real = vars.iter().find(|(key, _)| *key == "SWARMOPS_GIT");
    let real = real.expect("SWARMOPS_GIT").1;
    let script = format!(
        "#!/bin/sh\ncase \" $* \" in *\" fetch \"*) env | grep '^GIT_CONFIG_' > '{}' ;; esac\n\
         exec '{real}' \"$@\"\n",
        text(&dump)
    );
    let standin = dir.path().join("git");
    std::fs::write(&standin, script).expect("write the stand-in");
    let mode = std::fs::Permissions::from_mode(0o755);
    std::fs::set_permissions(&standin, mode).expect("make the stand-in runnable");

    // 2. A store's first fetch, through the stand-in.
    let store = remote.store();
    let url = remote.url();
    let line = ["remote", "add", ORIGIN, url.as_str(), "--branch", "main"];
    store.read(&line).ok();
    let standin = text(&standin);
    let mut env = remote.vars();
    env.push(("SWARMOPS_GIT", standin.as_str()));
    let line = ["--repo", store.path(), "--as", AGENT, "fetch", ORIGIN];
    run_within(&line, &env, SLOW_RUN).ok();

    // 3. The child's pairs (`GIT_CONFIG_KEY_<n>`, `GIT_CONFIG_VALUE_<n>`), past the places.
    let kept = std::fs::read_to_string(&dump).expect("the fetch child's configuration");
    let value = |name: String| {
        let line = kept
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{name}=")));
        line.unwrap_or_else(|| panic!("{name} in {kept}"))
            .to_owned()
    };
    let count: usize = value("GIT_CONFIG_COUNT".to_owned())
        .parse()
        .expect("a count");
    let mut product: Vec<String> = (0..count)
        .map(|n| {
            let key = value(format!("GIT_CONFIG_KEY_{n}"));
            format!("{key}={}", value(format!("GIT_CONFIG_VALUE_{n}")))
        })
        .filter(|pair| !PLACE_KEYS.iter().any(|place| pair.starts_with(place)))
        .collect();

    // 4. The same entries as the baseline's.
    let mut baseline = GIT_CONFIG.map(str::to_owned).to_vec();
    product.sort_unstable();
    baseline.sort_unstable();
    assert_eq!(baseline, product);
}

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
    "+refs/heads/*:refs/incoming/origin/heads/*",
    "+refs/tags/*:refs/incoming/origin/tags/*",
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

/// One phase of a sync, as timed by the client and by the binary.
struct Phase {
    /// The client's wall time, process start included.
    wall: Duration,
    /// The binary's `meta.elapsed_ms`.
    so: u32,
}

/// Runs `args` on `store` and times it ([`Phase`]); a failure fails the case.
fn phase(remote: &Remote, store: &Store, args: &[&str]) -> Phase {
    let mut run = None;
    let wall = timed(|| run = Some(remote.on(store, args)));
    let so = elapsed_ms(&run.expect("ran"));
    Phase { wall, so }
}

/// The phases of a sync of one land on `commits`, taken apart: the remote gains a commit and the
/// store lands one, then `fetch`, `sync --no-push` (plan and land, its own fetch finding nothing
/// new) and `push`, each timed; then the same again with `sync` whole, for the sum.
fn sync_phases(commits: usize) -> [(&'static str, Phase); 4] {
    let remote = Remote::with_history(commits);
    let store = remote.cloned();

    // 1. Both sides move, then the three phases one after another.
    remote.commit(0);
    land(&store, "local0.txt", "local 0\n");
    let fetched = phase(&remote, &store, &["fetch", ORIGIN]);
    let landed = phase(&remote, &store, &["sync", ORIGIN, "--no-push"]);
    let pushed = phase(&remote, &store, &["push", ORIGIN]);

    // 2. Both sides move again, and the sync runs whole.
    remote.commit(1);
    land(&store, "local1.txt", "local 1\n");
    let whole = phase(&remote, &store, &["sync", ORIGIN]);
    [
        ("so fetch", fetched),
        ("so sync --no-push", landed),
        ("so push", pushed),
        ("so sync (whole)", whole),
    ]
}

/// G17 sync's evidence: a sync of one land on 1,000 and 12,000 commits split into `fetch`,
/// `sync --no-push` and `push`, one line per phase, and the ratio of each.
#[test]
#[ignore = "slow, minutes: imports 13,000 commits, runs two syncs on each, run by `just perf`"]
fn sync_phases_scale_with_history() {
    let short = sync_phases(SHORT);
    let long = sync_phases(LONG);
    for (commits, phases) in [(SHORT, &short), (LONG, &long)] {
        for (name, phase) in phases {
            println!(
                "remote-perf: sync phases {commits} commits: {name} {:.3} s wall, {} ms \
                 elapsed_ms",
                phase.wall.as_secs_f64(),
                phase.so
            );
        }
    }
    for ((name, short), (_, long)) in short.iter().zip(&long) {
        println!(
            "remote-perf: sync phases ratio: {name} {:.2}",
            long.wall.as_secs_f64() / short.wall.as_secs_f64().max(0.001)
        );
    }
}

/// The first-sync profile's helpers: they read the phase log, which only a build with the test
/// hooks writes.
#[cfg(feature = "test-hooks")]
mod profile {
    use std::path::Path;

    use swarmops_testkit::remote::GitLedger;

    use super::{AGENT, LONG, ORIGIN, Remote, SHORT, SLOW_RUN, land, text, timed};
    use crate::support::{Store, run_within};

    /// The variable that names the sub-cases to run, as `<commits>-<key|nokey>` separated by
    /// commas (`1000-key,12000-nokey`); all four when unset.
    const CASES_VAR: &str = "SWARMOPS_TEST_CLI_FIRST_SYNC_CASES";

    /// The variable that names the phase log (the test hooks only).
    const PHASE_LOG_ENV: &str = "SWARMOPS_TEST_PHASE_LOG";

    /// The variable that names the memo keys' directory (the CLI's `context::MEMO_KEY_DIR_ENV`).
    const MEMO_KEY_DIR_ENV: &str = "SWARMOPS_MEMO_KEY_DIR";

    /// The memo-key derivation's directory in a store: present once an export or an import wrote a
    /// memo under the key.
    const KEYED_MEMOS: &str = "meta/derived/git_oid_mac";

    /// Whether the store's commands find a memo key.
    #[derive(Clone, Copy, PartialEq, Eq)]
    pub(super) enum KeyMode {
        /// [`MEMO_KEY_DIR_ENV`] names a fresh directory, where the first command makes the key.
        Present,
        /// No key directory and no state directory: [`MEMO_KEY_DIR_ENV`], `HOME` and
        /// `XDG_STATE_HOME` empty, which the CLI reads as unset, so it finds no key.
        Absent,
    }

    impl KeyMode {
        /// The mode as the case's lines print it.
        pub(super) fn label(self) -> &'static str {
            match self {
                KeyMode::Present => "key",
                KeyMode::Absent => "nokey",
            }
        }
    }

    /// One sub-case of the profile: a history length and a key mode.
    #[derive(Clone, Copy, PartialEq, Eq)]
    pub(super) struct Case {
        /// The remote's commits.
        pub(super) commits: usize,
        /// Whether the store finds a memo key.
        pub(super) mode: KeyMode,
    }

    impl Case {
        /// The four, in the order they run.
        const ALL: [Case; 4] = [
            Case::new(SHORT, KeyMode::Present),
            Case::new(SHORT, KeyMode::Absent),
            Case::new(LONG, KeyMode::Present),
            Case::new(LONG, KeyMode::Absent),
        ];

        /// The sub-case of `commits` commits in `mode`.
        const fn new(commits: usize, mode: KeyMode) -> Case {
            Case { commits, mode }
        }

        /// Its name in [`CASES_VAR`]: `1000-key`.
        fn name(self) -> String {
            format!("{}-{}", self.commits, self.mode.label())
        }

        /// The sub-cases [`CASES_VAR`] names, in [`Case::ALL`]'s order.
        pub(super) fn chosen() -> Vec<Case> {
            Case::named(std::env::var(CASES_VAR).ok().as_deref())
        }

        /// The sub-cases `list` names, in [`Case::ALL`]'s order; all of them if it is `None`. A
        /// name that is no sub-case's fails the run.
        fn named(list: Option<&str>) -> Vec<Case> {
            let Some(list) = list else {
                return Case::ALL.to_vec();
            };

            // 1. Every name must be a sub-case's: an unknown one fails before any clone.
            let named: Vec<Case> = (list.split(','))
                .map(|name| {
                    (Case::ALL.into_iter())
                        .find(|case| case.name() == name.trim())
                        .unwrap_or_else(|| panic!("no case {name:?} in {CASES_VAR}"))
                })
                .collect();

            // 2. The run order stays the full case's.
            Case::ALL
                .into_iter()
                .filter(|case| named.contains(case))
                .collect()
        }
    }

    /// One phase of a sync, as the phase log and the git ledger give it.
    pub(super) struct Timed {
        /// The phase log's name, or `git.<subcommand>` for git's children.
        pub(super) phase: String,
        /// Its time.
        pub(super) ms: f64,
        /// Its count (the phase log's; git's children by runs).
        pub(super) count: u64,
    }

    /// The phase log of `log`: its lines added up by phase, in their first line's order.
    fn phase_lines(log: &Path) -> Vec<Timed> {
        let written = std::fs::read_to_string(log).expect("the phase log");
        let mut phases: Vec<Timed> = Vec::new();
        for line in written.lines() {
            let fields: Vec<&str> = line.split(' ').collect();
            assert_eq!(fields.len(), 3, "a phase line: {line:?}");
            let us: f64 = fields[1].parse().expect("microseconds");
            let count: u64 = fields[2].parse().expect("a count");

            // A phase met again (a second attempt) adds to its first line.
            match phases.iter_mut().find(|timed| timed.phase == fields[0]) {
                Some(timed) => {
                    timed.ms += us / 1_000.0;
                    timed.count += count;
                }
                None => phases.push(Timed {
                    phase: fields[0].to_owned(),
                    ms: us / 1_000.0,
                    count,
                }),
            }
        }
        phases
    }

    /// git's children of `ledger`, added up by subcommand, in their first run's order.
    fn git_lines(ledger: &GitLedger) -> Vec<Timed> {
        let mut children: Vec<Timed> = Vec::new();
        for child in ledger.children() {
            let phase = format!("git.{}", child.subcommand);
            let ms = child.wall.as_secs_f64() * 1_000.0;
            match children.iter_mut().find(|timed| timed.phase == phase) {
                Some(timed) => {
                    timed.ms += ms;
                    timed.count += 1;
                }
                None => children.push(Timed {
                    phase,
                    ms,
                    count: 1,
                }),
            }
        }
        children
    }

    /// A sync of one land after the remote gained commit `n`, on `store`, with `env`: timed by the
    /// client, with the phase log and the git ledger; its lines, `whole` first.
    fn profiled_sync(
        remote: &Remote,
        ledger: &GitLedger,
        store: &Store,
        env: &[(&str, &str)],
        n: usize,
    ) -> Vec<Timed> {
        // 1. Both sides move.
        remote.commit(n);
        land(store, &format!("local{n}.txt"), &format!("local {n}\n"));

        // 2. The sync, with an empty ledger and a fresh log.
        ledger.clear();
        let log = store.tmp().join(format!("phases-{n}.log"));
        let log_text = text(&log);
        let mut hooked = env.to_vec();
        hooked.push((PHASE_LOG_ENV, log_text.as_str()));
        let line = ["--repo", store.path(), "--as", AGENT, "sync", ORIGIN];
        let mut synced = None;
        let took =
            timed(|| synced = Some(run_within(&line, &vars_with(remote, &hooked), SLOW_RUN)));
        let synced = synced.expect("ran").ok();
        assert_eq!(synced["status"], "pushed", "{synced}");

        // 3. Its lines: the client's time, the phases, git's children.
        let whole = Timed {
            phase: "whole".to_owned(),
            ms: took.as_secs_f64() * 1_000.0,
            count: 1,
        };
        let mut lines = vec![whole];
        lines.extend(phase_lines(&log));
        lines.extend(git_lines(ledger));
        lines
    }

    /// The first sync after a clone of `commits` commits with the key `mode`, then a second sync on
    /// the same store: each sync's lines.
    pub(super) fn first_sync(commits: usize, mode: KeyMode) -> (Vec<Timed>, Vec<Timed>) {
        let remote = Remote::with_history(commits);
        let ledger = GitLedger::new(&remote.server);
        let keys = tempfile::tempdir().expect("a memo keys' directory");
        let program = text(&ledger.program());
        let keys_text = text(keys.path());

        // 1. The store's variables: the ledger's git, and the key mode.
        let mut env = vec![("SWARMOPS_GIT", program.as_str())];
        match mode {
            KeyMode::Present => env.push((MEMO_KEY_DIR_ENV, keys_text.as_str())),
            KeyMode::Absent => {
                env.extend([(MEMO_KEY_DIR_ENV, ""), ("HOME", ""), ("XDG_STATE_HOME", "")]);
            }
        }

        // 2. The clone, as `Remote::cloned` makes one, every command in the mode.
        let store = Store::init().with_env(&vars_with(&remote, &env));
        let url = remote.url();
        store
            .read(&["remote", "add", ORIGIN, url.as_str(), "--branch", "main"])
            .ok();
        let line = ["--repo", store.path(), "--as", AGENT, "fetch", ORIGIN];
        let fetched = run_within(&line, &vars_with(&remote, &env), SLOW_RUN).ok();
        let head = fetched["head"]["changeset"].as_str().expect("a head");
        store
            .run_as(AGENT, &["ref", "set", "main", head, "--create"], &[])
            .ok();

        // 3. The first sync, then a second.
        let first = profiled_sync(&remote, &ledger, &store, &env, 0);
        let second = profiled_sync(&remote, &ledger, &store, &env, 1);

        // 4. The mode held: memos under a key exist exactly when a key was found.
        let keyed = Path::new(store.path()).join(KEYED_MEMOS).exists();
        assert_eq!(
            keyed,
            mode == KeyMode::Present,
            "keyed memos in {}",
            mode.label()
        );
        (first, second)
    }

    /// Unset, every sub-case runs, in the full case's order.
    #[test]
    fn unset_selects_all() {
        let names: Vec<String> = Case::named(None).into_iter().map(Case::name).collect();
        assert_eq!(
            names,
            ["1000-key", "1000-nokey", "12000-key", "12000-nokey"]
        );
    }

    /// A list selects its sub-cases once each, in the full case's order.
    #[test]
    fn list_selects_in_order() {
        let chosen = Case::named(Some("12000-nokey, 1000-key,12000-nokey"));
        let names: Vec<String> = chosen.into_iter().map(Case::name).collect();
        assert_eq!(names, ["1000-key", "12000-nokey"]);
    }

    /// A name that is no sub-case's fails the run.
    #[test]
    #[should_panic(expected = "no case")]
    fn unknown_is_refused() {
        Case::named(Some("1000-maybe"));
    }

    /// An empty list names no sub-case and fails the run.
    #[test]
    #[should_panic(expected = "no case")]
    fn empty_is_refused() {
        Case::named(Some(""));
    }

    /// `remote`'s variables, then `extra`, which a later reading takes over.
    fn vars_with<'a>(remote: &'a Remote, extra: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
        let mut vars = remote.vars();
        vars.extend_from_slice(extra);
        vars
    }
}

/// The first-sync profile (the M18 design §2.2): on 1,000 and 12,000 commits, with the memo key
/// present and absent, a clone, a land onto a remote that gained a commit, and one whole `sync`,
/// timed by the client, by phase (`SWARMOPS_TEST_PHASE_LOG`, which needs the test hooks) and by
/// git child (the test kit's ledger); then a second sync on the same store. One
/// `remote-perf: first sync <commits> <key|nokey> <phase> <ms>` line per phase, and the same as
/// `second sync`. `SWARMOPS_TEST_CLI_FIRST_SYNC_CASES` (`1000-key,12000-nokey`) runs only the
/// sub-cases it names, each as the full case runs it.
#[cfg(feature = "test-hooks")]
#[test]
#[ignore = "slow, ~30 min: imports 26,000 commits, four profiled clones (under 1 min a \
            1,000-commit sub-case, about 15 min a 12,000-commit one), run by `just perf`"]
fn first_sync_phases() {
    for profile::Case { commits, mode } in profile::Case::chosen() {
        let (first, second) = profile::first_sync(commits, mode);
        for (which, lines) in [("first", &first), ("second", &second)] {
            for timed in lines {
                println!(
                    "remote-perf: {which} sync {commits} {} {} {:.1} (count {})",
                    mode.label(),
                    timed.phase,
                    timed.ms,
                    timed.count
                );
            }
        }
    }
}
