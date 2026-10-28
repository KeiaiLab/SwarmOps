//! `remote add`, `remote remove`, `remote list`, `fetch` and `clone` through the binary (M13),
//! offline, against the test kit's loopback server. Written by the M13 plan's task C6.
//!
//! Each test serves bare repositories with the test kit's [`GitServer`] (git's own smart HTTP on
//! `127.0.0.1`, or its ssh stand-in) and runs the binary with a git environment of its own: the
//! server's git in `SWARMOPS_GIT`, an empty `HOME`, no system or global configuration, so that
//! nothing of the host's git setup reaches the product's git children.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde_json::{Value, json};
use swarmops_testkit::remote::{GitServer, HttpAuth, HttpServer, SshMode};
use tempfile::TempDir;

use crate::support::{BIN, Run, Store, run, run_bin, swarmops};

/// The agent the commands that need an identity run as.
const AGENT: &str = "agent/worker-17";

/// The repository every test serves.
const REPO: &str = "repo";

/// The remote's name in the store.
const ORIGIN: &str = "origin";

/// The store's file of remotes.
const REMOTES_TOML: &str = "remotes.toml";

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

    /// The commit the remote's `branch` names.
    fn head(&self, branch: &str) -> String {
        let rev = format!("refs/heads/{branch}");
        self.server
            .rev_parse(REPO, &rev)
            .expect("the branch exists")
    }

    /// Runs plain git with `args` on the remote's bare repository; what it prints.
    fn git(&self, args: &[&str]) -> String {
        git_out(&self.server, &self.server.bare(REPO), args)
    }

    /// The variables every run sets, plus `extra`.
    fn vars<'a>(&'a self, extra: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
        let own = self.env.iter().map(|(k, v)| (k.as_str(), v.as_str()));
        own.chain(extra.iter().copied()).collect()
    }

    /// A fresh store whose commands run with this remote's git environment.
    fn store(&self) -> Store {
        Store::init().with_env(&self.vars(&[]))
    }

    /// Runs `args` with this remote's git environment and `extra`.
    fn run(&self, args: &[&str], extra: &[(&str, &str)]) -> Run {
        run(args, &self.vars(extra))
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

/// The refs under `prefix` in `store`'s mirror, one `<name> <oid>` line each.
fn mirror_refs(remote: &Remote, store: &str, prefix: &str) -> Vec<String> {
    let mirror = Path::new(store).join("git").join("mirror.git");
    let format = "--format=%(refname) %(objectname)";
    let listed = git_out(&remote.server, &mirror, &["for-each-ref", format, prefix]);
    listed.lines().map(str::to_owned).collect()
}

/// `remote add <name> <url> <args…>` on `store`.
fn add(store: &Store, name: &str, url: &str, args: &[&str]) -> Run {
    let mut line = vec!["remote", "add", name, url];
    line.extend_from_slice(args);
    store.read(&line)
}

/// `fetch <name>` on `store`, which must succeed: its result.
fn fetch(store: &Store, name: &str) -> Value {
    store.read(&["fetch", name]).ok()
}

/// The store's refs, as `ref list` gives them.
fn refs(store: &Store) -> Value {
    store.read(&["ref", "list"]).ok()["refs"].take()
}

/// How many changesets the store holds.
fn changesets(store: &Store) -> u64 {
    store.info()["changesets"].as_u64().expect("a count")
}

/// The text of `store`'s `remotes.toml`.
fn remotes_toml(store: &Store) -> String {
    fs::read_to_string(Path::new(store.path()).join(REMOTES_TOML)).expect("remotes.toml")
}

/// `remote add` without `--branch` reads the remote's default branch (`trunk` here, its `HEAD`)
/// and writes `remotes.toml`: version 1, the name, the URL as given and the branch; `remote list`
/// gives the same remote back, with no warning.
#[test]
fn remote_add_writes_the_file_and_reads_the_default_branch() {
    let remote = Remote::empty();
    remote.commit("trunk", "first", &[("a.txt", "a\n")]);
    remote.git(&["symbolic-ref", "HEAD", "refs/heads/trunk"]);
    let store = remote.store();
    let url = remote.url();

    // 1. Added: the branch read from the remote, no warning.
    let added = add(&store, ORIGIN, &url, &[]).ok();
    assert_eq!(
        added,
        json!({"name": ORIGIN, "url": url, "branch": "trunk", "lossy": []})
    );

    // 2. The file holds the remote.
    let text = remotes_toml(&store);
    assert!(text.contains("version = 1"), "{text}");
    for line in [
        "name = \"origin\"".to_owned(),
        format!("url = \"{url}\""),
        "branch = \"trunk\"".to_owned(),
    ] {
        assert!(text.contains(&line), "{line} in {text}");
    }

    // 3. The list gives it back.
    let listed = store.read(&["remote", "list"]).ok();
    assert_eq!(listed, json!({"remotes": [added]}));
}

/// A second `remote add` of a name the store has is `E_EXISTS`, and leaves the file as it was.
#[test]
fn remote_add_refuses_a_name_twice() {
    let remote = Remote::new();
    let store = remote.store();
    add(&store, ORIGIN, &remote.url(), &["--branch", "main"]).ok();
    let before = remotes_toml(&store);

    let again = add(&store, ORIGIN, &remote.url(), &["--branch", "other"]);
    let error = again.err("E_EXISTS");
    assert_eq!(error["details"]["remote"], ORIGIN, "{again}");
    assert_eq!(remotes_toml(&store), before);
}

/// Each URL the rules refuse is `E_INVALID_ARG` with its reason, and so is each name the rules
/// refuse (`status` among them: `sync status` is a subcommand); nothing is written.
#[test]
fn remote_add_refuses_each_url_of_3_4() {
    let remote = Remote::new();
    let store = remote.store();
    let urls = [
        ("http://example.com/acme/acme", "cleartext_http"),
        ("http://localhost/acme/acme", "cleartext_http"),
        ("file:///srv/acme.git", "url_scheme"),
        ("/srv/acme.git", "url_scheme"),
        ("ext::sh -c touch% /tmp/x", "url_scheme"),
        ("fd::17", "url_scheme"),
        (
            "https://user:secret@github.com/acme/acme",
            "credential_in_url",
        ),
        ("ssh://-oProxyCommand=touch/acme", "url_option"),
    ];
    for (url, reason) in urls {
        let refused = add(&store, ORIGIN, url, &["--branch", "main"]);
        let error = refused.err("E_INVALID_ARG");
        assert_eq!(error["details"]["reason"], reason, "{url}: {refused}");
        assert!(!refused.stdout.contains("secret"), "{refused}");
    }

    // The names: the grammar, `status`, and the portable-name check.
    for name in ["status", "Origin", "a.", ".a", "con", "a b", "a/b", ""] {
        let refused = add(&store, name, &remote.url(), &["--branch", "main"]);
        let error = refused.err("E_INVALID_ARG");
        assert_eq!(error["details"]["reason"], "name", "{name:?}: {refused}");
    }
    assert!(!Path::new(store.path()).join(REMOTES_TOML).exists());
}

/// `remote remove` drops the remote's line and its mirror refs (tracking refs and tags), and
/// writes no changeset and moves no store ref; another remote's refs stay. A name the store
/// does not have is `E_REMOTE_NOT_FOUND`.
#[test]
fn remote_remove_drops_its_mirror_refs_and_no_changeset() {
    let remote = Remote::new();
    remote.git(&["tag", "v1", "main"]);
    let store = remote.store();
    add(&store, ORIGIN, &remote.url(), &[]).ok();
    add(&store, "upstream", &remote.url(), &[]).ok();
    fetch(&store, ORIGIN);
    fetch(&store, "upstream");
    let count = changesets(&store);
    let store_refs = refs(&store);

    // 1. Removed: its two refs (main, the tag), nothing of the store.
    let removed = store.read(&["remote", "remove", ORIGIN]).ok();
    assert_eq!(removed, json!({"name": ORIGIN, "removed_refs": 2}));
    assert_eq!(changesets(&store), count);
    assert_eq!(refs(&store), store_refs);

    // 2. The mirror and the file keep the other remote only.
    let none: Vec<String> = Vec::new();
    assert_eq!(
        mirror_refs(&remote, store.path(), "refs/remotes/origin/"),
        none
    );
    assert_eq!(
        mirror_refs(&remote, store.path(), "refs/remote-tags/origin/"),
        none
    );
    assert_eq!(
        mirror_refs(&remote, store.path(), "refs/remotes/upstream/").len(),
        1
    );
    let listed = store.read(&["remote", "list"]).ok();
    assert_eq!(listed["remotes"][0]["name"], "upstream");
    assert_eq!(listed["remotes"].as_array().map(Vec::len), Some(1));

    // 3. Removed again: not found.
    let again = store.read(&["remote", "remove", ORIGIN]);
    again.err("E_REMOTE_NOT_FOUND");
}

/// A fetch moves no ref of the store: `main` and every other ref name what they named.
#[test]
fn fetch_moves_no_store_ref() {
    let remote = Remote::new();
    let store = remote.store();
    store.commit(AGENT, "local.txt", "local\n", None);
    let before = refs(&store);
    add(&store, ORIGIN, &remote.url(), &[]).ok();

    let fetched = fetch(&store, ORIGIN);
    assert_eq!(fetched["imported"], 2, "{fetched}");
    assert_eq!(refs(&store), before);
}

/// `--repo` relative to the working directory: the fetch's git runs in the mirror, and still
/// finds it.
#[test]
fn fetch_with_a_relative_repo() {
    let remote = Remote::new();
    let store = remote.store();
    add(&store, ORIGIN, &remote.url(), &[]).ok();

    // The store named from its parent directory.
    let path = Path::new(store.path());
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .expect("a UTF-8 store name");
    let parent = path.parent().expect("the store's parent");
    let args = ["--repo", name, "fetch", ORIGIN];
    let output = swarmops(&args, &remote.vars(&[]))
        .current_dir(parent)
        .output()
        .expect("run swarmops");
    let fetched = Run {
        args: args.iter().map(|arg| (*arg).to_owned()).collect(),
        status: output.status,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
    .ok();
    assert_eq!(fetched["imported"], 2, "{fetched}");
}

/// A fetch imports the followed branch's commits only: the remote's other branches reach the
/// mirror, not the store. Its response names the followed branch's head, by commit and
/// changeset, and lists the branches that appeared.
#[test]
fn fetch_imports_the_followed_branch_only() {
    let remote = Remote::new();
    remote.commit("topic", "unrelated", &[("t.txt", "t\n")]);
    let store = remote.store();
    add(&store, ORIGIN, &remote.url(), &[]).ok();

    let fetched = fetch(&store, ORIGIN);
    assert_eq!(fetched["remote"], ORIGIN);
    assert_eq!(fetched["branch"], "main");
    assert_eq!(fetched["imported"], 2);
    assert_eq!(changesets(&store), 2);
    assert_eq!(fetched["head"]["sha"], remote.head("main"));
    assert!(fetched.get("old_head").is_none(), "{fetched}");
    let shown = store.show(fetched["head"]["changeset"].as_str().expect("an id"));
    assert_eq!(shown["metadata"]["git.oid"], remote.head("main"), "{shown}");
    let branches: Vec<&str> = (fetched["branches"].as_array().expect("a list").iter())
        .map(|update| update["branch"].as_str().expect("a name"))
        .collect();
    assert_eq!(branches, ["main", "topic"]);
    assert_eq!(fetched["branches_truncated"], false);

    // The topic is in the mirror only.
    assert_eq!(
        mirror_refs(&remote, store.path(), "refs/remotes/origin/topic"),
        [format!(
            "refs/remotes/origin/topic {}",
            remote.head("topic")
        )]
    );
}

/// A fetch run again with nothing new on the remote imports nothing and lists no branch; its
/// head and old head are the same.
#[test]
fn fetch_run_again_imports_nothing() {
    let remote = Remote::new();
    remote.git(&["tag", "v1", "main"]);
    let store = remote.store();
    add(&store, ORIGIN, &remote.url(), &[]).ok();
    let first = fetch(&store, ORIGIN);
    assert_eq!(first["tags_fetched"], 1, "{first}");

    let again = fetch(&store, ORIGIN);
    assert_eq!(again["imported"], 0, "{again}");
    assert_eq!(again["branches"], json!([]));
    assert_eq!(again["tags_fetched"], 0);
    assert_eq!(again["head"], first["head"]);
    assert_eq!(again["old_head"], first["head"]);
    assert_eq!(changesets(&store), 2);
}

/// After the remote moves one branch, adds one and deletes one, the fetch lists the three, by
/// name, with what each named before and names now; the branch that stayed is not listed, and
/// the followed branch's new commit is imported.
#[test]
fn fetch_reports_moved_new_and_pruned_branches() {
    let remote = Remote::new();
    remote.commit("gone", "doomed", &[("g.txt", "g\n")]);
    remote.commit("stay", "kept", &[("s.txt", "s\n")]);
    let store = remote.store();
    add(&store, ORIGIN, &remote.url(), &[]).ok();
    fetch(&store, ORIGIN);

    // The remote moves on.
    remote.commit("main", "third", &[("c.txt", "c\n")]);
    remote.commit("fresh", "new branch", &[("f.txt", "f\n")]);
    remote.git(&["update-ref", "-d", "refs/heads/gone"]);
    remote.git(&["tag", "v2", "main"]);

    let fetched = fetch(&store, ORIGIN);
    assert_eq!(fetched["imported"], 1);
    insta::assert_json_snapshot!(fetched);
}

/// The followed branch holding a submodule entry fails the fetch with `E_GIT_IMPORT_LOSSY`
/// unless the remote allows the class (`--lossy gitlinks`); the mirror keeps what git fetched.
#[test]
fn fetch_of_a_lossy_followed_branch_fails_unless_allowed() {
    let remote = Remote::new();
    let parent = remote.head("main");
    let blob = remote.git(&["rev-parse", &format!("{parent}:a.txt")]);
    let tree = mktree(
        &remote,
        &format!("160000 commit {parent}\tsub\n100644 blob {blob}\ta.txt\n"),
    );
    let lossy = remote.git(&["commit-tree", &tree, "-p", &parent, "-m", "with a gitlink"]);
    remote.git(&["update-ref", "refs/heads/main", &lossy, &parent]);
    let store = remote.store();

    // 1. Refused: nothing lossy allowed; git's fetch stays in the mirror.
    add(&store, ORIGIN, &remote.url(), &[]).ok();
    let refused = store.read(&["fetch", ORIGIN]);
    refused.err("E_GIT_IMPORT_LOSSY");
    assert_eq!(
        mirror_refs(&remote, store.path(), "refs/remotes/origin/main"),
        [format!("refs/remotes/origin/main {lossy}")]
    );

    // 2. Allowed: the commit is imported with its loss recorded.
    add(&store, "allowed", &remote.url(), &["--lossy", "gitlinks"]).ok();
    let fetched = fetch(&store, "allowed");
    assert_eq!(fetched["head"]["sha"], lossy, "{fetched}");
}

/// `git mktree` on the remote with `entries`; the tree.
fn mktree(remote: &Remote, entries: &str) -> String {
    let mut child = remote
        .server
        .command()
        .arg("--git-dir")
        .arg(remote.server.bare(REPO))
        .arg("mktree")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("run git mktree");
    let mut stdin = child.stdin.take().expect("a piped stdin");
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

/// `clone` over HTTP and over ssh makes a store named after the URL, records the remote, and
/// imports the followed branch into `main`: both clones name the same changeset.
#[test]
fn clone_over_http_and_ssh() {
    let remote = Remote::new();
    let dir = tempfile::tempdir().expect("a temp dir");
    let http_into = path_text(&dir.path().join("http"));
    let ssh_into = path_text(&dir.path().join("ssh"));

    // 1. Over HTTP.
    let url = remote.url();
    let args = ["--as", AGENT, "clone", url.as_str(), "--into", &http_into];
    let cloned = remote.run(&args, &[]).ok();
    assert_eq!(cloned["repo"]["name"], REPO, "{cloned}");
    assert_eq!(cloned["remote"]["name"], ORIGIN);
    assert_eq!(cloned["remote"]["branch"], "main");
    assert_eq!(cloned["fetch"]["head"]["sha"], remote.head("main"));
    assert_eq!(cloned["imported"], 2);
    let main = cloned["main"].as_str().expect("main").to_owned();
    assert_eq!(cloned["fetch"]["head"]["changeset"], main.as_str());

    // 2. Over ssh, through the stand-in.
    let ssh = remote.server.ssh_command(SshMode::Serve);
    let ssh_url = remote.server.ssh_url(REPO);
    let args = [
        "--as",
        AGENT,
        "clone",
        ssh_url.as_str(),
        "--into",
        &ssh_into,
    ];
    let cloned = remote.run(&args, &[("GIT_SSH_COMMAND", &ssh)]).ok();
    assert_eq!(cloned["main"], main.as_str(), "{cloned}");

    // 3. Each store's main names it.
    for into in [&http_into, &ssh_into] {
        let listed = run(&["--repo", into, "ref", "list"], &[]).ok();
        let main_ref = (listed["refs"].as_array().expect("refs").iter())
            .find(|entry| entry["name"] == "main")
            .cloned()
            .expect("main");
        assert_eq!(main_ref["target"], main.as_str(), "{listed}");
    }
}

/// A clone of an empty remote records `main` as the followed branch, with the warning
/// `default_branch_unknown`, and makes a store with no `main`.
#[test]
fn clone_of_an_empty_remote_has_no_main_and_follows_main() {
    let remote = Remote::empty();
    let dir = tempfile::tempdir().expect("a temp dir");
    let into = path_text(&dir.path().join("store"));
    let url = remote.url();

    let cloned = remote
        .run(&["--as", AGENT, "clone", &url, "--into", &into], &[])
        .ok();
    assert_eq!(
        cloned["remote"]["warnings"],
        json!(["default_branch_unknown"])
    );
    assert_eq!(cloned["remote"]["branch"], "main");
    assert!(cloned.get("main").is_none(), "{cloned}");
    assert_eq!(cloned["imported"], 0);
    let text = fs::read_to_string(Path::new(&into).join(REMOTES_TOML)).expect("remotes.toml");
    assert!(text.contains("branch = \"main\""), "{text}");
    let listed = run(&["--repo", &into, "ref", "list"], &[]).ok();
    assert_eq!(listed["refs"], json!([]), "{listed}");
}

/// `remote add --branch` of an empty remote records the branch given, with no warning, and
/// asks the remote nothing.
#[test]
fn remote_add_of_an_empty_remote_with_branch_has_no_warning() {
    let remote = Remote::empty();
    let store = remote.store();

    let added = add(&store, ORIGIN, &remote.url(), &["--branch", "trunk"]).ok();
    assert_eq!(added["branch"], "trunk");
    assert!(added.get("warnings").is_none(), "{added}");

    // Without it, the remote names no default branch: main, with the warning.
    let guessed = add(&store, "other", &remote.url(), &[]).ok();
    assert_eq!(guessed["branch"], "main");
    assert_eq!(guessed["warnings"], json!(["default_branch_unknown"]));
}

/// A clone interrupted after it recorded its remote resumes into its own store: run again, it
/// fetches and moves `main`; run once more, it changes nothing. A clone of another URL into it
/// is `E_EXISTS`.
#[test]
fn clone_resumes_into_its_own_store() {
    let remote = Remote::new();
    let url = remote.url();
    let store = remote.store();
    add(&store, ORIGIN, &url, &[]).ok();
    let into = store.path().to_owned();

    // 1. Resumed: fetched and imported into main.
    let args = ["--as", AGENT, "clone", url.as_str(), "--into", &into];
    let resumed = remote.run(&args, &[]).ok();
    assert_eq!(resumed["imported"], 2, "{resumed}");
    let main = resumed["main"].clone();
    assert!(main.is_string(), "{resumed}");

    // 2. Again: nothing new.
    let again = remote.run(&args, &[]).ok();
    assert_eq!(again["imported"], 0, "{again}");
    assert_eq!(again["main"], main);

    // 3. Another URL: refused.
    let other = remote.server.ssh_url(REPO);
    let refused = remote.run(&["--as", AGENT, "clone", &other, "--into", &into], &[]);
    refused.err("E_EXISTS");
}

/// A clone into a directory that holds something other than its own store is `E_EXISTS`, and
/// makes nothing there.
#[test]
fn clone_into_a_non_empty_directory_is_exists() {
    let remote = Remote::new();
    let dir = tempfile::tempdir().expect("a temp dir");
    fs::write(dir.path().join("notes.txt"), "mine\n").expect("write a file");
    let into = path_text(dir.path());
    let url = remote.url();

    let refused = remote.run(&["--as", AGENT, "clone", &url, "--into", &into], &[]);
    refused.err("E_EXISTS");
    let names: Vec<String> = fs::read_dir(dir.path())
        .expect("the directory")
        .map(|entry| {
            entry
                .expect("an entry")
                .file_name()
                .into_string()
                .expect("UTF-8")
        })
        .collect();
    assert_eq!(names, ["notes.txt"]);
}

/// A `remotes.toml` of another version is `E_STORE_CORRUPT`, naming the file as a store file,
/// for the commands that read it.
#[test]
fn remotes_toml_with_version_2_is_store_corrupt() {
    let remote = Remote::new();
    let store = remote.store();
    let file = Path::new(store.path()).join(REMOTES_TOML);
    fs::write(&file, "version = 2\n").expect("write remotes.toml");

    for line in [&["remote", "list"][..], &["fetch", ORIGIN][..]] {
        let failed = store.read(line);
        let error = failed.err("E_STORE_CORRUPT");
        assert_eq!(error["details"]["store_file"], REMOTES_TOML, "{failed}");
    }
}

/// The MCP server's `remote_list` and `fetch` tools give what the CLI gives.
#[test]
fn mcp_remote_list_and_fetch() {
    let remote = Remote::new();
    let store = remote.store();
    add(&store, ORIGIN, &remote.url(), &[]).ok();
    let meta = json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {},
        "io.modelcontextprotocol/clientInfo": {"name": "swarmops-tests", "version": "0"},
    });

    // 1. Two calls, one line each, then stdin closes.
    let calls = [
        ("remote_list", json!({})),
        ("fetch", json!({"remote": ORIGIN})),
    ];
    let mut input = String::new();
    for (id, (tool, arguments)) in calls.iter().enumerate() {
        let params = json!({"name": tool, "arguments": arguments, "_meta": meta});
        let request = json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": params});
        input.push_str(&request.to_string());
        input.push('\n');
    }
    let served = run_bin(
        BIN,
        &["mcp", "--repo", store.path()],
        &remote.vars(&[]),
        &input,
    );

    // 2. Each reply's envelope.
    let envelopes: Vec<Value> = served
        .stdout
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("one JSON-RPC reply a line"))
        .map(|reply| reply["result"]["structuredContent"].clone())
        .collect();
    assert_eq!(envelopes.len(), 2, "{served}");
    assert_eq!(envelopes[0]["ok"], true, "{served}");
    assert_eq!(envelopes[0]["result"]["remotes"][0]["name"], ORIGIN);
    assert_eq!(envelopes[1]["ok"], true, "{served}");
    assert_eq!(envelopes[1]["result"]["imported"], 2);
    assert_eq!(changesets(&store), 2);
}

