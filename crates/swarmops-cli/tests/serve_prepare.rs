//! `serve.git` prepared after every ref move (the M17 design §4.3): an operation that moved a ref
//! of a store holding `access.toml`, through the command line or the MCP server, starts a
//! detached `serve --prepare --prepare-wait zero` once its response is out, so that the next
//! fetch finds `serve.git` current and takes no writer lock. A preparation that fails, or none at
//! all, leaves the operation's envelope and exit code as they were.
//!
//! The detached runs are seen through the files they write: `git/serve.state` current
//! ([`freshness`]), and `git/serve.dirty`, the mark a run that found `serve.lock` held leaves.

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::thread;
use std::time::{Duration, Instant};

use camino::Utf8PathBuf;
use serde_json::{Value, json};
use swarmops_git::serve::repo::{ServeLock, ServeRepo};
use swarmops_ops::LocalOps;
use swarmops_ops::serve::refresh::{Freshness, SERVE_REFRESH_ATTEMPTS, freshness};
use swarmops_store::MetaLock;
use swarmops_testkit::cli::retry_busy;
use swarmops_testkit::git::{GitFixture, fixture};
use swarmops_testkit::serve::ServeSite;
use swarmops_testkit::store::layout_of;
use tempfile::TempDir;

use crate::support::{self, Run, Swarmops};

/// The agent that fetches over ssh.
const AGENT: &str = "person/ada";

/// The agent that writes the served store through the command line and the MCP server.
const WRITER: &str = "agent/worker-1";

/// The store every site serves.
const STORE: &str = "acme";

/// The served store's access file: [`AGENT`] reads.
const ACCESS: &str = r#"version = 1

[[grant]]
who = ["person/ada"]
read = true
"#;

/// The MCP revision these tests open a session in.
const MCP_REVISION: &str = "2025-11-25";

/// How long `serve` waits for `git/serve.lock`, and the store for its writer lock: a fetch that
/// waited for a held writer lock ends after it, refused.
const SERVE_LOCK_WAIT: Duration = Duration::from_secs(10);

/// How long a detached preparation may take to make `serve.git` current: far past its work on
/// these small stores, for a loaded host.
const PREPARED_WITHIN: Duration = Duration::from_secs(60);

/// How long a test watches for a detached preparation that must not start.
const QUIET_FOR: Duration = Duration::from_secs(2);

/// How often the files are looked at while waiting.
const POLL: Duration = Duration::from_millis(50);

/// The lands of [`prepares_coalesce`]'s burst.
const BURST: usize = 20;

/// A directory's mode that lets no preparation write in it, and the one that does again.
const READ_ONLY: u32 = 0o500;
const WRITABLE: u32 = 0o755;

/// The mark a preparation that found `serve.lock` held leaves for its holder.
const SERVE_DIRTY: &str = "git/serve.dirty";

/// A served root with the store [`STORE`] under [`ACCESS`], the `linear` fixture imported as
/// `main`, and a directory for clients.
struct Site {
    /// The root, its stand-ins and the client git.
    served: ServeSite,
    /// The clients' working directories and the change lists.
    work: TempDir,
    /// The `linear` fixture, which `git import` reads, and the preparation after it.
    linear: GitFixture,
}

impl Site {
    /// The store, served, with `main` imported before [`ACCESS`] is written: no preparation
    /// runs before the test's first connection.
    fn new() -> Site {
        let site = Site::unserved();
        site.grant();
        site
    }

    /// The store, with `main` imported, and no access file.
    fn unserved() -> Site {
        let site = Site::empty();
        site.import();
        site
    }

    /// The store, empty, and no access file.
    fn empty() -> Site {
        let served = ServeSite::new::<Swarmops>();
        served.init(STORE);
        Site {
            served,
            work: TempDir::new().unwrap(),
            linear: fixture("linear"),
        }
    }

    /// The `linear` fixture imported as `main`.
    fn import(&self) {
        let source = self.linear.repo.as_deref().unwrap().to_str().unwrap();
        self.so(&["git", "import", source, "--ref", "HEAD", "--into", "main"]);
    }

    /// The store's path.
    fn store(&self) -> String {
        self.served.store(STORE).to_str().unwrap().to_owned()
    }

    /// Writes [`ACCESS`] as the store's access file.
    fn grant(&self) {
        fs::write(self.served.store(STORE).join("access.toml"), ACCESS).unwrap();
    }

