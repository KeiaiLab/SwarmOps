//! A damaged or crafted changeset index (SECURITY.md, "Crafted or damaged store files"), below
//! the checkpoint, where the index counts its records as they are: no read answers with what
//! the damage makes up. A record whose parents do not lie where the index's own layout puts them
//! (a `par.col` entry moved onto another record's parents, as one flipped bit moves it) fails
//! every lookup that reads it, as does the record after it; a record whose parents are not its
//! changeset object's (a parent rewritten in `parents.log`) fails every read that returns it with
//! its object. Every first-parent walk (a revision's `~N`, `first_parents`, `is_ancestor`,
//! `merge_base`, a path's history, a land's squash) takes each step from the changeset objects,
//! hash-checked against their ids, so that none drops a committed changeset or loops on a cycle
//! the index makes up. `verify` reports the damage all the same.
//!
//! Every test owns a fresh store: a chain committed and checkpointed, then one index file damaged
//! by hand.

use std::fmt::Debug;
use std::fs;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_core::ChangesetId;
use swarmops_repo::{
    Depth, LandRequest, LandStatus, LocalRepo, MismatchCause, RepoError, StoreError, VerifyProblem,
};
use swarmops_store::Layout;
use swarmops_testkit::repo::{new_repo, path, put, request, signature};
use tempfile::TempDir;

/// How long a walk over a store of a few changesets may take before the test calls it a hang.
const HANG: Duration = Duration::from_secs(60);

/// The changesets of [`chain`]: `c[0]` to `c[4]` a first-parent chain from a root, `b` a branch
/// on `c[1]`, and `z` a root of a history of its own.
struct Chain {
    c: [ChangesetId; 5],
    b: ChangesetId,
    z: ChangesetId,
}

/// A store holding five changesets in a first-parent chain, `c1` (a root) to `c5`, each putting
/// `f` to its number; then `b` on `c2`, and `z`, a root of its own: index records 0 to 6, in that
/// order. `parents.log` holds `c1` at 0, `c2` at 32, `c3` at 64, `c4` at 96 and `c2` again at
/// 128, `b`'s parent. The metadata is checkpointed, so that the index counts every record as it
/// is rather than as the journal past the checkpoint vouches for it.
fn chain() -> (TempDir, Utf8PathBuf, LocalRepo, Chain) {
    let (dir, root, repo) = new_repo();
    let mut c = Vec::new();
    for n in 1..=5 {
        let parents: Vec<ChangesetId> = c.last().copied().into_iter().collect();
        let content = format!("{n}");
        let req = request(
            &parents,
            vec![put("f", content.as_bytes())],
            &format!("c{n}"),
        );
        c.push(repo.commit(req).unwrap().id);
    }
    let b = (repo.commit(request(&[c[1]], vec![put("g", b"b")], "b")))
        .unwrap()
        .id;
    let z = (repo.commit(request(&[], vec![put("h", b"z")], "z")))
        .unwrap()
        .id;
    repo.flush_metadata().unwrap();
    let c = c.try_into().unwrap();
    (dir, root, repo, Chain { c, b, z })
}

/// Writes `bytes` over index file `rel` (under `meta/`) at byte `at`, after checking that `was`
/// is there.
fn overwrite(root: &Utf8Path, rel: &str, at: usize, was: &[u8], bytes: &[u8]) {
    let file = Layout::new(root).meta_path(rel);
    let mut content = fs::read(&file).unwrap();
    let place = at..at + bytes.len();
    assert_eq!(&content[place.clone()], was, "{rel} at {at}");
    content[place].copy_from_slice(bytes);
    fs::write(&file, content).unwrap();
}

/// The `par.col` entry of parents at byte `offset` of `parents.log`, `count` of them.
fn par_entry(offset: u64, count: u8) -> [u8; 8] {
    ((offset << 8) | u64::from(count)).to_le_bytes()
}

