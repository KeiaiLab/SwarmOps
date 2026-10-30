//! The git remotes' test double (M13): a bare repository served by git's own server on loopback
//! HTTP and through an ssh stand-in, and the merge methods of a hosting service done with git's
//! plumbing, so that the remote tests never need the network. Written by the M13 plan's task C2.
//!
//! ```text
//!   test ── plain git or `swarmops` ──┬── http://127.0.0.1:<port>/<name>.git ── HttpServer
//!                                     │      one thread a connection ── git http-backend
//!                                     └── ssh://git@remote.invalid/<name>.git
//!                                            GIT_SSH_COMMAND = the stand-in ── git upload-pack
//!                                                                              git receive-pack
//!   GitServer: <temp>/repos/<name>.git, bare, served by both; commit_on, merge, decline and
//!   move_to act on them as people and the hosting service do.
//! ```
//!
//! - [`GitServer`]: a temporary root of bare repositories, made and moved by git in the pinned
//!   environment the git fixtures use (no system or global configuration, an empty `HOME`,
//!   fixed identities, and dates that tick 60 s a commit from `1700000000 +0000`).
//!   [`GitServer::new_sha256`] makes SHA-256 repositories.
//! - [`HttpServer`] ([`GitServer::http`]): git's smart HTTP on `127.0.0.1`, every request header
//!   passed to `git http-backend` as the CGI metavariable `HTTP_<NAME>` (RFC 3875 §4.1.18), so
//!   that git's gzipped fetch requests are inflated; [`HttpAuth::Basic`] asks for a password;
//!   [`HttpServer::stop`] refuses connections, [`HttpServer::cut_after`] ends a response early
//!   and [`HttpServer::flip_pack_trailer`] garbles the next pack's checksum;
//!   [`HttpServer::requests`] is what it was asked. The same request loop runs any CGI
//!   program instead, `swarmops serve --cgi` among them ([`crate::serve::ServeSite::http`]);
//!   a script with no header block is answered 500, one past [`CGI_TIMEOUT`] is killed.
//! - [`GitServer::ssh_command`]: the ssh stand-in, a value for `GIT_SSH_COMMAND`, which serves
//!   the root ([`SshMode::Serve`]) or fails as ssh does ([`SshMode::Deny`], [`SshMode::Down`]).
//! - [`GitServer::merge`] with a [`MergeMethod`]: the three ways a hosting service merges a
//!   branch; [`GitServer::decline`]: a branch whose pushes a hook refuses.
//!
//! Every test that uses it needs git 2.39 or later: [`GIT_ENV`] names it, else `git` on `PATH`;
//! without one, [`GitServer::new`] fails the test with a message that names [`GIT_ENV`] (the
//! remote tests test the product's path, which needs git). Each self-check in this module's
//! tests is a failure the double must produce, so that a test relying on it cannot pass
//! vacuously.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tempfile::TempDir;

use crate::child;

/// The absolute path of the git binary to run, a file named `git`; unset, `git` on `PATH`. The
/// git fixtures read the same variable (`git::GIT_ENV`).
pub const GIT_ENV: &str = "SWARMOPS_GIT";

/// The oldest git the remote tests run with: the product's floor.
pub const MIN_GIT: [u32; 2] = [2, 39];

/// The largest request body the HTTP server reads; a larger one is answered 413.
pub const MAX_BODY: usize = 256 << 20;

/// How long a CGI script may run for one request: the child runner's deadline. One still
/// running then is killed and the request answered 500.
pub const CGI_TIMEOUT: Duration = child::CHILD_TIMEOUT;

/// The host name in the ssh stand-in's URLs, which the stand-in drops.
pub const SSH_HOST: &str = "remote.invalid";

/// The environment every git the double runs gets, beside `PATH`, an empty `HOME` and the
/// caller's `TMPDIR`: the git fixtures' pinned environment.
const PINNED: [(&str, &str); 9] = [
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_TERMINAL_PROMPT", "0"),
    ("TZ", "UTC"),
    ("LC_ALL", "C"),
    ("GIT_AUTHOR_NAME", "Fixture Author"),
    ("GIT_AUTHOR_EMAIL", "author@fixture.invalid"),
    ("GIT_COMMITTER_NAME", "Fixture Committer"),
    ("GIT_COMMITTER_EMAIL", "committer@fixture.invalid"),
];

/// The committer of the commits the remote's side writes in [`GitServer::merge`]: the hosting
/// service, not the people whose commits it merges.
pub const REMOTE_NAME: &str = "Remote Host";

/// [`REMOTE_NAME`]'s email.
pub const REMOTE_EMAIL: &str = "host@remote.invalid";

/// The first commit date the double writes, in seconds; each commit is [`TICK`] later.
const EPOCH: u64 = 1_700_000_000;

/// The seconds between two commits the double writes.
const TICK: u64 = 60;

/// The width of a pkt-line's hex length prefix.
const PKT_PREFIX: usize = 4;

/// The side-band byte of pack data in a pkt-line payload (2 is progress, 3 an error).
const BAND_PACK: u8 = 1;

/// The bits [`HttpServer::flip_pack_trailer`] flips in the pack's last byte.
const FLIP: u8 = 0xff;

/// The default branch of every repository the double makes.
pub const DEFAULT_BRANCH: &str = "main";

/// The object format of the repositories a [`GitServer`] makes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ObjectFormat {
    /// SHA-1, git's default.
    Sha1,
    /// SHA-256 (git 2.29 and later).
    Sha256,
}

/// Whether [`HttpServer`] asks for a password.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HttpAuth {
    /// Every request is served.
    Anonymous,
    /// A request without `Authorization: Basic <user:token>` is answered 401 with
    /// `WWW-Authenticate`; one with it runs with `REMOTE_USER` set to `user`.
    Basic {
        /// The user name.
        user: String,
        /// The password, a token on a hosting service.
        token: String,
    },
}

/// What the ssh stand-in does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SshMode {
    /// Runs `git upload-pack` or `git receive-pack` on the repository the URL names.
    Serve,
    /// Prints `Permission denied (publickey).` and exits 255, as ssh does for a refused key.
    Deny,
    /// Prints a connection-refused line and exits 255, as ssh does for a host that is down.
    Down,
}

/// How [`GitServer::merge`] merges a branch, as a hosting service's pull requests do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeMethod {
    /// A merge commit with two parents, the target's head first.
    Commit,
    /// One commit on the target's head with the branch's tree and the given message.
    Squash(SquashMessage),
    /// Each commit of the branch written again on the target's head: the same author and
    /// message, the remote's committer and date.
    Rebase,
}

/// The message of a squash.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SquashMessage {
    /// The title, then each squashed commit's full message as a list item (`* <message>`),
    /// oldest first: the commits' trailers stay.
    CommitList,
    /// The title only: the commits' messages, their trailers among them, are gone.
    TitleOnly,
}

/// Whether [`HttpServer`] passes the request headers on as `HTTP_*` metavariables. Only the
/// self-check of the gzipped fetch turns it off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HeaderPassing {
    /// Every header but `Content-Type` and `Content-Length`, as RFC 3875 §4.1.18 says.
    Every,
    /// None: a server that drops them, which git's gzipped requests fail against.
    #[cfg(test)]
    Dropped,
}

/// A temporary root of bare repositories, served by [`GitServer::http`] and
/// [`GitServer::ssh_command`], and moved as a hosting service and its other writers move them.
#[derive(Debug)]
pub struct GitServer {
    /// `repos/` (the repositories), `home/` (git's empty `HOME`), `ssh-*/` (the stand-ins) and
    /// scratch directories.
    root: TempDir,
    /// The git binary: [`GIT_ENV`]'s, or `git`.
    git: PathBuf,
    /// git's `PATH`: the caller's, with [`GIT_ENV`]'s directory first.
    path: OsString,
    /// The object format of the repositories it makes.
    format: ObjectFormat,
    /// The commits written so far, for the next one's date.
    ticks: AtomicU64,
    /// The declined branches of each repository: `(repository, branch)` to the message.
    declines: Mutex<BTreeMap<(String, String), String>>,
}

impl GitServer {
    /// A server of SHA-1 repositories. Fails the test when git is missing or older than
    /// [`MIN_GIT`], naming [`GIT_ENV`].
    pub fn new() -> GitServer {
        GitServer::with_format(ObjectFormat::Sha1)
    }

    /// A server of SHA-256 repositories (`git init --bare --object-format=sha256`).
    pub fn new_sha256() -> GitServer {
        GitServer::with_format(ObjectFormat::Sha256)
    }

