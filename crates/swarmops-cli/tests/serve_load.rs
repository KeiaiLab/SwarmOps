//! Served git traffic under a swarm's load (the M17 design §7, G21, and G20's first clone beside
//! landers), for the `perf` recipe.
//!
//! 50 git clients each push a branch of their own and fetch `main` every 2 s for 10 minutes
//! (testkit's `served_load::GitClient`): through `swarmops serve` beside 50 agents that land on
//! the same store at G4's rate (`served_traffic_under_load`), and against git's own server
//! (`git_traffic_under_load`), two cases so that each stays under half the `ci-perf` kill. Every
//! command's attempts are counted per 10 s window: successes, refusals with a retryable code and
//! a hint, other failures. Before the clients, while the agents land, one first clone of the
//! served store (12,000 commits imported and prepared) is timed against git's own clone of the
//! bare copy.
//!
//! ```text
//!   fast-import ─▶ repo.git ◀── git upload-pack / receive-pack ◀── 50 clients (git side)
//!                     │ git import, serve --prepare --seed-from
//!   store repo ─▶ git/serve.git ◀── swarmops serve ◀── 50 clients (serve side)
//!        ▲
//!        └── 50 agents: land --changes --key, one every 2 s each
//! ```
//!
//! Asserted: no other failure in any window, every acknowledged push's branch at its commit,
//! every land on `main` once and the store clean (`verify --deep`), every clone `git fsck
//! --strict`. Recorded, never asserted: the first-attempt share (G21's 99.9 %), push and fetch
//! p99 (G21's 2× git's), the first clone's ratio (G20's 2×). `serve-load: ` lines; docs/perf.md
//! records the verdicts.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Barrier;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_core::{ChangesetId, ObjectId};
use swarmops_git::serve::repo::ServeRepo;
use swarmops_ops::LocalOps;
use swarmops_ops::serve::refresh::{Freshness, freshness};
use swarmops_testkit::landing::{
    Change, CliLander, LandRecord, Stats, Workload, base_files, check_invariants, flagged, load1,
    slot,
};
use swarmops_testkit::remote::{GitServer, SshMode};
use swarmops_testkit::serve::ServeSite;
use swarmops_testkit::served_load::{GitClient, Server, Step, Traffic, first_attempt};
use tempfile::TempDir;

use crate::serve_perf::fast_import;
use crate::support::{self, Store};

/// How many git clients run at once.
const CLIENTS_VAR: &str = "SWARMOPS_TEST_SERVE_LOAD_CLIENTS";

/// How many agents land beside them.
const LANDERS_VAR: &str = "SWARMOPS_TEST_SERVE_LOAD_LANDERS";

/// How long the clients run, in seconds.
const DURATION_VAR: &str = "SWARMOPS_TEST_SERVE_LOAD_DURATION_S";

/// How many commits the history holds.
const COMMITS_VAR: &str = "SWARMOPS_TEST_SERVE_LOAD_COMMITS";

/// Each client's and each agent's interval: G4's pace.
const INTERVAL: Duration = Duration::from_secs(2);

/// The windows the commands are counted in (the M17 design §7).
const WINDOW: Duration = Duration::from_secs(10);

/// How long the agents land before the first clone is timed: past their phases, at full pace.
const LEAD: Duration = Duration::from_secs(10);

/// The seed of the schedules.
const SEED: u64 = 1;

/// How a summary line starts.
const TAG: &str = "serve-load: ";

/// How a window's line starts.
const WINDOW_TAG: &str = "serve-load-window: ";

/// The repository's name on both sides.
const REPO: &str = "repo";

/// The agent that imports the history and commits the agents' base.
const WRITER: &str = "agent/worker-importer";

/// The served store's access file: every person reads, pushes `release/*` and lands onto
/// `main`.
const ACCESS: &str = r#"version = 1

[[grant]]
who = ["person/*"]
read = true
push = ["release/*"]
land = ["main"]
"#;

/// How long the import and the preparation may take: a loaded host takes minutes.
const SLOW_RUN: Duration = Duration::from_secs(3_600);

