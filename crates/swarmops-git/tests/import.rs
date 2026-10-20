//! The git import: every commit a git ref reaches comes into a store parents first, many to a
//! journal frame, each changeset holding its commit's oid and named by the external id index;
//! run again, the import takes only what is new; it refuses a target that diverged, writing
//! nothing; and an import cut short by a failed write resumes, run again, to the state an
//! uninterrupted one reaches.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_core::ChangesetId;
use swarmops_git::{
    ConvertError, GIT_LOSSY, GIT_OID, GitOid, GitSource, GixSource, ImportError, ImportReport,
    ImportRequest, LossyClass, LossySet, ObjectFormat, RawKind, RawTree, SourceError, Unsupported,
    import,
};
use swarmops_repo::{CasResult, LocalRepo, RepoError, StoreError};
use swarmops_store::{
    ExternalEntry, ExternalIds, Frame, FsMethod, Journal, Layout, RealFs, Record, SYSTEM_GIT,
};
use swarmops_testkit::fs::{FailingFs, NoSyncFs};
use swarmops_testkit::git::{self, GitFixture, Manifest};
use swarmops_testkit::repo::{commit, new_repo, put};

use crate::support::MemSource;

/// The fixtures every commit of which a changeset holds as git wrote it (Task 8's list).
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

/// Who moves the target in these tests.
const ACTOR: &str = "test/importer";

/// A request to import `git_ref` into `main`.
fn request(git_ref: &str) -> ImportRequest {
    ImportRequest::new(git_ref, "main", ACTOR)
}

fn oid(hex: &str) -> GitOid {
    hex.parse().unwrap_or_else(|error| panic!("{hex}: {error}"))
}

fn manifest(fixture: &GitFixture) -> &Manifest {
    fixture
        .manifest
        .as_ref()
        .expect("the fixture has a manifest")
}

fn open(fixture: &GitFixture) -> GixSource {
    let repo = fixture
        .repo
        .as_deref()
        .expect("the fixture holds a repository");
    GixSource::open(repo).unwrap_or_else(|error| panic!("open: {error}"))
}

/// A fresh store that never flushes to stable storage ([`NoSyncFs`]): for the tests whose
/// subject is what the store holds, not what survives a power loss.
fn unsynced_repo() -> (tempfile::TempDir, Utf8PathBuf, LocalRepo) {
    let dir = tempfile::tempdir().expect("a temp dir");
    let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).expect("a UTF-8 temp dir");
    let repo = LocalRepo::create_with_fs(&root, "test", Arc::new(NoSyncFs)).expect("create");
    (dir, root, repo)
}

/// The commit the manifest's ref `name` names: `HEAD`'s ref, or the branch `name`.
fn tip(manifest: &Manifest, name: &str) -> String {
    let full = if name == "HEAD" {
        manifest.head.clone().expect("HEAD names a branch")
    } else {
        format!("refs/heads/{name}")
    };
    let tip = manifest.refs.get(&full);
    tip.unwrap_or_else(|| panic!("the manifest has no {full}"))
        .clone()
}

/// Every commit `tip` reaches in the manifest, `tip` included.
fn reached(manifest: &Manifest, tip: &str) -> BTreeSet<String> {
    let parents: HashMap<&str, &[String]> = (manifest.commits.iter())
        .map(|commit| (commit.oid.as_str(), commit.parents.as_slice()))
        .collect();
    let mut reached = BTreeSet::new();
    let mut stack = vec![tip.to_owned()];
    while let Some(commit) = stack.pop() {
        if reached.insert(commit.clone()) {
            stack.extend(parents[commit.as_str()].iter().cloned());
        }
    }
    reached
}

/// The first-parent chain from `tip` in the manifest, newest first.
fn first_parents(manifest: &Manifest, tip: &str) -> Vec<GitOid> {
    let first: HashMap<&str, Option<&String>> = (manifest.commits.iter())
        .map(|commit| (commit.oid.as_str(), commit.parents.first()))
        .collect();
    let mut chain = vec![oid(tip)];
    let mut at = tip;
    while let Some(parent) = first[at] {
        chain.push(oid(parent));
        at = parent;
    }
    chain
}

/// Every blob the tree of the manifest's commit `commit` reaches.
fn blobs(manifest: &Manifest, commit: &str) -> BTreeSet<GitOid> {
    let tree = manifest
        .commits
        .iter()
        .find(|c| c.oid == commit)
        .unwrap_or_else(|| panic!("no commit {commit}"))
        .tree
        .clone();
    let mut blobs = BTreeSet::new();
    let mut trees = vec![tree];
    while let Some(tree) = trees.pop() {
        let bytes = manifest
            .trees
            .get(&tree)
            .expect("the manifest holds the tree");
        for entry in RawTree::parse(bytes).expect("a tree").entries {
            match entry.mode.as_slice() {
                b"40000" => trees.push(entry.oid.to_string()),
                b"160000" => {}
                _ => {
                    blobs.insert(entry.oid);
                }
            }
        }
    }
    blobs
}

/// Whether the tree of the manifest's commit `commit` holds a gitlink, at any depth.
fn holds_a_gitlink(manifest: &Manifest, commit: &str) -> bool {
    let tree = &manifest
        .commits
        .iter()
        .find(|c| c.oid == commit)
        .unwrap()
        .tree;
    let mut trees = vec![tree.clone()];
    while let Some(tree) = trees.pop() {
        for entry in RawTree::parse(&manifest.trees[&tree]).unwrap().entries {
            match entry.mode.as_slice() {
                b"160000" => return true,
                b"40000" => trees.push(entry.oid.to_string()),
                _ => {}
            }
        }
    }
    false
}

/// The git commit changeset `id` holds: its `git.oid` metadata.
fn git_oid(repo: &LocalRepo, id: ChangesetId) -> GitOid {
    let stored = repo
        .changeset(id)
        .unwrap()
        .expect("the changeset is stored");
    metadata(&stored.changeset.metadata, GIT_OID)
        .and_then(|value| GitOid::from_hex(&value))
        .unwrap_or_else(|| panic!("changeset {id} holds no git.oid"))
}

/// The value of metadata key `key`.
fn metadata(pairs: &[(String, Vec<u8>)], key: &str) -> Option<Vec<u8>> {
    (pairs.iter())
        .find(|(k, _)| k == key)
        .map(|(_, value)| value.clone())
}

/// What `main` names.
fn main_head(repo: &LocalRepo) -> ChangesetId {
    repo.get_ref("main").unwrap().expect("main exists").target
}

