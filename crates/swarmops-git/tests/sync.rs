//! The sync engine (M13): the base and the unpublished changes, the changes the remote holds
//! already, and the rest landed onto the remote's head. Written by the M13 plan's task C8a.
//!
//! Every test builds both sides in one store with the test kit's repo helpers: what the remote
//! holds is a changeset like any other once a fetch has imported it, so the engine reads the two
//! histories alike. Each test owns a fresh store.
//!
//! ```text
//!   main:   A ─ B ─ U1 ─ U2        plan(main = U2, remote = R2) → base B, unpublished U1, U2
//!                \
//!   remote:       R1 ─ R2
//! ```

use std::sync::Arc;

use camino::Utf8Path;
use swarmops_core::{ChangesetId, Signature};
use swarmops_git::sync::{self, DropReason, Recognised, SyncError, SyncPlan};
use swarmops_git::{ExportOid, ExportReader, RawKind, git_hash, register, render_commit, tree_oid};
use swarmops_repo::{Change, CommitRequest, LocalRepo};
use swarmops_store::{Journal, Layout, RealFs};
use swarmops_testkit::repo::{new_repo, path, put, request};

/// Who lands the relocated changes: their committer.
const LANDER: &str = "test/lander";

/// The metadata key a relocated changeset names its change by.
const SYNCED_FROM: &str = "synced_from";

/// The lander's signature: fixed, so that the same relocation makes the same changesets.
fn lander() -> Signature {
    Signature {
        name: LANDER.as_bytes().to_vec(),
        email: b"lander@test.swarmops.local".to_vec(),
        secs: 1_760_000_100,
        offset_secs: 0,
        negative_zero: false,
    }
}

/// Commits `changes` on `parents` with `message` and `metadata` (keys ascending): its id.
#[track_caller]
fn commit(
    repo: &LocalRepo,
    parents: &[ChangesetId],
    changes: Vec<Change>,
    message: &str,
    metadata: &[(&str, &str)],
) -> ChangesetId {
    let metadata = metadata
        .iter()
        .map(|(key, value)| ((*key).to_owned(), value.as_bytes().to_vec()))
        .collect();
    let req = CommitRequest {
        metadata,
        ..request(parents, changes, message)
    };
    repo.commit(req).unwrap().id
}

/// Commits `changes` on `parents` with a message of its own and no metadata.
#[track_caller]
fn plain(repo: &LocalRepo, parents: &[ChangesetId], changes: Vec<Change>) -> ChangesetId {
    commit(repo, parents, changes, "a test commit\n", &[])
}

/// The changeset `id`'s parents and metadata.
fn stored(repo: &LocalRepo, id: ChangesetId) -> swarmops_core::Changeset {
    repo.changeset(id).unwrap().unwrap().changeset
}

/// The value of `key` in `id`'s metadata, as text.
fn meta(repo: &LocalRepo, id: ChangesetId, key: &str) -> Option<String> {
    let cs = stored(repo, id);
    let (_, value) = cs.metadata.into_iter().find(|(k, _)| k == key)?;
    Some(String::from_utf8(value).unwrap())
}

/// The bytes of `file` in `id`'s tree.
fn file(repo: &LocalRepo, id: ChangesetId, file: &str) -> Vec<u8> {
    let (_, _, bytes) = repo.read_blob_at(id, &path(file)).unwrap().unwrap();
    bytes
}

/// How many frames the journal of the store at `root` lists.
fn frame_count(root: &Utf8Path) -> u64 {
    Journal::open(&Layout::new(root), Arc::new(RealFs))
        .unwrap()
        .frame_count()
        .unwrap()
}

/// The shared start of most tests: `A`, then `B` on it, both holding `a`.
fn trunk(repo: &LocalRepo) -> ChangesetId {
    let a = plain(repo, &[], vec![put("a", b"0\n")]);
    plain(repo, &[a], vec![put("base", b"b\n")])
}

/// What the remote at `remote` holds of `main`, from the plan on.
fn recognised(repo: &LocalRepo, main: ChangesetId, remote: ChangesetId) -> Recognised {
    let plan = sync::plan(repo, main, remote).unwrap();
    sync::recognise(repo, &plan, remote).unwrap()
}

