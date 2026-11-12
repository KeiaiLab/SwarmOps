//! The interop matrix's store → served store cells (the 2026-11-12 interop design, section 2,
//! column S→S): a store that clones, fetches, pushes and syncs another store which
//! `swarmops serve` serves over the git wire.
//!
//! Each test serves one store with the test kit's [`ServeSite`] (the ssh forced-command stand-in,
//! or `serve --cgi` behind loopback HTTP) and runs the binary as the client with a git
//! environment of its own: the pinned git in `SWARMOPS_GIT`, an empty `HOME`, no system or global
//! configuration. The served store's `access.toml` names the callers by the identity of their
//! key; the client's own `--as` never reaches it.
//!
//! ```text
//!   client store ── swarmops clone | fetch | push | sync ── git ── ssh stand-in (person/ada)
//!                                                                    └── swarmops serve --root
//!                                                                          └── the served store
//! ```
//!
//! A race is forced by a wrapper of the stand-in that holds the first push it meets once the
//! client has read the advertisement, until the test lets it go: the served `main` moves
//! meanwhile, and the held push then finds it moved.
//!
//! ```text
//!   store B ── push ── advertisement read ── (held) ──────────── update: main moved
//!   another writer ── moves the served main ───────────────────┘
//! ```
//!
//! A clone is cut and a killed push's client held by the stand-in's faults ([`SshFault`]).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;
use swarmops_testkit::child::{self, CHILD_TIMEOUT, Child};
use swarmops_testkit::git::fixture;
use swarmops_testkit::remote::{GitLedger, GitServer, HttpAuth, HttpServer, SshFault};
use swarmops_testkit::serve::{HttpIdentity, ServeSite};
use tempfile::TempDir;

use crate::support::{Run, Store, Swarmops, run, swarmops};

/// The agent a client store's commands run as: its own, which the served store never sees.
const AGENT: &str = "agent/worker-17";

/// The agent that writes the served store directly, through the command line.
const WRITER: &str = "agent/worker-1";

/// The identities of the keys: two that push `main`, one that pushes feature branches only, one
/// that no grant names.
const ADA: &str = "person/ada";
const CAROL: &str = "person/carol";
const BOB: &str = "person/bob";
const EVE: &str = "person/eve";

/// The store every site serves.
const STORE: &str = "acme";

/// The remote's name in a client store.
const ORIGIN: &str = "origin";

/// The variable that names the git the binary runs.
const GIT_VAR: &str = "SWARMOPS_GIT";

/// The variable git takes its ssh command from.
const SSH_VAR: &str = "GIT_SSH_COMMAND";

/// The served store's access file.
const ACCESS: &str = r#"version = 1

[[grant]]
who = ["person/ada", "person/carol"]
push = ["main", "release/*", "feature/**"]
force = ["feature/**"]
land = ["main"]

[[grant]]
who = ["person/bob"]
read = true
push = ["feature/**"]
"#;

/// How long the test waits for a held push to reach the wrapper.
const HOLD_WAIT: Duration = Duration::from_secs(60);

/// How often the test looks for the held push.
const HOLD_POLL: Duration = Duration::from_millis(20);

/// How often the wrapper looks for its release, in milliseconds; it gives up after
/// [`HOLD_WAIT`].
const HOLD_TICK_MS: u128 = 50;

/// The size of the file a cut clone fetches: a pack of about 6 MiB, at least 90 reads of the
/// stand-in's cutter.
const NOISE_BYTES: usize = 8 << 20;

/// Where a cut clone's answer ends. The stand-in's cutter counts every read as one 64 KiB
/// block, however short, so this passes 64 reads of each connection: the listing whole (its
/// lines are a read each at most), and of the fetch its lines and then pack data, never the
/// whole pack.
const CUT: usize = 63 * (64 << 10);

/// How long a held stand-in stays after its server ended: as long as the wait for the push to
/// arrive, which starts before the server ends, so that the client is still waiting when it is
/// killed.
const HOLD: Duration = CHILD_TIMEOUT;

/// `SIGKILL`'s number.
#[cfg(unix)]
const SIGKILL: i32 = 9;

/// How many stores sync the served store in the export test.
const STORES: usize = 3;

/// How a client reaches the served store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Transport {
    /// The ssh forced-command stand-in.
    Ssh,
    /// `serve --cgi` behind loopback HTTP.
    Http,
}

/// One caller's way to the served store: the URL and the variables its runs set.
struct Link {
    /// The served store's URL.
    url: String,
    /// The variables every run sets.
    env: Vec<(String, String)>,
    /// The HTTP server of an HTTP link, stopped when the link is dropped.
    _http: Option<HttpServer>,
}

impl Link {
    /// The variables every run sets, then `extra`, which a later reading takes over.
    fn vars<'a>(&'a self, extra: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
        let own = self.env.iter().map(|(k, v)| (k.as_str(), v.as_str()));
        own.chain(extra.iter().copied()).collect()
    }

    /// A store cloned from the served store through this link, with `extra` after the clone's
    /// arguments, and what the clone answered.
    fn cloned_with(&self, extra: &[&str]) -> (Store, Value) {
        let store = Store::empty().with_env(&self.vars(&[]));
        let mut args = vec!["--as", AGENT, "clone", &self.url, "--into", store.path()];
        args.extend(extra);
        let cloned = run(&args, &self.vars(&[])).ok();
        (store, cloned)
    }

    /// A store cloned from the served store through this link.
    fn cloned(&self) -> Store {
        self.cloned_with(&[]).0
    }
}

