//! The round trip, umbrella §13's completion condition for M8: a git history imported into a
//! store, changes landed on it, and the history exported back. Every faithful fixture keeps its
//! commits byte for byte and gains one single-parent commit per land, signed by the agent that
//! made the change and by the lander, its message ending in SwarmOps' trailers, its tree the
//! store's; a land undone after its export stops the next export (`source_moved_back`) until it is
//! forced. And umbrella §13's single-writer layout (M8 decisions 4 and 7): one host lands and
//! exports to a git remote and another imports it, each step taking only what is new; when both
//! write, both stop, and nothing is merged.
//!
//! Every test owns fresh stores, with `ExportOid` registered as every `LocalOps` registers it, on
//! a filesystem that never flushes: the subject is what crosses to git and back, not what a store
//! keeps through a power loss. Destinations are a fixture's own `repo.git` or
//! `git::empty_repository()`, read back through the import's reader (`GixSource`).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use camino::Utf8PathBuf;
use swarmops_core::bytes::to_u64;
use swarmops_core::{ChangesetId, EntryTarget, LeafKind, RepoPath, Signature};
use swarmops_git::{
    DivergedReason, ExportError, ExportOid, ExportReport, ExportRequest, GIT_LOSSY, GIT_OID,
    GitOid, GitSink, GitSource, GixSink, GixSource, ImportError, ImportReport, ImportRequest,
    RawCommit, RawIdent, RawKind, RawTree, Stored, export, import, register,
};
use swarmops_repo::{Change, CommitRequest, LandRequest, LandStatus, LocalRepo, OpRecord};
use swarmops_testkit::fs::NoSyncFs;
use swarmops_testkit::git::{self, GitFixture, Manifest};
use swarmops_testkit::repo::{put, put_as, request};
use tempfile::TempDir;

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

/// Who moves the store refs these tests move themselves: the imports' targets, an undo.
const ACTOR: &str = "test/round-trip";

/// The agent that makes the changes the tests land, as its `agent` metadata names it.
const AGENT: &str = "agent/worker-17";

/// The first landed change's message: a title alone, which SwarmOps' trailers follow after a
/// blank line.
const FIRST_MESSAGE: &str = "Add landed/one.txt\n";

/// The second's: a title and a trailer block of its own, which SwarmOps' trailers join.
const SECOND_MESSAGE: &str =
    "Change landed/one.txt, add landed/run.sh\n\nCo-Authored-By: A Person <a@example.com>\n";

/// The agent's signature, as SwarmOps' operations sign a commit: the agent's name, and
/// `<instance>@<family>.swarmops.local` at offset 0.
fn agent() -> Signature {
    Signature {
        name: AGENT.as_bytes().to_vec(),
        email: b"worker-17@claude-code.swarmops.local".to_vec(),
        secs: 1_760_000_000,
        offset_secs: 0,
        negative_zero: false,
    }
}

/// The lander's signature: the committer of every changeset a land makes.
fn lander() -> Signature {
    Signature {
        name: b"swarmops/lander".to_vec(),
        email: b"lander@swarmops.swarmops.local".to_vec(),
        secs: 1_760_000_100,
        offset_secs: 0,
        negative_zero: false,
    }
}

/// `sig` as git's commit header holds it.
fn ident(sig: &Signature) -> RawIdent {
    RawIdent {
        name: sig.name.clone(),
        email: sig.email.clone(),
        secs: sig.secs,
        offset_secs: sig.offset_secs,
        negative_zero: sig.negative_zero,
    }
}

fn oid(hex: &str) -> GitOid {
    hex.parse().unwrap_or_else(|error| panic!("{hex}: {error}"))
}

/// A store of its own with `ExportOid` registered, on a filesystem that never flushes
/// ([`NoSyncFs`]).
struct Store {
    _dir: TempDir,
    repo: LocalRepo,
}

fn store() -> Store {
    let dir = tempfile::tempdir().expect("a temp dir");
    let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).expect("a UTF-8 temp dir");
    let mut repo = LocalRepo::create_with_fs(&root, "test", Arc::new(NoSyncFs)).expect("create");
    register(&mut repo).expect("register ExportOid");
    Store { _dir: dir, repo }
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

