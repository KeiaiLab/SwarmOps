//! `swarmops serve`'s measurements (G16), for the `perf` recipe.
//!
//! The same 12,000-commit history is served twice through ssh stand-ins over pipes: by
//! `swarmops serve` from a store, and by git's own `upload-pack` from the bare repository the
//! store imported. Stock git clones and fetches from each; the medians and their ratio print as
//! `serve-perf: ` lines. A third case times one push to `refs/land/main`. Three more (M17) measure
//! the server's CPU and peak memory from process accounting (`swarmops_testkit::accounting`): 50
//! clones, fetches behind by 1 to 1,000 lands, pushes of 1 and 2 GiB. Each case asserts only
//! what it ran (the commands succeed, both sides serve the same commits), never the target: a
//! missed target is recorded in `docs/perf.md`, not a failing test, as `remote_perf`'s are. All
//! but the seeding check are ignored in `just check`; `just perf` runs them in a release build.
//!
//! ```text
//!   GitServer: fast-import ──▶ repo.git ◀── git upload-pack (ssh stand-in)  ◀── client git
//!                                 │ git import
//!   ServeSite: store repo ──▶ git/serve.git ◀── swarmops serve (ssh stand-in) ◀── client git
//! ```

use std::ffi::OsStr;
use std::fmt::Write as _;
use std::fs;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::Barrier;
use std::thread;
use std::time::{Duration, Instant};

use swarmops_testkit::accounting::{Accounting, Spent, ledger_dir, spent_since};
use swarmops_testkit::remote::{GitServer, SshMode};
use swarmops_testkit::serve::ServeSite;
use tempfile::TempDir;

use crate::support::{self, Swarmops};

/// The identity the forced command names.
const AGENT: &str = "person/ada";

/// The agent that imports the history into the store.
const WRITER: &str = "agent/worker-1";

/// The repository's name on both sides.
const REPO: &str = "repo";

/// The served store's access file: every person reads, and lands onto `main`.
const ACCESS: &str = r#"version = 1

[[grant]]
who = ["person/*"]
read = true
land = ["main"]
"#;

/// The history the roadmap's targets name.
const LONG: usize = 12_000;

/// How many runs of each command are timed; the median counts.
const ROUNDS: usize = 5;

/// How long the import of [`LONG`] commits may take: a loaded host takes minutes.
const SLOW_RUN: Duration = Duration::from_secs(3_600);

/// Where a push names the branch it lands onto.
const LAND_MAIN: &str = "HEAD:refs/land/main";

/// Which server a [`Side`] reaches.
#[derive(Clone, Copy)]
enum Server {
    /// `swarmops serve`, through its forced-command stand-in.
    Serve,
    /// git's own `upload-pack`, through the plain stand-in.
    Git,
}

/// One server of the history, as a client reaches it.
struct Side<'a> {
    /// The servers.
    bench: &'a Bench,
    /// Which one this is.
    server: Server,
    /// The stand-in for `GIT_SSH_COMMAND`.
    ssh: String,
    /// The ssh URL of [`REPO`].
    url: String,
}

impl Side<'_> {
    /// `args` in `dir`, which must succeed; its output.
    fn run(&self, dir: &Path, args: &[&str]) -> Output {
        self.run_env(dir, args, &[])
    }

    /// `args` in `dir` with `env` set, which must succeed; its output.
    fn run_env(&self, dir: &Path, args: &[&str], env: &[(&str, &OsStr)]) -> Output {
        let mut command = match self.server {
            Server::Serve => self.bench.site.command(),
            Server::Git => self.bench.server.command(),
        };
        let output = command
            .env("GIT_SSH_COMMAND", &self.ssh)
            .envs(env.iter().copied())
            .current_dir(dir)
            .args(args)
            .output()
            .expect("run git");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "git {args:?}: {stderr}");
        output
    }

    /// `rev` in the clone `dir`.
    fn rev(&self, dir: &Path, rev: &str) -> String {
        let output = self.run(dir, &["rev-parse", "--verify", rev]);
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }
}

