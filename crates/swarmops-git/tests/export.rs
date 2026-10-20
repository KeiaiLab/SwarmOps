//! The git export: the history behind a store ref written into a git branch, parents first. A
//! history the store imported faithfully comes back byte for byte; changesets made in the store
//! come back as canonical commits, a linear history on the commit they were made on; the same
//! store gives the same shas; an incremental export writes only what is new; the branch moves only
//! once everything it reaches is durable, by compare-and-swap; and a branch that diverged stops
//! the export, which says why and writes nothing. Every test owns a fresh store, and its
//! destinations are `git::empty_repository()` or a fixture's own `repo.git`, read back through
//! the import's reader (`GixSource`), or a `MemSink`, whose log replays into what a power loss
//! could leave.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use camino::Utf8PathBuf;
use swarmops_core::{
    ChangesetId, EntryTarget, LeafKind, ObjectKind, RepoPath, Tree, TreeEntry, TreeId,
};
use swarmops_git::{
    BranchUpdate, DivergedReason, ExportError, ExportOid, ExportReport, ExportRequest, GitOid,
    GitSink, GitSource, GixSink, GixSource, ImportReport, ImportRequest, LossyClass, RawCommit,
    RawKind, RawTree, commit_bytes, export, git_hash, import, register,
};
use swarmops_repo::{
    Advance, CasResult, Change, CommitRequest, LocalRepo, RawChangeset, RepoError, StoreError,
};
use swarmops_store::{ExternalEntry, ExternalIds, Layout, RealFs, SYSTEM_GIT};
use swarmops_testkit::fs::{HoldThenFail, NoSyncFs};
use swarmops_testkit::git::{self, GitFixture, Manifest};
use swarmops_testkit::repo::{blob, delete, put, put_as, request, signature};
use tempfile::TempDir;

use crate::support::{Keep, MemSink, SinkCall, reaches_only_kept};

/// The fixtures every commit of which a changeset holds as git wrote it (M3 Task 8's list): each
/// imported from `HEAD`, which names `main` in every one.
const FAITHFUL: [&str; 10] = [
    "headers",
    "linear",
    "loose_and_packed",
    "many_parents",
    "merges",
    "modes",
    "refs",
    "replace_ref",
    "roots",
    "signatures",
];

/// Who moves the store's refs in these tests.
const ACTOR: &str = "test/exporter";

fn oid(hex: &str) -> GitOid {
    hex.parse().unwrap_or_else(|error| panic!("{hex}: {error}"))
}

/// A request to export `main` into the branch `main`.
fn main_branch() -> ExportRequest {
    ExportRequest::new("main", "main")
}

/// A store of its own with `ExportOid` registered, on a filesystem that never flushes
/// ([`NoSyncFs`]): the subject is what the export writes into git, not what the store keeps
/// through a power loss.
struct Store {
    _dir: TempDir,
    root: Utf8PathBuf,
    repo: LocalRepo,
}

fn store() -> Store {
    let dir = tempfile::tempdir().expect("a temp dir");
    let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).expect("a UTF-8 temp dir");
    let mut repo = LocalRepo::create_with_fs(&root, "test", Arc::new(NoSyncFs)).expect("create");
    register(&mut repo).expect("register ExportOid");
    Store {
        _dir: dir,
        root,
        repo,
    }
}

/// A fixture, and a fresh store its `HEAD` was imported into, as `main`.
struct Imported {
    fixture: GitFixture,
    store: Store,
    report: ImportReport,
}

impl Imported {
    fn new(name: &str) -> Imported {
        Imported::with(name, &ImportRequest::new("HEAD", "main", ACTOR))
    }

    fn with(name: &str, req: &ImportRequest) -> Imported {
        let fixture = git::fixture(name);
        let store = store();
        let source = GixSource::open(repo_dir(&fixture)).expect("open the fixture");
        let report =
            import(&store.repo, &source, req).unwrap_or_else(|error| panic!("{name}: {error}"));
        Imported {
            fixture,
            store,
            report,
        }
    }

    fn manifest(&self) -> &Manifest {
        manifest(&self.fixture)
    }

    /// The commit the fixture's `main` names.
    fn tip(&self) -> GitOid {
        oid(&self.manifest().refs["refs/heads/main"])
    }

    /// The fixture's own repository.
    fn repo_dir(&self) -> &Path {
        repo_dir(&self.fixture)
    }
}

fn repo_dir(fixture: &GitFixture) -> &Path {
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

fn gix(path: &Path) -> GixSink {
    GixSink::open(path).unwrap_or_else(|error| panic!("open {}: {error}", path.display()))
}

/// What `main` names in `repo`.
fn main_head(repo: &LocalRepo) -> ChangesetId {
    repo.get_ref("main").unwrap().expect("main exists").target
}

/// Commits `changes` on `main`'s head with `message`, by `Repo::commit`, then moves `main` there
/// by `cas_ref`: the new changeset.
fn commit_on_main(repo: &LocalRepo, changes: Vec<Change>, message: &str) -> ChangesetId {
    let head = repo.get_ref("main").unwrap().map(|state| state.target);
    let id = repo
        .commit(request(head.as_slice(), changes, message))
        .expect("commit")
        .id;
    let moved = repo
        .cas_ref("main", head, Some(id), ACTOR)
        .expect("cas_ref");
    assert!(
        matches!(moved.result, CasResult::Applied { .. }),
        "{moved:?}"
    );
    id
}

/// Changeset `id`'s export oid.
fn export_oid(repo: &LocalRepo, id: ChangesetId) -> GitOid {
    repo.derived::<ExportOid>(id, &()).unwrap().git_oid()
}

/// `export`, which must succeed.
#[track_caller]
fn exported(repo: &LocalRepo, sink: &mut dyn GitSink, req: &ExportRequest) -> ExportReport {
    export(repo, sink, req).unwrap_or_else(|error| panic!("export: {error}"))
}

/// Every object the commit `tip` reaches in the repository at `dir`, read through the import's
/// reader, which checks each against its id: by id, its kind and its bytes. A gitlink names a
/// commit of another repository, and is passed over.
fn reachable(dir: &Path, tip: GitOid) -> BTreeMap<GitOid, (RawKind, Vec<u8>)> {
    let source = GixSource::open(dir).expect("open the destination");
    let mut objects = BTreeMap::new();
    let mut stack = vec![tip];
    while let Some(oid) = stack.pop() {
        if objects.contains_key(&oid) {
            continue;
        }
        let (kind, bytes) = source
            .read(oid)
            .unwrap_or_else(|error| panic!("read {oid}: {error}"));
        match kind {
            RawKind::Commit => {
                let commit = RawCommit::parse(&bytes).expect("a commit");
                stack.extend(commit.parents.iter().copied());
                stack.push(commit.tree);
            }
            RawKind::Tree => {
                let tree = RawTree::parse(&bytes).expect("a tree");
                let entries = tree.entries.into_iter();
                stack.extend(entries.filter(|e| e.mode != b"160000").map(|e| e.oid));
            }
            RawKind::Blob | RawKind::Tag => {}
        }
        objects.insert(oid, (kind, bytes));
    }
    objects
}

/// The commit `commit` of the repository at `dir`, parsed.
fn commit_at(dir: &Path, commit: GitOid) -> RawCommit {
    let (kind, bytes) = GixSource::open(dir).unwrap().read(commit).unwrap();
    assert_eq!(kind, RawKind::Commit, "{commit}");
    RawCommit::parse(&bytes).unwrap()
}

/// Every file under the objects directory of the repository at `dir`, by its path there.
fn object_files(dir: &Path) -> BTreeSet<PathBuf> {
    let objects = dir.join("objects");
    let mut files = BTreeSet::new();
    let mut dirs = vec![objects.clone()];
    while let Some(at) = dirs.pop() {
        for entry in fs::read_dir(&at).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                dirs.push(path);
            } else {
                files.insert(path.strip_prefix(&objects).unwrap().to_owned());
            }
        }
    }
    files
}

/// The loose file of `oid` in the repository whose git directory is `dir`.
fn loose_path(dir: &Path, oid: GitOid) -> PathBuf {
    let hex = oid.to_string();
    dir.join("objects").join(&hex[..2]).join(&hex[2..])
}

