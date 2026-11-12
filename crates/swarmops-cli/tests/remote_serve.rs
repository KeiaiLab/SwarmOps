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

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;
use swarmops_testkit::child;
use swarmops_testkit::git::fixture;
use swarmops_testkit::remote::{GitServer, HttpAuth, HttpServer};
use swarmops_testkit::serve::{HttpIdentity, ServeSite};
use tempfile::TempDir;

use crate::support::{Run, Store, Swarmops, run};

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
