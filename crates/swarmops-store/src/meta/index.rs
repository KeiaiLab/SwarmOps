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
//! number, its rec, is its position in the columns and its `seq`. The files only grow, by
//! [`Index::append`], which writes `parents.log`, `par.col`, `gen.col`, `dh.col` and
//! `ids.col`, in that order, with plain appends; [`Index::sync`] then makes them durable,
//! once for many appends. Lookups binary-search a sorted view's key column, then scan the
//! records past the view, the unsorted tail, read from `ids.col` (or `dh.col`) with one
//! `read_at`; [`Index::compact`] merges the tail into the views.
//!
//! # Crashes and concurrency
//!
//! Writers hold [`crate::MetaLock`]; readers take no lock and may open the index while a
//! writer appends or compacts. A crash in the middle of an append leaves the record's first
//! files written and the rest not. Every handle counts only the records that all five files
//! hold whole (`ids.col`, written last, is the one that lags behind), and
//! [`Index::repair_tail`] cuts the rest off. A crash in the middle of a compaction's renames
//! leaves the sorted views out of step; each view still covers the records it covers, or is
//! read as empty, so lookups stay right, and [`Index::repair_tail`] runs the compaction
//! again.

use std::fs::File;
use std::io;
use std::num::NonZero;
use std::os::unix::fs::FileExt;
use std::panic;
use std::sync::Arc;
use std::thread;

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_core::{ChangesetId, ObjectId};

