//! `swarmops serve --cgi`, smart HTTP (M14 design §2.2), as stock git meets it through the test
//! kit's loopback web server, which runs `serve --cgi` once a request: criterion 1 over HTTP
//! (the ssh suites' read commands with protocol v2 and v0, and their push rules), criterion 2's
//! round trip, the caller named by `--as` or by the web server's user, a push to
//! `refs/land/main`, and the refusals a web client reads (403, 404, 400). No response body and
//! no `remote:` line names the served root.
//!
//! ```text
//!   client git ── http://127.0.0.1:<port>/acme.git ── HttpServer (one per caller)
//!       └── per request: swarmops serve --cgi --root <root> (--as <id> | --user-family person)
//!             └── git http-backend on <store>/git/serve.git ── proc-receive hook
//! ```

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Output;

use serde_json::{Value, json};
use swarmops_testkit::child::CHILD_TIMEOUT;
use swarmops_testkit::git::fixture;
use swarmops_testkit::remote::{HttpAuth, HttpServer};
use swarmops_testkit::serve::{HttpIdentity, ServeSite};
use tempfile::TempDir;

use crate::support::{self, Swarmops};

/// The caller who reads, pushes and lands.
const AGENT: &str = "person/ada";

/// A caller who pushes feature branches, without `force`.
const PUSHER: &str = "person/bob";

/// A caller who only reads.
const READER: &str = "person/eve";

/// A caller the access file names and grants nothing.
const NOBODY: &str = "person/dan";

/// The agent that writes the served store directly, through the command line.
const WRITER: &str = "agent/worker-1";

/// The store every site serves.
const STORE: &str = "acme";

/// The web server's user and password, for `--user-family person`.
const WEB_USER: &str = "ada";
const WEB_TOKEN: &str = "token";

/// The served store's access file: [`AGENT`] pushes `main`, the release and feature branches
/// and the tags, forces the feature branches and lands onto `main`; [`PUSHER`] pushes the
/// feature branches; [`READER`] reads; [`NOBODY`] is named and granted nothing.
const ACCESS: &str = r#"version = 1

[[grant]]
who = ["person/ada"]
push = ["main", "release/*", "feature/**", "tags/**"]
force = ["feature/**"]
land = ["main"]

[[grant]]
who = ["person/bob"]
push = ["feature/**"]

[[grant]]
who = ["person/eve"]
read = true

[[grant]]
who = ["person/dan"]
"#;

/// The branches of the `merges` fixture, each imported into the store ref of its name.
const MERGES_BRANCHES: [&str; 8] = [
    "main", "feature", "one", "two", "three", "side", "cross-a", "cross-b",
];

/// The `merges` fixture's lightweight tag, imported as `tags/light`.
const MERGES_TAG: &str = "light";

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

/// A served root with the store [`STORE`] under [`ACCESS`], one web server per caller, and a
/// directory for clients.
struct Site {
    /// The root and the client git.
    served: ServeSite,
    /// The clients' working directories.
    work: TempDir,
    /// The web servers started so far, by the caller their `--as` names.
    servers: RefCell<BTreeMap<String, HttpServer>>,
    /// The changes written so far, for the next one's name.
    changes: Cell<u32>,
}

impl Site {
    /// An empty store, served.
    fn new() -> Site {
        let served = ServeSite::new::<Swarmops>();
        served.init(STORE);
        fs::write(served.store(STORE).join("access.toml"), ACCESS).unwrap();
        Site {
            served,
            work: TempDir::new().unwrap(),
            servers: RefCell::new(BTreeMap::new()),
            changes: Cell::new(0),
        }
    }

    /// The `linear` fixture imported as `main`.
    fn with_linear(self) -> Site {
        let linear = fixture("linear");
        self.import(linear.repo.as_deref().unwrap(), "HEAD", "main");
        self
    }

    /// The store's path.
    fn store(&self) -> String {
        self.served.store(STORE).to_str().unwrap().to_owned()
    }

    /// The root's path, which nothing a client is sent may hold.
    fn root(&self) -> String {
        self.served.root().to_str().unwrap().to_owned()
    }

