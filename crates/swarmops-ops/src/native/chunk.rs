//! The chunks that carry objects over the native wire (native sync design §2.2): a header, then
//! records, each an object's kind, id and payload. A reader checks every count against the
//! header and the announcement before it reads the bytes counted, and every record's id against
//! the hash of its kind and payload before it returns it (V1).
//!
//! ```text
//! chunk  = "SWCH" | u8 ver = 1 | 0 × 3 | u32le count | u64le bytes | record × count
//! record = u8 kind | id[32] | u32le len | payload[len]
//!          kind: the object file's tag, 0 blob or 2 changeset; a tree (1) is refused
//! ```
//!
//! A chunk's `bytes` count its records, at most [`MAX_CHUNK_BYTES`]; one record alone may be
//! longer, its payload up to [`MAX_OBJECT_LEN`]. No checksum: each record's id is its check.

use std::io::{self, Read, Write};

use serde_json::Value;
use swarmops_api::{MAX_CHUNK_BYTES, OpError, WireChunks};
use swarmops_core::{Hashed, ObjectId, ObjectKind, bytes};
use swarmops_store::MAX_OBJECT_LEN;

/// What every chunk starts with.
const MAGIC: &[u8; 4] = b"SWCH";

/// The layout's version.
const VERSION: u8 = 1;

/// A chunk header's length: magic, version, three zero bytes, count, bytes.
pub(crate) const HEADER_LEN: u64 = 20;

/// A record header's length: kind, id, payload length.
const RECORD_HEADER_LEN: u64 = 37;

/// The most bytes of a payload read ahead of its arrival: a header that lies about a length
/// costs no more memory than the bytes that come.
const READ_AHEAD: u64 = 64 * 1024;

/// The object kinds a reader takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Takes {
    /// Blobs and changesets: what a fetch gets and a put sends; a tree is refused.
    BlobsAndChangesets,
}

impl Takes {
    /// Whether a record of `kind` is taken.
    fn takes(self, kind: ObjectKind) -> bool {
        match self {
            Takes::BlobsAndChangesets => matches!(kind, ObjectKind::Blob | ObjectKind::Changeset),
        }
    }
}

/// Writes one chunk of `records` to `out`. The caller keeps the chunk within its bound.
#[allow(
    dead_code,
    reason = "the native fetch (N-3) and push (N-4) write chunks"
)]
pub(crate) fn write_chunk(
    out: &mut impl Write,
    records: &[(ObjectKind, ObjectId, &[u8])],
) -> io::Result<()> {
    // 1. The header: how many records, and their bytes.
    let too_long = || io::Error::new(io::ErrorKind::InvalidInput, "a record past the layout");
    let count = u32::try_from(records.len()).map_err(|_| too_long())?;
    let len: u64 = (records.iter())
        .map(|(_, _, payload)| RECORD_HEADER_LEN + bytes::to_u64(payload.len()))
        .sum();
    out.write_all(MAGIC)?;
    out.write_all(&[VERSION, 0, 0, 0])?;
    out.write_all(&count.to_le_bytes())?;
    out.write_all(&len.to_le_bytes())?;

    // 2. Each record.
    for (kind, id, payload) in records {
        let payload_len = u32::try_from(payload.len()).map_err(|_| too_long())?;
        out.write_all(&[kind.tag()])?;
        out.write_all(&id.0)?;
        out.write_all(&payload_len.to_le_bytes())?;
        out.write_all(payload)?;
    }
    Ok(())
}

/// A chunk's length, header included, once read into `records`.
pub(crate) fn chunk_len(records: &[Hashed]) -> u64 {
    let records: u64 = (records.iter())
        .map(|hashed| RECORD_HEADER_LEN + bytes::to_u64(hashed.payload().len()))
        .sum();
    HEADER_LEN + records
}

