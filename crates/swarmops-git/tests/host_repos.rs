//! Real repositories on this host, round-tripped: each bare mirror clone in the directory
//! `SWARMOPS_HOST_REPOS_DIR` names (`<name>.git`) is imported from its `HEAD` into a store of its
//! own, the store passes a deep verify, `main` is exported into an empty repository, which
//! `git fsck --strict --no-dangling` accepts, and that export, imported into a second store,
//! gives the first store's head. Every commit keeps its bytes: none is recorded with a loss, and
//! the export of the head is the mirror's own commit.
//!
//! Only with `SWARMOPS_HOST_REPOS=1` (the repository rules: tests that read real repositories on
//! this host run only then); without it the test says so and passes. With it, a missing or empty
//! directory fails the test. The weekly run's `host repositories` job refreshes the mirrors
//! (`scripts/host-repos.py`) and runs it. No network: the mirrors are local directories, and the
//! export writes into a temporary one.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::time::Instant;

use camino::Utf8PathBuf;
use swarmops_git::{ExportRequest, GixSink, GixSource, ImportRequest, export, import, register};
use swarmops_repo::{Depth, LocalRepo};
use swarmops_testkit::fs::NoSyncFs;
use swarmops_testkit::git;
use tempfile::TempDir;

/// Set to `1`, the test reads the host's repositories.
const ENABLE_ENV: &str = "SWARMOPS_HOST_REPOS";

/// The directory of bare mirror clones, `<name>.git` each.
const DIR_ENV: &str = "SWARMOPS_HOST_REPOS_DIR";

/// The suffix of a mirror's directory name.
const MIRROR_SUFFIX: &str = ".git";

/// Who moves the store refs the imports move.
const ACTOR: &str = "test/host-repos";

/// The store ref, and the export's branch.
const MAIN: &str = "main";

/// The mirrors `SWARMOPS_HOST_REPOS_DIR` holds, by name, sorted; `None` unless
/// `SWARMOPS_HOST_REPOS=1`.
fn mirrors() -> Option<Vec<(String, PathBuf)>> {
    if std::env::var_os(ENABLE_ENV).is_none_or(|value| value != "1") {
        return None;
    }
    let dir = std::env::var_os(DIR_ENV)
        .unwrap_or_else(|| panic!("{ENABLE_ENV}=1, but {DIR_ENV} names no directory"));
    let dir = PathBuf::from(dir);

    // 1. Every `<name>.git` directory, sorted, so that the runs go in one order.
    let entries =
        std::fs::read_dir(&dir).unwrap_or_else(|error| panic!("read {}: {error}", dir.display()));
    let mut found = Vec::new();
    for entry in entries {
        let entry = entry.expect("a directory entry");
        let file_name = entry.file_name();
        let Some(name) = file_name
            .to_str()
            .and_then(|n| n.strip_suffix(MIRROR_SUFFIX))
        else {
            continue;
        };
        if !entry.file_type().expect("a file type").is_dir() {
            continue;
        }
        found.push((name.to_owned(), entry.path()));
    }
    found.sort();

    // 2. None is a failure: the run was asked for and would check nothing.
    assert!(
        !found.is_empty(),
        "{} holds no <name>{MIRROR_SUFFIX} mirror",
        dir.display()
    );
    Some(found)
}

/// A store of its own, with `ExportOid` registered, on a filesystem that never flushes
/// ([`NoSyncFs`]): the subject is what crosses to git and back, not what a power loss leaves.
struct Store {
    _dir: TempDir,
    repo: LocalRepo,
}

fn store() -> Store {
    let dir = tempfile::tempdir().expect("a temp dir");
    let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).expect("a UTF-8 temp dir");
    let mut repo = LocalRepo::create_with_fs(&root, "host", Arc::new(NoSyncFs)).expect("create");
    register(&mut repo).expect("register ExportOid");
    Store { _dir: dir, repo }
}

