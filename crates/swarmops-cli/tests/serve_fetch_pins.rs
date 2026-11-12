//! What `swarmops serve` sends a fetch that names an object by its id (the M17 design §5 step
//! 2): a commit a served ref reaches, and nothing else. A protocol v2 client may ask for any
//! id, and git's `upload-pack` checks none, so the pack-objects hook is the only check: a tree
//! or a blob of a served commit is refused `not_served`, over ssh and over smart HTTP alike.
//!
//! ```text
//!   empty client ── git fetch <url> <oid> ── ssh stand-in | HttpServer
//!       └── swarmops serve [--cgi] ── git upload-pack | http-backend
//!             └── serve --hook pack-objects: <oid> a commit? ── no ──▶ not_served, no pack
//! ```

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

use swarmops_testkit::git::fixture;
use swarmops_testkit::remote::{HttpAuth, HttpServer};
use swarmops_testkit::serve::{HttpIdentity, ServeSite};
use tempfile::TempDir;

use crate::support::{self, Swarmops};

/// The caller every connection is.
const AGENT: &str = "person/ada";

/// The agent that writes the served store directly, through the command line.
const WRITER: &str = "agent/worker-1";

/// The store the site serves.
const STORE: &str = "acme";

/// The served store's access file: [`AGENT`] reads.
const ACCESS: &str = r#"version = 1

[[grant]]
who = ["person/ada"]
read = true
"#;

/// The setting that asks for protocol v2, whose `upload-pack` checks no wanted id.
const PROTOCOL_V2: &str = "protocol.version=2";

/// The reason of the refusal, as the client's stderr holds it over ssh.
const NOT_SERVED: &str = "not_served";

/// How a client reaches the site.
#[derive(Clone, Copy, Debug)]
enum Transport {
    /// The ssh stand-in: `serve` runs `upload-pack`.
    Ssh,
    /// The loopback web server: `serve --cgi` runs `http-backend`.
    Http,
}

/// A served root with the `linear` fixture as [`STORE`]'s `main`, and a directory for clients.
struct Site {
    /// The root, its stand-ins and the client git.
    served: ServeSite,
    /// The web server, started for [`Transport::Http`].
    http: Option<HttpServer>,
    /// The clients' working directories.
    work: TempDir,
}

impl Site {
    /// The site, reached over `transport`.
    fn new(transport: Transport) -> Site {
        // 1. The store, imported before its access file is there, so that no preparation runs
        //    beside the test's connections.
        let served = ServeSite::new::<Swarmops>();
        let store = served.init(STORE);
        let linear = fixture("linear");
        let source = linear.repo.as_deref().unwrap().to_str().unwrap();
        let line = [
            "--repo",
            store.to_str().unwrap(),
            "--as",
            WRITER,
            "git",
            "import",
            source,
            "--ref",
            "HEAD",
            "--into",
            "main",
        ];
        support::run(&line, &[]).ok();
        fs::write(store.join("access.toml"), ACCESS).unwrap();

        // 2. The web server, when the clients come over HTTP.
        let http = match transport {
            Transport::Ssh => None,
            Transport::Http => {
                let identity = HttpIdentity::Fixed(AGENT.to_owned());
                Some(served.http(identity, HttpAuth::Anonymous))
            }
        };
        Site {
            served,
            http,
            work: TempDir::new().unwrap(),
        }
    }

    /// The store's URL.
    fn url(&self) -> String {
        match &self.http {
            Some(http) => http.url(STORE),
            None => self.served.ssh_url(STORE),
        }
    }

    /// git with `args` in `dir` as [`AGENT`], asking for protocol v2; what it did. Its stderr
    /// never holds the root.
    fn git_out(&self, dir: &Path, args: &[&str]) -> Output {
        let output = self
            .served
            .command()
            .env("GIT_SSH_COMMAND", self.served.ssh_command(AGENT))
            .current_dir(dir)
            .args(["-c", PROTOCOL_V2])
            .args(args)
            .output()
            .unwrap();
        let stderr = text(&output.stderr);
        let root = self.served.root();
        assert!(
            !stderr.contains(root.to_str().unwrap()),
            "a host path: {stderr}"
        );
        output
    }

