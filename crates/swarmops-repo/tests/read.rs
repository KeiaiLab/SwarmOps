//! Reading a repository without a checkout: files and directories at a revision, revisions
//! resolved from refs, ids and prefixes with `~N`, the first-parent log, and refs moved
//! through the repository. Also the store's lifecycle: `create` (which resumes after a crash
//! at any of its filesystem calls, `FORMAT` written last), `open` and `info`.
//!
//! Every test owns a fresh store (`tempfile::tempdir()`). The crash sweep runs this test
//! binary again as its children (selected by [`CHILD_ROOT`]).

use std::fs;
#[cfg(feature = "test-hooks")]
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::sync::Arc;
#[cfg(feature = "test-hooks")]
use std::thread;

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_core::{ChangesetId, EntryKind, LeafKind, ObjectId, ObjectKind, hash_object};
use swarmops_repo::{CasOutcome, CasResult, LocalRepo, RefState, RepoError, RepoInfo};
#[cfg(feature = "test-hooks")]
use swarmops_store::{CRASH_AT_ENV, ORDER_CHECK_ENV, RealFs};
use swarmops_store::{ChangesetMeta, MetaStore, Recovered, StoreError};
#[cfg(feature = "test-hooks")]
use swarmops_testkit::child::{SIGABRT, run, this_test};
#[cfg(feature = "test-hooks")]
use swarmops_testkit::faults::checked;
use swarmops_testkit::fs::RecordingFs;
use swarmops_testkit::repo::{blob, commit, new_repo, path, put, put_as, request};
#[cfg(feature = "test-hooks")]
use swarmops_testkit::test_name;

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
            .map(|e| (e.name, e.target.kind(), e.target.id()))
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
            put_as("bin/run", LeafKind::Exec, script),
            put_as("bin/link", LeafKind::Symlink, b"run"),
            put("src/lib.rs", b"pub fn f() {}\n"),
        ],
    );

    // Each file with its kind, its blob's id and its bytes, read from the objects alone.
    let read = |p: &str| repo.read_blob_at(id, &path(p)).unwrap();
    let file = |kind: LeafKind, content: &[u8]| Some((kind, blob(content), content.to_vec()));
    assert_eq!(read("README.md"), file(LeafKind::File, b"# demo\n"));
    assert_eq!(read("bin/run"), file(LeafKind::Exec, script));
    assert_eq!(read("bin/link"), file(LeafKind::Symlink, b"run"));
    assert_eq!(read("src/lib.rs"), file(LeafKind::File, b"pub fn f() {}\n"));
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
            recovered: Recovered::default(),
        },
        "a move whose every step succeeds has no warnings, and repaired nothing first"
    );
    assert_eq!(
        repo.cas_ref("main", Some(c1), Some(c2), "agent/two")
            .unwrap()
            .result,
        CasResult::Applied { old: Some(c1) }
    );
    assert_eq!(
        repo.cas_ref("main", Some(c2), Some(c2), "agent/four")
            .unwrap()
            .result,
        CasResult::Applied { old: Some(c2) },
        "a move to the target it expects finds it there, under the lock"
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

    // Every move is in the journal, newest first; the mismatch and the move to the target it
    // expects are not.
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
fn a_huge_format_file_is_not_a_store_and_is_not_read_whole() {
    // FORMAT holds 17 bytes; one of any other length names no layout this build reads. A
    // crafted one of huge length (a sparse file, which costs no disk) is refused by its length,
    // before any of it is read, with the Format error a foreign FORMAT gets (E_NOT_A_REPO).
    let (_dir, root, repo) = new_repo();
    drop(repo);
    let format = root.join("FORMAT");
    fs::OpenOptions::new()
        .write(true)
        .open(&format)
        .unwrap()
        .set_len(1 << 30)
        .unwrap();
    let started = std::time::Instant::now();
    match LocalRepo::open(&root) {
        Err(RepoError::Store(StoreError::Format(what))) => assert_eq!(
            what,
            format!("{root}/FORMAT is 1073741824 bytes long, not a store layout this build reads")
        ),
        other => panic!("expected Format, got {:?}", other.map(|r| r.info())),
    }
    assert!(
        started.elapsed() < std::time::Duration::from_secs(1),
        "read whole: {:?}",
        started.elapsed()
    );
}

#[test]
fn a_format_file_one_byte_too_long_is_refused_by_its_length() {
    // The length check's boundary: a FORMAT of its 17 bytes is read, and refused for what it
    // names when that is another layout; one byte more is refused by its length, before it is
    // read.
    let (_dir, root, repo) = new_repo();
    drop(repo);
    let format = root.join("FORMAT");
    let refused = |what: String| match LocalRepo::open(&root) {
        Err(RepoError::Store(StoreError::Format(said))) => assert_eq!(said, what),
        other => panic!("{what}: expected Format, got {:?}", other.map(|r| r.info())),
    };
    fs::write(&format, b"swarmops-store 2\n").unwrap();
    refused(format!(
        "{root}/FORMAT names a store layout this build does not read"
    ));
    fs::write(&format, b"swarmops-store 1\n\n").unwrap();
    refused(format!(
        "{root}/FORMAT is 18 bytes long, not a store layout this build reads"
    ));
    fs::write(&format, b"swarmops-store 1\n").unwrap();
    assert_eq!(
        LocalRepo::open(&root).unwrap().info().unwrap().changesets,
        0
    );
}

#[test]
fn a_huge_repo_file_is_not_a_store_and_is_not_read_whole() {
    // REPO holds an id line and a name of at most 255 bytes, 288 bytes in all. A crafted one
    // of huge length (a sparse file, which costs no disk, and whose zeros after the id line are
    // UTF-8, so they would read as a name) is refused by its length, before any of it is read,
    // with the Format error a malformed REPO gets (E_NOT_A_REPO).
    let (_dir, root, repo) = new_repo();
    drop(repo);
    fs::OpenOptions::new()
        .write(true)
        .open(root.join("REPO"))
        .unwrap()
        .set_len(1 << 30)
        .unwrap();
    let started = std::time::Instant::now();
    match LocalRepo::open(&root) {
        Err(RepoError::Store(StoreError::Format(what))) => assert_eq!(
            what,
            format!(
                "{root}/REPO is 1073741824 bytes long, more than a repository id and a name of \
                 at most 255 bytes"
            )
        ),
        // The name's length, not the name: one read whole is a gigabyte long.
        other => panic!(
            "expected Format, got {:?}",
            other.map(|r| r.info().map(|info| info.name.len()))
        ),
    }
    assert!(
        started.elapsed() < std::time::Duration::from_secs(1),
        "read whole: {:?}",
        started.elapsed()
    );
}

#[test]
fn a_repo_file_holds_a_name_of_at_most_255_bytes() {
    // The length check's boundary: a REPO of the id line and a 255-byte name (288 bytes) opens
    // with that name; one byte more is refused by its length, before it is read.
    let (_dir, root, repo) = new_repo();
    let repo_id = repo.info().unwrap().repo_id;
    drop(repo);
    let repo_file = root.join("REPO");
    let name = "n".repeat(255);
    fs::write(&repo_file, format!("{repo_id}\n{name}")).unwrap();
    let info = LocalRepo::open(&root).unwrap().info().unwrap();
    assert_eq!((&info.repo_id, &info.name), (&repo_id, &name));
    fs::write(&repo_file, format!("{repo_id}\n{name}n")).unwrap();
    match LocalRepo::open(&root) {
        Err(RepoError::Store(StoreError::Format(what))) => assert_eq!(
            what,
            format!(
                "{root}/REPO is 289 bytes long, more than a repository id and a name of at most \
                 255 bytes"
            )
        ),
        other => panic!("expected Format, got {:?}", other.map(|r| r.info())),
    }
}

#[test]
fn create_refuses_a_name_of_more_than_255_bytes_before_touching_anything() {
    // A name is counted in bytes, not characters: 128 "é" are 256 bytes. One past the limit is
    // refused before create makes any filesystem call, so nothing is made; one of 255 bytes
    // fills REPO to its 288 bytes and reads back whole.
    let dir = tempfile::tempdir().unwrap();
    let store = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
    for name in ["n".repeat(256), "é".repeat(128)] {
        let chars = name.chars().count();
        let (recording, calls) = RecordingFs::new();
        match LocalRepo::create_with_fs(&store, &name, Arc::new(recording)) {
            Err(RepoError::Store(StoreError::Limit {
                what: swarmops_store::LimitKind::RepoNameBytes,
                max: 255,
                got: 256,
            })) => {}
            other => panic!(
                "{chars} characters: expected Limit, got {:?}",
                other.map(|r| r.info())
            ),
        }
        assert_eq!(calls(), Vec::<String>::new(), "{chars} characters");
        assert!(!store.exists(), "{chars} characters: {store} was made");
    }
    let name = "n".repeat(255);
    let (recording, _calls) = RecordingFs::new();
    LocalRepo::create_with_fs(&store, &name, Arc::new(recording)).unwrap();
    assert_eq!(fs::metadata(store.join("REPO")).unwrap().len(), 288);
    assert_eq!(LocalRepo::open(&store).unwrap().info().unwrap().name, name);
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

    // A symlink loop is an error, not a hang: one of working out where the path leads, which
    // names no file of a store (there is none yet).
    std::os::unix::fs::symlink(base.join("loop"), base.join("loop")).unwrap();
    match LocalRepo::create(&base.join("loop/x"), "x") {
        Err(RepoError::Store(StoreError::Io(swarmops_store::IoFailure {
            op: swarmops_store::FileOp::Resolve,
            file: None,
            ..
        }))) => {}
        other => panic!(
            "expected an I/O error resolving the path, got {:?}",
            other.map(|r| r.info())
        ),
    }
}

#[test]
fn open_refuses_a_path_that_leads_under_mnt() {
    // A store copied, moved or linked under /mnt/ (drvfs, whose locking and sync semantics the
    // refusal keeps out) is refused on open as on create: by where the path leads, before
    // FORMAT is read, so nothing under /mnt/ need exist, and nothing is made there.
    let before = entries("/mnt");
    let dir = tempfile::tempdir().unwrap();
    let base = Utf8PathBuf::from_path_buf(dir.path().canonicalize().unwrap()).unwrap();
    std::os::unix::fs::symlink("/mnt/swarmops-test-nowhere", base.join("dangling")).unwrap();
    let linked = base.join("dangling/x");
    for (store, leads_to) in [
        ("/mnt/c/swarmops-test-store", "/mnt/c/swarmops-test-store"),
        (linked.as_str(), "/mnt/swarmops-test-nowhere/x"),
    ] {
        match LocalRepo::open(Utf8Path::new(store)) {
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
    assert!(!Path::new("/mnt/swarmops-test-nowhere").exists());
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
/// takes a commit. This process opens, creates and commits under the order checker
/// ([`checked`]), as the child ran.
#[cfg(feature = "test-hooks")]
fn create_crashing_at(k: u64, total: u64) {
    let dir = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
    let child = run(
        this_test(&test_name!("create_resumes_after_a_crash_at_any_call"))
            .env(CHILD_ROOT, &root)
            .env(CRASH_AT_ENV, k.to_string())
            .env(ORDER_CHECK_ENV, "1"),
    );
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
    let repo = match LocalRepo::open_with_fs(&root, checked(RealFs)) {
        Ok(repo) => {
            assert!(k >= total, "k={k}: a store opened before FORMAT was linked");
            repo
        }
        Err(RepoError::Store(StoreError::Format(_))) => {
            assert!(k < total, "k={k}");
            LocalRepo::create_with_fs(&root, "demo", checked(RealFs)).unwrap()
        }
        Err(e) => panic!("k={k}: {e}"),
    };
    let info = repo.info().unwrap();
    assert_eq!((info.name.as_str(), info.changesets), ("demo", 0), "k={k}");
    let id = commit(&repo, &[], vec![put("f", b"after the crash")]);
    assert_eq!(repo.verify(true).unwrap().problems, [], "k={k}");
    let reopened = LocalRepo::open_with_fs(&root, checked(RealFs)).unwrap();
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

/// One snapshot per request (storage spec §8, operations spec §4): what a [`RepoRead`] reads,
/// it reads in one view of the metadata, which later commits and ref moves do not change.
///
/// [`RepoRead`]: swarmops_repo::RepoRead
mod snapshot {
    use swarmops_core::{BlobId, EntryTarget, TreeId};

    use super::*;

    /// A snapshot taken before another handle commits on `main` and moves it still resolves
    /// `main` (with `~N`) to the old head, shows the refs as they were, reads the old head's files
    /// and history, and holds none of what came after; a new snapshot sees the commit and the
    /// move.
    #[test]
    fn a_snapshot_reads_every_revision_of_a_request_in_one_view() {
        let (_dir, root, repo) = new_repo();
        let c1 = commit(&repo, &[], vec![put("f", b"1")]);
        let c2 = commit(&repo, &[c1], vec![put("f", b"2")]);
        repo.cas_ref("main", None, Some(c2), "test/agent").unwrap();

        let snap = repo.snapshot().unwrap();
        // Another handle on the store commits on main and moves it.
        let other = LocalRepo::open(&root).unwrap();
        let c3 = commit(&other, &[c2], vec![put("f", b"3")]);
        other
            .cas_ref("main", Some(c2), Some(c3), "test/other")
            .unwrap();

        assert_eq!(
            snap.resolve("main").unwrap(),
            c2,
            "main as the snapshot saw it"
        );
        assert_eq!(snap.resolve("main~1").unwrap(), c1);
        let main = RefState {
            target: c2,
            version: 1,
        };
        assert_eq!(snap.get_ref("main").unwrap(), Some(main));
        assert_eq!(snap.list_refs().unwrap(), [("main".to_owned(), main)]);
        let info = snap.info().unwrap();
        assert_eq!((info.changesets, info.main), (2, Some(c2)));
        assert_eq!(
            snap.read_blob_at(c2, &path("f")).unwrap(),
            Some((LeafKind::File, blob(b"2"), b"2".to_vec()))
        );
        let log: Vec<ChangesetId> = (snap.log_first_parent(c2, 10).unwrap().iter())
            .map(|cs| cs.id)
            .collect();
        assert_eq!(log, [c2, c1]);
        // What came after the snapshot is not in it.
        assert_eq!(snap.changeset(c3).unwrap(), None);
        let after = c3.to_string();
        assert_not_found(snap.resolve(&after), &after);
        assert_not_found(snap.read_blob_at(c3, &path("f")), &after);

        let fresh = repo.snapshot().unwrap();
        assert_eq!(fresh.resolve("main").unwrap(), c3);
        assert_eq!(fresh.resolve("main~1").unwrap(), c2);
        assert_eq!(fresh.changeset(c3).unwrap().map(|cs| cs.id), Some(c3));
        assert_eq!(fresh.info().unwrap().main, Some(c3));
    }

    /// `entry_at` names what a path holds at a changeset: a file with its kind and blob, a
    /// directory with its tree (the one its parent's listing gives), or nothing, also below a
    /// file or a missing directory. A changeset the store does not hold is `NotFound`.
    #[test]
    fn entry_at_names_what_a_path_holds() {
        let (_dir, _root, repo) = new_repo();
        let id = commit(
            &repo,
            &[],
            vec![
                put("src/lib.rs", b"pub fn f() {}\n"),
                put_as("bin/run", LeafKind::Exec, b"#!/bin/sh\n"),
            ],
        );
        let snap = repo.snapshot().unwrap();
        let at = |p: &str| snap.entry_at(id, &path(p)).unwrap();
        assert_eq!(
            at("src/lib.rs"),
            Some(EntryTarget::Leaf(LeafKind::File, blob(b"pub fn f() {}\n")))
        );
        assert_eq!(
            at("bin/run"),
            Some(EntryTarget::Leaf(LeafKind::Exec, blob(b"#!/bin/sh\n")))
        );
        let listed = snap.list_tree(id, None).unwrap().unwrap();
        let src = listed.iter().find(|entry| entry.name == "src").unwrap();
        assert!(matches!(src.target, EntryTarget::Dir(_)), "{src:?}");
        assert_eq!(at("src"), Some(src.target));
        for nothing in ["README.md", "src/main.rs", "src/lib.rs/x", "docs/a.md"] {
            assert_eq!(at(nothing), None, "{nothing}");
        }
        let unknown = unknown_id();
        assert_not_found(snap.entry_at(unknown, &path("src")), &unknown.to_string());
    }

    /// `tree` and `blob` read an object by id, verified as every object read is: a tree decoded,
    /// a blob's bytes; an id the store lacks, or that names an object of the other kind, is
    /// `MissingObject` with the kind asked for.
    #[test]
    fn tree_and_blob_read_verified_objects_by_id() {
        let (_dir, _root, repo) = new_repo();
        let id = commit(&repo, &[], vec![put("src/lib.rs", b"pub fn f() {}\n")]);
        let snap = repo.snapshot().unwrap();
        let Some(EntryTarget::Dir(src)) = snap.entry_at(id, &path("src")).unwrap() else {
            panic!("src is a directory");
        };
        let tree = snap.tree(src).unwrap();
        let names: Vec<&str> = tree.entries().iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["lib.rs"]);
        let lib = blob(b"pub fn f() {}\n");
        assert_eq!(snap.blob(lib).unwrap(), b"pub fn f() {}\n");

        match snap.blob(BlobId(src.0)) {
            Err(RepoError::MissingObject { id, kind }) => {
                assert_eq!((id, kind), (src.0, ObjectKind::Blob));
            }
            other => panic!("expected MissingObject, got {other:?}"),
        }
        match snap.tree(TreeId(lib.0)) {
            Err(RepoError::MissingObject { id, kind }) => {
                assert_eq!((id, kind), (lib.0, ObjectKind::Tree));
            }
            other => panic!("expected MissingObject, got {other:?}"),
        }
        let absent = hash_object(ObjectKind::Blob, b"never stored");
        match snap.blob(BlobId(absent)) {
            Err(RepoError::MissingObject { id, .. }) => assert_eq!(id, absent),
            other => panic!("expected MissingObject, got {other:?}"),
        }
    }
}

/// `read_file_span` and `list_tree_depth`: a part of a file, of which only what is returned is
/// kept while the whole blob is verified, and a listing to a depth, with sizes, that reads the
/// trees it enters and no more.
mod span {
    use swarmops_core::{EntryTarget, TreeId};
    use swarmops_repo::{FileSpan, FileSpanRead, ListedEntry};
    use swarmops_store::Layout;

    use super::*;

    /// `len` bytes that zstd cannot shrink, so that the store keeps them raw.
    fn incompressible(len: usize) -> Vec<u8> {
        let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state.to_le_bytes()[0]
            })
            .collect()
    }

    /// A mebibyte of 16-byte lines numbered from 1 (`line 0000000001\n`): text zstd shrinks.
    fn numbered_lines() -> Vec<u8> {
        (1..=65_536_u32)
            .flat_map(|n| format!("line {n:010}\n").into_bytes())
            .collect()
    }

    /// A repository at `root`, made by `new_repo`, opened again on a `RecordingFs` that names
    /// each call by its path in the store; and the function that returns the calls made since it
    /// last ran.
    fn recorded(root: &Utf8Path) -> (LocalRepo, impl Fn() -> Vec<String>) {
        let (recording, calls) = RecordingFs::new();
        let recording = recording.naming_paths_in(root.as_std_path().to_path_buf());
        (
            LocalRepo::open_with_fs(root, Arc::new(recording)).unwrap(),
            calls,
        )
    }

    /// `read objects/<2hex>/<62hex>`: how the recorder names a read of object `id`'s file.
    fn read_of(id: ObjectId) -> String {
        format!("read {}", Layout::new("").object_path(id))
    }

    /// Bytes 1000 to 1099 of a mebibyte stored raw and of one stored zstd: those bytes alone, with
    /// the file's kind, blob and size, from one read of the blob's object file (after the
    /// changeset's and the root tree's, which find it). A byte flipped far outside the span is
    /// `Corrupt` all the same: the whole blob is verified.
    #[test]
    fn read_range_keeps_only_what_it_returns_and_verifies_the_whole() {
        let (_dir, root, _) = new_repo();
        let (repo, calls) = recorded(&root);
        let raw = incompressible(1 << 20);
        let text = numbered_lines();
        let cs = commit(
            &repo,
            &[],
            vec![put("raw.bin", &raw), put("text.txt", &text)],
        );
        let layout = Layout::new(root.clone());
        let snap = repo.snapshot().unwrap();
        let tree = snap.changeset(cs).unwrap().unwrap().changeset.root;
        for (name, content, enc) in [("raw.bin", &raw, 0_u8), ("text.txt", &text, 1)] {
            let id = blob(content);
            let file = layout.object_path(id.0);
            assert_eq!(fs::read(&file).unwrap()[5], enc, "{name}: raw 0, zstd 1");
            calls();
            let span = FileSpan::Bytes { start: 1000 };
            let got = snap.read_file_span(cs, &path(name), span, 100).unwrap();
            let want = FileSpanRead {
                kind: LeafKind::File,
                blob: id,
                size: 1 << 20,
                start: 1000,
                bytes: content[1000..1100].to_vec(),
                lines: None,
            };
            assert_eq!(got, Some(want), "{name}");
            assert_eq!(
                calls(),
                [read_of(cs.0), read_of(tree.0), read_of(id.0)],
                "{name}: one FsOps::read of the blob's file"
            );

            let pristine = fs::read(&file).unwrap();
            let mut damaged = pristine.clone();
            let far = damaged.len() * 3 / 4;
            damaged[far] ^= 0x01;
            fs::write(&file, &damaged).unwrap();
            let got = snap.read_file_span(cs, &path(name), span, 100);
            fs::write(&file, &pristine).unwrap();
            match got {
                Err(RepoError::Store(StoreError::Corrupt { id: bad })) => assert_eq!(bad, id.0),
                other => panic!("{name}: expected Corrupt, got {other:?}"),
            }
        }
        // No file there: a directory, or nothing.
        let dir = commit(&repo, &[cs], vec![put("src/lib.rs", b"pub fn f() {}\n")]);
        let snap = repo.snapshot().unwrap();
        for nothing in ["src", "src/main.rs", "raw.bin/x"] {
            let got = snap.read_file_span(dir, &path(nothing), FileSpan::Bytes { start: 0 }, 9);
            assert_eq!(got.unwrap(), None, "{nothing}");
        }
    }

    /// Lines of a zstd mebibyte whose 64 KiB pieces end inside them, and the lines of a range that
    /// exist, by the pieces they came in: the bytes, where they start, and which lines they are.
    #[test]
    fn a_line_span_is_read_across_the_pieces_of_a_stream() {
        let (_dir, _root, repo) = new_repo();
        let text = numbered_lines();
        let short = b"one\ntwo\nthree".to_vec();
        let cs = commit(
            &repo,
            &[],
            vec![put("text.txt", &text), put("short.txt", &short)],
        );
        let snap = repo.snapshot().unwrap();
        let read = |name: &str, first: u64, last: u64, max: u64| {
            let span = FileSpan::Lines { first, last };
            let got = snap.read_file_span(cs, &path(name), span, max).unwrap();
            let got = got.expect("a file");
            (got.start, got.bytes, got.lines)
        };
        // Line n of the text starts at (n - 1) * 16: lines 4095 to 4097 cross 65,536.
        assert_eq!(
            read("text.txt", 4095, 4097, u64::MAX),
            (65_504, text[65_504..65_552].to_vec(), Some((4095, 4097)))
        );
        // Kept up to `max`, while the lines are still counted to the range's end.
        assert_eq!(
            read("text.txt", 4095, 4097, 20),
            (65_504, text[65_504..65_524].to_vec(), Some((4095, 4097)))
        );
        assert_eq!(
            read("text.txt", 65_535, 70_000, u64::MAX),
            (
                1_048_544,
                text[1_048_544..].to_vec(),
                Some((65_535, 65_536))
            )
        );
        assert_eq!(
            read("text.txt", 65_537, 65_537, 9),
            (1 << 20, Vec::new(), None)
        );
        // A last line without a newline is a line; a range before its start holds none.
        assert_eq!(
            read("short.txt", 2, 9, u64::MAX),
            (4, b"two\nthree".to_vec(), Some((2, 3)))
        );
        assert_eq!(read("short.txt", 3, 2, u64::MAX), (13, Vec::new(), None));
    }

    /// A page of a depth listing reads the changeset, the trees on the way to the listed
    /// directory and those of the directories it enters: none beyond the page to learn that more
    /// follows, and no blob (a leaf's size comes from its object's header). A page that resumes
    /// after a path reads the trees on that path the listing enters.
    #[test]
    fn a_listing_page_reads_the_trees_it_enters_and_no_more() {
        let (_dir, root, _) = new_repo();
        let (repo, calls) = recorded(&root);
        let cs = commit(
            &repo,
            &[],
            vec![
                put("a.txt", b"12345"),
                put("a/b.rs", b"fn b() {}\n"),
                put("a/c/d.md", b"# d\n"),
                put_as("bin/run", LeafKind::Exec, b"#!/bin/sh\n"),
            ],
        );
        let snap = repo.snapshot().unwrap();
        let root_tree = snap.changeset(cs).unwrap().unwrap().changeset.root;
        let dir = |p: &str| match snap.entry_at(cs, &path(p)).unwrap() {
            Some(EntryTarget::Dir(tree)) => tree,
            other => panic!("{p}: {other:?}"),
        };
        let (a, c): (TreeId, TreeId) = (dir("a"), dir("a/c"));
        let listed = |after: Option<&str>, limit: usize| {
            calls();
            let after = after.map(path);
            let page = (snap.list_tree_depth(cs, None, 3, after.as_ref(), limit))
                .unwrap()
                .expect("the root is a directory");
            let paths: Vec<String> = (page.entries.iter())
                .map(|entry| entry.path.to_string())
                .collect();
            (paths, page.truncated, calls())
        };

        // A page that ends at a directory the listing enters: more follow, its tree unread.
        assert_eq!(
            listed(None, 1),
            (
                vec!["a".to_owned()],
                true,
                vec![read_of(cs.0), read_of(root_tree.0)]
            )
        );
        assert_eq!(
            listed(None, 2),
            (
                vec!["a".to_owned(), "a/b.rs".to_owned()],
                true,
                vec![read_of(cs.0), read_of(root_tree.0), read_of(a.0)]
            )
        );
        assert_eq!(
            listed(Some("a/b.rs"), 2),
            (
                vec!["a/c".to_owned(), "a/c/d.md".to_owned()],
                true,
                vec![
                    read_of(cs.0),
                    read_of(root_tree.0),
                    read_of(a.0),
                    read_of(c.0)
                ]
            )
        );
        let page = snap
            .list_tree_depth(cs, Some(&path("a")), 2, None, 10)
            .unwrap()
            .expect("a directory");
        assert!(!page.truncated);
        assert_eq!(
            page.entries,
            [
                ListedEntry {
                    path: path("b.rs"),
                    target: EntryTarget::Leaf(LeafKind::File, blob(b"fn b() {}\n")),
                    size: Some(10),
                },
                ListedEntry {
                    path: path("c"),
                    target: EntryTarget::Dir(c),
                    size: None,
                },
                ListedEntry {
                    path: path("c/d.md"),
                    target: EntryTarget::Leaf(LeafKind::File, blob(b"# d\n")),
                    size: Some(4),
                },
            ]
        );
        // A file, or nothing, at the listed path.
        for nothing in ["a.txt", "zz"] {
            let got = snap.list_tree_depth(cs, Some(&path(nothing)), 1, None, 10);
            assert_eq!(got.unwrap(), None, "{nothing}");
        }
    }
}