/// Reads one chunk from `input`: `expect` is what is left of the chunks the line announced,
/// and the last one ends exactly where the announcement does. Every record is of a kind `take`
/// takes and hashes to its id; anything else returns no object of the chunk.
pub(crate) fn read_chunk(
    input: &mut impl Read,
    expect: WireChunks,
    take: Takes,
) -> Result<Vec<Hashed>, OpError> {
    if expect.chunks == 0 {
        return Err(super::refused("chunk_count", "no chunk is left to read"));
    }

    // 1. The header, checked against the bounds and the announcement before any record.
    let header: [u8; 20] = read_array(input)?;
    let (count, len) = header_of(&header)?;
    let total = HEADER_LEN.saturating_add(len);
    let past = total > expect.bytes || (expect.chunks == 1 && total != expect.bytes);
    if past {
        let message = format!("a chunk of {total} bytes, of {} announced", expect.bytes);
        return Err(super::refused("chunk_bytes", message));
    }

    // 2. Each record: its header within the chunk's bytes, its kind taken, its id its hash.
    let mut left = len;
    let mut records = Vec::new();
    for _ in 0..count {
        let record: [u8; 37] = read_array(input)?;
        let (kind, id, payload_len) = record_of(&record, take)?;
        let record_len = RECORD_HEADER_LEN + payload_len;
        if record_len > left {
            let message = format!("a record of {record_len} bytes, of {left} left in its chunk");
            return Err(super::refused("chunk_bytes", message));
        }
        left -= record_len;
        let hashed = Hashed::new(kind, read_payload(input, payload_len)?);
        if hashed.id() != id {
            let message = format!("the bytes sent as {id} hash to {}", hashed.id());
            let refused = super::refused("object_id", message);
            let mut details = refused.details.clone();
            details.insert("object".to_owned(), Value::from(id.to_hex()));
            return Err(refused.with_details(details));
        }
        records.push(hashed);
    }

    // 3. The records fill the chunk exactly.
    if left != 0 {
        let message = format!("{left} bytes of a chunk outside its records");
        return Err(super::refused("chunk_bytes", message));
    }
    Ok(records)
}

/// A chunk header's count and bytes: its layout, and its bytes within the bounds and able to
/// hold its count of records.
fn header_of(header: &[u8; 20]) -> Result<(u32, u64), OpError> {
    // 1. The layout.
    let bad = |message: &str| super::refused("chunk_header", message);
    if &header[..4] != MAGIC || header[4] != VERSION || header[5..8] != [0, 0, 0] {
        return Err(bad("a chunk header of another layout"));
    }
    let count = u32::from_le_bytes([header[8], header[9], header[10], header[11]]);
    let mut len = [0; 8];
    len.copy_from_slice(&header[12..]);
    let len = u64::from_le_bytes(len);

    // 2. The bounds: many records within a chunk's, one alone within an object's.
    let bound = match count {
        1 => MAX_OBJECT_LEN + RECORD_HEADER_LEN,
        _ => MAX_CHUNK_BYTES,
    };
    if len > bound {
        let message = format!("a chunk of {len} bytes in {count} records, past {bound}");
        return Err(super::refused("chunk_too_large", message));
    }

    // 3. The count: at least one record, each at least a record header long.
    if count == 0 || u64::from(count) * RECORD_HEADER_LEN > len {
        return Err(bad("a chunk's count of records does not fit its bytes"));
    }
    Ok((count, len))
}

/// A record header's kind, id and payload length; a kind `take` does not take is refused.
fn record_of(record: &[u8; 37], take: Takes) -> Result<(ObjectKind, ObjectId, u64), OpError> {
    let kind = ObjectKind::from_tag(record[0]).filter(|kind| take.takes(*kind));
    let Some(kind) = kind else {
        let message = format!("a record of kind {} is not sent over the wire", record[0]);
        return Err(super::refused("object_kind", message));
    };
    let mut id = [0; 32];
    id.copy_from_slice(&record[1..33]);
    let payload_len = u32::from_le_bytes([record[33], record[34], record[35], record[36]]);
    Ok((kind, ObjectId::new(id), u64::from(payload_len)))
}

