//! Batches of frames (the M18 D2 design §3.3, §3.6): a write transaction holding several frames,
//! one per transaction it carries, each seeing what the frames before it queued, committed with
//! one write and one sync of `journal.log` and listed one 512 B sector of `journal.off` at a
//! time, each frame byte-identical to what its records make alone.
//!
//! A sector holds 64 FORMAT 1 entries (8 bytes) or 32 FORMAT 2 entries (16 bytes): a journal of
//! `per_sector - 2` frames puts a batch's first two entries in sector 0.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use swarmops_core::{ChangesetId, ObjectKind, hash_object};
use swarmops_store::{
    CasResult, ChangesetMeta, FsMethod, FsOps, InsertOutcome, Journal, Layout, MetaStore,
    MetaWrite, RealFs, Record, StoreError, StoreFormat,
};
use swarmops_testkit::faults::checked;
use swarmops_testkit::fs::{Call, NoSyncFs, RecordingFs, Watch, Watched};
use swarmops_testkit::power::{ImageRecordingFs, RecordedCall};
use swarmops_testkit::store::{file_len, new_root};
use tempfile::TempDir;

const ACTOR: &str = "test/1";

/// The size of a `journal2.off` entry.
const ENTRY_V2: u64 = 16;

/// The two formats, with the entries a 512 B sector of `journal.off` holds in each.
const FORMATS: [(StoreFormat, u64); 2] = [(StoreFormat::V1, 64), (StoreFormat::V2, 32)];

/// A fresh store root of `format`.
fn root_of(format: StoreFormat) -> (TempDir, Layout) {
    let (dir, layout) = new_root();
    (dir, Layout::with_format(layout.root(), format))
}

/// A fresh metadata store of `format` on the real filesystem, its writes order-checked.
fn store_of(format: StoreFormat) -> (TempDir, Layout, MetaStore) {
    let (dir, layout) = root_of(format);
    let mut store = MetaStore::create_in(layout.clone(), checked(RealFs::new())).unwrap();
    store.set_parallelism(1);
    (dir, layout, store)
}

/// A journal on `fs`, its tail checked: what a writer holds.
fn journal_on(layout: &Layout, fs: Arc<dyn FsOps>) -> Journal {
    let journal = Journal::create(layout, fs).unwrap();
    journal.repair_tail().unwrap();
    journal
}

fn compacted(sorted_count: u64) -> Record {
    Record::Compacted { sorted_count }
}

/// `n` frames of one `Compacted` record each, from `first` on.
fn compacted_frames(first: u64, n: u64) -> Vec<Vec<Record>> {
    (first..first + n).map(|at| vec![compacted(at)]).collect()
}

/// `frames` as [`Journal::append_frames`] takes them.
fn slices(frames: &[Vec<Record>]) -> Vec<&[Record]> {
    frames.iter().map(Vec::as_slice).collect()
}

/// The id of changeset `n`: a real hash, so the ids look like a store's.
fn id(n: u64) -> ChangesetId {
    ChangesetId::new(hash_object(ObjectKind::Changeset, &n.to_le_bytes()))
}

/// Changeset `n` with parents `parents`, as a caller queues it.
fn cs(n: u64, parents: &[u64]) -> ChangesetMeta {
    let delta = hash_object(ObjectKind::Blob, &n.to_le_bytes()).0;
    ChangesetMeta::new(
        id(n),
        0,
        0,
        delta,
        parents.iter().copied().map(id).collect(),
    )
}

/// Queues changeset `n` (a child of `n - 1`, a root for 0) and moves `main` to it, in the open
/// frame of `write`.
fn advance(write: &mut MetaWrite<'_>, n: u64) {
    let parents: Vec<u64> = n.checked_sub(1).into_iter().collect();
    let outcome = write.insert_changeset(cs(n, &parents)).unwrap();
    assert!(
        matches!(outcome, InsertOutcome::Inserted { .. }),
        "{n}: {outcome:?}"
    );
    let from = n.checked_sub(1).map(id);
    let moved = write.cas_ref("main", from, Some(id(n)), ACTOR).unwrap();
    assert_eq!(moved, CasResult::Applied { old: from }, "{n}");
}

/// Commits changesets `range` on `main`, a frame each, as one batch.
fn batch(store: &MetaStore, range: std::ops::Range<u64>) {
    let mut write = store.write().unwrap();
    for n in range {
        write.begin_frame().unwrap();
        advance(&mut write, n);
    }
    write.commit_frames().unwrap();
}

