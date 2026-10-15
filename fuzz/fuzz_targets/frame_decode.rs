//! Journal record and frame decoding never panics: whatever decodes re-encodes to exactly the
//! bytes it was decoded from, and every error is `StoreError::JournalCorrupt` where the frame
//! layout says (storage spec §3).
//!
//! The input is read three ways:
//! 1. as records back to back, through `Record::decode`;
//! 2. as `journal.log` itself, nothing listed in `journal.off`: `tail_frames_from(0)` returns the
//!    whole frames from offset 0 up to a torn tail (a frame cut short, one failing its CRC, or
//!    zeros to the end of the file), or `JournalCorrupt` at the first frame no writer produces
//!    (an empty payload, or one that matches its CRC but does not decode);
//! 3. as one frame's payload, behind a header with its length and CRC so that the CRC never
//!    stands in the way: the frame reads, unlisted and listed, exactly when the payload is a
//!    record count of at least 1 followed by exactly that many records. An empty payload makes
//!    the frame 8 zero bytes: a torn tail unlisted, `JournalCorrupt` listed.
//!
//! The journal files live in a fresh `tempfile::tempdir()` per input.
#![no_main]

use std::fs;
use std::sync::Arc;

use libfuzzer_sys::fuzz_target;
use swarmops_fuzz::{Model, frame_around, frame_bytes, is_typed, model, to_u64, zero_filled};
use swarmops_store::{Frame, Journal, Layout, RealFs, Record, StoreError};

fuzz_target!(|data: &[u8]| {
    records_back_to_back(data);

    let dir = tempfile::tempdir().expect("create a temp dir");
    let root = dir.path().to_str().expect("the temp dir path is UTF-8");
    let layout = Layout::new(root.to_owned());
    fs::create_dir(layout.meta_dir()).expect("create meta/");
    fs::write(layout.journal_off(), []).expect("write journal.off");
    fs::write(layout.journal_log(), data).expect("write journal.log");
    // Readers measure the files' lengths through FsOps: the real ones.
    let journal = Journal::open(&layout, Arc::new(RealFs)).expect("open the journal");
    agrees(
        "the input as journal.log",
        &journal.tail_frames_from(0),
        &unlisted_frames(data),
        data,
    );

    // `fs::write` keeps the inode, so the open handle reads the new contents.
    let framed = frame_around(data);
    fs::write(layout.journal_log(), &framed).expect("rewrite journal.log");
    let listed = match model(&framed) {
        Model::Whole { records, .. } => Ok(vec![Frame {
            seq: 0,
            offset: 0,
            records,
        }]),
        Model::Invalid => Err(0),
        other => panic!("a frame with a matching CRC reads as {other:?}"),
    };
    agrees(
        "the input as an unlisted frame's payload",
        &journal.tail_frames_from(0),
        &unlisted_frames(&framed),
        &framed,
    );
    fs::write(layout.journal_off(), 0_u64.to_le_bytes()).expect("list the frame");
    agrees(
        "the input as a listed frame's payload",
        &journal.frames_from(0, 1),
        &listed,
        &framed,
    );
});

/// Decodes `data` as records back to back until one fails to decode. Each record re-encodes to
/// exactly the bytes it was decoded from, and those bytes cut short do not decode: a record
/// ends where its own lengths say.
fn records_back_to_back(data: &[u8]) {
    let mut rest = data;
    // Each pass consumes at least one byte or ends the loop.
    while !rest.is_empty() {
        match Record::decode(rest) {
            Ok((record, used)) => {
                assert!(
                    (1..=rest.len()).contains(&used),
                    "Record::decode used {used} of {} bytes",
                    rest.len()
                );
                let mut bytes = Vec::new();
                record.encode(&mut bytes);
                assert_eq!(
                    bytes,
                    rest[..used],
                    "{record:?} does not re-encode to the bytes it was decoded from"
                );
                let short = Record::decode(&rest[..used - 1]);
                assert!(
                    matches!(short, Err(StoreError::JournalCorrupt { .. })),
                    "{record:?} cut short by a byte decodes: {short:?}"
                );
                rest = &rest[used..];
            }
            Err(StoreError::JournalCorrupt { offset }) => {
                assert!(
                    offset <= to_u64(rest.len()),
                    "JournalCorrupt at {offset}, past the {} bytes decoded",
                    rest.len()
                );
                return;
            }
            Err(e) => panic!("Record::decode failed with {e:?}, not JournalCorrupt"),
        }
    }
}

/// What a reader must return for `log` when `journal.off` lists nothing: the whole frames from
/// offset 0, numbered from 0, up to a torn tail (a frame cut short, one failing its CRC, or
/// zeros to the end); or `Err(offset)` for `JournalCorrupt` at the first frame that no writer
/// produces.
fn unlisted_frames(log: &[u8]) -> Result<Vec<Frame>, u64> {
    let mut frames = Vec::new();
    let mut at = 0;
    // Each pass moves `at` forward by a whole frame (at least 11 bytes) or ends the loop.
    while at < log.len() {
        match model(&log[at..]) {
            Model::Whole { records, size } => {
                frames.push(Frame {
                    seq: to_u64(frames.len()),
                    offset: to_u64(at),
                    records,
                });
                at += size;
            }
            Model::Short | Model::BadCrc => break,
            Model::Invalid if zero_filled(&log[at..]) => break,
            Model::Invalid => return Err(to_u64(at)),
        }
    }
    Ok(frames)
}

/// Asserts that a reader returned what the model says, and that every frame it returned is
/// the canonical encoding of its records at its offset in `log`.
fn agrees(
    what: &str,
    got: &Result<Vec<Frame>, StoreError>,
    want: &Result<Vec<Frame>, u64>,
    log: &[u8],
) {
    match (got, want) {
        (Ok(frames), Ok(expected)) if frames == expected => {
            for frame in frames {
                let bytes = frame_bytes(&frame.records);
                let at = usize::try_from(frame.offset).expect("an offset inside the input");
                assert_eq!(
                    log.get(at..at + bytes.len()),
                    Some(&bytes[..]),
                    "{what}: frame {} does not re-encode to the bytes it was read from",
                    frame.seq
                );
            }
        }
        (Err(StoreError::JournalCorrupt { offset }), Err(at)) if offset == at => {}
        _ => {
            if let Err(e) = got {
                assert!(is_typed(e), "{what}: an untyped error: {e:?}");
            }
            panic!("{what}: the reader returned {got:?}, the model says {want:?}");
        }
    }
}