    /// A server whose repositories have `format`.
    fn with_format(format: ObjectFormat) -> GitServer {
        let (git, path) = git_from_env();
        let root = tempfile::tempdir().expect("create the server's root");

        // 1. The layout: the repositories and git's empty HOME.
        for dir in ["repos", "home"] {
            fs::create_dir(root.path().join(dir)).expect("create a server directory");
        }
        let server = GitServer {
            root,
            git,
            path,
            format,
            ticks: AtomicU64::new(0),
            declines: Mutex::new(BTreeMap::new()),
        };

        // 2. The git it runs is there and new enough, or the test fails naming GIT_ENV.
        server.check_version();
        server
    }

    /// Fails the test unless the git it runs is [`MIN_GIT`] or later.
    fn check_version(&self) {
        let output = self
            .command()
            .arg("--version")
            .stdin(Stdio::null())
            .output();
        let line = match output {
            Ok(output) if output.status.success() => text(&output.stdout),
            Ok(output) => panic!(
                "git --version failed ({}); set {GIT_ENV} to a git {}.{} or later",
                output.status, MIN_GIT[0], MIN_GIT[1],
            ),
            Err(error) => panic!(
                "no git to run ({error}): the remote tests need git {}.{} or later on PATH, or \
                 {GIT_ENV} naming one",
                MIN_GIT[0], MIN_GIT[1],
            ),
        };
        let release = version_of(&line);
        assert!(
            release.is_some_and(|release| release >= MIN_GIT),
            "{line}: the remote tests need git {}.{} or later; set {GIT_ENV} to one",
            MIN_GIT[0],
            MIN_GIT[1],
        );
    }

    /// A git command in the pinned environment the server's own gits run in: for a test's
    /// client side, so that no configuration of the host reaches it.
    pub fn command(&self) -> Command {
        let mut command = Command::new(&self.git);
        command
            .env_clear()
            .env("PATH", &self.path)
            .env("HOME", self.root.path().join("home"))
            .envs(PINNED);
        if let Some(tmpdir) = std::env::var_os("TMPDIR") {
            command.env("TMPDIR", tmpdir);
        }
        command
    }

    /// The git binary the server runs, for a test that starts git itself.
    pub fn git_program(&self) -> &Path {
        &self.git
    }

    /// The directory that holds the repositories, `<name>.git` each.
    pub fn repos(&self) -> PathBuf {
        self.root.path().join("repos")
    }

    /// The bare repository `name`'s directory (made or not).
    pub fn bare(&self, name: &str) -> PathBuf {
        self.repos().join(format!("{name}.git"))
    }

    /// Makes the empty bare repository `name` (its default branch [`DEFAULT_BRANCH`], pushes
    /// over HTTP allowed without a user: `http.receivepack=true`) and returns its directory.
    pub fn create(&self, name: &str) -> PathBuf {
        let bare = self.bare(name);

        // 1. git init, in the server's object format.
        let mut init = self.command();
        init.args(["init", "--quiet", "--bare"])
            .arg(format!("--initial-branch={DEFAULT_BRANCH}"));
        if self.format == ObjectFormat::Sha256 {
            init.arg("--object-format=sha256");
        }
        succeed(init.arg(&bare));

        // 2. Pushes over HTTP: git http-backend refuses one without REMOTE_USER otherwise.
        self.git_in(name, &["config", "http.receivepack", "true"]);
        bare
    }

    /// The commit `rev` names in repository `name`, if any.
    pub fn rev_parse(&self, name: &str, rev: &str) -> Option<String> {
        let output = child::run(
            self.at(name)
                .args(["rev-parse", "--verify", "--quiet"])
                .arg(format!("{rev}^{{commit}}")),
        );
        output.status.success().then(|| text(&output.stdout))
    }

    /// Writes a commit on `branch` of repository `name` with `message`: the branch's tree with
    /// each `(path, contents)` of `files` written over it (a root commit when the branch does
    /// not exist), as another person pushing would. Returns the commit.
    pub fn commit_on(
        &self,
        name: &str,
        branch: &str,
        message: &str,
        files: &[(&str, &str)],
    ) -> String {
        let parent = self.rev_parse(name, &format!("refs/heads/{branch}"));
        let scratch = self.scratch();
        let index = scratch.path().join("index");

        // 1. A scratch index holding the branch's tree.
        let mut read = self.at(name);
        read.env("GIT_INDEX_FILE", &index).arg("read-tree");
        match &parent {
            Some(parent) => read.arg(parent),
            None => read.arg("--empty"),
        };
        succeed(&mut read);

        // 2. Each file's blob, entered into it.
        for (path, contents) in files {
            let file = scratch.path().join("blob");
            fs::write(&file, contents).expect("write a blob's contents");
            let blob = self.out(name, &["hash-object", "-w", path_str(&file)]);
            let entry = format!("100644,{blob},{path}");
            succeed(self.at(name).env("GIT_INDEX_FILE", &index).args([
                "update-index",
                "--add",
                "--cacheinfo",
                &entry,
            ]));
        }

        // 3. Its tree, committed on the parent and moved to.
        let tree = text(
            &succeed(
                self.at(name)
                    .env("GIT_INDEX_FILE", &index)
                    .arg("write-tree"),
            )
            .stdout,
        );
        let parents: Vec<&str> = parent.iter().map(String::as_str).collect();
        let commit = self.commit_tree(name, &tree, &parents, message, Identity::Default);
        self.update(name, branch, &commit, parent.as_deref());
        commit
    }

    /// Merges `from` into `into` in repository `name` with `method`, as a hosting service merges
    /// a pull request, and moves `into` to the result, which it returns. A conflict fails the
    /// test.
    pub fn merge(&self, name: &str, method: MergeMethod, from: &str, into: &str) -> String {
        let base = self
            .rev_parse(name, &format!("refs/heads/{into}"))
            .unwrap_or_else(|| panic!("no branch {into} in {name}"));
        let head = self
            .rev_parse(name, &format!("refs/heads/{from}"))
            .unwrap_or_else(|| panic!("no branch {from} in {name}"));
        let merged = match method {
            MergeMethod::Commit => {
                let tree = self.merge_tree(name, &base, &head);
                let message = format!("Merge branch '{from}' into {into}");
                self.commit_tree(name, &tree, &[&base, &head], &message, Identity::Remote)
            }
            MergeMethod::Squash(kind) => {
                let tree = self.merge_tree(name, &base, &head);
                let message = self.squash_message(name, kind, from, &base, &head);
                self.commit_tree(name, &tree, &[&base], &message, Identity::Remote)
            }
            MergeMethod::Rebase => self.rebase(name, &base, &head),
        };
        self.update(name, into, &merged, Some(&base));
        merged
    }

    /// Makes repository `name` refuse every push to `branch`, printing `message`, as a
    /// `pre-receive` hook (a protected branch) does. The push fails whole.
    pub fn decline(&self, name: &str, branch: &str, message: &str) {
        let mut declines = lock(&self.declines);
        declines.insert((name.to_owned(), branch.to_owned()), message.to_owned());

        // The hook names each declined branch of this repository with its message.
        let mut script = String::from(
            "#!/bin/sh\nstatus=0\nwhile read -r old new ref; do\n  case \"$ref\" in\n",
        );
        for ((repo, declined), said) in declines.iter() {
            if repo != name {
                continue;
            }
            let _ = writeln!(
                script,
                "    {}) echo {} >&2; status=1 ;;",
                sh_quote(&format!("refs/heads/{declined}")),
                sh_quote(said),
            );
        }
        script.push_str("  esac\ndone\nexit $status\n");
        write_script(&self.bare(name).join("hooks").join("pre-receive"), &script);
    }

    /// Moves `branch` of repository `name` to `commit`, whatever it held, as another writer
    /// with force would.
    pub fn move_to(&self, name: &str, branch: &str, commit: &str) {
        self.git_in(
            name,
            &["update-ref", &format!("refs/heads/{branch}"), commit],
        );
    }