/// How the served store's `serve.git` starts.
#[derive(Clone, Copy)]
enum Start {
    /// Empty: the first connection exports the whole history (the v0.7.0 cases).
    Cold,
    /// Holding the history's objects already, and the server's CPU and memory accounted
    /// (`ServeSite::seed_serve_git`, `Accounting::Cpu`): the M17 cases.
    Seeded,
}

/// The history served by `swarmops serve` and by git's `upload-pack`, and a directory for
/// clients.
struct Bench {
    /// The bare repository and git's ssh stand-in.
    server: GitServer,
    /// The store's root and serve's ssh stand-in.
    site: ServeSite,
    /// The clients' directories.
    work: TempDir,
    /// How long the import into the store took.
    imported: Duration,
}

impl Bench {
    /// [`LONG`] commits on `main` of a bare repository, imported into the store [`REPO`].
    fn new() -> Bench {
        Bench::build(LONG, Start::Cold)
    }

    /// `commits` commits on `main` of a bare repository, imported into the store [`REPO`]; the
    /// server's `serve.git` starts as `start` says.
    fn build(commits: usize, start: Start) -> Bench {
        // 1. git's side: the history, written by fast-import.
        let server = GitServer::new();
        let bare = server.create(REPO);
        fast_import(&server, &bare, commits);

        // 2. serve's side: the store and its access file.
        let site = ServeSite::new::<Swarmops>();
        let site = match start {
            Start::Cold => site,
            Start::Seeded => site.with_accounting(Accounting::Cpu),
        };
        let store = site.init(REPO);
        fs::write(store.join("access.toml"), ACCESS).expect("write the access file");

        // 3. A seeded serve.git exists before the store holds a ref, and takes the objects.
        if matches!(start, Start::Seeded) {
            site.open_serve_git(REPO, AGENT);
        }

        // 4. The same history imported, timed.
        let line = [
            "--repo",
            path_str(&store),
            "--as",
            WRITER,
            "git",
            "import",
            path_str(&bare),
            "--ref",
            "main",
            "--into",
            "main",
        ];
        let started = Instant::now();
        support::run_within(&line, &[], SLOW_RUN).ok();
        let imported = started.elapsed();
        if matches!(start, Start::Seeded) {
            site.seed_serve_git(REPO, &bare);
        }
        Bench {
            server,
            site,
            work: TempDir::new().expect("a temp dir"),
            imported,
        }
    }

    /// How many connections the accounted stand-ins have finished: the `mark` of
    /// [`Bench::spent`].
    fn mark(&self) -> usize {
        spent_since(&ledger_dir(&self.site), 0).len()
    }

    /// What the connections finished after `mark` cost the server.
    fn spent(&self, mark: usize) -> Vec<Spent> {
        spent_since(&ledger_dir(&self.site), mark)
    }

    /// The store's path.
    fn store(&self) -> PathBuf {
        self.site.store(REPO)
    }

    /// The side `swarmops serve` serves.
    fn serve(&self) -> Side<'_> {
        Side {
            bench: self,
            server: Server::Serve,
            ssh: self.site.ssh_command(AGENT),
            url: self.site.ssh_url(REPO),
        }
    }

    /// The side git's own `upload-pack` serves.
    fn git(&self) -> Side<'_> {
        Side {
            bench: self,
            server: Server::Git,
            ssh: self.server.ssh_command(SshMode::Serve),
            url: self.server.ssh_url(REPO),
        }
    }

    /// A fresh client directory named `name`.
    fn dir(&self, name: &str) -> PathBuf {
        self.work.path().join(name)
    }
}

