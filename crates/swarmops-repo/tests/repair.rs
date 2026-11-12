//! Tests of `Repo::repair` (the M6 design §7): rebuilding `journal.off`, the changeset index and
//! the ref files from the journal, quarantining damaged objects so `put_blob` restores them,
//! dropping damaged or mismatched derived values and external id entries, dry runs that write
//! nothing (tree hash before = after), idempotency, refusal of journal damage past a torn tail
//! leaving `journal.log` byte-identical, maintenance lease contention, and no host paths in
//! responses.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::FileExt;
use std::time::{Duration, Instant};

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_core::{ChangesetId, ObjectId, RepoPath};
use swarmops_repo::{
    Depth, Derivation, FirstParentSkipList, LastModified, LastTouch, LocalRepo, PathKey,
    RepairAction, RepairKind, RepairReport, RepairRequest, RepoError, StoreError, StoreFormat,
};
use swarmops_store::{DerivedSlot, ExternalEntry, Layout, MaintLease, SYSTEM_GIT};
use swarmops_testkit::repo::{commit, put};
use tempfile::TempDir;

const ACTOR: &str = "test/1";
const FORMATS: [StoreFormat; 2] = [StoreFormat::V1, StoreFormat::V2];
const MAX_ZEROED: u64 = 512;

/// Creates a repository of `format` with `count` commits on `main` and `release/1` at the last.
fn build_repo(
    format: StoreFormat,
    count: usize,
) -> (TempDir, Utf8PathBuf, LocalRepo, Vec<ChangesetId>) {
    let dir = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
    let repo = if let Ok(repo) = LocalRepo::create_as(&root, "test", format) {
        repo
    } else {
        let v1 = LocalRepo::create_as(&root, "test", StoreFormat::V1).unwrap();
        v1.upgrade().unwrap();
        LocalRepo::open(&root).unwrap()
    };
    let mut ids = Vec::with_capacity(count);
    let mut parent = None;
    for i in 0..count {
        let parents: Vec<ChangesetId> = parent.into_iter().collect();
        let id = commit(
            &repo,
            &parents,
            vec![
                put("a.txt", format!("line {i}\n").as_bytes()),
                put("b.txt", b"constant\n"),
            ],
        );
        repo.cas_ref("main", parent, Some(id), ACTOR).unwrap();
        if i == 0 {
            repo.cas_ref("release/1", None, Some(id), ACTOR).unwrap();
        } else if i + 1 == count {
            let prev = repo.get_ref("release/1").unwrap().unwrap().target;
            repo.cas_ref("release/1", Some(prev), Some(id), ACTOR)
                .unwrap();
        }
        parent = Some(id);
        ids.push(id);
    }
    repo.flush_metadata().unwrap();
    (dir, root, repo, ids)
}

/// Every regular file and directory under `root`, mapped to its bytes (`None` for a directory),
/// so `tree_snapshot(&root)` before and after a dry run checks that not a single path or byte
/// changed.
fn tree_snapshot(root: &Utf8Path) -> BTreeMap<Utf8PathBuf, Option<Vec<u8>>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_owned()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = Utf8PathBuf::from_path_buf(entry.unwrap().path()).unwrap();
            let rel = path.strip_prefix(root).unwrap().to_owned();
            if fs::symlink_metadata(&path).unwrap().is_dir() {
                out.insert(rel, None);
                stack.push(path);
            } else {
                out.insert(rel, Some(fs::read(&path).unwrap()));
            }
        }
    }
    out
}

/// Asserts that no action target or verify problem in `report` contains `root` or starts with `/`.
fn assert_no_host_path(report: &RepairReport, root: &Utf8Path) {
    let root_str = root.as_str();
    for action in &report.actions {
        assert!(
            !action.target.contains(root_str) && !action.target.starts_with('/'),
            "action target {:?} carries a host path (root: {root_str:?})",
            action.target
        );
    }
    for problem in &report.after.problems {
        let text = problem.to_string();
        assert!(
            !text.contains(root_str),
            "verify problem {text:?} carries a host path (root: {root_str:?})"
        );
    }
}

