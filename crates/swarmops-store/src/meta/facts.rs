//! FORMAT 3's fact index (the FORMAT 3 design §2.8): `meta/idx/facts.col`, one row per fact in
//! frame order, and the lookups by subject and by other.
//!
//! ```text
//! row (24 B) = u64le offset | u32le len | u8 kind | u8 type | u16le 0 | u64le frame_seq
//!
//! facts.col:  [row 0 .......... covered_1)[row ...................... len)
//!              fact runs (domain 1):        the tail: each row's record read
//!              subject view, other view     and compared
//! ```
//!
//! A row names its fact's bytes in `journal2.log`: a lookup finds the row numbers in the fact
//! runs, then reads each fact with one `read_at` of the journal and checks it against the row and
//! the key; the rows past the runs are read the same way, one record each. Replay appends the
//! rows of the frames it materializes ([`FactRows::append`]), with no sync: `facts.col` is the
//! sixth index file, synced with the other five before `journal3.ckpt` passes it (W3). Rows the
//! checkpoint passed are trusted; a row past it counts only while the journal vouches for it,
//! field by field (the storage spec §3's rule for the changeset index): what a power loss left
//! there is cut by the next writer and appended again from the journal.

use std::fs::File;
use std::io;
use std::ops::ControlFlow;
use std::os::unix::fs::FileExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use camino::Utf8PathBuf;
use swarmops_core::bytes;

use super::column::AppendOnlyColumn;
use super::features;
use super::frame::{self, Base, Fact, Frame, MAX_FACT_BODY, Record};
use super::journal::Journal;
use super::sorted::{Pair, SortedView, Views};
use crate::error::{FileOp, StoreError};
use crate::format::StoreFormat;
use crate::fs_ops::{self, RealFs, StoreFs};
use crate::layout::Layout;

/// The bytes of one row of `facts.col`.
pub(crate) const ROW_LEN: usize = 24;

/// The record count a frame's payload starts with, before its records: a `u16le`.
const COUNT_LEN: u64 = 2;

/// The longest fact, in bytes: the envelope's 77 fixed bytes, the longest actor and the longest
/// body (the FORMAT 3 design §2.8). A row naming more is damage, refused before any read.
const MAX_FACT_LEN: usize = 77 + 255 + MAX_FACT_BODY;

/// The key a fact is found by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Side {
    /// Its subject: section 0 of a fact run.
    Subject,
    /// Its other, when not all zero: section 1 of a fact run.
    Other,
}

impl Side {
    /// The key `fact` is found by on this side.
    pub(crate) fn key_of(self, fact: &Fact) -> [u8; 32] {
        match self {
            Side::Subject => fact.subject(),
            Side::Other => fact.other(),
        }
    }

    /// This side's view of a fact run: its subject view, or its other view.
    fn view(self, pair: &Pair) -> &SortedView<32> {
        match self {
            Side::Subject => &pair.ids,
            Side::Other => &pair.dh,
        }
    }
}

/// One row of `facts.col`: where a fact lies in `journal2.log`, its kind, type and frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Row {
    /// Where the fact's record starts in `journal2.log`.
    pub(crate) offset: u64,
    /// The record's length in bytes.
    pub(crate) len: u32,
    /// Its kind number.
    pub(crate) kind: u8,
    /// Its type within the kind.
    pub(crate) ty: u8,
    /// The seq of the frame that holds it.
    pub(crate) seq: u64,
}

impl Row {
    /// Its 24 bytes (the FORMAT 3 design §2.8 table).
    pub(crate) fn encode(&self) -> [u8; ROW_LEN] {
        let mut bytes = [0; ROW_LEN];
        bytes[..8].copy_from_slice(&self.offset.to_le_bytes());
        bytes[8..12].copy_from_slice(&self.len.to_le_bytes());
        bytes[12] = self.kind;
        bytes[13] = self.ty;
        bytes[16..].copy_from_slice(&self.seq.to_le_bytes());
        bytes
    }