fn gix(dir: &Path) -> GixSink {
    GixSink::open(dir).unwrap_or_else(|error| panic!("open {}: {error}", dir.display()))
}

fn source(dir: &Path) -> GixSource {
    GixSource::open(dir).unwrap_or_else(|error| panic!("open {}: {error}", dir.display()))
}

/// What the branch `main` names in the repository at `dir`.
fn branch(dir: &Path) -> Option<GitOid> {
    gix(dir).branch("main").expect("read the branch")
}

/// A request to export `main` into the branch `main`.
fn main_branch() -> ExportRequest {
    ExportRequest::new("main", "main")
}

/// `main`'s head in `repo`.
fn main_head(repo: &LocalRepo) -> ChangesetId {
    repo.get_ref("main").unwrap().expect("main exists").target
}

/// Changeset `id`'s export oid.
fn export_oid(repo: &LocalRepo, id: ChangesetId) -> GitOid {
    repo.derived::<ExportOid>(id, &()).unwrap().git_oid()
}

/// The newest operation of `repo`'s log: its frame's seq.
fn last_op(repo: &LocalRepo) -> u64 {
    let (ops, _) = repo.op_log(None, 1).unwrap();
    ops[0].seq
}

/// `export`, which must succeed.
#[track_caller]
fn exported(repo: &LocalRepo, sink: &mut dyn GitSink, req: &ExportRequest) -> ExportReport {
    export(repo, sink, req).unwrap_or_else(|error| panic!("export: {error}"))
}

/// The import of `git_ref` from the repository at `dir` into `main`, which must succeed.
#[track_caller]
fn imported(repo: &LocalRepo, dir: &Path, git_ref: &str) -> ImportReport {
    let req = ImportRequest::new(git_ref, "main", ACTOR);
    import(repo, &source(dir), &req).unwrap_or_else(|error| panic!("import: {error}"))
}

/// The import of `main` from the repository at `dir` into `main`, which must fail: its error.
#[track_caller]
fn import_fails(repo: &LocalRepo, dir: &Path) -> ImportError {
    let req = ImportRequest::new("main", "main", ACTOR);
    match import(repo, &source(dir), &req) {
        Ok(report) => panic!("the import succeeded: {report:?}"),
        Err(error) => error,
    }
}

/// The export of `main` into the branch `main` of `sink`, which must fail: its error.
#[track_caller]
fn export_fails(repo: &LocalRepo, sink: &mut dyn GitSink, req: &ExportRequest) -> ExportError {
    match export(repo, sink, req) {
        Ok(report) => panic!("the export succeeded: {report:?}"),
        Err(error) => error,
    }
}

/// A change landed on `main`: what the agent committed, and the changeset the land made.
struct Landing {
    /// The changeset the agent committed on `main`'s head, which the land squashed.
    change: ChangesetId,
    /// The land's key.
    key: &'static str,
    /// The changeset the land made, `L`.
    landed: ChangesetId,
}

/// Commits `changes` on `main`'s head with `message`, signed by the agent and naming it in its
/// `agent` metadata, without moving `main` (`Repo::commit`), then lands that changeset on `main`
/// with the key `key`, by the lander (`Repo::land`): a land of one changeset onto the head it was
/// made on, which moves `main` to the changeset it makes.
#[track_caller]
fn land(repo: &LocalRepo, key: &'static str, changes: Vec<Change>, message: &str) -> Landing {
    let head = main_head(repo);
    let commit_req = CommitRequest {
        author: agent(),
        committer: agent(),
        metadata: vec![("agent".to_owned(), AGENT.as_bytes().to_vec())],
        ..request(&[head], changes, message)
    };
    let change = repo.commit(commit_req).expect("commit").id;
    let land_req = LandRequest {
        key: Some(key.to_owned()),
        ..LandRequest::new(change, "main", lander())
    };
    let outcome = repo.land(land_req).expect("land");
    assert_eq!(outcome.status, LandStatus::Landed);
    assert_eq!(
        (outcome.rebased_onto, &outcome.squashed_from[..]),
        (head, &[change][..])
    );
    assert_eq!(main_head(repo), outcome.landed);
    Landing {
        change,
        key,
        landed: outcome.landed,
    }
}

