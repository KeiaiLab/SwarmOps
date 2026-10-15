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
//! as one frame ([`Journal::append_frame`]: the commit point), materializes the frame (an
//! [`Index::append`] per changeset, without a sync; a ref file replaced per moved ref), and
//! unlocks. Every 32 frames past the checkpoint, when it compacts (the sync first), and on
//! [`MetaStore::flush`] if any frame lies past the checkpoint, it also syncs the index and
//! moves the checkpoint to the end of the journal: the batched durability of the projections.
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
//! An error from the append itself once the frame is whole in the journal (its sync, or its
//! listing in `journal.off`, failed) is [`StoreError::Undecided`]: the transaction may or may
//! not have committed, and the next [`MetaStore::write`] settles it (it lists or cuts what the
//! journal holds, then replays). Any other error from the append leaves no whole frame: the
//! transaction did not commit. A caller that retries after `PostCommit` or `Undecided` must look
//! again first: an insert is idempotent ([`InsertOutcome::AlreadyPresent`]), and a ref move
//! compares against the ref as it is then.
//!
//! A compaction appends a `Compacted` record, in a frame of its own, as an advisory marker: a
//! compaction that [`Index::repair_tail`] finishes after a crash appends none, so nothing may
//! expect one per rebuild of the sorted views.
//!
//! # A snapshot
//!
//! [`MetaStore::read`] takes no lock and writes nothing. It reads `journal.ckpt`, opens the
//! index, reads the frames past the checkpoint and every ref file, and reads those frames
//! again: if new ones have appeared, a ref file may already show one of them, so it reads the
//! files and the frames again. The frames past the checkpoint (usually 0 to 31, and more only
//! after a crash) then apply on top in memory: their changesets the index handle does not
//! count, and their ref moves the ref files do not show yet.

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use camino::Utf8Path;
use swarmops_core::ChangesetId;

