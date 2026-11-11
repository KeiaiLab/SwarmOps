//! The process counters at the store's file boundary (the M21 design §2.1): object reads and
//! writes, journal writes and syncs, counted where the store makes the call.
//!
//! The counters are the process's: each test runs in a process of its own (nextest).

use swarmops_core::{ChangesetId, ObjectKind, hash_object};
use swarmops_store::fs_ops::counters::{Counters, Counts};
use swarmops_store::{ChangesetMeta, FsObjectStore, MetaStore, MetaWrite, ObjectStore, RealFs};
use swarmops_testkit::store::new_root;

const ACTOR: &str = "test/counters";

/// The process's counts now.
fn now() -> Counts {
    Counters::process().snapshot()
}

/// Inserts changeset `name` and moves `main` from `old` to it.
fn advance(write: &mut MetaWrite<'_>, name: &[u8], old: Option<ChangesetId>) -> ChangesetId {
    let id = ChangesetId::new(hash_object(ObjectKind::Changeset, name));
    let delta = hash_object(ObjectKind::Blob, name).0;
    write
        .insert_changeset(ChangesetMeta::new(id, 0, 0, delta, Vec::new()))
        .unwrap();
    write.cas_ref("main", old, Some(id), ACTOR).unwrap();
    id
}

/// A put and a get move only the object counts; a commit's frame only the journal counts.
#[test]
fn counts_split_object_and_journal_calls() {
    let (_dir, layout) = new_root();
    let store = MetaStore::create(layout.root()).unwrap();
    let objects = FsObjectStore::open(layout.clone(), Box::new(RealFs::new()));

    // 1. One new object, flushed, then read back.
    let before = now();
    let id = objects.put(ObjectKind::Blob, b"counted").unwrap();
    objects.flush().unwrap();
    assert!(objects.get(id).unwrap().is_some());
    let moved = now().since(&before);
    assert_eq!(moved.object_writes, 1, "{moved:?}");
    assert_eq!(moved.object_reads, 1, "{moved:?}");
    assert_eq!(moved.journal_writes, 0, "{moved:?}");
    assert_eq!(moved.journal_syncs, 0, "{moved:?}");

    // 2. One committed frame.
    let before = now();
    let mut write = store.write().unwrap();
    advance(&mut write, b"one", None);
    write.commit().unwrap();
    let moved = now().since(&before);
    assert!(moved.journal_writes > 0, "{moved:?}");
    assert!(moved.journal_syncs > 0, "{moved:?}");
    assert_eq!(moved.object_writes, 0, "{moved:?}");
    assert_eq!(moved.object_reads, 0, "{moved:?}");
}

/// `since` is the difference of each field; nothing moved is all zeros.
#[test]
fn counts_since_subtracts() {
    let (_dir, layout) = new_root();
    let objects = FsObjectStore::open(layout, Box::new(RealFs::new()));
    let first = now();
    objects.put(ObjectKind::Blob, b"since").unwrap();
    let second = now();
    assert_eq!(second.since(&second), Counts::default());
    let moved = second.since(&first);
    assert_eq!(
        moved.object_writes,
        second.object_writes - first.object_writes
    );
    assert_eq!(moved.object_writes, 1);
    assert_eq!(moved.object_reads, second.object_reads - first.object_reads);
    assert_eq!(first.since(&second), Counts::default(), "never below zero");
}
