//! The metadata store's facade (storage spec §3): [`MetaStore`] opens a store's `meta/`,
//! [`MetaStore::read`] takes a lock-free snapshot ([`MetaRead`]) and [`MetaStore::write`]
//! starts a write transaction ([`MetaWrite`]).
//!
//! # A write transaction
//!
//! [`MetaStore::write`] takes [`MetaLock`], cuts a torn journal tail
//! ([`Journal::truncate_torn_tail`]) and a torn index tail ([`Index::repair_tail`]), and
//! replays the frames past `journal.ckpt` into the index and the ref files, so that both
//! show every committed frame. [`MetaWrite`] then queues records, checking them against the
//! index, the ref files and what it has queued already. [`MetaWrite::commit`] appends them
//! as one frame ([`Journal::append_frame`]: the commit point), materializes the frame (its
//! changesets with one [`Index::append_batch`], without a sync; a ref file replaced per moved
//! ref), and unlocks. Every 32 frames past the checkpoint, when it compacts (the sync first),
//! and on [`MetaStore::flush`] if any frame lies past the checkpoint, it also syncs the index
//! and moves the checkpoint to the end of the journal: the batched durability of the
//! projections.
//!
//! What the repairs at the start of a write transaction did, [`MetaWrite::recovered`] says
//! ([`Recovered`]): all zero when the writers before it finished cleanly.
//!
//! An error from [`MetaWrite::commit`] says whether the transaction committed. Once its frame
//! is appended, every error is [`StoreError::PostCommit`]: the frame is durable, a step after
//! it failed, and the next [`MetaStore::write`] repairs that. A failed index sync returns
//! before the checkpoint moves, so the checkpoint never passes records that are not durable;
//! and it first cuts the index back to the records the checkpoint has passed, or to those the
//! sorted views cover if more ([`Index::truncate_to`]), since a later sync may succeed without
//! the pages a failed writeback dropped: the next write's replay appends them again, to be
//! synced afresh. A snapshot taken before the cut can then fail with an I/O error on lookups
//! that read cut records or scan the index's tail (as [`Index::truncate_to`] lists); a new one
//! reads right.
//! An error from the append itself says it as definitely (storage spec §2 rule 3): a failed
//! append of the frame to `journal.log`, its write or its fdatasync, is
//! [`StoreError::NotCommitted`], the frame cut back off the journal under the lock before the
//! error returns (or the process aborted, should the cut fail too), so that the transaction did
//! not commit and no later writer can list its frame; a failed listing of the durable frame in
//! `journal.off` is [`StoreError::PostCommit`], since the frame is the commit point. A caller
//! that retries after `PostCommit` must look again first: an insert is idempotent
//! ([`InsertOutcome::AlreadyPresent`]), and a ref move compares against the ref as it is then.
//! After `NotCommitted` nothing of the transaction is in the store.
//!
//! A compaction appends a `Compacted` record, in a frame of its own, as an advisory marker: a
//! compaction that [`Index::repair_tail`] finishes after a crash appends none, so nothing may
//! expect one per rebuild of the sorted views.
//!
//! # A snapshot
//!
//! [`MetaStore::read`] takes no lock and writes nothing. It reads `journal.ckpt`, opens the
//! index, reads the frames past the checkpoint (which vouch for the index's records past it,
//! as [`Index::open`] says) and every ref file, and reads those frames again: if they are not
//! the frames it read before (new ones have appeared, or the next writer's frame has taken the
//! place of one that a failed sync or rewrite cut back, storage §2 rule 3 (a)/(c)), a ref file
//! may already show a move of a frame it did not read, so it reads the files and the frames
//! again. A `journal.off` that holds fewer entries when the frames are read than when it was
//! measured is no damage either (a writer cut entries that were never genuine: one whose own
//! append failed, storage §2 rule 3 (b), or zeroed ones a crash left at its end): it reads the
//! files and the frames again, and opens the index again if the cut met that open. Each such
//! reading of the files and the frames is a round. When the metadata changed during every one
//! of [`MetaStore::SNAPSHOT_ROUNDS`] (32) rounds, commits kept landing as it read, and it gives
//! up with [`StoreError::SnapshotContended`] (retryable) rather than wait for them to stop. It
//! reads `journal.ckpt` once, so each round reads every frame past that checkpoint, which stays
//! a frame boundary as the journal grows. The frames past the checkpoint (usually 0 to 31, and
//! more only after a crash) then apply on top in memory: their changesets the index handle does
//! not count, and their ref moves the ref files do not show yet.
//!
//! [`MetaStore::read_committed`] takes the same snapshot under [`MetaLock`], once the journal's
//! tail check has left every whole frame durable and listed: one that no power loss can take
//! back, for the readers whose results leave the host (storage spec §3).

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use camino::Utf8Path;
use swarmops_core::ChangesetId;

use super::frame::{Frame, MAX_RECORDS, Record};
use super::index::{ChangesetMeta, Index};
use super::journal::{Journal, LogCut, TailRepair};
use super::lock::MetaLock;
use super::parents::MAX_PARENTS;
use super::refs::{self, RefFile, RefState};
use super::replay::{self, Overlay};
use super::to_u64;
use crate::error::{FileOp, LimitKind, StoreError};
use crate::fs_ops::{FsOps, RealFs, StoreFs};
use crate::layout::Layout;

/// How long [`MetaStore::write`] waits for [`MetaLock`] unless
/// [`MetaStore::with_lock_timeout`] says otherwise.
const DEFAULT_LOCK_TIMEOUT: Duration = Duration::from_secs(10);

/// How many threads a write transaction's [`Index::sync`] syncs the five index files from
/// unless [`MetaStore::set_parallelism`] says otherwise: all five at once.
const DEFAULT_PARALLELISM: usize = 5;

/// How many frames past the checkpoint a commit leaves before it syncs the index and moves
/// the checkpoint.
const CHECKPOINT_EVERY: u64 = 32;

