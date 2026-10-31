//! `sync` and `sync status` through the binary (M13), offline, against the test kit's loopback
//! server and its merge methods. Written by the M13 plan's task C8b.
//!
//! Each test serves a bare repository with the test kit's [`GitServer`] (git's own smart HTTP on
//! `127.0.0.1`) and runs the binary with a git environment of its own: the server's git in
//! `SWARMOPS_GIT`, an empty `HOME`, no system or global configuration. The tests that make the
//! remote or `main` move in the middle of a sync, or hold it in its fetch, put a stand-in for git
//! in `SWARMOPS_GIT`: a shell script that does its part when the sync's git runs `fetch` or
//! `push`, then runs the real git. They are written for Unix shells, and gated so.
//!
//! ```text
//!   store ── so sync origin ── fetch ─ land onto the remote's head ─ main ─ push ── GitServer
//!                                          (the stand-in moves the remote or main here)
//! ```

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use swarmops_testkit::remote::{GitServer, HttpAuth, HttpServer};
use tempfile::TempDir;

use crate::support::{BIN, Run, Store, run, run_bin};

/// The agent the commands that need an identity run as.
const AGENT: &str = "agent/worker-17";

/// The repository every test syncs with.
const REPO: &str = "repo";

/// The remote's name in the store.
const ORIGIN: &str = "origin";

/// A served repository and the git environment the binary runs with.
struct Remote {
    /// The remote's side.
    server: GitServer,
    /// The server's smart HTTP.
    http: HttpServer,
    /// git's empty `HOME` for the product's children, removed when the remote is dropped.
    _home: TempDir,
    /// The variables every run sets.
    env: Vec<(String, String)>,
}

impl Remote {
    /// [`REPO`] served, with two commits on `main`.
    fn new() -> Remote {
        let server = GitServer::new();
        server.create(REPO);
        let http = server.http(HttpAuth::Anonymous);
        let home = tempfile::tempdir().expect("a temp HOME");
        let git = real_git(&server);
        let env = vec![
            ("SWARMOPS_GIT".to_owned(), path_text(&git)),
            ("HOME".to_owned(), path_text(home.path())),
            ("GIT_CONFIG_NOSYSTEM".to_owned(), "1".to_owned()),
            ("GIT_CONFIG_GLOBAL".to_owned(), "/dev/null".to_owned()),
        ];
        let remote = Remote {
            server,
            http,
            _home: home,
            env,
        };

        // Two commits, so that the remote has a history below its head.
        remote.commit("first", &[("a.txt", "a\n")]);
        remote.commit("second", &[("b.txt", "b\n")]);
        remote
    }

    /// The repository's HTTP URL.
    fn url(&self) -> String {
        self.http.url(REPO)
    }

    /// A commit on the remote's `main`, as another person pushing would; its id.
    fn commit(&self, message: &str, files: &[(&str, &str)]) -> String {
        self.server.commit_on(REPO, "main", message, files)
    }

    /// The commit the remote's `main` names.
    fn head(&self) -> String {
        self.server
            .rev_parse(REPO, "refs/heads/main")
            .expect("the remote's main")
    }