    /// Writes a commit on `branch` of repository `name` whose tree holds one entry twice, an
    /// error-level `git fsck` finding (`duplicateEntries`) that only `hash-object --literally`
    /// writes, and moves the branch to it. Returns the commit. SHA-1 repositories only.
    pub fn commit_fsck_error(&self, name: &str, branch: &str, message: &str) -> String {
        assert_eq!(self.format, ObjectFormat::Sha1, "a SHA-1 tree entry");
        let parent = self.rev_parse(name, &format!("refs/heads/{branch}"));
        let scratch = self.scratch();

        // 1. A blob, and a tree naming it twice by the same name.
        let file = scratch.path().join("blob");
        fs::write(&file, "twice\n").expect("write a blob's contents");
        let blob = self.out(name, &["hash-object", "-w", path_str(&file)]);
        let mut entry = b"100644 twice.txt\0".to_vec();
        entry.extend(hex_bytes(&blob));
        let tree_file = scratch.path().join("tree");
        fs::write(&tree_file, [entry.as_slice(), entry.as_slice()].concat())
            .expect("write the tree");
        let tree = self.out(
            name,
            &[
                "hash-object",
                "-t",
                "tree",
                "-w",
                "--literally",
                path_str(&tree_file),
            ],
        );

        // 2. Committed on the branch's tip and moved to.
        let parents: Vec<&str> = parent.iter().map(String::as_str).collect();
        let commit = self.commit_tree(name, &tree, &parents, message, Identity::Default);
        self.update(name, branch, &commit, parent.as_deref());
        commit
    }

    /// The ssh stand-in in `mode`: the value for `GIT_SSH_COMMAND`. Its URLs are
    /// [`GitServer::ssh_url`]'s.
    pub fn ssh_command(&self, mode: SshMode) -> String {
        let dir = self.root.path().join(match mode {
            SshMode::Serve => "ssh-serve",
            SshMode::Deny => "ssh-deny",
            SshMode::Down => "ssh-down",
        });

        // The file is named ssh, so that git takes it for OpenSSH and does not probe it.
        let program = dir.join("ssh");
        if !program.exists() {
            fs::create_dir_all(&dir).expect("create the stand-in's directory");
            write_script(&program, &self.ssh_script(mode));
        }
        sh_quote(path_str(&program))
    }

    /// The ssh URL of repository `name`: `ssh://git@remote.invalid/<name>.git`.
    pub fn ssh_url(&self, name: &str) -> String {
        format!("ssh://git@{SSH_HOST}/{name}.git")
    }

    /// Serves the repositories over git's smart HTTP on `127.0.0.1`, until the returned server
    /// is stopped or dropped. It serves while this server lives.
    pub fn http(&self, auth: HttpAuth) -> HttpServer {
        self.http_with(auth, HeaderPassing::Every)
    }

    /// [`GitServer::http`], passing the request headers on as `passing` says.
    fn http_with(&self, auth: HttpAuth, passing: HeaderPassing) -> HttpServer {
        let backend = Backend::GitHttpBackend {
            git: self.git.clone(),
            path: self.path.clone(),
            home: self.root.path().join("home"),
            repos: self.repos(),
        };
        HttpServer::start_with(backend, auth, passing)
    }

    /// git's `PATH`: the caller's, with [`GIT_ENV`]'s directory first, for a server of another
    /// program that runs this git.
    #[cfg(feature = "cli")]
    pub(crate) fn path_env(&self) -> &OsString {
        &self.path
    }

    /// The stand-in's script: skip ssh's options, drop the host, map the path under the
    /// repositories and run the service there; or fail as `mode` says.
    fn ssh_script(&self, mode: SshMode) -> String {
        match mode {
            SshMode::Deny => {
                "#!/bin/sh\necho 'git@remote.invalid: Permission denied (publickey).' >&2\nexit 255\n"
                    .to_owned()
            }
            SshMode::Down => {
                "#!/bin/sh\necho 'ssh: connect to host remote.invalid port 22: Connection refused' >&2\nexit 255\n"
                    .to_owned()
            }
            SshMode::Serve => format!(
                "#!/bin/sh\n\
                 # ssh's options: -o, -p, -l and -i take a value; -4 and -6 none.\n\
                 while [ $# -gt 0 ]; do\n\
                 \x20 case \"$1\" in\n\
                 \x20   -o|-p|-l|-i) shift 2 ;;\n\
                 \x20   -*) shift ;;\n\
                 \x20   *) break ;;\n\
                 \x20 esac\n\
                 done\n\
                 # The host goes; the command is git's quoted service and path.\n\
                 shift\n\
                 eval \"set -- $*\"\n\
                 service=${{1#git-}}\n\
                 path=${{2#/}}\n\
                 exec {git} \"$service\" {repos}/\"$path\"\n",
                git = sh_quote(path_str(&self.git)),
                repos = sh_quote(path_str(&self.repos())),
            ),
        }
    }

    /// The message of a squash of `head` onto `base`.
    fn squash_message(
        &self,
        name: &str,
        kind: SquashMessage,
        from: &str,
        base: &str,
        head: &str,
    ) -> String {
        let title = format!("{from} (squashed)");
        if kind == SquashMessage::TitleOnly {
            return title;
        }

        // Each commit's full message, oldest first, NUL-separated by git.
        let range = format!("{base}..{head}");
        let listed = succeed(
            self.at(name)
                .args(["log", "--reverse", "-z", "--format=%B", &range]),
        )
        .stdout;
        let mut message = title;
        for body in String::from_utf8_lossy(&listed).split('\0') {
            let body = body.trim();
            if body.is_empty() {
                continue;
            }
            let _ = write!(message, "\n\n* {body}");
        }
        message
    }

    /// Writes each commit of `base..head` again on `base`, as a hosting service's rebase does,
    /// in a scratch clone (a cherry-pick needs a work tree), and fetches the result. Returns the
    /// new head.
    fn rebase(&self, name: &str, base: &str, head: &str) -> String {
        let scratch = self.scratch();
        let work = scratch.path().join("work");

        // 1. A work tree at the base.
        succeed(
            self.command()
                .args(["clone", "--quiet", "--no-checkout"])
                .arg(self.bare(name))
                .arg(&work),
        );
        succeed(
            self.command()
                .arg("-C")
                .arg(&work)
                .args(["checkout", "--quiet", "--detach", base]),
        );

        // 2. Each commit picked onto it, the remote host committing.
        let range = format!("{base}..{head}");
        let date = self.next_date();
        succeed(
            self.command()
                .arg("-C")
                .arg(&work)
                .env("GIT_COMMITTER_NAME", REMOTE_NAME)
                .env("GIT_COMMITTER_EMAIL", REMOTE_EMAIL)
                .env("GIT_COMMITTER_DATE", &date)
                .args(["cherry-pick", "--allow-empty", &range]),
        );
        let picked = text(
            &succeed(
                self.command()
                    .arg("-C")
                    .arg(&work)
                    .args(["rev-parse", "HEAD"]),
            )
            .stdout,
        );

        // 3. The picked commits fetched into the bare repository (a fetch runs no hook).
        succeed(
            self.at(name)
                .arg("fetch")
                .arg("--quiet")
                .arg(&work)
                .arg("HEAD"),
        );
        picked
    }

    /// The tree of the merge of `ours` and `theirs`; a conflict fails the test.
    fn merge_tree(&self, name: &str, ours: &str, theirs: &str) -> String {
        let output = succeed(
            self.at(name)
                .args(["merge-tree", "--write-tree", ours, theirs]),
        );
        text(&output.stdout)
            .lines()
            .next()
            .expect("merge-tree prints the tree")
            .to_owned()
    }

    /// Writes a commit of `tree` on `parents` with `message`, at the next date, by `who`.
    fn commit_tree(
        &self,
        name: &str,
        tree: &str,
        parents: &[&str],
        message: &str,
        who: Identity,
    ) -> String {
        let date = self.next_date();
        let mut command = self.at(name);
        command
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .args(["commit-tree", tree, "-m", message]);
        for parent in parents {
            command.args(["-p", parent]);
        }
        if who == Identity::Remote {
            command
                .env("GIT_AUTHOR_NAME", REMOTE_NAME)
                .env("GIT_AUTHOR_EMAIL", REMOTE_EMAIL)
                .env("GIT_COMMITTER_NAME", REMOTE_NAME)
                .env("GIT_COMMITTER_EMAIL", REMOTE_EMAIL);
        }
        text(&succeed(&mut command).stdout)
    }

    /// Moves `branch` to `commit` if it still holds `old` (absent: if it does not exist).
    fn update(&self, name: &str, branch: &str, commit: &str, old: Option<&str>) {
        let zero = self.zero_oid();
        let old = old.unwrap_or(&zero);
        self.git_in(
            name,
            &["update-ref", &format!("refs/heads/{branch}"), commit, old],
        );
    }

