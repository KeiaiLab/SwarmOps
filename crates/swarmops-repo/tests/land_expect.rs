//! A land with `expect_target` (operations spec §3 (h)): after the keyed replay check, a target
//! that does not name the expected changeset is `RepoError::CasMismatch` and nothing is written;
//! one that does gets one attempt, merged onto it and published by compare-and-swap. A land of a
//! rebased head onto the head it was rebased on publishes the rebased tree. Without
//! `expect_target` a land is what it was.

use swarmops_core::ChangesetId;
use swarmops_repo::land::{LandRequest, LandStatus};
use swarmops_repo::{LocalRepo, RepoError};
use swarmops_testkit::repo::put;

use crate::rebase::{
    ACTOR, OTHER, advance, commit_by, frame_count, landing, main_head, new_store, rebasing,
};

/// [`landing`] of `head` from `base`, published only while `main` names `expected`.
fn expecting(head: ChangesetId, base: ChangesetId, expected: ChangesetId) -> LandRequest {
    let mut land = landing(head);
    land.base = Some(base);
    land.expect_target = Some(expected);
    land
}

/// Changeset `id`'s root tree.
fn root_of(repo: &LocalRepo, id: ChangesetId) -> swarmops_core::TreeId {
    repo.changeset(id).unwrap().expect("stored").changeset.root
}

/// A change rebased onto the target's head `T`, landed with `expect_target` `T`, publishes the
/// tree the rebase recorded: `L`'s root is `H'`'s, on `T`, in one attempt.
#[test]
fn land_with_expect_target_publishes_the_tested_tree() {
    let (_dir, _root, repo) = new_store();
    let t0 = advance(&repo, ACTOR, &[], vec![put("f", b"1\n2\n3\n4\n5\n")]);
    let head = commit_by(&repo, ACTOR, &[t0], vec![put("f", b"1\n2\n3\n4\nours\n")]);
    let t1 = advance(&repo, OTHER, &[t0], vec![put("f", b"one\n2\n3\n4\n5\n")]);

    // 1. The rebase: what an agent builds and tests.
    let rebased = repo.rebase(rebasing(head)).unwrap();
    assert_eq!(rebased.onto, t1);

    // 2. The exact land of it.
    let landed = repo.land(expecting(rebased.changeset, t1, t1)).unwrap();
    assert_eq!(landed.status, LandStatus::Landed);
    assert_eq!((landed.rebased_onto, landed.attempts), (t1, 1));
    assert_eq!(root_of(&repo, landed.landed), rebased.root);
    assert_eq!(main_head(&repo), landed.landed);
}

/// A target that moved after the rebase: `CasMismatch` naming the target, what it names and what
/// the land expected, and nothing written: the journal's length and the refs as they were.
#[test]
fn land_with_expect_target_on_a_moved_target_is_a_cas_mismatch_and_writes_nothing() {
    let (_dir, root, repo) = new_store();
    let t0 = advance(&repo, ACTOR, &[], vec![put("a", b"a\n")]);
    let head = commit_by(&repo, ACTOR, &[t0], vec![put("b", b"b\n")]);
    let rebased = repo.rebase(rebasing(head)).unwrap();
    assert_eq!(rebased.onto, t0);
    let t1 = advance(&repo, OTHER, &[t0], vec![put("c", b"c\n")]);
    let frames = frame_count(&root);

    let refused = repo.land(expecting(rebased.changeset, t0, t0)).unwrap_err();
    let RepoError::CasMismatch {
        target,
        actual,
        expected,
        ..
    } = refused
    else {
        panic!("a CAS mismatch, not {refused:?}");
    };
    assert_eq!((target.as_str(), actual, expected), ("main", t1, t0));
    assert_eq!((frame_count(&root), main_head(&repo)), (frames, t1));
    assert_eq!(repo.list_refs().unwrap().len(), 1);
}

/// The keyed replay check runs first: a land whose key landed its change replays it whatever the
/// target names since, an expected head it no longer names included.
#[test]
fn land_with_expect_target_replays_a_landed_key_first() {
    let (_dir, root, repo) = new_store();
    let t0 = advance(&repo, ACTOR, &[], vec![put("a", b"a\n")]);
    let head = commit_by(&repo, ACTOR, &[t0], vec![put("b", b"b\n")]);
    let keyed = || {
        let mut land = expecting(head, t0, t0);
        land.key = Some("key-1".to_owned());
        land
    };
    let first = repo.land(keyed()).unwrap();
    let later = advance(&repo, OTHER, &[first.landed], vec![put("c", b"c\n")]);
    let frames = frame_count(&root);

    let again = repo.land(keyed()).unwrap();
    assert_eq!(again.status, LandStatus::Replayed);
    assert_eq!((again.landed, again.rebased_onto), (first.landed, t0));
    assert_eq!((frame_count(&root), main_head(&repo)), (frames, later));
}

/// Without `expect_target` a land is unchanged: on a target that moved it merges onto the new
/// head; and given the head the target names, the land makes the very changeset the land
/// without it makes, on a store made the same way.
#[test]
fn land_without_expect_target_is_unchanged() {
    // 1. Two stores made alike: a change on t0, main moved on to t1.
    let made = || {
        let (dir, root, repo) = new_store();
        let t0 = advance(&repo, ACTOR, &[], vec![put("a", b"a\n")]);
        let head = commit_by(&repo, ACTOR, &[t0], vec![put("b", b"b\n")]);
        let t1 = advance(&repo, OTHER, &[t0], vec![put("c", b"c\n")]);
        (dir, root, repo, [t0, head, t1])
    };
    let (_one, _, plain, [t0, head, t1]) = made();
    let (_two, _, exact, _) = made();

    // 2. The land without it merges onto the moved target; the one with it, the same.
    let merged = plain.land(landing(head)).unwrap();
    assert_eq!(
        (merged.status, merged.rebased_onto, merged.base),
        (LandStatus::Landed, t1, t0)
    );
    let expected = exact.land(expecting(head, t0, t1)).unwrap();
    assert_eq!(expected.landed, merged.landed);
    assert_eq!(expected.files_changed, merged.files_changed);
}
