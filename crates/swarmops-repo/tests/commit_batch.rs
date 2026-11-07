//! Commits in one batch (the M18 D2 design §3, slice D2-5): a commit whose insert finds the
//! store's lock taken leaves a request in `meta/queue/` and waits; the next lock holder, a
//! commit or a land, plans it in a frame of its own after its own and commits the batch with one
//! sync of `journal.log`.
//!
//! The in-process tests drive one batch with a known leader ([`run_batch`]), as the lands' tests
//! do:
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
//! The crash sweep runs this test binary again as the leader, on seeded requests of commits a
//! killed child queued ([`seeded_template`]).

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
use swarmops_core::{ChangesetId, Signature};
use swarmops_repo::land::{LandOutcome, LandRequest, LandStatus};
use swarmops_repo::{Advance, Change, CommitOutcome, CommitRequest, LocalRepo, RepoError};
#[cfg(feature = "test-hooks")]
use swarmops_repo::{Advanced, Depth};
#[cfg(feature = "test-hooks")]
use swarmops_store::{CRASH_AT_ENV, LEAD_WAIT_ENV, ORDER_CHECK_ENV};
use swarmops_store::{FsMethod, InsertOutcome, Journal, MetaStore, RealFs};
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

/// Who makes the commits, and moves the refs they advance.
const ACTOR: &str = "test/agent";

/// How long a test waits for a file, a thread or a gate before it fails.
const PATIENCE: Duration = Duration::from_secs(10);

/// A fresh store, its writes checked against the store's ordering rules.
fn new_store() -> (TempDir, Utf8PathBuf, LocalRepo) {
    let dir = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
    let repo = LocalRepo::create_with_fs(&root, "test", checked(RealFs::new())).unwrap();
    (dir, root, repo)
}

/// A request for `changes` on `parents`.
fn by(parents: &[ChangesetId], changes: Vec<Change>) -> CommitRequest {
    request(parents, changes, "a test commit")
}

/// A request for `changes` on `parents` that advances ref `name`.
fn advancing(parents: &[ChangesetId], changes: Vec<Change>, name: &str) -> CommitRequest {
    let mut req = by(parents, changes);
    req.advance = Some(Advance::new(name, ACTOR));
    req
}

/// A store whose `main` is `t0`, made here: `t0`.
fn base(repo: &LocalRepo) -> ChangesetId {
    let req = advancing(&[], vec![put("a", b"a\n")], "main");
    repo.commit(req).unwrap().id
}

/// The lander's signature: fixed, so that the same land makes the same changeset.
fn lander() -> Signature {
    Signature::new(
        b"test/lander".to_vec(),
        b"lander@test.swarmops.local".to_vec(),
        1_760_000_100,
        0,
        false,
    )
    .unwrap()
}

/// What ref `name` names.
fn target(repo: &LocalRepo, name: &str) -> ChangesetId {
    repo.get_ref(name).unwrap().expect("the ref exists").target
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

/// One write a batch's member makes.
#[expect(
    clippy::large_enum_variant,
    reason = "a handful of jobs per test, each made once"
)]
enum Job {
    /// A commit.
    Commit(CommitRequest),
    /// A land.
    Land(LandRequest),
}

/// How a member's write ended.
#[derive(Debug)]
enum Ended {
    /// A commit's outcome.
    Committed(CommitOutcome),
    /// A land's outcome.
    Landed(LandOutcome),
}

impl Job {
    /// Makes this write on `repo`.
    fn make(self, repo: &LocalRepo) -> Result<Ended, RepoError> {
        match self {
            Job::Commit(req) => repo.commit(req).map(Ended::Committed),
            Job::Land(land) => repo.land(land).map(Ended::Landed),
        }
    }
}

/// One filesystem call a member made while the batch ran: the method and the path.
#[derive(Debug, Clone)]
struct Made {
    method: FsMethod,
    path: PathBuf,
}

/// The calls of every member, recorded once the batch starts.
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

/// What a member's filesystem does around each call: the leader's gate, then the log.
struct Probe {
    log: Arc<Log>,
    gate: Option<Arc<Gate>>,
}

impl Watch for Probe {
    fn before(&self, call: &Call<'_>) -> io::Result<()> {
        // 1. The leader's gate.
        if let Some(gate) = &self.gate {
            gate.pass();
        }

        // 2. The log.
        if self.log.on.load(Ordering::SeqCst) {
            self.log.calls.lock().unwrap().push(Made {
                method: call.method,
                path: call.path().to_owned(),
            });
        }
        Ok(())
    }
}

