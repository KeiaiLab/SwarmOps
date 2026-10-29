//! The store's git mirror (M13): its layout and marker, its lock, its rebuild, the staging
//! branch and the stale git lock files. Written by the M13 plan's task C4. And a damaged object
//! of the mirror, which the fetch's import refuses.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs::{self, File};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use camino::Utf8Path;
use swarmops_git::mirror::{Mirror, MirrorError, MirrorLock, Rebuilt, STALE_LOCK_AGE};
use swarmops_git::transport::{GitCli, RejectReason, Transport, TransportError};
use swarmops_git::{
    ConvertError, GitOid, GitSink, GixSink, GixSource, ImportError, ImportRequest, RawCommit,
    RawIdent, RawKind, RawTree, RawTreeEntry, SourceError, import,
};
use swarmops_repo::Depth;
use swarmops_testkit::repo::new_repo;
use tempfile::TempDir;

/// The marker a mirror of this layout holds.
const MARKER: &str = "swarmops-git-mirror 1\n";

/// The remote and branch the ref tests use.
const REMOTE: &str = "origin";
const BRANCH: &str = "main";

/// A temp dir standing in for a store's root: the mirror needs no more of a store.
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

    fn open(&self) -> Mirror {
        Mirror::open(self.path()).unwrap_or_else(|error| panic!("open the mirror: {error}"))
    }

    /// The names in `git/`, sorted.
    fn listing(&self) -> BTreeSet<String> {
        let entries = fs::read_dir(self.git()).expect("git/");
        entries
            .map(|entry| entry.expect("an entry of git/").file_name())
            .map(|name| name.into_string().expect("UTF-8 names"))
            .collect()
    }
}

/// A mirror made under its lock: the mirror and the held lock.
fn made(root: &Root) -> (Mirror, MirrorLock) {
    let mirror = root.open();
    let lock = mirror.lock().expect("the lock is free");
    assert_eq!(mirror.ensure(&lock).expect("ensure"), Rebuilt::Created);
    (mirror, lock)
}

/// Two blobs written into the mirror: objects a ref can name.
fn two_blobs(mirror: &Mirror) -> [GitOid; 2] {
    let mut sink = GixSink::open(mirror.git_dir()).expect("open the mirror");
    let first = sink.write_object(RawKind::Blob, b"first\n").expect("write");
    let second = sink
        .write_object(RawKind::Blob, b"second\n")
        .expect("write");
    sink.sync(&[first, second]).expect("sync");
    [first, second]
}

/// Writes an empty lock file at `name` (relative to `mirror.git`) whose mtime is `age` ago.
fn lock_file(mirror: &Mirror, name: &str, age: Duration) -> PathBuf {
    let path = mirror.git_dir().join(name);
    fs::create_dir_all(path.parent().expect("a parent")).expect("make its directory");
    let file = File::create(&path).expect("make the lock file");
    let mtime = SystemTime::now() - age;
    file.set_modified(mtime).expect("set its mtime");
    path
}

/// The value of `core.<key>` in the mirror's `config`, as written (keys compared without case).
fn core_value(git_dir: &Path, key: &str) -> Option<String> {
    let config = fs::read_to_string(git_dir.join("config")).expect("read config");
    let mut in_core = false;
    let mut found = None;
    for line in config.lines().map(str::trim) {
        if line.starts_with('[') {
            in_core = line.eq_ignore_ascii_case("[core]");
            continue;
        }
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        if in_core && name.trim().eq_ignore_ascii_case(key) {
            found = Some(value.trim().to_owned());
        }
    }
    found
}

