//! Reading a repository without a checkout: files and directories at a revision, revisions
//! resolved from refs, ids and prefixes with `~N`, the first-parent log, and refs moved
//! through the repository. Also the store's lifecycle: `create` (which resumes after a crash
//! at any of its filesystem calls, `FORMAT` written last), `open` and `info`.
//!
//! Every test owns a fresh store (`tempfile::tempdir()`). The crash sweep runs this test
//! binary again as its children (selected by [`CHILD_ROOT`]).

mod support;

use std::fs;
#[cfg(feature = "test-hooks")]
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::sync::Arc;
#[cfg(feature = "test-hooks")]
use std::thread;

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_core::{ChangesetId, EntryKind, ObjectId, ObjectKind, hash_object};
use swarmops_repo::{CasOutcome, CasResult, LocalRepo, RefState, RepoError, RepoInfo};
#[cfg(feature = "test-hooks")]
use swarmops_store::CRASH_AT_ENV;
use swarmops_store::{ChangesetMeta, MetaStore, StoreError};

#[cfg(feature = "test-hooks")]
use crate::support::child::{SIGABRT, run, this_test};
use crate::support::fs::RecordingFs;
use crate::support::{blob, commit, new_repo, path, put, put_as, request};

/// Set only in the children of the crash sweep: the store root.
#[cfg(feature = "test-hooks")]
const CHILD_ROOT: &str = "SWARMOPS_TEST_REPO_READ_CHILD_ROOT";

fn unknown_id() -> ChangesetId {
    ChangesetId(hash_object(ObjectKind::Changeset, b"never committed"))
}

/// The names, kinds and ids of directory `dir`'s entries at changeset `id`.
fn listing(
    repo: &LocalRepo,
    id: ChangesetId,
    dir: Option<&str>,
) -> Option<Vec<(String, EntryKind, ObjectId)>> {
    let dir = dir.map(path);
    repo.list_tree(id, dir.as_ref()).unwrap().map(|entries| {
        entries
            .into_iter()
            .map(|e| (e.name, e.kind, e.id))
            .collect()
    })
}

#[track_caller]
fn assert_not_found<T: std::fmt::Debug>(got: Result<T, RepoError>, what: &str) {
    match got {
        Err(RepoError::NotFound(missing)) => assert_eq!(missing, what),
        other => panic!("expected NotFound({what:?}), got {other:?}"),
    }
}

#[test]
fn read_blob_at_and_list_tree_without_checkout() {
    let (_dir, root, repo) = new_repo();
    let script = b"#!/bin/sh\necho hi\n";
    let id = commit(
        &repo,
        &[],
        vec![
            put("README.md", b"# demo\n"),
            put_as("bin/run", EntryKind::Exec, script),
            put_as("bin/link", EntryKind::Symlink, b"run"),
            put("src/lib.rs", b"pub fn f() {}\n"),
        ],
    );

    // Each file with its kind, its blob's id and its bytes, read from the objects alone.
    let read = |p: &str| repo.read_blob_at(id, &path(p)).unwrap();
    let file = |kind: EntryKind, content: &[u8]| Some((kind, blob(content), content.to_vec()));
    assert_eq!(read("README.md"), file(EntryKind::File, b"# demo\n"));
    assert_eq!(read("bin/run"), file(EntryKind::Exec, script));
    assert_eq!(read("bin/link"), file(EntryKind::Symlink, b"run"));
    assert_eq!(
        read("src/lib.rs"),
        file(EntryKind::File, b"pub fn f() {}\n")
    );
    assert_eq!(read("bin"), None, "a directory is no file");

    // Directories list in canonical order, each entry with its object id.
    let top = listing(&repo, id, None).unwrap();
    let kinds: Vec<(&str, EntryKind)> = top.iter().map(|(n, k, _)| (n.as_str(), *k)).collect();
    assert_eq!(
        kinds,
        [
            ("README.md", EntryKind::File),
            ("bin", EntryKind::Dir),
            ("src", EntryKind::Dir)
        ]
    );
    assert_eq!(top[0].2, blob(b"# demo\n").0);
    assert_eq!(
        listing(&repo, id, Some("bin")),
        Some(vec![
            ("link".to_owned(), EntryKind::Symlink, blob(b"run").0),
            ("run".to_owned(), EntryKind::Exec, blob(script).0),
        ])
    );
    // The store holds no working copy: only its own files are there.
    let mut entries: Vec<String> = fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    entries.sort();
    assert_eq!(entries, ["FORMAT", "REPO", "meta", "objects"]);
}