/// Writes `data` over `path` at `offset`.
fn write_at(path: impl AsRef<std::path::Path>, offset: u64, data: &[u8]) {
    let file = fs::OpenOptions::new().write(true).open(path).unwrap();
    file.write_all_at(data, offset).unwrap();
}

/// Appends `data` to `path`.
fn append_bytes(path: impl AsRef<std::path::Path>, data: &[u8]) {
    let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
    std::io::Write::write_all(&mut file, data).unwrap();
}

/// A damaged object is quarantined under `quarantine/objects/<64 hex>`, freeing its slot in
/// `objects/`, and a `put_blob` of the same bytes restores it so that a deep verify is clean.
#[test]
fn damaged_object_is_quarantined_and_restored_by_put_blob() {
    for format in FORMATS {
        let (_dir, root, repo, _ids) = build_repo(format, 3);
        let layout = Layout::with_format(&root, format);
        let blob_bytes = b"constant\n";
        let blob_id = repo.put_blob(blob_bytes.to_vec()).unwrap().id.0;
        let object_path = layout.object_path(blob_id);
        assert!(object_path.is_file());

        let corrupt_bytes = b"damaged object file bytes";
        fs::write(&object_path, corrupt_bytes).unwrap();
        let before_dry = tree_snapshot(&root);

        let dry = repo.repair(RepairRequest::new(true)).unwrap();
        assert_eq!(
            dry.actions,
            [RepairAction::new(
                RepairKind::QuarantineObject,
                blob_id.to_hex(),
                false
            )]
        );
        assert_eq!(tree_snapshot(&root), before_dry, "dry_run wrote to store");
        assert_no_host_path(&dry, &root);

        let report = repo.repair(RepairRequest::new(false)).unwrap();
        assert_eq!(
            report.actions,
            [RepairAction::new(
                RepairKind::QuarantineObject,
                blob_id.to_hex(),
                true
            )]
        );
        assert!(!object_path.exists(), "damaged object was not moved");
        let quarantined = root.join("quarantine/objects").join(blob_id.to_hex());
        assert_eq!(fs::read(&quarantined).unwrap(), corrupt_bytes);
        assert_no_host_path(&report, &root);

        // Restoring the blob with put_blob of the same bytes writes it afresh, and deep verify is
        // then clean.
        let put_out = repo.put_blob(blob_bytes.to_vec()).unwrap();
        assert!(put_out.written, "quarantined slot was not freed");
        let verified = repo.verify(Depth::Deep).unwrap();
        assert!(verified.problems.is_empty(), "{:?}", verified.problems);

        let second = repo.repair(RepairRequest::new(false)).unwrap();
        assert!(second.actions.is_empty(), "{second:?}");
        assert_eq!(second.after.found, 0);
    }
}

