//! Landing (operations spec §3, the store-only block): `Repo::land` merges a changeset, with the
//! first-parent chain it ends since a base, three-way onto a ref's head, and publishes the result
//! as one squashed changeset, `L`, in one journal transaction: `L`'s insert and the ref's
//! compare-and-swap together. A land that loses the race to the ref re-merges what it merged onto
//! what the ref gained, and tries again; a keyed land run again is a replay, decided under the
//! store's lock; a conflict comes back as data and records nothing; and a land that crashes or
//! fails at any filesystem call has landed once or not at all.
//!
//! Every test owns a fresh store, its writes checked against the store's ordering rules but where
//! a test says otherwise. The crash and failure sweeps run this test binary again as their
//! children (selected by [`CHILD_ROOT`]), on the real filesystem.

use std::collections::HashSet;
use std::io;
#[cfg(feature = "test-hooks")]
use std::os::unix::process::ExitStatusExt;
#[cfg(feature = "test-hooks")]
use std::process::Output;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
#[cfg(feature = "test-hooks")]
use std::thread;
use std::time::Duration;

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_core::{ChangesetId, ObjectId, RepoPath, Signature};
use swarmops_repo::land::{LandOutcome, LandRequest, LandStatus, NothingToLandReason, backoff};
use swarmops_repo::merge::tree_diff::{PathFilter, delta_of};
use swarmops_repo::{Advance, Change, CommitRequest, Depth, LocalRepo, RepoError};
#[cfg(feature = "test-hooks")]
use swarmops_store::{CRASH_AT_ENV, FAIL_AT_ENV, FS_CALLS_ENV, ORDER_CHECK_ENV};
use swarmops_store::{FsOps, Journal, Layout, MetaStore, RealFs};
#[cfg(feature = "test-hooks")]
use swarmops_testkit::child::{SIGABRT, run, this_test};
use swarmops_testkit::faults::checked;
#[cfg(feature = "test-hooks")]
use swarmops_testkit::fs::copy_dir;
use swarmops_testkit::fs::{Call, FailingFs, NoSyncFs, Watch, Watched};
use swarmops_testkit::repo::{blob, delete, path, put, request};
#[cfg(feature = "test-hooks")]
use swarmops_testkit::test_name;
use tempfile::TempDir;

/// Who moves `main` in the tests' setup, and makes the changes they land.
const ACTOR: &str = "test/agent";

/// The agent of the target's changesets that a change races or conflicts with.
const OTHER: &str = "test/other";

/// Who lands: the name of the landed changeset's committer.
const LANDER: &str = "test/lander";

/// A fresh store, its writes checked against the store's ordering rules.
fn new_store() -> (TempDir, Utf8PathBuf, LocalRepo) {
    let dir = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
    let repo = LocalRepo::create_with_fs(&root, "test", checked(RealFs)).unwrap();
    (dir, root, repo)
}

/// The lander's signature: fixed, so that the same land makes the same changeset.
fn lander() -> Signature {
    Signature {
        name: LANDER.as_bytes().to_vec(),
        email: b"lander@test.swarmops.local".to_vec(),
        secs: 1_760_000_100,
        offset_secs: 0,
        negative_zero: false,
    }
}

/// A land of `head` onto `main` by [`lander`], with the defaults ([`LandRequest::new`]).
fn landing(head: ChangesetId) -> LandRequest {
    LandRequest::new(head, "main", lander())
}

/// [`landing`] with the idempotency key `key`.
fn keyed(head: ChangesetId, key: &str) -> LandRequest {
    let mut land = landing(head);
    land.key = Some(key.to_owned());
    land
}

/// A request for `changes` on `parents`, recording `agent` in its metadata.
fn by(agent: &str, parents: &[ChangesetId], changes: Vec<Change>) -> CommitRequest {
    let mut req = request(parents, changes, "a test commit");
    req.metadata = vec![("agent".to_owned(), agent.as_bytes().to_vec())];
    req
}

/// Commits `changes` on `parents` by `agent`; the new changeset's id.
#[track_caller]
fn commit_by(
    repo: &LocalRepo,
    agent: &str,
    parents: &[ChangesetId],
    changes: Vec<Change>,
) -> ChangesetId {
    repo.commit(by(agent, parents, changes)).unwrap().id
}

/// Commits `changes` on `parents` by `agent`, moving `main` to the new changeset in the same
/// transaction.
#[track_caller]
fn advance(
    repo: &LocalRepo,
    agent: &str,
    parents: &[ChangesetId],
    changes: Vec<Change>,
) -> ChangesetId {
    let mut req = by(agent, parents, changes);
    req.advance = Some(Advance::new("main", agent));
    repo.commit(req).unwrap().id
}

/// What `main` names.
fn main_head(repo: &LocalRepo) -> ChangesetId {
    repo.get_ref("main").unwrap().expect("main exists").target
}

/// How many frames the journal of the store at `root` lists.
fn frame_count(root: &Utf8Path) -> u64 {
    Journal::open(&Layout::new(root), Arc::new(RealFs))
        .unwrap()
        .frame_count()
        .unwrap()
}

/// The records of the frames past the first `frames` of the store at `root`: each frame's seq,
/// the record's kind and its key.
fn records_past(root: &Utf8Path, frames: u64) -> Vec<(u64, &'static str, Vec<u8>)> {
    let meta = MetaStore::open(root).unwrap();
    let entries = meta
        .read()
        .unwrap()
        .journal_after(frames.checked_sub(1), 100)
        .unwrap();
    entries
        .into_iter()
        .map(|entry| (entry.seq, entry.kind, entry.key))
        .collect()
}

/// Changeset `id`'s metadata `key`, if it has one.
fn meta_value(repo: &LocalRepo, id: ChangesetId, key: &str) -> Option<Vec<u8>> {
    let stored = repo.changeset(id).unwrap().expect("the changeset");
    (stored.changeset.metadata.into_iter())
        .find(|(name, _)| name == key)
        .map(|(_, value)| value)
}

/// What the file at `p` holds in changeset `id`'s tree; `None` if there is none.
fn content(repo: &LocalRepo, id: ChangesetId, p: &str) -> Option<Vec<u8>> {
    repo.read_blob_at(id, &path(p))
        .unwrap()
        .map(|(_, _, bytes)| bytes)
}

/// Changeset `id`'s first parent.
fn first_parent(repo: &LocalRepo, id: ChangesetId) -> Option<ChangesetId> {
    let stored = repo.changeset(id).unwrap().expect("the changeset");
    stored.changeset.parents.first().copied()
}

/// A land of a commit made on the target's head is one new changeset on the target, `L`: its one
/// parent the head it landed on, its delta the commit's (the same delta hash) and its tree the
/// commit's; its message and author the commit's, its committer the lander; its metadata the
/// commit's `agent` and `run_id`, the land's key, the lander, the lander's own metadata and the
/// chain it squashed, sorted by key. `main` names it, and the land wrote one journal frame: `L`'s
/// insert, then `main`'s move.
#[test]
fn a_land_of_a_commit_on_the_head_is_one_new_changeset_on_the_target() {
    let (_dir, root, repo) = new_store();
    let t0 = advance(
        &repo,
        ACTOR,
        &[],
        vec![put("a", b"a\n"), put("src/lib.rs", b"l\n")],
    );
    let mut change = by(
        ACTOR,
        &[t0],
        vec![put("src/lib.rs", b"l 2\n"), put("b", b"b\n")],
    );
    change
        .metadata
        .push(("run_id".to_owned(), b"run-1".to_vec()));
    change.message = b"the change\n".to_vec();
    let head = repo.commit(change).unwrap().id;
    let frames = frame_count(&root);

    let mut land = keyed(head, "key-1");
    land.lander_metadata = vec![("lander_run_id".to_owned(), b"run-2".to_vec())];
    let out = repo.land(land).unwrap();
    let want = LandOutcome::new(
        LandStatus::Landed,
        out.landed,
        t0,
        t0,
        1,
        false,
        vec![head],
        2,
        Vec::new(),
        Vec::new(),
    );
    assert_eq!(out, want);
    let landed = repo.changeset(out.landed).unwrap().unwrap().changeset;
    let made = repo.changeset(head).unwrap().unwrap().changeset;
    assert_eq!(landed.parents, [t0]);
    assert_eq!(landed.delta, made.delta);
    assert_eq!(landed.delta_hash().unwrap(), made.delta_hash().unwrap());
    assert_eq!(landed.root, made.root);
    assert_eq!(landed.message, made.message);
    assert_eq!(landed.author, made.author);
    assert_eq!(landed.committer, lander());
    let metadata: Vec<(&str, &[u8])> = (landed.metadata.iter())
        .map(|(key, value)| (key.as_str(), value.as_slice()))
        .collect();
    let squashed = head.to_string();
    assert_eq!(
        metadata,
        [
            ("agent", ACTOR.as_bytes()),
            ("land_key", b"key-1"),
            ("lander", LANDER.as_bytes()),
            ("lander_run_id", b"run-2"),
            ("run_id", b"run-1"),
            ("squashed_from", squashed.as_bytes()),
        ]
    );
    assert_eq!(main_head(&repo), out.landed);
    assert_eq!(
        records_past(&root, frames),
        [
            (frames, "changeset", out.landed.0.0.to_vec()),
            (frames, "ref_move", b"main".to_vec())
        ]
    );
}

