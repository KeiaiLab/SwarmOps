//! `swarmops serve`'s read side (M14 design §2, §4), as stock git meets it over ssh through the
//! test kit's forced-command stand-in: clone, fetch, pull and shallow clones with protocol v2 and
//! v0, the round trip of an imported repository, and the refusals before git starts. A refusal is
//! one `ERR` pkt-line on stdout, which git prints as `remote error:`, and the envelope on stderr;
//! the caller is named by `--as` alone, never by `SWARMOPS_AGENT`; nothing a client is sent names
//! the served root.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

use serde_json::{Value, json};
use swarmops_testkit::git::fixture;
use swarmops_testkit::serve::ServeSite;
use tempfile::TempDir;

use crate::support::{self, Run, Swarmops};

/// The agent a forced command names.
const AGENT: &str = "person/ada";

/// What OpenSSH names the client's command in.
const SSH_ORIGINAL_COMMAND: &str = "SSH_ORIGINAL_COMMAND";

/// A fetch of the store `x`, as git over ssh asks for it.
const UPLOAD_X: &str = "git-upload-pack 'x'";

/// The agent that writes the served store directly, through the command line.
const WRITER: &str = "agent/worker-1";

/// The store every site serves.
const STORE: &str = "acme";

/// The served store's access file: [`AGENT`] reads, `person/bob` is named and granted nothing.
const ACCESS: &str = r#"version = 1

[[grant]]
who = ["person/ada"]
read = true

[[grant]]
who = ["person/bob"]
"#;

/// The branches of the `merges` fixture, each imported into the store ref of its name.
const MERGES_BRANCHES: [&str; 8] = [
    "main", "feature", "one", "two", "three", "side", "cross-a", "cross-b",
];

/// The `merges` fixture's lightweight tag, imported as `tags/light`.
const MERGES_TAG: &str = "light";

/// The MCP revision these tests open a session in.
const MCP_REVISION: &str = "2025-11-25";

/// What `run` refused with: the `ERR` pkt-line's text on stdout and the envelope on stderr, both
/// checked against the refusal's contract.
fn refusal(run: &Run) -> (String, Value) {
    // 1. stdout: one pkt-line, its four hex digits counting themselves.
    let stdout = run.stdout.as_str();
    let len = stdout
        .get(..4)
        .and_then(|digits| usize::from_str_radix(digits, 16).ok())
        .unwrap_or_else(|| panic!("a pkt-line on stdout: {run}"));
    assert_eq!(stdout.len(), len, "one pkt-line and nothing else: {run}");
    let text = stdout[4..]
        .strip_prefix("ERR ")
        .unwrap_or_else(|| panic!("an ERR pkt-line: {run}"))
        .to_owned();

    // 2. stderr: the envelope, with the run's exit code.
    let envelope: Value = serde_json::from_str(&run.stderr)
        .unwrap_or_else(|e| panic!("the envelope on stderr ({e}): {run}"));
    assert_eq!(
        (envelope["ok"].as_bool(), envelope["op"].as_str()),
        (Some(false), Some("serve")),
        "{run}"
    );
    assert_eq!(envelope["error"]["exit"], run.exit(), "{run}");
    let code = envelope["error"]["code"].as_str().unwrap();
    let message = envelope["error"]["message"].as_str().unwrap();
    assert_eq!(text, format!("{code}: {message}"), "{run}");
    (text, envelope["error"].clone())
}

/// `serve` without `--root` is a usage error: exit 2, its envelope on stderr, stdout left to
/// git.
#[test]
fn serve_requires_root() {
    for args in [&["serve"][..], &["serve", "--as", AGENT]] {
        let run = support::run(args, &[]);
        assert_eq!(run.exit(), 2, "{run}");
        assert_eq!(run.stdout, "", "{run}");
        let envelope: Value = serde_json::from_str(&run.stderr)
            .unwrap_or_else(|e| panic!("the envelope on stderr ({e}): {run}"));
        assert_eq!(envelope["error"]["code"], "E_INVALID_ARG", "{run}");
    }
}