/// The commits the manifest's commit `tip` reaches, `tip` included.
fn reached(manifest: &Manifest, tip: GitOid) -> BTreeSet<GitOid> {
    let parents: BTreeMap<GitOid, Vec<GitOid>> = (manifest.commits.iter())
        .map(|commit| {
            (
                oid(&commit.oid),
                commit.parents.iter().map(|p| oid(p)).collect(),
            )
        })
        .collect();
    let mut reached = BTreeSet::new();
    let mut stack = vec![tip];
    while let Some(commit) = stack.pop() {
        if reached.insert(commit) {
            stack.extend(parents[&commit].iter().copied());
        }
    }
    reached
}

/// A non-bare repository whose `HEAD` names `main`: an empty repository's files as a work tree's
/// `.git`. The work tree's directory.
fn work_tree() -> TempDir {
    let made = git::empty_repository();
    let work = tempfile::tempdir().unwrap();
    let dot_git = work.path().join(".git");
    fs::rename(&made.dir, &dot_git).unwrap();
    let config = "[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tbare = false\n";
    fs::write(dot_git.join("config"), config).unwrap();
    work
}

/// Stores, with `put_objects`, the tree that holds `files` (each a path and a file's content),
/// its subtrees and its blobs; the tree's id. (swarmops-repo's `tests/raw.rs` builds trees the
/// same way.)
fn store_tree(repo: &LocalRepo, files: &[(&str, &[u8])]) -> TreeId {
    let mut objects = Vec::new();
    let root = tree_of(files, &mut objects);
    repo.put_objects(objects).expect("put the tree");
    root
}

/// The tree that holds `files`, its subtrees and its blobs appended to `objects`.
fn tree_of(files: &[(&str, &[u8])], objects: &mut Vec<(ObjectKind, Vec<u8>)>) -> TreeId {
    let mut below: BTreeMap<&str, Vec<(&str, &[u8])>> = BTreeMap::new();
    let mut entries = BTreeMap::new();
    for &(file, content) in files {
        if let Some((dir, rest)) = file.split_once('/') {
            below.entry(dir).or_default().push((rest, content));
        } else {
            objects.push((ObjectKind::Blob, content.to_vec()));
            let entry = TreeEntry::leaf(file, LeafKind::File, blob(content));
            entries.insert(file.to_owned(), entry);
        }
    }
    for (dir, files) in below {
        let id = tree_of(&files, objects);
        entries.insert(dir.to_owned(), TreeEntry::dir(dir, id));
    }
    let tree = Tree::new(entries.into_values().collect()).expect("a valid tree");
    objects.push((ObjectKind::Tree, tree.encode()));
    tree.id()
}

/// A raw changeset of `root` on `parents`, by the test kit's signature, with `message` and no
/// extra headers or metadata: a native changeset.
fn raw(parents: &[ChangesetId], root: TreeId, message: &str) -> RawChangeset {
    RawChangeset {
        parents: parents.to_vec(),
        root,
        author: signature(),
        committer: signature(),
        message: message.as_bytes().to_vec(),
        extra_headers: Vec::new(),
        metadata: Vec::new(),
    }
}

/// On top of `main` (an imported head): a second root made natively, holding `other.txt`, and a
/// merge of it into `main`'s head, recorded by `commit_raw_batch`'s builder; `main` moved to the
/// merge. The second root and the merge.
fn second_root_and_merge(repo: &LocalRepo) -> (ChangesetId, ChangesetId) {
    let head = main_head(repo);
    let head_root = repo.changeset(head).unwrap().unwrap().changeset.root;
    let other = store_tree(repo, &[("other.txt", b"a second root\n")]);
    let mut batch = repo.raw_batch();
    let second = batch.push(raw(&[], other, "A second root\n")).unwrap();
    let merge = (batch.push(raw(&[head, second], head_root, "Merge the second root\n"))).unwrap();
    batch.commit().unwrap();
    let moved = repo
        .cas_ref("main", Some(head), Some(merge), ACTOR)
        .unwrap();
    assert!(matches!(moved.result, CasResult::Applied { .. }));
    (second, merge)
}

/// Three changesets made natively on `main`, the third deleting a file and adding an executable
/// and a symlink.
fn three_natives(repo: &LocalRepo) -> [ChangesetId; 3] {
    let first = commit_on_main(
        repo,
        vec![put("notes/one.txt", b"one\n")],
        "Add notes/one.txt\n",
    );
    let second = commit_on_main(
        repo,
        vec![
            put("notes/one.txt", b"one, again\n"),
            put("notes/two.txt", b"two\n"),
        ],
        "Change notes/one.txt, add notes/two.txt\n\nCo-Authored-By: A Person <a@example.com>\n",
    );
    let third = commit_on_main(
        repo,
        vec![
            delete("notes/two.txt"),
            put_as("run.sh", LeafKind::Exec, b"#!/bin/sh\necho run\n"),
            put_as("link", LeafKind::Symlink, b"notes/one.txt"),
        ],
        "Remove notes/two.txt; add run.sh and link\n",
    );
    [first, second, third]
}

/// Checks that git's tree `tree` in the repository `source` reads is changeset `cs`'s root, as
/// `RepoRead::list_tree` and `RepoRead::read_blob_at` give it: the same paths, git's modes for
/// their kinds, and the same bytes.
fn same_tree(source: &GixSource, tree: GitOid, repo: &LocalRepo, cs: ChangesetId) {
    let read = repo.snapshot().unwrap();
    let mut dirs: Vec<(GitOid, Option<RepoPath>)> = vec![(tree, None)];
    while let Some((tree, dir)) = dirs.pop() {
        let (kind, bytes) = source.read(tree).unwrap();
        assert_eq!(kind, RawKind::Tree, "{dir:?}");
        let entries = RawTree::parse(&bytes).unwrap().entries;
        let held = read
            .list_tree(cs, dir.as_ref())
            .unwrap()
            .expect("a directory");
        let git_names: BTreeSet<&[u8]> = entries.iter().map(|e| e.name.as_slice()).collect();
        let held_names: BTreeSet<&[u8]> = held.iter().map(|e| e.name.as_bytes()).collect();
        assert_eq!(git_names, held_names, "{dir:?}");
        for entry in entries {
            let name = std::str::from_utf8(&entry.name).unwrap();
            let path = match &dir {
                Some(dir) => RepoPath::new(format!("{dir}/{name}")).unwrap(),
                None => RepoPath::new(name).unwrap(),
            };
            let target = held.iter().find(|e| e.name == name).unwrap().target;
            match target {
                EntryTarget::Dir(_) => {
                    assert_eq!(entry.mode, b"40000", "{path}");
                    dirs.push((entry.oid, Some(path)));
                }
                EntryTarget::Leaf(kind, _) => {
                    let mode: &[u8] = match kind {
                        LeafKind::File => b"100644",
                        LeafKind::Exec => b"100755",
                        LeafKind::Symlink => b"120000",
                    };
                    assert_eq!(entry.mode, mode, "{path}");
                    let (_, _, content) = read.read_blob_at(cs, &path).unwrap().unwrap();
                    assert_eq!(
                        source.read(entry.oid).unwrap(),
                        (RawKind::Blob, content),
                        "{path}"
                    );
                }
            }
        }
    }
}

