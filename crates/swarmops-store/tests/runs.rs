//! FORMAT 3's sorted views (the FORMAT 3 design §2.4, §2.5 and its 2026-11-11 amendment, G6,
//! G12, G13): immutable runs under `meta/sorted3/MANIFEST`, built and merged outside the writer
//! lock. A commit leaves its tail to the caller's `MetaStore::step_views` until the tail passes
//! `RUN_BACKLOG_MAX`; then it steps once, after it unlocked.
//!
//! The stores are made with `MetaStore::create_in` of a FORMAT 3 layout: no `init` makes one yet.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, ThreadId};
use std::time::{Duration, Instant};

use swarmops_core::{ChangesetId, ObjectKind, hash_object};
use swarmops_store::{
    ChangesetMeta, FrameUse, FsOps, LEAD_WAIT_ENV, Layout, MAX_RUNS, MetaLock, MetaStore, Planned,
    RUN_BACKLOG_MAX, RUN_FANOUT, RUN_MIN, RealFs, Serve, Step, StoreError, StoreFormat, Turn,
};
use swarmops_testkit::child::{run, this_test};
use swarmops_testkit::fs::{Call, Watch, Watched};
use swarmops_testkit::store::{new_root, real_fs};
use swarmops_testkit::test_name;
use tempfile::TempDir;

/// The manifest's file name in `meta/sorted3/`.
const MANIFEST: &str = "MANIFEST";

/// How many writers one batch of the write queue takes at most.
const BATCH: u64 = 32;

/// Set only in the child that `v3_writes_hold_no_view_work` spawns for its batch, which leads
/// with the lead-wait hook: one lead gathers all 32 requests, whatever the host's load.
const BATCH_CHILD: &str = "SWARMOPS_TEST_RUNS_BATCH_CHILD";

/// Changeset `n`'s id.
fn id(n: u64) -> ChangesetId {
    ChangesetId::new(hash_object(ObjectKind::Changeset, &n.to_le_bytes()))
}

/// Changeset `n`, a root, as a caller hands it to `insert_changesets_batch`.
fn changeset(n: u64) -> ChangesetMeta {
    let delta_hash = hash_object(ObjectKind::Blob, &(n % 97).to_le_bytes()).0;
    ChangesetMeta::new(id(n), 0, 0, delta_hash, Vec::new())
}

/// A fresh FORMAT 3 store on `fs`, syncing its index from one thread.
fn store_on(fs: Arc<dyn FsOps>) -> (TempDir, Layout, MetaStore) {
    let (dir, layout) = new_root();
    let layout = Layout::with_format(layout.root(), StoreFormat::V3);
    let mut store = MetaStore::create_in(layout.clone(), fs).unwrap();
    store.set_parallelism(1);
    (dir, layout, store)
}

/// A fresh FORMAT 3 store on the real filesystem.
fn new_store() -> (TempDir, Layout, MetaStore) {
    store_on(real_fs())
}

/// Commits changesets `range` as one frame.
fn insert(store: &MetaStore, range: std::ops::Range<u64>) {
    let mut write = store.write().unwrap();
    write
        .insert_changesets_batch(range.map(changeset).collect())
        .unwrap();
    write.commit().unwrap();
}

/// Commits changesets `0..n` in frames of `per_frame`.
fn insert_in_frames(store: &MetaStore, n: u64, per_frame: u64) {
    let mut at = 0;
    while at < n {
        let end = (at + per_frame).min(n);
        insert(store, at..end);
        at = end;
    }
}

/// A run as the manifest names it: (domain, from, to).
type RunEntry = (u8, u64, u64);

