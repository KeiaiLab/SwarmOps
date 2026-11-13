//! Lands in one batch (the M18 D2 design §3, slice D2-4): a land whose publish finds the store's
//! lock taken leaves a request in `meta/queue/` and waits; the next lock holder plans it in a
//! frame of its own after its own, relocated onto the member before it, and commits the batch
//! with one sync of `journal.log` and one replace of each moved ref.
//!
//! The in-process tests drive one batch with a known leader ([`run_batch`]):
//!
//! ```text
//!  test            leader thread            waiter threads
//!  holds meta/lock
//!                  queues <rid>.req
//!  arms the gate,
//!  lets the lock go -> takes the lock, its first call held
//!                                           queue, in order
//!  opens the gate  -> plans its own, then the waiters'; one batch
//! ```
//!
//! The crash sweep runs this test binary again as the leader, on seeded requests of lands a
//! killed child queued ([`seeded_template`]).

use std::collections::HashSet;
use std::fs;
use std::io;
#[cfg(feature = "test-hooks")]
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
#[cfg(feature = "test-hooks")]
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use camino::{Utf8Path, Utf8PathBuf};
#[cfg(feature = "test-hooks")]
use swarmops_core::ObjectId;
use swarmops_core::{ChangesetId, Signature};
use swarmops_repo::land::{LandOutcome, LandRequest, LandStatus};
use swarmops_repo::{Advance, Change, CommitRequest, Depth, LocalRepo, OpId, RepoError};
#[cfg(feature = "test-hooks")]
use swarmops_store::{CRASH_AT_ENV, LEAD_WAIT_ENV, ORDER_CHECK_ENV};
use swarmops_store::{FsMethod, Journal, MetaStore, RealFs};
#[cfg(feature = "test-hooks")]
use swarmops_testkit::child::{Child, SIGABRT, run, this_test};
use swarmops_testkit::faults::checked;
#[cfg(feature = "test-hooks")]
use swarmops_testkit::fs::copy_dir;
use swarmops_testkit::fs::{Call, Watch, Watched};
use swarmops_testkit::repo::{put, request};
use swarmops_testkit::store::layout_of;
#[cfg(feature = "test-hooks")]
use swarmops_testkit::test_name;
use tempfile::TempDir;

/// Who makes the changes the tests land, and moves `main` in their setup.
const ACTOR: &str = "test/agent";

/// Who lands: the landed changesets' committer.
const LANDER: &str = "test/lander";

/// How long a test waits for a file, a thread or a gate before it fails.
const PATIENCE: Duration = Duration::from_secs(10);

/// A fresh store, its writes checked against the store's ordering rules.
fn new_store() -> (TempDir, Utf8PathBuf, LocalRepo) {
    let dir = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
    let repo = LocalRepo::create_with_fs(&root, "test", checked(RealFs::new())).unwrap();
    (dir, root, repo)
}

/// The lander's signature: fixed, so that the same land makes the same changeset.
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

/// A land of `head` onto `target`, keyed `key`.
fn keyed_on(head: ChangesetId, target: &str, key: &str) -> LandRequest {
    let mut land = LandRequest::new(head, target, lander());
    land.key = Some(key.to_owned());
    land
}

/// A land of `head` onto `main`, keyed `key`.
fn keyed(head: ChangesetId, key: &str) -> LandRequest {
    keyed_on(head, "main", key)
}

/// A request for `changes` on `parents`.
fn by(parents: &[ChangesetId], changes: Vec<Change>) -> CommitRequest {
    request(parents, changes, "a test commit")
}

/// Commits `changes` on `parents`, moving `main` to the new changeset.
#[track_caller]
fn advance(repo: &LocalRepo, parents: &[ChangesetId], changes: Vec<Change>) -> ChangesetId {
    let mut req = by(parents, changes);
    req.advance = Some(Advance::new("main", ACTOR));
    repo.commit(req).unwrap().id
}

/// Commits `changes` on `parents`, moving no ref.
#[track_caller]
fn commit_on(repo: &LocalRepo, parents: &[ChangesetId], changes: Vec<Change>) -> ChangesetId {
    repo.commit(by(parents, changes)).unwrap().id
}

/// What `main` names.
fn main_head(repo: &LocalRepo) -> ChangesetId {
    repo.get_ref("main").unwrap().expect("main exists").target
}

/// Changeset `id`'s first parent.
fn first_parent(repo: &LocalRepo, id: ChangesetId) -> Option<ChangesetId> {
    let stored = repo.changeset(id).unwrap().expect("the changeset");
    stored.changeset.parents.first().copied()
}