#[test]
fn base_is_the_newest_main_changeset_the_remote_reaches() {
    let (_dir, _root, repo) = new_repo();
    let b = trunk(&repo);
    let u1 = plain(&repo, &[b], vec![put("u1", b"1\n")]);
    let u2 = plain(&repo, &[u1], vec![put("u2", b"2\n")]);
    let r1 = plain(&repo, &[b], vec![put("r1", b"1\n")]);
    let r2 = plain(&repo, &[r1], vec![put("r2", b"2\n")]);

    // 1. Both moved: the base is B, the two lands above it unpublished, two behind.
    let expected = SyncPlan {
        base: b,
        unpublished: vec![u1, u2],
        behind: 2,
        behind_truncated: false,
    };
    assert_eq!(sync::plan(&repo, u2, r2).unwrap(), expected);

    // 2. The remote holds main: nothing unpublished.
    let plan = sync::plan(&repo, u1, u2).unwrap();
    assert_eq!((plan.base, plan.unpublished, plan.behind), (u1, vec![], 1));

    // 3. Main holds the remote: nothing behind.
    let plan = sync::plan(&repo, u2, u1).unwrap();
    assert_eq!(
        (plan.base, plan.unpublished, plan.behind),
        (u1, vec![u2], 0)
    );
}

#[test]
fn merge_commit_on_the_remote_publishes_its_second_parent() {
    let (_dir, _root, repo) = new_repo();
    let b = trunk(&repo);
    let u1 = plain(&repo, &[b], vec![put("u1", b"1\n")]);
    let u2 = plain(&repo, &[u1], vec![put("u2", b"2\n")]);
    let r1 = plain(&repo, &[b], vec![put("r1", b"1\n")]);
    let merge = plain(&repo, &[r1, u1], vec![put("u1", b"1\n")]);

    // U1 is published through the merge's second parent; R1 and the merge are behind.
    let expected = SyncPlan {
        base: u1,
        unpublished: vec![u2],
        behind: 2,
        behind_truncated: false,
    };
    assert_eq!(sync::plan(&repo, u2, merge).unwrap(), expected);
}

#[test]
fn no_common_base_within_the_walk() {
    let (_dir, _root, repo) = new_repo();
    let x1 = plain(&repo, &[], vec![put("x", b"1\n")]);
    let x2 = plain(&repo, &[x1], vec![put("x", b"2\n")]);
    let y1 = plain(&repo, &[], vec![put("y", b"1\n")]);

    // Unrelated histories share no changeset.
    let err = sync::plan(&repo, x2, y1).unwrap_err();
    assert!(matches!(err, SyncError::NoCommonBase), "{err:?}");
}

#[test]
fn trailer_changeset_line_drops_the_change() {
    let (_dir, _root, repo) = new_repo();
    let b = trunk(&repo);
    let u1 = plain(&repo, &[b], vec![put("u1", b"1\n")]);
    let u2 = plain(&repo, &[u1], vec![put("u2", b"2\n")]);
    let message = format!("rebased on GitHub\n\nSwarmOps-Changeset: {u1}\n");
    let r1 = commit(&repo, &[b], vec![put("other", b"x\n")], &message, &[]);

    let found = recognised(&repo, u2, r1);
    assert_eq!(found.dropped, vec![(u1, DropReason::Trailer)]);
    assert_eq!(found.to_land, vec![u2]);
}

#[test]
fn trailer_squashed_from_line_drops_the_change() {
    let (_dir, _root, repo) = new_repo();
    let b = trunk(&repo);
    let u1 = plain(&repo, &[b], vec![put("u1", b"1\n")]);
    let u2 = plain(&repo, &[u1], vec![put("u2", b"2\n")]);
    let u3 = plain(&repo, &[u2], vec![put("u3", b"3\n")]);
    let message = format!("squashed\n\nSwarmOps-Squashed-From: {u1} {u2}\n");
    let r1 = commit(&repo, &[b], vec![put("other", b"x\n")], &message, &[]);

    let found = recognised(&repo, u3, r1);
    let dropped = vec![(u1, DropReason::Trailer), (u2, DropReason::Trailer)];
    assert_eq!(found.dropped, dropped);
    assert_eq!(found.to_land, vec![u3]);
}

