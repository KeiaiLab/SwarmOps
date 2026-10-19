//! The sorted views of the changeset index (storage spec §3, §3.1): for the id column and
//! the delta-hash column, a key file and a rec file, parallel arrays in key order. A lookup
//! binary-searches the key file alone, one `read_at` of one key per probe, and reads one rec
//! once it has found its key.
//!
//! ```text
//! sorted/ids.key   id[32] × m, strictly ascending       sorted/ids.rec   rec u64le × m
//! sorted/dh.key    delta_hash[32] × m, by (key, rec)    sorted/dh.rec    rec u64le × m
//! ```
//!
//! A view covers the index records `[0, m)`; the records past it, the unsorted tail, are
//! found by scanning the index columns. View files are never changed in place: a compaction
//! writes each to `<name>.tmp` with plain appends, syncs the temp files, then renames them
//! over the view files in the order `ids.key`, `ids.rec`, `dh.key`, `dh.rec`, each rename
//! made durable before the next. A crash between two renames leaves the files before it new
//! and the rest old; the entry counts then never grow along that order, so any other shape
//! is damage, [`StoreError::IndexCorrupt`].

use std::fs::File;
use std::io;
use std::ops::Range;
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicU64, Ordering};

use camino::{Utf8Path, Utf8PathBuf};

use super::to_u64;
use crate::error::StoreError;
use crate::fs_ops::{FsOps, open_regular};

/// How many entries a compaction reads, and writes, with one call.
const BLOCK: usize = 4096;

/// How many keys a scan from a lower bound reads with one `read_at`.
const RUN_BLOCK: usize = 64;

/// The size of a rec entry.
const REC_LEN: usize = 8;

/// One sorted view: `len()` keys of `K` bytes in ascending order in one file, and in another
/// the index record (rec) each key belongs to, in the same order.
///
/// It counts its reads, for tests and diagnostics: [`SortedView::probes`] the reads of the
/// key file, [`SortedView::rec_reads`] those of the rec file.
#[derive(Debug)]
pub struct SortedView<const K: usize> {
    /// The key file and the rec file, read-only; `None` exactly when `len` is 0.
    files: Option<(File, File)>,
    len: u64,
    probes: AtomicU64,
    rec_reads: AtomicU64,
}

impl<const K: usize> SortedView<K> {
    /// A view of no entries.
    pub(crate) fn empty() -> Self {
        Self {
            files: None,
            len: 0,
            probes: AtomicU64::new(0),
            rec_reads: AtomicU64::new(0),
        }
    }

    /// The view held by a key file and a rec file, each with its entry count. If the counts
    /// differ the files are not one view (a compaction is renaming them, or crashed doing
    /// so), and the view is empty.
    fn pair((keys, len): (File, u64), (recs, rec_count): (File, u64)) -> Self {
        if len == rec_count && len > 0 {
            Self {
                files: Some((keys, recs)),
                len,
                probes: AtomicU64::new(0),
                rec_reads: AtomicU64::new(0),
            }
        } else {
            Self::empty()
        }
    }