/// A served root with the store [`STORE`] under [`ACCESS`]: the `linear` fixture imported as
/// `main` and one native change landed on it, so that its history mixes both.
struct Site {
    /// The root, its stand-ins and the pinned git.
    served: ServeSite,
    /// The git the clients run.
    git: GitServer,
    /// The git clients' directories and the served store's change lists.
    work: TempDir,
    /// git's empty `HOME` for the clients' children.
    home: TempDir,
    /// How many change lists [`Site::commit`] has written, for their names.
    commits: AtomicU64,
}

impl Site {
    /// The store, seeded and served.
    fn new() -> Site {
        Site::holding(&[])
    }

    /// [`Site::new`], each `(path, content)` of `files` landed as one more native change before
    /// the store is served: a change landed later is prepared for serving beside the test's
    /// next connection, which gives up when a large one keeps `serve.lock` too long.
    fn holding(files: &[(&str, &str)]) -> Site {
        let served = ServeSite::new::<Swarmops>();
        served.init(STORE);
        let site = Site {
            served,
            git: GitServer::new(),
            work: tempfile::tempdir().expect("a temp dir"),
            home: tempfile::tempdir().expect("a temp HOME"),
            commits: AtomicU64::new(0),
        };

        // 1. A git history, imported, and a native change on it.
        let linear = fixture("linear");
        let source = path_text(linear.repo.as_deref().expect("the fixture's repository"));
        let line = ["git", "import", &source, "--ref", "HEAD", "--into", "main"];
        site.so(&line);
        site.land("seed.txt", "seed\n");
        for (path, content) in files {
            site.land(path, content);
        }

        // 2. Written after the import, so that no preparation runs beside the test's connections.
        fs::write(site.served.store(STORE).join("access.toml"), ACCESS).expect("write access");
        site
    }

    /// The served store's path.
    fn store(&self) -> String {
        path_text(&self.served.store(STORE))
    }

    /// `args` on the served store as `agent`.
    fn so_as(&self, agent: &str, args: &[&str]) -> Run {
        let store = self.store();
        let line: Vec<&str> = ["--repo", &store, "--as", agent]
            .iter()
            .chain(args)
            .copied()
            .collect();
        run(&line, &[])
    }

    /// `args` on the served store as [`WRITER`], which must succeed; its result.
    fn so(&self, args: &[&str]) -> Value {
        self.so_as(WRITER, args).ok()
    }

    /// The changeset the served store's `main` names.
    fn main_head(&self) -> String {
        text_of(&self.so(&["show", "main"])["id"])
    }

    /// A changeset by [`WRITER`] in the served store putting `content` at `path` on its `main`,
    /// which no ref names yet; its id.
    fn commit(&self, path: &str, content: &str) -> String {
        let n = self.commits.fetch_add(1, Ordering::Relaxed);
        let file = self.work.path().join(format!("change-{n}.json"));
        let changes =
            serde_json::json!([{"path": path, "op": "put", "content": {"inline": content}}]);
        fs::write(&file, changes.to_string()).expect("write a change list");

        let head = self.main_head();
        let file = path_text(&file);
        let line = ["commit", "-m", path, "--changes", &file, "--parent", &head];
        text_of(&self.so(&line)["changeset"])
    }

    /// [`Site::commit`], with the served store's `main` moved to it: a native change, landed.
    fn land(&self, path: &str, content: &str) -> String {
        let old = self.main_head();
        let new = self.commit(path, content);
        self.so(&["ref", "set", "main", &new, "--expect", &old]);
        new
    }

    /// How many moves the served store's ref log holds for `main`.
    fn main_moves(&self) -> usize {
        let log = self.so(&["ref", "log", "main", "--limit", "1000"]);
        assert_eq!(log["truncated"], false, "{log}");
        log["moves"].as_array().expect("moves").len()
    }

    /// The newest of the served store's connection records.
    fn newest_record(&self) -> Value {
        let file = self.served.store(STORE).join("git/serve-records.jsonl");
        let records = fs::read_to_string(file).expect("the served store's records");
        let newest = records.lines().last().expect("a record");
        serde_json::from_str(newest).expect("a record is JSON")
    }

    /// The newest ref move of the served store's op log.
    fn newest_move(&self) -> Value {
        let op = self.so(&["op", "log", "-n", "1"])["ops"][0].take();
        assert_eq!(op["kind"], "ref_move", "{op}");
        op["ref_moves"][0].clone()
    }

    /// The git commit the served store's changeset `id` came in as.
    fn oid(&self, id: &Value) -> Value {
        let id = id.as_str().expect("a changeset id");
        self.so(&["show", id])["metadata"]["git.oid"].take()
    }

    /// The changeset the served store's ref `name` names, if it exists.
    fn ref_at(&self, name: &str) -> Option<Value> {
        let refs = self.so(&["ref", "list"])["refs"].take();
        let listed = refs.as_array().expect("refs");
        let found = listed.iter().find(|entry| entry["name"] == name)?;
        Some(found["target"].clone())
    }

    /// Asserts that `verify --deep` finds nothing in the served store.
    fn assert_clean(&self) {
        let verify = run(&["--repo", &self.store(), "verify", "--deep"], &[]);
        assert_eq!(verify.exit(), 0, "{verify}");
    }