/// The commits of `main`'s first-parent history, newest first.
fn main_log(repo: &LocalRepo) -> Vec<GitOid> {
    let log = repo.log_first_parent(main_head(repo), usize::MAX).unwrap();
    log.iter().map(|stored| git_oid(repo, stored.id)).collect()
}

fn journal_len(root: &Utf8Path) -> u64 {
    fs::metadata(root.join("meta/journal.log")).unwrap().len()
}

/// The journal's frames, as a reader reads them.
fn frames(root: &Utf8Path) -> Vec<Frame> {
    let journal = Journal::open(&Layout::new(root), Arc::new(RealFs)).expect("open the journal");
    journal.tail_frames_from(0).expect("read the journal")
}

/// Every file of the store at `root`, by its path in the store, with its bytes.
fn files(root: &Utf8Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut dirs = vec![root.as_std_path().to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                dirs.push(path);
            } else {
                let name = path.strip_prefix(root).unwrap().to_path_buf();
                files.insert(name, fs::read(&path).unwrap());
            }
        }
    }
    files
}

/// How many object files the store at `root` holds.
fn objects(root: &Utf8Path) -> usize {
    files(root)
        .keys()
        .filter(|path| path.starts_with("objects") && !path.starts_with("objects/tmp"))
        .count()
}

/// What an import leaves in the store at `root`: the changesets, the commit `main` names, and
/// every git entry of the external id index.
fn state(root: &Utf8Path) -> (u64, ChangesetId, Vec<(Vec<u8>, ExternalEntry)>) {
    let repo = LocalRepo::open(root).unwrap();
    let ids = ExternalIds::new(Layout::new(root), Arc::new(RealFs));
    let entries = (ids.list(SYSTEM_GIT).unwrap().into_iter())
        .map(|(key, entry)| (key, entry.expect("a whole entry")))
        .collect();
    (repo.info().unwrap().changesets, main_head(&repo), entries)
}

/// A [`GitSource`] that records every object read through it.
struct Counting<S> {
    inner: S,
    reads: RefCell<Vec<(GitOid, RawKind)>>,
}

impl<S: GitSource> GitSource for Counting<S> {
    fn object_format(&self) -> ObjectFormat {
        self.inner.object_format()
    }

    fn resolve(&self, name: &str) -> Result<GitOid, SourceError> {
        self.inner.resolve(name)
    }

    fn header(&self, oid: GitOid) -> Result<(RawKind, u64), SourceError> {
        self.inner.header(oid)
    }

    fn read(&self, oid: GitOid) -> Result<(RawKind, Vec<u8>), SourceError> {
        let (kind, bytes) = self.inner.read(oid)?;
        self.reads.borrow_mut().push((oid, kind));
        Ok((kind, bytes))
    }
}

/// The import of a faithful fixture's `HEAD` brings in every commit it reaches, each as the
/// changeset that holds its oid and that the external id index names for it; the target's
/// first-parent history is git's; and the store verifies.
#[test]
fn every_faithful_fixture_imports_whole() {
    for name in FAITHFUL {
        let fixture = git::fixture(name);
        let manifest = manifest(&fixture);
        let tip = tip(manifest, "HEAD");
        let reached = reached(manifest, &tip);
        let (_dir, _root, repo) = unsynced_repo();

        let report = import(&repo, &open(&fixture), &request("HEAD"))
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(
            report.imported,
            u64::try_from(reached.len()).unwrap(),
            "{name}"
        );
        assert_eq!(report.skipped, 0, "{name}");
        assert_eq!((report.old_head, report.ref_moved), (None, true), "{name}");
        assert_eq!(
            (report.lossy_total, &report.warnings[..]),
            (0, &[][..]),
            "{name}"
        );
        assert_eq!(report.git_head, oid(&tip), "{name}");
        assert_eq!(main_head(&repo), report.head, "{name}");
        assert_eq!(git_oid(&repo, report.head), report.git_head, "{name}");
        for commit in &reached {
            let entry = repo.external_id(SYSTEM_GIT, &oid(commit).0).unwrap();
            let entry = entry.unwrap_or_else(|| panic!("{name}: no entry for {commit}"));
            assert!(entry.faithful, "{name}: {commit}");
            assert_eq!(git_oid(&repo, entry.changeset), oid(commit), "{name}");
        }
        assert_eq!(repo.verify(true).unwrap().problems, [], "{name}");
        assert_eq!(main_log(&repo), first_parents(manifest, &tip), "{name}");
    }
}

/// An import of what the store holds already, the target at the tip's changeset, takes nothing
/// and writes nothing.
#[test]
fn import_again_imports_nothing_and_writes_nothing() {
    let fixture = git::fixture("merges");
    let source = open(&fixture);
    let (_dir, root, repo) = new_repo();
    let first = import(&repo, &source, &request("HEAD")).unwrap();
    let (log_len, object_count, before) = (journal_len(&root), objects(&root), files(&root));

    let again = import(&repo, &source, &request("HEAD")).unwrap();
    assert_eq!(
        (again.imported, again.skipped, again.ref_moved),
        (0, 1, false)
    );
    assert_eq!((again.head, again.old_head), (first.head, Some(first.head)));
    assert_eq!(
        (journal_len(&root), objects(&root)),
        (log_len, object_count)
    );
    assert_eq!(files(&root), before, "no file of the store changed");
}

/// After new commits, an import takes those alone and moves the target on from its head.
#[test]
fn an_incremental_import_takes_only_the_new_commits() {
    let fixture = git::fixture("refs");
    let manifest = manifest(&fixture);
    let source = open(&fixture);
    let (_dir, _root, repo) = unsynced_repo();
    let older = import(&repo, &source, &request("older")).unwrap();
    assert_eq!((older.imported, older.skipped), (3, 0));

    let newer = import(&repo, &source, &request("newer")).unwrap();
    assert_eq!(
        (newer.imported, newer.skipped, newer.ref_moved),
        (3, 1, true)
    );
    assert_eq!(newer.old_head, Some(older.head));
    assert_eq!(newer.git_head, oid(&tip(manifest, "newer")));
    assert_eq!(main_head(&repo), newer.head);
    assert_eq!(
        main_log(&repo),
        first_parents(manifest, &tip(manifest, "newer"))
    );
    assert_eq!(repo.verify(true).unwrap().problems, []);
}