/// The first use makes `git/`, `git/lock` and the marker, and nothing more; `ensure` under the
/// lock makes `mirror.git`, a bare SHA-1 repository the import's reader opens, and keeps it the
/// next time.
#[test]
fn mirror_is_made_on_first_use_with_its_marker() {
    let root = Root::new();
    let mirror = root.open();
    let listing = BTreeSet::from(["MIRROR".to_owned(), "lock".to_owned()]);
    assert_eq!(root.listing(), listing);
    let marker = fs::read_to_string(root.git().join("MIRROR")).expect("read MIRROR");
    assert_eq!(marker, MARKER);
    assert_eq!(mirror.git_dir(), root.git().join("mirror.git"));

    // Made under the lock, then kept.
    let lock = mirror.lock().expect("the lock is free");
    assert_eq!(mirror.ensure(&lock).expect("ensure"), Rebuilt::Created);
    GixSource::open(mirror.git_dir()).expect("the import's reader opens it");
    assert_eq!(
        core_value(mirror.git_dir(), "bare").as_deref(),
        Some("true")
    );
    assert_eq!(
        core_value(mirror.git_dir(), "repositoryformatversion").as_deref(),
        Some("0")
    );
    assert_eq!(
        core_value(mirror.git_dir(), "logallrefupdates").as_deref(),
        Some("false")
    );
    assert_eq!(mirror.ensure(&lock).expect("ensure"), Rebuilt::Kept);

    // A second open leaves the marker as it is.
    drop(lock);
    root.open();
    let marker = fs::read_to_string(root.git().join("MIRROR")).expect("read MIRROR");
    assert_eq!(marker, MARKER);
}

/// A marker of another value is left by `open` for `ensure`, which renames the mirror aside as
/// `mirror.git.broken-<unix ms>`, never deleting it, makes an empty one and writes the marker.
#[test]
fn unknown_marker_is_set_aside_and_rebuilt() {
    let root = Root::new();
    let (mirror, lock) = made(&root);
    fs::write(mirror.git_dir().join("left-here"), b"kept").expect("mark the old mirror");
    drop(lock);
    fs::write(root.git().join("MIRROR"), "swarmops-git-mirror 2\n").expect("write MIRROR");

    // `open` leaves an unknown value as it is.
    let mirror = root.open();
    let marker = fs::read_to_string(root.git().join("MIRROR")).expect("read MIRROR");
    assert_eq!(marker, "swarmops-git-mirror 2\n");

    // `ensure` sets the old one aside and makes a new one.
    let lock = mirror.lock().expect("the lock is free");
    let Rebuilt::SetAside { name } = mirror.ensure(&lock).expect("ensure") else {
        panic!("the mirror is set aside");
    };
    let ms = name.strip_prefix("mirror.git.broken-").expect("its name");
    assert!(ms.parse::<u64>().is_ok(), "{name}");
    let kept = fs::read(root.git().join(&name).join("left-here")).expect("the old mirror");
    assert_eq!(kept, b"kept");
    assert!(!mirror.git_dir().join("left-here").exists());
    GixSource::open(mirror.git_dir()).expect("the new one opens");
    let marker = fs::read_to_string(root.git().join("MIRROR")).expect("read MIRROR");
    assert_eq!(marker, MARKER);
    assert_eq!(mirror.ensure(&lock).expect("ensure"), Rebuilt::Kept);
}

/// The lock is not re-entrant and is never waited for: a second handle, in this process or
/// through a second `Mirror`, is refused at once until the first is dropped.
#[test]
fn second_lock_is_busy() {
    let root = Root::new();
    let mirror = root.open();
    let held = mirror.lock().expect("the lock is free");
    assert!(matches!(mirror.lock(), Err(MirrorError::Busy)));
    assert!(matches!(root.open().lock(), Err(MirrorError::Busy)));
    drop(held);
    mirror.lock().expect("free again once dropped");
}

/// A git lock file older than `STALE_LOCK_AGE` (a ref's, `packed-refs.lock`) is a dead process's:
/// the sweep under the mirror lock removes it and names it, relative to `mirror.git`.
#[test]
fn stale_ref_lock_is_removed_under_the_mirror_lock() {
    let root = Root::new();
    let (mirror, lock) = made(&root);
    let old = STALE_LOCK_AGE + Duration::from_mins(1);
    let tracking = lock_file(&mirror, "refs/remotes/origin/main.lock", old);
    let packed = lock_file(&mirror, "packed-refs.lock", old);

    let removed = mirror.sweep_git_locks(&lock).expect("sweep");
    let names = vec![
        "packed-refs.lock".to_owned(),
        "refs/remotes/origin/main.lock".to_owned(),
    ];
    assert_eq!(removed, names);
    assert!(!tracking.exists() && !packed.exists());
    assert_eq!(
        mirror.sweep_git_locks(&lock).expect("sweep"),
        Vec::<String>::new()
    );
}