/// `N` bytes of `input`; a stream that ends first is the peer gone.
fn read_array<const N: usize>(input: &mut impl Read) -> Result<[u8; N], OpError> {
    let mut out = [0; N];
    input.read_exact(&mut out).map_err(|e| lost(&e))?;
    Ok(out)
}

/// A payload of `len` bytes, the buffer growing as they arrive.
fn read_payload(input: &mut impl Read, len: u64) -> Result<Vec<u8>, OpError> {
    let ahead = usize::try_from(len.min(READ_AHEAD)).unwrap_or(0);
    let mut payload = Vec::with_capacity(ahead);
    let read = input.take(len).read_to_end(&mut payload);
    read.map_err(|e| lost(&e))?;
    if bytes::to_u64(payload.len()) != len {
        return Err(super::unreachable(super::Cause::Disconnected));
    }
    Ok(payload)
}

/// The refusal of a read that failed: the peer is gone or silent.
fn lost(failed: &io::Error) -> OpError {
    super::unreachable(super::Cause::of(failed))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use swarmops_api::ErrorCode;
    use swarmops_core::hash_object;

    use super::*;

    /// A blob and a changeset payload, with their ids.
    fn objects() -> Vec<(ObjectKind, ObjectId, Vec<u8>)> {
        [
            (ObjectKind::Blob, b"hello\n".to_vec()),
            (ObjectKind::Changeset, vec![7; 300]),
        ]
        .into_iter()
        .map(|(kind, payload)| (kind, hash_object(kind, &payload), payload))
        .collect()
    }

    /// One chunk of `objects`, written.
    fn written(objects: &[(ObjectKind, ObjectId, Vec<u8>)]) -> Vec<u8> {
        let records: Vec<_> = (objects.iter())
            .map(|(kind, id, payload)| (*kind, *id, payload.as_slice()))
            .collect();
        let mut out = Vec::new();
        write_chunk(&mut out, &records).unwrap();
        out
    }

    /// The announcement of `chunk` alone.
    fn alone(chunk: &[u8]) -> WireChunks {
        WireChunks::new(1, bytes::to_u64(chunk.len()))
    }

    /// The refusal's `details.reason`.
    fn reason(refused: &OpError) -> &str {
        refused.details["reason"].as_str().unwrap()
    }

    /// What is written reads back, each object with its id, and its length is the chunk's.
    #[test]
    fn a_chunk_round_trips() {
        let objects = objects();
        let chunk = written(&objects);

        let read = read_chunk(
            &mut Cursor::new(&chunk),
            alone(&chunk),
            Takes::BlobsAndChangesets,
        );
        let read = read.unwrap();
        let got: Vec<_> = (read.iter())
            .map(|h| (h.kind(), h.id(), h.payload().to_vec()))
            .collect();
        assert_eq!(got, objects);
        assert_eq!(chunk_len(&read), bytes::to_u64(chunk.len()));
    }

    /// A record whose payload is not what its id names is refused, and no object of the chunk
    /// comes back, the good one before it neither.
    #[test]
    fn a_record_whose_bytes_miss_its_id_is_refused() {
        let mut objects = objects();
        objects[1].2[0] ^= 1;
        let chunk = written(&objects);

        let refused = read_chunk(
            &mut Cursor::new(&chunk),
            alone(&chunk),
            Takes::BlobsAndChangesets,
        );
        let refused = refused.unwrap_err();
        assert_eq!(refused.code, ErrorCode::InvalidArg.as_str());
        assert_eq!(reason(&refused), "object_id");
        assert_eq!(refused.details["object"], objects[1].1.to_hex());
    }

    /// A tree is never sent: its record is refused even with a right id.
    #[test]
    fn a_tree_record_is_refused() {
        let payload = b"a tree's bytes".to_vec();
        let tree = (
            ObjectKind::Tree,
            hash_object(ObjectKind::Tree, &payload),
            payload,
        );
        let chunk = written(&[tree]);

        let refused = read_chunk(
            &mut Cursor::new(&chunk),
            alone(&chunk),
            Takes::BlobsAndChangesets,
        );
        let refused = refused.unwrap_err();
        assert_eq!(refused.code, ErrorCode::InvalidArg.as_str());
        assert_eq!(reason(&refused), "object_kind");
    }

    /// A header past the chunk bound, past the store's object bound, or past what the line
    /// announced is refused before any record is read: the input holds the header alone, so a
    /// reader that read on would answer `disconnected` instead.
    #[test]
    fn a_chunk_past_its_bound_is_refused_before_reading() {
        let header = |count: u32, len: u64| {
            let mut out = MAGIC.to_vec();
            out.extend_from_slice(&[VERSION, 0, 0, 0]);
            out.extend_from_slice(&count.to_le_bytes());
            out.extend_from_slice(&len.to_le_bytes());
            out
        };
        let one_object = MAX_OBJECT_LEN + RECORD_HEADER_LEN;
        let cases = [
            (header(2, MAX_CHUNK_BYTES + 1), u64::MAX, "chunk_too_large"),
            (header(1, one_object + 1), u64::MAX, "chunk_too_large"),
            (header(1, 100), HEADER_LEN + 99, "chunk_bytes"),
            (header(3, 100), HEADER_LEN + 100, "chunk_header"),
        ];
        for (chunk, announced, want) in cases {
            let expect = WireChunks::new(1, announced);
            let refused = read_chunk(&mut Cursor::new(&chunk), expect, Takes::BlobsAndChangesets);
            let refused = refused.unwrap_err();
            assert_eq!(refused.code, ErrorCode::InvalidArg.as_str(), "{refused:?}");
            assert_eq!(reason(&refused), want, "{refused:?}");
        }

        // A record longer than the chunk's bytes, before its payload is read.
        let objects = objects();
        let mut chunk = written(&objects[..1]);
        let len_at = usize::try_from(HEADER_LEN + 33).unwrap();
        chunk[len_at..len_at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        let refused = read_chunk(
            &mut Cursor::new(&chunk),
            alone(&chunk),
            Takes::BlobsAndChangesets,
        );
        assert_eq!(reason(&refused.unwrap_err()), "chunk_bytes");
    }

    /// A chunk cut anywhere is the peer gone: `E_REMOTE_UNREACHABLE`, `disconnected`,
    /// retryable, and no object of it.
    #[test]
    fn a_short_chunk_is_disconnected_and_retryable() {
        let chunk = written(&objects());
        for cut in [0, 5, 19, 20, 40, chunk.len() - 1] {
            let short = &chunk[..cut];
            let refused = read_chunk(
                &mut Cursor::new(short),
                alone(&chunk),
                Takes::BlobsAndChangesets,
            );
            let refused = refused.unwrap_err();
            assert_eq!(
                refused.code,
                ErrorCode::RemoteUnreachable.as_str(),
                "cut {cut}"
            );
            assert!(refused.retryable, "cut {cut}");
            assert_eq!(
                refused.details["cause"],
                Value::from("disconnected"),
                "cut {cut}"
            );
        }
    }

    /// A header of another layout, and a read with no chunk left to read, are refused.
    #[test]
    fn a_chunk_of_another_layout_is_refused() {
        let chunk = written(&objects());
        for at in [0, 4, 5] {
            let mut other = chunk.clone();
            other[at] ^= 1;
            let refused = read_chunk(
                &mut Cursor::new(&other),
                alone(&chunk),
                Takes::BlobsAndChangesets,
            );
            assert_eq!(reason(&refused.unwrap_err()), "chunk_header", "byte {at}");
        }

        let none = WireChunks::new(0, 0);
        let refused = read_chunk(&mut Cursor::new(&chunk), none, Takes::BlobsAndChangesets);
        assert_eq!(reason(&refused.unwrap_err()), "chunk_count");
    }
}