/// The base is the commit the target's head holds, known whatever the external id index holds:
/// with the index lost (it is a cache), an incremental import still goes on from the head.
#[test]
fn the_base_is_known_without_the_index() {
    let fixture = git::fixture("refs");
    let source = open(&fixture);
    let (_dir, root, repo) = unsynced_repo();
    let older = import(&repo, &source, &request("older")).unwrap();
    fs::remove_dir_all(root.join("meta/external_ids")).unwrap();

    let newer = import(&repo, &source, &request("newer")).unwrap();
    assert_eq!(
        (newer.imported, newer.skipped, newer.ref_moved),
        (3, 1, true)
    );
    assert_eq!(newer.old_head, Some(older.head));
    assert_eq!(repo.verify(true).unwrap().problems, []);
}

/// A base the tip reaches only through a merge's second parent is no divergence: `merged` forks
/// before `older`'s tip and merges it.
#[test]
fn a_base_reached_through_a_second_parent_is_not_diverged() {
    let fixture = git::fixture("refs");
    let manifest = manifest(&fixture);
    let source = open(&fixture);
    let (_dir, _root, repo) = unsynced_repo();
    let older = import(&repo, &source, &request("older")).unwrap();

    let merged = import(&repo, &source, &request("merged")).unwrap();
    // The side commit and the merge; the walk stops at the fork (the index names it) and at
    // the base, the merge's second parent.
    assert_eq!(
        (merged.imported, merged.skipped, merged.ref_moved),
        (2, 2, true)
    );
    assert_eq!(merged.old_head, Some(older.head));
    assert_eq!(
        git_oid(&repo, main_head(&repo)),
        oid(&tip(manifest, "merged"))
    );
    assert_eq!(repo.verify(true).unwrap().problems, []);
}

/// A tip the external id index names already, brought in by an import into another ref, stops
/// the walk at once: the store's own parents then say whether the target's head is behind it.
/// `newer` descends from `older`, so `main` moves on to it, writing no changeset; `rewritten`
/// does not, so it is diverged.
#[test]
fn a_known_tip_moves_the_target_only_from_its_ancestor() {
    let fixture = git::fixture("refs");
    let manifest = manifest(&fixture);
    let source = open(&fixture);
    let (_dir, _root, mut repo) = unsynced_repo();
    // Before it says the target diverged, the import tries the commit its head exports to.
    swarmops_git::register(&mut repo).unwrap();
    let into = |git_ref: &str, target: &str| {
        import(&repo, &source, &ImportRequest::new(git_ref, target, ACTOR))
    };
    into("newer", "release/newer").unwrap();
    into("rewritten", "release/rewritten").unwrap();
    let older = into("older", "main").unwrap();
    assert_eq!(
        (older.imported, older.skipped, older.ref_moved),
        (0, 1, true)
    );

    let error = into("rewritten", "main").unwrap_err();
    let older_tip = oid(&tip(manifest, "older"));
    assert!(
        matches!(error, ImportError::Diverged { base: Some(base), .. } if base == older_tip),
        "{error:?}"
    );
    let newer = into("newer", "main").unwrap();
    assert_eq!(
        (newer.imported, newer.skipped, newer.ref_moved),
        (0, 1, true)
    );
    assert_eq!(newer.old_head, Some(older.head));
    assert_eq!(
        git_oid(&repo, main_head(&repo)),
        oid(&tip(manifest, "newer"))
    );
}

/// An external id entry that is damaged, or names a changeset the store lacks, is no stop: the
/// walk goes past its commit, which is converted again into the changeset the store holds, and
/// the import warns once, leaving the entry as it is.
#[test]
fn a_damaged_or_dangling_external_id_is_a_miss_and_a_warning() {
    use swarmops_core::{ObjectKind, hash_object};

    for damage in ["damaged", "dangling"] {
        let fixture = git::fixture("refs");
        let manifest = manifest(&fixture);
        let source = open(&fixture);
        let (_dir, root, repo) = unsynced_repo();
        import(&repo, &source, &request("older")).unwrap();
        // `older`'s second commit, which `merged` reaches through its side commit.
        let fork = first_parents(manifest, &tip(manifest, "older"))[1];
        let hex = fork.to_string();
        let entry = root
            .join("meta/external_ids/git")
            .join(&hex[..2])
            .join(&hex[2..]);
        if damage == "damaged" {
            fs::write(&entry, b"no entry").unwrap();
        } else {
            fs::remove_file(&entry).unwrap();
            let nowhere = ChangesetId(hash_object(ObjectKind::Changeset, b"nowhere"));
            let dangling = ExternalEntry {
                changeset: nowhere,
                faithful: true,
            };
            let ids = ExternalIds::new(Layout::new(&root), Arc::new(RealFs));
            ids.put_batch(SYSTEM_GIT, &[(fork.0.to_vec(), dangling)])
                .unwrap();
        }
        let before = fs::read(&entry).unwrap();

        let merged = import(&repo, &source, &request("merged")).unwrap();
        // The fork, the side commit and the merge; the walk stops at the first commit and at
        // the base.
        assert_eq!((merged.imported, merged.skipped), (3, 2), "{damage}");
        let [warning] = &merged.warnings[..] else {
            panic!("{damage}: one warning: {:?}", merged.warnings);
        };
        assert!(warning.contains(&hex), "{damage}: {warning}");
        assert_eq!(
            fs::read(&entry).unwrap(),
            before,
            "{damage}: the entry as it was"
        );
        assert_eq!(
            git_oid(&repo, main_head(&repo)),
            oid(&tip(manifest, "merged"))
        );
    }
}

/// A source rewritten under the target, its tip no longer reaching the target's head, is
/// diverged, and the import writes nothing: only the derived store, a cache, gains the export
/// oids it derived to learn that the head exports to its own commit (M8 decision 7).
#[test]
fn a_rewritten_source_is_diverged_and_writes_nothing() {
    let fixture = git::fixture("refs");
    let manifest = manifest(&fixture);
    let source = open(&fixture);
    let (_dir, root, mut repo) = new_repo();
    swarmops_git::register(&mut repo).unwrap();
    let older = import(&repo, &source, &request("older")).unwrap();
    let (log_len, before) = (journal_len(&root), files_but_derived(&root));

    let error = import(&repo, &source, &request("rewritten")).unwrap_err();
    let ImportError::Diverged {
        target_ref,
        target_head,
        base,
        git_head,
    } = error
    else {
        panic!("diverged: {error:?}");
    };
    assert_eq!(target_ref, "main");
    assert_eq!(target_head, older.head);
    assert_eq!(base, Some(oid(&tip(manifest, "older"))));
    assert_eq!(git_head, oid(&tip(manifest, "rewritten")));
    assert_eq!(journal_len(&root), log_len);
    assert_eq!(files_but_derived(&root), before, "nothing written");
}