/// How many frames [`MetaRead::reflog`] reads with one call as it walks back.
const REFLOG_BATCH: u64 = 256;

/// The most bytes a ref move's actor may take: its length is one byte of the record.
const MAX_ACTOR_LEN: usize = 255;

/// The directory of the store root in which [`MetaStore::create`] stages a new `meta/`.
const STAGING: &str = "meta.tmp";

/// One store's metadata (`meta/`): a handle that many threads may share, and many processes
/// may hold at once.
pub struct MetaStore {
    layout: Layout,
    /// What every change goes through, and what locates a failure in the store.
    fs: StoreFs,
    journal: Journal,
    lock_timeout: Duration,
    /// What every write transaction's index gets ([`Index::set_parallelism`]).
    parallelism: usize,
}

/// What a write transaction's [`MetaWrite::commit`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommitInfo {
    /// The seq of the frame that holds the transaction's records. A transaction with nothing
    /// queued appends no frame of its own: `frame_seq` is then the seq the next frame takes,
    /// which names the `Compacted` frame its housekeeping appends if it compacts (for a flush,
    /// or a tail that replay took past the threshold).
    pub frame_seq: u64,
    /// How many changesets it inserted.
    pub inserted: u64,
    /// How many ref moves it recorded.
    pub moved_refs: u64,
    /// Whether the commit also synced the index and moved `journal.ckpt` to the end of the
    /// journal: at 32 frames past the checkpoint, after a compaction, or for a flush with a
    /// frame past the checkpoint.
    pub checkpointed: bool,
}

/// What a write transaction's recovery repaired before the transaction's own work
/// ([`MetaWrite::recovered`]): what the writers before it left when they crashed, or failed after
/// their commit point. All zero ([`Recovered::is_empty`]) when they finished cleanly: frames past
/// the checkpoint that replay only reads again, as every write does, count for nothing.
///
/// It is made of what the recovery knows already, with no filesystem call of its own, and kept
/// nowhere: a recovery that fails part way leaves its repairs made and the report lost.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Recovered {
    /// The torn tail cut off `journal.log`, if any: a transaction that never reached its commit
    /// point.
    pub log_cut: Option<LogCut>,
    /// The whole frames `journal.off` did not list that were listed: transactions that committed
    /// and that their writer never listed, and frames whose entries a power loss zeroed.
    pub frames_listed: u64,
    /// Of those, the frames written afresh first, because the sync before their listing failed
    /// (storage spec §2 rule 3 (c)).
    pub frames_rewritten: u64,
    /// The bytes dropped from the end of `journal.off`: a partial entry, zeroed entries.
    pub off_bytes_dropped: u64,
    /// The index record slots cut, counted in the longest column: records a crash left torn, and
    /// records past the checkpoint that the journal does not vouch for (a power loss's).
    pub index_records_dropped: u64,
    /// Whether a compaction a crash interrupted was finished: the sorted views rebuilt.
    pub views_rebuilt: bool,
    /// The changesets replay appended to the index: those of frames committed and not
    /// materialized, and those of records cut.
    pub records_replayed: u64,
    /// The ref files replay replaced or removed.
    pub refs_replayed: u64,
}

impl Recovered {
    /// Whether nothing was repaired.
    pub fn is_empty(&self) -> bool {
        *self == Recovered::default()
    }
}

/// What [`MetaWrite::insert_changeset`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertOutcome {
    /// The changeset is queued; once committed it is index record `seq`.
    Inserted {
        /// Its place in the index, and among the journal's changesets.
        seq: u64,
    },
    /// The store holds the changeset already, or this transaction queued it already.
    AlreadyPresent,
}

/// What [`MetaWrite::cas_ref`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CasResult {
    /// The ref named what the caller expected, and the move is queued.
    Applied {
        /// The target before the move; `None` if the ref did not exist.
        old: Option<ChangesetId>,
    },
    /// The ref named something else; nothing is queued.
    Mismatch {
        /// What the ref names; `None` if it does not exist.
        actual: Option<ChangesetId>,
    },
}

/// What stopped an index open of [`MetaStore::read`] before it gave the index.
enum NotOpened {
    /// `journal.off` held fewer entries when the frames past the checkpoint were read than it
    /// did when measured (a writer cut entries that were never genuine): the round is taken, and
    /// the next one opens the index again.
    Shrunk,
    /// The open failed, and the read fails with it.
    Failed(StoreError),
}

impl From<StoreError> for NotOpened {
    fn from(e: StoreError) -> NotOpened {
        NotOpened::Failed(e)
    }
}

/// One journal record, as [`MetaRead::journal_after`] lists them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalEntry {
    /// The seq of the frame (the transaction) that holds the record.
    pub seq: u64,
    /// `"changeset"`, `"ref_move"` or `"compacted"`.
    pub kind: &'static str,
    /// What the record is about: a changeset's 32-byte id, a moved ref's name; empty for a
    /// compaction.
    pub key: Vec<u8>,
    /// When a ref moved, as its writer recorded it (Unix seconds); 0 for the other kinds,
    /// which record no time.
    pub ts: i64,
}

/// One move of a ref, as [`MetaRead::reflog`] lists them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefMoveEntry {
    /// The seq of the frame that recorded the move.
    pub seq: u64,
    /// The target before the move; `None` if the ref did not exist.
    pub old: Option<ChangesetId>,
    /// The target after the move; `None` if the ref was deleted.
    pub new: Option<ChangesetId>,
    /// Who moved it.
    pub actor: String,
    /// When it moved, in Unix seconds, as its writer recorded it.
    pub ts: i64,
}

impl MetaStore {
    /// How many rounds [`MetaStore::read`] makes at most. A round reads every ref file and the
    /// frames past the checkpoint again, and a snapshot takes one more only when the metadata
    /// changed during the last: a commit landed, or a writer cut an entry of `journal.off` as
    /// the frames were read. With the frames two fdatasyncs apart at least, a second round is rare
    /// and a third rarer still, and a load that lands a frame during every round is not waited
    /// out by more of them: the snapshot gives up with [`StoreError::SnapshotContended`] instead.
    pub const SNAPSHOT_ROUNDS: u32 = 32;

