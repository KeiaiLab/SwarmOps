//! The names a store makes (storage-core design, §2.1 P8): a new ref name that another platform
//! could not hold as a file is refused on every platform, while a name the journal holds already
//! reads and moves as before; and a derived value's name or an external id system's that Windows
//! takes for a device is refused too.

use std::fs;
use std::sync::Arc;
use std::time::Duration;

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_core::{ChangesetId, ObjectKind, hash_object};
use swarmops_store::{
    CasResult, ChangesetMeta, DerivedSlot, DerivedStore, ExternalIds, Journal, Layout, MetaLock,
    MetaStore, RealFs, Record, StoreError,
};
use swarmops_testkit::faults::checked;
use swarmops_testkit::repo::{commit, new_repo, put};
use swarmops_testkit::store::temp_dir;
use tempfile::TempDir;

const ACTOR: &str = "test/1";

/// What the refusal of a name that differs from another ref's only in case says.
const CASE: &str = "only in case";

/// What the refusal of a Windows device name says.
const DEVICE: &str = "a Windows device name";

/// What the refusal of a name that ends in a dot says.
const DOT: &str = "end in a dot";

/// A fresh store, its writes checked against the store's ordering rules ([`checked`]); its
/// directory lives as long as the `TempDir`.
fn new_store() -> (TempDir, Utf8PathBuf, MetaStore) {
    let (dir, root) = temp_dir();
    let store = MetaStore::create_with_fs(&root, checked(RealFs)).unwrap();
    (dir, root, store)
}

/// The id of changeset `n`: a real hash, so the ids look like a store's.
fn id(n: u64) -> ChangesetId {
    ChangesetId(hash_object(ObjectKind::Changeset, &n.to_le_bytes()))
}

/// Changeset `n`, a root, committed in a transaction of its own; its id.
fn committed(store: &MetaStore, n: u64) -> ChangesetId {
    let mut write = store.write().unwrap();
    let meta = ChangesetMeta {
        id: id(n),
        seq: 0,
        generation: 0,
        delta_hash: hash_object(ObjectKind::Blob, &n.to_le_bytes()).0,
        parents: Vec::new(),
    };
    write.insert_changeset(meta).unwrap();
    write.commit().unwrap();
    id(n)
}

/// Ref `name` moved from `from` to `to` (`None` creates or deletes it), in a transaction of its
/// own.
#[track_caller]
fn moved(store: &MetaStore, name: &str, from: Option<ChangesetId>, to: Option<ChangesetId>) {
    let mut write = store.write().unwrap();
    let applied = write.cas_ref(name, from, to, ACTOR).unwrap();
    assert_eq!(applied, CasResult::Applied { old: from }, "{name}");
    write.commit().unwrap();
}

/// The names of the refs a snapshot of `store` lists, in order.
fn ref_names(store: &MetaStore) -> Vec<String> {
    let refs = store.read().unwrap().list_refs().unwrap();
    refs.into_iter().map(|(name, _)| name).collect()
}

/// That a move making ref `name` at `target` is refused, as [`StoreError::RefName`] of `name`
/// with a message that says `rule`, and queues nothing.
#[track_caller]
fn assert_refused(store: &MetaStore, name: &str, target: ChangesetId, rule: &str) {
    let mut write = store.write().unwrap();
    match write.cas_ref(name, None, Some(target), ACTOR) {
        Err(error @ StoreError::RefName(_)) => {
            let message = error.to_string();
            assert!(matches!(&error, StoreError::RefName(refused) if refused == name));
            assert!(message.contains(rule), "{name}: {message}");
        }
        other => panic!("{name}: expected a refusal, got {other:?}"),
    }
    assert!(write.queued().is_empty(), "{name}: nothing is queued");
}

/// Appends a frame of `records` to the journal of the store at `root` under the lock, and stops
/// there: what a writer that died right after its commit point leaves (no ref files, the
/// checkpoint where it was), so that the next writer's replay makes their files.
fn append_unmaterialized(root: &Utf8Path, records: &[Record]) {
    let layout = Layout::new(root);
    let _lock = MetaLock::acquire(&layout, Duration::from_secs(10)).unwrap();
    let journal = Journal::open(&layout, Arc::new(RealFs)).unwrap();
    journal.truncate_torn_tail().unwrap();
    journal.append_frame(records).unwrap();
}

