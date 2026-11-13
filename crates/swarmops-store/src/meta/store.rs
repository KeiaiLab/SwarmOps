//! The metadata store's facade (storage spec §3): [`MetaStore`] opens a store's `meta/`,
//! [`MetaStore::read`] takes a lock-free snapshot ([`MetaRead`]) and [`MetaStore::write`]
//! starts a write transaction ([`MetaWrite`]). The protocol is the spec's: §3's write
//! transaction, its commit results and errors, and its reads, with §2 rule 3 for a failed append
//! or sync. What follows says which method does which part, and what a caller must read again.
//!
//! # A write transaction
//!
//! [`MetaStore::write`] takes [`MetaLock`] and, before it returns, repairs what the writers
//! before it left: the journal's tail ([`Journal::repair_tail`]), the index's
//! ([`Index::repair_tail`]), and the frames past `journal.ckpt`, which it replays into the index
//! and the ref files. [`MetaWrite::recovered`] says what that repair did ([`Recovered`]: all
//! zero when those writers finished cleanly). The [`MetaWrite`] holds the lock until
//! [`MetaWrite::commit`] returns, or until it is dropped, which discards what it queued. It
//! checks each record it queues against the index, the ref files and its queue, and reads a ref
//! ([`MetaWrite::get_ref`]) or a changeset ([`MetaWrite::changeset`]) as it sees them: what it
//! queued first, then those files.
//! [`MetaWrite::commit`] appends them as one frame ([`Journal::append_frame`]: the commit
//! point), materializes the frame (its changesets with one [`Index::append_batch`], a ref file per
//! moved ref), and, in §3's batches or on [`MetaStore::flush`], syncs the index and moves the
//! checkpoint. A transaction may hold several frames, one per transaction it carries
//! ([`MetaWrite::begin_frame`], [`MetaWrite::drop_frame`], [`MetaWrite::keep_frames`]), each
//! seeing what the ones before it queued: [`MetaWrite::commit_frames`] appends them as a batch
//! ([`Journal::append_frames`]) and materializes them once (the M18 D2 design §3.3).
//!
//! An error says which side of the commit point it fell on. After [`StoreError::NotCommitted`]
//! nothing of the transaction is in the store. After [`StoreError::PostCommit`] the frame is
//! durable and the next [`MetaStore::write`] repairs the rest, and a caller that retries must
//! look again first: an insert is idempotent ([`InsertOutcome::AlreadyPresent`]), and a ref
//! move compares against the ref as it is then. A failed index sync cuts the index back
//! ([`Index::truncate_to`]) before it returns, so a snapshot taken before the cut can then fail
//! with an I/O error on the lookups that method lists; a new snapshot reads right. A
//! compaction's `Compacted` record is an advisory marker: a compaction that
//! [`Index::repair_tail`] finishes after a crash appends none, so nothing may expect one per
//! rebuild of the sorted views.
//!
//! # A snapshot
//!
//! [`MetaStore::read`] takes no lock and writes nothing. It reads `journal.ckpt` once, opens the
//! index ([`Index::open`] says which of its records it counts), and reads the frames past the
//! checkpoint and every ref file, in rounds, until the frames hold still: a round that finds them
//! changed, or `journal.off` cut back since it was measured, reads again (and opens the index
//! again if the cut met that open). When they changed during every one of
//! [`MetaStore::SNAPSHOT_ROUNDS`] rounds, it gives up with [`StoreError::SnapshotContended`]
//! (retryable). The frames it read then apply on top of the index and the ref files, in memory.
//! A [`MetaRead`] shows no later commit, and a new [`MetaStore::read`] shows this process's
//! writes at once.
//!
//! [`MetaStore::read_committed`] takes the same snapshot under [`MetaLock`], once the journal's
//! tail check has left every whole frame durable and listed: one that no power loss can take
//! back, for the readers whose results leave the host (storage spec §3).

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
#[cfg(feature = "test-hooks")]
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use camino::Utf8Path;
use swarmops_core::{ChangesetId, bytes};

use super::facts::Side;
use super::features;
use super::frame::{self, Fact, FactKind, Frame, MAX_RECORDS, MAX_RECORDS_V3, Record};
use super::index::{ChangesetMeta, Index};
use super::journal::{Journal, LogCut, PastFrames, TailRepair};
use super::lock::MetaLock;
use super::parents::MAX_PARENTS;
use super::refs::{self, RefFile, RefState};
use super::replay::{self, Overlay};
use super::runs::{RUN_BACKLOG_MAX, RUN_MIN};
use super::views::{self, Step};
use crate::error::{FileOp, LimitKind, StoreError};
use crate::format::{self, StoreFormat};
use crate::fs_ops::{FsOps, RealFs, StoreFs};
use crate::layout::Layout;

/// How many write transactions [`MetaStore::write`] and the write queue's leads began in this
/// process: the lock probe of the M16 design §4 (a land takes the lock once), which the test
/// hooks alone count.
#[cfg(feature = "test-hooks")]
static WRITE_CALLS: AtomicU64 = AtomicU64::new(0);

/// How many committed snapshots [`MetaStore::read_committed`] began in this process: the probe
/// of the M17 design §4.2 (a refresh takes one), which the test hooks alone count.
#[cfg(feature = "test-hooks")]
static READ_COMMITTED_CALLS: AtomicU64 = AtomicU64::new(0);

/// How long [`MetaStore::write`] waits for [`MetaLock`] unless
/// [`MetaStore::with_lock_timeout`] says otherwise.
const DEFAULT_LOCK_TIMEOUT: Duration = Duration::from_secs(10);

/// How many threads a write transaction's [`Index::sync`] syncs the index files from unless
/// [`MetaStore::set_parallelism`] says otherwise: all at once, the six of FORMAT 3 (`ids.col`,
/// `gen.col`, `dh.col`, `par.col`, `parents.log`, `facts.col`), the five of FORMAT 1 and 2.
const DEFAULT_PARALLELISM: usize = 6;

/// How many frames past the checkpoint a commit leaves before it syncs the index and moves
/// the checkpoint.
const CHECKPOINT_EVERY: u64 = 32;

/// When a write transaction makes the projections durable: at [`CHECKPOINT_EVERY`] frames
/// ([`Settle::Batched`], a commit) or now, if any frame lies past the checkpoint
/// ([`Settle::Flush`], [`MetaStore::flush`]).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Settle {
    Batched,
    Flush,
}

/// How many frames [`MetaRead::reflog`] and [`MetaRead::frames_before`] read with one call as
/// they walk back.
const REFLOG_BATCH: u64 = 256;

/// The most bytes a ref move's actor may take: its length is one byte of the record.
const MAX_ACTOR_LEN: usize = 255;

/// The directory of the store root in which [`MetaStore::create`] stages a new `meta/`.
const STAGING: &str = "meta.tmp";

/// One store's metadata (`meta/`): a handle that many threads may share, and many processes
/// may hold at once.
#[non_exhaustive]
pub struct MetaStore {
    pub(super) layout: Layout,
    /// What every change goes through, and what locates a failure in the store.
    pub(super) fs: StoreFs,
    journal: Journal,
    lock_timeout: Duration,
    /// What every write transaction's index gets ([`Index::set_parallelism`]).
    parallelism: usize,
}