    /// Creates the metadata store of the store rooted at `dir` on the real filesystem: see
    /// [`MetaStore::create_with_fs`].
    pub fn create(dir: &Utf8Path) -> Result<MetaStore, StoreError> {
        Self::create_with_fs(dir, Arc::new(RealFs))
    }

    /// Creates `meta/` under `dir`, every file made through `fs`: an empty journal
    /// (`journal.log`, `journal.off`) with `journal.ckpt` at 0, an empty index (`idx/` with
    /// its four columns, `parents.log`, `sorted/` with its four view files: the only place a
    /// store's index files are made), `refs/` and the lock file, all durable.
    /// [`StoreError::Format`] if `meta/` exists already.
    ///
    /// Crash-atomic: everything is staged in `meta.tmp/meta/`, the `meta/` of a store root of
    /// its own (so the journal and the index lay out their files there as they would in
    /// `meta/`), made durable, and renamed to `meta/` last, which syncs the store root. A crash
    /// before the rename leaves no `meta/`: [`MetaStore::open`] refuses the store with
    /// [`StoreError::Format`], and the next create removes what is left of `meta.tmp/` and
    /// starts over. That removal, and the one of the emptied `meta.tmp/` after the rename, go
    /// through `std::fs`: [`FsOps`] removes no directories. Two creates of one store at once
    /// are not supported.
    pub fn create_with_fs(dir: &Utf8Path, fs: Arc<dyn FsOps>) -> Result<MetaStore, StoreError> {
        // One on the real root, so that a failure on a staged file names it under meta.tmp/.
        let fs = StoreFs::new(fs, dir);
        let meta = Layout::new(dir).meta_dir();
        if std::fs::exists(&meta).map_err(|e| fs.locate(FileOp::Stat, &meta, e))? {
            return Err(StoreError::Format(format!("{meta} exists already")));
        }
        let staging = dir.join(STAGING);
        if std::fs::exists(&staging).map_err(|e| fs.locate(FileOp::Stat, &staging, e))? {
            // What a create that crashed left behind: no part of a store yet.
            std::fs::remove_dir_all(&staging)
                .map_err(|e| fs.locate(FileOp::RemoveDirAll, &staging, e))?;
        }
        let staged = Layout::new(staging.as_path());
        Journal::create_in(&staged, &fs)?.set_ckpt(0)?;
        Index::create_in(&staged, &fs)?;
        let staged_meta = staged.meta_dir();
        fs.create_dir_all(staged.meta_path("refs").as_std_path())?;
        fs.append(staged.meta_lock().as_std_path(), &[])?;
        // The journal and the index made their own entries durable; this makes the last two
        // durable before the rename publishes them.
        fs.fsync_dir(staged_meta.as_std_path())?;
        fs.rename_durable(staged_meta.as_std_path(), meta.as_std_path())?;
        // Best effort: the staging directory is empty now, and harmless if it stays.
        let _ = std::fs::remove_dir(&staging);
        Self::open_on(dir, fs)
    }

    /// Opens the metadata store of the store rooted at `dir` on the real filesystem: see
    /// [`MetaStore::open_with_fs`].
    pub fn open(dir: &Utf8Path) -> Result<MetaStore, StoreError> {
        Self::open_with_fs(dir, Arc::new(RealFs))
    }

    /// Opens the existing metadata store under `dir`, every change going through `fs` (a
    /// `CrashAtFs` in crash tests). Reads only that `meta/journal.log` is there:
    /// [`StoreError::Format`] if it is not.
    pub fn open_with_fs(dir: &Utf8Path, fs: Arc<dyn FsOps>) -> Result<MetaStore, StoreError> {
        Self::open_on(dir, StoreFs::new(fs, dir))
    }

    /// [`MetaStore::open_with_fs`] through `fs`, rooted at `dir`.
    fn open_on(dir: &Utf8Path, fs: StoreFs) -> Result<MetaStore, StoreError> {
        let layout = Layout::new(dir);
        let log = layout.journal_log();
        if !std::fs::exists(&log).map_err(|e| fs.locate(FileOp::Stat, &log, e))? {
            return Err(StoreError::Format(format!(
                "{log} is missing: no metadata store here"
            )));
        }
        let journal = Journal::open_in(&layout, fs.clone())?;
        Ok(Self::from_parts(layout, fs, journal))
    }

    /// This store, its writers waiting up to `timeout` for [`MetaLock`] (10 s unless set)
    /// before they give up with [`StoreError::Busy`].
    #[must_use]
    pub fn with_lock_timeout(mut self, timeout: Duration) -> MetaStore {
        self.lock_timeout = timeout;
        self
    }

    /// Makes the write transactions sync the five index files (their batched
    /// [`Index::sync`], and the sync a compaction starts with) from `threads` threads at once
    /// (0: one per available core), at most one per file. 5 unless set. 1 syncs them one after
    /// another, in write order, so that a test counting [`FsOps`] calls (a crash sweep's
    /// `CrashAtFs`) meets the same call at the same count in every run.
    pub fn set_parallelism(&mut self, threads: usize) {
        self.parallelism = threads;
    }