/// `swarmops serve --stdio`, the store-to-store sync, is M11's: refused `not_served`, asked for
/// through the forced command or on the line.
#[test]
fn serve_refuses_stdio_until_m11() {
    let root = TempDir::new().unwrap();
    let root = root.path().to_str().unwrap();
    let line = ["serve", "--root", root, "--as", AGENT];
    let asked = support::run(&line, &[(SSH_ORIGINAL_COMMAND, "swarmops serve --stdio")]);
    let flagged = support::run(&[&line[..], &["--stdio"]].concat(), &[]);
    for run in [asked, flagged] {
        assert_eq!(run.exit(), 2, "{run}");
        let (text, error) = refusal(&run);
        assert!(text.starts_with("E_INVALID_ARG: "), "{run}");
        assert_eq!(error["details"]["reason"], "not_served", "{run}");
        assert!(!text.contains(root), "no host path: {run}");
    }
}

/// The serving account's `SWARMOPS_AGENT` names no caller: without `--as`, a connection is
/// `E_AUTH` `no_identity` before anything else is looked at.
#[test]
fn serve_ignores_agent_env() {
    let root = TempDir::new().unwrap();
    let line = ["serve", "--root", root.path().to_str().unwrap()];
    let env = [("SWARMOPS_AGENT", AGENT), (SSH_ORIGINAL_COMMAND, UPLOAD_X)];
    let run = support::run(&line, &env);
    assert_eq!(run.exit(), 5, "{run}");
    let (text, error) = refusal(&run);
    assert!(text.starts_with("E_AUTH: "), "{run}");
    assert!(!text.contains("not built"), "{run}");
    assert_eq!(error["details"]["reason"], "no_identity", "{run}");
}

/// `serve` is no MCP tool, and the MCP server's documentation says so beside `derive`.
#[test]
fn serve_has_no_mcp_tool() {
    // 1. tools/list, in a session of its own.
    let messages = [
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": MCP_REVISION,
            "capabilities": {},
            "clientInfo": {"name": "serve-test", "version": "1"},
        }}),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    ];
    let lines: Vec<String> = messages.iter().map(Value::to_string).collect();
    let input = lines.join("\n") + "\n";
    let run = support::run_bin(support::BIN, &["mcp", "--repo", "unused"], &[], &input);
    assert_eq!(run.exit(), 0, "{run}");
    let listed: Value = run
        .stdout
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|reply| reply["id"] == 2)
        .unwrap_or_else(|| panic!("a tools/list reply: {run}"));
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"commit"), "{names:?}");
    assert!(
        !names.iter().any(|name| name.contains("serve")),
        "{names:?}"
    );

    // 2. The module's documentation.
    let doc = include_str!("../src/mcp.rs");
    let said = doc
        .lines()
        .filter(|line| line.starts_with("//!"))
        .any(|line| line.contains("`derive` and `serve` have no tool"));
    assert!(said, "src/mcp.rs names serve beside derive");
}

/// The protocol a client asks for.
#[derive(Clone, Copy, Debug)]
enum Protocol {
    /// Protocol v2, which git asks for by default.
    V2,
    /// The original protocol.
    V0,
}

impl Protocol {
    /// Both, newest first.
    const ALL: [Protocol; 2] = [Protocol::V2, Protocol::V0];

    /// The `-c` setting that asks for it.
    fn setting(self) -> &'static str {
        match self {
            Protocol::V2 => "protocol.version=2",
            Protocol::V0 => "protocol.version=0",
        }
    }
}

/// A served root with the store [`STORE`] under [`ACCESS`], and a directory for clients.
struct Site {
    /// The root, its stand-ins and the client git.
    served: ServeSite,
    /// The clients' working directories.
    work: TempDir,
    /// The changes written so far, for the next one's name.
    changes: std::cell::Cell<u32>,
}

impl Site {
    /// An empty store, served.
    fn new() -> Site {
        let site = ServeSite::new::<Swarmops>();
        site.init(STORE);
        let site = Site {
            served: site,
            work: TempDir::new().unwrap(),
            changes: std::cell::Cell::new(0),
        };
        site.grant(ACCESS);
        site
    }

    /// The store's path.
    fn store(&self) -> String {
        self.served.store(STORE).to_str().unwrap().to_owned()
    }

    /// The root's path, which no client output may hold.
    fn root(&self) -> String {
        self.served.root().to_str().unwrap().to_owned()
    }

    /// Writes `text` as the store's access file.
    fn grant(&self, text: &str) {
        fs::write(self.served.store(STORE).join("access.toml"), text).unwrap();
    }

    /// `args` on the store as [`WRITER`]; its result.
    fn so(&self, args: &[&str]) -> Value {
        let store = self.store();
        let line: Vec<&str> = ["--repo", &store, "--as", WRITER]
            .iter()
            .chain(args)
            .copied()
            .collect();
        support::run(&line, &[]).ok()
    }