/// A land squashes the chain since its base into one changeset: three commits on the target's
/// head land as one `L`, whose delta is the diff from that head to the chain's last commit, whose
/// tree is that commit's, and whose `squashed_from` lists the three, oldest first. A message the
/// land gives replaces the head's.
#[test]
fn a_land_squashes_a_chain_into_one_changeset() {
    let (_dir, _root, repo) = new_store();
    let t0 = advance(
        &repo,
        ACTOR,
        &[],
        vec![put("a", b"a\n"), put("gone", b"x\n")],
    );
    let c1 = commit_by(
        &repo,
        ACTOR,
        &[t0],
        vec![put("a", b"a 1\n"), put("b", b"b\n")],
    );
    let c2 = commit_by(
        &repo,
        ACTOR,
        &[c1],
        vec![delete("gone"), put("d/e", b"e\n")],
    );
    let c3 = commit_by(&repo, ACTOR, &[c2], vec![put("a", b"a 3\n"), delete("b")]);

    let mut land = landing(c3);
    land.message = Some(b"three in one\n".to_vec());
    let out = repo.land(land).unwrap();
    assert_eq!(
        (out.base, out.squashed_from.clone()),
        (t0, vec![c1, c2, c3])
    );
    let landed = repo.changeset(out.landed).unwrap().unwrap().changeset;
    let diff = repo
        .snapshot()
        .unwrap()
        .diff(t0, c3, &PathFilter::all())
        .unwrap();
    assert_eq!(landed.delta, delta_of(&diff));
    assert_eq!(out.files_changed, 3, "a, d/e and gone: b came and went");
    let made = repo.changeset(c3).unwrap().unwrap().changeset;
    assert_eq!(landed.root, made.root);
    assert_eq!(landed.message, b"three in one\n");
    let chain = format!("{c1} {c2} {c3}");
    assert_eq!(
        meta_value(&repo, out.landed, "squashed_from"),
        Some(chain.into_bytes())
    );
}

/// A land whose target moved after its change was made merges three-way: the change, made on
/// `t0`, lands on the head the target moved to, `x`, as `L` with `x` its parent and both sides'
/// changes in its tree, in one attempt (nothing moved the target during the land itself).
#[test]
fn a_land_after_the_target_moved_merges_three_way() {
    let (_dir, _root, repo) = new_store();
    let t0 = advance(&repo, ACTOR, &[], vec![put("a", b"a\n"), put("c", b"c\n")]);
    let head = commit_by(&repo, ACTOR, &[t0], vec![put("a", b"ours\n")]);
    let x = advance(
        &repo,
        OTHER,
        &[t0],
        vec![put("c", b"theirs\n"), put("x/y", b"y\n")],
    );

    let out = repo.land(landing(head)).unwrap();
    assert_eq!(
        (out.status, out.rebased_onto, out.base, out.attempts),
        (LandStatus::Landed, x, t0, 1)
    );
    assert_eq!(first_parent(&repo, out.landed), Some(x));
    assert_eq!(content(&repo, out.landed, "a"), Some(b"ours\n".to_vec()));
    assert_eq!(content(&repo, out.landed, "c"), Some(b"theirs\n".to_vec()));
    assert_eq!(content(&repo, out.landed, "x/y"), Some(b"y\n".to_vec()));
    assert_eq!(out.auto_merged, Vec::<RepoPath>::new());
    assert_eq!(main_head(&repo), out.landed);
}

/// Edits of one file that the change and the target made lines apart merge: the land reports the
/// file auto-merged, and `L` holds both edits.
#[test]
fn a_land_merges_separated_edits_of_one_file() {
    let (_dir, _root, repo) = new_store();
    let t0 = advance(&repo, ACTOR, &[], vec![put("f", b"1\n2\n3\n4\n5\n")]);
    let head = commit_by(&repo, ACTOR, &[t0], vec![put("f", b"one\n2\n3\n4\n5\n")]);
    advance(&repo, OTHER, &[t0], vec![put("f", b"1\n2\n3\n4\nfive\n")]);

    let out = repo.land(landing(head)).unwrap();
    assert_eq!(out.auto_merged, [path("f")]);
    assert_eq!(
        content(&repo, out.landed, "f"),
        Some(b"one\n2\n3\n4\nfive\n".to_vec())
    );
}

/// The conflicts a land's error carries, which must be `RepoError::Conflict`.
#[track_caller]
fn conflicts_of(
    landed: Result<LandOutcome, RepoError>,
) -> Box<swarmops_repo::land::MergeConflicts> {
    match landed {
        Err(RepoError::Conflict(conflicts)) => conflicts,
        other => panic!("expected Conflict, got {other:?}"),
    }
}

/// Each conflicting path of `conflicts`, with its kind's name.
fn kinds(conflicts: &swarmops_repo::land::MergeConflicts) -> Vec<(&str, &'static str)> {
    (conflicts.conflicts.iter())
        .map(|conflict| (conflict.path.as_str(), conflict.kind.name()))
        .collect()
}

/// A change that conflicts with the target records nothing: the land fails with the conflicts as
/// data, from the merge of the change (`ours`, from `base`) onto the target's head (`theirs`):
/// each conflicting path with its kind and its three leaves, and the newest changeset on the
/// target since the base that touched it (not an older one that did, nor a newer one that did
/// not), with its agent. The journal and `main` stay as they were.
#[test]
fn a_land_whose_change_conflicts_records_nothing() {
    let (_dir, root, repo) = new_store();
    let t0 = advance(
        &repo,
        ACTOR,
        &[],
        vec![put("f", b"1\n2\n3\n"), put("g", b"g\n")],
    );
    let head = commit_by(
        &repo,
        ACTOR,
        &[t0],
        vec![put("f", b"1\nours\n3\n"), put("h", b"h\n")],
    );
    let w = advance(&repo, OTHER, &[t0], vec![put("f", b"one\n2\n3\n")]);
    let x = advance(
        &repo,
        "test/third",
        &[w],
        vec![put("f", b"one\ntheirs\n3\n")],
    );
    let y = advance(&repo, "test/fourth", &[x], vec![put("g", b"g 2\n")]);
    let (frames, before) = (frame_count(&root), main_head(&repo));

    let conflicts = conflicts_of(repo.land(landing(head)));
    assert_eq!(
        (conflicts.base, conflicts.ours, conflicts.theirs),
        (t0, head, y)
    );
    assert_eq!(kinds(&conflicts), [("f", "content")]);
    let file = &conflicts.conflicts[0];
    let leaf = |side: Option<swarmops_repo::merge::tree_diff::Leaf>| side.map(|leaf| leaf.blob);
    assert_eq!(leaf(file.base), Some(blob(b"1\n2\n3\n")));
    assert_eq!(leaf(file.ours), Some(blob(b"1\nours\n3\n")));
    assert_eq!(leaf(file.theirs), Some(blob(b"one\ntheirs\n3\n")));
    assert_eq!(
        conflicts.last_touched.get(&path("f")),
        Some(&(x, "test/third".to_owned()))
    );
    assert_eq!(conflicts.auto_merged, Vec::<RepoPath>::new());
    assert_eq!((frame_count(&root), main_head(&repo)), (frames, before));
}

/// Who last touched a path counts a change below it: a file the change puts where the target made
/// a directory conflicts as a type change, and names the target's changeset that put a file in
/// that directory, not a newer one that changed elsewhere.
#[test]
fn a_type_change_names_who_touched_the_directory() {
    let (_dir, _root, repo) = new_store();
    let t0 = advance(&repo, ACTOR, &[], vec![put("a", b"a\n")]);
    let head = commit_by(&repo, ACTOR, &[t0], vec![put("d", b"a file\n")]);
    let x = advance(&repo, OTHER, &[t0], vec![put("d/x", b"x\n")]);
    advance(&repo, "test/third", &[x], vec![put("a", b"a 2\n")]);

    let conflicts = conflicts_of(repo.land(landing(head)));
    assert_eq!(kinds(&conflicts), [("d", "type_change")]);
    assert_eq!(
        conflicts.last_touched.get(&path("d")),
        Some(&(x, OTHER.to_owned()))
    );
}

/// A land whose change conflicts on a file at very many places carries few hunks: the first ones
/// as diff3 made them and one spanning the rest, each side's lines from the first one's to the
/// last one's, so the lines of every place where the sides' edits meet are within a hunk it
/// carries; and every conflicting path. Nothing is recorded.
#[test]
fn a_land_conflicting_at_very_many_places_carries_few_hunks() {
    use swarmops_repo::merge::conflict::ConflictKind;

    const PLACES: u32 = 2_000;
    let (_dir, root, repo) = new_store();
    // Line `2i` changed by each side, line `2i + 1` by none.
    let alternating = |side: &str| -> Vec<u8> {
        (0..PLACES)
            .flat_map(|i| format!("{side}{i}\n}}\n").into_bytes())
            .collect()
    };
    let t0 = advance(
        &repo,
        ACTOR,
        &[],
        vec![put("f", &alternating("l")), put("g", b"g\n")],
    );
    let ours = vec![put("f", &alternating("o")), put("g", b"ours\n")];
    let head = commit_by(&repo, ACTOR, &[t0], ours);
    let theirs = vec![put("f", &alternating("t")), put("g", b"theirs\n")];
    advance(&repo, OTHER, &[t0], theirs);
    let (frames, before) = (frame_count(&root), main_head(&repo));

    let conflicts = conflicts_of(repo.land(landing(head)));
    assert_eq!(kinds(&conflicts), [("f", "content"), ("g", "content")]);
    let ConflictKind::Content { hunks } = &conflicts.conflicts[0].kind else {
        unreachable!("f conflicts on its content");
    };
    assert!(hunks.len() <= 200, "{} hunks of {PLACES}", hunks.len());
    let first = &hunks[0];
    assert_eq!(
        (first.base.clone(), first.ours_text.as_slice()),
        (0..1, b"o0\n".as_slice())
    );
    for at in 0..PLACES {
        let line = 2 * at;
        let within = hunks.iter().any(|hunk| hunk.base.contains(&line));
        assert!(within, "base line {line} is in no hunk");
    }
    let last = &hunks[hunks.len() - 1];
    assert_eq!(
        last.base.end,
        2 * PLACES - 1,
        "the last place's line ends the last hunk"
    );
    let lines = (last.base.end - last.base.start) as usize;
    assert_eq!(
        last.base_text
            .split_inclusive(|&byte| byte == b'\n')
            .count(),
        lines
    );
    assert_eq!((frame_count(&root), main_head(&repo)), (frames, before));
}