#[track_caller]
fn assert_index_corrupt<T: Debug>(got: Result<T, RepoError>, what: &str) {
    match got {
        Err(RepoError::Store(StoreError::IndexCorrupt { .. })) => {}
        other => panic!("{what}: expected IndexCorrupt, got {other:?}"),
    }
}

/// `repo`'s first-parent log from `from`, at most `limit` long, as ids.
fn log(repo: &LocalRepo, from: ChangesetId, limit: usize) -> Result<Vec<ChangesetId>, RepoError> {
    let log = repo.log_first_parent(from, limit)?;
    Ok(log.into_iter().map(|stored| stored.id).collect())
}

/// The changeset `rev` names in a snapshot of `repo`.
fn resolve(repo: &LocalRepo, rev: &str) -> Result<ChangesetId, RepoError> {
    repo.snapshot()?.resolve(rev)
}

/// The first `limit` changesets of the first-parent walk from `from`, the first error ending it.
fn walk(repo: &LocalRepo, from: ChangesetId, limit: usize) -> Result<Vec<ChangesetId>, RepoError> {
    let read = repo.snapshot()?;
    read.first_parents(from).take(limit).collect()
}

/// `merge_base(a, b)` of the store at `root`, on a thread of its own and a handle of its own: a
/// walk that loops fails the test after [`HANG`] instead of hanging it.
fn merge_base_within(
    root: &Utf8Path,
    a: ChangesetId,
    b: ChangesetId,
) -> Result<Option<ChangesetId>, RepoError> {
    let (send, receive) = mpsc::channel();
    let root = root.to_owned();
    thread::spawn(move || {
        let repo = LocalRepo::open(&root).unwrap();
        let base = repo.snapshot().and_then(|read| read.merge_base(a, b));
        let _ = send.send(base);
    });
    receive
        .recv_timeout(HANG)
        .unwrap_or_else(|_| panic!("merge_base({a}, {b}) still walking after {HANG:?}"))
}

/// The ids of the changesets whose `f` a path history from `from` lists, newest first.
fn history_of_f(repo: &LocalRepo, from: ChangesetId) -> Result<Vec<ChangesetId>, RepoError> {
    let events = repo.snapshot()?.path_history(from, &path("f"), 16)?;
    Ok(events.into_iter().map(|event| event.changeset).collect())
}

/// A land of `head` from `base` onto `main` by a fixed lander.
fn land_from(base: ChangesetId, head: ChangesetId) -> LandRequest {
    let mut lander = signature();
    lander.name = b"test/lander".to_vec();
    let mut land = LandRequest::new(head, "main", lander);
    land.base = Some(base);
    land
}

/// What verify finds, deep.
fn problems(repo: &LocalRepo) -> Vec<VerifyProblem> {
    repo.verify(Depth::Deep).unwrap().problems
}

#[test]
fn a_par_col_entry_moved_onto_another_records_parents_drops_no_changeset() {
    let (_dir, root, repo, Chain { c, b, .. }) = chain();
    let [c1, c2, c3, c4, c5] = c;
    // c3's entry (record 2) points at parents.log's first parent, c1, instead of at its own, c2:
    // a single bit, 0x2001 to 0x0001.
    overwrite(
        &root,
        "idx/par.col",
        16,
        &par_entry(32, 1),
        &par_entry(0, 1),
    );

    // Reads of c3's record, and of c4's, whose parents no longer start where c3's end, fail.
    assert_index_corrupt(log(&repo, c3, 10), "log from c3");
    assert_index_corrupt(log(&repo, c5, 10), "log from c5, through c4");
    assert_index_corrupt(resolve(&repo, &format!("{c3}~1")), "c3~1");
    assert_index_corrupt(repo.changeset(c4), "show c4");
    assert_index_corrupt(history_of_f(&repo, c5), "the history of f, through c3");
    // Walks take their steps from the changeset objects: c2 is never dropped.
    assert_eq!(resolve(&repo, &format!("{c5}~3")).unwrap(), c2, "c5~3");
    assert_eq!(walk(&repo, c5, 10).unwrap(), [c5, c4, c3, c2, c1]);
    let read = repo.snapshot().unwrap();
    assert!(read.is_ancestor(c2, c5).unwrap(), "c2 is on c5's chain");
    // The merge base reads the records it steps to, for their generations: c4's fails.
    assert_index_corrupt(read.merge_base(c5, b), "merge base of c5 and b");

    assert_eq!(
        problems(&repo),
        [VerifyProblem::IndexMismatch {
            seq: 2,
            cause: MismatchCause::Journal
        }]
    );
}