    /// The URL of [`STORE`] on the web server whose `serve --cgi` runs `--as who`, without web
    /// authentication.
    fn url(&self, who: &str) -> String {
        let mut servers = self.servers.borrow_mut();
        let server = servers.entry(who.to_owned()).or_insert_with(|| {
            let identity = HttpIdentity::Fixed(who.to_owned());
            self.served.http(identity, HttpAuth::Anonymous)
        });
        server.url(STORE)
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

    /// The `git import` of the repository at `source`'s `git_ref` into `into`.
    fn import(&self, source: &Path, git_ref: &str, into: &str) {
        let source = source.to_str().unwrap();
        self.so(&["git", "import", source, "--ref", git_ref, "--into", into]);
    }

    /// An agent's change of `path` landed on `main`, as [`WRITER`].
    fn land(&self, path: &str) {
        let n = self.changes.get();
        self.changes.set(n + 1);
        let changes = self.work.path().join(format!("change-{n}.json"));
        let list = json!([{"path": path, "op": "put", "content": {"inline": format!("{n}\n")}}]);
        fs::write(&changes, list.to_string()).unwrap();
        let changes = changes.to_str().unwrap();
        let line = [
            "commit",
            "-m",
            path,
            "--changes",
            changes,
            "--parent",
            "main",
        ];
        let commit = self.so(&line);
        let id = commit["changeset"].as_str().unwrap();
        self.so(&["land", id, "--target", "main"]);
    }

    /// The store's newest operation.
    fn newest_op(&self) -> Value {
        self.so(&["op", "log", "-n", "1"])["ops"][0].clone()
    }

    /// The names of the store's refs.
    fn ref_names(&self) -> Vec<String> {
        let listed = self.so(&["ref", "list"]);
        (listed["refs"].as_array().unwrap().iter())
            .map(|entry| entry["name"].as_str().unwrap().to_owned())
            .collect()
    }

    /// Every ref of `serve.git`, one `<oid> <name>` line each.
    fn served_refs(&self) -> String {
        let output = self
            .served
            .command()
            .arg("--git-dir")
            .arg(self.served.store(STORE).join("git/serve.git"))
            .args(["for-each-ref", "--format=%(objectname) %(refname)"])
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", text(&output.stderr));
        text(&output.stdout)
    }

    /// Whether `serve.git` has `name` at `oid`.
    fn serves(&self, oid: &str, name: &str) -> bool {
        self.served_refs()
            .lines()
            .any(|line| line == format!("{oid} {name}"))
    }

    /// The commit `serve.git` has for `name`.
    fn served(&self, name: &str) -> String {
        let refs = self.served_refs();
        let found = refs.lines().find_map(|line| {
            let (oid, listed) = line.split_once(' ')?;
            (listed == name).then(|| oid.to_owned())
        });
        found.unwrap_or_else(|| panic!("no {name} in: {refs}"))
    }

    /// The store's refs and `serve.git`'s, to see that nothing moved.
    fn refs(&self) -> (Value, String) {
        (
            self.so(&["ref", "list"])["refs"].clone(),
            self.served_refs(),
        )
    }

    /// git with `args` in `dir`, asking for `protocol`; what it did. Its stderr, the `remote:`
    /// lines among it, never holds the root.
    fn git_out(&self, dir: &Path, protocol: Protocol, args: &[&str]) -> Output {
        let output = self
            .served
            .command()
            .current_dir(dir)
            .args(["-c", protocol.setting()])
            .args(args)
            .output()
            .unwrap();
        let stderr = text(&output.stderr);
        assert!(!stderr.contains(&self.root()), "a host path: {stderr}");
        output
    }

    /// git with `args` in `dir`, which must succeed; its stdout.
    fn git(&self, dir: &Path, protocol: Protocol, args: &[&str]) -> String {
        let output = self.git_out(dir, protocol, args);
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            text(&output.stderr)
        );
        text(&output.stdout)
    }

    /// The store cloned into `name` from [`AGENT`]'s server with `protocol` and `flags`; the
    /// clone's directory.
    fn clone(&self, name: &str, protocol: Protocol, flags: &[&str]) -> PathBuf {
        let url = self.url(AGENT);
        let line = [&["clone", "--quiet"][..], flags, &[url.as_str(), name]].concat();
        self.git(self.work.path(), protocol, &line);
        self.work.path().join(name)
    }