    /// A client git command's output: `args` in `dir`, by the key of `identity`.
    fn git_out(&self, identity: &str, dir: &Path, args: &[&str]) -> Output {
        let mut command = self.served.command();
        command
            .env(SSH_VAR, self.served.ssh_command(identity))
            .current_dir(dir)
            .args(args);
        child::run(&mut command)
    }

    /// [`Site::git_out`], which must succeed; what it printed, trimmed.
    fn git(&self, identity: &str, dir: &Path, args: &[&str]) -> String {
        let output = self.git_out(identity, dir, args);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "git {args:?}: {stderr}");
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    /// What plain git with `args` prints on the served store's `serve.git`, brought up to the
    /// store by one connection first.
    fn served_git(&self, args: &[&str]) -> String {
        let url = self.served.ssh_url(STORE);
        self.git(ADA, self.work.path(), &["ls-remote", &url]);
        let git_dir = self.served.store(STORE).join("git/serve.git");
        git_in(&self.served, &git_dir, args)
    }

    /// The served `main`'s first parents, newest first.
    fn history(&self) -> Vec<String> {
        let listed = self.served_git(&["rev-list", "--first-parent", "refs/heads/main"]);
        listed.lines().map(str::to_owned).collect()
    }

    /// The way of the key of `identity` to the served store over `transport`.
    fn link(&self, transport: Transport, identity: &str) -> Link {
        // 1. The pinned git, an empty HOME, no system or global configuration.
        let mut env = vec![
            (GIT_VAR.to_owned(), path_text(&real_git(&self.git))),
            ("HOME".to_owned(), path_text(self.home.path())),
            ("GIT_CONFIG_NOSYSTEM".to_owned(), "1".to_owned()),
            ("GIT_CONFIG_GLOBAL".to_owned(), "/dev/null".to_owned()),
        ];

        // 2. Over ssh the key's stand-in; over HTTP a server whose every request is the caller's.
        let (url, http) = match transport {
            Transport::Ssh => {
                env.push((SSH_VAR.to_owned(), self.served.ssh_command(identity)));
                (self.served.ssh_url(STORE), None)
            }
            Transport::Http => {
                let caller = HttpIdentity::Fixed(identity.to_owned());
                let http = self.served.http(caller, HttpAuth::Anonymous);
                (http.url(STORE), Some(http))
            }
        };
        Link {
            url,
            env,
            _http: http,
        }
    }

    /// [`Site::link`] over ssh.
    fn ssh(&self, identity: &str) -> Link {
        self.link(Transport::Ssh, identity)
    }
}

/// The real git `server` runs, as an absolute path.
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

/// `value`, a string of an envelope, as text.
fn text_of(value: &Value) -> String {
    value
        .as_str()
        .unwrap_or_else(|| panic!("no string: {value}"))
        .to_owned()
}

/// What plain git with `args` prints on the repository `git_dir`; a failure fails the test.
fn git_in(served: &ServeSite, git_dir: &Path, args: &[&str]) -> String {
    let mut command = served.command();
    command.arg("--git-dir").arg(git_dir).args(args);
    let output = child::run(&mut command);
    assert!(output.status.success(), "git {args:?}: {output:?}");
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// The changeset `main` names in `store`.
fn main_at(store: &Store) -> String {
    text_of(&store.show("main")["id"])
}

/// Commits `content` at `path` on `main` and moves `main` to it; the changeset.
fn land(store: &Store, path: &str, content: &str) -> String {
    let old = main_at(store);
    let new = store.commit(AGENT, path, content, Some(&old));
    let line = ["ref", "set", "main", new.as_str(), "--expect", old.as_str()];
    store.run_as(AGENT, &line, &[]).ok();
    new
}

/// `args` on `store` as [`AGENT`].
fn on(store: &Store, args: &[&str]) -> Run {
    store.run_as(AGENT, args, &[])
}

/// The commit a sync published last: its `published` list's last `sha`.
fn published(synced: &Value) -> String {
    let changes = synced["published"].as_array().expect("published");
    let last = changes
        .last()
        .unwrap_or_else(|| panic!("nothing published: {synced}"));
    text_of(&last["sha"])
}

/// Asserts the served `main` linear, holding each of `paths` once, `base` commits plus one
/// commit per path long.
fn assert_each_once(site: &Site, base: usize, paths: &[&str], what: &str) {
    let merges = site.served_git(&["rev-list", "--first-parent", "--merges", "main"]);
    assert_eq!(merges, "", "{what}");
    for path in paths {
        let line = ["log", "--first-parent", "--format=%H", "main", "--", path];
        assert_eq!(site.served_git(&line).lines().count(), 1, "{what}: {path}");
    }
    let chain = site.history();
    assert_eq!(chain.len(), base + paths.len(), "{what}: {chain:?}");
}

/// A wrapper of a key's stand-in that holds the first push it meets, once the client has read
/// the advertisement and sent its first byte, until the test lets it go; every other connection
/// passes.
#[cfg(unix)]
struct Hold {
    /// The wrapper (`ssh`) and its marks: `taken` (a push is held), `reached`, `released`.
    dir: TempDir,
}

#[cfg(unix)]
impl Hold {
    /// The wrapper of the stand-in of `identity`.
    fn on(site: &Site, identity: &str) -> Hold {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().expect("a temp dir");
        let mark = |name: &str| path_text(&dir.path().join(name));

        // 1. A fetch, and every push after the first, pass; the first push's first byte goes on,
        //    the wrapper says so and waits, then hands on the rest.
        let script = format!(
            "#!/bin/sh\nPATH=/usr/bin:/bin:$PATH\n\
             case \"$*\" in\n  *git-receive-pack*) ;;\n  *) exec {ssh} \"$@\" ;;\nesac\n\
             mkdir '{taken}' 2>/dev/null || exec {ssh} \"$@\"\n\
             {{\n  dd bs=1 count=1 status=none\n  : > '{reached}'\n  n=0\n  \
             while [ ! -e '{released}' ] && [ $n -lt {rounds} ]; do\n    \
             sleep 0.{HOLD_TICK_MS:03}; n=$((n+1))\n  done\n  exec cat\n}} | {ssh} \"$@\"\n",
            ssh = site.served.ssh_command(identity),
            rounds = HOLD_WAIT.as_millis() / HOLD_TICK_MS,
            taken = mark("taken"),
            reached = mark("reached"),
            released = mark("released"),
        );

        // 2. Named ssh, so that git takes it for OpenSSH and does not probe it.
        let program = dir.path().join("ssh");
        fs::write(&program, script).expect("write the wrapper");
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).expect("make it runnable");
        Hold { dir }
    }

    /// The wrapper, as git's ssh command.
    fn ssh(&self) -> String {
        format!("'{}'", path_text(&self.dir.path().join("ssh")))
    }

    /// Waits until a push is held; a push that never comes fails the test.
    fn reached(&self) {
        let reached = self.dir.path().join("reached");
        let deadline = Instant::now() + HOLD_WAIT;
        while !reached.exists() {
            assert!(Instant::now() < deadline, "no push reached the wrapper");
            std::thread::sleep(HOLD_POLL);
        }
    }

    /// Lets the held push go on.
    fn release(&self) {
        fs::write(self.dir.path().join("released"), "").expect("release the hold");
    }
}

