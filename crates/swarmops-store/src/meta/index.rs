//! The columnar changeset index (storage spec §3, §3.1): one record per changeset, in the
//! order the journal recorded them, kept as struct-of-arrays files of fixed-width entries.
//!
//! ```text
//! meta/idx/ids.col   id[32] per record              record `rec` at byte rec × 32
//! meta/idx/gen.col   generation u64le per record
//! meta/idx/dh.col    delta_hash[32] per record
//! meta/idx/par.col   u64le per record: parents_offset << 8 | parents_count
//! meta/parents.log   the parents' ids, 32 bytes each, in record order
//! meta/sorted/       the sorted views by id and by delta hash ([`SortedView`])
//! ```
//!
//! [`Index::create`] lays the files out, empty, when the store is created, and nothing else
//! creates or removes one, so a missing file is [`StoreError::IndexCorrupt`]. A record's
//! number, its rec, is its position in the columns and its `seq`. The files grow by
//! [`Index::append`], which writes `parents.log`, `par.col`, `gen.col`, `dh.col` and
//! `ids.col`, in that order, with plain appends; [`Index::sync`] then makes them durable,
//! once for many appends. They shrink only at the end, when a writer cuts what a crash left
//! of a record ([`Index::repair_tail`]) or, after a failed sync, the records past the
//! checkpoint, never below what the sorted views cover ([`Index::truncate_to`]). Lookups
//! binary-search a sorted view's key column, then scan the records past the view, the unsorted
//! tail, read from `ids.col` (or `dh.col`) with one `read_at`; [`Index::compact`] merges the
//! tail into the views.
//!
//! # Crashes and concurrency
//!
//! Writers hold [`crate::MetaLock`]; readers take no lock and may open the index while a
//! writer appends or compacts. A crash in the middle of an append leaves the record's first
//! files written and the rest not. Every handle counts only the records that all five files
//! hold whole (`ids.col`, written last, is the one that lags behind), and, past
//! `journal.ckpt` and the sorted views, only those the journal vouches for: appended without
//! a sync, they are what a power loss may leave with their bytes lost, zeros in their place.
//! [`Index::repair_tail`] cuts the rest off. A crash in the middle of a compaction's renames
//! leaves the sorted views out of step; each view still covers the records it covers, or is
//! read as empty, so lookups stay right, and [`Index::repair_tail`] runs the compaction again.
//! The one cut of whole records, after a failed sync, can take records from under a handle
//! opened before it, a reader's snapshot's included: [`Index::truncate_to`] says which lookups
//! then fail.

use std::num::NonZero;
use std::panic;
use std::sync::Arc;
use std::thread;
use std::{fmt, io};

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_core::{ChangesetId, ObjectId};

use super::column::AppendOnlyColumn;
use super::frame::{Frame, Record};
use super::journal::Journal;
use super::parents::{self, MAX_PARENTS, MAX_PARENTS_LEN, PARENT_LEN, ParentsAt};
use super::replay::{ChangesetRecord, changesets};
use super::sorted::{self, Rebuild, SortedView, VIEW_FILES, ViewName, Views};
use super::to_u64;
use crate::error::{LimitKind, StoreError};
use crate::fs_ops::{FsOps, RealFs};
use crate::layout::Layout;

/// One changeset as the index holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangesetMeta {
    /// The changeset's id.
    pub id: ChangesetId,
    /// Its record number in the index: its place among the journal's changesets, so parents
    /// come before their children. [`Index::append`] assigns it and ignores the value given.
    pub seq: u64,
    /// 1 + the largest parent generation; 1 for a root.
    pub generation: u64,
    /// The hash of the changeset's delta section (storage spec §1).
    pub delta_hash: [u8; 32],
    /// The parents in order, the mainline parent first; at most 255.
    pub parents: Vec<ChangesetId>,
}

// The five files, as paths in `meta/`, in the order a fresh index creates them.
const IDS: &str = "idx/ids.col";
const GENS: &str = "idx/gen.col";
const DHS: &str = "idx/dh.col";
const PARS: &str = "idx/par.col";
const PARENTS: &str = "parents.log";

/// A handle on one store's changeset index.
///
/// A handle counts the records that every file held whole when it opened, plus those it
/// appended since. [`Index::append`], [`Index::compact`] and [`Index::repair_tail`] first
/// catch up with what other handles wrote; lookups answer from what this handle counts.
/// Lookups take `&self` and read with `read_at` on read-only handles; changes take
/// `&mut self`, go through [`FsOps`], and need [`crate::MetaLock`].
pub struct Index {
    fs: Arc<dyn FsOps>,
    sorted_dir: Utf8PathBuf,
    ids: AppendOnlyColumn<32>,
    gens: AppendOnlyColumn<8>,
    dhs: AppendOnlyColumn<32>,
    pars: AppendOnlyColumn<8>,
    /// Addressed by byte offset, as `par.col` entries hold them.
    parents: AppendOnlyColumn<1>,
    /// The records this handle counts.
    len: u64,
    /// The bytes of `parents.log` those records' parents take.
    parents_len: u64,
    views: Views,
    /// How many threads [`Index::sync`] syncs the five files from: see
    /// [`Index::set_parallelism`].
    sync_threads: usize,
    /// The store's paths: the journal past its checkpoint vouches for the records past it
    /// ([`Index::open`]).
    layout: Layout,
}

impl Index {
    /// How many records the unsorted tail may hold before a writer should compact. The
    /// writer decides; [`Index::compact`] itself always compacts.
    pub const COMPACT_THRESHOLD: u64 = 1024;

    /// Creates the files of an empty index in `layout` where they are missing, changes going
    /// through `fs`: `meta/idx/` with the four columns, `meta/parents.log`, and
    /// `meta/sorted/` with the four view files; makes their directory entries durable; and
    /// opens the index. Existing files are kept as they are. [`crate::MetaStore::create`]
    /// calls it in its staging directory: a store gets its index files there, and nowhere
    /// else.
    pub fn create(layout: &Layout, fs: Arc<dyn FsOps>) -> Result<Index, StoreError> {
        let idx = layout.meta_path("idx");
        let sorted = layout.meta_path("sorted");
        fs.create_dir_all(idx.as_std_path())?;
        fs.create_dir_all(sorted.as_std_path())?;
        let columns = [IDS, GENS, DHS, PARS, PARENTS].map(|rel| layout.meta_path(rel));
        for path in columns
            .iter()
            .chain(&VIEW_FILES.map(|name| sorted.join(name)))
        {
            // An empty append creates a missing file and never truncates an existing one.
            fs.append(path.as_std_path(), &[])?;
        }
        fs.fsync_dir(idx.as_std_path())?;
        fs.fsync_dir(sorted.as_std_path())?;
        fs.fsync_dir(layout.meta_dir().as_std_path())?;
        Self::open(layout, fs)
    }

