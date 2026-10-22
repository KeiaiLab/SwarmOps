//! The git remotes' acceptance suite (M13): the roadmap's criteria over smart HTTP and the ssh
//! stand-in, the error codes' golden envelopes and the crash sweep. Written by the M13 plan's
//! task C10.
//!
//! Each test serves bare repositories with the test kit's [`GitServer`] and runs the binary with
//! a git environment of its own: the server's git in `SWARMOPS_GIT`, an empty `HOME`, no system
//! or global configuration. Each runs once per [`Transport`]: git's own smart HTTP on
//! `127.0.0.1`, and the ssh stand-in (`GIT_SSH_COMMAND`), which runs git's pack programs on the
//! server's repositories. Everything is offline.
//!
//! ```text
//!   store ── so fetch | push | sync ── git ──┬─ http://127.0.0.1:<port>/repo.git ─┐
//!                                            └─ ssh://git@remote.invalid/repo.git ─┴─ GitServer
//! ```
//!
//! The tests that put a stand-in for git in `SWARMOPS_GIT` (a shell script that does its part,
//! then runs the real git) are written for Unix shells, and gated so. The crash sweep drives the
//! test hooks, so it exists only with the `test-hooks` feature, which the gates turn on.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use swarmops_testkit::remote::{GitServer, HttpAuth, HttpServer, MergeMethod, SSH_HOST, SshMode};
use tempfile::TempDir;

use crate::support::{Run, Store, run};

/// The agent the commands that need an identity run as.
const AGENT: &str = "agent/worker-17";

/// The repository every test serves.
const REPO: &str = "repo";

/// The remote's name in the store.
const ORIGIN: &str = "origin";

/// The test hook that makes a host count as GitHub's.
const GITHUB_HOSTS: &str = "SWARMOPS_TEST_GITHUB_HOSTS";

/// The variable that names the git the binary runs.
const GIT_VAR: &str = "SWARMOPS_GIT";

/// The variable git takes its ssh command from.
const SSH_VAR: &str = "GIT_SSH_COMMAND";

/// GitHub's file limits, in bytes: 100 MiB refused, 50 MiB warned about.
const FILE_REFUSED: u64 = 100 * 1024 * 1024;
const FILE_WARNED: u64 = 50 * 1024 * 1024;

/// Where [`HttpServer::cut_after`] ends a response in these tests: past the headers of the
/// first response, inside its body, so that git meets an early end of file. (A cut inside the
/// headers loses the smart content type, and git then falls back to the dumb protocol and finds
/// no refs: not a network failure.)
const CUT_AT: usize = 200;

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
    /// The server's smart HTTP, served whatever the transport (the ssh tests do not use it).
    http: HttpServer,
    /// How the store reaches it.
    transport: Transport,
    /// git's empty `HOME` for the product's children, removed when the remote is dropped.
    _home: TempDir,
    /// The variables every run sets.
    env: Vec<(String, String)>,
}

impl Remote {
    /// [`REPO`] served over `transport`, with two commits on `main`.
    fn new(transport: Transport) -> Remote {
        let remote = Remote::empty(transport);
        remote.commit("main", "first", &[("a.txt", "a\n")]);
        remote.commit("main", "second", &[("b.txt", "b\n")]);
        remote
    }

    /// [`REPO`] served over `transport`, with no commit.
    fn empty(transport: Transport) -> Remote {
        let server = GitServer::new();
        server.create(REPO);
        Remote::serving(server, transport)
    }

    /// The repositories of `server`, served over `transport`.
    fn serving(server: GitServer, transport: Transport) -> Remote {
        let http = server.http(HttpAuth::Anonymous);
        let home = tempfile::tempdir().expect("a temp HOME");

        // 1. The server's git, an empty HOME, no system or global configuration.
        let git = real_git(&server);
        let mut env = vec![
            (GIT_VAR.to_owned(), path_text(&git)),
            ("HOME".to_owned(), path_text(home.path())),
            ("GIT_CONFIG_NOSYSTEM".to_owned(), "1".to_owned()),
            ("GIT_CONFIG_GLOBAL".to_owned(), "/dev/null".to_owned()),
        ];

        // 2. Over ssh, the stand-in as git's ssh.
        if transport == Transport::Ssh {
            env.push((SSH_VAR.to_owned(), server.ssh_command(SshMode::Serve)));
        }
        Remote {
            server,
            http,
            transport,
            _home: home,
            env,
        }
    }

    /// The URL of repository `name` over this remote's transport.
    fn url_of(&self, name: &str) -> String {
        match self.transport {
            Transport::Http => self.http.url(name),
            Transport::Ssh => self.server.ssh_url(name),
        }
    }

    /// [`REPO`]'s URL.
    fn url(&self) -> String {
        self.url_of(REPO)
    }