#[test]
fn a_parent_rewritten_in_parents_log_drops_no_changeset() {
    let (_dir, root, repo, Chain { c, b, .. }) = chain();
    let [c1, c2, c3, c4, c5] = c;
    // c3's parent in parents.log (bytes 32 to 64) names c1 instead of c2; the layout is whole.
    overwrite(&root, "parents.log", 32, &c2.0.0, &c1.0.0);

    // A read that returns c3's record with its object finds the two disagree.
    assert_index_corrupt(log(&repo, c3, 10), "log from c3");
    assert_index_corrupt(log(&repo, c5, 10), "log from c5, through c3");
    assert_index_corrupt(repo.changeset(c3), "show c3");
    // Walks take their steps from the changeset objects: c2 is never dropped.
    assert_eq!(resolve(&repo, &format!("{c3}~1")).unwrap(), c2, "c3~1");
    assert_eq!(resolve(&repo, &format!("{c5}~3")).unwrap(), c2, "c5~3");
    assert_eq!(walk(&repo, c5, 10).unwrap(), [c5, c4, c3, c2, c1]);
    assert_eq!(history_of_f(&repo, c5).unwrap(), [c5, c4, c3, c2, c1]);
    let read = repo.snapshot().unwrap();
    assert!(read.is_ancestor(c2, c5).unwrap(), "c2 is on c5's chain");
    assert_eq!(read.merge_base(c5, b).unwrap(), Some(c2));

    assert_eq!(
        problems(&repo),
        [VerifyProblem::IndexMismatch {
            seq: 2,
            cause: MismatchCause::Journal
        }]
    );
}

#[test]
fn a_first_parent_cycle_in_the_index_is_never_walked() {
    let (_dir, root, repo, Chain { c, z, .. }) = chain();
    let [c1, c2, c3, c4, c5] = c;
    repo.cas_ref("main", None, Some(c1), "test/agent").unwrap();
    // c2's parent in parents.log (bytes 0 to 32) names c5: c2, c5, c4, c3, c2, ... by the index.
    overwrite(&root, "parents.log", 0, &c1.0.0, &c5.0.0);

    assert_eq!(walk(&repo, c5, 12).unwrap(), [c5, c4, c3, c2, c1]);
    assert_eq!(resolve(&repo, &format!("{c5}~4")).unwrap(), c1, "c5~4");
    assert_eq!(resolve(&repo, &format!("{c2}~1")).unwrap(), c1, "c2~1");
    assert_index_corrupt(log(&repo, c5, 12), "log from c5, through c2");
    let read = repo.snapshot().unwrap();
    assert!(read.is_ancestor(c1, c5).unwrap(), "c1 is on c5's chain");
    // Two unrelated histories have no merge base: the walk down c5's chain reaches c1, a root.
    assert_eq!(merge_base_within(&root, c5, z).unwrap(), None);
    // A land of c2 to c5 onto main, at c1, squashes them, walked from the objects.
    let landed = repo.land(land_from(c1, c5)).unwrap();
    assert_eq!(landed.status, LandStatus::Landed);
    assert_eq!(landed.squashed_from, [c2, c3, c4, c5]);

    assert_eq!(
        problems(&repo),
        [VerifyProblem::IndexMismatch {
            seq: 1,
            cause: MismatchCause::Journal
        }]
    );
}