    /// Opens the index of `layout`, changes going through `fs`.
    ///
    /// Creates nothing, since lock-free readers open it: a store has every index file from
    /// [`Index::create`], so a missing one is [`StoreError::IndexCorrupt`]. Otherwise it only
    /// reads, without a lock: the files' lengths, the last record's `par.col` entry, and the
    /// sorted views' lengths; and when the files hold records past what the views cover, the
    /// journal's frames past its checkpoint (and, only if the index holds none of their
    /// changesets, the frames before it back to its last changeset), and the records past the
    /// checkpoint and the views, which the journal must vouch for to be counted (the journal's
    /// changesets past the checkpoint, in order, field by field). With no journal, a damaged
    /// one, or an index that lacks the last changeset before the checkpoint, it counts the
    /// records by their lengths, and reads the `par.col` entries of those
    /// past the views instead: [`StoreError::IndexCorrupt`] if they are out of their layout,
    /// which a power loss that zeroed some of them leaves. It repairs nothing, since a
    /// writer may be appending or compacting meanwhile: records that some files lack, or that
    /// the journal does not vouch for, are not counted, and a view that a compaction is
    /// renaming (or crashed renaming) counts as covering what it covers, or nothing. A writer
    /// repairs with [`Index::repair_tail`].
    pub fn open(layout: &Layout, fs: Arc<dyn FsOps>) -> Result<Index, StoreError> {
        let mut index = Self::handle(layout, fs)?;
        let lens = index.lens()?;
        let whole = index.whole_of(lens, None)?;
        index.set_count(&whole)?;
        Ok(index)
    }

    /// [`Index::open`], with the frames past the checkpoint that `read_past` reads vouching
    /// for the records past it, in place of a read of the journal of its own: a snapshot reads
    /// those frames for its overlay and a write transaction for its replay, so vouching costs
    /// them no read they did not make already. `read_past` runs once, after the files' lengths
    /// are measured, so that every record they hold has its frame among what it reads (a writer
    /// appends a frame before it materializes it); its frames come back with the handle. Its
    /// error is the caller's own type, which the index's errors convert into, so that the
    /// caller can tell its own reasons for stopping from the index's ([`crate::MetaStore::read`]
    /// takes another round when `journal.off` shrank as the frames were read).
    pub(crate) fn open_with_past<E: From<StoreError>>(
        layout: &Layout,
        fs: Arc<dyn FsOps>,
        read_past: impl FnOnce() -> Result<Vec<Frame>, E>,
    ) -> Result<(Index, Vec<Frame>), E> {
        let mut index = Self::handle(layout, fs)?;
        let lens = index.lens()?;
        let past = read_past()?;
        let whole = index.whole_of(lens, Some(&past))?;
        index.set_count(&whole)?;
        Ok((index, past))
    }

    /// A handle on the files of `layout`, counting no record yet.
    fn handle(layout: &Layout, fs: Arc<dyn FsOps>) -> Result<Index, StoreError> {
        let sorted_dir = layout.meta_path("sorted");
        // The views before the columns: a compaction covers records that were whole when it
        // ran, and whole records stay, so the columns read afterwards hold all that the
        // views cover.
        let views = Views::open(&sorted_dir)?;
        Ok(Index {
            ids: AppendOnlyColumn::open(layout.meta_path(IDS))?,
            gens: AppendOnlyColumn::open(layout.meta_path(GENS))?,
            dhs: AppendOnlyColumn::open(layout.meta_path(DHS))?,
            pars: AppendOnlyColumn::open(layout.meta_path(PARS))?,
            parents: AppendOnlyColumn::open(layout.meta_path(PARENTS))?,
            fs,
            sorted_dir,
            len: 0,
            parents_len: 0,
            views,
            sync_threads: 1,
            layout: layout.clone(),
        })
    }

    /// Makes what `whole` holds this handle's count, which the views must not cover more of.
    fn set_count(&mut self, whole: &Whole) -> Result<(), StoreError> {
        (self.len, self.parents_len) = (whole.records, whole.parents_len);
        self.views.check_within(self.len)
    }

    /// How many records this handle counts.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Whether this handle counts no records.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// How many records the four columns hold whole by their lengths, measured now (one
    /// `fstat` each, `ids.col` first, as an open measures them), whether or not this handle,
    /// or any, counts them: past the checkpoint and the sorted views a handle counts only the
    /// records the journal vouches for, so a record that no frame holds, past the journal's last
    /// changeset, is counted by none. What verify compares with the journal's changeset count:
    /// a writer appends a record only once its frame is durable and listed, so measured before
    /// the journal is read, the files hold no more records than the journal read then has
    /// changesets.
    pub fn held(&self) -> Result<u64, StoreError> {
        let ids = AppendOnlyColumn::<32>::whole(self.ids.len()?);
        let dhs = AppendOnlyColumn::<32>::whole(self.dhs.len()?);
        let gens = AppendOnlyColumn::<8>::whole(self.gens.len()?);
        let pars = AppendOnlyColumn::<8>::whole(self.pars.len()?);
        Ok(ids.min(dhs).min(gens).min(pars))
    }

    /// How many records both sorted views cover: the first `sorted_count()` records.
    pub fn sorted_count(&self) -> u64 {
        self.views.sorted_count()
    }

    /// How many records the sorted views do not both cover: the ones lookups scan.
    pub fn tail_len(&self) -> u64 {
        // The views never cover more than `len`: the two change only together, after the
        // checks. Saturating all the same, so that no state can make this panic.
        self.len.saturating_sub(self.sorted_count())
    }

    /// The sorted view by changeset id.
    pub fn ids_view(&self) -> &SortedView<32> {
        &self.views.ids
    }

    /// The sorted view by delta hash.
    pub fn dh_view(&self) -> &SortedView<32> {
        &self.views.dh
    }