/// A git lock file younger than `STALE_LOCK_AGE` may be a live git child's: the sweep removes
/// nothing and fails with its name, and the file stays.
#[test]
fn young_git_lock_is_busy() {
    let root = Root::new();
    let (mirror, lock) = made(&root);
    let old = STALE_LOCK_AGE + Duration::from_mins(1);
    let stale = lock_file(&mirror, "index.lock", old);
    let young = lock_file(&mirror, "refs/heads/push/origin/main.lock", Duration::ZERO);

    let error = mirror.sweep_git_locks(&lock).expect_err("busy");
    let MirrorError::GitLockBusy { file } = error else {
        panic!("GitLockBusy, not {error:?}");
    };
    assert_eq!(file, "refs/heads/push/origin/main.lock");
    assert!(young.exists() && stale.exists());
}

/// `open` never looks into `mirror.git`, even a broken one; `ensure` under the lock sets it aside
/// once, and keeps the one it made after that.
#[test]
fn open_never_touches_mirror_git() {
    let root = Root::new();
    let (mirror, lock) = made(&root);
    drop(lock);
    fs::remove_file(mirror.git_dir().join("HEAD")).expect("break the mirror");
    fs::remove_dir_all(mirror.git_dir().join("objects")).expect("break the mirror");
    let before = root.listing();

    let mirror = root.open();
    assert_eq!(root.listing(), before);
    assert!(!mirror.git_dir().join("HEAD").exists());

    let lock = mirror.lock().expect("the lock is free");
    assert!(matches!(
        mirror.ensure(&lock).expect("ensure"),
        Rebuilt::SetAside { .. }
    ));
    assert_eq!(mirror.ensure(&lock).expect("ensure"), Rebuilt::Kept);
    let broken = root.listing();
    let broken = broken
        .iter()
        .filter(|name| name.starts_with("mirror.git.broken-"));
    assert_eq!(broken.count(), 1);
}

/// `set_tracking` moves `refs/remotes/<remote>/<branch>` by compare-and-swap: from absent, then
/// from its value; from any other value it fails and leaves the ref as it was.
#[test]
fn tracking_ref_moves_only_from_the_expected_value() {
    let root = Root::new();
    let (mirror, _lock) = made(&root);
    let [first, second] = two_blobs(&mirror);
    assert_eq!(mirror.tracking(REMOTE, BRANCH).expect("read"), None);

    // From absent, then from its value.
    mirror
        .set_tracking(REMOTE, BRANCH, None, first)
        .expect("create");
    assert_eq!(mirror.tracking(REMOTE, BRANCH).expect("read"), Some(first));
    mirror
        .set_tracking(REMOTE, BRANCH, Some(first), second)
        .expect("move");
    assert_eq!(mirror.tracking(REMOTE, BRANCH).expect("read"), Some(second));

    // From another value: refused, the ref as it was, and no lock left.
    for stale in [None, Some(first)] {
        let refused = mirror.set_tracking(REMOTE, BRANCH, stale, first);
        assert!(refused.is_err(), "{refused:?}");
        assert_eq!(mirror.tracking(REMOTE, BRANCH).expect("read"), Some(second));
    }
    let lock = mirror.git_dir().join("refs/remotes/origin/main.lock");
    assert!(!lock.exists());
}

/// The tracking ref's move is durable as the export's branch move is: the value written to the
/// lock is synced before the rename, and the ref's directory, with each directory the move made,
/// after it.
#[cfg(feature = "test-hooks")]
#[test]
fn tracking_ref_move_is_synced() {
    let root = Root::new();
    let (mirror, _lock) = made(&root);
    let [first, _] = two_blobs(&mirror);

    let (mut sink, synced) = GixSink::open_recording_syncs(mirror.git_dir()).expect("open");
    let full = "refs/remotes/origin/main";
    let moved = sink.cas_ref(full, None, Some(first)).expect("create");
    assert_eq!(moved, swarmops_git::BranchUpdate::Applied);
    let synced: BTreeSet<PathBuf> = synced.lock().expect("the record").iter().cloned().collect();
    let expected = [
        "refs/remotes/origin/main.lock",
        "refs/remotes/origin",
        "refs/remotes",
        "refs",
    ];
    let expected: BTreeSet<PathBuf> = expected.iter().map(PathBuf::from).collect();
    assert_eq!(synced, expected);
}