    /// `args` on the store as [`WRITER`]: the run.
    fn run(&self, args: &[&str]) -> Run {
        let store = self.store();
        let line: Vec<&str> = ["--repo", &store, "--as", WRITER]
            .iter()
            .chain(args)
            .copied()
            .collect();
        support::run(&line, &[])
    }

    /// `args` on the store as [`WRITER`], which must succeed; its result.
    fn so(&self, args: &[&str]) -> Value {
        self.run(args).ok()
    }

    /// A change list that puts `path`, in a file of the work directory: the file's path.
    fn changes(&self, path: &str) -> String {
        let file = self.work.path().join(format!("{path}.json"));
        let list = json!([{"path": path, "op": "put", "content": {"inline": format!("{path}\n")}}]);
        fs::write(&file, list.to_string()).unwrap();
        file.to_str().unwrap().to_owned()
    }

    /// `path` put on `main` and landed there in one call, as [`WRITER`]: the run.
    fn land(&self, path: &str) -> Run {
        let changes = self.changes(path);
        self.run(&[
            "land",
            "main",
            "--base",
            "main",
            "--target",
            "main",
            "-m",
            path,
            "--changes",
            &changes,
        ])
    }

    /// The store cloned into `name` over ssh as [`AGENT`]: the clone's directory.
    fn clone(&self, name: &str) -> PathBuf {
        let url = self.served.ssh_url(STORE);
        self.git(self.work.path(), &["clone", "--quiet", &url, name]);
        self.work.path().join(name)
    }

    /// git with `args` in `dir`, as [`AGENT`] over ssh: what it did.
    fn git_output(&self, dir: &Path, args: &[&str]) -> Output {
        self.served
            .command()
            .env("GIT_SSH_COMMAND", self.served.ssh_command(AGENT))
            .current_dir(dir)
            .args(args)
            .output()
            .unwrap()
    }

    /// [`Site::git_output`], which must succeed: its stdout.
    fn git(&self, dir: &Path, args: &[&str]) -> String {
        let output = self.git_output(dir, args);
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            text(&output.stderr)
        );
        text(&output.stdout)
    }

    /// The commit `rev` names in the client at `dir`.
    fn rev(&self, dir: &Path, rev: &str) -> String {
        self.git(dir, &["rev-parse", "--verify", rev])
            .trim()
            .to_owned()
    }

    /// The commit `serve.git`'s `main` names.
    fn served_main(&self) -> String {
        let output = self
            .served
            .command()
            .arg("--git-dir")
            .arg(self.serve_git())
            .args(["rev-parse", "--verify", "refs/heads/main"])
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", text(&output.stderr));
        text(&output.stdout).trim().to_owned()
    }

    /// The store's `serve.git`.
    fn serve_git(&self) -> PathBuf {
        self.served.store(STORE).join("git/serve.git")
    }

    /// The store's serving repository, opened.
    fn serve_repo(&self) -> ServeRepo {
        ServeRepo::open(&Utf8PathBuf::from(self.store())).expect("open serve.git")
    }

    /// Whether `git/serve.dirty` is there.
    fn dirty(&self) -> bool {
        self.served.store(STORE).join(SERVE_DIRTY).exists()
    }

    /// `serve.lock`, held by the test until dropped.
    fn hold_serve_lock(&self) -> ServeLock {
        self.serve_repo()
            .lock(SERVE_LOCK_WAIT)
            .expect("take serve.lock")
    }

    /// Whether `serve.git` is current for the store as it is now, read without a lock.
    fn current(&self) -> bool {
        let ops = LocalOps::open(&Utf8PathBuf::from(self.store())).expect("open the store");
        let fresh = freshness(&ops, &self.serve_repo()).expect("read the freshness");
        matches!(fresh, Freshness::Current)
    }

    /// Waits, at most [`PREPARED_WITHIN`], until `serve.git` is current and no mark is left.
    fn wait_current(&self) {
        let deadline = Instant::now() + PREPARED_WITHIN;
        while self.dirty() || !self.current() {
            assert!(
                Instant::now() < deadline,
                "serve.git not prepared within {PREPARED_WITHIN:?} (dirty: {})",
                self.dirty()
            );
            thread::sleep(POLL);
        }
    }

    /// Waits, at most [`PREPARED_WITHIN`], until `git/serve.dirty` is there.
    fn wait_dirty(&self) {
        let deadline = Instant::now() + PREPARED_WITHIN;
        while !self.dirty() {
            assert!(
                Instant::now() < deadline,
                "no preparation marked serve.git within {PREPARED_WITHIN:?}"
            );
            thread::sleep(POLL);
        }
    }

    /// Watches for [`QUIET_FOR`] that no `git/serve.dirty` appears: no preparation started
    /// while the test holds `serve.lock`.
    fn assert_quiet(&self, what: &str) {
        let deadline = Instant::now() + QUIET_FOR;
        while Instant::now() < deadline {
            assert!(!self.dirty(), "{what}: a preparation started");
            thread::sleep(POLL);
        }
    }

    /// A `git fetch` in `dir` while the test holds the store's writer lock: it succeeds, and
    /// ends inside [`SERVE_LOCK_WAIT`].
    fn fetch_while_locked(&self, dir: &Path) {
        let root = Utf8PathBuf::from(self.store());
        let held = MetaLock::acquire(&layout_of(root), Duration::from_secs(1)).expect("meta/lock");
        let started = Instant::now();
        let output = self.git_output(dir, &["fetch", "--quiet"]);
        let took = started.elapsed();
        drop(held);
        assert!(
            output.status.success(),
            "a fetch behind the writer lock: {}",
            text(&output.stderr)
        );
        assert!(took < SERVE_LOCK_WAIT, "the fetch took {took:?}");
    }
}