    /// A commit of `path` in the clone `dir`, on its branch; its id.
    fn commit(&self, dir: &Path, path: &str) -> String {
        fs::write(dir.join(path), format!("{path}\n")).unwrap();
        self.git(dir, Protocol::V2, &["add", path]);
        self.git(dir, Protocol::V2, &["commit", "--quiet", "-m", path]);
        self.rev(dir, "HEAD")
    }

    /// The client's `rev-parse` of `rev` in `dir`.
    fn rev(&self, dir: &Path, rev: &str) -> String {
        let out = self.git(dir, Protocol::V2, &["rev-parse", "--verify", rev]);
        out.trim().to_owned()
    }

    /// The push of `specs` from `dir` as `who`; what it did.
    fn push_as(&self, who: &str, dir: &Path, specs: &[&str]) -> Output {
        let url = self.url(who);
        let line = [&["push", url.as_str()][..], specs].concat();
        self.git_out(dir, Protocol::V2, &line)
    }

    /// The push of `specs` from `dir` as [`AGENT`], which must succeed; its stderr.
    fn push(&self, dir: &Path, specs: &[&str]) -> String {
        let pushed = self.push_as(AGENT, dir, specs);
        let stderr = text(&pushed.stderr);
        assert!(pushed.status.success(), "push {specs:?}: {stderr}");
        stderr
    }
}

/// `bytes` as text.
fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// git's report of a push the hook refused: exit non-zero, `[remote rejected]`, and each of
/// `needles` in it.
fn assert_rejected(output: &Output, needles: &[&str]) {
    let stderr = text(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains("[remote rejected]"), "{stderr}");
    for needle in needles {
        assert!(stderr.contains(needle), "no {needle:?} in: {stderr}");
    }
}

/// A GET of `path` on `http` with no header beyond `Host`: the status and the body.
fn get(http: &HttpServer, path: &str) -> (u16, String) {
    // 1. The request, straight on the socket: git would normalise the path first.
    let mut stream = TcpStream::connect(http.host()).unwrap();
    stream.set_read_timeout(Some(CHILD_TIMEOUT)).unwrap();
    let raw = format!("GET {path} HTTP/1.1\r\nHost: {}\r\n\r\n", http.host());
    stream.write_all(raw.as_bytes()).unwrap();

    // 2. The answer, to its end.
    let mut answer = String::new();
    stream.read_to_string(&mut answer).unwrap();
    let status = answer
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("no status line: {answer}"));
    let body = answer.split_once("\r\n\r\n").map_or("", |(_, body)| body);
    (status, body.to_owned())
}

/// Every ref an `ls-remote` or `for-each-ref` lists, by name, `HEAD` aside.
fn listed(lines: &str) -> BTreeMap<String, String> {
    lines
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .filter(|(_, name)| *name != "HEAD")
        .map(|(oid, name)| (name.to_owned(), oid.to_owned()))
        .collect()
}

/// Criterion 2 over HTTP: a repository imported into the store comes back as it was, every
/// branch and the lightweight tag at the source's commits, with either protocol, and the clone
/// passes `git fsck --strict`.
#[test]
fn http_round_trips_an_imported_repository() {
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
    let url = site.url(AGENT);
    for protocol in Protocol::ALL {
        let advertised = site.git(site.work.path(), protocol, &["ls-remote", &url]);
        assert_eq!(listed(&advertised), expected, "{protocol:?}");

        let name = format!("round-{protocol:?}");
        let clone = site.clone(&name, protocol, &["--mirror"]);
        let format = "--format=%(objectname)\t%(refname)";
        let cloned = site.git(&clone, protocol, &["for-each-ref", format]);
        assert_eq!(listed(&cloned), expected, "{protocol:?}");
        site.git(&clone, protocol, &["fsck", "--strict", "--no-dangling"]);
    }
}