/// What a write transaction's [`MetaWrite::commit`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct CommitInfo {
    /// The seq of the frame that holds the transaction's records. A transaction with nothing
    /// queued appends no frame of its own: `frame_seq` is then the seq the next frame takes,
    /// which names the `Compacted` frame its housekeeping appends if it compacts (for a flush,
    /// or a tail that replay took past the threshold).
    pub frame_seq: u64,
    /// The payload CRC of the transaction's own frame ([`crate::Frame::crc`]); `None` when it
    /// queued nothing and so appended no frame of its own.
    pub frame_crc: Option<u32>,
    /// How many changesets it inserted.
    pub inserted: u64,
    /// How many ref moves it recorded.
    pub moved_refs: u64,
    /// Whether the commit also synced the index and moved `journal.ckpt` to the end of the
    /// journal: at 32 frames past the checkpoint, after a compaction, or for a flush with a
    /// frame past the checkpoint.
    pub checkpointed: bool,
}

impl CommitInfo {
    /// What a commit at frame `frame_seq` did: `inserted` changesets, `moved_refs` ref moves,
    /// and whether it `checkpointed`; no `frame_crc`, as for a commit with nothing queued.
    pub fn new(frame_seq: u64, inserted: u64, moved_refs: u64, checkpointed: bool) -> CommitInfo {
        CommitInfo {
            frame_seq,
            frame_crc: None,
            inserted,
            moved_refs,
            checkpointed,
        }
    }
}

/// What a write transaction's recovery repaired before the transaction's own work
/// ([`MetaWrite::recovered`]): what the writers before it left when they crashed, or failed after
/// their commit point. All zero ([`Recovered::is_empty`]) when they finished cleanly: frames past
/// the checkpoint that replay only reads again, as every write does, count for nothing.
///
/// It is made of what the recovery knows already, with no filesystem call of its own, and kept
/// nowhere: a recovery that fails part way leaves its repairs made and the report lost.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
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
#[non_exhaustive]
pub enum InsertOutcome {
    /// The changeset is queued; once committed it is index record `seq`.
    Inserted {
        /// Its place in the index, and among the journal's changesets.
        seq: u64,
    },
    /// The store holds the changeset already, or this transaction queued it already.
    AlreadyPresent,
}

/// What [`MetaWrite::put_fact`] expects of the facts the store holds before it queues one (the
/// FORMAT 3 design §2.8, the version CAS).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Expect {
    /// Nothing: the fact is queued whatever the store holds.
    Any,
    /// That the store and the transaction hold this many facts of the fact's kind about its
    /// subject: its version, for a kind whose facts are a subject's events (Review, Queue).
    Version(u32),
}

/// What [`MetaWrite::cas_ref`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
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

/// An expectation [`MetaWrite::expect_refs`] found false: the ref names another target.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RefMismatch {
    /// The ref.
    pub name: String,
    /// What the caller expected it to name; `None` for a ref expected not to exist.
    pub expected: Option<ChangesetId>,
    /// What it names; `None` if it does not exist.
    pub actual: Option<ChangesetId>,
}

impl RefMismatch {
    /// Ref `name`, expected at `expected`, found at `actual`.
    pub fn new(
        name: String,
        expected: Option<ChangesetId>,
        actual: Option<ChangesetId>,
    ) -> RefMismatch {
        RefMismatch {
            name,
            expected,
            actual,
        }
    }
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
#[non_exhaustive]
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

impl JournalEntry {
    /// A record of `kind` in frame `seq`, about `key`, at `ts`.
    pub fn new(seq: u64, kind: &'static str, key: Vec<u8>, ts: i64) -> JournalEntry {
        JournalEntry { seq, kind, key, ts }
    }
}

/// One move of a ref, as [`MetaRead::reflog`] lists them.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
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

impl RefMoveEntry {
    /// A move in frame `seq` from `old` to `new`, by `actor` at `ts`.
    pub fn new(
        seq: u64,
        old: Option<ChangesetId>,
        new: Option<ChangesetId>,
        actor: String,
        ts: i64,
    ) -> RefMoveEntry {
        RefMoveEntry {
            seq,
            old,
            new,
            actor,
            ts,
        }
    }
}

impl MetaStore {
    /// How many write transactions [`MetaStore::write`] and the leads of
    /// [`MetaStore::take_turn`] began in this process, by any store: a test compares two
    /// readings around the calls it counts. The test hooks alone keep the count.
    #[cfg(feature = "test-hooks")]
    pub fn write_calls() -> u64 {
        WRITE_CALLS.load(Ordering::Relaxed)
    }

    /// How many times [`MetaStore::read_committed`] was called in this process, by any store: a
    /// test compares two readings around the calls it counts. The test hooks alone keep the
    /// count.
    #[cfg(feature = "test-hooks")]
    pub fn read_committed_calls() -> u64 {
        READ_COMMITTED_CALLS.load(Ordering::Relaxed)
    }

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
    /// [`StoreError::Format`], and the next create removes what is left of `meta.tmp/`
    /// ([`FsOps::remove_dir_all`], so that the fault doubles reach it) and starts over. The look
    /// for it, and the removal of the emptied `meta.tmp/` after the rename, which is best effort,
    /// go through `std::fs`. Two creates of one store at once are not supported.
    pub fn create_with_fs(dir: &Utf8Path, fs: Arc<dyn FsOps>) -> Result<MetaStore, StoreError> {
        Self::create_in(Layout::new(dir), fs)
    }

    /// [`MetaStore::create_with_fs`] of a store of `layout`'s format, rooted at its root.
    pub fn create_in(layout: Layout, fs: Arc<dyn FsOps>) -> Result<MetaStore, StoreError> {
        // One on the real root, so that a failure on a staged file names it under meta.tmp/.
        let dir = layout.root().to_owned();
        let fs = StoreFs::new(fs, &dir);
        let meta = layout.meta_dir();
        if std::fs::exists(&meta).map_err(|e| fs.locate(FileOp::Stat, &meta, e))? {
            return Err(StoreError::Format(format!("{meta} exists already")));
        }
        let staging = dir.join(STAGING);
        if std::fs::exists(&staging).map_err(|e| fs.locate(FileOp::Stat, &staging, e))? {
            // What a create that crashed left behind: no part of a store yet.
            fs.remove_dir_all(staging.as_std_path())?;
        }
        let staged = Layout::with_format(staging.as_path(), layout.format());
        Journal::create_in(&staged, &fs)?.set_ckpt(0)?;
        Index::create_in(&staged, &fs)?;
        let staged_meta = staged.meta_dir();
        fs.create_dir_all(staged.refs_dir().as_std_path())?;
        fs.append(staged.meta_lock().as_std_path(), &[])?;
        // The journal and the index made their own entries durable; this makes the last two
        // durable before the rename publishes them.
        fs.fsync_dir(staged_meta.as_std_path())?;
        fs.rename_durable(staged_meta.as_std_path(), meta.as_std_path())?;
        // Best effort: the staging directory is empty now, and harmless if it stays.
        let _ = std::fs::remove_dir(&staging);
        Self::open_on(layout, fs)
    }

    /// Opens the metadata store of the store rooted at `dir` on the real filesystem: see
    /// [`MetaStore::open_with_fs`].
    pub fn open(dir: &Utf8Path) -> Result<MetaStore, StoreError> {
        Self::open_with_fs(dir, Arc::new(RealFs))
    }

