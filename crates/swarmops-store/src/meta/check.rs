//! The metadata checks of verify, in the store (the M6 design §6): the sorted views and the
//! index read in blocks, the journal's frames and offsets, and FORMAT 2's seq, previous CRC
//! and entry checksums. Damage is a finding of the report, not an error.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io;
use std::ops::Range;
use std::os::unix::fs::FileExt;

use swarmops_core::{ChangesetId, ObjectId, bytes};

use super::READ_WINDOW;
use super::column::AppendOnlyColumn;
use super::facts::{self, ROW_LEN, Row};
use super::features;
use super::frame::{Fact, Frame, Place, Record};
use super::fsck::{self, Nonce};
use super::index::{ChangesetMeta, Index};
use super::journal::Journal;
use super::lock::MetaLock;
use super::refs::RefFile;
use super::runs::{self, Domain};
use super::sorted::SortedView;
use super::store::MetaStore;
use crate::error::{FileOp, StoreError};
use crate::format::StoreFormat;
use crate::fs_ops;

/// How many journal frames [`MetaStore::check`] reads with one call.
const FRAME_BATCH: usize = 4_096;

/// FORMAT 3's findings (the FORMAT 3 design §2.3, §2.5, §2.6, §2.7), each the name a
/// [`MetaCheck::meta_corrupt`] line starts with: `journal3.ckpt` lost and a frame without a nonce
/// where the base seq would be, a fact record whose feature `meta/features` does not list, a
/// manifest whose runs do not split what the checkpoint passed (M1, M2), a run file failing its
/// sections' CRC, and a frame past the base seq without its nonce.
const BASE_SEQ_LOST: &str = "base_seq_lost";
const FEATURE_UNLISTED: &str = "feature_unlisted";
const BROKEN_PARTITION: &str = "broken_partition";
const RUN_CRC: &str = "run_crc";
const MISSING_NONCE: &str = "missing_nonce";

/// What a manifest that reads whole but does not split its domains' records says, as its decode
/// refuses it ([`runs::read_manifest`]).
const UNSPLIT: [&str; 2] = [
    "the manifest's runs leave a gap or overlap",
    "the manifest's runs are out of domain order",
];

/// A run file's header and its trailing CRC (the FORMAT 3 design §2.4), and one entry of a
/// section: the bytes the run CRC streams past and over.
const RUN_HEADER_LEN: u64 = 44;
const RUN_CRC_LEN: u64 = 4;
const RUN_ENTRY_LEN: u64 = 40;

/// The feature list's header line, before its names (the FORMAT 3 design §2.6).
const FEATURES_HEADER: &str = "swarmops-features 1";

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
    /// The names of FORMAT 3's findings among `meta_corrupt`, each once.
    findings: Vec<&'static str>,
    /// Every blob a fact names, with the fact's subject.
    fact_blobs: Vec<(ObjectId, [u8; 32])>,
}

impl MetaCheck {
    /// The names of the FORMAT 3 findings among [`MetaCheck::meta_corrupt`], each once, in the
    /// order found: `base_seq_lost`, `feature_unlisted`, `broken_partition`, `run_crc` and
    /// `missing_nonce` (the FORMAT 3 design §2.3, §2.5, §2.6, §2.7). Each such line starts with its
    /// name.
    pub fn findings(&self) -> &[&'static str] {
        &self.findings
    }