/// Whether the volume temp dirs are made on tells names apart by case: a file made as `probe` is
/// not found as `PROBE`.
fn case_sensitive() -> bool {
    let (_dir, dir) = temp_dir();
    fs::write(dir.join("probe"), b"").unwrap();
    !dir.join("PROBE").exists()
}

/// A new ref may not differ from another only in ASCII case: on a case-insensitive volume (APFS
/// by default, NTFS) `release/Main` and `release/main` would be one file. The names compared are
/// those of the refs the store holds, read under the lock, and those of the refs the transaction
/// made before; nothing is queued, and a name that differs in more than case is taken. A move
/// from nothing to nothing makes no file, but it is recorded, and its materialization would
/// remove the file a case-insensitive volume finds under the name: it is refused too.
#[test]
fn a_ref_that_differs_from_another_only_in_case_is_refused() {
    let (_dir, _root, store) = new_store();
    let c = committed(&store, 1);
    moved(&store, "release/main", None, Some(c));
    for name in ["release/Main", "release/MAIN", "release/mAiN"] {
        assert_refused(&store, name, c, CASE);
    }
    let mut write = store.write().unwrap();
    match write.cas_ref("release/Main", None, None, ACTOR) {
        Err(StoreError::RefName(name)) => assert_eq!(name, "release/Main"),
        other => panic!("expected a refusal of release/Main, got {other:?}"),
    }
    drop(write);

    let mut write = store.write().unwrap();
    let made = write.cas_ref("release/v1", None, Some(c), ACTOR).unwrap();
    assert_eq!(made, CasResult::Applied { old: None });
    match write.cas_ref("release/V1", None, Some(c), ACTOR) {
        Err(StoreError::RefName(name)) => assert_eq!(name, "release/V1"),
        other => panic!("expected a refusal of release/V1, got {other:?}"),
    }
    assert_eq!(write.queued().len(), 1, "release/v1's move alone");
    write.commit().unwrap();

    moved(&store, "release/main-2", None, Some(c));
    assert_eq!(
        ref_names(&store),
        ["release/main", "release/main-2", "release/v1"]
    );
}

/// A new ref may not be a Windows device name, in any case, with or without an extension:
/// Windows opens the device for `meta/refs/release/con`. A name that only starts or ends like one
/// is taken.
#[test]
fn a_ref_named_after_a_windows_device_is_refused() {
    let (_dir, _root, store) = new_store();
    let c = committed(&store, 1);
    let devices = [
        "release/con",
        "release/CON.txt",
        "release/lpt9",
        "release/Nul",
        "release/aux.tar.gz",
        "release/com1",
        "release/prn",
    ];
    for name in devices {
        assert_refused(&store, name, c, DEVICE);
    }

    let names = [
        "release/com0",
        "release/com10",
        "release/console",
        "release/x.con",
    ];
    for name in names {
        moved(&store, name, None, Some(c));
    }
    assert_eq!(ref_names(&store), names);
}

/// A new ref may not end in a dot, which Win32 path handling strips: `release/x.` would be the
/// file of `release/x`. A dot elsewhere is fine.
#[test]
fn a_ref_ending_in_a_dot_is_refused() {
    let (_dir, _root, store) = new_store();
    let c = committed(&store, 1);
    for name in ["release/x.", "release/v1.2.", "release/x.."] {
        assert_refused(&store, name, c, DOT);
    }

    for name in ["release/v1.2", "release/x.y"] {
        moved(&store, name, None, Some(c));
    }
    assert_eq!(ref_names(&store), ["release/v1.2", "release/x.y"]);
}

/// A name that is not `main` or `release/<x>` is refused with a message that says what a ref name
/// is, as before.
#[test]
fn a_ref_name_off_the_grammar_says_what_a_ref_name_is() {
    let (_dir, _root, store) = new_store();
    let c = committed(&store, 1);
    for name in ["feature/x", "release/.x", "release/a b"] {
        assert_refused(&store, name, c, "a ref name is main or release/<x>");
    }
}