    /// Appends `meta` as the next record and returns its rec (`meta.seq` is ignored): one
    /// plain append (no fsync) to each of `parents.log`, `par.col`, `gen.col`, `dh.col` and
    /// `ids.col`, in that order. [`Index::sync`] makes the records durable.
    ///
    /// The caller must hold [`crate::MetaLock`]. If the files do not end where this
    /// handle's records do (another handle appended, or a crash left part of a record),
    /// this handle catches up first, cutting off what is not whole, as
    /// [`Index::repair_tail`] does; files that hold fewer whole records than this handle
    /// counts, or than the sorted views cover, are damaged, [`StoreError::IndexCorrupt`],
    /// and nothing is cut or written then. More than 255 parents is [`StoreError::Limit`],
    /// and nothing is written then either. After any other error, what was written is cut
    /// off by the next append or repair.
    pub fn append(&mut self, meta: &ChangesetMeta) -> Result<u64, StoreError> {
        let count = u8::try_from(meta.parents.len()).map_err(|_| StoreError::Limit {
            what: LimitKind::Parents,
            max: to_u64(MAX_PARENTS),
            got: to_u64(meta.parents.len()),
        })?;
        if !self.ends_where_counted()? {
            self.catch_up()?;
        }
        let rec = self.len;
        let at = ParentsAt {
            offset: self.parents_len,
            count,
        };
        if at.offset > ParentsAt::MAX_OFFSET {
            return Err(io::Error::new(io::ErrorKind::FileTooLarge, "parents.log is full").into());
        }
        let mut buf = [0; MAX_PARENTS_LEN];
        let parents = parents::encode(&meta.parents, &mut buf);
        let fs = &*self.fs;
        self.parents.append(fs, parents, at.offset)?;
        self.pars.append(fs, &at.entry().to_le_bytes(), rec)?;
        self.gens.append(fs, &meta.generation.to_le_bytes(), rec)?;
        self.dhs.append(fs, &meta.delta_hash, rec)?;
        self.ids.append(fs, &meta.id.0.0, rec)?;
        self.len = rec + 1;
        self.parents_len = at.end();
        Ok(rec)
    }

    /// Makes [`Index::sync`] (also the sync a compaction starts with) flush the five files
    /// from up to `threads` threads at once (0: one per available core), at most one per file.
    /// A handle opens with 1: one file after another, in write order. [`crate::MetaStore`]
    /// gives the index of every write transaction its own parallelism, 5 unless set.
    pub fn set_parallelism(&mut self, threads: usize) {
        self.sync_threads = match threads {
            0 => thread::available_parallelism().map_or(1, NonZero::get),
            threads => threads,
        };
    }

    /// Flushes the five files to stable storage, one `sync_data` each. The writer calls it
    /// before it moves `journal.ckpt` past the records, which it does in batches, not per
    /// transaction (see [`crate::meta`]).
    ///
    /// With parallelism 1 ([`Index::set_parallelism`]) the syncs go one after another, in the
    /// order [`Index::append`] writes the files, and the first error stops them. With more,
    /// up to that many threads make them at once, each taking its share of the files in write
    /// order and stopping at its first error (one file each at 5); once every thread is done,
    /// the error of the first file in write order that failed comes back. A share whose thread
    /// cannot be spawned (the process is out of threads) is synced on the calling thread
    /// instead, so that this step never fails, or panics, for want of a thread. ext4 commits
    /// concurrent fsyncs together: five at once cost about two (docs/perf.md).
    pub fn sync(&self) -> Result<(), StoreError> {
        let files = self.write_order();
        let fs = &*self.fs;
        let sync_all = |share: &[&Utf8Path]| -> io::Result<()> {
            share
                .iter()
                .try_for_each(|path| fs.sync_data(path.as_std_path()))
        };
        let threads = self.sync_threads.min(files.len());
        if threads <= 1 {
            return Ok(sync_all(&files)?);
        }
        let results: Vec<io::Result<()>> = thread::scope(|scope| {
            let syncs: Vec<_> = files
                .chunks(files.len().div_ceil(threads))
                .map(|share| {
                    thread::Builder::new()
                        .spawn_scoped(scope, move || sync_all(share))
                        .map_err(|_| share)
                })
                .collect();
            syncs
                .into_iter()
                .map(|sync| match sync {
                    Ok(sync) => sync
                        .join()
                        .unwrap_or_else(|panic| panic::resume_unwind(panic)),
                    // No thread for this share: sync it here, in its place in write order.
                    Err(share) => sync_all(share),
                })
                .collect()
        });
        Ok(results.into_iter().collect::<io::Result<()>>()?)
    }

    /// Record `rec`: four `read_at`s of the columns, and one of `parents.log` if it has
    /// parents. A rec this handle does not count is [`StoreError::BadCall`] (the lookups never
    /// give one: they check the recs they read from a sorted view); parents off `parents.log`
    /// are [`StoreError::IndexCorrupt`].
    pub fn get(&self, rec: u64) -> Result<ChangesetMeta, StoreError> {
        if rec >= self.len {
            return Err(StoreError::BadCall(format!(
                "rec {rec} is past the index's {} records",
                self.len
            )));
        }
        let id = self.ids.entry(rec)?;
        let generation = u64::from_le_bytes(self.gens.entry(rec)?);
        let delta_hash = self.dhs.entry(rec)?;
        let at = ParentsAt::from_entry(u64::from_le_bytes(self.pars.entry(rec)?));
        Ok(ChangesetMeta {
            id: ChangesetId(ObjectId(id)),
            seq: rec,
            generation,
            delta_hash,
            parents: parents::read(&self.parents, at, self.parents_len)?,
        })
    }

    /// The rec of changeset `id`, if the index holds it: a binary search of the id view's
    /// key column (one `read_at` per probe), then, if the view lacks it, a scan of the
    /// tail of `ids.col`, read with one `read_at`. A rec the view names that the view does not
    /// cover (a view of m entries holds the recs `[0, m)`) is [`StoreError::IndexCorrupt`].
    pub fn find(&self, id: ChangesetId) -> Result<Option<u64>, StoreError> {
        let key = id.0.0;
        let view = &self.views.ids;
        let at = view.lower_bound(&key)?;
        if at < view.len() && view.key_at(at)? == key {
            return covered_by(view, view.rec_at(at)?).map(Some);
        }
        let from = view.len();
        let tail = self.ids.entries(from, self.len)?;
        Ok(tail
            .as_chunks::<32>()
            .0
            .iter()
            .position(|probe| *probe == key)
            .map(|i| from + to_u64(i)))
    }