/// Damaged derived values (both a corrupt file on disk and a mismatched value with a valid
/// trailer) and damaged or dangling external id entries are dropped by `repair`.
#[test]
fn damaged_derived_and_external_id_entries_are_dropped() {
    for format in FORMATS {
        let (_dir, root, repo, ids) = build_repo(format, 4);
        let layout = Layout::with_format(&root, format);
        let head = ids[3];

        // Populate built-in derived values, then corrupt one and craft a mismatched one.
        let path_a = RepoPath::new("a.txt".to_owned()).unwrap();
        let _ = repo.derived::<FirstParentSkipList>(head, &()).unwrap();
        let _ = repo
            .derived::<LastModified>(head, &PathKey::new(path_a.clone()))
            .unwrap();

        // 1. Corrupt the skip-list file of ids[3] on disk.
        let skip_slot = DerivedSlot::new(
            FirstParentSkipList::NAME,
            FirstParentSkipList::VERSION,
            head.0,
            &[],
        );
        let skip_path = layout.derived_path(&skip_slot);
        fs::write(&skip_path, b"corrupt derived bytes").unwrap();

        // 2. Plant a valid-trailer but wrong skip-list value on ids[2].
        let mut wrong_bytes = vec![1u8];
        wrong_bytes.extend_from_slice(&ids[0].0.0);
        let wrong_slot = DerivedSlot::new(
            FirstParentSkipList::NAME,
            FirstParentSkipList::VERSION,
            ids[2].0,
            &[],
        );
        let wrong_path = layout.derived_path(&wrong_slot);
        let _ = fs::remove_file(&wrong_path);
        repo.derived_store().put(&wrong_slot, &wrong_bytes).unwrap();

        // 3. Plant a valid-trailer but wrong last_modified value on ids[1].
        let key_bytes = postcard::to_allocvec(&PathKey::new(path_a)).unwrap();
        let wrong_touch = LastTouch::new(None);
        let touch_bytes = postcard::to_allocvec(&wrong_touch).unwrap();
        let touch_slot = DerivedSlot::new(
            LastModified::NAME,
            LastModified::VERSION,
            ids[1].0,
            &key_bytes,
        );
        let touch_path = layout.derived_path(&touch_slot);
        let _ = fs::remove_file(&touch_path);
        repo.derived_store().put(&touch_slot, &touch_bytes).unwrap();

        // 4. Plant a dangling external id (naming no indexed changeset) and a corrupt external id.
        let dangling_key = vec![0xaa; 20];
        let corrupt_key = vec![0xbb; 20];
        let valid_key = vec![0xcc; 20];
        let missing_cs = ChangesetId::new(ObjectId::new([0x99; 32]));
        repo.record_external_ids(
            SYSTEM_GIT,
            &[
                (dangling_key.clone(), ExternalEntry::new(missing_cs, true)),
                (corrupt_key.clone(), ExternalEntry::new(head, true)),
                (valid_key.clone(), ExternalEntry::new(head, true)),
            ],
        )
        .unwrap();
        let corrupt_ext_path = layout.external_id_path(SYSTEM_GIT, &corrupt_key);
        let dangling_ext_path = layout.external_id_path(SYSTEM_GIT, &dangling_key);
        let valid_ext_path = layout.external_id_path(SYSTEM_GIT, &valid_key);
        fs::write(&corrupt_ext_path, b"not an SXID entry").unwrap();

        let before_dry = tree_snapshot(&root);
        let dry = repo.repair(RepairRequest::new(true)).unwrap();
        assert_eq!(tree_snapshot(&root), before_dry, "dry_run wrote to store");
        assert!(dry.actions.iter().all(|a| !a.done));
        assert_no_host_path(&dry, &root);

        let report = repo.repair(RepairRequest::new(false)).unwrap();
        let expected_actions: Vec<RepairAction> = dry
            .actions
            .iter()
            .map(|a| RepairAction::new(a.kind, a.target.clone(), true))
            .collect();
        assert_eq!(report.actions, expected_actions);
        assert_eq!(report.after.found, 0, "{:?}", report.after.problems);
        assert!(!skip_path.exists());
        assert!(!wrong_path.exists());
        assert!(!touch_path.exists());
        assert!(!corrupt_ext_path.exists());
        assert!(!dangling_ext_path.exists());
        assert!(valid_ext_path.exists(), "valid external id was removed");
        assert_no_host_path(&report, &root);

        // Re-deriving works cleanly after the damaged cache files were dropped.
        let rederived = repo.derived::<FirstParentSkipList>(head, &()).unwrap();
        assert!(!rederived.is_empty());

        let second = repo.repair(RepairRequest::new(false)).unwrap();
        assert!(second.actions.is_empty(), "{second:?}");
    }
}

