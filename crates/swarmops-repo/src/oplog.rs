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
//! inserted, the refs it moved and the marker of a compaction ([`OpRecord`]). Its id
//! ([`OpId`], `<seq>:<crc>`) adds the frame header's payload CRC-32: a frame read before a
//! take-back cut it (a lock-free read, a frame committed and then lost) lends its number to the
//! next write, whose id differs unless its payload is the same bytes.
//!
//! # Undo
//!
//! [`Repo::undo`] takes an operation's id, and restores every ref operation N moved to what the
//! ref named before N: for each ref, N's net move, from the first `old` the frame records for it
//! to the last `new` (a frame that moved a ref twice counts once, and one that moved it there and
//! back not at all). It opens one write transaction, and under its lock, after the write's repair
//! of the journal's tail, reads frame N and compares its crc with the id's, before anything
//! else: a frame at N with another crc, or none, is [`RepoError::OpNotFound`], so a frame that a
//! take-back cut is never undone in place of the frame that took its seq. Then it moves each ref,
//! in the order of N's moves, from N's `new` back to N's `old` by compare-and-swap:
//!
//! - a ref that names N's `old` already is skipped: an undo made it so before, or another move
//!   since;
//! - a ref that names neither blocks the undo ([`RepoError::UndoBlocked`]): a later operation
//!   moved it. The transaction is dropped, so nothing is written, the other refs' moves
//!   included;
//! - a move of `main` that takes it off its first-parent history refuses the undo the same way
//!   ([`RepoError::HistoryProtected`], the M15 design §4): the undo of a land, of
//!   `commit --advance` or of a forward `ref set` takes `main` back. It is judged on a snapshot
//!   read under the lock, only for a move the compare-and-swap queued;
//! - with every ref skipped, nothing is written either: the undo is a replay
//!   ([`UndoOutcome::already_applied`]).
//!
//! The moves it makes are one frame, recorded for the undoer: an operation of its own, which
//! another undo can take back. Changesets are immutable and stay; an undo moves refs only. Its
//! frame is the commit point, and a failure after it is a warning, as for a ref move
//! ([`Repo::cas_ref`]).

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

use swarmops_core::ChangesetId;
use swarmops_store::{
    CasResult, Frame, ObjectStore, Record, Recovered, RefKind, StoreError, ref_kind,
};

use crate::error::RepoError;
use crate::repo::{self, Repo};

/// An operation's id, `<seq>:<crc>` (`4:9f3a01c2`): its number and its frame header's payload
/// CRC-32, as 8 lowercase hex digits. A frame a take-back cut and the one that took its seq
/// differ in their crc unless their payloads are equal, so an id never names the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct OpId {
    /// The operation's number: its frame's seq.
    pub seq: u64,
    /// Its frame header's `crc32(payload)`.
    pub crc: u32,
}

impl OpId {
    /// The id of the operation numbered `seq` whose frame's payload CRC-32 is `crc`.
    pub fn new(seq: u64, crc: u32) -> Self {
        Self { seq, crc }
    }
}

impl fmt::Display for OpId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{:08x}", self.seq, self.crc)
    }
}

/// A string that is not an [`OpId`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not an operation id: expected <op>:<crc>, as `op log` prints it (4:9f3a01c2)")]
#[non_exhaustive]
pub struct OpIdError;

/// The longest operation number an id spells: `u64::MAX` has 20 digits.
const MAX_SEQ_DIGITS: usize = 20;

/// The crc's length in an id: 8 lowercase hex digits.
const CRC_DIGITS: usize = 8;

impl FromStr for OpId {
    type Err = OpIdError;

