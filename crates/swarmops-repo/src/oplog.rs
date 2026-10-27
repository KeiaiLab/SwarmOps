//! The operation log and `undo` (storage spec §3): the journal's frames read back as the store's
//! operations, newest first ([`Repo::op_log`]), and an operation's ref moves restored by
//! compare-and-swap in one transaction ([`Repo::undo`]).
//!
//! The log lists only operations that are committed and listed: it reads the committed snapshot
//! ([`swarmops_store::MetaStore::read_committed`]), so it waits for the store's lock as a write
//! does. No take-back the crash model allows (a failed sync, a power loss before it) can reach a
//! frame it lists, so a number it lists is never given to another operation.
//!
//! An operation is one write transaction: a frame of the journal, numbered by the frame's seq,
//! from 0 in commit order. Its records are what the frame holds, in its order: the changesets it
//! inserted, the refs it moved and the marker of a compaction ([`OpRecord`]).
//!
//! # Undo
//!
//! [`Repo::undo`] restores every ref operation N moved to what the ref named before N: for each
//! ref, N's net move, from the first `old` the frame records for it to the last `new` (a frame
//! that moved a ref twice counts once, and one that moved it there and back not at all). It opens
//! one write transaction, and under its lock, after the write's repair of the journal's tail,
//! reads frame N: so a frame that a failed sync took back is never undone in place of the frame
//! that took its seq. Then it moves each ref, in the order of N's moves, from N's `new` back to
//! N's `old` by compare-and-swap:
//!
//! - a ref that names N's `old` already is skipped: an undo made it so before, or another move
//!   since;
//! - a ref that names neither blocks the undo ([`RepoError::UndoBlocked`]): a later operation
//!   moved it. The transaction is dropped, so nothing is written, the other refs' moves
//!   included;
//! - with every ref skipped, nothing is written either: the undo is a replay
//!   ([`UndoOutcome::already_applied`]).
//!
//! The moves it makes are one frame, recorded for the undoer: an operation of its own, which
//! another undo can take back. Changesets are immutable and stay; an undo moves refs only. Its
//! frame is the commit point, and a failure after it is a warning, as for a ref move
//! ([`Repo::cas_ref`]).

use std::collections::HashMap;

use swarmops_core::ChangesetId;
use swarmops_store::{CasResult, Frame, ObjectStore, Record, Recovered, StoreError};

use crate::error::RepoError;
use crate::repo::{self, Repo};

/// One operation: a write transaction, as the journal holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Operation {
    /// Its number: its frame's seq, from 0 in commit order.
    pub seq: u64,
    /// What it recorded, in its frame's order.
    pub records: Vec<OpRecord>,
}

/// One record of an [`Operation`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum OpRecord {
    /// A changeset joined the store.
    Changeset {
        /// Its id.
        id: ChangesetId,
        /// Its parents, the mainline parent first.
        parents: Vec<ChangesetId>,
    },
    /// A ref moved.
    RefMove {
        /// The ref.
        name: String,
        /// What it named before the move; `None` if the move created it.
        old: Option<ChangesetId>,
        /// What it named after the move; `None` if the move deleted it.
        new: Option<ChangesetId>,
        /// Who moved it.
        actor: String,
        /// When it moved, in Unix seconds, as its writer recorded it.
        ts: i64,
    },
    /// A compaction of the changeset index finished: an advisory marker (storage spec §3).
    Compacted,
}

/// What [`Repo::undo`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct UndoOutcome {
    /// The operation undone.
    pub undone: u64,
    /// The seq of the frame that recorded the undo: an operation of its own, which another undo
    /// can take back. `None` when it wrote nothing ([`UndoOutcome::already_applied`]).
    pub frame_seq: Option<u64>,
    /// The moves it made, in the order of the operation's: each the ref, what it named before
    /// (what the operation set it to), what it names after (what it named before the
    /// operation), and the time the undo's frame records for the move, in Unix seconds. Empty
    /// when it wrote nothing.
    pub moves: Vec<(String, Option<ChangesetId>, Option<ChangesetId>, i64)>,
    /// Whether every ref the operation moved named what it named before the operation already,
    /// so that nothing was written: an undo made again is a replay.
    pub already_applied: bool,
    /// Steps after the commit point that failed, one line each: the undo is committed all the
    /// same.
    pub warnings: Vec<String>,
    /// What the write transaction's recovery repaired before the undo, of what earlier writers
    /// left ([`swarmops_store::MetaWrite::recovered`]): on every outcome, and with
    /// [`RepoError::UndoBlocked`] too. It is no warning, and it is lost when the call fails
    /// otherwise after the repair.
    pub recovered: Recovered,
}

impl<O: ObjectStore> Repo<O> {
    /// Up to `limit` operations numbered below `before` (from the newest for `None`), newest
    /// first, each with its records, from one committed snapshot of the metadata; and whether
    /// older operations follow them in it.
    ///
    /// It lists only operations that are committed and listed, and waits for the store's lock as
    /// a write does (see [the module documentation](self)): [`StoreError::Busy`] after the lock
    /// timeout, and the tail check's errors ([`StoreError::NotCommitted`], a read-only
    /// filesystem) are returned as they are, never a lock-free listing instead.
    pub fn op_log(
        &self,
        before: Option<u64>,
        limit: usize,
    ) -> Result<(Vec<Operation>, bool), RepoError> {
        // The tail check's repair is no warning, as for the other committed readers.
        let (snapshot, _repair) = self.meta.read_committed()?;
        let mut frames = snapshot.frames_before(before, limit.saturating_add(1))?;
        let older = frames.len() > limit;
        frames.truncate(limit);
        let ops = frames
            .into_iter()
            .map(operation)
            .collect::<Result<_, _>>()?;
        Ok((ops, older))
    }

