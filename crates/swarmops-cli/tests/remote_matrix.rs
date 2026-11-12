//! The interop matrix's store → GitHub cells (the 2026-11-12 interop design, section 2, column
//! S→H) that no earlier suite holds: a clone that follows another branch, a FORMAT 2 branch
//! pushed, two stores writing one remote at once, a push cut mid-send, a read-only token, a
//! signed remote commit, and the ids every store exports after concurrent work.
//!
//! Each test serves bare repositories with the test kit's [`GitServer`] (git's own smart HTTP on
//! `127.0.0.1`, or the ssh stand-in) and runs the binary with a git environment of its own: the
//! server's git in `SWARMOPS_GIT`, an empty `HOME`, no system or global configuration.
//!
//! A race is forced by a `pre-receive` hook on the remote that holds the first push it meets
//! until the test lets it go: the held push passed its advertisement and its lease, and another
//! store's push lands before its update, which then finds the ref moved.
//!
//! ```text
//!   store B ── push ── receive-pack ── pre-receive (held) ─────────── update: main moved
//!   store A ── push ── receive-pack ── pre-receive (passes) ─ update ─┘
//! ```

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;
use swarmops_store::StoreFormat;
use swarmops_testkit::remote::{GitServer, HttpAuth, HttpServer, SshFault, SshMode};
use tempfile::TempDir;

use crate::support::{Racers, Run, Store, run};

/// The agent the commands that need an identity run as.
const AGENT: &str = "agent/worker-17";

/// The repository every test serves.
const REPO: &str = "repo";

/// The remote's name in the store.
const ORIGIN: &str = "origin";

/// The variable that names the git the binary runs.
const GIT_VAR: &str = "SWARMOPS_GIT";

/// The variable git takes its ssh command from.
const SSH_VAR: &str = "GIT_SSH_COMMAND";

/// The commits [`Remote::new`] writes on `main`.
const BASE_COMMITS: usize = 2;

/// The size of the file a cut push sends: a pack of about 780 KiB, well past [`CUT`].
const NOISE_BYTES: usize = 1 << 20;

/// Where a cut push's upload ends: inside the pack, as the probe's 300 KB cut did. The ssh
/// stand-in's `dd` counts a short read as a whole 64 KiB block: it passes 5 reads here, the
/// command list and then pack data; a cut below 64 KiB passes the command list alone.
const CUT: usize = 300 << 10;

/// How many stores sync one remote at once in the export test.
const STORES: usize = 4;

/// How long the test waits for a held push to reach the hook.
const HOLD_WAIT: Duration = Duration::from_secs(60);

/// How often the test looks for the held push.
const HOLD_POLL: Duration = Duration::from_millis(20);

/// How often the hook looks for its release, in milliseconds; it gives up after [`HOLD_WAIT`].
const HOOK_TICK_MS: u128 = 50;

/// How a test reaches the server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Transport {
    /// git's smart HTTP on loopback.
    Http,
    /// The ssh stand-in, serving.
    Ssh,
}

/// Every transport, in the order the tests run them.
const TRANSPORTS: [Transport; 2] = [Transport::Http, Transport::Ssh];

/// A served repository and the git environment the binary runs with.
struct Remote {
    /// The remote's side.
    server: GitServer,
    /// The server's smart HTTP, served whatever the transport.
    http: HttpServer,
    /// How the store reaches it.
    transport: Transport,
    /// git's empty `HOME` for the product's children, removed when the remote is dropped.
    _home: TempDir,
    /// The variables every run sets.
    env: Vec<(String, String)>,
}

impl Remote {
    /// [`REPO`] served over `transport`, with [`BASE_COMMITS`] commits on `main`.
    fn new(transport: Transport) -> Remote {
        Remote::with_auth(transport, HttpAuth::Anonymous)
    }