/// Whether the store at `layout` shows changeset `n`, in a snapshot of its own.
fn shows(layout: &Layout, n: u64) -> bool {
    let store = MetaStore::open_in(layout.clone(), Arc::new(NoSyncFs)).unwrap();
    let read = store.read().unwrap();
    read.changeset(id(n)).unwrap().is_some()
}

/// The file name of `path`.
fn name(path: &Path) -> String {
    path.file_name().unwrap().to_string_lossy().into_owned()
}

/// The `append_durable` calls `calls` made to `path`, by the bytes each appended.
fn appends_to(calls: &[RecordedCall], path: &Path) -> Vec<usize> {
    (calls.iter())
        .filter_map(|call| match call {
            RecordedCall::AppendDurable { path: to, bytes } if to == path => Some(bytes.len()),
            _ => None,
        })
        .collect()
}

/// A batch is appended with one `append_durable` of `journal.log`, its one sync the commit point
/// of every frame, then its entries with one more; and one frame makes the calls
/// [`Journal::append_frame`] makes.
#[test]
fn append_frames_syncs_the_log_once() {
    for (format, _) in FORMATS {
        let (_dir, layout) = root_of(format);
        let (fs, calls) = RecordingFs::new();
        let journal = journal_on(&layout, Arc::new(fs));
        calls();

        // 1. Three frames: one append of the log, one of the entries (all in sector 0).
        let frames = compacted_frames(0, 3);
        let appended = journal.append_frames(&slices(&frames)).unwrap();
        let (log, off) = (
            name(layout.journal_log().as_std_path()),
            name(layout.journal_off().as_std_path()),
        );
        assert_eq!(
            calls(),
            [
                format!("append_durable {log}"),
                format!("append_durable {off}")
            ],
            "{format:?}"
        );
        let seqs: Vec<u64> = appended.iter().map(|frame| frame.seq).collect();
        assert_eq!(seqs, [0, 1, 2], "{format:?}");
        assert_eq!(appended[0].offset, 0, "{format:?}");
        let ends_at = appended[2].offset + (appended[2].offset - appended[1].offset);
        assert_eq!(
            ends_at,
            journal.len(),
            "{format:?}: the frames lie end to end"
        );

        // 2. One frame of a batch: the calls of append_frame, in its order.
        journal.append_frames(&[&[compacted(3)]]).unwrap();
        let batched = calls();
        journal.append_frame(&[compacted(4)]).unwrap();
        assert_eq!(calls(), batched, "{format:?}");

        // 3. No frame at all is refused, before any call.
        let none = journal.append_frames(&[]);
        assert!(matches!(none, Err(StoreError::BadCall(_))), "{none:?}");
        assert!(calls().is_empty(), "{format:?}");
    }
}

/// A batch's entries are appended one 512 B sector of `journal.off` at a time: a batch that
/// starts two entries short of a sector's end and runs one past the next makes three appends.
#[test]
fn entries_are_synced_per_sector() {
    for (format, per_sector) in FORMATS {
        let (_dir, layout) = root_of(format);
        let fs = Arc::new(ImageRecordingFs::new());
        let journal = journal_on(&layout, Arc::clone(&fs) as Arc<dyn FsOps>);
        let before = compacted_frames(0, per_sector - 2);
        journal.append_frames(&slices(&before)).unwrap();
        let recorded = fs.calls().len();

        let frames = compacted_frames(per_sector - 2, 2 + per_sector + 1);
        journal.append_frames(&slices(&frames)).unwrap();
        let calls = &fs.calls()[recorded..];
        let entry = usize::try_from(512 / per_sector).unwrap();
        let sectors = usize::try_from(per_sector).unwrap();
        assert_eq!(
            appends_to(calls, layout.journal_off().as_std_path()),
            [2 * entry, sectors * entry, entry],
            "{format:?}"
        );
        assert_eq!(
            appends_to(calls, layout.journal_log().as_std_path()).len(),
            1,
            "{format:?}"
        );
        let listed = Journal::open(&layout, Arc::new(RealFs::new())).unwrap();
        assert_eq!(
            listed.frame_count().unwrap(),
            2 * per_sector + 1,
            "{format:?}"
        );
    }
}