/// Every directory under `dir`, `dir` included, given `mode`.
fn chmod_dirs(dir: &Path, mode: u32) {
    let mut dirs = vec![dir.to_owned()];
    while let Some(dir) = dirs.pop() {
        fs::set_permissions(&dir, fs::Permissions::from_mode(mode)).unwrap();
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                dirs.push(path);
            }
        }
    }
}

/// The loose objects under `objects`: files of its two-hex-digit directories.
fn loose(objects: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in fs::read_dir(objects).unwrap() {
        let dir = entry.unwrap().path();
        let name = dir.file_name().and_then(|name| name.to_str()).unwrap_or("");
        if name.len() != 2 || !name.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        found.extend(
            fs::read_dir(&dir)
                .unwrap()
                .map(|entry| entry.unwrap().path()),
        );
    }
    found
}

/// `bytes` as text.
fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// A land on a served store is prepared before the next fetch: once the detached run is done,
/// `serve.git` is current, and a fetch ends while the test holds `meta/lock`, with the land.
#[test]
fn a_local_land_is_prepared_before_the_next_fetch() {
    let site = Site::new();
    let clone = site.clone("local");
    site.wait_current();

    site.land("local.txt").ok();
    site.wait_current();
    site.fetch_while_locked(&clone);
    assert_eq!(site.rev(&clone, "origin/main"), site.served_main());
    assert_eq!(
        site.git(&clone, &["show", "origin/main:local.txt"]),
        "local.txt\n"
    );
}

/// A land through the MCP server is prepared the same way, after its response line.
#[test]
fn an_mcp_land_is_prepared() {
    let site = Site::new();
    let clone = site.clone("mcp");
    site.wait_current();

    // 1. One land with its changes, in a session of its own.
    let changes = json!([{"path": "mcp.txt", "op": "put", "content": {"inline": "mcp\n"}}]);
    let messages = [
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": MCP_REVISION,
            "capabilities": {},
            "clientInfo": {"name": "prepare-test", "version": "1"},
        }}),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
            "name": "land",
            "arguments": {"head": "main", "base": "main", "target": "main",
                          "message": "mcp", "changes": changes},
        }}),
    ];
    let lines: Vec<String> = messages.iter().map(Value::to_string).collect();
    let input = lines.join("\n") + "\n";
    let store = site.store();
    let line = ["mcp", "--repo", &store, "--as", WRITER];
    let run = support::run_bin(support::BIN, &line, &[], &input);
    assert_eq!(run.exit(), 0, "{run}");
    let landed: Value = run
        .stdout
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|reply| reply["id"] == 2)
        .unwrap_or_else(|| panic!("a tools/call reply: {run}"));
    assert_eq!(landed["result"]["isError"], false, "{run}");

    // 2. Prepared: the next fetch takes no writer lock.
    site.wait_current();
    site.fetch_while_locked(&clone);
    assert_eq!(site.git(&clone, &["show", "origin/main:mcp.txt"]), "mcp\n");
}