    /// The all-zero object id of the server's format: "no ref" to `update-ref`.
    fn zero_oid(&self) -> String {
        let len = match self.format {
            ObjectFormat::Sha1 => 40,
            ObjectFormat::Sha256 => 64,
        };
        "0".repeat(len)
    }

    /// The next commit date, `<seconds> +0000`.
    fn next_date(&self) -> String {
        let tick = self.ticks.fetch_add(1, Ordering::Relaxed);
        format!("{} +0000", EPOCH + tick * TICK)
    }

    /// A git command on repository `name`.
    fn at(&self, name: &str) -> Command {
        let mut git_dir = OsString::from("--git-dir=");
        git_dir.push(self.bare(name));
        let mut command = self.command();
        command.arg(git_dir);
        command
    }

    /// Runs git with `args` on repository `name`; a failure fails the test.
    fn git_in(&self, name: &str, args: &[&str]) {
        succeed(self.at(name).args(args));
    }

    /// What git with `args` on repository `name` prints; a failure fails the test.
    fn out(&self, name: &str, args: &[&str]) -> String {
        text(&succeed(self.at(name).args(args)).stdout)
    }

    /// A scratch directory under the root, removed when dropped.
    fn scratch(&self) -> TempDir {
        tempfile::tempdir_in(self.root.path()).expect("create a scratch directory")
    }
}

impl Default for GitServer {
    fn default() -> GitServer {
        GitServer::new()
    }
}

/// Who writes a commit the remote's side makes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Identity {
    /// The pinned fixture author and committer: another person.
    Default,
    /// [`REMOTE_NAME`], the hosting service.
    Remote,
}

/// One request the HTTP server answered.
#[derive(Clone, Debug)]
pub struct Request {
    /// The method, `GET` or `POST`.
    method: String,
    /// The path, without the query.
    path: String,
    /// The headers, as sent.
    headers: Vec<(String, String)>,
    /// The status it was answered with.
    status: u16,
}

impl Request {
    /// The method, `GET` or `POST`.
    pub fn method(&self) -> &str {
        &self.method
    }

    /// The path, without the query.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The first header named `name` (case-insensitive), if sent.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// The status it was answered with.
    pub fn status(&self) -> u16 {
        self.status
    }
}

/// git's smart HTTP on loopback, from [`GitServer::http`]. Stopped when dropped.
#[derive(Debug)]
pub struct HttpServer {
    /// Where it listens.
    addr: SocketAddr,
    /// What its threads share.
    shared: Arc<Shared>,
    /// The accepting thread, until stopped.
    thread: Option<JoinHandle<()>>,
}

impl HttpServer {
    /// Serves on `127.0.0.1` by running `backend` once a request, until stopped or dropped.
    #[cfg(feature = "cli")]
    pub(crate) fn start(backend: Backend, auth: HttpAuth) -> HttpServer {
        HttpServer::start_with(backend, auth, HeaderPassing::Every)
    }

    /// [`HttpServer::start`], passing the request headers on as `passing` says.
    fn start_with(backend: Backend, auth: HttpAuth, passing: HeaderPassing) -> HttpServer {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listen on loopback");
        let addr = listener.local_addr().expect("the listener's address");
        let shared = Arc::new(Shared {
            backend,
            auth,
            passing,
            log: Mutex::new(Vec::new()),
            cut: Mutex::new(None),
            flip: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
        });
        let accepting = Arc::clone(&shared);
        let thread = thread::spawn(move || accept(&listener, &accepting));
        HttpServer {
            addr,
            shared,
            thread: Some(thread),
        }
    }

    /// The URL of repository `name`: `http://127.0.0.1:<port>/<name>.git`.
    pub fn url(&self, name: &str) -> String {
        format!("http://{}/{name}.git", self.addr)
    }

    /// `127.0.0.1:<port>`.
    pub fn host(&self) -> String {
        self.addr.to_string()
    }

    /// Closes the listener: from now on a connection is refused. Requests under way end.
    pub fn stop(&mut self) {
        let Some(thread) = self.thread.take() else {
            return;
        };

        // Wake the accepting thread, which then drops the listener.
        self.shared.stopped.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr);
        thread.join().expect("the accepting thread");
    }

    /// Closes the next response longer than `bytes` after its first `bytes` bytes (status line
    /// and headers included): the client meets an early end of file. Shorter responses before
    /// it go whole; one cut, the server serves whole again.
    pub fn cut_after(&self, bytes: usize) {
        *lock(&self.shared.cut) = Some(bytes);
    }

    /// Garbles the next pack a `git-upload-pack` answer carries: the last byte of the last
    /// pack-data packet (side-band 1), which ends the pack's trailing checksum, is flipped, so
    /// git's receive fails on a checksum mismatch. Progress packets (bands 2 and 3) are left
    /// alone; one pack garbled, the server answers whole again.
    pub fn flip_pack_trailer(&self) {
        self.shared.flip.store(true, Ordering::SeqCst);
    }

    /// The requests answered so far, in order.
    pub fn requests(&self) -> Vec<Request> {
        lock(&self.shared.log).clone()
    }

    /// The `Authorization` headers sent so far, in order: for the credential tests.
    pub fn authorizations(&self) -> Vec<String> {
        self.requests()
            .iter()
            .filter_map(|request| request.header("Authorization").map(str::to_owned))
            .collect()
    }
}

impl Drop for HttpServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// What the HTTP server's threads share.
#[derive(Debug)]
struct Shared {
    /// The CGI script each request runs.
    backend: Backend,
    /// Whether it asks for a password.
    auth: HttpAuth,
    /// Whether the headers become `HTTP_*`.
    passing: HeaderPassing,
    /// The requests answered.
    log: Mutex<Vec<Request>>,
    /// The armed cut, in bytes.
    cut: Mutex<Option<usize>>,
    /// Set by [`HttpServer::flip_pack_trailer`], cleared once a pack is garbled.
    flip: AtomicBool,
    /// Set by [`HttpServer::stop`].
    stopped: AtomicBool,
}

/// The CGI script the HTTP server runs once a request, with the request's metavariables.
#[derive(Debug)]
pub(crate) enum Backend {
    /// `git http-backend` over a root of bare repositories ([`GitServer::http`]).
    GitHttpBackend {
        /// The git binary.
        git: PathBuf,
        /// Its `PATH`.
        path: OsString,
        /// Its empty `HOME`.
        home: PathBuf,
        /// `GIT_PROJECT_ROOT`.
        repos: PathBuf,
    },
    /// Any program, as a web server's CGI entry runs it: `swarmops serve --cgi …`
    /// ([`crate::serve::ServeSite::http`]).
    #[cfg(feature = "cli")]
    Cgi {
        /// The program.
        program: PathBuf,
        /// Its arguments.
        args: Vec<OsString>,
        /// Its environment beside the metavariables: nothing else of this process's reaches
        /// it.
        env: Vec<(OsString, OsString)>,
    },
}

impl Backend {
    /// The backend's command for `incoming`, its environment cleared but for what the backend
    /// needs.
    fn command(&self, incoming: &Incoming) -> Command {
        match self {
            Backend::GitHttpBackend {
                git,
                path,
                home,
                repos,
            } => {
                let mut command = Command::new(git);
                command
                    .arg("http-backend")
                    .env_clear()
                    .env("PATH", path)
                    .env("HOME", home)
                    .envs(PINNED)
                    .env("GIT_PROJECT_ROOT", repos)
                    .env("GIT_HTTP_EXPORT_ALL", "1");
                if let Some(protocol) = incoming.header("Git-Protocol") {
                    command.env("GIT_PROTOCOL", protocol);
                }
                command
            }
            #[cfg(feature = "cli")]
            Backend::Cgi { program, args, env } => {
                // A web server passes the protocol as `HTTP_GIT_PROTOCOL` only.
                let mut command = Command::new(program);
                command
                    .args(args)
                    .env_clear()
                    .envs(env.iter().map(|(name, value)| (name, value)));
                command
            }
        }
    }
}

/// A request as read off the connection.
struct Incoming {
    /// The method.
    method: String,
    /// The path, without the query.
    path: String,
    /// The query, without `?`.
    query: String,
    /// The headers.
    headers: Vec<(String, String)>,
    /// The body, de-chunked.
    body: Vec<u8>,
}