    /// The row `bytes` hold; its two zero bytes not zero is [`StoreError::IndexCorrupt`].
    pub(crate) fn decode(bytes: &[u8; ROW_LEN]) -> Result<Row, StoreError> {
        if bytes[14..16] != [0, 0] {
            return Err(StoreError::IndexCorrupt {
                what: "a fact row's reserved bytes are not zero",
            });
        }
        let u64_at = |at: usize| {
            let mut field = [0; 8];
            field.copy_from_slice(&bytes[at..at + 8]);
            u64::from_le_bytes(field)
        };
        Ok(Row {
            offset: u64_at(0),
            len: u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
            kind: bytes[12],
            ty: bytes[13],
            seq: u64_at(16),
        })
    }

    /// Where its record ends in `journal2.log`.
    fn end(&self) -> u64 {
        self.offset + u64::from(self.len)
    }
}

/// The rows of the facts `frames` hold, FORMAT 3 frames in journal order, each with its fact:
/// a record's place is its frame's offset, the header and the count, then the records before
/// it. A frame without a fact costs no encoding.
pub(crate) fn rows_of(frames: &[Frame]) -> Result<Vec<(Row, &Fact)>, StoreError> {
    let header = bytes::to_u64(frame::header_len(StoreFormat::V3));
    let mut rows = Vec::new();
    let mut scratch = Vec::new();
    for frame in frames {
        if !frame.records.iter().any(|r| matches!(r, Record::Fact(_))) {
            continue;
        }

        // Each record's length, the facts' rows on the way.
        let mut at = frame.offset + header + COUNT_LEN;
        for record in &frame.records {
            scratch.clear();
            record.encode(&mut scratch)?;
            let len = bytes::to_u64(scratch.len());
            if let Record::Fact(fact) = record {
                let row = Row {
                    offset: at,
                    len: u32::try_from(len).expect("a fact of at most 4,428 bytes"),
                    kind: fact.kind().code(),
                    ty: fact.ty(),
                    seq: frame.seq,
                };
                rows.push((row, fact));
            }
            at += len;
        }
    }
    Ok(rows)
}

/// A handle on `facts.col`: the rows it counts, the fact runs it searches, and the journal it
/// reads facts from. See the module documentation.
pub(crate) struct FactRows {
    column: AppendOnlyColumn<ROW_LEN>,
    /// `journal2.log`, read-only; `None` for an index on its own, with no journal (and so no
    /// fact to read: it counts no row the journal did not vouch for).
    log: Option<File>,
    log_path: Utf8PathBuf,
    fs: StoreFs,
    /// The rows this handle counts.
    len: u64,
    /// Where the last of them ends in `journal2.log`; 0 with none: the facts of frames past it
    /// are no row's.
    end: u64,
    /// The fact runs, by subject (`ids`) and by other (`dh`), in row order.
    views: Views,
    /// How many facts it has read from the journal.
    reads: AtomicU64,
}

impl FactRows {
    /// `facts.col` of `layout`, counting the `floor` rows the checkpoint passed and, past them,
    /// those `past` vouches for: the frames past the checkpoint, whose facts the rows past it
    /// must be, in order, field by field. Fewer rows than `floor` is
    /// [`StoreError::IndexCorrupt`]; a missing file too: the store's creation made it. `views`
    /// are the fact runs, which may cover no more than the rows counted (M2).
    pub(crate) fn open(
        layout: &Layout,
        fs: &StoreFs,
        floor: u64,
        past: &[Frame],
        views: Views,
    ) -> Result<FactRows, StoreError> {
        // 1. The files.
        let path = layout.facts_col();
        let column = AppendOnlyColumn::open(fs, path, "meta/idx/facts.col is missing")?;
        let log_path = layout.journal_log();
        let log = match fs_ops::open_regular(&log_path) {
            Ok(log) => Some(log),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(fs.locate(FileOp::Open, &log_path, e).into()),
        };

        // 2. The rows the checkpoint passed, then those the journal vouches for.
        let held = AppendOnlyColumn::<ROW_LEN>::whole(column.len()?);
        if held < floor {
            return Err(StoreError::IndexCorrupt {
                what: "facts.col holds fewer rows than the checkpoint passed",
            });
        }
        let want = rows_of(past)?;
        let end = held.min(floor + bytes::to_u64(want.len()));
        let mut vouched = floor;
        let scanned = column.scan(floor, end, |first, rows| {
            let want = &want[usize::try_from(first - floor).unwrap_or(usize::MAX)..];
            for (bytes, (row, _)) in rows.iter().zip(want) {
                if Row::decode(bytes).ok() != Some(*row) {
                    return ControlFlow::Break(());
                }
                vouched += 1;
            }
            ControlFlow::Continue(())
        });
        let mut rows = FactRows {
            column,
            log,
            log_path,
            fs: fs.clone(),
            len: vouched,
            end: 0,
            views,
            reads: AtomicU64::new(0),
        };

        // 3. Where the rows counted end. A writer may cut rows past the checkpoint meanwhile (a
        //    failed sync's, a repair's), never one before it: a read that meets the cut counts
        //    the rows the checkpoint passed alone, and the frames give the rest their facts.
        let counted = scanned.and_then(|_| rows.end_of(vouched));
        rows.end = match counted {
            Ok(end) => end,
            Err(StoreError::Io(e)) if e.kind() == io::ErrorKind::UnexpectedEof => {
                rows.len = floor;
                rows.end_of(floor)?
            }
            Err(e) => return Err(e),
        };
        if rows.views.covered() > rows.len {
            return Err(StoreError::IndexCorrupt {
                what: "a fact run covers rows facts.col does not count",
            });
        }
        Ok(rows)
    }