/// `main` of `bare` made of `commits` commits, one file changed in each, by `git fast-import`:
/// `remote_perf`'s generator, so both measure the same shape of history.
pub(crate) fn fast_import(server: &GitServer, bare: &Path, commits: usize) {
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

/// `path` as text.
fn path_str(path: &Path) -> &str {
    path.to_str().expect("UTF-8 paths")
}

/// Times `work`.
fn timed(work: impl FnOnce()) -> Duration {
    let start = Instant::now();
    work();
    start.elapsed()
}

/// The median of `times`.
fn median(mut times: Vec<Duration>) -> Duration {
    times.sort_unstable();
    times[times.len() / 2]
}

/// `git clone --no-checkout` of `side` into `dir`.
fn clone_into(side: &Side, dir: &Path) {
    let parent = dir.parent().expect("a parent");
    let line = [
        "clone",
        "--quiet",
        "--no-checkout",
        &side.url,
        path_str(dir),
    ];
    side.run(parent, &line);
}

/// The first clone of `side` (serve's refreshes `serve.git`) and the median of [`ROUNDS`] more;
/// the last clone's directory.
fn clones(bench: &Bench, side: &Side, label: &str) -> (Duration, Duration, PathBuf) {
    let first_dir = bench.dir(&format!("{label}-first"));
    let first = timed(|| clone_into(side, &first_dir));
    let mut times = Vec::new();
    for n in 0..ROUNDS {
        let dir = bench.dir(&format!("{label}-{n}"));
        times.push(timed(|| clone_into(side, &dir)));
    }
    (first, median(times), first_dir)
}

/// Prints serve's and git's medians of `what`, and their ratio against the target 2×.
fn print_ratio(what: &str, serve: Duration, git: Duration) {
    println!(
        "serve-perf: {what} on {LONG} commits: swarmops serve {:.3} s; git upload-pack {:.3} s; \
         ratio {:.2} (target at most 2.00)",
        serve.as_secs_f64(),
        git.as_secs_f64(),
        serve.as_secs_f64() / git.as_secs_f64().max(0.001)
    );
}

/// A clone of the same history from `swarmops serve` and from git's `upload-pack`, median of
/// [`ROUNDS`] each after a first clone (printed apart: serve's first refreshes `serve.git`).
#[test]
#[ignore = "slow, ~27.5 min: imports 12,000 commits, serve.git's first refresh, run by `just perf`"]
fn clone_against_git() {
    let bench = Bench::new();
    let (serve, git) = (bench.serve(), bench.git());
    let (serve_first, serve_median, serve_dir) = clones(&bench, &serve, "serve");
    let (git_first, git_median, git_dir) = clones(&bench, &git, "git");

    // Both served the same history.
    assert_eq!(
        serve.rev(&serve_dir, "origin/main"),
        git.rev(&git_dir, "origin/main"),
        "the two servers serve different commits"
    );
    println!(
        "serve-perf: import of {LONG} commits into the store: {:.3} s",
        bench.imported.as_secs_f64()
    );
    println!(
        "serve-perf: first clone on {LONG} commits: swarmops serve {:.3} s (refreshes \
         serve.git); git upload-pack {:.3} s",
        serve_first.as_secs_f64(),
        git_first.as_secs_f64()
    );
    print_ratio("clone", serve_median, git_median);
}

/// A fetch that finds nothing new, from `swarmops serve` and from git's `upload-pack`, median of
/// [`ROUNDS`] each.
#[test]
#[ignore = "slow, ~27.4 min: imports 12,000 commits, serve.git's first refresh, run by `just perf`"]
fn noop_fetch_against_git() {
    let bench = Bench::new();
    let mut medians = Vec::new();
    for (label, side) in [("serve", bench.serve()), ("git", bench.git())] {
        let dir = bench.dir(label);
        clone_into(&side, &dir);
        let before = side.rev(&dir, "origin/main");
        let mut times = Vec::new();
        for _ in 0..ROUNDS {
            times.push(timed(|| {
                side.run(&dir, &["fetch", "--quiet", "origin"]);
            }));
        }
        assert_eq!(side.rev(&dir, "origin/main"), before, "a no-op fetch moved");
        medians.push(median(times));
    }
    print_ratio("no-op fetch", medians[0], medians[1]);
}

/// One commit pushed to `refs/land/main` on [`LONG`] commits, timed by the client, median of
/// [`ROUNDS`]; each lands, and the client takes the landed commit before the next.
#[test]
#[ignore = "slow, ~26.7 min: imports 12,000 commits, serve.git's first refresh, run by `just perf`"]
fn land_push_on_12000() {
    let bench = Bench::new();
    let side = bench.serve();
    let dir = bench.dir("work");
    side.run(bench.work.path(), &["clone", "--quiet", &side.url, "work"]);

    let mut times = Vec::new();
    for n in 0..ROUNDS {
        // 1. One commit on the landed head.
        let path = format!("land{n}.txt");
        fs::write(dir.join(&path), format!("land {n}\n")).expect("write a file");
        side.run(&dir, &["add", &path]);
        side.run(&dir, &["commit", "--quiet", "-m", &path]);

        // 2. The push, timed; it lands.
        let mut pushed = None;
        times.push(timed(|| {
            pushed = Some(side.run(&dir, &["push", "origin", LAND_MAIN]));
        }));
        let stderr = String::from_utf8_lossy(&pushed.expect("ran").stderr).into_owned();
        assert!(stderr.contains("remote: landed onto main"), "{stderr}");

        // 3. The client moves to the landed commit.
        side.run(&dir, &["fetch", "--quiet", "origin"]);
        side.run(&dir, &["reset", "--quiet", "--hard", "origin/main"]);
    }
    println!(
        "serve-perf: land push of one commit on {LONG} commits: {:.3} s (target under 1 s)",
        median(times).as_secs_f64()
    );
}

/// The first clone after `git import` of [`LONG`] commits and `serve --prepare --seed-from` the
/// import's source (the M17 design §4.3, G20 alone): `swarmops serve`'s against git's own first
/// clone of the bare copy, target at most 2×. Asserted: the preparation wrote no object, the
/// clone writes none, both serve the same commit, and the clone passes `git fsck --strict`.
#[test]
#[ignore = "slow, ~8 min (estimated from the import's share of the cases above; `just perf` \
            measures it): imports 12,000 commits, then the seeded preparation"]