    /// How many entries the view holds.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Whether the view holds no entries.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The index of the first key not less than `key` (keys compare byte by byte, as
    /// slices), or `len()` if every key is less. A binary search that reads one key per
    /// probe with `read_at`, at most ⌊log2 len⌋ + 1 of them, and nothing of the rec file.
    pub fn lower_bound(&self, key: &[u8]) -> Result<u64, StoreError> {
        let (mut low, mut high) = (0, self.len);
        while low < high {
            let mid = low + (high - low) / 2;
            if self.key_at(mid)?.as_slice() < key {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        Ok(low)
    }

    /// Key `i`, read with one `read_at`. `i` past the end is [`StoreError::BadCall`].
    pub fn key_at(&self, i: u64) -> Result<[u8; K], StoreError> {
        let mut key = [0; K];
        self.read_keys(i, &mut key)?;
        Ok(key)
    }

    /// The rec of key `i`, read with one `read_at`. `i` past the end is
    /// [`StoreError::BadCall`]. The rec itself is as the file holds it: the index's lookups
    /// check it against what the view covers.
    pub fn rec_at(&self, i: u64) -> Result<u64, StoreError> {
        let mut rec = [0; REC_LEN];
        self.read_recs(i, &mut rec)?;
        Ok(u64::from_le_bytes(rec))
    }

    /// How many reads of the key file this view has made since it was opened: one per key a
    /// search probes, and one per block of keys a scan or a compaction reads.
    pub fn probes(&self) -> u64 {
        self.probes.load(Ordering::Relaxed)
    }

    /// How many reads of the rec file this view has made since it was opened.
    pub fn rec_reads(&self) -> u64 {
        self.rec_reads.load(Ordering::Relaxed)
    }

    /// Where the run of keys from `start` on that `matches` accepts ends: the index of the
    /// first key from `start` on that it rejects, or `len()`. Reads 64 keys per `read_at`.
    pub(crate) fn run_end(
        &self,
        start: u64,
        mut matches: impl FnMut(&[u8; K]) -> bool,
    ) -> Result<u64, StoreError> {
        let mut buf = Vec::new();
        let mut at = start;
        while at < self.len {
            let count = entries_from(at, self.len, RUN_BLOCK);
            buf.resize(count * K, 0);
            self.read_keys(at, &mut buf)?;
            if let Some(stop) = buf.as_chunks::<K>().0.iter().position(|key| !matches(key)) {
                return Ok(at + to_u64(stop));
            }
            at += to_u64(count);
        }
        Ok(at)
    }

    /// The recs of the keys in `range`, read with one `read_at` (none for an empty range).
    pub(crate) fn recs(&self, range: Range<u64>) -> Result<Vec<u64>, StoreError> {
        if range.is_empty() {
            return Ok(Vec::new());
        }
        let count = usize::try_from(range.end - range.start)
            .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
        let mut bytes = vec![0; count * REC_LEN];
        self.read_recs(range.start, &mut bytes)?;
        Ok(bytes
            .as_chunks::<REC_LEN>()
            .0
            .iter()
            .map(|rec| u64::from_le_bytes(*rec))
            .collect())
    }

    /// Fills `buf`, a whole number of keys, with the keys from key `first` on, in one
    /// `read_at`.
    fn read_keys(&self, first: u64, buf: &mut [u8]) -> Result<(), StoreError> {
        let (keys, _) = self.files_for(first, buf.len() / K)?;
        self.probes.fetch_add(1, Ordering::Relaxed);
        keys.read_exact_at(buf, first * to_u64(K))?;
        Ok(())
    }

    /// Fills `buf`, a whole number of recs, with the recs from entry `first` on, in one
    /// `read_at`.
    fn read_recs(&self, first: u64, buf: &mut [u8]) -> Result<(), StoreError> {
        let (_, recs) = self.files_for(first, buf.len() / REC_LEN)?;
        self.rec_reads.fetch_add(1, Ordering::Relaxed);
        recs.read_exact_at(buf, first * to_u64(REC_LEN))?;
        Ok(())
    }

    /// The files, if `count` entries from entry `first` on exist and `count` is not 0;
    /// otherwise [`StoreError::BadCall`]: no lookup asks for entries past the view's end.
    fn files_for(&self, first: u64, count: usize) -> Result<&(File, File), StoreError> {
        let end = first.checked_add(to_u64(count));
        match &self.files {
            Some(files) if count > 0 && end.is_some_and(|end| end <= self.len) => Ok(files),
            _ => Err(StoreError::BadCall(format!(
                "entries {first}..+{count} are not within a sorted view of {} entries",
                self.len
            ))),
        }
    }
}

/// The two sorted views of the changeset index, by id and by delta hash, as one handle
/// opened them.
#[derive(Debug)]
pub(crate) struct Views {
    pub(crate) ids: SortedView<32>,
    pub(crate) dh: SortedView<32>,
    /// The entry counts of `ids.key`, `ids.rec`, `dh.key` and `dh.rec`, in the order a
    /// compaction renames them.
    counts: [u64; 4],
}

impl Views {
    /// Opens the views in `dir`, `meta/sorted/`. A view whose key and rec files hold
    /// different counts is mid-rename (or a crash stopped one there) and is opened empty, so
    /// its records count as tail.
    ///
    /// Counts that grow along the rename order are [`StoreError::IndexCorrupt`], and so is
    /// a file that ends in a partial entry, or one that is missing: the store's creation
    /// made all four ([`crate::Index::create`]), and a compaction only renames over them.
    pub(crate) fn open(dir: &Utf8Path) -> Result<Views, StoreError> {
        // The reverse of the rename order: once a file is seen new, every file opened after
        // it was renamed before it and is seen new too, so the counts seen never grow along
        // the rename order, however many compactions run meanwhile.
        let dh_rec = open_entries(&dir.join(DH_REC), REC_LEN)?;
        let dh_key = open_entries(&dir.join(DH_KEY), 32)?;
        let ids_rec = open_entries(&dir.join(IDS_REC), REC_LEN)?;
        let ids_key = open_entries(&dir.join(IDS_KEY), 32)?;
        let counts = [ids_key.1, ids_rec.1, dh_key.1, dh_rec.1];
        if counts.windows(2).any(|pair| pair[0] < pair[1]) {
            return Err(StoreError::IndexCorrupt {
                what: "the sorted views are out of the order compaction renames them in",
            });
        }
        Ok(Views {
            ids: SortedView::pair(ids_key, ids_rec),
            dh: SortedView::pair(dh_key, dh_rec),
            counts,
        })
    }

    /// Whether the four files hold two whole views of the same records; false while a
    /// compaction renames them, or after one crashed doing so.
    pub(crate) fn in_step(&self) -> bool {
        // The counts never grow along the rename order: all equal iff the ends are.
        self.counts[0] == self.counts[3]
    }

    /// The records both views cover.
    pub(crate) fn sorted_count(&self) -> u64 {
        self.ids.len().min(self.dh.len())
    }

    /// The most records any of the four files covers, out of step or not: `ids.key`'s count,
    /// the largest, since the counts never grow along the rename order.
    pub(crate) fn covered(&self) -> u64 {
        self.counts[0]
    }

    /// Refuses views that cover more records than `records`, the number the index columns
    /// hold whole.
    pub(crate) fn check_within(&self, records: u64) -> Result<(), StoreError> {
        if self.covered() > records {
            return Err(StoreError::IndexCorrupt {
                what: "a sorted view covers records the index columns do not hold",
            });
        }
        Ok(())
    }
}

const IDS_KEY: &str = "ids.key";
const IDS_REC: &str = "ids.rec";
const DH_KEY: &str = "dh.key";
const DH_REC: &str = "dh.rec";

/// The four view files in `meta/sorted/`, in the order a compaction renames them.
pub(crate) const VIEW_FILES: [&str; 4] = [IDS_KEY, IDS_REC, DH_KEY, DH_REC];

/// Which of the two views.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ViewName {
    /// By changeset id: each key once.
    Ids,
    /// By delta hash: a key once per record that has it, those in rec order.
    DeltaHash,
}

impl ViewName {
    /// The key file and the rec file.
    fn files(self) -> (&'static str, &'static str) {
        match self {
            Self::Ids => (IDS_KEY, IDS_REC),
            Self::DeltaHash => (DH_KEY, DH_REC),
        }
    }
}

/// One view for [`rebuild`] to write.
pub(crate) struct Rebuild<'a> {
    pub(crate) view: ViewName,
    /// The view as it is; its entries are merged into the new one.
    pub(crate) old: &'a SortedView<32>,
    /// The (key, rec) entries of the records `old` does not cover, sorted.
    pub(crate) tail: Vec<([u8; 32], u64)>,
}

