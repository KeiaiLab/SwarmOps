//! The metadata checks of verify, in the store (the M6 design §6): the sorted views and the
//! index read in blocks, the journal's frames and offsets, and FORMAT 2's seq, previous CRC
//! and entry checksums. Damage is a finding of the report, not an error.

use std::collections::{BTreeSet, HashSet};
use std::io;

use swarmops_core::{ChangesetId, bytes};

use super::READ_WINDOW;
use super::frame::{Frame, Record};
use super::index::{ChangesetMeta, Index};
use super::journal::Journal;
use super::lock::MetaLock;
use super::refs::RefFile;
use super::sorted::SortedView;
use super::store::MetaStore;
use crate::error::StoreError;
use crate::format::StoreFormat;

/// How many journal frames [`MetaStore::check`] reads with one call.
const FRAME_BATCH: usize = 4_096;

/// What [`MetaStore::check`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct MetaCheck {
    /// One line per problem found; empty for sound metadata.
    pub problems: Vec<String>,
    /// Metadata files that do not read as such (`VerifyProblem::MetaCorrupt`'s payloads).
    pub meta_corrupt: Vec<String>,
    /// Where the journal stops at damage, if it does.
    pub journal_corrupt: Option<u64>,
    /// Records where the index disagrees with the journal.
    pub journal_mismatches: Vec<u64>,
    /// Records where a sorted view disagrees with the index columns.
    pub view_mismatches: Vec<u64>,
    /// Ref files that are not ref files.
    pub corrupt_refs: Vec<String>,
    /// Ref files that name no changeset in the store.
    pub dangling_refs: Vec<String>,
    /// The changesets the metadata records, in seq order: the journal's, and past any journal
    /// damage, the index's.
    pub changesets: Vec<ChangesetMeta>,
    /// Whether the journal read whole or the index opened, so that which changesets the store
    /// holds is known.
    pub targets_known: bool,
    /// The ref problems in ref-name order: `(name, true)` for a corrupt ref file, `(name, false)`
    /// for a dangling one.
    ref_problems: Vec<(String, bool)>,
    /// How many `read_at` calls the check made on the index's columns and sorted views.
    index_reads: u64,
}

impl MetaCheck {
    /// The ref problems in ref-name order: `(name, true)` for a corrupt ref file, `(name, false)`
    /// for a dangling one.
    pub fn ref_problems(&self) -> &[(String, bool)] {
        &self.ref_problems
    }

    /// How many `read_at` calls the check made on the index's columns and sorted views while
    /// comparing the index records and views.
    pub fn index_reads(&self) -> u64 {
        self.index_reads
    }
}

impl MetaStore {
    /// Checks the metadata files against the journal, reading the index and sorted views in
    /// blocks of 4,096 entries (`READ_WINDOW`). Damage to the metadata files, the journal (its
    /// FORMAT 2 frame seqs, previous CRCs and offset-entry checksums included), the index, the
    /// sorted views or the ref files is reported in [`MetaCheck`], never returned as an error;
    /// only an I/O error the check cannot read past fails it.
    pub fn check(&self) -> Result<MetaCheck, StoreError> {
        let mut run = Run::default();
        run.lock_file(self);
        run.metadata(self)?;
        Ok(run.finish())
    }
}

/// One run of [`MetaStore::check`].
#[derive(Default)]
struct Run {
    meta_corrupt: Vec<String>,
    journal_corrupt: Option<u64>,
    mismatches: BTreeSet<(u64, MetaMismatch)>,
    ref_problems: Vec<(String, bool)>,
    changesets: Vec<ChangesetMeta>,
    targets_known: bool,
    index_reads: u64,
    /// The checkpoint's file, by its place in the store: `meta/journal.ckpt`, or in FORMAT 2
    /// `meta/journal2.ckpt`.
    ckpt_file: String,
}

/// What a metadata mismatch found disagreeing, ordered as `VerifyProblem::IndexMismatch` orders
/// its causes (`Journal` before `SortedView`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum MetaMismatch {
    Journal,
    SortedView,
}

/// What reading the checkpoint before the journal gave.
#[derive(Debug, Clone, Copy)]
enum CkptState {
    /// The checkpoint file read as `offset`.
    At(u64),
    /// The checkpoint file is malformed; already reported in `meta_corrupt`.
    Malformed,
    /// FORMAT 2's `journal2.ckpt` disagreed with `journal2.off` at `offset` (or `journal2.off`
    /// itself is corrupt): reported as a non-boundary checkpoint if the journal reads whole.
    Disagreed(u64),
}

