//! `push` through the binary (M13): a compare-and-swap that never forces, and GitHub's limits,
//! offline, against the test kit's loopback server. Written by the M13 plan's task C7.
//!
//! Each test serves bare repositories with the test kit's [`GitServer`] (git's own smart HTTP on
//! `127.0.0.1`) and runs the binary with a git environment of its own: the server's git in
//! `SWARMOPS_GIT`, an empty `HOME`, no system or global configuration. The limit tests name the
//! loopback server in `SWARMOPS_TEST_GITHUB_HOSTS`, which the test build honours, so that it
//! counts as GitHub's; the URL stays the server's. They import and export a file of 50 or
//! 100 MiB, so they are slow and run in `just check-slow`; the size arithmetic runs in the
//! default gate, in the git crate's `limits` tests.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use swarmops_testkit::remote::{GitServer, HttpAuth, HttpServer};
use tempfile::TempDir;

use crate::support::{BIN, Run, Store, run, run_bin};

/// The agent the commands that need an identity run as.
const AGENT: &str = "agent/worker-17";

/// The repository every test pushes to.
const REPO: &str = "repo";

/// The remote's name in the store.
const ORIGIN: &str = "origin";

/// The test hook that makes a host count as GitHub's.
const GITHUB_HOSTS: &str = "SWARMOPS_TEST_GITHUB_HOSTS";