/// Each frame of a batch is byte for byte the frame its records make appended alone at its seq,
/// and so is each entry: in FORMAT 2 each header names its place, chained from the frame before.
#[test]
fn batched_frames_match_frames_alone() {
    for (format, per_sector) in FORMATS {
        let frames: Vec<Vec<Record>> = (0..per_sector + 3)
            .map(|n| match n % 3 {
                0 => vec![compacted(n)],
                1 => vec![compacted(n), compacted(n * 7)],
                _ => vec![Record::Changeset {
                    id: id(n),
                    generation: 1,
                    delta_hash: [u8::try_from(n % 256).unwrap(); 32],
                    parents: Vec::new(),
                }],
            })
            .collect();

        // 1. Alone, a frame at a time.
        let (_alone_dir, alone) = root_of(format);
        let journal = journal_on(&alone, Arc::new(NoSyncFs));
        for records in &frames {
            journal.append_frame(records).unwrap();
        }

        // 2. In batches: one of three frames, then one across a sector's end.
        let (_batched_dir, batched) = root_of(format);
        let journal = journal_on(&batched, Arc::new(NoSyncFs));
        let all = slices(&frames);
        let returned = [
            journal.append_frames(&all[..3]).unwrap(),
            journal.append_frames(&all[3..]).unwrap(),
        ]
        .concat();

        for file in [Layout::journal_log, Layout::journal_off] {
            let (want, got) = (
                fs::read(file(&alone)).unwrap(),
                fs::read(file(&batched)).unwrap(),
            );
            assert!(want == got, "{format:?}: {} differs", file(&alone));
        }
        let read = Journal::open(&alone, Arc::new(RealFs::new())).unwrap();
        assert_eq!(read.tail_frames_from(0).unwrap(), returned, "{format:?}");
    }
}

/// A frame dropped forgets what it queued: its changesets are not queued, and each ref it moved
/// is left where the frames before it left it, not where it was before the transaction.
#[test]
fn drop_frame_forgets_inserts_and_moves() {
    let (_dir, layout, store) = store_of(StoreFormat::V2);
    let mut write = store.write().unwrap();
    advance(&mut write, 0);

    // 1. Frame 1 moves main from 0 to 1, then is dropped.
    write.begin_frame().unwrap();
    advance(&mut write, 1);
    write.drop_frame();
    assert_eq!(write.get_ref("main").unwrap(), Some(id(0)));
    assert_eq!(write.changeset(id(1)).unwrap(), None);
    assert!(
        write.queued().iter().all(|record| match record {
            Record::Changeset { id: queued, .. } => *queued == id(0),
            _ => true,
        }),
        "{:?}",
        write.queued()
    );

    // 2. Queued again, changeset 1 is new to the transaction, and main moves from 0.
    advance(&mut write, 1);
    let infos = write.commit_frames().unwrap();
    assert_eq!(infos.len(), 2, "{infos:?}");
    assert_eq!((infos[0].inserted, infos[1].inserted), (1, 1));
    let read = store.read().unwrap();
    assert_eq!(
        read.get_ref("main").unwrap().map(|state| state.target),
        Some(id(1))
    );
    assert!(shows(&layout, 0) && shows(&layout, 1), "both committed");
}

/// `keep_frames(n)` closes the open frame, then discards every frame past the first `n`.
#[test]
fn keep_frames_drops_the_rest() {
    let (_dir, layout, store) = store_of(StoreFormat::V2);
    let mut write = store.write().unwrap();
    for n in 0..3 {
        write.begin_frame().unwrap();
        advance(&mut write, n);
    }
    write.keep_frames(2).unwrap();
    assert_eq!(write.get_ref("main").unwrap(), Some(id(1)));
    assert_eq!(write.changeset(id(2)).unwrap(), None);
    let infos = write.commit_frames().unwrap();
    let seqs: Vec<u64> = infos.iter().map(|info| info.frame_seq).collect();
    assert_eq!(seqs, [0, 1]);
    assert!(shows(&layout, 1) && !shows(&layout, 2), "frame 2 discarded");

    // None kept: nothing is appended.
    let mut write = store.write().unwrap();
    write.begin_frame().unwrap();
    advance(&mut write, 2);
    write.keep_frames(0).unwrap();
    let infos = write.commit_frames().unwrap();
    assert_eq!(infos.len(), 1, "{infos:?}");
    assert_eq!((infos[0].frame_seq, infos[0].frame_crc), (2, None));
    assert!(!shows(&layout, 2), "frame 2 discarded again");
}