/// A keyed land run again is a replay: it finds the changeset its key landed, squashed from the
/// same head, on the target below its head, and answers with it, writing nothing: no new frame,
/// `main` as it was.
#[test]
fn a_land_retried_with_its_key_is_a_replay() {
    let (_dir, root, repo) = new_store();
    let t0 = advance(&repo, ACTOR, &[], vec![put("a", b"a\n")]);
    let head = commit_by(&repo, ACTOR, &[t0], vec![put("b", b"b\n")]);
    let first = repo.land(keyed(head, "key-1")).unwrap();
    // Another land after it, so that the replay is found below the target's head.
    let other = commit_by(&repo, OTHER, &[first.landed], vec![put("c", b"c\n")]);
    let second = repo.land(landing(other)).unwrap();
    let frames = frame_count(&root);

    let again = repo.land(keyed(head, "key-1")).unwrap();
    assert_eq!(again.status, LandStatus::Replayed);
    assert_eq!(
        (again.landed, again.rebased_onto, again.base),
        (first.landed, t0, t0)
    );
    assert_eq!((again.squashed_from, again.files_changed), (vec![head], 1));
    assert_eq!(
        (frame_count(&root), main_head(&repo)),
        (frames, second.landed)
    );
}

/// A key that landed one change refuses another: a land of another head with the same key fails,
/// naming the changeset the key landed, and records nothing.
#[test]
fn a_key_reused_for_another_head_is_refused() {
    let (_dir, root, repo) = new_store();
    let t0 = advance(&repo, ACTOR, &[], vec![put("a", b"a\n")]);
    let one = commit_by(&repo, ACTOR, &[t0], vec![put("b", b"b\n")]);
    let landed = repo.land(keyed(one, "key-1")).unwrap().landed;
    let two = commit_by(&repo, ACTOR, &[t0], vec![put("c", b"c\n")]);
    let frames = frame_count(&root);

    match repo.land(keyed(two, "key-1")) {
        Err(RepoError::KeyReused { key, landed: found }) => {
            assert_eq!((key.as_str(), found), ("key-1", landed));
        }
        other => panic!("expected KeyReused, got {other:?}"),
    }
    assert_eq!((frame_count(&root), main_head(&repo)), (frames, landed));
}

/// A head on the target's first-parent chain, its head or one below it, has nothing to land.
#[test]
fn a_land_of_a_head_on_the_target_is_nothing_to_land() {
    let (_dir, root, repo) = new_store();
    let t0 = advance(&repo, ACTOR, &[], vec![put("a", b"a\n")]);
    let t1 = advance(&repo, ACTOR, &[t0], vec![put("a", b"a 1\n")]);
    let frames = frame_count(&root);
    for head in [t1, t0] {
        match repo.land(landing(head)) {
            Err(RepoError::NothingToLand(NothingToLandReason::OnTarget)) => {}
            other => panic!("{head}: expected NothingToLand(OnTarget), got {other:?}"),
        }
    }
    assert_eq!((frame_count(&root), main_head(&repo)), (frames, t1));
}

/// A change the target holds already has nothing to land: landed once under one key, the same
/// head landed again under another, or with none, finds its change on the target, and records
/// nothing.
#[test]
fn a_land_whose_change_is_on_the_target_already_is_nothing_to_land() {
    let (_dir, root, repo) = new_store();
    let t0 = advance(&repo, ACTOR, &[], vec![put("a", b"a\n")]);
    let head = commit_by(&repo, ACTOR, &[t0], vec![put("b", b"b\n")]);
    let landed = repo.land(keyed(head, "key-1")).unwrap().landed;
    let frames = frame_count(&root);
    for land in [keyed(head, "key-2"), landing(head)] {
        match repo.land(land) {
            Err(RepoError::NothingToLand(NothingToLandReason::NoChanges)) => {}
            other => panic!("expected NothingToLand(NoChanges), got {other:?}"),
        }
    }
    assert_eq!((frame_count(&root), main_head(&repo)), (frames, landed));
}

/// A land squashes a first-parent chain, which may not hold a merge: a changeset with two parents
/// between the base and the head refuses the land, naming it, whether the base is given or not.
#[test]
fn a_chain_with_a_merge_changeset_is_refused() {
    let (_dir, root, repo) = new_store();
    let t0 = advance(&repo, ACTOR, &[], vec![put("a", b"a\n")]);
    let one = commit_by(&repo, ACTOR, &[t0], vec![put("b", b"b\n")]);
    let two = commit_by(&repo, ACTOR, &[t0], vec![put("c", b"c\n")]);
    let merge = commit_by(&repo, ACTOR, &[one, two], vec![put("c", b"c\n")]);
    let head = commit_by(&repo, ACTOR, &[merge], vec![put("d", b"d\n")]);
    let frames = frame_count(&root);
    let mut based = landing(head);
    based.base = Some(t0);
    for land in [landing(head), based] {
        match repo.land(land) {
            Err(RepoError::MergeChangeset(id)) => assert_eq!(id, merge),
            other => panic!("expected MergeChangeset, got {other:?}"),
        }
    }
    assert_eq!((frame_count(&root), main_head(&repo)), (frames, t0));
}

/// A base must be a first-parent ancestor of the head: one off the head's chain (the target's
/// head here) refuses the land, naming both, and nothing is recorded.
#[test]
fn a_base_that_is_not_an_ancestor_is_refused() {
    let (_dir, root, repo) = new_store();
    let t0 = advance(&repo, ACTOR, &[], vec![put("a", b"a\n")]);
    let head = commit_by(&repo, ACTOR, &[t0], vec![put("b", b"b\n")]);
    let x = advance(&repo, OTHER, &[t0], vec![put("c", b"c\n")]);
    let frames = frame_count(&root);
    let mut land = landing(head);
    land.base = Some(x);
    match repo.land(land) {
        Err(RepoError::BaseNotAncestor { base, head: of }) => assert_eq!((base, of), (x, head)),
        other => panic!("expected BaseNotAncestor, got {other:?}"),
    }
    assert_eq!((frame_count(&root), main_head(&repo)), (frames, x));
}

/// A base need not be on the target (M11's form): a change taken from a branch the target never
/// held, its base on that branch, lands on the target's head as the diff from that base to the
/// head, without the base's own change.
#[test]
fn an_explicit_base_off_the_target_lands() {
    let (_dir, _root, repo) = new_store();
    let t0 = advance(&repo, ACTOR, &[], vec![put("a", b"a\n"), put("b", b"b\n")]);
    let t1 = advance(&repo, ACTOR, &[t0], vec![put("t", b"t\n")]);
    let base = commit_by(&repo, OTHER, &[t0], vec![put("a", b"branch\n")]);
    let head = commit_by(&repo, OTHER, &[base], vec![put("b", b"b 2\n")]);

    let mut land = landing(head);
    land.base = Some(base);
    let out = repo.land(land).unwrap();
    assert_eq!(
        (out.base, out.rebased_onto, out.squashed_from.clone()),
        (base, t1, vec![head])
    );
    assert_eq!(content(&repo, out.landed, "a"), Some(b"a\n".to_vec()));
    assert_eq!(content(&repo, out.landed, "b"), Some(b"b 2\n".to_vec()));
    assert_eq!(content(&repo, out.landed, "t"), Some(b"t\n".to_vec()));
}

/// A head that shares no first-parent ancestor with the target's head (a chain from a root of its
/// own) has no base to take by default: the land is refused, naming both, and nothing is
/// recorded. Given a base, it lands what came after it.
#[test]
fn a_head_unrelated_to_the_target_needs_a_base() {
    let (_dir, root, repo) = new_store();
    let t0 = advance(&repo, ACTOR, &[], vec![put("a", b"a\n")]);
    let other_root = commit_by(&repo, OTHER, &[], vec![put("z", b"z\n")]);
    let head = commit_by(&repo, OTHER, &[other_root], vec![put("y", b"y\n")]);
    let frames = frame_count(&root);
    match repo.land(landing(head)) {
        Err(RepoError::NoMergeBase { head: of, theirs }) => assert_eq!((of, theirs), (head, t0)),
        other => panic!("expected NoMergeBase, got {other:?}"),
    }
    assert_eq!(frame_count(&root), frames);

    let mut land = landing(head);
    land.base = Some(other_root);
    let out = repo.land(land).unwrap();
    assert_eq!(content(&repo, out.landed, "y"), Some(b"y\n".to_vec()));
    assert_eq!(content(&repo, out.landed, "z"), None);
    assert_eq!(content(&repo, out.landed, "a"), Some(b"a\n".to_vec()));
}

/// Counts the reads of the object files it watches, each named by its fan-out directory and its
/// file name.
#[derive(Default)]
struct ObjectReads {
    watched: Mutex<HashSet<(String, String)>>,
    reads: AtomicUsize,
}

impl ObjectReads {
    /// Watches the object file of changeset `id` in the store at `root`.
    fn watch(&self, root: &Utf8Path, id: ChangesetId) {
        let file = Layout::new(root).object_path(id.0);
        let named = last_two(file.as_std_path());
        self.watched.lock().unwrap().insert(named);
    }
}