use super::frame::{Frame, MAX_RECORDS, Record};
use super::index::{ChangesetMeta, Index};
use super::journal::Journal;
use super::lock::MetaLock;
use super::parents::MAX_PARENTS;
use super::refs::{self, RefFile, RefState};
use super::replay::{self, Overlay};
use super::to_u64;
use crate::error::{StoreError, invalid_input};
use crate::fs_ops::{FsOps, RealFs};
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
    fs: Arc<dyn FsOps>,
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
        let meta = Layout::new(dir).meta_dir();
        if std::fs::exists(&meta)? {
            return Err(StoreError::Format(format!("{meta} exists already")));
        }
        let staging = dir.join(STAGING);
        if std::fs::exists(&staging)? {
            // What a create that crashed left behind: no part of a store yet.
            std::fs::remove_dir_all(&staging)?;
        }
        let staged = Layout::new(staging.as_path());
        Journal::create(&staged, Arc::clone(&fs))?.set_ckpt(0)?;
        Index::create(&staged, Arc::clone(&fs))?;
        let staged_meta = staged.meta_dir();
        fs.create_dir_all(staged.meta_path("refs").as_std_path())?;
        fs.append(staged.meta_lock().as_std_path(), &[])?;
        // The journal and the index made their own entries durable; this makes the last two
        // durable before the rename publishes them.
        fs.fsync_dir(staged_meta.as_std_path())?;
        fs.rename_durable(staged_meta.as_std_path(), meta.as_std_path())?;
        // Best effort: the staging directory is empty now, and harmless if it stays.
        let _ = std::fs::remove_dir(&staging);
        Self::open_with_fs(dir, fs)
    }

    /// Opens the metadata store of the store rooted at `dir` on the real filesystem: see
    /// [`MetaStore::open_with_fs`].
    pub fn open(dir: &Utf8Path) -> Result<MetaStore, StoreError> {
        Self::open_with_fs(dir, Arc::new(RealFs))
    }

    /// Opens the existing metadata store under `dir`, every change going through `fs` (a
    /// [`crate::CrashAtFs`] in crash tests). Reads only that `meta/journal.log` is there:
    /// [`StoreError::Format`] if it is not.
    pub fn open_with_fs(dir: &Utf8Path, fs: Arc<dyn FsOps>) -> Result<MetaStore, StoreError> {
        let layout = Layout::new(dir);
        let log = layout.journal_log();
        if !std::fs::exists(&log)? {
            return Err(StoreError::Format(format!(
                "{log} is missing: no metadata store here"
            )));
        }
        let journal = Journal::open(&layout, Arc::clone(&fs))?;
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
    /// [`crate::CrashAtFs`]) meets the same call at the same count in every run.
    pub fn set_parallelism(&mut self, threads: usize) {
        self.parallelism = threads;
    }

    /// A snapshot of the metadata as committed now, taken without a lock: see the module
    /// documentation.
    pub fn read(&self) -> Result<MetaRead<'_>, StoreError> {
        let ckpt = self.journal.ckpt()?;
        // The index after the checkpoint: everything before the checkpoint was in the index
        // files before the checkpoint moved past it.
        let index = Index::open(&self.layout, Arc::clone(&self.fs))?;
        let mut frames = self.journal.tail_frames_from(ckpt)?;
        let files = loop {
            let files = refs::read_all(&self.layout, &*self.fs)?;
            // A writer replaces ref files only after its frame is whole in the journal, so
            // if no new frame has appeared, the files show none past `frames`.
            let again = self.journal.tail_frames_from(ckpt)?;
            if again.len() == frames.len() {
                break files;
            }
            frames = again;
        };
        let overlay = Overlay::new(&index, &frames, files)?;
        Ok(MetaRead {
            store: self,
            ckpt,
            index,
            frames,
            overlay,
        })
    }

    /// Starts a write transaction: takes [`MetaLock`] (waiting as
    /// [`MetaStore::with_lock_timeout`] says), repairs the journal's and the index's tails,
    /// and replays the frames past the checkpoint into the index and the ref files. The
    /// checkpoint itself moves only at the batched sync points of [`MetaWrite::commit`].
    pub fn write(&self) -> Result<MetaWrite<'_>, StoreError> {
        let lock = MetaLock::acquire(&self.layout, self.lock_timeout)?;
        self.journal.truncate_torn_tail()?;
        let mut index = Index::open(&self.layout, Arc::clone(&self.fs))?;
        index.set_parallelism(self.parallelism);
        index.repair_tail()?;
        let frames = self.journal.tail_frames_from(self.journal.ckpt()?)?;
        replay::replay(&mut index, &self.layout, &*self.fs, &frames)?;
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
        refs::read_each(&self.layout, &*self.fs)
    }

    /// Makes the materialized files durable now: takes the lock, replays, then, if any frame
    /// lies past the checkpoint, syncs the index and moves the checkpoint to the end of the
    /// journal. For callers that need the projections durable at once (tests, closing).
    pub fn flush(&self) -> Result<(), StoreError> {
        self.write()?.finish(true).map(drop)
    }

    fn from_parts(layout: Layout, fs: Arc<dyn FsOps>, journal: Journal) -> MetaStore {
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
    /// Changeset `id`: an index lookup (a binary search of the sorted view, then the tail),
    /// then the frames past the checkpoint.
    pub fn changeset(&self, id: ChangesetId) -> Result<Option<ChangesetMeta>, StoreError> {
        match self.index.find(id)? {
            Some(rec) => self.index.get(rec).map(Some),
            None => Ok(self.overlay.changeset(id).cloned()),
        }
    }

    /// The changesets whose ids start with `hex`, 1 to 64 hex digits of either case
    /// (anything else is an `InvalidInput` error), in seq order.
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
        refs::check_name(name)?;
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
        refs::check_name(name)?;
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

    /// The ids of index records `recs`.
    fn ids_at(&self, recs: Vec<u64>) -> Result<Vec<ChangesetId>, StoreError> {
        recs.into_iter()
            .map(|rec| self.index.get(rec).map(|meta| meta.id))
            .collect()
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
    _lock: MetaLock,
}

impl MetaWrite<'_> {
    /// Queues changeset `meta`, unless the store or this transaction holds it already
    /// ([`InsertOutcome::AlreadyPresent`]). Its `seq` and `generation` are ignored: the
    /// store numbers its changesets, and a generation is 1 + the largest parent generation,
    /// 1 for a root.
    ///
    /// Every parent must be in the store or queued before it: otherwise
    /// [`StoreError::MissingParent`]. A parent listed twice is
    /// [`StoreError::DuplicateParent`]; more than 255 parents, or a transaction that holds
    /// 65,535 records already (all one frame holds), an `InvalidInput` error. Nothing is queued
    /// then.
    pub fn insert_changeset(&mut self, meta: ChangesetMeta) -> Result<InsertOutcome, StoreError> {
        if meta.parents.len() > MAX_PARENTS {
            return Err(invalid_input(format!(
                "a changeset has at most {MAX_PARENTS} parents, not {}",
                meta.parents.len()
            )));
        }
        if self.queued.contains_key(&meta.id) || self.index.find(meta.id)?.is_some() {
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
                .generation(parent)?
                .ok_or(StoreError::MissingParent(parent))?;
            generation = generation.max(parent_generation.saturating_add(1));
        }
        self.check_room()?;
        let seq = self.index.len() + to_u64(self.queued.len());
        self.queued.insert(meta.id, generation);
        self.records.push(Record::Changeset {
            id: meta.id,
            generation,
            delta_hash: meta.delta_hash,
            parents: meta.parents,
        });
        Ok(InsertOutcome::Inserted { seq })
    }

    /// Queues a move of ref `name` from `expected` to `new` (`None` deletes it) by `actor`,
    /// if the ref names `expected` now: as its file says, or the last move this transaction
    /// queued. Otherwise [`CasResult::Mismatch`], and nothing is queued.
    ///
    /// A name that is not `main` or `release/<x>` is [`StoreError::RefName`]; a `new` that is
    /// neither in the store nor queued, [`StoreError::UnknownRefTarget`]; an actor longer than
    /// 255 bytes, or a move that would not fit in the transaction's frame (65,535 records), an
    /// `InvalidInput` error.
    pub fn cas_ref(
        &mut self,
        name: &str,
        expected: Option<ChangesetId>,
        new: Option<ChangesetId>,
        actor: &str,
    ) -> Result<CasResult, StoreError> {
        refs::check_name(name)?;
        if actor.len() > MAX_ACTOR_LEN {
            return Err(invalid_input(format!(
                "an actor takes at most {MAX_ACTOR_LEN} bytes, not {}",
                actor.len()
            )));
        }
        if let Some(target) = new
            && self.generation(target)?.is_none()
        {
            return Err(StoreError::UnknownRefTarget(target));
        }
        let current = match self.moved.get(name) {
            Some(&target) => target,
            None => {
                refs::read(&self.store.layout, &*self.store.fs, name)?.map(|state| state.target)
            }
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
    /// Once the frame is appended, every error is [`StoreError::PostCommit`]: the transaction
    /// committed. An append that fails once the frame is whole in the journal is
    /// [`StoreError::Undecided`]: the transaction may have committed, and the next write settles
    /// it. Any other error means it did not commit. With nothing queued, errors are the
    /// housekeeping's own.
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
        replay::materialize(&mut self.index, &store.layout, &*store.fs, &frame)
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
            journal.append_frame(&[Record::Compacted { sorted_count }])?;
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
    /// beside the sync's; the records stay, and the next writer keeps them, since it finds them
    /// whole: the hazard stays open until the next sync that succeeds, which may pass them.
    fn sync_index(&mut self) -> Result<(), StoreError> {
        let Err(failed) = self.index.sync() else {
            return Ok(());
        };
        match self.index.truncate_to(self.checkpointed_records) {
            Ok(_) => Err(failed),
            Err(cut) => Err(io::Error::other(format!(
                "the index sync failed ({failed}), and so did cutting the index back to the \
                 checkpoint ({cut})"
            ))
            .into()),
        }
    }

    /// Refuses, with an `InvalidInput` error, a record past what the transaction's one frame
    /// holds.
    fn check_room(&self) -> Result<(), StoreError> {
        if self.records.len() < MAX_RECORDS {
            Ok(())
        } else {
            Err(invalid_input(format!(
                "a transaction holds at most {MAX_RECORDS} records, what one frame holds"
            )))
        }
    }

    /// The generation of changeset `id` if this transaction queued it or the store holds
    /// it; `None` otherwise.
    fn generation(&self, id: ChangesetId) -> Result<Option<u64>, StoreError> {
        if let Some(&generation) = self.queued.get(&id) {
            return Ok(Some(generation));
        }
        match self.index.find(id)? {
            Some(rec) => Ok(Some(self.index.get(rec)?.generation)),
            None => Ok(None),
        }
    }
}

impl fmt::Debug for MetaWrite<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MetaWrite")
            .field("index", &self.index)
            .field("checkpointed_records", &self.checkpointed_records)
            .field("uncheckpointed", &self.uncheckpointed)
            .field("records_queued", &self.records.len())
            .finish_non_exhaustive()
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