    /// The variables every run sets, then `extra`, which a later reading takes over.
    fn vars<'a>(&'a self, extra: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
        let own = self.env.iter().map(|(k, v)| (k.as_str(), v.as_str()));
        own.chain(extra.iter().copied()).collect()
    }

    /// A store that records the remote as [`ORIGIN`], following `main`, without asking it.
    fn added(&self) -> Store {
        let store = Store::init().with_env(&self.vars(&[]));
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

    /// The files of the remote's `main`, by path, each with its contents.
    fn files(&self) -> Vec<(String, String)> {
        let bare = self.server.bare(REPO);
        let listed = git_out(
            &self.server,
            &bare,
            &["ls-tree", "-r", "--name-only", "main"],
        );
        listed
            .lines()
            .map(|path| {
                let blob = format!("main:{path}");
                let contents = git_out(&self.server, &bare, &["cat-file", "-p", &blob]);
                (path.to_owned(), contents)
            })
            .collect()
    }

    /// The first parents of the remote's `main`, newest first.
    fn first_parents(&self) -> Vec<String> {
        let bare = self.server.bare(REPO);
        let listed = git_out(&self.server, &bare, &["rev-list", "--first-parent", "main"]);
        listed.lines().map(str::to_owned).collect()
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

/// `sync <args…>` on `store` as [`AGENT`].
fn sync(store: &Store, args: &[&str]) -> Run {
    let mut line = vec!["sync", ORIGIN];
    line.extend_from_slice(args);
    store.run_as(AGENT, &line, &[])
}

/// `sync <args…>` on `store` as [`AGENT`], with `remote`'s variables and then `env` set.
fn sync_with(remote: &Remote, store: &Store, args: &[&str], env: &[(&str, &str)]) -> Run {
    let mut line = vec!["--repo", store.path(), "--as", AGENT, "sync", ORIGIN];
    line.extend_from_slice(args);
    run(&line, &remote.vars(env))
}

/// The parents of changeset `id` in `store`.
fn parents(store: &Store, id: &str) -> Vec<String> {
    let shown = store.show(id);
    let listed = shown["parents"].as_array().expect("parents");
    listed
        .iter()
        .map(|parent| parent.as_str().expect("an id").to_owned())
        .collect()
}

/// The changeset of the remote's head, as a fetch of it reports.
fn changeset_of(store: &Store) -> String {
    let fetched = store.read(&["fetch", ORIGIN]).ok();
    fetched["head"]["changeset"]
        .as_str()
        .expect("a head")
        .to_owned()
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

/// The shell test that holds when git's arguments name the subcommand `verb` and the stand-in
/// has not acted yet (`mark` absent): then it marks that it acts.
#[cfg(unix)]
fn first(verb: &str, mark: &Path) -> String {
    format!(
        "act=no\nfor arg in \"$@\"; do [ \"$arg\" = {verb} ] && act=yes; done\n\
         if [ $act = yes ] && [ ! -e '{mark}' ]; then\n  : > '{mark}'\n",
        mark = path_text(mark)
    )
}

/// A stand-in that moves the remote's `main` to `to`, by force, just before the sync's first
/// push: another writer pushing between the sync's fetch and its push.
#[cfg(unix)]
fn moving_remote(remote: &Remote, dir: &Path, to: &str) -> String {
    let real = path_text(&real_git(&remote.server));
    let bare = path_text(&remote.server.bare(REPO));
    let script = format!(
        "{}  '{real}' --git-dir '{bare}' update-ref refs/heads/main {to} >/dev/null 2>&1\nfi",
        first("push", &dir.join("moved"))
    );
    stand_in(remote, dir, &script)
}

/// A stand-in that moves `store`'s `main` from `from` to `to` during the sync's first fetch:
/// another agent landing while the sync works.
#[cfg(unix)]
fn moving_main(remote: &Remote, store: &Store, dir: &Path, from: &str, to: &str) -> String {
    let script = format!(
        "{}  '{BIN}' --repo '{}' --as agent/other ref set main {to} --expect {from} \
         >/dev/null 2>&1\nfi",
        first("fetch", &dir.join("moved")),
        store.path()
    );
    stand_in(remote, dir, &script)
}

/// A commit on the remote's side branch `next`, cut at the remote's head, holding `path`: what
/// another writer pushes to `main` while a sync runs; its id.
fn side_commit(remote: &Remote, path: &str, contents: &str) -> String {
    let head = remote.head();
    remote.server.move_to(REPO, "next", &head);
    remote
        .server
        .commit_on(REPO, "next", "theirs, later", &[(path, contents)])
}

/// A sync with nothing on either side changes nothing: `up_to_date`, `main` and the remote as
/// they were, nothing landed or dropped.
#[test]
fn sync_up_to_date_changes_nothing() {
    let remote = Remote::new();
    let store = remote.cloned();
    let main = main_at(&store).expect("main");
    let head = remote.head();

    let synced = sync(&store, &[]).ok();
    assert_eq!(synced["status"], "up_to_date", "{synced}");
    assert_eq!(synced["attempts"], 1);
    assert_eq!(synced["published"], json!([]));
    assert_eq!(synced["dropped"], json!([]));
    assert_eq!(synced["main"], json!({"old": main, "new": main}));
    assert_eq!(synced["remote_head"], json!({"old": head, "new": head}));
    assert_eq!(main_at(&store), Some(main));
    assert_eq!(remote.head(), head);
}

/// When only the remote moved, `main` fast-forwards to the remote's head and nothing is sent.
#[test]
fn sync_fast_forwards_main_when_only_the_remote_moved() {
    let remote = Remote::new();
    let store = remote.cloned();
    let main = main_at(&store).expect("main");
    let theirs = remote.commit("theirs", &[("c.txt", "c\n")]);

    let synced = sync(&store, &[]).ok();
    assert_eq!(synced["status"], "fast_forwarded", "{synced}");
    assert_eq!(synced["published"], json!([]));
    assert_eq!(synced["remote_head"]["new"], theirs.as_str());
    assert_eq!(synced["main"]["old"], main.as_str());
    let now = main_at(&store).expect("main");
    assert_eq!(synced["main"]["new"], now.as_str());
    assert_eq!(now, changeset_of(&store));
    assert_eq!(remote.head(), theirs);
}

/// When only `main` moved, its changes are pushed as they are: nothing is landed again.
#[test]
fn sync_pushes_when_only_main_moved() {
    let remote = Remote::new();
    let store = remote.cloned();
    let old_head = remote.head();
    let mine = land(&store, "mine.txt", "mine\n");

    let synced = sync(&store, &[]).ok();
    assert_eq!(synced["status"], "pushed", "{synced}");
    let published = synced["published"].as_array().expect("published");
    assert_eq!(published.len(), 1, "{synced}");
    assert_eq!(published[0]["changeset"], mine.as_str());
    assert_eq!(published[0]["landed_as"], mine.as_str());
    assert_eq!(published[0]["sha"], remote.head().as_str());
    assert_eq!(main_at(&store), Some(mine));
    assert_eq!(synced["remote_head"]["old"], old_head.as_str());
    assert_eq!(synced["remote_head"]["new"], remote.head().as_str());
}

/// When both moved, `main`'s change lands onto the remote's head as a new changeset, `main`
/// moves there, and the push sends it: the remote's first parents hold the change once, above
/// the remote's own commit, and its tree holds both sides.
#[test]
fn sync_lands_onto_the_remote_head_and_pushes() {
    let remote = Remote::new();
    let store = remote.cloned();
    let mine = land(&store, "mine.txt", "mine\n");
    let theirs = remote.commit("theirs", &[("theirs.txt", "theirs\n")]);

    // 1. The sync.
    let synced = sync(&store, &[]).ok();
    assert_eq!(synced["status"], "pushed", "{synced}");
    assert_eq!(synced["attempts"], 1);
    let published = synced["published"].as_array().expect("published");
    assert_eq!(published.len(), 1, "{synced}");
    let landed = published[0]["landed_as"].as_str().expect("landed_as");
    assert_eq!(published[0]["changeset"], mine.as_str());
    assert_ne!(landed, mine);
    assert_eq!(published[0]["sha"], remote.head().as_str());

    // 2. main names the landed change, on the remote's head.
    assert_eq!(main_at(&store).as_deref(), Some(landed));
    assert_eq!(synced["main"]["new"], landed);
    let onto = parents(&store, landed);
    assert_eq!(onto.len(), 1);
    assert_eq!(remote.first_parents()[1], theirs);

    // 3. The remote holds both sides.
    let files = remote.files();
    assert!(
        files.contains(&("mine.txt".to_owned(), "mine".to_owned())),
        "{files:?}"
    );
    assert!(
        files.contains(&("theirs.txt".to_owned(), "theirs".to_owned())),
        "{files:?}"
    );
}

/// `--no-push` lands onto the remote's head and moves `main`, and sends nothing.
#[test]
fn sync_no_push_moves_main_only() {
    let remote = Remote::new();
    let store = remote.cloned();
    land(&store, "mine.txt", "mine\n");
    let theirs = remote.commit("theirs", &[("theirs.txt", "theirs\n")]);

    let synced = sync(&store, &["--no-push"]).ok();
    assert_eq!(synced["status"], "landed_locally", "{synced}");
    let published = synced["published"].as_array().expect("published");
    assert_eq!(published.len(), 1, "{synced}");
    let landed = published[0]["landed_as"].as_str().expect("landed_as");
    assert_eq!(main_at(&store).as_deref(), Some(landed));
    assert_eq!(remote.head(), theirs);
    assert_eq!(synced["remote_head"], json!({"old": theirs, "new": theirs}));
}

/// The remote moved by another writer between the sync's fetch and its push: the lease fails,
/// and the next attempt fetches, lands onto the new head and pushes. The remote holds the
/// change once.
#[cfg(unix)]
#[test]
fn sync_retries_when_the_remote_moves_between_fetch_and_push() {
    let remote = Remote::new();
    let store = remote.cloned();
    land(&store, "mine.txt", "mine\n");
    let later = side_commit(&remote, "later.txt", "later\n");
    let dir = tempfile::tempdir().expect("a temp dir");
    let git = moving_remote(&remote, dir.path(), &later);

    let synced = sync_with(&remote, &store, &[], &[("SWARMOPS_GIT", &git)]).ok();
    assert_eq!(synced["status"], "pushed", "{synced}");
    assert_eq!(synced["attempts"], 2, "{synced}");

    // The remote: the change once, on top of the other writer's commit.
    let chain = remote.first_parents();
    assert_eq!(chain[1], later, "{chain:?}");
    let files = remote.files();
    assert!(
        files.contains(&("mine.txt".to_owned(), "mine".to_owned())),
        "{files:?}"
    );
    assert!(
        files.contains(&("later.txt".to_owned(), "later".to_owned())),
        "{files:?}"
    );
    assert_eq!(chain.len(), 4, "{chain:?}");
}

/// `main` moved by another agent while the sync works: its compare-and-swap fails, and the next
/// attempt lands `main`'s new changes too, without a new fetch.
#[cfg(unix)]
#[test]
fn sync_retries_when_main_moves_under_it() {
    let remote = Remote::new();
    let store = remote.cloned();
    let mine = land(&store, "mine.txt", "mine\n");
    let other = store.commit(AGENT, "other.txt", "other\n", Some(&mine));
    remote.commit("theirs", &[("theirs.txt", "theirs\n")]);
    let dir = tempfile::tempdir().expect("a temp dir");
    let git = moving_main(&remote, &store, dir.path(), &mine, &other);

    let synced = sync_with(&remote, &store, &[], &[("SWARMOPS_GIT", &git)]).ok();
    assert_eq!(synced["status"], "pushed", "{synced}");
    assert_eq!(synced["attempts"], 2, "{synced}");
    let published = synced["published"].as_array().expect("published");
    let changes: Vec<&str> = published
        .iter()
        .map(|change| change["changeset"].as_str().expect("an id"))
        .collect();
    assert_eq!(changes, [mine.as_str(), other.as_str()], "{synced}");
    assert_eq!(synced["main"]["old"], mine.as_str());

    // The remote holds both.
    let files = remote.files();
    assert!(
        files.contains(&("mine.txt".to_owned(), "mine".to_owned())),
        "{files:?}"
    );
    assert!(
        files.contains(&("other.txt".to_owned(), "other".to_owned())),
        "{files:?}"
    );
}

/// Attempts used up: a remote that moved is `E_REMOTE_DIVERGED` `remote_moved` with
/// `main_moved: true` (main names the landed change), and a `main` that moved is
/// `E_CAS_RETRY_EXHAUSTED` with `main_moved: false` (main is the other agent's).
#[cfg(unix)]
#[test]
fn sync_exhausted_reports_main_moved() {
    // 1. The remote moved before the one push, main landed onto it.
    let remote = Remote::new();
    let store = remote.cloned();
    let mine = land(&store, "mine.txt", "mine\n");
    remote.commit("theirs", &[("theirs.txt", "theirs\n")]);
    let later = side_commit(&remote, "later.txt", "later\n");
    let dir = tempfile::tempdir().expect("a temp dir");
    let git = moving_remote(&remote, dir.path(), &later);
    let failed = sync_with(
        &remote,
        &store,
        &["--max-attempts", "1"],
        &[("SWARMOPS_GIT", &git)],
    );
    let error = failed.err("E_REMOTE_DIVERGED");
    assert_eq!(error["details"]["reason"], "remote_moved", "{failed}");
    assert_eq!(error["details"]["main_moved"], true, "{failed}");
    let main = main_at(&store).expect("main");
    assert_ne!(main, mine);
    assert_eq!(remote.head(), later);

    // 2. main moved before its one move.
    let remote = Remote::new();
    let store = remote.cloned();
    let mine = land(&store, "mine.txt", "mine\n");
    let other = store.commit(AGENT, "other.txt", "other\n", Some(&mine));
    let theirs = remote.commit("theirs", &[("theirs.txt", "theirs\n")]);
    let dir = tempfile::tempdir().expect("a temp dir");
    let git = moving_main(&remote, &store, dir.path(), &mine, &other);
    let failed = sync_with(
        &remote,
        &store,
        &["--max-attempts", "1"],
        &[("SWARMOPS_GIT", &git)],
    );
    let error = failed.err("E_CAS_RETRY_EXHAUSTED");
    assert_eq!(error["details"]["main_moved"], false, "{failed}");
    assert_eq!(main_at(&store), Some(other));
    assert_eq!(remote.head(), theirs);
}

/// The golden envelope of `E_CAS_RETRY_EXHAUSTED` (exit 4, retryable): a sync of one attempt
/// whose git stand-in moves `main` before the sync's own move. Its details name the remote, the
/// branch, the attempts and `main_moved`, and no id, so the envelope without its `meta` is the
/// same in every run.
#[cfg(unix)]
#[test]
fn cas_retry_exhausted_golden() {
    // 1. A change landed on main, another agent's change on it, and the remote moved.
    let remote = Remote::new();
    let store = remote.cloned();
    let mine = land(&store, "mine.txt", "mine\n");
    let other = store.commit(AGENT, "other.txt", "other\n", Some(&mine));
    remote.commit("theirs", &[("theirs.txt", "theirs\n")]);

    // 2. The sync's one attempt loses main to the stand-in.
    let dir = tempfile::tempdir().expect("a temp dir");
    let git = moving_main(&remote, &store, dir.path(), &mine, &other);
    let failed = sync_with(
        &remote,
        &store,
        &["--max-attempts", "1"],
        &[("SWARMOPS_GIT", &git)],
    );
    let error = failed.err("E_CAS_RETRY_EXHAUSTED");
    assert_eq!(
        (&error["exit"], &error["retryable"]),
        (&4.into(), &true.into())
    );

    // 3. The golden: the envelope without its `meta`.
    let mut envelope = failed.envelope();
    let meta = envelope.as_object_mut().expect("an object").remove("meta");
    assert!(meta.is_some(), "{failed}");
    insta::assert_json_snapshot!(envelope);
}

/// A push the remote refuses leaves `main` landed on the remote's head: the error says so, and
/// hints at a push to another branch for a pull request.
#[test]
fn sync_push_refused_leaves_main_landed() {
    let remote = Remote::new();
    let store = remote.cloned();
    let mine = land(&store, "mine.txt", "mine\n");
    let theirs = remote.commit("theirs", &[("theirs.txt", "theirs\n")]);
    remote
        .server
        .decline(REPO, "main", "main is protected: open a pull request");

    let failed = sync(&store, &[]);
    let error = failed.err("E_REMOTE_REJECTED");
    assert_eq!(error["details"]["reason"], "declined", "{failed}");
    assert_eq!(error["details"]["main_moved"], true, "{failed}");
    // The way back is a land: main moves only forward (the M15 design §4).
    assert_eq!(
        error["hint"],
        "main holds the landed changes: swarmops push origin main:<a branch of yours>, then open \
         a pull request; to take main back, land a change that reverts them",
        "{failed}"
    );

    // main names the landed change, on the remote's head; the remote did not move.
    let main = main_at(&store).expect("main");
    assert_ne!(main, mine);
    assert_eq!(remote.head(), theirs);
    assert_eq!(parents(&store, &main), [changeset_of(&store)]);
}

/// A change that conflicts with the remote's head is `E_CONFLICT`, naming the change and what it
/// landed onto; nothing moves and nothing is sent.
#[test]
fn sync_conflict_moves_nothing() {
    let remote = Remote::new();
    let store = remote.cloned();
    let mine = land(&store, "a.txt", "mine\n");
    let theirs = remote.commit("theirs", &[("a.txt", "theirs\n")]);

    let failed = sync(&store, &[]);
    let error = failed.err("E_CONFLICT");
    assert_eq!(error["details"]["change"], mine.as_str(), "{failed}");
    assert_eq!(error["details"]["onto"], changeset_of(&store).as_str());
    assert_eq!(main_at(&store), Some(mine));
    assert_eq!(remote.head(), theirs);
}

/// A sync moves `main`, so it needs an identity.
#[test]
fn sync_needs_an_identity() {
    let remote = Remote::new();
    let store = remote.cloned();
    land(&store, "mine.txt", "mine\n");
    let head = remote.head();

    let failed = store.read(&["sync", ORIGIN]);
    failed.err("E_IDENTITY_REQUIRED");
    assert_eq!(remote.head(), head);
}

/// The sync takes the mirror's lock once, at its start, and runs its fetch and push under it:
/// while a stand-in holds the sync in its fetch, a second handle on `git/lock` would block, and
/// the sync then completes without `E_BUSY`.
#[cfg(unix)]
#[test]
fn sync_holds_one_mirror_lock() {
    use std::fs::{File, TryLockError};
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    let remote = Remote::new();
    let store = remote.cloned();
    land(&store, "mine.txt", "mine\n");
    let dir = tempfile::tempdir().expect("a temp dir");
    let started = dir.path().join("started");
    let release = dir.path().join("release");
    let script = format!(
        "{}  while [ ! -e '{}' ]; do sleep 0.05; done\nfi",
        first("fetch", &started),
        path_text(&release)
    );
    let git = stand_in(&remote, dir.path(), &script);

    // 1. The sync, held in its fetch.
    let line = ["--repo", store.path(), "--as", AGENT, "sync", ORIGIN];
    let vars = remote.vars(&[("SWARMOPS_GIT", &git)]);
    let child = crate::support::swarmops(&line, &vars)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the sync");
    let deadline = Instant::now() + Duration::from_secs(60);
    while !started.exists() {
        assert!(Instant::now() < deadline, "the sync never fetched");
        std::thread::sleep(Duration::from_millis(20));
    }

    // 2. The mirror's lock is the sync's.
    let lock = File::open(Path::new(store.path()).join("git").join("lock")).expect("git/lock");
    let second = lock.try_lock();
    assert!(
        matches!(second, Err(TryLockError::WouldBlock)),
        "{second:?}"
    );
    drop(lock);

    // 3. Released, it completes, its push under the same lock.
    fs::write(&release, "").expect("release the fetch");
    let output = child.wait_with_output().expect("the sync ends");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let envelope: Value = serde_json::from_str(stdout.trim()).expect("one envelope");
    assert_eq!(envelope["ok"], true, "{envelope}");
    assert_eq!(envelope["result"]["status"], "pushed", "{envelope}");
}

/// `sync status` counts what is ahead and behind from what the last fetch saw, offline: the
/// server stopped, it still answers.
#[test]
fn sync_status_offline_counts_ahead_and_behind() {
    let mut remote = Remote::new();
    let store = remote.cloned();
    let base = main_at(&store).expect("main");
    let one = land(&store, "one.txt", "1\n");
    let two = land(&store, "two.txt", "2\n");
    for n in 0..3 {
        remote.commit(&format!("theirs {n}"), &[(&format!("t{n}.txt"), "t\n")]);
    }
    let head = remote.head();
    store.read(&["fetch", ORIGIN]).ok();
    remote.http.stop();

    let status = store.read(&["sync", "status", ORIGIN]).ok();
    assert_eq!(status["fetched"], true, "{status}");
    assert_eq!(status["branch"], "main");
    assert_eq!(status["remote_head"]["sha"], head.as_str());
    assert_eq!(status["base"], base.as_str());
    assert_eq!(status["ahead"], 2);
    assert_eq!(status["behind"], 3);
    assert_eq!(status["truncated"], false);
    let unpublished: Vec<&str> = (status["unpublished"]
        .as_array()
        .expect("unpublished")
        .iter())
    .map(|entry| entry["id"].as_str().expect("an id"))
    .collect();
    assert_eq!(unpublished, [two.as_str(), one.as_str()], "{status}");

    // A limit lists fewer, and says so.
    let short = store.read(&["sync", "status", ORIGIN, "-n", "1"]).ok();
    assert_eq!(
        short["unpublished"].as_array().expect("unpublished").len(),
        1
    );
    assert_eq!(short["truncated"], true, "{short}");
}

/// Before any fetch, `sync status` says so, and counts all of `main` as ahead.
#[test]
fn sync_status_before_any_fetch() {
    let remote = Remote::new();
    let store = remote.added();
    land(&store, "one.txt", "1\n");
    land(&store, "two.txt", "2\n");
    land(&store, "three.txt", "3\n");

    let status = store.read(&["sync", "status", ORIGIN]).ok();
    assert_eq!(status["fetched"], false, "{status}");
    assert!(status.get("remote_head").is_none(), "{status}");
    assert!(status.get("base").is_none(), "{status}");
    assert_eq!(status["ahead"], 3);
    assert_eq!(status["behind"], 0);
    assert_eq!(
        status["unpublished"].as_array().expect("unpublished").len(),
        3
    );
}

/// `sync` and `sync_status` as MCP tools, by the remote's name.
#[test]
fn mcp_sync_and_sync_status() {
    let remote = Remote::new();
    let store = remote.cloned();
    land(&store, "mine.txt", "mine\n");
    remote.commit("theirs", &[("theirs.txt", "theirs\n")]);
    let meta = json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {},
        "io.modelcontextprotocol/clientInfo": {"name": "swarmops-tests", "version": "0"},
    });

    // 1. Two calls, then stdin closes.
    let call = |id: u32, name: &str| {
        let params = json!({"name": name, "arguments": {"remote": ORIGIN}, "_meta": meta});
        json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": params})
    };
    let input = format!("{}\n{}\n", call(0, "sync"), call(1, "sync_status"));
    let served = run_bin(
        BIN,
        &["mcp", "--repo", store.path(), "--as", AGENT],
        &remote.vars(&[]),
        &input,
    );

    // 2. Their envelopes, in order.
    let replies: Vec<Value> = served
        .stdout
        .lines()
        .map(|line| serde_json::from_str(line).expect("a JSON-RPC reply"))
        .collect();
    assert_eq!(replies.len(), 2, "{served}");
    let synced = &replies[0]["result"]["structuredContent"];
    assert_eq!(synced["ok"], true, "{served}");
    assert_eq!(synced["result"]["status"], "pushed", "{served}");
    let status = &replies[1]["result"]["structuredContent"];
    assert_eq!(status["ok"], true, "{served}");
    assert_eq!(status["result"]["ahead"], 0, "{served}");
    assert_eq!(status["result"]["behind"], 0, "{served}");
    assert_eq!(
        status["result"]["remote_head"]["sha"],
        remote.head().as_str()
    );
}