/// What [`run_batch`] gives: each write's end, in the order given, and the calls made while the
/// batch ran.
struct Ran {
    ends: Vec<Result<Ended, RepoError>>,
    calls: Vec<Made>,
}

/// Makes `jobs` on the store at `root` in one batch: `jobs[0]` leads it and the others are its
/// members, queued in order, while the test holds the lock and then the leader's first call
/// under it (the module documentation). Every member opens a handle of its own, its calls
/// checked against the store's ordering rules.
fn run_batch(root: &Utf8Path, jobs: Vec<Job>) -> Ran {
    let log = Arc::new(Log::default());
    let gate = Arc::new(Gate::new());
    let fs_of = |who: usize| {
        let probe = Probe {
            log: Arc::clone(&log),
            gate: (who == 0).then(|| Arc::clone(&gate)),
        };
        checked(Watched(probe, RealFs::new()))
    };

    let ends =
        thread::scope(|scope| {
            // 1. The lock held; the leader queued behind it.
            let other = MetaStore::open(root).unwrap();
            let taken = other.write().unwrap();
            let mut jobs = jobs.into_iter().enumerate();
            let (_, first) = jobs.next().expect("a leader");
            let fs = fs_of(0);
            let mut members = vec![scope.spawn(move || {
                LocalRepo::open_with_fs(root, fs).and_then(|repo| first.make(&repo))
            })];
            wait_requests(root, 1);

            // 2. The lock to the leader, held at its first call.
            gate.set(Phase::Armed);
            drop(taken);
            gate.wait_for(Phase::Holding);

            // 3. The members, queued in order.
            for (who, job) in jobs {
                let fs = fs_of(who);
                members.push(scope.spawn(move || {
                    LocalRepo::open_with_fs(root, fs).and_then(|repo| job.make(&repo))
                }));
                wait_requests(root, who + 1);
            }

            // 4. The batch.
            log.on.store(true, Ordering::SeqCst);
            gate.set(Phase::Gone);
            let ends: Vec<_> = members.into_iter().map(|m| m.join().unwrap()).collect();
            ends
        });
    let calls = std::mem::take(&mut *log.calls.lock().unwrap());
    Ran { ends, calls }
}

impl Ran {
    /// The `n`-th member's commit outcome, which must have succeeded.
    #[track_caller]
    fn committed(&self, n: usize) -> &CommitOutcome {
        match &self.ends[n] {
            Ok(Ended::Committed(out)) => out,
            other => panic!("commit {n}: {other:?}"),
        }
    }

    /// The `n`-th member's land outcome, which must have succeeded.
    #[track_caller]
    fn landed(&self, n: usize) -> &LandOutcome {
        match &self.ends[n] {
            Ok(Ended::Landed(out)) => out,
            other => panic!("land {n}: {other:?}"),
        }
    }

    /// How many calls of `method` on `path` the members made.
    fn count(&self, method: FsMethod, path: &Utf8Path) -> usize {
        (self.calls.iter())
            .filter(|made| made.method == method && made.path == path.as_std_path())
            .count()
    }

    /// How many times `journal.log` was appended and synced: one per batch.
    fn log_syncs(&self, root: &Utf8Path) -> usize {
        self.count(FsMethod::AppendDurable, &layout_of(root).journal_log())
    }
}

/// Whether `out` inserted its changeset.
fn inserted(out: &CommitOutcome) -> bool {
    matches!(out.insert, InsertOutcome::Inserted { .. })
}

/// Commits waiting on the lock commit in one batch: a frame each, one sync of `journal.log`,
/// each changeset in the store; the queue is left empty.
#[test]
fn waiting_commits_commit_in_one_batch() {
    let (_dir, root, repo) = new_store();
    let t0 = base(&repo);
    let frames = frame_count(&root);

    let jobs = (0..3)
        .map(|i| Job::Commit(by(&[t0], vec![put(&format!("f{i}"), b"f\n")])))
        .collect();
    let ran = run_batch(&root, jobs);

    for n in 0..3 {
        let out = ran.committed(n);
        assert!(inserted(out), "{n}: {:?}", out.insert);
        assert_eq!(first_parent(&repo, out.id), Some(t0), "{n}");
    }
    assert_eq!(frame_count(&root), frames + 3, "a frame each");
    assert_eq!(ran.log_syncs(&root), 1, "one sync of journal.log");
    assert_eq!(
        queue_files(&root),
        Vec::<String>::new(),
        "the queue is empty"
    );
}