    /// The recs, ascending, of the changesets whose ids start with `prefix_nibbles`, 1 to 64
    /// hex digits of either case; an odd count compares the last digit with the high half of
    /// its byte. Anything else is [`StoreError::BadPrefix`]. A lower-bound search of the id
    /// view, a forward scan of its keys while they match, one read of their recs, and a scan
    /// of the tail of `ids.col`.
    pub fn find_prefix(&self, prefix_nibbles: &str) -> Result<Vec<u64>, StoreError> {
        let prefix = Prefix::parse(prefix_nibbles)?;
        self.find_all(&self.views.ids, &self.ids, &prefix.low, |key| {
            prefix.matches(key)
        })
    }

    /// The recs, ascending, of the changesets whose delta hash is `dh`: changesets that make
    /// the same change on different bases, as a clean rebase does. Searched like
    /// [`Index::find_prefix`], in the delta-hash view and the tail of `dh.col`.
    pub fn find_delta_hash(&self, dh: &[u8; 32]) -> Result<Vec<u64>, StoreError> {
        self.find_all(&self.views.dh, &self.dhs, dh, |key| key == dh)
    }

    /// The recs, ascending, of the records whose key `matches` accepts, the keys it accepts
    /// being a run in key order that starts at `low` or after it: that run of `view`, from a
    /// lower-bound search, one read of its recs (each checked against what the view covers,
    /// [`covered_by`]), and a scan of the tail of `column`.
    fn find_all(
        &self,
        view: &SortedView<32>,
        column: &AppendOnlyColumn<32>,
        low: &[u8; 32],
        matches: impl Fn(&[u8; 32]) -> bool,
    ) -> Result<Vec<u64>, StoreError> {
        let start = view.lower_bound(low)?;
        let end = view.run_end(start, &matches)?;
        let mut recs = view.recs(start..end)?;
        for &rec in &recs {
            covered_by(view, rec)?;
        }
        let from = view.len();
        let tail = column.entries(from, self.len)?;
        recs.extend(
            (from..)
                .zip(tail.as_chunks::<32>().0)
                .filter(|(_, key)| matches(key))
                .map(|(rec, _)| rec),
        );
        recs.sort_unstable();
        Ok(recs)
    }

    /// Merges the unsorted tail into both sorted views, so that they cover every record, and
    /// returns how many records they cover. The caller must hold [`crate::MetaLock`]; it
    /// decides when to compact ([`Index::COMPACT_THRESHOLD`]).
    ///
    /// Catches up first, as [`Index::repair_tail`] does. With nothing to merge it writes
    /// nothing. Otherwise it syncs the five files (a view must not cover records that a power
    /// loss could take back), then for each view whose records it does not all cover: sorts
    /// the tail's (key, rec) pairs, read from the column with one `read_at`, with
    /// `sort_unstable`; merges them with the old view, read a block at a time, into
    /// `sorted/<name>.tmp`, written with plain appends a block at a time; syncs the temp
    /// files; and renames them into place, `ids.key`, `ids.rec`, `dh.key`, `dh.rec`, each
    /// rename made durable before the next. A crash leaves each view old or new, which
    /// readers handle and which the next compaction finishes: compaction is idempotent.
    pub fn compact(&mut self) -> Result<u64, StoreError> {
        self.catch_up()?;
        self.rebuild_views()?;
        Ok(self.sorted_count())
    }

    /// Brings the files to a clean end after a crash, and this handle up to date with them;
    /// returns how many records were dropped. What a writer calls, holding
    /// [`crate::MetaLock`], before it replays the journal into the index.
    ///
    /// - A crash in the middle of an append leaves the record in some files and not in
    ///   others (`ids.col` is written last, `parents.log` first; with power lost before a
    ///   sync, any of them can lag). A power loss can also keep a file's length and lose the
    ///   bytes of records past `journal.ckpt` (zeros in their place, or in the first sector of
    ///   them only), which the journal then does not vouch for. Every column is cut to the
    ///   records that all five files hold whole and the journal vouches for, and `parents.log`
    ///   to the end of the last such record's parents (its `par.col` entry says where). The
    ///   records cut, counted as slots of the longest column, are returned: their journal
    ///   frames lie past `journal.ckpt`, since the checkpoint moves only after
    ///   [`Index::sync`], so replay appends them again.
    /// - A crash in the middle of a compaction's renames leaves the views out of step: the
    ///   compaction is run again.
    ///
    /// Files that hold fewer whole records than this handle counts, or than the sorted views
    /// cover, are damaged rather than torn: [`StoreError::IndexCorrupt`], and nothing is cut.
    /// (A handle opened after such damage counts only what is whole; the journal, not the
    /// index, can tell it records are missing.)
    ///
    /// Records it keeps may be ones a crashed writer appended and never synced: sync before
    /// moving `journal.ckpt` past them.
    pub fn repair_tail(&mut self) -> Result<u64, StoreError> {
        self.repair_tail_vouched_by(None)
    }

    /// [`Index::repair_tail`], with `past`, the frames past the checkpoint that this handle
    /// opened with ([`Index::open_with_past`]), vouching for the records it does not count, in
    /// place of a read of the journal of its own: what a write transaction calls right after it
    /// opened the index, holding [`crate::MetaLock`], so that the journal is as it read it.
    pub(crate) fn repair_tail_with(&mut self, past: &[Frame]) -> Result<u64, StoreError> {
        self.repair_tail_vouched_by(Some(past))
    }

    fn repair_tail_vouched_by(&mut self, past: Option<&[Frame]>) -> Result<u64, StoreError> {
        let dropped = self.catch_up_vouched_by(past)?;
        if !self.views.in_step() {
            self.rebuild_views()?;
        }
        Ok(dropped)
    }