/// [`files`] but the derived store's (`meta/derived/`): a cache, which an import fills when it
/// derives the export oid of its target's head (M8 decision 7).
fn files_but_derived(root: &Utf8Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = files(root);
    files.retain(|path, _| !path.starts_with("meta/derived"));
    files
}

/// A native changeset committed on `main`'s head (`native.txt`), and `main` moved to it.
fn native_on_main(repo: &LocalRepo) -> ChangesetId {
    let head = main_head(repo);
    let native = commit(repo, &[head], vec![put("native.txt", b"native\n")]);
    let moved = repo
        .cas_ref("main", Some(head), Some(native), "test/native")
        .unwrap();
    assert!(matches!(moved.result, CasResult::Applied { .. }));
    native
}

/// Writes into the repository at `dir` a commit made in git: tree `tree`, the one parent
/// `parent`, a fixed author and committer, and `message`; then moves its branch `main` from
/// `parent` to it, as git would. The commit.
fn git_commit(dir: &std::path::Path, tree: GitOid, parent: GitOid, message: &str) -> GitOid {
    use swarmops_core::Signature;
    use swarmops_git::{BranchUpdate, GitSink, GixSink, commit_bytes};

    let mut sink = GixSink::open(dir).unwrap_or_else(|error| panic!("open: {error}"));
    let author = Signature {
        name: b"A Git User".to_vec(),
        email: b"git@example.com".to_vec(),
        secs: 1_760_000_100,
        offset_secs: 3_600,
        negative_zero: false,
    };
    let bytes = commit_bytes(tree, &[parent], &author, &author, &[], message.as_bytes());
    let commit = sink.write_object(RawKind::Commit, &bytes).unwrap();
    sink.sync(&[commit]).unwrap();
    let moved = sink.cas_branch("main", Some(parent), commit);
    assert_eq!(moved, Ok(BranchUpdate::Applied));
    commit
}

/// Writes into the repository at `dir` a commit made in git on top of `parent` whose tree is
/// `parent`'s with one more file, `file` (holding its own name), and moves `main` to it
/// ([`git_commit`]). The commit.
fn commit_in_git(dir: &std::path::Path, parent: GitOid, file: &str) -> GitOid {
    use swarmops_git::{GitSink, GixSink, RawCommit, RawTreeEntry};

    let mut sink = GixSink::open(dir).unwrap_or_else(|error| panic!("open: {error}"));
    let (_, bytes) = sink.read(parent).unwrap();
    let (_, bytes) = sink.read(RawCommit::parse(&bytes).unwrap().tree).unwrap();
    let mut tree = RawTree::parse(&bytes).unwrap();
    let blob = sink.write_object(RawKind::Blob, file.as_bytes()).unwrap();
    tree.entries.push(RawTreeEntry {
        mode: b"100644".to_vec(),
        name: file.as_bytes().to_vec(),
        oid: blob,
    });
    // git's order: the names' bytes, a directory's as if it ended in `/`.
    tree.entries.sort_by_key(|entry| {
        let mut key = entry.name.clone();
        if entry.mode == b"40000" {
            key.push(b'/');
        }
        key
    });
    let tree = sink.write_object(RawKind::Tree, &tree.render()).unwrap();
    sink.sync(&[blob, tree]).unwrap();
    git_commit(dir, tree, parent, &format!("Add {file}\n"))
}

/// The parents of changeset `id`.
fn parents(repo: &LocalRepo, id: ChangesetId) -> Vec<ChangesetId> {
    let stored = repo.changeset(id).unwrap();
    stored.expect("the changeset is stored").changeset.parents
}

/// A native changeset on the target that was never exported: the import goes on from the commit
/// it exports to (M8 decision 7), which the tip does not reach, so the import is diverged with
/// that commit as its base, and writes nothing but the derived values it made to find it.
#[test]
fn an_import_onto_an_unexported_native_head_is_diverged() {
    use swarmops_git::{ExportOid, register};

    let fixture = git::fixture("linear");
    let source = open(&fixture);
    let (_dir, root, mut repo) = new_repo();
    register(&mut repo).unwrap();
    import(&repo, &source, &request("HEAD")).unwrap();
    let native = native_on_main(&repo);
    let (log_len, before) = (journal_len(&root), files_but_derived(&root));

    let error = import(&repo, &source, &request("HEAD")).unwrap_err();
    let exported = repo.derived::<ExportOid>(native, &()).unwrap().git_oid();
    assert!(
        matches!(
            error,
            ImportError::Diverged { base: Some(base), target_head, .. }
                if base == exported && target_head == native
        ),
        "{error:?}"
    );
    assert_eq!(journal_len(&root), log_len);
    assert_eq!(files_but_derived(&root), before, "nothing written");
}

/// The base of a native head is the commit it exports to, which a repository derives only with
/// `ExportOid` registered, as every `LocalOps` registers it: without it an import onto a native
/// head fails before it writes anything.
#[test]
fn an_import_onto_a_native_head_needs_export_oids_registered() {
    let fixture = git::fixture("linear");
    let source = open(&fixture);
    let (_dir, root, repo) = new_repo();
    import(&repo, &source, &request("HEAD")).unwrap();
    native_on_main(&repo);
    let before = files(&root);

    let error = import(&repo, &source, &request("HEAD")).unwrap_err();
    assert!(
        matches!(
            &error,
            ImportError::Store {
                source: RepoError::Derived { name, .. },
                imported: 0,
                last: None,
            } if name == "git_oid"
        ),
        "{error:?}"
    );
    assert_eq!(files(&root), before, "nothing written");
}