/// Each frame of a batch sees what the frames before it queued: a parent, a generation, a ref's
/// target to compare against.
#[test]
fn later_frames_see_earlier_queues() {
    let (_dir, _layout, store) = store_of(StoreFormat::V1);
    let mut write = store.write().unwrap();
    advance(&mut write, 0);
    write.begin_frame().unwrap();
    // `advance` inserts 1 as a child of 0, queued in frame 0, and moves main from 0.
    advance(&mut write, 1);
    assert_eq!(write.changeset(id(1)).unwrap().unwrap().generation, 2);
    assert_eq!(write.get_ref("main").unwrap(), Some(id(1)));
    let infos = write.commit_frames().unwrap();
    let seqs: Vec<u64> = infos.iter().map(|info| info.frame_seq).collect();
    assert_eq!(seqs, [0, 1]);
    assert_eq!((infos[0].moved_refs, infos[1].moved_refs), (1, 1));
    let read = store.read().unwrap();
    assert_eq!(read.changeset(id(1)).unwrap().unwrap().generation, 2);
    assert_eq!(
        read.get_ref("main").unwrap().map(|state| state.target),
        Some(id(1))
    );
}

/// [`MetaWrite::frames_from`] gives the journal's frames, then the batch's closed ones with the
/// seq, offset and payload CRC they take once committed; after `keep_frames`, the last planned
/// frame too (the frame a claim's manifest names).
#[test]
fn frames_from_shows_the_closed_frames_of_the_batch() {
    for (format, _) in FORMATS {
        let (_dir, layout, store) = store_of(format);
        batch(&store, 0..2);
        let mut write = store.write().unwrap();

        // 1. Frame 2 open: none of the batch's frames shows.
        write.begin_frame().unwrap();
        advance(&mut write, 2);
        let committed = write.frames_from(1).unwrap();
        assert_eq!(committed.len(), 1, "{format:?}: frame 1 only");

        // 2. Frame 2 closed, frame 3 open: frame 2 shows.
        write.begin_frame().unwrap();
        advance(&mut write, 3);
        let seqs: Vec<u64> = (write.frames_from(1).unwrap().iter())
            .map(|f| f.seq)
            .collect();
        assert_eq!(seqs, [1, 2], "{format:?}");

        // 3. Kept, the last planned frame shows too, as it will be committed.
        write.keep_frames(2).unwrap();
        let planned = write.frames_from(2).unwrap();
        assert_eq!(planned.len(), 2, "{format:?}: {planned:?}");
        write.commit_frames().unwrap();
        let journal = Journal::open(&layout, Arc::new(RealFs::new())).unwrap();
        assert_eq!(journal.frames_from(2, 10).unwrap(), planned, "{format:?}");
    }
}

/// A batch materializes once: each ref it moves has its file replaced once, with the target the
/// last move leaves, and its version grows by one.
#[test]
fn a_batch_replaces_each_ref_once() {
    let (_dir, layout) = root_of(StoreFormat::V1);
    let (fs, calls) = RecordingFs::new();
    let mut store = MetaStore::create_in(layout.clone(), Arc::new(fs)).unwrap();
    store.set_parallelism(1);
    calls();
    batch(&store, 0..3);
    let replaced: Vec<String> = (calls().into_iter())
        .filter(|call| call.starts_with("atomic_replace main"))
        .collect();
    assert_eq!(replaced.len(), 1, "{replaced:?}");
    let file = fs::read_to_string(layout.meta_path("refs/main")).unwrap();
    assert_eq!(file, format!("{}\n1\n", id(2)));
}

/// What fails an `append_durable` of a file once armed, after its write ran: a sync that fails,
/// what it wrote left behind. It fails the `nth` such call (from 1) after it is armed.
struct FailAfter {
    file: String,
    nth: u64,
    armed: AtomicBool,
    seen: AtomicU64,
}

impl FailAfter {
    fn new(file: &Path, nth: u64) -> Arc<FailAfter> {
        Arc::new(FailAfter {
            file: name(file),
            nth,
            armed: AtomicBool::new(false),
            seen: AtomicU64::new(0),
        })
    }

    fn arm(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }

    /// Whether it failed a call.
    fn failed(&self) -> bool {
        self.seen.load(Ordering::SeqCst) >= self.nth
    }
}

impl Watch for FailAfter {
    fn before(&self, _call: &Call<'_>) -> io::Result<()> {
        Ok(())
    }