/// The manifest's generation and runs, read from its bytes by the design's table (§2.5).
fn manifest(layout: &Layout) -> (u64, Vec<RunEntry>) {
    let bytes = fs::read(layout.views_dir().join(MANIFEST)).unwrap();
    assert_eq!(&bytes[..8], b"SWMF\x01\0\0\0", "magic, version, zeros");
    let crc = u32::from_le_bytes(bytes[bytes.len() - 4..].try_into().unwrap());
    assert_eq!(crc, crc32fast::hash(&bytes[..bytes.len() - 4]), "the CRC");
    let generation = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
    let count = u32::from_le_bytes(bytes[16..20].try_into().unwrap());
    let count = usize::try_from(count).unwrap();
    assert_eq!(bytes.len(), 20 + 24 * count + 4, "the length");
    let runs = (bytes[20..20 + 24 * count].chunks(24))
        .map(|entry| {
            let from = u64::from_le_bytes(entry[8..16].try_into().unwrap());
            let to = u64::from_le_bytes(entry[16..24].try_into().unwrap());
            (entry[0], from, to)
        })
        .collect();
    (generation, runs)
}

/// The manifest's changeset runs (domain 0).
fn changeset_runs(layout: &Layout) -> Vec<(u64, u64)> {
    let (_, runs) = manifest(layout);
    (runs.into_iter())
        .filter(|&(domain, _, _)| domain == 0)
        .map(|(_, from, to)| (from, to))
        .collect()
}

/// The `.run` files in `meta/sorted3/`, by name.
fn run_files(layout: &Layout) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(layout.views_dir())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| Path::new(name).extension().is_some_and(|ext| ext == "run"))
        .collect();
    names.sort();
    names
}

/// The file name of the changeset run `[from, to)`.
fn run_name(from: u64, to: u64) -> String {
    format!("c-{from:016x}-{to:016x}.run")
}

/// The index records `journal3.ckpt` has passed (bytes 24..32, the design §2.3).
fn ckpt_records(layout: &Layout) -> u64 {
    let bytes = fs::read(layout.journal_ckpt()).unwrap();
    u64::from_le_bytes(bytes[24..32].try_into().unwrap())
}

/// Asserts that the changeset runs split `[0, covered)` with no gap or overlap, that `covered`
/// is within what the checkpoint passed (M1, M2), and returns `covered`.
fn assert_partition(layout: &Layout) -> u64 {
    let mut covered = 0;
    for (from, to) in changeset_runs(layout) {
        assert_eq!(from, covered, "a gap or an overlap before [{from}, {to})");
        assert!(to > from, "an empty run [{from}, {to})");
        covered = to;
    }
    assert!(covered <= ckpt_records(layout), "past the checkpoint");
    covered
}

/// Asserts that a snapshot finds changesets `ids` at their recs.
fn assert_finds(store: &MetaStore, ids: impl IntoIterator<Item = u64>) {
    let read = store.read().unwrap();
    for n in ids {
        let found = read.changeset(id(n)).unwrap();
        assert_eq!(found.map(|meta| meta.seq), Some(n), "changeset {n}");
    }
}

/// Whether `path` lies under a store's `meta/sorted3/`.
fn in_views(path: &Path) -> bool {
    path.components().any(|part| part.as_os_str() == "sorted3")
}

/// Records each call that names a path under `meta/sorted3/`, in order among all calls, with
/// whether `meta/lock` was held when it was made: whether a lock of its own, taken without
/// waiting, gives up.
struct LockProbe {
    layout: Layout,
    /// Every call, in order: the thread that made it, whether it names a view path, and
    /// whether the lock was held.
    calls: Mutex<Vec<(ThreadId, bool, bool)>>,
}

impl LockProbe {
    fn new(layout: &Layout) -> Arc<Watched<LockProbe>> {
        let probe = LockProbe {
            layout: layout.clone(),
            calls: Mutex::new(Vec::new()),
        };
        Arc::new(Watched(probe, RealFs::new()))
    }

    /// The calls recorded so far, emptied: whether each names a view path, and whether the
    /// lock was held.
    fn take(&self) -> Vec<(bool, bool)> {
        let calls = self.take_by_thread();
        calls
            .into_iter()
            .map(|(_, views, held)| (views, held))
            .collect()
    }