#[test]
fn list_tree_of_missing_dir_is_none() {
    let (_dir, _root, repo) = new_repo();
    let id = commit(
        &repo,
        &[],
        vec![put("README.md", b"r"), put("src/lib.rs", b"l")],
    );
    for missing in [
        "nope",
        "src/nope",
        "README.md",
        "README.md/under",
        "src/lib.rs",
    ] {
        assert_eq!(listing(&repo, id, Some(missing)), None, "{missing}");
    }
    for missing in ["nope", "src", "src/nope", "README.md/under"] {
        assert_eq!(
            repo.read_blob_at(id, &path(missing)).unwrap(),
            None,
            "{missing}"
        );
    }
    // An unknown changeset is an error, not an empty tree.
    let unknown = unknown_id();
    assert_not_found(repo.list_tree(unknown, None), &unknown.to_string());
    assert_not_found(
        repo.read_blob_at(unknown, &path("README.md")),
        &unknown.to_string(),
    );
    assert_eq!(repo.changeset(unknown).unwrap(), None);
}

/// Inserts two changesets whose ids share their first 12 hex digits (`ab` × 6), with no
/// objects, straight into the metadata store: only an index lookup can tell them apart.
fn insert_prefix_twins(root: &Utf8Path) -> [ChangesetId; 2] {
    let twins = [1u8, 2].map(|last| {
        let mut id = [0xab; 32];
        id[31] = last;
        ChangesetId(ObjectId(id))
    });
    let meta = MetaStore::open(root).unwrap();
    let mut write = meta.write().unwrap();
    for (n, id) in (0u8..).zip(twins) {
        write
            .insert_changeset(ChangesetMeta {
                id,
                seq: 0,
                generation: 0,
                delta_hash: [n; 32],
                parents: Vec::new(),
            })
            .unwrap();
    }
    write.commit().unwrap();
    twins
}

#[test]
fn resolve_ref_full_prefix_and_tilde() {
    let (_dir, root, repo) = new_repo();
    let c1 = commit(&repo, &[], vec![put("f", b"1")]);
    let c2 = commit(&repo, &[c1], vec![put("f", b"2")]);
    let c3 = commit(&repo, &[c2], vec![put("f", b"3")]);
    repo.cas_ref("main", None, Some(c3), "test/agent").unwrap();
    repo.cas_ref("release/1.0", None, Some(c1), "test/agent")
        .unwrap();

    let resolve = |rev: &str| repo.resolve(rev).unwrap();
    assert_eq!(resolve("main"), c3);
    assert_eq!(resolve("release/1.0"), c1);
    assert_eq!(resolve("main~0"), c3);
    assert_eq!(resolve("main~1"), c2);
    assert_eq!(resolve("main~2"), c1);
    assert_eq!(resolve("main~1~1"), c1, "suffixes add up");
    let hex = c2.to_string();
    assert_eq!(resolve(&hex), c2);
    assert_eq!(resolve(&hex.to_uppercase()), c2);
    assert_eq!(resolve(&hex[..12]), c2, "a 12-digit prefix");
    assert_eq!(
        resolve(&hex[..20].to_uppercase()),
        c2,
        "an uppercase prefix"
    );
    assert_eq!(resolve(&format!("{}~1", &hex[..40])), c1);

    // Revisions that name nothing.
    let unknown = unknown_id().to_string();
    let absent_prefix = (0..16)
        .map(|n| format!("{n:x}").repeat(12))
        .find(|prefix| {
            ![c1, c2, c3]
                .iter()
                .any(|id| id.to_string().starts_with(prefix.as_str()))
        })
        .unwrap();
    for rev in ["main~3", "release/2.0", &unknown, &absent_prefix] {
        assert_not_found(repo.resolve(rev), rev);
    }

    // A prefix two changesets share is ambiguous; one digit more tells them apart.
    let twins = insert_prefix_twins(&root);
    match repo.resolve("abababababab") {
        Err(RepoError::Ambiguous(rev)) => assert_eq!(rev, "abababababab"),
        other => panic!("expected Ambiguous, got {other:?}"),
    }
    assert_eq!(resolve(&format!("{}01", "ab".repeat(31))), twins[0]);

    // Anything else is no revision at all.
    let too_short = &hex[..11];
    let too_long = format!("{hex}0");
    for rev in [
        "",
        "bogus",
        "HEAD",
        "main~",
        "main~x",
        "main~-1",
        "main~+1",
        "~1",
        "main^",
        "release/",
        "release/.hidden",
        "gggggggggggg",
        too_short,
        too_long.as_str(),
    ] {
        match repo.resolve(rev) {
            Err(RepoError::BadRev(bad)) => assert_eq!(bad, rev),
            other => panic!("expected BadRev({rev:?}), got {other:?}"),
        }
    }
}

