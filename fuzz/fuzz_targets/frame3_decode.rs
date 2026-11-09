//! FORMAT 3 frame payloads never panic when decoded, and whatever decodes is canonical (the
//! FORMAT 3 design §2.7, §2.8, §6): a payload past the base seq that reads as a record count, the
//! records of its kinds (the fact kinds among them) and the nonce record last re-encodes to
//! exactly its bytes, and a frame of those records and that nonce has the payload's CRC.
//!
//! The input is read as such a payload, and its records also one by one before the base, where
//! FORMAT 2's kinds alone decode.
#![no_main]

use libfuzzer_sys::fuzz_target;
use swarmops_store::{Base, FactKind, Frame, Record, StoreFormat};

/// The nonce record's kind and length.
const NONCE_KIND: u8 = 11;
const NONCE_LEN: usize = 9;

fuzz_target!(|data: &[u8]| {
    // 1. The input as a payload past the base: re-encoded when it reads.
    if let Some((records, nonce)) = payload(data) {
        let mut bytes = u16::try_from(records.len() + 1)
            .expect("a count of at most 65,535")
            .to_le_bytes()
            .to_vec();
        for record in &records {
            record.encode(&mut bytes).expect("a decoded record encodes");
        }
        bytes.push(NONCE_KIND);
        bytes.extend_from_slice(&nonce.to_le_bytes());
        assert_eq!(bytes, data, "a decoded payload re-encodes to its bytes");
        let frame = Frame::new_in(StoreFormat::V3, 0, 0, records, Some(nonce));
        assert_eq!(
            frame.crc,
            crc32fast::hash(data),
            "the frame's CRC is its payload's"
        );
    }

    // 2. Its records before the base: whatever decodes is FORMAT 2's.
    let mut rest = data;
    while let Ok((record, used)) =
        Record::decode_in(StoreFormat::V3, Base::Before, &FactKind::ALL, rest)
    {
        assert!(
            !matches!(record, Record::Fact(_)),
            "no fact before the base"
        );
        assert!(used > 0 && used <= rest.len());
        rest = &rest[used..];
    }
});

/// The records and nonce of `data` as a FORMAT 3 payload past the base, if it is one: a count
/// of at least 2, that many records less one, then exactly the nonce record.
fn payload(data: &[u8]) -> Option<(Vec<Record>, u64)> {
    let (count, mut rest) = data.split_first_chunk::<2>()?;
    let count = usize::from(u16::from_le_bytes(*count));
    let mut records = Vec::new();
    for _ in 1..count {
        let (record, used) =
            Record::decode_in(StoreFormat::V3, Base::After, &FactKind::ALL, rest).ok()?;
        records.push(record);
        rest = rest.get(used..)?;
    }
    let (kind, nonce) = rest.split_first()?;
    let nonce: [u8; 8] = nonce.try_into().ok()?;
    (!records.is_empty() && *kind == NONCE_KIND && rest.len() == NONCE_LEN)
        .then(|| (records, u64::from_le_bytes(nonce)))
}
