//! Helpers shared by the journal fuzz targets (`v2` holds the FORMAT 2 ones): the canonical frame encoding, and a model of how
//! the bytes at a frame boundary read. The model's frame layer (length, CRC, record count) is
//! written from the layout (storage spec §3), not from the store's parser. Its records are
//! decoded by the store's own `Record::decode`, which the targets check by re-encoding what it
//! decodes and by reading back what the store wrote, not by comparing it with the model.
//!
//! ```text
//! frame   = u32le len | u32le crc32(payload) | payload      len = payload bytes, never 0
//! payload = u16le record count (1 to 65,535) | records
//! ```

use std::io;

pub mod v2;

use swarmops_store::{Record, StoreError};

/// A frame header: `u32le len | u32le crc32(payload)`.
pub const HEADER_LEN: usize = 8;

/// The size of one `journal.off` entry: a frame's start offset, `u64le`.
pub const ENTRY_LEN: usize = 8;

/// The most zeroed entries at the end of `journal.off`, past frame 0, that readers and recovery
/// drop (storage spec §2 rule 3): a 4 KiB block of them. A power loss leaves zeroed only the
/// entry being written, so more is damage, `JournalCorrupt` at 0 (the offset such an entry
/// holds), for every reader and for recovery.
pub const MAX_ZEROED: usize = 512;

/// The canonical frame holding `records`.
pub fn frame_bytes(records: &[Record]) -> Vec<u8> {
    let count = u16::try_from(records.len()).expect("a frame holds at most 65,535 records");
    let mut payload = count.to_le_bytes().to_vec();
    for record in records {
        record
            .encode(&mut payload)
            .expect("a record within the layout's limits encodes");
    }
    frame_around(&payload)
}

/// `payload` behind a header holding its length and CRC: a frame whose CRC matches, whatever
/// the payload holds.
pub fn frame_around(payload: &[u8]) -> Vec<u8> {
    let len = u32::try_from(payload.len()).expect("a payload under 4 GiB");
    let mut frame = Vec::with_capacity(HEADER_LEN + payload.len());
    frame.extend(len.to_le_bytes());
    frame.extend(crc32fast::hash(payload).to_le_bytes());
    frame.extend(payload);
    frame
}

/// How the bytes at a frame boundary read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Model {
    /// A whole frame: it matches its CRC and its payload decodes.
    Whole {
        /// Its records, in order.
        records: Vec<Record>,
        /// Its size in bytes, header included.
        size: usize,
    },
    /// The bytes end before the frame does: its header, or the payload the header announces,
    /// is cut short.
    Short,
    /// The frame is all there, but its payload does not match its CRC.
    BadCrc,
    /// A header announcing an empty payload (a length of 0), or a payload that matches its
    /// CRC but does not decode. Listed, either is damage; unlisted, the second is a frame a
    /// writer wrote, and the first a torn tail (see [`zero_length`]).
    Invalid,
}

/// How the frame at the start of `buf` reads; `buf` runs from a frame boundary to the end of
/// `journal.log`.
pub fn model(buf: &[u8]) -> Model {
    let Some((header, rest)) = buf.split_first_chunk::<HEADER_LEN>() else {
        return Model::Short;
    };
    let len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
    let crc = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
    if len == 0 {
        return Model::Invalid;
    }
    let len = usize::try_from(len).expect("u32 fits in usize");
    let Some(payload) = rest.get(..len) else {
        return Model::Short;
    };
    if crc32fast::hash(payload) != crc {
        return Model::BadCrc;
    }
    match records_of(payload) {
        Some(records) => Model::Whole {
            records,
            size: HEADER_LEN + len,
        },
        None => Model::Invalid,
    }
}

/// A payload's records: a count of at least 1, then exactly that many records, nothing after.
fn records_of(payload: &[u8]) -> Option<Vec<Record>> {
    let (count, mut rest) = payload.split_first_chunk::<2>()?;
    let count = u16::from_le_bytes(*count);
    if count == 0 {
        return None;
    }
    let mut records = Vec::new();
    for _ in 0..count {
        let (record, used) = Record::decode(rest).ok()?;
        records.push(record);
        rest = rest
            .get(used..)
            .expect("Record::decode used no more bytes than it had");
    }
    rest.is_empty().then_some(records)
}

/// Whether `rest`, the unlisted bytes from a frame boundary to the end of `journal.log`, start
/// with a frame header whose length reads 0. No writer writes a frame of length 0, and an
/// append's sync makes its header durable, so such a header never reached the disk (zeros a
/// filesystem exposed after a crash, or the first sector of an append that a power loss lost
/// while it kept a later one): a torn tail, whatever follows it.
pub fn zero_length(rest: &[u8]) -> bool {
    rest.starts_with(&[0; 4])
}

/// Whether `e` is an error the journal reports for damage: `JournalCorrupt`, or `Io` with
/// `InvalidData`, `UnexpectedEof` or `InvalidInput`.
pub fn is_typed(e: &StoreError) -> bool {
    match e {
        StoreError::JournalCorrupt { .. } => true,
        StoreError::Io(e) => matches!(
            e.kind(),
            io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof | io::ErrorKind::InvalidInput
        ),
        _ => false,
    }
}

/// `n` as a `u64`.
pub fn to_u64(n: usize) -> u64 {
    u64::try_from(n).expect("usize fits in u64")
}