/// How many frames the journal of the store at `root` lists.
fn frame_count(root: &Utf8Path) -> u64 {
    Journal::open(&layout_of(root), Arc::new(RealFs::new()))
        .unwrap()
        .frame_count()
        .unwrap()
}

/// The names in the store's `meta/queue/`, sorted; none if it does not exist.
fn queue_files(root: &Utf8Path) -> Vec<String> {
    let dir = layout_of(root).queue_dir();
    let Ok(entries) = fs::read_dir(dir.as_std_path()) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

/// The requests waiting in the store's queue: its `.req` files.
fn requests(root: &Utf8Path) -> Vec<String> {
    let mut names = queue_files(root);
    names.retain(|name| {
        !name.starts_with('.') && Path::new(name).extension().is_some_and(|e| e == "req")
    });
    names
}

/// Waits until the store's queue holds `n` requests; their names.
#[track_caller]
fn wait_requests(root: &Utf8Path, n: usize) -> Vec<String> {
    let limit = Instant::now() + PATIENCE;
    loop {
        let found = requests(root);
        if found.len() >= n {
            return found;
        }
        assert!(
            Instant::now() < limit,
            "the queue holds {found:?}, not {n} requests"
        );
        thread::sleep(Duration::from_millis(1));
    }
}

// ---- One batch, its leader known ----

/// One filesystem call a lander made while the batch ran: who made it (0 the leader, then the
/// waiters in order), the method and the path.
#[derive(Debug, Clone)]
struct Made {
    who: usize,
    method: FsMethod,
    path: PathBuf,
}

/// The calls of every lander, recorded once the batch starts.
#[derive(Default)]
struct Log {
    on: AtomicBool,
    calls: Mutex<Vec<Made>>,
}

/// Where the leader is: free to call, armed to hold its next call, holding it, or let go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Free,
    Armed,
    Holding,
    Gone,
}

/// The leader's gate: once armed, its next filesystem call (the first under the lock) waits
/// until the test lets it go.
struct Gate {
    phase: Mutex<Phase>,
    turned: Condvar,
}

impl Gate {
    fn new() -> Gate {
        Gate {
            phase: Mutex::new(Phase::Free),
            turned: Condvar::new(),
        }
    }

    /// Sets the phase to `to`.
    fn set(&self, to: Phase) {
        *self.phase.lock().unwrap() = to;
        self.turned.notify_all();
    }

    /// Waits, within [`PATIENCE`], until the phase is `want`.
    fn wait_for(&self, want: Phase) {
        let phase = self.phase.lock().unwrap();
        let (phase, waited) = self
            .turned
            .wait_timeout_while(phase, PATIENCE, |phase| *phase != want)
            .unwrap();
        assert!(
            !waited.timed_out(),
            "the gate is {:?}, not {want:?}",
            *phase
        );
    }

    /// The phase now.
    fn phase(&self) -> Phase {
        *self.phase.lock().unwrap()
    }

    /// Holds the call if armed.
    fn pass(&self) {
        let mut phase = self.phase.lock().unwrap();
        if *phase != Phase::Armed {
            return;
        }
        *phase = Phase::Holding;
        self.turned.notify_all();
        let _released = self
            .turned
            .wait_while(phase, |phase| *phase == Phase::Holding)
            .unwrap();
    }
}

/// What a lander's filesystem does around each call: the leader's gate, an injected failure of
/// the leader's first call of one method under one path once let go, and the log.
struct Probe {
    who: usize,
    log: Arc<Log>,
    gate: Option<Arc<Gate>>,
    /// The method and the path of the call to fail, and whether it failed already.
    fail: Option<(FsMethod, PathBuf, Arc<AtomicBool>)>,
}

impl Watch for Probe {
    fn before(&self, call: &Call<'_>) -> io::Result<()> {
        // 1. The leader's gate.
        if let Some(gate) = &self.gate {
            gate.pass();
        }

        // 2. The leader's first such call once let go, failed.
        let gone = self.gate.as_ref().is_some_and(|g| g.phase() == Phase::Gone);
        if let Some((method, under, failed)) = &self.fail
            && gone
            && call.method == *method
            && call.path().starts_with(under)
            && !failed.swap(true, Ordering::SeqCst)
        {
            return Err(io::Error::other("injected: a call of the leader"));
        }

        // 3. The log.
        if self.log.on.load(Ordering::SeqCst) {
            self.log.calls.lock().unwrap().push(Made {
                who: self.who,
                method: call.method,
                path: call.path().to_owned(),
            });
        }
        Ok(())
    }
}

/// Which of [`run_batch`]'s leader's calls fails once the batch starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reads {
    /// None.
    Plain,
    /// Its first object read.
    FailFirst,
    /// Its first append to `journal.off`: the commit's listing, after the commit point.
    FailListing,
}

