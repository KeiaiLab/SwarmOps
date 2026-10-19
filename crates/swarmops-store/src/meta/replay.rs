//! Replay (storage spec §2 rule 3, §3): bringing the materialized files, the changeset index
//! and the ref files, up to date with the journal frames past the checkpoint, and the same in
//! memory for a reader, which may not write.
//!
//! The index holds a prefix of the journal's changesets, in journal order: records are
//! appended frame by frame, what a crash takes from the index files is a suffix, and past the
//! checkpoint and the sorted views an index handle counts only the records the journal vouches
//! for (those the views cover count as they are, even past the checkpoint), so that what a
//! power loss leaves there with its bytes lost is a suffix too ([`Index::open`]). So replay
//! finds how many of the changesets past the checkpoint the index holds with a binary search
//! (one lookup when it holds them all, the usual case) and appends the rest, a frame's with
//! one [`Index::append_batch`].
//!
//! Every frame past the checkpoint but the last is materialized: a writer appends its frame
//! only after replaying all the others. Each ref those frames move ends at the target of its
//! last move; a ref file is brought there as [`refs::write`] does, which leaves alone a file
//! that is there already, so replaying a frame twice changes nothing.

use std::collections::{BTreeMap, HashMap};
use std::slice;

use swarmops_core::ChangesetId;

use super::frame::{Frame, Record};
use super::index::{ChangesetMeta, Index};
use super::refs::{self, RefState};
use super::to_u64;
use crate::error::StoreError;
use crate::fs_ops::StoreFs;
use crate::layout::Layout;

/// What [`replay`] changed: what the frames it applied hold that the index and the ref files
/// lacked. Frames it only reads again, whose records are there already, count for nothing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Replayed {
    /// The changesets it appended to the index.
    pub(crate) records: u64,
    /// The ref files it replaced or removed.
    pub(crate) refs: u64,
}

/// Brings the index and the ref files up to date with `frames`, the frames past the
/// checkpoint in journal order, appending only what they lack (each frame's changesets with one
/// [`Index::append_batch`]), and returns what it changed. The caller holds
/// [`crate::MetaLock`] and has repaired the journal's and the index's tails.
pub(crate) fn replay(
    index: &mut Index,
    layout: &Layout,
    fs: &StoreFs,
    frames: &[Frame],
) -> Result<Replayed, StoreError> {
    // The ref names first: frames this version cannot apply change nothing at all.
    let targets = ref_targets(frames)?;
    let changesets = changesets(frames);
    let held = held(index, &changesets)?;
    // Those past `held`, a frame at a time.
    let mut from = 0;
    for frame in frames {
        let to = from + changeset_count(frame);
        if to > held {
            let metas: Vec<ChangesetMeta> = changesets[from.max(held)..to]
                .iter()
                .map(|record| record.meta(0))
                .collect();
            index.append_batch(&metas)?;
        }
        from = to;
    }
    let mut files = 0;
    for (name, target) in targets {
        if refs::write(layout, fs, name, target)? {
            files += 1;
        }
    }
    Ok(Replayed {
        records: to_u64(changesets.len() - held),
        refs: files,
    })
}

/// [`replay`] of the one frame a writer has just appended: its materialization, which is no
/// recovery, so what it changed goes unreported.
pub(crate) fn materialize(
    index: &mut Index,
    layout: &Layout,
    fs: &StoreFs,
    frame: &Frame,
) -> Result<(), StoreError> {
    replay(index, layout, fs, slice::from_ref(frame)).map(drop)
}

/// What a reader's snapshot adds to an index handle and the ref files: the frames past the
/// checkpoint, applied in memory.
pub(crate) struct Overlay {
    /// The changesets of those frames that the index handle does not count, in journal
    /// order, their seqs following on from its records.
    extras: Vec<ChangesetMeta>,
    /// Where each extra is in `extras`.
    by_id: HashMap<ChangesetId, usize>,
    /// Every ref the snapshot sees.
    refs: BTreeMap<String, RefState>,
}