/// The staging branch `refs/heads/push/<remote>/<branch>` is seeded to a value, moved to
/// another, and deleted by `None`, packed or loose; seeding `None` when it is absent does nothing.
#[test]
fn seed_staging_to_none_deletes_it() {
    let root = Root::new();
    let (mirror, _lock) = made(&root);
    let [first, second] = two_blobs(&mirror);
    let staged = || {
        let sink = GixSink::open(mirror.git_dir()).expect("open");
        sink.branch("push/origin/main").expect("read")
    };

    // Seeded, moved, deleted.
    let name = mirror
        .seed_staging(REMOTE, BRANCH, Some(first))
        .expect("seed");
    assert_eq!(name, "push/origin/main");
    assert_eq!(staged(), Some(first));
    mirror
        .seed_staging(REMOTE, BRANCH, Some(second))
        .expect("seed");
    assert_eq!(staged(), Some(second));
    mirror.seed_staging(REMOTE, BRANCH, None).expect("delete");
    assert_eq!(staged(), None);
    mirror.seed_staging(REMOTE, BRANCH, None).expect("absent");

    // A packed one is deleted from `packed-refs`, its neighbours kept.
    let packed = format!(
        "# pack-refs with: peeled fully-peeled sorted \n\
         {first} refs/heads/push/origin/main\n\
         {second} refs/remotes/origin/main\n"
    );
    fs::write(mirror.git_dir().join("packed-refs"), packed).expect("write packed-refs");
    assert_eq!(staged(), Some(first));
    mirror.seed_staging(REMOTE, BRANCH, None).expect("delete");
    assert_eq!(staged(), None);
    assert_eq!(mirror.tracking(REMOTE, BRANCH).expect("read"), Some(second));
}

/// `remove_remote` deletes the remote's tracking refs, tags and staging branches, and counts
/// them, and its incoming refs; another remote's refs stay.
#[test]
fn remove_remote_drops_its_refs_only() {
    let root = Root::new();
    let (mirror, _lock) = made(&root);
    let [first, _] = two_blobs(&mirror);
    let mut sink = GixSink::open(mirror.git_dir()).expect("open");
    for full in [
        "refs/remotes/origin/main",
        "refs/remotes/origin/topic/a",
        "refs/remote-tags/origin/v1",
        "refs/heads/push/origin/main",
        "refs/incoming/origin/heads/main",
        "refs/remotes/upstream/main",
    ] {
        let made = sink.cas_ref(full, None, Some(first)).expect(full);
        assert_eq!(made, swarmops_git::BranchUpdate::Applied, "{full}");
    }

    // The incoming refs go too, uncounted: a fetch's scratch, not the remote's refs.
    assert_eq!(mirror.remove_remote(REMOTE).expect("remove"), 4);
    assert!(mirror.incoming(REMOTE).expect("incoming").is_empty());
    assert_eq!(mirror.tracking(REMOTE, BRANCH).expect("read"), None);
    assert_eq!(
        mirror.tracking("upstream", BRANCH).expect("read"),
        Some(first)
    );
    assert_eq!(mirror.remove_remote(REMOTE).expect("remove"), 0);
}

/// `branches` and `tags` list one remote's tracking refs and tags by their short names, and
/// nothing of another remote or of the staging branches.
#[test]
fn branches_and_tags_list_one_remote() {
    let root = Root::new();
    let (mirror, _lock) = made(&root);
    let [first, second] = two_blobs(&mirror);
    let mut sink = GixSink::open(mirror.git_dir()).expect("open");
    for (full, oid) in [
        ("refs/remotes/origin/main", first),
        ("refs/remotes/origin/topic/a", second),
        ("refs/remote-tags/origin/v1", first),
        ("refs/heads/push/origin/main", second),
        ("refs/remotes/upstream/main", second),
    ] {
        let made = sink.cas_ref(full, None, Some(oid)).expect(full);
        assert_eq!(made, swarmops_git::BranchUpdate::Applied, "{full}");
    }

    let branches = mirror.branches(REMOTE).expect("list the branches");
    let expected = [("main".to_owned(), first), ("topic/a".to_owned(), second)];
    assert_eq!(branches, expected.into_iter().collect());
    let tags = mirror.tags(REMOTE).expect("list the tags");
    assert_eq!(tags, [("v1".to_owned(), first)].into_iter().collect());
    assert!(mirror.branches("none").expect("list").is_empty());
}