/// `op undo` of a ref's deletion makes the name again, so the rule applies to it: once
/// `release/A` exists, the undo of the deletion of `release/a` is refused, and writes nothing.
#[test]
fn op_undo_of_a_delete_refuses_a_name_that_now_collides() {
    let (_dir, _root, repo) = new_repo();
    let c = commit(&repo, &[], vec![put("a.txt", b"a")]);
    let made = |name: &str, expected, new| repo.cas_ref(name, expected, new, ACTOR).unwrap().result;
    assert_eq!(
        made("release/a", None, Some(c)),
        CasResult::Applied { old: None }
    );
    assert_eq!(
        made("release/a", Some(c), None),
        CasResult::Applied { old: Some(c) }
    );
    let (newest, _) = repo.op_log(None, 1).unwrap();
    let deletion = newest[0].seq;
    assert_eq!(
        made("release/A", None, Some(c)),
        CasResult::Applied { old: None }
    );

    let (before, _) = repo.op_log(None, usize::MAX).unwrap();
    let undo = repo.undo(deletion, ACTOR).map(|outcome| outcome.undone);
    assert_eq!(format!("{undo:?}"), r#"Err(RefName("release/a"))"#);
    let (after, _) = repo.op_log(None, usize::MAX).unwrap();
    assert_eq!(after.len(), before.len(), "nothing is written");
    let refs = repo.list_refs().unwrap();
    let names: Vec<&str> = refs.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, ["release/A"]);
}

/// The rule is for new names only. A frame an earlier version committed holds names this one
/// would not make: the Windows device names and the dotted end where the platform holds such a
/// file, and a case variant of `release/main` where the volume tells the two apart. Replay makes
/// their files as it makes any (it never applies the rule), and the refs read, move and go as
/// they did. The frame is left as a writer that died right after its commit point leaves it, so
/// that the next writer's replay makes the files.
#[test]
fn a_ref_the_journal_already_holds_reads_and_moves_as_before() {
    let (_dir, root, store) = new_store();
    let c1 = committed(&store, 1);
    let c2 = committed(&store, 2);
    moved(&store, "release/main", None, Some(c1));
    drop(store);
    let mut held = Vec::new();
    if !cfg!(windows) {
        held.extend([
            "release/CON.txt",
            "release/con",
            "release/lpt9",
            "release/x.",
        ]);
    }
    if case_sensitive() {
        held.push("release/Main");
    }
    let created = |name: &str| Record::RefMove {
        name: name.to_owned(),
        old: None,
        new: Some(c1),
        actor: ACTOR.to_owned(),
        ts: 1_760_000_000,
    };
    let records: Vec<Record> = held.iter().copied().map(created).collect();
    append_unmaterialized(&root, &records);

    let store = MetaStore::open_with_fs(&root, checked(RealFs)).unwrap();
    let read = store.read().unwrap();
    for &name in &held {
        let state = read.get_ref(name).unwrap();
        assert_eq!(state.map(|state| state.target), Some(c1), "{name}");
        assert_eq!(read.reflog(name, 10).unwrap().len(), 1, "{name}");
    }
    drop(read);

    for &name in &held {
        moved(&store, name, Some(c1), Some(c2));
    }
    let mut want = held.clone();
    want.push("release/main");
    want.sort_unstable();
    assert_eq!(ref_names(&store), want);
    for &name in &held {
        moved(&store, name, Some(c2), None);
    }
    assert_eq!(ref_names(&store), ["release/main"]);
}

/// A move or a deletion by a name no ref holds finds no ref, whatever file the volume finds under
/// that name: on a case-insensitive volume, `release/Main` opens the file of `release/main`. It
/// is the mismatch it is on Linux, on every platform, and `release/main` stays as it was.
#[test]
fn a_case_variant_of_a_ref_moves_no_ref() {
    let (_dir, _root, store) = new_store();
    let c1 = committed(&store, 1);
    let c2 = committed(&store, 2);
    moved(&store, "release/main", None, Some(c1));

    let mut write = store.write().unwrap();
    let moving = write.cas_ref("release/Main", Some(c1), Some(c2), ACTOR);
    assert_eq!(moving.unwrap(), CasResult::Mismatch { actual: None });
    let deleting = write.cas_ref("release/MAIN", Some(c1), None, ACTOR);
    assert_eq!(deleting.unwrap(), CasResult::Mismatch { actual: None });
    assert!(write.queued().is_empty(), "nothing is queued");
    drop(write);

    let state = store.read().unwrap().get_ref("release/main").unwrap();
    let state = state.expect("release/main is there");
    assert_eq!((state.target, state.version), (c1, 1));
}

