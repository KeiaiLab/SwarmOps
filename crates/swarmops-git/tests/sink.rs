//! The repository the export writes, through gix: objects written as loose files that the
//! import's own reader reads back, made durable file by file and directory by directory, and a
//! branch moved by git's own lock protocol as a compare-and-swap; and `MemSink`, the in-memory
//! sink whose log the export's tests replay into the images a power loss could leave.

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use swarmops_git::{
    BranchUpdate, GitOid, GitSink, GitSource, GixSink, GixSource, RawCommit, RawIdent, RawKind,
    RawTree, RawTreeEntry, SinkError, SourceError, Stored, Unsupported, git_hash,
};
use swarmops_testkit::git::{self, GitFixture, Manifest};

use crate::support::{Keep, MemImage, MemSink, SinkCall, reaches_only_kept};

/// An id no object of these tests has.
const NOTHING: &str = "0123456789abcdef0123456789abcdef01234567";

fn oid(hex: &str) -> GitOid {
    hex.parse().unwrap_or_else(|error| panic!("{hex}: {error}"))
}

fn repo(fixture: &GitFixture) -> &Path {
    fixture
        .repo
        .as_deref()
        .expect("the fixture holds a repository")
}

fn manifest(fixture: &GitFixture) -> &Manifest {
    fixture
        .manifest
        .as_ref()
        .expect("the fixture has a manifest")
}

fn open(path: &Path) -> GixSink {
    GixSink::open(path).unwrap_or_else(|error| panic!("open {}: {error}", path.display()))
}

/// The loose file of `oid` in the repository whose git directory is `git_dir`.
fn loose_path(git_dir: &Path, oid: GitOid) -> PathBuf {
    let hex = oid.to_string();
    git_dir.join("objects").join(&hex[..2]).join(&hex[2..])
}

/// Cuts the file at `path` to 0 bytes, as a power loss can leave a file written and never
/// synced. git writes loose objects read-only.
fn cut(path: &Path) {
    fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
    fs::write(path, b"").unwrap();
}

/// The first three commits of `linear`, written into `sink` (their bytes alone, not their trees):
/// objects a branch can name.
fn three_commits(sink: &mut dyn GitSink) -> [GitOid; 3] {
    let fixture = git::fixture("linear");
    let commits = &manifest(&fixture).commits;
    [0, 1, 2].map(|at| {
        let written = sink.write_object(RawKind::Commit, &commits[at].raw);
        assert_eq!(written, Ok(oid(&commits[at].oid)));
        oid(&commits[at].oid)
    })
}

/// Every file under `<git_dir>/refs`, relative to `git_dir`.
fn ref_files(git_dir: &Path) -> BTreeSet<PathBuf> {
    let mut files = BTreeSet::new();
    let mut dirs = vec![git_dir.join("refs")];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                dirs.push(path);
            } else {
                files.insert(path.strip_prefix(git_dir).unwrap().to_owned());
            }
        }
    }
    files
}

/// `Err(RefConflict)` naming `name`.
fn in_the_way<T>(name: &str) -> Result<T, SinkError> {
    Err(SinkError::RefConflict {
        name: name.to_owned(),
    })
}

#[test]
fn a_written_object_reads_back_through_gix_source() {
    let fixture = git::fixture("linear");
    let manifest = manifest(&fixture);
    let first = &manifest.commits[0];
    let tree = manifest.trees[&first.tree].clone();
    let source = GixSource::open(repo(&fixture)).unwrap();
    // The first commit, its tree and its blobs, as the fixture holds them.
    let mut objects = vec![
        (oid(&first.oid), RawKind::Commit, first.raw.clone()),
        (oid(&first.tree), RawKind::Tree, tree.clone()),
    ];
    for entry in RawTree::parse(&tree).unwrap().entries {
        let (kind, bytes) = source.read(entry.oid).unwrap();
        let size = u64::try_from(bytes.len()).unwrap();
        assert_eq!(manifest.blobs.get(&entry.oid.to_string()), Some(&size));
        objects.push((entry.oid, kind, bytes));
    }
    let destination = git::empty_repository();
    let mut sink = open(&destination.dir);
    for (id, kind, bytes) in &objects {
        assert_eq!(sink.stored(*id), Ok(Stored::Absent), "{kind} {id}");
        assert_eq!(sink.write_object(*kind, bytes), Ok(*id), "{kind}");
        assert_eq!(sink.stored(*id), Ok(Stored::Loose), "{kind} {id}");
        assert!(loose_path(&destination.dir, *id).is_file(), "{kind} {id}");
        assert!(sink.read(*id) == Ok((*kind, bytes.clone())), "{kind} {id}");
    }
    // The import's reader, on the repository opened afresh, reads each as git wrote it.
    let reader = GixSource::open(&destination.dir).unwrap();
    for (id, kind, bytes) in &objects {
        assert!(
            reader.read(*id) == Ok((*kind, bytes.clone())),
            "{kind} {id}"
        );
    }
}