/// An empty bare repository `name` under `dir`, with `store`'s `main` exported into it.
fn exported(site: &Site, store: &Store, dir: &Path, name: &str) -> PathBuf {
    let bare = dir.join(name);
    let mut init = site.served.command();
    init.args(["init", "--quiet", "--bare"]).arg(&bare);
    assert!(child::run(&mut init).status.success(), "git init");
    store.read(&["git", "export", &path_text(&bare)]).ok();
    bare
}

/// Clone, fetch, push and sync against the served store over `transport`: the clone imports
/// the served history by its ids, a change the remote gained comes by `fetch` and `sync`
/// fast-forwards to it, and a local change goes out by `push` and by `sync` as a fast-forward
/// that rewrites nothing, which the remote records as a ref move by the caller's key.
fn round_trip(transport: Transport) {
    let site = Site::new();
    let link = site.link(transport, ADA);
    let served = site.history();

    // 1. The clone: the whole history, main at the served head.
    let (store, cloned) = link.cloned_with(&[]);
    assert_eq!(cloned["imported"], served.len(), "{transport:?}: {cloned}");
    assert_eq!(cloned["fetch"]["head"]["sha"], served[0].as_str());
    assert_eq!(cloned["main"], cloned["fetch"]["head"]["changeset"]);
    assert_eq!(cloned["repo"]["refs"], 1, "{cloned}");

    // 2. The remote gains a change: fetch imports it, and sync fast-forwards main to it.
    site.land("theirs.txt", "theirs\n");
    let head = site.history().remove(0);
    let fetched = store.read(&["fetch", ORIGIN]).ok();
    assert_eq!(fetched["imported"], 1, "{transport:?}: {fetched}");
    assert_eq!(fetched["head"]["sha"], head.as_str(), "{fetched}");
    let status = store.read(&["sync", "status", ORIGIN]).ok();
    assert_eq!(
        (&status["ahead"], &status["behind"]),
        (&0.into(), &1.into())
    );
    let synced = on(&store, &["sync", ORIGIN]).ok();
    assert_eq!(synced["status"], "fast_forwarded", "{synced}");
    assert_eq!(synced["main"]["new"], fetched["head"]["changeset"]);

    // 3. A local change, pushed: a fast-forward, nothing rewritten, moved by the key's identity.
    land(&store, "ours.txt", "ours\n");
    let pushed = store.read(&["push", ORIGIN]).ok();
    assert_eq!(pushed["status"], "pushed", "{transport:?}: {pushed}");
    assert_eq!(pushed["old"], head.as_str(), "{pushed}");
    assert_eq!(pushed["rewritten"], 0, "{pushed}");
    let moved = site.newest_move();
    assert_eq!(moved["name"], "main", "{moved}");
    assert_eq!(moved["actor"], ADA, "{moved}");
    assert_eq!(site.oid(&moved["new"]), pushed["new"], "{moved}");

    // 4. Another, synced: published on the pushed commit, again by the key's identity.
    land(&store, "more.txt", "more\n");
    let synced = on(&store, &["sync", ORIGIN]).ok();
    assert_eq!(synced["status"], "pushed", "{transport:?}: {synced}");
    assert_eq!(synced["attempts"], 1, "{synced}");
    let chain = site.history();
    assert_eq!(chain[0], published(&synced), "{transport:?}");
    assert_eq!(chain[1], pushed["new"].as_str().expect("new"));
    assert_eq!(chain.len(), served.len() + 3, "{chain:?}");
    assert_eq!(site.newest_move()["actor"], ADA);

    // 5. Both stores sound.
    store.assert_clean("after the round trip");
    site.assert_clean();
}

/// [`round_trip`] over the ssh stand-in.
#[test]
fn clone_fetch_sync_a_served_store() {
    round_trip(Transport::Ssh);
}