    /// [`FactRows::open`], the checkpoint and the frames past it read from the journal of
    /// `layout` on a handle of its own. A journal it cannot read vouches for nothing: only the
    /// rows the checkpoint passed count (and none if the checkpoint does not read either, or
    /// there is no journal: an index on its own).
    pub(crate) fn open_alone(
        layout: &Layout,
        fs: &StoreFs,
        views: Views,
    ) -> Result<FactRows, StoreError> {
        let journal = match Journal::open(layout, Arc::new(RealFs)) {
            Ok(journal) => journal,
            Err(StoreError::Io(e)) if e.kind() == io::ErrorKind::NotFound => {
                return Self::open(layout, fs, 0, &[], views);
            }
            Err(e) => return Err(e),
        };
        let Ok(ckpt) = journal.ckpt3() else {
            return Self::open(layout, fs, 0, &[], views);
        };
        let past = journal.tail_frames_from(ckpt.offset).unwrap_or_default();
        Self::open(layout, fs, ckpt.fact_rows, &past, views)
    }

    /// How many rows it counts.
    pub(crate) fn len(&self) -> u64 {
        self.len
    }

    /// How many of them no fact run covers.
    pub(crate) fn tail_len(&self) -> u64 {
        self.len.saturating_sub(self.views.covered())
    }

    /// Where the last row it counts ends in `journal2.log`, 0 with none: a fact of a frame from
    /// there on is no row's yet.
    pub(crate) fn end(&self) -> u64 {
        self.end
    }

    /// The path of `facts.col`, for the index's sync.
    pub(crate) fn path(&self) -> &camino::Utf8Path {
        self.column.path()
    }

    /// How many facts it has read from the journal: one per hit of a run, one per row past them.
    #[cfg(test)]
    pub(crate) fn reads(&self) -> u64 {
        self.reads.load(Ordering::Relaxed)
    }

    /// Cuts `facts.col` to the rows it counts: a torn row, or rows the journal does not vouch
    /// for, which replay appends again. The caller holds [`crate::MetaLock`].
    pub(crate) fn repair(&mut self) -> Result<u64, StoreError> {
        self.column.truncate(self.column.len()?, self.len)
    }

    /// Appends the rows of the facts of `frames`, frames past the checkpoint in journal order,
    /// that it does not count yet: one plain append (no sync). The caller holds
    /// [`crate::MetaLock`] and replays them.
    pub(crate) fn append(&mut self, frames: &[Frame]) -> Result<(), StoreError> {
        let rows = rows_of(frames)?;
        let held = rows.partition_point(|(row, _)| row.offset < self.end);
        let new = &rows[held..];
        let Some((last, _)) = new.last() else {
            return Ok(());
        };
        let bytes: Vec<u8> = new.iter().flat_map(|(row, _)| row.encode()).collect();
        self.column.append(&bytes, self.len)?;
        self.len += bytes::to_u64(new.len());
        self.end = last.end();
        Ok(())
    }