    /// [`Remote::new`], its HTTP asking for `auth`.
    fn with_auth(transport: Transport, auth: HttpAuth) -> Remote {
        let server = GitServer::new();
        server.create(REPO);
        let http = server.http(auth);
        let home = tempfile::tempdir().expect("a temp HOME");

        // 1. The server's git, an empty HOME, no system or global configuration.
        let mut env = vec![
            (GIT_VAR.to_owned(), path_text(&real_git(&server))),
            ("HOME".to_owned(), path_text(home.path())),
            ("GIT_CONFIG_NOSYSTEM".to_owned(), "1".to_owned()),
            ("GIT_CONFIG_GLOBAL".to_owned(), "/dev/null".to_owned()),
        ];

        // 2. Over ssh, the stand-in as git's ssh.
        if transport == Transport::Ssh {
            env.push((SSH_VAR.to_owned(), server.ssh_command(SshMode::Serve)));
        }
        let remote = Remote {
            server,
            http,
            transport,
            _home: home,
            env,
        };

        // 3. A history below the head.
        remote.commit("main", "first", &[("a.txt", "a\n")]);
        remote.commit("main", "second", &[("b.txt", "b\n")]);
        remote
    }

    /// [`REPO`]'s URL over this remote's transport.
    fn url(&self) -> String {
        match self.transport {
            Transport::Http => self.http.url(REPO),
            Transport::Ssh => self.server.ssh_url(REPO),
        }
    }

    /// A commit on the remote's `branch`, as another person pushing would; its id.
    fn commit(&self, branch: &str, message: &str, files: &[(&str, &str)]) -> String {
        self.server.commit_on(REPO, branch, message, files)
    }

    /// The commit the remote's `branch` names, if it exists.
    fn head(&self, branch: &str) -> Option<String> {
        self.server.rev_parse(REPO, &format!("refs/heads/{branch}"))
    }

    /// What plain git with `args` prints on [`REPO`].
    fn git(&self, args: &[&str]) -> String {
        git_out(&self.server, &self.server.bare(REPO), args)
    }

    /// The variables every run sets, then `extra`, which a later reading takes over.
    fn vars<'a>(&'a self, extra: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
        let own = self.env.iter().map(|(k, v)| (k.as_str(), v.as_str()));
        own.chain(extra.iter().copied()).collect()
    }

    /// `store` recording the remote as [`ORIGIN`] following `main`, fetched, its `main` at the
    /// remote's, as a clone leaves it.
    fn adopted(&self, store: Store) -> Store {
        let store = store.with_env(&self.vars(&[]));
        let url = self.url();
        let line = ["remote", "add", ORIGIN, url.as_str(), "--branch", "main"];
        store.read(&line).ok();

        // main at the fetched head.
        let fetched = store.read(&["fetch", ORIGIN]).ok();
        let head = fetched["head"]["changeset"].as_str().expect("a head");
        let line = ["ref", "set", "main", head, "--create"];
        store.run_as(AGENT, &line, &[]).ok();
        store
    }

    /// A store of this build's default format, as a clone leaves it ([`Remote::adopted`]).
    fn cloned(&self) -> Store {
        self.adopted(Store::init())
    }

    /// The first parents of the remote's `main`, newest first.
    fn first_parents(&self) -> Vec<String> {
        let listed = self.git(&["rev-list", "--first-parent", "main"]);
        listed.lines().map(str::to_owned).collect()
    }

    /// How many commits of the remote `main`'s first-parent chain change `path`.
    fn changed(&self, path: &str) -> usize {
        let listed = self.git(&["log", "--first-parent", "--format=%H", "main", "--", path]);
        listed.lines().count()
    }

    /// The merge commits on the remote `main`'s first-parent chain.
    fn merges(&self) -> usize {
        let listed = self.git(&["rev-list", "--first-parent", "--merges", "main"]);
        listed.lines().count()
    }

    /// Asserts the remote's `main` linear, holding each of `paths` once, [`BASE_COMMITS`] plus
    /// one commit per path long.
    fn assert_each_once(&self, paths: &[&str], what: &str) {
        assert_eq!(self.merges(), 0, "{what}");
        for path in paths {
            assert_eq!(self.changed(path), 1, "{what}: {path}");
        }
        let chain = self.first_parents();
        assert_eq!(chain.len(), BASE_COMMITS + paths.len(), "{what}: {chain:?}");
    }
}