#[test]
fn resolve_through_a_dangling_ref_is_not_found() {
    let (_dir, root, repo) = new_repo();
    commit(&repo, &[], vec![put("f", b"1")]);
    // main's file names a changeset the store does not hold (damage verify reports).
    let nowhere = unknown_id();
    fs::write(root.join("meta/refs/main"), format!("{nowhere}\n1\n")).unwrap();
    for rev in ["main", "main~1"] {
        match repo.resolve(rev) {
            Err(RepoError::NotFound(what)) => {
                assert!(what.starts_with(&format!("{rev} ")), "{what}");
                assert!(what.contains(&nowhere.to_string()), "{what}");
            }
            other => panic!("{rev}: expected NotFound, got {other:?}"),
        }
    }
}

#[test]
fn log_first_parent_order_and_limit() {
    let (_dir, _root, repo) = new_repo();
    let c1 = commit(&repo, &[], vec![put("a", b"a")]);
    let c2 = commit(&repo, &[c1], vec![put("b", b"b")]);
    let side = commit(&repo, &[c1], vec![put("side", b"side")]);
    let merge = repo
        .commit(request(&[c2, side], vec![put("m", b"m")], "merge"))
        .unwrap()
        .id;

    let log = repo.log_first_parent(merge, 10).unwrap();
    let ids: Vec<ChangesetId> = log.iter().map(|cs| cs.id).collect();
    assert_eq!(ids, [merge, c2, c1], "first parents only");
    let numbers: Vec<(u64, u64)> = log.iter().map(|cs| (cs.seq, cs.generation)).collect();
    assert_eq!(numbers, [(3, 3), (1, 2), (0, 1)]);
    assert_eq!(log[0].changeset.parents, [c2, side]);
    assert_eq!(log[0].changeset.message, b"merge");

    let first_two: Vec<ChangesetId> = repo
        .log_first_parent(merge, 2)
        .unwrap()
        .iter()
        .map(|cs| cs.id)
        .collect();
    assert_eq!(first_two, [merge, c2]);
    assert_eq!(repo.log_first_parent(merge, 0).unwrap(), []);
    let side_log: Vec<ChangesetId> = repo
        .log_first_parent(side, 10)
        .unwrap()
        .iter()
        .map(|cs| cs.id)
        .collect();
    assert_eq!(side_log, [side, c1]);
    let unknown = unknown_id();
    assert_not_found(repo.log_first_parent(unknown, 10), &unknown.to_string());

    // The merge's delta is against its first parent: its tree is c2's plus m.
    let names: Vec<String> = listing(&repo, merge, None)
        .unwrap()
        .into_iter()
        .map(|(name, _, _)| name)
        .collect();
    assert_eq!(names, ["a", "b", "m"]);
}