/// Commits made in git on top of an export come back (M8 decision 7): `linear` imported, a
/// native changeset committed on `main` and exported into the fixture's own repository, and a
/// commit made there on top of the export. The import of `main` takes that commit alone, onto
/// the native changeset, whose export is the base and seeds the conversion, so that the one blob
/// read is the new file's; an export afterwards writes nothing, the commit being its changeset's
/// export.
#[test]
fn an_import_after_an_export_takes_only_the_git_side_commits() {
    use swarmops_git::{ExportRequest, GixSink, export, git_hash, register};

    let fixture = git::fixture("linear");
    let dir = fixture
        .repo
        .clone()
        .expect("the fixture holds a repository");
    let (_dir, _root, mut repo) = unsynced_repo();
    register(&mut repo).unwrap();
    import(&repo, &open(&fixture), &request("HEAD")).unwrap();
    let native = native_on_main(&repo);
    let to_main = ExportRequest::new("main", "main");
    let exported = export(&repo, &mut GixSink::open(&dir).unwrap(), &to_main).unwrap();
    assert_eq!((exported.head, exported.commits_written), (native, 1));
    let made = commit_in_git(&dir, exported.head_oid, "git.txt");

    let counting = Counting {
        inner: open(&fixture),
        reads: RefCell::new(Vec::new()),
    };
    let report = import(&repo, &counting, &request("main")).unwrap();
    assert_eq!(
        (report.imported, report.skipped, report.ref_moved),
        (1, 1, true)
    );
    assert_eq!((report.old_head, report.git_head), (Some(native), made));
    let blobs: Vec<GitOid> = (counting.reads.borrow().iter())
        .filter(|(_, kind)| *kind == RawKind::Blob)
        .map(|(oid, _)| *oid)
        .collect();
    assert_eq!(blobs, [git_hash(RawKind::Blob, b"git.txt")]);
    assert_eq!(main_head(&repo), report.head);
    assert_eq!(parents(&repo, report.head), [native]);
    assert_eq!(git_oid(&repo, report.head), made);
    assert_eq!(repo.verify(true).unwrap().problems, []);

    let again = export(&repo, &mut GixSink::open(&dir).unwrap(), &to_main).unwrap();
    assert_eq!((again.head_oid, again.old), (made, Some(made)));
    assert_eq!(
        (
            again.branch_moved,
            again.commits_written,
            again.objects_written
        ),
        (false, 0, 0)
    );
}

/// A head imported with a loss exports to another commit than its own, and a tip descending from
/// either continues it, from the one it descends from (M8 decision 7). `gitlinks`' second commit
/// is imported with its gitlinks dropped, then its third, which descends from the original
/// second: onto it. That history exported into an empty repository becomes new commits there
/// from the second on, and a commit made there on top of the export is imported onto the third
/// changeset, the export its base and seeding the conversion, so that the one blob read is the
/// new file's; an export afterwards writes nothing. A history that descends from neither is
/// diverged, naming the head's own commit.
#[test]
fn a_lossy_head_takes_the_oid_the_tip_descends_from() {
    use swarmops_git::{ExportOid, ExportRequest, GixSink, export, git_hash, register};

    let fixture = git::fixture("gitlinks");
    let manifest = manifest(&fixture);
    let source = open(&fixture);
    let (_dir, _root, mut repo) = unsynced_repo();
    register(&mut repo).unwrap();
    let lossy = |git_ref: &str| {
        let mut req = request(git_ref);
        req.lossy = [LossyClass::Gitlinks].into_iter().collect();
        req
    };
    let second = import(&repo, &source, &lossy(&manifest.commits[1].oid)).unwrap();
    let third = import(&repo, &source, &lossy("HEAD")).unwrap();
    assert_eq!(
        (third.imported, third.skipped, third.old_head),
        (1, 1, Some(second.head))
    );
    let head = third.head;
    let own = git_oid(&repo, head);
    let exported = repo.derived::<ExportOid>(head, &()).unwrap();
    assert!(!exported.kept);
    assert_ne!(exported.git_oid(), own);

    let destination = git::empty_repository();
    let to_main = ExportRequest::new("main", "main");
    let mut sink = GixSink::open(&destination.dir).unwrap();
    let report = export(&repo, &mut sink, &to_main).unwrap();
    assert_eq!(report.head_oid, exported.git_oid());

    let linear = git::fixture("linear");
    let error = import(&repo, &open(&linear), &request("HEAD")).unwrap_err();
    assert!(
        matches!(
            error,
            ImportError::Diverged { base: Some(base), target_head, .. }
                if base == own && target_head == head
        ),
        "{error:?}"
    );

    let made = commit_in_git(&destination.dir, report.head_oid, "git.txt");
    let from_export = Counting {
        inner: GixSource::open(&destination.dir).unwrap(),
        reads: RefCell::new(Vec::new()),
    };
    let continued = import(&repo, &from_export, &request("main")).unwrap();
    assert_eq!(
        (continued.imported, continued.skipped, continued.ref_moved),
        (1, 1, true)
    );
    assert_eq!((continued.old_head, continued.git_head), (Some(head), made));
    assert_eq!(parents(&repo, continued.head), [head]);
    assert_eq!(git_oid(&repo, continued.head), made);
    let blobs: Vec<GitOid> = (from_export.reads.borrow().iter())
        .filter(|(_, kind)| *kind == RawKind::Blob)
        .map(|(oid, _)| *oid)
        .collect();
    assert_eq!(blobs, [git_hash(RawKind::Blob, b"git.txt")]);

    let again = export(
        &repo,
        &mut GixSink::open(&destination.dir).unwrap(),
        &to_main,
    )
    .unwrap();
    assert_eq!(
        (again.head_oid, again.branch_moved, again.objects_written),
        (made, false, 0)
    );
}

/// A head that is faithful itself but sits on a commit imported with a loss exports to another
/// commit than its own too, and a tip descending from that export continues it. With the target
/// moved back to that head afterwards, a tip that reaches it only through a commit the store
/// holds already continues it as well, from a repository that lacks the head's own commit: the
/// base, not met by the walk, then seeds nothing.
#[test]
fn a_head_above_a_lossy_commit_is_continued_from_its_export() {
    use swarmops_git::{ExportOid, ExportRequest, GixSink, export, register};

    let fixture = git::fixture("gitlinks");
    let manifest = manifest(&fixture);
    let dir = fixture
        .repo
        .clone()
        .expect("the fixture holds a repository");
    let (_dir, _root, mut repo) = unsynced_repo();
    register(&mut repo).unwrap();
    let mut req = request("HEAD");
    req.lossy = [LossyClass::Gitlinks].into_iter().collect();
    let lossy = import(&repo, &open(&fixture), &req).unwrap();
    // A commit made in git on the original that drops both gitlinks: the first commit's tree.
    let first_tree = oid(&manifest.commits[0].tree);
    let dropped = git_commit(&dir, first_tree, lossy.git_head, "Drop the gitlinks\n");
    let faithful = import(&repo, &open(&fixture), &request("main")).unwrap();
    assert_eq!(
        (faithful.imported, faithful.lossy_total, faithful.git_head),
        (1, 0, dropped)
    );
    let head = faithful.head;
    let exported = repo.derived::<ExportOid>(head, &()).unwrap();
    assert!(!exported.kept);

    let destination = git::empty_repository();
    let to_main = ExportRequest::new("main", "main");
    let report = export(
        &repo,
        &mut GixSink::open(&destination.dir).unwrap(),
        &to_main,
    )
    .unwrap();
    assert_eq!(report.head_oid, exported.git_oid());
    let made = commit_in_git(&destination.dir, report.head_oid, "git.txt");
    let from_export = || GixSource::open(&destination.dir).unwrap();
    let continued = import(&repo, &from_export(), &request("main")).unwrap();
    assert_eq!(
        (continued.imported, continued.skipped, continued.old_head),
        (1, 1, Some(head))
    );
    assert_eq!(parents(&repo, continued.head), [head]);

    let back = repo
        .cas_ref("main", Some(continued.head), Some(head), ACTOR)
        .unwrap();
    assert!(matches!(back.result, CasResult::Applied { .. }));
    let more = commit_in_git(&destination.dir, made, "more.txt");
    let again = import(&repo, &from_export(), &request("main")).unwrap();
    assert_eq!(
        (again.imported, again.skipped, again.old_head),
        (1, 1, Some(head))
    );
    assert_eq!(parents(&repo, again.head), [continued.head]);
    assert_eq!(git_oid(&repo, again.head), more);
}