#[test]
fn stored_tells_loose_from_packed() {
    let fixture = git::fixture("loose_and_packed");
    let manifest = manifest(&fixture);
    let sink = open(repo(&fixture));
    let ids = (manifest.commits.iter().map(|commit| &commit.oid))
        .chain(manifest.trees.keys())
        .chain(manifest.blobs.keys());
    let mut seen = HashSet::new();
    for hex in ids {
        let id = oid(hex);
        let expected = if loose_path(repo(&fixture), id).is_file() {
            Stored::Loose
        } else {
            Stored::Packed
        };
        assert_eq!(sink.stored(id), Ok(expected), "{hex}");
        let (kind, bytes) = sink
            .read(id)
            .unwrap_or_else(|error| panic!("{hex}: {error}"));
        assert_eq!(git_hash(kind, &bytes), id, "{hex}");
        seen.insert(expected);
    }
    assert_eq!(seen, HashSet::from([Stored::Loose, Stored::Packed]));
    let nothing = oid(NOTHING);
    assert_eq!(sink.stored(nothing), Ok(Stored::Absent));
    assert_eq!(sink.read(nothing), Err(SinkError::Missing { oid: nothing }));
}

#[test]
fn a_damaged_loose_object_is_replaced() {
    let destination = git::empty_repository();
    let mut sink = open(&destination.dir);
    let bytes = b"what a power loss empties\n";
    let id = sink.write_object(RawKind::Blob, bytes).unwrap();
    let path = loose_path(&destination.dir, id);
    // Cut to 0 bytes, then holding another object's bytes: the file is there, and it does not
    // read back as the object.
    let other = sink.write_object(RawKind::Blob, b"another\n").unwrap();
    for damage in [None, Some(other)] {
        match damage {
            None => cut(&path),
            Some(other) => {
                cut(&path);
                fs::copy(loose_path(&destination.dir, other), &path).unwrap();
            }
        }
        assert_eq!(sink.stored(id), Ok(Stored::Loose), "{damage:?}");
        assert!(
            matches!(sink.read(id), Err(SinkError::Corrupt { oid, .. }) if oid == id),
            "{damage:?}: {:?}",
            sink.read(id)
        );
        assert_eq!(
            sink.write_object(RawKind::Blob, bytes),
            Ok(id),
            "{damage:?}"
        );
        assert!(
            sink.read(id) == Ok((RawKind::Blob, bytes.to_vec())),
            "{damage:?}"
        );
        let reader = GixSource::open(&destination.dir).unwrap();
        assert!(
            reader.read(id) == Ok((RawKind::Blob, bytes.to_vec())),
            "{damage:?}"
        );
    }
}

#[test]
fn the_empty_tree_is_absent_until_written() {
    // gix answers for the empty tree in every repository, whether or not it is stored; the sink
    // goes by what is there, as git's own object writer does, so that a branch never reaches a
    // tree the repository does not hold.
    let destination = git::empty_repository();
    let mut sink = open(&destination.dir);
    let empty = git_hash(RawKind::Tree, b"");
    assert_eq!(empty, oid("4b825dc642cb6eb9a060e54bf8d69288fbee4904"));
    assert_eq!(sink.stored(empty), Ok(Stored::Absent));
    assert_eq!(sink.read(empty), Err(SinkError::Missing { oid: empty }));
    assert_eq!(sink.write_object(RawKind::Tree, b""), Ok(empty));
    assert!(loose_path(&destination.dir, empty).is_file());
    assert_eq!(sink.stored(empty), Ok(Stored::Loose));
    assert_eq!(sink.read(empty), Ok((RawKind::Tree, Vec::new())));
    // Damaged, it reads as damaged rather than as the tree gix makes up, and is written again.
    cut(&loose_path(&destination.dir, empty));
    assert!(matches!(sink.read(empty), Err(SinkError::Corrupt { .. })));
    assert_eq!(sink.write_object(RawKind::Tree, b""), Ok(empty));
    assert_eq!(sink.read(empty), Ok((RawKind::Tree, Vec::new())));
}