    /// The host the store's URL names, as `SWARMOPS_TEST_GITHUB_HOSTS` takes it.
    fn host(&self) -> String {
        match self.transport {
            Transport::Http => self.http.host(),
            Transport::Ssh => SSH_HOST.to_owned(),
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

    /// A fresh store whose commands run with this remote's git environment.
    fn store(&self) -> Store {
        Store::init().with_env(&self.vars(&[]))
    }

    /// A store that records the remote as [`ORIGIN`], following `main`, without asking it.
    fn added(&self) -> Store {
        let store = self.store();
        let url = self.url();
        let line = ["remote", "add", ORIGIN, url.as_str(), "--branch", "main"];
        store.read(&line).ok();
        store
    }

    /// A store that fetched the remote and whose `main` is at the remote's `main`, as a clone
    /// leaves it.
    fn cloned(&self) -> Store {
        let store = self.added();
        let fetched = store.read(&["fetch", ORIGIN]).ok();
        let head = fetched["head"]["changeset"].as_str().expect("a head");
        let line = ["ref", "set", "main", head, "--create"];
        store.run_as(AGENT, &line, &[]).ok();
        store
    }

    /// How many commits of the remote `branch`'s first-parent chain change `path`: 1 when the
    /// change is there once.
    fn changed(&self, branch: &str, path: &str) -> usize {
        let listed = self.git(&["log", "--first-parent", "--format=%H", branch, "--", path]);
        listed.lines().count()
    }

    /// The merge commits on the remote `branch`'s first-parent chain.
    fn merges(&self, branch: &str) -> usize {
        let listed = self.git(&["rev-list", "--first-parent", "--merges", branch]);
        listed.lines().count()
    }

    /// Every ref of [`REPO`] and what it names: the remote's state, to compare.
    fn refs(&self) -> String {
        self.git(&["for-each-ref", "--format=%(refname) %(objectname)"])
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

/// `store`'s mirror.
fn mirror(store: &Store) -> PathBuf {
    Path::new(store.path()).join("git").join("mirror.git")
}

/// What the mirror's tracking ref of [`ORIGIN`]'s `branch` names, if it exists.
fn tracking(remote: &Remote, store: &Store, branch: &str) -> Option<String> {
    let full = format!("refs/remotes/{ORIGIN}/{branch}");
    let format = "--format=%(objectname)";
    let listed = git_out(
        &remote.server,
        &mirror(store),
        &["for-each-ref", format, &full],
    );
    (!listed.is_empty()).then_some(listed)
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
    let old = main_at(store);
    let new = store.commit(AGENT, path, content, old.as_deref());

    // main moved from what it named, or made.
    let mut line = vec!["ref", "set", "main", new.as_str()];
    match &old {
        Some(old) => line.extend(["--expect", old.as_str()]),
        None => line.push("--create"),
    }
    store.run_as(AGENT, &line, &[]).ok();
    new
}

/// `push <args…>` on `store`.
fn push(store: &Store, args: &[&str]) -> Run {
    let mut line = vec!["push", ORIGIN];
    line.extend_from_slice(args);
    store.read(&line)
}

/// `sync <args…>` on `store` as [`AGENT`].
fn sync(store: &Store, args: &[&str]) -> Run {
    let mut line = vec!["sync", ORIGIN];
    line.extend_from_slice(args);
    store.run_as(AGENT, &line, &[])
}

/// `args` on `store` as [`AGENT`], with `remote`'s variables and then `env` set.
fn run_with(remote: &Remote, store: &Store, args: &[&str], env: &[(&str, &str)]) -> Run {
    let mut line = vec!["--repo", store.path(), "--as", AGENT];
    line.extend_from_slice(args);
    run(&line, &remote.vars(env))
}

/// The changeset of the remote's head, as a fetch of it reports.
fn changeset_of(store: &Store) -> String {
    let fetched = store.read(&["fetch", ORIGIN]).ok();
    fetched["head"]["changeset"]
        .as_str()
        .expect("a head")
        .to_owned()
}

/// Every file under `dir` but those under `skip` (relative names), with its bytes: a tree's
/// state, to compare.
fn files_of(dir: &Path, skip: &[&str]) -> BTreeMap<String, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        for entry in fs::read_dir(&next).expect("read a directory") {
            let path = entry.expect("an entry").path();
            let name = path_text(path.strip_prefix(dir).expect("under dir"));

            // Skipped trees, then directories walked, then files read.
            if skip.contains(&name.as_str()) {
                continue;
            }
            if path.is_dir() {
                pending.push(path);
                continue;
            }
            files.insert(name, fs::read(&path).expect("read a file"));
        }
    }
    files
}

/// A stand-in for git: `script` runs first, with `$@` git's arguments, then the real git. The
/// value for `SWARMOPS_GIT`.
#[cfg(unix)]
fn stand_in(remote: &Remote, dir: &Path, script: &str) -> String {
    use std::os::unix::fs::PermissionsExt as _;

    let program = dir.join("git");
    let real = real_git(&remote.server);
    let text = format!("#!/bin/sh\n{script}\nexec '{}' \"$@\"\n", path_text(&real));
    fs::write(&program, text).expect("write the stand-in");
    let mut mode = fs::metadata(&program).expect("the stand-in").permissions();
    mode.set_mode(0o755);
    fs::set_permissions(&program, mode).expect("make the stand-in runnable");
    path_text(&program)
}

/// The shell test that holds when git's arguments name the subcommand `verb`.
#[cfg(unix)]
fn names_verb(verb: &str) -> String {
    format!("act=no\nfor arg in \"$@\"; do [ \"$arg\" = {verb} ] && act=yes; done\n")
}

// ---------------------------------------------------------------------------------------------
// Criterion 1: the round trip.

/// The fixtures whose repositories `git fsck --strict` accepts and whose every commit a
/// changeset holds as git wrote it (the round trip's `FSCK_CLEAN`): no lossy class.
const FAITHFUL: [&str; 6] = [
    "linear",
    "loose_and_packed",
    "merges",
    "modes",
    "refs",
    "roots",
];

/// A server holding the fixture `name`'s repository as [`REPO`], pushes allowed.
fn fixture_server(name: &str) -> GitServer {
    let fixture = swarmops_testkit::git::fixture(name);
    let source = fixture.repo.as_ref().expect("the fixture's repository");
    let server = GitServer::new();
    swarmops_testkit::fs::copy_dir(source, &server.bare(REPO));
    git_out(
        &server,
        &server.bare(REPO),
        &["config", "http.receivepack", "true"],
    );
    server
}

/// Each faithful fixture cloned, then `main` pushed to a new branch: nothing is rewritten,
/// every pushed ref names the commit git wrote, and the remote passes `git fsck --strict`.
#[test]
fn clone_then_push_main_to_a_new_branch_keeps_every_id() {
    for transport in TRANSPORTS {
        for name in FAITHFUL {
            let remote = Remote::serving(fixture_server(name), transport);
            let dir = tempfile::tempdir().expect("a temp dir");
            let into = path_text(&dir.path().join("store"));
            let url = remote.url();

            // 1. The clone follows the fixture's main.
            let line = ["--as", AGENT, "clone", url.as_str(), "--into", &into];
            let cloned = run(&line, &remote.vars(&[])).ok();
            assert_eq!(cloned["remote"]["branch"], "main", "{name} {transport:?}");

            // 2. main pushed as a new branch: the same commit, nothing rewritten.
            let line = ["--repo", &into, "push", ORIGIN, "main:copy"];
            let pushed = run(&line, &remote.vars(&[])).ok();
            assert_eq!(
                pushed["status"], "created",
                "{name} {transport:?}: {pushed}"
            );
            assert_eq!(pushed["rewritten"], 0, "{name} {transport:?}: {pushed}");
            assert_eq!(
                pushed["objects_written"], 0,
                "{name} {transport:?}: {pushed}"
            );
            let main = remote.head("main").expect("main");
            assert_eq!(
                remote.head("copy"),
                Some(main.clone()),
                "{name} {transport:?}"
            );
            assert_eq!(pushed["new"], main.as_str(), "{name} {transport:?}");

            // 3. The remote is a sound repository.
            remote.git(&["fsck", "--strict", "--no-dangling"]);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Criterion 2: fast-forward push, rerun, moved remote.

/// Lands on `main` push as a fast-forward of what the clone saw: one commit per land on the
/// remote's first-parent chain, each holding its change once, the tracking ref at the new head.
#[test]
fn push_after_lands_fast_forwards() {
    for transport in TRANSPORTS {
        let remote = Remote::new(transport);
        let old = remote.head("main").expect("main");
        let store = remote.cloned();
        land(&store, "one.txt", "one\n");
        land(&store, "two.txt", "two\n");

        let pushed = push(&store, &[]).ok();
        let new = remote.head("main").expect("main");
        assert_eq!(pushed["status"], "pushed", "{transport:?}: {pushed}");
        assert_eq!(pushed["old"], old.as_str());
        assert_eq!(pushed["new"], new.as_str());
        assert_eq!(remote.git(&["rev-parse", "main~2"]), old, "{transport:?}");
        for path in ["one.txt", "two.txt"] {
            assert_eq!(remote.changed("main", path), 1, "{transport:?} {path}");
        }
        assert_eq!(tracking(&remote, &store, "main"), Some(new));
    }
}

/// A stand-in for git that counts its runs in a file, then runs the real git: the value for
/// `SWARMOPS_GIT`, and the file.
#[cfg(unix)]
fn counting_git(remote: &Remote, dir: &Path) -> (String, PathBuf) {
    let count = dir.join("git-runs");
    let script = format!("echo run >> '{}'", path_text(&count));
    (stand_in(remote, dir, &script), count)
}

/// The same push run again sends nothing and starts no git at all: `status: "up_to_date"`.
#[cfg(unix)]
#[test]
fn push_run_again_starts_no_git() {
    for transport in TRANSPORTS {
        let remote = Remote::new(transport);
        let store = remote.cloned();
        land(&store, "local.txt", "local\n");
        let first = push(&store, &[]).ok();

        // The stand-in counts every git the second push starts.
        let dir = tempfile::tempdir().expect("a temp dir");
        let (git, count) = counting_git(&remote, dir.path());
        let again = run_with(&remote, &store, &["push", ORIGIN], &[(GIT_VAR, &git)]).ok();
        assert_eq!(again["status"], "up_to_date", "{transport:?}: {again}");
        assert_eq!(again["new"], first["new"]);
        let runs = fs::read_to_string(&count).map_or(0, |text| text.lines().count());
        assert_eq!(runs, 0, "{transport:?}");
    }
}

/// A push to a remote that moved since the last fetch fails with `E_REMOTE_DIVERGED`
/// `remote_moved`, and changes nothing: the store's files, the mirror's tracking ref and the
/// remote's refs are byte for byte what they were.
#[test]
fn push_to_a_moved_remote_fails_and_changes_nothing() {
    for transport in TRANSPORTS {
        let remote = Remote::new(transport);
        let store = remote.cloned();
        land(&store, "local.txt", "local\n");

        // The export the failing push makes, made once before: its external ids are in.
        push(&store, &["main:scratch"]).ok();
        remote.commit("main", "elsewhere", &[("e.txt", "e\n")]);
        let store_before = files_of(Path::new(store.path()), &["git"]);
        let tracking_before = tracking(&remote, &store, "main");
        let remote_before = remote.refs();

        let failed = push(&store, &[]);
        let error = failed.err("E_REMOTE_DIVERGED");
        assert_eq!(failed.exit(), 3, "{failed}");
        assert_eq!(error["details"]["reason"], "remote_moved", "{failed}");
        assert_eq!(error["retryable"], false, "{failed}");
        let store_after = files_of(Path::new(store.path()), &["git"]);
        assert!(
            store_after == store_before,
            "{transport:?}: the store changed"
        );
        assert_eq!(tracking(&remote, &store, "main"), tracking_before);
        assert_eq!(remote.refs(), remote_before, "{transport:?}");
    }
}

// ---------------------------------------------------------------------------------------------
// Criterion 3: sync after the three merge shapes.

/// A store with two lands on the remote's `main`, pushed to the branch `feature` as a person's
/// pull request: the remote, the store, and the landed paths.
fn feature_branch(transport: Transport) -> (Remote, Store, [&'static str; 2]) {
    let remote = Remote::new(transport);
    let store = remote.cloned();
    let paths = ["feature/one.txt", "feature/two.txt"];
    for path in paths {
        land(&store, path, &format!("{path}\n"));
    }
    push(&store, &["main:feature"]).ok();

    // Another person's work on main meanwhile, so that no merge is a fast-forward.
    remote.commit("main", "theirs", &[("theirs.txt", "theirs\n")]);
    (remote, store, paths)
}

/// After a sync: the remote's first-parent chain holds each landed change once and no merge
/// the sync made, `main` names the remote's head, and the store verifies clean.
fn assert_synced(remote: &Remote, store: &Store, paths: &[&str], what: &str) {
    for path in paths {
        assert_eq!(remote.changed("main", path), 1, "{what}: {path}");
    }
    assert_eq!(main_at(store), Some(changeset_of(store)), "{what}");
    store.assert_clean(what);
}

/// The pull request merged with a merge commit: the sync fast-forwards `main` to it and pushes
/// nothing.
#[test]
fn sync_after_a_merge_commit() {
    for transport in TRANSPORTS {
        let (remote, store, paths) = feature_branch(transport);
        let merged = remote
            .server
            .merge(REPO, MergeMethod::Commit, "feature", "main");

        let synced = sync(&store, &[]).ok();
        assert_eq!(
            synced["status"], "fast_forwarded",
            "{transport:?}: {synced}"
        );
        assert_eq!(remote.head("main"), Some(merged));
        assert_synced(&remote, &store, &paths, &format!("{transport:?}"));
    }
}

/// The pull request squashed with each commit's message listed: the sync drops both lands by
/// their trailers, and the remote keeps the squash alone.
#[test]
fn sync_after_a_squash_listing_commits() {
    use swarmops_testkit::remote::SquashMessage;

    for transport in TRANSPORTS {
        let (remote, store, paths) = feature_branch(transport);
        let method = MergeMethod::Squash(SquashMessage::CommitList);
        let squashed = remote.server.merge(REPO, method, "feature", "main");

        let synced = sync(&store, &[]).ok();
        let by: Vec<&Value> = synced["dropped"]
            .as_array()
            .expect("dropped")
            .iter()
            .map(|dropped| &dropped["by"])
            .collect();
        assert_eq!(by, [&json!("trailer"), &json!("trailer")], "{synced}");
        assert_eq!(remote.head("main"), Some(squashed));
        assert_synced(&remote, &store, &paths, &format!("{transport:?}"));
    }
}

/// The pull request squashed with its title only: the trailers are gone, so the sync drops the
/// lands because they merge as no change.
#[test]
fn sync_after_a_squash_with_title_only() {
    use swarmops_testkit::remote::SquashMessage;

    for transport in TRANSPORTS {
        let (remote, store, paths) = feature_branch(transport);
        let method = MergeMethod::Squash(SquashMessage::TitleOnly);
        let squashed = remote.server.merge(REPO, method, "feature", "main");

        let synced = sync(&store, &[]).ok();
        let dropped = synced["dropped"].as_array().expect("dropped");
        assert_eq!(dropped.len(), 2, "{synced}");
        assert!(
            dropped.iter().all(|dropped| dropped["by"] == "no_op"),
            "{synced}"
        );
        assert_eq!(remote.head("main"), Some(squashed));
        assert_synced(&remote, &store, &paths, &format!("{transport:?}"));
    }
}

/// The pull request rebased: each commit written again by the remote, its message kept, so the
/// sync drops the lands by their trailers.
#[test]
fn sync_after_a_rebase() {
    for transport in TRANSPORTS {
        let (remote, store, paths) = feature_branch(transport);
        let rebased = remote
            .server
            .merge(REPO, MergeMethod::Rebase, "feature", "main");

        let synced = sync(&store, &[]).ok();
        let dropped = synced["dropped"].as_array().expect("dropped");
        assert_eq!(dropped.len(), 2, "{synced}");
        assert_eq!(remote.head("main"), Some(rebased));
        assert_synced(&remote, &store, &paths, &format!("{transport:?}"));
    }
}

/// Lands on both sides: the store's land onto the remote's new head and are pushed, once each.
#[test]
fn sync_with_unpublished_lands_on_both_sides() {
    for transport in TRANSPORTS {
        let remote = Remote::new(transport);
        let store = remote.cloned();
        land(&store, "ours/one.txt", "one\n");
        land(&store, "ours/two.txt", "two\n");
        remote.commit("main", "theirs", &[("theirs.txt", "theirs\n")]);

        let synced = sync(&store, &[]).ok();
        assert_eq!(synced["status"], "pushed", "{transport:?}: {synced}");
        assert_eq!(synced["published"].as_array().map(Vec::len), Some(2));
        let paths = ["ours/one.txt", "ours/two.txt", "theirs.txt"];
        assert_eq!(remote.merges("main"), 0, "{transport:?}");
        assert_synced(&remote, &store, &paths, &format!("{transport:?}"));
    }
}

/// How many agents land and sync at once.
const AGENTS: usize = 8;

/// The answers an agent runs a command again on: the mirror lock or the store's taken, `main`
/// or the remote moved past the attempts.
fn retryable(run: &Run) -> bool {
    let envelope: Value = serde_json::from_str(&run.stdout).unwrap_or_default();
    envelope["error"]["retryable"] == true
        || envelope["error"]["code"] == "E_CAS_RETRY_EXHAUSTED"
        || envelope["error"]["code"] == "E_REMOTE_DIVERGED"
}

/// `args` on `store` as `agent`, run again while it ends retryable; the run that did not.
fn until_done(store: &Store, agent: &str, args: &[&str]) -> Run {
    loop {
        let done = store.run_as(agent, args, &[]);
        if !retryable(&done) {
            return done;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Eight agents each land a change of their own with a key and sync, at once: the remote's
/// `main` is linear and holds every land once, and `main` names its head.
#[test]
fn sync_under_eight_landing_agents() {
    for transport in TRANSPORTS {
        let remote = Remote::new(transport);
        let store = remote.cloned();
        let paths: Vec<String> = (0..AGENTS).map(|n| format!("agents/{n}.txt")).collect();

        std::thread::scope(|scope| {
            for (n, path) in paths.iter().enumerate() {
                let store = &store;
                scope.spawn(move || {
                    let agent = format!("agent/worker-{n}");

                    // 1. A commit on main's head, landed with the agent's key.
                    let head = main_at(store);
                    let changes = store.changes(&format!("agent-{n}"), path, "mine\n");
                    let mut line = vec!["commit", "-m", path.as_str(), "--changes", &changes];
                    line.extend(head.iter().flat_map(|head| ["--parent", head.as_str()]));
                    let committed = store.run_as(&agent, &line, &[]).ok();
                    let change = committed["changeset"].as_str().expect("an id").to_owned();
                    let key = format!("agent-{n}");
                    until_done(store, &agent, &["land", &change, "--key", &key]).ok();

                    // 2. Its sync.
                    until_done(store, &agent, &["sync", ORIGIN]).ok();
                });
            }
        });

        // A last sync publishes what a lost race left.
        until_done(&store, AGENT, &["sync", ORIGIN]).ok();
        let paths: Vec<&str> = paths.iter().map(String::as_str).collect();
        assert_eq!(remote.merges("main"), 0, "{transport:?}");
        assert_synced(&remote, &store, &paths, &format!("{transport:?}"));
    }
}

/// A keyed land that a sync published onto the remote's head, retried with its key, replays:
/// the sync's copy carries the key, so nothing lands twice.
#[test]
fn keyed_land_retried_after_sync_replays() {
    for transport in TRANSPORTS {
        let remote = Remote::new(transport);
        let store = remote.cloned();
        let head = main_at(&store).expect("main");
        let change = store.commit(AGENT, "keyed.txt", "keyed\n", Some(&head));
        let line = ["land", change.as_str(), "--key", "the-key"];
        store.run_as(AGENT, &line, &[]).ok();

        // The remote moved: the sync lands the change again, onto its head.
        remote.commit("main", "theirs", &[("theirs.txt", "theirs\n")]);
        let synced = sync(&store, &[]).ok();
        assert_eq!(synced["status"], "pushed", "{transport:?}: {synced}");
        let main = main_at(&store);

        // The retry replays: main does not move, the change is on the remote once.
        let retried = store.run_as(AGENT, &line, &[]).ok();
        assert_eq!(retried["status"], "replayed", "{transport:?}: {retried}");
        assert_eq!(main_at(&store), main);
        assert_eq!(remote.changed("main", "keyed.txt"), 1, "{transport:?}");
        store.assert_clean(&format!("{transport:?}"));
    }
}

// ---------------------------------------------------------------------------------------------
// Criterion 5: GitHub's limits.

/// A repository `big` on the server whose `main` holds one commit with a file of `size` zero
/// bytes at `data/huge.bin`; its directory.
fn with_large_file(remote: &Remote, size: u64) -> PathBuf {
    let bare = remote.server.create("big");
    let dir = tempfile::tempdir().expect("a temp dir");
    let file = dir.path().join("huge.bin");
    let zeros = fs::File::create(&file).expect("make the file");
    zeros.set_len(size).expect("size the file");

    // The blob, a tree that holds it under data/, a commit, and main.
    let git = |args: &[&str]| git_out(&remote.server, &bare, args);
    let blob = git(&["hash-object", "-w", &path_text(&file)]);
    let inner = mktree(remote, &bare, &format!("100644 blob {blob}\thuge.bin\n"));
    let root = mktree(remote, &bare, &format!("040000 tree {inner}\tdata\n"));
    let commit = git(&["commit-tree", &root, "-m", "large"]);
    git(&["update-ref", "refs/heads/main", &commit]);
    bare
}

/// `git mktree` of `entries` in `bare`: the tree's id.
fn mktree(remote: &Remote, bare: &Path, entries: &str) -> String {
    use std::io::Write as _;
    use std::process::Stdio;

    let mut child = remote
        .server
        .command()
        .arg("--git-dir")
        .arg(bare)
        .arg("mktree")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("run git mktree");
    let mut stdin = child.stdin.take().expect("its stdin");
    stdin
        .write_all(entries.as_bytes())
        .expect("write the entries");
    drop(stdin);
    let output = child.wait_with_output().expect("git mktree");
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout)
        .expect("UTF-8")
        .trim()
        .to_owned()
}

/// A store following the empty [`REPO`] whose `main` holds the large file of
/// [`with_large_file`], imported.
fn store_with_large_file(remote: &Remote, size: u64) -> Store {
    let big = with_large_file(remote, size);
    let store = remote.added();
    let line = [
        "git",
        "import",
        big.to_str().expect("UTF-8"),
        "--ref",
        "refs/heads/main",
    ];
    store.run_as(AGENT, &line, &[]).ok();
    store
}

/// A file over 100 MiB pushed to a host that counts as GitHub's is refused before anything is
/// sent: `E_TOO_LARGE` `remote_file` with its path; the remote stays empty.
#[test]
#[ignore = "slow, ~70 s: imports and exports a 100 MiB file per transport; just check-slow runs it"]
fn a_file_over_githubs_limit_is_refused_locally() {
    for transport in TRANSPORTS {
        let remote = Remote::empty(transport);
        let store = store_with_large_file(&remote, FILE_REFUSED + 1);
        let host = remote.host();

        let failed = run_with(&remote, &store, &["push", ORIGIN], &[(GITHUB_HOSTS, &host)]);
        let error = failed.err("E_TOO_LARGE");
        assert_eq!(error["details"]["scope"], "remote_file", "{failed}");
        assert_eq!(error["details"]["path"], "data/huge.bin", "{failed}");
        assert_eq!(error["details"]["size"], FILE_REFUSED + 1, "{failed}");
        assert_eq!(error["details"]["limit"], FILE_REFUSED, "{failed}");
        assert_eq!(remote.head("main"), None, "{transport:?}");
    }
}

/// A file over 50 MiB pushed to a host that counts as GitHub's is sent, with a warning naming
/// its path.
#[test]
#[ignore = "slow, ~40 s: imports, exports and pushes a 50 MiB file per transport; just check-slow runs it"]
fn limits_warn_from_50_mib() {
    for transport in TRANSPORTS {
        let remote = Remote::empty(transport);
        let store = store_with_large_file(&remote, FILE_WARNED + 1);
        let host = remote.host();

        let line = ["push", ORIGIN];
        let pushed = run_with(&remote, &store, &line, &[(GITHUB_HOSTS, &host)]).ok();
        assert_eq!(pushed["status"], "created", "{transport:?}: {pushed}");
        let warnings = pushed["warnings"].to_string();
        assert!(warnings.contains("data/huge.bin"), "{pushed}");
        assert!(remote.head("main").is_some(), "{transport:?}");
    }
}

/// The limits apply to GitHub's hosts only: a file over 50 MiB pushed to a host not named is
/// sent with no warning.
#[test]
#[ignore = "slow, ~40 s: imports, exports and pushes a 50 MiB file per transport; just check-slow runs it"]
fn the_limits_do_not_apply_to_other_hosts() {
    for transport in TRANSPORTS {
        let remote = Remote::empty(transport);
        let store = store_with_large_file(&remote, FILE_WARNED + 1);

        // Another host named: this one is not GitHub's.
        let other = [(GITHUB_HOSTS, "github.example:1")];
        let pushed = run_with(&remote, &store, &["push", ORIGIN], &other).ok();
        assert_eq!(pushed["status"], "created", "{transport:?}: {pushed}");
        assert!(pushed.get("warnings").is_none(), "{pushed}");
        assert!(remote.head("main").is_some(), "{transport:?}");
    }
}

// ---------------------------------------------------------------------------------------------
// Criterion 6: the network gone.

/// What a failed network command may not change: the store's files and the remote's refs.
struct Untouched {
    /// The store's files but its mirror, with their bytes.
    store: BTreeMap<String, Vec<u8>>,
    /// The remote's refs.
    remote: String,
}

impl Untouched {
    /// The state now.
    fn of(remote: &Remote, store: &Store) -> Untouched {
        Untouched {
            store: files_of(Path::new(store.path()), &["git"]),
            remote: remote.refs(),
        }
    }

    /// Asserts that nothing changed since; `what` says after what.
    fn check(&self, remote: &Remote, store: &Store, what: &str) {
        let now = Untouched::of(remote, store);
        assert!(now.store == self.store, "{what}: the store changed");
        assert_eq!(now.remote, self.remote, "{what}");
    }
}

/// `failed` is `E_REMOTE_UNREACHABLE`, retryable, exit 4; its details.
fn unreachable(failed: &Run) -> Value {
    let error = failed.err("E_REMOTE_UNREACHABLE");
    assert_eq!(failed.exit(), 4, "{failed}");
    assert_eq!(error["retryable"], true, "{failed}");
    error["details"].clone()
}

/// fetch, push and sync with the remote down (a stopped server, a cut connection, an ssh host
/// that refuses) each fail with `E_REMOTE_UNREACHABLE`, retryable, and change nothing.
#[test]
fn fetch_push_sync_fail_retryably_without_the_network() {
    for transport in TRANSPORTS {
        let mut remote = Remote::new(transport);
        let store = remote.cloned();
        land(&store, "local.txt", "local\n");

        // The export the push makes before it meets the network, made once: its external ids
        // are in the store already.
        push(&store, &["main:scratch"]).ok();
        remote.commit("main", "theirs", &[("theirs.txt", "theirs\n")]);
        let before = Untouched::of(&remote, &store);
        let lines: [&[&str]; 3] = [&["fetch", ORIGIN], &["push", ORIGIN], &["sync", ORIGIN]];

        // 1. A response cut short (HTTP only): an early end of file.
        if transport == Transport::Http {
            remote.http.cut_after(CUT_AT);
            let failed = run_with(&remote, &store, &["fetch", ORIGIN], &[]);
            let details = unreachable(&failed);
            assert_eq!(details["cause"], "disconnected", "{failed}");
            before.check(&remote, &store, "a cut fetch");
        }

        // 2. The host down: a stopped server, or the stand-in that cannot connect.
        let down = remote.server.ssh_command(SshMode::Down);
        remote.http.stop();
        for line in lines {
            let failed = run_with(&remote, &store, line, &[(SSH_VAR, &down)]);
            let details = unreachable(&failed);
            assert_eq!(details["remote"], ORIGIN, "{failed}");
            before.check(&remote, &store, &format!("{transport:?} {line:?}"));
        }
    }
}

/// With the remote down, every local command works: commits, lands, reads, `sync status` and
/// `remote list`.
#[test]
fn every_local_command_runs_with_the_remote_down() {
    for transport in TRANSPORTS {
        let mut remote = Remote::new(transport);
        let store = remote.cloned();
        let down = remote.server.ssh_command(SshMode::Down);
        remote.http.stop();
        let env = [(SSH_VAR, down.as_str())];

        // 1. Writes: a commit and a land.
        let head = main_at(&store).expect("main");
        let change = store.commit(AGENT, "offline.txt", "offline\n", Some(&head));
        let line = ["land", change.as_str(), "--key", "offline"];
        run_with(&remote, &store, &line, &env).ok();

        // 2. Reads, the remote's offline ones among them.
        let reads: [&[&str]; 7] = [
            &["log"],
            &["ref", "list"],
            &["show", "main"],
            &["diff", &head, "main"],
            &["remote", "list"],
            &["sync", "status", ORIGIN],
            &["verify", "--deep"],
        ];
        for line in reads {
            run_with(&remote, &store, line, &env).ok();
        }
        let status = run_with(&remote, &store, &["sync", "status", ORIGIN], &env).ok();
        assert_eq!(status["ahead"], 1, "{transport:?}: {status}");
    }
}

// ---------------------------------------------------------------------------------------------
// Beyond the roadmap's list.

/// `value` with the remote's `url` and `host`, which change from run to run, replaced by
/// `[url]` and `[host]`: what a golden envelope pins.
fn normalised(value: &Value, url: &str, host: &str) -> Value {
    match value {
        Value::String(text) => Value::String(text.replace(url, "[url]").replace(host, "[host]")),
        Value::Array(items) => {
            let items = items.iter().map(|item| normalised(item, url, host));
            Value::Array(items.collect())
        }
        Value::Object(map) => {
            let map = map
                .iter()
                .map(|(key, item)| (key.clone(), normalised(item, url, host)));
            Value::Object(map.collect())
        }
        other => other.clone(),
    }
}

/// The four remote error codes, raised through the binary over each transport, each pinned by
/// its envelope: code, exit, message, details, hint, retryable. git's stderr, which each git
/// and curl words its own way, is checked present and left out.
#[test]
fn remote_error_codes_golden() {
    let mut golden = BTreeMap::new();
    for transport in TRANSPORTS {
        let mut remote = Remote::new(transport);
        let store = remote.cloned();
        land(&store, "local.txt", "local\n");
        let (url, host) = (remote.url(), remote.host());
        let mut envelopes = Vec::new();
        let mut record = |run: Run| {
            let envelope: Value = serde_json::from_str(&run.stdout).expect("an envelope");
            let mut error = normalised(&envelope["error"], &url, &host);

            // git's own words, which differ between gits and curls: present, not pinned.
            if let Some(tail) = error["details"].get_mut("stderr_tail") {
                assert!(tail.is_string(), "{run}");
                *tail = json!("[stderr_tail]");
            }
            envelopes.push(error);
        };

        // 1. E_REMOTE_NOT_FOUND: a name the store does not have.
        record(store.read(&["fetch", "upstream"]));

        // 2. E_REMOTE_DIVERGED: the remote moved since the fetch.
        let moved = remote.commit("main", "elsewhere", &[("e.txt", "e\n")]);
        record(push(&store, &[]));

        // 3. E_REMOTE_REJECTED: a hook declines the push.
        remote.server.move_to(REPO, "main", &moved);
        store.read(&["fetch", ORIGIN]).ok();
        sync(&store, &["--no-push"]).ok();
        remote
            .server
            .decline(REPO, "main", "main is protected: open a pull request");
        record(push(&store, &[]));

        // 4. E_REMOTE_UNREACHABLE: the host down.
        let down = remote.server.ssh_command(SshMode::Down);
        remote.http.stop();
        record(run_with(
            &remote,
            &store,
            &["fetch", ORIGIN],
            &[(SSH_VAR, &down)],
        ));
        golden.insert(format!("{transport:?}"), envelopes);
    }
    insta::assert_json_snapshot!(golden);
}

/// A token from `--token-env` reaches the server and nothing else: not the envelopes, not
/// `remotes.toml`, not any file under the store, in its own form or in base64. Over HTTP only:
/// the ssh stand-in takes no token.
#[test]
fn credentials_never_reach_the_disk_or_the_output() {
    const TOKEN: &str = "ghp_secretTOKENvalue0123456789";
    const VAR: &str = "SO_TEST_TOKEN";

    let server = GitServer::new();
    server.create(REPO);
    let auth = HttpAuth::Basic {
        user: "x-access-token".to_owned(),
        token: TOKEN.to_owned(),
    };
    let mut remote = Remote::serving(server, Transport::Http);
    remote.http = remote.server.http(auth);
    remote.commit("main", "first", &[("a.txt", "a\n")]);
    let store = remote.store();
    let encoded = base64(format!("x-access-token:{TOKEN}").as_bytes());
    let env = [(VAR, TOKEN)];

    // 1. Fetch and push with the token; then a push with a wrong one fails as auth.
    let url = remote.url();
    let line = ["remote", "add", ORIGIN, url.as_str(), "--token-env", VAR];
    let mut runs = vec![run_with(&remote, &store, &line, &env)];
    runs.push(run_with(&remote, &store, &["fetch", ORIGIN], &env));
    let head = changeset_of_with(&remote, &store, &env);
    let line = ["ref", "set", "main", head.as_str(), "--create"];
    runs.push(run_with(&remote, &store, &line, &env));
    land(&store, "local.txt", "local\n");
    runs.push(run_with(&remote, &store, &["push", ORIGIN], &env));
    for done in &runs {
        assert_eq!(done.exit(), 0, "{done}");
    }
    land(&store, "more.txt", "more\n");
    let wrong = run_with(&remote, &store, &["push", ORIGIN], &[(VAR, "wrong")]);
    let error = wrong.err("E_REMOTE_REJECTED");
    assert_eq!(error["details"]["reason"], "auth", "{wrong}");
    runs.push(wrong);

    // 2. The server saw the token.
    let header = format!("Basic {encoded}");
    assert!(remote.http.authorizations().contains(&header));

    // 3. Nowhere else: the runs' output, every file of the store.
    for done in &runs {
        for text in [&done.stdout, &done.stderr] {
            assert!(!text.contains(TOKEN) && !text.contains(&encoded), "{done}");
        }
    }
    let files = files_of(Path::new(store.path()), &[]);
    for (name, bytes) in &files {
        for secret in [TOKEN, encoded.as_str()] {
            let found = bytes
                .windows(secret.len())
                .any(|window| window == secret.as_bytes());
            assert!(!found, "{name} holds the token");
        }
    }
    let toml = fs::read_to_string(Path::new(store.path()).join("remotes.toml")).expect("toml");
    assert!(toml.contains(VAR), "{toml}");
}

/// `bytes` in standard padded base64 (RFC 4648 §4), as HTTP's Basic scheme carries a
/// credential.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut text = String::new();
    for chunk in bytes.chunks(3) {
        // Three bytes as one 24-bit number, then four 6-bit digits, `=` for the missing ones.
        let word = chunk.iter().enumerate().fold(0u32, |word, (i, byte)| {
            word | u32::from(*byte) << (16 - 8 * i)
        });
        for digit in 0..4 {
            if digit > chunk.len() {
                text.push('=');
                continue;
            }
            let index = (word >> (18 - 6 * digit)) & 0x3f;
            text.push(char::from(ALPHABET[index as usize]));
        }
    }
    text
}

/// The changeset of the remote's head, as a fetch with `env` reports it.
fn changeset_of_with(remote: &Remote, store: &Store, env: &[(&str, &str)]) -> String {
    let fetched = run_with(remote, store, &["fetch", ORIGIN], env).ok();
    fetched["head"]["changeset"]
        .as_str()
        .expect("a head")
        .to_owned()
}

/// The URLs a remote may not have, each refused by `remote add` and by `clone` before anything
/// is written, with its reason.
#[test]
fn urls_that_are_refused() {
    let refused = [
        ("http://example.com/repo.git", "cleartext_http"),
        ("http://localhost:8080/repo.git", "cleartext_http"),
        ("file:///srv/repo.git", "url_scheme"),
        ("/srv/repo.git", "url_scheme"),
        ("ext::sh -c touch% /tmp/pwned", "url_scheme"),
        ("fd::17", "url_scheme"),
        ("ftp://example.com/repo.git", "url_scheme"),
        (
            "https://user:secret@example.com/repo.git",
            "credential_in_url",
        ),
        ("-oProxyCommand=evil", "url_option"),
        ("ssh://-oProxyCommand=evil/repo.git", "url_option"),
    ];
    for transport in TRANSPORTS {
        let remote = Remote::empty(transport);
        let store = remote.store();
        let toml = Path::new(store.path()).join("remotes.toml");
        let dir = tempfile::tempdir().expect("a temp dir");

        for (url, reason) in refused {
            // 1. remote add: nothing written.
            let line = ["remote", "add", ORIGIN, "--branch", "main", "--", url];
            let failed = store.read(&line);
            let error = failed.err("E_INVALID_ARG");
            assert_eq!(error["details"]["reason"], reason, "{url}: {failed}");
            assert!(!toml.exists(), "{url}");

            // 2. clone: no store made.
            let into = path_text(&dir.path().join("clone"));
            let line = ["--as", AGENT, "clone", "--into", &into, "--", url];
            let failed = run(&line, &remote.vars(&[]));
            let error = failed.err("E_INVALID_ARG");
            assert_eq!(error["details"]["reason"], reason, "{url}: {failed}");
            assert!(!Path::new(&into).exists(), "{url}");
        }
    }
}

/// A parent environment that points git at another repository (`GIT_DIR`, `GIT_WORK_TREE`,
/// `GIT_INDEX_FILE`, `GIT_OBJECT_DIRECTORY`) changes nothing: fetch, push and sync work on the
/// mirror, and the other repository is untouched.
#[test]
fn the_childs_environment_is_clean() {
    for transport in TRANSPORTS {
        let remote = Remote::new(transport);
        let store = remote.cloned();
        let other = remote.server.create("other");
        let before = files_of(&other, &[]);
        let other_text = path_text(&other);
        let objects = path_text(&other.join("objects"));
        let env = [
            ("GIT_DIR", other_text.as_str()),
            ("GIT_WORK_TREE", other_text.as_str()),
            ("GIT_INDEX_FILE", "/nonexistent/index"),
            ("GIT_OBJECT_DIRECTORY", objects.as_str()),
            ("GIT_TRACE", "1"),
        ];

        land(&store, "local.txt", "local\n");
        remote.commit("main", "theirs", &[("theirs.txt", "theirs\n")]);
        run_with(&remote, &store, &["fetch", ORIGIN], &env).ok();
        let synced = run_with(&remote, &store, &["sync", ORIGIN], &env).ok();
        assert_eq!(synced["status"], "pushed", "{transport:?}: {synced}");
        land(&store, "later.txt", "later\n");
        let pushed = run_with(&remote, &store, &["push", ORIGIN], &env).ok();
        assert_eq!(pushed["status"], "pushed", "{transport:?}: {pushed}");
        assert!(
            files_of(&other, &[]) == before,
            "{transport:?}: the other repository changed"
        );
    }
}

/// A shell script that, when git's arguments name `fetch`, marks `held` and waits until
/// `release` exists: a sync held in its fetch.
#[cfg(unix)]
fn holding_fetch(held: &Path, release: &Path) -> String {
    format!(
        "{}if [ $act = yes ]; then\n  : > '{}'\n  while [ ! -e '{}' ]; do sleep 0.05; done\nfi",
        names_verb("fetch"),
        path_text(held),
        path_text(release)
    )
}

/// Two syncs at once: the second meets the first's mirror lock and fails at once with `E_BUSY`
/// `git_mirror`, retryable; the first completes.
#[cfg(unix)]
#[test]
fn two_syncs_one_busy() {
    for transport in TRANSPORTS {
        let remote = Remote::new(transport);
        let store = remote.cloned();
        land(&store, "local.txt", "local\n");
        let dir = tempfile::tempdir().expect("a temp dir");
        let (held, release) = (dir.path().join("held"), dir.path().join("release"));
        let git = stand_in(&remote, dir.path(), &holding_fetch(&held, &release));

        std::thread::scope(|scope| {
            // 1. The first sync, held in its fetch.
            let first =
                scope.spawn(|| run_with(&remote, &store, &["sync", ORIGIN], &[(GIT_VAR, &git)]));
            while !held.exists() {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }

            // 2. The second: busy at once.
            let second = sync(&store, &[]);
            let error = second.err("E_BUSY");
            assert_eq!(error["details"]["lock"], "git_mirror", "{second}");
            assert_eq!(error["retryable"], true, "{second}");

            // 3. The first completes.
            fs::write(&release, "").expect("release the fetch");
            let first = first.join().expect("the first sync");
            assert_eq!(first.ok()["status"], "pushed", "{transport:?}: {first}");
        });
    }
}

/// A store whose mirror was deleted rebuilds it in the next fetch, imports nothing it holds
/// already, and pushes again.
#[test]
fn mirror_deleted_is_rebuilt_by_fetch() {
    for transport in TRANSPORTS {
        let remote = Remote::new(transport);
        let store = remote.cloned();
        fs::remove_dir_all(Path::new(store.path()).join("git")).expect("delete the mirror");

        let fetched = store.read(&["fetch", ORIGIN]).ok();
        assert_eq!(fetched["imported"], 0, "{transport:?}: {fetched}");
        assert_eq!(tracking(&remote, &store, "main"), remote.head("main"));
        land(&store, "local.txt", "local\n");
        let pushed = push(&store, &[]).ok();
        assert_eq!(pushed["status"], "pushed", "{transport:?}: {pushed}");
        store.assert_clean(&format!("{transport:?}"));
    }
}

/// A git lock file in the mirror older than ten minutes, a killed process's, is removed with a
/// warning; a young one is `E_BUSY` `git_mirror_file`, naming it relative to the mirror.
#[test]
fn stale_git_lock_in_the_mirror_is_removed() {
    use std::time::{Duration, SystemTime};

    for transport in TRANSPORTS {
        let remote = Remote::new(transport);
        let store = remote.cloned();
        let name = format!("refs/remotes/{ORIGIN}/main.lock");
        let lock = mirror(&store).join(&name);

        // 1. A young lock: busy, kept.
        fs::write(&lock, "").expect("write the lock");
        let failed = store.read(&["fetch", ORIGIN]);
        let error = failed.err("E_BUSY");
        assert_eq!(error["details"]["lock"], "git_mirror_file", "{failed}");
        assert_eq!(error["details"]["file"], name.as_str(), "{failed}");
        assert!(lock.exists(), "{transport:?}");

        // 2. Eleven minutes old: removed, said, and the fetch runs.
        let old = SystemTime::now() - Duration::from_mins(11);
        let file = fs::File::options()
            .write(true)
            .open(&lock)
            .expect("the lock");
        file.set_modified(old).expect("age the lock");
        drop(file);
        let fetched = store.read(&["fetch", ORIGIN]).ok();
        assert!(fetched["warnings"].to_string().contains(&name), "{fetched}");
        assert!(!lock.exists(), "{transport:?}");
    }
}

/// A clone cut off in its fetch, run again, resumes into the store it made: same URL, `main`
/// at the remote's head.
#[test]
fn clone_resumes() {
    for transport in TRANSPORTS {
        let remote = Remote::new(transport);
        let dir = tempfile::tempdir().expect("a temp dir");
        let into = path_text(&dir.path().join("store"));
        let url = remote.url();
        let line = [
            "--as",
            AGENT,
            "clone",
            url.as_str(),
            "--into",
            &into,
            "--branch",
            "main",
        ];

        // 1. Cut off: a response cut short, or ssh's host down.
        remote.http.cut_after(CUT_AT);
        let down = remote.server.ssh_command(SshMode::Down);
        let failed = run(&line, &remote.vars(&[(SSH_VAR, &down)]));
        unreachable(&failed);
        assert!(Path::new(&into).join("remotes.toml").exists(), "{failed}");

        // 2. Run again: it resumes.
        let resumed = run(&line, &remote.vars(&[])).ok();
        assert_eq!(resumed["imported"], 2, "{transport:?}: {resumed}");
        assert_eq!(
            resumed["fetch"]["head"]["sha"],
            remote.head("main").expect("main")
        );

        // 3. The store it resumed is sound.
        run(&["--repo", &into, "verify", "--deep"], &[]).ok();
    }
}

/// A remote branch moved to an unrelated commit is never overwritten, whatever the store holds:
/// a push before the fetch fails its lease, one after it is diverged, and a sync finds no common
/// base. The branch stays where it was.
#[test]
fn push_never_forces() {
    for transport in TRANSPORTS {
        let remote = Remote::new(transport);
        let store = remote.cloned();
        land(&store, "local.txt", "local\n");

        // Another writer replaces main with an unrelated root.
        let empty = remote.git(&["mktree"]);
        let root = remote.git(&["commit-tree", &empty, "-m", "unrelated root"]);
        remote.server.move_to(REPO, "main", &root);

        let failed = push(&store, &[]);
        let reason = &failed.err("E_REMOTE_DIVERGED")["details"]["reason"];
        assert_eq!(reason, "remote_moved", "{failed}");
        store.read(&["fetch", ORIGIN]).ok();
        let failed = push(&store, &[]);
        let reason = &failed.err("E_REMOTE_DIVERGED")["details"]["reason"];
        assert_eq!(reason, "diverged", "{failed}");
        let failed = sync(&store, &[]);
        let reason = &failed.err("E_REMOTE_DIVERGED")["details"]["reason"];
        assert_eq!(reason, "no_common_base", "{failed}");
        assert_eq!(remote.head("main"), Some(root), "{transport:?}");
    }
}

// ---------------------------------------------------------------------------------------------
// The crash sweep (§6.5).

#[cfg(feature = "test-hooks")]
mod crash {
    //! `so sync` crashed at each of its filesystem calls, then run again: the remote's
    //! first-parent chain holds each change once, `main` names the remote's head, and the store
    //! verifies clean. And crashed after its push, before the tracking ref moved (§6.5's last
    //! row), which no store call marks: a stand-in for git kills the sync when its push is done.

    use std::fs;
    use std::path::Path;

    use swarmops_ops::NOW_ENV;
    use swarmops_store::{CRASH_AT_ENV, FS_CALLS_ENV};

    use super::{
        AGENT, ORIGIN, REPO, Remote, TRANSPORTS, Transport, assert_synced, git_out, land,
        path_text, sync,
    };
    use crate::support::sweep::{NOW, reported_calls, sweep};
    use crate::support::{Store, run};

    /// The paths the swept sync publishes, and the one the remote gains meanwhile.
    const PATHS: [&str; 3] = ["ours/one.txt", "ours/two.txt", "theirs.txt"];

    /// The template: a clone with two lands, and a remote that moved since; the sync lands them
    /// onto its head and pushes.
    fn template(transport: Transport) -> (Remote, Store) {
        let remote = Remote::new(transport);
        let store = remote.cloned();
        land(&store, PATHS[0], "one\n");
        land(&store, PATHS[1], "two\n");
        remote.commit("main", "theirs", &[(PATHS[2], "theirs\n")]);
        (remote, store)
    }

    /// A copy of the template's remote as `repo-<k>` and of its store, following it: one run of
    /// the sweep.
    fn copy(remote: &Remote, store: &Store, k: u64) -> (String, Store) {
        let name = format!("{REPO}-{k}");
        swarmops_testkit::fs::copy_dir(&remote.server.bare(REPO), &remote.server.bare(&name));
        let copy = Store::copy_of(store);
        let toml = Path::new(copy.path()).join("remotes.toml");
        let text = fs::read_to_string(&toml).expect("remotes.toml");
        let text = text.replace(&remote.url(), &remote.url_of(&name));
        fs::write(&toml, text).expect("point the copy at its remote");
        (name, copy)
    }

    /// After the crash and the sync run again, on the copy `name`: each change once on its
    /// first-parent chain, `main` at its head, the store clean.
    fn check(remote: &Remote, name: &str, store: &Store, what: &str) {
        let bare = remote.server.bare(name);
        for path in PATHS {
            let listed = git_out(
                &remote.server,
                &bare,
                &["log", "--first-parent", "--format=%H", "main", "--", path],
            );
            assert_eq!(listed.lines().count(), 1, "{what}: {path}");
        }
        let fetched = store.read(&["fetch", ORIGIN]).ok();
        let head = fetched["head"]["changeset"].as_str().expect("a head");
        assert_eq!(super::main_at(store).as_deref(), Some(head), "{what}");
        store.assert_clean(what);
    }

    /// Which of the sync's calls a sweep crashes it at.
    #[derive(Clone, Copy, Debug)]
    enum Points {
        /// Every one.
        All,
        /// About this many, spread evenly from the first to the last.
        Spread(u64),
    }

    /// `so sync` crashed at every filesystem call it makes (about 460), over each transport,
    /// then run again.
    #[test]
    #[ignore = "slow, ~140 s: ~920 crashed syncs; just check-slow runs it"]
    fn sync_crash_sweep() {
        for transport in TRANSPORTS {
            sweep_sync(transport, Points::All);
        }
    }

    /// The sweep's default-gate share: 16 call points spread over the sync, the first and the
    /// last among them.
    #[test]
    fn sync_crash_sweep_sampled() {
        for transport in TRANSPORTS {
            sweep_sync(transport, Points::Spread(16));
        }
    }

    /// Crashes the template's sync at each of `points`, then syncs again.
    fn sweep_sync(transport: Transport, points: Points) {
        let (remote, template) = template(transport);
        let now = NOW.to_string();

        // 1. A dry run counts the sync's calls, on a copy of its own.
        let (name, dry) = copy(&remote, &template, 0);
        let calls = dry.tmp().join("fs-calls");
        let calls_text = path_text(&calls);
        let env = [(FS_CALLS_ENV, calls_text.as_str()), (NOW_ENV, now.as_str())];
        dry.run_as(AGENT, &["sync", ORIGIN], &env).ok();
        let n = reported_calls(&calls);
        check(&remote, &name, &dry, "the dry run");

        // 2. Each crash point, then the sync again.
        let step = match points {
            Points::All => 1,
            Points::Spread(count) => n.div_ceil(count).max(1),
        };
        let points: Vec<u64> = (1..=n).filter(|k| (k - 1) % step == 0 || *k == n).collect();
        let last = u64::try_from(points.len() - 1).expect("a count");
        let outcomes = sweep(0..=last, |i| {
            let k = points[usize::try_from(i).expect("an index")];
            let (name, store) = copy(&remote, &template, k);
            let at = k.to_string();
            let env = [(CRASH_AT_ENV, at.as_str()), (NOW_ENV, now.as_str())];
            let crashed = store.run_as(AGENT, &["sync", ORIGIN], &env);
            assert!(crashed.aborted(), "{transport:?} k={k}: {crashed}");
            let again = sync(&store, &[]);
            assert_eq!(again.exit(), 0, "{transport:?} k={k}: {again}");
            check(&remote, &name, &store, &format!("{transport:?} k={k}"));
        });
        assert_eq!(outcomes.len(), points.len());
    }

    /// Crashed after the push, before the tracking ref moved: the next sync finds the remote at
    /// the export of `main` and pushes nothing (§6.5's last row).
    #[test]
    fn sync_crashed_after_its_push_finds_nothing_to_publish() {
        use std::os::unix::fs::PermissionsExt as _;

        for transport in TRANSPORTS {
            let (remote, store) = template(transport);
            let dir = tempfile::tempdir().expect("a temp dir");

            // A git that kills the sync, its parent, once its push succeeded.
            let program = dir.path().join("git");
            let real = path_text(&super::real_git(&remote.server));
            let script = format!(
                "#!/bin/sh\n{}if [ $act = yes ]; then\n  '{real}' \"$@\" && kill -9 $PPID\n  \
                 exit 0\nfi\nexec '{real}' \"$@\"\n",
                super::names_verb("push")
            );
            fs::write(&program, script).expect("write the stand-in");
            let mut mode = fs::metadata(&program).expect("the stand-in").permissions();
            mode.set_mode(0o755);
            fs::set_permissions(&program, mode).expect("make it runnable");

            let git = path_text(&program);
            let line = ["--repo", store.path(), "--as", AGENT, "sync", ORIGIN];
            let killed = run(&line, &remote.vars(&[("SWARMOPS_GIT", &git)]));
            assert!(killed.status.code().is_none(), "{transport:?}: {killed}");

            let again = sync(&store, &[]).ok();
            assert_eq!(again["status"], "up_to_date", "{transport:?}: {again}");
            assert_synced(&remote, &store, &PATHS, &format!("{transport:?}"));
        }
    }
}