/// Two changes landed one after the other on `main`, each with a key: the first adds a file, the
/// second changes it and adds an executable.
fn two_lands(repo: &LocalRepo) -> [Landing; 2] {
    let first = land(
        repo,
        "land-1",
        vec![put("landed/one.txt", b"one\n")],
        FIRST_MESSAGE,
    );
    let changes = vec![
        put("landed/one.txt", b"one, again\n"),
        put_as("landed/run.sh", LeafKind::Exec, b"#!/bin/sh\necho landed\n"),
    ];
    let second = land(repo, "land-2", changes, SECOND_MESSAGE);
    [first, second]
}

/// The trailer block of the commit `land`'s changeset exports to: that changeset, the land's
/// key, and the change the land squashed.
fn trailers(land: &Landing) -> String {
    format!(
        "SwarmOps-Changeset: {}\nSwarmOps-Land-Key: {}\nSwarmOps-Squashed-From: {}\n",
        land.landed, land.key, land.change
    )
}

/// The commit `commit` of the repository at `dir`, parsed.
fn commit_at(dir: &Path, commit: GitOid) -> RawCommit {
    let (kind, bytes) = source(dir).read(commit).unwrap();
    assert_eq!(kind, RawKind::Commit, "{commit}");
    RawCommit::parse(&bytes).unwrap()
}

/// Every object the commit `tip` reaches in the repository at `dir`, read through the import's
/// reader, which checks each against its id: by id, its kind and its bytes.
fn reachable(dir: &Path, tip: GitOid) -> BTreeMap<GitOid, (RawKind, Vec<u8>)> {
    let source = source(dir);
    let mut objects = BTreeMap::new();
    let mut stack = vec![tip];
    while let Some(oid) = stack.pop() {
        if objects.contains_key(&oid) {
            continue;
        }
        let (kind, bytes) =
            (source.read(oid)).unwrap_or_else(|error| panic!("read {oid}: {error}"));
        match kind {
            RawKind::Commit => {
                let commit = RawCommit::parse(&bytes).expect("a commit");
                stack.extend(commit.parents.iter().copied());
                stack.push(commit.tree);
            }
            RawKind::Tree => {
                let tree = RawTree::parse(&bytes).expect("a tree");
                stack.extend(tree.entries.iter().map(|entry| entry.oid));
            }
            RawKind::Blob | RawKind::Tag => {}
        }
        objects.insert(oid, (kind, bytes));
    }
    objects
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

/// Every file of git's tree `tree` in the repository at `dir`, by path: its mode and its bytes.
fn git_files(dir: &Path, tree: GitOid) -> BTreeMap<String, (Vec<u8>, Vec<u8>)> {
    let source = source(dir);
    let mut files = BTreeMap::new();
    let mut trees = vec![(tree, String::new())];
    while let Some((tree, at)) = trees.pop() {
        let (kind, bytes) = source.read(tree).unwrap();
        assert_eq!(kind, RawKind::Tree, "{at}");
        for entry in RawTree::parse(&bytes).unwrap().entries {
            let name = String::from_utf8(entry.name).expect("a UTF-8 name");
            let path = if at.is_empty() {
                name
            } else {
                format!("{at}/{name}")
            };
            if entry.mode == b"40000" {
                trees.push((entry.oid, path));
            } else {
                let (kind, content) = source.read(entry.oid).unwrap();
                assert_eq!(kind, RawKind::Blob, "{path}");
                files.insert(path, (entry.mode, content));
            }
        }
    }
    files
}

/// Every file of changeset `cs`'s root, by path, as the store reads it (`RepoRead::list_tree` and
/// `RepoRead::read_blob_at`): git's mode for its kind, and its bytes.
fn store_files(repo: &LocalRepo, cs: ChangesetId) -> BTreeMap<String, (Vec<u8>, Vec<u8>)> {
    let read = repo.snapshot().unwrap();
    let mut files = BTreeMap::new();
    let mut dirs: Vec<Option<RepoPath>> = vec![None];
    while let Some(dir) = dirs.pop() {
        let entries = (read.list_tree(cs, dir.as_ref()).unwrap()).expect("a directory");
        for entry in entries {
            let path = match &dir {
                Some(dir) => RepoPath::new(format!("{dir}/{}", entry.name)).unwrap(),
                None => RepoPath::new(entry.name.as_str()).unwrap(),
            };
            let kind = match entry.target {
                EntryTarget::Dir(_) => {
                    dirs.push(Some(path));
                    continue;
                }
                EntryTarget::Leaf(kind, _) => kind,
            };
            let mode: &[u8] = match kind {
                LeafKind::File => b"100644",
                LeafKind::Exec => b"100755",
                LeafKind::Symlink => b"120000",
            };
            let (_, _, content) = (read.read_blob_at(cs, &path).unwrap()).expect("a file");
            files.insert(path.to_string(), (mode.to_vec(), content));
        }
    }
    files
}

/// The changesets `head` reaches in `repo`, `head` included, over every parent, each with the
/// git commit it was imported from (`git.oid`); each must have been imported without a loss.
fn imported_commits(repo: &LocalRepo, head: ChangesetId) -> BTreeMap<ChangesetId, GitOid> {
    let mut commits = BTreeMap::new();
    let mut stack = vec![head];
    while let Some(id) = stack.pop() {
        if commits.contains_key(&id) {
            continue;
        }
        let cs = repo
            .changeset(id)
            .unwrap()
            .expect("a stored changeset")
            .changeset;
        let value = |key: &str| {
            (cs.metadata.iter())
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.clone())
        };
        assert_eq!(value(GIT_LOSSY), None, "{id} was imported with a loss");
        let git_oid = value(GIT_OID).unwrap_or_else(|| panic!("{id} holds no git commit"));
        commits.insert(id, oid(std::str::from_utf8(&git_oid).unwrap()));
        stack.extend(cs.parents.iter().copied());
    }
    commits
}