/// A [`GitSource`] whose first read of the tip moves `main` to a native changeset, as another
/// writer would while the import runs.
struct MovesMain<'r> {
    inner: GixSource,
    repo: &'r LocalRepo,
    tip: GitOid,
    native: Cell<Option<ChangesetId>>,
}

impl GitSource for MovesMain<'_> {
    fn object_format(&self) -> ObjectFormat {
        self.inner.object_format()
    }

    fn resolve(&self, name: &str) -> Result<GitOid, SourceError> {
        self.inner.resolve(name)
    }

    fn header(&self, oid: GitOid) -> Result<(RawKind, u64), SourceError> {
        self.inner.header(oid)
    }

    fn read(&self, oid: GitOid) -> Result<(RawKind, Vec<u8>), SourceError> {
        if oid == self.tip && self.native.get().is_none() {
            let head = main_head(self.repo);
            let native = commit(self.repo, &[head], vec![put("native.txt", b"native\n")]);
            let moved = self
                .repo
                .cas_ref("main", Some(head), Some(native), "test/native");
            assert!(matches!(moved.unwrap().result, CasResult::Applied { .. }));
            self.native.set(Some(native));
        }
        self.inner.read(oid)
    }
}

/// The target moved by another writer while the import ran: the import records every commit
/// and external id, and its compare-and-swap reports what the target names.
#[test]
fn a_target_moved_meanwhile_is_a_cas_mismatch() {
    let fixture = git::fixture("refs");
    let manifest = manifest(&fixture);
    let (_dir, _root, repo) = unsynced_repo();
    import(&repo, &open(&fixture), &request("older")).unwrap();
    let moving = MovesMain {
        inner: open(&fixture),
        repo: &repo,
        tip: oid(&tip(manifest, "newer")),
        native: Cell::new(None),
    };

    let error = import(&repo, &moving, &request("newer")).unwrap_err();
    let native = moving.native.get().expect("main moved during the import");
    assert!(
        matches!(
            &error,
            ImportError::CasMismatch { target_ref, actual } if target_ref == "main"
                && *actual == Some(native)
        ),
        "{error:?}"
    );
    assert_eq!(main_head(&repo), native, "the import did not move it");
    for commit in first_parents(manifest, &tip(manifest, "newer")) {
        assert!(repo.external_id(SYSTEM_GIT, &commit.0).unwrap().is_some());
    }
}

/// The import flushes the metadata, so that no reader parses its frames again: afterwards the
/// checkpoint is the journal's end.
#[test]
fn the_import_leaves_no_frame_past_the_checkpoint() {
    let fixture = git::fixture("linear");
    let (_dir, root, repo) = new_repo();
    import(&repo, &open(&fixture), &request("HEAD")).unwrap();
    let journal = Journal::open(&Layout::new(&root), Arc::new(RealFs)).unwrap();
    assert!(
        frames(&root).len() >= 2,
        "a frame of changesets and the target's move"
    );
    assert_eq!(journal.ckpt().unwrap(), journal_len(&root));
}

/// A frame holds at most `frame_changesets` changesets, and the index compacts about once a
/// frame: 3,300 commits in frames of 1,100 are three frames, and at most ⌈3,300 / 1,100⌉ + 1
/// compactions.
#[test]
fn frames_hold_at_most_their_size_and_compactions_stay_few() {
    let mut source = MemSource::default();
    let mut parent: Option<GitOid> = None;
    for n in 1..=3_300 {
        let blob = source.blob(format!("{n}\n").as_bytes());
        let tree = source.tree(&[("100644", b"count.txt", blob)]);
        let message = format!("count to {n}\n");
        parent = Some(source.commit(tree, parent.as_slice(), &message));
    }
    let tip = parent.expect("3,300 commits");
    let (_dir, root, repo) = unsynced_repo();
    let mut req = request(&tip.to_string());
    req.frame_changesets = 1_100;

    let report = import(&repo, &source, &req).unwrap();
    assert_eq!(report.imported, 3_300);
    let frames = frames(&root);
    let holding = |frame: &Frame| -> usize {
        let changesets = (frame.records.iter())
            .filter(|record| matches!(record, Record::Changeset { .. }))
            .count();
        assert!(changesets == 0 || changesets == frame.records.len());
        changesets
    };
    let changeset_frames: Vec<usize> = frames.iter().map(holding).filter(|&n| n > 0).collect();
    assert_eq!(changeset_frames, [1_100, 1_100, 1_100]);
    let compactions = (frames.iter())
        .filter(|frame| matches!(frame.records[..], [Record::Compacted { .. }]))
        .count();
    assert!((1..=4).contains(&compactions), "{compactions} compactions");
}

/// An import whose second frame fails to commit stops after the first, saying so; run again on
/// a fresh handle, it takes the rest and ends where an uninterrupted import ends.
#[test]
fn a_crashed_import_resumes_past_its_committed_frames() {
    let fixture = git::fixture("refs");
    let manifest = manifest(&fixture);
    let source = open(&fixture);
    let (_dir, root, repo) = new_repo();
    drop(repo);
    let appends = AtomicUsize::new(0);
    let failing = FailingFs::new(move |method, path| {
        method == FsMethod::AppendDurable
            && path.file_name().is_some_and(|name| name == "journal.log")
            && appends.fetch_add(1, Ordering::SeqCst) == 1
    });
    let repo = LocalRepo::open_with_fs(&root, Arc::new(failing)).unwrap();
    let mut req = request("newer");
    req.frame_changesets = 2;

    let error = import(&repo, &source, &req).unwrap_err();
    let second = first_parents(manifest, &tip(manifest, "newer"))[4];
    assert!(
        matches!(
            error,
            ImportError::Store {
                source: RepoError::Store(StoreError::NotCommitted { .. }),
                imported: 2,
                last: Some(last),
            } if last == second
        ),
        "{error:?}"
    );
    drop(repo);

    let repo = LocalRepo::open(&root).unwrap();
    let resumed = import(&repo, &source, &req).unwrap();
    assert_eq!(resumed.imported, 4);
    assert!(resumed.skipped >= 1);
    let (_other_dir, other, fresh) = new_repo();
    let whole = import(&fresh, &source, &req).unwrap();
    assert_eq!(resumed.head, whole.head);
    assert_eq!(state(&root), state(&other));
    assert_eq!(repo.verify(true).unwrap().problems, []);
}