    /// A snapshot of the metadata as committed now, taken without a lock in at most
    /// [`MetaStore::SNAPSHOT_ROUNDS`] rounds, or [`StoreError::SnapshotContended`] if the
    /// metadata changed during every one: see the module documentation.
    pub fn read(&self) -> Result<MetaRead<'_>, StoreError> {
        let ckpt = self.journal.ckpt()?;
        let mut opened = None;
        for _ in 0..Self::SNAPSHOT_ROUNDS {
            // The index after the checkpoint, opened in the first round (and again after a round
            // whose open met a cut journal.off): everything before the checkpoint was in the
            // index files before the checkpoint moved past it. The frames past it, read once the
            // index files are measured, vouch for the index's records past it. A journal.off that
            // a writer cut as they were read takes the round.
            let (index, frames) = if let Some(opened) = opened.take() {
                opened
            } else {
                let past = || {
                    let frames = self.journal.tail_frames_from_unless_shrunk(ckpt)?;
                    frames.ok_or(NotOpened::Shrunk)
                };
                match Index::open_with_past(&self.layout, self.fs.clone(), past) {
                    Ok(opened) => opened,
                    Err(NotOpened::Shrunk) => continue,
                    Err(NotOpened::Failed(e)) => return Err(e),
                }
            };
            let files = refs::read_all(&self.layout, &self.fs)?;
            // A writer replaces ref files only after its frame is whole in the journal, so if the
            // frames are the ones read before (none new, and none that a failed sync or rewrite cut
            // back, storage §2 rule 3 (a)/(c), and the next writer replaced at its seq), the files
            // show none past `frames`. A journal.off cut as they were read again says nothing
            // about the files, and takes the round all the same.
            match self.journal.tail_frames_from_unless_shrunk(ckpt)? {
                Some(again) if again == frames => {
                    let overlay = Overlay::new(&index, &frames, files)?;
                    return Ok(MetaRead {
                        store: self,
                        ckpt,
                        index,
                        frames,
                        overlay,
                    });
                }
                Some(again) => opened = Some((index, again)),
                None => opened = Some((index, frames)),
            }
        }
        Err(StoreError::SnapshotContended {
            rounds: Self::SNAPSHOT_ROUNDS,
        })
    }

    /// A snapshot of the committed metadata only, for the readers whose results leave the host
    /// (M8's git export, M11's `serve` and `push`): the snapshot [`MetaStore::read`] takes, taken
    /// under [`MetaLock`] after the journal's tail check. It takes the lock as
    /// [`MetaStore::write`] does (waiting as [`MetaStore::with_lock_timeout`] says, then
    /// [`StoreError::Busy`]), runs the tail check ([`Journal::repair_tail`], storage spec §2 rule
    /// 3: the whole frames `journal.off` does not list synced and listed, or written afresh first;
    /// a torn tail cut), takes the snapshot while it still holds the lock (in one round, since no
    /// writer runs), and lets the lock go before it returns the snapshot with what the tail check
    /// repaired. No writer is between its frame's write and its `fdatasync`, or its cut,
    /// meanwhile, and every whole frame left has passed its commit point: neither a power loss nor
    /// a cut under rule 3 (a) can take back what the snapshot returns, which a lock-free snapshot
    /// cannot promise (storage spec §3, "읽기"). Local readers keep the lock-free one.
    ///
    /// It costs a wait behind the writer that holds the lock: about one transaction, longer under
    /// contention. The lock is held for the tail check and the snapshot only, not while the caller
    /// reads what the snapshot names. It repairs no index and replays nothing: the snapshot's
    /// overlay supplies the frames past the checkpoint. On a clean tail it writes nothing; a tail
    /// that needs a repair it repairs as the next writer would, and it fails where that writer's
    /// tail check fails ([`MetaStore::write`]): on a read-only filesystem, say.
    pub fn read_committed(&self) -> Result<(MetaRead<'_>, TailRepair), StoreError> {
        let _lock = MetaLock::acquire(&self.layout, self.lock_timeout)?;
        let repair = self.journal.repair_tail()?;
        // Under the lock no frame lands, nor is any cut, while it reads: one round.
        let snapshot = self.read()?;
        Ok((snapshot, repair))
    }

    /// Starts a write transaction: takes [`MetaLock`] (waiting as
    /// [`MetaStore::with_lock_timeout`] says), repairs the journal's and the index's tails,
    /// and replays the frames past the checkpoint into the index and the ref files. The
    /// checkpoint itself moves only at the batched sync points of [`MetaWrite::commit`]. If the
    /// journal's tail check cannot sync the whole frames `journal.off` does not list yet, nor
    /// write them afresh, or cannot list them, the error is [`StoreError::NotCommitted`] (storage
    /// spec §2 rule 3 (c), (b)): the transaction never began, and running it again lists them.
    pub fn write(&self) -> Result<MetaWrite<'_>, StoreError> {
        let lock = MetaLock::acquire(&self.layout, self.lock_timeout)?;
        let tail = self.journal.repair_tail()?;
        let ckpt = self.journal.ckpt()?;
        // The frames past the checkpoint vouch for the index's records past it, and are
        // replayed.
        let (mut index, frames) = Index::open_with_past(&self.layout, self.fs.clone(), || {
            self.journal.tail_frames_from(ckpt)
        })?;
        index.set_parallelism(self.parallelism);
        let repaired = index.repair_tail_with(&frames)?;
        let replayed = replay::replay(&mut index, &self.layout, &self.fs, &frames)?;
        // The index holds every changeset of the journal now, those of the frames past the
        // checkpoint last.
        let past = frames
            .iter()
            .flat_map(|frame| &frame.records)
            .filter(|record| matches!(record, Record::Changeset { .. }))
            .count();
        Ok(MetaWrite {
            store: self,
            checkpointed_records: index.len().saturating_sub(to_u64(past)),
            index,
            uncheckpointed: to_u64(frames.len()),
            records: Vec::new(),
            queued: HashMap::new(),
            moved: HashMap::new(),
            recovered: Recovered {
                log_cut: tail.cut,
                frames_listed: tail.frames_listed,
                frames_rewritten: tail.frames_rewritten,
                off_bytes_dropped: tail.off_bytes_dropped,
                index_records_dropped: repaired.dropped,
                views_rebuilt: repaired.views_rebuilt,
                records_replayed: replayed.records,
                refs_replayed: replayed.refs,
            },
            _lock: lock,
        })
    }

    /// Every ref file, by name (`main`, then `release/<x>` sorted), each with its state or the
    /// error reading it gave. Takes no lock and writes nothing. For checking the files
    /// themselves (verify): one damaged ref file fails every [`MetaStore::read`], so this
    /// reads each file on its own. It shows the files only, not the moves in frames past the
    /// checkpoint that no write has materialized yet, which a snapshot applies on top.
    /// Entries of `meta/refs/` that cannot be refs (temp files, names this version does not
    /// use) are skipped, as snapshots skip them.
    pub fn ref_files(&self) -> Result<Vec<RefFile>, StoreError> {
        refs::read_each(&self.layout, &self.fs)
    }

    /// Makes the materialized files durable now: takes the lock, replays, then, if any frame
    /// lies past the checkpoint, syncs the index and moves the checkpoint to the end of the
    /// journal. For callers that need the projections durable at once (tests, closing).
    pub fn flush(&self) -> Result<(), StoreError> {
        self.write()?.finish(true).map(drop)
    }

    fn from_parts(layout: Layout, fs: StoreFs, journal: Journal) -> MetaStore {
        Self {
            layout,
            fs,
            journal,
            lock_timeout: DEFAULT_LOCK_TIMEOUT,
            parallelism: DEFAULT_PARALLELISM,
        }
    }
}