    /// [`LockProbe::take`], with the thread that made each call.
    fn take_by_thread(&self) -> Vec<(ThreadId, bool, bool)> {
        std::mem::take(&mut *self.calls.lock().unwrap())
    }
}

impl Watch for LockProbe {
    fn before(&self, call: &Call<'_>) -> io::Result<()> {
        let attempt = MetaLock::acquire(&self.layout, Duration::ZERO);
        let held = matches!(attempt, Err(StoreError::Busy));
        drop(attempt);
        let views = call.paths.iter().any(|path| in_views(path));
        let thread = thread::current().id();
        self.calls.lock().unwrap().push((thread, views, held));
        Ok(())
    }
}

/// A store whose calls `probe` records: the store made on the real filesystem, then opened on
/// the probe.
fn probed() -> (TempDir, Layout, MetaStore, Arc<Watched<LockProbe>>) {
    let (dir, layout, _) = new_store();
    let probe = LockProbe::new(&layout);
    let mut store = MetaStore::open_in(layout.clone(), probe.clone()).unwrap();
    store.set_parallelism(1);
    (dir, layout, store, probe)
}

/// Asserts that `calls` made view work, all of it with `meta/lock` free, and after the last
/// call made with it held.
fn assert_stepped_after_unlock(calls: &[(bool, bool)], what: &str) {
    let first_view = (calls.iter()).position(|&(views, _)| views);
    let first_view = first_view.unwrap_or_else(|| panic!("{what}: no view work"));
    let last_held = calls.iter().rposition(|&(_, held)| held);
    let last_held = last_held.unwrap_or_else(|| panic!("{what}: never locked"));
    assert!(
        last_held < first_view,
        "{what}: view work before the unlock"
    );
    let locked: Vec<_> = (calls.iter())
        .filter(|&&(views, held)| views && held)
        .collect();
    assert!(locked.is_empty(), "{what}: view work under the lock");
}

/// Leads the write queue's turn of `store` with request `n`: plans every request it may, each
/// inserting the changeset its body names in a frame of its own, and returns how many members
/// the batch held. A waiter that a lead answered returns 0.
fn take_turn(store: &MetaStore, n: u64) -> usize {
    let Turn::Lead(mut lead) = store.take_turn(&n.to_le_bytes(), Serve::Queue).unwrap() else {
        return 0;
    };
    let requests = lead.requests().to_vec();
    let mut plans = Vec::new();
    for (frame, request) in requests.iter().enumerate() {
        if frame > 0 && !lead.may_plan() {
            break;
        }
        let body: [u8; 8] = request.body.as_slice().try_into().unwrap();
        let write = lead.write();
        write.begin_frame().unwrap();
        write
            .insert_changeset(changeset(u64::from_le_bytes(body)))
            .unwrap();
        plans.push(Planned::new(
            request.rid.clone(),
            FrameUse::Frame(frame),
            Vec::new(),
        ));
    }
    let members = lead.claim(plans).unwrap();
    lead.commit().unwrap();
    members
}

/// Runs changesets `first..first + 32` through the write queue at once: 32 writers wait while
/// the lock is held, then one leads them as a batch. Returns the largest batch led.
fn batch_of_32(store: &MetaStore, layout: &Layout, first: u64) -> usize {
    let held = store.write().unwrap();
    let largest = AtomicUsize::new(0);
    thread::scope(|scope| {
        for n in first..first + BATCH {
            let largest = &largest;
            scope.spawn(move || largest.fetch_max(take_turn(store, n), Ordering::Relaxed));
        }

        // 1. Every writer waits, its request in the queue, before the lock comes free.
        let deadline = Instant::now() + Duration::from_secs(30);
        let queued = || {
            let names = fs::read_dir(layout.queue_dir()).into_iter().flatten();
            let names = names.filter_map(Result::ok);
            names
                .filter(|entry| entry.file_name().to_string_lossy().ends_with(".req"))
                .count()
        };
        while queued() < usize::try_from(BATCH).unwrap() {
            assert!(Instant::now() < deadline, "the writers never queued");
            thread::sleep(Duration::from_millis(5));
        }
        drop(held);
    });
    largest.into_inner()
}