/// Blobs `blob <n>\n`, n from 0: the first two whose ids share a fan-out directory, and the first
/// one in another directory.
#[cfg(feature = "test-hooks")]
fn three_blobs() -> [(Vec<u8>, GitOid); 3] {
    let blobs: Vec<(Vec<u8>, GitOid)> = (0..1000_u32)
        .map(|n| {
            let bytes = format!("blob {n}\n").into_bytes();
            let id = git_hash(RawKind::Blob, &bytes);
            (bytes, id)
        })
        .collect();
    for (at, later) in blobs.iter().enumerate() {
        let fan_out = later.1.0[0];
        if let Some(earlier) = blobs[..at].iter().find(|earlier| earlier.1.0[0] == fan_out) {
            let other = blobs.iter().find(|blob| blob.1.0[0] != fan_out).unwrap();
            return [earlier.clone(), later.clone(), other.clone()];
        }
    }
    panic!("no two of a thousand blobs share a fan-out directory");
}

/// What `synced` holds, emptied.
#[cfg(feature = "test-hooks")]
fn take(synced: &std::sync::Mutex<Vec<PathBuf>>) -> Vec<PathBuf> {
    std::mem::take(&mut *synced.lock().unwrap())
}

#[cfg(feature = "test-hooks")]
#[test]
fn sync_covers_the_files_and_directories_it_is_given() {
    let destination = git::empty_repository();
    let [first, second, third] = three_blobs();
    let objects = destination.dir.join("objects");
    let fan_out = |id: GitOid| PathBuf::from(format!("objects/{:02x}", id.0[0]));
    let file = |id: GitOid| loose_path(Path::new(""), id);
    // The first two blobs' fan-out is there before the sink opens; the third's it makes.
    fs::create_dir(destination.dir.join(fan_out(first.1))).unwrap();
    let (mut sink, synced) = GixSink::open_recording_syncs(&destination.dir).unwrap();
    for (bytes, id) in [&first, &second, &third] {
        assert_eq!(sink.write_object(RawKind::Blob, bytes), Ok(*id));
    }
    assert_eq!(
        take(&synced),
        Vec::<PathBuf>::new(),
        "a write syncs nothing"
    );
    let ids = [first.1, second.1, third.1];
    assert_eq!(sink.sync(&ids), Ok(()));
    let got = take(&synced);
    let expected: BTreeSet<PathBuf> = (ids.iter().map(|&id| file(id)))
        .chain([fan_out(first.1), fan_out(third.1), PathBuf::from("objects")])
        .collect();
    assert_eq!(got.len(), expected.len(), "each once: {got:?}");
    assert_eq!(got.into_iter().collect::<BTreeSet<_>>(), expected);
    // Once synced, the new fan-out's name is durable: `objects/` is not synced again for it.
    assert_eq!(sink.sync(&[third.1, third.1]), Ok(()));
    let again: BTreeSet<PathBuf> = take(&synced).into_iter().collect();
    assert_eq!(again, BTreeSet::from([file(third.1), fan_out(third.1)]));
    assert!(objects.join(format!("{:02x}", third.1.0[0])).is_dir());
}

#[cfg(feature = "test-hooks")]
#[test]
fn sync_needs_no_file_for_a_packed_object() {
    let fixture = git::fixture("loose_and_packed");
    let packed = (manifest(&fixture).commits.iter())
        .map(|commit| oid(&commit.oid))
        .find(|&id| !loose_path(repo(&fixture), id).exists())
        .expect("a commit only in the pack");
    let (mut sink, synced) = GixSink::open_recording_syncs(repo(&fixture)).unwrap();
    assert_eq!(sink.stored(packed), Ok(Stored::Packed));
    assert_eq!(sink.sync(&[packed]), Ok(()));
    assert_eq!(take(&synced), Vec::<PathBuf>::new());
    let nothing = oid(NOTHING);
    assert_eq!(
        sink.sync(&[packed, nothing]),
        Err(SinkError::Missing { oid: nothing })
    );
    assert_eq!(take(&synced), Vec::<PathBuf>::new());
}