/// [`round_trip`] over HTTP, every request the caller's.
#[test]
fn http_clone_fetch_sync_a_served_store() {
    round_trip(Transport::Http);
}

/// A branch of the client store is pushed to the served branch of its name, which it creates:
/// a feature branch and a release branch, each at the change's commit, by the key's identity,
/// and moved forward by the next push; the served `main` stays.
#[test]
fn push_creates_a_branch_on_a_served_store() {
    let site = Site::new();
    let store = site.ssh(ADA).cloned();
    let main = site.main_head();

    for branch in ["feature/x", "release/1"] {
        // 1. The branch one change past main, pushed: created.
        let base = main_at(&store);
        let change = store.commit(AGENT, "x.txt", branch, Some(&base));
        on(&store, &["ref", "set", branch, &change, "--create"]).ok();
        let pushed = store.read(&["push", ORIGIN, branch]).ok();
        assert_eq!(pushed["status"], "created", "{branch}: {pushed}");
        assert_eq!(pushed["branch"], branch, "{pushed}");
        assert_eq!(pushed["rewritten"], 0, "{pushed}");

        // 2. The served store holds it at that commit, made by the key's identity.
        let made = site.ref_at(branch).expect("the branch made");
        assert_eq!(site.oid(&made), pushed["new"], "{branch}");
        let moved = site.newest_move();
        assert_eq!(moved["name"], branch, "{moved}");
        assert_eq!(moved["actor"], ADA, "{moved}");

        // 3. One change further, pushed: moved forward.
        let next = store.commit(AGENT, "y.txt", branch, Some(&change));
        on(&store, &["ref", "set", branch, &next, "--expect", &change]).ok();
        let again = store.read(&["push", ORIGIN, branch]).ok();
        assert_eq!(again["status"], "pushed", "{branch}: {again}");
        assert_eq!(again["old"], pushed["new"], "{again}");
        let moved = site.ref_at(branch).expect("the branch");
        assert_eq!(site.oid(&moved), again["new"], "{branch}");
    }

    // 4. main as it was.
    assert_eq!(site.main_head(), main);
    site.assert_clean();
}

/// The error of a push the served store's policy refused: `E_REMOTE_REJECTED` `declined`, exit
/// 5, not retryable, with the remote's `E_POLICY_DENIED` line for `identity` and `branch`.
fn assert_denied(failed: &Run, identity: &str, branch: &str) {
    let error = failed.err("E_REMOTE_REJECTED");
    assert_eq!(failed.exit(), 5, "{failed}");
    assert_eq!(error["details"]["reason"], "declined", "{failed}");
    assert_eq!(error["retryable"], false, "{failed}");
    let said = error["details"]["remote_messages"].to_string();
    let line = format!("E_POLICY_DENIED: {identity} may not push {branch}");
    assert!(said.contains(&line), "{failed}");
}

/// The served store's refusals reach the client by their reason: a remote that moved since the
/// last fetch is `remote_moved`, a diverged one `diverged`, a ref the key's grant does not
/// cover `declined` with the remote's policy line, and a key no grant names `auth`. The grant
/// is by ref: the same key creates a feature branch, which the remote's op log records under
/// the key's identity, never the client's `--as`.
#[test]
fn served_refusals_reach_the_store_client() {
    let site = Site::new();
    let store = site.ssh(CAROL).cloned();

    // 1. The remote moved since the fetch: remote_moved, nothing sent.
    let theirs = site.land("theirs.txt", "theirs\n");
    land(&store, "ours.txt", "ours\n");
    let failed = store.read(&["push", ORIGIN]);
    let error = failed.err("E_REMOTE_DIVERGED");
    assert_eq!(failed.exit(), 3, "{failed}");
    assert_eq!(error["details"]["reason"], "remote_moved", "{failed}");

    // 2. Fetched, the histories differ: diverged.
    store.read(&["fetch", ORIGIN]).ok();
    let failed = store.read(&["push", ORIGIN]);
    let error = failed.err("E_REMOTE_DIVERGED");
    assert_eq!(failed.exit(), 3, "{failed}");
    assert_eq!(error["details"]["reason"], "diverged", "{failed}");
    assert_eq!(site.main_head(), theirs);

    // 3. A key that may not push main: its sync lands locally and is declined by policy.
    let bob = site.ssh(BOB);
    let failed = store.run_as(AGENT, &["sync", ORIGIN], &bob.vars(&[]));
    assert_denied(&failed, BOB, "main");
    let error = failed.err("E_REMOTE_REJECTED");
    assert_eq!(error["details"]["main_moved"], true, "{failed}");
    assert_eq!(site.main_head(), theirs);

    // 4. The grant is by ref: the same key creates feature/bob, and release/bob is declined.
    let reader = bob.cloned();
    let change = reader.commit(AGENT, "bob.txt", "bob\n", Some(&main_at(&reader)));
    for branch in ["feature/bob", "release/bob"] {
        on(&reader, &["ref", "set", branch, &change, "--create"]).ok();
    }
    let pushed = reader.read(&["push", ORIGIN, "feature/bob"]).ok();
    assert_eq!(pushed["status"], "created", "{pushed}");
    let moved = site.newest_move();
    assert_eq!(moved["name"], "feature/bob", "{moved}");
    assert_eq!(moved["actor"], BOB, "{moved}");
    assert_denied(
        &reader.read(&["push", ORIGIN, "release/bob"]),
        BOB,
        "release/bob",
    );
    assert_eq!(site.ref_at("release/bob"), None);

    // 5. A key no grant names: auth, on a clone and on a fetch of a store that has the remote.
    let eve = site.ssh(EVE);
    let into = Store::empty();
    let line = ["--as", AGENT, "clone", &eve.url, "--into", into.path()];
    let fetch = store.run_as(AGENT, &["fetch", ORIGIN], &eve.vars(&[]));
    for failed in [run(&line, &eve.vars(&[])), fetch] {
        let error = failed.err("E_REMOTE_REJECTED");
        assert_eq!(failed.exit(), 5, "{failed}");
        assert_eq!(error["details"]["reason"], "auth", "{failed}");
        assert_eq!(error["retryable"], false, "{failed}");
    }

    // 6. No op of the served store names the client's own agent.
    let ops = site.so(&["op", "log", "-n", "1000"]).to_string();
    assert!(!ops.contains(AGENT), "{ops}");
    site.assert_clean();
}