    /// The `git import` of the fixture repository at `source`'s `git_ref` into `into`.
    fn import(&self, source: &Path, git_ref: &str, into: &str) {
        let source = source.to_str().unwrap();
        self.so(&["git", "import", source, "--ref", git_ref, "--into", into]);
    }

    /// The `linear` fixture imported as `main`.
    fn with_linear(self) -> Site {
        let linear = fixture("linear");
        self.import(linear.repo.as_deref().unwrap(), "HEAD", "main");
        self
    }

    /// An agent's change of `path` landed on `main`, as [`WRITER`].
    fn land(&self, path: &str) {
        let n = self.changes.get();
        self.changes.set(n + 1);
        let changes = self.work.path().join(format!("change-{n}.json"));
        let list = json!([{"path": path, "op": "put", "content": {"inline": format!("{n}\n")}}]);
        fs::write(&changes, list.to_string()).unwrap();
        let changes = changes.to_str().unwrap();
        let commit = self.so(&[
            "commit",
            "-m",
            path,
            "--changes",
            changes,
            "--parent",
            "main",
        ]);
        let id = commit["changeset"].as_str().unwrap();
        self.so(&["land", id, "--target", "main"]);
    }

    /// The commit `serve.git` advertises for `git_ref`.
    fn served(&self, git_ref: &str) -> String {
        let serve_git = self.served.store(STORE).join("git/serve.git");
        let output = self
            .served
            .command()
            .arg("--git-dir")
            .arg(serve_git)
            .args(["rev-parse", "--verify", git_ref])
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", text(&output.stderr));
        text(&output.stdout).trim().to_owned()
    }

    /// A client directory, `name` under the work directory.
    fn dir(&self, name: &str) -> PathBuf {
        self.work.path().join(name)
    }

    /// git with `args` in `dir`, as `identity` over ssh, asking for `protocol`; what it did. Its
    /// stderr never holds the root.
    fn git_as(&self, identity: &str, dir: &Path, protocol: Protocol, args: &[&str]) -> Output {
        let output = self
            .served
            .command()
            .env("GIT_SSH_COMMAND", self.served.ssh_command(identity))
            .current_dir(dir)
            .args(["-c", protocol.setting()])
            .args(args)
            .output()
            .unwrap();
        let stderr = text(&output.stderr);
        assert!(!stderr.contains(&self.root()), "a host path: {stderr}");
        output
    }

    /// git with `args` in `dir` as [`AGENT`], which must succeed; its stdout.
    fn git(&self, dir: &Path, protocol: Protocol, args: &[&str]) -> String {
        let output = self.git_as(AGENT, dir, protocol, args);
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            text(&output.stderr)
        );
        text(&output.stdout)
    }

    /// The store cloned into `name` with `protocol` and `flags`; the clone's directory.
    fn clone(&self, name: &str, protocol: Protocol, flags: &[&str]) -> PathBuf {
        let url = self.served.ssh_url(STORE);
        let dir = self.dir(name);
        let line = [&["clone", "--quiet"][..], flags, &[url.as_str(), name]].concat();
        self.git(self.work.path(), protocol, &line);
        dir
    }

    /// The client's `rev-parse` of `rev` in `dir`.
    fn rev(&self, dir: &Path, rev: &str) -> String {
        let out = self.git(dir, Protocol::V2, &["rev-parse", "--verify", rev]);
        out.trim().to_owned()
    }
}

/// `bytes` as text.
fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Every ref an `ls-remote` lists, by name, `HEAD` aside.
fn listed(ls_remote: &str) -> BTreeMap<String, String> {
    ls_remote
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .filter(|(_, name)| *name != "HEAD")
        .map(|(oid, name)| (name.to_owned(), oid.to_owned()))
        .collect()
}