    /// Cuts the index back to its first `records` records, or to as many as the sorted views
    /// cover if that is more, and returns how many it keeps; a handle that counts no more
    /// than that cuts nothing. What a writer whose [`Index::sync`] failed calls, holding
    /// [`crate::MetaLock`] (see [`crate::meta`]): pages whose writeback failed may be lost
    /// even though a later sync succeeds, so the records past the checkpoint are cut, and
    /// replay appends them again from the journal. The views' records were synced before the
    /// views were written, and a view must not cover records the columns lack.
    ///
    /// The files are cut in the reverse of the write order, `ids.col` first, each cut synced
    /// ([`FsOps::truncate`]), so that a crash in the middle leaves a torn tail for
    /// [`Index::repair_tail`] to cut the rest of; this handle counts only the records kept
    /// from before the first cut.
    ///
    /// Another handle that counted records cut, opened before the cut (a reader's snapshot,
    /// verify), then fails with an I/O error (`UnexpectedEof`), never with wrong data: on
    /// [`Index::get`] of a record cut, on every [`Index::find`] that its sorted view misses (the
    /// scan of the tail reads past the new end), and on every [`Index::find_prefix`] and
    /// [`Index::find_delta_hash`]. A [`crate::MetaStore::read`] whose handle opened before the
    /// cut fails the same way while it builds its snapshot. A handle opened after the cut, and
    /// so a new snapshot, reads the index as cut, the journal's frames over it. A handle that
    /// opens while the cut is made, its files measured before it and read after, counts every
    /// record the checkpoint passed or the sorted views cover, which the cut keeps, and of the
    /// others only those it still finds whole that the journal vouches for (where the journal
    /// cannot vouch, those the cut keeps, counted by their lengths), and fails as one opened
    /// before the cut does only if the files are cut again meanwhile: it never counts fewer.
    pub fn truncate_to(&mut self, records: u64) -> Result<u64, StoreError> {
        let keep = records.max(self.views.covered());
        if keep >= self.len {
            return Ok(self.len);
        }
        // The parents of the records kept end where the last one's par.col entry says.
        let parents_len = match keep.checked_sub(1) {
            Some(last) => ParentsAt::from_entry(u64::from_le_bytes(self.pars.entry(last)?)).end(),
            None => 0,
        };
        (self.len, self.parents_len) = (keep, parents_len);
        // Every length first, then the cuts.
        let [ids, dhs, gens, pars, parents] = [
            self.ids.len()?,
            self.dhs.len()?,
            self.gens.len()?,
            self.pars.len()?,
            self.parents.len()?,
        ];
        let fs = &*self.fs;
        self.ids.truncate(fs, ids, keep)?;
        self.dhs.truncate(fs, dhs, keep)?;
        self.gens.truncate(fs, gens, keep)?;
        self.pars.truncate(fs, pars, keep)?;
        self.parents.truncate(fs, parents, parents_len)?;
        Ok(keep)
    }

    /// Reopens the sorted views (another handle may have compacted since this one opened
    /// them) and repairs the columns; returns how many records were dropped. On an error the
    /// handle is left as it was.
    fn catch_up(&mut self) -> Result<u64, StoreError> {
        self.catch_up_vouched_by(None)
    }

    /// [`Index::catch_up`], `past` vouching for the records this handle does not count, if
    /// given (see [`Index::repair_tail_with`]); else a read of the journal of its own.
    fn catch_up_vouched_by(&mut self, past: Option<&[Frame]>) -> Result<u64, StoreError> {
        let views = Views::open(&self.sorted_dir)?;
        let (whole, dropped) = self.repair_files(&views, past)?;
        (self.len, self.parents_len) = (whole.records, whole.parents_len);
        self.views = views;
        Ok(dropped)
    }

    /// Rebuilds each sorted view that does not cover every record (see [`Index::compact`]).
    fn rebuild_views(&mut self) -> Result<(), StoreError> {
        let ids_stale = self.views.ids.len() < self.len;
        let dh_stale = self.views.dh.len() < self.len;
        if !ids_stale && !dh_stale {
            return Ok(());
        }
        self.sync()?;
        let mut rebuilds = Vec::with_capacity(2);
        if ids_stale {
            rebuilds.push(Rebuild {
                view: ViewName::Ids,
                old: &self.views.ids,
                tail: sorted_tail(&self.ids, self.views.ids.len(), self.len)?,
            });
        }
        if dh_stale {
            rebuilds.push(Rebuild {
                view: ViewName::DeltaHash,
                old: &self.views.dh,
                tail: sorted_tail(&self.dhs, self.views.dh.len(), self.len)?,
            });
        }
        sorted::rebuild(&*self.fs, &self.sorted_dir, &rebuilds)?;
        let views = Views::open(&self.sorted_dir)?;
        views.check_within(self.len)?;
        self.views = views;
        Ok(())
    }

    /// Cuts the files back to the records that all five hold whole, and returns those, with
    /// how many record slots were cut from the longest column; the caller makes them this
    /// handle's count. Checks first, and cuts nothing if the files hold fewer whole records
    /// than this handle counts (no crash and no writer cuts a whole record, so the files are
    /// damaged) or than `views` cover: [`StoreError::IndexCorrupt`]. `past` is as
    /// [`Index::catch_up_vouched_by`] takes it.
    fn repair_files(
        &self,
        views: &Views,
        past: Option<&[Frame]>,
    ) -> Result<(Whole, u64), StoreError> {
        let whole = self.whole_of(self.lens()?, past)?;
        if whole.records < self.len {
            return Err(StoreError::IndexCorrupt {
                what: "records this handle counted are no longer whole in every index file",
            });
        }
        views.check_within(whole.records)?;
        let [parents, pars, gens, dhs, ids] = whole.lens;
        let (fs, records) = (&*self.fs, whole.records);
        // Cut in the reverse of the write order, so that ids.col goes first.
        let dropped = [
            self.ids.truncate(fs, ids, records)?,
            self.dhs.truncate(fs, dhs, records)?,
            self.gens.truncate(fs, gens, records)?,
            self.pars.truncate(fs, pars, records)?,
        ];
        self.parents.truncate(fs, parents, whole.parents_len)?;
        Ok((whole, dropped.into_iter().max().unwrap_or(0)))
    }

    /// The five files' lengths, one `fstat` each, in write order: `parents.log`, `par.col`,
    /// `gen.col`, `dh.col`, `ids.col`. Measured from `ids.col` on: append writes it last, so
    /// with no power lost a record it holds is whole in the files measured after it.
    fn lens(&self) -> Result<[u64; 5], StoreError> {
        let ids = self.ids.len()?;
        let dhs = self.dhs.len()?;
        let gens = self.gens.len()?;
        let pars = self.pars.len()?;
        let parents = self.parents.len()?;
        Ok([parents, pars, gens, dhs, ids])
    }