/// Criterion 1's read half over HTTP, with either protocol: a clone, a fetch after an agent
/// lands, `pull --rebase` and `pull --no-rebase` over a local commit, and a clone of depth 1.
#[test]
fn http_serves_clone_fetch_pull_and_depth() {
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
        let ff = ["merge", "--quiet", "--ff-only", "origin/main"];
        site.git(&clone, protocol, &ff);
        site.commit(&clone, &format!("rebased-{protocol:?}.txt"));
        site.land(&format!("under-rebase-{protocol:?}.txt"));
        site.git(&clone, protocol, &["pull", "--quiet", "--rebase"]);
        assert_eq!(site.rev(&clone, "HEAD~1"), site.served("refs/heads/main"));

        // 4. pull --no-rebase: a merge of the local commit and the store's main.
        site.commit(&clone, &format!("merged-{protocol:?}.txt"));
        site.land(&format!("under-merge-{protocol:?}.txt"));
        let merge = ["pull", "--quiet", "--no-rebase", "--no-edit"];
        site.git(&clone, protocol, &merge);
        assert_eq!(site.rev(&clone, "HEAD^2"), site.served("refs/heads/main"));

        // 5. A clone of depth 1: one commit, shallow.
        let name = format!("shallow-{protocol:?}");
        let shallow = site.clone(&name, protocol, &["--depth", "1"]);
        let count = site.git(&shallow, protocol, &["rev-list", "--count", "HEAD"]);
        assert_eq!(count.trim(), "1", "{protocol:?}");
        assert!(shallow.join(".git/shallow").is_file(), "{protocol:?}");
        assert_eq!(site.rev(&shallow, "HEAD"), site.served("refs/heads/main"));
    }
}

/// Criterion 1's push half over HTTP, without web authentication (`--as` names the caller, and
/// git's `http.receivepack` lets the push reach the hook): `main` fast-forwarded as the caller,
/// a FORMAT 2 branch and a lightweight tag created, a branch deleted with `force`.
#[test]
fn http_push_moves_and_creates() {
    let site = Site::new().with_linear();
    let work = site.clone("work", Protocol::V2, &[]);

    // 1. main, a fast-forward: moved by the caller --as names, byte for byte.
    let pushed = site.commit(&work, "pushed.txt");
    site.push(&work, &["main"]);
    let op = site.newest_op();
    let moved = &op["ref_moves"][0];
    assert_eq!(moved["name"], "main", "{op}");
    assert_eq!(moved["actor"], AGENT, "{op}");
    assert!(
        site.serves(&pushed, "refs/heads/main"),
        "{}",
        site.served_refs()
    );
    let fresh = site.clone("fresh", Protocol::V0, &[]);
    assert_eq!(site.rev(&fresh, "origin/main"), pushed);

    // 2. feature/x and the tag v1.4.0, created.
    let tip = site.commit(&work, "feature.txt");
    site.push(&work, &["HEAD:refs/heads/feature/x"]);
    site.git(&work, Protocol::V2, &["tag", "v1.4.0"]);
    site.push(&work, &["refs/tags/v1.4.0"]);
    let names = site.ref_names();
    for name in ["feature/x", "tags/v1.4.0"] {
        assert!(names.contains(&name.to_owned()), "{name}: {names:?}");
    }
    assert!(
        site.serves(&tip, "refs/heads/feature/x"),
        "{}",
        site.served_refs()
    );
    assert!(
        site.serves(&tip, "refs/tags/v1.4.0"),
        "{}",
        site.served_refs()
    );

    // 3. feature/x deleted: refused without force, deleted with it.
    let denied = site.push_as(PUSHER, &work, &[":refs/heads/feature/x"]);
    assert_rejected(&denied, &["E_POLICY_DENIED"]);
    site.push(&work, &[":refs/heads/feature/x"]);
    assert!(!site.ref_names().contains(&"feature/x".to_owned()));
    assert!(!site.served_refs().contains("refs/heads/feature/x"));
}

