//! Journal records and frames: fixed little-endian layouts, no varints.
//!
//! ```text
//! frame     = u32le len | u32le crc32(payload) | payload      len = payload bytes, never 0
//! payload   = u16le record count (1 to 65,535) | records
//! Changeset = u8 1 | id[32] | u64le generation | delta_hash[32] | u8 n | parents[n][32]
//! RefMove   = u8 2 | u8 name_len | name | u8 has_old | old[32]? | u8 has_new | new[32]?
//!             | u8 actor_len | actor | i64le ts
//! Compacted = u8 4 | u64le sorted_count
//! ```
//!
//! A frame is one transaction: its records commit together. Kinds 3 (`ReviewEvent`) and 5
//! (`Annotation`) are reserved for later milestones (storage spec §3). Decoding checks every
//! length against the bytes at hand before it slices, so garbage gives an error, never a
//! panic.

use swarmops_core::bytes::Reader;
use swarmops_core::{ChangesetId, ObjectId};

use crate::error::{StoreError, invalid_input};

/// A frame header: `u32le len | u32le crc32(payload)`.
pub(crate) const HEADER_LEN: usize = 8;

/// The most records a frame holds: its record count is a `u16le`.
pub(crate) const MAX_RECORDS: usize = 65_535;

const KIND_CHANGESET: u8 = 1;
const KIND_REF_MOVE: u8 = 2;
const KIND_COMPACTED: u8 = 4;

/// One change to the metadata, as the journal records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Record {
    /// A changeset joined the index.
    Changeset {
        /// The changeset's id.
        id: ChangesetId,
        /// 1 + the largest parent generation; 1 for a root.
        generation: u64,
        /// The hash of the changeset's delta section (storage spec §1).
        delta_hash: [u8; 32],
        /// The parents in order, the mainline parent first; at most 255.
        parents: Vec<ChangesetId>,
    },
    /// A ref moved.
    RefMove {
        /// The ref's name; at most 255 bytes.
        name: String,
        /// The target before the move; `None` if the ref did not exist.
        old: Option<ChangesetId>,
        /// The target after the move; `None` if the ref was deleted.
        new: Option<ChangesetId>,
        /// Who moved it; at most 255 bytes.
        actor: String,
        /// When it moved, as the writer recorded it; stored verbatim.
        ts: i64,
    },
    /// A compaction of the changeset index finished.
    Compacted {
        /// How many index records the sorted views hold now.
        sorted_count: u64,
    },
}

/// One transaction as the journal holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// The frame's index in `journal.off`, from 0: its place in the journal's total order.
    pub seq: u64,
    /// Where the frame starts in `journal.log`.
    pub offset: u64,
    /// Its records, in the order they were appended.
    pub records: Vec<Record>,
}

impl Record {
    /// Appends the record's encoding to `out`.
    ///
    /// # Panics
    ///
    /// If a length does not fit its `u8` field: more than 255 parents, or a ref name or
    /// an actor longer than 255 bytes. [`crate::Journal::append_frame`] refuses such
    /// records with an error instead.
    pub fn encode(&self, out: &mut Vec<u8>) {
        if let Err(too_long) = self.try_encode(out) {
            panic!("cannot encode {too_long}");
        }
    }

    /// Decodes the record at the start of `buf`, returning it and how many bytes it took;
    /// the bytes after it are left alone.
    ///
    /// Malformed or truncated bytes give [`StoreError::JournalCorrupt`], with `offset` the
    /// position in `buf` where decoding stopped.
    pub fn decode(buf: &[u8]) -> Result<(Record, usize), StoreError> {
        let mut r = Reader::new(buf, corrupt_at);
        let record = match r.u8()? {
            KIND_CHANGESET => Self::Changeset {
                id: r.id()?,
                generation: r.u64_le()?,
                delta_hash: r.array()?,
                parents: {
                    let n = r.u8()?;
                    (0..n).map(|_| r.id()).collect::<Result<_, _>>()?
                },
            },
            KIND_REF_MOVE => Self::RefMove {
                name: r.string()?,
                old: r.optional_id()?,
                new: r.optional_id()?,
                actor: r.string()?,
                ts: r.i64_le()?,
            },
            KIND_COMPACTED => Self::Compacted {
                sorted_count: r.u64_le()?,
            },
            _ => return Err(r.error_at(0)),
        };
        Ok((record, r.pos()))
    }

    /// [`Record::encode`], or which field is too long for its `u8` length; `out` is left
    /// untouched then.
    fn try_encode(&self, out: &mut Vec<u8>) -> Result<(), &'static str> {
        match self {
            Self::Changeset {
                id,
                generation,
                delta_hash,
                parents,
            } => {
                let n = u8::try_from(parents.len()).map_err(|_| "more than 255 parents")?;
                out.push(KIND_CHANGESET);
                out.extend_from_slice(&id.0.0);
                out.extend_from_slice(&generation.to_le_bytes());
                out.extend_from_slice(delta_hash);
                out.push(n);
                for parent in parents {
                    out.extend_from_slice(&parent.0.0);
                }
            }
            Self::RefMove {
                name,
                old,
                new,
                actor,
                ts,
            } => {
                let name_len =
                    u8::try_from(name.len()).map_err(|_| "a ref name longer than 255 bytes")?;
                let actor_len =
                    u8::try_from(actor.len()).map_err(|_| "an actor longer than 255 bytes")?;
                out.push(KIND_REF_MOVE);
                out.push(name_len);
                out.extend_from_slice(name.as_bytes());
                put_optional_id(out, *old);
                put_optional_id(out, *new);
                out.push(actor_len);
                out.extend_from_slice(actor.as_bytes());
                out.extend_from_slice(&ts.to_le_bytes());
            }
            Self::Compacted { sorted_count } => {
                out.push(KIND_COMPACTED);
                out.extend_from_slice(&sorted_count.to_le_bytes());
            }
        }
        Ok(())
    }
}