/// A frame that commits but whose listing in `journal.off` fails, a failure after its commit
/// point, stops the import after it, before its external ids; run again, the import records
/// them and the rest, and ends where an uninterrupted import ends.
#[test]
fn a_frame_failing_after_its_commit_point_stops_the_import() {
    let fixture = git::fixture("refs");
    let manifest = manifest(&fixture);
    let source = open(&fixture);
    let (_dir, root, repo) = new_repo();
    drop(repo);
    let listings = AtomicUsize::new(0);
    let failing = FailingFs::new(move |method, path| {
        method == FsMethod::AppendDurable
            && path.file_name().is_some_and(|name| name == "journal.off")
            && listings.fetch_add(1, Ordering::SeqCst) == 1
    });
    let repo = LocalRepo::open_with_fs(&root, Arc::new(failing)).unwrap();
    let mut req = request("newer");
    req.frame_changesets = 2;
    let chain = first_parents(manifest, &tip(manifest, "newer"));

    let error = import(&repo, &source, &req).unwrap_err();
    assert!(
        matches!(
            &error,
            ImportError::PostCommit { imported: 4, last: Some(last), warnings, .. }
                if *last == chain[2] && warnings.len() == 1
        ),
        "{error:?}"
    );
    let entry = |commit: GitOid| repo.external_id(SYSTEM_GIT, &commit.0).unwrap();
    assert!(entry(chain[4]).is_some(), "the first frame's external ids");
    assert!(entry(chain[2]).is_none(), "none of the second's");
    drop(repo);

    let repo = LocalRepo::open(&root).unwrap();
    let resumed = import(&repo, &source, &req).unwrap();
    assert_eq!((resumed.imported, resumed.skipped), (4, 1));
    let (_other_dir, other, fresh) = new_repo();
    import(&fresh, &source, &req).unwrap();
    assert_eq!(state(&root), state(&other));
    assert_eq!(repo.verify(true).unwrap().problems, []);
}

/// An incremental import reads the new commits' trees and blobs alone: a blob of a file the
/// new commits leave as the base has it is never read, whether that is `refs`' newer commits
/// (each changes the one file) or `linear`'s last two (which leave `hello.txt` alone).
#[test]
fn an_incremental_import_reads_only_what_changed() {
    let refs = git::fixture("refs");
    let linear = git::fixture("linear");
    let linear_third = manifest(&linear).commits[2].oid.clone();
    let cases = [
        (&refs, "older".to_owned(), "newer".to_owned()),
        (&linear, linear_third, "HEAD".to_owned()),
    ];
    for (fixture, base, then) in cases {
        let manifest = manifest(fixture);
        let (_dir, _root, repo) = unsynced_repo();
        let base_report = import(&repo, &open(fixture), &request(&base)).unwrap();
        let counting = Counting {
            inner: open(fixture),
            reads: RefCell::new(Vec::new()),
        };

        let report = import(&repo, &counting, &request(&then)).unwrap();
        assert_eq!(report.old_head, Some(base_report.head));
        let base_tip = base_report.git_head.to_string();
        let new_commits: BTreeSet<String> = reached(manifest, &report.git_head.to_string())
            .difference(&reached(manifest, &base_tip))
            .cloned()
            .collect();
        let mut wanted = BTreeSet::new();
        for commit in &new_commits {
            wanted.extend(blobs(manifest, commit));
        }
        let left_alone = blobs(manifest, &base_tip);
        let wanted: BTreeSet<GitOid> = wanted.difference(&left_alone).copied().collect();
        let read: Vec<GitOid> = (counting.reads.borrow().iter())
            .filter(|(_, kind)| *kind == RawKind::Blob)
            .map(|(oid, _)| *oid)
            .collect();
        let once: BTreeSet<GitOid> = read.iter().copied().collect();
        assert_eq!(
            once.len(),
            read.len(),
            "{then}: a blob read twice: {read:?}"
        );
        assert_eq!(once, wanted, "{then}");
    }
    // `linear`'s case reads around a file the new commits leave alone.
    let linear_manifest = manifest(&linear);
    let hello = blobs(linear_manifest, &linear_manifest.commits[2].oid);
    assert!(hello.is_subset(&blobs(linear_manifest, &tip(linear_manifest, "HEAD"))));
}

/// With `gitlinks` allowed, each commit that holds a gitlink is recorded with the loss, listed
/// in the report, and marked in its changeset (`git.lossy`) and its external id entry.
#[test]
fn lossy_commits_are_listed_in_the_report() {
    let fixture = git::fixture("gitlinks");
    let manifest = manifest(&fixture);
    let (_dir, _root, repo) = unsynced_repo();
    let mut req = request("HEAD");
    req.lossy = [LossyClass::Gitlinks].into_iter().collect();

    let report = import(&repo, &open(&fixture), &req).unwrap();
    let holding: BTreeSet<GitOid> = (reached(manifest, &tip(manifest, "HEAD")).iter())
        .filter(|commit| holds_a_gitlink(manifest, commit))
        .map(|commit| oid(commit))
        .collect();
    assert!(!holding.is_empty());
    assert_eq!(report.lossy_total, u64::try_from(holding.len()).unwrap());
    let listed: BTreeSet<GitOid> = report.lossy.iter().map(|lossy| lossy.commit).collect();
    assert_eq!(listed, holding);
    let gitlinks: LossySet = [LossyClass::Gitlinks].into_iter().collect();
    for lossy in &report.lossy {
        assert_eq!(lossy.classes, gitlinks);
        let stored = repo.changeset(lossy.changeset).unwrap().unwrap();
        let marked = metadata(&stored.changeset.metadata, GIT_LOSSY);
        assert_eq!(marked.as_deref(), Some(&b"gitlinks"[..]));
        let entry = repo
            .external_id(SYSTEM_GIT, &lossy.commit.0)
            .unwrap()
            .unwrap();
        assert_eq!(entry.changeset, lossy.changeset);
        assert!(!entry.faithful);
    }
    assert_eq!(repo.verify(true).unwrap().problems, []);
}