impl Run {
    /// Reports a `meta/lock` that every write refuses before it opens it (one that is not a
    /// regular file), looked at without opening it.
    fn lock_file(&mut self, store: &MetaStore) {
        if MetaLock::refuses(&store.layout) {
            self.meta_corrupt.push(
                "meta/lock is not a regular file: every write fails on it until it is removed, \
                 and the next write makes a new one"
                    .to_owned(),
            );
        }
    }

    /// Reads and compares the metadata files.
    fn metadata(&mut self, store: &MetaStore) -> Result<(), StoreError> {
        let layout = &store.layout;
        let ckpt = layout.journal_ckpt();
        self.ckpt_file = (ckpt.strip_prefix(layout.root()))
            .unwrap_or(&ckpt)
            .to_string();
        let (journal, journal_damage) = match open_journal(store)? {
            Ok(journal) => (Some(journal), None),
            Err(what) => (None, Some(what)),
        };
        let ckpt_state = match &journal {
            Some(journal) => self.checkpoint(journal, layout.format())?,
            None => CkptState::Malformed,
        };
        let (index, index_damage) = match open_index(store)? {
            Ok(index) => (Some(index), None),
            Err(what) => (None, Some(what)),
        };
        // Before the journal is read: a writer appends a record only once its frame is durable
        // and listed, so every record the index files hold now has its frame in what is read.
        let held = match &index {
            Some(index) => index.held()?,
            None => 0,
        };
        let ref_files = store.ref_files()?;
        let (frames, whole) = match &journal {
            Some(journal) => {
                let (frames, corrupt) = read_frames(journal)?;
                self.journal_corrupt = corrupt;
                (frames, corrupt.is_none())
            }
            None => (Vec::new(), false),
        };
        // Every snapshot and write transaction reads the frames past the checkpoint: a
        // checkpoint that is no frame boundary fails them all, so verify must say so.
        let ckpt = match (&journal, ckpt_state) {
            (Some(journal), CkptState::At(ckpt)) if whole => {
                self.checkpoint_boundary(journal, ckpt)?
            }
            (_, CkptState::At(ckpt)) => Some(ckpt),
            (_, CkptState::Disagreed(offset)) if whole => {
                self.report_bad_boundary(offset);
                None
            }
            _ => None,
        };
        self.report_open_damage(
            journal_damage,
            index_damage,
            journal.is_some(),
            index.is_some(),
            whole,
        );
        self.targets_known = whole || index.is_some();

        let journaled = journal_changesets(&frames);
        let mut records = Vec::new();
        if let Some(index) = &index {
            let reads_before = index.column_reads();
            records = self.index_records(index)?;
            let frames = if whole { Frames::Whole } else { Frames::Cut };
            self.compare(&journaled, &records, ckpt, frames);
            // A record past the journal's last changeset, which no frame holds, whether the index
            // counts it or not.
            if whole && held > bytes::to_u64(journaled.len()) {
                self.mismatches
                    .insert((bytes::to_u64(journaled.len()), MetaMismatch::Journal));
            }
            self.check_view(index.ids_view(), &records, |meta| meta.id.0.0, Keys::One)?;
            self.check_view(
                index.dh_view(),
                &records,
                |meta| meta.delta_hash,
                Keys::Many,
            )?;
            self.index_reads = (index.column_reads() - reads_before)
                + index.ids_view().probes()
                + index.ids_view().rec_reads()
                + index.dh_view().probes()
                + index.dh_view().rec_reads();
        }

        let journaled_count = journaled.len();
        let mut changesets: Vec<ChangesetMeta> =
            journaled.into_iter().map(|(meta, _)| meta).collect();
        if !whole {
            // The journal stops at damage, or is gone: past it, the index's records are what
            // there is to check.
            changesets.extend(records.into_iter().skip(journaled_count).flatten());
        }
        self.changesets = changesets;
        self.check_refs(ref_files)
    }