impl Overlay {
    /// The overlay of `frames`, the frames past the checkpoint in journal order, on `index`
    /// and `files`, the ref files. The files must show no frame after `frames`, and every
    /// frame of `frames` but the last; then each ref ends up as it will once materialized.
    pub(crate) fn new(
        index: &Index,
        frames: &[Frame],
        mut files: BTreeMap<String, RefState>,
    ) -> Result<Overlay, StoreError> {
        let changesets = changesets(frames);
        let extras: Vec<ChangesetMeta> = changesets[held(index, &changesets)?..]
            .iter()
            .zip(index.len()..)
            .map(|(record, seq)| record.meta(seq))
            .collect();
        let by_id = extras
            .iter()
            .enumerate()
            .map(|(at, meta)| (meta.id, at))
            .collect();
        for (name, target) in ref_targets(frames)? {
            match refs::moved(files.get(name).copied(), target) {
                Some(state) => files.insert(name.to_owned(), state),
                None => files.remove(name),
            };
        }
        Ok(Overlay {
            extras,
            by_id,
            refs: files,
        })
    }

    /// The changesets the index handle does not count, in journal order.
    pub(crate) fn extras(&self) -> &[ChangesetMeta] {
        &self.extras
    }

    /// Changeset `id`, if it is an extra.
    pub(crate) fn changeset(&self, id: ChangesetId) -> Option<&ChangesetMeta> {
        self.by_id.get(&id).map(|&at| &self.extras[at])
    }

    /// Every ref, by name.
    pub(crate) fn refs(&self) -> &BTreeMap<String, RefState> {
        &self.refs
    }
}

/// A `Changeset` record of a frame, its fields borrowed: what replay appends, and what the
/// index compares its records past the checkpoint with ([`Index::open`]).
pub(super) struct ChangesetRecord<'a> {
    pub(super) id: ChangesetId,
    pub(super) generation: u64,
    pub(super) delta_hash: &'a [u8; 32],
    pub(super) parents: &'a [ChangesetId],
}

impl ChangesetRecord<'_> {
    /// The record as the index holds it at `seq`.
    fn meta(&self, seq: u64) -> ChangesetMeta {
        ChangesetMeta {
            id: self.id,
            seq,
            generation: self.generation,
            delta_hash: *self.delta_hash,
            parents: self.parents.to_vec(),
        }
    }
}

/// The `Changeset` records of `frames`, in journal order.
pub(super) fn changesets(frames: &[Frame]) -> Vec<ChangesetRecord<'_>> {
    frames
        .iter()
        .flat_map(|frame| &frame.records)
        .filter_map(|record| match record {
            Record::Changeset {
                id,
                generation,
                delta_hash,
                parents,
            } => Some(ChangesetRecord {
                id: *id,
                generation: *generation,
                delta_hash,
                parents,
            }),
            Record::RefMove { .. } | Record::Compacted { .. } => None,
        })
        .collect()
}

/// How many `Changeset` records `frame` holds.
fn changeset_count(frame: &Frame) -> usize {
    (frame.records.iter())
        .filter(|record| matches!(record, Record::Changeset { .. }))
        .count()
}

/// How many of `changesets`, in journal order, `index` holds: a prefix, so a binary search
/// finds where it ends, after one lookup of the last when the index holds them all.
fn held(index: &Index, changesets: &[ChangesetRecord<'_>]) -> Result<usize, StoreError> {
    let Some((last, before)) = changesets.split_last() else {
        return Ok(0);
    };
    if index.find(last.id)?.is_some() {
        return Ok(changesets.len());
    }
    let (mut low, mut high) = (0, before.len());
    while low < high {
        let mid = low + (high - low) / 2;
        if index.find(before[mid].id)?.is_some() {
            low = mid + 1;
        } else {
            high = mid;
        }
    }
    Ok(low)
}

/// The target each ref that `frames` move is left at: its last move's. A name that is not
/// a ref name is no record this version writes: [`StoreError::JournalCorrupt`] at its
/// frame, before anything reaches the filesystem.
fn ref_targets(frames: &[Frame]) -> Result<BTreeMap<&str, Option<ChangesetId>>, StoreError> {
    let mut targets = BTreeMap::new();
    for frame in frames {
        for record in &frame.records {
            if let Record::RefMove { name, new, .. } = record {
                refs::check_ref_name(name).map_err(|_| StoreError::JournalCorrupt {
                    offset: frame.offset,
                })?;
                targets.insert(name.as_str(), *new);
            }
        }
    }
    Ok(targets)
}