    /// Cuts the rows past the first `keep`, if it counts more: what a writer whose index sync
    /// failed does, `keep` the rows the checkpoint passed.
    pub(crate) fn truncate_to(&mut self, keep: u64) -> Result<(), StoreError> {
        if keep >= self.len {
            return Ok(());
        }
        self.column.truncate(self.column.len()?, keep)?;
        self.len = keep;
        self.end = self.end_of(keep)?;
        Ok(())
    }

    /// The facts whose `side` is `key`, with their row numbers, ascending: each fact run's rows
    /// of the key (a search, then one read of their row numbers), then the rows past the runs.
    /// Each fact is read with one `read_at` of the journal and checked against its row and the
    /// key: a run that names a row of another key, a row twice, or a row past what the run covers
    /// is [`StoreError::IndexCorrupt`].
    pub(crate) fn find(&self, side: Side, key: &[u8; 32]) -> Result<Vec<(u64, Fact)>, StoreError> {
        // 1. The runs' rows of the key.
        let mut hits = Vec::new();
        for pair in self.views.pairs() {
            let view = side.view(pair);
            let start = view.lower_bound(key)?;
            let stop = view.run_end(start, u64::MAX, |found| found == key)?;
            for rec in view.recs(start..stop)? {
                if !view.covers(rec) {
                    return Err(StoreError::IndexCorrupt {
                        what: "a fact run names a row it does not cover",
                    });
                }
                let fact = self.fact(rec)?;
                if side.key_of(&fact) != *key {
                    return Err(StoreError::IndexCorrupt {
                        what: "a fact run names a row of another key",
                    });
                }
                hits.push((rec, fact));
            }
        }

        // 2. The rows past the runs, each fact read and compared.
        let from = self.views.covered();
        let mut tail = Vec::new();
        self.column.scan(from, self.len, |first, rows| {
            tail.extend((first..).zip(rows.iter().copied()));
            ControlFlow::<()>::Continue(())
        })?;
        for (rec, bytes) in tail {
            let fact = self.read(Row::decode(&bytes)?)?;
            if side.key_of(&fact) == *key {
                hits.push((rec, fact));
            }
        }

        // 3. In row order, each row once.
        hits.sort_unstable_by_key(|(rec, _)| *rec);
        if hits.windows(2).any(|pair| pair[0].0 == pair[1].0) {
            return Err(StoreError::IndexCorrupt {
                what: "a fact run names a row twice",
            });
        }
        Ok(hits)
    }

    /// The fact of row `rec`: its row, then its record.
    fn fact(&self, rec: u64) -> Result<Fact, StoreError> {
        self.read(Row::decode(&self.column.entry(rec)?)?)
    }

    /// The fact `row` names: one `read_at` of its bytes in `journal2.log`, decoded with the
    /// build's kinds, and checked against the row's length, kind and type.
    fn read(&self, row: Row) -> Result<Fact, StoreError> {
        // 1. Its bytes, at most a fact's.
        let not_its = || StoreError::IndexCorrupt {
            what: "a fact row names bytes of the journal that are not its fact",
        };
        let len = usize::try_from(row.len).unwrap_or(usize::MAX);
        if len > MAX_FACT_LEN {
            return Err(not_its());
        }
        let mut buf = vec![0; len];
        self.reads.fetch_add(1, Ordering::Relaxed);
        let read = match &self.log {
            Some(log) => log.read_exact_at(&mut buf, row.offset),
            None => Err(io::ErrorKind::NotFound.into()),
        };
        read.map_err(|e| self.fs.locate(FileOp::ReadAt, &self.log_path, e))?;

        // 2. The fact they hold, whole, as the row says.
        let known = features::known_kinds();
        let Ok((Record::Fact(fact), used)) =
            Record::decode_in(StoreFormat::V3, Base::After, &known, &buf)
        else {
            return Err(not_its());
        };
        let named = (fact.kind().code(), fact.ty()) == (row.kind, row.ty);
        if used != len || !named {
            return Err(not_its());
        }
        Ok(fact)
    }