#[test]
fn cas_ref_through_repo_records_reflog() {
    let (_dir, root, repo) = new_repo();
    let c1 = commit(&repo, &[], vec![put("f", b"1")]);
    let c2 = commit(&repo, &[c1], vec![put("f", b"2")]);

    assert_eq!(repo.get_ref("main").unwrap(), None);
    assert_eq!(
        repo.cas_ref("main", None, Some(c1), "agent/one").unwrap(),
        CasOutcome {
            result: CasResult::Applied { old: None },
            warnings: Vec::new(),
        },
        "a move whose every step succeeds has no warnings"
    );
    assert_eq!(
        repo.cas_ref("main", Some(c1), Some(c2), "agent/two")
            .unwrap()
            .result,
        CasResult::Applied { old: Some(c1) }
    );
    assert_eq!(
        repo.cas_ref("main", Some(c1), Some(c1), "agent/three")
            .unwrap()
            .result,
        CasResult::Mismatch { actual: Some(c2) },
        "a stale expectation moves nothing"
    );
    let main = RefState {
        target: c2,
        version: 2,
    };
    assert_eq!(repo.get_ref("main").unwrap(), Some(main));
    assert_eq!(repo.list_refs().unwrap(), [("main".to_owned(), main)]);

    // Every move is in the journal, newest first; the mismatch is not.
    let store = MetaStore::open(&root).unwrap();
    let log = store.read().unwrap().reflog("main", 10).unwrap();
    let moves: Vec<(Option<ChangesetId>, Option<ChangesetId>, &str)> = log
        .iter()
        .map(|entry| (entry.old, entry.new, entry.actor.as_str()))
        .collect();
    assert_eq!(
        moves,
        [
            (Some(c1), Some(c2), "agent/two"),
            (None, Some(c1), "agent/one")
        ]
    );

    match repo.cas_ref("feature/x", None, Some(c1), "agent/one") {
        Err(RepoError::RefName(name)) => assert_eq!(name, "feature/x"),
        other => panic!("expected RefName, got {other:?}"),
    }
    match repo.get_ref("HEAD") {
        Err(RepoError::RefName(name)) => assert_eq!(name, "HEAD"),
        other => panic!("expected RefName, got {other:?}"),
    }
    let unknown = unknown_id();
    assert_not_found(
        repo.cas_ref("release/1", None, Some(unknown), "agent/one"),
        &unknown.to_string(),
    );

    assert_eq!(
        repo.cas_ref("main", Some(c2), None, "agent/two")
            .unwrap()
            .result,
        CasResult::Applied { old: Some(c2) },
        "a delete"
    );
    assert_eq!(repo.get_ref("main").unwrap(), None);
    assert_eq!(repo.list_refs().unwrap(), []);
}

