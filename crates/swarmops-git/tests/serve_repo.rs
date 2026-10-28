//! The serving repository (`swarmops_git::serve::repo`, M14 design §0.2 and §2.4): its layout,
//! the hook replaced whole, a broken one set aside, the lock waited for and then `Busy`, and the
//! compare-and-swap of its refs. Written by the M14 plan's task S1.

use std::collections::BTreeSet;
use std::fs;
use std::io::{BufRead as _, BufReader};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use camino::Utf8Path;
use swarmops_git::serve::repo::{HOOK_SCRIPT, SERVE_MARKER, ServeError, ServeRepo};
use swarmops_git::{BranchUpdate, GitSink, GixSink, GixSource, RawKind, Stored};
use tempfile::TempDir;

/// A short wait, which a held lock outlasts.
const SHORT: Duration = Duration::from_millis(300);

/// A wait no test reaches when the lock is let go.
const LONG: Duration = Duration::from_secs(60);

/// A temp dir standing in for a store's root: the serving repository needs no more of a store.
struct Root {
    dir: TempDir,
}

impl Root {
    fn new() -> Root {
        Root {
            dir: tempfile::tempdir().expect("make a temp dir"),
        }
    }

    fn path(&self) -> &Utf8Path {
        Utf8Path::from_path(self.dir.path()).expect("UTF-8 temp paths")
    }

    /// `<root>/git`.
    fn git(&self) -> PathBuf {
        self.dir.path().join("git")
    }

    /// `<root>/git/serve-hooks/proc-receive`.
    fn hook(&self) -> PathBuf {
        self.git().join("serve-hooks").join("proc-receive")
    }

    fn open(&self) -> ServeRepo {
        ServeRepo::open(self.path()).unwrap_or_else(|error| panic!("open serve.git: {error}"))
    }

    /// The names in `git/`, sorted.
    fn listing(&self) -> BTreeSet<String> {
        let entries = fs::read_dir(self.git()).expect("git/");
        entries
            .map(|entry| entry.expect("an entry of git/").file_name())
            .map(|name| name.into_string().expect("UTF-8 names"))
            .collect()
    }

    /// The names in `git/` that start with `serve.git.broken-`.
    fn set_aside(&self) -> Vec<String> {
        let listing = self.listing().into_iter();
        listing
            .filter(|name| name.starts_with("serve.git.broken-"))
            .collect()
    }
}

/// The first open makes `git/serve.git` (bare, SHA-1, `HEAD` naming `refs/heads/main`), the
/// marker, the lock file and the hook (its constant bytes, mode 0755), and nothing else; a
/// second open keeps all of it.
#[test]
fn open_makes_the_serving_repository() {
    let root = Root::new();
    let repo = root.open();
    let listing = ["SERVE", "serve-hooks", "serve.git", "serve.lock"];
    let listing: BTreeSet<String> = listing.iter().map(|name| (*name).to_owned()).collect();
    assert_eq!(root.listing(), listing);
    assert_eq!(repo.git_dir(), root.path().join("git/serve.git"));
    assert_eq!(repo.hooks_dir(), root.path().join("git/serve-hooks"));

    // The repository: bare, SHA-1, HEAD on main, no ref.
    let git_dir = repo.git_dir().as_std_path();
    GixSource::open(git_dir).expect("the import's reader opens it");
    let head = fs::read_to_string(git_dir.join("HEAD")).expect("read HEAD");
    assert_eq!(head, "ref: refs/heads/main\n");
    assert_eq!(git(git_dir, &["rev-parse", "--is-bare-repository"]), "true");
    assert_eq!(git(git_dir, &["rev-parse", "--show-object-format"]), "sha1");
    assert!(repo.refs().expect("refs").is_empty());

    // The marker and the hook.
    let marker = fs::read(root.git().join("SERVE")).expect("read SERVE");
    assert_eq!(marker, SERVE_MARKER);
    let hook = fs::read(root.hook()).expect("read the hook");
    assert_eq!(hook, HOOK_SCRIPT.as_bytes());
    let mode = fs::metadata(root.hook())
        .expect("the hook")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o755);

    // A second open keeps it: the same repository, no broken one.
    let inode = fs::metadata(git_dir).expect("serve.git").ino();
    let again = root.open();
    let inode_again = fs::metadata(again.git_dir()).expect("serve.git").ino();
    assert_eq!(inode_again, inode);
    assert_eq!(root.set_aside(), Vec::<String>::new());
}