fn first_clone_after_import() {
    let bench = Bench::new();
    let store = bench.store();
    let git_dir = store.join("git/serve.git");

    // 1. The preparation, seeded from the import's source.
    let bare = bench.server.bare(REPO);
    let line = [
        "--repo",
        path_str(&store),
        "serve",
        "--prepare",
        "--seed-from",
        path_str(&bare),
    ];
    let mut run = None;
    let prepared = timed(|| run = Some(support::run_within(&line, &[], SLOW_RUN)));
    let result = run.expect("ran").ok();
    assert_eq!(
        result["refreshed"]["objects"], 0,
        "the preparation wrote objects"
    );
    assert_eq!(
        loose_objects(&git_dir),
        0,
        "the preparation left loose objects"
    );

    // 2. Both first clones, timed; the served one writes nothing.
    let (serve, git) = (bench.serve(), bench.git());
    let (serve_dir, git_dir_clone) = (bench.dir("serve-first"), bench.dir("git-first"));
    let serve_first = timed(|| clone_into(&serve, &serve_dir));
    let git_first = timed(|| clone_into(&git, &git_dir_clone));
    assert_eq!(loose_objects(&git_dir), 0, "the first clone wrote objects");
    serve.run(&serve_dir, &["fsck", "--strict"]);
    assert_eq!(
        serve.rev(&serve_dir, "origin/main"),
        git.rev(&git_dir_clone, "origin/main"),
        "the two servers serve different commits"
    );
    println!(
        "serve-perf: import of {LONG} commits into the store: {:.3} s; serve --prepare \
         --seed-from: {:.3} s",
        bench.imported.as_secs_f64(),
        prepared.as_secs_f64()
    );
    print_ratio("first clone after import", serve_first, git_first);
}

// ---------------------------------------------------------------------------------------------
// M17 baselines (the M17 design §1): server CPU and peak memory from process accounting.
// ---------------------------------------------------------------------------------------------

/// How many clients clone at once, or one after another ([`APART`]).
const CLIENTS: usize = 50;

/// The gap between the starts of two clones that do not overlap.
const APART: Duration = Duration::from_secs(10);

/// How many clones `git fsck` checks at a time.
const FSCK_AT_ONCE: usize = 8;

/// `n` as a float, exact up to 2^32 (a figure beyond that is not one these cases print).
pub(crate) fn float(n: u64) -> f64 {
    f64::from(u32::try_from(n).unwrap_or(u32::MAX))
}

/// A count as a float.
fn count(n: usize) -> f64 {
    float(u64::try_from(n).unwrap_or(u64::MAX))
}

