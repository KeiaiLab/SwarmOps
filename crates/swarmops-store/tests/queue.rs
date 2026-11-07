//! The write queue through the public API (the M18 D2 design §3.2): a writer that finds the lock
//! free leads its own request at once, making the calls a plain write transaction makes. The
//! protocol among writers is tested in the queue's own module.

use std::sync::Arc;

use swarmops_core::{ChangesetId, ObjectKind, hash_object};
use swarmops_store::{
    ChangesetMeta, FrameUse, MetaStore, MetaWrite, Outcome, Planned, Serve, Turn,
};
use swarmops_testkit::fs::RecordingFs;
use swarmops_testkit::store::new_root;

const ACTOR: &str = "test/queue";

/// Inserts changeset 1 on `main` and moves `main` to it.
fn advance(write: &mut MetaWrite<'_>) -> ChangesetId {
    let id = ChangesetId::new(hash_object(ObjectKind::Changeset, b"one"));
    let delta = hash_object(ObjectKind::Blob, b"one").0;
    write
        .insert_changeset(ChangesetMeta::new(id, 0, 0, delta, Vec::new()))
        .unwrap();
    write.cas_ref("main", None, Some(id), ACTOR).unwrap();
    id
}

/// The calls a store makes for `run`, on a fresh store recorded from its open on.
fn calls_of(run: impl FnOnce(&MetaStore)) -> Vec<String> {
    let (_dir, layout) = new_root();
    MetaStore::create(layout.root()).unwrap();
    let (fs, calls) = RecordingFs::new();
    let mut store = MetaStore::open_with_fs(layout.root(), Arc::new(fs)).unwrap();
    store.set_parallelism(1);
    calls();
    run(&store);
    calls()
}

/// An uncontended turn's lead, planned and committed, makes the calls a plain write
/// transaction of the same records makes, in the same order: the pinned single-writer sequences
/// stay as they are, and it makes no queue file.
#[test]
fn an_uncontended_turn_makes_the_calls_of_a_write() {
    let plain = calls_of(|store| {
        let mut write = store.write().unwrap();
        advance(&mut write);
        write.commit().unwrap();
    });
    let turned = calls_of(|store| {
        let Turn::Lead(mut lead) = store.take_turn(b"own", Serve::Queue).unwrap() else {
            panic!("the lock is free");
        };
        assert_eq!(lead.requests().len(), 1);
        assert!(!lead.may_plan(), "no other request");
        let rid = lead.requests()[0].rid.clone();
        advance(lead.write());
        let plans = vec![Planned::new(rid, FrameUse::Frame(0), b"body".to_vec())];
        assert_eq!(lead.claim(plans).unwrap(), 1);
        let answer = lead.commit().unwrap();
        assert_eq!(answer.outcome, Outcome::Committed { frame_seq: 0 });
        assert_eq!(answer.body, b"body");
    });
    assert_eq!(turned, plain);
}