    /// Exactly `^[0-9]{1,20}:[0-9a-f]{8}$` with the number at most `u64::MAX`: a bare number,
    /// upper case, a sign or a space is refused.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (seq, crc) = s.split_once(':').ok_or(OpIdError)?;
        let digits = |part: &str, ok: fn(&u8) -> bool| part.bytes().all(|b| ok(&b));
        if seq.is_empty() || seq.len() > MAX_SEQ_DIGITS || !digits(seq, u8::is_ascii_digit) {
            return Err(OpIdError);
        }
        let lower_hex = |b: &u8| b.is_ascii_digit() || (b'a'..=b'f').contains(b);
        if crc.len() != CRC_DIGITS || !digits(crc, lower_hex) {
            return Err(OpIdError);
        }
        let seq = seq.parse().map_err(|_| OpIdError)?;
        let crc = u32::from_str_radix(crc, 16).map_err(|_| OpIdError)?;
        Ok(Self { seq, crc })
    }
}

/// Why [`Repo::undo`] found no operation by an id ([`RepoError::OpNotFound`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum OpNotFoundReason {
    /// The number is past the journal's last operation.
    PastEnd,
    /// The frame at the number has another crc: the operation named was taken back and the
    /// number went to another, or the crc is mistyped.
    Checksum,
}

impl OpNotFoundReason {
    /// The reason's name, as the operations layer reports it: `past_end` or `checksum`.
    pub fn as_str(self) -> &'static str {
        match self {
            OpNotFoundReason::PastEnd => "past_end",
            OpNotFoundReason::Checksum => "checksum",
        }
    }
}

/// One operation: a write transaction, as the journal holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Operation {
    /// Its number: its frame's seq, from 0 in commit order.
    pub seq: u64,
    /// Its frame header's payload CRC-32: with `seq`, its [`OpId`].
    pub crc: u32,
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
    /// That frame's payload CRC-32: with `frame_seq`, the undo's own [`OpId`]. `None` exactly
    /// when `frame_seq` is.
    pub frame_crc: Option<u32>,
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
    /// [`RepoError::OpNotFound`], [`RepoError::NothingToUndo`], [`RepoError::UndoBlocked`] and
    /// [`RepoError::HistoryProtected`] too. It is no warning, and it is lost when the call fails
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

    /// Undoes operation `id` for `actor`: moves every ref it moved back to what the ref named
    /// before it, in one write transaction of its own, all of them or none (see [the module
    /// documentation](self)). [`RepoError::OpNotFound`] for an operation the journal does not
    /// hold, [`RepoError::NothingToUndo`] for one that left every ref where it found it, and
    /// [`RepoError::UndoBlocked`] when a ref it moved names something else now, and
    /// [`RepoError::HistoryProtected`] when it would move `main` off its first-parent history
    /// (the M15 design §4): nothing is written then.
    ///
    /// A journal append that fails ([`StoreError::NotCommitted`]) is returned as it is: nothing
    /// was undone, and the same undo made again undoes it.
    pub fn undo(&self, id: OpId, actor: &str) -> Result<UndoOutcome, RepoError> {
        let op = id.seq;
        let mut write = self.meta.write()?;
        let recovered = write.recovered();
        // Read under the lock, after the write's tail check: the frame as committed. Its crc
        // must be the id's, before anything else is decided: a frame a take-back cut lent its
        // number to the next write, whose crc differs.
        let moves = {
            let read = self.meta.read()?;
            let frame = match read.frame(op)? {
                Some(frame) if frame.crc == id.crc => frame,
                found => {
                    let reason = if found.is_some() {
                        OpNotFoundReason::Checksum
                    } else {
                        OpNotFoundReason::PastEnd
                    };
                    let last = read.frames_before(None, 1)?.first().map(|frame| frame.seq);
                    return Err(RepoError::OpNotFound {
                        op,
                        crc: id.crc,
                        last,
                        reason,
                        recovered: Box::new(recovered),
                    });
                }
            };
            net_moves(&frame)
        };
        if moves.is_empty() {
            return Err(RepoError::NothingToUndo {
                op,
                recovered: Box::new(recovered),
            });
        }
        for (name, old, new) in moves {
            match write
                .cas_ref(&name, new, old, actor)
                .map_err(repo::ref_error)?
            {
                // Queued: a move of main keeps to its first-parent history, judged on a snapshot
                // read under the lock (main moves once in a frame's net moves). Returning drops
                // the write, as for a block.
                CasResult::Applied { .. } => {
                    if let Err(e) = self.undo_forward(&name, new, old) {
                        return Err(protected(e, id, recovered));
                    }
                }
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
                frame_crc: None,
                moves: made,
                already_applied: true,
                warnings: Vec::new(),
                recovered,
            });
        }
        let mut warnings = Vec::new();
        let (frame_seq, frame_crc) = match write.commit() {
            Ok(info) => (info.frame_seq, info.frame_crc),
            Err(StoreError::PostCommit {
                frame_seq,
                frame_crc,
                source,
            }) => {
                let what = format!("the undo of operation {id}");
                let committed = OpId::new(frame_seq, frame_crc);
                warnings.push(repo::post_commit_warning(&what, committed, &source));
                (frame_seq, Some(frame_crc))
            }
            Err(e) => return Err(e.into()),
        };
        Ok(UndoOutcome {
            undone: op,
            frame_seq: Some(frame_seq),
            frame_crc,
            moves: made,
            already_applied: false,
            warnings,
            recovered,
        })
    }

    /// The queued undo move of `name` from `head` to `target`, if `name` is `main`, judged by
    /// [`crate::RepoRead::check_forward`] on a snapshot read under the write's lock.
    fn undo_forward(
        &self,
        name: &str,
        head: Option<ChangesetId>,
        target: Option<ChangesetId>,
    ) -> Result<(), RepoError> {
        if ref_kind(name) != Some(RefKind::Main) {
            return Ok(());
        }
        self.snapshot()?.check_forward(name, head, target)
    }
}