/// Every faithful fixture, imported into `main`, two changes landed on it with keys, and exported
/// into its own repository on its branch: every commit the fixture had is still there under its
/// oid with its bytes, and the branch moves from the fixture's tip to two new commits of one
/// parent each, the first's the old tip. Each is the commit a land made: its message the change's
/// followed by SwarmOps' trailers (the changeset, the land's key, the change it squashed), its
/// author the agent that committed the change and its committer the lander, its tree the files
/// the store holds. Exported again into an empty repository, the history is the same objects
/// under the same branch oid.
#[test]
fn import_land_export_round_trips_every_faithful_fixture() {
    for name in FAITHFUL {
        let fixture = git::fixture(name);
        let (dir, manifest) = (repo_dir(&fixture), manifest(&fixture));
        let tip = oid(&manifest.refs["refs/heads/main"]);
        let store = store();
        let repo = &store.repo;
        let from_git = imported(repo, dir, "HEAD");
        assert_eq!(from_git.git_head, tip, "{name}");
        let lands = two_lands(repo);

        let report = exported(repo, &mut gix(dir), &main_branch());
        let head_oid = export_oid(repo, lands[1].landed);
        assert_eq!(
            (report.head, report.head_oid),
            (lands[1].landed, head_oid),
            "{name}"
        );
        assert_eq!(
            (report.old, report.base, report.branch_moved),
            (Some(tip), Some(from_git.head), true),
            "{name}"
        );
        assert_eq!(
            (
                report.commits_written,
                report.rewritten,
                &report.warnings[..]
            ),
            (2, 0, &[][..]),
            "{name}"
        );
        assert_eq!(branch(dir), Some(head_oid), "{name}");

        // Every commit the fixture had is still there, as git wrote it.
        let reader = source(dir);
        for commit in &manifest.commits {
            let read = (reader.read(oid(&commit.oid)))
                .unwrap_or_else(|error| panic!("{name}: {}: {error}", commit.oid));
            let raw = (RawKind::Commit, commit.raw.clone());
            assert_eq!(read, raw, "{name}: {}", commit.oid);
        }

        // Two new commits, one parent each, the first's the fixture's tip.
        let first_oid = export_oid(repo, lands[0].landed);
        let second = commit_at(dir, head_oid);
        let first = commit_at(dir, first_oid);
        assert_eq!(
            (&second.parents[..], &first.parents[..]),
            (&[first_oid][..], &[tip][..]),
            "{name}"
        );
        let messages = [
            format!("{FIRST_MESSAGE}\n{}", trailers(&lands[0])),
            format!("{SECOND_MESSAGE}{}", trailers(&lands[1])),
        ];
        for ((commit, landed), message) in [first, second].iter().zip(&lands).zip(&messages) {
            let at = landed.landed;
            assert_eq!(
                String::from_utf8_lossy(&commit.message),
                message.as_str(),
                "{name}: {at}"
            );
            assert_eq!(
                (&commit.author, &commit.committer),
                (&ident(&agent()), &ident(&lander())),
                "{name}: {at}"
            );
            let no_headers: [(Vec<u8>, Vec<u8>); 0] = [];
            assert_eq!(commit.extra_headers, no_headers, "{name}: {at}");
            assert_eq!(
                git_files(dir, commit.tree),
                store_files(repo, at),
                "{name}: {at}"
            );
        }

        // Into an empty repository: the same objects, to the same branch oid.
        let empty = git::empty_repository();
        let whole = exported(repo, &mut gix(&empty.dir), &main_branch());
        assert_eq!(
            (whole.head_oid, whole.old, whole.base, whole.branch_moved),
            (head_oid, None, None, true),
            "{name}"
        );
        assert_eq!(branch(&empty.dir), Some(head_oid), "{name}");
        let objects = reachable(&empty.dir, head_oid);
        let commits = (objects.values()).filter(|(kind, _)| *kind == RawKind::Commit);
        assert_eq!(whole.commits_written, to_u64(commits.count()), "{name}");
        assert_eq!(objects, reachable(dir, head_oid), "{name}");
    }
}

