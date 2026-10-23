//! Journal records and frames: fixed little-endian layouts, no varints.
//!
//! ```text
//! frame     = u32le len | u32le crc32(payload) | payload      len = payload bytes, never 0
//! frame2    = u32le len | u32le crc32(payload) | u64le seq | u32le prev_crc
//!             | u32le crc32(the 20 bytes before) | payload    (FORMAT 2, the M6 design §2.3)
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

use swarmops_core::bytes::{self, Reader};
use swarmops_core::{ChangesetId, ObjectId};
use thiserror::Error;

use crate::error::{LimitKind, StoreError};
use crate::format::StoreFormat;

/// A frame header: `u32le len | u32le crc32(payload)`.
pub(crate) const HEADER_LEN: usize = 8;

/// A FORMAT 2 frame header: `u32le len | u32le crc32(payload) | u64le seq | u32le prev_crc |
/// u32le hcrc`, `hcrc` the CRC of the 20 bytes before it.
pub(crate) const HEADER_LEN_V2: usize = 24;

/// The bytes of a FORMAT 2 header that its own CRC covers.
const HCRC_AT: usize = 20;

/// The most records a frame holds: its record count is a `u16le`.
pub(crate) const MAX_RECORDS: usize = 65_535;

/// The longest record: a changeset with 255 parents (74 bytes and 32 per parent).
const MAX_RECORD_LEN: usize = 74 + 32 * 255;

/// The largest payload this build decodes, 539,615,192 bytes: its count, then 65,535 records of
/// the longest kind. A FORMAT 2 header that checks and announces more (or 0) is no frame a
/// writer wrote.
pub(crate) const MAX_PAYLOAD_LEN: usize = 2 + MAX_RECORDS * MAX_RECORD_LEN;

const KIND_CHANGESET: u8 = 1;
const KIND_REF_MOVE: u8 = 2;
const KIND_COMPACTED: u8 = 4;

/// One change to the metadata, as the journal records it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
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

/// Bytes that are not a journal record: where [`Record::decode`] stopped in them. A position in
/// the bytes it was given, not in `journal.log`: a reader of the journal reports a frame whose
/// records do not decode as [`StoreError::JournalCorrupt`] at the frame's own offset there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("not a journal record: it does not decode at byte {at}")]
#[non_exhaustive]
pub struct BadRecord {
    /// Where in the bytes decoding stopped: the start of the field it could not read, or of
    /// the record for a kind it does not know.
    pub at: usize,
}

/// One transaction as the journal holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Frame {
    /// The frame's index in `journal.off`, from 0: its place in the journal's total order.
    pub seq: u64,
    /// Where the frame starts in `journal.log`.
    pub offset: u64,
    /// Its records, in the order they were appended.
    pub records: Vec<Record>,
}

impl Frame {
    /// The frame `seq` at `offset`, holding `records`.
    ///
    /// Invariant: `seq` and `offset` are the frame's own in `journal.off` and `journal.log`.
    /// Nothing checks it here, and nothing needs to: no store call takes a frame, and the
    /// journal's reads take both from `journal.off` (see [`crate::Journal`]). A frame built
    /// here is a value to compare with what the journal returns.
    pub fn new(seq: u64, offset: u64, records: Vec<Record>) -> Frame {
        Frame {
            seq,
            offset,
            records,
        }
    }
}

