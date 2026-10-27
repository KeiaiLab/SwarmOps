//! Rebase (operations spec §3, the store-only `rebase`): `Repo::rebase` merges a change, the
//! first-parent chain it ends since a base, three-way onto a ref's head as a land does, and
//! records the result as one changeset whose parent is that head, `H'`, moving no ref. A conflict
//! comes back as a land's does and records nothing; the same request made again is the same
//! changeset, found without a frame; a chain already on the head, or one the head holds, writes
//! nothing; and a rebase that crashes at any filesystem call has recorded `H'` once or not at all.
//!
//! Every test owns a fresh store, its writes checked against the store's ordering rules. The
//! crash sweep runs this test binary again as its children (selected by [`CHILD_ROOT`]).

use std::sync::Arc;
#[cfg(feature = "test-hooks")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(feature = "test-hooks")]
use std::thread;

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_core::{ChangesetId, Signature};
use swarmops_repo::land::LandRequest;
use swarmops_repo::rebase::{RebaseOutcome, RebaseRequest, RebaseStatus};
use swarmops_repo::{Advance, Change, CommitRequest, Depth, LocalRepo, RepoError};
use swarmops_store::{Journal, MetaStore, RealFs};
use swarmops_testkit::faults::checked;
use swarmops_testkit::repo::{put, request};
use swarmops_testkit::store::layout_of;
use tempfile::TempDir;

/// Who moves `main` in the tests' setup, and makes the changes they rebase.
pub(crate) const ACTOR: &str = "test/agent";

/// The agent of the target's changesets that a change is rebased onto.
pub(crate) const OTHER: &str = "test/other";

/// Who rebases and lands: the name of the recorded changeset's author and committer.
pub(crate) const REBASER: &str = "test/rebaser";

/// A fresh store, its writes checked against the store's ordering rules.
pub(crate) fn new_store() -> (TempDir, Utf8PathBuf, LocalRepo) {
    let dir = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
    let repo = LocalRepo::create_with_fs(&root, "test", checked(RealFs::new())).unwrap();
    (dir, root, repo)
}

/// The rebaser's signature: fixed, so that the same rebase makes the same changeset.
pub(crate) fn rebaser() -> Signature {
    Signature::new(
        REBASER.as_bytes().to_vec(),
        b"rebaser@test.swarmops.local".to_vec(),
        1_760_000_200,
        0,
        false,
    )
    .unwrap()
}

/// A rebase of `head` onto `main` by [`rebaser`], with the defaults ([`RebaseRequest::new`]).
pub(crate) fn rebasing(head: ChangesetId) -> RebaseRequest {
    RebaseRequest::new(head, "main", rebaser())
}

/// A land of `head` onto `main` by [`rebaser`], with the defaults ([`LandRequest::new`]).
pub(crate) fn landing(head: ChangesetId) -> LandRequest {
    LandRequest::new(head, "main", rebaser())
}

/// A request for `changes` on `parents`, recording `agent` in its metadata.
fn by(agent: &str, parents: &[ChangesetId], changes: Vec<Change>) -> CommitRequest {
    let mut req = request(parents, changes, "a test commit");
    req.metadata = vec![("agent".to_owned(), agent.as_bytes().to_vec())];
    req
}