/// The push rules over HTTP, each refusal moving nothing: a tag moved, an annotated tag, `main`
/// deleted, `main` rewritten, a read-only caller (both refs refused by the access file alone),
/// and one push whose one ref is refused while the other moves.
#[test]
fn http_push_refusals() {
    let site = Site::new().with_linear();
    let work = site.clone("work", Protocol::V2, &[]);
    let tip = site.commit(&work, "tagged.txt");
    site.git(&work, Protocol::V2, &["tag", "v1.4.0"]);
    site.push(&work, &["refs/tags/v1.4.0"]);
    let before = site.refs();

    // 1. Tags: a moved one, an annotated one.
    site.commit(&work, "moved.txt");
    site.git(&work, Protocol::V2, &["tag", "--force", "v1.4.0"]);
    let moved = site.push_as(AGENT, &work, &["--force", "refs/tags/v1.4.0"]);
    assert_rejected(&moved, &["E_TAG_IMMUTABLE"]);
    let annotate = ["tag", "--annotate", "--message", "v2", "v2.0.0"];
    site.git(&work, Protocol::V2, &annotate);
    let annotated = site.push_as(AGENT, &work, &["refs/tags/v2.0.0"]);
    assert_rejected(&annotated, &["E_INVALID_ARG", "annotated tag"]);

    // 2. main: never deleted, never rewritten.
    site.git(
        &work,
        Protocol::V2,
        &["reset", "--quiet", "--hard", "origin/main"],
    );
    let deleted = site.push_as(AGENT, &work, &[":refs/heads/main"]);
    assert_rejected(&deleted, &["E_REF_PROTECTED"]);
    let rewrite = ["commit", "--quiet", "--amend", "--message", "rewritten"];
    site.git(&work, Protocol::V2, &rewrite);
    let forced = site.push_as(AGENT, &work, &["--force", "HEAD:refs/heads/main"]);
    assert_rejected(&forced, &["non-fast-forward", "refs/land/main"]);
    assert_eq!(site.refs(), before);

    // 3. A reader, with no web authentication, pushing fast-forwards: the hook ran, and the
    //    access file refused both.
    site.git(&work, Protocol::V2, &["reset", "--quiet", "--hard", &tip]);
    let next = site.commit(&work, "next.txt");
    let both = ["HEAD:refs/heads/main", "HEAD:refs/heads/feature/y"];
    let denied = site.push_as(READER, &work, &both);
    assert_rejected(&denied, &["E_POLICY_DENIED"]);
    let stderr = text(&denied.stderr);
    assert_eq!(stderr.matches("E_POLICY_DENIED").count(), 2, "{stderr}");
    assert_eq!(site.refs(), before);

    // 4. One push, one ref refused and the other moved.
    let mixed = site.push_as(AGENT, &work, &["HEAD:refs/heads/main", "HEAD:refs/notes/x"]);
    assert_rejected(&mixed, &["refs/notes/x", "is not served"]);
    assert!(
        site.serves(&next, "refs/heads/main"),
        "{}",
        site.served_refs()
    );
    assert!(!site.served_refs().contains("refs/notes/x"));
}

/// Criterion 3 over HTTP: a push whose old value is stale, because another clone pushed between
/// its advertisement and its commands, moves nothing and exits non-zero; the store verifies
/// clean.
#[test]
fn http_push_refuses_a_stale_old() {
    let site = Site::new().with_linear();
    let first = site.clone("first", Protocol::V2, &[]);
    let second = site.clone("second", Protocol::V2, &[]);
    let won = site.commit(&first, "first.txt");
    site.commit(&second, "second.txt");

    // 1. The second clone's pre-push hook, which runs after the advertisement, pushes the first
    //    clone's commit to main.
    let pre_push = second.join(".git/hooks/pre-push");
    let race = format!(
        "#!/bin/sh\nunset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_PREFIX\n\
         cd '{}' && exec git push --quiet origin main\n",
        first.to_str().unwrap()
    );
    fs::write(&pre_push, race).unwrap();
    fs::set_permissions(&pre_push, fs::Permissions::from_mode(0o755)).unwrap();

    // 2. The second push: stale, refused; main is the first clone's, and the store is whole.
    let stale = site.push_as(AGENT, &second, &["main"]);
    assert_rejected(&stale, &["stale: refs/heads/main moved; fetch first"]);
    assert!(
        site.serves(&won, "refs/heads/main"),
        "{}",
        site.served_refs()
    );
    site.so(&["verify", "--deep"]);
}