#[test]
fn create_open_and_info() {
    let dir = tempfile::tempdir().unwrap();
    let base = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    // create makes the directory, and the whole layout in it.
    let root = base.join("nested/store");
    let repo = LocalRepo::create(&root, "demo").unwrap();
    let info = repo.info().unwrap();
    assert_eq!(info.name, "demo");
    assert_eq!(info.repo_id.len(), 32);
    assert!(
        info.repo_id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "{}",
        info.repo_id
    );
    assert_eq!(
        info,
        RepoInfo {
            repo_id: info.repo_id.clone(),
            name: "demo".to_owned(),
            store_version: 1,
            changesets: 0,
            refs: 0,
            main: None,
        }
    );
    assert_eq!(
        fs::read(root.join("FORMAT")).unwrap(),
        b"swarmops-store 1\n"
    );
    for byte in 0..=255u8 {
        assert!(
            root.join(format!("objects/{byte:02x}")).is_dir(),
            "{byte:02x}"
        );
    }
    assert!(root.join("objects/tmp").is_dir());

    let id = commit(&repo, &[], vec![put("f", b"1")]);
    repo.cas_ref("main", None, Some(id), "test/agent").unwrap();
    let info = repo.info().unwrap();
    assert_eq!((info.changesets, info.refs, info.main), (1, 1, Some(id)));
    assert_eq!(
        LocalRepo::open(&root).unwrap().info().unwrap(),
        info,
        "reopened"
    );

    // Any name round-trips, line breaks and all.
    let odd = "a name\nwith a line break and ünïcödé";
    let other = LocalRepo::create(&base.join("odd"), odd).unwrap();
    assert_eq!(other.info().unwrap().name, odd);
    assert_eq!(
        LocalRepo::open(&base.join("odd"))
            .unwrap()
            .info()
            .unwrap()
            .name,
        odd
    );

    // open refuses what is not a store.
    let empty = base.join("empty");
    fs::create_dir(&empty).unwrap();
    for not_a_store in [empty.clone(), base.join("missing")] {
        match LocalRepo::open(&not_a_store) {
            Err(RepoError::Store(StoreError::Format(_))) => {}
            other => panic!(
                "{not_a_store}: expected Format, got {:?}",
                other.map(|r| r.info())
            ),
        }
    }
    fs::write(root.join("FORMAT"), b"swarmops-store 2\n").unwrap();
    match LocalRepo::open(&root) {
        Err(RepoError::Store(StoreError::Format(_))) => {}
        other => panic!("expected Format, got {:?}", other.map(|r| r.info())),
    }
}

#[test]
fn create_on_existing_store_is_exists() {
    let (_dir, root, repo) = new_repo();
    let id = commit(&repo, &[], vec![put("f", b"1")]);
    let info = repo.info().unwrap();
    // FORMAT is there: a store exists, and create changes nothing of it.
    match LocalRepo::create(&root, "other") {
        Err(RepoError::Exists) => {}
        other => panic!("expected Exists, got {:?}", other.map(|r| r.info())),
    }
    let reopened = LocalRepo::open(&root).unwrap();
    assert_eq!(reopened.info().unwrap(), info);
    assert!(reopened.changeset(id).unwrap().is_some());

    // Without FORMAT, the store is one a create left half done: a create finishes it.
    fs::remove_file(root.join("FORMAT")).unwrap();
    let resumed = LocalRepo::create(&root, "other").unwrap();
    assert_eq!(resumed.info().unwrap().name, "other");
    assert!(resumed.changeset(id).unwrap().is_some());
}

#[test]
fn open_requires_the_repo_file() {
    let (_dir, root, repo) = new_repo();
    let info = repo.info().unwrap();
    let repo_file = root.join("REPO");
    // A line with the id, then the name to the end of the file.
    assert_eq!(
        fs::read_to_string(&repo_file).unwrap(),
        format!("{}\ntest", info.repo_id)
    );
    let refused = |what: &str| match LocalRepo::open(&root) {
        Err(RepoError::Store(StoreError::Format(_))) => {}
        other => panic!("{what}: expected Format, got {:?}", other.map(|r| r.info())),
    };
    for (bytes, what) in [
        (b"no line break".as_slice(), "no id line"),
        (b"0123\ntest", "an id that is not 32 hex digits"),
        (b"\xff\ntest", "not UTF-8"),
    ] {
        fs::write(&repo_file, bytes).unwrap();
        refused(what);
    }
    fs::remove_file(&repo_file).unwrap();
    refused("no REPO");
}

#[test]
fn create_refuses_mnt_paths_before_touching_them() {
    // WSL mounts Windows drives under /mnt/ (drvfs): no store may live there. The check is
    // made before anything is created, so this test leaves no trace on the drive.
    for store in ["/mnt/c/swarmops-test-store", "/mnt/z/no/such/drive/store"] {
        match LocalRepo::create(Utf8Path::new(store), "x") {
            Err(RepoError::Store(StoreError::UnsupportedFs(refused))) => {
                assert_eq!(refused, store);
            }
            other => panic!(
                "{store}: expected UnsupportedFs, got {:?}",
                other.map(|r| r.info())
            ),
        }
        assert!(!Path::new(store).exists(), "{store} was created");
    }
}