#[test]
fn cas_branch_creates_moves_and_refuses_a_mismatch() {
    let destination = git::empty_repository();
    let mut sink = open(&destination.dir);
    let [first, second, third] = three_commits(&mut sink);
    let main = destination.dir.join("refs/heads/main");
    assert_eq!(sink.branch("main"), Ok(None));
    assert_eq!(
        sink.cas_branch("main", None, first),
        Ok(BranchUpdate::Applied)
    );
    assert_eq!(sink.branch("main"), Ok(Some(first)));
    assert_eq!(
        sink.cas_branch("main", Some(first), second),
        Ok(BranchUpdate::Applied)
    );
    assert_eq!(fs::read_to_string(&main).unwrap(), format!("{second}\n"));
    let moved = Ok(BranchUpdate::Mismatch {
        actual: Some(second),
    });
    assert_eq!(sink.cas_branch("main", Some(first), third), moved);
    assert_eq!(sink.cas_branch("main", None, third), moved);
    assert_eq!(fs::read_to_string(&main).unwrap(), format!("{second}\n"));
    assert_eq!(sink.branch("main"), Ok(Some(second)));
    // A branch naming an object the repository lacks is refused, as git refuses it.
    let nothing = oid(NOTHING);
    assert_eq!(
        sink.cas_branch("main", Some(second), nothing),
        Err(SinkError::Missing { oid: nothing })
    );
    // A branch in a directory of its own, which the move makes.
    assert_eq!(
        sink.cas_branch("feat/x", None, third),
        Ok(BranchUpdate::Applied)
    );
    let refs = ["refs/heads/feat/x", "refs/heads/main"].map(PathBuf::from);
    assert_eq!(
        ref_files(&destination.dir),
        BTreeSet::from(refs),
        "no lock left"
    );
    let reader = GixSource::open(&destination.dir).unwrap();
    assert_eq!(reader.resolve("main"), Ok(second));
    assert_eq!(reader.resolve("feat/x"), Ok(third));
}

#[cfg(feature = "test-hooks")]
#[test]
fn a_moved_branch_is_synced_with_each_directory_the_move_made() {
    let destination = git::empty_repository();
    let (mut sink, synced) = GixSink::open_recording_syncs(&destination.dir).unwrap();
    let [first, second, _] = three_commits(&mut sink);
    assert_eq!(
        take(&synced),
        Vec::<PathBuf>::new(),
        "a write syncs nothing"
    );
    let paths = |paths: &[&str]| paths.iter().map(PathBuf::from).collect::<BTreeSet<_>>();
    assert_eq!(
        sink.cas_branch("main", None, first),
        Ok(BranchUpdate::Applied)
    );
    let got: BTreeSet<PathBuf> = take(&synced).into_iter().collect();
    assert_eq!(got, paths(&["refs/heads/main.lock", "refs/heads"]));
    // Two directories the move makes: each is synced, and so is its parent.
    assert_eq!(
        sink.cas_branch("feat/x/y", None, second),
        Ok(BranchUpdate::Applied)
    );
    let got: BTreeSet<PathBuf> = take(&synced).into_iter().collect();
    let expected = [
        "refs/heads/feat/x/y.lock",
        "refs/heads/feat/x",
        "refs/heads/feat",
        "refs/heads",
    ];
    assert_eq!(got, paths(&expected));
    // A mismatch moves nothing, and syncs nothing.
    assert_eq!(
        sink.cas_branch("main", None, second),
        Ok(BranchUpdate::Mismatch {
            actual: Some(first)
        })
    );
    assert_eq!(take(&synced), Vec::<PathBuf>::new());
}

#[test]
fn cas_branch_moves_a_packed_ref() {
    let fixture = git::fixture("refs");
    let refs = &manifest(&fixture).refs;
    let (older, newer) = (
        oid(&refs["refs/heads/older"]),
        oid(&refs["refs/heads/newer"]),
    );
    let loose = repo(&fixture).join("refs/heads/older");
    assert!(!loose.exists(), "older is in packed-refs alone");
    let mut sink = open(repo(&fixture));
    assert_eq!(sink.branch("older"), Ok(Some(older)));
    assert_eq!(
        sink.cas_branch("older", None, newer),
        Ok(BranchUpdate::Mismatch {
            actual: Some(older)
        })
    );
    assert!(!loose.exists());
    assert_eq!(
        sink.cas_branch("older", Some(older), newer),
        Ok(BranchUpdate::Applied)
    );
    assert_eq!(fs::read_to_string(&loose).unwrap(), format!("{newer}\n"));
    assert_eq!(sink.branch("older"), Ok(Some(newer)));
    let reader = GixSource::open(repo(&fixture)).unwrap();
    assert_eq!(reader.resolve("refs/heads/older"), Ok(newer));
}