#[test]
fn trailer_land_key_drops_the_change() {
    let (_dir, _root, repo) = new_repo();
    let b = trunk(&repo);
    let keyed = [("land_key", "key-1")];
    let u1 = commit(&repo, &[b], vec![put("u1", b"1\n")], "keyed\n", &keyed);
    let u2 = plain(&repo, &[u1], vec![put("u2", b"2\n")]);
    let message = "a land made elsewhere\n\nSwarmOps-Land-Key: key-1\n";
    let r1 = commit(&repo, &[b], vec![put("other", b"x\n")], message, &[]);

    let found = recognised(&repo, u2, r1);
    assert_eq!(found.dropped, vec![(u1, DropReason::Trailer)]);
    assert_eq!(found.to_land, vec![u2]);
}

#[test]
fn indented_trailer_lines_count() {
    let (_dir, _root, repo) = new_repo();
    let b = trunk(&repo);
    let u1 = plain(&repo, &[b], vec![put("u1", b"1\n")]);
    let u2 = plain(&repo, &[u1], vec![put("u2", b"2\n")]);
    let u3 = plain(&repo, &[u2], vec![put("u3", b"3\n")]);

    // A squash message listing the commits' bodies, indented by spaces and by a tab.
    let message = format!(
        "Two changes (#3)\n\n  first\n\n    SwarmOps-Changeset: {u1}\n\n\tsecond\n\n\tSwarmOps-Changeset: {u2}\n"
    );
    let r1 = commit(&repo, &[b], vec![put("other", b"x\n")], &message, &[]);

    let found = recognised(&repo, u3, r1);
    let dropped = vec![(u1, DropReason::Trailer), (u2, DropReason::Trailer)];
    assert_eq!(found.dropped, dropped);
    assert_eq!(found.to_land, vec![u3]);
}

#[test]
fn bulleted_trailer_lines_count() {
    let (_dir, _root, repo) = new_repo();
    let b = trunk(&repo);
    let u1 = plain(&repo, &[b], vec![put("u1", b"1\n")]);
    let u2 = plain(&repo, &[u1], vec![put("u2", b"2\n")]);
    let u3 = plain(&repo, &[u2], vec![put("u3", b"3\n")]);
    let u4 = plain(&repo, &[u3], vec![put("u4", b"4\n")]);

    // GitHub's default squash description: one bullet per commit message; a second bullet is
    // not stripped.
    let message = format!(
        "Squash (#4)\n\n* SwarmOps-Changeset: {u1}\n  - SwarmOps-Changeset: {u2}\n* * SwarmOps-Changeset: {u3}\n"
    );
    let r1 = commit(&repo, &[b], vec![put("other", b"x\n")], &message, &[]);

    let found = recognised(&repo, u4, r1);
    let dropped = vec![(u1, DropReason::Trailer), (u2, DropReason::Trailer)];
    assert_eq!(found.dropped, dropped);
    assert_eq!(found.to_land, vec![u3, u4]);
}

#[test]
fn no_op_prefix_drops_a_squash_without_bodies() {
    let (_dir, _root, repo) = new_repo();
    let b = trunk(&repo);
    let u1 = plain(&repo, &[b], vec![put("u1", b"1\n")]);
    let u2 = plain(&repo, &[u1], vec![put("u2", b"2\n")]);
    let u3 = plain(&repo, &[u2], vec![put("u3", b"3\n")]);

    // A squash of U1 and U2 whose message dropped the bodies, then another change on the remote.
    let squash = vec![put("u1", b"1\n"), put("u2", b"2\n")];
    let r1 = commit(&repo, &[b], squash, "Squash (#5)\n", &[]);
    let r2 = plain(&repo, &[r1], vec![put("r2", b"2\n")]);

    let found = recognised(&repo, u3, r2);
    let dropped = vec![(u1, DropReason::NoOp), (u2, DropReason::NoOp)];
    assert_eq!(found.dropped, dropped);
    assert_eq!(found.to_land, vec![u3]);
}