/// The entries of directory `dir`, sorted; `None` if it does not exist.
fn entries(dir: &str) -> Option<Vec<String>> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .ok()?
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    Some(names)
}

#[test]
fn create_refuses_a_path_that_leads_under_mnt_before_touching_it() {
    // As given, the path is not under /mnt/, but it leads there: the check resolves it one
    // component at a time, as the kernel will once its missing directories exist (a `..` takes
    // back the name before it, there or not), and refuses before anything is created.
    let before = entries("/mnt");
    for (store, leads_to) in [
        ("/tmp/../mnt/x", "/mnt/x"),
        ("/tmp/../mnt/no/such/../drive/store", "/mnt/no/drive/store"),
    ] {
        match LocalRepo::create(Utf8Path::new(store), "x") {
            Err(RepoError::Store(StoreError::UnsupportedFs(refused))) => {
                assert_eq!(refused, leads_to);
            }
            other => panic!(
                "{store}: expected UnsupportedFs, got {:?}",
                other.map(|r| r.info())
            ),
        }
    }
    assert_eq!(entries("/mnt"), before, "nothing was created under /mnt");
}

#[test]
fn create_follows_symlinks_below_a_missing_directory_before_touching_anything() {
    let dir = tempfile::tempdir().unwrap();
    let base = Utf8PathBuf::from_path_buf(dir.path().canonicalize().unwrap()).unwrap();
    // Each link leads under /mnt/: one to a path that does not exist (where a directory
    // would be made all the same), one to /mnt itself. `nope/..` cancels a directory that does
    // not exist, so a walk up to the nearest existing ancestor would not see the link.
    std::os::unix::fs::symlink("/mnt/swarmops-test-nowhere", base.join("dangling")).unwrap();
    std::os::unix::fs::symlink("/mnt", base.join("mnt")).unwrap();
    for (store, leads_to) in [
        ("nope/../dangling/x", "/mnt/swarmops-test-nowhere/x"),
        ("nope/../mnt/x", "/mnt/x"),
        ("dangling/deeper/x", "/mnt/swarmops-test-nowhere/deeper/x"),
    ] {
        match LocalRepo::create(&base.join(store), "x") {
            Err(RepoError::Store(StoreError::UnsupportedFs(refused))) => {
                assert_eq!(refused, leads_to, "{store}");
            }
            other => panic!(
                "{store}: expected UnsupportedFs, got {:?}",
                other.map(|r| r.info())
            ),
        }
    }
    assert_eq!(
        entries(base.as_str()),
        Some(vec!["dangling".to_owned(), "mnt".to_owned()]),
        "nothing was created here"
    );
    assert!(!Path::new("/mnt/swarmops-test-nowhere").exists());

    // A symlink loop is an error, not a hang.
    std::os::unix::fs::symlink(base.join("loop"), base.join("loop")).unwrap();
    match LocalRepo::create(&base.join("loop/x"), "x") {
        Err(RepoError::Store(StoreError::Io(_))) => {}
        other => panic!("expected an I/O error, got {:?}", other.map(|r| r.info())),
    }
}