#[cfg(feature = "test-hooks")]
#[test]
fn a_held_ref_lock_is_ref_locked() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    let destination = git::empty_repository();
    let mut sink = open(&destination.dir);
    let [first, ..] = three_commits(&mut sink);
    let lock = destination.dir.join("refs/heads/main.lock");
    fs::write(&lock, "held by another writer\n").unwrap();
    let retries = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&retries);
    sink.on_lock_retry(move || {
        counted.fetch_add(1, Ordering::Relaxed);
    });
    let started = Instant::now();
    assert_eq!(
        sink.cas_branch("main", None, first),
        Err(SinkError::RefLocked {
            lock: "refs/heads/main.lock".to_owned()
        })
    );
    assert!(started.elapsed() >= Duration::from_millis(100));
    assert!(retries.load(Ordering::Relaxed) >= 1, "it tried again");
    assert_eq!(
        fs::read_to_string(&lock).unwrap(),
        "held by another writer\n"
    );
    assert_eq!(sink.branch("main"), Ok(None));
    // The other writer lets the lock go between two attempts: the move takes it then.
    let (held, released) = (lock.clone(), Arc::new(AtomicUsize::new(0)));
    let counted = Arc::clone(&released);
    sink.on_lock_retry(move || {
        fs::remove_file(&held).unwrap();
        counted.fetch_add(1, Ordering::Relaxed);
    });
    assert_eq!(
        sink.cas_branch("main", None, first),
        Ok(BranchUpdate::Applied)
    );
    assert_eq!(released.load(Ordering::Relaxed), 1);
    assert_eq!(sink.branch("main"), Ok(Some(first)));
    assert!(!lock.exists());
}

#[test]
fn a_bad_branch_name_is_refused() {
    let destination = git::empty_repository();
    let mut sink = open(&destination.dir);
    let [first, ..] = three_commits(&mut sink);
    for name in ["a..b", "x.lock", "a b", "a~1", ""] {
        let bad = SinkError::BadBranch {
            name: name.to_owned(),
        };
        assert_eq!(sink.branch(name), Err(bad.clone()), "{name:?}");
        assert_eq!(sink.cas_branch(name, None, first), Err(bad), "{name:?}");
    }
    assert_eq!(ref_files(&destination.dir), BTreeSet::new());
}

#[test]
fn a_ref_in_the_way_is_a_ref_conflict() {
    let fixture = git::fixture("refs");
    let git_dir = repo(&fixture);
    let older = oid(&manifest(&fixture).refs["refs/heads/older"]);
    // A packed ref under a name the test then takes as a directory's, written in packed-refs'
    // sorted order.
    let packed = fs::read_to_string(git_dir.join("packed-refs")).unwrap();
    let mut lines: Vec<String> = packed.lines().map(str::to_owned).collect();
    lines.push(format!("{older} refs/heads/p/q"));
    lines[1..].sort_by(|a, b| a.split(' ').nth(1).cmp(&b.split(' ').nth(1)));
    fs::write(git_dir.join("packed-refs"), lines.join("\n") + "\n").unwrap();
    let mut sink = open(git_dir);
    let before = ref_files(git_dir);
    // A ref whose name is a directory of the branch's: packed, and loose.
    assert_eq!(
        sink.cas_branch("older/x", None, older),
        in_the_way("refs/heads/older")
    );
    assert_eq!(sink.cas_branch("a", None, older), Ok(BranchUpdate::Applied));
    assert_eq!(
        sink.cas_branch("a/b", None, older),
        in_the_way("refs/heads/a")
    );
    // Refs under the branch's name: packed, and loose.
    assert_eq!(
        sink.cas_branch("p", None, older),
        in_the_way("refs/heads/p/q")
    );
    assert_eq!(
        sink.cas_branch("c/d", None, older),
        Ok(BranchUpdate::Applied)
    );
    assert_eq!(
        sink.cas_branch("c", None, older),
        in_the_way("refs/heads/c/d")
    );
    // A file that is no ref, where a directory of the branch's goes; a symbolic ref.
    fs::write(git_dir.join("refs/heads/junk"), "not a ref\n").unwrap();
    assert_eq!(
        sink.cas_branch("junk/x", None, older),
        in_the_way("refs/heads/junk")
    );
    fs::write(git_dir.join("refs/heads/sym"), "ref: refs/heads/main\n").unwrap();
    assert_eq!(sink.branch("sym"), in_the_way("refs/heads/sym"));
    assert_eq!(
        sink.cas_branch("sym", None, older),
        in_the_way("refs/heads/sym")
    );
    let added = [
        "refs/heads/a",
        "refs/heads/c/d",
        "refs/heads/junk",
        "refs/heads/sym",
    ];
    let expected = (before.into_iter())
        .chain(added.map(PathBuf::from))
        .collect();
    assert_eq!(
        ref_files(git_dir),
        expected,
        "nothing else written, no lock left"
    );
}