    /// Every blob the facts of a FORMAT 3 store name, with the subject of the fact, in journal
    /// order: roots that verify looks for as it looks for the blobs trees name (the FORMAT 3
    /// design §2.8). Empty in FORMAT 1 and 2.
    pub fn fact_blobs(&self) -> &[(ObjectId, [u8; 32])] {
        &self.fact_blobs
    }

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
    /// `meta/journal2.ckpt`, in FORMAT 3 `meta/journal3.ckpt`.
    ckpt_file: String,
    findings: Vec<&'static str>,
    fact_blobs: Vec<(ObjectId, [u8; 32])>,
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
        let listed = self.feature_list(store)?;
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
        if layout.format() == StoreFormat::V3 {
            self.frames3(store, &frames, listed.as_deref())?;
        }
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
            let read = if whole { Frames::Whole } else { Frames::Cut };
            self.compare(&journaled, &records, ckpt, read);
            // A record past the journal's last changeset, which no frame holds, whether the index
            // counts it or not.
            if whole && held > bytes::to_u64(journaled.len()) {
                self.mismatches
                    .insert((bytes::to_u64(journaled.len()), MetaMismatch::Journal));
            }
            if layout.format() == StoreFormat::V3 {
                self.runs3(store, index, &records, &frames, whole)?;
            } else {
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
        }
        if layout.format() == StoreFormat::V3 {
            self.manifest3(store, journal.as_ref())?;
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
                    StoreFormat::V2 => "an offset, a seq and their CRC",
                    StoreFormat::V3 => "an offset, a seq, a base seq, two counts and their CRC",
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

    /// Reports FORMAT 3's finding `name`: a line of `meta_corrupt` that starts with it, the name
    /// listed once.
    fn finding(&mut self, name: &'static str, what: &str) {
        if !self.findings.contains(&name) {
            self.findings.push(name);
        }
        self.meta_corrupt.push(format!("{name}: {what}"));
    }

    /// The names `meta/features` lists, in FORMAT 3; `None` in the others, and for a file that is
    /// no feature list (reported). One that names a feature this build lacks, or of a later list
    /// format, is [`StoreError::TooNew`]: a newer build's store, not damage (the FORMAT 3 design
    /// §2.6).
    fn feature_list(&mut self, store: &MetaStore) -> Result<Option<Vec<String>>, StoreError> {
        if store.layout.format() != StoreFormat::V3 {
            return Ok(None);
        }
        let list = match features::check(&store.layout, &store.fs) {
            Ok(()) => features::read(&store.layout, &store.fs)?,
            Err(StoreError::Io(e)) if e.kind() == io::ErrorKind::InvalidData => {
                self.meta_corrupt.push(
                    "meta/features is no feature list: the features of the fact records are not \
                     checked"
                        .to_owned(),
                );
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        let encoded = String::from_utf8_lossy(&list.encode()).into_owned();
        let names = (encoded.lines())
            .filter(|line| *line != FEATURES_HEADER)
            .map(str::to_owned)
            .collect();
        Ok(Some(names))
    }

    /// FORMAT 3's checks of the journal's frames, `frames` the whole ones read: what stops the
    /// journal, if a frame without a nonce does (`missing_nonce`, or `base_seq_lost` with
    /// `journal3.ckpt` lost); every fact record's feature against `listed` (`feature_unlisted`);
    /// and the blobs the facts name.
    fn frames3(
        &mut self,
        store: &MetaStore,
        frames: &[Frame],
        listed: Option<&[String]>,
    ) -> Result<(), StoreError> {
        // 1. The frame that stops the journal: a feature listed since, of a newer build, or one
        //    whose records end in no nonce.
        if let Some(offset) = self.journal_corrupt {
            features::check(&store.layout, &store.fs)?;
            let place = Place {
                seq: bytes::to_u64(frames.len()),
                prev_crc: frames.last().map_or(0, |frame| frame.crc),
            };
            if fsck::nonce_at(&store.layout, &store.fs, offset, place)? == Some(Nonce::Without) {
                self.nonce_missing(store, offset, place.seq)?;
            }
        }

        // 2. Each fact's feature, the first frame of each that the list lacks; its blobs.
        let mut unlisted: BTreeMap<&'static str, u64> = BTreeMap::new();
        for frame in frames {
            for record in &frame.records {
                let Record::Fact(fact) = record else { continue };
                let name = features::feature_of(fact.kind());
                if listed.is_some_and(|listed| !listed.iter().any(|have| have == name)) {
                    unlisted.entry(name).or_insert(frame.offset);
                }
                let subject = fact.subject();
                (self.fact_blobs).extend(fact.blob_ids().into_iter().map(|id| (id, subject)));
            }
        }
        for (name, offset) in unlisted {
            self.finding(
                FEATURE_UNLISTED,
                &format!(
                    "the frame at offset {offset} holds a fact of feature {name}, which \
                     meta/features does not list: the record was written before its feature"
                ),
            );
        }
        Ok(())
    }

    /// Reports frame `seq` at `offset`, whole but without the nonce a frame from the base seq on
    /// ends in: `base_seq_lost` if `journal3.ckpt` is missing or no checkpoint (repair finds the
    /// base in the journal), else `missing_nonce`.
    fn nonce_missing(
        &mut self,
        store: &MetaStore,
        offset: u64,
        seq: u64,
    ) -> Result<(), StoreError> {
        let path = store.layout.journal_ckpt();
        let present =
            std::fs::exists(&path).map_err(|e| store.fs.locate(FileOp::Stat, &path, e))?;
        let lost = !present
            || self
                .meta_corrupt
                .iter()
                .any(|line| line.starts_with(&self.ckpt_file));
        if lost {
            self.finding(
                BASE_SEQ_LOST,
                &format!(
                    "{} is lost, and frame {seq} at offset {offset} ends in no nonce: the frames \
                     before the base seq are not told from damage; repair finds the base seq in \
                     the journal",
                    self.ckpt_file
                ),
            );
            return Ok(());
        }
        self.finding(
            MISSING_NONCE,
            &format!(
                "frame {seq} at offset {offset} ends in no nonce, past the base seq: no FORMAT 3 \
                 writer wrote it"
            ),
        );
        Ok(())
    }

    /// FORMAT 3's checks of the manifest and its run files, read without a lock: runs that do
    /// not split each domain's records from 0 (M1) or cover more than `journal3.ckpt` passed (M2)
    /// are `broken_partition`; a run whose sections fail its trailing CRC, streamed a window at a
    /// time, is `run_crc`. A run gone meanwhile was replaced by a merge, and is passed over.
    fn manifest3(
        &mut self,
        store: &MetaStore,
        journal: Option<&Journal>,
    ) -> Result<(), StoreError> {
        // 1. The manifest, split or not.
        let manifest = match runs::read_manifest(&store.layout, &store.fs) {
            Ok((_, manifest)) => manifest,
            Err(StoreError::IndexCorrupt { what }) if UNSPLIT.contains(&what) => {
                self.finding(
                    BROKEN_PARTITION,
                    &format!("meta/sorted3/MANIFEST: {what}: rebuild the index"),
                );
                return Ok(());
            }
            // Reported as the index's damage: the index does not open on it.
            Err(StoreError::IndexCorrupt { .. }) => return Ok(()),
            Err(e) => return Err(e),
        };

        // 2. What the checkpoint passed, if it reads.
        if let Some(ckpt) = journal.and_then(|journal| journal.ckpt3().ok()) {
            let past = manifest.covered(Domain::Changesets) > ckpt.records
                || manifest.covered(Domain::Facts) > ckpt.fact_rows;
            if past {
                self.finding(
                    BROKEN_PARTITION,
                    "meta/sorted3/MANIFEST's runs cover records the checkpoint has not passed: \
                     rebuild the index",
                );
            }
        }

        // 3. Each run's sections against its trailing CRC.
        for run in &manifest.runs {
            let path = runs::run_path(&store.layout, run);
            if crc_fails(store, &path)? {
                self.finding(
                    RUN_CRC,
                    &format!(
                        "meta/sorted3/{} fails the CRC of its sections: rebuild the index",
                        run.name()
                    ),
                );
            }
        }
        Ok(())
    }

    /// FORMAT 3's checks of the runs against the index (the FORMAT 3 design §2.4, §2.8): each
    /// changeset run's sections against the columns, as FORMAT 1's and 2's views are; the rows of
    /// `facts.col` the index counts against the journal's facts; each fact run's sections against
    /// those facts. Runs that do not open are the index's damage, reported with its open.
    fn runs3(
        &mut self,
        store: &MetaStore,
        index: &Index,
        records: &[Option<ChangesetMeta>],
        frames: &[Frame],
        whole: bool,
    ) -> Result<(), StoreError> {
        let Ok((changesets, fact_views)) = runs::open_all(&store.layout, &store.fs) else {
            return Ok(());
        };

        // 1. The changeset runs.
        let record = |rec: u64| records.get(to_usize(rec)).and_then(Option::as_ref);
        for pair in changesets.pairs() {
            let range = run_range(&pair.ids);
            let id = |rec| record(rec).map(|meta| meta.id.0.0);
            let dh = |rec| record(rec).map(|meta| meta.delta_hash);
            for (view, keys, key_of) in [
                (
                    &pair.ids,
                    Keys::One,
                    &id as &dyn Fn(u64) -> Option<[u8; 32]>,
                ),
                (&pair.dh, Keys::Many, &dh),
            ] {
                let bad = section_mismatches(view, range.clone(), keys, Need::All, key_of)?;
                for rec in bad {
                    self.mismatches.insert((rec, MetaMismatch::SortedView));
                }
                self.index_reads += view.probes() + view.rec_reads();
            }
        }

        // 2. The fact rows the index counts, against the journal's facts.
        let expected = facts::rows_of(frames)?;
        let counted = index.fact_rows();
        let column = AppendOnlyColumn::<ROW_LEN>::open(
            &store.fs,
            store.layout.facts_col(),
            "meta/idx/facts.col is missing",
        )?;
        let mut wrong = None;
        let mut start = 0;
        while start < counted && wrong.is_none() {
            let end = (start + READ_WINDOW).min(counted);
            let block = column.entries(start, end)?;
            for (rec, bytes) in (start..).zip(block.as_chunks::<ROW_LEN>().0) {
                let row = Row::decode(bytes).ok();
                let want = expected.get(to_usize(rec)).map(|(row, _)| *row);
                let unvouched = want.is_none() && !whole;
                if row != want && !unvouched {
                    wrong = Some(rec);
                    break;
                }
            }
            start = end;
        }

        // 3. The fact runs: every row by its subject, and those with an other by it.
        let fact = |rec: u64| expected.get(to_usize(rec)).map(|(_, fact)| *fact);
        let subject = |rec| fact(rec).map(Fact::subject);
        let other = |rec| fact(rec).map(Fact::other).filter(|other| *other != [0; 32]);
        for pair in fact_views.pairs() {
            let range = run_range(&pair.ids);
            let by_subject =
                section_mismatches(&pair.ids, range.clone(), Keys::Many, Need::All, &subject)?;
            let by_other = section_mismatches(&pair.dh, range, Keys::Many, Need::Keyed, &other)?;
            let first = by_subject.into_iter().chain(by_other).min();
            wrong = [wrong, first].into_iter().flatten().min();
        }
        if let Some(rec) = wrong {
            self.meta_corrupt.push(format!(
                "meta/idx/facts.col row {rec}, or a fact run naming it, disagrees with the \
                 journal: rebuild the index"
            ));
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
            findings: self.findings,
            fact_blobs: self.fact_blobs,
        }
    }
}

/// Which records of its range a run's section must name: all of them, or those with a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Need {
    All,
    Keyed,
}

/// The records a FORMAT 3 run's section `view` names wrongly, or does not name, read in blocks
/// of `READ_WINDOW`: each entry's key must be `key_of` its record, a record of the run's `range`,
/// the entries ascending (strictly by key for [`Keys::One`], by key and then record for
/// [`Keys::Many`]), and each record of the range that `need` asks for named once.
fn section_mismatches(
    view: &SortedView<32>,
    range: Range<u64>,
    keys: Keys,
    need: Need,
    key_of: &dyn Fn(u64) -> Option<[u8; 32]>,
) -> Result<BTreeSet<u64>, StoreError> {
    let mut named = BTreeSet::new();
    let mut bad = BTreeSet::new();
    let mut previous: Option<([u8; 32], u64)> = None;

    // 1. Each entry: in range, keyed as its record, in order.
    let mut start = 0;
    while start < view.len() {
        let end = (start + READ_WINDOW).min(view.len());
        let keys_block = view.keys(start..end)?;
        let recs_block = view.recs(start..end)?;
        for (key, rec) in keys_block.into_iter().zip(recs_block) {
            let keyed = range.contains(&rec) && key_of(rec) == Some(key);
            let ordered = previous.is_none_or(|before| match keys {
                Keys::One => before.0 < key,
                Keys::Many => before < (key, rec),
            });
            if !(keyed && ordered && named.insert(rec)) {
                bad.insert(rec);
            }
            previous = Some((key, rec));
        }
        start = end;
    }

    // 2. Each record it must name, named.
    for rec in range {
        let wanted = need == Need::All || key_of(rec).is_some();
        if wanted && !named.contains(&rec) {
            bad.insert(rec);
        }
    }
    Ok(bad)
}

/// The records a run covers, by its first section, which names each of them once.
fn run_range(first: &SortedView<32>) -> Range<u64> {
    let end = first.covers_end();
    end.saturating_sub(first.len())..end
}

/// Whether the run file at `path` fails the CRC of its sections, which its last 4 bytes hold:
/// streamed past its header a window of `READ_WINDOW` entries at a time. A file gone, or shorter
/// than a header and a CRC, is not looked at: a merge replaced it, or the index's open reports it.
fn crc_fails(store: &MetaStore, path: &camino::Utf8Path) -> Result<bool, StoreError> {
    let failed = |op, e| StoreError::from(store.fs.locate(op, path, e));
    let file = match fs_ops::open_regular(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(failed(FileOp::Open, e)),
    };
    let len = file.metadata().map_err(|e| failed(FileOp::Stat, e))?.len();
    let Some(end) = len
        .checked_sub(RUN_CRC_LEN)
        .filter(|&end| end >= RUN_HEADER_LEN)
    else {
        return Ok(false);
    };

    // The sections, a window at a time, then the CRC they must have.
    let mut hasher = crc32fast::Hasher::new();
    let mut buf = vec![0; to_usize(READ_WINDOW * RUN_ENTRY_LEN)];
    let mut at = RUN_HEADER_LEN;
    while at < end {
        let take = to_usize((end - at).min(bytes::to_u64(buf.len())));
        file.read_exact_at(&mut buf[..take], at)
            .map_err(|e| failed(FileOp::ReadAt, e))?;
        hasher.update(&buf[..take]);
        at += bytes::to_u64(take);
    }
    let mut crc = [0; 4];
    file.read_exact_at(&mut crc, end)
        .map_err(|e| failed(FileOp::ReadAt, e))?;
    Ok(u32::from_le_bytes(crc) != hasher.finalize())
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