/// The memos a git export writes under a memo key (`git_oid_mac`, 54 bytes, and
/// `git_tree_oid_mac`, 52, each under the key's 16-byte id) are dropped when their length or
/// their slot key is not theirs, whole files with a right trailer: a 53-byte `git_oid_mac`, a
/// 52-byte one, and a `git_tree_oid_mac` under a 15-byte key; a valid one of each is kept.
#[test]
fn repair_drops_a_mac_memo_of_the_wrong_length() {
    let (_dir, root, repo, ids) = build_repo(StoreFormat::V2, 2);
    let layout = Layout::with_format(&root, StoreFormat::V2);
    let derived = repo.derived_store();
    let key_id = [7_u8; 16];
    let placed = |name: &str, subject: ObjectId, key: &[u8], value: Vec<u8>| {
        let slot = DerivedSlot::new(name, 1, subject, key);
        assert!(derived.put(&slot, &value).unwrap(), "{name}: placed");
        layout.derived_path(&slot)
    };
    let mut commit = vec![0_u8; 54];
    commit[20] = 1;
    let valid_commit = placed("git_oid_mac", ids[0].0, &key_id, commit.clone());
    let short = placed("git_oid_mac", ids[1].0, &key_id, commit[..53].to_vec());
    let tree_sized = placed("git_oid_mac", ObjectId::new([1; 32]), &key_id, vec![0; 52]);
    let valid_tree = placed(
        "git_tree_oid_mac",
        ObjectId::new([2; 32]),
        &key_id,
        vec![0; 52],
    );
    let short_key = placed(
        "git_tree_oid_mac",
        ObjectId::new([3; 32]),
        &[7; 15],
        vec![0; 52],
    );

    let report = repo.repair(RepairRequest::new(false)).unwrap();
    assert_no_host_path(&report, &root);
    assert!(valid_commit.exists(), "a valid git_oid_mac was dropped");
    assert!(valid_tree.exists(), "a valid git_tree_oid_mac was dropped");
    for dropped in [short, tree_sized, short_key] {
        assert!(!dropped.exists(), "{dropped} was kept");
    }
}

/// Index, sorted view, generation and checkpoint damage are repaired by `rebuild_index`, and a
/// second repair does nothing.
#[test]
fn index_view_and_checkpoint_damage_trigger_rebuild_index() {
    for format in FORMATS {
        let (_dir, root, repo, ids) = build_repo(format, 5);
        let layout = Layout::with_format(&root, format);

        // Corrupt a generation in gen.col and a record id in ids.col.
        write_at(
            layout.meta_path("idx/gen.col"),
            2 * 8,
            &999_u64.to_le_bytes(),
        );
        write_at(layout.meta_path("idx/ids.col"), 3 * 32, &[0xff; 32]);

        let before_dry = tree_snapshot(&root);
        let dry = repo.repair(RepairRequest::new(true)).unwrap();
        assert_eq!(
            dry.actions,
            [RepairAction::new(
                RepairKind::RebuildIndex,
                "changeset_index",
                false
            )]
        );
        assert_eq!(tree_snapshot(&root), before_dry);

        let report = repo.repair(RepairRequest::new(false)).unwrap();
        assert_eq!(
            report.actions,
            [RepairAction::new(
                RepairKind::RebuildIndex,
                "changeset_index",
                true
            )]
        );
        assert_eq!(report.after.found, 0, "{:?}", report.after.problems);
        for &id in &ids {
            assert!(repo.changeset_meta(id).unwrap().is_some());
        }

        // Corrupt the checkpoint file and verify rebuild_index heals it too.
        fs::write(layout.journal_ckpt(), b"badckpt").unwrap();
        let report2 = repo.repair(RepairRequest::new(false)).unwrap();
        assert_eq!(
            report2.actions,
            [RepairAction::new(
                RepairKind::RebuildIndex,
                "changeset_index",
                true
            )]
        );
        assert_eq!(report2.after.found, 0, "{:?}", report2.after.problems);

        let again = repo.repair(RepairRequest::new(false)).unwrap();
        assert!(again.actions.is_empty(), "{again:?}");
    }
}

/// Corrupt, missing and dangling ref files are rebuilt from the journal with their versions
/// preserved.
#[test]
fn damaged_and_missing_refs_are_rebuilt_from_the_journal() {
    for format in FORMATS {
        let (_dir, root, repo, ids) = build_repo(format, 4);
        let layout = Layout::with_format(&root, format);
        let before_refs = repo.list_refs().unwrap();

        // Corrupt or remove all ref files in refs_dir.
        for entry in fs::read_dir(layout.refs_dir()).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                fs::remove_dir_all(&path).unwrap();
            } else {
                fs::write(&path, b"corrupt ref file\n").unwrap();
            }
        }

        let before_dry = tree_snapshot(&root);
        let dry = repo.repair(RepairRequest::new(true)).unwrap();
        assert_eq!(
            dry.actions,
            [
                RepairAction::new(RepairKind::RebuildRef, "main", false),
                RepairAction::new(RepairKind::RebuildRef, "release/1", false),
            ]
        );
        assert_eq!(tree_snapshot(&root), before_dry);

        let report = repo.repair(RepairRequest::new(false)).unwrap();
        assert_eq!(
            report.actions,
            [
                RepairAction::new(RepairKind::RebuildRef, "main", true),
                RepairAction::new(RepairKind::RebuildRef, "release/1", true),
            ]
        );
        assert_eq!(report.after.found, 0, "{:?}", report.after.problems);
        assert_eq!(repo.list_refs().unwrap(), before_refs);
        assert_eq!(repo.get_ref("main").unwrap().unwrap().target, ids[3]);

        let again = repo.repair(RepairRequest::new(false)).unwrap();
        assert!(again.actions.is_empty(), "{again:?}");
    }
}