/// `mirror.git`'s `core.hooksPath` names `hooks-none`, an empty directory in it, by a path
/// relative to the repository, so that no host path is written into the store and git runs no
/// hook there.
#[test]
fn hooks_path_points_at_an_empty_directory() {
    let root = Root::new();
    let (mirror, _lock) = made(&root);
    let hooks = core_value(mirror.git_dir(), "hooksPath").expect("core.hooksPath is set");
    assert_eq!(hooks, "hooks-none");
    let dir = mirror.git_dir().join(&hooks);
    let entries = fs::read_dir(&dir).expect("hooks-none is a directory");
    assert_eq!(entries.count(), 0);
}

/// A repository gix cannot make is named by its own name, never by its host path: gix's text,
/// which carries the absolute path, does not reach the error.
#[cfg(unix)]
#[test]
fn create_bare_error_names_no_host_path() {
    let root = Root::new();
    let git = root.git();
    fs::create_dir(&git).expect("make git/");

    // 1. A dangling symlink where the repository goes: gix finds no directory and cannot make
    // one, as root too.
    let path = git.join("mirror.git");
    std::os::unix::fs::symlink(git.join("absent"), &path).expect("make the symlink");

    // 2. The error names mirror.git and no part of the host path.
    let error = GixSink::create_bare(&path).expect_err("the symlink is in the way");
    let text = error.to_string();
    let host = root.dir.path().display().to_string();
    assert!(!text.contains(&host), "{text}");
    assert!(text.contains("mirror.git"), "{text}");
}

/// Sets each `(full name, value)` ref of the mirror from absent.
fn set_refs(mirror: &Mirror, refs: &[(&str, GitOid)]) {
    let mut sink = GixSink::open(mirror.git_dir()).expect("open");
    for (full, oid) in refs {
        let made = sink.cas_ref(full, None, Some(*oid)).expect(full);
        assert_eq!(made, swarmops_git::BranchUpdate::Applied, "{full}");
    }
}

/// `promote` moves each tracking ref and tag to its incoming value, makes those that are new,
/// deletes those with no incoming value, and leaves the incoming refs and other remotes' refs.
#[test]
fn promote_moves_and_prunes_tracking_refs() {
    let root = Root::new();
    let (mirror, _lock) = made(&root);
    let [first, second] = two_blobs(&mirror);
    set_refs(
        &mirror,
        &[
            ("refs/remotes/origin/main", first),
            ("refs/remotes/origin/gone", first),
            ("refs/remote-tags/origin/old", first),
            ("refs/remotes/upstream/main", first),
            ("refs/incoming/origin/heads/main", second),
            ("refs/incoming/origin/heads/fresh", first),
            ("refs/incoming/origin/tags/v1", second),
        ],
    );
    let incoming = mirror.incoming(REMOTE).expect("incoming");
    let expected = [
        ("heads/fresh".to_owned(), first),
        ("heads/main".to_owned(), second),
        ("tags/v1".to_owned(), second),
    ];
    assert_eq!(incoming, expected.into_iter().collect());

    // Promoted from what was read: main moved, fresh and v1 made, gone and old deleted.
    let branches = mirror.branches(REMOTE).expect("branches");
    let tags = mirror.tags(REMOTE).expect("tags");
    let promoted = mirror.promote(REMOTE, &branches, &tags).expect("promote");
    assert_eq!((promoted.moved, promoted.deleted), (3, 2));
    let branches = [("fresh".to_owned(), first), ("main".to_owned(), second)];
    assert_eq!(
        mirror.branches(REMOTE).expect("branches"),
        branches.into_iter().collect()
    );
    let tags = [("v1".to_owned(), second)];
    assert_eq!(
        mirror.tags(REMOTE).expect("tags"),
        tags.into_iter().collect()
    );
    assert_eq!(mirror.incoming(REMOTE).expect("incoming").len(), 3);
    assert_eq!(
        mirror.tracking("upstream", BRANCH).expect("read"),
        Some(first)
    );
}

/// A second `promote` finds every tracking ref at its incoming value: nothing moves.
#[test]
fn promote_twice_moves_nothing() {
    let root = Root::new();
    let (mirror, _lock) = made(&root);
    let [first, second] = two_blobs(&mirror);
    set_refs(
        &mirror,
        &[
            ("refs/remotes/origin/main", first),
            ("refs/incoming/origin/heads/main", second),
        ],
    );
    let promote = || {
        let branches = mirror.branches(REMOTE).expect("branches");
        let tags = mirror.tags(REMOTE).expect("tags");
        mirror.promote(REMOTE, &branches, &tags).expect("promote")
    };

    let once = promote();
    assert_eq!((once.moved, once.deleted), (1, 0));
    let twice = promote();
    assert_eq!((twice.moved, twice.deleted), (0, 0));
    assert_eq!(mirror.tracking(REMOTE, BRANCH).expect("read"), Some(second));
}

