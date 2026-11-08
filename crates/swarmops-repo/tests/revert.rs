//! Revert (the M18 design §4): `Repo::revert` takes back a first-parent range of a target by a
//! land with changes, `C` committed on the range's last changeset and landed from it. What the
//! target gained since stays; a later edit of the same lines is a conflict that records nothing;
//! a keyed revert run again replays; and every refusal writes nothing.

use std::collections::BTreeMap;
use std::sync::Arc;

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_core::{ChangesetId, EntryTarget, RepoPath, Signature};
use swarmops_repo::land::{LandStatus, NothingToLandReason};
use swarmops_repo::{
    Advance, Change, LocalRepo, MAX_REVERT_CHANGESETS, OpId, RepoError, RevertRequest,
};
use swarmops_store::{Journal, RealFs};
use swarmops_testkit::faults::checked;
use swarmops_testkit::fs::NoSyncFs;
use swarmops_testkit::repo::{delete, path, put, request};
use swarmops_testkit::store::layout_of;
use tempfile::TempDir;

/// Who makes the changes the tests revert.
const ACTOR: &str = "test/agent";

/// Who edits after them.
const LATER: &str = "test/later";

/// Who reverts.
const LANDER: &str = "test/lander";

/// The reason every revert gives.
const REASON: &str = "it broke the build";

/// A fresh store, its writes checked against the store's ordering rules.
fn new_store() -> (TempDir, Utf8PathBuf, LocalRepo) {
    let dir = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
    let repo = LocalRepo::create_with_fs(&root, "test", checked(RealFs::new())).unwrap();
    (dir, root, repo)
}

/// The lander's signature: fixed, so that the same revert makes the same changesets.
fn lander() -> Signature {
    Signature::new(
        LANDER.as_bytes().to_vec(),
        b"lander@test.swarmops.local".to_vec(),
        1_760_000_100,
        0,
        false,
    )
    .unwrap()
}

/// A revert of `changeset` on `main` by [`lander`], for [`REASON`].
fn reverting(changeset: ChangesetId) -> RevertRequest {
    RevertRequest::new(changeset, "main", REASON, lander())
}

/// [`reverting`] with the idempotency key `key`.
fn keyed(changeset: ChangesetId, key: &str) -> RevertRequest {
    let mut revert = reverting(changeset);
    revert.key = Some(key.to_owned());
    revert
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
    let mut req = request(parents, changes, &format!("{agent}'s change\n\nits body"));
    req.metadata = vec![("agent".to_owned(), agent.as_bytes().to_vec())];
    req.advance = Some(Advance::new("main", agent));
    repo.commit(req).unwrap().id
}

/// What `main` names.
fn main_head(repo: &LocalRepo) -> ChangesetId {
    repo.get_ref("main").unwrap().expect("main exists").target
}

/// How many frames the journal of the store at `root` lists.
fn frame_count(root: &Utf8Path) -> u64 {
    Journal::open(&layout_of(root), Arc::new(RealFs::new()))
        .unwrap()
        .frame_count()
        .unwrap()
}

/// Every file of changeset `cs`'s tree, by path, with its content.
fn files(repo: &LocalRepo, cs: ChangesetId) -> BTreeMap<String, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut dirs: Vec<Option<RepoPath>> = vec![None];
    while let Some(dir) = dirs.pop() {
        let entries = repo.list_tree(cs, dir.as_ref()).unwrap().unwrap();
        for entry in entries {
            let at = match &dir {
                Some(dir) => format!("{}/{}", dir.as_str(), entry.name),
                None => entry.name.clone(),
            };
            match entry.target {
                EntryTarget::Dir(_) => dirs.push(Some(path(&at))),
                EntryTarget::Leaf(..) => {
                    let (_, _, bytes) = repo.read_blob_at(cs, &path(&at)).unwrap().unwrap();
                    files.insert(at, bytes);
                }
                other => panic!("{at}: an entry of no known kind: {other:?}"),
            }
        }
    }
    files
}

/// `files` as [`files`] gives them.
fn tree(files: &[(&str, &str)]) -> BTreeMap<String, Vec<u8>> {
    (files.iter())
        .map(|(at, content)| ((*at).to_owned(), content.as_bytes().to_vec()))
        .collect()
}