/// The real git the server runs, as an absolute path.
fn real_git(server: &GitServer) -> PathBuf {
    let program = server.git_program();
    if program.is_absolute() {
        return program.to_path_buf();
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
        .expect("git on PATH")
}

/// `path` as text.
fn path_text(path: &Path) -> String {
    path.to_str().expect("UTF-8 temp paths").to_owned()
}

/// What plain git with `args` prints on the repository `git_dir`; a failure fails the test.
fn git_out(server: &GitServer, git_dir: &Path, args: &[&str]) -> String {
    let output = server
        .command()
        .arg("--git-dir")
        .arg(git_dir)
        .args(args)
        .output()
        .expect("run git");
    assert!(output.status.success(), "git {args:?}: {output:?}");
    String::from_utf8(output.stdout)
        .expect("UTF-8")
        .trim_end()
        .to_owned()
}

/// The changeset `main` names in `store`, if it exists.
fn main_at(store: &Store) -> Option<String> {
    let refs = store.read(&["ref", "list"]).ok();
    let listed = refs["refs"].as_array().expect("refs");
    let main = listed.iter().find(|entry| entry["name"] == "main")?;
    Some(main["target"].as_str().expect("a target").to_owned())
}

/// Commits `content` at `path` on `main` and moves `main` to it; the changeset.
fn land(store: &Store, path: &str, content: &str) -> String {
    let old = main_at(store).expect("main");
    let new = store.commit(AGENT, path, content, Some(&old));
    let line = ["ref", "set", "main", new.as_str(), "--expect", old.as_str()];
    store.run_as(AGENT, &line, &[]).ok();
    new
}

/// `args` on `store` as [`AGENT`].
fn on(store: &Store, args: &[&str]) -> Run {
    store.run_as(AGENT, args, &[])
}

/// The error a push that lost a race must give: `E_REMOTE_DIVERGED`
/// `remote_moved`, exit 3, not retryable (the caller syncs).
fn assert_moved(failed: &Run) {
    let error = failed.err("E_REMOTE_DIVERGED");
    assert_eq!(failed.exit(), 3, "{failed}");
    assert_eq!(error["details"]["reason"], "remote_moved", "{failed}");
}

/// The commit a sync published last: its `published` list's last `sha`.
fn published(synced: &Value) -> String {
    let changes = synced["published"].as_array().expect("published");
    let last = changes
        .last()
        .unwrap_or_else(|| panic!("nothing published: {synced}"));
    last["sha"].as_str().expect("a sha").to_owned()
}

/// `--repo <store> --as <AGENT> <args…>`, for a racer.
fn racer_args(store: &Store, args: &[&str]) -> Vec<String> {
    store.args(AGENT, args)
}

/// Whether an agent runs `run` again: a lost race or a retryable failure.
fn again(run: &Run) -> bool {
    let envelope: Value = serde_json::from_str(&run.stdout).unwrap_or_default();
    let error = &envelope["error"];
    error["retryable"] == true
        || error["code"] == "E_CAS_RETRY_EXHAUSTED"
        || error["code"] == "E_REMOTE_DIVERGED"
}

/// `sync` on `store`, run again while it ends as [`again`] says; the run that did not.
fn synced(store: &Store) -> Run {
    loop {
        let done = on(store, &["sync", ORIGIN]);
        if !again(&done) {
            return done;
        }
        std::thread::sleep(HOLD_POLL);
    }
}

/// A `pre-receive` hook on the remote that holds the first push it meets until the test lets
/// it go; every later push passes.
#[cfg(unix)]
struct Hold {
    /// Its marks: `taken` (a push is held), `reached`, `released`.
    dir: TempDir,
}

#[cfg(unix)]
impl Hold {
    /// The hook, installed on `remote`'s [`REPO`].
    fn on(remote: &Remote) -> Hold {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().expect("a temp dir");
        let mark = |name: &str| path_text(&dir.path().join(name));

        // 1. The first push takes the hold, says so and waits; the rest pass.
        let script = format!(
            "#!/bin/sh\nPATH=/usr/bin:/bin\ncat >/dev/null\n\
             if mkdir '{taken}' 2>/dev/null; then\n  : > '{reached}'\n  n=0\n  \
             while [ ! -e '{released}' ] && [ $n -lt {rounds} ]; do\n    \
             sleep 0.{HOOK_TICK_MS:03}; n=$((n+1))\n  done\nfi\nexit 0\n",
            rounds = HOLD_WAIT.as_millis() / HOOK_TICK_MS,
            taken = mark("taken"),
            reached = mark("reached"),
            released = mark("released"),
        );

        // 2. Installed as the remote's hook.
        let hook = remote.server.bare(REPO).join("hooks").join("pre-receive");
        fs::write(&hook, script).expect("write the hook");
        let mut mode = fs::metadata(&hook).expect("the hook").permissions();
        mode.set_mode(0o755);
        fs::set_permissions(&hook, mode).expect("make the hook runnable");
        Hold { dir }
    }

    /// Waits until a push is held; a push that never comes fails the test.
    fn reached(&self) {
        let reached = self.dir.path().join("reached");
        let deadline = Instant::now() + HOLD_WAIT;
        while !reached.exists() {
            assert!(Instant::now() < deadline, "no push reached the hook");
            std::thread::sleep(HOLD_POLL);
        }
    }

    /// Lets the held push go on to its update.
    fn release(&self) {
        fs::write(self.dir.path().join("released"), "").expect("release the hold");
    }
}

/// `clone --branch dev` follows `dev`: the store's `main` is `dev`'s head, and a later commit on
/// `dev` reaches `main` by `sync`, while the remote's `main` stays as it was.
#[test]
fn clone_branch_follows_a_non_default_branch() {
    for transport in TRANSPORTS {
        let remote = Remote::new(transport);
        let main = remote.head("main").expect("main");
        remote.server.move_to(REPO, "dev", &main);
        let dev = remote.commit("dev", "on dev", &[("dev.txt", "dev\n")]);
        let dir = tempfile::tempdir().expect("a temp dir");
        let into = path_text(&dir.path().join("store"));

        // 1. The clone follows dev: main is dev's head.
        let url = remote.url();
        let line = [
            "--as", AGENT, "clone", &url, "--into", &into, "--branch", "dev",
        ];
        let cloned = run(&line, &remote.vars(&[])).ok();
        assert_eq!(cloned["remote"]["branch"], "dev", "{transport:?}: {cloned}");
        assert_eq!(cloned["fetch"]["head"]["sha"], dev.as_str(), "{cloned}");
        assert_eq!(cloned["imported"], BASE_COMMITS + 1, "{cloned}");
        assert_eq!(cloned["main"], cloned["fetch"]["head"]["changeset"]);

        // 2. A later commit on dev: sync fast-forwards main to it.
        let later = remote.commit("dev", "later", &[("later.txt", "later\n")]);
        let line = ["--repo", &into, "--as", AGENT, "sync", ORIGIN];
        let synced = run(&line, &remote.vars(&[])).ok();
        assert_eq!(
            synced["status"], "fast_forwarded",
            "{transport:?}: {synced}"
        );
        assert_eq!(synced["remote_head"]["new"], later.as_str(), "{synced}");

        // 3. The remote's main never moved.
        assert_eq!(remote.head("main"), Some(main), "{transport:?}");
    }
}

/// A branch of a FORMAT 2 store, `feature/x`, is pushed to the remote branch of its name, which
/// it creates: `created`, nothing rewritten, `main` untouched.
#[test]
fn push_of_a_format_2_branch_creates_it() {
    for transport in TRANSPORTS {
        let remote = Remote::new(transport);
        let main = remote.head("main");
        let store = remote.adopted(Store::init_as(StoreFormat::V2));

        // 1. feature/x one change past main.
        let base = main_at(&store).expect("main");
        let change = store.commit(AGENT, "x.txt", "x\n", Some(&base));
        on(&store, &["ref", "set", "feature/x", &change, "--create"]).ok();

        // 2. Pushed: created at the change's export.
        let pushed = store.read(&["push", ORIGIN, "feature/x"]).ok();
        assert_eq!(pushed["status"], "created", "{transport:?}: {pushed}");
        assert_eq!(pushed["branch"], "feature/x", "{pushed}");
        assert_eq!(pushed["head_changeset"], change.as_str(), "{pushed}");
        assert_eq!(pushed["rewritten"], 0, "{pushed}");
        let created = remote.head("feature/x").expect("feature/x made");
        assert_eq!(pushed["new"], created.as_str(), "{pushed}");
        let parent = remote.git(&["rev-parse", &format!("{created}^")]);
        assert_eq!(Some(parent), main, "{transport:?}");

        // 3. main as it was.
        assert_eq!(remote.head("main"), main, "{transport:?}");
    }
}

/// Two stores push their own change to one remote at once. In every interleaving one is
/// `pushed` and the other loses as `remote_moved`, which its sync then publishes on top: each
/// change on the remote once, linear, the winner's commit kept.
#[test]
fn two_stores_push_one_remote_at_once() {
    for transport in TRANSPORTS {
        let remote = Remote::new(transport);
        let stores = [remote.cloned(), remote.cloned()];
        let paths = ["one.txt", "two.txt"];
        for (store, path) in stores.iter().zip(paths) {
            land(store, path, "mine\n");
        }

        // 1. Both pushes at once.
        let dir = tempfile::tempdir().expect("a temp dir");
        let mut racers = Racers::new(dir.path());
        for store in &stores {
            racers.spawn(&racer_args(store, &["push", ORIGIN]), &remote.vars(&[]));
        }
        let runs = racers.race();

        // 2. One won; the other lost as remote_moved.
        let won: Vec<usize> = (0..2).filter(|&n| runs[n].exit() == 0).collect();
        assert_eq!(won.len(), 1, "{transport:?}: {} / {}", runs[0], runs[1]);
        let (winner, loser) = (won[0], 1 - won[0]);
        let pushed = runs[winner].ok();
        assert_eq!(pushed["status"], "pushed", "{pushed}");
        assert_moved(&runs[loser]);

        // 3. The loser's sync publishes its change on the winner's commit.
        let synced = on(&stores[loser], &["sync", ORIGIN]).ok();
        let chain = remote.first_parents();
        assert_eq!(chain[0], published(&synced), "{transport:?}");
        assert_eq!(
            chain[1],
            pushed["new"].as_str().expect("new"),
            "{transport:?}"
        );
        remote.assert_each_once(&paths, &format!("{transport:?}"));
    }
}

/// [`two_stores_push_one_remote_at_once`] with the loss forced: B's push is held after its
/// advertisement, A's lands, and B's update finds `main` moved: `remote_moved`, never a
/// `declined` refusal.
#[cfg(unix)]
#[test]
fn two_stores_push_a_forced_loss() {
    for transport in TRANSPORTS {
        let remote = Remote::new(transport);
        let (a, b) = (remote.cloned(), remote.cloned());
        land(&a, "one.txt", "a\n");
        land(&b, "two.txt", "b\n");
        let gate = Hold::on(&remote);

        // 1. B's push, held; A's lands meanwhile; then B's goes on.
        let (lost, won) = std::thread::scope(|scope| {
            let pushing = scope.spawn(|| b.read(&["push", ORIGIN]));
            gate.reached();
            let won = a.read(&["push", ORIGIN]).ok();
            gate.release();
            (pushing.join().expect("B's push"), won)
        });

        // 2. B lost as remote_moved; the remote holds A's commit.
        assert_moved(&lost);
        assert_eq!(won["status"], "pushed", "{transport:?}: {won}");
        let a_commit = won["new"].as_str().expect("new").to_owned();
        assert_eq!(remote.head("main"), Some(a_commit.clone()), "{transport:?}");

        // 3. B's sync publishes on A's commit.
        let synced = on(&b, &["sync", ORIGIN]).ok();
        let chain = remote.first_parents();
        assert_eq!(chain[0], published(&synced), "{transport:?}");
        assert_eq!(chain[1], a_commit, "{transport:?}");
        remote.assert_each_once(&["one.txt", "two.txt"], &format!("{transport:?}"));
    }
}

/// Two stores sync their own change to one remote at once. In every interleaving both end
/// `pushed`: each change on the remote once, linear, the first published commit kept below the
/// second.
#[test]
fn two_stores_sync_one_remote_at_once() {
    for transport in TRANSPORTS {
        let remote = Remote::new(transport);
        let stores = [remote.cloned(), remote.cloned()];
        let paths = ["one.txt", "two.txt"];
        for (store, path) in stores.iter().zip(paths) {
            land(store, path, "mine\n");
        }

        // 1. Both syncs at once.
        let dir = tempfile::tempdir().expect("a temp dir");
        let mut racers = Racers::new(dir.path());
        for store in &stores {
            racers.spawn(&racer_args(store, &["sync", ORIGIN]), &remote.vars(&[]));
        }
        let runs = racers.race();

        // 2. Both published, one on the other.
        let shas: Vec<String> = runs.iter().map(|done| published(&done.ok())).collect();
        let chain = remote.first_parents();
        assert!(
            shas.contains(&chain[0]),
            "{transport:?}: {chain:?} {shas:?}"
        );
        assert!(
            shas.contains(&chain[1]),
            "{transport:?}: {chain:?} {shas:?}"
        );
        for done in &runs {
            assert_eq!(done.ok()["status"], "pushed", "{transport:?}: {done}");
        }
        remote.assert_each_once(&paths, &format!("{transport:?}"));
    }
}

/// [`two_stores_sync_one_remote_at_once`] with the loss forced: B's sync's push is held after
/// its advertisement and A's push lands; B's sync takes the loss as `remote_moved`, lands
/// again on A's commit and pushes on its second attempt.
#[cfg(unix)]
#[test]
fn two_stores_sync_a_forced_loss() {
    for transport in TRANSPORTS {
        let remote = Remote::new(transport);
        let (a, b) = (remote.cloned(), remote.cloned());
        land(&a, "one.txt", "a\n");
        land(&b, "two.txt", "b\n");
        let gate = Hold::on(&remote);

        // 1. B's sync, its push held; A's push lands meanwhile.
        let (synced, won) = std::thread::scope(|scope| {
            let pushing = scope.spawn(|| on(&b, &["sync", ORIGIN]));
            gate.reached();
            let won = a.read(&["push", ORIGIN]).ok();
            gate.release();
            (pushing.join().expect("B's sync"), won)
        });

        // 2. B's sync retried once and published on A's commit.
        let synced = synced.ok();
        assert_eq!(synced["status"], "pushed", "{transport:?}: {synced}");
        assert_eq!(synced["attempts"], 2, "{transport:?}: {synced}");
        let chain = remote.first_parents();
        assert_eq!(chain[0], published(&synced), "{transport:?}");
        assert_eq!(chain[1], won["new"].as_str().expect("new"), "{transport:?}");
        remote.assert_each_once(&["one.txt", "two.txt"], &format!("{transport:?}"));
    }
}

/// [`STORES`] stores each land a change and sync at once, then sync again until done: every
/// store's export of `main` is the remote's history, commit by commit and tree by tree, sound
/// for `git fsck --strict`, and every store passes `verify --deep`. Changeset ids are each
/// store's own and are not compared.
#[test]
fn stores_export_the_same_ids_after_concurrent_work() {
    for transport in TRANSPORTS {
        let remote = Remote::new(transport);
        let stores: Vec<Store> = (0..STORES).map(|_| remote.cloned()).collect();
        let paths: Vec<String> = (0..STORES).map(|n| format!("stores/{n}.txt")).collect();
        for (store, path) in stores.iter().zip(&paths) {
            land(store, path, "mine\n");
        }

        // 1. Every sync at once; a loser syncs again.
        let dir = tempfile::tempdir().expect("a temp dir");
        let mut racers = Racers::new(dir.path());
        for store in &stores {
            racers.spawn(&racer_args(store, &["sync", ORIGIN]), &remote.vars(&[]));
        }
        for (store, done) in stores.iter().zip(racers.race()) {
            if again(&done) {
                synced(store).ok();
                continue;
            }
            done.ok();
        }

        // 2. Each store takes what the others published.
        for store in &stores {
            synced(store).ok();
        }
        let paths: Vec<&str> = paths.iter().map(String::as_str).collect();
        remote.assert_each_once(&paths, &format!("{transport:?}"));

        // 3. Each export of main is the remote's history, by commit and tree.
        let history = "--format=%H %T";
        let expected = remote.git(&["log", history, "main"]);
        for (n, store) in stores.iter().enumerate() {
            let bare = dir.path().join(format!("export-{n}.git"));
            let init = remote
                .server
                .command()
                .args(["init", "--quiet", "--bare"])
                .arg(&bare)
                .output();
            assert!(init.expect("git init").status.success(), "{transport:?}");
            store.read(&["git", "export", &path_text(&bare)]).ok();
            let exported = git_out(&remote.server, &bare, &["log", history, "main"]);
            assert_eq!(exported, expected, "{transport:?}: store {n}");
            git_out(
                &remote.server,
                &bare,
                &["fsck", "--strict", "--no-dangling"],
            );
            assert_eq!(
                store.problems(),
                Vec::<String>::new(),
                "{transport:?}: store {n}"
            );
        }
    }
}

/// A push cut mid-send, the server sound, is `E_REMOTE_UNREACHABLE`, retryable, and moves
/// nothing; the same push run again goes whole. Over HTTP the request's body is cut; over ssh
/// the stand-in passes on [`CUT`] bytes of what the client sends.
#[test]
fn a_cut_push_is_retryable() {
    for transport in TRANSPORTS {
        let remote = Remote::new(transport);
        let store = remote.cloned();
        let before = remote.head("main");
        land(&store, "noise.txt", &noise(NOISE_BYTES));

        // 1. The cut armed for this transport.
        let cut_ssh = remote
            .server
            .ssh_command_with(SshMode::Serve, SshFault::CutSend(CUT));
        let mut env = Vec::new();
        match transport {
            Transport::Http => remote.http.cut_request_after(CUT),
            Transport::Ssh => env.push((SSH_VAR, cut_ssh.as_str())),
        }

        // 2. The cut push: unreachable, retryable; the remote unmoved.
        let line = ["--repo", store.path(), "push", ORIGIN];
        let failed = run(&line, &remote.vars(&env));
        let error = failed.err("E_REMOTE_UNREACHABLE");
        assert_eq!(failed.exit(), 4, "{transport:?}: {failed}");
        assert_eq!(error["retryable"], true, "{transport:?}: {failed}");
        assert_eq!(remote.head("main"), before, "{transport:?}");

        // 3. Run again, uncut: pushed.
        let pushed = store.read(&["push", ORIGIN]).ok();
        assert_eq!(pushed["status"], "pushed", "{transport:?}: {pushed}");
        assert_eq!(remote.head("main").as_deref(), pushed["new"].as_str());
    }
}

/// `len` bytes of text that compress poorly, the same every run: a pack past [`CUT`].
fn noise(len: usize) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    (0..len)
        .map(|_| {
            // xorshift64: one 6-bit digit per byte.
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            char::from(ALPHABET[(state & 0x3f) as usize])
        })
        .collect()
}