#[test]
fn checked_out_is_heads_branch_in_a_work_tree() {
    let bare = git::empty_repository();
    assert_eq!(open(&bare.dir).checked_out(), Ok(None));
    // The same files as a work tree's `.git`.
    let made = git::empty_repository();
    let work = tempfile::tempdir().unwrap();
    let dot_git = work.path().join(".git");
    fs::rename(&made.dir, &dot_git).unwrap();
    let config = "[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tbare = false\n";
    fs::write(dot_git.join("config"), config).unwrap();
    let mut sink = open(work.path());
    assert_eq!(sink.checked_out(), Ok(Some("main".to_owned())));
    fs::write(dot_git.join("HEAD"), "ref: refs/heads/feat/x\n").unwrap();
    assert_eq!(
        open(work.path()).checked_out(),
        Ok(Some("feat/x".to_owned()))
    );
    // `HEAD` detached to a commit.
    let [first, ..] = three_commits(&mut sink);
    fs::write(dot_git.join("HEAD"), format!("{first}\n")).unwrap();
    assert_eq!(open(work.path()).checked_out(), Ok(None));
}

#[test]
fn the_sink_refuses_what_the_source_refuses() {
    let unsupported = [
        "[core]\n\trepositoryformatversion = 1\n\tbare = true\n\
         [extensions]\n\tobjectFormat = sha256\n",
        "[core]\n\trepositoryformatversion = 1\n\tbare = true\n\
         [extensions]\n\trefStorage = reftable\n",
    ];
    let linear = || git::fixture("linear");
    let mut fixtures = Vec::new();
    for config in unsupported {
        let fixture = linear();
        fs::write(repo(&fixture).join("config"), config).unwrap();
        fixtures.push(fixture);
    }
    let shallow = linear();
    let root = &manifest(&shallow).commits[0].oid;
    fs::write(repo(&shallow).join("shallow"), format!("{root}\n")).unwrap();
    fixtures.extend([shallow, git::fixture("sha256"), git::fixture("shallow")]);
    for fixture in &fixtures {
        let path = repo(fixture);
        let Err(SourceError::Unsupported(refused)) = GixSource::open(path).map(|_| ()) else {
            panic!("{}: the source opens it", path.display());
        };
        assert_eq!(
            GixSink::open(path).map(|_| ()),
            Err(SinkError::Unsupported(refused)),
            "{}",
            path.display()
        );
    }
    assert_eq!(
        GixSink::open(repo(&fixtures[2])).map(|_| ()),
        Err(SinkError::Unsupported(Unsupported::Shallow))
    );
    // No repository: a fixture's directory, which holds one, and an empty directory.
    let empty = tempfile::tempdir().unwrap();
    for path in [fixtures[0].dir.as_path(), empty.path()] {
        let source = GixSource::open(path).map(|_| ());
        let Err(SourceError::NotARepo { reason }) = source else {
            panic!("{}: {source:?}", path.display());
        };
        assert_eq!(
            GixSink::open(path).map(|_| ()),
            Err(SinkError::NotARepo { reason }),
            "{}",
            path.display()
        );
    }
}

/// A root commit of `tree`, written as git writes one.
fn commit_of(tree: GitOid) -> Vec<u8> {
    let ident = RawIdent {
        name: b"Mem Sink".to_vec(),
        email: b"mem@sink.invalid".to_vec(),
        secs: 1_700_000_000,
        offset_secs: 0,
        negative_zero: false,
    };
    let commit = RawCommit {
        tree,
        parents: Vec::new(),
        author: ident.clone(),
        committer: ident,
        extra_headers: Vec::new(),
        message: b"one file\n".to_vec(),
    };
    commit.render()
}