/// Every faithful fixture, imported from its `HEAD` and exported into an empty repository, comes
/// back byte for byte: the branch names the fixture's head commit, and every commit it reaches
/// reads back as the manifest's raw bytes, every tree as the manifest's bytes, every blob with
/// the manifest's size; each object is written once, and no commit is rewritten.
#[test]
fn an_export_of_an_imported_fixture_reproduces_it_byte_for_byte() {
    for name in FAITHFUL {
        let fx = Imported::new(name);
        let manifest = fx.manifest();
        let tip = fx.tip();
        let destination = git::empty_repository();
        let report = exported(&fx.store.repo, &mut gix(&destination.dir), &main_branch());
        assert_eq!(
            (report.head, report.head_oid),
            (fx.report.head, tip),
            "{name}"
        );
        assert_eq!(
            (report.old, report.base, report.branch_moved),
            (None, None, true),
            "{name}"
        );
        assert_eq!(
            (report.rewritten, &report.warnings[..]),
            (0, &[][..]),
            "{name}"
        );
        assert_eq!(
            gix(&destination.dir).branch("main"),
            Ok(Some(tip)),
            "{name}"
        );

        let objects = reachable(&destination.dir, tip);
        let commits = reached(manifest, tip);
        let mut read_back = 0;
        for commit in &manifest.commits {
            if commits.contains(&oid(&commit.oid)) {
                let raw = (RawKind::Commit, commit.raw.clone());
                assert_eq!(
                    objects.get(&oid(&commit.oid)),
                    Some(&raw),
                    "{name}: {}",
                    commit.oid
                );
                read_back += 1;
            }
        }
        assert_eq!(read_back, commits.len(), "{name}");
        for (id, (kind, bytes)) in &objects {
            let hex = id.to_string();
            match kind {
                RawKind::Commit => assert!(commits.contains(id), "{name}: commit {id}"),
                RawKind::Tree => assert_eq!(Some(bytes), manifest.trees.get(&hex), "{name}: {id}"),
                RawKind::Blob => {
                    let size = u64::try_from(bytes.len()).unwrap();
                    assert_eq!(manifest.blobs.get(&hex), Some(&size), "{name}: blob {id}");
                }
                RawKind::Tag => panic!("{name}: a tag {id}"),
            }
        }
        let written = u64::try_from(objects.len()).unwrap();
        let commits = u64::try_from(commits.len()).unwrap();
        assert_eq!(
            (
                report.commits_written,
                report.objects_written,
                report.objects_checked
            ),
            (commits, written, 0),
            "{name}"
        );
    }
}

/// The same export into the fixture's own repository, on its branch, finds the branch at the
/// head's export already: it writes nothing and moves nothing. Into a new branch of the same
/// repository, it writes nothing either: the history is there, packed, and a packed commit is
/// trusted with everything it reaches (a loose one is read back).
#[test]
fn an_export_into_the_import_source_writes_nothing() {
    for name in FAITHFUL {
        let fx = Imported::new(name);
        let before = object_files(fx.repo_dir());
        let report = exported(&fx.store.repo, &mut gix(fx.repo_dir()), &main_branch());
        assert_eq!(report.head_oid, fx.tip(), "{name}");
        assert_eq!(
            (report.old, report.base, report.branch_moved),
            (Some(fx.tip()), Some(fx.report.head), false),
            "{name}"
        );
        assert_eq!(
            (
                report.commits_written,
                report.objects_written,
                report.objects_checked
            ),
            (0, 0, 0),
            "{name}"
        );
        assert_eq!(object_files(fx.repo_dir()), before, "{name}");

        let new_branch = ExportRequest::new("main", "exported");
        let report = exported(&fx.store.repo, &mut gix(fx.repo_dir()), &new_branch);
        assert_eq!(
            (report.old, report.base, report.branch_moved),
            (None, None, true),
            "{name}"
        );
        assert_eq!(
            (report.commits_written, report.objects_written),
            (0, 0),
            "{name}"
        );
        let loose =
            (before.iter()).any(|file| !file.starts_with("pack") && !file.starts_with("info"));
        if !loose {
            assert_eq!(report.objects_checked, 0, "{name}: every object is packed");
        }
        assert_eq!(
            gix(fx.repo_dir()).branch("exported"),
            Ok(Some(fx.tip())),
            "{name}"
        );
        assert_eq!(object_files(fx.repo_dir()), before, "{name}");
    }
}

/// Three changesets made natively on an imported `main`, exported into the repository it was
/// imported from: the branch moves from the fixture's tip to the third's export, three commits of
/// one parent each, the first's the old tip; each tree holds the paths, modes and bytes the store
/// holds.
#[test]
fn native_changesets_export_as_a_linear_history() {
    let fx = Imported::new("linear");
    let repo = &fx.store.repo;
    let natives = three_natives(repo);
    let report = exported(repo, &mut gix(fx.repo_dir()), &main_branch());
    assert_eq!(report.head, natives[2]);
    assert_eq!(report.head_oid, export_oid(repo, natives[2]));
    assert_eq!(
        (report.old, report.base, report.branch_moved),
        (Some(fx.tip()), Some(fx.report.head), true)
    );
    assert_eq!((report.commits_written, report.rewritten), (3, 0));
    assert_eq!(gix(fx.repo_dir()).branch("main"), Ok(Some(report.head_oid)));

    let source = GixSource::open(fx.repo_dir()).unwrap();
    let mut at = report.head_oid;
    for &native in natives.iter().rev() {
        assert_eq!(at, export_oid(repo, native));
        let commit = commit_at(fx.repo_dir(), at);
        assert_eq!(commit.parents.len(), 1, "{native}");
        same_tree(&source, commit.tree, repo, native);
        let trailer = format!("SwarmOps-Changeset: {native}\n");
        assert!(commit.message.ends_with(trailer.as_bytes()), "{native}");
        at = commit.parents[0];
    }
    assert_eq!(
        at,
        fx.tip(),
        "the first native commit's parent is the old tip"
    );
    // The third's tree holds an executable and a symlink, and no longer notes/two.txt.
    let third = commit_at(fx.repo_dir(), report.head_oid);
    let root = RawTree::parse(&source.read(third.tree).unwrap().1).unwrap();
    let modes: BTreeMap<&[u8], &[u8]> = (root.entries.iter())
        .map(|e| (e.name.as_slice(), e.mode.as_slice()))
        .collect();
    assert_eq!(modes.get(&b"run.sh"[..]), Some(&&b"100755"[..]));
    assert_eq!(modes.get(&b"link"[..]), Some(&&b"120000"[..]));
}

/// A second root and a merge of it with `main`'s head, made natively (no `git.oid`), are both
/// written; the merge names its two parents in their order.
#[test]
fn a_second_root_and_a_merge_made_natively_export_whole() {
    let fx = Imported::new("linear");
    let repo = &fx.store.repo;
    let (second, merge) = second_root_and_merge(repo);
    let report = exported(repo, &mut gix(fx.repo_dir()), &main_branch());
    assert_eq!(
        (report.head, report.head_oid),
        (merge, export_oid(repo, merge))
    );
    assert_eq!(
        (report.old, report.base),
        (Some(fx.tip()), Some(fx.report.head))
    );
    assert_eq!(report.commits_written, 2);

    let merged = commit_at(fx.repo_dir(), report.head_oid);
    assert_eq!(merged.parents, [fx.tip(), export_oid(repo, second)]);
    let root = commit_at(fx.repo_dir(), export_oid(repo, second));
    assert_eq!(root.parents, [] as [GitOid; 0]);
    same_tree(
        &GixSource::open(fx.repo_dir()).unwrap(),
        root.tree,
        repo,
        second,
    );
}

/// A store exported into two empty repositories names the same commit and writes the same objects
/// into each, and so does another store made the same way; the first exported into again writes
/// nothing.
#[test]
fn exporting_twice_gives_the_same_shas() {
    let build = || {
        let fx = Imported::new("linear");
        three_natives(&fx.store.repo);
        second_root_and_merge(&fx.store.repo);
        fx
    };
    let fx = build();
    let repo = &fx.store.repo;
    let (first, second) = (git::empty_repository(), git::empty_repository());
    let one = exported(repo, &mut gix(&first.dir), &main_branch());
    let two = exported(repo, &mut gix(&second.dir), &main_branch());
    assert_eq!(one.head_oid, two.head_oid);
    assert_eq!(object_files(&first.dir), object_files(&second.dir));
    assert_eq!(
        reachable(&first.dir, one.head_oid),
        reachable(&second.dir, two.head_oid)
    );

    let other = build();
    let third = git::empty_repository();
    let three = exported(&other.store.repo, &mut gix(&third.dir), &main_branch());
    assert_eq!(three.head_oid, one.head_oid);
    assert_eq!(object_files(&third.dir), object_files(&first.dir));

    let again = exported(repo, &mut gix(&first.dir), &main_branch());
    assert_eq!(again.head_oid, one.head_oid);
    assert_eq!(
        (
            again.branch_moved,
            again.commits_written,
            again.objects_written
        ),
        (false, 0, 0)
    );
}