/// `--user-family person` behind Basic authentication: the web server's user `ada` pushes as
/// `person/ada`, the journal's actor.
#[test]
fn http_user_family_names_the_pusher() {
    let site = Site::new().with_linear();
    let auth = HttpAuth::Basic {
        user: WEB_USER.to_owned(),
        token: WEB_TOKEN.to_owned(),
    };
    let http = site
        .served
        .http(HttpIdentity::UserFamily("person".to_owned()), auth);
    let url = http
        .url(STORE)
        .replacen("http://", &format!("http://{WEB_USER}:{WEB_TOKEN}@"), 1);

    // 1. Cloned and pushed with the web server's credentials.
    let line = ["clone", "--quiet", url.as_str(), "web"];
    site.git(site.work.path(), Protocol::V2, &line);
    let work = site.work.path().join("web");
    let pushed = site.commit(&work, "web.txt");
    site.git(&work, Protocol::V2, &["push", "--quiet", "origin", "main"]);

    // 2. The move, by person/ada.
    let op = site.newest_op();
    let moved = &op["ref_moves"][0];
    assert_eq!(moved["actor"], AGENT, "{op}");
    assert!(
        site.serves(&pushed, "refs/heads/main"),
        "{}",
        site.served_refs()
    );
}

/// A push to `refs/land/main` over HTTP lands one changeset keyed by the push, and the client
/// reads which commit landed on a `remote:` line.
#[test]
fn http_land_push_lands() {
    let site = Site::new().with_linear();
    let work = site.clone("work", Protocol::V2, &[]);
    let before = site.so(&["show", "main"])["id"].clone();
    site.commit(&work, "landed.txt");

    let stderr = site.push(&work, &["HEAD:refs/land/main"]);
    let shown = site.so(&["show", "main"]);
    assert_eq!(shown["parents"][0], before, "{shown}");
    assert_eq!(shown["lander"]["agent"], AGENT, "{shown}");
    let served = site.served("refs/heads/main");
    let told = format!("remote: landed onto main as {served}: git fetch brings it in");
    assert!(stderr.contains(&told), "{stderr}");
}

/// Before git starts, a web client reads the refusal as a status and a `<CODE>: <message>`
/// body: no caller (403 `E_AUTH`), a caller without a read (403 `E_POLICY_DENIED`), a path that
/// is no smart HTTP request (404), a path out of the root (400 `E_INVALID_ARG`), a store that is
/// not there (404). No body names the root.
#[test]
fn http_refuses_before_git() {
    let site = Site::new().with_linear();
    let refs = format!("/{STORE}.git/info/refs?service=git-upload-pack");
    let root = site.root();
    let check = |http: &HttpServer, path: &str, status: u16, code: &str| {
        let (got, body) = get(http, path);
        assert_eq!(got, status, "{path}: {body}");
        assert!(body.starts_with(&format!("{code}: ")), "{path}: {body}");
        assert!(!body.contains(&root), "{path}: a host path: {body}");
    };

    // 1. --user-family with no REMOTE_USER and no --as: nobody, whatever the path.
    let nobody = site.served.http(
        HttpIdentity::UserFamily("person".to_owned()),
        HttpAuth::Anonymous,
    );
    check(&nobody, &refs, 403, "E_AUTH");
    check(&nobody, "/elsewhere", 403, "E_AUTH");
    let listed = site.git_out(
        site.work.path(),
        Protocol::V2,
        &["ls-remote", &nobody.url(STORE)],
    );
    assert!(!listed.status.success(), "{}", text(&listed.stderr));

    // 2. A caller the access file grants nothing.
    let url = site.url(NOBODY);
    let denied = site.git_out(site.work.path(), Protocol::V2, &["ls-remote", &url]);
    assert!(!denied.status.success(), "{}", text(&denied.stderr));
    let servers = site.servers.borrow();
    check(&servers[NOBODY], &refs, 403, "E_POLICY_DENIED");
    drop(servers);

    // 3. Paths: no smart HTTP request, out of the root, no such store.
    site.url(AGENT);
    let servers = site.servers.borrow();
    let http = &servers[AGENT];
    for path in [
        "/",
        "/acme.git",
        "/acme.git/HEAD",
        "/acme.git/objects/info/packs",
        "/acme.git/info/refs/x",
        "/acme/inner.git/info/refs",
    ] {
        check(http, path, 404, "E_NOT_A_REPO");
    }
    for path in ["/../x/info/refs", "/../x.git/info/refs", "/a%2Fb/info/refs"] {
        check(http, path, 400, "E_INVALID_ARG");
    }
    check(http, "/nowhere.git/info/refs", 404, "E_NOT_A_REPO");

    // 4. The same store, asked for properly: served.
    let (status, body) = get(http, &refs);
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("git-upload-pack"), "{body}");
}