/// `e`, a refusal of [`crate::RepoRead::check_forward`], as the refusal of the undo of
/// operation `id`, reporting what its write repaired first; any other error as it is.
fn protected(e: RepoError, id: OpId, recovered: Recovered) -> RepoError {
    match e {
        RepoError::HistoryProtected {
            name, head, target, ..
        } => RepoError::HistoryProtected {
            name,
            head,
            target,
            op: Some(id),
            recovered: Box::new(recovered),
        },
        other => other,
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
        crc: frame.crc,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_op_id_round_trips() {
        let id = OpId::new(4, 0x9f3a_01c2);
        assert_eq!(id.to_string(), "4:9f3a01c2", "display");
        assert_eq!("4:9f3a01c2".parse(), Ok(id), "parse");
        assert_eq!("0:00000000".parse(), Ok(OpId::new(0, 0)), "zero");
        let max = format!("{}:ffffffff", u64::MAX);
        assert_eq!(max.parse(), Ok(OpId::new(u64::MAX, u32::MAX)), "maximum");
        assert_eq!("004:9f3a01c2".parse(), Ok(id), "leading zeros");
    }

    #[test]
    fn an_op_id_refuses_every_other_form() {
        let refused = [
            "",
            "4",
            ":9f3a01c2",
            "4:",
            "4:9F3A01C2",
            "4:9f3a",
            "4:9f3a01c2a",
            "4:9f3a01cg",
            "4:9f3a01c2:1",
            "+4:9f3a01c2",
            "-4:9f3a01c2",
            " 4:9f3a01c2",
            "4:9f3a01c2 ",
            "4 :9f3a01c2",
            "4: 9f3a01c",
            "0x4:9f3a01c2",
            "4:0x9f3a01",
            "4:+9f3a01c",
            "18446744073709551616:9f3a01c2",
            "000000000000000000004:9f3a01c2",
            "\u{0664}:9f3a01c2",
        ];
        for s in refused {
            assert_eq!(s.parse::<OpId>(), Err(OpIdError), "{s:?}");
        }
    }
}