/// After one more native changeset that changes `a/b/c.txt`, the export writes exactly that
/// commit, the three trees on the file's path and its blob, makes those five durable in one sync,
/// and then moves the branch.
#[test]
fn an_incremental_export_writes_only_what_is_new() {
    let store = store();
    let repo = &store.repo;
    let first = commit_on_main(
        repo,
        vec![
            put("a/b/c.txt", b"one\n"),
            put("a/b/kept.txt", b"kept\n"),
            put("a/d.txt", b"d\n"),
            put("e.txt", b"e\n"),
        ],
        "Add a tree\n",
    );
    let mut sink = MemSink::default();
    let before = exported(repo, &mut sink, &main_branch());
    let logged = sink.log().len();

    let second = commit_on_main(repo, vec![put("a/b/c.txt", b"two\n")], "Change a/b/c.txt\n");
    let report = exported(repo, &mut sink, &main_branch());
    assert_eq!((report.head, report.base), (second, Some(first)));
    assert_eq!(
        (
            report.commits_written,
            report.objects_written,
            report.objects_checked
        ),
        (1, 5, 0)
    );
    let calls = &sink.log()[logged..];
    assert_eq!(calls.len(), 7, "{calls:?}");
    let written: Vec<GitOid> = (calls[..5].iter())
        .map(|call| match call {
            SinkCall::Write { oid } => *oid,
            other => panic!("a write, not {other:?}"),
        })
        .collect();
    let mut kinds: Vec<RawKind> = (written.iter())
        .map(|&oid| sink.read(oid).unwrap().0)
        .collect();
    kinds.sort_by_key(|kind| kind.as_str());
    assert_eq!(
        kinds,
        [
            RawKind::Blob,
            RawKind::Commit,
            RawKind::Tree,
            RawKind::Tree,
            RawKind::Tree
        ]
    );
    assert!(written.contains(&git_hash(RawKind::Blob, b"two\n")));
    assert!(written.contains(&report.head_oid));
    let SinkCall::Sync { oids } = &calls[5] else {
        panic!("a sync, not {:?}", calls[5]);
    };
    let set = |oids: &[GitOid]| oids.iter().copied().collect::<BTreeSet<_>>();
    assert_eq!((oids.len(), set(oids)), (5, set(&written)));
    assert_eq!(
        calls[6],
        SinkCall::Cas {
            name: "main".to_owned(),
            from: Some(before.head_oid),
            to: report.head_oid,
        }
    );
}

/// Every object written is named by a sync before the branch moves, and when it moves, the native
/// commits' external id entries are there already.
#[test]
fn objects_are_durable_before_the_branch_moves() {
    let fx = Imported::new("linear");
    let repo = &fx.store.repo;
    let natives = three_natives(repo);
    let expected: Vec<(GitOid, ChangesetId)> = (natives.iter())
        .map(|&native| (export_oid(repo, native), native))
        .collect();
    let at_cas: Arc<Mutex<Vec<Option<ExternalEntry>>>> = Arc::default();
    let mut sink = MemSink::default();
    {
        let (root, at_cas) = (fx.store.root.clone(), Arc::clone(&at_cas));
        sink.on_cas(move |_, _| {
            let other = LocalRepo::open(&root).expect("a second handle");
            let entries = (expected.iter())
                .map(|(oid, _)| other.external_id(SYSTEM_GIT, &oid.0).unwrap())
                .collect();
            *at_cas.lock().unwrap() = entries;
        });
    }
    let report = exported(repo, &mut sink, &main_branch());
    assert!(report.branch_moved);

    let calls = sink.log();
    let cas = (calls.iter())
        .position(|call| matches!(call, SinkCall::Cas { .. }))
        .expect("the branch moved");
    assert_eq!(cas, calls.len() - 1, "the move is the last call");
    for (at, call) in calls.iter().enumerate() {
        if let SinkCall::Write { oid } = call {
            let synced = (calls[at + 1..cas].iter())
                .any(|call| matches!(call, SinkCall::Sync { oids } if oids.contains(oid)));
            assert!(synced, "{oid} is written and moved to unsynced");
        }
    }
    let entries: Vec<Option<ExternalEntry>> = (natives.iter())
        .map(|&changeset| {
            Some(ExternalEntry {
                changeset,
                faithful: true,
            })
        })
        .collect();
    assert_eq!(*at_cas.lock().unwrap(), entries);
}

/// What a power loss after any call of an export could leave never has the branch name an object
/// it lost: a native export, and `merges`' whole history, each replayed after every call, keeping
/// none, all, and seeded draws of the objects not synced yet.
#[test]
fn power_loss_images_of_an_export_never_name_a_lost_object() {
    let native = store();
    three_natives(&native.repo);
    let merges = Imported::new("merges");
    for repo in [&native.repo, &merges.store.repo] {
        let mut sink = MemSink::default();
        let report = exported(repo, &mut sink, &main_branch());
        let calls = sink.log().len();
        let keeps = [Keep::None, Keep::All]
            .into_iter()
            .chain((2..=9).map(Keep::Seeded));
        for keep in keeps {
            for after in 0..=calls {
                let image = sink.image(after, keep);
                if let Err(why) = reaches_only_kept(&image, "main") {
                    panic!("after {after} of {calls} calls, keeping {keep:?}: {why}");
                }
            }
            let end = sink.image(calls, keep);
            assert_eq!(end.branches.get("main"), Some(&report.head_oid));
        }
    }
}

/// The oracle the test above relies on fails a log made by hand that moves the branch before the
/// sync of what it names: the image right after the move, keeping none of the pending objects,
/// lacks the commit.
#[test]
fn the_sink_oracle_rejects_a_branch_moved_before_its_objects_are_durable() {
    let mut sink = MemSink::default();
    let tree = sink.write_object(RawKind::Tree, b"").unwrap();
    let sig = signature();
    let bytes = commit_bytes(tree, &[], &sig, &sig, &[], b"An empty tree\n");
    let commit = sink.write_object(RawKind::Commit, &bytes).unwrap();
    assert_eq!(
        sink.cas_branch("main", None, commit),
        Ok(BranchUpdate::Applied)
    );
    sink.sync(&[tree, commit]).unwrap();
    let missing = reaches_only_kept(&sink.image(3, Keep::None), "main").unwrap_err();
    assert!(missing.contains(&format!("commit {commit}")), "{missing}");
    assert_eq!(reaches_only_kept(&sink.image(3, Keep::All), "main"), Ok(()));
    assert_eq!(
        reaches_only_kept(&sink.image(4, Keep::None), "main"),
        Ok(())
    );
}

/// A branch moved in git to a commit of another history is a divergence: nothing is written, and
/// the branch stays where git put it.
#[test]
fn a_branch_moved_in_git_is_diverged_and_writes_nothing() {
    let fx = Imported::new("linear");
    let repo = &fx.store.repo;
    let destination = git::empty_repository();
    let first = exported(repo, &mut gix(&destination.dir), &main_branch());
    commit_on_main(repo, vec![put("new.txt", b"new\n")], "Add new.txt\n");
    // A commit of the `roots` fixture, which shares nothing with `linear`.
    let roots = git::fixture("roots");
    let foreign = &manifest(&roots).commits[1];
    let mut sink = gix(&destination.dir);
    let moved_to = sink.write_object(RawKind::Commit, &foreign.raw).unwrap();
    assert_eq!(moved_to, oid(&foreign.oid));
    sink.sync(&[moved_to]).unwrap();
    let applied = sink.cas_branch("main", Some(first.head_oid), moved_to);
    assert_eq!(applied, Ok(BranchUpdate::Applied));

    let before = object_files(&destination.dir);
    let error = export(repo, &mut gix(&destination.dir), &main_branch()).unwrap_err();
    let ExportError::Diverged {
        branch,
        git_head,
        head,
        reason,
    } = error
    else {
        panic!("expected a divergence, got {error}");
    };
    assert_eq!(
        (branch.as_str(), git_head, head, reason),
        (
            "main",
            moved_to,
            main_head(repo),
            DivergedReason::BranchMoved
        )
    );
    assert_eq!(reason.as_str(), "branch_moved");
    assert_eq!(object_files(&destination.dir), before, "nothing written");
    assert_eq!(gix(&destination.dir).branch("main"), Ok(Some(moved_to)));
}