/// A KiB figure in MiB.
pub(crate) fn mib(kib: u64) -> f64 {
    float(kib) / 1024.0
}

/// The CPU time of `spent`, summed.
fn cpu(spent: &[Spent]) -> Duration {
    spent.iter().map(Spent::cpu).sum()
}

/// The largest peak of `spent`, in KiB.
fn peak(spent: &[Spent]) -> u64 {
    spent.iter().map(|one| one.peak_kib).max().unwrap_or(0)
}

/// The median and the largest of `times`.
fn spread(times: &[Duration]) -> (Duration, Duration) {
    let longest = times.iter().copied().max().unwrap_or_default();
    (median(times.to_vec()), longest)
}

/// `count` clones of `side`, the `n`-th started `n * gap` after the first (all together with a
/// zero gap): each one's time and directory.
fn together(
    bench: &Bench,
    side: &Side,
    label: &str,
    count: usize,
    gap: Duration,
) -> Vec<(Duration, PathBuf)> {
    let start = Barrier::new(count);
    thread::scope(|scope| {
        let runs: Vec<_> = (0..count)
            .map(|n| {
                let (start, dir) = (&start, bench.dir(&format!("{label}-{n}")));
                scope.spawn(move || {
                    start.wait();
                    thread::sleep(gap * u32::try_from(n).expect("a small count"));
                    (timed(|| clone_into(side, &dir)), dir)
                })
            })
            .collect();
        runs.into_iter()
            .map(|run| run.join().expect("a clone ran"))
            .collect()
    })
}

/// Every clone in `dirs` passes `git fsck --strict` and names the commit `expected` as
/// `origin/main`; the directories are removed after.
fn check_clones(side: &Side, dirs: &[PathBuf], expected: &str) {
    for chunk in dirs.chunks(FSCK_AT_ONCE) {
        thread::scope(|scope| {
            for dir in chunk {
                scope.spawn(move || {
                    side.run(dir, &["fsck", "--strict"]);
                    assert_eq!(side.rev(dir, "origin/main"), expected, "a clone differs");
                });
            }
        });
    }
    for dir in dirs {
        fs::remove_dir_all(dir).expect("remove a checked clone");
    }
}

/// Prints one `serve-perf: ` line for 50 clones: `server` is the figures of the served side, the
/// accounted CPU and peak memory of its connections; `one` is one clone alone.
fn print_clones(
    what: &str,
    serve: &[Duration],
    git: Option<&[Duration]>,
    spent: &[Spent],
    one: (Duration, Duration),
) {
    let (median_serve, longest_serve) = spread(serve);
    let against = git.map_or_else(String::new, |times| {
        let (median_git, longest_git) = spread(times);
        format!(
            "; git upload-pack median {:.3} s, longest {:.3} s",
            median_git.as_secs_f64(),
            longest_git.as_secs_f64()
        )
    });
    println!(
        "serve-perf: {CLIENTS} clones {what} on {LONG} commits: swarmops serve median {:.3} s, \
         longest {:.3} s{against}; server CPU {:.3} s in all over {} connections, {:.3} s \
         against one clone's {:.3} s (x{:.2} of {CLIENTS} times it); peak memory {:.1} MiB",
        median_serve.as_secs_f64(),
        longest_serve.as_secs_f64(),
        cpu(spent).as_secs_f64(),
        spent.len(),
        cpu(spent).as_secs_f64() / count(spent.len().max(1)),
        one.0.as_secs_f64(),
        cpu(spent).as_secs_f64() / (one.0.as_secs_f64() * count(CLIENTS)).max(0.001),
        mib(peak(spent)),
    );
}