impl Record {
    /// Decodes the record at the start of `buf`, returning it and how many bytes it took;
    /// the bytes after it are left alone.
    ///
    /// Malformed or truncated bytes give [`BadRecord`], at the position in `buf` where decoding
    /// stopped.
    pub fn decode(buf: &[u8]) -> Result<(Record, usize), BadRecord> {
        let mut r = Reader::new(buf, bad_record);
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

    /// Appends the record's encoding to `out`. A length that does not fit its `u8` field (more
    /// than 255 parents, or a ref name or an actor longer than 255 bytes) is
    /// [`StoreError::Limit`], and `out` is left untouched then.
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<(), StoreError> {
        match self {
            Self::Changeset {
                id,
                generation,
                delta_hash,
                parents,
            } => {
                let n = fits_u8(LimitKind::Parents, parents.len())?;
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
                let name_len = fits_u8(LimitKind::RefNameBytes, name.len())?;
                let actor_len = fits_u8(LimitKind::ActorBytes, actor.len())?;
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

/// `len` as the `u8` a record's length field holds; past 255, [`StoreError::Limit`] for `what`.
fn fits_u8(what: LimitKind, len: usize) -> Result<u8, StoreError> {
    u8::try_from(len).map_err(|_| StoreError::Limit {
        what,
        max: u64::from(u8::MAX),
        got: bytes::to_u64(len),
    })
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

/// The bytes of a frame holding `records`. Refuses more than 65,535 records, or a record
/// whose lengths do not fit the layout, with [`StoreError::Limit`]; no records, which no
/// transaction appends, with [`StoreError::BadCall`].
pub(crate) fn encode_frame(records: &[Record]) -> Result<Vec<u8>, StoreError> {
    encode_behind(HEADER_LEN, records)
}

/// The bytes of a frame holding `records` behind a header of `header_len` bytes, of which the
/// first 8 (`len`, `crc`) are written and the rest left 0: [`encode_frame`]'s checks.
fn encode_behind(header_len: usize, records: &[Record]) -> Result<Vec<u8>, StoreError> {
    if records.is_empty() {
        return Err(StoreError::BadCall(
            "a frame holds at least one record".to_owned(),
        ));
    }
    let count = u16::try_from(records.len()).map_err(|_| StoreError::Limit {
        what: LimitKind::Records,
        max: bytes::to_u64(MAX_RECORDS),
        got: bytes::to_u64(records.len()),
    })?;
    let mut frame = vec![0; header_len];
    frame.extend_from_slice(&count.to_le_bytes());
    for record in records {
        record.encode(&mut frame)?;
    }
    let payload = &frame[header_len..];
    // At most 65,535 records of at most 8,234 bytes (a changeset with 255 parents).
    let len = u32::try_from(payload.len()).expect("a frame payload fits in u32");
    let crc = crc32fast::hash(payload);
    frame[..4].copy_from_slice(&len.to_le_bytes());
    frame[4..8].copy_from_slice(&crc.to_le_bytes());
    Ok(frame)
}

/// The payload CRC a frame's header holds, in either format: its bytes 4 to 8.
pub(crate) fn payload_crc(frame: &[u8]) -> u32 {
    u32::from_le_bytes([frame[4], frame[5], frame[6], frame[7]])
}

/// Writes the place of `frame`, the bytes of a FORMAT 2 frame, into its header: its `seq`, the
/// payload CRC `prev_crc` of the frame before it, and the header's own CRC over them.
pub(crate) fn set_place(frame: &mut [u8], seq: u64, prev_crc: u32) {
    frame[8..16].copy_from_slice(&seq.to_le_bytes());
    frame[16..HCRC_AT].copy_from_slice(&prev_crc.to_le_bytes());
    let hcrc = crc32fast::hash(&frame[..HCRC_AT]);
    frame[HCRC_AT..HEADER_LEN_V2].copy_from_slice(&hcrc.to_le_bytes());
}

/// A frame header, as [`decode_header_in`] reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FrameHeader {
    /// The payload's length in bytes.
    pub(crate) len: u32,
    /// The payload's CRC.
    pub(crate) crc: u32,
    /// The frame's seq, which FORMAT 2 headers hold; `None` in FORMAT 1.
    pub(crate) seq: Option<u64>,
    /// The CRC of the payload of the frame before, which FORMAT 2 headers hold (0 for seq 0);
    /// `None` in FORMAT 1.
    pub(crate) prev_crc: Option<u32>,
}

/// The size of a frame header in `format`.
pub(crate) fn header_len(format: StoreFormat) -> usize {
    match format {
        StoreFormat::V1 => HEADER_LEN,
        StoreFormat::V2 => HEADER_LEN_V2,
    }
}

/// The bytes of a frame of `format` holding `records`: [`encode_frame`] in FORMAT 1, where
/// `seq` and `prev_crc` are not written; in FORMAT 2 the frame's seq and the payload CRC of the
/// frame before it go in its header ([`set_place`]).
pub(crate) fn encode_frame_in(
    format: StoreFormat,
    records: &[Record],
    seq: u64,
    prev_crc: u32,
) -> Result<Vec<u8>, StoreError> {
    match format {
        StoreFormat::V1 => encode_frame(records),
        StoreFormat::V2 => {
            let mut frame = encode_behind(HEADER_LEN_V2, records)?;
            set_place(&mut frame, seq, prev_crc);
            Ok(frame)
        }
    }
}

/// Why the bytes at a FORMAT 2 frame boundary are no header to trust.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BadHeader {
    /// Its CRC does not match the 20 bytes before it: a torn write, unlisted at the tail; damage
    /// elsewhere.
    Crc,
    /// Its CRC matches, but its length is 0 or more than [`MAX_PAYLOAD_LEN`]: no writer writes
    /// that, so it is damage wherever it lies.
    Len,
}

/// The header at the start of `buf`, a frame boundary, in `format`; `None` if `buf` ends
/// before the header does. FORMAT 2 checks the header's own CRC before it trusts `len`
/// ([`BadHeader::Crc`]), then `len` ([`BadHeader::Len`]); FORMAT 1 has no check to make.
pub(crate) fn decode_header_in(
    format: StoreFormat,
    buf: &[u8],
) -> Result<Option<FrameHeader>, BadHeader> {
    match format {
        StoreFormat::V1 => {
            let Some((header, _)) = buf.split_first_chunk::<HEADER_LEN>() else {
                return Ok(None);
            };
            let (len, crc) = header.split_at(4);
            Ok(Some(FrameHeader {
                len: u32::from_le_bytes(len.try_into().expect("4 bytes")),
                crc: u32::from_le_bytes(crc.try_into().expect("4 bytes")),
                seq: None,
                prev_crc: None,
            }))
        }
        StoreFormat::V2 => {
            let Some((header, _)) = buf.split_first_chunk::<HEADER_LEN_V2>() else {
                return Ok(None);
            };

            // 1. The header's own CRC first: nothing in it is trusted before.
            let u32_at = |at: usize| {
                u32::from_le_bytes([header[at], header[at + 1], header[at + 2], header[at + 3]])
            };
            if crc32fast::hash(&header[..HCRC_AT]) != u32_at(HCRC_AT) {
                return Err(BadHeader::Crc);
            }

            // 2. Then its length, against what this build decodes.
            let len = u32_at(0);
            if len == 0 || usize::try_from(len).is_ok_and(|len| len > MAX_PAYLOAD_LEN) {
                return Err(BadHeader::Len);
            }
            let (seq, _) = header[8..].split_first_chunk::<8>().expect("24 bytes");
            Ok(Some(FrameHeader {
                len,
                crc: u32_at(4),
                seq: Some(u64::from_le_bytes(*seq)),
                prev_crc: Some(u32_at(16)),
            }))
        }
    }
}

/// Where a frame must sit, which a FORMAT 2 header names: its seq, and the payload CRC of the
/// frame before it (0 before frame 0). FORMAT 1 headers name neither, and nothing checks it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Place {
    /// The frame's seq: its index in the journal.
    pub(crate) seq: u64,
    /// The payload CRC of the frame before it; 0 for frame 0.
    pub(crate) prev_crc: u32,
}

impl FrameHeader {
    /// Whether the header names `place` as its own: always, in FORMAT 1.
    pub(crate) fn sits_at(&self, place: Place) -> bool {
        self.seq.is_none_or(|seq| seq == place.seq)
            && self.prev_crc.is_none_or(|prev| prev == place.prev_crc)
    }
}

/// What the bytes at a frame boundary hold.
pub(crate) enum Parsed {
    /// A whole frame whose payload matches its CRC and decodes.
    Whole {
        records: Vec<Record>,
        /// The frame's size in bytes, header included.
        size: usize,
        /// Its payload's CRC.
        crc: u32,
    },
    /// The bytes end before the frame does: its header, or the payload the header
    /// announces, is cut short.
    Short,
    /// The frame is all there, but its payload does not match its CRC; or a FORMAT 2 header
    /// does not match its own ([`BadHeader::Crc`]).
    BadCrc,
    /// A header announcing an empty payload (a length of 0, `zero_len`), or a payload that
    /// matches its CRC but does not decode. Listed, either is damage. Unlisted, the second is a
    /// frame a writer wrote, and the first a header that never reached the disk, a torn write
    /// (see [`crate::Journal::truncate_torn_tail`]). FORMAT 2 has no such header: see
    /// [`Parsed::Corrupt`].
    Invalid {
        /// Whether the header announced a length of 0.
        zero_len: bool,
    },
    /// A FORMAT 2 header that matches its CRC but announces a length no writer writes
    /// ([`BadHeader::Len`]), or a place that is not its own (a seq other than its position, a
    /// previous CRC other than the payload CRC of the frame before it): a misplaced, stale or
    /// lost frame, damage wherever it lies.
    Corrupt,
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
        return Parsed::Invalid { zero_len: true };
    }
    let Some(payload) = usize::try_from(len).ok().and_then(|len| rest.get(..len)) else {
        return Parsed::Short;
    };
    parse_payload(HEADER_LEN, crc, payload)
}

/// Parses the frame of `format` at the start of `buf`, which runs from a frame boundary to (at
/// most) the end of the journal, and must sit at `place` (FORMAT 2 checks its header names it).
/// The journal reads a FORMAT 2 frame header first, from the disk; this parses one at hand, for
/// the codec's tests.
#[cfg(test)]
pub(crate) fn parse_frame_in(format: StoreFormat, buf: &[u8], place: Place) -> Parsed {
    if format == StoreFormat::V1 {
        return parse_frame(buf);
    }
    let header = match decode_header_in(format, buf) {
        Ok(Some(header)) => header,
        Ok(None) => return Parsed::Short,
        Err(BadHeader::Crc) => return Parsed::BadCrc,
        Err(BadHeader::Len) => return Parsed::Corrupt,
    };
    if !header.sits_at(place) {
        return Parsed::Corrupt;
    }
    let payload = usize::try_from(header.len)
        .ok()
        .and_then(|len| buf[HEADER_LEN_V2..].get(..len));
    match payload {
        Some(payload) => parse_payload(HEADER_LEN_V2, header.crc, payload),
        None => Parsed::Short,
    }
}

/// Parses `payload`, all of it there, behind a header of `header_len` bytes that gives its CRC
/// as `crc`.
pub(crate) fn parse_payload(header_len: usize, crc: u32, payload: &[u8]) -> Parsed {
    if crc32fast::hash(payload) != crc {
        return Parsed::BadCrc;
    }
    match decode_payload(payload) {
        Some(records) => Parsed::Whole {
            records,
            size: header_len + payload.len(),
            crc,
        },
        None => Parsed::Invalid { zero_len: false },
    }
}

/// The records of a frame payload: a count of at least 1, then exactly that many records.
/// A record that does not decode makes the frame [`Parsed::Invalid`], which the journal's
/// readers report as [`StoreError::JournalCorrupt`] at the frame's offset in `journal.log`: the
/// position in the payload where its record stopped decoding goes no further.
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

/// What a record's bytes that do not decode are: [`BadRecord`] at `at`, the position in them
/// where decoding stopped.
fn bad_record(at: usize) -> BadRecord {
    BadRecord { at }
}

/// The reads of a record's fields beyond fixed-width integers.
trait RecordReader {
    fn id(&mut self) -> Result<ChangesetId, BadRecord>;

    /// A `u8` flag (0 or 1), then the id if the flag is 1.
    fn optional_id(&mut self) -> Result<Option<ChangesetId>, BadRecord>;

    /// A `u8` length, then that many bytes of UTF-8.
    fn string(&mut self) -> Result<String, BadRecord>;
}

impl RecordReader for Reader<'_, BadRecord> {
    fn id(&mut self) -> Result<ChangesetId, BadRecord> {
        self.array()
            .map(|bytes| ChangesetId::new(ObjectId::new(bytes)))
    }

    fn optional_id(&mut self) -> Result<Option<ChangesetId>, BadRecord> {
        let flag_at = self.pos();
        match self.u8()? {
            0 => Ok(None),
            1 => self.id().map(Some),
            _ => Err(self.error_at(flag_at)),
        }
    }

    fn string(&mut self) -> Result<String, BadRecord> {
        let len = usize::from(self.u8()?);
        let start = self.pos();
        let bytes = self.bytes(len)?;
        let text = std::str::from_utf8(bytes).map_err(|_| self.error_at(start))?;
        Ok(text.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compacted(sorted_count: u64) -> Record {
        Record::Compacted { sorted_count }
    }

    /// The frame bound is the umbrella design's 539,615,192 bytes, and the longest record is
    /// a changeset with 255 parents.
    #[test]
    fn the_largest_payload_is_65535_of_the_longest_record() {
        assert_eq!(MAX_PAYLOAD_LEN, 539_615_192);
        let longest = Record::Changeset {
            id: ChangesetId::new(ObjectId::new([1; 32])),
            generation: 2,
            delta_hash: [3; 32],
            parents: vec![ChangesetId::new(ObjectId::new([4; 32])); 255],
        };
        let mut bytes = Vec::new();
        longest.encode(&mut bytes).unwrap();
        assert_eq!(bytes.len(), MAX_RECORD_LEN);
    }

    /// A FORMAT 2 frame round-trips with its place; FORMAT 1's bytes do not change.
    #[test]
    fn frame2_round_trips_with_its_place() {
        let records = [compacted(1), compacted(2)];
        let v1 = encode_frame(&records).unwrap();
        assert_eq!(
            encode_frame_in(StoreFormat::V1, &records, 9, 9).unwrap(),
            v1
        );
        let v2 = encode_frame_in(StoreFormat::V2, &records, 7, 0xdead_beef).unwrap();
        assert_eq!(v2[..8], v1[..8], "len and crc as in FORMAT 1");
        assert_eq!(v2[HEADER_LEN_V2..], v1[HEADER_LEN..], "the same payload");
        assert_eq!(v2.len(), HEADER_LEN_V2 + v1.len() - HEADER_LEN);

        let header = decode_header_in(StoreFormat::V2, &v2).unwrap().unwrap();
        assert_eq!(header.seq, Some(7));
        assert_eq!(header.prev_crc, Some(0xdead_beef));
        assert_eq!(header.crc, payload_crc(&v1));
        let place = Place {
            seq: 7,
            prev_crc: 0xdead_beef,
        };
        let Parsed::Whole {
            records: got,
            size,
            crc,
        } = parse_frame_in(StoreFormat::V2, &v2, place)
        else {
            panic!("a whole frame");
        };
        assert_eq!(
            (got.as_slice(), size, crc),
            (&records[..], v2.len(), header.crc)
        );

        // Anywhere else, it is damage, not a torn write.
        for place in [
            Place { seq: 8, ..place },
            Place {
                prev_crc: 0,
                ..place
            },
        ] {
            assert!(matches!(
                parse_frame_in(StoreFormat::V2, &v2, place),
                Parsed::Corrupt
            ));
        }

        // Short of its header or payload it is short; a flipped header byte fails its CRC.
        assert!(matches!(
            parse_frame_in(StoreFormat::V2, &v2[..23], place),
            Parsed::Short
        ));
        let cut = &v2[..v2.len() - 1];
        assert!(matches!(
            parse_frame_in(StoreFormat::V2, cut, place),
            Parsed::Short
        ));
        assert_eq!(decode_header_in(StoreFormat::V2, &v2[..23]), Ok(None));
        for at in 0..HEADER_LEN_V2 {
            let mut bad = v2.clone();
            bad[at] ^= 0x10;
            assert_eq!(
                decode_header_in(StoreFormat::V2, &bad),
                Err(BadHeader::Crc),
                "{at}"
            );
            assert!(matches!(
                parse_frame_in(StoreFormat::V2, &bad, place),
                Parsed::BadCrc
            ));
        }
    }

    /// A header that checks but announces 0 or more than the bound is refused from the header.
    #[test]
    fn a_checked_header_past_the_bound_is_refused() {
        for (len, bad) in [
            (0, true),
            (1, false),
            (MAX_PAYLOAD_LEN, false),
            (MAX_PAYLOAD_LEN + 1, true),
        ] {
            let mut header = encode_frame_in(StoreFormat::V2, &[compacted(0)], 0, 0).unwrap();
            header.truncate(HEADER_LEN_V2);
            header[..4].copy_from_slice(&u32::try_from(len).unwrap().to_le_bytes());
            set_place(&mut header, 0, 0);
            let decoded = decode_header_in(StoreFormat::V2, &header);
            assert_eq!(
                decoded.is_err_and(|e| e == BadHeader::Len),
                bad,
                "len {len}"
            );
            let parsed = parse_frame_in(
                StoreFormat::V2,
                &header,
                Place {
                    seq: 0,
                    prev_crc: 0,
                },
            );
            assert_eq!(matches!(parsed, Parsed::Corrupt), bad, "len {len}");
        }
    }
}