/// A commit made in git on the export of the source's head: the branch is ahead, and the export
/// names the changeset it continues. Once the store moves on too, both moved.
#[test]
fn a_branch_ahead_names_the_export_it_continues() {
    let store = store();
    let repo = &store.repo;
    let head = commit_on_main(repo, vec![put("a.txt", b"a\n")], "Add a.txt\n");
    let destination = git::empty_repository();
    let first = exported(repo, &mut gix(&destination.dir), &main_branch());
    let mut sink = gix(&destination.dir);
    let tree = commit_at(&destination.dir, first.head_oid).tree;
    let sig = signature();
    let bytes = commit_bytes(tree, &[first.head_oid], &sig, &sig, &[], b"Made in git\n");
    let ahead = sink.write_object(RawKind::Commit, &bytes).unwrap();
    sink.sync(&[ahead]).unwrap();
    let applied = sink.cas_branch("main", Some(first.head_oid), ahead);
    assert_eq!(applied, Ok(BranchUpdate::Applied));

    let diverged = |error: ExportError| match error {
        ExportError::Diverged {
            git_head,
            head,
            reason,
            ..
        } => (git_head, head, reason),
        other => panic!("expected a divergence, got {other}"),
    };
    let error = export(repo, &mut gix(&destination.dir), &main_branch()).unwrap_err();
    let reason = DivergedReason::BranchAhead { exported: head };
    assert_eq!(diverged(error), (ahead, head, reason));
    assert_eq!(reason.as_str(), "branch_ahead");

    let newer = commit_on_main(repo, vec![put("b.txt", b"b\n")], "Add b.txt\n");
    let error = export(repo, &mut gix(&destination.dir), &main_branch()).unwrap_err();
    let reason = DivergedReason::BothMoved { exported: head };
    assert_eq!(diverged(error), (ahead, newer, reason));
    assert_eq!(reason.as_str(), "both_moved");
    assert_eq!(gix(&destination.dir).branch("main"), Ok(Some(ahead)));
}

/// A store with two native changesets on `main`, exported into an empty repository, then `main`
/// moved back to the first: the store, its first changeset, the export of the second, and the
/// destination.
fn moved_back() -> (Store, ChangesetId, ExportReport, git::EmptyRepository) {
    let store = store();
    let repo = &store.repo;
    let first = commit_on_main(repo, vec![put("a.txt", b"a\n")], "Add a.txt\n");
    let second = commit_on_main(repo, vec![put("b.txt", b"b\n")], "Add b.txt\n");
    let destination = git::empty_repository();
    let report = exported(repo, &mut gix(&destination.dir), &main_branch());
    assert_eq!(report.head, second);
    let back = repo
        .cas_ref("main", Some(second), Some(first), ACTOR)
        .unwrap();
    assert!(matches!(back.result, CasResult::Applied { .. }));
    (store, first, report, destination)
}

/// A branch at the export of a changeset the source no longer reaches (its ref moved back): the
/// source moved back, and nothing is written.
#[test]
fn a_source_moved_back_is_diverged() {
    let (store, first, exported_second, destination) = moved_back();
    let before = object_files(&destination.dir);
    let error = export(&store.repo, &mut gix(&destination.dir), &main_branch()).unwrap_err();
    let ExportError::Diverged {
        git_head,
        head,
        reason,
        ..
    } = error
    else {
        panic!("expected a divergence, got {error}");
    };
    let reason_expected = DivergedReason::SourceMovedBack {
        exported: exported_second.head,
    };
    assert_eq!(
        (git_head, head, reason),
        (exported_second.head_oid, first, reason_expected)
    );
    assert_eq!(reason.as_str(), "source_moved_back");
    assert_eq!(object_files(&destination.dir), before);
}

/// `force` moves a diverged branch to the export of the source's head, by compare-and-swap
/// against what the export read: a branch another writer moves meanwhile stops it all the same.
#[test]
fn force_moves_a_diverged_branch() {
    let (store, first, exported_second, destination) = moved_back();
    let repo = &store.repo;
    let forced = ExportRequest {
        force: true,
        ..main_branch()
    };
    let report = exported(repo, &mut gix(&destination.dir), &forced);
    assert_eq!(
        (report.head, report.head_oid),
        (first, export_oid(repo, first))
    );
    assert_eq!(
        (report.old, report.base, report.branch_moved),
        (
            Some(exported_second.head_oid),
            Some(exported_second.head),
            true
        )
    );
    assert_eq!((report.commits_written, report.objects_written), (0, 0));
    assert_eq!(
        gix(&destination.dir).branch("main"),
        Ok(Some(report.head_oid))
    );

    // The same in a sink whose branch another writer moves just before the swap.
    let mut sink = MemSink::default();
    let moved = commit_on_main(repo, vec![put("c.txt", b"c\n")], "Add c.txt\n");
    let at = exported(repo, &mut sink, &main_branch());
    assert_eq!(at.head, moved);
    let cas = repo
        .cas_ref("main", Some(moved), Some(first), ACTOR)
        .unwrap();
    assert!(matches!(cas.result, CasResult::Applied { .. }));
    let elsewhere = oid("0123456789abcdef0123456789abcdef01234567");
    sink.on_cas(move |_, actual| *actual = Some(elsewhere));
    let error = export(repo, &mut sink, &forced).unwrap_err();
    let ExportError::Diverged {
        git_head, reason, ..
    } = error
    else {
        panic!("expected a divergence, got {error}");
    };
    let reason_expected = DivergedReason::MovedDuringExport {
        actual: Some(elsewhere),
    };
    assert_eq!((git_head, reason), (elsewhere, reason_expected));
    assert_eq!(reason.as_str(), "moved_during_export");
}

/// `gitlinks`, imported with its gitlinks dropped, cannot be exported onto its own branch, whose
/// commit its head was imported from and exports otherwise; into an empty repository, the
/// commits from the first lossy one on are new (rewritten, and a warning says so), and the ones
/// before it keep their oids.
#[test]
fn a_lossy_history_cannot_export_onto_its_original() {
    let mut req = ImportRequest::new("HEAD", "main", ACTOR);
    req.lossy = [LossyClass::Gitlinks].into_iter().collect();
    let fx = Imported::with("gitlinks", &req);
    let repo = &fx.store.repo;
    let before = object_files(fx.repo_dir());
    let error = export(repo, &mut gix(fx.repo_dir()), &main_branch()).unwrap_err();
    let ExportError::Diverged {
        git_head,
        head,
        reason,
        ..
    } = error
    else {
        panic!("expected a divergence, got {error}");
    };
    let reason_expected = DivergedReason::LossyHistory {
        changeset: fx.report.head,
    };
    assert_eq!(
        (git_head, head, reason),
        (fx.tip(), fx.report.head, reason_expected)
    );
    assert_eq!(reason.as_str(), "lossy_history");
    assert_eq!(object_files(fx.repo_dir()), before);

    let destination = git::empty_repository();
    let report = exported(repo, &mut gix(&destination.dir), &main_branch());
    let commits = &fx.manifest().commits;
    let first_lossy = (commits.iter())
        .position(|commit| {
            let entry = repo.external_id(SYSTEM_GIT, &oid(&commit.oid).0).unwrap();
            !entry.expect("the import's entry").faithful
        })
        .expect("a lossy commit");
    assert!(first_lossy > 0, "a faithful commit comes first");
    for (at, commit) in commits.iter().enumerate() {
        let entry = repo.external_id(SYSTEM_GIT, &oid(&commit.oid).0).unwrap();
        let value = repo
            .derived::<ExportOid>(entry.unwrap().changeset, &())
            .unwrap();
        let kept = value.git_oid() == oid(&commit.oid);
        assert_eq!(
            (kept, value.kept),
            (at < first_lossy, at < first_lossy),
            "{at}"
        );
    }
    let new = u64::try_from(commits.len() - first_lossy).unwrap();
    assert_eq!(report.rewritten, new);
    assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
    assert!(
        report.warnings[0].contains(&new.to_string()),
        "{:?}",
        report.warnings
    );
    let kept = &commits[first_lossy - 1];
    let objects = reachable(&destination.dir, report.head_oid);
    assert_eq!(
        objects.get(&oid(&kept.oid)),
        Some(&(RawKind::Commit, kept.raw.clone()))
    );
}