/// 50 clones at once, and 50 clones 10 s apart, from `swarmops serve`: the server's CPU of each
/// against one clone's, and each clone's time against git's `upload-pack` under the same 50
/// (R-3).
#[test]
#[ignore = "slow, ~20 min (estimate): imports 12,000 commits, 150 clones, run by `just perf`"]
fn fifty_clones_at_once_and_apart() {
    let bench = Bench::build(LONG, Start::Seeded);
    let (serve, git) = (bench.serve(), bench.git());

    // 1. One clone alone, after a clone that pays the first refresh: its CPU is the unit.
    let first = timed(|| clone_into(&serve, &bench.dir("first")));
    let mark = bench.mark();
    let solo_dir = bench.dir("solo");
    let solo = timed(|| clone_into(&serve, &solo_dir));
    let one = (cpu(&bench.spent(mark)), solo);
    let expected = serve.rev(&solo_dir, "origin/main");
    println!(
        "serve-perf: first clone on {LONG} commits from a seeded serve.git: {:.3} s; one clone \
         alone: {:.3} s, server CPU {:.3} s",
        first.as_secs_f64(),
        solo.as_secs_f64(),
        one.0.as_secs_f64()
    );
    check_clones(&serve, &[bench.dir("first"), solo_dir], &expected);

    // 2. 50 at once: from serve, then from git's upload-pack.
    let mark = bench.mark();
    let at_once = together(&bench, &serve, "serve-at-once", CLIENTS, Duration::ZERO);
    let spent = bench.spent(mark);
    let git_once = together(&bench, &git, "git-at-once", CLIENTS, Duration::ZERO);
    let serve_times: Vec<_> = at_once.iter().map(|(time, _)| *time).collect();
    let git_times: Vec<_> = git_once.iter().map(|(time, _)| *time).collect();
    assert!(
        spent.len() >= CLIENTS,
        "{} connections were accounted",
        spent.len()
    );
    print_clones(
        "at once",
        &serve_times,
        Some(&git_times),
        &spent,
        (one.0, solo),
    );
    let dirs: Vec<_> = at_once.into_iter().map(|(_, dir)| dir).collect();
    check_clones(&serve, &dirs, &expected);
    let dirs: Vec<_> = git_once.into_iter().map(|(_, dir)| dir).collect();
    check_clones(&git, &dirs, &expected);

    // 3. 50 started 10 s apart: each finds the server idle, so each costs about one clone.
    let mark = bench.mark();
    let apart = together(&bench, &serve, "serve-apart", CLIENTS, APART);
    let spent = bench.spent(mark);
    let times: Vec<_> = apart.iter().map(|(time, _)| *time).collect();
    assert!(
        spent.len() >= CLIENTS,
        "{} connections were accounted",
        spent.len()
    );
    print_clones("10 s apart", &times, None, &spent, (one.0, solo));
    let dirs: Vec<_> = apart.into_iter().map(|(_, dir)| dir).collect();
    check_clones(&serve, &dirs, &expected);
}