/// A batched commit's advance compares against what the members before it left: a member that
/// advances `main` from `t0` after the leader advanced it is `RefMoved`, naming the leader's
/// changeset, and records nothing, while a member advancing another ref moves it.
#[test]
fn a_batched_advance_cases_against_the_last() {
    let (_dir, root, repo) = new_store();
    let t0 = base(&repo);
    repo.cas_ref("release/1", None, Some(t0), ACTOR).unwrap();
    let frames = frame_count(&root);

    let ran = run_batch(
        &root,
        vec![
            Job::Commit(advancing(&[t0], vec![put("f0", b"0\n")], "main")),
            Job::Commit(advancing(&[t0], vec![put("f1", b"1\n")], "main")),
            Job::Commit(advancing(&[t0], vec![put("f2", b"2\n")], "release/1")),
        ],
    );

    let leader = ran.committed(0).id;
    match &ran.ends[1] {
        Err(RepoError::RefMoved { name, actual, .. }) => {
            assert_eq!((name.as_str(), *actual), ("main", Some(leader)));
        }
        other => panic!("expected RefMoved, got {other:?}"),
    }
    let other = ran.committed(2);
    let moved = other.advanced.as_ref().expect("the advance");
    assert_eq!((moved.old, moved.already_applied), (Some(t0), false));
    assert_eq!(target(&repo, "main"), leader);
    assert_eq!(target(&repo, "release/1"), other.id);
    assert_eq!(
        frame_count(&root),
        frames + 2,
        "nothing of the moved member"
    );
    assert_eq!(ran.log_syncs(&root), 1, "one batch, then nothing");
    assert_eq!(
        queue_files(&root),
        Vec::<String>::new(),
        "the queue is empty"
    );
}

/// A member whose commit the store holds already, its advance made, is answered without a
/// frame: `AlreadyPresent`, already applied, and the batch's one sync the leader's.
#[test]
fn a_present_commit_is_answered_without_a_frame() {
    let (_dir, root, repo) = new_store();
    let t0 = base(&repo);
    repo.cas_ref("release/1", None, Some(t0), ACTOR).unwrap();
    let present = advancing(&[t0], vec![put("p", b"p\n")], "release/1");
    let id = repo.commit(present.clone()).unwrap().id;
    let frames = frame_count(&root);

    let ran = run_batch(
        &root,
        vec![
            Job::Commit(by(&[t0], vec![put("f", b"f\n")])),
            Job::Commit(present),
        ],
    );

    assert!(inserted(ran.committed(0)));
    let member = ran.committed(1);
    assert_eq!(
        (member.id, member.insert),
        (id, InsertOutcome::AlreadyPresent)
    );
    let moved = member.advanced.as_ref().expect("the advance");
    assert!(moved.already_applied);
    assert_eq!(frame_count(&root), frames + 1, "the leader's frame alone");
    assert_eq!(ran.log_syncs(&root), 1, "one batch");
    assert_eq!(
        queue_files(&root),
        Vec::<String>::new(),
        "the queue is empty"
    );
}

/// Commits and lands share a batch: a commit that advances `main` leads, a land made on the old
/// `main` is made anew on it, and a commit beside them is inserted; one sync of `journal.log`.
#[test]
fn commits_and_lands_share_a_batch() {
    let (_dir, root, repo) = new_store();
    let t0 = base(&repo);
    let head = repo.commit(by(&[t0], vec![put("g", b"g\n")])).unwrap().id;
    let frames = frame_count(&root);

    let mut land = LandRequest::new(head, "main", lander());
    land.key = Some("key-land".to_owned());
    let ran = run_batch(
        &root,
        vec![
            Job::Commit(advancing(&[t0], vec![put("f0", b"0\n")], "main")),
            Job::Land(land),
            Job::Commit(by(&[t0], vec![put("f2", b"2\n")])),
        ],
    );

    let leader = ran.committed(0).id;
    let landed = ran.landed(1);
    assert_eq!(landed.status, LandStatus::Landed);
    assert_eq!((landed.rebased_onto, landed.relocated), (leader, true));
    assert_eq!(first_parent(&repo, landed.landed), Some(leader));
    assert!(inserted(ran.committed(2)));
    assert_eq!(target(&repo, "main"), landed.landed);
    assert_eq!(frame_count(&root), frames + 3, "a frame each");
    assert_eq!(ran.log_syncs(&root), 1, "one batch");
    assert_eq!(
        queue_files(&root),
        Vec::<String>::new(),
        "the queue is empty"
    );
}

