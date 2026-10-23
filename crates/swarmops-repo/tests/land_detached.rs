//! The detached land (M13): a chain merged three-way onto a changeset and written as one new
//! changeset, with no ref moved. Written by the M13 plan's task C5b.
//!
//! `Repo::land_detached` is the land's first attempt without its publication: the same merge and
//! the same writer of `L`, then `L`'s insert alone, in a journal frame that moves no ref. It makes
//! no key walk: the key and the chain it is given are recorded as they are.

use std::sync::Arc;

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_core::{ChangesetId, Signature};
use swarmops_repo::land::{DetachedLand, DetachedOutcome, LandRequest};
use swarmops_repo::{Advance, Change, CommitRequest, LocalRepo, RepoError};
use swarmops_store::{Journal, Layout, MetaStore, RealFs};
use swarmops_testkit::faults::checked;
use swarmops_testkit::repo::{path, put, request};
use tempfile::TempDir;

/// Who makes the changes and moves `main` in the tests' setup.
const ACTOR: &str = "test/agent";

/// The agent of the changesets a change is moved onto.
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

/// A detached land of the chain `(base, head]` onto `onto` by [`lander`]: no key, no chain given,
/// the head's message, no metadata of the lander's.
fn detached(base: ChangesetId, head: ChangesetId, onto: ChangesetId) -> DetachedLand {
    DetachedLand::new(base, head, onto, lander())
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

/// Commits `changes` on `parents` by `agent`, moving `main` to the new changeset.
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

/// Changeset `id`'s metadata, as text keys and byte values.
fn metadata_of(repo: &LocalRepo, id: ChangesetId) -> Vec<(String, Vec<u8>)> {
    repo.changeset(id)
        .unwrap()
        .expect("the changeset")
        .changeset
        .metadata
}

/// What the file at `p` holds in changeset `id`'s tree; `None` if there is none.
fn content(repo: &LocalRepo, id: ChangesetId, p: &str) -> Option<Vec<u8>> {
    repo.read_blob_at(id, &path(p))
        .unwrap()
        .map(|(_, _, bytes)| bytes)
}

/// The id a detached land landed, which must have landed.
#[track_caller]
fn landed(outcome: Result<DetachedOutcome, RepoError>) -> ChangesetId {
    match outcome {
        Ok(DetachedOutcome::Landed(id)) => id,
        other => panic!("expected Landed, got {other:?}"),
    }
}

/// A detached land writes `L` on `onto` and moves no ref: `main` stays where it was, the land's
/// one journal frame holds `L`'s insert and nothing else, and `L` merges both sides' changes,
/// its one parent `onto`, its committer the lander.
#[test]
fn detached_land_moves_no_ref() {
    let (_dir, root, repo) = new_store();

    // 1. A change made on t0, and main moved past t0 by another agent.
    let t0 = advance(&repo, ACTOR, &[], vec![put("a", b"a\n"), put("c", b"c\n")]);
    let head = commit_by(&repo, ACTOR, &[t0], vec![put("a", b"ours\n")]);
    let x = advance(&repo, OTHER, &[t0], vec![put("c", b"theirs\n")]);
    let frames = frame_count(&root);

    // 2. The detached land onto main's head.
    let id = landed(repo.land_detached(detached(t0, head, x)));

    // 3. One frame, the insert alone; main untouched.
    assert_eq!(main_head(&repo), x);
    assert_eq!(
        records_past(&root, frames),
        [(frames, "changeset", id.0.0.to_vec())]
    );

    // 4. L: on x, both sides' changes, signed by the lander.
    let stored = repo.changeset(id).unwrap().expect("L is in the index");
    assert_eq!(stored.changeset.parents, [x]);
    assert_eq!(stored.changeset.committer, lander());
    assert_eq!(content(&repo, id, "a"), Some(b"ours\n".to_vec()));
    assert_eq!(content(&repo, id, "c"), Some(b"theirs\n".to_vec()));
}

/// A chain that conflicts with `onto` writes nothing: the error is the land's conflict report,
/// the chain's base and head against `onto`, and the journal holds no new frame.
#[test]
fn detached_land_conflict_writes_nothing() {
    let (_dir, root, repo) = new_store();

    // 1. Both sides change the same line of f.
    let t0 = advance(&repo, ACTOR, &[], vec![put("f", b"1\n2\n3\n")]);
    let head = commit_by(&repo, ACTOR, &[t0], vec![put("f", b"1\nours\n3\n")]);
    let x = advance(&repo, OTHER, &[t0], vec![put("f", b"1\ntheirs\n3\n")]);
    let frames = frame_count(&root);

    // 2. The conflict, as data, naming who last touched f on onto's chain.
    let conflicts = match repo.land_detached(detached(t0, head, x)) {
        Err(RepoError::Conflict(conflicts)) => conflicts,
        other => panic!("expected Conflict, got {other:?}"),
    };
    assert_eq!(
        (conflicts.base, conflicts.ours, conflicts.theirs),
        (t0, head, x)
    );
    let paths: Vec<&str> = (conflicts.conflicts.iter())
        .map(|conflict| conflict.path.as_str())
        .collect();
    assert_eq!(paths, ["f"]);
    assert_eq!(
        conflicts.last_touched.get(&path("f")),
        Some(&(x, OTHER.to_owned()))
    );

    // 3. Nothing recorded.
    assert_eq!((frame_count(&root), main_head(&repo)), (frames, x));
}

/// A chain whose change `onto` holds already, landed before or made again by hand, merges to
/// `onto`'s tree: `NoChanges`, and nothing is recorded.
#[test]
fn detached_land_of_a_held_change_is_no_changes() {
    let (_dir, root, repo) = new_store();

    // 1. The change, landed once on main.
    let t0 = advance(&repo, ACTOR, &[], vec![put("a", b"a\n")]);
    let head = commit_by(&repo, ACTOR, &[t0], vec![put("b", b"b\n")]);
    let on_main = repo
        .land(LandRequest::new(head, "main", lander()))
        .unwrap()
        .landed;

    // 2. The same change made again by hand, on top.
    let again = advance(&repo, OTHER, &[on_main], vec![put("c", b"c\n")]);
    let frames = frame_count(&root);

    // 3. Its detached land onto main's head changes nothing.
    let outcome = repo.land_detached(detached(t0, head, again)).unwrap();
    assert_eq!(outcome, DetachedOutcome::NoChanges);
    assert_eq!((frame_count(&root), main_head(&repo)), (frames, again));
}

/// The key and the chain a detached land is given are recorded as they are: `land_key` without
/// a key walk (the key landed another change on `onto` already, which a land would refuse), and
/// `squashed_from` replacing the chain's own ids. Without a chain given, `squashed_from` is the
/// chain `(base, head]`, oldest first.
#[test]
fn detached_land_copies_key_and_squashed_from() {
    let (_dir, _root, repo) = new_store();

    // 1. A two-commit change, and the key used by another land on main.
    let t0 = advance(&repo, ACTOR, &[], vec![put("a", b"a\n")]);
    let c1 = commit_by(&repo, ACTOR, &[t0], vec![put("b", b"b\n")]);
    let c2 = commit_by(&repo, ACTOR, &[c1], vec![put("c", b"c\n")]);
    let other = commit_by(&repo, OTHER, &[t0], vec![put("o", b"o\n")]);
    let mut keyed = LandRequest::new(other, "main", lander());
    keyed.key = Some("key-1".to_owned());
    let onto = repo.land(keyed).unwrap().landed;

    // 2. Given a key and a chain: both copied, no walk.
    let given = [other, c1, c2];
    let mut copy = detached(t0, c2, onto);
    copy.key = Some("key-1".to_owned());
    copy.squashed_from = Some(given.to_vec());
    let id = landed(repo.land_detached(copy));
    let metadata = metadata_of(&repo, id);
    let value = |key: &str| {
        (metadata.iter())
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.clone())
    };
    assert_eq!(value("land_key"), Some(b"key-1".to_vec()));
    let chain = format!("{other} {c1} {c2}");
    assert_eq!(value("squashed_from"), Some(chain.into_bytes()));

    // 3. Given neither: no key, the chain's own ids.
    let id = landed(repo.land_detached(detached(t0, c2, onto)));
    let metadata = metadata_of(&repo, id);
    assert!(!metadata.iter().any(|(name, _)| name == "land_key"));
    let chain = format!("{c1} {c2}");
    assert!(metadata.contains(&("squashed_from".to_owned(), chain.into_bytes())));
}