/// The report lists the first hundred lossy commits, in the order recorded, and counts them all.
#[test]
fn the_report_lists_the_first_hundred_lossy_commits() {
    let mut source = MemSource::default();
    let mut commits = Vec::new();
    for n in 1..=105 {
        let blob = source.blob(format!("{n}\n").as_bytes());
        // A gitlink beside the file, so that each commit is lossy.
        let tree = source.tree(&[("100644", b"count.txt", blob), ("160000", b"sub", blob)]);
        let parents: Vec<GitOid> = commits.last().copied().into_iter().collect();
        commits.push(source.commit(tree, &parents, &format!("commit {n}\n")));
    }
    let (_dir, _root, repo) = unsynced_repo();
    let mut req = request(&commits[104].to_string());
    req.lossy = [LossyClass::Gitlinks].into_iter().collect();

    let report = import(&repo, &source, &req).unwrap();
    assert_eq!((report.imported, report.lossy_total), (105, 105));
    let listed: Vec<GitOid> = report.lossy.iter().map(|lossy| lossy.commit).collect();
    assert_eq!(listed, commits[..100]);
}

/// Without `gitlinks` allowed, the commit that holds one is refused by name, and nothing of the
/// frame that would hold it is written.
#[test]
fn a_refused_commit_stops_the_import_before_its_frame() {
    let fixture = git::fixture("gitlinks");
    let (_dir, root, repo) = new_repo();
    let before = files(&root);

    let error = import(&repo, &open(&fixture), &request("HEAD")).unwrap_err();
    assert!(
        matches!(
            &error,
            ImportError::Convert(ConvertError::Lossy { class: LossyClass::Gitlinks, path, .. })
                if path == b"lib/sub"
        ),
        "{error:?}"
    );
    assert_eq!(
        files(&root),
        before,
        "journal.log, objects and index as they were"
    );
}

/// Two imports of one source into one store at once, each on a handle of its own, both succeed
/// and leave the target at the tip's changeset; one of them moved it.
#[test]
fn two_imports_racing_end_at_the_same_head() {
    let fixture = git::fixture("merges");
    let manifest = manifest(&fixture);
    let (_dir, root, repo) = new_repo();
    drop(repo);
    let start = Barrier::new(2);

    let reports: Vec<ImportReport> = std::thread::scope(|scope| {
        let racers: Vec<_> = (0..2)
            .map(|_| {
                scope.spawn(|| {
                    let repo = LocalRepo::open(&root).unwrap();
                    let source = open(&fixture);
                    start.wait();
                    import(&repo, &source, &request("HEAD"))
                })
            })
            .collect();
        (racers.into_iter())
            .map(|racer| racer.join().expect("no panic").expect("both succeed"))
            .collect()
    });
    let repo = LocalRepo::open(&root).unwrap();
    for report in &reports {
        assert_eq!(report.head, main_head(&repo));
    }
    assert_eq!(reports.iter().filter(|report| report.ref_moved).count(), 1);
    assert_eq!(
        git_oid(&repo, main_head(&repo)),
        oid(&tip(manifest, "HEAD"))
    );
    assert_eq!(repo.verify(true).unwrap().problems, []);
}

/// What the import does not read is refused before anything is written: a SHA-256 repository and
/// a shallow clone when the source opens, a partial clone's missing blob and a damaged object
/// when the conversion reads them; the store stays as `init` left it.
#[test]
fn the_unsupported_fixtures_are_refused_before_writing() {
    for (name, unsupported) in [
        ("sha256", Unsupported::ObjectFormat("sha256".to_owned())),
        ("shallow", Unsupported::Shallow),
    ] {
        let fixture = git::fixture(name);
        let repo = fixture.repo.as_deref().expect("a repository");
        let refused = GixSource::open(repo).map(|_| ()).unwrap_err();
        assert_eq!(refused, SourceError::Unsupported(unsupported), "{name}");
    }
    for name in ["partial_clone", "corrupt_object"] {
        let fixture = git::fixture(name);
        let (_dir, root, repo) = new_repo();
        let before = files(&root);
        let error = import(&repo, &open(&fixture), &request("HEAD")).unwrap_err();
        let unread = match name {
            "partial_clone" => matches!(
                error,
                ImportError::Convert(ConvertError::Source(SourceError::Missing { .. }))
            ),
            _ => matches!(
                error,
                ImportError::Convert(ConvertError::Source(SourceError::Corrupt { .. }))
            ),
        };
        assert!(unread, "{name}: {error:?}");
        assert_eq!(files(&root), before, "{name}: the store as init left it");
    }
}

/// A source of another object format than SHA-1 is refused before it is read, whatever its
/// kind: the import itself checks, not only `GixSource::open`.
#[test]
fn a_source_of_another_object_format_is_refused_before_reading() {
    struct Sha256Claim(MemSource);

    impl GitSource for Sha256Claim {
        fn object_format(&self) -> ObjectFormat {
            ObjectFormat::Sha256
        }

        fn resolve(&self, name: &str) -> Result<GitOid, SourceError> {
            panic!("{name} was resolved")
        }

        fn header(&self, oid: GitOid) -> Result<(RawKind, u64), SourceError> {
            self.0.header(oid)
        }

        fn read(&self, oid: GitOid) -> Result<(RawKind, Vec<u8>), SourceError> {
            self.0.read(oid)
        }
    }

    let (_dir, root, repo) = new_repo();
    let before = files(&root);
    let error = import(&repo, &Sha256Claim(MemSource::default()), &request("HEAD")).unwrap_err();
    assert!(
        matches!(
            &error,
            ImportError::Source(SourceError::Unsupported(Unsupported::ObjectFormat(format)))
                if format == "sha256"
        ),
        "{error:?}"
    );
    assert_eq!(files(&root), before);
}

/// A target that is no ref name is refused before the source is read or the store written.
#[test]
fn a_target_that_is_no_ref_name_is_refused() {
    let fixture = git::fixture("linear");
    let (_dir, root, repo) = new_repo();
    let before = files(&root);
    let counting = Counting {
        inner: open(&fixture),
        reads: RefCell::new(Vec::new()),
    };
    let req = ImportRequest::new("HEAD", "feature/x", ACTOR);

    let error = import(&repo, &counting, &req).unwrap_err();
    assert!(
        matches!(&error, ImportError::RefName(name) if name == "feature/x"),
        "{error:?}"
    );
    assert!(counting.reads.borrow().is_empty());
    assert_eq!(files(&root), before);
}