/// A land undone after its export: the branch names the export of a changeset `main` no longer
/// reaches, so the next export stops as `source_moved_back` and writes nothing. Forced, it moves
/// the branch back to the first land's export by compare-and-swap, writing nothing either: that
/// commit is there already, the parent of the one the branch named.
#[test]
fn an_undone_land_after_an_export_is_source_moved_back() {
    let fixture = git::fixture("linear");
    let dir = repo_dir(&fixture);
    let store = store();
    let repo = &store.repo;
    imported(repo, dir, "HEAD");
    let [first, second] = two_lands(repo);
    let report = exported(repo, &mut gix(dir), &main_branch());
    assert_eq!((report.head, report.commits_written), (second.landed, 2));

    // The last land's operation is the newest: the export wrote no journal frame.
    let (ops, _) = repo.op_log(None, 1).unwrap();
    let op = &ops[0];
    let landed = OpRecord::Changeset {
        id: second.landed,
        parents: vec![first.landed],
    };
    let main_moved = (op.records.iter()).any(|record| {
        matches!(record, OpRecord::RefMove { name, old, new, .. }
            if name == "main" && *old == Some(first.landed) && *new == Some(second.landed))
    });
    assert!(op.records.contains(&landed) && main_moved, "{op:?}");
    let undone = repo.undo(op.seq, ACTOR).unwrap();
    let moves: Vec<_> = (undone.moves.iter())
        .map(|(name, from, to, _)| (name.as_str(), *from, *to))
        .collect();
    assert_eq!(moves, [("main", Some(second.landed), Some(first.landed))]);
    assert_eq!(main_head(repo), first.landed);

    let before = object_files(dir);
    let error = export_fails(repo, &mut gix(dir), &main_branch());
    let ExportError::Diverged {
        branch: name,
        git_head,
        head,
        reason,
    } = error
    else {
        panic!("expected a divergence, got {error}");
    };
    let reason_expected = DivergedReason::SourceMovedBack {
        exported: second.landed,
    };
    assert_eq!(
        (name.as_str(), git_head, head, reason),
        ("main", report.head_oid, first.landed, reason_expected)
    );
    assert_eq!(reason.as_str(), "source_moved_back");
    assert_eq!(object_files(dir), before, "nothing written");
    assert_eq!(branch(dir), Some(report.head_oid));

    let forced = ExportRequest {
        force: true,
        ..main_branch()
    };
    let back = exported(repo, &mut gix(dir), &forced);
    let first_oid = export_oid(repo, first.landed);
    assert_eq!((back.head, back.head_oid), (first.landed, first_oid));
    assert_eq!(
        (back.old, back.base, back.branch_moved),
        (Some(report.head_oid), Some(second.landed), true)
    );
    assert_eq!((back.commits_written, back.objects_written), (0, 0));
    assert_eq!(object_files(dir), before, "nothing written");
    assert_eq!(branch(dir), Some(first_oid));
    assert_eq!(commit_at(dir, report.head_oid).parents, [first_oid]);
}