/// Lands in a burst, each starting a preparation: every land is acknowledged (one that met
/// `E_BUSY` behind the others runs again, as an agent's would), a preparation that
/// runs beside them stops after [`SERVE_REFRESH_ATTEMPTS`] rounds at most, and once they are done
/// `serve.git` is current with the last land.
#[test]
fn prepares_coalesce() {
    let site = Site::new();
    let clone = site.clone("burst");
    site.wait_current();

    // 1. The burst, and an operator's preparation beside it.
    let store = site.store();
    let (lands, prepared) = thread::scope(|scope| {
        let lands: Vec<_> = (0..BURST)
            .map(|n| {
                let site = &site;
                let path = format!("burst-{n}.txt");
                scope.spawn(move || retry_busy(site.land(&path), || site.land(&path)))
            })
            .collect();
        let line = ["--repo", store.as_str(), "serve", "--prepare"];
        let prepared = support::run(&line, &[]);
        let lands: Vec<Run> = lands.into_iter().map(|land| land.join().unwrap()).collect();
        (lands, prepared)
    });
    for land in &lands {
        land.ok();
    }
    let rounds = prepared.ok()["rounds"].as_u64().unwrap();
    assert!(rounds <= u64::from(SERVE_REFRESH_ATTEMPTS), "{prepared}");

    // 2. Current with every land.
    site.wait_current();
    site.fetch_while_locked(&clone);
    for n in 0..BURST {
        let path = format!("burst-{n}.txt");
        let shown = site.git(&clone, &["show", &format!("origin/main:{path}")]);
        assert_eq!(shown, format!("{path}\n"));
    }
}

/// A preparation that cannot write `serve.git` leaves the land as it is without one: acknowledged,
/// with the envelope and the exit code of a land on a store without `access.toml`. The next
/// connection refreshes once `serve.git` is writable again.
#[test]
fn a_failed_prepare_leaves_the_land_acknowledged() {
    let site = Site::new();
    let clone = site.clone("failed");
    site.wait_current();

    // 1. The land, every directory of serve.git read-only while its preparation runs.
    chmod_dirs(&site.serve_git(), READ_ONLY);
    let served = site.land("failed.txt");
    thread::sleep(QUIET_FOR);
    chmod_dirs(&site.serve_git(), WRITABLE);

    // 2. The same land on a store without access.toml.
    let unserved = Site::unserved().land("failed.txt");
    assert_eq!(served.exit(), unserved.exit(), "{served}\n{unserved}");
    let (served, unserved) = (served.envelope(), unserved.envelope());
    assert_eq!(served["ok"], true, "{served}");
    assert_eq!(keys(&served), keys(&unserved));
    assert_eq!(keys(&served["result"]), keys(&unserved["result"]));
    assert_eq!(keys(&served["meta"]), keys(&unserved["meta"]));
    assert_eq!(served["result"]["status"], unserved["result"]["status"]);

    // 3. The next connection serves the land.
    site.git(&clone, &["fetch", "--quiet"]);
    assert_eq!(
        site.git(&clone, &["show", "origin/main:failed.txt"]),
        "failed.txt\n"
    );
}

/// A `git import` on a served store is prepared from the repository it read: once the
/// detached run is done, `serve.git` is current and holds the history packed, no loose object.
#[test]
fn an_import_is_prepared_from_its_source() {
    let site = Site::empty();
    site.grant();
    site.clone("empty");
    site.wait_current();

    site.import();
    site.wait_current();
    let objects = site.serve_git().join("objects");
    assert_eq!(loose(&objects), Vec::<PathBuf>::new());
}

/// The member names of `value`, an object, in order.
fn keys(value: &Value) -> Vec<String> {
    let object = value
        .as_object()
        .unwrap_or_else(|| panic!("an object: {value}"));
    object.keys().cloned().collect()
}

/// No preparation starts after an operation that failed, or on a store without `access.toml`;
/// one does after a land that succeeded (seen as the mark it leaves while the test holds
/// `serve.lock`).
#[test]
fn no_prepare_without_access_or_success() {
    let site = Site::new();
    site.clone("quiet");
    site.wait_current();
    let held = site.hold_serve_lock();

    // 1. A land that fails: no head of that name.
    let changes = site.changes("refused.txt");
    let refused = site.run(&[
        "land",
        "no-such-head",
        "-m",
        "refused",
        "--changes",
        &changes,
    ]);
    assert_ne!(refused.exit(), 0, "{refused}");
    site.assert_quiet("after a failed land");

    // 2. A land on the store without access.toml.
    let access = site.served.store(STORE).join("access.toml");
    fs::remove_file(&access).unwrap();
    site.land("unserved.txt").ok();
    site.assert_quiet("without access.toml");

    // 3. A land with access.toml: the preparation found serve.lock held, and marked it.
    site.grant();
    site.land("served.txt").ok();
    site.wait_dirty();
    drop(held);
}