/// Writes each view of `rebuilds` (the ids view first, if it is among them) to temp files in
/// `dir`, its old entries merged with its tail in (key, rec) order, a block at a time; then
/// syncs the temp files; then renames them over the view files, key file then rec file, each
/// rename made durable before the next. Temp files a crash left behind are removed first.
///
/// Entries out of order (a damaged view), or an id twice in the ids view, are
/// [`StoreError::IndexCorrupt`]; nothing is renamed then.
pub(crate) fn rebuild(
    fs: &dyn FsOps,
    dir: &Utf8Path,
    rebuilds: &[Rebuild<'_>],
) -> Result<(), StoreError> {
    let mut renames: Vec<(Utf8PathBuf, Utf8PathBuf)> = Vec::with_capacity(2 * rebuilds.len());
    for rebuild in rebuilds {
        let (key_name, rec_name) = rebuild.view.files();
        let key_tmp = dir.join(format!("{key_name}.tmp"));
        let rec_tmp = dir.join(format!("{rec_name}.tmp"));
        remove_stale(fs, &key_tmp)?;
        remove_stale(fs, &rec_tmp)?;
        let mut out = Writer {
            fs,
            key_path: &key_tmp,
            rec_path: &rec_tmp,
            unique_keys: rebuild.view == ViewName::Ids,
            keys: Vec::with_capacity(BLOCK * 32),
            recs: Vec::with_capacity(BLOCK * REC_LEN),
            last: None,
            flushed: false,
        };
        merge(rebuild.old, &rebuild.tail, &mut out)?;
        out.finish()?;
        renames.push((key_tmp, dir.join(key_name)));
        renames.push((rec_tmp, dir.join(rec_name)));
    }
    for (temp, _) in &renames {
        fs.sync_data(temp.as_std_path())?;
    }
    for (temp, path) in &renames {
        fs.rename_durable(temp.as_std_path(), path.as_std_path())?;
    }
    Ok(())
}

/// Removes `path` if it exists.
fn remove_stale(fs: &dyn FsOps, path: &Utf8Path) -> Result<(), StoreError> {
    match fs.remove(path.as_std_path()) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

/// Feeds `out` the entries of `old`, read a block at a time, and those of `tail`, merged
/// in (key, rec) order.
fn merge<const K: usize>(
    old: &SortedView<K>,
    tail: &[([u8; K], u64)],
    out: &mut Writer<'_, K>,
) -> Result<(), StoreError> {
    let mut old = Entries::new(old);
    let mut tail = tail.iter().copied().peekable();
    loop {
        let next = match (old.peek()?, tail.peek().copied()) {
            (Some(from_old), Some(from_tail)) if from_old <= from_tail => {
                old.advance();
                from_old
            }
            (_, Some(from_tail)) => {
                tail.next();
                from_tail
            }
            (Some(from_old), None) => {
                old.advance();
                from_old
            }
            (None, None) => return Ok(()),
        };
        out.push(next)?;
    }
}

/// The entries of a view in order, read a block of keys and a block of recs at a time.
struct Entries<'a, const K: usize> {
    view: &'a SortedView<K>,
    keys: Vec<u8>,
    recs: Vec<u8>,
    /// The index of the first buffered entry.
    first: u64,
    /// How many entries are buffered.
    buffered: usize,
    /// The next buffered entry to yield.
    next: usize,
}

impl<'a, const K: usize> Entries<'a, K> {
    fn new(view: &'a SortedView<K>) -> Self {
        Self {
            view,
            keys: Vec::new(),
            recs: Vec::new(),
            first: 0,
            buffered: 0,
            next: 0,
        }
    }

    /// The next entry, reading the next block if need be; `None` past the last.
    fn peek(&mut self) -> Result<Option<([u8; K], u64)>, StoreError> {
        if self.next == self.buffered {
            let start = self.first + to_u64(self.buffered);
            let count = entries_from(start, self.view.len, BLOCK);
            if count == 0 {
                return Ok(None);
            }
            self.keys.resize(count * K, 0);
            self.recs.resize(count * REC_LEN, 0);
            self.view.read_keys(start, &mut self.keys)?;
            self.view.read_recs(start, &mut self.recs)?;
            (self.first, self.buffered, self.next) = (start, count, 0);
        }
        let key = self.keys.as_chunks::<K>().0[self.next];
        let rec = u64::from_le_bytes(self.recs.as_chunks::<REC_LEN>().0[self.next]);
        if rec >= self.view.len {
            return Err(StoreError::IndexCorrupt {
                what: "a sorted view points at a record it does not cover",
            });
        }
        Ok(Some((key, rec)))
    }

    fn advance(&mut self) {
        self.next += 1;
    }
}

/// Writes a view's entries to its two temp files, a block at a time.
struct Writer<'a, const K: usize> {
    fs: &'a dyn FsOps,
    key_path: &'a Utf8Path,
    rec_path: &'a Utf8Path,
    /// Whether each key may appear once only (the ids view).
    unique_keys: bool,
    keys: Vec<u8>,
    recs: Vec<u8>,
    last: Option<([u8; K], u64)>,
    flushed: bool,
}