impl fmt::Debug for MetaStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MetaStore")
            .field("root", &self.layout.root())
            .field("journal", &self.journal)
            .field("lock_timeout", &self.lock_timeout)
            .field("parallelism", &self.parallelism)
            .finish_non_exhaustive()
    }
}

/// A snapshot of the metadata, from [`MetaStore::read`]: the index and the ref files as they
/// were, with the frames past the checkpoint applied on top. Later commits do not show.
pub struct MetaRead<'a> {
    store: &'a MetaStore,
    /// The checkpoint the snapshot was taken at: `frames` start there.
    ckpt: u64,
    index: Index,
    /// The frames past the checkpoint, in journal order.
    frames: Vec<Frame>,
    overlay: Overlay,
}

impl MetaRead<'_> {
    /// Changeset `id`: an index lookup (a search of the sorted view, then the tail), then the
    /// frames past the checkpoint.
    pub fn changeset(&self, id: ChangesetId) -> Result<Option<ChangesetMeta>, StoreError> {
        match self.index.find(id)? {
            Some(rec) => self.index.get(rec).map(Some),
            None => Ok(self.overlay.changeset(id).cloned()),
        }
    }

    /// The first parent of changeset `id`, as [`MetaRead::changeset`] gives it, without the
    /// rest of its record: `None` if the snapshot does not hold `id`, `Some(None)` for a root.
    /// The same index lookup, then the record's `par.col` entry and its first parent in
    /// `parents.log` ([`Index::first_parent`]); or the frames past the checkpoint. A step of a
    /// first-parent walk.
    pub fn first_parent(&self, id: ChangesetId) -> Result<Option<Option<ChangesetId>>, StoreError> {
        match self.index.find(id)? {
            Some(rec) => self.index.first_parent(rec).map(Some),
            None => Ok(self
                .overlay
                .changeset(id)
                .map(|meta| meta.parents.first().copied())),
        }
    }

    /// The changesets whose ids start with `hex`, 1 to 64 hex digits of either case
    /// (anything else is [`StoreError::BadPrefix`]), in seq order.
    pub fn resolve_prefix(&self, hex: &str) -> Result<Vec<ChangesetId>, StoreError> {
        let mut ids = self.ids_at(self.index.find_prefix(hex)?)?;
        let prefix = hex.to_ascii_lowercase();
        ids.extend(
            self.overlay
                .extras()
                .iter()
                .filter(|meta| meta.id.0.to_hex().starts_with(&prefix))
                .map(|meta| meta.id),
        );
        Ok(ids)
    }

    /// The changesets whose delta hash is `dh` (the same change on different bases), in seq
    /// order.
    pub fn by_delta_hash(&self, dh: &[u8; 32]) -> Result<Vec<ChangesetId>, StoreError> {
        let mut ids = self.ids_at(self.index.find_delta_hash(dh)?)?;
        ids.extend(
            self.overlay
                .extras()
                .iter()
                .filter(|meta| meta.delta_hash == *dh)
                .map(|meta| meta.id),
        );
        Ok(ids)
    }

    /// Ref `name`; `None` if it does not exist. [`StoreError::RefName`] for a name that is
    /// not `main` or `release/<x>`.
    pub fn get_ref(&self, name: &str) -> Result<Option<RefState>, StoreError> {
        refs::check_ref_name(name)?;
        Ok(self.overlay.refs().get(name).copied())
    }

    /// Every ref, by name.
    pub fn list_refs(&self) -> Result<Vec<(String, RefState)>, StoreError> {
        Ok(self
            .overlay
            .refs()
            .iter()
            .map(|(name, state)| (name.clone(), *state))
            .collect())
    }

    /// The last `limit` moves of ref `name`, newest first: the journal's `RefMove` records
    /// for it, read back from the end of the snapshot (`O(frames)` until `limit` are found).
    pub fn reflog(&self, name: &str, limit: usize) -> Result<Vec<RefMoveEntry>, StoreError> {
        refs::check_ref_name(name)?;
        let mut log = Vec::new();
        collect_moves(self.frames.iter().rev(), name, limit, &mut log);
        // Then the frames before the checkpoint, a batch at a time; with no frames past the
        // checkpoint, frames listed since the snapshot was taken are skipped.
        let mut end = match self.frames.first() {
            Some(frame) => frame.seq,
            None => self.store.journal.frame_count()?,
        };
        while log.len() < limit && end > 0 {
            let start = end.saturating_sub(REFLOG_BATCH);
            let batch = self
                .store
                .journal
                .frames_from(start, to_usize(end - start))?;
            let before = batch.iter().rev().filter(|frame| frame.offset < self.ckpt);
            collect_moves(before, name, limit, &mut log);
            end = start;
        }
        Ok(log)
    }

    /// The records of up to `limit` frames (whole frames, in journal order) after frame
    /// `after`, or from the first frame for `None`: a cursor for following the journal.
    pub fn journal_after(
        &self,
        after: Option<u64>,
        limit: usize,
    ) -> Result<Vec<JournalEntry>, StoreError> {
        let first = match after {
            None => 0,
            Some(seq) => match seq.checked_add(1) {
                Some(first) => first,
                None => return Ok(Vec::new()),
            },
        };
        let mut frames = Vec::new();
        // The frames before the checkpoint come from journal.off; the snapshot holds the rest.
        let overlay_start = self.frames.first().map_or(u64::MAX, |frame| frame.seq);
        if first < overlay_start {
            let wanted = to_usize(overlay_start - first).min(limit);
            let listed = self.store.journal.frames_from(first, wanted)?;
            frames.extend(
                listed
                    .into_iter()
                    .take_while(|frame| frame.offset < self.ckpt),
            );
        }
        let room = limit - frames.len();
        let past = self.frames.iter().filter(|frame| frame.seq >= first);
        frames.extend(past.take(room).cloned());
        Ok(frames.iter().flat_map(entries).collect())
    }

    /// How many changesets the store holds.
    pub fn count_changesets(&self) -> Result<u64, StoreError> {
        Ok(self.index.len() + to_u64(self.overlay.extras().len()))
    }

    /// The ids of index records `recs`: one `ids.col` read each ([`Index::id`]).
    fn ids_at(&self, recs: Vec<u64>) -> Result<Vec<ChangesetId>, StoreError> {
        recs.into_iter().map(|rec| self.index.id(rec)).collect()
    }
}