use super::parents::{self, MAX_PARENTS, MAX_PARENTS_LEN, PARENT_LEN, ParentsAt};
use super::sorted::{self, Rebuild, SortedView, VIEW_FILES, ViewName, Views, index_file_error};
use crate::error::StoreError;
use crate::fs_ops::FsOps;
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
    ids: Column<32>,
    gens: Column<8>,
    dhs: Column<32>,
    pars: Column<8>,
    parents_path: Utf8PathBuf,
    /// Read-only.
    parents: File,
    /// The records this handle counts.
    len: u64,
    /// The bytes of `parents.log` those records' parents take.
    parents_len: u64,
    views: Views,
    /// How many threads [`Index::sync`] syncs the five files from: see
    /// [`Index::set_parallelism`].
    sync_threads: usize,
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
    /// sorted views' lengths. It repairs nothing, since a writer may be appending or
    /// compacting meanwhile: records that some files lack are not counted, and a view that a
    /// compaction is renaming (or crashed renaming) counts as covering what it covers, or
    /// nothing. A writer repairs with [`Index::repair_tail`].
    pub fn open(layout: &Layout, fs: Arc<dyn FsOps>) -> Result<Index, StoreError> {
        let sorted_dir = layout.meta_path("sorted");
        // The views before the columns: a compaction covers records that were whole when it
        // ran, and whole records stay, so the columns read afterwards hold all that the
        // views cover.
        let views = Views::open(&sorted_dir)?;
        let parents_path = layout.meta_path(PARENTS);
        let mut index = Index {
            ids: Column::open(layout.meta_path(IDS)).map_err(index_file_error)?,
            gens: Column::open(layout.meta_path(GENS)).map_err(index_file_error)?,
            dhs: Column::open(layout.meta_path(DHS)).map_err(index_file_error)?,
            pars: Column::open(layout.meta_path(PARS)).map_err(index_file_error)?,
            parents: File::open(&parents_path).map_err(index_file_error)?,
            parents_path,
            fs,
            sorted_dir,
            len: 0,
            parents_len: 0,
            views,
            sync_threads: 1,
        };
        let whole = index.whole()?;
        (index.len, index.parents_len) = (whole.records, whole.parents_len);
        index.views.check_within(index.len)?;
        Ok(index)
    }

    /// How many records this handle counts.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Whether this handle counts no records.
    pub fn is_empty(&self) -> bool {
        self.len == 0
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
    /// and nothing is cut or written then. More than 255 parents is an `InvalidInput` error,
    /// and nothing is written then either. After any other error, what was written is cut
    /// off by the next append or repair.
    pub fn append(&mut self, meta: &ChangesetMeta) -> Result<u64, StoreError> {
        let count = u8::try_from(meta.parents.len()).map_err(|_| {
            invalid_input(format!(
                "a changeset has at most {MAX_PARENTS} parents, not {}",
                meta.parents.len()
            ))
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
        self.append_at(&self.parents_path, parents, at.offset)?;
        self.append_at(&self.pars.path, &at.entry().to_le_bytes(), rec * 8)?;
        self.append_at(&self.gens.path, &meta.generation.to_le_bytes(), rec * 8)?;
        self.append_at(&self.dhs.path, &meta.delta_hash, rec * 32)?;
        self.append_at(&self.ids.path, &meta.id.0.0, rec * 32)?;
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
    /// the error of the first file in write order that failed comes back. ext4 commits
    /// concurrent fsyncs together: five at once cost about two (docs/perf.md).
    pub fn sync(&self) -> Result<(), StoreError> {
        let files = self.write_order();
        let fs = &*self.fs;
        let sync_all = |share: &[&Utf8PathBuf]| -> io::Result<()> {
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
                .map(|share| scope.spawn(move || sync_all(share)))
                .collect();
            syncs
                .into_iter()
                .map(|sync| {
                    sync.join()
                        .unwrap_or_else(|panic| panic::resume_unwind(panic))
                })
                .collect()
        });
        Ok(results.into_iter().collect::<io::Result<()>>()?)
    }

    /// Record `rec`: four `read_at`s of the columns, and one of `parents.log` if it has
    /// parents. A rec this handle does not count is an `InvalidInput` error; parents off
    /// `parents.log` are [`StoreError::IndexCorrupt`].
    pub fn get(&self, rec: u64) -> Result<ChangesetMeta, StoreError> {
        if rec >= self.len {
            return Err(invalid_input(format!(
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
    /// tail of `ids.col`, read with one `read_at`.
    pub fn find(&self, id: ChangesetId) -> Result<Option<u64>, StoreError> {
        let key = id.0.0;
        let view = &self.views.ids;
        let at = view.lower_bound(&key)?;
        if at < view.len() && view.key_at(at)? == key {
            return view.rec_at(at).map(Some);
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
    /// its byte. Anything else is an `InvalidInput` error. A lower-bound search of the id
    /// view, a forward scan of its keys while they match, one read of their recs, and a scan
    /// of the tail of `ids.col`.
    pub fn find_prefix(&self, prefix_nibbles: &str) -> Result<Vec<u64>, StoreError> {
        let prefix = Prefix::parse(prefix_nibbles)?;
        let view = &self.views.ids;
        let start = view.lower_bound(&prefix.low)?;
        let end = view.run_end(start, |key| prefix.matches(key))?;
        let mut recs = view.recs(start..end)?;
        let from = view.len();
        let tail = self.ids.entries(from, self.len)?;
        recs.extend(
            (from..)
                .zip(tail.as_chunks::<32>().0)
                .filter(|(_, key)| prefix.matches(key))
                .map(|(rec, _)| rec),
        );
        recs.sort_unstable();
        Ok(recs)
    }

    /// The recs, ascending, of the changesets whose delta hash is `dh`: changesets that make
    /// the same change on different bases, as a clean rebase does. Searched like
    /// [`Index::find_prefix`], in the delta-hash view and the tail of `dh.col`.
    pub fn find_delta_hash(&self, dh: &[u8; 32]) -> Result<Vec<u64>, StoreError> {
        let view = &self.views.dh;
        let start = view.lower_bound(dh)?;
        let end = view.run_end(start, |key| key == dh)?;
        let mut recs = view.recs(start..end)?;
        let from = view.len();
        let tail = self.dhs.entries(from, self.len)?;
        recs.extend(
            (from..)
                .zip(tail.as_chunks::<32>().0)
                .filter(|(_, key)| *key == dh)
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
    ///   sync, any of them can lag). Every column is cut to the records that all five files
    ///   hold whole, and `parents.log` to the end of the last such record's parents (its
    ///   `par.col` entry says where). The records cut, counted as slots of the longest
    ///   column, are returned: their journal frames lie past `journal.ckpt`, since the
    ///   checkpoint moves only after [`Index::sync`], so replay appends them again.
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
        let dropped = self.catch_up()?;
        if !self.views.in_step() {
            self.rebuild_views()?;
        }
        Ok(dropped)
    }

    /// Reopens the sorted views (another handle may have compacted since this one opened
    /// them) and repairs the columns; returns how many records were dropped. On an error the
    /// handle is left as it was.
    fn catch_up(&mut self) -> Result<u64, StoreError> {
        let views = Views::open(&self.sorted_dir)?;
        let (whole, dropped) = self.repair_files(&views)?;
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
    /// damaged) or than `views` cover: [`StoreError::IndexCorrupt`].
    fn repair_files(&self, views: &Views) -> Result<(Whole, u64), StoreError> {
        let whole = self.whole()?;
        if whole.records < self.len {
            return Err(StoreError::IndexCorrupt {
                what: "records this handle counted are no longer whole in every index file",
            });
        }
        views.check_within(whole.records)?;
        let [parents, pars, gens, dhs, ids] = whole.lens;
        let mut dropped = 0;
        // Cut in the reverse of the write order, so that ids.col goes first.
        let columns = [
            (&self.ids.path, ids, 32),
            (&self.dhs.path, dhs, 32),
            (&self.gens.path, gens, 8),
            (&self.pars.path, pars, 8),
        ];
        for (path, len, width) in columns {
            let keep = whole.records * width;
            if len != keep {
                dropped = dropped.max(len.div_ceil(width) - whole.records);
                self.fs.truncate(path.as_std_path(), keep)?;
            }
        }
        if parents != whole.parents_len {
            self.fs
                .truncate(self.parents_path.as_std_path(), whole.parents_len)?;
        }
        Ok((whole, dropped))
    }

    /// What the files hold whole now, judged from their lengths (one `fstat` each) and the
    /// `par.col` entry of the last record that all four columns hold.
    fn whole(&self) -> Result<Whole, StoreError> {
        // ids.col first: append writes it last, so with no power lost a record it holds is
        // whole in the files read after it. The minimum covers a power loss as well.
        let ids = self.ids.byte_len()?;
        let dhs = self.dhs.byte_len()?;
        let gens = self.gens.byte_len()?;
        let pars = self.pars.byte_len()?;
        let parents = self.parents.metadata()?.len();
        let mut records = (ids / 32).min(dhs / 32).min(gens / 8).min(pars / 8);
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
            lens: [parents, pars, gens, dhs, ids],
        })
    }

    /// Whether the files end exactly where this handle's records do.
    fn ends_where_counted(&self) -> io::Result<bool> {
        Ok(self.parents.metadata()?.len() == self.parents_len
            && self.pars.byte_len()? == self.len * 8
            && self.gens.byte_len()? == self.len * 8
            && self.dhs.byte_len()? == self.len * 32
            && self.ids.byte_len()? == self.len * 32)
    }

    /// Appends `bytes` to `path`, which must be `expected` bytes long until then.
    fn append_at(&self, path: &Utf8Path, bytes: &[u8], expected: u64) -> Result<(), StoreError> {
        if self.fs.append(path.as_std_path(), bytes)? == expected {
            Ok(())
        } else {
            Err(StoreError::IndexCorrupt {
                what: "an index file changed during an append: a writer without meta/lock?",
            })
        }
    }

    /// The five files in the order [`Index::append`] writes them.
    fn write_order(&self) -> [&Utf8PathBuf; 5] {
        [
            &self.parents_path,
            &self.pars.path,
            &self.gens.path,
            &self.dhs.path,
            &self.ids.path,
        ]
    }
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

/// One column file: `W` bytes per record, record `rec` at byte `rec × W`.
struct Column<const W: usize> {
    path: Utf8PathBuf,
    /// Read-only.
    file: File,
}

impl<const W: usize> Column<W> {
    fn open(path: Utf8PathBuf) -> io::Result<Self> {
        let file = File::open(&path)?;
        Ok(Self { path, file })
    }

    fn byte_len(&self) -> io::Result<u64> {
        Ok(self.file.metadata()?.len())
    }

    /// Record `rec`'s entry.
    fn entry(&self, rec: u64) -> io::Result<[u8; W]> {
        let mut entry = [0; W];
        self.file.read_exact_at(&mut entry, rec * to_u64(W))?;
        Ok(entry)
    }

    /// The entries of records `[from, to)`, read with one `read_at` (none for no records).
    /// A range that starts past its end is [`StoreError::IndexCorrupt`].
    fn entries(&self, from: u64, to: u64) -> Result<Vec<u8>, StoreError> {
        let Some(count) = to.checked_sub(from) else {
            return Err(StoreError::IndexCorrupt {
                what: "a sorted view covers records past those the index counts",
            });
        };
        let len = usize::try_from(count * to_u64(W))
            .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
        let mut bytes = vec![0; len];
        if len > 0 {
            self.file.read_exact_at(&mut bytes, from * to_u64(W))?;
        }
        Ok(bytes)
    }
}

/// The (key, rec) pairs of records `[from, to)` of `column`, read with one `read_at`, sorted
/// by key, then rec.
fn sorted_tail(
    column: &Column<32>,
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
    fn parse(hex: &str) -> Result<Prefix, StoreError> {
        if hex.is_empty() || hex.len() > 64 {
            return Err(invalid_input(format!(
                "an id prefix is 1 to 64 hex digits, not {} bytes",
                hex.len()
            )));
        }
        let mut low = [0; 32];
        for (i, digit) in hex.bytes().enumerate() {
            let nibble = char::from(digit)
                .to_digit(16)
                .and_then(|nibble| u8::try_from(nibble).ok())
                .ok_or_else(|| invalid_input(format!("id prefix {hex:?} is not hex")))?;
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

fn invalid_input(message: String) -> StoreError {
    io::Error::new(io::ErrorKind::InvalidInput, message).into()
}

fn to_u64(n: usize) -> u64 {
    u64::try_from(n).expect("usize fits in u64")
}