#[test]
fn no_op_scan_picks_the_largest_prefix() {
    let (_dir, _root, repo) = new_repo();
    let b = trunk(&repo);
    let u1 = plain(&repo, &[b], vec![put("a", b"1\n")]);
    let u2 = plain(&repo, &[u1], vec![put("a", b"2\n")]);
    let u3 = plain(&repo, &[u2], vec![put("c", b"3\n")]);

    // The remote holds U1 and U2 squashed: `a` at 2. U1 alone conflicts with it, so a scan from
    // the smallest prefix up would stop before U2.
    let r1 = commit(&repo, &[b], vec![put("a", b"2\n")], "Squash (#6)\n", &[]);

    let found = recognised(&repo, u3, r1);
    let dropped = vec![(u1, DropReason::NoOp), (u2, DropReason::NoOp)];
    assert_eq!(found.dropped, dropped);
    assert_eq!(found.to_land, vec![u3]);
}

#[test]
fn relocate_lands_each_change_once_in_order() {
    let (_dir, root, repo) = new_repo();
    let b = trunk(&repo);
    let u1 = plain(&repo, &[b], vec![put("u1", b"1\n")]);
    let u2 = plain(&repo, &[u1], vec![put("u2", b"2\n")]);
    let r1 = plain(&repo, &[b], vec![put("r1", b"1\n")]);
    let refs = repo.list_refs().unwrap();
    let frames = frame_count(&root);

    let relocated = sync::relocate(&repo, &[u1, u2], r1, &lander()).unwrap();

    // 1. One new changeset per change, each on the one before, the first on the remote's head.
    let [(c1, l1), (c2, l2)] = relocated.landed[..] else {
        panic!("{relocated:?}");
    };
    assert_eq!((c1, c2), (u1, u2));
    assert_eq!(stored(&repo, l1).parents, vec![r1]);
    assert_eq!(stored(&repo, l2).parents, vec![l1]);
    assert_eq!(relocated.head, l2);
    assert_eq!(relocated.no_ops, Vec::<ChangesetId>::new());

    // 2. The last holds both sides' files.
    assert_eq!(file(&repo, l2, "u1"), b"1\n");
    assert_eq!(file(&repo, l2, "u2"), b"2\n");
    assert_eq!(file(&repo, l2, "r1"), b"1\n");

    // 3. No ref moved: one frame per land, each its insert alone.
    assert_eq!(repo.list_refs().unwrap(), refs);
    assert_eq!(frame_count(&root), frames + 2);

    // 4. A change already on the head it lands onto merges as no change.
    let again = sync::relocate(&repo, &[u1], l2, &lander()).unwrap();
    assert_eq!(
        (again.head, again.landed, again.no_ops),
        (l2, vec![], vec![u1])
    );
}

#[test]
fn relocate_copies_key_and_squashed_from_and_adds_synced_from() {
    let (_dir, _root, repo) = new_repo();
    let b = trunk(&repo);
    let x = plain(&repo, &[b], vec![put("x", b"x\n")]);
    let squashed = x.to_string();
    let metadata = [
        ("agent", "test/agent"),
        ("land_key", "key-7"),
        ("run_id", "run-1"),
        ("squashed_from", squashed.as_str()),
    ];
    let u1 = commit(&repo, &[b], vec![put("u1", b"1\n")], "landed\n", &metadata);
    let u2 = plain(&repo, &[u1], vec![put("u2", b"2\n")]);
    let r1 = plain(&repo, &[b], vec![put("r1", b"1\n")]);

    let relocated = sync::relocate(&repo, &[u1, u2], r1, &lander()).unwrap();
    let [(_, l1), (_, l2)] = relocated.landed[..] else {
        panic!("{relocated:?}");
    };

    // 1. The keyed land's key and chain are copied; who made it too; the sync adds itself.
    assert_eq!(meta(&repo, l1, "land_key").as_deref(), Some("key-7"));
    assert_eq!(meta(&repo, l1, "squashed_from"), Some(squashed));
    assert_eq!(meta(&repo, l1, "agent").as_deref(), Some("test/agent"));
    assert_eq!(meta(&repo, l1, "run_id").as_deref(), Some("run-1"));
    assert_eq!(meta(&repo, l1, "lander").as_deref(), Some(LANDER));
    assert_eq!(meta(&repo, l1, SYNCED_FROM), Some(u1.to_string()));

    // 2. A change without a key has none; its chain is its own.
    assert_eq!(meta(&repo, l2, "land_key"), None);
    assert_eq!(meta(&repo, l2, "squashed_from"), Some(u2.to_string()));
    assert_eq!(meta(&repo, l2, SYNCED_FROM), Some(u2.to_string()));

    // 3. Author and message are the change's; the committer is the lander.
    let (change, landed) = (stored(&repo, u1), stored(&repo, l1));
    assert_eq!(landed.author, change.author);
    assert_eq!(landed.message, change.message);
    assert_eq!(landed.committer, lander());
}