/// A repository imported into the store comes back through `serve` as it was: every branch and
/// the lightweight tag at the commits the source has, with either protocol, and the clone passes
/// `git fsck --strict`.
#[test]
fn serve_round_trips_an_imported_repository() {
    // 1. The `merges` fixture's branches and lightweight tag, imported.
    let site = Site::new();
    let merges = fixture("merges");
    let source = merges.repo.as_deref().unwrap();
    for branch in MERGES_BRANCHES {
        site.import(source, &format!("refs/heads/{branch}"), branch);
    }
    let tag = format!("refs/tags/{MERGES_TAG}");
    site.import(source, &tag, &format!("tags/{MERGES_TAG}"));
    let refs = &merges.manifest.as_ref().unwrap().refs;
    let expected: BTreeMap<String, String> = MERGES_BRANCHES
        .iter()
        .map(|branch| format!("refs/heads/{branch}"))
        .chain([tag])
        .map(|name| (name.clone(), refs[&name].clone()))
        .collect();

    // 2. Advertised as the source has them, cloned and checked, with each protocol.
    let url = site.served.ssh_url(STORE);
    for protocol in Protocol::ALL {
        let advertised = site.git(site.work.path(), protocol, &["ls-remote", &url]);
        assert_eq!(listed(&advertised), expected, "{protocol:?}");

        let clone = site.clone(&format!("round-{protocol:?}"), protocol, &["--mirror"]);
        let cloned = site.git(
            &clone,
            protocol,
            &["for-each-ref", "--format=%(objectname)\t%(refname)"],
        );
        let cloned: BTreeMap<String, String> = cloned
            .lines()
            .filter_map(|line| line.split_once('\t'))
            .map(|(oid, name)| (name.to_owned(), oid.to_owned()))
            .collect();
        assert_eq!(cloned, expected, "{protocol:?}");
        site.git(&clone, protocol, &["fsck", "--strict", "--no-dangling"]);
    }
}

/// Stock git's read commands work against `serve` with either protocol: a clone, a fetch after
/// an agent lands, `pull --rebase` and `pull --no-rebase` over a local commit, and a clone of
/// depth 1.
#[test]
fn serve_serves_clone_fetch_pull_and_depth() {
    let site = Site::new().with_linear();
    for protocol in Protocol::ALL {
        // 1. A clone has main where serve.git has it.
        let clone = site.clone(&format!("work-{protocol:?}"), protocol, &[]);
        assert_eq!(
            site.rev(&clone, "origin/main"),
            site.served("refs/heads/main")
        );

        // 2. A fetch after an agent lands moves origin/main to the land's export.
        let before = site.rev(&clone, "origin/main");
        site.land(&format!("fetched-{protocol:?}.txt"));
        site.git(&clone, protocol, &["fetch", "--quiet"]);
        let after = site.rev(&clone, "origin/main");
        assert_ne!(after, before, "{protocol:?}");
        assert_eq!(after, site.served("refs/heads/main"), "{protocol:?}");

        // 3. pull --rebase over a local commit: the commit on top of the store's main.
        site.git(
            &clone,
            protocol,
            &["merge", "--quiet", "--ff-only", "origin/main"],
        );
        let local = |name: &str| {
            fs::write(clone.join(name), "local\n").unwrap();
            site.git(&clone, protocol, &["add", name]);
            site.git(&clone, protocol, &["commit", "--quiet", "-m", name]);
        };
        local(&format!("rebased-{protocol:?}.txt"));
        site.land(&format!("under-rebase-{protocol:?}.txt"));
        site.git(&clone, protocol, &["pull", "--quiet", "--rebase"]);
        assert_eq!(site.rev(&clone, "HEAD~1"), site.served("refs/heads/main"));

        // 4. pull --no-rebase: a merge of the local commit and the store's main.
        local(&format!("merged-{protocol:?}.txt"));
        site.land(&format!("under-merge-{protocol:?}.txt"));
        site.git(
            &clone,
            protocol,
            &["pull", "--quiet", "--no-rebase", "--no-edit"],
        );
        assert_eq!(site.rev(&clone, "HEAD^2"), site.served("refs/heads/main"));

        // 5. A clone of depth 1: one commit, shallow.
        let shallow = site.clone(
            &format!("shallow-{protocol:?}"),
            protocol,
            &["--depth", "1"],
        );
        let count = site.git(&shallow, protocol, &["rev-list", "--count", "HEAD"]);
        assert_eq!(count.trim(), "1", "{protocol:?}");
        assert!(shallow.join(".git/shallow").is_file(), "{protocol:?}");
        assert_eq!(site.rev(&shallow, "HEAD"), site.served("refs/heads/main"));
    }
}