/// A blob, a tree of it and a commit of that tree: `[(kind, bytes); 3]`, the commit last.
fn one_commit() -> [(RawKind, Vec<u8>); 3] {
    let blob = b"a\n".to_vec();
    let tree = RawTree {
        entries: vec![RawTreeEntry {
            mode: b"100644".to_vec(),
            name: b"a".to_vec(),
            oid: git_hash(RawKind::Blob, &blob),
        }],
    }
    .render();
    let commit = commit_of(git_hash(RawKind::Tree, &tree));
    [
        (RawKind::Blob, blob),
        (RawKind::Tree, tree),
        (RawKind::Commit, commit),
    ]
}

/// What `sink` answers to one run of calls over `objects`, each answer a line.
fn transcript(sink: &mut dyn GitSink, objects: &[(RawKind, Vec<u8>)]) -> Vec<String> {
    let mut lines = vec![format!("format {:?}", sink.object_format())];
    let ids: Vec<GitOid> = objects
        .iter()
        .map(|(kind, bytes)| git_hash(*kind, bytes))
        .collect();
    for ((kind, bytes), id) in objects.iter().zip(&ids) {
        lines.push(format!("stored {id}: {:?}", sink.stored(*id)));
        lines.push(format!(
            "write {kind}: {:?}",
            sink.write_object(*kind, bytes)
        ));
        lines.push(format!("stored {id}: {:?}", sink.stored(*id)));
        let read = sink.read(*id).map(|(kind, bytes)| (kind, bytes == *bytes));
        lines.push(format!("read {id}: {read:?}"));
    }
    let (first, last, nothing) = (ids[0], ids[ids.len() - 1], oid(NOTHING));
    lines.push(format!("read nothing: {:?}", sink.read(nothing)));
    lines.push(format!("sync: {:?}", sink.sync(&ids)));
    lines.push(format!("sync nothing: {:?}", sink.sync(&[first, nothing])));
    lines.push(format!("branch: {:?}", sink.branch("main")));
    for (expected, new) in [(None, first), (None, last), (Some(first), last)] {
        let update = sink.cas_branch("main", expected, new);
        lines.push(format!("cas {expected:?} {new}: {update:?}"));
    }
    let to_nothing = sink.cas_branch("main", Some(last), nothing);
    lines.push(format!("cas to nothing: {to_nothing:?}"));
    lines.push(format!("branch: {:?}", sink.branch("main")));
    lines.push(format!("checked out: {:?}", sink.checked_out()));
    lines
}

#[test]
fn mem_sink_answers_as_gix_sink_does() {
    let objects = one_commit();
    let destination = git::empty_repository();
    let gix = transcript(&mut open(&destination.dir), &objects);
    let mem = transcript(&mut MemSink::default(), &objects);
    assert_eq!(mem, gix);
    // Packed objects: stored as packed in both, and durable without a file to sync.
    let fixture = git::fixture("loose_and_packed");
    let packed = (manifest(&fixture).commits.iter())
        .find(|commit| !loose_path(repo(&fixture), oid(&commit.oid)).exists())
        .expect("a commit only in the pack");
    let id = oid(&packed.oid);
    let mut mem = MemSink::with_packed([(RawKind::Commit, packed.raw.clone())]);
    let mut gix = open(repo(&fixture));
    for sink in [&mut mem as &mut dyn GitSink, &mut gix] {
        assert_eq!(sink.stored(id), Ok(Stored::Packed));
        assert_eq!(sink.write_object(RawKind::Commit, &packed.raw), Ok(id));
        assert_eq!(sink.stored(id), Ok(Stored::Packed));
        assert_eq!(sink.sync(&[id]), Ok(()));
    }
    assert_eq!(
        mem.log(),
        [
            SinkCall::Write { oid: id },
            SinkCall::Sync { oids: vec![id] }
        ]
    );
    assert_eq!(
        mem.image(0, Keep::None).objects.keys().collect::<Vec<_>>(),
        [&id]
    );
}