impl fmt::Debug for MetaRead<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MetaRead")
            .field("ckpt", &self.ckpt)
            .field("index", &self.index)
            .field("frames_past_ckpt", &self.frames.len())
            .finish_non_exhaustive()
    }
}

/// A write transaction, from [`MetaStore::write`]: it holds [`MetaLock`] until it is
/// committed or dropped. Dropping it without [`MetaWrite::commit`] discards what it queued.
pub struct MetaWrite<'a> {
    store: &'a MetaStore,
    /// Up to date with every committed frame: [`MetaStore::write`] replayed them.
    index: Index,
    /// How many of the index's records lie before the checkpoint: synced before it moved
    /// past them, so a failed sync never cuts them.
    checkpointed_records: u64,
    /// How many frames lie past the checkpoint, not counting this transaction's.
    uncheckpointed: u64,
    /// The records queued, in order: the frame [`MetaWrite::commit`] appends.
    records: Vec<Record>,
    /// The generation of each changeset queued.
    queued: HashMap<ChangesetId, u64>,
    /// The target each ref moved so far is left at.
    moved: HashMap<String, Option<ChangesetId>>,
    /// What [`MetaStore::write`] repaired before it returned this transaction.
    recovered: Recovered,
    _lock: MetaLock,
}

impl MetaWrite<'_> {
    /// What the recovery that opened this transaction repaired ([`MetaStore::write`]): what the
    /// writers before it left when they crashed or failed after their commit point. All zero
    /// when they finished cleanly. It reads nothing.
    pub fn recovered(&self) -> Recovered {
        self.recovered
    }

    /// Queues changeset `meta`, unless the store or this transaction holds it already
    /// ([`InsertOutcome::AlreadyPresent`]). Its `seq` and `generation` are ignored: the
    /// store numbers its changesets, and a generation is 1 + the largest parent generation,
    /// 1 for a root.
    ///
    /// Every parent must be in the store or queued before it: otherwise
    /// [`StoreError::MissingParent`]. A parent listed twice is
    /// [`StoreError::DuplicateParent`]; more than 255 parents, or a transaction that holds
    /// 65,535 records already (all one frame holds), [`StoreError::Limit`]. Nothing is queued
    /// then. It is [`MetaWrite::insert_changesets_batch`] of `meta` alone.
    pub fn insert_changeset(&mut self, meta: ChangesetMeta) -> Result<InsertOutcome, StoreError> {
        let mut staged = Staged::default();
        let outcome = self.stage(meta, &mut staged)?;
        self.queue(staged)?;
        Ok(outcome)
    }

    /// Queues `metas` in order, each as [`MetaWrite::insert_changeset`] queues one, and returns
    /// what it did with each: [`InsertOutcome::AlreadyPresent`] for a changeset the store holds,
    /// this transaction queued already, or `metas` named before it. A parent may be in the
    /// store, queued before, or earlier in `metas`. They join the transaction's one frame, whose
    /// commit materializes its changesets with one [`Index::append_batch`].
    ///
    /// All or nothing: for more than 255 parents ([`StoreError::Limit`]), a parent listed twice
    /// ([`StoreError::DuplicateParent`]) or a parent that is nowhere
    /// ([`StoreError::MissingParent`]) in any of `metas`, or for more records than the frame's
    /// 65,535 with those queued already ([`StoreError::Limit`], `got` the records the frame
    /// would hold), it queues nothing of `metas`; what the transaction queued before stays
    /// queued.
    pub fn insert_changesets_batch(
        &mut self,
        metas: Vec<ChangesetMeta>,
    ) -> Result<Vec<InsertOutcome>, StoreError> {
        let mut staged = Staged::default();
        let outcomes = (metas.into_iter())
            .map(|meta| self.stage(meta, &mut staged))
            .collect::<Result<Vec<_>, _>>()?;
        self.queue(staged)?;
        Ok(outcomes)
    }

    /// Checks `meta` as [`MetaWrite::insert_changeset`] does, against the store, this
    /// transaction's queue and `staged`, but for the room in the frame, which
    /// [`MetaWrite::queue`] checks; and stages it, unless one of them holds it already.
    fn stage(&self, meta: ChangesetMeta, staged: &mut Staged) -> Result<InsertOutcome, StoreError> {
        if meta.parents.len() > MAX_PARENTS {
            return Err(StoreError::Limit {
                what: LimitKind::Parents,
                max: to_u64(MAX_PARENTS),
                got: to_u64(meta.parents.len()),
            });
        }
        if staged.generations.contains_key(&meta.id)
            || self.queued.contains_key(&meta.id)
            || self.index.find(meta.id)?.is_some()
        {
            return Ok(InsertOutcome::AlreadyPresent);
        }
        for (at, parent) in meta.parents.iter().enumerate() {
            if meta.parents[..at].contains(parent) {
                return Err(StoreError::DuplicateParent(*parent));
            }
        }
        let mut generation = 1;
        for &parent in &meta.parents {
            let parent_generation = self
                .staged_generation(parent, staged)?
                .ok_or(StoreError::MissingParent(parent))?;
            generation = generation.max(parent_generation.saturating_add(1));
        }
        let seq = self.index.len() + to_u64(self.queued.len() + staged.generations.len());
        staged.generations.insert(meta.id, generation);
        staged.records.push(Record::Changeset {
            id: meta.id,
            generation,
            delta_hash: meta.delta_hash,
            parents: meta.parents,
        });
        Ok(InsertOutcome::Inserted { seq })
    }

    /// Queues what `staged` holds, if the transaction's frame has room for it: else
    /// [`StoreError::Limit`], and nothing is queued.
    fn queue(&mut self, staged: Staged) -> Result<(), StoreError> {
        let records = self.records.len() + staged.records.len();
        if records > MAX_RECORDS {
            return Err(StoreError::Limit {
                what: LimitKind::Records,
                max: to_u64(MAX_RECORDS),
                got: to_u64(records),
            });
        }
        self.queued.extend(staged.generations);
        self.records.extend(staged.records);
        Ok(())
    }

    /// Queues a move of ref `name` from `expected` to `new` (`None` deletes it) by `actor`,
    /// if the ref names `expected` now: as its file says, or the last move this transaction
    /// queued. Otherwise [`CasResult::Mismatch`], and nothing is queued.
    ///
    /// A name that is not `main` or `release/<x>` is [`StoreError::RefName`]; a `new` that is
    /// neither in the store nor queued, [`StoreError::UnknownRefTarget`]; an actor longer than
    /// 255 bytes, or a move that would not fit in the transaction's frame (65,535 records),
    /// [`StoreError::Limit`].
    pub fn cas_ref(
        &mut self,
        name: &str,
        expected: Option<ChangesetId>,
        new: Option<ChangesetId>,
        actor: &str,
    ) -> Result<CasResult, StoreError> {
        refs::check_ref_name(name)?;
        if actor.len() > MAX_ACTOR_LEN {
            return Err(StoreError::Limit {
                what: LimitKind::ActorBytes,
                max: to_u64(MAX_ACTOR_LEN),
                got: to_u64(actor.len()),
            });
        }
        if let Some(target) = new
            && self.generation(target)?.is_none()
        {
            return Err(StoreError::UnknownRefTarget(target));
        }
        let current = match self.moved.get(name) {
            Some(&target) => target,
            None => refs::read(&self.store.layout, &self.store.fs, name)?.map(|state| state.target),
        };
        if current != expected {
            return Ok(CasResult::Mismatch { actual: current });
        }
        self.check_room()?;
        self.records.push(Record::RefMove {
            name: name.to_owned(),
            old: current,
            new,
            actor: actor.to_owned(),
            ts: unix_now(),
        });
        self.moved.insert(name.to_owned(), new);
        Ok(CasResult::Applied { old: current })
    }

    /// Commits what is queued as one journal frame, materializes it, and unlocks: see the
    /// module documentation. A transaction with nothing queued appends no frame of its own,
    /// though its housekeeping may still compact (appending a `Compacted` frame) or
    /// checkpoint.
    ///
    /// Once the frame is durable in `journal.log`, every error is [`StoreError::PostCommit`]:
    /// the transaction committed. A failed append of the frame is [`StoreError::NotCommitted`],
    /// the frame cut back off the journal; any other error before it means the transaction did
    /// not commit either. With nothing queued, errors are the housekeeping's own.
    pub fn commit(self) -> Result<CommitInfo, StoreError> {
        self.finish(false)
    }

    /// [`MetaWrite::commit`]; `flush` also syncs and checkpoints any frame past the
    /// checkpoint.
    fn finish(mut self, flush: bool) -> Result<CommitInfo, StoreError> {
        let store = self.store;
        if self.records.is_empty() {
            let info = CommitInfo {
                frame_seq: store.journal.frame_count()?,
                inserted: 0,
                moved_refs: 0,
                checkpointed: false,
            };
            return self.settle(info, flush);
        }
        let frame = store.journal.append_frame(&self.records)?; // the commit point
        self.uncheckpointed += 1;
        let info = CommitInfo {
            frame_seq: frame.seq,
            inserted: to_u64(self.queued.len()),
            moved_refs: to_u64(self.records.len() - self.queued.len()),
            checkpointed: false,
        };
        replay::materialize(&mut self.index, &store.layout, &store.fs, &frame)
            .and_then(|()| self.settle(info, flush))
            .map_err(|source| StoreError::PostCommit {
                frame_seq: frame.seq,
                source: Box::new(source),
            })
    }

    /// The batched durability of the projections, once the frames are materialized: with the
    /// tail past [`Index::COMPACT_THRESHOLD`], an index sync, a compaction and its `Compacted`
    /// frame; else, at 32 frames past the checkpoint or for a flush with any, an index sync.
    /// Then the checkpoint moves to the end of the journal; a failed sync or compaction
    /// returns first, so it never moves past what is not durable, and a failed sync cuts the
    /// index back to the checkpoint first ([`MetaWrite::sync_index`]).
    fn settle(&mut self, mut info: CommitInfo, flush: bool) -> Result<CommitInfo, StoreError> {
        let compact = self.index.tail_len() > Index::COMPACT_THRESHOLD;
        let due = self.uncheckpointed >= CHECKPOINT_EVERY || (flush && self.uncheckpointed > 0);
        if !compact && !due {
            return Ok(info);
        }
        // Before a compaction too, which syncs the index again itself: a failure of that sync
        // could not be told from one writing the views, and would cut nothing.
        self.sync_index()?;
        let journal = &self.store.journal;
        if compact {
            let sorted_count = self.index.compact()?;
            journal
                .append_frame(&[Record::Compacted { sorted_count }])
                .map_err(marker_error)?;
        }
        journal.set_ckpt(journal.len())?;
        info.checkpointed = true;
        Ok(info)
    }

    /// [`Index::sync`]; if it fails, first cuts the index back to the records the checkpoint
    /// has passed ([`Index::truncate_to`]). Linux reports a failed writeback once, to the next
    /// sync, and may drop the pages it failed to write: a later sync can then succeed without
    /// those records ever reaching the disk. Cut, they are appended again by the next write's
    /// replay, from the journal, and synced afresh. If the cut fails too, the error says so
    /// beside the sync's, as what else failed; the records stay, and the next writer keeps them,
    /// since it finds them whole: the hazard stays open until the next sync that succeeds, which
    /// may pass them.
    fn sync_index(&mut self) -> Result<(), StoreError> {
        let Err(failed) = self.index.sync() else {
            return Ok(());
        };
        let Err(cut) = self.index.truncate_to(self.checkpointed_records) else {
            return Err(failed);
        };
        Err(match failed {
            StoreError::Io(failed) => failed
                .with_also(format!(
                    "cutting the index back to the checkpoint failed too: {cut}"
                ))
                .into(),
            // The sync fails only on the filesystem: every one of its errors is an `Io`.
            other => other,
        })
    }

    /// Refuses, with [`StoreError::Limit`], a record past what the transaction's one frame
    /// holds.
    fn check_room(&self) -> Result<(), StoreError> {
        if self.records.len() < MAX_RECORDS {
            Ok(())
        } else {
            Err(StoreError::Limit {
                what: LimitKind::Records,
                max: to_u64(MAX_RECORDS),
                got: to_u64(self.records.len()) + 1,
            })
        }
    }

    /// The generation of changeset `id` if `staged` holds it, this transaction queued it, or
    /// the store holds it; `None` otherwise.
    fn staged_generation(
        &self,
        id: ChangesetId,
        staged: &Staged,
    ) -> Result<Option<u64>, StoreError> {
        if let Some(&generation) = staged.generations.get(&id) {
            return Ok(Some(generation));
        }
        self.generation(id)
    }

    /// The generation of changeset `id` if this transaction queued it or the store holds
    /// it (one `gen.col` read once it is found: [`Index::generation`]); `None` otherwise.
    fn generation(&self, id: ChangesetId) -> Result<Option<u64>, StoreError> {
        if let Some(&generation) = self.queued.get(&id) {
            return Ok(Some(generation));
        }
        self.index
            .find(id)?
            .map(|rec| self.index.generation(rec))
            .transpose()
    }
}