/// How long the detached preparations may take, after the last move, to leave `serve.git`
/// current: far past one refresh, for a loaded host.
const PREPARED_WITHIN: Duration = Duration::from_secs(120);

/// How often `serve.git` is looked at while waiting.
const POLL: Duration = Duration::from_millis(100);

/// The mark a preparation that found `serve.lock` held leaves for its holder.
const SERVE_DIRTY: &str = "git/serve.dirty";

/// The file every export writes beside `serve.git`.
const SERVE_STATE: &str = "git/serve.state";

/// How many clones `git fsck` checks at a time.
const FSCK_AT_ONCE: usize = 8;

/// The case's shape, from the variables.
#[derive(Debug, Clone, Copy)]
struct Shape {
    /// The git clients.
    clients: usize,
    /// The agents beside them (serve's side only).
    landers: usize,
    /// How long the clients run.
    duration: Duration,
    /// The history's commits.
    commits: usize,
}

impl Shape {
    /// The shape the variables give, G21's by default.
    fn from_env() -> Shape {
        let var = |name: &str, default: u64| -> u64 {
            std::env::var(name).map_or(default, |n| n.parse().expect("a number"))
        };
        let count = |name: &str, default: u64| usize::try_from(var(name, default)).expect("fits");
        Shape {
            clients: count(CLIENTS_VAR, 50),
            landers: count(LANDERS_VAR, 50),
            duration: Duration::from_secs(var(DURATION_VAR, 600)),
            commits: count(COMMITS_VAR, 12_000),
        }
    }
}

/// Which server a lane's clients reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    /// `swarmops serve`, beside the agents.
    Serve,
    /// git's own server.
    Git,
}

impl Side {
    /// Its name on the lines.
    fn name(self) -> &'static str {
        match self {
            Side::Serve => "serve",
            Side::Git => "git",
        }
    }
}

/// `path` as text.
fn path_str(path: &Path) -> &str {
    path.to_str().expect("UTF-8 paths")
}

/// `command` run, which must succeed.
fn succeed(command: &mut Command) {
    let output = command.output().expect("run git");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{command:?}: {stderr}");
}

/// Times `work`.
fn timed(work: impl FnOnce()) -> Duration {
    let start = Instant::now();
    work();
    start.elapsed()
}

/// `git clone --no-checkout` of `url` into `dir` through `ssh`, with `server`'s git.
fn clone_into(server: Server<'_>, ssh: &str, url: &str, dir: &Path) {
    let mut command = git(server);
    command.env("GIT_SSH_COMMAND", ssh).args([
        "clone",
        "--quiet",
        "--no-checkout",
        url,
        path_str(dir),
    ]);
    succeed(&mut command);
}

/// A git command in `server`'s pinned client environment.
fn git(server: Server<'_>) -> Command {
    match server {
        Server::Serve(site) => site.command(),
        Server::Git(bare) => bare.command(),
    }
}

/// `count` client clones under `work`, each a local clone of `first` whose `origin` is `url`:
/// cheap copies of one served clone, so that every client starts at the trunk it served.
fn client_dirs(
    server: Server<'_>,
    first: &Path,
    url: &str,
    work: &Path,
    count: usize,
) -> Vec<PathBuf> {
    (0..count)
        .map(|n| {
            let dir = work.join(format!("client-{n}"));
            let mut local = git(server);
            local.args([
                "clone",
                "--quiet",
                "--no-checkout",
                "--local",
                path_str(first),
            ]);
            succeed(local.arg(&dir));
            let mut remote = git(server);
            remote
                .current_dir(&dir)
                .args(["remote", "set-url", "origin", url]);
            succeed(&mut remote);
            dir
        })
        .collect()
}

/// Every clone of `dirs` passes `git fsck --strict`, [`FSCK_AT_ONCE`] at a time.
fn fsck_all(server: Server<'_>, dirs: &[PathBuf]) {
    for chunk in dirs.chunks(FSCK_AT_ONCE) {
        thread::scope(|scope| {
            for dir in chunk {
                scope.spawn(move || {
                    let mut fsck = git(server);
                    succeed(fsck.current_dir(dir).args(["fsck", "--strict"]));
                });
            }
        });
    }
}