/// A local change that conflicts with the served head is `E_CONFLICT`, naming the change and
/// what it landed onto; neither side's `main` moves.
#[test]
fn sync_conflict_through_a_served_store_is_e_conflict() {
    let site = Site::new();
    let store = site.ssh(ADA).cloned();
    let mine = land(&store, "seed.txt", "mine\n");
    let theirs = site.land("seed.txt", "theirs\n");

    // 1. The sync: a conflict, with the change and its target.
    let failed = on(&store, &["sync", ORIGIN]);
    let error = failed.err("E_CONFLICT");
    assert_eq!(failed.exit(), 3, "{failed}");
    assert_eq!(error["details"]["change"], mine.as_str(), "{failed}");
    let status = store.read(&["sync", "status", ORIGIN]).ok();
    assert_eq!(error["details"]["onto"], status["remote_head"]["changeset"]);

    // 2. Nothing moved, on either side.
    assert_eq!(main_at(&store), mine);
    assert_eq!(site.main_head(), theirs);
    store.assert_clean("after the conflict");
    site.assert_clean();
}

/// A merge commit a git client pushed to a served branch crosses a store whole: a clone that
/// follows the branch imports it with both parents and exports it by its id, and a change the
/// store pushes on it has the merge as its parent. The served `main` stays.
#[test]
fn a_merge_on_a_served_branch_round_trips() {
    let site = Site::new();
    let main = site.main_head();
    let served = site.history().len();

    // 1. A git client merges a side branch into feature/m with a merge commit, and pushes it.
    let url = site.served.ssh_url(STORE);
    site.git(ADA, site.work.path(), &["clone", "--quiet", &url, "git"]);
    let work = site.work.path().join("git");
    let commit = |branch: &str, path: &str| {
        site.git(ADA, &work, &["checkout", "--quiet", "-b", branch, "main"]);
        fs::write(work.join(path), path).expect("write a file");
        site.git(ADA, &work, &["add", path]);
        site.git(ADA, &work, &["commit", "--quiet", "-m", path]);
        site.git(ADA, &work, &["rev-parse", "HEAD"])
    };
    let merged = commit("feature/side", "side.txt");
    let ours = commit("feature/m", "ours.txt");
    let line = [
        "merge",
        "--quiet",
        "--no-ff",
        "-m",
        "merge side",
        "feature/side",
    ];
    site.git(ADA, &work, &line);
    let merge = site.git(ADA, &work, &["rev-parse", "HEAD"]);
    site.git(ADA, &work, &["push", "--quiet", "origin", "feature/m"]);

    // 2. A store that follows feature/m: the merge and both its sides, imported.
    let (store, cloned) = site.ssh(ADA).cloned_with(&["--branch", "feature/m"]);
    assert_eq!(cloned["fetch"]["head"]["sha"], merge.as_str(), "{cloned}");
    assert_eq!(cloned["imported"], served + 3, "{cloned}");
    let parents = store.show("main")["parents"].take();
    assert_eq!(parents.as_array().expect("parents").len(), 2, "{parents}");

    // 3. Exported, it is the same commit with the same parents.
    let bare = exported(&site, &store, site.work.path(), "export.git");
    let listed = git_in(
        &site.served,
        &bare,
        &["rev-list", "--parents", "-1", "main"],
    );
    assert_eq!(listed, format!("{merge} {ours} {merged}"));

    // 4. A change pushed on it goes to feature/m, the merge its parent; main stays.
    land(&store, "after.txt", "after\n");
    let pushed = store.read(&["push", ORIGIN]).ok();
    assert_eq!(pushed["status"], "pushed", "{pushed}");
    assert_eq!(pushed["branch"], "feature/m", "{pushed}");
    assert_eq!(pushed["rewritten"], 0, "{pushed}");
    site.git(ADA, &work, &["fetch", "--quiet", "origin"]);
    let tip = site.git(ADA, &work, &["rev-parse", "origin/feature/m"]);
    assert_eq!(pushed["new"], tip.as_str());
    assert_eq!(
        site.git(ADA, &work, &["rev-parse", "origin/feature/m^"]),
        merge
    );
    assert_eq!(site.main_head(), main);
    store.assert_clean("after the merge's round trip");
    site.assert_clean();
}