impl Incoming {
    /// The first header named `name`, if sent.
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// An answer: status, headers and body.
struct Answer {
    /// The status code.
    status: u16,
    /// The status line's reason.
    reason: String,
    /// The headers, without `Content-Length` and `Connection`.
    headers: Vec<(String, String)>,
    /// The body.
    body: Vec<u8>,
}

impl Answer {
    /// A body-less answer with `status`.
    fn bare(status: u16, reason: &str) -> Answer {
        Answer {
            status,
            reason: reason.to_owned(),
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    /// The bytes on the wire, `Connection: close`.
    fn bytes(&self) -> Vec<u8> {
        let mut head = format!("HTTP/1.1 {} {}\r\n", self.status, self.reason);
        for (name, value) in &self.headers {
            let _ = write!(head, "{name}: {value}\r\n");
        }
        let _ = write!(
            head,
            "Content-Length: {}\r\nConnection: close\r\n\r\n",
            self.body.len()
        );
        let mut bytes = head.into_bytes();
        bytes.extend_from_slice(&self.body);
        bytes
    }
}

/// Accepts connections until stopped, each served on a thread of its own.
fn accept(listener: &TcpListener, shared: &Arc<Shared>) {
    for stream in listener.incoming() {
        if shared.stopped.load(Ordering::SeqCst) {
            return;
        }
        let Ok(stream) = stream else {
            continue;
        };
        let shared = Arc::clone(shared);
        thread::spawn(move || {
            // A client that went away mid-request is no failure of the server.
            let _ = serve(stream, &shared);
        });
    }
}

/// Reads one request, answers it and closes the connection.
fn serve(stream: TcpStream, shared: &Shared) -> io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;

    // 1. The request, its body bounded.
    let Some(incoming) = read_request(&mut reader, &mut writer)? else {
        return write_answer(
            &mut writer,
            shared,
            &Answer::bare(413, "Payload Too Large"),
            None,
        );
    };

    // 2. The password, when asked for; then git http-backend.
    let mut answer = match authenticate(&shared.auth, &incoming) {
        Err(answer) => answer,
        Ok(user) => run_backend(&shared.backend, shared.passing, &incoming, user.as_deref())?,
    };
    if incoming.path.ends_with("/git-upload-pack") {
        garble_pack(shared, &mut answer.body);
    }

    // 3. The answer, cut when armed; the request logged.
    let logged = Request {
        method: incoming.method.clone(),
        path: incoming.path.clone(),
        headers: incoming.headers.clone(),
        status: answer.status,
    };
    write_answer(&mut writer, shared, &answer, Some(logged))
}

/// Flips the last byte of the pack in `body`, an upload-pack answer, when a flip is armed and
/// the answer carries pack data; the flip is then spent.
fn garble_pack(shared: &Shared, body: &mut [u8]) {
    if !shared.flip.load(Ordering::SeqCst) {
        return;
    }
    let Some(at) = last_pack_byte(body) else {
        return;
    };
    if shared.flip.swap(false, Ordering::SeqCst) {
        body[at] ^= FLIP;
    }
}

/// The offset of the last byte of the last pack-data packet (side-band 1) of `body`, a pkt-line
/// stream; `None` without one. The last such packet ends the pack's checksum; the packets after
/// it, if any, are progress or errors (bands 2 and 3) and flush packets.
fn last_pack_byte(body: &[u8]) -> Option<usize> {
    let mut last = None;
    let mut at = 0;
    while let Some(prefix) = body.get(at..at + PKT_PREFIX) {
        let size = usize::from_str_radix(std::str::from_utf8(prefix).ok()?, 16).ok()?;

        // 1. Flush, delimiter and response-end packets carry no payload.
        if size < PKT_PREFIX {
            at += PKT_PREFIX;
            continue;
        }

        // 2. A data packet; a pack-data one starts with band 1 and holds at least a byte.
        let end = at + size;
        let payload = body.get(at + PKT_PREFIX..end)?;
        if payload.len() > 1 && payload[0] == BAND_PACK {
            last = Some(end - 1);
        }
        at = end;
    }
    last
}

/// Writes `answer`, cut short when a cut is armed and it is longer, and logs `logged`.
fn write_answer(
    writer: &mut TcpStream,
    shared: &Shared,
    answer: &Answer,
    logged: Option<Request>,
) -> io::Result<()> {
    if let Some(logged) = logged {
        lock(&shared.log).push(logged);
    }
    let bytes = answer.bytes();

    // A cut takes the first response longer than it, once.
    let cut = {
        let mut armed = lock(&shared.cut);
        match *armed {
            Some(limit) if bytes.len() > limit => armed.take(),
            _ => None,
        }
    };
    let sent = cut.map_or(bytes.as_slice(), |limit| &bytes[..limit]);
    writer.write_all(sent)?;
    writer.flush()?;
    writer.shutdown(std::net::Shutdown::Both)
}

/// The request on `reader`: its line, headers and body (by `Content-Length` or chunked). `None`
/// for a body over [`MAX_BODY`]. Answers `Expect: 100-continue` on `writer`.
fn read_request(
    reader: &mut BufReader<TcpStream>,
    writer: &mut TcpStream,
) -> io::Result<Option<Incoming>> {
    // 1. The request line: method, target, version.
    let line = read_line(reader)?;
    let mut parts = line.split(' ');
    let method = parts.next().unwrap_or_default().to_owned();
    let target = parts.next().unwrap_or_default();
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let (path, query) = (path.to_owned(), query.to_owned());

    // 2. The headers, to the empty line.
    let mut headers = Vec::new();
    loop {
        let line = read_line(reader)?;
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_owned(), value.trim().to_owned()));
        }
    }
    let mut incoming = Incoming {
        method,
        path,
        query,
        headers,
        body: Vec::new(),
    };

    // 3. The body: a client that waits for 100 is told to go on.
    if incoming
        .header("Expect")
        .is_some_and(|value| value.eq_ignore_ascii_case("100-continue"))
    {
        writer.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
    }
    let chunked = incoming
        .header("Transfer-Encoding")
        .is_some_and(|value| value.eq_ignore_ascii_case("chunked"));
    let body = if chunked {
        read_chunked(reader)?
    } else {
        let length = incoming
            .header("Content-Length")
            .map_or(Ok(0), str::parse::<usize>)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "a bad Content-Length"))?;
        read_sized(reader, length)?
    };
    let Some(body) = body else {
        return Ok(None);
    };
    incoming.body = body;
    Ok(Some(incoming))
}

/// `length` bytes, or `None` over [`MAX_BODY`].
fn read_sized(reader: &mut BufReader<TcpStream>, length: usize) -> io::Result<Option<Vec<u8>>> {
    if length > MAX_BODY {
        return Ok(None);
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    Ok(Some(body))
}

/// A chunked body, de-chunked, or `None` over [`MAX_BODY`].
fn read_chunked(reader: &mut BufReader<TcpStream>) -> io::Result<Option<Vec<u8>>> {
    let mut body = Vec::new();
    loop {
        // Each chunk: its size in hex (extensions after `;`), the bytes, CRLF.
        let line = read_line(reader)?;
        let size = line.split(';').next().unwrap_or_default().trim();
        let size = usize::from_str_radix(size, 16)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "a bad chunk size"))?;
        if size == 0 {
            break;
        }
        if body.len() + size > MAX_BODY {
            return Ok(None);
        }
        let start = body.len();
        body.resize(start + size, 0);
        reader.read_exact(&mut body[start..])?;
        read_line(reader)?;
    }

    // The trailer, to the empty line.
    while !read_line(reader)?.is_empty() {}
    Ok(Some(body))
}

/// One line, without its CRLF; an end of file is an error.
fn read_line(reader: &mut BufReader<TcpStream>) -> io::Result<String> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(line.trim_end_matches(['\r', '\n']).to_owned())
}

/// The user a request authenticates as (`None` when no password is asked), or the 401 answer.
fn authenticate(auth: &HttpAuth, incoming: &Incoming) -> Result<Option<String>, Answer> {
    let HttpAuth::Basic { user, token } = auth else {
        return Ok(None);
    };
    let expected = format!("Basic {}", base64(format!("{user}:{token}").as_bytes()));
    if incoming.header("Authorization") == Some(expected.as_str()) {
        return Ok(Some(user.clone()));
    }
    let mut answer = Answer::bare(401, "Unauthorized");
    answer.headers.push((
        "WWW-Authenticate".to_owned(),
        "Basic realm=\"git\"".to_owned(),
    ));
    Err(answer)
}