/// GitHub's file limits, in bytes: 100 MiB refused, 50 MiB warned about.
const FILE_REFUSED: u64 = 100 * 1024 * 1024;
const FILE_WARNED: u64 = 50 * 1024 * 1024;

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
        let remote = Remote::empty();
        remote.commit("main", "first", &[("a.txt", "a\n")]);
        remote.commit("main", "second", &[("b.txt", "b\n")]);
        remote
    }

    /// [`REPO`] served, with no commit.
    fn empty() -> Remote {
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

    /// A commit on the remote's `branch`; its id.
    fn commit(&self, branch: &str, message: &str, files: &[(&str, &str)]) -> String {
        self.server.commit_on(REPO, branch, message, files)
    }

    /// The commit the remote's `branch` names, if it exists.
    fn head(&self, branch: &str) -> Option<String> {
        self.server.rev_parse(REPO, &format!("refs/heads/{branch}"))
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

/// `push <args…>` on `store`.
fn push(store: &Store, args: &[&str]) -> Run {
    let mut line = vec!["push", ORIGIN];
    line.extend_from_slice(args);
    store.read(&line)
}

/// `push <args…>` on `store`, with `remote`'s variables and then `env` set.
fn push_with(remote: &Remote, store: &Store, args: &[&str], env: &[(&str, &str)]) -> Run {
    let mut line = vec!["--repo", store.path(), "push", ORIGIN];
    line.extend_from_slice(args);
    run(&line, &remote.vars(env))
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

/// A stand-in for git that counts its runs in a file, then runs the real git: the value for
/// `SWARMOPS_GIT`, and the file.
#[cfg(unix)]
fn counting_git(remote: &Remote, dir: &Path) -> (String, PathBuf) {
    use std::os::unix::fs::PermissionsExt as _;

    let count = dir.join("git-runs");
    let program = dir.join("git");
    let real = real_git(&remote.server);
    let script = format!(
        "#!/bin/sh\necho run >> '{}'\nexec '{}' \"$@\"\n",
        path_text(&count),
        path_text(&real)
    );
    fs::write(&program, script).expect("write the stand-in");
    let mut mode = fs::metadata(&program).expect("the stand-in").permissions();
    mode.set_mode(0o755);
    fs::set_permissions(&program, mode).expect("make the stand-in runnable");
    (path_text(&program), count)
}

/// How many times the stand-in of [`counting_git`] ran.
#[cfg(unix)]
fn runs(count: &Path) -> usize {
    fs::read_to_string(count).map_or(0, |text| text.lines().count())
}

/// A push to a remote branch that does not exist creates it at the export of `main`'s head:
/// `status: "created"`, no `old`, and the mirror's tracking ref names it too.
#[test]
fn push_creates_a_branch() {
    let remote = Remote::empty();
    let store = remote.added();
    land(&store, "a.txt", "a\n");
    let head = land(&store, "b.txt", "b\n");

    let pushed = push(&store, &[]).ok();
    let new = remote.head("main").expect("main made");
    assert_eq!(pushed["status"], "created", "{pushed}");
    assert_eq!(pushed["new"], new.as_str());
    assert_eq!(pushed["head_changeset"], head.as_str());
    assert_eq!(pushed["branch"], "main");
    assert_eq!(pushed["rewritten"], 0);
    assert!(pushed.get("old").is_none(), "{pushed}");
    assert_eq!(tracking(&remote, &store, "main"), Some(new));
}

/// A push of `main` built on what the last fetch saw moves the remote branch forward: the new
/// commit's parent is the old head.
#[test]
fn push_fast_forwards() {
    let remote = Remote::new();
    let old = remote.head("main").expect("main");
    let store = remote.cloned();
    land(&store, "local.txt", "local\n");

    let pushed = push(&store, &[]).ok();
    let new = remote.head("main").expect("main");
    assert_eq!(pushed["status"], "pushed", "{pushed}");
    assert_eq!(pushed["old"], old.as_str());
    assert_eq!(pushed["new"], new.as_str());
    let parent = git_out(
        &remote.server,
        &remote.server.bare(REPO),
        &["rev-parse", &format!("{new}^")],
    );
    assert_eq!(parent, old);
    assert_eq!(tracking(&remote, &store, "main"), Some(new));
}

/// The same push run again sends nothing and starts no git at all: `status: "up_to_date"`.
#[cfg(unix)]
#[test]
fn push_run_again_starts_no_git() {
    let remote = Remote::new();
    let store = remote.cloned();
    land(&store, "local.txt", "local\n");
    let first = push(&store, &[]).ok();

    // The stand-in counts every git the second push starts.
    let dir = tempfile::tempdir().expect("a temp dir");
    let (git, count) = counting_git(&remote, dir.path());
    let again = push_with(&remote, &store, &[], &[("SWARMOPS_GIT", &git)]).ok();
    assert_eq!(again["status"], "up_to_date", "{again}");
    assert_eq!(again["new"], first["new"]);
    assert_eq!(runs(&count), 0);
}

/// A remote branch that moved since the last fetch fails the push's lease:
/// `E_REMOTE_DIVERGED` `remote_moved` (exit 3), and nothing moved: the remote, the tracking ref.
#[test]
fn push_to_a_moved_remote_is_remote_moved() {
    let remote = Remote::new();
    let store = remote.cloned();
    let seen = remote.head("main").expect("main");
    land(&store, "local.txt", "local\n");
    let moved = remote.commit("main", "elsewhere", &[("e.txt", "e\n")]);

    let failed = push(&store, &[]);
    let error = failed.err("E_REMOTE_DIVERGED");
    assert_eq!(failed.exit(), 3, "{failed}");
    assert_eq!(error["details"]["reason"], "remote_moved", "{failed}");
    assert_eq!(remote.head("main"), Some(moved));
    assert_eq!(tracking(&remote, &store, "main"), Some(seen));
}

/// A store ref behind what the last fetch saw is `E_REMOTE_DIVERGED` `behind`, with the hint
/// to sync, and the remote does not move.
#[test]
fn push_of_a_ref_behind_the_remote_is_behind() {
    let remote = Remote::new();
    let store = remote.cloned();
    let ahead = remote.commit("main", "third", &[("c.txt", "c\n")]);
    store.read(&["fetch", ORIGIN]).ok();

    let failed = push(&store, &[]);
    let error = failed.err("E_REMOTE_DIVERGED");
    assert_eq!(error["details"]["reason"], "behind", "{failed}");
    let next = error["hint"].as_str().unwrap_or_default();
    assert!(next.contains("sync"), "{failed}");
    assert_eq!(remote.head("main"), Some(ahead));
}

/// A store ref and a remote branch that each moved past their common commit are
/// `E_REMOTE_DIVERGED` `diverged`, and the remote does not move.
#[test]
fn push_of_diverged_history_is_diverged() {
    let remote = Remote::new();
    let store = remote.cloned();
    land(&store, "local.txt", "local\n");
    let theirs = remote.commit("main", "theirs", &[("t.txt", "t\n")]);
    store.read(&["fetch", ORIGIN]).ok();

    let failed = push(&store, &[]);
    let error = failed.err("E_REMOTE_DIVERGED");
    assert_eq!(error["details"]["reason"], "diverged", "{failed}");
    assert_eq!(remote.head("main"), Some(theirs));
}

/// A remote branch that holds an unrelated history is never overwritten, whatever the store
/// holds: `E_REMOTE_DIVERGED`, the branch as it was.
#[test]
fn push_never_overwrites_an_unrelated_remote_branch() {
    let remote = Remote::new();
    let unrelated = remote.commit("other", "unrelated root", &[("u.txt", "u\n")]);
    let store = remote.cloned();
    land(&store, "local.txt", "local\n");

    let failed = push(&store, &["main:other"]);
    let error = failed.err("E_REMOTE_DIVERGED");
    assert_eq!(error["details"]["reason"], "diverged", "{failed}");
    assert_eq!(remote.head("other"), Some(unrelated.clone()));
    assert_eq!(tracking(&remote, &store, "other"), Some(unrelated));
}

/// A push the remote declines leaves the tracking ref where the fetch put it; the same push
/// accepted moves it to the commit sent.
#[test]
fn push_moves_the_tracking_ref_after_success_only() {
    let remote = Remote::new();
    let store = remote.cloned();
    let seen = remote.head("main").expect("main");
    land(&store, "local.txt", "local\n");

    // 1. Declined: the tracking ref stays.
    remote.server.decline(REPO, "main", "closed for now");
    push(&store, &[]).err("E_REMOTE_REJECTED");
    assert_eq!(tracking(&remote, &store, "main"), Some(seen));

    // 2. Accepted once the hook is gone: the tracking ref is the commit sent.
    let hook = remote.server.bare(REPO).join("hooks").join("pre-receive");
    fs::remove_file(hook).expect("remove the hook");
    let pushed = push(&store, &[]).ok();
    let new = remote.head("main").expect("main");
    assert_eq!(pushed["new"], new.as_str());
    assert_eq!(tracking(&remote, &store, "main"), Some(new));
}

/// A push a hook declines is `E_REMOTE_REJECTED` `declined` (exit 5), carrying the remote's
/// message and the refused ref.
#[test]
fn push_declined_by_a_hook_is_rejected_with_the_message() {
    let remote = Remote::new();
    let store = remote.cloned();
    let seen = remote.head("main");
    land(&store, "local.txt", "local\n");
    remote
        .server
        .decline(REPO, "main", "main is protected: open a pull request");

    let failed = push(&store, &[]);
    let error = failed.err("E_REMOTE_REJECTED");
    assert_eq!(failed.exit(), 5, "{failed}");
    assert_eq!(error["details"]["reason"], "declined", "{failed}");
    let said = error["details"]["remote_messages"].to_string();
    assert!(said.contains("main is protected"), "{failed}");
    assert_eq!(remote.head("main"), seen);
}

/// `push origin release/<x>` sends the store's release ref to the remote branch of the same
/// name, which it creates.
#[test]
fn push_of_release_x() {
    let remote = Remote::new();
    let store = remote.cloned();
    let head = main_at(&store).expect("main");
    let line = ["ref", "set", "release/1.0", head.as_str(), "--create"];
    store.run_as(AGENT, &line, &[]).ok();

    let pushed = push(&store, &["release/1.0"]).ok();
    assert_eq!(pushed["status"], "created", "{pushed}");
    assert_eq!(pushed["branch"], "release/1.0");
    assert_eq!(remote.head("release/1.0"), remote.head("main"));
}

/// A remote branch name git takes for no ref is `E_INVALID_ARG` `branch_name`, and no git runs.
#[cfg(unix)]
#[test]
fn push_bad_branch_name_starts_no_git() {
    let remote = Remote::new();
    let store = remote.cloned();
    let dir = tempfile::tempdir().expect("a temp dir");
    let (git, count) = counting_git(&remote, dir.path());

    let failed = push_with(&remote, &store, &["main:a..b"], &[("SWARMOPS_GIT", &git)]);
    let error = failed.err("E_INVALID_ARG");
    assert_eq!(error["details"]["reason"], "branch_name", "{failed}");
    assert_eq!(runs(&count), 0);
}

/// A store ref that is no ref name, and one that does not exist, are refused before anything
/// else: `E_REF_NAME_INVALID` and `E_REV_NOT_FOUND`.
#[test]
fn push_of_a_bad_or_missing_ref_is_refused() {
    let remote = Remote::new();
    let store = remote.cloned();
    push(&store, &["topic"]).err("E_REF_NAME_INVALID");
    push(&store, &["release/none"]).err("E_REV_NOT_FOUND");
}

/// The remote moved to `E`, the very commit the push would send, by another writer before the
/// push: `status: "up_to_date"`, and the tracking ref moves to `E`.
#[test]
fn push_of_what_the_remote_already_holds_is_up_to_date() {
    let remote = Remote::new();
    let store = remote.cloned();
    land(&store, "local.txt", "local\n");

    // 1. Another writer sends the same export to the remote, by plain git.
    let dir = tempfile::tempdir().expect("a temp dir");
    let other = dir.path().join("other.git");
    git_out(
        &remote.server,
        &other,
        &["init", "--quiet", "--bare", path_text(&other).as_str()],
    );
    let exported = store
        .read(&["git", "export", &path_text(&other), "--branch", "main"])
        .ok();
    let e = exported["head_sha"].as_str().expect("a commit").to_owned();
    let bare = path_text(&remote.server.bare(REPO));
    git_out(
        &remote.server,
        &other,
        &["push", "--quiet", &bare, "main:main"],
    );
    assert_eq!(remote.head("main").as_deref(), Some(e.as_str()));

    // 2. The push finds it there.
    let pushed = push(&store, &[]).ok();
    assert_eq!(pushed["status"], "up_to_date", "{pushed}");
    assert_eq!(pushed["new"], e.as_str());
    assert_eq!(tracking(&remote, &store, "main"), Some(e));
}

/// A fresh lock file on the push's staging branch may belong to a git child still writing: the
/// push is `E_BUSY` `git_mirror_file`, retryable, naming the file in the mirror, and its hint
/// does not tell anyone to remove it.
#[test]
fn young_staging_lock_is_busy_not_a_removal_hint() {
    let remote = Remote::new();
    let store = remote.cloned();
    land(&store, "local.txt", "local\n");
    let staging = mirror(&store).join("refs/heads/push").join(ORIGIN);
    fs::create_dir_all(&staging).expect("make the staging directory");
    fs::write(staging.join("main.lock"), b"").expect("write a lock file");

    let failed = push(&store, &[]);
    let error = failed.err("E_BUSY");
    assert_eq!(error["details"]["lock"], "git_mirror_file", "{failed}");
    assert_eq!(
        error["details"]["file"], "refs/heads/push/origin/main.lock",
        "{failed}"
    );
    assert_eq!(error["retryable"], true, "{failed}");
    let next = error["hint"].as_str().unwrap_or_default();
    assert!(!next.contains("remove"), "{failed}");
}

/// The MCP server's `push` tool gives what the CLI gives.
#[test]
fn mcp_push() {
    let remote = Remote::new();
    let store = remote.cloned();
    land(&store, "local.txt", "local\n");
    let meta = json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {},
        "io.modelcontextprotocol/clientInfo": {"name": "swarmops-tests", "version": "0"},
    });

    // 1. One call, then stdin closes.
    let params = json!({"name": "push", "arguments": {"remote": ORIGIN}, "_meta": meta});
    let request = json!({"jsonrpc": "2.0", "id": 0, "method": "tools/call", "params": params});
    let served = run_bin(
        BIN,
        &["mcp", "--repo", store.path()],
        &remote.vars(&[]),
        &format!("{request}\n"),
    );

    // 2. Its envelope.
    let reply: Value = serde_json::from_str(served.stdout.trim()).expect("one JSON-RPC reply");
    let envelope = &reply["result"]["structuredContent"];
    assert_eq!(envelope["ok"], true, "{served}");
    assert_eq!(envelope["result"]["status"], "pushed", "{served}");
    assert_eq!(
        envelope["result"]["new"],
        remote.head("main").expect("main").as_str()
    );
}

/// A repository on the server whose `main` holds one commit with a file of `size` zero bytes at
/// `data/huge.bin`; its directory.
fn with_large_file(remote: &Remote, size: u64) -> PathBuf {
    let bare = remote.server.create("big");
    let dir = tempfile::tempdir().expect("a temp dir");
    let file = dir.path().join("huge.bin");
    let zeros = fs::File::create(&file).expect("make the file");
    zeros.set_len(size).expect("size the file");

    // The blob, a tree that holds it under data/, a commit, and main.
    let blob = git_out(
        &remote.server,
        &bare,
        &["hash-object", "-w", &path_text(&file)],
    );
    let inner = mktree(remote, &bare, &format!("100644 blob {blob}\thuge.bin\n"));
    let root = mktree(remote, &bare, &format!("040000 tree {inner}\tdata\n"));
    let commit = git_out(
        &remote.server,
        &bare,
        &["commit-tree", &root, "-m", "large"],
    );
    git_out(
        &remote.server,
        &bare,
        &["update-ref", "refs/heads/main", &commit],
    );
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

/// A store following an empty remote whose `main` holds the large file of [`with_large_file`].
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

/// A file over 100 MiB, pushed to a host that counts as GitHub's, is refused before anything is
/// sent: `E_TOO_LARGE` `remote_file`, naming its path, its size and the limit; the remote stays
/// empty.
#[test]
#[ignore = "slow, ~35 s: imports and exports a 100 MiB file; just check-slow runs it"]
fn limits_refuse_a_large_blob_with_its_path() {
    let remote = Remote::empty();
    let store = store_with_large_file(&remote, FILE_REFUSED + 1);
    let host = remote.http.host();

    let failed = push_with(&remote, &store, &[], &[(GITHUB_HOSTS, &host)]);
    let error = failed.err("E_TOO_LARGE");
    assert_eq!(error["details"]["scope"], "remote_file", "{failed}");
    assert_eq!(error["details"]["path"], "data/huge.bin", "{failed}");
    assert_eq!(error["details"]["size"], FILE_REFUSED + 1, "{failed}");
    assert_eq!(error["details"]["limit"], FILE_REFUSED, "{failed}");
    assert_eq!(error["details"]["remote"], ORIGIN, "{failed}");
    assert_eq!(remote.head("main"), None);
}

/// A file over 50 MiB pushed to a host that counts as GitHub's is sent, with a warning naming
/// its path.
#[test]
#[ignore = "slow, ~20 s: imports, exports and pushes a 50 MiB file; just check-slow runs it"]
fn limits_warn_from_50_mib() {
    let remote = Remote::empty();
    let store = store_with_large_file(&remote, FILE_WARNED + 1);
    let host = remote.http.host();

    let pushed = push_with(&remote, &store, &[], &[(GITHUB_HOSTS, &host)]).ok();
    assert_eq!(pushed["status"], "created", "{pushed}");
    let warnings = pushed["warnings"].to_string();
    assert!(warnings.contains("data/huge.bin"), "{pushed}");
    assert!(remote.head("main").is_some());
}

/// The limits apply to GitHub's hosts only: the same file over 100 MiB is refused when the
/// loopback server counts as GitHub's, and sent when it does not.
#[test]
#[ignore = "slow, ~50 s: imports, exports and pushes a 100 MiB file; just check-slow runs it"]
fn limits_apply_to_github_hosts_only() {
    let remote = Remote::empty();
    let store = store_with_large_file(&remote, FILE_REFUSED + 1);
    let host = remote.http.host();

    // 1. Named in the test hook: GitHub's, refused.
    push_with(&remote, &store, &[], &[(GITHUB_HOSTS, &host)]).err("E_TOO_LARGE");
    assert_eq!(remote.head("main"), None);

    // 2. Not named: another host, sent.
    let pushed = push(&store, &[]).ok();
    assert_eq!(pushed["status"], "created", "{pushed}");
    assert!(pushed.get("warnings").is_none(), "{pushed}");
    assert!(remote.head("main").is_some());
}