/// What [`run_batch`] gives: each land's end, in the order given, and the calls made while the
/// batch ran.
struct Ran {
    ends: Vec<Result<LandOutcome, RepoError>>,
    calls: Vec<Made>,
    /// Whether the leader's call failed as [`Reads`] asked.
    read_failed: bool,
}

/// Lands `lands` on the store at `root` in one batch: `lands[0]` leads it and the others are
/// its members, queued in order, while the test holds the lock and then the leader's first
/// call under it (the module documentation). Every lander opens a handle of its own, its calls
/// checked against the store's ordering rules.
fn run_batch(root: &Utf8Path, lands: Vec<LandRequest>, reads: Reads) -> Ran {
    let log = Arc::new(Log::default());
    let gate = Arc::new(Gate::new());
    let failed = Arc::new(AtomicBool::new(false));
    let fail = match reads {
        Reads::Plain => None,
        Reads::FailFirst => Some((FsMethod::Read, root.join("objects"))),
        Reads::FailListing => Some((FsMethod::AppendDurable, layout_of(root).journal_off())),
    };
    let fs_of = |who: usize| {
        let leads = who == 0;
        let probe = Probe {
            who,
            log: Arc::clone(&log),
            gate: leads.then(|| Arc::clone(&gate)),
            fail: (fail.as_ref()).filter(|_| leads).map(|(method, under)| {
                (
                    *method,
                    under.clone().into_std_path_buf(),
                    Arc::clone(&failed),
                )
            }),
        };
        checked(Watched(probe, RealFs::new()))
    };

    let ends =
        thread::scope(|scope| {
            // 1. The lock held; the leader queued behind it.
            let other = MetaStore::open(root).unwrap();
            let taken = other.write().unwrap();
            let mut lands = lands.into_iter().enumerate();
            let (_, first) = lands.next().expect("a leader");
            let fs = fs_of(0);
            let mut landers = vec![scope.spawn(move || {
                LocalRepo::open_with_fs(root, fs).and_then(|repo| repo.land(first))
            })];
            wait_requests(root, 1);

            // 2. The lock to the leader, held at its first call.
            gate.set(Phase::Armed);
            drop(taken);
            gate.wait_for(Phase::Holding);

            // 3. The members, queued in order.
            for (who, land) in lands {
                let fs = fs_of(who);
                landers.push(scope.spawn(move || {
                    LocalRepo::open_with_fs(root, fs).and_then(|repo| repo.land(land))
                }));
                wait_requests(root, who + 1);
            }

            // 4. The batch.
            log.on.store(true, Ordering::SeqCst);
            gate.set(Phase::Gone);
            let ends: Vec<_> = landers.into_iter().map(|l| l.join().unwrap()).collect();
            ends
        });
    let calls = std::mem::take(&mut *log.calls.lock().unwrap());
    Ran {
        ends,
        calls,
        read_failed: failed.load(Ordering::SeqCst),
    }
}

impl Ran {
    /// The `n`-th land's outcome, which must have landed or replayed.
    #[track_caller]
    fn landed(&self, n: usize) -> &LandOutcome {
        match &self.ends[n] {
            Ok(out) => out,
            Err(e) => panic!("land {n} failed: {e:?}"),
        }
    }

    /// How many calls of `method` on `path` the landers made.
    fn count(&self, method: FsMethod, path: &Utf8Path) -> usize {
        (self.calls.iter())
            .filter(|made| made.method == method && made.path == path.as_std_path())
            .count()
    }

    /// How many times `journal.log` was appended and synced: one per batch.
    fn log_syncs(&self, root: &Utf8Path) -> usize {
        self.count(FsMethod::AppendDurable, &layout_of(root).journal_log())
    }

    /// How many ref files were replaced.
    fn ref_replaces(&self, root: &Utf8Path) -> usize {
        let refs = layout_of(root).refs_dir();
        (self.calls.iter())
            .filter(|made| {
                made.method == FsMethod::AtomicReplace && made.path.starts_with(refs.as_std_path())
            })
            .count()
    }
}

/// A store whose `main` is `t0`, and `n` changes made on it by [`ACTOR`], each a file of its
/// own (`f<i>`): `(t0, heads)`.
fn changes(repo: &LocalRepo, n: usize) -> (ChangesetId, Vec<ChangesetId>) {
    let t0 = advance(repo, &[], vec![put("a", b"a\n")]);
    let heads = (0..n)
        .map(|i| commit_on(repo, &[t0], vec![put(&format!("f{i}"), b"f\n")]))
        .collect();
    (t0, heads)
}