/// A token the remote lets read and not write: `fetch` with it works, and `push` is
/// `E_REMOTE_REJECTED` `declined` with the remote's message, not retryable, the remote
/// unmoved. The remote refuses the token's user in a `pre-receive` hook, as a host's
/// permission check does.
#[cfg(unix)]
#[test]
fn a_read_only_token_fetches_and_is_declined_on_push() {
    use std::os::unix::fs::PermissionsExt as _;

    const USER: &str = "x-access-token";
    const TOKEN: &str = "ghp_readOnlyTOKEN0123456789";
    const VAR: &str = "SO_TEST_TOKEN";
    const DENIED: &str = "Permission to repo.git denied to reader.";

    let auth = HttpAuth::Basic {
        user: USER.to_owned(),
        token: TOKEN.to_owned(),
    };
    let remote = Remote::with_auth(Transport::Http, auth);
    let before = remote.head("main");
    let env = [(VAR, TOKEN)];

    // 1. The remote refuses every push by the reader.
    let hook = remote.server.bare(REPO).join("hooks").join("pre-receive");
    let script = format!(
        "#!/bin/sh\ncat >/dev/null\nif [ \"$REMOTE_USER\" = {USER} ]; then\n  \
         echo '{DENIED}' >&2\n  exit 1\nfi\nexit 0\n"
    );
    fs::write(&hook, script).expect("write the hook");
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).expect("make it runnable");

    // 2. Fetch with the token works.
    let store = Store::init().with_env(&remote.vars(&env));
    let url = remote.url();
    let line = ["remote", "add", ORIGIN, url.as_str(), "--token-env", VAR];
    store.read(&line).ok();
    let fetched = store.read(&["fetch", ORIGIN]).ok();
    assert_eq!(fetched["head"]["sha"], before.as_deref().expect("main"));
    let head = fetched["head"]["changeset"].as_str().expect("a head");
    on(&store, &["ref", "set", "main", head, "--create"]).ok();

    // 3. Push with it is declined, with the remote's reason.
    land(&store, "local.txt", "local\n");
    let failed = store.read(&["push", ORIGIN]);
    let error = failed.err("E_REMOTE_REJECTED");
    assert_eq!(failed.exit(), 5, "{failed}");
    assert_eq!(error["details"]["reason"], "declined", "{failed}");
    assert_eq!(error["retryable"], false, "{failed}");
    let said = error["details"]["remote_messages"].to_string();
    assert!(said.contains(DENIED), "{failed}");
    assert_eq!(remote.head("main"), before);
}