/// No call a FORMAT 3 write makes while it holds `meta/lock` names a path under
/// `meta/sorted3/`: not a commit's, not a 32-member batch's lead, not a settle that passes
/// `RUN_MIN` (the FORMAT 3 design §0 3; G18 and A1). The view work they leave runs after the
/// unlock, once the backlog passes `RUN_BACKLOG_MAX`.
#[test]
fn v3_writes_hold_no_view_work() {
    // 0. The batch, in a child that leads with the lead-wait hook (step 2).
    if std::env::var_os(BATCH_CHILD).is_some() {
        batch_holds_no_view_work();
        return;
    }

    // 1. Plain commits of 2,000 changesets each: each settle passes RUN_MIN and checkpoints,
    //    the fifth leaves a backlog of 10,000 and steps.
    let (_dir, layout, store, probe) = probed();
    insert_in_frames(&store, 10_000, 2_000);
    let calls = probe.take();
    assert_eq!(ckpt_records(&layout), 10_000, "every settle checkpointed");
    assert_stepped_after_unlock(&calls, "plain commits");
    assert_eq!(changeset_runs(&layout), [(0, 10_000)]);

    // 2. A 32-member batch past a backlog of exactly RUN_BACKLOG_MAX.
    let lead_wait = (BATCH - 1).to_string();
    let ran = run(this_test(&test_name!("v3_writes_hold_no_view_work"))
        .env(BATCH_CHILD, "1")
        .env(LEAD_WAIT_ENV, &lead_wait));
    let stderr = String::from_utf8_lossy(&ran.stderr);
    assert!(ran.status.success(), "{:?}\n{stderr}", ran.status);
}

/// [`v3_writes_hold_no_view_work`]'s batch: 32 writers led as one batch past a backlog of
/// exactly `RUN_BACKLOG_MAX`; the lead steps once its commit's calls are done, and none of its
/// calls under the lock names a path under `meta/sorted3/`.
///
/// The lock probe tells a hold, not its holder: once the lead lets the lock go, the members'
/// waits take it in turn to find their answers, during the lead's step. So the lead's own
/// calls are checked in order instead: every call of its commit (each made under its lock,
/// which the probe sees) before the first of its step. The uncontended lead of
/// `a_backlog_past_the_bound_steps_after_unlock` checks the unlock itself.
fn batch_holds_no_view_work() {
    let (_dir, layout, store, probe) = probed();
    insert(&store, 0..RUN_BACKLOG_MAX);
    assert_eq!(changeset_runs(&layout), [], "no step at the bound");
    probe.take();
    let largest = batch_of_32(&store, &layout, RUN_BACKLOG_MAX);
    let calls = probe.take_by_thread();
    #[cfg(feature = "test-hooks")]
    assert_eq!(largest, usize::try_from(BATCH).unwrap(), "one lead led all");
    #[cfg(not(feature = "test-hooks"))]
    assert!(largest > 0, "a lead led");

    // 1. One thread stepped: the lead. Its commit's calls came first, under its lock.
    let steppers: HashSet<ThreadId> = (calls.iter())
        .filter(|&&(_, views, _)| views)
        .map(|&(thread, _, _)| thread)
        .collect();
    assert_eq!(steppers.len(), 1, "one thread stepped");
    let lead = steppers.into_iter().next().unwrap();
    let own: Vec<(bool, bool)> = (calls.iter())
        .filter(|&&(thread, _, _)| thread == lead)
        .map(|&(_, views, held)| (views, held))
        .collect();
    let first_view = own.iter().position(|&(views, _)| views).unwrap();
    let last_commit = own.iter().rposition(|&(views, _)| !views).unwrap();
    assert!(
        last_commit < first_view,
        "the lead stepped before its commit was done"
    );
    assert!(
        own[..first_view].iter().all(|&(_, held)| held),
        "a commit call unlocked"
    );
    // Its 32 frames past the checkpoint made the settle checkpoint: the run covers them too.
    assert_eq!(changeset_runs(&layout), [(0, RUN_BACKLOG_MAX + BATCH)]);
    for n in RUN_BACKLOG_MAX..RUN_BACKLOG_MAX + BATCH {
        assert!(store.read().unwrap().changeset(id(n)).unwrap().is_some());
    }
}