    fn after_success(&self, call: &Call<'_>) -> io::Result<()> {
        let mine = call.method == FsMethod::AppendDurable && name(call.path()) == self.file;
        if !mine || !self.armed.load(Ordering::SeqCst) {
            return Ok(());
        }
        if self.seen.fetch_add(1, Ordering::SeqCst) + 1 == self.nth {
            return Err(io::Error::other("injected: the sync failed"));
        }
        Ok(())
    }
}

/// The journal file of a layout: `Layout::journal_log`, say.
type JournalFile = fn(&Layout) -> camino::Utf8PathBuf;

/// A store of `format` on the real filesystem, its writes order-checked, failing the `nth`
/// `append_durable` of `file` once armed; changeset 0 committed before.
fn store_with(
    format: StoreFormat,
    file: JournalFile,
    nth: u64,
) -> (TempDir, Layout, MetaStore, Arc<FailAfter>) {
    let (dir, layout) = root_of(format);
    let watch = FailAfter::new(file(&layout).as_std_path(), nth);
    let fs = Watched(Arc::clone(&watch), RealFs::new());
    let mut store = MetaStore::create_in(layout.clone(), checked(fs)).unwrap();
    store.set_parallelism(1);
    batch(&store, 0..1);
    (dir, layout, store, watch)
}

/// A failed sync of a batch's frames cuts every one of them back off `journal.log`: none
/// committed, and the next batch commits where they would have been.
#[test]
fn a_failed_log_sync_cuts_the_batch() {
    let (_dir, layout, store, watch) = store_with(StoreFormat::V2, Layout::journal_log, 1);
    let len = file_len(layout.journal_log());
    watch.arm();
    let mut write = store.write().unwrap();
    for n in 1..4 {
        write.begin_frame().unwrap();
        advance(&mut write, n);
    }
    let failed = write.commit_frames();
    assert!(
        matches!(failed, Err(StoreError::NotCommitted { .. })),
        "{failed:?}"
    );
    assert!(watch.failed(), "the injected failure was reached");
    assert_eq!(
        file_len(layout.journal_log()),
        len,
        "the batch is cut back off"
    );
    assert!(
        !shows(&layout, 1) && !shows(&layout, 3),
        "nothing of it committed"
    );

    let recovered = store.write().unwrap().recovered();
    assert_eq!(recovered.frames_listed, 0, "{recovered:?}");
    batch(&store, 1..4);
    assert!(shows(&layout, 3), "the next batch commits");
}

/// A failed append of a batch's entries, every frame durable, is `PostCommit` at the first
/// frame's seq; the next write lists the frames.
#[test]
fn a_failed_entry_append_is_post_commit() {
    let (_dir, layout, store, watch) = store_with(StoreFormat::V1, Layout::journal_off, 1);
    let off = file_len(layout.journal_off());
    watch.arm();
    let mut write = store.write().unwrap();
    for n in 1..4 {
        write.begin_frame().unwrap();
        advance(&mut write, n);
    }
    let failed = write.commit_frames();
    assert!(
        matches!(failed, Err(StoreError::PostCommit { frame_seq: 1, .. })),
        "{failed:?}"
    );
    assert_eq!(
        file_len(layout.journal_off()),
        off,
        "the entries are cut back off"
    );
    let recovered = store.write().unwrap().recovered();
    assert_eq!(recovered.frames_listed, 3, "{recovered:?}");
    assert!(
        shows(&layout, 1) && shows(&layout, 3),
        "every frame committed"
    );
}

/// What a reader's snapshot shows of a batch's changesets after each of the calls that append
/// its frames and entries (`journal.log`, then each sector of `journal.off`), with how many of
/// the batch's frames `journal.off` lists then.
struct Snapshots {
    layout: Layout,
    armed: AtomicBool,
    batch: std::ops::Range<u64>,
    seen: Mutex<Vec<Seen>>,
}

/// One snapshot of [`Snapshots`]: the call, the batch's frames listed, and which of its
/// changesets the snapshot shows.
type Seen = (String, u64, Vec<bool>);

impl Watch for Snapshots {
    fn before(&self, _call: &Call<'_>) -> io::Result<()> {
        Ok(())
    }

    fn after(&self, call: &Call<'_>) {
        let journal = [self.layout.journal_log(), self.layout.journal_off()];
        let appends = call.method == FsMethod::AppendDurable
            && journal.iter().any(|file| file.as_std_path() == call.path());
        if !appends || !self.armed.load(Ordering::SeqCst) {
            return;
        }
        let entries = file_len(self.layout.journal_off()) / ENTRY_V2;
        let listed = entries.saturating_sub(self.batch.start);
        let shown = self.batch.clone().map(|n| shows(&self.layout, n)).collect();
        self.seen
            .lock()
            .unwrap()
            .push((call.named(), listed, shown));
    }
}