/// A tracking ref that moved after the caller read it is refused, by compare-and-swap, and
/// stays where it was moved to.
#[test]
fn promote_refuses_a_tracking_ref_moved_meanwhile() {
    let root = Root::new();
    let (mirror, _lock) = made(&root);
    let [first, second] = two_blobs(&mirror);
    set_refs(
        &mirror,
        &[
            ("refs/remotes/origin/main", first),
            ("refs/incoming/origin/heads/main", second),
        ],
    );
    let branches = mirror.branches(REMOTE).expect("branches");
    let tags = mirror.tags(REMOTE).expect("tags");

    // Something other than SwarmOps moves the ref after the read.
    let mut sink = GixSink::open(mirror.git_dir()).expect("open");
    let third = sink.write_object(RawKind::Blob, b"third\n").expect("write");
    sink.sync(&[third]).expect("sync");
    let moved = sink
        .cas_ref("refs/remotes/origin/main", Some(first), Some(third))
        .expect("move");
    assert_eq!(moved, swarmops_git::BranchUpdate::Applied);

    let refused = mirror.promote(REMOTE, &branches, &tags);
    assert!(matches!(refused, Err(MirrorError::Sink(_))), "{refused:?}");
    assert_eq!(mirror.tracking(REMOTE, BRANCH).expect("read"), Some(third));
}

/// How a test damages a loose object's file.
#[derive(Clone, Copy, Debug)]
enum Damage {
    /// Bytes that do not inflate.
    Garbled,
    /// Another loose object's file in its place, as a disk or a copy can leave it: it inflates,
    /// to bytes that hash to the other object's id.
    Swapped,
}

/// The file of the loose object `oid` in the git directory `git_dir`.
fn loose(git_dir: &Path, oid: GitOid) -> PathBuf {
    let hex = oid.to_string();
    git_dir.join("objects").join(&hex[..2]).join(&hex[2..])
}

/// The user's git as a transport, in an environment of its own: the caller's `PATH` and git
/// under test (`SWARMOPS_GIT`), `home` for an empty `HOME`, and no system or global
/// configuration.
fn git_cli(home: &Path) -> GitCli {
    let mut env: Vec<(OsString, OsString)> = vec![
        ("PATH".into(), std::env::var_os("PATH").unwrap_or_default()),
        ("HOME".into(), home.into()),
        ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
        ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
    ];
    let git = swarmops_testkit::git::GIT_ENV;
    if let Some(binary) = std::env::var_os(git) {
        env.push((git.into(), binary));
    }
    GitCli::find_with_env(env).expect("find git")
}

/// The objects of a commit of one file, written loose into a mirror, and one blob more.
struct OneFile {
    blob: GitOid,
    tree: GitOid,
    commit: GitOid,
    /// A blob no ref reaches: the bytes a swapped file takes.
    other: GitOid,
}

impl OneFile {
    /// Writes them into `mirror`, synced.
    fn write(mirror: &Mirror) -> OneFile {
        let mut sink = GixSink::open(mirror.git_dir()).expect("open the mirror");
        let mut write = |kind, bytes: &[u8]| sink.write_object(kind, bytes).expect("write");
        let blob = write(RawKind::Blob, b"damaged\n");
        let other = write(RawKind::Blob, b"other\n");
        let entry = RawTreeEntry::new(b"100644".to_vec(), b"a.txt".to_vec(), blob);
        let tree = write(RawKind::Tree, &RawTree::new(vec![entry]).render());
        let ident = RawIdent::new(
            b"A".to_vec(),
            b"a@a.invalid".to_vec(),
            1_700_000_000,
            0,
            false,
        );
        let message = b"One file\n".to_vec();
        let commit = RawCommit::new(tree, Vec::new(), ident.clone(), ident, Vec::new(), message);
        let commit = write(RawKind::Commit, &commit.render());
        sink.sync(&[blob, other, tree, commit]).expect("sync");
        OneFile {
            blob,
            tree,
            commit,
            other,
        }
    }