/// The metadata the caller adds joins what the land records: the head's `agent` and `run_id`,
/// the lander, the caller's keys and the chain, sorted by key. The message given replaces the
/// head's.
#[test]
fn detached_land_records_extra_metadata() {
    let (_dir, _root, repo) = new_store();

    // 1. A change with an agent and a run id, onto a moved main.
    let t0 = advance(&repo, ACTOR, &[], vec![put("a", b"a\n")]);
    let mut change = by(ACTOR, &[t0], vec![put("b", b"b\n")]);
    change
        .metadata
        .push(("run_id".to_owned(), b"run-1".to_vec()));
    let head = repo.commit(change).unwrap().id;
    let x = advance(&repo, OTHER, &[t0], vec![put("c", b"c\n")]);

    // 2. The land, with the sync's key.
    let synced = head.to_string();
    let mut land = detached(t0, head, x);
    land.message = Some(b"moved\n".to_vec());
    land.extra_metadata = vec![("synced_from".to_owned(), synced.clone().into_bytes())];
    let id = landed(repo.land_detached(land));

    // 3. Every key, sorted.
    let metadata = metadata_of(&repo, id);
    let pairs: Vec<(&str, &[u8])> = (metadata.iter())
        .map(|(key, value)| (key.as_str(), value.as_slice()))
        .collect();
    assert_eq!(
        pairs,
        [
            ("agent", ACTOR.as_bytes()),
            ("lander", LANDER.as_bytes()),
            ("run_id", b"run-1"),
            ("squashed_from", synced.as_bytes()),
            ("synced_from", synced.as_bytes()),
        ]
    );
    let message = repo.changeset(id).unwrap().unwrap().changeset.message;
    assert_eq!(message, b"moved\n");
}