/// Runs the backend for `incoming` as a CGI script, within [`CGI_TIMEOUT`], and its output as
/// the answer.
fn run_backend(
    backend: &Backend,
    passing: HeaderPassing,
    incoming: &Incoming,
    user: Option<&str>,
) -> io::Result<Answer> {
    let mut command = backend.command(incoming);
    command
        .env("GATEWAY_INTERFACE", "CGI/1.1")
        .env("SERVER_PROTOCOL", "HTTP/1.1")
        .env("REQUEST_METHOD", &incoming.method)
        .env("PATH_INFO", &incoming.path)
        .env("QUERY_STRING", &incoming.query)
        .env("CONTENT_LENGTH", incoming.body.len().to_string());

    // 1. The CGI metavariables of the request.
    if let Some(kind) = incoming.header("Content-Type") {
        command.env("CONTENT_TYPE", kind);
    }
    if let Some(user) = user {
        command.env("REMOTE_USER", user);
    }
    if passing == HeaderPassing::Every {
        for (name, value) in &incoming.headers {
            if let Some(variable) = metavariable(name) {
                command.env(variable, value);
            }
        }
    }

    // 2. The body on stdin from a thread of its own, while the child runner reads stdout.
    let mut spawned = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = spawned.stdin.take().expect("a piped stdin");
    let body = incoming.body.clone();
    let feeder = thread::spawn(move || {
        // The backend may stop reading early; what it answers still counts.
        let _ = stdin.write_all(&body);
    });

    // 3. A script past its deadline is killed: the request is answered, never left hanging.
    let (output, killed) = child::Child::adopt(spawned).finish(Instant::now() + CGI_TIMEOUT);
    let _ = feeder.join();
    if killed {
        let mut answer = Answer::bare(500, "Internal Server Error");
        answer.body = b"the CGI script ran past its deadline".to_vec();
        return Ok(answer);
    }
    Ok(cgi_answer(&output))
}

/// `HTTP_<NAME>` for header `name`, as RFC 3875 §4.1.18 makes it; `None` for the two headers
/// that are metavariables of their own.
fn metavariable(name: &str) -> Option<String> {
    if name.eq_ignore_ascii_case("Content-Type") || name.eq_ignore_ascii_case("Content-Length") {
        return None;
    }
    Some(format!(
        "HTTP_{}",
        name.to_ascii_uppercase().replace('-', "_")
    ))
}

/// A CGI script's output as an answer: its headers, `Status:` its status (200 without one).
/// An output without a header block (RFC 3875 §6.2: headers, then an empty line) is a failed
/// script, answered 500 with its stderr as the body.
fn cgi_answer(output: &Output) -> Answer {
    let Some((head, body)) = split_head(&output.stdout) else {
        let mut answer = Answer::bare(500, "Internal Server Error");
        answer.body.clone_from(&output.stderr);
        return answer;
    };
    let mut answer = Answer::bare(200, "OK");
    answer.body = body.to_vec();
    for line in String::from_utf8_lossy(head).lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if !name.eq_ignore_ascii_case("Status") {
            answer
                .headers
                .push((name.trim().to_owned(), value.to_owned()));
            continue;
        }
        let (code, reason) = value.split_once(' ').unwrap_or((value, ""));
        answer.status = code.parse().unwrap_or(500);
        reason.clone_into(&mut answer.reason);
    }
    answer
}

/// A CGI output's headers and body, split at the first empty line (CRLF or LF); `None` without
/// one.
fn split_head(bytes: &[u8]) -> Option<(&[u8], &[u8])> {
    for (separator, len) in [(&b"\r\n\r\n"[..], 4), (&b"\n\n"[..], 2)] {
        if let Some(at) = bytes.windows(len).position(|window| window == separator) {
            return Some((&bytes[..at], &bytes[at + len..]));
        }
    }
    None
}

/// `bytes` in standard base64 with padding, for the `Authorization` header.
pub(crate) fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let triple = chunk.iter().enumerate().fold(0u32, |acc, (at, byte)| {
            acc | u32::from(*byte) << (16 - 8 * at)
        });

        // Four sextets, the ones past the input as `=`.
        for at in 0..4 {
            if at > chunk.len() {
                out.push('=');
                continue;
            }
            let sextet = (triple >> (18 - 6 * at)) & 0x3f;
            out.push(char::from(ALPHABET[sextet as usize]));
        }
    }
    out
}

/// The git [`GIT_ENV`] names, or `git` on `PATH`, and the `PATH` it runs with.
fn git_from_env() -> (PathBuf, OsString) {
    let inherited = std::env::var_os("PATH").unwrap_or_else(|| OsString::from("/usr/bin:/bin"));
    let Some(program) = std::env::var_os(GIT_ENV) else {
        return (PathBuf::from("git"), inherited);
    };
    let program = PathBuf::from(program);
    assert!(
        program.is_absolute() && program.file_name().is_some_and(|name| name == "git"),
        "{GIT_ENV} is the absolute path of a file named git: {}",
        program.display(),
    );

    // Its directory first, so that the gits git starts are the same.
    let first = program.parent().expect("an absolute path has a parent");
    let dirs = std::iter::once(first.to_path_buf()).chain(std::env::split_paths(&inherited));
    let path = std::env::join_paths(dirs).expect("join PATH");
    (program, path)
}

/// The `[major, minor]` of a `git version X.Y.Z…` line.
fn version_of(line: &str) -> Option<[u32; 2]> {
    let release = line.strip_prefix("git version ")?;
    let mut numbers = release.split('.').map(str::parse::<u32>);
    let major = numbers.next()?.ok()?;
    let minor = numbers.next()?.ok()?;
    Some([major, minor])
}

/// Runs `command` to its end; one that fails fails the test with what it printed.
fn succeed(command: &mut Command) -> Output {
    let output = child::run(command);
    assert!(
        output.status.success(),
        "{command:?}: {}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim_end(),
    );
    output
}

/// `bytes`, which git printed as text, without the line feeds that end it.
fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .trim_end_matches('\n')
        .to_owned()
}

/// `path` as UTF-8: the temporary directories' paths are.
pub(crate) fn path_str(path: &Path) -> &str {
    path.to_str().expect("a UTF-8 temporary path")
}

/// The bytes the hex text `hex` (an object id git printed) spells.
fn hex_bytes(hex: &str) -> Vec<u8> {
    let digits = hex.as_bytes().chunks(2);
    digits
        .map(|pair| {
            let pair = std::str::from_utf8(pair).expect("ASCII hex");
            u8::from_str_radix(pair, 16).expect("a hex byte")
        })
        .collect()
}