    /// Damages the loose file of `oid` in `mirror` as `damage` says.
    fn damage(&self, mirror: &Mirror, oid: GitOid, damage: Damage) {
        let file = loose(mirror.git_dir(), oid);
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).expect("chmod");
        match damage {
            Damage::Garbled => fs::write(&file, b"garbage").expect("garble it"),
            Damage::Swapped => {
                fs::copy(loose(mirror.git_dir(), self.other), &file).expect("swap it");
            }
        }
    }
}

/// An object of the mirror damaged on disk where the fetch's closure check does not read it, a
/// blob garbled or another's, or the tip commit's file another object's (which the check then
/// takes for that object), passes that check, which finds missing objects and checks no object
/// against its id (git 2.43); the fetch's import of the promoted tracking ref then refuses it and
/// records nothing. Its reader checks each object it reads against its id, so a blob is
/// `Corrupt`, naming it; the tip's header, read before its bytes, says it is a blob, so the tip
/// is no commit.
#[test]
fn a_damaged_object_is_refused_by_the_fetch_s_import() {
    let cases = [
        ("blob", Damage::Garbled),
        ("blob", Damage::Swapped),
        ("commit", Damage::Swapped),
    ];
    for (what, damage) in cases {
        // 1. A commit of one file, as `origin`'s incoming `main`; one of its objects damaged.
        let root = Root::new();
        let (mirror, _lock) = made(&root);
        let objects = OneFile::write(&mirror);
        set_refs(
            &mirror,
            &[("refs/incoming/origin/heads/main", objects.commit)],
        );
        let damaged = if what == "blob" {
            objects.blob
        } else {
            objects.commit
        };
        objects.damage(&mirror, damaged, damage);

        // 2. The closure check, then the promotion, as a fetch makes them. The check finds
        //    missing objects: a damaged one may pass it, and these do with git 2.43.
        let home = TempDir::new().expect("a temp dir");
        let closure = git_cli(home.path()).closed(mirror.git_dir(), &[objects.commit]);
        assert!(
            matches!(
                closure,
                Ok(())
                    | Err(TransportError::Rejected {
                        reason: RejectReason::BadObjects,
                        ..
                    })
            ),
            "{what} {damage:?}: {closure:?}"
        );
        let branches = mirror.branches(REMOTE).expect("branches");
        let tags = mirror.tags(REMOTE).expect("tags");
        mirror.promote(REMOTE, &branches, &tags).expect("promote");

        // 3. The fetch's import of the tracking ref: refused, naming the object; nothing recorded.
        let (_dir, _store, repo) = new_repo();
        let source = GixSource::open(mirror.git_dir()).expect("open the mirror to read");
        let request = ImportRequest::objects_only("refs/remotes/origin/main");
        let refused = import(&repo, &source, &request).expect_err("the damage is refused");
        let as_expected = match &refused {
            ImportError::Convert(ConvertError::Source(SourceError::Corrupt { oid, .. })) => {
                what == "blob" && *oid == damaged
            }
            ImportError::Source(SourceError::NotACommit { kind, .. }) => {
                what == "commit" && *kind == RawKind::Blob
            }
            _ => false,
        };
        assert!(as_expected, "{what} {damage:?}: {refused:?}");
        assert_eq!(
            repo.info().expect("info").changesets,
            0,
            "{what} {damage:?}"
        );
        let verified = repo.verify(Depth::Deep).expect("verify");
        assert_eq!(verified.problems, [], "{what} {damage:?}");
    }
}

/// A tree or a commit of the mirror that the closure check cannot parse where it walks, its file
/// garbled, fails the check: [`RejectReason::BadObjects`], so no tracking ref moves to it.
#[test]
fn a_garbled_tree_or_commit_fails_the_closure_check() {
    for what in ["tree", "commit"] {
        let root = Root::new();
        let (mirror, _lock) = made(&root);
        let objects = OneFile::write(&mirror);
        let damaged = if what == "tree" {
            objects.tree
        } else {
            objects.commit
        };
        objects.damage(&mirror, damaged, Damage::Garbled);

        let home = TempDir::new().expect("a temp dir");
        let closure = git_cli(home.path()).closed(mirror.git_dir(), &[objects.commit]);
        assert!(
            matches!(
                closure,
                Err(TransportError::Rejected {
                    reason: RejectReason::BadObjects,
                    ..
                })
            ),
            "{what}: {closure:?}"
        );
    }
}