/// The land and the detached land share one merge and one writer of `L`: the same chain landed
/// both ways onto the same head, with the same lander and no key, is the same changeset, byte
/// for byte (the same id), on the same tree.
#[test]
fn land_after_the_refactor_is_byte_identical() {
    let (_dir, _root, repo) = new_store();

    // 1. A two-commit change, and main moved past its base with a file both sides edit.
    let t0 = advance(&repo, ACTOR, &[], vec![put("f", b"1\n2\n3\n4\n5\n")]);
    let c1 = commit_by(&repo, ACTOR, &[t0], vec![put("f", b"one\n2\n3\n4\n5\n")]);
    let c2 = commit_by(&repo, ACTOR, &[c1], vec![put("g", b"g\n")]);
    let x = advance(&repo, OTHER, &[t0], vec![put("f", b"1\n2\n3\n4\nfive\n")]);

    // 2. Detached first, onto main's head; then the land itself, onto the same head.
    let id = landed(repo.land_detached(detached(t0, c2, x)));
    let mut land = LandRequest::new(c2, "main", lander());
    land.base = Some(t0);
    let out = repo.land(land).unwrap();

    // 3. One changeset: the same id, parent and tree.
    assert_eq!((out.landed, out.rebased_onto), (id, x));
    let root = |id| repo.changeset(id).unwrap().unwrap().changeset.root;
    assert_eq!(root(id), root(out.landed));
    assert_eq!(
        content(&repo, id, "f"),
        Some(b"one\n2\n3\n4\nfive\n".to_vec())
    );
}