/// The value of metadata `key` of changeset `id`, as text.
fn meta(repo: &LocalRepo, id: ChangesetId, key: &str) -> Option<String> {
    let stored = repo.changeset(id).unwrap().unwrap();
    let value = stored.changeset.metadata.iter().find(|(k, _)| k == key);
    value.map(|(_, value)| String::from_utf8(value.clone()).unwrap())
}

/// Every file of the store at `root` below `meta/` and `objects/`, with its bytes.
fn store_files(root: &Utf8Path) -> BTreeMap<String, Vec<u8>> {
    let mut found = BTreeMap::new();
    let mut dirs = vec![root.join("meta"), root.join("objects")];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let at = Utf8PathBuf::from_path_buf(entry.path()).unwrap();
            if entry.file_type().unwrap().is_dir() {
                dirs.push(at);
                continue;
            }
            let name = at.strip_prefix(root).unwrap().to_string();
            found.insert(name, std::fs::read(&at).unwrap());
        }
    }
    found
}

/// A, then B on another file, then the revert of A: the head holds B and not A, on B, from one
/// changeset `C` on A that the land squashed. The land's "head on the target" check does not
/// fire for A, which is on main: a land with changes skips it.
#[test]
fn a_revert_keeps_later_lands_elsewhere() {
    let (_dir, root, repo) = new_store();
    let t0 = advance(&repo, ACTOR, &[], vec![put("f", b"f\n")]);
    let a = advance(&repo, ACTOR, &[t0], vec![put("a", b"a\n")]);
    let b = advance(&repo, LATER, &[a], vec![put("b", b"b\n")]);
    let frames = frame_count(&root);

    let outcome = repo.revert(reverting(a)).unwrap();
    assert_eq!(outcome.status, LandStatus::Landed);
    assert_eq!((outcome.rebased_onto, outcome.base), (b, a));
    assert_eq!(main_head(&repo), outcome.landed);
    assert_eq!(
        files(&repo, outcome.landed),
        tree(&[("b", "b\n"), ("f", "f\n")])
    );
    assert_eq!(outcome.files_changed, 1);

    // One frame: `C` on A, and `L` on B.
    let [c] = outcome.squashed_from[..] else {
        panic!("one changeset squashed: {outcome:?}");
    };
    let made = repo.changeset(c).unwrap().unwrap();
    assert_eq!(made.changeset.parents, [a]);
    assert_eq!(frame_count(&root), frames + 1);
}

/// A later edit of the lines the reverted changeset changed: `E_CONFLICT`, naming the later
/// changeset as the one that last touched the file; nothing written, the store's files under
/// `meta/` and `objects/` byte for byte as they were.
#[test]
fn a_revert_over_a_later_edit_names_it() {
    let (_dir, root, repo) = new_store();
    let t0 = advance(&repo, ACTOR, &[], vec![put("f", b"1\n2\n3\n")]);
    let a = advance(&repo, ACTOR, &[t0], vec![put("f", b"1\nA\n3\n")]);
    let later = advance(&repo, LATER, &[a], vec![put("f", b"1\nlater\n3\n")]);
    let before = store_files(&root);

    let conflicts = match repo.revert(reverting(a)) {
        Err(RepoError::Conflict(conflicts)) => conflicts,
        other => panic!("expected a conflict, got {other:?}"),
    };
    assert_eq!((conflicts.base, conflicts.theirs), (a, later));
    let paths: Vec<&str> = (conflicts.conflicts.iter())
        .map(|file| file.path.as_str())
        .collect();
    assert_eq!(paths, ["f"]);
    assert_eq!(
        conflicts.last_touched.get(&path("f")),
        Some(&(later, LATER.to_owned()))
    );
    assert_eq!(main_head(&repo), later);

    // Nothing written: `meta/` and `objects/` byte for byte as they were. The land holds `C`,
    // the inverse committed on A, in memory until its publish.
    assert_eq!(store_files(&root), before, "the store's files changed");
}