/// A hook file whose bytes or mode differ is replaced whole, by a rename: a new file, with the
/// constant bytes and mode 0755.
#[test]
fn a_changed_hook_is_replaced_atomically() {
    let root = Root::new();
    root.open();

    // 1. Other bytes.
    let before = fs::metadata(root.hook()).expect("the hook").ino();
    fs::write(root.hook(), "#!/bin/sh\nexit 0\n").expect("change the hook");
    root.open();
    let after = fs::metadata(root.hook()).expect("the hook");
    assert_eq!(fs::read(root.hook()).expect("read"), HOOK_SCRIPT.as_bytes());
    assert_ne!(
        after.ino(),
        before,
        "the hook was written in place, not replaced"
    );

    // 2. Its bytes, another mode.
    let not_executable = fs::Permissions::from_mode(0o644);
    fs::set_permissions(root.hook(), not_executable).expect("chmod");
    root.open();
    let mode = fs::metadata(root.hook())
        .expect("the hook")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o755);

    // 3. Gone.
    fs::remove_file(root.hook()).expect("remove the hook");
    root.open();
    assert_eq!(fs::read(root.hook()).expect("read"), HOOK_SCRIPT.as_bytes());
    let aside = root.set_aside();
    assert_eq!(
        aside,
        Vec::<String>::new(),
        "a hook alone sets nothing aside"
    );
}

/// A marker of another layout sets `serve.git` aside, never deleting it, and makes an empty one
/// under the known marker.
#[test]
fn an_unknown_marker_sets_the_repository_aside() {
    let root = Root::new();
    let repo = root.open();
    let blob = one_blob(&repo);
    fs::write(root.git().join("SERVE"), "swarmops-git-serve 2\n").expect("write SERVE");

    let repo = root.open();
    let aside = root.set_aside();
    assert_eq!(aside.len(), 1, "{aside:?}");
    let kept = root.git().join(&aside[0]).join("objects");
    assert!(kept.is_dir(), "the repository set aside is kept whole");
    assert_eq!(
        fs::read(root.git().join("SERVE")).expect("read"),
        SERVE_MARKER
    );
    let sink = GixSink::open(repo.git_dir().as_std_path()).expect("open");
    assert!(
        sink.stored(blob).expect("look") == Stored::Absent,
        "the new repository is empty"
    );
}

/// A repository gix cannot open is set aside and made again; a second break gets a name of its
/// own.
#[test]
fn a_broken_repository_is_set_aside_and_made_again() {
    let root = Root::new();
    let repo = root.open();
    for round in 1..=2 {
        fs::remove_dir_all(repo.git_dir().join("objects")).expect("break it");
        fs::write(repo.git_dir().join("HEAD"), "nonsense").expect("break HEAD");
        assert!(GixSink::open(repo.git_dir().as_std_path()).is_err());

        let repo = root.open();
        assert_eq!(root.set_aside().len(), round);
        GixSink::open(repo.git_dir().as_std_path()).expect("made again");
    }
}

/// The lock is held against a second taker, in this process and in another (`flock(1)`): it
/// waits up to the given wait, then gives `Busy`; a lock let go within the wait is taken.
#[test]
fn the_lock_waits_then_is_busy() {
    let root = Root::new();
    let repo = root.open();

    // 1. Held here: a short wait runs out, and no wait at all gives up at once.
    let held = repo.lock(Duration::ZERO).expect("the lock is free");
    let started = Instant::now();
    assert_eq!(repo.lock(SHORT).unwrap_err(), ServeError::Busy);
    assert!(
        started.elapsed() >= SHORT,
        "it waited {:?}",
        started.elapsed()
    );
    assert_eq!(repo.lock(Duration::ZERO).unwrap_err(), ServeError::Busy);

    // 2. Let go within the wait: taken.
    let letting_go = thread::spawn(move || {
        thread::sleep(SHORT);
        drop(held);
    });
    let taken = repo.lock(LONG).expect("taken once let go");
    letting_go.join().expect("the thread");
    drop(taken);

    // 3. Held by another process: busy until it ends.
    let mut holder = hold_in_another_process(&root.git().join("serve.lock"));
    assert_eq!(repo.lock(SHORT).unwrap_err(), ServeError::Busy);
    drop(holder.stdin.take());
    let ended = holder.wait().expect("reap the holder");
    assert!(ended.success(), "{ended}");
    repo.lock(LONG).expect("taken once the other process ended");
}

