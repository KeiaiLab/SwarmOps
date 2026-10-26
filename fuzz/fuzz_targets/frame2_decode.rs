//! FORMAT 2 journal frames never panic when read, and whatever reads is canonical: the frames a
//! reader returns re-encode, with the place their headers name, to exactly the bytes they were
//! read from; every error is a typed one (the M6 design §2.3, storage spec §3).
//!
//! The input is read two ways, in a fresh `tempfile::tempdir()` holding a FORMAT 2 store's
//! journal files with nothing listed in `journal2.off`:
//! 1. as `journal2.log` itself: `tail_frames_from(0)` returns frames numbered from 0 that abut,
//!    each the canonical encoding of its records behind a header naming its seq and the payload
//!    CRC of the frame before it, or a typed error;
//! 2. as one frame's payload, behind a header at place (0, 0) whose CRCs match, so that they
//!    never stand in the way: the frame reads exactly when the payload is a record count of at
//!    least 1 followed by exactly that many records, and is `JournalCorrupt` at 0 otherwise (an
//!    empty payload included: a length of 0 is no frame a writer writes);
//! 3. as the payload of a second frame, behind a first one, at the place it should have (it
//!    reads as the model says) and at places it should not: a wrong seq, or the payload CRC of
//!    another frame in `prev_crc`. A header that checks and names another place is
//!    `JournalCorrupt` at its offset, whatever the payload holds.
#![no_main]

use std::fs;
use std::sync::Arc;

use libfuzzer_sys::fuzz_target;
use swarmops_fuzz::v2::{frame2_around, frame2_bytes, payload_crc};
use swarmops_fuzz::{Model, frame_around, is_typed, model, to_u64};
use swarmops_store::{Frame, Journal, Layout, RealFs, Record, StoreError, StoreFormat};

fuzz_target!(|data: &[u8]| {
    let dir = tempfile::tempdir().expect("create a temp dir");
    let root = dir.path().to_str().expect("the temp dir path is UTF-8");
    let layout = Layout::with_format(root.to_owned(), StoreFormat::V2);
    fs::create_dir(layout.meta_dir()).expect("create meta/");
    fs::write(layout.journal_off(), []).expect("write journal2.off");
    fs::write(layout.journal_log(), data).expect("write journal2.log");
    let journal = Journal::open(&layout, Arc::new(RealFs::new())).expect("open the journal");

    // 1. The input as the log: what reads is canonical.
    match journal.tail_frames_from(0) {
        Ok(frames) => canonical(&frames, data),
        Err(e) => assert!(
            is_typed(&e),
            "the input as journal2.log: untyped error {e:?}"
        ),
    }

    // 2. The input as a payload behind a header that checks: the model decides.
    // `fs::write` keeps the inode, so the open handle reads the new contents.
    let framed = frame2_around(data, 0, 0);
    fs::write(layout.journal_log(), &framed).expect("rewrite journal2.log");
    let got = journal.tail_frames_from(0);
    match model(&frame_around(data)) {
        Model::Whole { records, .. } => {
            let want = vec![Frame::new(0, 0, records)];
            assert_eq!(
                got.as_ref().ok(),
                Some(&want),
                "a whole frame reads: {got:?}"
            );
            canonical(&want, &framed);
        }
        _ => assert!(
            matches!(got, Err(StoreError::JournalCorrupt { offset: 0 })),
            "a frame no writer writes reads as {got:?}"
        ),
    }

    // 3. Behind a frame, in its place and out of it.
    behind_a_frame(&journal, &layout, data);
});

/// The payload of the first frame of case 3.
const FIRST: Record = Record::Compacted { sorted_count: 1 };

/// Case 3: the input as the payload of the frame at offset `first.len()`.
fn behind_a_frame(journal: &Journal, layout: &Layout, data: &[u8]) {
    let first = frame2_bytes(std::slice::from_ref(&FIRST), 0, 0);
    let first_crc = payload_crc(&first);
    let at = to_u64(first.len());
    let mut places = vec![(1, first_crc, true)];
    places.extend([
        (1, first_crc ^ 1, false),
        (0, first_crc, false),
        (2, first_crc, false),
    ]);
    for (seq, prev_crc, right) in places {
        let log = [first.clone(), frame2_around(data, seq, prev_crc)].concat();
        fs::write(layout.journal_log(), &log).expect("rewrite journal2.log");
        let got = journal.tail_frames_from(0);
        let whole = match model(&frame_around(data)) {
            Model::Whole { records, .. } => Some(records),
            _ => None,
        };
        match (right, whole) {
            (true, Some(records)) => {
                let want = vec![Frame::new(0, 0, vec![FIRST]), Frame::new(1, at, records)];
                assert_eq!(got.ok(), Some(want), "a frame in its place reads");
            }
            _ => assert!(
                matches!(got, Err(StoreError::JournalCorrupt { offset }) if offset == at),
                "a frame ({seq}, {prev_crc}) that is no frame in its place reads as {got:?}"
            ),
        }
    }
}

/// Asserts that `frames`, read from `log` from offset 0, are numbered from 0, abut, and are each
/// the canonical encoding of their records at the place the frames before them make.
fn canonical(frames: &[Frame], log: &[u8]) {
    let (mut at, mut prev_crc) = (0, 0);
    for (seq, frame) in frames.iter().enumerate() {
        assert_eq!(frame.seq, to_u64(seq), "frames are numbered from 0");
        assert_eq!(frame.offset, to_u64(at), "frame {seq} does not abut");
        let bytes = frame2_bytes(&frame.records, frame.seq, prev_crc);
        assert_eq!(
            log.get(at..at + bytes.len()),
            Some(&bytes[..]),
            "frame {seq} does not re-encode to the bytes it was read from"
        );
        at += bytes.len();
        prev_crc = payload_crc(&bytes);
    }
}