/// A branch a work tree has checked out is refused before the store's ref or the branch is read,
/// and nothing is written; another branch of the same repository is written.
#[test]
fn a_checked_out_branch_is_refused() {
    let store = store();
    commit_on_main(&store.repo, vec![put("a.txt", b"a\n")], "Add a.txt\n");
    let work = work_tree();
    let dot_git = work.path().join(".git");
    let before = object_files(&dot_git);
    for source in ["main", "release/none"] {
        let req = ExportRequest::new(source, "main");
        let error = export(&store.repo, &mut gix(work.path()), &req).unwrap_err();
        assert!(
            matches!(&error, ExportError::CheckedOut { branch } if branch == "main"),
            "{error}"
        );
    }
    assert_eq!(object_files(&dot_git), before);
    let mut sink = MemSink::default();
    sink.set_checked_out(Some("main"));
    let error = export(&store.repo, &mut sink, &main_branch()).unwrap_err();
    assert!(matches!(error, ExportError::CheckedOut { .. }), "{error}");
    assert_eq!(sink.log(), []);

    let other = ExportRequest::new("main", "other");
    let report = exported(&store.repo, &mut gix(work.path()), &other);
    assert!(report.branch_moved);
    assert_eq!(gix(work.path()).branch("other"), Ok(Some(report.head_oid)));
    assert_eq!(gix(work.path()).branch("main"), Ok(None));
}

/// A loose object an earlier export left, emptied since (as a power loss can leave a file never
/// synced): the next export reads back every loose object it needs, writes the damaged one again,
/// and the branch reaches only objects that read back whole.
#[test]
fn a_damaged_loose_object_is_rewritten() {
    let store = store();
    let repo = &store.repo;
    commit_on_main(
        repo,
        vec![put("a.txt", b"a\n"), put("dir/b.txt", b"b\n")],
        "Add two files\n",
    );
    let destination = git::empty_repository();
    let first = exported(repo, &mut gix(&destination.dir), &main_branch());
    // The earlier export cut short before its move: its objects are there, its branch is not.
    fs::remove_file(destination.dir.join("refs/heads/main")).unwrap();
    let damaged = loose_path(&destination.dir, git_hash(RawKind::Blob, b"b\n"));
    fs::set_permissions(&damaged, fs::Permissions::from_mode(0o644)).unwrap();
    fs::write(&damaged, b"").unwrap();

    let report = exported(repo, &mut gix(&destination.dir), &main_branch());
    assert_eq!(report.head_oid, first.head_oid);
    assert_eq!((report.old, report.branch_moved), (None, true));
    assert_eq!(
        (
            report.commits_written,
            report.objects_written,
            report.objects_checked
        ),
        (0, 1, first.objects_written)
    );
    let objects = reachable(&destination.dir, report.head_oid);
    assert_eq!(u64::try_from(objects.len()).unwrap(), first.objects_written);
    let blob = git_hash(RawKind::Blob, b"b\n");
    assert_eq!(objects.get(&blob), Some(&(RawKind::Blob, b"b\n".to_vec())));
}

/// The export records each native commit it writes in the external id index, faithful, under the
/// commit's oid; the imported commits keep the import's entries, and no other is written.
#[test]
fn exported_native_commits_are_recorded_as_external_ids() {
    let fx = Imported::new("linear");
    let repo = &fx.store.repo;
    let natives = three_natives(repo);
    let entries = || -> BTreeMap<Vec<u8>, ExternalEntry> {
        let ids = ExternalIds::new(Layout::new(&fx.store.root), Arc::new(RealFs));
        (ids.list(SYSTEM_GIT).unwrap().into_iter())
            .map(|(key, entry)| (key, entry.expect("a whole entry")))
            .collect()
    };
    let before = entries();
    exported(repo, &mut gix(fx.repo_dir()), &main_branch());
    let mut after = entries();
    for native in natives {
        let entry = after.remove(export_oid(repo, native).0.as_slice());
        let expected = ExternalEntry {
            changeset: native,
            faithful: true,
        };
        assert_eq!(entry, Some(expected), "{native}");
    }
    assert_eq!(after, before, "the import's entries, and no other");
}

/// design-L107's test 5: a writer held between its frame's write and its sync, which then fails
/// and cuts the frame. An export started meanwhile waits for the lock, and exports the head
/// without the changeset that frame recorded.
#[test]
fn an_export_taken_while_a_writer_is_mid_commit_excludes_it() {
    let store = store();
    let head = commit_on_main(&store.repo, vec![put("a.txt", b"one\n")], "Add a.txt\n");
    let (fs, writer_held) = HoldThenFail::new(Layout::new(&store.root).journal_log(), RealFs);
    let writer = LocalRepo::open_with_fs(&store.root, Arc::new(fs)).unwrap();
    let mut exporter = LocalRepo::open(&store.root).unwrap();
    register(&mut exporter).unwrap();
    let destination = git::empty_repository();
    thread::scope(|scope| {
        // One frame records the changeset and moves `main` to it (`advance`).
        let writing = scope.spawn(|| {
            let req = CommitRequest {
                advance: Some(Advance {
                    name: "main".to_owned(),
                    actor: ACTOR.to_owned(),
                }),
                ..request(&[head], vec![put("a.txt", b"two\n")], "Change a.txt\n")
            };
            writer.commit(req)
        });
        writer_held.wait();

        let (started, starting) = mpsc::channel();
        let exporting = scope.spawn({
            let (exporter, dir) = (&exporter, destination.dir.as_path());
            move || {
                let mut sink = gix(dir);
                started.send(()).unwrap();
                export(exporter, &mut sink, &main_branch())
            }
        });
        starting.recv_timeout(Duration::from_secs(60)).unwrap();
        thread::sleep(Duration::from_millis(300));
        assert!(
            !exporting.is_finished(),
            "an export returned while a writer held the lock"
        );

        // Let go: the writer's sync fails, and it cuts its frame before it lets the lock go.
        writer_held.release();
        match writing.join().unwrap() {
            Err(RepoError::Store(StoreError::NotCommitted { source })) => {
                assert_eq!(source.raw_os_error(), Some(5), "{source}");
            }
            other => panic!("expected NotCommitted, got {other:?}"),
        }
        let report = exporting.join().unwrap().expect("the export");
        assert_eq!(
            (report.head, report.head_oid),
            (head, export_oid(&store.repo, head))
        );
        assert_eq!(
            gix(&destination.dir).branch("main"),
            Ok(Some(report.head_oid))
        );
    });
}

/// Two exports of one store into one repository at once, through two handles: both succeed, the
/// branch names the head's export, and no lock file is left. Each export's ref lock, held by the
/// other, is waited on until the other lets it go (the sink's retry hook), so that a slow fsync
/// under the other's lock never fails it.
#[cfg(feature = "test-hooks")]
#[test]
fn two_exports_racing_end_at_the_same_head() {
    use std::sync::Barrier;
    use std::time::Instant;

    let fx = Imported::new("linear");
    let natives = three_natives(&fx.store.repo);
    let head_oid = export_oid(&fx.store.repo, natives[2]);
    let destination = git::empty_repository();
    let lock = destination.dir.join("refs/heads/main.lock");
    let barrier = Barrier::new(2);
    let reports: Vec<ExportReport> = thread::scope(|scope| {
        let racers: Vec<_> = (0..2)
            .map(|_| {
                scope.spawn(|| {
                    let mut repo = LocalRepo::open(&fx.store.root).unwrap();
                    register(&mut repo).unwrap();
                    let mut sink = gix(&destination.dir);
                    let held = lock.clone();
                    sink.on_lock_retry(move || {
                        let patience = Instant::now() + Duration::from_secs(60);
                        while held.exists() && Instant::now() < patience {
                            thread::sleep(Duration::from_millis(1));
                        }
                    });
                    barrier.wait();
                    export(&repo, &mut sink, &main_branch())
                })
            })
            .collect();
        (racers.into_iter())
            .map(|racer| racer.join().unwrap().expect("each export"))
            .collect()
    });
    for report in &reports {
        assert_eq!(report.head_oid, head_oid);
    }
    let moved = reports.iter().filter(|report| report.branch_moved).count();
    assert_eq!(
        moved, 1,
        "one export moves the branch, the other finds it moved"
    );
    assert_eq!(gix(&destination.dir).branch("main"), Ok(Some(head_oid)));
    assert!(!lock.exists(), "a lock file is left");
    reachable(&destination.dir, head_oid);
}