/// `git fsck --strict --no-dangling` on `git_dir`, by the git `SWARMOPS_GIT` names (else `git`
/// on `PATH`), without system or global configuration and with an empty `HOME`.
fn fsck(git_dir: &Path) -> Output {
    let program = std::env::var_os(git::GIT_ENV).unwrap_or_else(|| "git".into());
    let home = tempfile::tempdir().expect("a temp dir");
    let mut command = Command::new(program);
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home.path())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("LC_ALL", "C")
        .arg("--git-dir")
        .arg(git_dir)
        .args(["fsck", "--strict", "--no-dangling"]);
    swarmops_testkit::child::run(&mut command)
}

/// How a run ended and what it printed, for a failure's message.
fn said(output: &Output) -> String {
    format!(
        "{}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout).trim_end(),
        String::from_utf8_lossy(&output.stderr).trim_end(),
    )
}

/// The seconds since `lap`, which then starts again.
fn split(lap: &mut Instant) -> f64 {
    let secs = lap.elapsed().as_secs_f64();
    *lap = Instant::now();
    secs
}

/// One mirror's round trip; its numbers, and each step's seconds, on one line.
fn round_trip(name: &str, mirror: &Path) {
    let mut lap = Instant::now();

    // 1. Import HEAD: every commit as git wrote it.
    let first = store();
    let source = GixSource::open(mirror).unwrap_or_else(|e| panic!("{name}: open: {e}"));
    let req = ImportRequest::new("HEAD", MAIN, ACTOR);
    let imported = import(&first.repo, &source, &req).unwrap_or_else(|e| panic!("{name}: {e}"));
    assert_eq!(
        imported.lossy_total, 0,
        "{name}: lossy: {:?}",
        imported.lossy
    );
    assert!(
        imported.warnings.is_empty(),
        "{name}: {:?}",
        imported.warnings
    );
    let import_secs = split(&mut lap);

    // 2. A deep verify finds nothing.
    let verified = first.repo.verify(Depth::Deep).expect("verify");
    assert!(
        verified.problems.is_empty(),
        "{name}: {:?}",
        verified.problems
    );
    let verify_secs = split(&mut lap);

    // 3. Export into an empty repository: the head exports to the mirror's own commit.
    let empty = git::empty_repository();
    let mut sink = GixSink::open(&empty.dir).unwrap_or_else(|e| panic!("{name}: sink: {e}"));
    let exported = export(&first.repo, &mut sink, &ExportRequest::new(MAIN, MAIN))
        .unwrap_or_else(|e| panic!("{name}: export: {e}"));
    assert_eq!(
        exported.head_oid, imported.git_head,
        "{name}: head rewritten"
    );
    assert_eq!(exported.commits_written, imported.imported, "{name}");
    let export_secs = split(&mut lap);

    // 4. git itself accepts the export.
    let checked = fsck(&empty.dir);
    assert!(
        checked.status.success(),
        "{name}: git fsck: {}",
        said(&checked)
    );
    let fsck_secs = split(&mut lap);

    // 5. The export, imported again, is the same history.
    let second = store();
    let source = GixSource::open(&empty.dir).unwrap_or_else(|e| panic!("{name}: open: {e}"));
    let again = import(
        &second.repo,
        &source,
        &ImportRequest::new(MAIN, MAIN, ACTOR),
    )
    .unwrap_or_else(|e| panic!("{name}: re-import: {e}"));
    assert_eq!(again.head, imported.head, "{name}: re-imported head");
    assert_eq!(again.git_head, imported.git_head, "{name}: re-imported tip");
    assert_eq!(
        again.imported, imported.imported,
        "{name}: re-imported commits"
    );
    let again_secs = split(&mut lap);

    println!(
        "{name}: {} commits, {} objects verified deep, 0 problems, {} objects exported, \
         git fsck --strict clean, re-import head {}; seconds: import {import_secs:.1}, \
         verify {verify_secs:.1}, export {export_secs:.1}, fsck {fsck_secs:.1}, \
         re-import {again_secs:.1}",
        imported.imported, verified.objects_checked, exported.objects_written, again.head,
    );
}

/// Every mirror round-trips (the module's description); skipped unless `SWARMOPS_HOST_REPOS=1`.
#[test]
fn every_mirror_round_trips() {
    let Some(mirrors) = mirrors() else {
        println!("every_mirror_round_trips: skipped: set {ENABLE_ENV}=1 and {DIR_ENV}");
        return;
    };
    for (name, mirror) in &mirrors {
        round_trip(name, mirror);
    }
}