/// Host A and host B in umbrella §13's single-writer layout, and the git remote R between them:
/// A imported `linear`, landed a change and exported to R; B imported R; A landed one more change
/// and exported again; B imported R again.
struct Hosts {
    a: Store,
    b: Store,
    /// R: a bare repository, which A exports to and B imports from.
    remote: git::EmptyRepository,
    /// A's lands, in order.
    lands: [Landing; 2],
    /// A's second export: R's `main` names its head's export.
    exported: ExportReport,
    /// B's second import: B's `main` names its head, the changeset of what R's `main` names.
    imported: ImportReport,
}

/// The single-writer layout's round trip, each step checked: A's first export creates R's `main`,
/// writing the whole history, and B's first import takes all of it; A's second export goes on from
/// the first land and writes one commit, and B's second import takes that commit alone, onto the
/// head its first import left, whose commit is that one's parent.
fn single_writer() -> Hosts {
    let linear = git::fixture("linear");
    let (a, b, remote) = (store(), store(), git::empty_repository());
    let from_git = imported(&a.repo, repo_dir(&linear), "HEAD");
    let first = land(
        &a.repo,
        "host-a-1",
        vec![put("hosts/a.txt", b"one\n")],
        "Host A's first change\n",
    );
    let created = exported(&a.repo, &mut gix(&remote.dir), &main_branch());
    assert_eq!(
        (
            created.head,
            created.old,
            created.base,
            created.branch_moved
        ),
        (first.landed, None, None, true)
    );
    assert_eq!(created.commits_written, from_git.imported + 1);

    let mirrored = imported(&b.repo, &remote.dir, "main");
    assert_eq!(
        (mirrored.imported, mirrored.skipped, mirrored.old_head),
        (from_git.imported + 1, 0, None)
    );
    assert_eq!(mirrored.git_head, created.head_oid);

    let second = land(
        &a.repo,
        "host-a-2",
        vec![put("hosts/a.txt", b"two\n")],
        "Host A's second change\n",
    );
    let exported = exported(&a.repo, &mut gix(&remote.dir), &main_branch());
    // Incremental: from the first land's export, which R's `main` named.
    assert_eq!(
        (exported.head, exported.old, exported.base),
        (second.landed, Some(created.head_oid), Some(first.landed))
    );
    assert_eq!((exported.branch_moved, exported.commits_written), (true, 1));
    assert_eq!(branch(&remote.dir), Some(exported.head_oid));

    let imported = imported(&b.repo, &remote.dir, "main");
    assert_eq!(
        (imported.imported, imported.skipped, imported.ref_moved),
        (1, 1, true)
    );
    assert_eq!(
        (imported.old_head, imported.git_head),
        (Some(mirrored.head), exported.head_oid)
    );
    Hosts {
        a,
        b,
        remote,
        lands: [first, second],
        exported,
        imported,
    }
}