/// The names of `path`'s parent directory and of its file.
fn last_two(path: &std::path::Path) -> (String, String) {
    let name = |p: Option<&std::path::Path>| {
        p.and_then(std::path::Path::file_name)
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    (name(path.parent()), name(Some(path)))
}

impl Watch for ObjectReads {
    fn before(&self, call: &Call<'_>) -> io::Result<()> {
        if call.method == "read"
            && self
                .watched
                .lock()
                .unwrap()
                .contains(&last_two(call.path()))
        {
            self.reads.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }
}

/// A keyed land whose base is off the target (M11's form) looks for its key only down to the
/// target's first changeset at or below the base's generation: on a target of 1,000 changesets
/// whose head is 3 generations above the base, it reads at most 6 of the target's changesets, and
/// lands. Its head and the one below it, as the check whether the change's head is on the target
/// walks down to that head's generation, 2 below the target's (each step of a first-parent walk
/// reads the changeset's object); then the target's head and the 3 below it, for the key. The
/// store is built without syncs, and read through a watcher that counts the reads of the target's
/// changeset objects.
#[test]
fn a_keyed_land_with_a_base_off_the_target_stops_at_its_generation() {
    let dir = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
    let repo = LocalRepo::create_with_fs(&root, "test", Arc::new(NoSyncFs)).unwrap();
    let mut chain = vec![commit_by(&repo, ACTOR, &[], vec![put("f", b"0\n")])];
    while chain.len() < 1_000 {
        let n = chain.len();
        let mut empty = by(ACTOR, &[chain[n - 1]], Vec::new());
        empty.allow_empty = true;
        empty.message = format!("t{n}").into_bytes();
        chain.push(repo.commit(empty).unwrap().id);
    }
    let top = chain[999];
    repo.cas_ref("main", None, Some(top), ACTOR).unwrap();
    // A branch off the target's changeset of generation 996: its first changeset, of generation
    // 997, three below the target's head (1,000), is the base.
    let base = commit_by(&repo, OTHER, &[chain[995]], vec![put("g", b"g\n")]);
    let head = commit_by(&repo, OTHER, &[base], vec![put("h", b"h\n")]);
    drop(repo);

    let reads = Arc::new(ObjectReads::default());
    for &id in &chain {
        reads.watch(&root, id);
    }
    let fs: Arc<dyn FsOps> = Arc::new(Watched(Arc::clone(&reads), NoSyncFs));
    let repo = LocalRepo::open_with_fs(&root, fs).unwrap();
    let mut land = keyed(head, "key-1");
    land.base = Some(base);
    let out = repo.land(land).unwrap();
    assert_eq!(
        (out.status, out.rebased_onto, out.base),
        (LandStatus::Landed, top, base)
    );
    let read = reads.reads.load(Ordering::SeqCst);
    assert!(
        (1..=6).contains(&read),
        "the land read {read} of the target's changesets"
    );
}

/// A land onto a ref that does not exist, or onto a name that is no ref name, is refused, and
/// nothing is recorded.
#[test]
fn a_land_onto_a_missing_target_is_not_found() {
    let (_dir, root, repo) = new_store();
    let t0 = advance(&repo, ACTOR, &[], vec![put("a", b"a\n")]);
    let head = commit_by(&repo, ACTOR, &[t0], vec![put("b", b"b\n")]);
    let frames = frame_count(&root);
    match repo.land(LandRequest::new(head, "release/none", lander())) {
        Err(RepoError::NotFound(what)) => assert_eq!(what, "release/none"),
        other => panic!("expected NotFound, got {other:?}"),
    }
    match repo.land(LandRequest::new(head, "feature/x", lander())) {
        Err(RepoError::RefName(name)) => assert_eq!(name, "feature/x"),
        other => panic!("expected RefName, got {other:?}"),
    }
    assert_eq!(frame_count(&root), frames);
}

/// Races a land as another lander would: at the land's first flush of objects after each call it
/// makes on the metadata (its first `fsync_dir` under `objects/` while it writes a changeset's
/// objects, outside the lock), it lands `change(n)`, the n-th race's change from 0, on `main`'s
/// head through a handle of its own on the real filesystem, which the land does not see, until it
/// has run `races` races.
struct Racer {
    root: Utf8PathBuf,
    change: fn(usize) -> Vec<Change>,
    races: usize,
    /// Whether the land made a call on the metadata since the last race: a new attempt's objects
    /// are due.
    armed: AtomicBool,
    /// What each race landed, in order.
    landed: Mutex<Vec<ChangesetId>>,
}

impl Racer {
    /// A racer of the store at `root`, which runs `races` races.
    fn new(root: &Utf8Path, races: usize, change: fn(usize) -> Vec<Change>) -> Arc<Racer> {
        Arc::new(Racer {
            root: root.to_owned(),
            change,
            races,
            armed: AtomicBool::new(true),
            landed: Mutex::new(Vec::new()),
        })
    }

    /// What the races landed, in order.
    fn landed(&self) -> Vec<ChangesetId> {
        self.landed.lock().unwrap().clone()
    }

    /// Runs the next race, if one is left.
    fn race(&self) {
        let mut landed = self.landed.lock().unwrap();
        if landed.len() >= self.races {
            return;
        }
        let other = LocalRepo::open_with_fs(&self.root, checked(RealFs)).unwrap();
        let (head, n) = (main_head(&other), landed.len());
        landed.push(advance(&other, OTHER, &[head], (self.change)(n)));
    }

    /// A handle on the store whose lands this racer races, under the order checker.
    fn open(self: &Arc<Self>) -> LocalRepo {
        LocalRepo::open_with_fs(&self.root, checked(Watched(Arc::clone(self), RealFs))).unwrap()
    }
}

impl Watch for Racer {
    fn before(&self, call: &Call<'_>) -> io::Result<()> {
        let at = call.path().strip_prefix(&self.root).unwrap_or(call.path());
        let under = |dir: &str| at.components().next().is_some_and(|c| c.as_os_str() == dir);
        if under("meta") {
            self.armed.store(true, Ordering::SeqCst);
        } else if call.method == "fsync_dir"
            && under("objects")
            && self.armed.swap(false, Ordering::SeqCst)
        {
            self.race();
        }
        Ok(())
    }
}

/// A land that loses the race to the target (another lander moved it while the land wrote its
/// changeset's objects) records nothing of that attempt, re-merges what it merged onto what the
/// target gained, and lands on the next: `L` on the racer's changeset, with both changes.
#[test]
fn a_land_that_loses_a_race_re_merges_and_lands_on_the_next_attempt() {
    let (_dir, root, repo) = new_store();
    let t0 = advance(&repo, ACTOR, &[], vec![put("a", b"a\n"), put("f", b"f\n")]);
    let head = commit_by(&repo, ACTOR, &[t0], vec![put("a", b"ours\n")]);
    drop(repo);
    let racer = Racer::new(&root, 1, |n| vec![put(&format!("race/{n}"), b"theirs\n")]);
    let repo = racer.open();

    let out = repo.land(landing(head)).unwrap();
    let gained = racer.landed();
    assert_eq!(gained.len(), 1, "one race");
    assert_eq!(
        (out.status, out.attempts, out.rebased_onto, out.base),
        (LandStatus::Landed, 2, gained[0], t0)
    );
    assert_eq!(first_parent(&repo, out.landed), Some(gained[0]));
    assert_eq!(content(&repo, out.landed, "a"), Some(b"ours\n".to_vec()));
    assert_eq!(
        content(&repo, out.landed, "race/0"),
        Some(b"theirs\n".to_vec())
    );
    assert_eq!(main_head(&repo), out.landed);
    assert_eq!(repo.verify(Depth::Deep).unwrap().problems, []);
}

/// A re-merge after a lost race that conflicts reports the whole three-way merge: the change
/// (`ours`, from the land's base) against what the target holds now (`theirs`, the racer's
/// changeset), whose agent last touched the file; and nothing is recorded.
#[test]
fn the_re_merge_after_a_lost_race_conflicts_with_a_full_report() {
    let (_dir, root, repo) = new_store();
    let t0 = advance(&repo, ACTOR, &[], vec![put("f", b"1\n2\n3\n")]);
    let head = commit_by(&repo, ACTOR, &[t0], vec![put("f", b"1\nours\n3\n")]);
    drop(repo);
    let racer = Racer::new(&root, 1, |_| vec![put("f", b"1\ntheirs\n3\n")]);
    let repo = racer.open();

    let conflicts = conflicts_of(repo.land(landing(head)));
    let gained = racer.landed();
    assert_eq!(gained.len(), 1, "one race");
    assert_eq!(
        (conflicts.base, conflicts.ours, conflicts.theirs),
        (t0, head, gained[0])
    );
    assert_eq!(kinds(&conflicts), [("f", "content")]);
    assert_eq!(
        conflicts.last_touched.get(&path("f")),
        Some(&(gained[0], OTHER.to_owned()))
    );
    assert_eq!(main_head(&repo), gained[0]);
}

/// A land that loses every race stops at its last attempt: with `max_attempts` 3 and a racer that
/// moves the target before each publish, it fails after its third, naming what the target names
/// then, and records nothing.
#[test]
fn retries_stop_at_max_attempts() {
    let (_dir, root, repo) = new_store();
    let t0 = advance(&repo, ACTOR, &[], vec![put("a", b"a\n")]);
    let head = commit_by(&repo, ACTOR, &[t0], vec![put("a", b"ours\n")]);
    drop(repo);
    let racer = Racer::new(&root, 10, |n| vec![put(&format!("race/{n}"), b"theirs\n")]);
    let repo = racer.open();

    let mut land = landing(head);
    land.max_attempts = 3;
    let out = repo.land(land);
    let gained = racer.landed();
    assert_eq!(gained.len(), 3, "a race before each publish");
    match out {
        Err(RepoError::RetryExhausted { attempts, actual }) => {
            assert_eq!((attempts, actual), (3, gained[2]));
        }
        other => panic!("expected RetryExhausted, got {other:?}"),
    }
    assert_eq!(main_head(&repo), gained[2]);
}

/// The backoff before a land's attempt (operations spec §3 step 7): none before the fourth; from
/// it, `min(2 s, 25 ms·2^attempt)` times a factor in [0.5, 1.5) drawn from the seed and the
/// attempt, the same for the same two.
#[test]
fn the_backoff_follows_ops_spec_step_7() {
    for attempt in 1..=3 {
        assert_eq!(
            backoff(attempt, b"seed"),
            Duration::ZERO,
            "attempt {attempt}"
        );
    }
    let seeds: [&[u8]; 4] = [b"a", b"b", b"key-1", &[7; 32]];
    for seed in seeds {
        let fourth = backoff(4, seed);
        let in_range = Duration::from_millis(200) <= fourth && fourth < Duration::from_millis(600);
        assert!(in_range, "{fourth:?}");
        let seventh = backoff(7, seed);
        let in_range = Duration::from_secs(1) <= seventh && seventh < Duration::from_secs(3);
        assert!(in_range, "{seventh:?}");
        assert_eq!(backoff(4, seed), fourth, "one seed, one value");
    }
    let draws: HashSet<Duration> = (0..32_u8).map(|seed| backoff(5, &[seed])).collect();
    assert!(
        draws.len() > 16,
        "the factor varies with the seed: {draws:?}"
    );
    assert!(
        backoff(32, b"x") < Duration::from_secs(3),
        "at most 2 s, times the factor"
    );
}

/// `Repo::merge` and `Repo::merge_trees` merge with the three-way text merge: edits apart in one
/// file merge into new content, and edits of one line conflict.
#[test]
fn merge_and_merge_trees_merge_text_three_way() {
    let (_dir, _root, repo) = new_store();
    let base = commit_by(&repo, ACTOR, &[], vec![put("f", b"1\n2\n3\n4\n5\n")]);
    let ours = commit_by(&repo, ACTOR, &[base], vec![put("f", b"one\n2\n3\n4\n5\n")]);
    let theirs = commit_by(&repo, OTHER, &[base], vec![put("f", b"1\n2\n3\n4\nfive\n")]);
    let merged = repo.merge(base, ours, theirs).unwrap();
    assert_eq!(merged.auto_merged, [path("f")]);
    let contents: Vec<&[u8]> = merged.new_blobs.values().map(Vec::as_slice).collect();
    assert_eq!(contents, [b"one\n2\n3\n4\nfive\n".as_slice()]);

    let clash = commit_by(&repo, OTHER, &[base], vec![put("f", b"uno\n2\n3\n4\n5\n")]);
    let root = |id| repo.changeset(id).unwrap().unwrap().changeset.root;
    let paths = [path("f")];
    let conflicted = repo
        .merge_trees(root(base), root(ours), root(clash), &paths)
        .unwrap();
    let names: Vec<(&str, &str)> = (conflicted.conflicts.iter())
        .map(|conflict| (conflict.path.as_str(), conflict.kind.name()))
        .collect();
    assert_eq!(names, [("f", "content")]);
}

/// Resolutions (operations spec §3, resolution path (b); the M4 plan's Decision 9): a land told
/// what to make of some paths, against the target's head a conflict report named, applies them to
/// each attempt's merge before its conflict check, while the target holds those paths as that
/// head did; a path it holds otherwise is stale, and fails the land with a fresh report.
mod resolutions {
    use swarmops_core::{BlobId, LeafKind};
    use swarmops_repo::land::{Resolution, Resolutions, Side};
    use swarmops_testkit::repo::put_as;

    use super::*;

    /// [`landing`] of `head`, with `files` resolved against `against`.
    fn resolving(
        head: ChangesetId,
        against: ChangesetId,
        files: Vec<(&str, Resolution)>,
    ) -> LandRequest {
        let files = (files.into_iter())
            .map(|(p, resolution)| (path(p), resolution))
            .collect();
        let mut land = landing(head);
        land.resolutions = Some(Resolutions::new(against, files));
        land
    }

    /// A resolution that puts `bytes`, of the kind the land picks.
    fn new_content(bytes: &[u8]) -> Resolution {
        Resolution::Put {
            kind: None,
            content: bytes.to_vec(),
        }
    }

    /// The kind of the leaf at `p` in changeset `id`'s tree; `None` if there is none.
    fn kind_at(repo: &LocalRepo, id: ChangesetId, p: &str) -> Option<LeafKind> {
        (repo.read_blob_at(id, &path(p)).unwrap()).map(|(kind, _, _)| kind)
    }

    /// Each kind of resolution lands: of four files that conflict, one takes ours, one takes
    /// theirs, one takes new content and one is deleted. `L`, on the head the report named, holds
    /// what each says (theirs' file needs no change in its delta), no path it resolved is
    /// reported auto-merged, and the store verifies.
    #[test]
    fn each_kind_of_resolution_lands() {
        let (_dir, _root, repo) = new_store();
        let files = ["a", "b", "c", "d"];
        let each = |content: &[u8]| files.iter().map(|f| put(f, content)).collect::<Vec<_>>();
        let t0 = advance(&repo, ACTOR, &[], each(b"1\n2\n3\n"));
        let head = commit_by(&repo, ACTOR, &[t0], each(b"1\nours\n3\n"));
        let x = advance(&repo, OTHER, &[t0], each(b"1\ntheirs\n3\n"));
        let conflicts = conflicts_of(repo.land(landing(head)));
        assert_eq!(
            kinds(&conflicts),
            [
                ("a", "content"),
                ("b", "content"),
                ("c", "content"),
                ("d", "content")
            ]
        );
        assert_eq!((conflicts.theirs, conflicts.stale.clone()), (x, Vec::new()));

        let land = resolving(
            head,
            conflicts.theirs,
            vec![
                ("a", Resolution::Take(Side::Ours)),
                ("b", Resolution::Take(Side::Theirs)),
                ("c", new_content(b"1\nboth\n3\n")),
                ("d", Resolution::Delete),
            ],
        );
        let out = repo.land(land).unwrap();
        assert_eq!(
            (out.status, out.rebased_onto, out.attempts),
            (LandStatus::Landed, x, 1)
        );
        assert_eq!(main_head(&repo), out.landed);
        assert_eq!(
            content(&repo, out.landed, "a"),
            Some(b"1\nours\n3\n".to_vec())
        );
        assert_eq!(
            content(&repo, out.landed, "b"),
            Some(b"1\ntheirs\n3\n".to_vec())
        );
        assert_eq!(
            content(&repo, out.landed, "c"),
            Some(b"1\nboth\n3\n".to_vec())
        );
        assert_eq!(content(&repo, out.landed, "d"), None);
        let delta = repo.changeset(out.landed).unwrap().unwrap().changeset.delta;
        let paths: Vec<&str> = delta.iter().map(|entry| entry.path.as_str()).collect();
        assert_eq!(paths, ["a", "c", "d"], "b keeps theirs");
        assert_eq!(out.files_changed, 3);
        assert_eq!(out.auto_merged, Vec::<RepoPath>::new());
        assert_eq!(repo.verify(Depth::Deep).unwrap().problems, []);
    }

    /// A content resolution's leaf is of the kind it gives; else of the kind of the leaf ours holds
    /// at its path, else theirs', else a file.
    #[test]
    fn a_content_resolution_s_kind_is_its_own_else_ours_else_theirs_else_a_file() {
        let (_dir, _root, repo) = new_store();
        let base = b"1\n2\n3\n";
        let t0 = advance(
            &repo,
            ACTOR,
            &[],
            vec![put("e", base), put("f", base), put("g", base)],
        );
        // Ours makes e executable and edits it, deletes f, edits g.
        let ours = vec![
            put_as("e", LeafKind::Exec, b"1\nours\n3\n"),
            delete("f"),
            put("g", b"1\nours\n3\n"),
        ];
        let head = commit_by(&repo, ACTOR, &[t0], ours);
        // Theirs edits e, makes f executable and edits it, edits g.
        let theirs = vec![
            put("e", b"1\ntheirs\n3\n"),
            put_as("f", LeafKind::Exec, b"1\ntheirs\n3\n"),
            put("g", b"1\ntheirs\n3\n"),
        ];
        let x = advance(&repo, OTHER, &[t0], theirs);
        let conflicts = conflicts_of(repo.land(landing(head)));
        assert_eq!(
            kinds(&conflicts),
            [("e", "content"), ("f", "delete_modify"), ("g", "content")]
        );

        let symlink = Resolution::Put {
            kind: Some(LeafKind::Symlink),
            content: b"e".to_vec(),
        };
        let land = resolving(
            head,
            x,
            vec![
                ("e", new_content(b"e\n")),
                ("f", new_content(b"f\n")),
                ("g", symlink),
                ("n", new_content(b"n\n")),
            ],
        );
        let out = repo.land(land).unwrap();
        let landed = |p| kind_at(&repo, out.landed, p);
        assert_eq!(
            [landed("e"), landed("f"), landed("g"), landed("n")],
            [
                Some(LeafKind::Exec),
                Some(LeafKind::Exec),
                Some(LeafKind::Symlink),
                Some(LeafKind::File)
            ],
            "e is ours' kind, f theirs' (ours has none), g its own, n a file (neither has one)"
        );
        assert_eq!(content(&repo, out.landed, "n"), Some(b"n\n".to_vec()));
    }

    /// A resolution made against a head of the target that held its path otherwise than the
    /// target's head does now is stale: the land fails with a fresh report, the merge onto the
    /// head now, whose `stale` names the path (which still conflicts, so is a conflict of the
    /// report too), and records nothing. Made against the head now, the same resolution lands.
    #[test]
    fn a_resolution_against_a_stale_theirs_is_refused_with_a_fresh_report() {
        let (_dir, root, repo) = new_store();
        let t0 = advance(
            &repo,
            ACTOR,
            &[],
            vec![put("f", b"1\n2\n3\n"), put("g", b"g\n")],
        );
        let head = commit_by(&repo, ACTOR, &[t0], vec![put("f", b"1\nours\n3\n")]);
        let x = advance(&repo, OTHER, &[t0], vec![put("f", b"1\ntheirs\n3\n")]);
        assert_eq!(conflicts_of(repo.land(landing(head))).theirs, x);
        // The target changes f again after the report.
        let y = advance(
            &repo,
            "test/third",
            &[x],
            vec![put("f", b"1\ntheirs\nthree\n")],
        );
        let (frames, before) = (frame_count(&root), main_head(&repo));

        let resolved =
            |against| resolving(head, against, vec![("f", new_content(b"1\nboth\n3\n"))]);
        let conflicts = conflicts_of(repo.land(resolved(x)));
        assert_eq!(
            (conflicts.base, conflicts.ours, conflicts.theirs),
            (t0, head, y)
        );
        assert_eq!(conflicts.stale, [path("f")]);
        assert_eq!(kinds(&conflicts), [("f", "content")], "f conflicts with y");
        assert_eq!(
            conflicts.last_touched.get(&path("f")),
            Some(&(y, "test/third".to_owned()))
        );
        assert_eq!((frame_count(&root), main_head(&repo)), (frames, before));

        let out = repo.land(resolved(y)).unwrap();
        assert_eq!((out.rebased_onto, out.attempts), (y, 1));
        assert_eq!(
            content(&repo, out.landed, "f"),
            Some(b"1\nboth\n3\n".to_vec())
        );
    }

    /// A stale resolution fails the land also where the merge onto the target's head now is
    /// clean: the target took its change of the path back after the report, so that the change
    /// merges there, but the lander decided the path against what the target held before, and
    /// the report says so, with no conflict; nothing is recorded.
    #[test]
    fn a_stale_resolution_fails_the_land_where_the_merge_is_clean() {
        let (_dir, root, repo) = new_store();
        let t0 = advance(&repo, ACTOR, &[], vec![put("f", b"1\n2\n3\n")]);
        let head = commit_by(&repo, ACTOR, &[t0], vec![put("f", b"1\nours\n3\n")]);
        let x = advance(&repo, OTHER, &[t0], vec![put("f", b"1\ntheirs\n3\n")]);
        let y = advance(&repo, OTHER, &[x], vec![put("f", b"1\n2\n3\n")]);
        let (frames, before) = (frame_count(&root), main_head(&repo));

        let land = resolving(head, x, vec![("f", new_content(b"1\nboth\n3\n"))]);
        let conflicts = conflicts_of(repo.land(land));
        assert_eq!(conflicts.theirs, y);
        assert_eq!(conflicts.stale, [path("f")]);
        assert_eq!(kinds(&conflicts), [], "the merge onto y is clean");
        assert_eq!((frame_count(&root), main_head(&repo)), (frames, before));
    }

    /// A resolution applies while the target holds its path as `against` did, whatever else the
    /// target gained since: made against the head a report named, it lands on the head the target
    /// moved to after, which changed a file beside the resolved one, in its directory.
    #[test]
    fn a_resolution_applies_while_the_target_holds_its_path_as_against_did() {
        let (_dir, _root, repo) = new_store();
        let t0 = advance(
            &repo,
            ACTOR,
            &[],
            vec![put("d/f", b"1\n2\n3\n"), put("d/g", b"g\n")],
        );
        let head = commit_by(&repo, ACTOR, &[t0], vec![put("d/f", b"1\nours\n3\n")]);
        let x = advance(&repo, OTHER, &[t0], vec![put("d/f", b"1\ntheirs\n3\n")]);
        assert_eq!(conflicts_of(repo.land(landing(head))).theirs, x);
        let y = advance(&repo, "test/third", &[x], vec![put("d/g", b"g 2\n")]);

        let land = resolving(head, x, vec![("d/f", Resolution::Take(Side::Ours))]);
        let out = repo.land(land).unwrap();
        assert_eq!(out.rebased_onto, y);
        assert_eq!(
            content(&repo, out.landed, "d/f"),
            Some(b"1\nours\n3\n".to_vec())
        );
        assert_eq!(content(&repo, out.landed, "d/g"), Some(b"g 2\n".to_vec()));
    }

    /// A land that resolves some of its conflicts fails with the others: the report lists the
    /// conflicts left, not the one resolved, and no path stale; nothing is recorded.
    #[test]
    fn a_missing_resolution_reports_the_remaining_conflicts() {
        let (_dir, root, repo) = new_store();
        let both = |content: &[u8]| vec![put("f", content), put("g", content)];
        let t0 = advance(&repo, ACTOR, &[], both(b"1\n2\n3\n"));
        let head = commit_by(&repo, ACTOR, &[t0], both(b"1\nours\n3\n"));
        let x = advance(&repo, OTHER, &[t0], both(b"1\ntheirs\n3\n"));
        let (frames, before) = (frame_count(&root), main_head(&repo));

        let land = resolving(head, x, vec![("f", Resolution::Take(Side::Theirs))]);
        let conflicts = conflicts_of(repo.land(land));
        assert_eq!(kinds(&conflicts), [("g", "content")]);
        assert_eq!(conflicts.stale, Vec::<RepoPath>::new());
        assert_eq!(conflicts.theirs, x);
        assert_eq!(
            conflicts.last_touched.keys().collect::<Vec<_>>(),
            [&path("g")]
        );
        assert_eq!((frame_count(&root), main_head(&repo)), (frames, before));
    }

    /// A resolution applies whether its path conflicts or not: one for a file both sides edited
    /// apart, which merges cleanly, and one for a file neither side changed put their contents in
    /// `L`, and one that takes theirs for a file the merge merged leaves theirs' content, not the
    /// merge's; none of them is reported auto-merged.
    #[test]
    fn a_resolution_for_a_path_that_merges_cleanly_still_applies() {
        let (_dir, _root, repo) = new_store();
        let five = b"1\n2\n3\n4\n5\n";
        let base = vec![
            put("f", b"1\n2\n3\n"),
            put("h", five),
            put("k", five),
            put("u", b"u\n"),
        ];
        let t0 = advance(&repo, ACTOR, &[], base);
        let ours = vec![
            put("f", b"1\nours\n3\n"),
            put("h", b"one\n2\n3\n4\n5\n"),
            put("k", b"one\n2\n3\n4\n5\n"),
        ];
        let head = commit_by(&repo, ACTOR, &[t0], ours);
        let theirs = vec![
            put("f", b"1\ntheirs\n3\n"),
            put("h", b"1\n2\n3\n4\nfive\n"),
            put("k", b"1\n2\n3\n4\nfive\n"),
        ];
        let x = advance(&repo, OTHER, &[t0], theirs);
        let conflicts = conflicts_of(repo.land(landing(head)));
        assert_eq!(kinds(&conflicts), [("f", "content")]);
        assert_eq!(conflicts.auto_merged, [path("h"), path("k")]);

        let land = resolving(
            head,
            x,
            vec![
                ("f", Resolution::Take(Side::Ours)),
                ("h", new_content(b"h, resolved\n")),
                ("k", Resolution::Take(Side::Theirs)),
                ("u", new_content(b"u, resolved\n")),
            ],
        );
        let out = repo.land(land).unwrap();
        let landed = |p| content(&repo, out.landed, p);
        assert_eq!(landed("f"), Some(b"1\nours\n3\n".to_vec()));
        assert_eq!(landed("h"), Some(b"h, resolved\n".to_vec()));
        assert_eq!(
            landed("k"),
            Some(b"1\n2\n3\n4\nfive\n".to_vec()),
            "theirs' k, not the merged one"
        );
        assert_eq!(landed("u"), Some(b"u, resolved\n".to_vec()));
        assert_eq!(out.auto_merged, Vec::<RepoPath>::new());
    }

    /// A resolution decides its path and every path below it: where the change put a file and
    /// the target made a directory (a type change), taking ours leaves the file and none of the
    /// directory's files, taking theirs the directory and not the file, new content a file of
    /// it, and a delete neither.
    #[test]
    fn a_resolution_decides_its_path_and_every_path_below_it() {
        let file = Some(b"a file\n".to_vec());
        let below = Some(b"x\n".to_vec());
        for (resolution, d, d_x) in [
            (Resolution::Take(Side::Ours), file.clone(), None),
            (Resolution::Take(Side::Theirs), None, below),
            (new_content(b"new\n"), Some(b"new\n".to_vec()), None),
            (Resolution::Delete, None, None),
        ] {
            let (_dir, _root, repo) = new_store();
            let t0 = advance(&repo, ACTOR, &[], vec![put("a", b"a\n")]);
            let ours = vec![put("a", b"a 2\n"), put("d", b"a file\n")];
            let head = commit_by(&repo, ACTOR, &[t0], ours);
            let theirs = vec![put("d/x", b"x\n"), put("d/y", b"y\n")];
            let x = advance(&repo, OTHER, &[t0], theirs);
            let conflicts = conflicts_of(repo.land(landing(head)));
            assert_eq!(kinds(&conflicts), [("d", "type_change")]);

            let what = format!("{resolution:?}");
            let out = repo
                .land(resolving(head, x, vec![("d", resolution)]))
                .unwrap();
            assert_eq!(content(&repo, out.landed, "d"), d, "{what}");
            assert_eq!(content(&repo, out.landed, "d/x"), d_x.clone(), "{what}");
            assert_eq!(
                content(&repo, out.landed, "d/y"),
                d_x.map(|_| b"y\n".to_vec())
            );
            assert_eq!(content(&repo, out.landed, "a"), Some(b"a 2\n".to_vec()));
            assert_eq!(repo.verify(Depth::Deep).unwrap().problems, [], "{what}");
        }
    }

    /// A resolution of a directory resolves the conflicts below it: taking ours for `src`, where
    /// two files conflict, lands ours' files there, and leaves what the merge made beside it, in
    /// `src2`, whose name starts as the directory's does.
    #[test]
    fn a_resolution_of_a_directory_resolves_the_conflicts_below_it() {
        let (_dir, _root, repo) = new_store();
        let each = |content: &[u8]| vec![put("src/a", content), put("src/b", content)];
        let mut base = each(b"1\n2\n3\n");
        base.push(put("src2", b"s\n"));
        let t0 = advance(&repo, ACTOR, &[], base);
        let mut ours = each(b"1\nours\n3\n");
        ours.push(put("src2", b"s 2\n"));
        let head = commit_by(&repo, ACTOR, &[t0], ours);
        let x = advance(&repo, OTHER, &[t0], each(b"1\ntheirs\n3\n"));
        let conflicts = conflicts_of(repo.land(landing(head)));
        assert_eq!(
            kinds(&conflicts),
            [("src/a", "content"), ("src/b", "content")]
        );

        let land = resolving(head, x, vec![("src", Resolution::Take(Side::Ours))]);
        let out = repo.land(land).unwrap();
        for p in ["src/a", "src/b"] {
            let landed = content(&repo, out.landed, p);
            assert_eq!(landed, Some(b"1\nours\n3\n".to_vec()), "{p}");
        }
        assert_eq!(content(&repo, out.landed, "src2"), Some(b"s 2\n".to_vec()));
    }

    /// A resolution's content can be a blob the store holds, named by its id, which the land
    /// neither reads nor writes again. An id the store holds no blob under fails the land before
    /// its first attempt, naming the path and the id, and records nothing.
    #[test]
    fn a_resolution_names_a_stored_blob_by_its_id() {
        let (_dir, root, repo) = new_store();
        let t0 = advance(&repo, ACTOR, &[], vec![put("f", b"1\n2\n3\n")]);
        let head = commit_by(&repo, ACTOR, &[t0], vec![put("f", b"1\nours\n3\n")]);
        let x = advance(&repo, OTHER, &[t0], vec![put("f", b"1\ntheirs\n3\n")]);
        let (frames, before) = (frame_count(&root), main_head(&repo));
        let named = |id| {
            let resolution = Resolution::PutBlob { kind: None, id };
            resolving(head, x, vec![("f", resolution)])
        };

        let never = blob(b"never stored\n");
        let tree = repo.changeset(x).unwrap().unwrap().changeset.root;
        for id in [never, BlobId(tree.0)] {
            match repo.land(named(id)) {
                Err(RepoError::MissingBlob {
                    path: at,
                    id: named,
                }) => {
                    assert_eq!((at, named), (path("f"), id));
                }
                other => panic!("expected MissingBlob, got {other:?}"),
            }
        }
        assert_eq!((frame_count(&root), main_head(&repo)), (frames, before));

        let stored = repo.put_blob(b"1\nstored\n3\n".to_vec()).unwrap();
        let out = repo.land(named(stored.id)).unwrap();
        assert_eq!(
            content(&repo, out.landed, "f"),
            Some(b"1\nstored\n3\n".to_vec())
        );
        assert_eq!(kind_at(&repo, out.landed, "f"), Some(LeafKind::File));
    }

    /// Resolutions apply in every attempt: a land whose first publish loses its race to a change
    /// elsewhere applies them again to its re-merge onto the target's new head, and lands there.
    #[test]
    fn resolutions_apply_again_after_a_lost_race() {
        let (_dir, root, repo) = new_store();
        let t0 = advance(&repo, ACTOR, &[], vec![put("f", b"1\n2\n3\n")]);
        let head = commit_by(&repo, ACTOR, &[t0], vec![put("f", b"1\nours\n3\n")]);
        let x = advance(&repo, OTHER, &[t0], vec![put("f", b"1\ntheirs\n3\n")]);
        drop(repo);
        let racer = Racer::new(&root, 1, |n| vec![put(&format!("race/{n}"), b"theirs\n")]);
        let repo = racer.open();

        let land = resolving(head, x, vec![("f", new_content(b"1\nboth\n3\n"))]);
        let out = repo.land(land).unwrap();
        let gained = racer.landed();
        assert_eq!(gained.len(), 1, "one race");
        assert_eq!((out.attempts, out.rebased_onto), (2, gained[0]));
        assert_eq!(
            content(&repo, out.landed, "f"),
            Some(b"1\nboth\n3\n".to_vec())
        );
        assert_eq!(
            content(&repo, out.landed, "race/0"),
            Some(b"theirs\n".to_vec())
        );
        assert_eq!(repo.verify(Depth::Deep).unwrap().problems, []);
    }
}

/// A replay is decided under the store's lock, after the write's tail check, as a keyed commit's
/// is: a land whose frame is whole but not listed in `journal.off` (its listing failed after the
/// commit point, which the land reports as a warning) is found by its key, and the replay lists
/// that frame before it answers, so that it never answers with a land that a failed sync could
/// still take back.
#[test]
fn a_replay_is_decided_under_the_lock_after_the_tail_check() {
    let (_dir, root, repo) = new_store();
    let t0 = advance(&repo, ACTOR, &[], vec![put("a", b"a\n")]);
    let head = commit_by(&repo, ACTOR, &[t0], vec![put("b", b"b\n")]);
    drop(repo);
    let unlisting =
        FailingFs::new(|method, at| method == "append_durable" && at.ends_with("journal.off"));
    let repo = LocalRepo::open_with_fs(&root, Arc::new(unlisting)).unwrap();
    let first = repo.land(keyed(head, "key-1")).unwrap();
    assert_eq!(first.status, LandStatus::Landed);
    assert_eq!(first.warnings.len(), 1, "{:?}", first.warnings);
    drop(repo);
    let listed = frame_count(&root);

    let repo = LocalRepo::open_with_fs(&root, checked(RealFs)).unwrap();
    let again = repo.land(keyed(head, "key-1")).unwrap();
    assert_eq!(
        (again.status, again.landed),
        (LandStatus::Replayed, first.landed)
    );
    assert_eq!(
        frame_count(&root),
        listed + 1,
        "the replay's write listed the land's frame before it answered"
    );
}

/// What the crash and failure sweeps land: on a store whose `main` is `t0`, an agent's commit on
/// it, `head`, with [`SWEPT_KEY`]; `want` is the changeset the land makes.
#[cfg(feature = "test-hooks")]
#[derive(Debug, Clone, Copy)]
struct Swept {
    t0: ChangesetId,
    head: ChangesetId,
    want: ChangesetId,
}

/// The key the sweeps land with.
#[cfg(feature = "test-hooks")]
const SWEPT_KEY: &str = "swept-key-1";

/// Set only in the children the sweeps spawn: the store root.
#[cfg(feature = "test-hooks")]
const CHILD_ROOT: &str = "SWARMOPS_TEST_REPO_LAND_CHILD_ROOT";

/// Set with [`CHILD_ROOT`]: the head the child lands, as a full id.
#[cfg(feature = "test-hooks")]
const CHILD_HEAD: &str = "SWARMOPS_TEST_REPO_LAND_CHILD_HEAD";

/// How a child of the sweeps reports how its land ended, among the harness's output.
#[cfg(feature = "test-hooks")]
const REPORT: &str = "land-child: ";

/// The store and the head this process lands, if it is a child one of the sweeps spawned.
#[cfg(feature = "test-hooks")]
fn child() -> Option<(Utf8PathBuf, ChangesetId)> {
    let root = std::env::var(CHILD_ROOT).ok()?;
    let head = std::env::var(CHILD_HEAD).expect("the head to land");
    let head = ChangesetId(ObjectId::from_hex(&head).expect("a full id"));
    Some((Utf8PathBuf::from(root), head))
}

/// A child's part of the sweeps: lands `head` with [`SWEPT_KEY`] on the store at `root`, opened
/// with the test hooks the environment sets (a call that crashes, or fails, and the order
/// checker), and reports how the land ended: `landed <id> warnings <n>`, or `failed <error>`.
#[cfg(feature = "test-hooks")]
fn land_as_child(root: &Utf8Path, head: ChangesetId) {
    let landed = LocalRepo::open(root).and_then(|repo| repo.land(keyed(head, SWEPT_KEY)));
    match landed {
        Ok(out) => println!(
            "{REPORT}landed {} warnings {}",
            out.landed,
            out.warnings.len()
        ),
        Err(e) => println!("{REPORT}failed {e:?}"),
    }
}

/// The sweeps' template: a store whose `main` is `t0`, with an agent's commit on it; and what the
/// land of that commit makes, landed on a copy.
#[cfg(feature = "test-hooks")]
fn swept_template() -> (TempDir, Utf8PathBuf, Swept) {
    let (dir, template, repo) = new_store();
    let t0 = advance(
        &repo,
        ACTOR,
        &[],
        vec![put("a", b"a\n"), put("f", b"1\n2\n")],
    );
    let head = commit_by(
        &repo,
        ACTOR,
        &[t0],
        vec![put("f", b"1\n2 2\n"), put("d/n", b"n\n")],
    );
    drop(repo);
    let copy = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(copy.path().join("store")).unwrap();
    copy_dir(template.as_std_path(), root.as_std_path());
    let repo = LocalRepo::open_with_fs(&root, checked(RealFs)).unwrap();
    let want = repo.land(keyed(head, SWEPT_KEY)).unwrap().landed;
    (dir, template, Swept { t0, head, want })
}

/// A fresh copy of `template`, in a temp dir of its own.
#[cfg(feature = "test-hooks")]
fn copy_of(template: &Utf8Path) -> (TempDir, Utf8PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
    copy_dir(template.as_std_path(), root.as_std_path());
    (dir, root)
}

/// Whether the store holds the swept land, checked: a changeset with [`SWEPT_KEY`] on `main`'s
/// first-parent chain exists exactly when `main` is that changeset, `want`, and its parent is
/// `t0`; otherwise `main` is `t0`. `what` names the run.
#[cfg(feature = "test-hooks")]
fn swept_landed(repo: &LocalRepo, swept: &Swept, what: &str) -> bool {
    let main = main_head(repo);
    let mut keyed = Vec::new();
    let mut at = Some(main);
    while let Some(id) = at {
        if meta_value(repo, id, "land_key").as_deref() == Some(SWEPT_KEY.as_bytes()) {
            keyed.push(id);
        }
        at = first_parent(repo, id);
    }
    if main == swept.want {
        assert_eq!(keyed, [swept.want], "{what}");
        assert_eq!(first_parent(repo, main), Some(swept.t0), "{what}");
        true
    } else {
        assert_eq!((main, keyed), (swept.t0, Vec::new()), "{what}");
        false
    }
}

/// The same land again on the store, as the swept land left it (`landed`): it ends with the land
/// on `main` once, replayed if it was there already, and the store verifies clean.
#[cfg(feature = "test-hooks")]
fn land_again(repo: &LocalRepo, swept: &Swept, landed: bool, what: &str) {
    let again = repo.land(keyed(swept.head, SWEPT_KEY)).unwrap();
    let status = if landed {
        LandStatus::Replayed
    } else {
        LandStatus::Landed
    };
    assert_eq!((again.status, again.landed), (status, swept.want), "{what}");
    assert!(swept_landed(repo, swept, what), "{what}: landed again");
    assert_eq!(repo.verify(Depth::Deep).unwrap().problems, [], "{what}");
}

/// How many runs of a sweep go at once: each has a child process and a store of its own.
#[cfg(feature = "test-hooks")]
const AT_ONCE: u64 = 8;

/// Runs `run(k)` for k = 1, 2, … [`AT_ONCE`] at a time, until a run reports that its child
/// finished; returns how many filesystem calls the child makes (the last k it crashed at). Every
/// run past that one must finish too.
#[cfg(feature = "test-hooks")]
fn sweep(run: impl Fn(u64) -> bool + Sync) -> u64 {
    let run = &run;
    let mut first = 1;
    loop {
        let finished: Vec<bool> = thread::scope(|scope| {
            let runs: Vec<_> = (first..first + AT_ONCE)
                .map(|k| scope.spawn(move || run(k)))
                .collect();
            runs.into_iter().map(|run| run.join().unwrap()).collect()
        });
        if let Some(at) = finished.iter().position(|&done| done) {
            assert!(
                finished[at..].iter().all(|&done| done),
                "a crash after the child's last call: {finished:?} from k={first}"
            );
            return first + u64::try_from(at).unwrap() - 1;
        }
        first += AT_ONCE;
        assert!(first < 2_000, "the child never finished");
    }
}

/// Runs the land of `test`'s child again in a child process on the store at `root`, with
/// `hooks` set, and every call checked against the store's write-ordering rules.
#[cfg(feature = "test-hooks")]
fn land_child(test: &str, root: &Utf8Path, head: ChangesetId, hooks: &[(&str, &str)]) -> Output {
    let mut command = this_test(&test_name!(test));
    command
        .env(CHILD_ROOT, root)
        .env(CHILD_HEAD, head.to_string())
        .env(ORDER_CHECK_ENV, "1");
    for (name, value) in hooks {
        command.env(name, value);
    }
    run(&mut command)
}

/// One run of the crash sweep: a child lands the swept head on a copy of `template` until its
/// k-th filesystem call aborts it. The land must then be on `main` once or not at all
/// ([`swept_landed`]), the store must verify clean, and the same land again must end with it on
/// `main` once ([`land_again`]). Returns whether the child finished, and whether the crash left
/// the land on `main`.
#[cfg(feature = "test-hooks")]
fn land_crashing_at(template: &Utf8Path, swept: &Swept, k: u64) -> (bool, bool) {
    let (_dir, root) = copy_of(template);
    let crash_at = k.to_string();
    let child = land_child(
        "a_land_crashing_at_any_call_lands_once",
        &root,
        swept.head,
        &[(CRASH_AT_ENV, crash_at.as_str())],
    );
    let finished = child.status.success();
    assert!(
        finished || child.status.signal() == Some(SIGABRT),
        "k={k}: {:?}\n{}",
        child.status,
        String::from_utf8_lossy(&child.stderr)
    );
    let what = format!("k={k}");
    let repo = LocalRepo::open_with_fs(&root, checked(RealFs)).unwrap();
    let landed = swept_landed(&repo, swept, &what);
    assert!(
        landed || !finished,
        "{what}: the land returned but is not on main"
    );
    assert_eq!(repo.verify(Depth::Deep).unwrap().problems, [], "{what}");
    land_again(&repo, swept, landed, &what);
    (finished, landed)
}

/// A land that crashes at any filesystem call lands once or not at all: for every k, a changeset
/// with the land's key is on `main` exactly when `main` is that changeset on the head the land
/// read, the store verifies clean, and the same land again ends with exactly one changeset with
/// the key on `main`, replayed where the crash left it landed. Some crash comes after the commit
/// point.
#[test]
#[cfg(feature = "test-hooks")]
fn a_land_crashing_at_any_call_lands_once() {
    if let Some((root, head)) = child() {
        land_as_child(&root, head);
        return;
    }
    let (_template_dir, template, swept) = swept_template();
    let past_the_commit_point = AtomicBool::new(false);
    let calls = sweep(|k| {
        let (finished, landed) = land_crashing_at(&template, &swept, k);
        if landed && !finished {
            past_the_commit_point.store(true, Ordering::Relaxed);
        }
        finished
    });
    assert!(calls > 20, "the child finished after {calls} calls");
    assert!(
        past_the_commit_point.load(Ordering::Relaxed),
        "no crash came after the commit point"
    );
}

/// The failure modes the failure sweep fails each call with: one call (`EIO`, which lets a flush
/// do its work first, or `ENOSPC`, which fails a call before it does anything), or every mutating
/// call from it on.
#[cfg(feature = "test-hooks")]
const FAILURES: [&str; 3] = ["once:eio", "once:enospc", "persistent:eio"];

/// What the store's documented abort says on stderr: a failed journal append whose cut back off
/// the journal failed too.
#[cfg(feature = "test-hooks")]
const ABORT: &str = "could not be cut back off it";

/// How a child of the failure sweep ended.
#[cfg(feature = "test-hooks")]
#[derive(Debug)]
enum Ended {
    /// It landed, with this many warnings.
    Landed(usize),
    /// It failed, with this error (its `Debug`).
    Failed(String),
    /// It aborted, as the store documents.
    Aborted,
}

/// How many filesystem calls the swept land makes, the child's opening of the store included: a
/// dry run's count ([`FS_CALLS_ENV`]).
#[cfg(feature = "test-hooks")]
fn dry_run_calls(template: &Utf8Path, swept: &Swept) -> u64 {
    let (dir, root) = copy_of(template);
    let count = dir.path().join("calls");
    let child = land_child(
        "a_failed_publish_reports_honestly",
        &root,
        swept.head,
        &[(FS_CALLS_ENV, count.to_str().unwrap())],
    );
    assert!(child.status.success(), "the dry run: {child:?}");
    let calls = std::fs::read_to_string(&count).unwrap();
    calls.trim().parse().unwrap()
}

/// One run of the failure sweep: a child lands the swept head on a copy of `template`, its k-th
/// filesystem call failing as `failure` says. However it ended, the store then verifies clean, and
/// holds the land exactly when the child said it landed (either, after the documented abort):
/// never a land behind an error. The same land again ends with it on `main` once. Returns how the
/// child ended.
#[cfg(feature = "test-hooks")]
fn land_failing_at(template: &Utf8Path, swept: &Swept, k: u64, failure: &str) -> Ended {
    let (_dir, root) = copy_of(template);
    let fail_at = format!("{k}:{failure}");
    let child = land_child(
        "a_failed_publish_reports_honestly",
        &root,
        swept.head,
        &[(FAIL_AT_ENV, fail_at.as_str())],
    );
    let what = format!("call {k} failing {failure}");
    let stdout = String::from_utf8_lossy(&child.stdout);
    let stderr = String::from_utf8_lossy(&child.stderr);
    let ended = if child.status.signal() == Some(SIGABRT) {
        assert!(
            stderr.contains(ABORT),
            "{what}: an abort not documented\n{stderr}"
        );
        Ended::Aborted
    } else {
        assert!(
            child.status.success(),
            "{what}: {:?}\n{stderr}",
            child.status
        );
        let report = (stdout.lines())
            .find_map(|line| line.strip_prefix(REPORT))
            .unwrap_or_else(|| panic!("{what}: no report\n{stdout}"));
        match report.split_once(" warnings ") {
            Some((_landed, warnings)) => Ended::Landed(warnings.parse().unwrap()),
            None => Ended::Failed(report.strip_prefix("failed ").unwrap().to_owned()),
        }
    };
    let repo = LocalRepo::open_with_fs(&root, checked(RealFs)).unwrap();
    let landed = swept_landed(&repo, swept, &what);
    match &ended {
        Ended::Landed(_) => assert!(landed, "{what}: landed, but not on main"),
        Ended::Failed(error) => assert!(!landed, "{what}: on main behind an error: {error}"),
        Ended::Aborted => {}
    }
    assert_eq!(repo.verify(Depth::Deep).unwrap().problems, [], "{what}");
    land_again(&repo, swept, landed, &what);
    ended
}

/// A land whose filesystem fails at any call reports what it did: a failed journal append, cut
/// back off the journal (`NotCommitted`), leaves `main` as it was, and the land again lands; a
/// failure after the commit point has landed, with a warning; any other failure has recorded
/// nothing; and the documented abort (a failed append whose cut fails too) may leave either. Each
/// call of a dry run fails in turn, once with `EIO`, once with `ENOSPC`, and with `EIO` for good.
#[test]
#[cfg(feature = "test-hooks")]
fn a_failed_publish_reports_honestly() {
    if let Some((root, head)) = child() {
        land_as_child(&root, head);
        return;
    }
    let (_template_dir, template, swept) = swept_template();
    let calls = dry_run_calls(&template, &swept);
    assert!(calls > 20, "the land made {calls} calls");
    let (template, swept) = (&template, &swept);
    let mut ends = Vec::new();
    for failure in FAILURES {
        let mut first = 1;
        while first <= calls {
            let last = calls.min(first + AT_ONCE - 1);
            let ran: Vec<Ended> = thread::scope(|scope| {
                let runs: Vec<_> = (first..=last)
                    .map(|k| scope.spawn(move || land_failing_at(template, swept, k, failure)))
                    .collect();
                runs.into_iter().map(|run| run.join().unwrap()).collect()
            });
            ends.extend(ran);
            first = last + 1;
        }
    }
    let not_committed = (ends.iter())
        .filter(|ended| matches!(ended, Ended::Failed(e) if e.starts_with("Store(NotCommitted")))
        .count();
    let warned = (ends.iter())
        .filter(|ended| matches!(ended, Ended::Landed(warnings) if *warnings > 0))
        .count();
    assert!(
        not_committed > 0,
        "no failure came at the journal append: {ends:?}"
    );
    assert!(
        warned > 0,
        "no failure came after the commit point: {ends:?}"
    );
}