/// The k-th land of a run on the store: one new file, landed onto `main` in one call.
fn land_one(bench: &Bench, n: usize) {
    let path = format!("lands/f{n}.txt");
    let list = format!(r#"[{{"path":"{path}","op":"put","content":{{"inline":"land {n}\n"}}}}]"#);
    let changes = bench.dir(&format!("changes-{n}.json"));
    fs::write(&changes, list).expect("write a change list");
    let store = bench.store();
    let (store, changes) = (path_str(&store), path_str(&changes));
    let (message, key) = (format!("land {n}"), format!("perf-land-{n}"));
    let line = [
        "--repo",
        store,
        "--as",
        WRITER,
        "land",
        "main",
        "--changes",
        changes,
        "-m",
        &message,
        "--key",
        &key,
    ];
    support::run_within(&line, &[], SLOW_RUN).ok();
}

/// `landed` lands behind: the client at `dir` fetches. The time, the server's CPU for it, the
/// bytes of the pack it received, and the commits on `origin/main` after.
fn fetch_behind(bench: &Bench, dir: &Path, landed: usize) -> (Duration, Vec<Spent>, u64, usize) {
    let side = bench.serve();
    let trace = bench.dir(&format!("pack-{landed}-{}", bench.mark()));
    let mark = bench.mark();
    let time = timed(|| {
        side.run_env(
            dir,
            &["fetch", "--quiet", "origin"],
            &[("GIT_TRACE_PACKFILE", trace.as_os_str())],
        );
    });
    let bytes = fs::metadata(&trace).map_or(0, |meta| meta.len());
    let counted = side.run(dir, &["rev-list", "--count", "origin/main"]);
    let commits = String::from_utf8_lossy(&counted.stdout)
        .trim()
        .parse()
        .expect("a count");
    (time, bench.spent(mark), bytes, commits)
}

/// A fetch 1 land behind on 1,000 and 12,000 commits, and on 12,000 the server's CPU per byte
/// sent 1, 10, 100 and 1,000 lands behind (R-5).
#[test]
#[ignore = "slow, ~15 min (estimate): imports 13,000 commits twice over, 1,111 lands, run by `just perf`"]
fn fetch_k_behind() {
    for (commits, steps) in [(1_000, vec![1]), (LONG, vec![1, 10, 100, 1_000])] {
        let bench = Bench::build(commits, Start::Seeded);
        let side = bench.serve();
        let dir = bench.dir("client");
        clone_into(&side, &dir);
        let mut landed = 0;

        // 1. k lands behind, a fetch, and the figures.
        for k in steps {
            for n in landed..landed + k {
                land_one(&bench, n);
            }
            landed += k;
            let (time, spent, bytes, seen) = fetch_behind(&bench, &dir, k);
            assert_eq!(
                seen,
                commits + landed,
                "the fetch did not take the {k} lands"
            );
            let cpu = cpu(&spent);
            println!(
                "serve-perf: fetch {k} lands behind on {commits} commits: {:.3} s, server CPU \
                 {:.3} s, {bytes} bytes sent, {:.3} ms of CPU per KiB; peak memory {:.1} MiB",
                time.as_secs_f64(),
                cpu.as_secs_f64(),
                cpu.as_secs_f64() * 1000.0 / (float(bytes) / 1024.0).max(1.0),
                mib(peak(&spent)),
            );
        }

        // 2. What it fetched is whole.
        side.run(&dir, &["fsck", "--strict"]);
    }
}

/// How big one pushed blob is.
const BLOB: usize = 1 << 20;

/// The blobs of the push both servers take: 1 GiB.
const ONE_GIB_BLOBS: usize = 1_024;

/// The most blobs of 1 MiB that fit `receive.maxInputSize` = `MAX_PUSH_BYTES` (2 GiB) with their
/// pack's headers.
const TWO_GIB_BLOBS: usize = 2_040;

/// One blob more than git's input bound takes: 2 GiB of blobs and the headers.
const OVER_BOUND_BLOBS: usize = 2_048;

/// Writes `count` files of 1 MiB of random bytes under `dir`'s `big/<tag>/`, and commits them.
fn commit_blobs(side: &Side, dir: &Path, tag: &str, count: usize) {
    let random = fs::File::open("/dev/urandom");
    let mut random = random.expect("open /dev/urandom");
    let under = dir.join("big").join(tag);
    fs::create_dir_all(&under).expect("create the blobs' directory");
    let mut bytes = vec![0_u8; BLOB];
    for n in 0..count {
        random.read_exact(&mut bytes).expect("read random bytes");
        fs::write(under.join(format!("b{n:04}")), &bytes).expect("write a blob");
    }
    side.run(dir, &["add", "--", &format!("big/{tag}")]);
    side.run(dir, &["commit", "--quiet", "-m", tag]);
}

/// A push of one commit of `count` blobs to `refs/land/main`: the time, what the server spent
/// on it, and whether it was taken.
fn push_blobs(
    bench: &Bench,
    side: &Side,
    dir: &Path,
    tag: &str,
    count: usize,
) -> (Duration, Vec<Spent>, bool) {
    commit_blobs(side, dir, tag, count);
    let mark = bench.mark();
    let started = Instant::now();
    let mut command = bench.site.command();
    let output = command
        .env("GIT_SSH_COMMAND", &side.ssh)
        .current_dir(dir)
        .args(["push", "origin", LAND_MAIN])
        .output()
        .expect("run git push");
    let time = started.elapsed();
    (time, bench.spent(mark), output.status.success())
}

/// The commit `refs/heads/main` of the server names, from the client at `dir`.
fn served_main(side: &Side, dir: &Path) -> String {
    let output = side.run(dir, &["ls-remote", "origin", "refs/heads/main"]);
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// Pushes of 1 GiB (1,024 blobs) and 2 GiB (2,040 blobs) of incompressible 1 MiB objects in one
/// commit, and one of 2,048 blobs that git's input bound refuses and that moves nothing: the
/// server's peak resident memory and time for each (R-6, G22).
#[test]
#[ignore = "slow, ~25 min (estimate): pushes 1, 2 and 2 GiB, run by `just perf`"]
fn push_one_and_two_gib() {
    let bench = Bench::build(200, Start::Seeded);
    let side = bench.serve();
    let dir = bench.dir("work");
    side.run(bench.work.path(), &["clone", "--quiet", &side.url, "work"]);

    // 1. The two pushes under the bound land.
    for (tag, count) in [("one-gib", ONE_GIB_BLOBS), ("two-gib", TWO_GIB_BLOBS)] {
        let (time, spent, accepted) = push_blobs(&bench, &side, &dir, tag, count);
        assert!(accepted, "the push of {count} blobs was refused");
        println!(
            "serve-perf: push of {count} blobs of 1 MiB: {:.3} s, server peak memory {:.1} MiB, \
             server CPU {:.3} s",
            time.as_secs_f64(),
            mib(peak(&spent)),
            cpu(&spent).as_secs_f64(),
        );

        // The client takes the landed commit; main names it.
        side.run(&dir, &["fetch", "--quiet", "origin"]);
        side.run(&dir, &["reset", "--quiet", "--hard", "origin/main"]);
        assert_eq!(side.rev(&dir, "origin/main"), side.rev(&dir, "HEAD"));
    }

    // 2. One over the bound is refused and moves nothing.
    let before = served_main(&side, &dir);
    let (time, spent, accepted) = push_blobs(&bench, &side, &dir, "over", OVER_BOUND_BLOBS);
    assert!(!accepted, "the push of {OVER_BOUND_BLOBS} blobs was taken");
    assert_eq!(
        served_main(&side, &dir),
        before,
        "a refused push moved main"
    );
    println!(
        "serve-perf: push of {OVER_BOUND_BLOBS} blobs of 1 MiB (refused): {:.3} s, server peak \
         memory {:.1} MiB",
        time.as_secs_f64(),
        mib(peak(&spent)),
    );

    // 3. The store is whole.
    let store = bench.store();
    support::run_within(
        &["--repo", path_str(&store), "verify", "--deep"],
        &[],
        SLOW_RUN,
    )
    .ok();
}

/// The loose objects of the git directory `git_dir`.
fn loose_objects(git_dir: &Path) -> usize {
    let mut count = 0;
    for entry in fs::read_dir(git_dir.join("objects")).expect("read objects/") {
        let entry = entry.expect("an entry");
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.len() == 2 && entry.path().is_dir() {
            count += fs::read_dir(entry.path()).expect("read a fan-out").count();
        }
    }
    count
}

/// A `serve.git` that holds the history's objects before the first refresh keeps them as a pack:
/// the first clone writes no loose object, and the clone is whole (so the accounting's
/// `times` line is not on the transport).
#[test]
fn seeded_serve_git_exports_nothing() {
    let bench = Bench::build(200, Start::Seeded);
    let serve = bench.serve();
    let git_dir = bench.store().join("git/serve.git");
    assert_eq!(loose_objects(&git_dir), 0, "the seed left loose objects");

    // One connection: the refresh finds the objects there.
    let dir = bench.dir("clone");
    let first = timed(|| clone_into(&serve, &dir));
    serve.run(&dir, &["fsck", "--strict"]);
    let (git, git_dir_clone) = (bench.git(), bench.dir("git-clone"));
    clone_into(&git, &git_dir_clone);
    assert_eq!(
        serve.rev(&dir, "origin/main"),
        git.rev(&git_dir_clone, "origin/main"),
        "the two servers serve different commits"
    );
    assert_eq!(loose_objects(&git_dir), 0, "the refresh wrote objects");
    assert!(!bench.spent(0).is_empty(), "no connection was accounted");
    println!(
        "serve-perf: first clone on 200 seeded commits: {:.3} s",
        first.as_secs_f64()
    );
}