    /// What the files hold whole, judged from `lens`, their lengths as [`Index::lens`] measured
    /// them; from the journal past its checkpoint, for the records this handle does not count
    /// (those it counts were vouched for when it counted them, or it appended them itself, from
    /// the journal); and from the `par.col` entry of the last record left. `past`, if given, is
    /// the frames past the checkpoint, read after `lens` were measured, to vouch with
    /// ([`Index::vouched_with`]); `None` reads them ([`Index::vouched`]).
    fn whole_of(&self, lens: [u64; 5], past: Option<&[Frame]>) -> Result<Whole, StoreError> {
        let [parents, pars, gens, dhs, ids] = lens;
        // The minimum covers a power loss as well, as far as lengths go; the journal vouches
        // for the bytes.
        let records = AppendOnlyColumn::<32>::whole(ids)
            .min(AppendOnlyColumn::<32>::whole(dhs))
            .min(AppendOnlyColumn::<8>::whole(gens))
            .min(AppendOnlyColumn::<8>::whole(pars));
        let mut records = match past {
            _ if records <= self.len => records,
            Some(frames) => self.vouched_with(records, frames, parents, None)?,
            None => self.vouched(records, parents)?,
        };
        let parents_len = loop {
            let Some(last) = records.checked_sub(1) else {
                break 0;
            };
            let at = ParentsAt::from_entry(u64::from_le_bytes(self.pars.entry(last)?));
            if !at.offset.is_multiple_of(PARENT_LEN) {
                return Err(StoreError::IndexCorrupt {
                    what: "a record's parents lie off parents.log's parent boundaries",
                });
            }
            if at.end() <= parents {
                break at.end();
            }
            records = last; // parents.log lacks some of its parents
        };
        Ok(Whole {
            records,
            parents_len,
            lens,
        })
    }

    /// [`Index::vouched_with`] the frames past the journal's checkpoint, read on handles of
    /// its own, not through this index's [`FsOps`]; `parents_file_len` is the length of
    /// `parents.log`. Records the sorted views cover were synced before the views were
    /// written: with no record past them, nothing is read. With no journal to read (an index
    /// on its own), a checkpoint that does not hold a u64le, or a journal that is damaged, the
    /// records are counted by their lengths ([`Index::by_length`]): that damage is the
    /// journal's, which verify reports. Any other error reading them is returned.
    fn vouched(&self, records: u64, parents_file_len: u64) -> Result<u64, StoreError> {
        if records <= self.views.covered() {
            return Ok(records);
        }
        let journal = match Journal::open(&self.layout, Arc::new(RealFs)) {
            Ok(journal) => journal,
            Err(StoreError::Io(e)) if e.kind() == io::ErrorKind::NotFound => {
                return self.by_length(records);
            }
            Err(e) => return Err(e),
        };
        let ckpt = match journal.ckpt() {
            Ok(ckpt) => ckpt,
            Err(StoreError::Io(e)) if e.kind() == io::ErrorKind::InvalidData => {
                return self.by_length(records);
            }
            Err(e) => return Err(e),
        };
        let frames = match journal.tail_frames_from(ckpt) {
            Ok(frames) => frames,
            Err(StoreError::JournalCorrupt { .. }) => return self.by_length(records),
            Err(e) => return Err(e),
        };
        self.vouched_with(records, &frames, parents_file_len, Some(&journal))
    }

    /// How many of the first `records` records, those the five files hold by their lengths,
    /// `frames`, the journal's frames past its checkpoint, vouch for: every one before the
    /// checkpoint (synced before it moved past them) or that the sorted views cover (synced
    /// before the views were written), and of the others the run that are the changesets of
    /// `frames`, in order and field by field, each one's parents where the record before it
    /// leaves off in `parents.log` (`parents_file_len` long). The records past the checkpoint
    /// were appended without a sync: a power loss can keep a file's length and lose their bytes
    /// (zeros in their place), or lose the first sector they lie in and keep a later one. The
    /// first such record, and every one after it, holds nothing the journal vouches for: no
    /// handle counts them, a reader's snapshot reads their changesets from the journal, and
    /// [`Index::repair_tail`] cuts them for replay to append the journal's in their place.
    ///
    /// With no record past the sorted views, nothing is read. The records past the checkpoint
    /// start after the last changeset before it. Where the index holds one of the changesets
    /// past it among its last records, that record's place says where; only otherwise is the
    /// journal read back from the checkpoint to its last changeset, on `journal` or on a handle
    /// of its own (not through this index's [`FsOps`]), and a damaged frame there counts the
    /// records by their lengths ([`Index::by_length`]), as an index lacking the last changeset
    /// before the checkpoint does.
    ///
    /// A writer may cut records past the checkpoint after `records` were measured (a failed
    /// sync's [`Index::truncate_to`], or a repair after a power loss), but never one before it,
    /// nor one the sorted views cover: where those records start is looked for in what
    /// `ids.col` holds then ([`held_entries`]), so a cut never makes the count fall short of the
    /// records the checkpoint passed or the views cover, and records cut vouch for nothing.
    fn vouched_with(
        &self,
        records: u64,
        frames: &[Frame],
        parents_file_len: u64,
        journal: Option<&Journal>,
    ) -> Result<u64, StoreError> {
        if records <= self.views.covered() {
            return Ok(records);
        }
        let past = changesets(frames);
        let Some(first_frame) = frames.first().filter(|_| !past.is_empty()) else {
            return Ok(records); // no changeset past the checkpoint: no record past it either
        };
        // The records the sorted views cover count, whatever a writer has cut past them since
        // `records` were measured: they may lie past the checkpoint (a compaction's checkpoint
        // failed after it, or moved after the checkpoint was read), and no cut takes them.
        let count_from = |first: u64| -> Result<u64, StoreError> {
            let vouched = self.vouched_from(first, records, &past, parents_file_len)?;
            Ok(vouched.max(self.views.covered()))
        };
        if let Some(first) = self.first_past_by_id(&past, records)? {
            return count_from(first);
        }
        // None of them among its last records: none is materialized yet, a power loss zeroed
        // their ids, or a writer has cut them since. The last changeset before the checkpoint
        // says where they start, which the index holds among its last `past + 1` records (it
        // holds no more than `past` past the checkpoint), and which no cut takes.
        let opened;
        let journal = match journal {
            Some(journal) => journal,
            None => match Journal::open(&self.layout, Arc::new(RealFs)) {
                Ok(journal) => {
                    opened = journal;
                    &opened
                }
                Err(StoreError::Io(e)) if e.kind() == io::ErrorKind::NotFound => {
                    return self.by_length(records);
                }
                Err(e) => return Err(e),
            },
        };
        let last = match last_changeset_before(journal, first_frame.seq) {
            Ok(last) => last,
            Err(StoreError::JournalCorrupt { .. }) => return self.by_length(records),
            Err(e) => return Err(e),
        };
        let first = match last {
            None => 0,
            Some(last) => {
                let from = records.saturating_sub(to_u64(past.len()) + 1);
                let window = held_entries(&self.ids, from, records)?;
                let at = window
                    .as_chunks::<32>()
                    .0
                    .iter()
                    .position(|id| *id == last.0.0);
                match at {
                    Some(at) => from + to_u64(at) + 1,
                    // The index lacks records the checkpoint passed: damage, which verify
                    // reports against the journal.
                    None => return self.by_length(records),
                }
            }
        };
        count_from(first)
    }

