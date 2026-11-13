//! Stock git against `swarmops serve` (topology 1) where a team meets the wire's hard cases: a
//! lease, a transfer cut either way, a land whose report is lost, a client killed mid-push, the
//! history shapes a team pushes, and push options.
//! Each runs over ssh through the test kit's forced-command stand-in, so that no test needs the
//! network or an sshd: a push is cut and a client held by the stand-in's faults ([`SshFault`]),
//! an answer is cut at an exact byte by a wrapper of this file. One run alone,
//! [`real_sshd_round_trip`], goes through a real OpenSSH sshd on loopback, only when
//! `SWARMOPS_REAL_SSHD=1` (the Scheduled run).
//!
//! ```text
//!   client git ──stdin──▶ stand-in ──▶ swarmops serve     SshFault::CutSend: the push cut
//!   client git ◀─stdout── dd bs=1 ◀── stand-in ◀── serve  cut_answer(n): a pack or report cut
//! ```

use std::fs;
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{self, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use swarmops_testkit::child::{self, CHILD_TIMEOUT, Child, SIGKILL};
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

    /// Asserts that the clone `dir` converged on the remote's `main`: `HEAD` is `origin/main`
    /// and nothing is left uncommitted.
    fn assert_converged(&self, dir: &Path) {
        assert_eq!(self.rev(dir, "HEAD"), self.rev(dir, "origin/main"));
        assert_eq!(self.git(dir, &["status", "--porcelain"]), "");
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

/// A land of three commits lands once; `pull --rebase` then converges on the landed `main`,
/// holding every commit's file.
#[test]
fn a_multi_commit_land_then_pull_rebase_converges() {
    let site = Site::new();
    let work = site.clone("work");
    let moves = site.main_moves();

    // 1. Three commits, landed by one push.
    for name in ["one.txt", "two.txt", "three.txt"] {
        site.commit(&work, name, name.as_bytes());
    }
    let stderr = site.push_land(&work);
    assert!(stderr.contains(LANDED), "{stderr}");
    assert_eq!(site.main_moves(), moves + 1);

    // 2. pull --rebase: the clone is main, with the three files.
    site.git(&work, &["pull", "--quiet", "--rebase"]);
    site.assert_converged(&work);
    for name in ["one.txt", "two.txt", "three.txt"] {
        assert_eq!(fs::read(work.join(name)).unwrap(), name.as_bytes());
    }
    site.assert_fsck(&work);
    site.assert_clean();
}

/// A land whose base `main` has moved since the clone's fetch is relocated onto the new `main`;
/// `pull --rebase` then converges, holding both sides' files.
#[test]
fn a_land_that_relocates_then_pull_rebase_converges() {
    let site = Site::new();
    let work = site.clone("work");
    let other = site.clone("other");

    // 1. main moves under the first clone, by a fast-forward push of the second.
    site.commit(&other, "theirs.txt", b"theirs\n");
    site.git(&other, &["push", "--quiet", "origin", "main"]);
    let moved = site.main_head();

    // 2. The first clone lands on its stale base: relocated onto the moved main.
    site.commit(&work, "ours.txt", b"ours\n");
    let stderr = site.push_land(&work);
    assert!(stderr.contains(LANDED), "{stderr}");
    let shown = site.so(&["show", "main"]);
    assert_eq!(shown["parents"][0], moved.as_str(), "{shown}");

    // 3. pull --rebase: the clone is main, with both files.
    site.git(&work, &["pull", "--quiet", "--rebase"]);
    site.assert_converged(&work);
    assert_eq!(fs::read(work.join("theirs.txt")).unwrap(), b"theirs\n");
    assert_eq!(fs::read(work.join("ours.txt")).unwrap(), b"ours\n");
    site.assert_fsck(&work);
    site.assert_clean();
}

/// A merge commit pushed to a feature branch keeps its id and both parents: a fresh clone holds
/// the same commit.
#[test]
fn a_merge_commit_round_trips_on_a_branch() {
    let site = Site::new();
    let work = site.clone("work");

    // 1. Two branches off main, merged with a merge commit on feature/m.
    site.git(&work, &["checkout", "--quiet", "-b", "feature/side"]);
    let merged = site.commit(&work, "side.txt", b"side\n");
    site.git(&work, &["checkout", "--quiet", "-b", "feature/m", "main"]);
    let ours = site.commit(&work, "ours.txt", b"ours\n");
    site.git(
        &work,
        &[
            "merge",
            "--quiet",
            "--no-ff",
            "-m",
            "merge side",
            "feature/side",
        ],
    );
    let merge = site.rev(&work, "HEAD");

    // 2. Pushed, then cloned afresh: the same merge, the same parents.
    site.git(&work, &["push", "--quiet", "origin", "feature/m"]);
    let fresh = site.clone("fresh");
    assert_eq!(site.rev(&fresh, "origin/feature/m"), merge);
    assert_eq!(site.rev(&fresh, "origin/feature/m^1"), ours);
    assert_eq!(site.rev(&fresh, "origin/feature/m^2"), merged);
    site.assert_fsck(&fresh);
    site.assert_clean();
}

/// Commits git writes but rarely meets keep their ids through the store: a signature header, an
/// encoding header, an empty commit, a mode change and a symlink.
#[test]
fn unusual_commits_round_trip_by_id() {
    let site = Site::new();
    let work = site.clone("work");
    site.git(&work, &["checkout", "--quiet", "-b", "feature/odd"]);
    let mut made = Vec::new();

    // 1. An empty commit.
    site.git(
        &work,
        &["commit", "--quiet", "--allow-empty", "-m", "empty"],
    );
    made.push(site.rev(&work, "HEAD"));

    // 2. A file made executable.
    made.push(site.commit(&work, "run.sh", b"#!/bin/sh\n"));
    site.git(&work, &["update-index", "--chmod=+x", "run.sh"]);
    site.git(&work, &["commit", "--quiet", "-m", "executable"]);
    made.push(site.rev(&work, "HEAD"));

    // 3. A symlink.
    std::os::unix::fs::symlink("run.sh", work.join("link")).unwrap();
    site.git(&work, &["add", "link"]);
    site.git(&work, &["commit", "--quiet", "-m", "link"]);
    made.push(site.rev(&work, "HEAD"));

    // 4. A Latin-1 message under an encoding header.
    let message = work.join("message.txt");
    fs::write(&message, b"caf\xe9\n").unwrap();
    let file = message.to_str().unwrap();
    let encoding = "i18n.commitEncoding=ISO-8859-1";
    site.git(
        &work,
        &[
            "-c",
            encoding,
            "commit",
            "--quiet",
            "--allow-empty",
            "-F",
            file,
        ],
    );
    let encoded = site.rev(&work, "HEAD");
    let raw = site.git(&work, &["cat-file", "commit", &encoded]);
    assert!(raw.contains("\nencoding ISO-8859-1\n"), "{raw}");
    made.push(encoded);

    // 5. A signature header, written as git writes one: after the committer.
    site.git(
        &work,
        &["commit", "--quiet", "--allow-empty", "-m", "signed"],
    );
    let raw = site.git(&work, &["cat-file", "commit", "HEAD"]);
    let (head, body) = raw.split_once("\n\n").unwrap();
    let signature = "gpgsig -----BEGIN PGP SIGNATURE-----\n \n iQEz\n -----END PGP SIGNATURE-----";
    let signed = format!("{head}\n{signature}\n\n{body}");
    let path = work.join("signed.commit");
    fs::write(&path, signed).unwrap();
    let path = path.to_str().unwrap();
    let id = site.git(&work, &["hash-object", "-t", "commit", "-w", path]);
    let id = id.trim().to_owned();
    site.git(&work, &["update-ref", "HEAD", &id]);
    made.push(id);

    // 6. Pushed, then cloned afresh: every commit by its id.
    site.git(&work, &["push", "--quiet", "origin", "feature/odd"]);
    let fresh = site.clone("fresh");
    let tip = made.last().unwrap();
    assert_eq!(&site.rev(&fresh, "origin/feature/odd"), tip);
    let listed = site.git(&fresh, &["rev-list", "main..origin/feature/odd"]);
    let mut listed: Vec<&str> = listed.lines().collect();
    listed.reverse();
    assert_eq!(listed, made);
    site.assert_fsck(&fresh);
    site.assert_clean();
}

/// `git push --all` into an empty store creates every branch with its ids kept.
#[test]
fn push_all_into_an_empty_store() {
    let site = Site::new();
    let empty = "empty";
    site.served.init(empty);
    fs::write(site.served.store(empty).join("access.toml"), ACCESS).unwrap();
    let work = site.clone("work");

    // 1. Three branches: main and two made here.
    for branch in ["feature/a", "release/1"] {
        site.git(&work, &["checkout", "--quiet", "-b", branch, "main"]);
        site.commit(&work, "branch.txt", branch.as_bytes());
    }
    let local = site.git(
        &work,
        &[
            "for-each-ref",
            "--format=%(objectname) %(refname)",
            "refs/heads",
        ],
    );

    // 2. Pushed whole into the empty store.
    let url = site.served.ssh_url(empty);
    site.git(&work, &["remote", "add", empty, &url]);
    site.git(&work, &["push", "--quiet", "--all", empty]);

    // 3. A fresh clone holds every branch at its id.
    let fresh = site.clone_of(empty, "fresh");
    let remote = site.git(&fresh, &["ls-remote", "--heads", "origin"]);
    let remote: Vec<String> = remote.lines().map(|line| line.replace('\t', " ")).collect();
    assert_eq!(remote, local.lines().collect::<Vec<_>>());
    site.assert_fsck(&fresh);
    let store = site.served.store(empty);
    let verify = support::run(
        &["--repo", store.to_str().unwrap(), "verify", "--deep"],
        &[],
    );
    assert_eq!(verify.exit(), 0, "{verify}");
}

/// A push with push options is refused whole, before anything is sent: neither of its two refs
/// moves; the same push without them moves both.
#[test]
fn push_options_are_refused_whole() {
    let site = Site::new();
    let work = site.clone("work");
    let head = site.commit(&work, "options.txt", b"options\n");
    let refs = ["origin", "main", "HEAD:refs/heads/feature/o"];

    // 1. With an option: refused by the client, nothing moved.
    let before = site.refs();
    let line: Vec<&str> = ["push", "-o", "ci.skip"]
        .iter()
        .chain(&refs)
        .copied()
        .collect();
    let refused = site.git_out(&work, &line);
    let stderr = text(&refused.stderr);
    assert!(!refused.status.success(), "{stderr}");
    assert!(stderr.contains("push options"), "{stderr}");
    assert_eq!(site.refs(), before);

    // 2. Without: both move.
    let line: Vec<&str> = ["push", "--quiet"].iter().chain(&refs).copied().collect();
    site.git(&work, &line);
    let fresh = site.clone("fresh");
    assert_eq!(site.rev(&fresh, "origin/main"), head);
    assert_eq!(site.rev(&fresh, "origin/feature/o"), head);
    site.assert_clean();
}

/// The variable that turns the run over a real sshd on.
const REAL_SSHD_VAR: &str = "SWARMOPS_REAL_SSHD";

/// Where the OpenSSH programs are looked for after `PATH`: sshd is a system daemon, in an sbin
/// directory a user's `PATH` often lacks.
const SBIN_DIRS: [&str; 3] = ["/usr/sbin", "/usr/local/sbin", "/sbin"];

/// The OpenSSH programs the run over a real sshd needs.
#[derive(Debug, PartialEq, Eq)]
struct OpenSsh {
    /// The daemon.
    sshd: PathBuf,
    /// The client git runs.
    ssh: PathBuf,
    /// What makes the host's and the client's keys.
    keygen: PathBuf,
}

/// Whether the run over a real sshd goes.
#[derive(Debug, PartialEq, Eq)]
enum SshdGate {
    /// Not asked for; why.
    Off(String),
    /// Asked for, with the programs found.
    On(OpenSsh),
}

/// The gate: off unless [`REAL_SSHD_VAR`] is `1`; on, each OpenSSH program is looked for in
/// `dirs`, and one missing fails the run, so that a host asked to run it never skips it.
fn sshd_gate(
    lookup: &dyn Fn(&str) -> Option<String>,
    dirs: &[PathBuf],
) -> Result<SshdGate, String> {
    if lookup(REAL_SSHD_VAR).as_deref() != Some("1") {
        return Ok(SshdGate::Off(format!("{REAL_SSHD_VAR} is not 1")));
    }
    let find = |name: &str| {
        dirs.iter()
            .map(|dir| dir.join(name))
            .find(|path| path.is_file())
            .ok_or_else(|| format!("{REAL_SSHD_VAR}=1 and no {name} in {dirs:?}"))
    };
    Ok(SshdGate::On(OpenSsh {
        sshd: find("sshd")?,
        ssh: find("ssh")?,
        keygen: find("ssh-keygen")?,
    }))
}

/// The directories the OpenSSH programs are looked for in: `PATH`'s, then [`SBIN_DIRS`].
fn program_dirs() -> Vec<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .chain(SBIN_DIRS.iter().map(PathBuf::from))
        .collect()
}

/// With [`REAL_SSHD_VAR`] unset the run is off and says why; set, a missing sshd fails it,
/// naming the program; with every program found it goes.
#[test]
fn a_missing_sshd_fails_the_real_run() {
    let empty = TempDir::new().unwrap();
    let dirs = [empty.path().to_owned()];

    // 1. Unset: off, whatever is installed.
    let off = sshd_gate(&|_| None, &dirs);
    assert!(
        matches!(&off, Ok(SshdGate::Off(reason)) if reason.contains(REAL_SSHD_VAR)),
        "{off:?}"
    );

    // 2. Set, no sshd: the run fails.
    let on = |name: &str| (name == REAL_SSHD_VAR).then(|| "1".to_owned());
    let missing = sshd_gate(&on, &dirs);
    assert!(
        matches!(&missing, Err(wrong) if wrong.contains("no sshd")),
        "{missing:?}"
    );

    // 3. Set, every program there: on.
    for name in ["sshd", "ssh", "ssh-keygen"] {
        fs::write(empty.path().join(name), "").unwrap();
    }
    assert!(matches!(sshd_gate(&on, &dirs), Ok(SshdGate::On(_))));
}

/// The address the real sshd listens on.
const LOOPBACK: &str = "127.0.0.1";

/// An OpenSSH sshd on loopback, run as the test's own user, that knows one client key: its
/// `authorized_keys` line forces the test kit's stand-in for `swarmops serve` as [`AGENT`],
/// which takes `SSH_ORIGINAL_COMMAND` as the stand-in's own command line would carry it.
///
/// ```text
///   client git ── ssh -i <key> -p <port> ── sshd ── <user's shell> -c forced
///       forced: exec <stand-in> sshd "$SSH_ORIGINAL_COMMAND"   (GIT_PROTOCOL kept: AcceptEnv)
///         └── env -i … swarmops serve --root <root> --as person/ada
/// ```
struct RealSshd {
    /// The keys, the configuration, the forced command and the daemon's log.
    dir: TempDir,
    /// The daemon, in the foreground; killed when dropped.
    daemon: process::Child,
    /// The `GIT_SSH_COMMAND` value of the client key.
    ssh: String,
    /// The account sshd runs the forced command as: the test's own.
    user: String,
    /// The port it listens on.
    port: u16,
}

impl RealSshd {
    /// Tries to listen at most this many times: a port free when picked may be taken before
    /// sshd binds it.
    const ATTEMPTS: usize = 3;

    /// The daemon of `openssh`, its forced command `stand_in` (a `GIT_SSH_COMMAND` value of
    /// [`ServeSite::ssh_command`]), listening once it accepts a connection.
    fn start(openssh: &OpenSsh, stand_in: &str) -> RealSshd {
        let dir = TempDir::new().unwrap();
        let at = |name: &str| dir.path().join(name).to_str().unwrap().to_owned();

        // 1. The host's key and the client's, with no passphrase.
        for key in ["host_key", "client_key"] {
            let made = child::run(Command::new(&openssh.keygen).args([
                "-q",
                "-t",
                "ed25519",
                "-N",
                "",
                "-C",
                key,
                "-f",
                &at(key),
            ]));
            assert!(made.status.success(), "{}", text(&made.stderr));
        }

        // 2. The forced command: the stand-in drops its first word as the host.
        let forced = at("forced");
        fs::write(
            &forced,
            format!("#!/bin/sh\nexec {stand_in} sshd \"$SSH_ORIGINAL_COMMAND\"\n"),
        )
        .unwrap();
        fs::set_permissions(&forced, fs::Permissions::from_mode(0o755)).unwrap();
        let client = fs::read_to_string(at("client_key.pub")).unwrap();
        let line = format!("command=\"{forced}\",restrict {client}");
        fs::write(at("authorized_keys"), line).unwrap();

        // 3. Listening on a free loopback port, again on another if that one was taken.
        let (daemon, port) = (0..Self::ATTEMPTS)
            .find_map(|_| Self::listen(openssh, &at))
            .expect("sshd never listened");

        // 4. The client: this host's key alone is known, the client key alone is offered.
        let host = fs::read_to_string(at("host_key.pub")).unwrap();
        fs::write(at("known_hosts"), format!("[{LOOPBACK}]:{port} {host}")).unwrap();
        let ssh = format!(
            "'{ssh}' -F /dev/null -i '{key}' -o IdentitiesOnly=yes \
             -o UserKnownHostsFile='{known}' -o GlobalKnownHostsFile=/dev/null \
             -o StrictHostKeyChecking=yes -o BatchMode=yes",
            ssh = openssh.ssh.to_str().unwrap(),
            key = at("client_key"),
            known = at("known_hosts"),
        );
        let user = child::run(Command::new("id").arg("-un"));
        assert!(user.status.success(), "{}", text(&user.stderr));
        RealSshd {
            dir,
            daemon,
            ssh,
            user: text(&user.stdout).trim().to_owned(),
            port,
        }
    }

    /// sshd started on a port that was free; it and the port once it accepts a connection,
    /// none when it ended because the port was taken. Fails the test on any other end.
    fn listen(openssh: &OpenSsh, at: &dyn Fn(&str) -> String) -> Option<(process::Child, u16)> {
        // 1. A port the kernel gave out a moment ago, and the configuration naming it.
        let port = TcpListener::bind((LOOPBACK, 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let config = format!(
            "ListenAddress {LOOPBACK}\nPort {port}\nHostKey {host}\nPidFile {pid}\n\
             AuthorizedKeysFile {keys}\nUsePAM no\nStrictModes no\nPasswordAuthentication no\n\
             KbdInteractiveAuthentication no\nPubkeyAuthentication yes\nAcceptEnv GIT_PROTOCOL\n\
             LogLevel VERBOSE\n",
            host = at("host_key"),
            pid = at("sshd.pid"),
            keys = at("authorized_keys"),
        );
        fs::write(at("sshd_config"), config).unwrap();

        // 2. The daemon in the foreground, its log in the directory.
        let log = at("sshd.log");
        let _ = fs::remove_file(&log);
        let mut daemon = Command::new(&openssh.sshd)
            .args(["-D", "-f", &at("sshd_config"), "-E", &log])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();

        // 3. Up once it accepts; ended, the port was taken or the test fails with its log.
        let deadline = Instant::now() + CHILD_TIMEOUT;
        loop {
            if TcpStream::connect((LOOPBACK, port)).is_ok() {
                return Some((daemon, port));
            }
            if let Some(status) = daemon.try_wait().unwrap() {
                let log = fs::read_to_string(&log).unwrap_or_default();
                assert!(
                    log.contains("Address already in use"),
                    "sshd {status}: {log}"
                );
                return None;
            }
            assert!(Instant::now() < deadline, "sshd never listened on {port}");
            thread::sleep(POLL);
        }
    }

    /// The ssh URL of store `name` on this daemon.
    fn url(&self, name: &str) -> String {
        format!("ssh://{}@{LOOPBACK}:{}/{name}.git", self.user, self.port)
    }

    /// The client key's ssh session running `command`, as a person at a terminal would.
    fn shell(&self, command: &str) -> Output {
        let line = format!(
            "{} -p {} {}@{LOOPBACK} {command}",
            self.ssh, self.port, self.user
        );
        child::run(
            Command::new("sh")
                .args(["-c", &line])
                .env("HOME", self.dir.path()),
        )
    }

    /// The daemon's log so far, for a failure's message.
    fn log(&self) -> String {
        fs::read_to_string(self.dir.path().join("sshd.log")).unwrap_or_default()
    }
}

impl Drop for RealSshd {
    fn drop(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}

/// The served store's `main`, as `serve.git` holds it.
fn served_main(site: &Site) -> String {
    let refs = site.served_refs();
    let line = refs.lines().find(|line| line.ends_with(" refs/heads/main"));
    let line = line.unwrap_or_else(|| panic!("no main in {refs}"));
    line.split(' ').next().unwrap().to_owned()
}

/// The round trip of topology 1 over a real OpenSSH sshd on loopback, which the stand-in only
/// plays: clones over both protocol versions, `GIT_PROTOCOL` crossing (`AcceptEnv`), a land and
/// `pull --rebase`, a refused push, a shell command refused by `serve`, each id kept.
#[test]
#[ignore = "real sshd: an OpenSSH daemon on loopback, only with SWARMOPS_REAL_SSHD=1"]
fn real_sshd_round_trip() {
    let openssh = match sshd_gate(&|name| std::env::var(name).ok(), &program_dirs()) {
        Ok(SshdGate::On(openssh)) => openssh,
        Ok(SshdGate::Off(reason)) => {
            eprintln!("skipped: {reason}");
            return;
        }
        Err(wrong) => panic!("{wrong}"),
    };
    let site = Site::new();
    let sshd = RealSshd::start(&openssh, &site.served.ssh_command(AGENT));
    let url = sshd.url(STORE);
    let git = |dir: &Path, args: &[&str]| {
        let output = site.git_via(&sshd.ssh, dir, args);
        let stderr = text(&output.stderr);
        assert!(
            output.status.success(),
            "git {args:?}: {stderr}\n{}",
            sshd.log()
        );
        text(&output.stdout)
    };

    // 1. Clones over protocol version 2 and 0 (the first makes serve.git): each holds the served
    // main by id.
    let mut clones = Vec::new();
    for version in ["2", "0"] {
        let protocol = format!("protocol.version={version}");
        let into = format!("v{version}");
        git(
            site.work.path(),
            &["-c", &protocol, "clone", "--quiet", &url, &into],
        );
        let clone = site.work.path().join(&into);
        assert_eq!(site.rev(&clone, "origin/main"), served_main(&site));
        clones.push(clone);
    }
    let work = &clones[0];
    let served = served_main(&site);

    // 2. Version 2 is spoken only if sshd passed GIT_PROTOCOL on.
    let mut traced = site.git_command(
        &sshd.ssh,
        work,
        &["-c", "protocol.version=2", "ls-remote", "origin", "main"],
    );
    let traced = child::run(traced.env("GIT_TRACE_PACKET", "1"));
    let stderr = text(&traced.stderr);
    assert!(traced.status.success(), "{stderr}");
    assert!(stderr.contains("< version 2"), "{stderr}");
    assert!(
        text(&traced.stdout).starts_with(&served),
        "{}",
        text(&traced.stdout)
    );

    // 3. A land, then pull --rebase: the clone converges on the served main.
    let moves = site.main_moves();
    site.commit(work, "real.txt", b"over a real sshd\n");
    let pushed = site.git_via(&sshd.ssh, work, &["push", "origin", LAND_MAIN]);
    let stderr = text(&pushed.stderr);
    assert!(pushed.status.success(), "{stderr}\n{}", sshd.log());
    assert!(stderr.contains(LANDED), "{stderr}");
    assert_eq!(site.main_moves(), moves + 1);
    git(work, &["pull", "--quiet", "--rebase"]);
    site.assert_converged(work);
    assert_eq!(site.rev(work, "origin/main"), served_main(&site));

    // 4. A push outside the grant: refused by the hook, nothing moved.
    let before = site.refs();
    let refused = site.git_via(
        &sshd.ssh,
        work,
        &["push", "origin", "HEAD:refs/heads/other"],
    );
    let stderr = text(&refused.stderr);
    assert!(!refused.status.success(), "{stderr}");
    assert!(stderr.contains("E_POLICY_DENIED"), "{stderr}");
    assert_eq!(site.refs(), before);

    // 5. A shell command: the forced command runs serve, which refuses it.
    let shell = sshd.shell("id");
    assert!(!shell.status.success(), "{shell:?}");
    let stderr = text(&shell.stderr);
    assert!(stderr.contains("E_INVALID_ARG"), "{stderr}");
    assert!(!text(&shell.stdout).contains("uid="), "{shell:?}");

    // 6. Every clone and the store are clean.
    for clone in &clones {
        site.assert_fsck(clone);
    }
    site.assert_clean();
}
