//! The export where the platforms differ: branch names a case-insensitive or Windows target
//! cannot hold, the warnings for trees such a checkout cannot hold, and how the sink syncs its
//! files and directories (storage-core design, §7, the paragraph on export and platforms; §2.1
//! P2).

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use swarmops_git::{ExportError, ExportReport, ExportRequest, GitSink, GixSink, SinkError, export};
use swarmops_repo::{CasResult, LocalRepo};
use swarmops_testkit::git::{self, EmptyRepository};
use swarmops_testkit::repo::{commit, new_repo, put};
use tempfile::TempDir;

/// Who moves the store's refs in these tests.
const ACTOR: &str = "test/exporter";

/// A fresh store whose `main` names one changeset holding `files`, each with its own bytes.
fn store_with(files: &[&str]) -> (TempDir, LocalRepo) {
    let (dir, _root, repo) = new_repo();

    // 1. One changeset, a root, holding every file.
    let changes = (files.iter())
        .map(|path| put(path, path.as_bytes()))
        .collect();
    let id = commit(&repo, &[], changes);

    // 2. `main` names it.
    let moved = repo
        .cas_ref("main", None, Some(id), ACTOR)
        .expect("cas_ref");
    assert!(
        matches!(moved.result, CasResult::Applied { .. }),
        "{moved:?}"
    );
    (dir, repo)
}

/// An empty bare repository whose `core.ignorecase` is true, as git makes one on a
/// case-insensitive volume.
fn ignoring_case() -> EmptyRepository {
    let destination = git::empty_repository();
    let config = destination.dir.join("config");
    let mut text = fs::read_to_string(&config).unwrap();
    text.push_str("\tignorecase = true\n");
    fs::write(&config, text).unwrap();
    destination
}

fn gix(path: &Path) -> GixSink {
    GixSink::open(path).unwrap_or_else(|error| panic!("open {}: {error}", path.display()))
}

/// Exports the store's `main` into the branch `branch` of the repository at `dir`.
fn export_to(repo: &LocalRepo, dir: &Path, branch: &str) -> Result<ExportReport, ExportError> {
    export(repo, &mut gix(dir), &ExportRequest::new("main", branch))
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
                continue;
            }
            files.insert(path.strip_prefix(git_dir).unwrap().to_owned());
        }
    }
    files
}

/// On a repository that ignores case, `main` is the file `Main` is: the export refuses it, and
/// moves nothing.
#[test]
fn a_branch_that_differs_only_in_case_is_refused_when_the_repository_ignores_case() {
    let (_dir, repo) = store_with(&["a.txt"]);
    let destination = ignoring_case();

    // 1. `Main` is made.
    let first = export_to(&repo, &destination.dir, "Main").expect("export to Main");
    let main = PathBuf::from("refs/heads/Main");
    assert_eq!(ref_files(&destination.dir), BTreeSet::from([main.clone()]));

    // 2. `main` is refused, and `Main` stays where it was.
    let refused = export_to(&repo, &destination.dir, "main");
    assert!(
        matches!(
            &refused,
            Err(ExportError::Sink(SinkError::BadBranch { name })) if name == "main"
        ),
        "{refused:?}"
    );
    assert_eq!(ref_files(&destination.dir), BTreeSet::from([main]));
    let sink = gix(&destination.dir);
    assert_eq!(sink.branch("Main"), Ok(Some(first.head_oid)));
}

/// On a repository that ignores case, the branch `Feat` is the directory `feat/x` would go in:
/// the export refuses it as a ref in the way.
#[test]
fn a_branch_in_the_way_in_another_case_is_a_ref_conflict_when_the_repository_ignores_case() {
    let (_dir, repo) = store_with(&["a.txt"]);
    let destination = ignoring_case();
    export_to(&repo, &destination.dir, "Feat").expect("export to Feat");

    let refused = export_to(&repo, &destination.dir, "feat/x");
    assert!(
        matches!(
            &refused,
            Err(ExportError::Sink(SinkError::RefConflict { name })) if name == "refs/heads/Feat"
        ),
        "{refused:?}"
    );
    let feat = PathBuf::from("refs/heads/Feat");
    assert_eq!(ref_files(&destination.dir), BTreeSet::from([feat]));
}

/// On a repository that keeps case, `Main` and `main` are two branches.
#[test]
fn a_case_sensitive_repository_keeps_both_branches() {
    let (_dir, repo) = store_with(&["a.txt"]);
    let destination = git::empty_repository();
    let upper = export_to(&repo, &destination.dir, "Main").expect("export to Main");
    let lower = export_to(&repo, &destination.dir, "main").expect("export to main");
    assert!(lower.branch_moved);

    let both = ["refs/heads/Main", "refs/heads/main"].map(PathBuf::from);
    assert_eq!(ref_files(&destination.dir), BTreeSet::from(both));
    let sink = gix(&destination.dir);
    assert_eq!(sink.branch("Main"), Ok(Some(upper.head_oid)));
    assert_eq!(sink.branch("main"), Ok(Some(lower.head_oid)));
}

/// gix writes a loose object mode 0444. The sink syncs it through a read-only handle: a handle
/// open for writing would be refused (`PermissionDenied`) for any user but root.
#[cfg(all(unix, feature = "test-hooks"))]
#[test]
fn a_loose_object_gix_wrote_read_only_is_synced() {
    use std::os::unix::fs::PermissionsExt as _;

    let (_dir, repo) = store_with(&["a.txt"]);
    let destination = git::empty_repository();
    let (mut sink, synced) = GixSink::open_recording_syncs(&destination.dir).unwrap();
    let report = export(&repo, &mut sink, &ExportRequest::new("main", "main")).expect("export");
    assert_eq!(report.objects_written, 3, "a blob, a tree and a commit");

    // The commit's loose file, read-only, is among the paths synced.
    let hex = report.head_oid.to_string();
    let loose = PathBuf::from("objects").join(&hex[..2]).join(&hex[2..]);
    let mode = fs::metadata(destination.dir.join(&loose))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o444, "{}", loose.display());
    let synced = synced.lock().unwrap();
    assert!(synced.contains(&loose), "{synced:?}");
}