#[test]
fn relocate_conflict_reports_the_change() {
    let (_dir, root, repo) = new_repo();
    let b = trunk(&repo);
    let u1 = plain(&repo, &[b], vec![put("u1", b"1\n")]);
    let u2 = plain(&repo, &[u1], vec![put("a", b"mine\n")]);
    let r1 = plain(&repo, &[b], vec![put("a", b"theirs\n")]);
    let frames = frame_count(&root);

    let err = sync::relocate(&repo, &[u1, u2], r1, &lander()).unwrap_err();

    // The second change conflicts with the first's landed changeset, which it was landing onto.
    let SyncError::Conflict {
        change,
        onto,
        conflicts,
    } = err
    else {
        panic!("{err:?}");
    };
    assert_eq!(change, u2);
    assert_eq!(stored(&repo, onto).parents, vec![r1]);
    assert_eq!(conflicts.ours, u2);
    assert_eq!(conflicts.conflicts.len(), 1);
    assert_eq!(frame_count(&root), frames + 1);
}

#[test]
fn relocate_refuses_a_merge_changeset() {
    let (_dir, root, repo) = new_repo();
    let b = trunk(&repo);
    let u1 = plain(&repo, &[b], vec![put("u1", b"1\n")]);
    let side = plain(&repo, &[b], vec![put("side", b"s\n")]);
    let merge = plain(&repo, &[u1, side], vec![put("side", b"s\n")]);
    let r1 = plain(&repo, &[b], vec![put("r1", b"1\n")]);
    let frames = frame_count(&root);

    // Refused before anything lands: U1 is not landed either.
    let err = sync::relocate(&repo, &[u1, merge], r1, &lander()).unwrap_err();
    assert!(
        matches!(err, SyncError::MergeChangeset(id) if id == merge),
        "{err:?}"
    );
    assert_eq!(frame_count(&root), frames);
}

#[test]
fn synced_from_is_not_rendered_by_the_export() {
    let (_dir, _root, mut repo) = new_repo();
    register(&mut repo).unwrap();
    let b = trunk(&repo);
    let u1 = plain(&repo, &[b], vec![put("u1", b"1\n")]);
    let r1 = plain(&repo, &[b], vec![put("r1", b"1\n")]);
    let relocated = sync::relocate(&repo, &[u1], r1, &lander()).unwrap();
    let landed = relocated.head;

    // 1. The changeset as recorded, and the same without `synced_from`.
    let cs = stored(&repo, landed);
    let mut without = cs.clone();
    without.metadata.retain(|(key, _)| key != SYNCED_FROM);
    assert_ne!(without.metadata, cs.metadata);

    // 2. Its export oid is the hash of the rendering without the key: version 1 holds.
    let read = repo.snapshot().unwrap();
    let reader = ExportReader {
        read: &read,
        store: repo.derived_store(),
    };
    let tree = tree_oid(&reader, cs.root).unwrap();
    let parent = repo.derived::<ExportOid>(r1, &()).unwrap().git_oid();
    let rendered = render_commit(landed, &without, &[parent], tree);
    let exported = repo.derived::<ExportOid>(landed, &()).unwrap();
    assert_eq!(exported.git_oid(), git_hash(RawKind::Commit, &rendered));

    // 3. The rendering carries no trailer for the key.
    let text = String::from_utf8(rendered).unwrap();
    assert!(!text.to_lowercase().contains("synced"), "{text}");
}