/// Lands waiting on the lock land in one batch: a frame each, one sync of `journal.log`, one
/// replace of `main`'s file, and `main` on the last of them; the queue is left empty.
#[test]
fn waiting_lands_land_in_one_batch() {
    let (_dir, root, repo) = new_store();
    let (_, heads) = changes(&repo, 3);
    let frames = frame_count(&root);

    let lands = (heads.iter().enumerate())
        .map(|(i, &head)| keyed(head, &format!("key-{i}")))
        .collect();
    let ran = run_batch(&root, lands, Reads::Plain);

    for n in 0..3 {
        let out = ran.landed(n);
        assert_eq!((out.status, out.attempts), (LandStatus::Landed, 1), "{n}");
    }
    assert_eq!(frame_count(&root), frames + 3, "a frame each");
    assert_eq!(ran.log_syncs(&root), 1, "one sync of journal.log");
    assert_eq!(ran.ref_replaces(&root), 1, "one replace of main's file");
    assert_eq!(main_head(&repo), ran.landed(2).landed);
    assert_eq!(
        queue_files(&root),
        Vec::<String>::new(),
        "the queue is empty"
    );
}

/// Each member of a batch is made anew on the member before it: the leader's land on `t0`, the
/// next on the leader's, and so on, each in its first attempt; `main` is linear.
#[test]
fn a_batch_relocates_each_onto_the_last() {
    let (_dir, root, repo) = new_store();
    let (t0, heads) = changes(&repo, 3);

    let lands = (heads.iter().enumerate())
        .map(|(i, &head)| keyed(head, &format!("key-{i}")))
        .collect();
    let ran = run_batch(&root, lands, Reads::Plain);

    let mut onto = t0;
    for n in 0..3 {
        let out = ran.landed(n);
        assert_eq!(out.rebased_onto, onto, "{n} landed on the one before");
        assert_eq!((out.attempts, out.relocated), (1, n > 0), "{n}");
        assert_eq!(first_parent(&repo, out.landed), Some(onto), "{n}");
        onto = out.landed;
    }
    assert_eq!(main_head(&repo), onto);
    assert_eq!(ran.log_syncs(&root), 1, "one batch");
}

/// A member whose change overlaps the leader's in a way the lock holder does not merge is moved:
/// it records nothing in the batch, and its next attempt meets the conflict.
#[test]
fn an_overlapping_member_is_moved() {
    let (_dir, root, repo) = new_store();
    let t0 = advance(&repo, &[], vec![put("f", b"1\n2\n")]);
    let ours = commit_on(&repo, &[t0], vec![put("f", b"1\nours\n")]);
    let theirs = commit_on(&repo, &[t0], vec![put("f", b"1\ntheirs\n")]);
    let frames = frame_count(&root);

    let ran = run_batch(
        &root,
        vec![keyed(ours, "key-ours"), keyed(theirs, "key-theirs")],
        Reads::Plain,
    );

    assert_eq!(ran.landed(0).attempts, 1);
    assert!(
        matches!(ran.ends[1], Err(RepoError::Conflict(_))),
        "{:?}",
        ran.ends[1]
    );
    assert_eq!(main_head(&repo), ran.landed(0).landed);
    assert_eq!(frame_count(&root), frames + 1, "the leader's frame alone");
    assert_eq!(ran.log_syncs(&root), 1, "one batch, then nothing");
}

/// A member the leader cannot plan (here, its object read fails in the leader) is declined
/// before the claim: its lander publishes it again alone, in the same attempt, made anew on the
/// leader's land.
#[test]
fn a_declined_member_lands_alone() {
    let (_dir, root, repo) = new_store();
    let (_, heads) = changes(&repo, 2);
    let frames = frame_count(&root);

    let ran = run_batch(
        &root,
        vec![keyed(heads[0], "key-0"), keyed(heads[1], "key-1")],
        Reads::FailFirst,
    );

    assert!(ran.read_failed, "the leader's read of the member failed");
    let (leader, member) = (ran.landed(0), ran.landed(1));
    assert_eq!((leader.attempts, leader.relocated), (1, false));
    assert_eq!((member.attempts, member.relocated), (1, true));
    assert_eq!(member.rebased_onto, leader.landed);
    assert_eq!(main_head(&repo), member.landed);
    assert_eq!(frame_count(&root), frames + 2);
    assert_eq!(ran.log_syncs(&root), 2, "the batch, then the member alone");
    assert_eq!(
        queue_files(&root),
        Vec::<String>::new(),
        "the queue is empty"
    );
}