/// A lock-free snapshot taken while a batch is being listed, a sector of entries at a time,
/// reads: it sees every frame listed so far, and, as a local reader does, the whole frames past
/// them that are not listed yet.
#[test]
fn a_snapshot_mid_batch_sees_every_listed_frame() {
    let (_dir, layout) = root_of(StoreFormat::V2);
    let watch = Arc::new(Snapshots {
        layout: layout.clone(),
        armed: AtomicBool::new(false),
        batch: 30..34,
        seen: Mutex::new(Vec::new()),
    });
    let fs = Watched(Arc::clone(&watch), RealFs::new());
    let mut store = MetaStore::create_in(layout.clone(), checked(fs)).unwrap();
    store.set_parallelism(1);
    batch(&store, 0..30);
    watch.armed.store(true, Ordering::SeqCst);
    batch(&store, 30..34);
    watch.armed.store(false, Ordering::SeqCst);

    // The frames written, none listed; two listed (sector 0's end); all four.
    let seen = watch.seen.lock().unwrap().clone();
    let want: Vec<Seen> = [
        ("journal2.log", 0),
        ("journal2.off", 2),
        ("journal2.off", 4),
    ]
    .into_iter()
    .map(|(file, listed)| (format!("append_durable {file}"), listed, vec![true; 4]))
    .collect();
    assert_eq!(seen, want);
}

#[cfg(feature = "test-hooks")]
mod hooks {
    //! What needs the test hooks: the order checker around a failed sector append, and the
    //! crash and power-loss sweeps of a batch.

    use std::ffi::OsString;
    use std::os::unix::process::ExitStatusExt;

    use camino::Utf8PathBuf;
    use swarmops_store::{CRASH_AT_ENV, CrashAtFs, OrderCheckFs};
    use swarmops_testkit::child::{SIGABRT, run, this_test};
    use swarmops_testkit::fs::copy_dir;
    use swarmops_testkit::power::{Keep, KeepAllBut, SECTOR, Seeded, Timeline};
    use swarmops_testkit::test_name;

    use super::*;

    /// Set only in the children of the crash sweep: the store root.
    const CRASH_ROOT: &str = "SWARMOPS_TEST_BATCH_CRASH_ROOT";

    /// The frames committed before the swept batch: two short of sector 0's end in FORMAT 2.
    const BEFORE: u64 = 30;

    /// The swept batch: changesets 30 to 33, a frame each, two entries in each of two sectors;
    /// with the 30 before, they take the checkpoint past 32 frames.
    const BATCH: std::ops::Range<u64> = 30..34;

    /// A failed append of a batch's second sector of entries cuts `journal.off` back to that
    /// append's first entry, never below the first sector's, which this writer synced: under the
    /// order checker, which refuses a cut below a synced entry (W1).
    #[test]
    fn a_failed_second_sector_append_keeps_the_first_listed() {
        let (_dir, layout) = root_of(StoreFormat::V2);
        let watch = FailAfter::new(layout.journal_off().as_std_path(), 2);
        let fs = Arc::new(OrderCheckFs::new(Watched(
            Arc::clone(&watch),
            RealFs::new(),
        )));
        let mut store = MetaStore::create_in(layout.clone(), fs).unwrap();
        store.set_parallelism(1);
        batch(&store, 0..BEFORE);
        watch.arm();
        let mut write = store.write().unwrap();
        for n in BATCH {
            write.begin_frame().unwrap();
            advance(&mut write, n);
        }
        let failed = write.commit_frames();
        assert!(
            matches!(
                failed,
                Err(StoreError::PostCommit {
                    frame_seq: BEFORE,
                    ..
                })
            ),
            "{failed:?}"
        );
        assert!(watch.failed(), "the second sector's append failed");
        assert_eq!(
            file_len(layout.journal_off()),
            32 * ENTRY_V2,
            "the first sector stays listed"
        );
        let recovered = store.write().unwrap().recovered();
        assert_eq!(recovered.frames_listed, 2, "{recovered:?}");
        assert!(shows(&layout, 33), "every frame committed");
    }