/// `git import` of a store's own mirror is `E_INVALID_ARG` `store_mirror`, while `fetch` and
/// `clone` import from that mirror.
#[test]
fn fetch_and_clone_import_the_mirror_despite_the_store_mirror_refusal() {
    let remote = Remote::new();
    let dir = tempfile::tempdir().expect("a temp dir");
    let into = path_text(&dir.path().join("store"));
    let url = remote.url();
    remote
        .run(&["--as", AGENT, "clone", &url, "--into", &into], &[])
        .ok();
    let mirror = path_text(&Path::new(&into).join("git").join("mirror.git"));

    // 1. Refused to a person.
    let args = [
        "--repo",
        into.as_str(),
        "--as",
        AGENT,
        "git",
        "import",
        &mirror,
    ];
    let refused = remote.run(&args, &[]);
    let error = refused.err("E_INVALID_ARG");
    assert_eq!(error["details"]["reason"], "store_mirror", "{refused}");

    // 2. Imported by a fetch.
    remote.commit("main", "third", &[("c.txt", "c\n")]);
    let fetched = remote.run(&["--repo", &into, "fetch", ORIGIN], &[]).ok();
    assert_eq!(fetched["imported"], 1, "{fetched}");
}

/// A remote whose branch reaches an object git's fsck refuses (a tree with one entry twice)
/// fails the fetch with `E_REMOTE_REJECTED` `bad_objects`: no tracking ref moves and nothing is
/// imported, and the same fetch fails the same way again. Once the remote's branch names good
/// commits only, the fetch succeeds and imports.
#[test]
fn a_fetch_of_a_bad_object_moves_no_ref_and_imports_nothing() {
    let remote = Remote::new();
    let store = remote.store();
    add(&store, ORIGIN, &remote.url(), &[]).ok();
    fetch(&store, ORIGIN);
    let good = remote.head("main");
    let tracked = || mirror_refs(&remote, store.path(), "refs/remotes/origin/");
    let before = (tracked(), refs(&store), changesets(&store));
    remote
        .server
        .commit_fsck_error(REPO, "main", "a tree with one entry twice");

    // 1. Refused, twice the same way, with nothing moved.
    for _ in 0..2 {
        let refused = store.read(&["fetch", ORIGIN]);
        let error = refused.err("E_REMOTE_REJECTED");
        assert_eq!(error["details"]["reason"], "bad_objects", "{refused}");
        assert_eq!(error["retryable"], false, "{refused}");
        assert_eq!((tracked(), refs(&store), changesets(&store)), before);
    }

    // 2. The remote's branch back on good commits: fetched and imported.
    remote.server.move_to(REPO, "main", &good);
    remote.commit("main", "third", &[("c.txt", "c\n")]);
    let fetched = fetch(&store, ORIGIN);
    assert_eq!(fetched["imported"], 1, "{fetched}");
    assert_eq!(fetched["head"]["sha"], remote.head("main"));
}