#[test]
fn create_syncs_each_directory_it_makes() {
    // create makes the store root and the missing directories above it: each new entry is
    // made durable, by syncing the directory that holds it, outermost first.
    let dir = tempfile::tempdir().unwrap();
    let base = Utf8PathBuf::from_path_buf(dir.path().canonicalize().unwrap()).unwrap();
    let (fs, calls) = RecordingFs::new();
    let fs = fs.naming_paths_in(base.clone().into_std_path_buf());
    LocalRepo::create_with_fs(&base.join("a/b/store"), "demo", Arc::new(fs)).unwrap();
    let recorded = calls();
    assert_eq!(
        recorded[..5],
        [
            "create_dir_all a/b/store",
            "fsync_dir .",
            "fsync_dir a",
            "fsync_dir a/b",
            "exists a/b/store/FORMAT",
        ]
    );

    // A store directory that exists already (made by hand, or by a create that crashed) has
    // its entry synced once all the same.
    fs::create_dir(base.join("made")).unwrap();
    let (fs, calls) = RecordingFs::new();
    let fs = fs.naming_paths_in(base.clone().into_std_path_buf());
    LocalRepo::create_with_fs(&base.join("made"), "demo", Arc::new(fs)).unwrap();
    let recorded = calls();
    assert_eq!(
        recorded[..3],
        ["create_dir_all made", "fsync_dir .", "exists made/FORMAT"]
    );
}

/// The filesystem calls of `LocalRepo::create`, in order, on a fresh store.
fn create_calls() -> Vec<String> {
    let dir = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
    // create canonicalizes the store path; record paths against the canonical root.
    let canonical_dir = dir.path().canonicalize().unwrap();
    let (fs, calls) = RecordingFs::new();
    let fs = fs.naming_paths_in(canonical_dir.join("store"));
    LocalRepo::create_with_fs(&root, "demo", Arc::new(fs)).unwrap();
    calls()
}

/// Where in [`create_calls`] the metadata store's own create runs: after `exists meta`, up to
/// and including its rename of `meta.tmp/meta` to `meta`.
fn meta_create_span(calls: &[String]) -> (usize, usize) {
    let start = calls.iter().position(|c| c == "exists meta").unwrap() + 1;
    let end = calls
        .iter()
        .position(|c| c == "rename_durable meta")
        .unwrap();
    (start, end)
}

#[test]
fn create_makes_the_documented_fs_calls() {
    let calls = create_calls();
    // The store root made, and its entry in its parent made durable.
    let mut want = ["create_dir_all .", "fsync_dir ..", "exists FORMAT"]
        .map(str::to_owned)
        .to_vec();
    want.extend((0..=255u8).map(|byte| format!("create_dir_all objects/{byte:02x}")));
    want.extend(
        [
            "create_dir_all objects/tmp",
            "fsync_dir objects",
            "fsync_dir .",
            "exists meta",
        ]
        .map(str::to_owned),
    );
    // Then the metadata store's create, staged in meta.tmp/ and renamed into place: the
    // journal and its checkpoint, the index (four columns, parents.log, the four sorted view
    // files), refs/ and the lock file, each directory synced.
    let (start, end) = meta_create_span(&calls);
    assert_eq!(calls[..start], want[..]);
    let staged = [
        "create_dir_all meta.tmp/meta",
        "append_durable meta.tmp/meta/journal.log",
        "append_durable meta.tmp/meta/journal.off",
        "fsync_dir meta.tmp/meta",
        "fsync_dir meta.tmp",
        "atomic_replace meta.tmp/meta/journal.ckpt",
        "create_dir_all meta.tmp/meta/idx",
        "create_dir_all meta.tmp/meta/sorted",
        "append meta.tmp/meta/idx/ids.col",
        "append meta.tmp/meta/idx/gen.col",
        "append meta.tmp/meta/idx/dh.col",
        "append meta.tmp/meta/idx/par.col",
        "append meta.tmp/meta/parents.log",
        "append meta.tmp/meta/sorted/ids.key",
        "append meta.tmp/meta/sorted/ids.rec",
        "append meta.tmp/meta/sorted/dh.key",
        "append meta.tmp/meta/sorted/dh.rec",
        "fsync_dir meta.tmp/meta/idx",
        "fsync_dir meta.tmp/meta/sorted",
        "fsync_dir meta.tmp/meta",
        "create_dir_all meta.tmp/meta/refs",
        "append meta.tmp/meta/lock",
        "fsync_dir meta.tmp/meta",
    ];
    assert_eq!(calls[start..end], staged);
    // Then the identity, and FORMAT last of all: linked into place, never half written.
    assert_eq!(
        calls[end..],
        [
            "rename_durable meta",
            "atomic_replace REPO",
            "write_durable_temp objects/tmp",
            "link_noclobber FORMAT",
            "fsync_dir .",
        ]
    );
}