    /// Where the records past the checkpoint start, if the index holds one of `past`, the
    /// changesets past it, among its last `past` records, as far as `ids.col` holds them now
    /// ([`held_entries`]): that record's rec less its place among them. `None` if it holds none
    /// of them there: none is materialized yet (a reader that came between a writer's frame and
    /// its materialization, or a writer that crashed there), a power loss zeroed their ids, or
    /// a writer has cut them since `records` were measured.
    fn first_past_by_id(
        &self,
        past: &[ChangesetRecord<'_>],
        records: u64,
    ) -> Result<Option<u64>, StoreError> {
        let from = records.saturating_sub(to_u64(past.len()));
        let window = held_entries(&self.ids, from, records)?;
        let window = window.as_chunks::<32>().0;
        for (place, changeset) in past.iter().enumerate() {
            if let Some(at) = window.iter().position(|probe| *probe == changeset.id.0.0) {
                return Ok((from + to_u64(at)).checked_sub(to_u64(place)));
            }
        }
        Ok(None)
    }

    /// `first` plus how many of the records from `first` up to `records` are `past`, the
    /// changesets past the checkpoint, in order: id, generation, delta hash, and parents, which
    /// start where the record before leaves off in `parents.log` (`parents_file_len` long). One
    /// read of each column, and one of `parents.log` for the parents of them all.
    fn vouched_from(
        &self,
        first: u64,
        records: u64,
        past: &[ChangesetRecord<'_>],
        parents_file_len: u64,
    ) -> Result<u64, StoreError> {
        let end = records.min(first + to_u64(past.len()));
        let columns = (
            read_or_cut(&self.ids, first, end)?,
            read_or_cut(&self.gens, first, end)?,
            read_or_cut(&self.dhs, first, end)?,
            read_or_cut(&self.pars, first, end)?,
        );
        let (Some(ids), Some(gens), Some(dhs), Some(pars)) = columns else {
            return Ok(first);
        };
        let start = match first.checked_sub(1) {
            Some(before) => {
                ParentsAt::from_entry(u64::from_le_bytes(self.pars.entry(before)?)).end()
            }
            None => 0,
        };
        let (ids, gens) = (ids.as_chunks::<32>().0, gens.as_chunks::<8>().0);
        let (dhs, pars) = (dhs.as_chunks::<32>().0, pars.as_chunks::<8>().0);
        let past = &past[..ids.len()];
        // The parents the journal gives them lie back to back from `start`: one read of as much
        // of them as parents.log holds.
        let wanted: u64 = past
            .iter()
            .map(|changeset| PARENT_LEN * to_u64(changeset.parents.len()))
            .sum();
        let held = parents_file_len.saturating_sub(start).min(wanted);
        let logged = read_or_cut(&self.parents, start, start + held)?.unwrap_or_default();
        let mut next = start;
        for (i, want) in past.iter().enumerate() {
            let at = ParentsAt::from_entry(u64::from_le_bytes(pars[i]));
            let from = usize::try_from(next - start).unwrap_or(usize::MAX);
            let len =
                usize::try_from(PARENT_LEN * to_u64(want.parents.len())).unwrap_or(usize::MAX);
            let parents_there = logged.get(from..).and_then(|rest| rest.get(..len));
            let vouched = ids[i] == want.id.0.0
                && u64::from_le_bytes(gens[i]) == want.generation
                && dhs[i] == *want.delta_hash
                && at.offset == next
                && usize::from(at.count) == want.parents.len()
                && parents_there.is_some_and(|there| {
                    (there.as_chunks::<32>().0.iter())
                        .zip(want.parents)
                        .all(|(there, parent)| *there == parent.0.0)
                });
            if !vouched {
                return Ok(first + to_u64(i));
            }
            next = at.end();
        }
        Ok(end)
    }

    /// The first `records` records, counted by their lengths: what vouching falls back to when
    /// the journal cannot vouch for them (there is none to read, or it is damaged: that damage
    /// is the journal's, which verify reports). They must keep `par.col`'s layout all the same:
    /// each record's parents start in `parents.log` where the record before it leaves off (a
    /// record with no parents points where the next one's parents start), record 0's at 0. A
    /// power loss may have zeroed the records past the checkpoint, and a zeroed entry says "no
    /// parents, at 0": taken for the last record's, it would end `parents.log` before the
    /// parents of the records before it, which a writer's [`Index::repair_tail`] would then
    /// cut. So the records past those this handle counts and the sorted views cover (vouched
    /// for, or synced before the views were written) are checked, with one read of their
    /// `par.col` entries and the one before them: a record out of that layout is
    /// [`StoreError::IndexCorrupt`].
    ///
    /// A writer may cut the records past the checkpoint after `records` were measured (a failed
    /// sync's [`Index::truncate_to`], or a repair after a power loss), never one the checkpoint
    /// passed nor one the sorted views cover. So the entries are read as `par.col` holds them
    /// when they are read ([`held_entries`]: measured again after a short read), and the count
    /// is of the records the cut keeps: those `par.col` still holds, and of them no more than
    /// `ids.col`, which a cut takes first, holds once they are read. A count below the records
    /// this handle counts or the views cover is no cut but damage, which the callers report
    /// ([`StoreError::IndexCorrupt`]). As the vouched count does, it fails with an I/O error
    /// (`UnexpectedEof`) only if the files are cut again meanwhile.
    fn by_length(&self, records: u64) -> Result<u64, StoreError> {
        let trusted = self.len.max(self.views.covered());
        if trusted >= records {
            return Ok(records);
        }
        let from = trusted.saturating_sub(1);
        let entries = held_entries(&self.pars, from, records)?;
        let entries = entries.as_chunks::<8>().0;
        // What the cut keeps: the records par.col holds, bounded by ids.col measured after it.
        let ids = AppendOnlyColumn::<32>::whole(self.ids.len()?);
        let held = (from + to_u64(entries.len())).min(ids);
        let checked = usize::try_from(held.saturating_sub(from)).unwrap_or(usize::MAX);
        let mut ats = (entries.iter().take(checked))
            .map(|entry| ParentsAt::from_entry(u64::from_le_bytes(*entry)));
        // Where the record before the first one checked leaves off: at 0 before record 0.
        let mut next = match trusted {
            0 => 0,
            _ => ats.next().map_or(0, ParentsAt::end),
        };
        for at in ats {
            if at.offset != next {
                return Err(StoreError::IndexCorrupt {
                    what: "a record's parents do not start where the record before it leaves off \
                           in parents.log",
                });
            }
            next = at.end();
        }
        Ok(held)
    }

    /// Whether the files end exactly where this handle's records do.
    fn ends_where_counted(&self) -> io::Result<bool> {
        Ok(self.parents.holds(self.parents_len)?
            && self.pars.holds(self.len)?
            && self.gens.holds(self.len)?
            && self.dhs.holds(self.len)?
            && self.ids.holds(self.len)?)
    }

    /// The five files in the order [`Index::append`] writes them.
    fn write_order(&self) -> [&Utf8Path; 5] {
        [
            self.parents.path(),
            self.pars.path(),
            self.gens.path(),
            self.dhs.path(),
            self.ids.path(),
        ]
    }
}

impl fmt::Debug for Index {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Index")
            .field("sorted_dir", &self.sorted_dir)
            .field("len", &self.len)
            .field("parents_len", &self.parents_len)
            .field("views", &self.views)
            .field("sync_threads", &self.sync_threads)
            .finish_non_exhaustive()
    }
}

/// `rec`, read from `view`, if the view covers it: a view of m entries holds the recs `[0, m)`,
/// so a rec at or past m is damage (a bit flipped in a `.rec` file), never a record to read. It
/// would point into the unsorted tail (another changeset's record) or past the index's end. The
/// same rule as a compaction's when it merges the view; an in-memory comparison, no read. A
/// wrong rec within `[0, m)` is verify's to find.
fn covered_by<const K: usize>(view: &SortedView<K>, rec: u64) -> Result<u64, StoreError> {
    if rec < view.len() {
        Ok(rec)
    } else {
        Err(StoreError::IndexCorrupt {
            what: "a sorted view points at a record it does not cover",
        })
    }
}

/// The entries `[from, to)` of `column`; `None` if the file is shorter now than when it was
/// measured (a writer cut what it found torn): records that are gone vouch for nothing.
fn read_or_cut<const W: usize>(
    column: &AppendOnlyColumn<W>,
    from: u64,
    to: u64,
) -> Result<Option<Vec<u8>>, StoreError> {
    match column.entries(from, to) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(StoreError::Io(e)) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(None),
        Err(e) => Err(e),
    }
}