    /// Opens the existing metadata store under `dir`, every change going through `fs` (a
    /// `CrashAtFs` in crash tests), in the format `<dir>/FORMAT` names (FORMAT 1 if there is no
    /// `FORMAT`: a metadata store made on its own, without a repository around it). Reads only
    /// that the format's journal (`meta/journal.log`) is there: [`StoreError::Format`] if it is
    /// not, or if `FORMAT` names no format this build reads.
    pub fn open_with_fs(dir: &Utf8Path, fs: Arc<dyn FsOps>) -> Result<MetaStore, StoreError> {
        let store_fs = StoreFs::new(fs, dir);
        let format = format::read_format_or_v1(&Layout::new(dir), &store_fs)?;
        Self::open_on(Layout::with_format(dir, format), store_fs)
    }

    /// [`MetaStore::open_with_fs`] in `layout`'s format, whatever `FORMAT` says: for a caller
    /// that read `FORMAT` itself, as a repository's open does.
    pub fn open_in(layout: Layout, fs: Arc<dyn FsOps>) -> Result<MetaStore, StoreError> {
        let store_fs = StoreFs::new(fs, layout.root());
        Self::open_on(layout, store_fs)
    }

    /// [`MetaStore::open_in`] through `fs`.
    fn open_on(layout: Layout, fs: StoreFs) -> Result<MetaStore, StoreError> {
        let log = layout.journal_log();
        if !std::fs::exists(&log).map_err(|e| fs.locate(FileOp::Stat, &log, e))? {
            return Err(StoreError::Format(format!(
                "{log} is missing: no metadata store here"
            )));
        }
        // A FORMAT 3 store that lists a feature this build lacks is refused before anything.
        features::check(&layout, &fs)?;
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

    /// Makes the write transactions sync the index files (their batched [`Index::sync`], and the
    /// sync a compaction starts with) from `threads` threads at once (0: one per available core),
    /// at most one per file. 6 unless set: FORMAT 3's six files at once. 1 syncs them one after
    /// another, in write order, so that a test counting [`FsOps`] calls (a crash sweep's
    /// `CrashAtFs`) meets the same call at the same count in every run.
    pub fn set_parallelism(&mut self, threads: usize) {
        self.parallelism = threads;
    }

    /// A snapshot of the metadata as committed now, taken without a lock in at most
    /// [`MetaStore::SNAPSHOT_ROUNDS`] rounds, or [`StoreError::SnapshotContended`] if the
    /// metadata changed during every one: see the module documentation.
    ///
    /// A FORMAT 3 store's feature list is read every round: one that lists a feature this build
    /// lacks is [`StoreError::TooNew`], and so is a frame of a kind this build does not know
    /// once the list, read again, names such a feature (the FORMAT 3 design §2.6).
    pub fn read(&self) -> Result<MetaRead<'_>, StoreError> {
        self.snapshot()
            .map_err(|e| features::too_new_or(&self.layout, &self.fs, e))
    }