/// Memos of the derived store (`meta/derived/git_blob_oid`, `git_tree_oid`, `git_oid`) placed
/// with a value their object does not give, each file whole and its trailer right: anyone who
/// writes a store's files can make one, as the trailer is a checksum over public fields, not a
/// signature. An export never takes such a memo for what an object is: what the branch names
/// holds the store's own content, never the object the memo names, which the repository holds
/// (packed, as git trusts it, or loose); the export refuses, naming the memo, before it writes
/// anything, and with the memo's file removed it writes the store's content.
mod crafted_memos {
    use std::io;

    use swarmops_core::ObjectId;
    use swarmops_git::{RawTreeEntry, Stored, verify_export_oids};
    use swarmops_store::DerivedSlot;

    use super::*;

    /// Places `value` as the memo `name` (version 1, no key) of `subject` in `store`'s derived
    /// store, its file whole and its trailer right, in place of any there. The file's path.
    fn plant(store: &Store, name: &str, subject: ObjectId, value: &[u8]) -> Utf8PathBuf {
        let slot = DerivedSlot {
            name,
            version: 1,
            subject,
            key: &[],
        };
        let file = Layout::new(&store.root).derived_path(&slot);
        match fs::remove_file(&file) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => panic!("remove {file}: {error}"),
        }
        let placed = store.repo.derived_store().put(&slot, value);
        assert!(
            placed.expect("place the memo"),
            "{file}: a memo is in the way"
        );
        file
    }

    /// A memo of `git_oid` for a changeset that exports to `commit`, as its file holds it: the
    /// commit's 20 bytes, then 1 for a commit that is the changeset's own.
    fn commit_memo(commit: GitOid) -> Vec<u8> {
        let mut value = commit.0.to_vec();
        value.push(1);
        value
    }

    /// The bytes of the file at `path` (`/` between its names) in git's tree `tree` of the
    /// repository at `dir`; `None` if nothing, or no file, is there.
    fn file_at(dir: &Path, tree: GitOid, path: &str) -> Option<Vec<u8>> {
        let source = GixSource::open(dir).unwrap();
        let mut at = tree;
        for name in path.split('/') {
            let (kind, bytes) = source.read(at).unwrap();
            if kind != RawKind::Tree {
                return None;
            }
            let tree = RawTree::parse(&bytes).unwrap();
            at = tree.entries.iter().find(|e| e.name == name.as_bytes())?.oid;
        }
        let (kind, bytes) = source.read(at).unwrap();
        (kind == RawKind::Blob).then_some(bytes)
    }

    /// Checks what an export into the repository at `dir`, whose branch `main` named `before`,
    /// did with a crafted memo of `memo` in the store, which holds `content` at `path` in the
    /// head's tree: wherever the branch is, the file there is the store's; the export refused,
    /// naming the memo; and the branch did not move.
    #[track_caller]
    fn refused(
        result: &Result<ExportReport, ExportError>,
        dir: &Path,
        before: Option<GitOid>,
        (path, content): (&str, &[u8]),
        memo: &str,
    ) {
        let after = gix(dir).branch("main").unwrap();
        if let Some(head) = after.filter(|&head| Some(head) != before) {
            let tree = commit_at(dir, head).tree;
            assert_eq!(
                file_at(dir, tree, path).as_deref(),
                Some(content),
                "the branch moved to {head}, whose {path} is not the store's: {result:?}"
            );
        }
        match result {
            Err(ExportError::Repo(error)) => {
                let text = error.to_string();
                assert!(text.contains(memo), "{text}");
            }
            other => panic!("an export over a crafted {memo} memo: {other:?}"),
        }
        assert_eq!(after, before, "the branch moved");
    }

    /// With `file`, a crafted memo, removed, the export of `store`'s `main` into the repository
    /// at `dir` moves the branch to a commit other than `planted`, whose `path` holds the store's
    /// `content` and whose tree is the head's root.
    #[track_caller]
    fn exported_once_removed(
        store: &Store,
        dir: &Path,
        file: &Utf8PathBuf,
        (path, content): (&str, &[u8]),
        planted: GitOid,
    ) {
        fs::remove_file(file).unwrap();
        let report = exported(&store.repo, &mut gix(dir), &main_branch());
        assert!(report.branch_moved, "{report:?}");
        assert_ne!(report.head_oid, planted);
        assert_eq!(gix(dir).branch("main"), Ok(Some(report.head_oid)));
        let commit = commit_at(dir, report.head_oid);
        assert_eq!(file_at(dir, commit.tree, path).as_deref(), Some(content));
        let source = GixSource::open(dir).unwrap();
        same_tree(&source, commit.tree, &store.repo, report.head);
    }

    /// A `git_blob_oid` memo naming another blob the repository holds: in a pack (the fixture's
    /// own `hello.txt` of its first commit, under the content a native changeset gives it), or as
    /// a loose file of an empty repository someone placed there.
    #[test]
    fn a_crafted_blob_memo_puts_no_other_blob_in_the_branch() {
        let fx = Imported::new("linear");
        let repo = &fx.store.repo;
        let new: &[u8] = b"hello, again\n";
        commit_on_main(repo, vec![put("hello.txt", new)], "Change hello.txt\n");
        let packed = git_hash(RawKind::Blob, b"hello\n");
        assert_eq!(gix(fx.repo_dir()).stored(packed), Ok(Stored::Packed));
        let file = plant(&fx.store, "git_blob_oid", blob(new).0, &packed.0);
        let result = export(repo, &mut gix(fx.repo_dir()), &main_branch());
        let file_and_content = ("hello.txt", new);
        refused(
            &result,
            fx.repo_dir(),
            Some(fx.tip()),
            file_and_content,
            "git_blob_oid",
        );
        exported_once_removed(&fx.store, fx.repo_dir(), &file, file_and_content, packed);

        let store = store();
        let (good, evil): (&[u8], &[u8]) = (b"#!/bin/sh\necho GOOD\n", b"#!/bin/sh\necho EVIL\n");
        let app = put_as("app.sh", LeafKind::Exec, good);
        commit_on_main(&store.repo, vec![app], "Add app.sh\n");
        let destination = git::empty_repository();
        let mut sink = gix(&destination.dir);
        let loose = sink.write_object(RawKind::Blob, evil).unwrap();
        sink.sync(&[loose]).unwrap();
        assert_eq!(sink.stored(loose), Ok(Stored::Loose));
        let file = plant(&store, "git_blob_oid", blob(good).0, &loose.0);
        let result = export(&store.repo, &mut gix(&destination.dir), &main_branch());
        let file_and_content = ("app.sh", good);
        refused(
            &result,
            &destination.dir,
            None,
            file_and_content,
            "git_blob_oid",
        );
        exported_once_removed(&store, &destination.dir, &file, file_and_content, loose);
    }

    /// A `git_tree_oid` memo for a native changeset's root naming another tree the repository
    /// holds in a pack: the fixture's tip's root, which a changed `hello.txt` is not.
    #[test]
    fn a_crafted_tree_memo_puts_no_other_tree_in_the_branch() {
        let fx = Imported::new("linear");
        let repo = &fx.store.repo;
        let new: &[u8] = b"hello, again\n";
        let native = commit_on_main(repo, vec![put("hello.txt", new)], "Change hello.txt\n");
        let root = repo.changeset(native).unwrap().unwrap().changeset.root;
        let packed = commit_at(fx.repo_dir(), fx.tip()).tree;
        assert_eq!(gix(fx.repo_dir()).stored(packed), Ok(Stored::Packed));
        let file = plant(&fx.store, "git_tree_oid", root.0, &packed.0);
        let result = export(repo, &mut gix(fx.repo_dir()), &main_branch());
        let file_and_content = ("hello.txt", new);
        refused(
            &result,
            fx.repo_dir(),
            Some(fx.tip()),
            file_and_content,
            "git_tree_oid",
        );
        exported_once_removed(&fx.store, fx.repo_dir(), &file, file_and_content, packed);
    }

    /// A `git_oid` memo for the head naming another commit the repository holds: in a pack (the
    /// fixture's first commit, which the branch would go back to), or as a loose file someone
    /// placed on the branch's commit (whose `app.sh` is not the store's). The branch never names
    /// it.
    #[test]
    fn a_crafted_commit_memo_never_moves_the_branch_to_another_commit() {
        let fx = Imported::new("linear");
        let repo = &fx.store.repo;
        let new: &[u8] = b"hello, again\n";
        let native = commit_on_main(repo, vec![put("hello.txt", new)], "Change hello.txt\n");
        let packed = oid(&fx.manifest().commits[0].oid);
        assert_eq!(gix(fx.repo_dir()).stored(packed), Ok(Stored::Packed));
        let file = plant(&fx.store, "git_oid", native.0, &commit_memo(packed));
        let result = export(repo, &mut gix(fx.repo_dir()), &main_branch());
        assert_ne!(gix(fx.repo_dir()).branch("main"), Ok(Some(packed)));
        let file_and_content = ("hello.txt", new);
        refused(
            &result,
            fx.repo_dir(),
            Some(fx.tip()),
            file_and_content,
            "git_oid",
        );
        exported_once_removed(&fx.store, fx.repo_dir(), &file, file_and_content, packed);

        let store = store();
        let (one, two): (&[u8], &[u8]) = (b"echo one\n", b"echo two\n");
        commit_on_main(&store.repo, vec![put("app.sh", one)], "Add app.sh\n");
        let destination = git::empty_repository();
        let first = exported(&store.repo, &mut gix(&destination.dir), &main_branch());
        let second = commit_on_main(&store.repo, vec![put("app.sh", two)], "Change app.sh\n");
        let mut sink = gix(&destination.dir);
        let evil = sink.write_object(RawKind::Blob, b"echo EVIL\n").unwrap();
        let tree = RawTree {
            entries: vec![RawTreeEntry {
                mode: b"100644".to_vec(),
                name: b"app.sh".to_vec(),
                oid: evil,
            }],
        };
        let tree = sink.write_object(RawKind::Tree, &tree.render()).unwrap();
        let parents = [first.head_oid];
        let bytes = commit_bytes(tree, &parents, &signature(), &signature(), &[], b"two\n");
        let loose = sink.write_object(RawKind::Commit, &bytes).unwrap();
        sink.sync(&[evil, tree, loose]).unwrap();
        assert_eq!(sink.stored(loose), Ok(Stored::Loose));
        let file = plant(&store, "git_oid", second.0, &commit_memo(loose));
        let result = export(&store.repo, &mut gix(&destination.dir), &main_branch());
        assert_ne!(gix(&destination.dir).branch("main"), Ok(Some(loose)));
        let file_and_content = ("app.sh", two);
        refused(
            &result,
            &destination.dir,
            Some(first.head_oid),
            file_and_content,
            "git_oid",
        );
        exported_once_removed(&store, &destination.dir, &file, file_and_content, loose);
    }

    /// A deep verify re-derives every git export oid memo against the store's objects and reports
    /// each that is not what they give — a `git_blob_oid`, a `git_tree_oid` and a `git_oid` planted
    /// over the oids a legit export wrote — as it does the other derived values; a sound store has
    /// none.
    #[test]
    fn verify_deep_reports_a_crafted_git_memo() {
        let store = store();
        let repo = &store.repo;
        let good: &[u8] = b"#!/bin/sh\necho GOOD\n";
        let head = commit_on_main(repo, vec![put("app.sh", good)], "Add app.sh\n");
        // A legit export writes every memo (git_blob_oid, git_tree_oid, git_oid).
        let destination = git::empty_repository();
        exported(repo, &mut gix(&destination.dir), &main_branch());
        assert!(
            verify_export_oids(repo).expect("verify").is_empty(),
            "a sound store has no mismatch",
        );

        // A crafted git_blob_oid: the app.sh blob's memo names another blob's oid.
        let app = blob(good).0;
        let evil = git_hash(RawKind::Blob, b"#!/bin/sh\necho EVIL\n");
        plant(&store, "git_blob_oid", app, &evil.0);
        let found = verify_export_oids(repo).expect("verify");
        let blob_memo = found
            .iter()
            .find(|m| m.memo == "git_blob_oid")
            .unwrap_or_else(|| panic!("no git_blob_oid mismatch in {found:?}"));
        assert_eq!(blob_memo.subject, app);
        assert_eq!(blob_memo.stored, evil);

        // A crafted git_tree_oid: the root's memo names another tree's oid.
        let root = repo.changeset(head).unwrap().unwrap().changeset.root;
        let other_tree = git_hash(RawKind::Tree, b"");
        plant(&store, "git_tree_oid", root.0, &other_tree.0);
        assert!(
            (verify_export_oids(repo).expect("verify").iter())
                .any(|m| m.memo == "git_tree_oid" && m.subject == root.0),
            "no git_tree_oid mismatch reported",
        );

        // A crafted git_oid: the head's export oid memo names another commit.
        let other_commit = git_hash(RawKind::Commit, b"");
        plant(&store, "git_oid", head.0, &commit_memo(other_commit));
        assert!(
            (verify_export_oids(repo).expect("verify").iter())
                .any(|m| m.memo == "git_oid" && m.subject == head.0 && m.stored == other_commit),
            "no git_oid mismatch reported",
        );
    }

    /// A deep verify re-derives the export oid of a changeset atop a long history without
    /// recursion: over a first-parent chain of 10,000 native changesets and a merge of its tip
    /// with a second root (recorded in one raw batch, on a filesystem that never syncs), whose
    /// head alone has a `git_oid` memo, a crafted one, it reports that memo, with the oid the
    /// engine derives over both parents in their order, on a thread with a 256 KiB stack, which a
    /// walk that recursed once a parent would overflow.
    #[test]
    fn a_long_history_verifies_without_recursion() {
        const DEPTH: usize = 10_000;
        let store = store();
        let repo = &store.repo;
        let root = store_tree(repo, &[("f.txt", b"deep\n")]);
        let mut batch = repo.raw_batch();
        let mut tip: Option<ChangesetId> = None;
        for i in 0..DEPTH {
            let message = format!("{i}\n");
            tip = Some(batch.push(raw(tip.as_slice(), root, &message)).unwrap());
        }
        let tip = tip.expect("a chain of at least one changeset");
        let side = batch.push(raw(&[], root, "A second root\n")).unwrap();
        let merge = raw(&[tip, side], root, "Merge the second root\n");
        let head = batch.push(merge).unwrap();
        batch.commit().unwrap();

        // The head's memo alone, a crafted one: the verify derives the whole chain below it.
        let crafted = git_hash(RawKind::Commit, b"");
        let file = plant(&store, "git_oid", head.0, &commit_memo(crafted));
        let found = thread::scope(|scope| {
            thread::Builder::new()
                .name("a deep verify".to_owned())
                .stack_size(256 * 1024)
                .spawn_scoped(scope, || verify_export_oids(repo))
                .unwrap()
                .join()
                .unwrap()
        })
        .expect("verify");

        // That memo, against the export oid the engine derives once the memo is gone.
        fs::remove_file(&file).unwrap();
        let real = export_oid(repo, head);
        let [mismatch] = found.as_slice() else {
            panic!("one mismatch, the head's: {found:?}");
        };
        let got = (mismatch.memo.as_str(), mismatch.subject, mismatch.stored);
        assert_eq!(got, ("git_oid", head.0, crafted));
        assert_eq!(mismatch.real, real);
    }
}