/// A race the served store calls stale reaches the store client as `remote_moved`, and its sync
/// retries: B's push of two changes is held after its advertisement while A moves the served
/// `main` to the first of them, which B had published on a branch. The served store finds B's
/// tip forward of the new head and its old value stale; B's sync fetches, finds its first
/// change on the remote and pushes the second on its second attempt.
#[cfg(unix)]
#[test]
fn a_race_the_served_store_calls_stale_is_retried() {
    let site = Site::new();
    let base = site.history().len();
    let b = site.ssh(CAROL).cloned();

    // 1. B lands a change, publishes it as feature/w, and lands another on it.
    let first = land(&b, "one.txt", "one\n");
    on(&b, &["ref", "set", "feature/w", &first, "--create"]).ok();
    let branch = b.read(&["push", ORIGIN, "feature/w"]).ok();
    land(&b, "two.txt", "two\n");

    // 2. A follows feature/w: its main is B's first change.
    let (a, _) = site.ssh(ADA).cloned_with(&["--branch", "feature/w"]);
    let gate = Hold::on(&site, CAROL);
    let held = gate.ssh();

    // 3. B's sync, its push held; A's push moves the served main to the first change meanwhile.
    let line = ["sync", ORIGIN];
    let (synced, won) = std::thread::scope(|scope| {
        let pushing = scope.spawn(|| b.run_as(AGENT, &line, &[(SSH_VAR, &held)]));
        gate.reached();
        let won = a.read(&["push", ORIGIN, "main:main"]).ok();
        gate.release();
        (pushing.join().expect("B's sync"), won)
    });
    assert_eq!(won["status"], "pushed", "{won}");
    assert_eq!(won["new"], branch["new"], "{won}");

    // 4. B's sync retried once and published its second change on the first.
    let synced = synced.ok();
    assert_eq!(synced["status"], "pushed", "{synced}");
    assert_eq!(synced["attempts"], 2, "{synced}");
    let chain = site.history();
    assert_eq!(chain[0], published(&synced));
    assert_eq!(chain[1], won["new"].as_str().expect("new"));
    assert_each_once(&site, base, &["one.txt", "two.txt"], "a stale race");
    b.assert_clean("after the race");
    site.assert_clean();
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

/// A clone whose answer is cut inside the pack is `E_REMOTE_UNREACHABLE`, retryable, and leaves
/// a sound store that holds the remote and no changeset of the cut pack; the same clone run
/// again into it imports the rest, and once more changes nothing.
#[test]
fn a_cut_clone_of_a_served_store_resumes() {
    let site = Site::holding(&[("noise.txt", &noise(NOISE_BYTES))]);
    let served = site.history();
    let link = site.ssh(ADA);
    let cut = site.served.ssh_command_with(ADA, SshFault::CutReceive(CUT));

    // 1. The cut clone: unreachable in its fetch, retryable, the store made.
    let store = Store::empty().with_env(&link.vars(&[]));
    let args = ["--as", AGENT, "clone", &link.url, "--into", store.path()];
    let failed = run(&args, &link.vars(&[(SSH_VAR, &cut)]));
    let error = failed.err("E_REMOTE_UNREACHABLE");
    assert_eq!(failed.exit(), 4, "{failed}");
    assert_eq!(error["retryable"], true, "{failed}");
    assert_eq!(error["details"]["created"], true, "{failed}");
    assert_eq!(error["details"]["phase"], "fetch", "{failed}");

    // 2. What the cut left: the remote recorded, no changeset, no main, nothing damaged.
    let partial = store.info();
    assert_eq!(partial["changesets"], 0, "{partial}");
    assert_eq!(partial["refs"], 0, "{partial}");
    let remotes = store.read(&["remote", "list"]).ok();
    assert_eq!(remotes["remotes"][0]["name"], ORIGIN, "{remotes}");
    store.assert_clean("after the cut clone");

    // 3. The same clone again, uncut: the whole history, main at the served head.
    let resumed = run(&args, &link.vars(&[])).ok();
    assert_eq!(resumed["imported"], served.len(), "{resumed}");
    assert_eq!(resumed["fetch"]["head"]["sha"], served[0].as_str());
    assert_eq!(resumed["main"], resumed["fetch"]["head"]["changeset"]);
    assert_eq!(store.info()["changesets"], served.len());

    // 4. Once more: nothing new.
    let again = run(&args, &link.vars(&[])).ok();
    assert_eq!(again["imported"], 0, "{again}");
    assert_eq!(again["main"], resumed["main"]);
    store.assert_clean("after the resumed clone");
    site.assert_clean();
}

/// A push whose client is killed after the served store applied it and before the answer came:
/// the remote holds the change, moved once by the key's identity. The push run again finds the
/// remote at its commit and sends nothing: `up_to_date`, with `old` the commit the client last
/// knew and `new` the one the lost push made, its one `git push` sending no pack; run once more,
/// `old` is `new`.
#[cfg(unix)]
#[test]
fn a_killed_push_replays_on_a_served_store() {
    use std::os::unix::fs::PermissionsExt as _;
    use std::os::unix::process::ExitStatusExt as _;

    let site = Site::new();
    let link = site.ssh(ADA);
    let store = link.cloned();
    let before = site.main_head();
    let known = site.history().remove(0);
    let count = site.main_moves();
    let change = land(&store, "killed.txt", "killed\n");

    // 1. A stand-in that stays after a push's server ended, its stderr off the client's, so
    //    that the client still waits and nobody holds the pipes the test reads once it is
    //    killed; a fetch passes.
    let dir = site.work.path().join("held");
    fs::create_dir(&dir).expect("make the wrapper's directory");
    let wrapper = dir.join("ssh");
    let script = format!(
        "#!/bin/sh\nexec 2>/dev/null\ncase \"$*\" in\n  \
         *git-receive-pack*) exec {held} \"$@\" ;;\n  *) exec {plain} \"$@\" ;;\nesac\n",
        held = site
            .served
            .ssh_command_with(ADA, SshFault::HoldAfterExit(HOLD)),
        plain = site.served.ssh_command(ADA),
    );
    fs::write(&wrapper, script).expect("write the wrapper");
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).expect("make it runnable");
    let ssh = format!("'{}'", path_text(&wrapper));

    // 2. The push, killed once the served main moved.
    let args = ["--repo", store.path(), "push", ORIGIN];
    let mut push = Child::spawn(&mut swarmops(&args, &link.vars(&[(SSH_VAR, &ssh)])));
    let deadline = Instant::now() + CHILD_TIMEOUT;
    while site.main_head() == before {
        assert!(Instant::now() < deadline, "the push never arrived");
        std::thread::sleep(HOLD_POLL);
    }
    let kill = std::process::Command::new("kill")
        .args(["-KILL", &push.id().to_string()])
        .status()
        .expect("run kill");
    assert!(kill.success());
    let (killed, _) = push.finish(Instant::now() + CHILD_TIMEOUT);
    assert_eq!(killed.status.signal(), Some(SIGKILL), "{killed:?}");

    // 3. Between the kill and the retry: the remote holds the change, moved once, by the key.
    assert_eq!(site.main_moves(), count + 1);
    let moved = site.newest_move();
    assert_eq!(moved["old"], before.as_str(), "{moved}");
    assert_eq!(moved["actor"], ADA, "{moved}");
    let sent = site.oid(&moved["new"]);
    assert_eq!(site.history()[..2], [text_of(&sent), known.clone()]);
    site.assert_clean();

    // 4. The push again: up to date from the commit it knew to the one the lost push made,
    //    nothing written; its one git push brought the remote no pack.
    let ledger = GitLedger::new(&site.git);
    let counted = path_text(&ledger.program());
    let again = run(&args, &link.vars(&[(GIT_VAR, &counted)])).ok();
    assert_eq!(again["status"], "up_to_date", "{again}");
    assert_eq!(again["old"], known.as_str(), "{again}");
    assert_eq!(again["new"], sent, "{again}");
    assert_eq!(again["head_changeset"], change.as_str(), "{again}");
    assert_eq!(again["objects_written"], 0, "{again}");
    let children = ledger.children();
    let pushes = children.iter().filter(|child| child.subcommand == "push");
    assert_eq!(pushes.count(), 1, "{children:?}");
    let record = site.newest_record();
    assert_eq!(record["service"], "push", "{record}");
    assert_eq!(record["bytes"], 0, "{record}");
    assert_eq!(record["exit"], 0, "{record}");

    // 5. Nothing moved on the remote; once more, old is new.
    assert_eq!(site.main_moves(), count + 1);
    let settled = store.read(&["push", ORIGIN]).ok();
    assert_eq!(settled["status"], "up_to_date", "{settled}");
    assert_eq!(settled["old"], sent, "{settled}");
    assert_eq!(settled["new"], sent, "{settled}");
    store.assert_clean("after the replay");
    site.assert_clean();
}