/// Damaged `journal.off` (> 512 zeroed entries, crafted entry, deep zeroed entry) is rebuilt by
/// `rebuild_offsets`, while mid-log damage in `journal.log` fails with `StoreError::JournalCorrupt`
/// and leaves `journal.log` byte-identical.
#[test]
fn journal_offsets_are_rebuilt_and_mid_log_damage_is_refused() {
    for format in FORMATS {
        let (_dir, root, _repo, ids) = build_repo(format, 5);
        let layout = Layout::with_format(&root, format);
        let entry_len: u64 = match format {
            StoreFormat::V1 => 8,
            _ => 16,
        };
        let good_off = fs::read(layout.journal_off()).unwrap();
        let good_log = fs::read(layout.journal_log()).unwrap();

        // 1. > 512 zeroed entries appended to journal.off.
        let zeros = usize::try_from((MAX_ZEROED + 32) * entry_len).unwrap();
        append_bytes(layout.journal_off(), &vec![0; zeros]);
        let repo = LocalRepo::open(&root).unwrap();
        let before_dry = tree_snapshot(&root);
        let dry = repo.repair(RepairRequest::new(true)).unwrap();
        assert_eq!(
            dry.actions,
            [RepairAction::new(
                RepairKind::RebuildOffsets,
                "journal_offsets",
                false
            )]
        );
        assert_eq!(tree_snapshot(&root), before_dry);

        let report = repo.repair(RepairRequest::new(false)).unwrap();
        assert_eq!(
            report.actions,
            [RepairAction::new(
                RepairKind::RebuildOffsets,
                "journal_offsets",
                true
            )]
        );
        assert_eq!(report.after.found, 0, "{:?}", report.after.problems);
        assert_eq!(fs::read(layout.journal_off()).unwrap(), good_off);
        assert_eq!(fs::read(layout.journal_log()).unwrap(), good_log);

        // 2. A deep zeroed entry in journal.off.
        write_at(
            layout.journal_off(),
            2 * entry_len,
            &vec![0; usize::try_from(entry_len).unwrap()],
        );
        let repo = LocalRepo::open(&root).unwrap();
        let report2 = repo.repair(RepairRequest::new(false)).unwrap();
        assert_eq!(
            report2.actions,
            [RepairAction::new(
                RepairKind::RebuildOffsets,
                "journal_offsets",
                true
            )]
        );
        assert_eq!(report2.after.found, 0);
        assert_eq!(fs::read(layout.journal_off()).unwrap(), good_off);
        let reopened = LocalRepo::open(&root).unwrap();
        assert_eq!(reopened.get_ref("main").unwrap().unwrap().target, ids[4]);

        // 3. Mid-journal damage in frame 1's payload -> refused, journal.log byte-identical.
        let frame1_start = u64::from_le_bytes(
            good_off[usize::try_from(entry_len).unwrap()..usize::try_from(entry_len).unwrap() + 8]
                .try_into()
                .unwrap(),
        );
        let header_len: u64 = match format {
            StoreFormat::V1 => 8,
            _ => 24,
        };
        let flip_at = frame1_start + header_len + 1;
        let orig_byte = good_log[usize::try_from(flip_at).unwrap()];
        write_at(layout.journal_log(), flip_at, &[orig_byte ^ 0xff]);
        let damaged_log = fs::read(layout.journal_log()).unwrap();

        let repo = LocalRepo::open(&root).unwrap();
        let dry_err = repo.repair(RepairRequest::new(true)).unwrap_err();
        assert!(
            matches!(
                dry_err,
                RepoError::Store(StoreError::JournalCorrupt { offset }) if offset == frame1_start
            ),
            "{dry_err:?}"
        );
        assert_eq!(fs::read(layout.journal_log()).unwrap(), damaged_log);

        let err = repo.repair(RepairRequest::new(false)).unwrap_err();
        assert!(
            matches!(
                err,
                RepoError::Store(StoreError::JournalCorrupt { offset }) if offset == frame1_start
            ),
            "{err:?}"
        );
        assert_eq!(fs::read(layout.journal_log()).unwrap(), damaged_log);
    }
}