/// A land with an expected target behind another in a batch is never made anew: the target
/// moved, so it fails with `CasMismatch` and records nothing.
#[test]
fn an_exact_land_is_never_relocated_in_a_batch() {
    let (_dir, root, repo) = new_store();
    let (t0, heads) = changes(&repo, 2);
    let mut exact = keyed(heads[1], "key-1");
    exact.expect_target = Some(t0);

    let ran = run_batch(&root, vec![keyed(heads[0], "key-0"), exact], Reads::Plain);

    let leader = ran.landed(0).landed;
    match &ran.ends[1] {
        Err(RepoError::CasMismatch {
            actual, expected, ..
        }) => assert_eq!((*actual, *expected), (leader, t0)),
        other => panic!("expected CasMismatch, got {other:?}"),
    }
    assert_eq!(main_head(&repo), leader);
    assert_eq!(ran.log_syncs(&root), 1, "one batch");
}

/// The same change landed twice with one key in one batch lands once: the member's key check
/// sees the leader's frame, not committed yet, and replays the leader's land.
#[test]
fn a_key_twice_in_one_batch_lands_once() {
    let (_dir, root, repo) = new_store();
    let (_, heads) = changes(&repo, 1);
    let frames = frame_count(&root);

    let ran = run_batch(
        &root,
        vec![keyed(heads[0], "key-0"), keyed(heads[0], "key-0")],
        Reads::Plain,
    );

    let (leader, member) = (ran.landed(0), ran.landed(1));
    assert_eq!(leader.status, LandStatus::Landed);
    assert_eq!(
        (member.status, member.landed),
        (LandStatus::Replayed, leader.landed)
    );
    assert_eq!(frame_count(&root), frames + 1, "one frame of the key");
    assert_eq!(ran.log_syncs(&root), 1, "one batch");
}

/// One key landed on two targets in one batch lands once: the member's key check sees the
/// leader's move of `main`, and the member fails with `KeyReused` naming it.
#[test]
fn a_key_on_two_targets_in_one_batch_lands_once() {
    let (_dir, root, repo) = new_store();
    let (t0, heads) = changes(&repo, 1);
    repo.cas_ref("release/1", None, Some(t0), ACTOR).unwrap();
    let frames = frame_count(&root);

    let ran = run_batch(
        &root,
        vec![
            keyed(heads[0], "key-0"),
            keyed_on(heads[0], "release/1", "key-0"),
        ],
        Reads::Plain,
    );

    let leader = ran.landed(0).landed;
    match &ran.ends[1] {
        Err(RepoError::KeyReused {
            key,
            landed,
            target,
        }) => assert_eq!(
            (key.as_str(), *landed, target.as_str()),
            ("key-0", leader, "main")
        ),
        other => panic!("expected KeyReused, got {other:?}"),
    }
    assert_eq!(repo.get_ref("release/1").unwrap().unwrap().target, t0);
    assert_eq!(frame_count(&root), frames + 1, "one frame of the key");
    assert_eq!(ran.log_syncs(&root), 1, "one batch");
}

/// A batch whose commit fails to list its frames in `journal.off` is committed: each member's
/// warning names its operation by seq and CRC, as `op log` prints it, though no entry lists the
/// frame yet, since a snapshot reads the whole frames past the checkpoint from `journal.log`.
#[test]
fn a_members_post_commit_warning_names_its_crc() {
    let (_dir, root, repo) = new_store();
    let (_, heads) = changes(&repo, 2);

    let ran = run_batch(
        &root,
        vec![keyed(heads[0], "key-0"), keyed(heads[1], "key-1")],
        Reads::FailListing,
    );

    assert!(ran.read_failed, "the leader's listing failed");
    let member = ran.landed(1);
    let [warning] = &member.warnings[..] else {
        panic!("one warning: {:?}", member.warnings);
    };
    let op = (warning.split_once("(operation "))
        .and_then(|(_, rest)| rest.split_once(')'))
        .map_or("", |(op, _)| op);
    let op: OpId = op
        .parse()
        .unwrap_or_else(|_| panic!("no operation id in {warning:?}"));
    let listed = MetaStore::open(&root).unwrap();
    let (read, _) = listed.read_committed().unwrap();
    let frame = read.frame(op.seq).unwrap().expect("the member's frame");
    assert_eq!(op, OpId::new(frame.seq, frame.crc));
}

