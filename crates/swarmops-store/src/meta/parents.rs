//! `meta/parents.log`: the parents of every changeset in the index, back to back in record
//! order, one 32-byte id each; and the `idx/par.col` entry that finds a record's parents
//! there.
//!
//! ```text
//! parents.log   parent id[32] × (sum of the records' parent counts)
//! par.col       u64le per record = parents_offset << 8 | parents_count
//! ```
//!
//! `parents_offset` is the byte offset of the record's first parent in `parents.log`, so the
//! offsets grow with the records. A record with no parents points where the next record's
//! parents will start.

use std::io;

use swarmops_core::{ChangesetId, ObjectId};

use super::column::AppendOnlyColumn;
use crate::error::StoreError;

/// The size of one parent id in `parents.log`.
pub(crate) const PARENT_LEN: u64 = 32;

/// The most parents a record can have: its count is one byte of its `par.col` entry.
pub(crate) const MAX_PARENTS: usize = 255;

/// The most bytes of `parents.log` one record's parents take.
pub(crate) const MAX_PARENTS_LEN: usize = 32 * MAX_PARENTS;

/// Where a record's parents lie in `parents.log`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ParentsAt {
    /// The byte offset of the first parent.
    pub(crate) offset: u64,
    /// How many parents follow it.
    pub(crate) count: u8,
}

impl ParentsAt {
    /// The largest offset the 56 high bits of a `par.col` entry hold.
    pub(crate) const MAX_OFFSET: u64 = u64::MAX >> 8;

    /// The `par.col` entry; `offset` must be at most [`ParentsAt::MAX_OFFSET`].
    pub(crate) fn entry(self) -> u64 {
        debug_assert!(
            self.offset <= Self::MAX_OFFSET,
            "Index::append_batch refuses a parents.log offset past 56 bits"
        );
        (self.offset << 8) | u64::from(self.count)
    }

    /// The parents a `par.col` entry points at.
    pub(crate) fn from_entry(entry: u64) -> Self {
        Self {
            offset: entry >> 8,
            count: entry.to_le_bytes()[0],
        }
    }

    /// Where the parents end in `parents.log`.
    pub(crate) fn end(self) -> u64 {
        self.offset + PARENT_LEN * u64::from(self.count)
    }

    /// Whether the parents start on a parent boundary and end by `log_len`, the length of
    /// `parents.log` (or of the part of it that the records in question use).
    pub(crate) fn lies_within(self, log_len: u64) -> bool {
        self.offset.is_multiple_of(PARENT_LEN) && self.end() <= log_len
    }
}

/// Writes the ids of `parents` into the start of `buf`, as `parents.log` holds them, and
/// returns that part of `buf`.
///
/// # Panics
///
/// If `buf` is too short: it must hold 32 bytes per parent.
pub(crate) fn encode<'a>(parents: &[ChangesetId], buf: &'a mut [u8]) -> &'a [u8] {
    let bytes = &mut buf[..32 * parents.len()];
    for (slot, parent) in bytes.as_chunks_mut::<32>().0.iter_mut().zip(parents) {
        *slot = parent.0.0;
    }
    bytes
}

/// The parents at `at` in `parents.log`, read with one `read_at`. `log_len` is how much of
/// `parents.log` the caller's records use: parents that lie off a parent boundary or past it
/// are [`StoreError::IndexCorrupt`].
pub(crate) fn read(
    log: &AppendOnlyColumn<1>,
    at: ParentsAt,
    log_len: u64,
) -> Result<Vec<ChangesetId>, StoreError> {
    if !at.lies_within(log_len) {
        return Err(StoreError::IndexCorrupt {
            what: "a record's parents lie off parents.log's parent boundaries or past its end",
        });
    }
    let mut buf = [0; MAX_PARENTS_LEN];
    let bytes = &mut buf[..32 * usize::from(at.count)];
    log.read_at(at.offset, bytes).map_err(|e| {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            StoreError::IndexCorrupt {
                what: "a record's parents lie past the end of parents.log",
            }
        } else {
            e.into()
        }
    })?;
    Ok(bytes
        .as_chunks::<32>()
        .0
        .iter()
        .map(|id| ChangesetId(ObjectId(*id)))
        .collect())
}