/// A held maintenance lease refuses `repair` at once with `StoreError::MaintenanceBusy`, without
/// waiting or writing anything.
#[test]
fn held_maintenance_lease_refuses_repair_at_once() {
    let (_dir, root, repo, _ids) = build_repo(StoreFormat::V2, 2);
    let layout = Layout::with_format(&root, StoreFormat::V2);
    let held = MaintLease::try_acquire(&layout).unwrap();
    let before = tree_snapshot(&root);

    let start = Instant::now();
    let err = repo.repair(RepairRequest::new(false)).unwrap_err();
    assert!(
        matches!(err, RepoError::Store(StoreError::MaintenanceBusy)),
        "{err:?}"
    );
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "repair waited on lease"
    );
    assert_eq!(tree_snapshot(&root), before);

    drop(held);
    let ok = repo.repair(RepairRequest::new(false)).unwrap();
    assert_eq!(ok.actions, []);
}

/// Repair of a FORMAT 3 store (the FORMAT 3 design §2.3, §2.5, §2.8): the runs and the fact rows
/// rebuilt from the journal, and a lost `journal3.ckpt`'s base seq found again in the journal. The
/// store is made FORMAT 2, then turned FORMAT 3 by hand as an upgrade does (no `init` makes one
/// yet); its fact kinds are enabled with the test hooks' `supported_for_tests`.
#[cfg(feature = "test-hooks")]
mod format_3 {
    use std::sync::Arc;

    use swarmops_store::{
        Expect, Fact, FactKind, Guard, Journal, MetaStore, RealFs, Record, Step,
        supported_for_tests,
    };

    use super::*;
    use crate::verify::crc32;

    /// The bytes of a `journal3.ckpt` of `fields`: offset, seq, base seq, records, fact rows.
    fn ckpt3(fields: [u64; 5]) -> Vec<u8> {
        let mut bytes: Vec<u8> = fields.iter().flat_map(|f| f.to_le_bytes()).collect();
        bytes.extend(crc32(&bytes).to_le_bytes());
        bytes
    }

    /// A FORMAT 2 repository of `count` commits turned FORMAT 3 by hand, its base seq the frames
    /// it had (returned), the lineage feature supported while the guard lives.
    fn v3_repo(count: usize) -> (TempDir, Utf8PathBuf, LocalRepo, u64, Guard) {
        let guard = supported_for_tests(&["lineage"]);
        let (dir, root, repo, _ids) = build_repo(StoreFormat::V2, count);
        drop(repo);
        let v2 = Layout::with_format(&root, StoreFormat::V2);
        let base = (Journal::open(&v2, Arc::new(RealFs::new())).unwrap())
            .frame_count()
            .unwrap();

        // FORMAT, journal3.ckpt, the manifest of no run, facts.col: what an upgrade writes.
        let v3 = Layout::with_format(&root, StoreFormat::V3);
        fs::write(v3.format_file(), b"swarmops-store 3\n").unwrap();
        fs::write(v3.journal_ckpt(), ckpt3([0, 0, base, 0, 0])).unwrap();
        fs::create_dir_all(v3.views_dir().join("tmp")).unwrap();
        let mut manifest = b"SWMF\x01\0\0\0".to_vec();
        manifest.extend([0; 12]);
        manifest.extend(crc32(&manifest).to_le_bytes());
        fs::write(v3.views_dir().join("MANIFEST"), manifest).unwrap();
        fs::write(v3.facts_col(), b"").unwrap();
        let repo = LocalRepo::open(&root).unwrap();
        (dir, root, repo, base, guard)
    }