/// `set_ref` moves a ref by compare-and-swap: made from absent, a mismatch moves nothing and
/// names what is there, deleted from its value; `refs` lists what is there.
#[test]
fn set_ref_compares_and_swaps() {
    let root = Root::new();
    let repo = root.open();
    let [first, second] = [one_blob(&repo), other_blob(&repo)];
    let lock = repo.lock(Duration::ZERO).expect("the lock is free");
    let name = "refs/heads/feature/x";

    let made = repo.set_ref(&lock, name, None, Some(first)).expect("make");
    assert_eq!(made, BranchUpdate::Applied);
    let stale = repo.set_ref(&lock, name, None, Some(second)).expect("cas");
    assert_eq!(
        stale,
        BranchUpdate::Mismatch {
            actual: Some(first)
        }
    );
    let tag = repo
        .set_ref(&lock, "refs/tags/v1", None, Some(second))
        .expect("tag");
    assert_eq!(tag, BranchUpdate::Applied);
    let listed = repo.refs().expect("refs");
    let expected = [
        (name.to_owned(), first),
        ("refs/tags/v1".to_owned(), second),
    ];
    assert_eq!(listed, expected.into_iter().collect());

    let deleted = repo
        .set_ref(&lock, name, Some(first), None)
        .expect("delete");
    assert_eq!(deleted, BranchUpdate::Applied);
    assert!(!repo.refs().expect("refs").contains_key(name));
}

/// A blob written into `repo` and synced: an object a ref can name.
fn one_blob(repo: &ServeRepo) -> swarmops_git::GitOid {
    blob(repo, b"first\n")
}

/// Another blob.
fn other_blob(repo: &ServeRepo) -> swarmops_git::GitOid {
    blob(repo, b"second\n")
}

fn blob(repo: &ServeRepo, bytes: &[u8]) -> swarmops_git::GitOid {
    let mut sink = GixSink::open(repo.git_dir().as_std_path()).expect("open serve.git");
    let oid = sink.write_object(RawKind::Blob, bytes).expect("write");
    sink.sync(&[oid]).expect("sync");
    oid
}

/// A `flock(1)` process holding the lock file `lock` until its stdin closes, once it holds it.
fn hold_in_another_process(lock: &std::path::Path) -> Child {
    let mut child = Command::new("flock")
        .arg("--exclusive")
        .arg(lock)
        .args(["sh", "-c", "echo held; read -r line || true"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("run flock");
    let stdout = child.stdout.take().expect("its stdout");
    let mut line = String::new();
    BufReader::new(stdout)
        .read_line(&mut line)
        .expect("read flock's line");
    assert_eq!(line, "held\n", "flock did not take the lock");
    child
}

/// What `git <args>` prints in the git directory `git_dir`, trimmed, by the git `SWARMOPS_GIT`
/// names (else `git` on `PATH`), without any configuration.
fn git(git_dir: &std::path::Path, args: &[&str]) -> String {
    let program = std::env::var_os(swarmops_testkit::git::GIT_ENV).unwrap_or_else(|| "git".into());
    let mut command = Command::new(program);
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("LC_ALL", "C")
        .arg("--git-dir")
        .arg(git_dir)
        .args(args);
    let output = swarmops_testkit::child::run(&mut command);
    assert!(output.status.success(), "git {args:?}: {output:?}");
    String::from_utf8(output.stdout)
        .expect("UTF-8")
        .trim()
        .to_owned()
}