/// One run of the create crash sweep: a child creates a store until its k-th filesystem call
/// (of `total`) aborts it. A store must then open only if `FORMAT` is in place, and a create
/// must otherwise pick up where the crashed one stopped; either way the store is fresh and
/// takes a commit.
#[cfg(feature = "test-hooks")]
fn create_crashing_at(k: u64, total: u64) {
    let dir = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
    let child = run(this_test("create_resumes_after_a_crash_at_any_call")
        .env(CHILD_ROOT, &root)
        .env(CRASH_AT_ENV, k.to_string()));
    let finished = child.status.success();
    assert_eq!(
        finished,
        k > total,
        "k={k}: {:?}\n{}",
        child.status,
        String::from_utf8_lossy(&child.stderr)
    );
    assert!(finished || child.status.signal() == Some(SIGABRT), "k={k}");

    // The crash at the last call (the store root's sync) comes after FORMAT's link.
    let repo = match LocalRepo::open(&root) {
        Ok(repo) => {
            assert!(k >= total, "k={k}: a store opened before FORMAT was linked");
            repo
        }
        Err(RepoError::Store(StoreError::Format(_))) => {
            assert!(k < total, "k={k}");
            LocalRepo::create(&root, "demo").unwrap()
        }
        Err(e) => panic!("k={k}: {e}"),
    };
    let info = repo.info().unwrap();
    assert_eq!((info.name.as_str(), info.changesets), ("demo", 0), "k={k}");
    let id = commit(&repo, &[], vec![put("f", b"after the crash")]);
    assert_eq!(repo.verify(true).unwrap().problems, [], "k={k}");
    let reopened = LocalRepo::open(&root).unwrap();
    assert_eq!(reopened.info().unwrap(), repo.info().unwrap(), "k={k}");
    assert!(reopened.changeset(id).unwrap().is_some(), "k={k}");
}

#[test]
#[cfg(feature = "test-hooks")]
fn create_resumes_after_a_crash_at_any_call() {
    if let Some(root) = std::env::var(CHILD_ROOT).ok().map(Utf8PathBuf::from) {
        // SWARMOPS_TEST_CRASH_AT is set: create's k-th filesystem call aborts the process.
        LocalRepo::create(&root, "demo").unwrap();
        return;
    }
    let calls = create_calls();
    let total = u64::try_from(calls.len()).unwrap();
    // Every call (k counts from 1), except in two runs of alike calls, where the sweep takes
    // the first, the middle and the last two: the 257 directory creations of objects/, and
    // the metadata store's own create, whose own crash sweep covers it call by call. Past the
    // last call, the child finishes.
    let k_of = |call: &str| {
        let at = calls.iter().position(|c| c == call).unwrap();
        u64::try_from(at + 1).unwrap()
    };
    let (meta_start, meta_end) = meta_create_span(&calls);
    let runs = [
        (
            k_of("create_dir_all objects/00"),
            k_of("create_dir_all objects/tmp"),
        ),
        (
            u64::try_from(meta_start + 1).unwrap(),
            u64::try_from(meta_end + 1).unwrap(),
        ),
    ];
    let sampled = |k: u64| {
        runs.iter().all(|&(first, last)| {
            !(first..=last).contains(&k)
                || [first, first.midpoint(last), last - 1, last].contains(&k)
        })
    };
    let ks: Vec<u64> = (1..=total + 1).filter(|&k| sampled(k)).collect();
    // Eight runs at a time, each with a child and a store of its own.
    for at_once in ks.chunks(8) {
        thread::scope(|scope| {
            for &k in at_once {
                scope.spawn(move || create_crashing_at(k, total));
            }
        });
    }
}