impl<const K: usize> Writer<'_, K> {
    fn push(&mut self, entry: ([u8; K], u64)) -> Result<(), StoreError> {
        if let Some(last) = self.last {
            let in_order = if self.unique_keys {
                last.0 < entry.0
            } else {
                last < entry
            };
            if !in_order {
                return Err(StoreError::IndexCorrupt {
                    what: if self.unique_keys {
                        "a sorted view is out of order, or the index holds an id twice"
                    } else {
                        "a sorted view is out of order"
                    },
                });
            }
        }
        self.last = Some(entry);
        self.keys.extend_from_slice(&entry.0);
        self.recs.extend_from_slice(&entry.1.to_le_bytes());
        if self.keys.len() == BLOCK * K {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), StoreError> {
        self.fs.append(self.key_path.as_std_path(), &self.keys)?;
        self.fs.append(self.rec_path.as_std_path(), &self.recs)?;
        self.keys.clear();
        self.recs.clear();
        self.flushed = true;
        Ok(())
    }

    /// Writes what is buffered; creates the files if nothing was written at all.
    fn finish(mut self) -> Result<(), StoreError> {
        if !self.keys.is_empty() || !self.flushed {
            self.flush()?;
        }
        Ok(())
    }
}

/// Opens `path` read-only with its length in entries of `width` bytes;
/// [`StoreError::IndexCorrupt`] if it is missing or ends in a partial entry.
fn open_entries(path: &Utf8Path, width: usize) -> Result<(File, u64), StoreError> {
    let file = open_regular(path).map_err(index_file_error)?;
    let len = file.metadata()?.len();
    let width = to_u64(width);
    if !len.is_multiple_of(width) {
        return Err(StoreError::IndexCorrupt {
            what: "a sorted view file ends in a partial entry",
        });
    }
    Ok((file, len / width))
}

/// What a failure to open an index file is: [`StoreError::IndexCorrupt`] if the file is not
/// there, since a complete store has every index file; the I/O error otherwise.
pub(crate) fn index_file_error(e: io::Error) -> StoreError {
    if e.kind() == io::ErrorKind::NotFound {
        StoreError::IndexCorrupt {
            what: "an index file is missing",
        }
    } else {
        e.into()
    }
}

/// How many entries from index `start` a read of at most `max` takes, in a view of `len`.
fn entries_from(start: u64, len: u64, max: usize) -> usize {
    usize::try_from(len.saturating_sub(start)).map_or(max, |left| left.min(max))
}