fn put_optional_id(out: &mut Vec<u8>, id: Option<ChangesetId>) {
    match id {
        Some(id) => {
            out.push(1);
            out.extend_from_slice(&id.0.0);
        }
        None => out.push(0),
    }
}

/// The bytes of a frame holding `records`. Refuses, with `InvalidInput`, no records, more
/// than 65,535, or a record whose lengths do not fit the layout.
pub(crate) fn encode_frame(records: &[Record]) -> Result<Vec<u8>, StoreError> {
    let count = u16::try_from(records.len())
        .ok()
        .filter(|&count| count > 0)
        .ok_or_else(|| {
            invalid_input(format!(
                "a frame holds 1 to 65,535 records, not {}",
                records.len()
            ))
        })?;
    let mut frame = vec![0; HEADER_LEN];
    frame.extend_from_slice(&count.to_le_bytes());
    for record in records {
        record
            .try_encode(&mut frame)
            .map_err(|too_long| invalid_input(format!("cannot encode {too_long}")))?;
    }
    let payload = &frame[HEADER_LEN..];
    // At most 65,535 records of at most 8,234 bytes (a changeset with 255 parents).
    let len = u32::try_from(payload.len()).expect("a frame payload fits in u32");
    let crc = crc32fast::hash(payload);
    frame[..4].copy_from_slice(&len.to_le_bytes());
    frame[4..HEADER_LEN].copy_from_slice(&crc.to_le_bytes());
    Ok(frame)
}

/// What the bytes at a frame boundary hold.
pub(crate) enum Parsed {
    /// A whole frame whose payload matches its CRC and decodes.
    Whole {
        records: Vec<Record>,
        /// The frame's size in bytes, header included.
        size: usize,
    },
    /// The bytes end before the frame does: its header, or the payload the header
    /// announces, is cut short.
    Short,
    /// The frame is all there, but its payload does not match its CRC.
    BadCrc,
    /// An empty payload, or one that matches its CRC but does not decode: never a torn write.
    Invalid,
}

/// Parses the frame at the start of `buf`, which runs from a frame boundary to (at most)
/// the end of the journal.
pub(crate) fn parse_frame(buf: &[u8]) -> Parsed {
    let Some((header, rest)) = buf.split_first_chunk::<HEADER_LEN>() else {
        return Parsed::Short;
    };
    let len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
    let crc = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
    if len == 0 {
        return Parsed::Invalid;
    }
    let Some(payload) = usize::try_from(len).ok().and_then(|len| rest.get(..len)) else {
        return Parsed::Short;
    };
    if crc32fast::hash(payload) != crc {
        return Parsed::BadCrc;
    }
    match decode_payload(payload) {
        Some(records) => Parsed::Whole {
            records,
            size: HEADER_LEN + payload.len(),
        },
        None => Parsed::Invalid,
    }
}

/// The records of a frame payload: a count of at least 1, then exactly that many records.
fn decode_payload(payload: &[u8]) -> Option<Vec<Record>> {
    let (count, mut rest) = payload.split_first_chunk::<2>()?;
    let count = usize::from(u16::from_le_bytes(*count));
    if count == 0 {
        return None;
    }
    // Every record takes at least one byte, so the count cannot reserve beyond the payload.
    let mut records = Vec::with_capacity(count.min(rest.len()));
    for _ in 0..count {
        let (record, used) = Record::decode(rest).ok()?;
        records.push(record);
        rest = rest.get(used..)?;
    }
    rest.is_empty().then_some(records)
}

/// What a record's bytes that do not decode are: [`StoreError::JournalCorrupt`] at `pos`, the
/// position in them where decoding stopped.
fn corrupt_at(pos: usize) -> StoreError {
    StoreError::JournalCorrupt {
        offset: u64::try_from(pos).expect("a buffer position fits in u64"),
    }
}

/// The reads of a record's fields beyond fixed-width integers.
trait RecordReader {
    fn id(&mut self) -> Result<ChangesetId, StoreError>;

    /// A `u8` flag (0 or 1), then the id if the flag is 1.
    fn optional_id(&mut self) -> Result<Option<ChangesetId>, StoreError>;

    /// A `u8` length, then that many bytes of UTF-8.
    fn string(&mut self) -> Result<String, StoreError>;
}

impl RecordReader for Reader<'_, StoreError> {
    fn id(&mut self) -> Result<ChangesetId, StoreError> {
        self.array().map(|bytes| ChangesetId(ObjectId(bytes)))
    }

    fn optional_id(&mut self) -> Result<Option<ChangesetId>, StoreError> {
        let flag_at = self.pos();
        match self.u8()? {
            0 => Ok(None),
            1 => self.id().map(Some),
            _ => Err(self.error_at(flag_at)),
        }
    }

    fn string(&mut self) -> Result<String, StoreError> {
        let len = usize::from(self.u8()?);
        let start = self.pos();
        let bytes = self.bytes(len)?;
        let text = std::str::from_utf8(bytes).map_err(|_| self.error_at(start))?;
        Ok(text.to_owned())
    }
}
