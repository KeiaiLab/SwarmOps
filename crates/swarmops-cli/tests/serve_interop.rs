//! Stock git against `swarmops serve` (topology 1) where a team meets the wire's hard cases: a
//! lease, a transfer cut either way, a land whose report is lost and a client killed mid-push.
//! Each runs over ssh through the test kit's forced-command stand-in, so that no test needs the
//! network or an sshd: a push is cut and a client held by the stand-in's faults ([`SshFault`]),
//! an answer is cut at an exact byte by a wrapper of this file.
//!
//! ```text
//!   client git ──stdin──▶ stand-in ──▶ swarmops serve     SshFault::CutSend: the push cut
//!   client git ◀─stdout── dd bs=1 ◀── stand-in ◀── serve  cut_answer(n): a pack or report cut
//! ```

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use swarmops_testkit::child::{self, CHILD_TIMEOUT, Child};
use swarmops_testkit::git::fixture;
use swarmops_testkit::remote::SshFault;
use swarmops_testkit::serve::ServeSite;
use tempfile::TempDir;

use crate::support::{self, Swarmops};

/// The agent a forced command names.
const AGENT: &str = "person/ada";

/// The agent that writes the served store directly, through the command line.
const WRITER: &str = "agent/worker-1";

/// The store every site serves.
const STORE: &str = "acme";

/// The served stores' access file: [`AGENT`] pushes `main`, the release and feature branches,
/// forces the feature branches and lands onto `main`.
const ACCESS: &str = r#"version = 1

[[grant]]
who = ["person/ada"]
push = ["main", "release/*", "feature/**"]
force = ["feature/**"]
land = ["main"]
"#;

/// Where a push names the branch it lands onto.
const LAND_MAIN: &str = "HEAD:refs/land/main";

/// The bytes of noise a commit carries, so that its pack is larger than [`CUT`].
const NOISE_BYTES: usize = 256 << 10;

/// Where a stream is cut: past the advertisement and the requests, inside the pack. The test
/// kit's cutter stops at its first read, so a push is cut there, at most this far.
const CUT: usize = 20_000;

/// How long a held stand-in stays after its server ended: as long as the wait for the land,
/// which starts before the server ends, so that the client is still waiting when it is killed.
const HOLD: Duration = CHILD_TIMEOUT;

/// How often a wait looks again.
const POLL: Duration = Duration::from_millis(50);

/// `SIGKILL`'s number.
const SIGKILL: i32 = 9;

/// What the hook tells the client once a land is done.
const LANDED: &str = "landed onto main as ";

/// A served root with the store [`STORE`] holding the `linear` fixture as `main`, under
/// [`ACCESS`], and a directory for clients.
struct Site {
    /// The root, its stand-ins and the client git.
    served: ServeSite,
    /// The clients' working directories.
    work: TempDir,
}

impl Site {
    /// The store, served with the test kit's environment.
    fn new() -> Site {
        let served = ServeSite::new::<Swarmops>();
        served.init(STORE);
        let site = Site {
            served,
            work: TempDir::new().unwrap(),
        };
        let linear = fixture("linear");
        let source = linear.repo.as_deref().unwrap().to_str().unwrap();
        site.so(&["git", "import", source, "--ref", "HEAD", "--into", "main"]);
        // Written after the import, so that no preparation runs beside the test's connections.
        fs::write(site.served.store(STORE).join("access.toml"), ACCESS).unwrap();
        site
    }