/// The agents' base on `main` of `store` ([`base_files`] for `landers` agents), committed on its
/// head with `--advance main`: its id.
fn commit_base(store: &Store, landers: usize) -> String {
    let files = base_files(landers, Workload::Shared);
    let puts: Vec<(&str, &str)> = (files.iter())
        .map(|(path, content)| (path.as_str(), std::str::from_utf8(content).expect("text")))
        .collect();
    let changes = store.changes_of("base", &puts);
    let args = [
        "commit",
        "-m",
        "base",
        "--changes",
        &changes,
        "--advance",
        "main",
    ];
    let made = store.run_as(WRITER, &args, &[]).ok();
    made["changeset"]
        .as_str()
        .expect("the base's id")
        .to_owned()
}

/// Agent `lander`'s land `land`: a file of its own.
fn change(lander: usize, land: usize) -> Change {
    Change {
        puts: vec![(
            format!("agents/{lander}/land-{land}.txt"),
            format!("lander {lander} land {land}\n").into_bytes(),
        )],
        registry_line: None,
    }
}

/// Agent `lander`'s lands from `go`, one at each of its slots, until `stop` is set.
fn land_until(
    cli: &CliLander<'_, support::Swarmops>,
    lander: usize,
    go: Instant,
    stop: &AtomicBool,
) -> Vec<LandRecord> {
    let mut records = Vec::new();
    for land in 0.. {
        let due = go + slot(SEED, lander, land, INTERVAL);
        thread::sleep(due.saturating_duration_since(Instant::now()));
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let at = u64::try_from(go.elapsed().as_micros()).unwrap_or(u64::MAX);
        let mut record = cli.land(land, &change(lander, land));
        record.at_us = at;
        records.push(record);
    }
    records
}

/// Runs the preparation `args` to its end, again while it meets `serve.lock` held (`E_BUSY`)
/// for up to [`SLOW_RUN`]: a detached preparation (after the import, a seeded fetch and a full
/// repack) can hold it past one 10 s wait.
fn prepare_once(args: &[&str]) {
    let deadline = Instant::now() + SLOW_RUN;
    loop {
        let run = support::run_within(args, &[], SLOW_RUN);
        if !run.busy() || Instant::now() >= deadline {
            run.ok();
            return;
        }
    }
}

/// Waits, at most [`PREPARED_WITHIN`], until the store at `root` holds no preparation mark and
/// its `serve.git` is current.
fn wait_prepared(root: &Utf8Path) {
    let deadline = Instant::now() + PREPARED_WITHIN;
    let current = || {
        let ops = LocalOps::open(root).expect("open the store");
        let repo = ServeRepo::open(root).expect("open serve.git");
        matches!(
            freshness(&ops, &repo).expect("read the freshness"),
            Freshness::Current
        )
    };
    while root.join(SERVE_DIRTY).exists() || !root.join(SERVE_STATE).exists() || !current() {
        assert!(
            Instant::now() < deadline,
            "serve.git not prepared within {PREPARED_WITHIN:?}"
        );
        thread::sleep(POLL);
    }
}