    /// git with `args` in `dir`, which must succeed; its stdout, trimmed.
    fn git(&self, dir: &Path, args: &[&str]) -> String {
        let output = self.git_out(dir, args);
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            text(&output.stderr)
        );
        text(&output.stdout).trim().to_owned()
    }

    /// The store cloned into `name`; the clone's directory.
    fn clone(&self, name: &str) -> PathBuf {
        let url = self.url();
        self.git(self.work.path(), &["clone", "--quiet", &url, name]);
        self.work.path().join(name)
    }

    /// An empty repository `name`, which holds no object a fetch could leave out.
    fn empty(&self, name: &str) -> PathBuf {
        self.git(self.work.path(), &["init", "--quiet", name]);
        self.work.path().join(name)
    }

    /// Whether the repository `dir` holds the object `oid`.
    fn holds(&self, dir: &Path, oid: &str) -> bool {
        let checked = self.git_out(dir, &["cat-file", "-e", oid]);
        checked.status.success()
    }
}

/// `bytes` as text.
fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The ids a clone of `site` reads for the served `main`: its commit, that commit's tree and
/// one blob of the tree.
struct Served {
    commit: String,
    tree: String,
    blob: String,
}

impl Served {
    /// Read from a clone of `site`.
    fn of(site: &Site) -> Served {
        let clone = site.clone("whole");
        let commit = site.git(&clone, &["rev-parse", "--verify", "origin/main"]);
        let tree = site.git(&clone, &["rev-parse", "--verify", "origin/main^{tree}"]);

        // `<mode> blob <oid>\t<path>`: the first file of the tree.
        let files = site.git(&clone, &["ls-tree", "-r", "origin/main"]);
        let blob = files
            .lines()
            .filter_map(|line| line.split_once('\t'))
            .find_map(|(entry, _)| entry.strip_prefix("100644 blob "))
            .expect("the linear fixture holds a file")
            .to_owned();

        for (oid, kind) in [(&commit, "commit"), (&tree, "tree"), (&blob, "blob")] {
            assert_eq!(site.git(&clone, &["cat-file", "-t", oid]), kind);
        }
        Served { commit, tree, blob }
    }
}

/// A fetch of `oid` by its id into an empty repository of `site`; what git did, and whether the
/// repository holds the object after it.
fn fetch_by_id(site: &Site, name: &str, oid: &str) -> (Output, bool) {
    let dir = site.empty(name);
    let url = site.url();
    let fetched = site.git_out(&dir, &["fetch", "--quiet", &url, oid]);
    let held = site.holds(&dir, oid);
    (fetched, held)
}

/// The fetch of `oid`, an object of a served commit that is no commit, is refused and brings
/// nothing; the same fetch of the served commit works, so the id's kind is what refuses it.
fn assert_refused(site: &Site, served: &Served, oid: &str) -> String {
    // 1. The control: a fetch by id of the served commit brings it.
    let (fetched, held) = fetch_by_id(site, "by-commit", &served.commit);
    assert!(fetched.status.success(), "{}", text(&fetched.stderr));
    assert!(held, "the served commit was not sent");

    // 2. The object that is no commit: refused, not sent.
    let (fetched, held) = fetch_by_id(site, "by-object", oid);
    let stderr = text(&fetched.stderr);
    assert!(!fetched.status.success(), "{oid} was sent: {stderr}");
    assert!(!held, "{oid} was sent: {stderr}");
    stderr
}

/// A tree a served commit holds is not sent to a client that asks for it by id: the fetch is
/// refused `not_served`.
#[test]
fn serve_fetch_refuses_a_tree_want() {
    let site = Site::new(Transport::Ssh);
    let served = Served::of(&site);
    let stderr = assert_refused(&site, &served, &served.tree);
    assert!(stderr.contains(NOT_SERVED), "{stderr}");
}

/// A blob a served commit holds is not sent to a client that asks for it by id: the fetch is
/// refused `not_served`.
#[test]
fn serve_fetch_refuses_a_blob_want() {
    let site = Site::new(Transport::Ssh);
    let served = Served::of(&site);
    let stderr = assert_refused(&site, &served, &served.blob);
    assert!(stderr.contains(NOT_SERVED), "{stderr}");
}

/// `http-backend` answers a fetch through the same pack-objects hook as `upload-pack` over
/// ssh: a tree and a blob asked for by id over smart HTTP are refused and not sent. The hook's
/// reason stays in the web server's log, so the client reads git's failure alone.
#[test]
fn the_http_backend_arm_refuses_as_ssh_does() {
    let site = Site::new(Transport::Http);
    let served = Served::of(&site);
    for oid in [&served.tree, &served.blob] {
        assert_refused(&site, &served, oid);
    }
}