/// A signed commit on the remote (a `gpgsig` header) keeps its id: the store fetches it by its
/// id, exports it byte for byte, and a change pushed on it has it as its parent.
#[test]
fn a_signed_remote_commit_keeps_its_id() {
    for transport in TRANSPORTS {
        let remote = Remote::new(transport);
        let signed = sign_head(&remote);

        // 1. The clone sees the signed commit by its id.
        let store = remote.cloned();
        let fetched = store.read(&["fetch", ORIGIN]).ok();
        assert_eq!(fetched["head"]["sha"], signed.as_str(), "{transport:?}");

        // 2. Exported, the same bytes.
        let dir = tempfile::tempdir().expect("a temp dir");
        let bare = dir.path().join("export.git");
        let init = remote
            .server
            .command()
            .args(["init", "--quiet", "--bare"])
            .arg(&bare)
            .output();
        assert!(init.expect("git init").status.success(), "{transport:?}");
        store.read(&["git", "export", &path_text(&bare)]).ok();
        let ours = git_out(&remote.server, &bare, &["cat-file", "commit", &signed]);
        assert_eq!(
            ours,
            remote.git(&["cat-file", "commit", &signed]),
            "{transport:?}"
        );

        // 3. A change pushed on it: nothing rewritten, its parent the signed commit.
        land(&store, "after.txt", "after\n");
        let pushed = store.read(&["push", ORIGIN]).ok();
        assert_eq!(pushed["status"], "pushed", "{transport:?}: {pushed}");
        assert_eq!(pushed["rewritten"], 0, "{pushed}");
        assert_eq!(remote.first_parents()[1], signed, "{transport:?}");
    }
}

/// Rewrites the remote `main`'s head as a commit with a `gpgsig` header (git checks no
/// signature to store one) and moves `main` to it; its id.
fn sign_head(remote: &Remote) -> String {
    let raw = remote.git(&["cat-file", "commit", "main"]);
    let (head, body) = raw.split_once("\n\n").expect("a commit");
    let signature = "gpgsig -----BEGIN PGP SIGNATURE-----\n \n iQEz\n -----END PGP SIGNATURE-----";
    let dir = tempfile::tempdir().expect("a temp dir");
    let file = dir.path().join("signed.commit");
    fs::write(&file, format!("{head}\n{signature}\n\n{body}\n")).expect("write the commit");
    let signed = remote.git(&["hash-object", "-t", "commit", "-w", &path_text(&file)]);
    remote.server.move_to(REPO, "main", &signed);
    signed
}