/// A fetch with nothing new exports nothing: `serve.git` holds the same objects and refs after
/// it, and the client is told nothing.
#[test]
fn serve_noop_fetch_exports_nothing() {
    let site = Site::new().with_linear();
    let clone = site.clone("noop", Protocol::V2, &[]);
    let serve_git = site.served.store(STORE).join("git/serve.git");
    let state = || {
        let objects = walk(&serve_git.join("objects"));
        let refs = site.git(&clone, Protocol::V2, &["ls-remote", "origin"]);
        (objects, refs)
    };
    let before = state();
    let output = site.git_as(AGENT, &clone, Protocol::V2, &["fetch"]);
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert_eq!(text(&output.stderr), "", "a fetch of nothing says nothing");
    let after = state();
    assert_eq!(after.0, before.0, "no object written");
    assert_eq!(after.1, before.1);
}

/// Every file under `dir`, by its path.
fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut dirs = vec![dir.to_owned()];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                dirs.push(path);
            } else {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

/// A partial clone gets the whole repository: `serve` advertises no `filter`, and git, warned,
/// fetches every blob.
#[test]
fn serve_advertises_no_filter() {
    let site = Site::new().with_linear();

    // 1. The v2 capabilities: fetch without filter.
    let url = site.served.ssh_url(STORE);
    let traced = site
        .served
        .command()
        .env("GIT_SSH_COMMAND", site.served.ssh_command(AGENT))
        .env("GIT_TRACE_PACKET", "1")
        .current_dir(site.work.path())
        .args(["ls-remote", &url])
        .output()
        .unwrap();
    assert!(traced.status.success(), "{}", text(&traced.stderr));
    let trace = text(&traced.stderr);
    assert!(trace.contains("< version 2"), "{trace}");
    let fetch: Vec<&str> = trace
        .lines()
        .filter(|line| line.contains("< fetch="))
        .collect();
    assert!(!fetch.is_empty(), "{trace}");
    assert!(
        fetch.iter().all(|line| !line.contains("filter")),
        "{fetch:?}"
    );

    // 2. A clone asking for a filter: warned, and whole.
    for protocol in Protocol::ALL {
        let name = format!("filtered-{protocol:?}");
        let line = ["clone", "--filter=blob:none", url.as_str(), name.as_str()];
        let output = site.git_as(AGENT, site.work.path(), protocol, &line);
        let stderr = text(&output.stderr);
        assert!(output.status.success(), "{stderr}");
        assert!(
            stderr.contains("filtering not recognized by server"),
            "{stderr}"
        );
        let clone = site.dir(&name);
        let objects = ["rev-list", "--objects", "--missing=print", "--all"];
        let missing = site.git(&clone, protocol, &objects);
        assert!(
            !missing.lines().any(|line| line.starts_with('?')),
            "{missing}"
        );
    }
}

/// Before git starts, `serve` refuses with git's `remote error:`: a caller the access file names
/// without a read, one it does not name, a store without an access file, one that is not there,
/// a directory that is no store, and a path that names no store. No refusal names the root.
#[test]
fn serve_refuses_readers_before_git() {
    let site = Site::new().with_linear();
    let url = site.served.ssh_url(STORE);
    let refused = |identity: &str, url: &str| {
        let output = site.git_as(
            identity,
            site.work.path(),
            Protocol::V2,
            &["ls-remote", url],
        );
        assert!(!output.status.success(), "{identity} {url}");
        text(&output.stderr)
    };

    // 1. Named without a read; not named.
    let denied = refused("person/bob", &url);
    assert!(denied.contains("remote error: E_POLICY_DENIED"), "{denied}");
    let unknown = refused("person/carol", &url);
    assert!(unknown.contains("remote error: E_AUTH"), "{unknown}");

    // 2. A store that is not there, a directory that is no store, and a path that names none.
    let missing = refused(AGENT, &site.served.ssh_url("nowhere"));
    assert!(missing.contains("remote error: E_NOT_A_REPO"), "{missing}");
    fs::create_dir(site.served.root().join("plain")).unwrap();
    let plain = refused(AGENT, &site.served.ssh_url("plain"));
    assert!(plain.contains("remote error: E_NOT_A_REPO"), "{plain}");
    let nested = refused(AGENT, "ssh://git@remote.invalid/acme/inner.git");
    assert!(nested.contains("remote error: E_INVALID_ARG"), "{nested}");
    assert!(nested.contains("bad_path"), "{nested}");

    // 3. No access file: nobody reads.
    fs::remove_file(site.served.store(STORE).join("access.toml")).unwrap();
    let closed = refused(AGENT, &url);
    assert!(closed.contains("remote error: E_AUTH"), "{closed}");
    assert!(closed.contains("no_access_file"), "{closed}");
}