/// `value` quoted for `sh`.
pub(crate) fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// Writes `script` to `path`, executable.
///
/// A child `sh` writes it, not this process: a descriptor open here for writing is copied
/// into every child another test thread forks meanwhile, and running the script before
/// that child execs fails with ETXTBSY ("Text file busy").
pub(crate) fn write_script(path: &Path, script: &str) {
    let status = Command::new("sh")
        .args(["-c", r#"printf '%s' "$2" > "$1" && chmod 755 "$1""#, "sh"])
        .arg(path)
        .arg(script)
        .status()
        .expect("start sh to write a script");
    assert!(status.success(), "write {}: {status}", path.display());
}

/// `mutex`'s guard, a poisoned one taken as is: a test thread's panic is reported on its own.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;
    use std::net::TcpStream;
    use std::path::Path;
    use std::process::Output;

    use super::{
        GitServer, HeaderPassing, HttpAuth, MergeMethod, REMOTE_NAME, SquashMessage, SshMode,
        base64, text, write_script,
    };
    use crate::child;

    /// The repository every test serves.
    const REPO: &str = "repo";

    /// Threads that write and run scripts at once in the script race test.
    const SCRIPT_THREADS: usize = 16;

    /// Scripts each of those threads writes and runs.
    const SCRIPTS_PER_THREAD: usize = 50;

    /// Branches in the gzipped fetch's repository: enough wants and haves for git to gzip the
    /// request (above about 1 KiB).
    const BRANCHES: usize = 40;

    /// Runs a client git in `dir` (or anywhere) with `args`.
    fn client(
        server: &GitServer,
        dir: Option<&Path>,
        env: &[(&str, &str)],
        args: &[&str],
    ) -> Output {
        let mut command = server.command();
        if let Some(dir) = dir {
            command.arg("-C").arg(dir);
        }
        command.envs(env.iter().copied()).args(args);
        child::run(&mut command)
    }

    /// `output` succeeded; else the test fails with its stderr.
    fn ok(output: &Output) -> String {
        assert!(
            output.status.success(),
            "git failed: {}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr),
        );
        text(&output.stdout)
    }

    /// `output` failed; its stderr.
    fn failed(output: &Output) -> String {
        assert!(!output.status.success(), "git succeeded where it must fail");
        String::from_utf8_lossy(&output.stderr).into_owned()
    }

    /// A server with [`REPO`] holding one commit on main.
    fn served() -> GitServer {
        let server = GitServer::new();
        server.create(REPO);
        server.commit_on(REPO, "main", "first", &[("a.txt", "a\n")]);
        server
    }

    /// Clones `url` into a temp dir, commits a file, pushes main back; the server's main is the
    /// pushed commit.
    fn clone_and_push(server: &GitServer, url: &str, env: &[(&str, &str)]) {
        let work = tempfile::tempdir().expect("a temp dir");
        let dir = work.path().join("clone");
        let dir_str = dir.to_str().expect("UTF-8");

        // 1. Clone: the server's head arrives.
        ok(&client(
            server,
            None,
            env,
            &["clone", "--quiet", url, dir_str],
        ));
        let cloned = ok(&client(server, Some(&dir), env, &["rev-parse", "HEAD"]));
        assert_eq!(Some(cloned), server.rev_parse(REPO, "main"));

        // 2. A commit pushed back moves the server's main.
        std::fs::write(dir.join("b.txt"), "b\n").expect("write a file");
        ok(&client(server, Some(&dir), env, &["add", "b.txt"]));
        ok(&client(
            server,
            Some(&dir),
            env,
            &["commit", "--quiet", "-m", "second"],
        ));
        ok(&client(
            server,
            Some(&dir),
            env,
            &["push", "--quiet", "origin", "main"],
        ));
        let pushed = ok(&client(server, Some(&dir), env, &["rev-parse", "HEAD"]));
        assert_eq!(Some(pushed), server.rev_parse(REPO, "main"));
    }

    #[test]
    fn plain_git_clones_and_pushes_over_http() {
        let server = served();
        let http = server.http(HttpAuth::Anonymous);
        clone_and_push(&server, &http.url(REPO), &[]);

        // The smart protocol ran: a POST to each service.
        let posts: Vec<String> = http
            .requests()
            .iter()
            .filter(|request| request.method() == "POST")
            .map(|request| request.path().to_owned())
            .collect();
        assert!(
            posts.iter().any(|path| path.ends_with("/git-upload-pack")),
            "{posts:?}"
        );
        assert!(
            posts.iter().any(|path| path.ends_with("/git-receive-pack")),
            "{posts:?}"
        );
    }

    #[test]
    fn plain_git_clones_and_pushes_over_ssh() {
        let server = served();
        let ssh = server.ssh_command(SshMode::Serve);
        clone_and_push(&server, &server.ssh_url(REPO), &[("GIT_SSH_COMMAND", &ssh)]);
    }

    #[test]
    fn basic_auth_is_required_when_asked() {
        let server = served();
        let auth = HttpAuth::Basic {
            user: "user".to_owned(),
            token: "token".to_owned(),
        };
        let http = server.http(auth);

        // 1. Without credentials: 401, and git fails.
        let refused = client(&server, None, &[], &["ls-remote", &http.url(REPO)]);
        failed(&refused);
        assert!(
            http.requests()
                .iter()
                .any(|request| request.status() == 401)
        );

        // 2. With them: git sends them after the 401, and the server takes them.
        let url = http.url(REPO).replacen("http://", "http://user:token@", 1);
        let listed = ok(&client(&server, None, &[], &["ls-remote", &url]));
        assert!(listed.contains("refs/heads/main"), "{listed}");
        let expected = format!("Basic {}", base64(b"user:token"));
        assert!(
            http.authorizations().contains(&expected),
            "{:?}",
            http.authorizations()
        );

        // 3. With wrong ones: 401 again.
        let wrong = http.url(REPO).replacen("http://", "http://user:nope@", 1);
        failed(&client(&server, None, &[], &["ls-remote", &wrong]));
    }

    #[test]
    fn stop_refuses_connections() {
        let server = served();
        let mut http = server.http(HttpAuth::Anonymous);
        let url = http.url(REPO);
        ok(&client(&server, None, &[], &["ls-remote", &url]));

        // Stopped: the port refuses, and git fails to connect.
        http.stop();
        assert!(
            TcpStream::connect(http.host()).is_err(),
            "a stopped server accepted"
        );
        let stderr = failed(&client(&server, None, &[], &["ls-remote", &url]));
        assert!(
            stderr.contains("Failed to connect") || stderr.contains("Connection refused"),
            "{stderr}",
        );
    }

    #[test]
    fn cut_after_ends_a_fetch_early() {
        let server = served();
        let mut big = String::new();
        for n in 0..20_000 {
            let _ = writeln!(big, "line {n}");
        }
        server.commit_on(REPO, "main", "big", &[("big.txt", &big)]);
        let http = server.http(HttpAuth::Anonymous);
        let work = tempfile::tempdir().expect("a temp dir");
        let dir = work.path().join("clone");
        let dir = dir.to_str().expect("UTF-8");

        // 1. Armed past the ref advertisement: the pack's response ends early, the clone fails.
        http.cut_after(4096);
        let stderr = failed(&client(
            &server,
            None,
            &[],
            &["clone", "--quiet", &http.url(REPO), dir],
        ));
        assert!(
            stderr.contains("transfer closed") || stderr.contains("early EOF"),
            "{stderr}",
        );

        // 2. One cut only: the same clone then succeeds.
        let work = tempfile::tempdir().expect("a temp dir");
        let dir = work.path().join("clone");
        ok(&client(
            &server,
            None,
            &[],
            &[
                "clone",
                "--quiet",
                &http.url(REPO),
                dir.to_str().expect("UTF-8"),
            ],
        ));
    }

    #[test]
    fn flip_pack_trailer_fails_a_clone_once() {
        let server = served();
        let http = server.http(HttpAuth::Anonymous);
        let work = tempfile::tempdir().expect("a temp dir");

        // 1. Armed: a clone with progress on receives a pack whose checksum is wrong.
        http.flip_pack_trailer();
        let dir = work.path().join("first");
        let stderr = failed(&client(
            &server,
            None,
            &[],
            &[
                "clone",
                "--progress",
                &http.url(REPO),
                dir.to_str().expect("UTF-8"),
            ],
        ));
        let stderr = stderr.to_lowercase();
        assert!(
            stderr.contains("sha1 mismatch") || stderr.contains("sha1 did not match"),
            "{stderr}",
        );

        // 2. One pack only: the same clone then succeeds.
        let dir = work.path().join("second");
        ok(&client(
            &server,
            None,
            &[],
            &["clone", &http.url(REPO), dir.to_str().expect("UTF-8")],
        ));
    }

    #[test]
    fn gzipped_fetch_request_is_served() {
        let server = GitServer::new();
        server.create(REPO);
        for branch in 0..BRANCHES {
            server.commit_on(
                REPO,
                &format!("b{branch}"),
                "start",
                &[("f", &format!("{branch}\n"))],
            );
        }
        let work = tempfile::tempdir().expect("a temp dir");
        let dir = work.path().join("clone");

        // 1. A clone, then every branch moved on the server.
        let http = server.http(HttpAuth::Anonymous);
        ok(&client(
            &server,
            None,
            &[],
            &[
                "clone",
                "--quiet",
                "--no-checkout",
                &http.url(REPO),
                dir.to_str().expect("UTF-8"),
            ],
        ));
        for branch in 0..BRANCHES {
            server.commit_on(
                REPO,
                &format!("b{branch}"),
                "moved",
                &[("f", &format!("{branch} moved\n"))],
            );
        }

        // 2. The fetch's request is gzipped (dozens of wants and haves) and served.
        ok(&client(
            &server,
            Some(&dir),
            &[],
            &["fetch", "--quiet", "origin"],
        ));
        let gzipped = http
            .requests()
            .iter()
            .any(|request| request.header("Content-Encoding") == Some("gzip"));
        assert!(
            gzipped,
            "no gzipped request: the self-check below would pass vacuously"
        );

        // 3. The self-check: a server that drops the HTTP_* metavariables fails that fetch.
        for branch in 0..BRANCHES {
            server.commit_on(
                REPO,
                &format!("b{branch}"),
                "again",
                &[("f", &format!("{branch} again\n"))],
            );
        }
        let dropping = server.http_with(HttpAuth::Anonymous, HeaderPassing::Dropped);
        let stderr = failed(&client(
            &server,
            Some(&dir),
            &[],
            &[
                "fetch",
                "--quiet",
                &dropping.url(REPO),
                "refs/heads/*:refs/remotes/origin/*",
            ],
        ));
        assert!(
            stderr.contains("expected '") || stderr.contains("protocol error"),
            "{stderr}"
        );
    }

    #[test]
    fn ssh_deny_and_down_fail_git() {
        let server = served();
        let url = server.ssh_url(REPO);
        for (mode, said) in [
            (SshMode::Deny, "Permission denied (publickey)."),
            (SshMode::Down, "Connection refused"),
        ] {
            let ssh = server.ssh_command(mode);
            let stderr = failed(&client(
                &server,
                None,
                &[("GIT_SSH_COMMAND", &ssh)],
                &["ls-remote", &url],
            ));
            assert!(stderr.contains(said), "{mode:?}: {stderr}");
        }
    }

    /// A server whose `feature` branch holds two commits off main, with trailers in their
    /// bodies, and whose main moved on beside it.
    fn diverged() -> GitServer {
        let server = served();
        let base = server.rev_parse(REPO, "main").expect("main");
        server.move_to(REPO, "feature", &base);
        server.commit_on(
            REPO,
            "feature",
            "one\n\nbody one\n\nSwarmOps-Change: c1",
            &[("one.txt", "1\n")],
        );
        server.commit_on(
            REPO,
            "feature",
            "two\n\nbody two\n\nSwarmOps-Change: c2",
            &[("two.txt", "2\n")],
        );
        server.commit_on(REPO, "main", "beside", &[("main.txt", "m\n")]);
        server
    }

    /// The parents of `rev`.
    fn parents(server: &GitServer, rev: &str) -> Vec<String> {
        let line = server.out(REPO, &["rev-list", "--parents", "-n", "1", rev]);
        line.split(' ').skip(1).map(str::to_owned).collect()
    }

    /// `rev`'s message.
    fn message(server: &GitServer, rev: &str) -> String {
        server.out(REPO, &["log", "-1", "--format=%B", rev])
    }

    #[test]
    fn merge_commit_has_two_parents() {
        let server = diverged();
        let main = server.rev_parse(REPO, "main").expect("main");
        let feature = server.rev_parse(REPO, "feature").expect("feature");
        let merged = server.merge(REPO, MergeMethod::Commit, "feature", "main");
        assert_eq!(server.rev_parse(REPO, "main"), Some(merged.clone()));
        assert_eq!(parents(&server, &merged), [main, feature]);
        let files = server.out(REPO, &["ls-tree", "--name-only", &merged]);
        assert_eq!(files, "a.txt\nmain.txt\none.txt\ntwo.txt");
    }

    #[test]
    fn squash_has_one_parent_and_the_listed_messages() {
        let server = diverged();
        let main = server.rev_parse(REPO, "main").expect("main");
        let squashed = server.merge(
            REPO,
            MergeMethod::Squash(SquashMessage::CommitList),
            "feature",
            "main",
        );
        assert_eq!(parents(&server, &squashed), [main]);
        let said = message(&server, &squashed);
        assert!(
            said.starts_with("feature (squashed)\n\n* one\n\nbody one"),
            "{said}"
        );
        assert!(
            said.contains("SwarmOps-Change: c1") && said.contains("SwarmOps-Change: c2"),
            "{said}"
        );
        assert!(
            said.find("* one").expect("one") < said.find("* two").expect("two"),
            "{said}"
        );
    }

    #[test]
    fn squash_title_only_drops_the_bodies() {
        let server = diverged();
        let squashed = server.merge(
            REPO,
            MergeMethod::Squash(SquashMessage::TitleOnly),
            "feature",
            "main",
        );
        assert_eq!(message(&server, &squashed), "feature (squashed)");
        assert_eq!(parents(&server, &squashed).len(), 1);
    }

    #[test]
    fn rebase_keeps_messages_and_changes_the_committer() {
        let server = diverged();
        let main = server.rev_parse(REPO, "main").expect("main");
        let feature = server.rev_parse(REPO, "feature").expect("feature");
        let rebased = server.merge(REPO, MergeMethod::Rebase, "feature", "main");

        // Two new commits on main, linear, with the old messages and authors.
        let range = format!("{main}..{rebased}");
        let new = server.out(REPO, &["rev-list", "--reverse", &range]);
        let new: Vec<&str> = new.lines().collect();
        assert_eq!(new.len(), 2);
        assert_eq!(parents(&server, new[0]), [main]);
        let old = server.out(
            REPO,
            &["rev-list", "--reverse", &format!("{feature}~2..{feature}")],
        );
        for (new, old) in new.iter().zip(old.lines()) {
            assert_ne!(new, &old);
            assert_eq!(message(&server, new), message(&server, old));
            let who = |rev: &str| server.out(REPO, &["log", "-1", "--format=%an <%ae>|%cn", rev]);
            let (new_who, old_who) = (who(new), who(old));
            assert_eq!(new_who.split('|').next(), old_who.split('|').next());
            assert_eq!(new_who.split('|').nth(1), Some(REMOTE_NAME));
            assert_ne!(old_who.split('|').nth(1), Some(REMOTE_NAME));
        }
    }

    #[test]
    fn decline_refuses_a_push_with_the_message() {
        let server = served();
        server.decline(REPO, "main", "main is protected");
        let http = server.http(HttpAuth::Anonymous);
        let work = tempfile::tempdir().expect("a temp dir");
        let dir = work.path().join("clone");
        ok(&client(
            &server,
            None,
            &[],
            &[
                "clone",
                "--quiet",
                &http.url(REPO),
                dir.to_str().expect("UTF-8"),
            ],
        ));
        ok(&client(
            &server,
            Some(&dir),
            &[],
            &["commit", "--quiet", "--allow-empty", "-m", "x"],
        ));
        let before = server.rev_parse(REPO, "main");

        // 1. main: refused with the message, unchanged.
        let stderr = failed(&client(
            &server,
            Some(&dir),
            &[],
            &["push", "origin", "HEAD:main"],
        ));
        assert!(stderr.contains("main is protected"), "{stderr}");
        assert_eq!(server.rev_parse(REPO, "main"), before);

        // 2. Another branch: taken.
        ok(&client(
            &server,
            Some(&dir),
            &[],
            &["push", "--quiet", "origin", "HEAD:topic"],
        ));
        assert!(server.rev_parse(REPO, "topic").is_some());
    }

    #[test]
    fn new_sha256_makes_a_sha256_repository() {
        let server = GitServer::new_sha256();
        server.create(REPO);
        let commit = server.commit_on(REPO, "main", "first", &[("a.txt", "a\n")]);
        assert_eq!(commit.len(), 64);
        assert_eq!(
            server.out(REPO, &["rev-parse", "--show-object-format"]),
            "sha256"
        );

        // Served too: plain git clones it over HTTP.
        let http = server.http(HttpAuth::Anonymous);
        let work = tempfile::tempdir().expect("a temp dir");
        let dir = work.path().join("clone");
        ok(&client(
            &server,
            None,
            &[],
            &[
                "clone",
                "--quiet",
                &http.url(REPO),
                dir.to_str().expect("UTF-8"),
            ],
        ));
        assert_eq!(
            ok(&client(&server, Some(&dir), &[], &["rev-parse", "HEAD"])),
            commit
        );
    }

    #[test]
    fn base64_encodes_as_the_rfc_says() {
        // RFC 4648 §10's vectors.
        let vectors = [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foobar", "Zm9vYmFy"),
        ];
        for (plain, encoded) in vectors {
            assert_eq!(base64(plain.as_bytes()), encoded);
        }
    }

    #[test]
    fn scripts_run_while_other_threads_fork() {
        // A thread's fork copies every open descriptor, so a script another thread is
        // writing stays open for writing until that child execs: running the script then
        // fails with ETXTBSY ("Text file busy"). Each thread writes and runs at once.
        let dir = tempfile::tempdir().expect("a temporary directory");
        std::thread::scope(|scope| {
            for t in 0..SCRIPT_THREADS {
                let dir = dir.path();
                scope.spawn(move || {
                    for n in 0..SCRIPTS_PER_THREAD {
                        let script = dir.join(format!("script-{t}-{n}"));
                        write_script(&script, "#!/bin/sh\nexit 0\n");
                        let status = std::process::Command::new(&script)
                            .status()
                            .unwrap_or_else(|error| panic!("run {}: {error}", script.display()));
                        assert!(status.success(), "{}: {status}", script.display());
                    }
                });
            }
        });
    }
}