/// A revert whose landed changeset cannot be encoded, its lander's metadata naming a key the
/// land records itself: [`RepoError::Codec`] when the land writes `L`, after `C` was staged; the
/// store's files under `meta/` and `objects/` byte for byte as they were.
#[test]
fn a_revert_refused_at_its_write_writes_nothing() {
    let (_dir, root, repo) = new_store();
    let t0 = advance(&repo, ACTOR, &[], vec![put("f", b"f\n")]);
    let a = advance(&repo, ACTOR, &[t0], vec![put("a", b"a\n")]);
    advance(&repo, LATER, &[a], vec![put("b", b"b\n")]);
    let before = store_files(&root);

    let mut revert = reverting(a);
    revert.lander_metadata = vec![("lander".to_owned(), b"someone else".to_vec())];
    let refused = repo.revert(revert);
    assert!(matches!(refused, Err(RepoError::Codec(_))), "{refused:?}");
    assert_eq!(store_files(&root), before, "the store's files changed");
}

/// A keyed revert run again replays the revert it made, writing nothing: `C`'s tree is a
/// function of the range, though `C` is made again.
#[test]
fn a_keyed_revert_rerun_replays() {
    let (_dir, root, repo) = new_store();
    let t0 = advance(&repo, ACTOR, &[], vec![put("f", b"f\n")]);
    let a = advance(&repo, ACTOR, &[t0], vec![put("a", b"a\n")]);
    advance(&repo, LATER, &[a], vec![put("b", b"b\n")]);
    let first = repo.revert(keyed(a, "revert-1")).unwrap();
    let frames = frame_count(&root);

    let again = repo.revert(keyed(a, "revert-1")).unwrap();
    assert_eq!(again.status, LandStatus::Replayed);
    assert_eq!(
        (again.landed, &again.squashed_from),
        (first.landed, &first.squashed_from)
    );
    assert_eq!(
        (frame_count(&root), main_head(&repo)),
        (frames, first.landed)
    );
}

/// The same key for the revert of another range: [`RepoError::KeyReused`], naming the first
/// revert's changeset, and no frame.
#[test]
fn a_revert_key_used_for_another_range_is_refused() {
    let (_dir, root, repo) = new_store();
    let t0 = advance(&repo, ACTOR, &[], vec![put("f", b"f\n")]);
    let a = advance(&repo, ACTOR, &[t0], vec![put("a", b"a\n")]);
    let b = advance(&repo, LATER, &[a], vec![put("b", b"b\n")]);
    let first = repo.revert(keyed(a, "revert-1")).unwrap();
    let frames = frame_count(&root);

    match repo.revert(keyed(b, "revert-1")) {
        Err(RepoError::KeyReused {
            key,
            landed,
            target,
        }) => assert_eq!(
            (key.as_str(), landed, target.as_str()),
            ("revert-1", first.landed, "main")
        ),
        other => panic!("expected KeyReused, got {other:?}"),
    }
    assert_eq!(
        (frame_count(&root), main_head(&repo)),
        (frames, first.landed)
    );
}

/// A range of three changesets reverts as one landed changeset, whose `reverts` lists the three,
/// oldest first, and whose message names its ends; with nothing landed after them, the tree is
/// the one before the range.
#[test]
fn a_range_reverts_as_one_changeset() {
    let (_dir, root, repo) = new_store();
    let t0 = advance(&repo, ACTOR, &[], vec![put("f", b"f\n")]);
    let one = advance(&repo, ACTOR, &[t0], vec![put("a", b"a\n")]);
    let two = advance(
        &repo,
        ACTOR,
        &[one],
        vec![put("f", b"f 2\n"), put("b", b"b\n")],
    );
    let three = advance(&repo, ACTOR, &[two], vec![delete("a"), put("c", b"c\n")]);
    let frames = frame_count(&root);

    let mut revert = reverting(three);
    revert.since = Some(t0);
    let outcome = repo.revert(revert).unwrap();
    assert_eq!(outcome.rebased_onto, three);
    assert_eq!(files(&repo, outcome.landed), files(&repo, t0));
    let ids = [one, two, three].map(|id| id.to_string()).join(" ");
    assert_eq!(meta(&repo, outcome.landed, "reverts"), Some(ids));
    let message = repo
        .changeset(outcome.landed)
        .unwrap()
        .unwrap()
        .changeset
        .message;
    let short = |id: ChangesetId| id.to_string()[..12].to_owned();
    let expected = format!(
        "Revert 3 changesets {}..{}\n\n{REASON}\n",
        short(t0),
        short(three)
    );
    assert_eq!(String::from_utf8(message).unwrap(), expected);
    assert_eq!(frame_count(&root), frames + 1);
}