/// A commit whose tail stays at or below `RUN_BACKLOG_MAX` past the runs makes no call under
/// `meta/sorted3/`: the view work is the caller's (`MetaStore::step_views`) or gc's (G6, G12).
#[test]
fn a_commit_below_the_backlog_runs_no_step() {
    let (_dir, layout, store, probe) = probed();
    insert_in_frames(&store, RUN_BACKLOG_MAX, 2_048);
    let calls = probe.take();
    let views = calls.iter().filter(|&&(views, _)| views).count();
    assert_eq!(views, 0, "no view work below the backlog bound");
    assert_eq!(manifest(&layout), (0, Vec::new()));
    assert_eq!(
        ckpt_records(&layout),
        RUN_BACKLOG_MAX,
        "checkpointed all the same"
    );
}

/// Past `RUN_BACKLOG_MAX` a commit steps once, after it unlocked: a plain write, and a write
/// queue's lead (G12).
#[test]
fn a_backlog_past_the_bound_steps_after_unlock() {
    // 1. A plain write.
    let (_dir, layout, store, probe) = probed();
    insert(&store, 0..RUN_BACKLOG_MAX + 1);
    assert_stepped_after_unlock(&probe.take(), "a plain write");
    assert_eq!(changeset_runs(&layout), [(0, RUN_BACKLOG_MAX + 1)]);

    // 2. A lead, uncontended: its own request alone.
    let (_dir, layout, store, probe) = probed();
    insert(&store, 0..RUN_BACKLOG_MAX);
    probe.take();
    assert_eq!(take_turn(&store, RUN_BACKLOG_MAX), 1);
    assert_stepped_after_unlock(&probe.take(), "a lead");
    // Its one record is past the checkpoint: the run covers what the checkpoint passed.
    assert_eq!(changeset_runs(&layout), [(0, RUN_BACKLOG_MAX)]);
    assert_finds(&store, [0, RUN_BACKLOG_MAX]);
}

/// `MetaStore::step_views` builds the runs the commits left: the records the checkpoint passed,
/// not those past it (G13).
#[test]
fn step_views_builds_what_commits_left() {
    let (_dir, layout, store) = new_store();
    insert_in_frames(&store, 3_000, 1_500);
    assert_eq!(
        manifest(&layout),
        (0, Vec::new()),
        "no step below the bound"
    );

    assert_eq!(store.step_views().unwrap(), Step::Done);
    assert_eq!(manifest(&layout), (1, vec![(0, 0, 3_000)]));
    assert_eq!(run_files(&layout), [run_name(0, 3_000)]);
    assert_finds(&store, [0, 1_499, 2_999]);

    // Nothing more to build: nothing written.
    assert_eq!(store.step_views().unwrap(), Step::Done);
    assert_eq!(manifest(&layout).0, 1);
}

/// The runs split exactly what the checkpoint passed (M1, M2): records past it stay in the
/// tail, and a step leaves them there.
#[test]
fn runs_partition_what_the_checkpoint_passed() {
    let (_dir, layout, store) = new_store();
    insert(&store, 0..3_000);
    // A frame of 10 neither passes RUN_MIN nor makes 32 frames: no checkpoint.
    insert(&store, 3_000..3_010);
    assert_eq!(ckpt_records(&layout), 3_000);
    store.step_views().unwrap();
    assert_eq!(assert_partition(&layout), 3_000);
    assert_finds(&store, [0, 2_999, 3_000, 3_009]);

    // Past RUN_MIN the next settle checkpoints, and the next step builds the rest.
    insert(&store, 3_010..4_100);
    store.step_views().unwrap();
    assert_eq!(assert_partition(&layout), 4_100);
    assert_finds(&store, [0, 3_005, 4_099]);
}