// ---- The crash sweep ----

/// Set only in the children the sweep spawns: what the child is (`leader` or `waiter`).
#[cfg(feature = "test-hooks")]
const CHILD_ROLE: &str = "SWARMOPS_TEST_REPO_COMMIT_BATCH_ROLE";

/// Set with [`CHILD_ROLE`]: the store root.
#[cfg(feature = "test-hooks")]
const CHILD_ROOT: &str = "SWARMOPS_TEST_REPO_COMMIT_BATCH_ROOT";

/// Set with [`CHILD_ROLE`]: which of the sweep's commits the child makes.
#[cfg(feature = "test-hooks")]
const CHILD_N: &str = "SWARMOPS_TEST_REPO_COMMIT_BATCH_N";

/// How a child reports how its commit ended, among the harness's output.
#[cfg(feature = "test-hooks")]
const REPORT: &str = "commit-batch-child: ";

/// The metadata key that names each of the sweep's commits.
#[cfg(feature = "test-hooks")]
const KEY: &str = "commit_key";

/// How many waiters the sweep seeds behind its leader.
#[cfg(feature = "test-hooks")]
const WAITERS: usize = 2;

/// How many runs of the sweep go at once: each has a child and a store of its own.
#[cfg(feature = "test-hooks")]
const AT_ONCE: u64 = 8;

/// The ref the sweep's `n`-th commit advances.
#[cfg(feature = "test-hooks")]
fn sweep_ref(n: usize) -> String {
    format!("release/{n}")
}

/// The commit the sweep's `n`-th writer makes on `t0`: a file of its own, keyed `key-<n>`, and
/// the advance of [`sweep_ref`]. The same request makes the same changeset.
#[cfg(feature = "test-hooks")]
fn sweep_commit(t0: ChangesetId, n: usize) -> CommitRequest {
    let mut req = advancing(&[t0], vec![put(&format!("f{n}"), b"f\n")], &sweep_ref(n));
    req.metadata = vec![(KEY.to_owned(), format!("key-{n}").into_bytes())];
    req
}

/// A child's part of the sweep, if this process is one: makes commit [`CHILD_N`] on `main`'s
/// head of the store at [`CHILD_ROOT`], opened with the hooks the environment sets, and reports
/// how it ended.
#[cfg(feature = "test-hooks")]
fn commit_as_child() -> bool {
    if std::env::var(CHILD_ROLE).is_err() {
        return false;
    }
    let root = Utf8PathBuf::from(std::env::var(CHILD_ROOT).unwrap());
    let n: usize = std::env::var(CHILD_N).unwrap().parse().unwrap();
    let made = LocalRepo::open(&root).and_then(|repo| {
        let t0 = target(&repo, "main");
        repo.commit(sweep_commit(t0, n))
    });
    match made {
        Ok(out) => println!("{REPORT}committed {}", out.id),
        Err(e) => println!("{REPORT}failed {e:?}"),
    }
    true
}

/// A child of the sweep, `role`, making commit `n` on the store at `root` with `hooks` set.
#[cfg(feature = "test-hooks")]
fn sweep_child(role: &str, root: &Utf8Path, n: usize, hooks: &[(&str, &str)]) -> Command {
    let mut command = this_test(&test_name!(
        "a_batch_of_commits_crashing_at_any_call_commits_each_once"
    ));
    command
        .env(CHILD_ROLE, role)
        .env(CHILD_ROOT, root)
        .env(CHILD_N, n.to_string());
    for (name, value) in hooks {
        command.env(name, value);
    }
    command
}

/// The sweep's template: a store whose `main` and every [`sweep_ref`] are `t0`, and each
/// waiter's request as a killed child queued it.
#[cfg(feature = "test-hooks")]
struct Seeded {
    t0: ChangesetId,
    /// Each waiter's request file: its name and bytes.
    requests: Vec<(String, Vec<u8>)>,
}