    /// Reports what damaged journal or index files leave unchecked.
    fn report_open_damage(
        &mut self,
        journal_damage: Option<String>,
        index_damage: Option<&'static str>,
        has_journal: bool,
        has_index: bool,
        whole: bool,
    ) {
        if let Some(what) = journal_damage {
            let rest = if has_index {
                ", and the index is checked as it is"
            } else {
                ""
            };
            self.meta_corrupt
                .push(format!("{what}: the journal is not checked{rest}"));
        }
        if let Some(what) = index_damage {
            let rest = if whole {
                ", and the journal's changesets are"
            } else if !has_journal {
                "; with no journal either, no changeset is known, so neither changesets nor the \
                 targets of refs are checked"
            } else {
                "; with the journal read only up to its damage, neither the changesets past it \
                 nor the targets of refs are checked"
            };
            self.meta_corrupt.push(format!(
                "the changeset index does not open ({what}): it is not checked{rest}"
            ));
        }
    }

    /// Checks each ref file against `self.changesets`.
    fn check_refs(&mut self, ref_files: Vec<RefFile>) -> Result<(), StoreError> {
        let known: HashSet<ChangesetId> = self.changesets.iter().map(|meta| meta.id).collect();
        for RefFile { name, state, .. } in ref_files {
            match state {
                Ok(state) if self.targets_known && !known.contains(&state.target) => {
                    self.ref_problems.push((name, false));
                }
                Ok(_) => {}
                // A file that is no ref file; anything else that fails is no damage to report.
                Err(StoreError::Io(e)) if e.kind() == io::ErrorKind::InvalidData => {
                    self.ref_problems.push((name, true));
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// The journal's checkpoint; reported if its file is malformed: in a store of `format`, the
    /// file holds a `u64le` offset (FORMAT 1), or an offset, a seq and their CRC (FORMAT 2).
    fn checkpoint(
        &mut self,
        journal: &Journal,
        format: StoreFormat,
    ) -> Result<CkptState, StoreError> {
        match journal.ckpt() {
            Ok(ckpt) => Ok(CkptState::At(ckpt)),
            Err(StoreError::Io(e)) if e.kind() == io::ErrorKind::InvalidData => {
                let holds = match format {
                    StoreFormat::V1 => "a u64le offset",
                    _ => "an offset, a seq and their CRC",
                };
                self.meta_corrupt.push(format!(
                    "{} does not hold {holds}: the records the checkpoint passed are not checked \
                     against the index",
                    self.ckpt_file
                ));
                Ok(CkptState::Malformed)
            }
            Err(StoreError::JournalCorrupt { offset }) => Ok(CkptState::Disagreed(offset)),
            Err(e) => Err(e),
        }
    }

    /// Whether the checkpoint is a frame boundary (0, a listed frame's start, or the end of the
    /// last listed frame), read as every snapshot and write transaction reads the frames past
    /// it; reported, and `None`, if not. Checked only when the journal read whole: otherwise
    /// the damage reported is the journal's.
    fn checkpoint_boundary(
        &mut self,
        journal: &Journal,
        ckpt: u64,
    ) -> Result<Option<u64>, StoreError> {
        match journal.tail_frames_from(ckpt) {
            Ok(_) => Ok(Some(ckpt)),
            Err(StoreError::JournalCorrupt { .. }) => {
                self.report_bad_boundary(ckpt);
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    fn report_bad_boundary(&mut self, ckpt: u64) {
        self.meta_corrupt.push(format!(
            "{} holds {ckpt}, which is not a frame boundary: reads and writes fail on it, and \
             the records the checkpoint passed are not checked against the index",
            self.ckpt_file
        ));
    }

    /// The index's records, by rec, read in blocks of `READ_WINDOW`; `None` for one that does
    /// not read (a mismatch).
    fn index_records(&mut self, index: &Index) -> Result<Vec<Option<ChangesetMeta>>, StoreError> {
        let len = index.len();
        let mut records = Vec::with_capacity(to_usize(len));
        let mut start = 0;
        while start < len {
            let end = (start + READ_WINDOW).min(len);
            for (offset, item) in index.get_range(start..end)?.into_iter().enumerate() {
                let rec = start + bytes::to_u64(offset);
                match item {
                    Ok(meta) => records.push(Some(meta)),
                    Err(StoreError::IndexCorrupt { .. }) => {
                        self.mismatches.insert((rec, MetaMismatch::Journal));
                        records.push(None);
                    }
                    Err(e) => return Err(e),
                }
            }
            start = end;
        }
        Ok(records)
    }

    /// Compares the index's records with the journal's changeset records (each with its
    /// frame's offset), place by place.
    fn compare(
        &mut self,
        journaled: &[(ChangesetMeta, u64)],
        records: &[Option<ChangesetMeta>],
        ckpt: Option<u64>,
        frames: Frames,
    ) {
        for (rec, record) in records.iter().enumerate() {
            let Some(record) = record else { continue };
            let wrong = match journaled.get(rec) {
                Some((want, _)) => want != record,
                // A record no committed transaction holds; past damage in the journal, or with
                // no journal, there is no telling.
                None => frames == Frames::Whole,
            };
            if wrong {
                self.mismatches
                    .insert((bytes::to_u64(rec), MetaMismatch::Journal));
            }
        }
        // The checkpoint passes a record only once the index holds it durably.
        let Some(ckpt) = ckpt else { return };
        for (seq, (_, offset)) in journaled.iter().enumerate().skip(records.len()) {
            if *offset < ckpt {
                self.mismatches
                    .insert((bytes::to_u64(seq), MetaMismatch::Journal));
            }
        }
    }

    /// Checks a sorted view against the index columns in blocks of `READ_WINDOW`: each entry's
    /// key must be the key of the record it names, the entries ascending (strictly by key for
    /// the id view, by key and then record for the delta-hash view), and a view of m entries
    /// must name each of the records 0 to m - 1 once. A record named wrongly, or not at all, is
    /// a mismatch.
    fn check_view(
        &mut self,
        view: &SortedView<32>,
        records: &[Option<ChangesetMeta>],
        key_of: impl Fn(&ChangesetMeta) -> [u8; 32],
        keys: Keys,
    ) -> Result<(), StoreError> {
        // The index opened only views that cover no more records than it holds.
        let len = view.len();
        let mut named = vec![false; to_usize(len)];
        let mut previous: Option<([u8; 32], u64)> = None;
        let mut start = 0;
        while start < len {
            let end = (start + READ_WINDOW).min(len);
            let keys_block = view.keys(start..end)?;
            let recs_block = view.recs(start..end)?;
            for (key, rec) in keys_block.into_iter().zip(recs_block) {
                let slot = usize::try_from(rec).ok().filter(|&slot| slot < named.len());
                // A record named twice is named once under a key that is not its own, or out of
                // order: the coverage below finds the record it pushed out.
                let keyed = slot.is_some_and(|slot| {
                    records[slot]
                        .as_ref()
                        .is_some_and(|meta| key_of(meta) == key)
                });
                let ordered = previous.is_none_or(|before| match keys {
                    Keys::One => before.0 < key,
                    Keys::Many => before < (key, rec),
                });
                if let Some(slot) = slot {
                    named[slot] = true;
                }
                if !(keyed && ordered) {
                    self.mismatches.insert((rec, MetaMismatch::SortedView));
                }
                previous = Some((key, rec));
            }
            start = end;
        }
        for (rec, named) in named.iter().enumerate() {
            if !named {
                self.mismatches
                    .insert((bytes::to_u64(rec), MetaMismatch::SortedView));
            }
        }
        Ok(())
    }

    fn finish(self) -> MetaCheck {
        let mut problems = Vec::new();
        for what in &self.meta_corrupt {
            problems.push(format!("damaged metadata: {what}"));
        }
        if let Some(offset) = self.journal_corrupt {
            problems.push(format!("the journal is corrupt at offset {offset}"));
        }
        let mut journal_mismatches = Vec::new();
        let mut view_mismatches = Vec::new();
        for (seq, cause) in &self.mismatches {
            match cause {
                MetaMismatch::Journal => {
                    journal_mismatches.push(*seq);
                    problems.push(format!(
                        "changeset record {seq}: the index disagrees with the journal; rebuild \
                         the index from the journal"
                    ));
                }
                MetaMismatch::SortedView => {
                    view_mismatches.push(*seq);
                    problems.push(format!(
                        "changeset record {seq}: a sorted view disagrees with the index columns; \
                         rebuild the sorted views from the columns"
                    ));
                }
            }
        }
        let mut corrupt_refs = Vec::new();
        let mut dangling_refs = Vec::new();
        for (name, corrupt) in &self.ref_problems {
            if *corrupt {
                corrupt_refs.push(name.clone());
                problems.push(format!("the file of ref {name} is not a ref file"));
            } else {
                dangling_refs.push(name.clone());
                problems.push(format!("ref {name} names no changeset in the store"));
            }
        }
        MetaCheck {
            problems,
            meta_corrupt: self.meta_corrupt,
            journal_corrupt: self.journal_corrupt,
            journal_mismatches,
            view_mismatches,
            corrupt_refs,
            dangling_refs,
            changesets: self.changesets,
            targets_known: self.targets_known,
            ref_problems: self.ref_problems,
            index_reads: self.index_reads,
        }
    }
}

/// A handle on the journal, or what is wrong with it if one of its files is missing: the
/// failed open names the file, relative to the store's root.
fn open_journal(store: &MetaStore) -> Result<Result<Journal, String>, StoreError> {
    match Journal::open_in(&store.layout, store.fs.clone()) {
        Ok(journal) => Ok(Ok(journal)),
        Err(StoreError::Io(e)) if e.kind() == io::ErrorKind::NotFound => Ok(Err(match &e.file {
            Some(file) => format!("{file} is missing"),
            None => format!("the journal does not open ({e})"),
        })),
        Err(e) => Err(e),
    }
}

/// A handle on the changeset index, or what is wrong with its files if they are too
/// damaged to open.
fn open_index(store: &MetaStore) -> Result<Result<Index, &'static str>, StoreError> {
    match Index::open_in(&store.layout, store.fs.clone()) {
        Ok(index) => Ok(Ok(index)),
        Err(StoreError::IndexCorrupt { what }) => Ok(Err(what)),
        Err(e) => Err(e),
    }
}

/// Every committed frame of `journal`, in order, read without a lock: the frames
/// `journal.off` lists, each of which must read whole (the last one too), then the whole
/// frames after them that a crash left unlisted. With where the journal is corrupt, if it is;
/// the frames are then the ones before that point.
fn read_frames(journal: &Journal) -> Result<(Vec<Frame>, Option<u64>), StoreError> {
    let mut frames: Vec<Frame> = Vec::new();
    let mut batch = FRAME_BATCH;
    loop {
        match journal.frames_from(bytes::to_u64(frames.len()), batch) {
            Ok(read) if read.is_empty() => break,
            Ok(read) => frames.extend(read),
            Err(StoreError::JournalCorrupt { offset }) if batch == 1 => {
                return Ok((frames, Some(offset)));
            }
            // The damage lies in this batch: read on a frame at a time, keeping those before.
            Err(StoreError::JournalCorrupt { .. }) => batch = 1,
            Err(e) => return Err(e),
        }
    }
    // From the last listed frame's start, a frame boundary: it comes again, then the whole
    // frames after it, listed since or never.
    let again = usize::from(!frames.is_empty());
    let start = frames.last().map_or(0, |frame| frame.offset);
    match journal.tail_frames_from(start) {
        Ok(tail) => {
            frames.extend(tail.into_iter().skip(again));
            Ok((frames, None))
        }
        Err(StoreError::JournalCorrupt { offset }) => Ok((frames, Some(offset))),
        Err(e) => Err(e),
    }
}

/// The changeset records of `frames`, in order, each as the index should hold it (its seq its
/// place among them), with the offset of its frame.
fn journal_changesets(frames: &[Frame]) -> Vec<(ChangesetMeta, u64)> {
    let mut changesets = Vec::new();
    for frame in frames {
        for record in &frame.records {
            if let Record::Changeset {
                id,
                generation,
                delta_hash,
                parents,
            } = record
            {
                let meta = ChangesetMeta::new(
                    *id,
                    bytes::to_u64(changesets.len()),
                    *generation,
                    *delta_hash,
                    parents.clone(),
                );
                changesets.push((meta, frame.offset));
            }
        }
    }
    changesets
}

/// The journal's frames [`MetaStore::check`] read: all of them, or only up to damage (or none,
/// with no journal).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Frames {
    Whole,
    Cut,
}

/// The records a sorted view's key names: one (the id view), or many, which then ascend by
/// record (the delta-hash view).
#[derive(Debug, Clone, Copy)]
enum Keys {
    One,
    Many,
}

/// `n` as a `usize`; the counts [`MetaStore::check`] converts are lengths of files it has open.
fn to_usize(n: u64) -> usize {
    usize::try_from(n).expect("a count of entries in an open file fits in usize")
}