    /// A FORMAT 2 store of [`BEFORE`] changesets, a batch, on a filesystem that never syncs (a
    /// process crash keeps the page cache).
    fn crash_template() -> (TempDir, Layout) {
        let (dir, layout) = root_of(StoreFormat::V2);
        let mut store = MetaStore::create_in(layout.clone(), Arc::new(NoSyncFs)).unwrap();
        store.set_parallelism(1);
        batch(&store, 0..BEFORE);
        (dir, layout)
    }

    /// The swept batch, in a child process, through a `CrashAtFs` that aborts on its k-th call,
    /// under the order checker.
    fn crash_child(root: OsString) {
        let k = std::env::var(CRASH_AT_ENV).unwrap().parse().unwrap();
        let root = Utf8PathBuf::from_path_buf(root.into()).unwrap();
        let layout = Layout::with_format(&root, StoreFormat::V2);
        let mut store = MetaStore::open_in(layout, checked(CrashAtFs::new(NoSyncFs, k))).unwrap();
        store.set_parallelism(1);
        batch(&store, BATCH);
    }

    /// How many of the store's changesets, counted from 0, the store at `layout` recovers to,
    /// once a write repairs it; every one of them present, or why not.
    fn recovered(layout: &Layout) -> Result<u64, String> {
        let mut store = MetaStore::open_in(layout.clone(), checked(NoSyncFs))
            .map_err(|e| format!("open: {e}"))?;
        store.set_parallelism(1);
        (store.write().and_then(MetaWrite::commit)).map_err(|e| format!("recovery: {e}"))?;
        store.flush().map_err(|e| format!("flush: {e}"))?;
        let read = store.read().map_err(|e| format!("read: {e}"))?;
        let count = read.count_changesets().map_err(|e| format!("count: {e}"))?;
        for n in 0..count {
            if read.changeset(id(n)).map_err(|e| e.to_string())?.is_none() {
                return Err(format!("{count} changesets but {n} missing"));
            }
        }
        let main = read.get_ref("main").map_err(|e| e.to_string())?;
        if main.map(|state| state.target) != count.checked_sub(1).map(id) {
            return Err(format!("{count} changesets but main at {main:?}"));
        }
        drop(read);
        batch(&store, count..count + 1);
        Ok(count)
    }

    /// A crash at any call of a batch's commit leaves a store that recovers to a prefix of the
    /// batch's frames on top of what was there: here none or all, its frames written at once.
    #[test]
    fn a_batch_crashing_at_any_call_leaves_a_prefix() {
        if let Some(root) = std::env::var_os(CRASH_ROOT) {
            crash_child(root);
            return;
        }
        let test = "a_batch_crashing_at_any_call_leaves_a_prefix";
        let calls = {
            let (_dir, layout) = crash_template();
            let counter = Arc::new(CrashAtFs::new(NoSyncFs, 0));
            let mut store =
                MetaStore::open_in(layout, Arc::clone(&counter) as Arc<dyn FsOps>).unwrap();
            store.set_parallelism(1);
            batch(&store, BATCH);
            counter.calls()
        };
        assert!(calls > 10, "{calls} calls");
        let (_template_dir, template) = crash_template();
        let mut counts = Vec::new();
        for k in 1..=calls + 1 {
            let dir = tempfile::tempdir().unwrap();
            let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
            copy_dir(template.root().as_std_path(), root.as_std_path());
            let layout = Layout::with_format(&root, StoreFormat::V2);
            let child = run(this_test(&test_name!(test))
                .env(CRASH_ROOT, layout.root())
                .env(CRASH_AT_ENV, k.to_string()));
            let ended = if k <= calls {
                child.status.signal() == Some(SIGABRT)
            } else {
                child.status.success()
            };
            assert!(
                ended,
                "k={k}: {:?}\n{}",
                child.status,
                String::from_utf8_lossy(&child.stderr)
            );
            let count = recovered(&layout).unwrap_or_else(|why| panic!("k={k}: {why}"));
            assert!((BEFORE..=BATCH.end).contains(&count), "k={k}: {count}");
            counts.push(count);
        }
        // The sweep reached both sides of the commit point.
        assert!(counts.contains(&BEFORE), "{counts:?}");
        assert!(counts.contains(&BATCH.end), "{counts:?}");
    }

    /// What the power-loss sweep records: a FORMAT 2 store of [`BEFORE`] changesets, then the
    /// swept batch.
    struct Recorded {
        timeline: Timeline,
        /// The step from which the sweep cuts: the batch's start.
        from: usize,
        /// The first step of the batch's append of its frames.
        begun: usize,
        /// The step at which the batch's commit returned.
        returned: usize,
    }