/// Commits `changes` on `parents` by `agent`; the new changeset's id.
#[track_caller]
pub(crate) fn commit_by(
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
pub(crate) fn advance(
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
pub(crate) fn main_head(repo: &LocalRepo) -> ChangesetId {
    repo.get_ref("main").unwrap().expect("main exists").target
}

/// How many frames the journal of the store at `root` lists.
pub(crate) fn frame_count(root: &Utf8Path) -> u64 {
    Journal::open(&layout_of(root), Arc::new(RealFs::new()))
        .unwrap()
        .frame_count()
        .unwrap()
}

/// The kinds of the records of the frames past the first `frames` of the store at `root`.
fn kinds_past(root: &Utf8Path, frames: u64) -> Vec<&'static str> {
    let meta = MetaStore::open(root).unwrap();
    let entries = (meta.read().unwrap())
        .journal_after(frames.checked_sub(1), 100)
        .unwrap();
    entries.into_iter().map(|entry| entry.kind).collect()
}

/// Changeset `id`'s metadata `key`, if it has one.
pub(crate) fn meta_value(repo: &LocalRepo, id: ChangesetId, key: &str) -> Option<Vec<u8>> {
    let stored = repo.changeset(id).unwrap().expect("the changeset");
    (stored.changeset.metadata.into_iter())
        .find(|(name, _)| name == key)
        .map(|(_, value)| value)
}

/// What the file at `p` holds in changeset `id`'s tree; `None` if there is none.
fn content(repo: &LocalRepo, id: ChangesetId, p: &str) -> Option<Vec<u8>> {
    let at = swarmops_testkit::repo::path(p);
    repo.read_blob_at(id, &at)
        .unwrap()
        .map(|(_, _, bytes)| bytes)
}

/// A store whose `main` moved on from `t0` to `t1` after an agent's change, `head`, was made on
/// `t0`: the change puts `b`, the target `c`.
fn moved_on() -> (TempDir, Utf8PathBuf, LocalRepo, [ChangesetId; 3]) {
    let (dir, root, repo) = new_store();
    let t0 = advance(&repo, ACTOR, &[], vec![put("a", b"a\n")]);
    let head = commit_by(&repo, ACTOR, &[t0], vec![put("b", b"b\n")]);
    let t1 = advance(&repo, OTHER, &[t0], vec![put("c", b"c\n")]);
    (dir, root, repo, [t0, head, t1])
}

/// A rebase records one changeset, `H'`, on the target's head: its one parent that head, its tree
/// the change merged onto it, its message the head's, its author and committer the rebaser, its
/// metadata the request's and the chain it squashed. No ref moves: the one frame holds the
/// changeset's insert alone.
#[test]
fn rebase_records_one_changeset_on_the_target_head_and_moves_no_ref() {
    let (_dir, root, repo, [t0, head, t1]) = moved_on();
    let frames = frame_count(&root);

    // 1. Rebase the change onto main's head, with metadata of the caller's.
    let mut req = rebasing(head);
    req.metadata = vec![("agent".to_owned(), REBASER.as_bytes().to_vec())];
    let out = repo.rebase(req).unwrap();

    // 2. What it says: recorded, on t1, from t0, one path changed.
    assert_eq!(out.status, RebaseStatus::Rebased);
    assert_eq!((out.onto, out.base), (t1, t0));
    assert_eq!(
        (out.files_changed, out.squashed_from.clone()),
        (1, vec![head])
    );
    assert_eq!(out.auto_merged, []);

    // 3. The changeset itself.
    let stored = repo
        .changeset(out.changeset)
        .unwrap()
        .expect("H' is recorded");
    let changeset = &stored.changeset;
    assert_eq!(changeset.parents, [t1]);
    assert_eq!(changeset.root, out.root);
    assert_eq!(changeset.message, b"a test commit");
    assert_eq!(
        (&changeset.author, &changeset.committer),
        (&rebaser(), &rebaser())
    );
    for (p, want) in [("a", b"a\n"), ("b", b"b\n"), ("c", b"c\n")] {
        assert_eq!(
            content(&repo, out.changeset, p).as_deref(),
            Some(&want[..]),
            "{p}"
        );
    }
    let squashed = head.to_string().into_bytes();
    assert_eq!(
        changeset.metadata,
        [
            ("agent".to_owned(), REBASER.as_bytes().to_vec()),
            ("squashed_from".to_owned(), squashed),
        ]
    );

    // 4. One frame, an insert and no ref move; main where it was.
    assert_eq!(frame_count(&root), frames + 1);
    assert_eq!(kinds_past(&root, frames), ["changeset"]);
    assert_eq!(main_head(&repo), t1);
}

/// A rebase whose change conflicts with the target's head fails as a land of the same change
/// does, with the same report, field by field, and records nothing.
#[test]
fn rebase_conflicts_report_as_land_does_and_record_nothing() {
    let (_dir, root, repo) = new_store();
    let t0 = advance(&repo, ACTOR, &[], vec![put("f", b"1\n2\n3\n")]);
    let head = commit_by(&repo, ACTOR, &[t0], vec![put("f", b"1\nours\n3\n")]);
    let x = advance(&repo, OTHER, &[t0], vec![put("f", b"1\ntheirs\n3\n")]);
    let (frames, before) = (frame_count(&root), main_head(&repo));

    // 1. The rebase and the land of the same change, onto the same head.
    let Err(RepoError::Conflict(rebased)) = repo.rebase(rebasing(head)) else {
        panic!("the rebase conflicts");
    };
    let Err(RepoError::Conflict(landed)) = repo.land(landing(head)) else {
        panic!("the land conflicts");
    };

    // 2. One report, naming who touched the path on the target; nothing written.
    assert_eq!(rebased, landed);
    assert_eq!((rebased.base, rebased.ours, rebased.theirs), (t0, head, x));
    let touched = rebased.last_touched.get(&swarmops_testkit::repo::path("f"));
    assert_eq!(touched, Some(&(x, OTHER.to_owned())));
    assert_eq!((frame_count(&root), main_head(&repo)), (frames, before));
}

/// The same rebase made again on the same head of the target, the same signature and metadata
/// (a keyed rebase's, whose time is its key's), is the same changeset: found, and no frame.
#[test]
fn a_keyed_rebase_replays_without_a_frame() {
    let (_dir, root, repo, [_, head, t1]) = moved_on();
    let keyed = || {
        let mut rebase = rebasing(head);
        rebase.metadata = vec![(
            "commit_key".to_owned(),
            b"01890a5d-ac96-774b-bcce-b302099a8057".to_vec(),
        )];
        rebase
    };
    let first = repo.rebase(keyed()).unwrap();
    let frames = frame_count(&root);

    let again = repo.rebase(keyed()).unwrap();
    assert_eq!(again.status, RebaseStatus::Replayed);
    assert_eq!(
        (again.changeset, again.root, again.onto, again.files_changed),
        (first.changeset, first.root, t1, first.files_changed)
    );
    assert_eq!(frame_count(&root), frames);
}

/// A chain whose base is the target's head sits on it already: `up_to_date`, the head itself,
/// and nothing written.
#[test]
fn rebase_of_a_chain_already_on_the_target_is_up_to_date() {
    let (_dir, root, repo) = new_store();
    let t0 = advance(&repo, ACTOR, &[], vec![put("a", b"a\n")]);
    let h1 = commit_by(&repo, ACTOR, &[t0], vec![put("b", b"b\n")]);
    let head = commit_by(&repo, ACTOR, &[h1], vec![put("c", b"c\n")]);
    let frames = frame_count(&root);

    let out = repo.rebase(rebasing(head)).unwrap();
    assert_eq!(out.status, RebaseStatus::UpToDate);
    assert_eq!((out.changeset, out.onto, out.base), (head, t0, t0));
    let root_of = |id| repo.changeset(id).unwrap().expect("stored").changeset.root;
    assert_eq!(out.root, root_of(head));
    assert_eq!(out.files_changed, 2, "b and c, against the target's head");
    assert_eq!(frame_count(&root), frames);
}

/// A chain whose change the target holds already, made there again or merged in, is absorbed:
/// `absorbed`, the target's head, and nothing written. So is a head on the target's chain.
#[test]
fn rebase_of_a_chain_that_the_target_holds_is_absorbed() {
    let (_dir, root, repo) = new_store();
    let t0 = advance(&repo, ACTOR, &[], vec![put("a", b"a\n")]);
    let head = commit_by(&repo, ACTOR, &[t0], vec![put("b", b"b\n")]);
    let t1 = advance(
        &repo,
        OTHER,
        &[t0],
        vec![put("b", b"b\n"), put("c", b"c\n")],
    );
    let frames = frame_count(&root);

    // 1. The target made the same change.
    let out = repo.rebase(rebasing(head)).unwrap();
    assert_eq!(out.status, RebaseStatus::Absorbed);
    assert_eq!((out.changeset, out.onto, out.base), (t1, t1, t0));
    let root_of = |id| repo.changeset(id).unwrap().expect("stored").changeset.root;
    assert_eq!((out.root, out.files_changed), (root_of(t1), 0));

    // 2. A head on the target's first-parent chain.
    let on = repo.rebase(rebasing(t0)).unwrap();
    assert_eq!(
        (on.status, on.changeset, on.onto),
        (RebaseStatus::Absorbed, t1, t1)
    );
    assert_eq!(frame_count(&root), frames);
}

/// A rebased changeset is no land: it has no `lander` and no `land_key`, whatever the request,
/// so that a changeset with `lander` is one a land made.
#[test]
fn a_rebased_changeset_has_no_lander_and_no_land_key() {
    let (_dir, _root, repo, [_, head, _]) = moved_on();
    let out = repo.rebase(rebasing(head)).unwrap();
    assert_eq!(meta_value(&repo, out.changeset, "lander"), None);
    assert_eq!(meta_value(&repo, out.changeset, "land_key"), None);
    let squashed = head.to_string().into_bytes();
    assert_eq!(
        meta_value(&repo, out.changeset, "squashed_from"),
        Some(squashed)
    );
}

/// Set only in the children the crash sweep spawns: the store root.
#[cfg(feature = "test-hooks")]
const CHILD_ROOT: &str = "SWARMOPS_TEST_REPO_REBASE_CHILD_ROOT";

/// Set with [`CHILD_ROOT`]: the head the child rebases, as a full id.
#[cfg(feature = "test-hooks")]
const CHILD_HEAD: &str = "SWARMOPS_TEST_REPO_REBASE_CHILD_HEAD";

/// How many runs of the sweep go at once: each has a child process and a store of its own.
#[cfg(feature = "test-hooks")]
const AT_ONCE: u64 = 8;

/// What the sweep rebases: on a store whose `main` moved on to `t1`, an agent's change `head`;
/// `want` is the changeset the rebase records.
#[cfg(feature = "test-hooks")]
#[derive(Debug, Clone, Copy)]
struct Swept {
    t1: ChangesetId,
    head: ChangesetId,
    want: ChangesetId,
}

/// The sweep's template, and what the rebase records, made on a copy.
#[cfg(feature = "test-hooks")]
fn swept_template() -> (TempDir, Utf8PathBuf, Swept) {
    let (dir, template, repo, [_, head, t1]) = moved_on();
    drop(repo);
    let (_copy, root) = copy_of(&template);
    let repo = LocalRepo::open_with_fs(&root, checked(RealFs::new())).unwrap();
    let want = repo.rebase(rebasing(head)).unwrap().changeset;
    (dir, template, Swept { t1, head, want })
}

/// A fresh copy of `template`, in a temp dir of its own.
#[cfg(feature = "test-hooks")]
fn copy_of(template: &Utf8Path) -> (TempDir, Utf8PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
    swarmops_testkit::fs::copy_dir(template.as_std_path(), root.as_std_path());
    (dir, root)
}

/// One run of the sweep: a child rebases on a copy of `template` until its k-th filesystem call
/// aborts it, every call checked against the store's write-ordering rules. `H'` must then be
/// recorded or not, `main` unmoved, the store must verify clean, and the same rebase again must
/// end with `H'` recorded: found if the crash left it there. Returns whether the child finished,
/// and whether the crash left `H'` recorded.
#[cfg(feature = "test-hooks")]
fn rebase_crashing_at(template: &Utf8Path, swept: &Swept, k: u64) -> (bool, bool) {
    use std::os::unix::process::ExitStatusExt;

    use swarmops_store::{CRASH_AT_ENV, ORDER_CHECK_ENV};
    use swarmops_testkit::child::{SIGABRT, run, this_test};
    use swarmops_testkit::test_name;

    // 1. The child, crashing at call k.
    let (_dir, root) = copy_of(template);
    let mut command = this_test(&test_name!("a_rebase_crashing_at_any_call_records_once"));
    command
        .env(CHILD_ROOT, &root)
        .env(CHILD_HEAD, swept.head.to_string())
        .env(ORDER_CHECK_ENV, "1")
        .env(CRASH_AT_ENV, k.to_string());
    let child = run(&mut command);
    let finished = child.status.success();
    assert!(
        finished || child.status.signal() == Some(SIGABRT),
        "k={k}: {:?}\n{}",
        child.status,
        String::from_utf8_lossy(&child.stderr)
    );

    // 2. What it left: H' or nothing, main unmoved, a clean store.
    let what = format!("k={k}");
    let repo = LocalRepo::open_with_fs(&root, checked(RealFs::new())).unwrap();
    let recorded = repo.changeset(swept.want).unwrap().is_some();
    assert!(
        recorded || !finished,
        "{what}: the rebase returned, H' is not recorded"
    );
    assert_eq!(main_head(&repo), swept.t1, "{what}");
    assert_eq!(repo.verify(Depth::Deep).unwrap().problems, [], "{what}");

    // 3. The same rebase again.
    let again: RebaseOutcome = repo.rebase(rebasing(swept.head)).unwrap();
    let status = if recorded {
        RebaseStatus::Replayed
    } else {
        RebaseStatus::Rebased
    };
    assert_eq!(
        (again.status, again.changeset),
        (status, swept.want),
        "{what}"
    );
    assert_eq!(main_head(&repo), swept.t1, "{what}");
    (finished, recorded)
}

/// A rebase that crashes at any filesystem call has recorded `H'` once or not at all, moved no
/// ref, and left a store that verifies clean; made again it records `H'`, or finds it. Some crash
/// comes after the commit point.
#[test]
#[cfg(feature = "test-hooks")]
fn a_rebase_crashing_at_any_call_records_once() {
    use swarmops_core::ObjectId;

    // 1. A child: rebase, then report nothing (its exit says how it ended).
    if let Ok(root) = std::env::var(CHILD_ROOT) {
        let head = std::env::var(CHILD_HEAD).expect("the head to rebase");
        let head = ChangesetId::new(ObjectId::from_hex(&head).expect("a full id"));
        let repo = LocalRepo::open(Utf8Path::new(&root)).unwrap();
        repo.rebase(rebasing(head)).unwrap();
        return;
    }

    // 2. The sweep: k = 1, 2, … until a child finishes, AT_ONCE at a time.
    let (_template_dir, template, swept) = swept_template();
    let past_the_commit_point = AtomicBool::new(false);
    let mut first = 1;
    let calls = loop {
        let finished: Vec<bool> = thread::scope(|scope| {
            let runs: Vec<_> = (first..first + AT_ONCE)
                .map(|k| {
                    let (template, swept) = (&template, &swept);
                    scope.spawn(move || rebase_crashing_at(template, swept, k))
                })
                .collect();
            let ends = runs.into_iter().map(|run| run.join().unwrap());
            ends.map(|(finished, recorded)| {
                if recorded && !finished {
                    past_the_commit_point.store(true, Ordering::Relaxed);
                }
                finished
            })
            .collect()
        });
        if let Some(at) = finished.iter().position(|&done| done) {
            assert!(finished[at..].iter().all(|&done| done), "{finished:?}");
            break first + u64::try_from(at).unwrap() - 1;
        }
        first += AT_ONCE;
        assert!(first < 2_000, "the child never finished");
    };

    // 3. The sweep reached past the commit point.
    assert!(calls > 10, "the child finished after {calls} calls");
    assert!(
        past_the_commit_point.load(Ordering::Relaxed),
        "no crash came after the commit point"
    );
}