    /// Undoes operation `op` for `actor`: moves every ref it moved back to what the ref named
    /// before it, in one write transaction of its own, all of them or none (see [the module
    /// documentation](self)). [`RepoError::OpNotFound`] for an operation the journal does not
    /// hold, [`RepoError::NothingToUndo`] for one that left every ref where it found it, and
    /// [`RepoError::UndoBlocked`] when a ref it moved names something else now: nothing is
    /// written then.
    ///
    /// A journal append that fails ([`StoreError::NotCommitted`]) is returned as it is: nothing
    /// was undone, and the same undo made again undoes it.
    pub fn undo(&self, op: u64, actor: &str) -> Result<UndoOutcome, RepoError> {
        let mut write = self.meta.write()?;
        let recovered = write.recovered();
        // Read under the lock, after the write's tail check: the frame as committed.
        let moves = {
            let read = self.meta.read()?;
            let Some(frame) = read.frame(op)? else {
                let last = read.frames_before(None, 1)?.first().map(|frame| frame.seq);
                return Err(RepoError::OpNotFound { op, last });
            };
            net_moves(&frame)
        };
        if moves.is_empty() {
            return Err(RepoError::NothingToUndo(op));
        }
        for (name, old, new) in moves {
            match write
                .cas_ref(&name, new, old, actor)
                .map_err(repo::ref_error)?
            {
                CasResult::Applied { .. } => {}
                // Back where the operation found it already: nothing to move.
                CasResult::Mismatch { actual } if actual == old => {}
                // Moved since. Returning drops the write, which appends nothing and releases the
                // lock: none of the moves queued before this one is made either.
                CasResult::Mismatch { actual } => {
                    return Err(RepoError::UndoBlocked {
                        op,
                        name,
                        expected: new,
                        actual,
                        recovered: Box::new(recovered),
                    });
                }
                _ => return Err(RepoError::unknown_variant("CasResult")),
            }
        }
        let made: Vec<(String, Option<ChangesetId>, Option<ChangesetId>, i64)> = write
            .queued()
            .iter()
            .filter_map(|record| match record {
                Record::RefMove {
                    name, old, new, ts, ..
                } => Some(Ok((name.clone(), *old, *new, *ts))),
                Record::Changeset { .. } | Record::Compacted { .. } => None,
                _ => Some(Err(RepoError::unknown_variant("Record"))),
            })
            .collect::<Result<_, _>>()?;
        if made.is_empty() {
            // Every ref was back already: dropping the write appends nothing.
            return Ok(UndoOutcome {
                undone: op,
                frame_seq: None,
                moves: made,
                already_applied: true,
                warnings: Vec::new(),
                recovered,
            });
        }
        let mut warnings = Vec::new();
        let frame_seq = match write.commit() {
            Ok(info) => info.frame_seq,
            Err(StoreError::PostCommit { frame_seq, source }) => {
                let what = format!("the undo of operation {op}");
                warnings.push(repo::post_commit_warning(&what, frame_seq, &source));
                frame_seq
            }
            Err(e) => return Err(e.into()),
        };
        Ok(UndoOutcome {
            undone: op,
            frame_seq: Some(frame_seq),
            moves: made,
            already_applied: false,
            warnings,
            recovered,
        })
    }
}

/// `frame` as an operation.
fn operation(frame: Frame) -> Result<Operation, RepoError> {
    let records = frame.records.into_iter().map(|record| match record {
        Record::Changeset { id, parents, .. } => Ok(OpRecord::Changeset { id, parents }),
        Record::RefMove {
            name,
            old,
            new,
            actor,
            ts,
        } => Ok(OpRecord::RefMove {
            name,
            old,
            new,
            actor,
            ts,
        }),
        Record::Compacted { .. } => Ok(OpRecord::Compacted),
        _ => Err(RepoError::unknown_variant("Record")),
    });
    let records = records.collect::<Result<_, RepoError>>()?;
    Ok(Operation {
        seq: frame.seq,
        records,
    })
}

/// The net move of each ref `frame` moved, in the order of its first move: the ref, the first
/// `old` the frame records for it and the last `new`. A ref the frame left where it found it has
/// none.
fn net_moves(frame: &Frame) -> Vec<(String, Option<ChangesetId>, Option<ChangesetId>)> {
    let mut moves: Vec<(String, Option<ChangesetId>, Option<ChangesetId>)> = Vec::new();
    let mut at: HashMap<&str, usize> = HashMap::new();
    for record in &frame.records {
        let Record::RefMove { name, old, new, .. } = record else {
            continue;
        };
        if let Some(&i) = at.get(name.as_str()) {
            moves[i].2 = *new;
        } else {
            at.insert(name, moves.len());
            moves.push((name.clone(), *old, *new));
        }
    }
    moves.retain(|(_, old, new)| old != new);
    moves
}