    /// [`MetaStore::read`], its errors as they come.
    fn snapshot(&self) -> Result<MetaRead<'_>, StoreError> {
        let (ckpt, fact_rows) = self.ckpt_and_facts()?;
        let mut opened = None;
        for _ in 0..Self::SNAPSHOT_ROUNDS {
            features::check(&self.layout, &self.fs)?;
            // The index after the checkpoint, opened in the first round (and again after a round
            // whose open met a cut journal.off): everything before the checkpoint was in the
            // index files before the checkpoint moved past it. The frames past it, read once the
            // index files are measured, vouch for the index's records past it. A journal.off that
            // a writer cut as they were read takes the round.
            let (index, past) = if let Some(opened) = opened.take() {
                opened
            } else {
                let mut first = 0;
                let past = || {
                    let past = self.journal.tail_frames_from_unless_shrunk(ckpt)?;
                    let past = past.ok_or(NotOpened::Shrunk)?;
                    first = past.first;
                    Ok(past.frames)
                };
                match Index::open_with_past(&self.layout, self.fs.clone(), fact_rows, past) {
                    Ok((index, frames)) => (index, PastFrames { first, frames }),
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
                Some(again) if again == past => {
                    let PastFrames { first, frames } = past;
                    let overlay = Overlay::new(&index, &frames, files)?;
                    let next_seq = frames.last().map_or(first, |frame| frame.seq + 1);
                    return Ok(MetaRead {
                        store: self,
                        ckpt,
                        next_seq,
                        index,
                        frames,
                        overlay,
                    });
                }
                Some(again) => opened = Some((index, again)),
                None => opened = Some((index, past)),
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
    /// cannot promise (storage spec §3, "읽기"). Local readers keep the lock-free one, but for
    /// the operation log (`Repo::op_log`): `op undo` takes the numbers it lists, so a number it
    /// lists must never be given to another operation by a take-back.
    ///
    /// It costs a wait behind the writer that holds the lock: about one transaction, longer under
    /// contention. The lock is held for the tail check and the snapshot only, not while the caller
    /// reads what the snapshot names. It repairs no index and replays nothing: the snapshot's
    /// overlay supplies the frames past the checkpoint. On a clean tail it writes nothing; a tail
    /// that needs a repair it repairs as the next writer would, and it fails where that writer's
    /// tail check fails ([`MetaStore::write`]). Before any of that it opens `meta/lock` for
    /// writing, as a writer does (making it if it is missing): on a read-only filesystem it fails
    /// there, a [`StoreError::Io`] at `open meta/lock`, whatever the tail.
    ///
    /// A tail check that cannot list a frame, or write afresh the frames whose sync failed, is
    /// [`StoreError::NotCommitted`], as for a writer: here no snapshot is taken, nothing more is
    /// listed, and the next committed read lists the frame. A cut that fails too aborts the
    /// process, as a writer's does ([`Journal::repair_tail`]).
    pub fn read_committed(&self) -> Result<(MetaRead<'_>, TailRepair), StoreError> {
        #[cfg(feature = "test-hooks")]
        READ_COMMITTED_CALLS.fetch_add(1, Ordering::Relaxed);
        let _lock = MetaLock::acquire(&self.layout, self.lock_timeout)?;
        // Before the tail check, which may write: a store too new is left as it is.
        features::check(&self.layout, &self.fs)?;
        let repair = (self.journal.repair_tail())
            .map_err(|e| features::too_new_or(&self.layout, &self.fs, e))?;
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
        self.write_locked(lock)
    }

    /// [`MetaStore::write`] once `lock` is held: the recovery, then the transaction. A write
    /// queue's lead ([`MetaStore::take_turn`]) begins here too, so the lock probe
    /// (`MetaStore::write_calls`) counts each plain write and each lead once.
    ///
    /// A FORMAT 3 store that lists a feature this build lacks is [`StoreError::TooNew`] before
    /// the tail check, which may write (the M20 plan's A3: a lead begins here too); so is a frame
    /// of a kind this build does not know once the list, read again, names one.
    pub(crate) fn write_locked(&self, lock: MetaLock) -> Result<MetaWrite<'_>, StoreError> {
        #[cfg(feature = "test-hooks")]
        WRITE_CALLS.fetch_add(1, Ordering::Relaxed);
        features::check(&self.layout, &self.fs)?;
        self.recover(lock)
            .map_err(|e| features::too_new_or(&self.layout, &self.fs, e))
    }

    /// The recovery and the transaction of [`MetaStore::write_locked`], once the feature list is
    /// checked.
    fn recover(&self, lock: MetaLock) -> Result<MetaWrite<'_>, StoreError> {
        let tail = self.journal.repair_tail()?;
        let (ckpt, fact_rows) = self.ckpt_and_facts()?;
        // The frames past the checkpoint vouch for the index's records past it, and are
        // replayed.
        let past = || self.journal.tail_frames_from(ckpt);
        let (mut index, frames) =
            Index::open_with_past(&self.layout, self.fs.clone(), fact_rows, past)?;
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
        // The first frame's nonce, drawn as it opens (the M20 plan's A2).
        let nonce = self.draw_nonce()?;
        Ok(MetaWrite {
            store: self,
            checkpointed_records: index.len().saturating_sub(bytes::to_u64(past)),
            checkpointed_facts: fact_rows,
            index,
            uncheckpointed: bytes::to_u64(frames.len()),
            records: Vec::new(),
            closed: Vec::new(),
            nonce,
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
            lock: Some(lock),
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
        self.write()?.finish(Settle::Flush).map(drop)
    }

    /// One step of a FORMAT 3 store's view work (the FORMAT 3 design §2.5 and its amendment of
    /// 2026-11-11, G6, G13): with more than [`crate::RUN_MIN`] changeset records past the runs that
    /// the checkpoint has passed, it builds their run, merges it at [`crate::RUN_FANOUT`] and
    /// publishes the new manifest, under `meta/views.lock` and never under [`MetaLock`].
    /// [`Step::Skipped`] at once if another step holds `meta/views.lock`, and below FORMAT 3,
    /// which has no runs; [`Step::Done`] otherwise. A feature this build does not support,
    /// listed since the store was opened, is [`StoreError::TooNew`] before anything is written.
    ///
    /// A commit leaves this work to its caller, who calls it once it has answered (a CLI after
    /// it wrote its response, a server after it sent one), or to gc; only a commit that leaves
    /// more than [`crate::RUN_BACKLOG_MAX`] records past the runs steps itself, after it
    /// unlocked. A step that fails part way leaves the runs a reader reads (the old manifest or
    /// the new one), and the next step finishes: its error is no failure of the commit before it.
    pub fn step_views(&self) -> Result<Step, StoreError> {
        views::step(&self.layout, &self.fs, &self.journal)
    }

    /// The step a commit that left `backlog` records past the runs owes, once it unlocked: one,
    /// past [`RUN_BACKLOG_MAX`]. Its error is dropped: the commit stands, and the next step, by a
    /// caller, gc or the next commit past the bound, does the work (the FORMAT 3 design's
    /// amendment of 2026-11-11, G12).
    pub(super) fn step_past(&self, backlog: u64) {
        if backlog > RUN_BACKLOG_MAX {
            let _ = self.step_views();
        }
    }

    /// The store's format.
    pub fn format(&self) -> StoreFormat {
        self.layout.format()
    }

    /// Refuses a ref name this store's format does not take ([`refs::check_ref_name_in`]).
    pub fn check_ref_name(&self, name: &str) -> Result<(), StoreError> {
        refs::check_ref_name_in(self.layout.format(), name)
    }

    /// How long a writer waits for [`MetaLock`] ([`MetaStore::with_lock_timeout`]).
    pub(super) fn lock_timeout(&self) -> Duration {
        self.lock_timeout
    }

    /// A new frame's nonce in a FORMAT 3 store ([`features::draw_nonce`]); `None` in the others.
    fn draw_nonce(&self) -> Result<Option<u64>, StoreError> {
        match self.layout.format() {
            StoreFormat::V3 => features::draw_nonce().map(Some),
            _ => Ok(None),
        }
    }

    /// The most records a frame of this store's holds of a caller's: in FORMAT 3 the nonce takes
    /// one of the 65,535 places.
    fn max_records(&self) -> usize {
        match self.layout.format() {
            StoreFormat::V3 => MAX_RECORDS_V3,
            _ => MAX_RECORDS,
        }
    }

    /// The checkpoint's offset and, in FORMAT 3, the fact rows it passed, read together from
    /// `journal3.ckpt` (0 rows in the other formats).
    fn ckpt_and_facts(&self) -> Result<(u64, u64), StoreError> {
        match self.layout.format() {
            StoreFormat::V3 => {
                let ckpt = self.journal.ckpt3()?;
                Ok((ckpt.offset, ckpt.fact_rows))
            }
            _ => Ok((self.journal.ckpt()?, 0)),
        }
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
#[non_exhaustive]
pub struct MetaRead<'a> {
    store: &'a MetaStore,
    /// The checkpoint the snapshot was taken at: `frames` start there.
    ckpt: u64,
    /// The seq of the first frame the snapshot does not hold.
    next_seq: u64,
    index: Index,
    /// The frames past the checkpoint, in journal order.
    frames: Vec<Frame>,
    overlay: Overlay,
}

impl MetaRead<'_> {
    /// Changeset `id`: an index lookup (a search of the sorted view, then the tail), then the
    /// frames past the checkpoint. The record the index finds must hold `id`, and its parents
    /// must start where the record before it leaves off in `parents.log`; else the index is
    /// damaged, or crafted, and the lookup fails with [`StoreError::IndexCorrupt`] rather than
    /// answer with another changeset's record or another record's parents. The parents are what
    /// the index holds there all the same: only the changeset's object, which the store does not
    /// read, names them for certain, by their hashes.
    pub fn changeset(&self, id: ChangesetId) -> Result<Option<ChangesetMeta>, StoreError> {
        Ok(self
            .index
            .lookup(id)?
            .or_else(|| self.overlay.changeset(id).cloned()))
    }

    /// The first parent of changeset `id`, as [`MetaRead::changeset`] gives it, without the
    /// rest of its record: `None` if the snapshot does not hold `id`, `Some(None)` for a root.
    /// The same index lookup, then the record's `par.col` entry, with the one before it, and its
    /// first parent in `parents.log` ([`Index::first_parent`]), with the same checks and the same
    /// [`StoreError::IndexCorrupt`]; or the frames past the checkpoint.
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

    /// Ref `name`; `None` if it does not exist. [`StoreError::RefName`] for a name no format
    /// takes, [`StoreError::FormatRequired`] for one only a later format takes.
    pub fn get_ref(&self, name: &str) -> Result<Option<RefState>, StoreError> {
        self.store.check_ref_name(name)?;
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
    /// [`MetaRead::reflog_page`] with no `before`.
    pub fn reflog(&self, name: &str, limit: usize) -> Result<Vec<RefMoveEntry>, StoreError> {
        self.reflog_page(name, None, limit)
    }

    /// Up to `limit` moves of ref `name` recorded in frames whose seq is below `before` (all
    /// for `None`), newest first: [`MetaRead::reflog`] a page at a time, `before` the seq of
    /// the last move of the page before. The frames past the checkpoint, then those before it
    /// from `journal.off`, 256 at a time, as [`MetaRead::frames_before`] reads them.
    pub fn reflog_page(
        &self,
        name: &str,
        before: Option<u64>,
        limit: usize,
    ) -> Result<Vec<RefMoveEntry>, StoreError> {
        self.store.check_ref_name(name)?;
        let below = before.unwrap_or(u64::MAX);

        // 1. The frames past the checkpoint, below `before`.
        let mut log = Vec::new();
        let past = self.frames.iter().rev().filter(|frame| frame.seq < below);
        collect_moves(past, name, limit, &mut log);

        // 2. Then the frames before the checkpoint, a batch at a time; with no frames past the
        // checkpoint, frames listed since the snapshot was taken are skipped.
        let mut end = match self.frames.first() {
            Some(frame) => frame.seq,
            None => self.store.journal.frame_count()?,
        }
        .min(below);
        while log.len() < limit && end > 0 {
            let start = end.saturating_sub(REFLOG_BATCH);
            let batch = self
                .store
                .journal
                .frames_from(start, to_usize(end - start))?;
            let before_ckpt = batch.iter().rev().filter(|frame| frame.offset < self.ckpt);
            collect_moves(before_ckpt, name, limit, &mut log);
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

    /// Up to `limit` of the snapshot's frames whose seq is below `before` (from its newest for
    /// `None`), newest first, each whole: the operation log (storage spec §3). The frames past
    /// the checkpoint come from the snapshot itself, then those before it from `journal.off`,
    /// 256 at a time, as [`MetaRead::reflog`] reads them. The snapshot ends at its own newest
    /// frame: a frame committed after it was taken is never among them.
    pub fn frames_before(
        &self,
        before: Option<u64>,
        limit: usize,
    ) -> Result<Vec<Frame>, StoreError> {
        let below = before.unwrap_or(u64::MAX);
        let mut frames: Vec<Frame> = self
            .frames
            .iter()
            .rev()
            .filter(|frame| frame.seq < below)
            .take(limit)
            .cloned()
            .collect();
        if frames.len() == limit {
            return Ok(frames);
        }
        // Then the frames before the checkpoint, a batch at a time; with no frames past the
        // checkpoint, frames listed since the snapshot was taken are skipped.
        let mut end = match self.frames.first() {
            Some(frame) => frame.seq,
            None => self.store.journal.frame_count()?,
        }
        .min(below);
        while frames.len() < limit && end > 0 {
            let start = end.saturating_sub(REFLOG_BATCH);
            let batch = self
                .store
                .journal
                .frames_from(start, to_usize(end - start))?;
            let room = limit - frames.len();
            let before_ckpt = batch
                .into_iter()
                .rev()
                .filter(|frame| frame.offset < self.ckpt);
            frames.extend(before_ckpt.take(room));
            end = start;
        }
        Ok(frames)
    }

    /// Frame `seq` of the snapshot, whole: from the snapshot itself past the checkpoint, from
    /// `journal.off` before it. `None` past the snapshot's newest frame, also once later frames
    /// are committed.
    pub fn frame(&self, seq: u64) -> Result<Option<Frame>, StoreError> {
        if let Some(first) = self.frames.first()
            && seq >= first.seq
        {
            return Ok(self.frames.iter().find(|frame| frame.seq == seq).cloned());
        }
        let listed = self.store.journal.frames_from(seq, 1)?;
        Ok(listed.into_iter().find(|frame| frame.offset < self.ckpt))
    }

    /// The seq of the first frame the snapshot does not hold (the M16 design §6): the last seq it
    /// read past the checkpoint + 1, else the number of frames `journal.off` listed before the
    /// checkpoint when it was taken. It reads nothing.
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// The facts about `subject`, in journal order (the FORMAT 3 design §2.8): the fact index's
    /// (a search of each fact run, then the rows past them; one `read_at` of the journal per fact
    /// found, checked against its row and `subject`), then the frames past the checkpoint. None
    /// below FORMAT 3. [`StoreError::IndexCorrupt`] for a run or a row that names another fact.
    pub fn facts_by_subject(&self, subject: [u8; 32]) -> Result<Vec<Fact>, StoreError> {
        self.facts_by(Side::Subject, subject)
    }

    /// The facts whose other is `other`, in journal order, as [`MetaRead::facts_by_subject`]
    /// finds them. All zero names no other: no fact is found by it.
    pub fn facts_by_other(&self, other: [u8; 32]) -> Result<Vec<Fact>, StoreError> {
        if other == [0; 32] {
            return Ok(Vec::new());
        }
        self.facts_by(Side::Other, other)
    }

    /// The facts whose `side` is `key`: the index's, then the overlay's.
    fn facts_by(&self, side: Side, key: [u8; 32]) -> Result<Vec<Fact>, StoreError> {
        let mut found = self.index.find_facts(side, &key)?;
        let past = self.overlay.facts().iter();
        found.extend(past.filter(|fact| side.key_of(fact) == key).cloned());
        Ok(found)
    }

    /// How many facts this snapshot's lookups have read from the journal.
    #[cfg(test)]
    pub(crate) fn fact_reads(&self) -> u64 {
        self.index.fact_reads()
    }

    /// How many changesets the store holds.
    pub fn count_changesets(&self) -> Result<u64, StoreError> {
        Ok(self.index.len() + bytes::to_u64(self.overlay.extras().len()))
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
#[non_exhaustive]
pub struct MetaWrite<'a> {
    store: &'a MetaStore,
    /// Up to date with every committed frame: [`MetaStore::write`] replayed them.
    index: Index,
    /// How many of the index's records lie before the checkpoint: synced before it moved
    /// past them, so a failed sync never cuts them.
    checkpointed_records: u64,
    /// How many fact rows the checkpoint passed (FORMAT 3; 0 in the others).
    checkpointed_facts: u64,
    /// How many frames lie past the checkpoint, not counting this transaction's.
    uncheckpointed: u64,
    /// The records queued, in order: the frames [`MetaWrite::commit_frames`] appends, the closed
    /// ones first, then the open one.
    records: Vec<Record>,
    /// The frames closed so far ([`MetaWrite::begin_frame`]), in order: the open frame holds the
    /// records after the last of them.
    closed: Vec<Closed>,
    /// The open frame's nonce (FORMAT 3; `None` in the others): drawn when the frame opened, and
    /// kept by [`MetaWrite::drop_frame`], since no frame holding it was written.
    nonce: Option<u64>,
    /// The generation of each changeset queued.
    queued: HashMap<ChangesetId, u64>,
    /// The target each ref moved so far is left at.
    moved: HashMap<String, Option<ChangesetId>>,
    /// What [`MetaStore::write`] repaired before it returned this transaction.
    recovered: Recovered,
    /// Held until the transaction ends, unless a write queue's lead took it to hold past the
    /// commit, for its answers ([`MetaWrite::take_lock`]).
    lock: Option<MetaLock>,
}

impl MetaWrite<'_> {
    /// What the recovery that opened this transaction repaired ([`MetaStore::write`]): what the
    /// writers before it left when they crashed or failed after their commit point. All zero
    /// when they finished cleanly. It reads nothing.
    pub fn recovered(&self) -> Recovered {
        self.recovered
    }

    /// The nonce the open frame will end in, in a FORMAT 3 store (the FORMAT 3 design §2.7):
    /// each frame of the transaction has its own, drawn when it opened. `None` below FORMAT 3.
    pub fn nonce(&self) -> Option<u64> {
        self.nonce
    }

    /// Ref `name` as this transaction compares against it ([`MetaWrite::cas_ref`]): the target
    /// this transaction's last queued move of it leaves, or else what its file says, every
    /// committed frame replayed into it by [`MetaStore::write`]; `None` if the ref does not
    /// exist. Read under the lock, so no other writer moves the ref before this transaction is
    /// committed or dropped. [`StoreError::RefName`] for a name no format takes,
    /// [`StoreError::FormatRequired`] for one only a later format takes.
    pub fn get_ref(&self, name: &str) -> Result<Option<ChangesetId>, StoreError> {
        self.store.check_ref_name(name)?;
        self.current(name)
    }

    /// Changeset `id` as this transaction sees it: one it queued
    /// ([`MetaWrite::insert_changeset`]), as the index will hold it once committed (at the seq
    /// the insert returned, with the generation, delta hash and parents queued); or else the
    /// index's record, every committed changeset replayed into the index by
    /// [`MetaStore::write`], found and checked as [`MetaRead::changeset`] finds and checks it;
    /// `None` if neither holds it. Read under the lock.
    pub fn changeset(&self, id: ChangesetId) -> Result<Option<ChangesetMeta>, StoreError> {
        if self.queued.contains_key(&id) {
            let queued = self.records.iter().filter_map(|record| match record {
                Record::Changeset {
                    id,
                    generation,
                    delta_hash,
                    parents,
                } => Some((id, generation, delta_hash, parents)),
                Record::RefMove { .. } | Record::Compacted { .. } | Record::Fact(_) => None,
            });
            // The queued changesets take the seqs after the index's, in the queue's order.
            for (at, (queued_id, generation, delta_hash, parents)) in queued.enumerate() {
                if *queued_id == id {
                    return Ok(Some(ChangesetMeta {
                        id,
                        seq: self.index.len() + bytes::to_u64(at),
                        generation: *generation,
                        delta_hash: *delta_hash,
                        parents: parents.clone(),
                    }));
                }
            }
        }
        self.index.lookup(id)
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
                max: bytes::to_u64(MAX_PARENTS),
                got: bytes::to_u64(meta.parents.len()),
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
        let seq = self.index.len() + bytes::to_u64(self.queued.len() + staged.generations.len());
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
        let records = self.open_len() + staged.records.len();
        let max = self.store.max_records();
        if records > max {
            return Err(StoreError::Limit {
                what: LimitKind::Records,
                max: bytes::to_u64(max),
                got: bytes::to_u64(records),
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
    /// A name no format takes is [`StoreError::RefName`], one only a later format takes
    /// [`StoreError::FormatRequired`]; a move its kind does not allow, after the compare: a
    /// deletion of `main`, [`StoreError::RefProtected`], or a tag's from one target to another,
    /// [`StoreError::TagImmutable`]; a `new` that is neither in the store nor queued,
    /// [`StoreError::UnknownRefTarget`]; an actor longer than 255 bytes, or a move that would
    /// not fit in the transaction's frame (65,535 records), [`StoreError::Limit`].
    pub fn cas_ref(
        &mut self,
        name: &str,
        expected: Option<ChangesetId>,
        new: Option<ChangesetId>,
        actor: &str,
    ) -> Result<CasResult, StoreError> {
        self.store.check_ref_name(name)?;
        if actor.len() > MAX_ACTOR_LEN {
            return Err(StoreError::Limit {
                what: LimitKind::ActorBytes,
                max: bytes::to_u64(MAX_ACTOR_LEN),
                got: bytes::to_u64(actor.len()),
            });
        }
        if let Some(target) = new
            && self.generation(target)?.is_none()
        {
            return Err(StoreError::UnknownRefTarget(target));
        }
        let current = self.current(name)?;
        if current != expected {
            return Ok(CasResult::Mismatch { actual: current });
        }
        refs::check_move(self.store.layout.format(), name, current, new)?;
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

    /// Every expectation of `expected` (a ref and the target it should name; `None` for absent)
    /// that is false, in order: each compared as [`MetaWrite::cas_ref`] compares, against the
    /// last move this transaction queued of the ref or else its file, under the lock. It queues
    /// nothing, so a caller checks every ref of a multi-ref move before it queues any (the M18
    /// design §7). A name no format takes is [`StoreError::RefName`], one only a later format
    /// takes [`StoreError::FormatRequired`].
    pub fn expect_refs(
        &self,
        expected: &[(&str, Option<ChangesetId>)],
    ) -> Result<Vec<RefMismatch>, StoreError> {
        let mut mismatches = Vec::new();
        for &(name, want) in expected {
            let actual = self.get_ref(name)?;
            if actual != want {
                mismatches.push(RefMismatch::new(name.to_owned(), want, actual));
            }
        }
        Ok(mismatches)
    }

    /// Queues `fact` (the FORMAT 3 design §2.8), if the store holds what `expect` says: with
    /// [`Expect::Version`], as many facts of its kind about its subject as the version, the
    /// committed ones (the fact index under the lock) and the ones this transaction queued
    /// counted together. Otherwise [`CasResult::Mismatch`] (its `actual` `None`: a fact names no
    /// changeset), and nothing is queued: the version CAS. [`CasResult::Applied`] (`old` `None`)
    /// when queued; its feature is listed before its frame is appended (W10).
    ///
    /// Refused before anything is queued or written: on a FORMAT 1 or 2 store,
    /// [`StoreError::FormatRequired`] naming its kind's feature (`needs` 3); a kind whose feature
    /// this build does not support, or a blob it names that the store does not hold (W9: every
    /// blob a fact names is durable before its frame), [`StoreError::BadCall`]; a frame with no
    /// room left, [`StoreError::Limit`].
    pub fn put_fact(&mut self, fact: Fact, expect: Expect) -> Result<CasResult, StoreError> {
        // 1. A FORMAT 3 record of a kind this build gives a meaning.
        let kind = fact.kind();
        let format = self.store.layout.format();
        if format != StoreFormat::V3 {
            return Err(StoreError::FormatRequired {
                name: features::feature_of(kind).to_owned(),
                needs: StoreFormat::V3.version(),
                store: format.version(),
            });
        }
        if !features::known_kinds().contains(&kind) {
            return Err(StoreError::BadCall(format!(
                "a {kind:?} fact needs the feature {:?}, which this build does not support",
                features::feature_of(kind)
            )));
        }

        // 2. The version CAS, under the lock.
        if let Expect::Version(version) = expect
            && self.fact_count(fact.subject(), kind)? != u64::from(version)
        {
            return Ok(CasResult::Mismatch { actual: None });
        }

        // 3. Its blobs durable (W9), and room in the frame.
        for id in fact.blob_ids() {
            let path = self.store.layout.object_path(id);
            if !self.store.fs.exists(path.as_std_path())? {
                return Err(StoreError::BadCall(format!(
                    "a {kind:?} fact names blob {}, which the store does not hold",
                    id.to_hex()
                )));
            }
        }
        self.check_room()?;
        self.records.push(Record::Fact(fact));
        Ok(CasResult::Applied { old: None })
    }

    /// How many facts of `kind` about `subject` the store and this transaction hold.
    fn fact_count(&self, subject: [u8; 32], kind: FactKind) -> Result<u64, StoreError> {
        let held = self.index.find_facts(Side::Subject, &subject)?;
        let queued = self.records.iter().filter_map(|record| match record {
            Record::Fact(fact) => Some(fact),
            _ => None,
        });
        let count = (held.iter().chain(queued))
            .filter(|fact| fact.kind() == kind && fact.subject() == subject)
            .count();
        Ok(bytes::to_u64(count))
    }

    /// The committed frames whose seq is `first` or greater, in journal order: a snapshot's
    /// [`MetaRead::next_seq`] gives those it did not hold (the M16 design §6). Read under the
    /// lock, after [`MetaStore::write`]'s tail check, so every whole frame is listed and no other
    /// writer appends one meanwhile; it reads [`Journal::frames_from`] and writes nothing.
    ///
    /// Then this transaction's closed frames ([`MetaWrite::begin_frame`]) whose seq is `first` or
    /// greater, each with the seq, offset and payload CRC it takes once committed: so a frame of a
    /// batch sees the frames before it as if they were committed (the M18 D2 design §3.3, D12).
    /// The open frame is not among them.
    pub fn frames_from(&self, first: u64) -> Result<Vec<Frame>, StoreError> {
        let journal = &self.store.journal;
        let mut frames = journal.frames_from(first, usize::MAX)?;
        if self.closed.is_empty() {
            return Ok(frames);
        }

        // The closed frames follow the journal's last, in order.
        let mut offset = journal.len();
        let mut start = 0;
        for (seq, closed) in (journal.frame_count()?..).zip(&self.closed) {
            if seq >= first {
                frames.push(Frame {
                    seq,
                    offset,
                    records: self.records[start..closed.end].to_vec(),
                    crc: closed.crc,
                    nonce: closed.nonce,
                });
            }
            offset += closed.len;
            start = closed.end;
        }
        Ok(frames)
    }

    /// Closes the open frame, if it holds any record, and opens the next: what is queued from
    /// now on goes to a frame of its own, which sees every record queued before it (the M18 D2
    /// design §3.3). A transaction that never calls it commits one frame. A closed frame is
    /// encoded, so that [`MetaWrite::frames_from`] gives it with its payload CRC; records that
    /// no frame holds are [`StoreError::Limit`], as [`MetaWrite::commit`] would find them.
    pub fn begin_frame(&mut self) -> Result<(), StoreError> {
        self.close_frame()
    }

    /// Discards what the open frame queued: its changesets and its ref moves, each ref left as
    /// the frames before it leave it. The closed frames stay.
    pub fn drop_frame(&mut self) {
        let start = self.open_start();
        self.forget_from(start);
    }

    /// Closes the open frame, if it holds any record, then keeps the first `n` frames and
    /// discards the rest, as [`MetaWrite::drop_frame`] discards one; `n` past the frames there
    /// are keeps them all. Errors as [`MetaWrite::begin_frame`].
    pub fn keep_frames(&mut self, n: usize) -> Result<(), StoreError> {
        self.close_frame()?;
        if n >= self.closed.len() {
            return Ok(());
        }
        let at = n.checked_sub(1).map_or(0, |last| self.closed[last].end);
        self.closed.truncate(n);
        self.forget_from(at);
        Ok(())
    }

    /// The lock, taken out of the transaction so that a write queue's lead holds it past the
    /// commit while it answers its members (the M18 D2 design §3.1, step 14).
    pub(super) fn take_lock(&mut self) -> Option<MetaLock> {
        self.lock.take()
    }

    /// How many records of a domain a FORMAT 3 store holds past its runs once this transaction
    /// commits, the larger of the two: the index's tail and the changesets queued, or the fact
    /// rows' tail and the facts queued (0 below FORMAT 3, which compacts).
    pub(super) fn backlog(&self) -> u64 {
        if self.store.layout.format() != StoreFormat::V3 {
            return 0;
        }
        let queued = |of: fn(&Record) -> bool| {
            bytes::to_u64(self.records.iter().filter(|record| of(record)).count())
        };
        let changesets =
            self.index.tail_len() + queued(|record| matches!(record, Record::Changeset { .. }));
        let facts = self.index.fact_tail() + queued(|record| matches!(record, Record::Fact(_)));
        changesets.max(facts)
    }

    /// How many frames are closed ([`MetaWrite::begin_frame`]).
    pub(super) fn closed_frames(&self) -> usize {
        self.closed.len()
    }

    /// The seq the transaction's first frame takes: the journal's frame count.
    pub(super) fn next_seq(&self) -> Result<u64, StoreError> {
        self.store.journal.frame_count()
    }

    /// Up to `limit` of the journal's listed frames from seq `first` on, this transaction's not
    /// among them.
    pub(super) fn listed_frames(&self, first: u64, limit: usize) -> Result<Vec<Frame>, StoreError> {
        self.store.journal.frames_from(first, limit)
    }

    /// The records this transaction has queued, in order: the frames [`MetaWrite::commit`]
    /// appends, each ref move with the time it records. A refused move queued none. It reads
    /// nothing.
    pub fn queued(&self) -> &[Record] {
        &self.records
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
        // One info at least: the frame's, or the empty transaction's.
        self.finish(Settle::Batched).map(|infos| infos[0])
    }

    /// Commits every frame of the transaction ([`MetaWrite::begin_frame`]) as a batch, with one
    /// [`Journal::append_frames`]: the commit point of all of them. Then it materializes them
    /// once (each moved ref's file replaced once, with the target the last move leaves) and
    /// settles as [`MetaWrite::commit`] does, and unlocks. Returns what each frame committed, in
    /// order; with no frame, the one [`CommitInfo`] of [`MetaWrite::commit`]'s empty
    /// transaction. [`MetaWrite::commit`] is this of one frame.
    ///
    /// Errors as [`MetaWrite::commit`]'s, for every frame at once: a failed append of the frames
    /// is [`StoreError::NotCommitted`], none of them committed; once they are durable, every
    /// error is [`StoreError::PostCommit`] at the first frame's seq.
    pub fn commit_frames(self) -> Result<Vec<CommitInfo>, StoreError> {
        self.finish(Settle::Batched)
    }

    /// [`MetaWrite::commit_frames`]; [`Settle::Flush`] also syncs and checkpoints any frame past
    /// the checkpoint. Then it unlocks, and a commit that leaves a FORMAT 3 store more than
    /// [`RUN_BACKLOG_MAX`] records past its runs steps the view work once
    /// ([`MetaStore::step_past`]): only while it still holds the lock, since a write queue's lead
    /// took it out to answer its members, and steps itself after them (the FORMAT 3 design's
    /// amendment of 2026-11-11, G12; lock order §2.5 rule 8).
    fn finish(mut self, settle: Settle) -> Result<Vec<CommitInfo>, StoreError> {
        let backlog = self.backlog();
        let committed = self.commit_all(settle);
        let Some(lock) = self.lock.take() else {
            return committed;
        };
        drop(lock);
        if committed.is_ok() {
            self.store.step_past(backlog);
        }
        committed
    }

    /// The commit of [`MetaWrite::finish`], under the lock.
    fn commit_all(&mut self, settle: Settle) -> Result<Vec<CommitInfo>, StoreError> {
        let store = self.store;

        // 1. The frames: the closed ones, then the open one if it holds any record.
        let mut start = 0;
        let mut slices = Vec::with_capacity(self.closed.len() + 1);
        for closed in &self.closed {
            slices.push((&self.records[start..closed.end], closed.nonce));
            start = closed.end;
        }
        if start < self.records.len() {
            slices.push((&self.records[start..], self.nonce));
        }
        if slices.is_empty() {
            let info = CommitInfo {
                frame_seq: store.journal.frame_count()?,
                frame_crc: None,
                inserted: 0,
                moved_refs: 0,
                checkpointed: false,
            };
            let checkpointed = self.settle(settle)?;
            return Ok(vec![CommitInfo {
                checkpointed,
                ..info
            }]);
        }

        // 2. The commit point of every frame.
        let frames = store.journal.append_frames_in(&slices)?;
        self.uncheckpointed += bytes::to_u64(frames.len());
        let mut infos: Vec<CommitInfo> = frames.iter().map(commit_info).collect();

        // 3. One materialization of them all, then the batched durability.
        let first = &frames[0];
        replay::materialize(&mut self.index, &store.layout, &store.fs, &frames)
            .and_then(|()| self.settle(settle))
            .map(|checkpointed| {
                for info in &mut infos {
                    info.checkpointed = checkpointed;
                }
                infos
            })
            .map_err(|source| StoreError::PostCommit {
                frame_seq: first.seq,
                frame_crc: first.crc,
                source: Box::new(source),
            })
    }

    /// The batched durability of the projections, once the frames are materialized: with the
    /// tail past [`Index::COMPACT_THRESHOLD`], an index sync, a compaction and its `Compacted`
    /// frame; else, at 32 frames past the checkpoint or for a flush with any, an index sync.
    /// Then the checkpoint moves to the end of the journal; a failed sync or compaction
    /// returns first, so it never moves past what is not durable, and a failed sync cuts the
    /// index back to the checkpoint first ([`MetaWrite::sync_index`]). Returns whether the
    /// checkpoint moved.
    ///
    /// A FORMAT 3 writer never compacts, nor appends a `Compacted` frame (the FORMAT 3 design §0
    /// 3, §2.7): its checkpoint records the index records and the fact rows the sync passed
    /// ([`Journal::set_ckpt3`]). It syncs and checkpoints also once more than [`RUN_MIN`]
    /// records or rows lie past the checkpoint, so that a step can build their run (§2.5, the
    /// checkpoint's cadence).
    fn settle(&mut self, settle: Settle) -> Result<bool, StoreError> {
        let v3 = self.store.layout.format() == StoreFormat::V3;
        let compact = !v3 && self.index.tail_len() > Index::COMPACT_THRESHOLD;
        let flush = settle == Settle::Flush;
        let unsynced = self.index.len().saturating_sub(self.checkpointed_records);
        let unsynced_facts = self
            .index
            .fact_rows()
            .saturating_sub(self.checkpointed_facts);
        let due = self.uncheckpointed >= CHECKPOINT_EVERY
            || (flush && self.uncheckpointed > 0)
            || (v3 && unsynced.max(unsynced_facts) > RUN_MIN);
        if !compact && !due {
            return Ok(false);
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
        if v3 {
            journal.set_ckpt3(journal.len(), self.index.len(), self.index.fact_rows())?;
        } else {
            journal.set_ckpt(journal.len())?;
        }
        Ok(true)
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

    /// Refuses, with [`StoreError::Limit`], a record past what the open frame holds.
    fn check_room(&self) -> Result<(), StoreError> {
        let max = self.store.max_records();
        if self.open_len() < max {
            Ok(())
        } else {
            Err(StoreError::Limit {
                what: LimitKind::Records,
                max: bytes::to_u64(max),
                got: bytes::to_u64(self.open_len()) + 1,
            })
        }
    }

    /// Where the open frame's records start in `records`.
    fn open_start(&self) -> usize {
        self.closed.last().map_or(0, |closed| closed.end)
    }

    /// How many records the open frame holds.
    fn open_len(&self) -> usize {
        self.records.len() - self.open_start()
    }

    /// Closes the open frame, if it holds any record: encodes it, with its nonce in FORMAT 3, so
    /// that its payload CRC and length are final ([`MetaWrite::frames_from`]); then the next
    /// frame opens with a nonce of its own (the M20 plan's A2).
    fn close_frame(&mut self) -> Result<(), StoreError> {
        let start = self.open_start();
        if start == self.records.len() {
            return Ok(());
        }
        let format = self.store.layout.format();
        let records = &self.records[start..];
        let bytes = frame::encode_frame_with_nonce(format, records, self.nonce, 0, 0)?;
        let next = self.store.draw_nonce()?;
        self.closed.push(Closed {
            end: self.records.len(),
            len: bytes::to_u64(bytes.len()),
            crc: frame::payload_crc(&bytes),
            nonce: self.nonce,
        });
        self.nonce = next;
        Ok(())
    }

    /// Forgets the records from `at` on: the changesets they queue and the moves they make.
    /// What each ref is left at is folded again from the records kept.
    fn forget_from(&mut self, at: usize) {
        for record in self.records.drain(at..) {
            if let Record::Changeset { id, .. } = record {
                self.queued.remove(&id);
            }
        }
        self.moved.clear();
        for record in &self.records {
            if let Record::RefMove { name, new, .. } = record {
                self.moved.insert(name.clone(), *new);
            }
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

    /// What ref `name` (a valid name) names for this transaction: the target its last queued
    /// move leaves, or else what the ref's file says.
    fn current(&self, name: &str) -> Result<Option<ChangesetId>, StoreError> {
        match self.moved.get(name) {
            Some(&target) => Ok(target),
            None => {
                Ok(refs::read(&self.store.layout, &self.store.fs, name)?.map(|state| state.target))
            }
        }
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

/// A frame of a write transaction, closed ([`MetaWrite::begin_frame`]): encoded, its bytes
/// final but for its place, which [`Journal::append_frames`] writes.
struct Closed {
    /// Where its records end in the transaction's records: they start where the frame before
    /// it ends.
    end: usize,
    /// Its length in `journal.log`.
    len: u64,
    /// Its payload CRC.
    crc: u32,
    /// The nonce it ends in (FORMAT 3; `None` in the others).
    nonce: Option<u64>,
}

/// What `frame`, just appended, committed: its seq and payload CRC, the changesets it inserts
/// and the moves it records.
fn commit_info(frame: &Frame) -> CommitInfo {
    let count = |of: fn(&Record) -> bool| {
        bytes::to_u64(frame.records.iter().filter(|record| of(record)).count())
    };
    CommitInfo {
        frame_seq: frame.seq,
        frame_crc: Some(frame.crc),
        inserted: count(|record| matches!(record, Record::Changeset { .. })),
        moved_refs: count(|record| matches!(record, Record::RefMove { .. })),
        checkpointed: false,
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
            Record::Fact(fact) => ("fact", fact.subject().to_vec(), fact.ts()),
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

#[cfg(all(test, feature = "test-hooks"))]
mod tests {
    use std::fs;

    use super::*;
    use crate::fs_ops::{FailAt, FailAtFs, FailMode, FsMethod, InjectedError};

    /// A create removes what a crashed create left in `meta.tmp/` through the seam, its first
    /// call there: a failure injected into call 1 is that removal's (`remove_dir_all meta.tmp`),
    /// which leaves the staging as it was and makes no `meta/`. The create after it removes the
    /// staging and makes the store.
    #[test]
    fn a_create_removes_old_staging_through_the_seam() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(dir.path()).unwrap();
        let staging = root.join(STAGING);
        let left = staging.join("meta/left");
        fs::create_dir_all(left.parent().unwrap()).unwrap();
        fs::write(&left, b"a crashed create's").unwrap();

        // 1. The removal fails: the create fails with it, before it makes anything.
        let fail = FailAt::new(1, FailMode::Once, InjectedError::Enospc);
        let failing = Arc::new(FailAtFs::new(RealFs, fail));
        let fs_ops = Arc::clone(&failing) as Arc<dyn FsOps>;
        let Err(StoreError::Io(failure)) = MetaStore::create_with_fs(root, fs_ops) else {
            panic!("the create's first call fails");
        };
        assert_eq!(
            (failure.op, failure.file.as_deref(), failure.raw_os_error()),
            (FileOp::RemoveDirAll, Some(Utf8Path::new(STAGING)), Some(28))
        );
        assert_eq!(
            failing.failed_calls(),
            [(FsMethod::Remove, staging.clone().into_std_path_buf())]
        );
        assert!(
            left.exists(),
            "the staging is as the crashed create left it"
        );
        assert!(!root.join("meta").exists(), "no meta/ was made");

        // 2. The next create removes it and makes the store.
        MetaStore::create_with_fs(root, failing).unwrap();
        assert!(!left.exists(), "the crashed create's file is gone");
        assert!(root.join("meta/lock").exists(), "the store is made");
    }
}