    fn record() -> (TempDir, PathBuf, Recorded) {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let root = Utf8PathBuf::from_path_buf(base.join("store")).unwrap();
        let layout = Layout::with_format(&root, StoreFormat::V2);
        let fs = Arc::new(ImageRecordingFs::new());
        // The store root made durable first, as a repository's init makes it.
        fs.create_dir_all(root.as_std_path()).unwrap();
        fs.fsync_dir(&base).unwrap();
        let mut store =
            MetaStore::create_in(layout.clone(), Arc::clone(&fs) as Arc<dyn FsOps>).unwrap();
        store.set_parallelism(1);
        batch(&store, 0..BEFORE);
        fs.mark("before");
        batch(&store, BATCH);
        fs.mark("returned");

        let calls = fs.calls();
        let timeline = Timeline::new(&base, &calls);
        let from = timeline.mark("before");
        let log = layout.journal_log().into_std_path_buf();
        // The batch's one write of its frames: 4 of 219 bytes (a header of 24, a count of 2, a
        // changeset of 106 with its parent, a move of `main` of 87).
        let (at, written) = (calls.iter().enumerate())
            .filter(|&(at, _)| timeline.steps_of(at).start >= from)
            .find_map(|(at, call)| match call {
                RecordedCall::AppendDurable { path, bytes } if *path == log => Some((at, bytes)),
                _ => None,
            })
            .unwrap();
        assert_eq!(written.len(), 4 * 219);
        let begun = timeline.steps_of(at).start;
        let returned = timeline.mark("returned");
        let recorded = Recorded {
            timeline,
            from,
            begun,
            returned,
        };
        (dir, base, recorded)
    }

    /// The images of a power failure after every step of a batch's commit, for the seeds 0 (the
    /// least the disk can hold), 1 (the most) and 2 to 5 (random): each recovers to a prefix of
    /// the batch on top of what was before it, holding every frame once the commit returned.
    #[test]
    fn power_loss_in_a_batch_keeps_a_prefix() {
        let (_dir, _base, rec) = record();
        let mut failures = Vec::new();
        let mut counts = Vec::new();
        // 1. Every cut, every seed.
        for cut in rec.from..=rec.timeline.steps() {
            for seed in 0..6 {
                let image = rec.timeline.image(cut, &mut Seeded::new(seed, cut));
                let dir = tempfile::tempdir().unwrap();
                image.materialize(dir.path()).unwrap();
                let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
                let layout = Layout::with_format(&root, StoreFormat::V2);
                let acked = if cut >= rec.returned {
                    BATCH.end
                } else {
                    BEFORE
                };
                let begun = if cut > rec.begun { BATCH.end } else { BEFORE };
                match recovered(&layout) {
                    Ok(count) if (acked..=begun).contains(&count) => counts.push(count),
                    Ok(count) => failures.push(format!(
                        "cut {cut}, seed {seed}: {count} changesets, {acked} acknowledged, \
                         {begun} begun"
                    )),
                    Err(why) => failures.push(format!("cut {cut}, seed {seed}: {why}")),
                }
            }
        }
        assert!(
            failures.is_empty(),
            "{} images failed; the first:\n{}",
            failures.len(),
            failures[..failures.len().min(5)].join("\n")
        );
        // 2. A torn batch, deterministically: of the frames' write (876 bytes, 219 a frame),
        // only what lies before its first or its second 512 B boundary reached the disk, every
        // other change did. One of the two falls past a whole frame and before the last.
        let log = Path::new("store/meta/journal2.log");
        for pick in 0..2 {
            let torn = |file: &Path| {
                (file == log).then_some(Keep::Prefix {
                    granule: SECTOR,
                    pick,
                })
            };
            let image = rec.timeline.image(rec.begun + 1, &mut KeepAllBut(torn));
            let dir = tempfile::tempdir().unwrap();
            image.materialize(dir.path()).unwrap();
            let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
            match recovered(&Layout::with_format(&root, StoreFormat::V2)) {
                Ok(count) => counts.push(count),
                Err(why) => panic!("the torn batch, boundary {pick}: {why}"),
            }
        }

        // The sweep reached a torn batch (some of its frames whole), and the whole one.
        assert!(
            counts.iter().any(|&n| BEFORE < n && n < BATCH.end),
            "{counts:?}"
        );
        assert!(counts.contains(&BATCH.end), "{counts:?}");
    }
}
