//! FORMAT 2 helpers of the journal fuzz targets: the canonical frame encoding written from the
//! layout (the M6 design §2.3), not from the store's codec, and arbitrary records.
//!
//! ```text
//! frame2 = u32le len | u32le crc32(payload) | u64le seq | u32le prev_crc
//!          | u32le crc32(the 20 bytes before) | payload
//! ```

use arbitrary::Arbitrary;
use swarmops_core::{ChangesetId, ObjectId};
use swarmops_store::Record;

use crate::frame_bytes;

/// A FORMAT 2 frame header.
pub const HEADER_LEN_V2: usize = 24;

/// The size of one `journal2.off` entry.
pub const ENTRY_LEN_V2: usize = 16;

/// The bytes of a FORMAT 2 header that its own CRC covers.
const HCRC_AT: usize = 20;

/// `payload` behind a header at place (`seq`, `prev_crc`) whose CRCs match, whatever the payload
/// holds.
pub fn frame2_around(payload: &[u8], seq: u64, prev_crc: u32) -> Vec<u8> {
    let len = u32::try_from(payload.len()).expect("a payload under 4 GiB");
    let mut frame = Vec::with_capacity(HEADER_LEN_V2 + payload.len());
    frame.extend(len.to_le_bytes());
    frame.extend(crc32fast::hash(payload).to_le_bytes());
    frame.extend(seq.to_le_bytes());
    frame.extend(prev_crc.to_le_bytes());
    let hcrc = crc32fast::hash(&frame[..HCRC_AT]);
    frame.extend(hcrc.to_le_bytes());
    frame.extend(payload);
    frame
}

/// The canonical FORMAT 2 frame holding `records` at place (`seq`, `prev_crc`): the payload of
/// the FORMAT 1 frame, behind a FORMAT 2 header.
pub fn frame2_bytes(records: &[Record], seq: u64, prev_crc: u32) -> Vec<u8> {
    let v1 = frame_bytes(records);
    frame2_around(&v1[crate::HEADER_LEN..], seq, prev_crc)
}

/// The payload CRC a frame's header (either format) holds: its bytes 4 to 8.
pub fn payload_crc(frame: &[u8]) -> u32 {
    u32::from_le_bytes([frame[4], frame[5], frame[6], frame[7]])
}

/// A journal record, before its lengths are clamped to what the layout holds.
#[derive(Arbitrary, Debug)]
pub enum ArbRecord {
    /// A changeset joined the index.
    Changeset {
        /// Its id.
        id: [u8; 32],
        /// Its generation.
        generation: u64,
        /// Its delta hash.
        delta_hash: [u8; 32],
        /// Its parents, cut to 255.
        parents: Vec<[u8; 32]>,
    },
    /// A ref moved.
    RefMove {
        /// The name, cut to 255 bytes.
        name: String,
        /// The target before.
        old: Option<[u8; 32]>,
        /// The target after.
        new: Option<[u8; 32]>,
        /// The actor, cut to 255 bytes.
        actor: String,
        /// The time.
        ts: i64,
    },
    /// A compaction finished.
    Compacted {
        /// The sorted count.
        sorted_count: u64,
    },
}

impl ArbRecord {
    /// The record, with at most 255 parents, and a ref name and an actor of at most 255 bytes.
    pub fn into_record(self) -> Record {
        match self {
            Self::Changeset {
                id,
                generation,
                delta_hash,
                parents,
            } => Record::Changeset {
                id: changeset_id(id),
                generation,
                delta_hash,
                parents: parents.into_iter().take(255).map(changeset_id).collect(),
            },
            Self::RefMove {
                name,
                old,
                new,
                actor,
                ts,
            } => Record::RefMove {
                name: at_most_255_bytes(name),
                old: old.map(changeset_id),
                new: new.map(changeset_id),
                actor: at_most_255_bytes(actor),
                ts,
            },
            Self::Compacted { sorted_count } => Record::Compacted { sorted_count },
        }
    }
}

fn changeset_id(bytes: [u8; 32]) -> ChangesetId {
    ChangesetId::new(ObjectId::new(bytes))
}

/// `s` cut to at most 255 bytes, at a character boundary.
fn at_most_255_bytes(mut s: String) -> String {
    let mut len = s.len().min(255);
    while !s.is_char_boundary(len) {
        len -= 1;
    }
    s.truncate(len);
    s
}