/// Umbrella §13's single-writer layout through a git remote: host A lands on `main` and exports
/// to R, host B imports R, and each next export and import takes only what is new. B's `main` then
/// holds R's history exactly: one changeset for each commit R's `main` reaches, each imported
/// from its commit without a loss, and its head exports to the commit R's `main` names.
#[test]
fn the_single_writer_layout_round_trips_through_a_git_remote() {
    let hosts = single_writer();
    let b = &hosts.b.repo;
    let r_main = branch(&hosts.remote.dir).expect("R's main");
    assert_eq!(r_main, hosts.exported.head_oid);
    let in_r: BTreeSet<GitOid> = (reachable(&hosts.remote.dir, r_main).into_iter())
        .filter(|(_, (kind, _))| *kind == RawKind::Commit)
        .map(|(oid, _)| oid)
        .collect();
    let b_head = main_head(b);
    assert_eq!(b_head, hosts.imported.head);
    let in_b = imported_commits(b, b_head);
    assert_eq!(in_b.len(), in_r.len());
    assert_eq!(in_b.values().copied().collect::<BTreeSet<_>>(), in_r);
    assert_eq!(in_b[&b_head], r_main);
    let exported = b.derived::<ExportOid>(b_head, &()).unwrap();
    assert_eq!((exported.git_oid(), exported.kept), (r_main, true));
    // A's history is the same commits: its head exports to R's `main`.
    let a = &hosts.a.repo;
    assert_eq!(main_head(a), hosts.lands[1].landed);
    assert_eq!(export_oid(a, main_head(a)), r_main);
}

/// Two hosts writing to `main` in the single-writer layout stop rather than merge. B lands
/// natively: its import of R is diverged, its head a native changeset R does not build on, and it
/// writes no frame; B's export to R finds R's `main` at the export of its head's parent, so it is
/// incremental and R moves. Then A lands and exports: R's history holds the export of A's older
/// land, so both moved, and nothing is written; and A's import of R is diverged too, A's head
/// being its own land, never exported. Nothing is merged: each `main` stays where its own host
/// put it.
#[test]
fn two_writers_stop() {
    let hosts = single_writer();
    let (a, b, remote) = (&hosts.a.repo, &hosts.b.repo, hosts.remote.dir.as_path());
    let from_a = hosts.exported.head_oid;

    let on_b = land(
        b,
        "host-b-1",
        vec![put("hosts/b.txt", b"b\n")],
        "Host B's change\n",
    );
    let op = last_op(b);
    let error = import_fails(b, remote);
    let b_base = export_oid(b, on_b.landed);
    assert!(
        matches!(
            error,
            ImportError::Diverged { target_head, base: Some(base), git_head, .. }
                if target_head == on_b.landed && base == b_base && git_head == from_a
        ),
        "{error:?}"
    );
    assert_eq!((main_head(b), last_op(b)), (on_b.landed, op));

    let from_b = exported(b, &mut gix(remote), &main_branch());
    assert_eq!((from_b.head, from_b.head_oid), (on_b.landed, b_base));
    assert_eq!(
        (from_b.old, from_b.base, from_b.branch_moved),
        (Some(from_a), Some(hosts.imported.head), true)
    );
    assert_eq!(from_b.commits_written, 1);
    assert_eq!(branch(remote), Some(b_base));
    assert_eq!(commit_at(remote, b_base).parents, [from_a]);

    let on_a = land(
        a,
        "host-a-3",
        vec![put("hosts/a.txt", b"three\n")],
        "Host A's third change\n",
    );
    let before = object_files(remote);
    let error = export_fails(a, &mut gix(remote), &main_branch());
    let ExportError::Diverged {
        branch: name,
        git_head,
        head,
        reason,
    } = error
    else {
        panic!("expected a divergence, got {error}");
    };
    let reason_expected = DivergedReason::BothMoved {
        exported: hosts.lands[1].landed,
    };
    assert_eq!(
        (name.as_str(), git_head, head, reason),
        ("main", b_base, on_a.landed, reason_expected)
    );
    assert_eq!(reason.as_str(), "both_moved");
    assert_eq!(object_files(remote), before, "nothing written");
    assert_eq!(branch(remote), Some(b_base));

    let op = last_op(a);
    let error = import_fails(a, remote);
    let a_base = export_oid(a, on_a.landed);
    assert!(
        matches!(
            error,
            ImportError::Diverged { target_head, base: Some(base), git_head, .. }
                if target_head == on_a.landed && base == a_base && git_head == b_base
        ),
        "{error:?}"
    );
    assert_eq!(
        gix(remote).stored(a_base),
        Ok(Stored::Absent),
        "never exported"
    );
    assert_eq!(last_op(a), op);

    // Nothing merged: each host's `main` where its own land put it, R's where B's export did.
    assert_eq!(main_head(a), on_a.landed);
    assert_eq!(main_head(b), on_b.landed);
    assert_eq!(branch(remote), Some(b_base));
}