/// A run is a pure function of its records: built again after its removal, it has the same
/// name and the same bytes (the design §2.4).
#[test]
fn a_run_is_rebuilt_byte_identical() {
    let (_dir, layout, store) = new_store();
    insert(&store, 0..2_000);
    store.step_views().unwrap();
    let name = run_name(0, 2_000);
    let path = layout.views_dir().join(&name);
    let built = fs::read(&path).unwrap();
    assert_eq!(built.len(), 48 + 40 * 2 * 2_000, "the design's length");

    // The run removed and the manifest emptied: the next step builds the same bytes.
    fs::remove_file(&path).unwrap();
    fs::write(layout.views_dir().join(MANIFEST), empty_manifest(1)).unwrap();
    store.step_views().unwrap();
    assert_eq!(fs::read(&path).unwrap(), built);

    // So does another store of the same records.
    let (_dir, other, store) = new_store();
    insert(&store, 0..2_000);
    store.step_views().unwrap();
    assert_eq!(fs::read(other.views_dir().join(&name)).unwrap(), built);
}

/// The manifest of no run at `generation`, by the design's table.
fn empty_manifest(generation: u64) -> Vec<u8> {
    let mut bytes = b"SWMF\x01\0\0\0".to_vec();
    bytes.extend_from_slice(&generation.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    let crc = crc32fast::hash(&bytes);
    bytes.extend_from_slice(&crc.to_le_bytes());
    bytes
}

/// ⌈log₄(n / 1,024)⌉ + 1: the most runs a domain of n records keeps (the design §2.5 rule 5).
fn log_bound(n: u64) -> usize {
    let mut runs = 1;
    let mut reach = RUN_MIN;
    while reach < n {
        reach = reach.saturating_mul(RUN_FANOUT);
        runs += 1;
    }
    runs
}

/// Merging at `RUN_FANOUT` keeps every domain within ⌈log₄(n / 1,024)⌉ + 1 runs after every
/// step, n up to 300,000 (the design §2.5 rule 5), and within `MAX_RUNS`.
#[test]
#[ignore = "slow, ~35 s: 300,000 changesets in 114 commits, each stepped"]
fn merges_keep_runs_under_the_log_bound() {
    assert_log_bound_to(300_000);
}

/// [`merges_keep_runs_under_the_log_bound`] up to 40,000 records, for the regular suite.
#[test]
fn runs_stay_logarithmic() {
    assert_log_bound_to(40_000);
}

/// Commits chunks of uneven sizes past `RUN_MIN`, each checkpointed and stepped, until `limit`
/// records, and checks after every step that the runs split what the checkpoint passed in at
/// most ⌈log₄(n / 1,024)⌉ + 1 runs; then that lookups find the first, middle and last.
fn assert_log_bound_to(limit: u64) {
    let (_dir, layout, store) = new_store();
    let mut n = 0;
    for (step, chunk) in (0..).zip([1_100, 3_000, 1_500, 7_000, 1_025, 2_200].iter().cycle()) {
        if n >= limit {
            break;
        }
        insert(&store, n..n + chunk);
        n += chunk;
        store.step_views().unwrap();
        let covered = assert_partition(&layout);
        assert_eq!(covered, n, "step {step}: every checkpointed record covered");
        let runs = changeset_runs(&layout).len();
        assert!(runs <= log_bound(n), "step {step}: {runs} runs for {n}");
        assert!(runs <= MAX_RUNS);
    }
    assert_eq!(run_files(&layout).len(), changeset_runs(&layout).len());
    assert_finds(&store, [0, 1_099, n / 2, n - 1]);
}

/// A reader that races merges reads right: a run a merge removes under it sends it to the
/// manifest again (the design §2.5 M3), never to `IndexCorrupt` or a wrong answer.
#[test]
fn a_reader_racing_a_merge_retries_then_reads_right() {
    let (_dir, layout, store) = new_store();
    insert(&store, 0..1_100);
    store.step_views().unwrap();
    let done = AtomicBool::new(false);
    thread::scope(|scope| {
        // 1. The writer: chunks just past RUN_MIN, each stepped, so most steps merge.
        scope.spawn(|| {
            let mut n = 1_100;
            for _ in 0..40 {
                insert(&store, n..n + 1_100);
                n += 1_100;
                store.step_views().unwrap();
            }
            done.store(true, Ordering::Relaxed);
        });

        // 2. The reader, all the while.
        let reader = MetaStore::open_in(layout.clone(), real_fs()).unwrap();
        let mut reads = 0;
        while !done.load(Ordering::Relaxed) || reads == 0 {
            match reader.read() {
                Ok(read) => {
                    for n in [0, 555, 1_099] {
                        let found = read.changeset(id(n)).unwrap();
                        assert_eq!(found.map(|meta| meta.seq), Some(n));
                    }
                }
                Err(StoreError::SnapshotContended { .. }) => {}
                Err(other) => panic!("a racing read: {other}"),
            }
            reads += 1;
        }
    });
    assert!(changeset_runs(&layout).len() < 40, "the steps merged");
}

/// A step that finds `meta/views.lock` held does nothing and returns at once
/// (`Step::Skipped`); the next one builds (the design §2.5 rule 3).
#[test]
fn views_step_skips_when_the_lease_is_held() {
    let (_dir, layout, store) = new_store();
    insert(&store, 0..2_000);
    let lease = fs::File::create(layout.views_lock()).unwrap();
    lease.try_lock().unwrap();
    let before = fs::read(layout.views_dir().join(MANIFEST)).unwrap();

    assert_eq!(store.step_views().unwrap(), Step::Skipped);
    assert_eq!(fs::read(layout.views_dir().join(MANIFEST)).unwrap(), before);
    assert_eq!(run_files(&layout), Vec::<String>::new());

    drop(lease);
    assert_eq!(store.step_views().unwrap(), Step::Done);
    assert_eq!(changeset_runs(&layout), [(0, 2_000)]);
}

/// A manifest that names a run which is not there, read again unchanged, is damage:
/// `IndexCorrupt` (the design §2.5 M3).
#[test]
fn a_manifest_naming_a_missing_run_is_corrupt() {
    let (_dir, layout, store) = new_store();
    insert(&store, 0..2_000);
    store.step_views().unwrap();
    fs::remove_file(layout.views_dir().join(run_name(0, 2_000))).unwrap();
    let got = store.read().map(drop);
    assert!(
        matches!(got, Err(StoreError::IndexCorrupt { .. })),
        "{got:?}"
    );
}

/// The self-check of the reader's M3 check: a fake merge that removes a run the manifest still
/// names, before its new manifest, is caught by the reader's second read of the manifest.
#[test]
fn a_fake_merge_deleting_a_live_run_is_caught() {
    let (_dir, layout, store) = new_store();
    // Runs of 5,000 and 1,100: 5,000 is not below 4 × 1,100, so they stay two.
    insert(&store, 0..5_000);
    store.step_views().unwrap();
    insert(&store, 5_000..6_100);
    store.step_views().unwrap();
    assert_eq!(changeset_runs(&layout), [(0, 5_000), (5_000, 6_100)]);
    assert_finds(&store, [0, 5_500]);

    // The fake merge: the newer run removed first, the manifest never replaced.
    fs::remove_file(layout.views_dir().join(run_name(5_000, 6_100))).unwrap();
    let got = store.read().map(drop);
    assert!(
        matches!(got, Err(StoreError::IndexCorrupt { .. })),
        "{got:?}"
    );
}