    /// A lineage fact of changeset subject `n`, its lineage `n + 1` (no blob).
    fn lineage(n: u64) -> Fact {
        let mut subject = [0x5a; 32];
        subject[..8].copy_from_slice(&n.to_le_bytes());
        let mut other = subject;
        other[31] = 0xa5;
        Fact::new(
            FactKind::Lineage,
            1,
            subject,
            other,
            7,
            ACTOR.to_owned(),
            vec![3; 32],
        )
        .unwrap()
    }

    /// Commits the facts `range` names, 550 to a transaction, through the store at `root`.
    fn put_lineages(root: &Utf8Path, range: std::ops::Range<u64>) {
        let layout = Layout::with_format(root, StoreFormat::V3);
        let store = MetaStore::open_in(layout, Arc::new(RealFs::new())).unwrap();
        let ns: Vec<u64> = range.collect();
        for chunk in ns.chunks(550) {
            let mut write = store.write().unwrap();
            for &n in chunk {
                write.put_fact(lineage(n), Expect::Any).unwrap();
            }
            write.commit().unwrap();
        }
    }

    /// A fact run removed and a fact row damaged: verify reports both; repair rebuilds the
    /// index, and the next step the run, as the journal says.
    #[test]
    fn repair_v3_rebuilds_runs_and_facts() {
        let (_dir, root, repo, _base, _guard) = v3_repo(2);
        put_lineages(&root, 0..1_100);
        repo.flush_metadata().unwrap();
        assert_eq!(repo.step_views().unwrap(), Step::Done);
        let layout = Layout::with_format(&root, StoreFormat::V3);
        let run = layout
            .views_dir()
            .join(format!("f-{:016x}-{:016x}.run", 0, 1_100));
        let (run_bytes, rows) = (
            fs::read(&run).unwrap(),
            fs::read(layout.facts_col()).unwrap(),
        );
        assert_eq!(repo.verify(Depth::Deep).unwrap().problems, []);

        // 1. The damage, which verify sees.
        fs::remove_file(&run).unwrap();
        write_at(layout.facts_col(), 24 * 10, &[0xee; 8]);
        assert_ne!(repo.verify(Depth::Deep).unwrap().found, 0);

        // 2. The repair: the index rebuilt, then the run.
        let report = repo.repair(RepairRequest::new(false)).unwrap();
        assert_eq!(
            report.actions,
            [RepairAction::new(
                RepairKind::RebuildIndex,
                "changeset_index",
                true
            )]
        );
        assert_eq!(report.after.found, 0, "{:?}", report.after.problems);
        assert_eq!(fs::read(layout.facts_col()).unwrap(), rows);
        assert_eq!(fs::read(&run).unwrap(), run_bytes);
        assert_eq!(repo.repair(RepairRequest::new(false)).unwrap().actions, []);
    }

    /// A dry run of a FORMAT 3 store whose `journal3.ckpt` holds no checkpoint writes nothing
    /// and lists what the repair then does, no action more: the checkpoint the rebuild of the
    /// offsets writes again is the one the rest is judged by, in the stage as in the store.
    #[test]
    fn format_3_repair_dry_run_writes_nothing() {
        // 1. Commits before and after the base, flushed; then the checkpoint damaged.
        let (_dir, root, repo, _base, _guard) = v3_repo(3);
        commit(&repo, &[], vec![put("c.txt", b"three\n")]);
        repo.flush_metadata().unwrap();
        let layout = Layout::with_format(&root, StoreFormat::V3);
        assert_eq!(repo.verify(Depth::Deep).unwrap().problems, []);
        fs::write(layout.journal_ckpt(), b"no checkpoint").unwrap();

        // 2. The dry run: not a path or a byte changed.
        let before_dry = tree_snapshot(&root);
        let dry = repo.repair(RepairRequest::new(true)).unwrap();
        assert_eq!(tree_snapshot(&root), before_dry, "dry_run wrote to store");
        assert!(dry.actions.iter().all(|action| !action.done));
        assert_no_host_path(&dry, &root);

        // 3. The repair does what the dry run listed, and the store is whole.
        let expected: Vec<RepairAction> = (dry.actions.iter())
            .map(|action| RepairAction::new(action.kind, action.target.clone(), true))
            .collect();
        let report = repo.repair(RepairRequest::new(false)).unwrap();
        assert_eq!(report.actions, expected);
        assert_eq!(
            report.actions,
            [RepairAction::new(
                RepairKind::RebuildOffsets,
                "journal_offsets",
                true
            )]
        );
        assert_eq!(report.after.found, 0, "{:?}", report.after.problems);
        assert_eq!(repo.repair(RepairRequest::new(false)).unwrap().actions, []);
    }