    /// The store's path.
    fn store(&self) -> String {
        self.served.store(STORE).to_str().unwrap().to_owned()
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

    /// The store's refs and `serve.git`'s, to see that nothing moved.
    fn refs(&self) -> (Value, String) {
        (
            self.so(&["ref", "list"])["refs"].clone(),
            self.served_refs(),
        )
    }

    /// Every ref of the store's `serve.git`, one `<oid> <name>` line each.
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

    /// The store's `main` head.
    fn main_head(&self) -> String {
        self.so(&["show", "main"])["id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    /// How many moves the store's ref log holds for `main`.
    fn main_moves(&self) -> usize {
        let log = self.so(&["ref", "log", "main", "--limit", "1000"]);
        assert_eq!(log["truncated"], false, "{log}");
        log["moves"].as_array().unwrap().len()
    }

    /// The length of the store's journal.
    fn journal_len(&self) -> u64 {
        let log = swarmops_testkit::store::layout_of(self.store()).journal_log();
        fs::metadata(log).unwrap().len()
    }

    /// Asserts that `verify --deep` finds nothing.
    fn assert_clean(&self) {
        let verify = support::run(&["--repo", &self.store(), "verify", "--deep"], &[]);
        assert_eq!(verify.exit(), 0, "{verify}");
    }

    /// A client git command in `dir`, its ssh the stand-in `ssh`.
    fn git_command(&self, ssh: &str, dir: &Path, args: &[&str]) -> Command {
        let mut command = self.served.command();
        command
            .env("GIT_SSH_COMMAND", ssh)
            .current_dir(dir)
            .args(args);
        command
    }

    /// git with `args` in `dir`, its ssh the stand-in `ssh`; what it did. Its stderr never holds
    /// the store's path.
    fn git_via(&self, ssh: &str, dir: &Path, args: &[&str]) -> Output {
        let output = child::run(&mut self.git_command(ssh, dir, args));
        let stderr = text(&output.stderr);
        assert!(!stderr.contains(&self.store()), "a host path: {stderr}");
        output
    }

    /// git with `args` in `dir` as [`AGENT`]; what it did.
    fn git_out(&self, dir: &Path, args: &[&str]) -> Output {
        self.git_via(&self.served.ssh_command(AGENT), dir, args)
    }

    /// git with `args` in `dir` as [`AGENT`], which must succeed; its stdout.
    fn git(&self, dir: &Path, args: &[&str]) -> String {
        let output = self.git_out(dir, args);
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            text(&output.stderr)
        );
        text(&output.stdout)
    }

    /// The store `name` cloned into the work directory's `into`; the clone's directory.
    fn clone_of(&self, name: &str, into: &str) -> PathBuf {
        let url = self.served.ssh_url(name);
        self.git(self.work.path(), &["clone", "--quiet", &url, into]);
        self.work.path().join(into)
    }

    /// The store cloned into `into`; the clone's directory.
    fn clone(&self, into: &str) -> PathBuf {
        self.clone_of(STORE, into)
    }

    /// A commit writing `content` to `path` in the clone `dir`, on its branch; its id.
    fn commit(&self, dir: &Path, path: &str, content: &[u8]) -> String {
        fs::write(dir.join(path), content).unwrap();
        self.git(dir, &["add", path]);
        self.git(dir, &["commit", "--quiet", "-m", path]);
        self.rev(dir, "HEAD")
    }

    /// The clone's `rev-parse` of `rev` in `dir`.
    fn rev(&self, dir: &Path, rev: &str) -> String {
        self.git(dir, &["rev-parse", "--verify", rev])
            .trim()
            .to_owned()
    }

    /// Asserts that `git fsck --strict` finds nothing in the clone `dir`.
    fn assert_fsck(&self, dir: &Path) {
        self.git(dir, &["fsck", "--strict", "--no-progress"]);
    }

    /// A `GIT_SSH_COMMAND` value that runs `body`, a shell script given the client's arguments
    /// as `"$@"`. The file is named ssh, so that git takes it for OpenSSH and does not probe it.
    fn wrapper(&self, name: &str, body: &str) -> String {
        let dir = self.work.path().join(name);
        fs::create_dir(&dir).unwrap();
        let program = dir.join("ssh");
        fs::write(&program, format!("#!/bin/sh\n{body}")).unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        program.to_str().unwrap().to_owned()
    }

    /// The stand-in `ssh` with its stderr, and its server's, off the client's: a client killed
    /// while the stand-in lives leaves no one holding the pipes the test reads.
    fn detached(&self, name: &str, ssh: &str) -> String {
        self.wrapper(name, &format!("exec 2>/dev/null\nexec {ssh} \"$@\"\n"))
    }

    /// The stand-in for [`AGENT`] with its answer cut after exactly `bytes` bytes. The test
    /// kit's [`SshFault::CutReceive`] ends at its cutter's first read when `bytes` is under one
    /// read block, wherever that read ended; this one copies a byte a read. Cut after the
    /// client's last request, the server then ends on its next write, and so does the stand-in.
    fn cut_answer(&self, name: &str, bytes: usize) -> String {
        let ssh = self.served.ssh_command(AGENT);
        let body = format!("{ssh} \"$@\" | dd bs=1 count={bytes} status=none\n");
        self.wrapper(name, &body)
    }

    /// The push of `dir`'s `HEAD` to `refs/land/main` as [`AGENT`], which must succeed; its
    /// stderr.
    fn push_land(&self, dir: &Path) -> String {
        let pushed = self.git_out(dir, &["push", "origin", LAND_MAIN]);
        let stderr = text(&pushed.stderr);
        assert!(pushed.status.success(), "{stderr}");
        stderr
    }
}

/// `bytes` as text.
fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// `len` bytes that no compression shrinks, the same for the same `seed`.
fn noise(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed | 1;
    let mut bytes = Vec::with_capacity(len + 8);
    while bytes.len() < len {
        // xorshift64.
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        bytes.extend_from_slice(&state.to_le_bytes());
    }
    bytes.truncate(len);
    bytes
}

/// The offset just past the first pkt-line of `stream` whose payload holds `needle`: the
/// stream is pkt-lines end to end (`0000`, `0001` and `0002` carry no payload).
fn packet_end(stream: &[u8], needle: &[u8]) -> usize {
    let mut at = 0;
    while at + 4 <= stream.len() {
        let len = std::str::from_utf8(&stream[at..at + 4]).unwrap();
        let len = usize::from_str_radix(len, 16).unwrap();

        // 1. A special packet: its four bytes alone.
        if len < 4 {
            at += 4;
            continue;
        }

        // 2. A data packet: the needle ends the walk.
        let end = at + len;
        if stream[at + 4..end]
            .windows(needle.len())
            .any(|window| window == needle)
        {
            return end;
        }
        at = end;
    }
    panic!("no packet holds {}", text(needle));
}

/// A lease is honoured: a forced push whose lease names a commit the branch no longer holds is
/// refused by the client and moves nothing; with the branch's present commit as its lease, the
/// same rewrite goes.
#[test]
fn force_with_lease_is_honoured() {
    let site = Site::new();
    let first = site.clone("first");
    let second = site.clone("second");

    // 1. feature/x made by the first clone, which the second fetches.
    site.git(&first, &["checkout", "--quiet", "-b", "feature/x"]);
    let made = site.commit(&first, "x.txt", b"x\n");
    site.git(&first, &["push", "--quiet", "origin", "feature/x"]);
    site.git(&second, &["fetch", "--quiet", "origin"]);
    assert_eq!(site.rev(&second, "origin/feature/x"), made);

    // 2. The first clone rewrites it, forced.
    site.git(&first, &["commit", "--quiet", "--amend", "-m", "x again"]);
    let rewritten = site.rev(&first, "HEAD");
    site.git(
        &first,
        &["push", "--quiet", "--force", "origin", "feature/x"],
    );

    // 3. The second clone's rewrite, its lease the commit it fetched: refused, nothing moved.
    site.git(&second, &["checkout", "--quiet", "-b", "feature/x", &made]);
    let ours = site.commit(&second, "y.txt", b"y\n");
    let before = site.refs();
    let lease = format!("--force-with-lease=feature/x:{made}");
    let stale = site.git_out(&second, &["push", &lease, "origin", "feature/x"]);
    let stderr = text(&stale.stderr);
    assert!(!stale.status.success(), "{stderr}");
    assert!(stderr.contains("stale info"), "{stderr}");
    assert_eq!(site.refs(), before);

    // 4. Its lease the present commit: the rewrite goes, byte for byte.
    let lease = format!("--force-with-lease=feature/x:{rewritten}");
    site.git(&second, &["push", "--quiet", &lease, "origin", "feature/x"]);
    let fresh = site.clone("fresh");
    assert_eq!(site.rev(&fresh, "origin/feature/x"), ours);
}

/// A clone cut inside its pack fails and leaves no directory, over protocol v2 and v0; cloned
/// again it is whole. A fetch cut inside its pack fails and moves no tracking ref; fetched again
/// it is whole.
#[test]
fn a_cut_clone_and_fetch_recover() {
    let site = Site::new();
    let writer = site.clone("writer");
    site.commit(&writer, "noise-1.bin", &noise(1, NOISE_BYTES));
    site.git(&writer, &["push", "--quiet", "origin", "main"]);
    let url = site.served.ssh_url(STORE);
    let cut = site.cut_answer("cut", CUT);

    // 1. Each protocol: the cut clone fails and leaves nothing; the clone again is whole.
    for version in ["2", "0"] {
        let into = format!("clone-v{version}");
        let protocol = format!("protocol.version={version}");
        let line = ["-c", &protocol, "clone", "--quiet", &url, &into];
        let failed = site.git_via(&cut, site.work.path(), &line);
        assert!(!failed.status.success(), "{}", text(&failed.stderr));
        assert!(!site.work.path().join(&into).exists(), "a cut clone stayed");

        site.git(site.work.path(), &line);
        let dir = site.work.path().join(&into);
        site.assert_fsck(&dir);
        assert_eq!(site.rev(&dir, "HEAD"), site.rev(&writer, "HEAD"));
    }

    // 2. The cut fetch fails and moves no tracking ref.
    let reader = site.work.path().join("clone-v2");
    let before = site.rev(&reader, "origin/main");
    let head = site.commit(&writer, "noise-2.bin", &noise(2, NOISE_BYTES));
    site.git(&writer, &["push", "--quiet", "origin", "main"]);
    let failed = site.git_via(&cut, &reader, &["fetch", "--quiet", "origin"]);
    assert!(!failed.status.success(), "{}", text(&failed.stderr));
    assert_eq!(site.rev(&reader, "origin/main"), before);

    // 3. Fetched again: whole.
    site.git(&reader, &["fetch", "--quiet", "origin"]);
    assert_eq!(site.rev(&reader, "origin/main"), head);
    site.assert_fsck(&reader);
}

/// A land push whose pack is cut on the way in moves nothing and leaves the store clean; the
/// same push again lands once.
#[test]
fn a_cut_push_moves_nothing_and_lands_once() {
    let site = Site::new();
    let work = site.clone("work");
    let head = site.main_head();
    let moves = site.main_moves();
    site.commit(&work, "noise.bin", &noise(3, NOISE_BYTES));

    // 1. The push, cut inside its pack: refused, nothing moved.
    let before = site.refs();
    let cut = site.served.ssh_command_with(AGENT, SshFault::CutSend(CUT));
    let failed = site.git_via(&cut, &work, &["push", "origin", LAND_MAIN]);
    let stderr = text(&failed.stderr);
    assert!(!failed.status.success(), "{stderr}");
    assert!(!stderr.contains(LANDED), "{stderr}");
    assert_eq!(site.refs(), before);
    site.assert_clean();

    // 2. The same push again: one land onto main.
    let stderr = site.push_land(&work);
    assert!(stderr.contains(LANDED), "{stderr}");
    assert_eq!(site.main_moves(), moves + 1);
    let shown = site.so(&["show", "main"]);
    assert_eq!(shown["parents"][0], head.as_str(), "{shown}");
    site.assert_clean();
}

/// A land whose report is cut on the way back, after the hook said it landed: the client fails,
/// main moved once; the same push again replays, writing nothing, and a fetch brings the land.
#[test]
fn a_land_whose_report_is_lost_replays() {
    let site = Site::new();
    let work = site.clone("work");

    // 1. A land of the same shape, its answer recorded, finds where the report starts.
    site.commit(&work, "measured.txt", b"measured\n");
    let capture = site.work.path().join("answer.bin");
    let tee = format!(
        "{} \"$@\" | tee {}\n",
        site.served.ssh_command(AGENT),
        capture.to_str().unwrap()
    );
    let tee = site.wrapper("tee", &tee);
    let measured = site.git_via(&tee, &work, &["push", "origin", LAND_MAIN]);
    assert!(measured.status.success(), "{}", text(&measured.stderr));
    let report = packet_end(&fs::read(&capture).unwrap(), LANDED.as_bytes());
    site.git(&work, &["pull", "--quiet", "--rebase"]);

    // 2. The land, its answer cut after the hook's word: the client fails, main moved once.
    site.commit(&work, "lost.txt", b"lost\n");
    let moves = site.main_moves();
    let cut = site.cut_answer("cut", report);
    let failed = site.git_via(&cut, &work, &["push", "origin", LAND_MAIN]);
    let stderr = text(&failed.stderr);
    assert!(!failed.status.success(), "{stderr}");
    assert!(stderr.contains(LANDED), "{stderr}");
    assert_eq!(site.main_moves(), moves + 1);
    let landed = site.main_head();

    // 3. The same push again: landed already, nothing written.
    let journal = site.journal_len();
    let stderr = site.push_land(&work);
    assert!(stderr.contains("landed already onto main as "), "{stderr}");
    assert_eq!(site.journal_len(), journal, "the replay wrote: {stderr}");
    assert_eq!(site.main_head(), landed);
    assert_eq!(site.main_moves(), moves + 1);

    // 4. A fetch brings the land; the store is clean.
    site.git(&work, &["pull", "--quiet", "--rebase"]);
    assert_eq!(site.rev(&work, "HEAD"), site.rev(&work, "origin/main"));
    site.assert_clean();
}

/// A client killed mid-push leaves a clean store, at either end of the push: killed before its
/// pack (by its own pre-push hook), nothing moved and the push again lands; killed after the
/// remote landed (while its stand-in is held open), the push again replays.
#[test]
fn a_client_killed_mid_push_leaves_a_clean_store() {
    let site = Site::new();
    let work = site.clone("work");
    let head = site.main_head();
    let moves = site.main_moves();

    // 1. Killed by its pre-push hook, after the advertisement and before the pack.
    site.commit(&work, "before.txt", b"before\n");
    let pre_push = work.join(".git/hooks/pre-push");
    fs::write(&pre_push, "#!/bin/sh\nkill -KILL $PPID\n").unwrap();
    fs::set_permissions(&pre_push, fs::Permissions::from_mode(0o755)).unwrap();
    let ssh = site.detached("before", &site.served.ssh_command(AGENT));
    let killed = site.git_via(&ssh, &work, &["push", "origin", LAND_MAIN]);
    assert_eq!(killed.status.signal(), Some(SIGKILL), "{killed:?}");
    fs::remove_file(&pre_push).unwrap();
    assert_eq!(site.main_head(), head);
    site.assert_clean();
    let stderr = site.push_land(&work);
    assert!(stderr.contains(LANDED), "{stderr}");
    assert_eq!(site.main_moves(), moves + 1);
    site.git(&work, &["pull", "--quiet", "--rebase"]);

    // 2. Killed once the remote landed: its stand-in held open, so the client still waits.
    site.commit(&work, "after.txt", b"after\n");
    let before = site.main_head();
    let holding = site
        .served
        .ssh_command_with(AGENT, SshFault::HoldAfterExit(HOLD));
    let ssh = site.detached("held", &holding);
    let mut push = Child::spawn(&mut site.git_command(&ssh, &work, &["push", "origin", LAND_MAIN]));
    let deadline = Instant::now() + CHILD_TIMEOUT;
    while site.main_head() == before {
        assert!(Instant::now() < deadline, "the land never came");
        thread::sleep(POLL);
    }
    let kill = Command::new("kill")
        .args(["-KILL", &push.id().to_string()])
        .status()
        .unwrap();
    assert!(kill.success());
    let (killed, _) = push.finish(Instant::now() + CHILD_TIMEOUT);
    assert_eq!(killed.status.signal(), Some(SIGKILL), "{killed:?}");
    site.assert_clean();

    // 3. The push again: landed already, one move for it.
    let stderr = site.push_land(&work);
    assert!(stderr.contains("landed already onto main as "), "{stderr}");
    assert_eq!(site.main_moves(), moves + 2);
    site.assert_clean();
}