/// A member that lands with changes records its commit `C` and its land `L` in its frame of the
/// batch, read by the leader by their ids: `C` is in the store and `L` squashes it.
#[test]
fn a_land_with_changes_in_a_batch_records_c_and_l() {
    let (_dir, root, repo) = new_store();
    let (_, heads) = changes(&repo, 2);
    let mut with_changes = keyed(heads[1], "key-1");
    with_changes.commit = Some(by(&[heads[1]], vec![put("g", b"g\n")]));
    let frames = frame_count(&root);

    let ran = run_batch(
        &root,
        vec![keyed(heads[0], "key-0"), with_changes],
        Reads::Plain,
    );

    let member = ran.landed(1);
    let c = *member.squashed_from.last().expect("L squashes C");
    assert_ne!(c, heads[1], "C is the change's commit");
    assert_eq!(first_parent(&repo, c), Some(heads[1]));
    assert_eq!(member.rebased_onto, ran.landed(0).landed);
    assert_eq!(main_head(&repo), member.landed);
    assert_eq!(frame_count(&root), frames + 2, "a frame each");
    assert_eq!(ran.log_syncs(&root), 1, "one batch");
}

/// The objects every frame of a batch names are durable before the batch's log write (I1, W5):
/// the leader writes the members' relocated changesets and flushes them before it appends, and
/// the order checker on every lander finds nothing wrong.
#[test]
fn batch_objects_are_durable_before_its_frames() {
    let (_dir, root, repo) = new_store();
    let (_, heads) = changes(&repo, 3);

    let lands = (heads.iter().enumerate())
        .map(|(i, &head)| keyed(head, &format!("key-{i}")))
        .collect();
    let ran = run_batch(&root, lands, Reads::Plain);

    for n in 0..3 {
        assert_eq!(ran.landed(n).relocated, n > 0, "{n}");
    }
    let objects = root.join("objects");
    let leader: Vec<&Made> = ran.calls.iter().filter(|made| made.who == 0).collect();
    let append = (leader.iter())
        .position(|made| {
            made.method == FsMethod::AppendDurable
                && made.path == layout_of(&root).journal_log().as_std_path()
        })
        .expect("the leader appends the batch");
    let in_objects = |made: &&&Made| made.path.starts_with(objects.as_std_path());
    let placed = (leader.iter().filter(in_objects))
        .filter(|made| made.method == FsMethod::LinkNoclobber)
        .count();
    let flushed_at = (leader.iter().enumerate())
        .filter(|(_, made)| made.method == FsMethod::FsyncDir && in_objects(made))
        .map(|(at, _)| at)
        .max()
        .expect("the leader flushes the objects");
    assert!(placed > 0, "the leader writes the relocated changesets");
    assert!(
        flushed_at < append,
        "the objects are flushed before the append"
    );
    let after: Vec<&&Made> = leader[append..].iter().filter(in_objects).collect();
    assert!(after.is_empty(), "no object written after it: {after:?}");
}

/// A publish's request carries ids and small fields only: the request of a land whose delta
/// has 10,000 entries takes under 1 KiB.
#[test]
fn a_publish_request_carries_ids_only() {
    let (_dir, root, repo) = new_store();
    let t0 = advance(&repo, &[], vec![put("a", b"a\n")]);
    let wide: Vec<Change> = (0..10_000)
        .map(|i| put(&format!("d/{i:05}"), b"x\n"))
        .collect();
    let head = commit_on(&repo, &[t0], wide);
    drop(repo);

    let other = MetaStore::open(&root).unwrap();
    let taken = other.write().unwrap();
    let landed = thread::scope(|scope| {
        let lander = scope.spawn(|| LocalRepo::open(&root).and_then(|r| r.land(keyed(head, "k"))));
        let name = wait_requests(&root, 1).remove(0);
        let path = layout_of(&root).queue_dir().join(name);
        let size = fs::metadata(path.as_std_path()).unwrap().len();
        drop(taken);
        assert!(size < 1024, "the request takes {size} bytes");
        lander.join().unwrap()
    });
    assert_eq!(landed.unwrap().files_changed, 10_000);
}

// ---- The crash sweep ----

/// Set only in the children the sweep spawns: what the child is (`leader` or `waiter`).
#[cfg(feature = "test-hooks")]
const CHILD_ROLE: &str = "SWARMOPS_TEST_REPO_LAND_BATCH_ROLE";

/// Set with [`CHILD_ROLE`]: the store root.
#[cfg(feature = "test-hooks")]
const CHILD_ROOT: &str = "SWARMOPS_TEST_REPO_LAND_BATCH_ROOT";

/// Set with [`CHILD_ROLE`]: the head the child lands, as a full id.
#[cfg(feature = "test-hooks")]
const CHILD_HEAD: &str = "SWARMOPS_TEST_REPO_LAND_BATCH_HEAD";

/// Set with [`CHILD_ROLE`]: the key the child lands with.
#[cfg(feature = "test-hooks")]
const CHILD_KEY: &str = "SWARMOPS_TEST_REPO_LAND_BATCH_KEY";