/// Builds the sweep's template: `t0`, its refs, and the request of each waiter's commit, which
/// a child queued behind the lock this test held and was killed in.
#[cfg(feature = "test-hooks")]
fn seeded_template() -> (TempDir, Utf8PathBuf, Seeded) {
    let (dir, root, repo) = new_store();
    let t0 = base(&repo);
    for n in 0..=WAITERS {
        repo.cas_ref(&sweep_ref(n), None, Some(t0), ACTOR).unwrap();
    }
    drop(repo);

    let mut requests = Vec::new();
    for n in 1..=WAITERS {
        let other = MetaStore::open(&root).unwrap();
        let taken = other.write().unwrap();
        let waiter = Child::spawn(&mut sweep_child("waiter", &root, n, &[]));
        let name = wait_requests(&root, 1).remove(0);
        drop(waiter);
        let path = layout_of(&root).queue_dir().join(&name);
        requests.push((name, fs::read(path.as_std_path()).unwrap()));
        fs::remove_file(path.as_std_path()).unwrap();
        drop(taken);
    }
    (dir, root, Seeded { t0, requests })
}

/// What one run of the sweep left: whether its leader finished, and the queue's files when it
/// ended.
#[cfg(feature = "test-hooks")]
struct Swept {
    finished: bool,
    left: Vec<String>,
}

/// One run of the sweep: on a copy of `template`, the seeded requests in the queue, each kept
/// alive by a flock this test holds, a child leads its commit and theirs until its k-th
/// filesystem call aborts it. Then the waiters are let die, and every commit is made again:
/// each ref names its own commit, keyed once, on `t0`, and the store verifies clean.
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
    let child = run(&mut sweep_child("leader", &root, 0, &hooks));
    let finished = child.status.success();
    assert!(
        finished || child.status.signal() == Some(SIGABRT),
        "{what}: {:?}\n{}",
        child.status,
        String::from_utf8_lossy(&child.stderr)
    );
    let left = queue_files(&root);

    // 3. The waiters die; every commit again, a replay of each if the leader finished.
    drop(alive);
    let repo = LocalRepo::open_with_fs(&root, checked(RealFs::new())).unwrap();
    assert_eq!(repo.verify(Depth::Deep).unwrap().problems, [], "{what}");
    for n in 0..=WAITERS {
        let again = repo.commit(sweep_commit(seeded.t0, n));
        let again = again.unwrap_or_else(|e| panic!("{what}: commit {n} again: {e:?}"));
        if finished {
            let replay = Advanced::new(&sweep_ref(n), Some(seeded.t0), true);
            assert_eq!(again.insert, InsertOutcome::AlreadyPresent, "{what}: {n}");
            assert_eq!(again.advanced, Some(replay), "{what}: {n}");
        }

        // 4. Its ref on its commit, keyed once, on `t0`.
        let at = target(&repo, &sweep_ref(n));
        assert_eq!(at, again.id, "{what}: {n}");
        let stored = repo.changeset(at).unwrap().expect("the commit");
        let keys: Vec<&Vec<u8>> = (stored.changeset.metadata.iter())
            .filter(|(name, _)| name == KEY)
            .map(|(_, value)| value)
            .collect();
        assert_eq!(keys, [&format!("key-{n}").into_bytes()], "{what}: {n}");
        assert_eq!(first_parent(&repo, at), Some(seeded.t0), "{what}: {n}");
    }
    assert_eq!(target(&repo, "main"), seeded.t0, "{what}");
    assert_eq!(repo.verify(Depth::Deep).unwrap().problems, [], "{what}");
    Swept { finished, left }
}

/// A batch of commits whose leader crashes at any filesystem call commits each once: for every
/// k, the leader's commit and the two seeded waiters' each end on their refs once after the
/// commits are made again, and the store verifies clean. Some crash leaves claimed requests
/// (`.com`), and some answers (`.ans`).
#[test]
#[cfg(feature = "test-hooks")]
fn a_batch_of_commits_crashing_at_any_call_commits_each_once() {
    if commit_as_child() {
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
        runs.len() > 10,
        "the leader finished after {} calls",
        runs.len()
    );
    assert!(
        crashed_with(".com", &runs),
        "no crash left a claimed request"
    );
    assert!(crashed_with(".ans", &runs), "no crash left an answer");
}