impl fmt::Debug for MetaWrite<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MetaWrite")
            .field("index", &self.index)
            .field("checkpointed_records", &self.checkpointed_records)
            .field("uncheckpointed", &self.uncheckpointed)
            .field("records_queued", &self.records.len())
            .field("recovered", &self.recovered)
            .finish_non_exhaustive()
    }
}

/// What one call of [`MetaWrite::insert_changeset`] or [`MetaWrite::insert_changesets_batch`]
/// queues, held apart until all of it is checked, so that an error leaves none of it queued.
#[derive(Default)]
struct Staged {
    /// The generation of each changeset staged.
    generations: HashMap<ChangesetId, u64>,
    /// Their records, in order.
    records: Vec<Record>,
}

/// The error of a failed append of the advisory `Compacted` marker, once the compaction is
/// done: a marker cut back off the journal is told as the plain I/O failure it is, saying what
/// the append was. [`StoreError::NotCommitted`] would say that the transaction did not commit,
/// when it did (or queued nothing), and inside [`StoreError::PostCommit`] it would say both.
fn marker_error(e: StoreError) -> StoreError {
    match e {
        StoreError::NotCommitted { source } => source
            .with_also(
                "it was appending the Compacted marker, which was cut back off the journal (the \
                 compaction itself is done)",
            )
            .into(),
        other => other,
    }
}