/// How a child reports how its land ended, among the harness's output.
#[cfg(feature = "test-hooks")]
const REPORT: &str = "land-batch-child: ";

/// How many waiters the sweep seeds behind its leader.
#[cfg(feature = "test-hooks")]
const WAITERS: usize = 2;

/// How many runs of the sweep go at once: each has a child and a store of its own.
#[cfg(feature = "test-hooks")]
const AT_ONCE: u64 = 8;

/// The sweep's template: a store whose `main` is `t0`, the heads its lands land (the leader's
/// first, then the waiters'), and each waiter's request as a killed child queued it.
#[cfg(feature = "test-hooks")]
struct Seeded {
    t0: ChangesetId,
    heads: Vec<ChangesetId>,
    /// Each waiter's request file: its name and bytes.
    requests: Vec<(String, Vec<u8>)>,
}

/// The land the sweep's `n`-th lander makes: its head, keyed `key-<n>`.
#[cfg(feature = "test-hooks")]
fn sweep_land(heads: &[ChangesetId], n: usize) -> LandRequest {
    keyed(heads[n], &format!("key-{n}"))
}

/// A child's part of the sweep, if this process is one: lands [`CHILD_HEAD`] with [`CHILD_KEY`]
/// on the store at [`CHILD_ROOT`], opened with the hooks the environment sets, and reports how
/// it ended.
#[cfg(feature = "test-hooks")]
fn land_as_child() -> bool {
    if std::env::var(CHILD_ROLE).is_err() {
        return false;
    }
    let root = Utf8PathBuf::from(std::env::var(CHILD_ROOT).unwrap());
    let head = ObjectId::from_hex(&std::env::var(CHILD_HEAD).unwrap()).unwrap();
    let land = keyed(ChangesetId::new(head), &std::env::var(CHILD_KEY).unwrap());
    match LocalRepo::open(&root).and_then(|repo| repo.land(land)) {
        Ok(out) => println!("{REPORT}landed {}", out.landed),
        Err(e) => println!("{REPORT}failed {e:?}"),
    }
    true
}

/// A child of the sweep, `role`, making land `land` on the store at `root` with `hooks` set.
#[cfg(feature = "test-hooks")]
fn sweep_child(role: &str, root: &Utf8Path, land: &LandRequest, hooks: &[(&str, &str)]) -> Command {
    let mut command = this_test(&test_name!(
        "a_batch_of_lands_crashing_at_any_call_lands_each_once"
    ));
    command
        .env(CHILD_ROLE, role)
        .env(CHILD_ROOT, root)
        .env(CHILD_HEAD, land.head.to_string())
        .env(CHILD_KEY, land.key.as_deref().expect("a keyed land"));
    for (name, value) in hooks {
        command.env(name, value);
    }
    command
}

/// Builds the sweep's template: `t0`, a change per lander, and the request of each waiter's
/// land, which a child queued behind the lock this test held and was killed in.
#[cfg(feature = "test-hooks")]
fn seeded_template() -> (TempDir, Utf8PathBuf, Seeded) {
    let (dir, root, repo) = new_store();
    let (t0, heads) = changes(&repo, 1 + WAITERS);
    drop(repo);

    let mut requests = Vec::new();
    for n in 1..=WAITERS {
        let other = MetaStore::open(&root).unwrap();
        let taken = other.write().unwrap();
        let waiter = Child::spawn(&mut sweep_child(
            "waiter",
            &root,
            &sweep_land(&heads, n),
            &[],
        ));
        let name = wait_requests(&root, 1).remove(0);
        drop(waiter);
        let path = layout_of(&root).queue_dir().join(&name);
        requests.push((name, fs::read(path.as_std_path()).unwrap()));
        fs::remove_file(path.as_std_path()).unwrap();
        drop(taken);
    }
    (
        dir,
        root,
        Seeded {
            t0,
            heads,
            requests,
        },
    )
}

/// What one run of the sweep left: whether its leader finished, and the queue's files when it
/// ended.
#[cfg(feature = "test-hooks")]
struct Swept {
    finished: bool,
    left: Vec<String>,
}