#[test]
fn mem_sink_images_keep_what_was_synced() {
    let objects = one_commit();
    let [blob, tree, commit] = objects.clone().map(|(kind, bytes)| git_hash(kind, &bytes));
    let mut sink = MemSink::default();
    for (kind, bytes) in &objects {
        sink.write_object(*kind, bytes).unwrap();
    }
    sink.sync(&[blob, tree]).unwrap();
    // The branch moved before the commit is durable: what the oracle exists to catch.
    let moved = sink.cas_branch("main", None, commit);
    assert_eq!(moved, Ok(BranchUpdate::Applied));
    sink.sync(&[commit]).unwrap();
    let cas = SinkCall::Cas {
        name: "main".to_owned(),
        from: None,
        to: commit,
    };
    let writes = [blob, tree, commit].map(|oid| SinkCall::Write { oid });
    let syncs = [vec![blob, tree], vec![commit]].map(|oids| SinkCall::Sync { oids });
    let [first_sync, second_sync] = syncs;
    let mut expected = writes.to_vec();
    expected.extend([first_sync, cas, second_sync]);
    assert_eq!(sink.log(), expected);
    assert_eq!(sink.image(0, Keep::All), MemImage::default());
    // Right after the move, keeping no pending object: the branch names a commit the image lacks.
    let image = sink.image(5, Keep::None);
    assert_eq!(image.branches["main"], commit);
    let lost = format!("commit {commit}, which main reaches, is not in the image");
    assert_eq!(reaches_only_kept(&image, "main"), Err(lost));
    let mut kept = sink.image(5, Keep::All);
    assert_eq!(reaches_only_kept(&kept, "main"), Ok(()));
    kept.objects.remove(&blob);
    let lost = format!("blob {blob}, which main reaches, is not in the image");
    assert_eq!(reaches_only_kept(&kept, "main"), Err(lost));
    // After the last sync every image holds the history, whatever the power loss keeps.
    for keep in [Keep::None, Keep::All, Keep::Seeded(2)] {
        assert_eq!(reaches_only_kept(&sink.image(6, keep), "main"), Ok(()));
    }
    // Before the move there is no branch, and so nothing it reaches is missing.
    assert_eq!(
        reaches_only_kept(&sink.image(4, Keep::None), "main"),
        Ok(())
    );
    // A seed keeps some pending objects and drops others, the same ones each time.
    let pending = [blob, tree, commit];
    let mut kept_by = BTreeSet::new();
    let mut dropped_by = BTreeSet::new();
    for seed in 0..64 {
        let image = sink.image(3, Keep::Seeded(seed));
        assert_eq!(image, sink.image(3, Keep::Seeded(seed)), "{seed}");
        for oid in pending {
            if image.objects.contains_key(&oid) {
                kept_by.insert(oid);
            } else {
                dropped_by.insert(oid);
            }
        }
    }
    assert_eq!(kept_by, BTreeSet::from(pending));
    assert_eq!(dropped_by, BTreeSet::from(pending));
}

#[test]
fn mem_sink_fails_on_purpose_and_runs_its_cas_hook() {
    let [(_, blob_bytes), (_, tree_bytes), (_, commit_bytes)] = one_commit();
    let mut sink = MemSink::default();
    sink.fail_at(2);
    let blob = sink.write_object(RawKind::Blob, &blob_bytes).unwrap();
    let failed = sink.write_object(RawKind::Tree, &tree_bytes);
    assert!(
        matches!(&failed, Err(SinkError::Write { what, .. }) if what == "write_object"),
        "{failed:?}"
    );
    let tree = git_hash(RawKind::Tree, &tree_bytes);
    assert_eq!(sink.stored(tree), Ok(Stored::Absent));
    assert_eq!(sink.write_object(RawKind::Tree, &tree_bytes), Ok(tree));
    assert_eq!(
        sink.log(),
        [SinkCall::Write { oid: blob }, SinkCall::Write { oid: tree }]
    );
    // Another writer moves the branch at the compare-and-swap, which then finds it moved.
    let commit = sink.write_object(RawKind::Commit, &commit_bytes).unwrap();
    sink.set_branch("main", tree);
    sink.set_checked_out(Some("main"));
    assert_eq!(sink.checked_out(), Ok(Some("main".to_owned())));
    sink.on_cas(move |name, actual| {
        assert_eq!(name, "main");
        *actual = Some(blob);
    });
    assert_eq!(
        sink.cas_branch("main", Some(tree), commit),
        Ok(BranchUpdate::Mismatch { actual: Some(blob) })
    );
    assert_eq!(sink.branch("main"), Ok(Some(blob)));
    assert_eq!(sink.log().len(), 3, "a mismatch changes nothing");
    // An image starts from the branch as set, and does not replay the hook's move.
    let image = sink.image(3, Keep::All);
    assert_eq!(image.branches["main"], tree);
}
