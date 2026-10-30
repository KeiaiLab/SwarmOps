//! `swarmops serve`'s measurements (G16), for the `perf` recipe.
//!
//! The same 12,000-commit history is served twice through ssh stand-ins over pipes: by
//! `swarmops serve` from a store, and by git's own `upload-pack` from the bare repository the
//! store imported. Stock git clones and fetches from each; the medians and their ratio print as
//! `serve-perf: ` lines. A third case times one push to `refs/land/main`. Each case asserts only
//! what it ran (the commands succeed, both sides serve the same commits), never the target: a
//! missed target is recorded in `docs/perf.md`, not a failing test, as `remote_perf`'s are. All
//! are ignored in `just check`; `just perf` runs them in a release build.
//!
//! ```text
//!   GitServer: fast-import ──▶ repo.git ◀── git upload-pack (ssh stand-in)  ◀── client git
//!                                 │ git import
//!   ServeSite: store repo ──▶ git/serve.git ◀── swarmops serve (ssh stand-in) ◀── client git
//! ```

use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::{Duration, Instant};

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
        let mut command = match self.server {
            Server::Serve => self.bench.site.command(),
            Server::Git => self.bench.server.command(),
        };
        let output = command
            .env("GIT_SSH_COMMAND", &self.ssh)
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

/// The history served by `swarmops serve` and by git's `upload-pack`, and a directory for
/// clients.
struct Bench {
    /// The bare repository and git's ssh stand-in.
    server: GitServer,
    /// The store's root and serve's ssh stand-in.
    site: ServeSite,
    /// The clients' directories.
    work: TempDir,
}

impl Bench {
    /// [`LONG`] commits on `main` of a bare repository, imported into the store [`REPO`].
    fn new() -> Bench {
        // 1. git's side: the history, written by fast-import.
        let server = GitServer::new();
        let bare = server.create(REPO);
        fast_import(&server, &bare, LONG);

        // 2. serve's side: the store, its access file, the same history imported.
        let site = ServeSite::new::<Swarmops>();
        let store = site.init(REPO);
        fs::write(store.join("access.toml"), ACCESS).expect("write the access file");
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
        support::run_within(&line, &[], SLOW_RUN).ok();
        Bench {
            server,
            site,
            work: TempDir::new().expect("a temp dir"),
        }
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