    /// Where row `count - 1` ends in `journal2.log`; 0 for no row.
    fn end_of(&self, count: u64) -> Result<u64, StoreError> {
        let Some(last) = count.checked_sub(1) else {
            return Ok(0);
        };
        Ok(Row::decode(&self.column.entry(last)?)?.end())
    }
}

/// A section of a run as it is built: (key, row) entries.
pub(crate) type Section = Vec<([u8; 32], u64)>;

/// The two sections of the fact run of rows `[from, to)` of `layout`: (subject, row) of every
/// row, and (other, row) of the rows whose other is not all zero, unsorted. Rows below the
/// checkpoint, synced and never cut; each fact read from the journal with one `read_at`.
pub(crate) fn sections(
    layout: &Layout,
    fs: &StoreFs,
    from: u64,
    to: u64,
) -> Result<[Section; 2], StoreError> {
    let rows = FactRows::open(layout, fs, 0, &[], Views::of_runs(Vec::new(), 0))?;
    let held = AppendOnlyColumn::<ROW_LEN>::whole(rows.column.len()?);
    if held < to {
        return Err(StoreError::IndexCorrupt {
            what: "facts.col is shorter than the checkpoint passed",
        });
    }

    // 1. The rows, a window at a time.
    let mut read = Vec::with_capacity(usize::try_from(to - from).unwrap_or(0));
    rows.column.scan(from, to, |first, entries| {
        read.extend((first..).zip(entries.iter().copied()));
        ControlFlow::<()>::Continue(())
    })?;

    // 2. Each one's fact: its subject, and its other unless all zero.
    let (mut subjects, mut others) = (Vec::with_capacity(read.len()), Vec::new());
    for (rec, bytes) in read {
        let fact = rows.read(Row::decode(&bytes)?)?;
        subjects.push((fact.subject(), rec));
        if fact.other() != [0; 32] {
            others.push((fact.other(), rec));
        }
    }
    Ok([subjects, others])
}

impl std::fmt::Debug for FactRows {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FactRows")
            .field("len", &self.len)
            .field("end", &self.end)
            .field("views", &self.views)
            .finish_non_exhaustive()
    }
}

#[cfg(all(test, feature = "test-hooks"))]
mod tests {
    use camino::Utf8Path;

    use super::*;
    use crate::{Expect, FactKind, MetaStore, Step, supported_for_tests};

    /// A Lineage fact about `subject`, naming no other.
    fn lineage(subject: [u8; 32]) -> Fact {
        Fact::new(
            FactKind::Lineage,
            0,
            subject,
            [0; 32],
            0,
            "a".to_owned(),
            vec![1; 32],
        )
        .unwrap()
    }

    /// A lookup reads one fact from the journal per hit in the runs, and one per row past them:
    /// never the run's other rows.
    #[test]
    fn a_fact_lookup_reads_one_record() {
        let _guard = supported_for_tests(&["lineage"]);
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(dir.path()).unwrap();
        let layout = Layout::with_format(root, StoreFormat::V3);
        let store = MetaStore::create_in(layout, Arc::new(RealFs)).unwrap();

        // 1. 1,100 facts, three of them about one subject, all in a fact run.
        let shared = [7; 32];
        let mut write = store.write().unwrap();
        for n in 0..1_100u32 {
            let mut subject = [0; 32];
            subject[..4].copy_from_slice(&n.to_le_bytes());
            let subject = if n % 400 == 0 { shared } else { subject };
            write.put_fact(lineage(subject), Expect::Any).unwrap();
        }
        write.commit().unwrap();
        store.flush().unwrap();
        assert_eq!(store.step_views().unwrap(), Step::Done);

        // 2. Three hits, three reads.
        let read = store.read().unwrap();
        assert_eq!(read.facts_by_subject(shared).unwrap().len(), 3);
        assert_eq!(read.fact_reads(), 3);
        drop(read);

        // 3. One more row, past the run: a read of it too, and one of the overlay none.
        let mut write = store.write().unwrap();
        write.put_fact(lineage([9; 32]), Expect::Any).unwrap();
        write.commit().unwrap();
        store.flush().unwrap();
        let read = store.read().unwrap();
        assert_eq!(read.facts_by_subject(shared).unwrap().len(), 3);
        assert_eq!(read.fact_reads(), 4);
    }
}