/// A file put where a directory was, and a directory made where a file was, each revert to what
/// was there: the directory's files come back and the file goes, and the other way round.
#[test]
fn a_revert_of_a_file_over_a_directory_restores_it() {
    let (_dir, _root, repo) = new_store();
    let t0 = advance(
        &repo,
        ACTOR,
        &[],
        vec![put("d/x", b"x\n"), put("d/y", b"y\n"), put("e", b"e\n")],
    );
    let swapped = advance(
        &repo,
        ACTOR,
        &[t0],
        vec![
            delete("d/x"),
            delete("d/y"),
            put("d", b"a file\n"),
            delete("e"),
            put("e/z", b"z\n"),
        ],
    );
    advance(&repo, LATER, &[swapped], vec![put("g", b"g\n")]);

    let outcome = repo.revert(reverting(swapped)).unwrap();
    assert_eq!(
        files(&repo, outcome.landed),
        tree(&[("d/x", "x\n"), ("d/y", "y\n"), ("e", "e\n"), ("g", "g\n")])
    );
}

/// The revert of a root changeset takes back every path it made, against the empty tree, and
/// keeps what landed after it.
#[test]
fn a_revert_of_the_root_empties_its_paths() {
    let (_dir, _root, repo) = new_store();
    let root_cs = advance(
        &repo,
        ACTOR,
        &[],
        vec![put("a", b"a\n"), put("d/b", b"b\n")],
    );
    advance(&repo, LATER, &[root_cs], vec![put("c", b"c\n")]);

    let outcome = repo.revert(reverting(root_cs)).unwrap();
    assert_eq!(files(&repo, outcome.landed), tree(&[("c", "c\n")]));
    assert_eq!(
        meta(&repo, outcome.landed, "reverts"),
        Some(root_cs.to_string())
    );
}

/// A changeset off the target's first-parent chain is [`RepoError::RevertOffTarget`], a `since`
/// that is no first-parent ancestor of it [`RepoError::BaseNotAncestor`], and a range whose
/// changes cancel out [`RepoError::NothingToLand`] before any land; none writes a frame.
#[test]
fn a_revert_off_the_target_is_refused() {
    let (_dir, root, repo) = new_store();
    let t0 = advance(&repo, ACTOR, &[], vec![put("f", b"f\n")]);
    let off = repo
        .commit(request(&[t0], vec![put("o", b"o\n")], "off main"))
        .unwrap()
        .id;
    let a = advance(&repo, ACTOR, &[t0], vec![put("a", b"a\n")]);
    let undone = advance(&repo, ACTOR, &[a], vec![delete("a")]);
    let frames = frame_count(&root);

    // 1. Off the target.
    match repo.revert(reverting(off)) {
        Err(RepoError::RevertOffTarget { changeset, target }) => {
            assert_eq!((changeset, target.as_str()), (off, "main"));
        }
        other => panic!("expected RevertOffTarget, got {other:?}"),
    }

    // 2. A `since` above the changeset.
    let mut revert = reverting(a);
    revert.since = Some(undone);
    match repo.revert(revert) {
        Err(RepoError::BaseNotAncestor { base, head }) => assert_eq!((base, head), (undone, a)),
        other => panic!("expected BaseNotAncestor, got {other:?}"),
    }

    // 3. A range whose changes cancel out.
    let mut revert = reverting(undone);
    revert.since = Some(t0);
    match repo.revert(revert) {
        Err(RepoError::NothingToLand(NothingToLandReason::NoChanges)) => {}
        other => panic!("expected NothingToLand, got {other:?}"),
    }
    assert_eq!(frame_count(&root), frames);
}