/// [`STORES`] stores and the served store's own agent each add changes, the stores syncing in
/// turn: every store's export of `main` is then the served history, commit by commit and tree
/// by tree, sound for `git fsck --strict`, and every store passes `verify --deep`. Changeset
/// ids are each store's own and are not compared.
#[test]
fn every_store_exports_the_same_ids() {
    let site = Site::new();
    let base = site.history().len();
    let keys = [ADA, CAROL];
    let links: Vec<Link> = (0..STORES).map(|n| site.ssh(keys[n % 2])).collect();
    let stores: Vec<Store> = links.iter().map(Link::cloned).collect();

    // 1. Each store lands a change and syncs it, the served store landing its own between: a
    //    later store's change is landed again onto what the others published.
    let mut paths = Vec::new();
    for (n, store) in stores.iter().enumerate() {
        let (ours, theirs) = (format!("stores/{n}.txt"), format!("served/{n}.txt"));
        land(store, &ours, "ours\n");
        let synced = on(store, &["sync", ORIGIN]).ok();
        assert_eq!(synced["status"], "pushed", "store {n}: {synced}");
        site.land(&theirs, "theirs\n");
        paths.extend([ours, theirs]);
    }

    // 2. Each store takes what the others published.
    for (n, store) in stores.iter().enumerate() {
        let synced = on(store, &["sync", ORIGIN]).ok();
        assert_eq!(synced["status"], "fast_forwarded", "store {n}: {synced}");
    }
    let paths: Vec<&str> = paths.iter().map(String::as_str).collect();
    assert_each_once(&site, base, &paths, "every store synced");

    // 3. Each export of main is the served history, by commit and tree.
    let history = "--format=%H %T";
    let expected = site.served_git(&["log", history, "main"]);
    for (n, store) in stores.iter().enumerate() {
        let export = exported(&site, store, site.work.path(), &format!("export-{n}.git"));
        let ours = git_in(&site.served, &export, &["log", history, "main"]);
        assert_eq!(ours, expected, "store {n}");
        git_in(
            &site.served,
            &export,
            &["fsck", "--strict", "--no-dangling"],
        );
        store.assert_clean(&format!("store {n}"));
    }
    site.assert_clean();
}