/// One run of the sweep: on a copy of `template`, the seeded requests in the queue, each kept
/// alive by a flock this test holds, a child leads its land and theirs until its k-th
/// filesystem call aborts it. Then the waiters are let die, and every land is made again: each
/// key ends on `main` once, every land on the one before it, and the store verifies clean.
#[cfg(feature = "test-hooks")]
fn batch_crashing_at(template: &Utf8Path, seeded: &Seeded, k: u64) -> Swept {
    let dir = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
    copy_dir(template.as_std_path(), root.as_std_path());
    let what = format!("k={k}");

    // 1. The waiters, alive: their requests in place, flocked.
    let queue = layout_of(&root).queue_dir();
    fs::create_dir_all(queue.as_std_path()).unwrap();
    let alive: Vec<fs::File> = (seeded.requests.iter())
        .map(|(name, bytes)| {
            let path = queue.join(name);
            fs::write(path.as_std_path(), bytes).unwrap();
            let file = fs::File::open(path.as_std_path()).unwrap();
            file.lock().unwrap();
            file
        })
        .collect();

    // 2. The leader, crashing at its k-th call.
    let crash_at = k.to_string();
    let waiters = WAITERS.to_string();
    let hooks = [
        (CRASH_AT_ENV, crash_at.as_str()),
        (LEAD_WAIT_ENV, waiters.as_str()),
        (ORDER_CHECK_ENV, "1"),
    ];
    let child = run(&mut sweep_child(
        "leader",
        &root,
        &sweep_land(&seeded.heads, 0),
        &hooks,
    ));
    let finished = child.status.success();
    assert!(
        finished || child.status.signal() == Some(SIGABRT),
        "{what}: {:?}\n{}",
        child.status,
        String::from_utf8_lossy(&child.stderr)
    );
    let left = queue_files(&root);

    // 3. The waiters die; every land again.
    drop(alive);
    let repo = LocalRepo::open_with_fs(&root, checked(RealFs::new())).unwrap();
    assert_eq!(repo.verify(Depth::Deep).unwrap().problems, [], "{what}");
    for n in 0..=WAITERS {
        let again = repo.land(sweep_land(&seeded.heads, n));
        let again = again.unwrap_or_else(|e| panic!("{what}: land {n} again: {e:?}"));
        if finished {
            assert_eq!(again.status, LandStatus::Replayed, "{what}: land {n}");
        }
    }

    // 4. Each key once on main, every land on the one before.
    let mut keys = Vec::new();
    let mut at = main_head(&repo);
    while at != seeded.t0 {
        let stored = repo.changeset(at).unwrap().expect("on main");
        let key = (stored.changeset.metadata.iter())
            .find(|(name, _)| name == "land_key")
            .map(|(_, value)| String::from_utf8(value.clone()).unwrap());
        keys.push(key.unwrap_or_else(|| panic!("{what}: {at} carries no key")));
        at = first_parent(&repo, at).expect("main reaches t0");
    }
    let unique: HashSet<&String> = keys.iter().collect();
    assert_eq!(
        (keys.len(), unique.len()),
        (1 + WAITERS, 1 + WAITERS),
        "{what}: {keys:?}"
    );
    assert_eq!(repo.verify(Depth::Deep).unwrap().problems, [], "{what}");
    Swept { finished, left }
}

/// A batch of lands whose leader crashes at any filesystem call lands each once: for every k,
/// the leader's land and the two seeded waiters' are each on `main` once after the lands are
/// made again, and the store verifies clean. Some crash leaves claimed requests (`.com`), and
/// some answers (`.ans`).
#[test]
#[cfg(feature = "test-hooks")]
fn a_batch_of_lands_crashing_at_any_call_lands_each_once() {
    if land_as_child() {
        return;
    }
    let (_dir, template, seeded) = seeded_template();
    let crashed_with = |suffix: &str, runs: &[Swept]| {
        (runs.iter())
            .filter(|run| !run.finished)
            .any(|run| run.left.iter().any(|name| name.ends_with(suffix)))
    };
    let (template, seeded) = (&template, &seeded);
    let mut runs = Vec::new();
    let mut first = 1;
    loop {
        let batch: Vec<Swept> = thread::scope(|scope| {
            let started: Vec<_> = (first..first + AT_ONCE)
                .map(|k| scope.spawn(move || batch_crashing_at(template, seeded, k)))
                .collect();
            started.into_iter().map(|run| run.join().unwrap()).collect()
        });
        let done = batch.iter().position(|run| run.finished);
        runs.extend(batch);
        if let Some(at) = done {
            let tail = &runs[runs.len() - usize::try_from(AT_ONCE).unwrap() + at..];
            assert!(tail.iter().all(|run| run.finished), "a crash after the end");
            break;
        }
        first += AT_ONCE;
        assert!(first < 2_000, "the leader never finished");
    }
    assert!(
        runs.len() > 20,
        "the leader finished after {} calls",
        runs.len()
    );
    assert!(
        crashed_with(".com", &runs),
        "no crash left a claimed request"
    );
    assert!(crashed_with(".ans", &runs), "no crash left an answer");
}