/// A range of one changeset more than [`MAX_REVERT_CHANGESETS`] is
/// [`RepoError::RevertTooLong`] and writes no frame; one of exactly that many lands.
#[test]
fn a_revert_past_its_bound_is_too_large() {
    let dir = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
    let repo = LocalRepo::create_with_fs(&root, "test", Arc::new(NoSyncFs)).unwrap();
    let t0 = advance(&repo, ACTOR, &[], vec![put("f", b"f\n")]);
    let mut chain = vec![t0];
    for n in 0..=MAX_REVERT_CHANGESETS {
        let file = format!("c{n}");
        let next = advance(&repo, ACTOR, &[chain[n]], vec![put(&file, b"c\n")]);
        chain.push(next);
    }
    let last = *chain.last().unwrap();
    let frames = frame_count(&root);

    // 1. One past the bound, from t0.
    let mut revert = reverting(last);
    revert.since = Some(t0);
    match repo.revert(revert) {
        Err(RepoError::RevertTooLong { changeset, max }) => {
            assert_eq!((changeset, max), (last, MAX_REVERT_CHANGESETS));
        }
        other => panic!("expected RevertTooLong, got {other:?}"),
    }
    assert_eq!(frame_count(&root), frames);

    // 2. Exactly the bound, from the first changeset above t0.
    let mut revert = reverting(last);
    revert.since = Some(chain[1]);
    let outcome = repo.revert(revert).unwrap();
    assert_eq!(outcome.status, LandStatus::Landed);
}

/// The landed changeset records the range as `reverts` and the reason as `revert_reason`, beside
/// the land's own metadata; its default message names the changeset and its title, the reason
/// below. The same range reverted again without a key has nothing to land.
#[test]
fn a_revert_records_what_it_reverted() {
    let (_dir, _root, repo) = new_store();
    let t0 = advance(&repo, ACTOR, &[], vec![put("f", b"f\n")]);
    let a = advance(&repo, ACTOR, &[t0], vec![put("a", b"a\n")]);
    advance(&repo, LATER, &[a], vec![put("b", b"b\n")]);

    let mut revert = keyed(a, "revert-1");
    revert.metadata = vec![("agent".to_owned(), LANDER.as_bytes().to_vec())];
    let outcome = repo.revert(revert).unwrap();
    let landed = outcome.landed;
    assert_eq!(meta(&repo, landed, "reverts"), Some(a.to_string()));
    assert_eq!(
        meta(&repo, landed, "revert_reason"),
        Some(REASON.to_owned())
    );
    assert_eq!(meta(&repo, landed, "land_key").as_deref(), Some("revert-1"));
    assert_eq!(meta(&repo, landed, "lander").as_deref(), Some(LANDER));
    assert_eq!(meta(&repo, landed, "agent").as_deref(), Some(LANDER));
    let message = repo.changeset(landed).unwrap().unwrap().changeset.message;
    let expected = format!(
        "Revert {}: {ACTOR}'s change\n\n{REASON}\n",
        &a.to_string()[..12]
    );
    assert_eq!(String::from_utf8(message).unwrap(), expected);

    // The same range again: the target holds the inverse already.
    match repo.revert(reverting(a)) {
        Err(RepoError::NothingToLand(NothingToLandReason::NoChanges)) => {}
        other => panic!("expected NothingToLand, got {other:?}"),
    }
}

/// A revert is a land, and is undone as one: on main the undo is refused, as main moves only
/// forward ([`RepoError::HistoryProtected`]), and on a `release/<x>` ref it moves the ref back
/// to the head the revert landed on.
#[test]
fn a_revert_undoes_as_a_land() {
    let (_dir, _root, repo) = new_store();
    let t0 = advance(&repo, ACTOR, &[], vec![put("f", b"f\n")]);
    let a = advance(&repo, ACTOR, &[t0], vec![put("a", b"a\n")]);
    repo.cas_ref("release/1", None, Some(a), ACTOR).unwrap();
    let newest = |repo: &LocalRepo| {
        let (ops, _) = repo.op_log(None, 1).unwrap();
        OpId::new(ops[0].seq, ops[0].crc)
    };

    // 1. On main: refused, main where the revert left it.
    let on_main = repo.revert(reverting(a)).unwrap().landed;
    match repo.undo(newest(&repo), ACTOR) {
        Err(RepoError::HistoryProtected { name, .. }) => assert_eq!(name, "main"),
        other => panic!("expected HistoryProtected, got {other:?}"),
    }
    assert_eq!(main_head(&repo), on_main);

    // 2. On release/1: the ref back at A.
    let mut revert = reverting(a);
    revert.target = "release/1".to_owned();
    let on_release = repo.revert(revert).unwrap();
    assert_eq!(on_release.rebased_onto, a);
    repo.undo(newest(&repo), ACTOR).unwrap();
    let release = repo.get_ref("release/1").unwrap().unwrap().target;
    assert_eq!(release, a);
}