    /// The bytes of a frame of `records` with no nonce, a FORMAT 2 frame, at seq `seq` after a
    /// frame of payload CRC `prev_crc`.
    fn frame_without_nonce(records: &[Record], seq: u64, prev_crc: u32) -> Vec<u8> {
        let mut payload = u16::try_from(records.len()).unwrap().to_le_bytes().to_vec();
        for record in records {
            record.encode(&mut payload).unwrap();
        }
        let mut frame = u32::try_from(payload.len()).unwrap().to_le_bytes().to_vec();
        frame.extend(crc32(&payload).to_le_bytes());
        frame.extend(seq.to_le_bytes());
        frame.extend(prev_crc.to_le_bytes());
        let hcrc = crc32(&frame);
        frame.extend(hcrc.to_le_bytes());
        frame.extend(payload);
        frame
    }

    /// A lost `journal3.ckpt`: verify names it `base_seq_lost`; repair takes the first frame that
    /// ends in a nonce as the base, every frame after it ending in one, and writes the checkpoint
    /// again. A frame without a nonce past that base is refused, and nothing is written.
    #[test]
    fn repair_recovers_base_seq_from_the_journal() {
        // 1. Three FORMAT 2 frames' commits, then two of FORMAT 3.
        let (_dir, root, repo, base, _guard) = v3_repo(3);
        assert!(base > 0);
        put_lineages(&root, 0..2);
        let more = commit(&repo, &[], vec![put("c.txt", b"three\n")]);
        repo.flush_metadata().unwrap();
        let layout = Layout::with_format(&root, StoreFormat::V3);
        let before = fs::read(layout.journal_ckpt()).unwrap();
        assert_eq!(before[16..24], base.to_le_bytes());

        // 2. The checkpoint lost: verify names it.
        fs::remove_file(layout.journal_ckpt()).unwrap();
        let found = repo.verify(Depth::Shallow).unwrap();
        assert!(
            (found.problems.iter()).any(|p| p
                .to_string()
                .starts_with("damaged metadata: base_seq_lost:")),
            "{:?}",
            found.problems
        );

        // 3. The repair: the base found again, the store whole.
        let report = repo.repair(RepairRequest::new(false)).unwrap();
        assert_eq!(report.after.found, 0, "{:?}", report.after.problems);
        let after = fs::read(layout.journal_ckpt()).unwrap();
        assert_eq!(after[16..24], base.to_le_bytes());
        assert!(repo.changeset_meta(more).unwrap().is_some());

        // 4. A frame without its nonce past the base, the checkpoint lost again: refused.
        let journal = Journal::open(&layout, Arc::new(RealFs::new())).unwrap();
        let last = journal.frames_from(0, 1_000).unwrap().pop().unwrap();
        drop(journal);
        let at = fs::metadata(layout.journal_log()).unwrap().len();
        let record = Record::RefMove {
            name: "stray".to_owned(),
            old: None,
            new: Some(more),
            actor: ACTOR.to_owned(),
            ts: 1_700_000_000,
        };
        append_bytes(
            layout.journal_log(),
            &frame_without_nonce(&[record], last.seq + 1, last.crc),
        );
        fs::remove_file(layout.journal_ckpt()).unwrap();
        let untouched = tree_snapshot(&root);
        let refused = repo.repair(RepairRequest::new(false));
        assert!(
            matches!(refused, Err(RepoError::Store(StoreError::JournalCorrupt { offset })) if offset == at),
            "{refused:?}"
        );
        let mut now = tree_snapshot(&root);
        now.remove(Utf8Path::new("meta/maint.lock"));
        let mut was = untouched;
        was.remove(Utf8Path::new("meta/maint.lock"));
        assert_eq!(now, was);
    }
}