/// Replay brings each ref the frames past the checkpoint move to its last target, every write
/// again. Once `release/a` is deleted and `release/A` made, replaying the deletion removes no
/// file: on a case-insensitive volume the path of `release/a` finds the file of `release/A`,
/// which is not that ref's.
#[test]
fn a_replayed_deletion_leaves_a_case_variant_alone() {
    let (_dir, root, store) = new_store();
    let c = committed(&store, 1);

    // Make, delete, then make the case variant, all past the checkpoint.
    moved(&store, "release/a", None, Some(c));
    moved(&store, "release/a", Some(c), None);
    moved(&store, "release/A", None, Some(c));

    // The next write replays those frames; release/A's file stays.
    drop(store.write().unwrap());
    let file = root.join("meta/refs/release/A");
    assert!(file.exists(), "release/A's file is there");
    let state = store.read().unwrap().get_ref("release/A").unwrap();
    assert_eq!(state.map(|state| state.target), Some(c));
}

/// A derived value's name may not be a Windows device name either: it names a directory under
/// `meta/derived/`. Only code chooses these names; one that only starts like a device's is taken.
#[test]
fn a_derived_value_named_after_a_windows_device_is_refused() {
    let (_dir, root) = temp_dir();
    let derived = DerivedStore::open(Layout::new(&root), Arc::new(RealFs));
    let slot = |name| DerivedSlot {
        name,
        version: 1,
        subject: id(1).0,
        key: b"",
    };
    for name in ["con", "nul", "aux", "prn", "com1", "lpt9"] {
        let put = derived.put(&slot(name), b"value");
        let refused = matches!(&put, Err(StoreError::BadCall(why)) if why.contains(DEVICE));
        assert!(refused, "{name}: {put:?}");
        let got = derived.get(&slot(name));
        assert!(
            matches!(got, Err(StoreError::BadCall(_))),
            "{name}: {got:?}"
        );
        let listed = derived.list(name, 1);
        assert!(
            matches!(listed, Err(StoreError::BadCall(_))),
            "{name}: {listed:?}"
        );
    }

    assert!(derived.put(&slot("console"), b"value").unwrap());
    let got = derived.get(&slot("console")).unwrap();
    assert_eq!(got.as_deref(), Some(&b"value"[..]));
}

/// An external id system's name may not be a Windows device name either: it names a directory
/// under `meta/external_ids/`. A directory so named is no system's, and the list of systems
/// passes over it, where the platform holds one.
#[test]
fn an_external_id_system_named_after_a_windows_device_is_refused() {
    let (_dir, root) = temp_dir();
    let layout = Layout::new(&root);
    let ids = ExternalIds::new(layout.clone(), Arc::new(RealFs));
    for system in ["con", "nul", "com1", "lpt9"] {
        let put = ids.put_batch(system, &[]);
        let refused = matches!(&put, Err(StoreError::BadCall(why)) if why.contains(DEVICE));
        assert!(refused, "{system}: {put:?}");
        let got = ids.get(system, &[0xaa; 20]);
        assert!(
            matches!(got, Err(StoreError::BadCall(_))),
            "{system}: {got:?}"
        );
        let listed = ids.list(system);
        assert!(matches!(listed, Err(StoreError::BadCall(_))), "{system}");
    }

    let mut dirs = vec!["console", "git"];
    if !cfg!(windows) {
        dirs.push("con");
    }
    for dir in dirs {
        fs::create_dir_all(layout.external_ids_dir().join(dir)).unwrap();
    }
    assert_eq!(ids.systems().unwrap(), ["console", "git"]);
}