/// The entries `[from, to)` of `column` that it holds: all of them, or, if the file is shorter
/// now than when it was measured, those before its end, measured again (one more `fstat` and
/// `read_at`). A writer cuts only records past the checkpoint, at the end (a failed sync's
/// [`Index::truncate_to`], or a repair after a power loss), so the records the checkpoint passed
/// are all still there. A file cut again in between is an I/O error (`UnexpectedEof`).
fn held_entries<const W: usize>(
    column: &AppendOnlyColumn<W>,
    from: u64,
    to: u64,
) -> Result<Vec<u8>, StoreError> {
    match column.entries(from, to) {
        Err(StoreError::Io(e)) if e.kind() == io::ErrorKind::UnexpectedEof => {
            let held = AppendOnlyColumn::<W>::whole(column.len()?).min(to);
            column.entries(from.min(held), held)
        }
        read => read,
    }
}

/// The id of the last changeset in the frames before frame `seq`, read back from it a batch
/// at a time (one frame, then twice as many each time, up to 256); `None` if none holds one.
fn last_changeset_before(journal: &Journal, seq: u64) -> Result<Option<ChangesetId>, StoreError> {
    let (mut end, mut batch) = (seq, 1);
    while end > 0 {
        let start = end.saturating_sub(batch);
        let count = usize::try_from(end - start).expect("a batch fits in usize");
        let frames = journal.frames_from(start, count)?;
        let last = frames.iter().rev().find_map(|frame| {
            frame.records.iter().rev().find_map(|record| match record {
                Record::Changeset { id, .. } => Some(*id),
                Record::RefMove { .. } | Record::Compacted { .. } => None,
            })
        });
        if last.is_some() {
            return Ok(last);
        }
        (end, batch) = (start, (batch * 2).min(256));
    }
    Ok(None)
}

/// What the files hold whole.
struct Whole {
    /// The records all five files hold whole.
    records: u64,
    /// The bytes of `parents.log` those records' parents take.
    parents_len: u64,
    /// The files' lengths, in write order: `parents.log`, `par.col`, `gen.col`, `dh.col`,
    /// `ids.col`.
    lens: [u64; 5],
}

/// The (key, rec) pairs of records `[from, to)` of `column`, read with one `read_at`, sorted
/// by key, then rec.
fn sorted_tail(
    column: &AppendOnlyColumn<32>,
    from: u64,
    to: u64,
) -> Result<Vec<([u8; 32], u64)>, StoreError> {
    let bytes = column.entries(from, to)?;
    let mut tail: Vec<([u8; 32], u64)> = bytes
        .as_chunks::<32>()
        .0
        .iter()
        .zip(from..)
        .map(|(key, rec)| (*key, rec))
        .collect();
    tail.sort_unstable();
    Ok(tail)
}

/// A hex prefix of an id: its digits as bytes (an odd last digit in the high half of its
/// byte), zeros after, so that it is also the least id with the prefix.
struct Prefix {
    low: [u8; 32],
    nibbles: usize,
}

impl Prefix {
    /// The prefix `hex` spells: 1 to 64 hex digits of either case, else
    /// [`StoreError::BadPrefix`].
    fn parse(hex: &str) -> Result<Prefix, StoreError> {
        let bad = || StoreError::BadPrefix(hex.to_owned());
        if hex.is_empty() || hex.len() > 64 {
            return Err(bad());
        }
        let mut low = [0; 32];
        for (i, digit) in hex.bytes().enumerate() {
            let nibble = char::from(digit)
                .to_digit(16)
                .and_then(|nibble| u8::try_from(nibble).ok())
                .ok_or_else(bad)?;
            low[i / 2] |= if i.is_multiple_of(2) {
                nibble << 4
            } else {
                nibble
            };
        }
        Ok(Prefix {
            low,
            nibbles: hex.len(),
        })
    }

    fn matches(&self, key: &[u8; 32]) -> bool {
        let whole = self.nibbles / 2;
        key[..whole] == self.low[..whole]
            && (self.nibbles.is_multiple_of(2) || key[whole] >> 4 == self.low[whole] >> 4)
    }
}