/// Appends to `log` the moves of ref `name` that `frames` (newest first) record, newest
/// first, until it holds `limit`.
fn collect_moves<'f>(
    frames: impl Iterator<Item = &'f Frame>,
    name: &str,
    limit: usize,
    log: &mut Vec<RefMoveEntry>,
) {
    for frame in frames {
        for record in frame.records.iter().rev() {
            if log.len() >= limit {
                return;
            }
            if let Record::RefMove {
                name: moved,
                old,
                new,
                actor,
                ts,
            } = record
                && moved == name
            {
                log.push(RefMoveEntry {
                    seq: frame.seq,
                    old: *old,
                    new: *new,
                    actor: actor.clone(),
                    ts: *ts,
                });
            }
        }
    }
}

/// The entries of `frame`'s records, in order.
fn entries(frame: &Frame) -> impl Iterator<Item = JournalEntry> + '_ {
    frame.records.iter().map(|record| {
        let (kind, key, ts) = match record {
            Record::Changeset { id, .. } => ("changeset", id.0.0.to_vec(), 0),
            Record::RefMove { name, ts, .. } => ("ref_move", name.as_bytes().to_vec(), *ts),
            Record::Compacted { .. } => ("compacted", Vec::new(), 0),
        };
        JournalEntry {
            seq: frame.seq,
            kind,
            key,
            ts,
        }
    })
}

/// Now, in Unix seconds (negative before 1970).
fn unix_now() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(since) => i64::try_from(since.as_secs()).unwrap_or(i64::MAX),
        Err(before) => i64::try_from(before.duration().as_secs()).map_or(i64::MIN, |secs| -secs),
    }
}

/// `n`, or `usize::MAX` if it does not fit (a limit that large means no limit).
fn to_usize(n: u64) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}