/// Each client's open loop from one go, all at once; their traffic and the run's wall time.
fn drive(clients: &mut [GitClient<'_>], shape: &Shape) -> (Vec<Traffic>, Duration) {
    let start = Barrier::new(clients.len() + 1);
    thread::scope(|scope| {
        let runs: Vec<_> = (clients.iter_mut().enumerate())
            .map(|(n, client)| {
                let start = &start;
                scope.spawn(move || {
                    start.wait();
                    client.run(Instant::now(), (SEED, n), INTERVAL, shape.duration)
                })
            })
            .collect();
        start.wait();
        let go = Instant::now();
        let traffic = (runs.into_iter())
            .map(|run| run.join().unwrap_or_else(|e| std::panic::resume_unwind(e)))
            .collect();
        (traffic, go.elapsed())
    })
}

/// Prints `side`'s lines for its clients' `traffic` (one per [`Step`], then its windows) and
/// asserts that the windows add up to the run and that none holds another failure.
fn report(side: Side, shape: &Shape, traffic: &[Traffic], load: (f64, f64)) {
    for step in Step::ALL {
        let records: Vec<Vec<LandRecord>> = (traffic.iter())
            .map(|one| match step {
                Step::Push => one.pushes.clone(),
                Step::Fetch => one.fetches.clone(),
            })
            .collect();
        let stats = Stats::from(&records, shape.duration);
        let failed = records.iter().flatten().filter(|r| r.id.is_none()).count();
        println!(
            "{TAG}side={} what={} clients={} landers={} duration_s={} first_attempt_pct={:.3} \
             p99_ms={:.1} {} failed={failed} load_start={:.2} load_end={:.2}",
            side.name(),
            step.name(),
            shape.clients,
            if side == Side::Serve {
                shape.landers
            } else {
                0
            },
            shape.duration.as_secs(),
            first_attempt(&records) * 100.0,
            Duration::from_micros(stats.latency_p99_us).as_secs_f64() * 1000.0,
            stats.summary(),
            load.0,
            load.1,
        );
        assert_windows(side.name(), step.name(), &records, &stats);
    }
}

/// Prints `records`' windows ([`WINDOW`]) as `side`'s `what`, and asserts that their counts add
/// up to `whole`'s and that none is flagged.
fn assert_windows(side: &str, what: &str, records: &[Vec<LandRecord>], whole: &Stats) {
    let windows = Stats::per_window(records, WINDOW);
    for (window, stats) in &windows {
        println!(
            "{WINDOW_TAG}side={side} what={what} start_s={} succeeded={} retryable={} other={} \
             p99_us={}",
            window.start.as_secs(),
            stats.landed,
            stats.retryable,
            stats.other,
            stats.latency_p99_us,
        );
    }
    let sum = |pick: fn(&Stats) -> u64| windows.iter().map(|(_, s)| pick(s)).sum::<u64>();
    let summed = (sum(|s| s.landed), sum(|s| s.retryable), sum(|s| s.other));
    assert_eq!(
        summed,
        (whole.landed, whole.retryable, whole.other),
        "{side} {what}"
    );
    let flags = flagged(&windows);
    println!(
        "{TAG}side={side} what={what} windows={} flagged={}",
        windows.len(),
        flags.len()
    );
    assert!(
        flags.is_empty(),
        "{side} {what}: other failures in {flags:?}"
    );
}

/// Every client's branch, as `origin` lists it, names the commit its last acknowledged push
/// set: no push lost, none made that its client was not told of.
fn assert_acknowledged(clients: &[GitClient<'_>]) {
    let heads = clients
        .first()
        .map(GitClient::remote_heads)
        .unwrap_or_default();
    for client in clients {
        let branch = client.branch();
        assert_eq!(
            heads.get(&branch).map(String::as_str),
            client.acknowledged(),
            "{branch}: the served value is not the last acknowledged push"
        );
    }
}

/// The served side of [`served_traffic_under_load`]: the history on git's side, the store that
/// imported it, prepared, and the agents' base on its `main`.
struct Served {
    /// git's side: the bare copy and its server.
    server: GitServer,
    /// serve's side: the root of stores.
    site: ServeSite,
    /// The served store's root.
    root: Utf8PathBuf,
    /// The agents' handle on it: a link to [`Served::root`].
    store: Store,
    /// The agents' base, in full hex.
    base: String,
}

impl Served {
    /// `shape`'s history on both sides, imported, prepared from the bare copy, and the agents'
    /// base committed and prepared too.
    fn build(shape: &Shape) -> Served {
        // 1. git's side: the history; serve's side: the store and its access file.
        let server = GitServer::new();
        let source = server.create(REPO);
        fast_import(&server, &source, shape.commits);
        let site = ServeSite::new::<support::Swarmops>();
        let path = site.init(REPO);
        fs::write(path.join("access.toml"), ACCESS).expect("write the access file");

        // 2. The same history imported, then prepared from the bare copy.
        let (store_path, bare_path) = (path_str(&path), path_str(&source));
        let import = [
            "--repo", store_path, "--as", WRITER, "git", "import", bare_path, "--ref", "main",
            "--into", "main",
        ];
        support::run_within(&import, &[], SLOW_RUN).ok();
        let prepare = [
            "--repo",
            store_path,
            "serve",
            "--prepare",
            "--seed-from",
            bare_path,
        ];
        prepare_once(&prepare);
        let root = Utf8PathBuf::from(store_path);
        wait_prepared(&root);

        // 3. The agents' handle on the store, and their base on main, prepared too.
        let store = Store::empty();
        std::os::unix::fs::symlink(&path, store.path()).expect("link the agents' store");
        let base = commit_base(&store, shape.landers);
        wait_prepared(&root);
        Served {
            server,
            site,
            root,
            store,
            base,
        }
    }

    /// G20 beside the agents: serve's first clone into `work`, timed against git's own clone of
    /// the bare copy, both checked; serve's clone's directory.
    fn first_clones(&self, shape: &Shape, work: &Path) -> PathBuf {
        let (serve_side, git_side) = (Server::Serve(&self.site), Server::Git(&self.server));
        let (first, git_first) = (work.join("serve-first"), work.join("git-first"));
        let serve_ssh = self.site.ssh_command("person/first");
        let git_ssh = self.server.ssh_command(SshMode::Serve);
        let (url, git_url) = (self.site.ssh_url(REPO), self.server.ssh_url(REPO));
        let serve_time = timed(|| clone_into(serve_side, &serve_ssh, &url, &first));
        let git_time = timed(|| clone_into(git_side, &git_ssh, &git_url, &git_first));
        println!(
            "{TAG}g20 first clone on {} commits beside {} landers: swarmops serve {:.3} s; git \
             upload-pack {:.3} s; ratio {:.2} (target at most 2.00)",
            shape.commits,
            shape.landers,
            serve_time.as_secs_f64(),
            git_time.as_secs_f64(),
            serve_time.as_secs_f64() / git_time.as_secs_f64().max(0.001),
        );
        fsck_all(serve_side, std::slice::from_ref(&first));
        fsck_all(git_side, &[git_first]);
        first
    }

    /// `shape.clients` clients through `serve`, each on a copy of the clone `first` under
    /// `work`, as a person of its own.
    fn clients(&self, shape: &Shape, first: &Path, work: &Path) -> Vec<GitClient<'_>> {
        let side = Server::Serve(&self.site);
        let dirs = client_dirs(side, first, &self.site.ssh_url(REPO), work, shape.clients);
        (dirs.iter().enumerate())
            .map(|(n, dir)| {
                let ssh = self.site.ssh_command(&format!("person/client-{n}"));
                GitClient::new(side, n, dir, &ssh)
            })
            .collect()
    }

    /// The end of the run: every acknowledged push served, every land of `lands` on `main` once
    /// (its time aside), the store clean, every clone whole.
    fn check(&self, lands: &[Vec<LandRecord>], clients: &[GitClient<'_>]) {
        // 1. The pushes, as a last connection sees them; then one preparation after it, so
        //    that no mark a connection's hold left stays and no detached one outlives the case.
        assert_acknowledged(clients);
        let prepare = ["--repo", self.root.as_str(), "serve", "--prepare"];
        prepare_once(&prepare);
        wait_prepared(&self.root);

        // 2. The lands, the store, the clones.
        let base_id = ChangesetId::new(ObjectId::from_hex(&self.base).expect("a changeset id"));
        let untimed: Vec<Vec<LandRecord>> = (lands.iter())
            .map(|of| {
                (of.iter())
                    .map(|r| LandRecord {
                        latency_us: 0,
                        ..r.clone()
                    })
                    .collect()
            })
            .collect();
        if let Err(rule) = check_invariants(&self.root, "main", base_id, &untimed) {
            panic!("{rule}");
        }
        self.store.assert_clean("after the run");
        let dirs: Vec<PathBuf> = clients.iter().map(|c| c.dir().to_owned()).collect();
        fsck_all(Server::Serve(&self.site), &dirs);
    }
}

/// G21's lane through `swarmops serve`: 50 clients beside 50 agents landing on the same store
/// for 10 minutes, after G20's first clone beside the agents (the module's documentation).
#[test]
#[ignore = "slow, perf, ~20 min (estimate; `just perf` measures it): imports 12,000 commits, \
            50 git clients beside 50 landing agents for 10 minutes"]
fn served_traffic_under_load() {
    let shape = Shape::from_env();
    let served = Served::build(&shape);

    // 1. The agents land until the clients are done; the first clones are timed meanwhile,
    //    then the clients run.
    let work = TempDir::new().expect("a temp dir");
    let stop = AtomicBool::new(false);
    let start = Barrier::new(shape.landers + 1);
    let load_start = load1();
    let (lands, clients, traffic, landing) = thread::scope(|scope| {
        let (start, stop, store) = (&start, &stop, &served.store);
        let agents: Vec<_> = (0..shape.landers)
            .map(|lander| {
                scope.spawn(move || {
                    let cli = CliLander::new(store, lander, "main");
                    start.wait();
                    land_until(&cli, lander, Instant::now(), stop)
                })
            })
            .collect();
        start.wait();
        let go = Instant::now();
        thread::sleep(LEAD);

        // 1a. G20 beside the agents, then the clients on copies of serve's clone.
        let first = served.first_clones(&shape, work.path());
        let mut clients = served.clients(&shape, &first, work.path());
        let (traffic, _) = drive(&mut clients, &shape);

        // 1b. The agents stop.
        stop.store(true, Ordering::Relaxed);
        let lands: Vec<Vec<LandRecord>> = (agents.into_iter())
            .map(|agent| {
                agent
                    .join()
                    .unwrap_or_else(|e| std::panic::resume_unwind(e))
            })
            .collect();
        (lands, clients, traffic, go.elapsed())
    });
    let load = (load_start, load1());

    // 2. The clients' lines and windows, the agents' line and windows.
    report(Side::Serve, &shape, &traffic, load);
    let landed = Stats::from(&lands, landing);
    let unlanded = lands.iter().flatten().filter(|r| r.id.is_none()).count();
    println!(
        "{TAG}side=serve what=lands landers={} {} unlanded={unlanded}",
        shape.landers,
        landed.summary()
    );
    assert_windows("serve", "lands", &lands, &landed);

    // 3. What the run must leave.
    served.check(&lands, &clients);
}

/// G21's lane against git's own server: the same 50 clients, for 10 minutes, on the same
/// history (the module's documentation): the push and fetch p99 serve's are held against.
#[test]
#[ignore = "slow, perf, ~11 min (estimate; `just perf` measures it): 12,000 commits, 50 git \
            clients against git's own server for 10 minutes"]
fn git_traffic_under_load() {
    let shape = Shape::from_env();
    let server = GitServer::new();
    let bare = server.create(REPO);
    fast_import(&server, &bare, shape.commits);
    let side = Server::Git(&server);

    // 1. One clone, then the clients on copies of it.
    let work = TempDir::new().expect("a temp dir");
    let first = work.path().join("git-first");
    let (ssh, url) = (server.ssh_command(SshMode::Serve), server.ssh_url(REPO));
    clone_into(side, &ssh, &url, &first);
    let dirs = client_dirs(side, &first, &url, work.path(), shape.clients);
    let mut clients: Vec<GitClient<'_>> = (dirs.iter().enumerate())
        .map(|(n, dir)| GitClient::new(side, n, dir, &ssh))
        .collect();

    // 2. The run, its lines and windows.
    let load_start = load1();
    let (traffic, _) = drive(&mut clients, &shape);
    report(Side::Git, &shape, &traffic, (load_start, load1()));

    // 3. Every acknowledged push there, every clone whole.
    assert_acknowledged(&clients);
    fsck_all(side, &dirs);
}
