//! Metadata journal: record and frame layout, sequential frames, torn-tail recovery,
//! corruption detection, the checkpoint file, crash points, and the `meta/lock` writer lock.
//!
//! Every test owns a fresh store root (`tempfile::tempdir()`), so tests are
//! independent and may run in any order or in parallel.
//!
//! Frame sizes below are worked out by hand from the layout: a frame is
//! `8 (len, crc) + 2 (record count) + its records`; a changeset record is 74 bytes plus
//! 32 per parent, a ref move 13 bytes plus its name, its actor and 32 per id present, a
//! `Compacted` record 9 bytes.

#[cfg(feature = "test-hooks")]
use std::ffi::OsString;
use std::fmt::Debug;
use std::fs;
use std::io;
#[cfg(feature = "test-hooks")]
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::slice;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use camino::Utf8PathBuf;
use proptest::prelude::*;
use swarmops_core::{ChangesetId, ObjectId};
use swarmops_store::{
    BadRecord, Frame, FsMethod, FsOps, Journal, Layout, LimitKind, LogCut, MetaLock, RealFs,
    Record, StoreError, TailRepair,
};
#[cfg(feature = "test-hooks")]
use swarmops_store::{CRASH_AT_ENV, CrashAtFs};
#[cfg(feature = "test-hooks")]
use swarmops_testkit::child::{SIGABRT, run, this_test};
use swarmops_testkit::faults::{FsyncModelFs, checked};
use swarmops_testkit::fs::{Call, Hook, RecordingFs, Watch, Watched, When};
use swarmops_testkit::store::{append_bytes, file_len, new_root, real_fs, set_len};
#[cfg(feature = "test-hooks")]
use swarmops_testkit::test_name;

/// Set only in the children that `crash_at_any_journal_write_recovers_to_whole_frames`
/// spawns: the store root.
#[cfg(feature = "test-hooks")]
const CRASH_CHILD_ROOT: &str = "SWARMOPS_TEST_JOURNAL_CRASH_ROOT";

/// A new, empty journal whose tail this handle has checked: what a writer holds.
fn new_journal(layout: &Layout) -> Journal {
    let journal = Journal::create(layout, real_fs()).unwrap();
    assert_eq!(journal.truncate_torn_tail().unwrap(), None);
    journal
}

/// A second handle on the journal, as another process would open it.
fn reopen(layout: &Layout) -> Journal {
    Journal::open(layout, real_fs()).unwrap()
}

/// [`reopen`] under the order checker ([`checked`]): the crash sweeps recover so, every write
/// of their recovery checked against the write-ordering rules.
#[cfg(feature = "test-hooks")]
fn reopen_checked(layout: &Layout) -> Journal {
    Journal::open(layout, checked(RealFs)).unwrap()
}

fn id(byte: u8) -> ChangesetId {
    ChangesetId(ObjectId([byte; 32]))
}

/// 74 bytes encoded, so a frame of it alone is 84.
fn root_changeset() -> Record {
    Record::Changeset {
        id: id(0x01),
        generation: 1,
        delta_hash: [0xd1; 32],
        parents: vec![],
    }
}

/// 106 bytes encoded.
fn child_changeset() -> Record {
    Record::Changeset {
        id: id(0x02),
        generation: 2,
        delta_hash: [0xd2; 32],
        parents: vec![id(0x01)],
    }
}

/// 57 bytes encoded; a frame of the child and this move is 173.
fn main_created() -> Record {
    Record::RefMove {
        name: "main".into(),
        old: None,
        new: Some(id(0x02)),
        actor: "worker/1".into(),
        ts: 1_760_486_400,
    }
}

/// 9 bytes encoded, so a frame of it alone is 19.
fn compacted(sorted_count: u64) -> Record {
    Record::Compacted { sorted_count }
}

fn encoded(record: &Record) -> Vec<u8> {
    let mut bytes = Vec::new();
    record
        .encode(&mut bytes)
        .expect("a record within the layout's limits encodes");
    bytes
}

/// Frame bytes with a correct length and CRC around `payload`.
fn frame_bytes(payload: &[u8]) -> Vec<u8> {
    let mut frame = u32::try_from(payload.len()).unwrap().to_le_bytes().to_vec();
    frame.extend(crc32fast::hash(payload).to_le_bytes());
    frame.extend(payload);
    frame
}

/// The `u64le` entries of `journal.off`.
fn offsets_on_disk(layout: &Layout) -> Vec<u64> {
    let bytes = fs::read(layout.journal_off()).unwrap();
    let (entries, partial) = bytes.as_chunks::<8>();
    assert!(partial.is_empty(), "journal.off ends in a partial entry");
    entries
        .iter()
        .map(|entry| u64::from_le_bytes(*entry))
        .collect()
}

fn flip_byte(path: impl AsRef<Path>, at: usize) {
    let mut bytes = fs::read(&path).unwrap();
    bytes[at] ^= 0x01;
    fs::write(&path, bytes).unwrap();
}

/// The names of the entries in `dir`, sorted.
fn names_in(dir: impl AsRef<Path>) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

#[track_caller]
fn assert_corrupt_at<T: Debug>(got: Result<T, StoreError>, offset: u64, what: &str) {
    match got {
        Err(StoreError::JournalCorrupt { offset: at }) => assert_eq!(at, offset, "{what}"),
        other => panic!("{what}: expected JournalCorrupt at {offset}, got {other:?}"),
    }
}

/// A read of the journal from a reader's handle.
type Read = fn(&Journal) -> Result<Vec<Frame>, StoreError>;

/// Another handle on the journal whose FsOps calls are recorded, and a function that
/// returns the calls made since it last ran.
fn recording_journal(layout: &Layout) -> (Journal, impl Fn() -> Vec<String>) {
    racing_journal(layout, None)
}

/// [`recording_journal`], running `hook` once, right before or right after the call it names.
fn racing_journal(layout: &Layout, hook: Option<Hook>) -> (Journal, impl Fn() -> Vec<String>) {
    let (fs, calls) = RecordingFs::new();
    let fs = match hook {
        Some(hook) => fs.with_hook(hook),
        None => fs,
    };
    let journal = Journal::open(layout, Arc::new(fs)).unwrap();
    (journal, calls)
}

#[test]
fn frame_roundtrip_all_record_kinds() {
    let (a, b) = (id(0xaa), id(0xbb));
    let records = [
        Record::Changeset {
            id: id(0x11),
            generation: 1,
            delta_hash: [0xd1; 32],
            parents: vec![],
        },
        Record::Changeset {
            id: id(0x12),
            generation: 2,
            delta_hash: [0xd2; 32],
            parents: vec![a],
        },
        Record::Changeset {
            id: id(0x13),
            generation: 3,
            delta_hash: [0xd3; 32],
            parents: vec![a, b],
        },
        Record::RefMove {
            name: "main".into(),
            old: None,
            new: Some(a),
            actor: "worker/1".into(),
            ts: 1_760_486_400,
        },
        Record::RefMove {
            name: "main".into(),
            old: Some(a),
            new: Some(b),
            actor: "worker/1".into(),
            ts: 1_760_486_401,
        },
        Record::RefMove {
            name: "release/v1".into(),
            old: Some(b),
            new: None,
            actor: "ci/7".into(),
            ts: -2,
        },
        Record::RefMove {
            name: "release/v2".into(),
            old: None,
            new: None,
            actor: String::new(),
            ts: i64::MIN,
        },
        Record::Compacted { sorted_count: 1024 },
    ];
    let lens = [74, 106, 138, 57, 89, 59, 23, 9];
    for (record, len) in records.iter().zip(lens) {
        let mut bytes = encoded(record);
        assert_eq!(bytes.len(), len, "{record:?}");
        bytes.extend_from_slice(b"next record");
        assert_eq!(
            Record::decode(&bytes).unwrap(),
            (record.clone(), len),
            "decode stops where the record ends"
        );
    }

    let (_root, layout) = new_root();
    let journal = new_journal(&layout);
    journal.append_frame(&[compacted(0)]).unwrap(); // frame 0, 19 bytes
    let frame = journal.append_frame(&records).unwrap();
    let want = Frame {
        seq: 1,
        offset: 19,
        records: records.to_vec(),
    };
    assert_eq!(frame, want);
    assert_eq!(file_len(layout.journal_log()), 19 + 8 + 2 + 555);
    let reader = reopen(&layout);
    assert_eq!(reader.frames_after(0, 1).unwrap(), slice::from_ref(&want));
    assert_eq!(reader.tail_frames_from(19).unwrap(), [want]);
}

#[test]
fn records_and_frames_use_the_fixed_byte_layout() {
    let changeset = Record::Changeset {
        id: id(0x11),
        generation: 0x0102_0304_0506_0708,
        delta_hash: [0xdd; 32],
        parents: vec![id(0xaa), id(0xbb)],
    };
    let mut want = vec![1];
    want.extend([0x11; 32]);
    want.extend([8, 7, 6, 5, 4, 3, 2, 1]);
    want.extend([0xdd; 32]);
    want.push(2);
    want.extend([0xaa; 32]);
    want.extend([0xbb; 32]);
    assert_eq!(encoded(&changeset), want, "changeset");

    let ref_move = Record::RefMove {
        name: "main".into(),
        old: Some(id(0xaa)),
        new: None,
        actor: "ci/7".into(),
        ts: -2,
    };
    let mut want = vec![2, 4];
    want.extend(b"main");
    want.push(1);
    want.extend([0xaa; 32]);
    want.extend([0, 4]);
    want.extend(b"ci/7");
    want.extend([0xfe, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);
    assert_eq!(encoded(&ref_move), want, "ref move");

    assert_eq!(
        encoded(&compacted(1024)),
        [4, 0x00, 0x04, 0, 0, 0, 0, 0, 0],
        "compacted"
    );

    // On disk: u32le len | u32le crc32(payload) (CRC-32/IEEE, `crc32fast`) | payload, payload =
    // u16le count | records; journal.off gets the frame's start offset as u64le.
    let (_root, layout) = new_root();
    new_journal(&layout)
        .append_frame(&[compacted(1024)])
        .unwrap();
    let payload = [1, 0, 4, 0x00, 0x04, 0, 0, 0, 0, 0, 0];
    let mut want = vec![11, 0, 0, 0];
    // CRC-32 (IEEE 802.3, zlib's crc32: reflected polynomial 0xEDB88320, init and xor-out
    // 0xFFFFFFFF) of `payload`: 0x03aa1b33, little-endian. A literal, so that no swap of the
    // algorithm passes by comparing the CRC with itself.
    want.extend([0x33, 0x1b, 0xaa, 0x03]);
    want.extend(payload);
    assert_eq!(fs::read(layout.journal_log()).unwrap(), want);
    assert_eq!(fs::read(layout.journal_off()).unwrap(), [0; 8]);
}

#[test]
fn record_decode_rejects_truncations_unknown_kinds_and_bad_flags() {
    // Bytes that are not a record are a `BadRecord` at the position in them where decoding
    // stopped, never a `JournalCorrupt`, whose offset is always one in `journal.log`.
    for record in [
        root_changeset(),
        child_changeset(),
        main_created(),
        compacted(9),
    ] {
        let bytes = encoded(&record);
        for cut in 0..bytes.len() {
            let got = Record::decode(&bytes[..cut]);
            assert!(
                matches!(got, Err(BadRecord { at }) if at <= cut),
                "{record:?} cut to {cut} bytes: {got:?}"
            );
        }
    }
    for kind in [0, 3, 5, 0xff] {
        let got = Record::decode(&[kind; 80]);
        assert!(
            matches!(got, Err(BadRecord { at: 0 })),
            "kind {kind}: {got:?}"
        );
    }
    // `2 | 4 "main" | has_old`: a flag byte must be 0 or 1.
    let mut bad_flag = encoded(&main_created());
    bad_flag[6] = 2;
    let got = Record::decode(&bad_flag);
    assert!(matches!(got, Err(BadRecord { at: 6 })), "{got:?}");
    // A ref name that is not UTF-8: decoding stops where the name starts.
    let mut not_utf8 = encoded(&main_created());
    not_utf8[2] = 0xff;
    let got = Record::decode(&not_utf8);
    assert!(matches!(got, Err(BadRecord { at: 2 })), "{got:?}");
}

fn arb_id() -> impl Strategy<Value = ChangesetId> {
    any::<[u8; 32]>().prop_map(|bytes| ChangesetId(ObjectId(bytes)))
}

fn arb_record() -> impl Strategy<Value = Record> {
    prop_oneof![
        (
            arb_id(),
            any::<u64>(),
            any::<[u8; 32]>(),
            proptest::collection::vec(arb_id(), 0..4)
        )
            .prop_map(|(id, generation, delta_hash, parents)| Record::Changeset {
                id,
                generation,
                delta_hash,
                parents,
            }),
        (
            "[a-z0-9/._-]{0,40}",
            proptest::option::of(arb_id()),
            proptest::option::of(arb_id()),
            "\\PC{0,20}",
            any::<i64>()
        )
            .prop_map(|(name, old, new, actor, ts)| Record::RefMove {
                name,
                old,
                new,
                actor,
                ts,
            }),
        any::<u64>().prop_map(|sorted_count| Record::Compacted { sorted_count }),
    ]
}

proptest! {
    #[test]
    fn records_roundtrip(record in arb_record()) {
        let bytes = encoded(&record);
        prop_assert_eq!(Record::decode(&bytes).unwrap(), (record, bytes.len()));
    }

    #[test]
    fn record_decode_never_panics_and_accepts_only_canonical_bytes(
        kind in prop_oneof![Just(1_u8), Just(2), Just(4), any::<u8>()],
        rest in proptest::collection::vec(any::<u8>(), 0..160),
    ) {
        let mut bytes = vec![kind];
        bytes.extend(rest);
        if let Ok((record, used)) = Record::decode(&bytes) {
            prop_assert_eq!(&encoded(&record)[..], &bytes[..used]);
        }
    }
}

#[test]
fn append_frame_assigns_sequential_seq_and_offsets() {
    let (_root, layout) = new_root();
    let journal = new_journal(&layout);
    assert!(journal.is_empty());
    let first = journal.append_frame(&[root_changeset()]).unwrap();
    let second = journal
        .append_frame(&[child_changeset(), main_created()])
        .unwrap();
    let third = journal.append_frame(&[compacted(2)]).unwrap();
    assert_eq!((first.seq, first.offset), (0, 0));
    assert_eq!((second.seq, second.offset), (1, 84));
    assert_eq!((third.seq, third.offset), (2, 257));
    assert_eq!(second.records, [child_changeset(), main_created()]);
    assert_eq!(journal.len(), 276);
    assert_eq!(file_len(layout.journal_log()), 276);
    assert_eq!(offsets_on_disk(&layout), [0, 84, 257]);

    // frames_after(seq, ..) is exclusive: the frames whose seq is greater than `seq`.
    let all = [first.clone(), second.clone(), third.clone()];
    assert_eq!(journal.frames_after(0, 10).unwrap(), all[1..]);
    assert_eq!(journal.frames_after(1, 10).unwrap(), all[2..]);
    assert_eq!(journal.frames_after(2, 10).unwrap(), Vec::<Frame>::new());
    assert_eq!(
        journal.frames_after(0, 1).unwrap(),
        all[1..2],
        "at most limit"
    );
    assert_eq!(journal.frames_after(0, 0).unwrap(), Vec::<Frame>::new());
    assert_eq!(
        journal.frames_after(u64::MAX, 10).unwrap(),
        Vec::<Frame>::new()
    );

    // Another handle reads the same frames, numbered from any frame boundary.
    let reader = reopen(&layout);
    assert_eq!(reader.len(), 276);
    assert_eq!(reader.frames_after(0, 10).unwrap(), all[1..]);
    assert_eq!(reader.tail_frames_from(0).unwrap(), all);
    assert_eq!(reader.tail_frames_from(84).unwrap(), all[1..]);
    assert_eq!(reader.tail_frames_from(276).unwrap(), Vec::<Frame>::new());
    assert_corrupt_at(reader.tail_frames_from(50), 50, "inside frame 0");
    assert_corrupt_at(reader.tail_frames_from(260), 260, "inside the last frame");
    assert_corrupt_at(reader.tail_frames_from(277), 277, "past the end");
}

#[test]
fn frames_from_is_inclusive_and_starts_at_frame_zero() {
    let (_root, layout) = new_root();
    let journal = new_journal(&layout);
    let first = journal.append_frame(&[root_changeset()]).unwrap();
    let second = journal
        .append_frame(&[child_changeset(), main_created()])
        .unwrap();
    let third = journal.append_frame(&[compacted(2)]).unwrap();
    let all = [first, second, third];
    assert_eq!(journal.frame_count().unwrap(), 3);

    let reader = reopen(&layout);
    assert_eq!(reader.frame_count().unwrap(), 3);
    assert_eq!(reader.frames_from(0, 10).unwrap(), all);
    assert_eq!(reader.frames_from(1, 10).unwrap(), all[1..]);
    assert_eq!(reader.frames_from(0, 2).unwrap(), all[..2], "at most limit");
    assert_eq!(reader.frames_from(2, 1).unwrap(), all[2..]);
    assert_eq!(reader.frames_from(3, 10).unwrap(), Vec::<Frame>::new());
    assert_eq!(reader.frames_from(0, 0).unwrap(), Vec::<Frame>::new());
    assert_eq!(
        reader.frames_from(u64::MAX, 10).unwrap(),
        Vec::<Frame>::new()
    );
    for seq in 0..3 {
        let after = reader.frames_after(seq, 10).unwrap();
        assert_eq!(after, reader.frames_from(seq + 1, 10).unwrap(), "seq {seq}");
    }
}

#[test]
fn a_first_frame_that_does_not_start_at_zero_is_journal_corrupt() {
    // journal.off lists the frame at 19 as frame 0: the bytes before it are no frame.
    let (_root, layout) = new_root();
    let journal = new_journal(&layout);
    journal.append_frame(&[compacted(1)]).unwrap(); // bytes 0..19
    journal.append_frame(&[root_changeset()]).unwrap(); // bytes 19..103
    fs::write(layout.journal_off(), 19_u64.to_le_bytes()).unwrap();
    assert_corrupt_at(reopen(&layout).frames_from(0, 10), 19, "frames_from");
}

#[test]
fn open_needs_a_journal_and_create_keeps_an_existing_one() {
    let (_root, layout) = new_root();
    let got = Journal::open(&layout, real_fs()).map(|journal| journal.len());
    assert!(
        matches!(&got, Err(StoreError::Io(e)) if e.kind() == io::ErrorKind::NotFound),
        "{got:?}"
    );

    let journal = new_journal(&layout);
    let first = journal.append_frame(&[root_changeset()]).unwrap();
    let again = Journal::create(&layout, real_fs()).unwrap();
    assert_eq!(again.tail_frames_from(0).unwrap(), [first]);
    assert_eq!(again.len(), 84);
}

#[test]
fn torn_tail_is_ignored_by_readers_and_truncated_by_writer() {
    // One `Compacted { sorted_count: 0 }`, behind a CRC that does not match it.
    let payload = [1, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0];
    let mut bad_crc = vec![11, 0, 0, 0];
    bad_crc.extend((crc32fast::hash(&payload) ^ 1).to_le_bytes());
    bad_crc.extend(payload);
    let torn_tails = [
        ("7 garbage bytes", b"garbage".to_vec()),
        (
            "a header and part of its payload",
            vec![76, 0, 0, 0, 9, 9, 9, 9, 1, 0, 1],
        ),
        (
            "a length past the end of the file",
            vec![0xff, 0xff, 0xff, 0xff, 0, 0, 0, 0],
        ),
        ("a whole frame failing its CRC", bad_crc),
    ];
    for (what, torn) in torn_tails {
        let (_root, layout) = new_root();
        let journal = new_journal(&layout);
        let first = journal.append_frame(&[root_changeset()]).unwrap();
        let second = journal
            .append_frame(&[child_changeset(), main_created()])
            .unwrap();
        append_bytes(layout.journal_log(), &torn);

        let reader = reopen(&layout);
        let whole = [first.clone(), second.clone()];
        assert_eq!(reader.tail_frames_from(0).unwrap(), whole, "{what}");
        assert_eq!(reader.frames_after(0, 10).unwrap(), whole[1..], "{what}");

        assert_eq!(journal.truncate_torn_tail().unwrap(), Some(257), "{what}");
        assert_eq!(file_len(layout.journal_log()), 257, "{what}");
        assert_eq!(journal.len(), 257, "{what}");
        let third = journal.append_frame(&[compacted(2)]).unwrap();
        assert_eq!((third.seq, third.offset), (2, 257), "{what}");
        assert_eq!(
            reopen(&layout).tail_frames_from(0).unwrap(),
            [first, second, third],
            "{what}"
        );
        assert_eq!(offsets_on_disk(&layout), [0, 84, 257], "{what}");
        assert_eq!(journal.truncate_torn_tail().unwrap(), None, "{what}");
    }
}

#[test]
fn zero_filled_tail_is_torn_and_cut() {
    // A filesystem may expose zero-filled blocks at the end of a file after a crash. No frame
    // starts with `len == 0`, so unlisted bytes that are zero all the way to the end of
    // journal.log hold no committed frame: a torn tail, whatever its length. (Up to 7 bytes
    // they are a header cut short as well; from 8 on, a header announcing an empty payload.)
    for zeros in [1, 7, 8, 9, 100] {
        let what = format!("{zeros} zero bytes after the last listed frame");
        let (_root, layout) = new_root();
        let journal = new_journal(&layout);
        let first = journal.append_frame(&[root_changeset()]).unwrap(); // bytes 0..84
        append_bytes(layout.journal_log(), &vec![0; zeros]);

        let reader = reopen(&layout);
        assert_eq!(
            reader.tail_frames_from(0).unwrap(),
            slice::from_ref(&first),
            "{what}"
        );
        assert_eq!(
            reader.tail_frames_from(84).unwrap(),
            Vec::<Frame>::new(),
            "{what}"
        );
        assert_eq!(journal.truncate_torn_tail().unwrap(), Some(84), "{what}");
        assert_eq!(file_len(layout.journal_log()), 84, "{what}: cut");
        assert_eq!(offsets_on_disk(&layout), [0], "{what}");
        assert_eq!(
            journal.truncate_torn_tail().unwrap(),
            None,
            "{what}: a second call"
        );
        assert_eq!(
            reopen(&layout).tail_frames_from(0).unwrap(),
            [first],
            "{what}"
        );
    }

    // A byte that is not zero after them changes nothing: from 4 zeros on, the header's
    // length reads 0, which no writer writes, so the header never reached the disk (see
    // `a_frame_header_that_never_reached_the_disk_is_a_torn_tail`).
    for zeros in [4, 7, 8, 100] {
        let what = format!("{zeros} zero bytes, then a 1");
        let (_root, layout) = new_root();
        let journal = new_journal(&layout);
        let first = journal.append_frame(&[root_changeset()]).unwrap();
        let mut tail = vec![0; zeros];
        tail.push(1);
        append_bytes(layout.journal_log(), &tail);
        assert_eq!(
            reopen(&layout).tail_frames_from(0).unwrap(),
            [first],
            "{what}"
        );
        assert_eq!(journal.truncate_torn_tail().unwrap(), Some(84), "{what}");
        assert_eq!(file_len(layout.journal_log()), 84, "{what}: cut");
    }
}

#[test]
fn a_frame_header_that_never_reached_the_disk_is_a_torn_tail() {
    // A frame is written, then synced. A power loss in between may keep a later sector of the
    // write and lose the first, zeros there: the header's length reads 0 (with its CRC lost
    // too, or not), and bytes that are not zero follow. No writer writes a frame of length 0,
    // and a sync that completed would have made the header durable: the frame never reached its
    // commit point. Readers stop there, and recovery cuts it, as any torn tail.
    let payload = [
        [2, 0].as_slice(),
        &encoded(&child_changeset()),
        &encoded(&main_created()),
    ]
    .concat();
    let second = frame_bytes(&payload); // 173 bytes
    for lost in [4, 6, 8, 20] {
        let what = format!("the first {lost} bytes of frame 1 lost");
        let (_root, layout) = new_root();
        let journal = new_journal(&layout);
        let first = journal.append_frame(&[root_changeset()]).unwrap(); // bytes 0..84
        let mut torn = second.clone();
        torn[..lost].fill(0);
        append_bytes(layout.journal_log(), &torn);

        let reader = reopen(&layout);
        assert_eq!(
            reader.tail_frames_from(0).unwrap(),
            slice::from_ref(&first),
            "{what}"
        );
        assert_eq!(
            reader.tail_frames_from(84).unwrap(),
            Vec::<Frame>::new(),
            "{what}"
        );
        assert_eq!(journal.truncate_torn_tail().unwrap(), Some(84), "{what}");
        assert_eq!(file_len(layout.journal_log()), 84, "{what}: cut");
        assert_eq!(offsets_on_disk(&layout), [0], "{what}");
        let next = journal.append_frame(&[compacted(1)]).unwrap();
        assert_eq!(next.offset, 84, "{what}: the next frame lands at the cut");
        assert_eq!(
            reopen(&layout).tail_frames_from(0).unwrap(),
            [first, next],
            "{what}"
        );
    }
}

#[test]
fn crc_mismatch_is_journal_corrupt_not_panic() {
    let (_root, layout) = new_root();
    let journal = new_journal(&layout);
    journal.append_frame(&[root_changeset()]).unwrap();
    let second = journal
        .append_frame(&[child_changeset(), main_created()])
        .unwrap();

    flip_byte(layout.journal_log(), 40); // a payload byte of frame 0 (bytes 10..84)
    assert_corrupt_at(journal.tail_frames_from(0), 0, "tail_frames_from");
    assert_eq!(
        journal.frames_after(0, 10).unwrap(),
        [second],
        "frame 0 itself is not read"
    );
    assert_eq!(journal.truncate_torn_tail().unwrap(), None);
    assert_eq!(file_len(layout.journal_log()), 257);

    // Frame 1 of 2, the last: journal.off lists it, so it was whole once. A CRC mismatch
    // there is damage, not a torn write, and recovery must not cut it.
    flip_byte(layout.journal_log(), 200); // a payload byte of frame 1 (bytes 94..257)
    assert_corrupt_at(journal.frames_after(0, 10), 84, "frames_after");
    assert_corrupt_at(journal.tail_frames_from(84), 84, "tail_frames_from");
    assert_corrupt_at(journal.truncate_torn_tail(), 84, "truncate_torn_tail");
    assert_eq!(file_len(layout.journal_log()), 257, "nothing was cut");
    assert_eq!(offsets_on_disk(&layout), [0, 84]);
}

#[test]
fn frames_no_writer_produces_are_journal_corrupt_not_torn() {
    // Frames whose payload matches its CRC and does not decode: a writer wrote them (a newer
    // version, say), so they are damage, not a torn tail. (A header announcing an empty
    // payload is no such frame unless it is listed: unlisted, its length of 0 says it never
    // reached the disk, `a_frame_header_that_never_reached_the_disk_is_a_torn_tail`.)
    let cases = [
        ("zero records", frame_bytes(&[0, 0])),
        ("an unknown record kind", frame_bytes(&[1, 0, 3])),
        (
            "bytes after the last record",
            frame_bytes(&[1, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0xee]),
        ),
        (
            "a ref name that is not UTF-8",
            frame_bytes(&[1, 0, 2, 1, 0xff, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
        ),
    ];
    for (what, bad) in cases {
        let (_root, layout) = new_root();
        let journal = new_journal(&layout);
        journal.append_frame(&[root_changeset()]).unwrap();
        let second = journal.append_frame(&[compacted(1)]).unwrap(); // bytes 84..103
        append_bytes(layout.journal_log(), &bad);
        assert_corrupt_at(reopen(&layout).tail_frames_from(0), 103, what);
        assert_corrupt_at(journal.truncate_torn_tail(), 103, what);
        assert_eq!(
            file_len(layout.journal_log()),
            103 + u64::try_from(bad.len()).unwrap(),
            "{what}: nothing was cut"
        );

        append_bytes(layout.journal_off(), &103_u64.to_le_bytes());
        assert_corrupt_at(reopen(&layout).frames_after(0, 10), 103, what);
        assert_eq!(
            reopen(&layout).frames_after(0, 1).unwrap(),
            [second],
            "{what}"
        );
    }

    // Listed, a header announcing an empty payload is damage too: a listed frame was durable.
    let (_root, layout) = new_root();
    let journal = new_journal(&layout);
    journal.append_frame(&[root_changeset()]).unwrap();
    journal.append_frame(&[compacted(1)]).unwrap(); // bytes 84..103
    append_bytes(
        layout.journal_log(),
        &[frame_bytes(&[]), vec![0xee]].concat(),
    );
    append_bytes(layout.journal_off(), &103_u64.to_le_bytes());
    let what = "a listed empty payload";
    assert_corrupt_at(reopen(&layout).frames_after(0, 10), 103, what);
    assert_corrupt_at(reopen(&layout).tail_frames_from(0), 103, what);
    assert_corrupt_at(journal.truncate_torn_tail(), 103, what);
}

#[test]
fn truncate_torn_tail_lists_a_whole_frame_that_lost_its_entry() {
    // A crash between a frame's two appends leaves the frame whole and journal.off one
    // entry short, or holding the first bytes of that entry.
    let entry = 84_u64.to_le_bytes();
    for partial_entry in [&[][..], &entry[..3]] {
        let (_root, layout) = new_root();
        let journal = new_journal(&layout);
        let first = journal.append_frame(&[root_changeset()]).unwrap();
        let second = journal
            .append_frame(&[child_changeset(), main_created()])
            .unwrap();
        set_len(layout.journal_off(), 8);
        append_bytes(layout.journal_off(), partial_entry);

        let reader = reopen(&layout);
        assert_eq!(
            reader.frames_after(0, 10).unwrap(),
            Vec::<Frame>::new(),
            "journal.off does not list frame 1 yet"
        );
        assert_eq!(
            reader.tail_frames_from(0).unwrap(),
            [first, second.clone()],
            "the whole frame is committed, so readers see it"
        );

        assert_eq!(
            journal.truncate_torn_tail().unwrap(),
            None,
            "nothing is cut"
        );
        assert_eq!(offsets_on_disk(&layout), [0, 84]);
        assert_eq!(file_len(layout.journal_log()), 257);
        assert_eq!(reader.frames_after(0, 10).unwrap(), [second]);
        assert_eq!(journal.append_frame(&[compacted(2)]).unwrap().seq, 2);
    }
}

#[test]
fn a_listed_frame_cut_short_is_journal_corrupt() {
    // A listed frame's bytes were synced before its entry was appended, so journal.log
    // ending inside one is a filesystem losing a committed frame, never a torn write.
    // Recovery cuts nothing and reports it; readers, which measure journal.off before
    // journal.log, report it too.
    let (_root, layout) = new_root();
    let journal = new_journal(&layout);
    let first = journal.append_frame(&[root_changeset()]).unwrap(); // bytes 0..84
    journal
        .append_frame(&[child_changeset(), main_created()])
        .unwrap(); // bytes 84..257
    set_len(layout.journal_log(), 104); // 20 bytes into frame 1
    assert_corrupt_at(journal.truncate_torn_tail(), 84, "truncate_torn_tail");
    assert_corrupt_at(journal.truncate_torn_tail(), 84, "a second call");
    assert_corrupt_at(journal.append_frame(&[compacted(2)]), 84, "append_frame");
    assert_eq!(
        file_len(layout.journal_log()),
        104,
        "nothing was cut or appended"
    );
    assert_eq!(offsets_on_disk(&layout), [0, 84], "no entry was dropped");
    let reader = reopen(&layout);
    assert_corrupt_at(reader.tail_frames_from(0), 84, "tail_frames_from(0)");
    assert_corrupt_at(reader.tail_frames_from(84), 84, "tail_frames_from(84)");
    assert_corrupt_at(reader.frames_from(0, 10), 84, "frames_from");
    assert_corrupt_at(reader.frames_after(0, 10), 84, "frames_after");
    assert_eq!(
        reader.frames_from(0, 1).unwrap(),
        slice::from_ref(&first),
        "frame 0 still reads"
    );

    // Damage before the frame changes nothing: frame 0 failing its CRC, journal.log ending 2
    // bytes into frame 1 (found by `fuzz/fuzz_targets/journal_recovery.rs`).
    let (_root, layout) = new_root();
    let journal = new_journal(&layout);
    journal.append_frame(&[compacted(1)]).unwrap(); // bytes 0..19
    journal.append_frame(&[compacted(2)]).unwrap(); // bytes 19..38
    set_len(layout.journal_log(), 21);
    flip_byte(layout.journal_log(), 17); // a payload byte of frame 0
    assert_corrupt_at(journal.truncate_torn_tail(), 19, "frame 1 is cut short");
    assert_eq!(file_len(layout.journal_log()), 21, "nothing was cut");
    assert_eq!(offsets_on_disk(&layout), [0, 19], "no entry was dropped");

    // Nor do garbage entries: entry 1 (43) points into frame 0 (bytes 0..84), at its
    // generation, which reads as a header announcing 1 byte, so a frame ending at 52, where
    // entry 2 points, at bytes that read as a frame cut short. Cutting at 52 would destroy
    // frame 0. Recovery checks the two links back from the last listed frame before it reads
    // that frame, and the second one, entry 1, does not start where frame 0 ends.
    let (_root, layout) = new_root();
    let journal = new_journal(&layout);
    journal.append_frame(&[root_changeset()]).unwrap();
    append_bytes(layout.journal_off(), &43_u64.to_le_bytes());
    append_bytes(layout.journal_off(), &52_u64.to_le_bytes());
    assert_corrupt_at(
        journal.truncate_torn_tail(),
        43,
        "entry 1 is not where frame 0 ends",
    );
    assert_eq!(file_len(layout.journal_log()), 84, "nothing was cut");
    assert_eq!(
        offsets_on_disk(&layout),
        [0, 43, 52],
        "no entry was dropped"
    );
}

#[test]
fn a_log_ending_at_the_last_listed_frames_start_is_journal_corrupt() {
    // journal.off lists frame 1, but journal.log ends where it starts: the committed frame
    // is missing as a whole, for recovery and for readers alike.
    let (_root, layout) = new_root();
    let journal = new_journal(&layout);
    let first = journal.append_frame(&[root_changeset()]).unwrap(); // bytes 0..84
    journal
        .append_frame(&[child_changeset(), main_created()])
        .unwrap(); // bytes 84..257
    set_len(layout.journal_log(), 84);
    assert_corrupt_at(journal.truncate_torn_tail(), 84, "truncate_torn_tail");
    assert_eq!(file_len(layout.journal_log()), 84, "nothing was cut");
    assert_eq!(offsets_on_disk(&layout), [0, 84], "no entry was dropped");
    let reader = reopen(&layout);
    assert_eq!(reader.frame_count().unwrap(), 2);
    assert_corrupt_at(reader.tail_frames_from(0), 84, "tail_frames_from(0)");
    assert_corrupt_at(
        reader.tail_frames_from(84),
        84,
        "tail_frames_from(84), the end",
    );
    assert_corrupt_at(reader.frames_from(0, 10), 84, "frames_from");
    assert_corrupt_at(reader.frames_after(0, 10), 84, "frames_after");
    assert_eq!(reader.frames_from(0, 1).unwrap(), [first]);

    // The only listed frame missing: an empty journal.log under journal.off = [0].
    set_len(layout.journal_off(), 8);
    set_len(layout.journal_log(), 0);
    assert_corrupt_at(journal.truncate_torn_tail(), 0, "frame 0 is missing");
    assert_eq!(offsets_on_disk(&layout), [0], "no entry was dropped");
    assert_corrupt_at(
        reopen(&layout).tail_frames_from(0),
        0,
        "tail_frames_from(0)",
    );
}

#[test]
fn readers_measure_off_before_log() {
    // A frame listed in a snapshot of journal.off was synced before its entry was written,
    // so it is whole in any journal.log measured after: readers measure journal.off first,
    // and never again once journal.log is measured.
    let order = ["file_len journal.off", "file_len journal.log"];
    let (_root, layout) = new_root();
    let journal = new_journal(&layout);
    journal.append_frame(&[root_changeset()]).unwrap(); // bytes 0..84
    journal
        .append_frame(&[child_changeset(), main_created()])
        .unwrap(); // bytes 84..257
    let (reader, calls) = recording_journal(&layout);
    reader.tail_frames_from(0).unwrap();
    assert_eq!(calls(), order, "tail_frames_from(0)");
    reader.tail_frames_from(257).unwrap();
    assert_eq!(calls(), order, "tail_frames_from at the end");
    reader.frames_from(0, 10).unwrap();
    assert_eq!(calls(), order, "frames_from");
    reader.frames_after(0, 10).unwrap();
    assert_eq!(calls(), order, "frames_after");

    // A writer that appends a whole frame and its entry between the two measurements: to the
    // reader that frame is whole and not listed yet.
    let (_root, layout) = new_root();
    let journal = new_journal(&layout);
    let first = journal.append_frame(&[root_changeset()]).unwrap(); // bytes 0..84
    let racing_at = |call: &str| -> Hook {
        let (log, off) = (layout.journal_log(), layout.journal_off());
        let frame = frame_bytes(&[&[1, 0][..], &encoded(&compacted(2))].concat());
        let append = move || {
            append_bytes(&log, &frame);
            append_bytes(&off, &84_u64.to_le_bytes());
        };
        (call.to_owned(), When::Before, Box::new(append))
    };
    let racing = || racing_at("file_len journal.log");
    let raced = Frame {
        seq: 1,
        offset: 84,
        records: vec![compacted(2)],
    };
    let (reader, calls) = racing_journal(&layout, Some(racing()));
    assert_eq!(
        reader.tail_frames_from(0).unwrap(),
        [first.clone(), raced.clone()],
        "tail_frames_from includes the frame, unlisted when it measured journal.off"
    );
    assert_eq!(calls(), order);

    set_len(layout.journal_log(), 84);
    set_len(layout.journal_off(), 8);
    let (reader, calls) = racing_journal(&layout, Some(racing()));
    assert_eq!(
        reader.frames_from(0, 10).unwrap(),
        slice::from_ref(&first),
        "frames_from returns the frames listed when it measured journal.off"
    );
    assert_eq!(calls(), order);
    assert_eq!(
        reader.frames_from(0, 10).unwrap(),
        [first.clone(), raced.clone()]
    );

    // A writer that appends just before journal.off is measured: the reader sees the frame
    // listed and whole. One that measured journal.log first would find it listed and missing.
    set_len(layout.journal_log(), 84);
    set_len(layout.journal_off(), 8);
    let (reader, _) = racing_journal(&layout, Some(racing_at("file_len journal.off")));
    assert_eq!(reader.tail_frames_from(0).unwrap(), [first, raced]);
}

#[test]
fn journal_off_cut_after_a_reader_measured_it_is_read_again() {
    // A reader measures journal.off, then reads the entries it holds. If journal.off is
    // shorter by then, a writer has cut entries that were never genuine ones (an entry whose own
    // append failed, or zeroed ones a crash left at the end): the reader measures journal.off
    // again, before it measures journal.log, and reads what it lists then. The frame whose entry
    // was cut is whole in journal.log, a frame not listed yet.
    let (_root, layout) = new_root();
    let journal = new_journal(&layout);
    let first = journal.append_frame(&[root_changeset()]).unwrap(); // bytes 0..84
    let second = journal
        .append_frame(&[child_changeset(), main_created()])
        .unwrap(); // bytes 84..257
    let cut_to_one_entry = || -> Hook {
        let off = layout.journal_off();
        let cut = move || set_len(&off, 8);
        (
            "file_len journal.off".to_owned(),
            When::After,
            Box::new(cut),
        )
    };
    let again = ["file_len journal.off", "file_len journal.off"];
    let then_log = [&again[..], &["file_len journal.log"]].concat();
    let both = [first.clone(), second];
    let reads: [(&str, Read, &[Frame], &[&str]); 3] = [
        (
            "tail_frames_from(0)",
            |reader| reader.tail_frames_from(0),
            &both,
            &then_log,
        ),
        (
            "frames_from(0, 10)",
            |reader| reader.frames_from(0, 10),
            slice::from_ref(&first),
            &then_log,
        ),
        // It finds no frame to read, so it returns before it measures journal.log.
        (
            "frames_after(0, 10)",
            |reader| reader.frames_after(0, 10),
            &[],
            &again,
        ),
    ];
    for (what, read, frames, measured) in reads {
        set_len(layout.journal_off(), 8);
        append_bytes(layout.journal_off(), &84_u64.to_le_bytes());
        let (reader, calls) = racing_journal(&layout, Some(cut_to_one_entry()));
        assert_eq!(read(&reader).unwrap(), frames, "{what}");
        assert_eq!(calls(), measured, "{what}");
    }

    // A writer that cuts journal.off back each time the reader has measured it: the reader reads
    // three times more, then reports the journal corrupt where the shrink would have, before it
    // ever measures journal.log.
    let entries: Vec<u8> = [0_u64, 84].iter().flat_map(|e| e.to_le_bytes()).collect();
    let cutting = Arc::new(Watched(
        CutAfterEachMeasurement {
            entries,
            calls: Mutex::new(Vec::new()),
        },
        RealFs,
    ));
    let reader = Journal::open(&layout, Arc::clone(&cutting) as Arc<dyn FsOps>).unwrap();
    for (what, read, _, _) in reads {
        assert_corrupt_at(read(&reader), 0, what);
        let calls = std::mem::take(&mut *cutting.calls.lock().unwrap());
        assert_eq!(calls, ["file_len journal.off"; 4], "{what}");
    }

    // Three frames, journal.off cut from three entries to two: the reader meets the cut reading
    // the last entry it measured, reads again, and finds the third frame whole in journal.log,
    // not listed.
    let (_root, layout) = new_root();
    let journal = new_journal(&layout);
    let all: Vec<Frame> = (1..=3)
        .map(|n| journal.append_frame(&[compacted(n)]).unwrap()) // bytes 0..19, 19..38, 38..57
        .collect();
    let cut_to_two_entries = || -> Hook {
        let off = layout.journal_off();
        let cut = move || set_len(&off, 16);
        (
            "file_len journal.off".to_owned(),
            When::After,
            Box::new(cut),
        )
    };
    for (from, frames) in [(0, &all[..]), (19, &all[1..])] {
        let what = format!("tail_frames_from({from})");
        let off: Vec<u8> = [0_u64, 19, 38]
            .iter()
            .flat_map(|e| e.to_le_bytes())
            .collect();
        fs::write(layout.journal_off(), off).unwrap();
        let (reader, calls) = racing_journal(&layout, Some(cut_to_two_entries()));
        assert_eq!(reader.tail_frames_from(from).unwrap(), frames, "{what}");
        assert_eq!(calls(), then_log, "{what}");
    }
}

/// Restores `journal.off` to `entries` right before each measurement of it, and cuts it back to
/// its first entry right after: a writer that cuts an entry (one whose own append failed, say)
/// every time a reader has measured. Records every call, by name ([`Call::named`]).
struct CutAfterEachMeasurement {
    entries: Vec<u8>,
    calls: Mutex<Vec<String>>,
}

impl Watch for CutAfterEachMeasurement {
    fn before(&self, call: &Call<'_>) -> io::Result<()> {
        let named = call.named();
        if named == "file_len journal.off" {
            fs::write(call.path(), &self.entries).unwrap();
        }
        self.calls.lock().unwrap().push(named);
        Ok(())
    }

    fn after(&self, call: &Call<'_>) {
        if call.named() == "file_len journal.off" {
            set_len(call.path(), 8);
        }
    }
}

#[test]
fn a_single_entry_cut_after_a_reader_measured_it_is_read_again() {
    // With one entry listed there is no last entry for the reader to check before the rest (the
    // check that meets a cut of two or more entries first), so a cut of journal.off to nothing
    // is met where the reader reads the entries: frames_from's read of them, and
    // tail_frames_from's search for the first frame at or after its start. Both measure
    // journal.off again, as at any cut, before they measure journal.log.
    let (_root, layout) = new_root();
    let journal = new_journal(&layout);
    let first = journal.append_frame(&[root_changeset()]).unwrap(); // bytes 0..84, entry [0]
    let cut_to_nothing = || -> Hook {
        let off = layout.journal_off();
        let cut = move || set_len(&off, 0);
        (
            "file_len journal.off".to_owned(),
            When::After,
            Box::new(cut),
        )
    };
    let (off, log) = ("file_len journal.off", "file_len journal.log");

    // Nothing is listed once the entry is cut, so frames_from returns nothing, before it ever
    // measures journal.log.
    let (reader, calls) = racing_journal(&layout, Some(cut_to_nothing()));
    assert_eq!(reader.frames_from(0, 10).unwrap(), []);
    assert_eq!(calls(), [off, off]);

    // The frame is whole in journal.log, not listed any more: tail_frames_from reads it as such.
    append_bytes(layout.journal_off(), &0_u64.to_le_bytes());
    let (reader, calls) = racing_journal(&layout, Some(cut_to_nothing()));
    assert_eq!(reader.tail_frames_from(0).unwrap(), [first]);
    assert_eq!(calls(), [off, off, log]);
}

#[test]
fn a_zeroed_entry_before_the_end_of_journal_off_is_journal_corrupt_for_readers_too() {
    // Zeroed entries at the end of journal.off are dropped, by readers and recovery alike
    // (`zeroed_entries_at_the_end_of_journal_off_are_dropped_by_readers_too`). One before the
    // end is garbage like any entry that does not start where the frame listed before it ends.
    //
    // Read from a listed frame's start, 19, with a bad entry before it: the frame listed just
    // before the read must start where its own predecessor ends (frame 0 at 0) and end where
    // the read starts. [0, 0, 19] over two frames holds frame 0 twice, so frame 1 would read as
    // seq 2; [19, 38] over three puts frame 0 at 19. Each tail is clean, so recovery has
    // nothing to cut or list and does not walk journal.off; it checks the same two links back
    // from the last listed frame, so it refuses both too.
    for (entries, log_len, bad) in [(&[0, 0, 19][..], 38, 0), (&[19, 38], 57, 19)] {
        let what = format!("journal.off {entries:?}, read from 19");
        let (_root, layout) = new_root();
        let journal = new_journal(&layout);
        journal.append_frame(&[compacted(1)]).unwrap(); // bytes 0..19
        journal.append_frame(&[compacted(2)]).unwrap(); // bytes 19..38
        journal.append_frame(&[compacted(3)]).unwrap(); // bytes 38..57
        set_len(layout.journal_log(), log_len);
        let off: Vec<u8> = entries.iter().flat_map(|e: &u64| e.to_le_bytes()).collect();
        fs::write(layout.journal_off(), &off).unwrap();
        assert_corrupt_at(reopen(&layout).tail_frames_from(19), bad, &what);
        assert_corrupt_at(journal.truncate_torn_tail(), bad, &what);
        assert_eq!(
            fs::read(layout.journal_off()).unwrap(),
            off,
            "{what}: nothing changed"
        );
    }
}

#[test]
fn zeroed_entries_at_the_end_of_journal_off_are_dropped_by_readers_too() {
    // A power loss between an append to journal.off and its sync may keep the file's length
    // and lose the entry's bytes: an entry pointing at 0, which no frame but the first starts
    // at. Recovery drops such entries at the end as it drops a partial entry; readers drop them
    // too, and read the frame such an entry would list as a whole frame not listed yet, as
    // they read one whose entry was never appended. `frame_count` counts what they list.
    for unlisted in [false, true] {
        let what = if unlisted {
            "with frame 2 in journal.log"
        } else {
            "with frame 2 missing"
        };
        let (_root, layout) = new_root();
        let journal = new_journal(&layout);
        journal.append_frame(&[root_changeset()]).unwrap(); // bytes 0..84
        journal
            .append_frame(&[child_changeset(), main_created()])
            .unwrap(); // bytes 84..257
        if unlisted {
            journal.append_frame(&[compacted(2)]).unwrap(); // bytes 257..276
            set_len(layout.journal_off(), 16);
        }
        append_bytes(layout.journal_off(), &[0; 8]);
        let starts = |frames: Vec<Frame>| -> Vec<(u64, u64)> {
            frames
                .iter()
                .map(|frame| (frame.seq, frame.offset))
                .collect()
        };
        let all: &[(u64, u64)] = if unlisted {
            &[(0, 0), (1, 84), (2, 257)]
        } else {
            &[(0, 0), (1, 84)]
        };
        let reader = reopen(&layout);
        assert_eq!(
            reader.frame_count().unwrap(),
            2,
            "{what}: the listed frames"
        );
        assert_eq!(starts(reader.tail_frames_from(0).unwrap()), all, "{what}");
        assert_eq!(
            starts(reader.tail_frames_from(84).unwrap()),
            all[1..],
            "{what}"
        );
        assert_eq!(
            starts(reader.frames_from(0, 10).unwrap()),
            [(0, 0), (1, 84)],
            "{what}: the listed frames"
        );
        assert_eq!(
            starts(reader.frames_after(0, 10).unwrap()),
            [(1, 84)],
            "{what}"
        );

        assert_eq!(journal.truncate_torn_tail().unwrap(), None, "{what}");
        let listed: Vec<u64> = all.iter().map(|&(_, offset)| offset).collect();
        assert_eq!(
            offsets_on_disk(&layout),
            listed,
            "{what}: dropped, then listed"
        );
        assert_eq!(starts(reader.frames_from(0, 10).unwrap()), all, "{what}");
        let count = u64::try_from(all.len()).unwrap();
        assert_eq!(reader.frame_count().unwrap(), count, "{what}: all listed");
    }

    // More than one, the last listed frame then frame 0 or frame 1, and a read from where
    // frame 0 ends: frame 1, listed or not.
    for (entries, listed) in [
        (&[0, 0][..], false),
        (&[0, 19, 0, 0], true),
        (&[0, 0, 0, 0], false),
    ] {
        let what = format!("journal.off {entries:?}, read from 19");
        let (_root, layout) = new_root();
        let journal = new_journal(&layout);
        journal.append_frame(&[compacted(1)]).unwrap(); // bytes 0..19
        let second = journal.append_frame(&[compacted(2)]).unwrap(); // bytes 19..38
        let off: Vec<u8> = entries.iter().flat_map(|e: &u64| e.to_le_bytes()).collect();
        fs::write(layout.journal_off(), off).unwrap();
        assert_eq!(
            reopen(&layout).tail_frames_from(19).unwrap(),
            slice::from_ref(&second),
            "{what}"
        );
        let first_two = if listed { 2 } else { 1 };
        assert_eq!(
            reopen(&layout).frames_from(0, 10).unwrap().len(),
            first_two,
            "{what}: the listed frames"
        );
        assert_eq!(
            reopen(&layout).frame_count().unwrap(),
            u64::try_from(first_two).unwrap(),
            "{what}: the listed frames"
        );
    }
}

#[test]
fn trailing_zeroed_entries_are_dropped_like_a_partial_entry() {
    // A zero-filled block at the end of journal.off holds entries pointing at 0. Past frame 0
    // no entry can be genuine, so recovery drops them as it drops a partial entry, and the
    // frame left last is then checked as the last listed frame is.
    let tails: [(&str, &[u8]); 3] = [
        ("a zeroed entry", &[0; 8]),
        ("two zeroed entries", &[0; 16]),
        ("two zeroed entries and part of a third", &[0; 19]),
    ];
    for (what, tail) in tails {
        let (_root, layout) = new_root();
        let journal = new_journal(&layout);
        journal.append_frame(&[root_changeset()]).unwrap(); // bytes 0..84
        journal
            .append_frame(&[child_changeset(), main_created()])
            .unwrap(); // bytes 84..257
        append_bytes(layout.journal_off(), tail);
        assert_eq!(journal.truncate_torn_tail().unwrap(), None, "{what}");
        assert_eq!(offsets_on_disk(&layout), [0, 84], "{what}: dropped");
        assert_eq!(
            file_len(layout.journal_log()),
            257,
            "{what}: journal.log kept"
        );
        assert_eq!(journal.truncate_torn_tail().unwrap(), None, "{what}: again");
        assert_eq!(
            reopen(&layout).tail_frames_from(0).unwrap().len(),
            2,
            "{what}"
        );
    }

    // Entries [0, 0, 0] over frame 0 alone: entry 0 is frame 0's, the others are dropped.
    let (_root, layout) = new_root();
    let journal = new_journal(&layout);
    journal.append_frame(&[root_changeset()]).unwrap(); // bytes 0..84
    append_bytes(layout.journal_off(), &[0; 16]);
    assert_eq!(journal.truncate_torn_tail().unwrap(), None);
    assert_eq!(offsets_on_disk(&layout), [0]);

    // The frame left last fails its CRC: damage, so nothing is dropped.
    let (_root, layout) = new_root();
    let journal = new_journal(&layout);
    journal.append_frame(&[root_changeset()]).unwrap(); // bytes 0..84
    journal
        .append_frame(&[child_changeset(), main_created()])
        .unwrap(); // bytes 84..257
    flip_byte(layout.journal_log(), 200); // a payload byte of frame 1
    append_bytes(layout.journal_off(), &[0; 8]);
    assert_corrupt_at(
        journal.truncate_torn_tail(),
        84,
        "frame 1 left last fails its CRC",
    );
    assert_eq!(offsets_on_disk(&layout), [0, 84, 0], "nothing was dropped");

    // Zeros in journal.log, then data: the zeros make a header of length 0, a torn tail
    // whatever follows it, and it is cut.
    let (_root, layout) = new_root();
    let journal = new_journal(&layout);
    journal.append_frame(&[root_changeset()]).unwrap(); // bytes 0..84
    append_bytes(
        layout.journal_log(),
        &[[0; 16].as_slice(), b"data"].concat(),
    );
    assert_eq!(
        journal.truncate_torn_tail().unwrap(),
        Some(84),
        "zeros, then data"
    );
    assert_eq!(file_len(layout.journal_log()), 84, "cut");
}

#[test]
fn zeroed_entries_past_a_4_kib_block_at_the_end_of_journal_off_are_journal_corrupt() {
    // A power loss leaves zeroed only the entry being written (storage spec §2 rule 3), so
    // readers and recovery drop at most a 4 KiB block of zeroed entries at the end of
    // journal.off, 512, as they drop a few in
    // `zeroed_entries_at_the_end_of_journal_off_are_dropped_by_readers_too` and
    // `trailing_zeroed_entries_are_dropped_like_a_partial_entry`. One more is no power loss's:
    // damage, JournalCorrupt at 0 (the offset such an entry holds) for every reader and for
    // recovery, which changes nothing. Part of an entry after them changes neither. Over two
    // frames, and over frame 0 alone, whose own entry holds 0.
    for (frames, zeroed) in [(2_u64, 512), (2, 513), (1, 512), (1, 513)] {
        let what = format!("{frames} frames, then {zeroed} zeroed entries and part of another");
        let (_root, layout) = new_root();
        let journal = new_journal(&layout);
        journal.append_frame(&[root_changeset()]).unwrap(); // bytes 0..84
        if frames == 2 {
            journal
                .append_frame(&[child_changeset(), main_created()])
                .unwrap(); // bytes 84..257
        }
        append_bytes(layout.journal_off(), &vec![0; zeroed * 8 + 3]);
        let (off_len, log_len) = (
            file_len(layout.journal_off()),
            file_len(layout.journal_log()),
        );
        let reader = reopen(&layout);
        if zeroed == 512 {
            let all = usize::try_from(frames).unwrap();
            assert_eq!(reader.frame_count().unwrap(), frames, "{what}");
            assert_eq!(reader.frames_from(0, 10).unwrap().len(), all, "{what}");
            assert_eq!(reader.tail_frames_from(0).unwrap().len(), all, "{what}");
            let repair = journal.repair_tail().unwrap();
            assert_eq!(repair.off_bytes_dropped, off_len - frames * 8, "{what}");
            assert_eq!(
                (
                    file_len(layout.journal_off()),
                    file_len(layout.journal_log())
                ),
                (frames * 8, log_len),
                "{what}: dropped, journal.log kept"
            );
        } else {
            assert_corrupt_at(reader.frame_count(), 0, &what);
            assert_corrupt_at(reader.frames_from(0, 10), 0, &what);
            assert_corrupt_at(reader.frames_after(0, 10), 0, &what);
            assert_corrupt_at(reader.tail_frames_from(0), 0, &what);
            assert_corrupt_at(reader.tail_frames_from(84), 0, &what);
            assert_corrupt_at(journal.repair_tail(), 0, &what);
            assert_corrupt_at(journal.append_frame(&[compacted(1)]), 0, &what);
            assert_eq!(
                (
                    file_len(layout.journal_off()),
                    file_len(layout.journal_log())
                ),
                (off_len, log_len),
                "{what}: nothing changed"
            );
        }
    }
}

#[test]
fn a_journal_off_zero_filled_far_past_its_entries_is_refused_at_once() {
    // A crafted or damaged journal.off of huge length (1 TiB here, a sparse file, which costs no
    // disk), zero-filled past the entries of the frames journal.log holds. Readers and recovery
    // look at no more than the last 4 KiB of journal.off to find where its zeroed entries
    // start, however long it is, and refuse this one as damage (more zeroed entries than a
    // power loss leaves: JournalCorrupt at 0) at once, changing nothing. Each call runs on a
    // handle of its own, on a thread with a deadline, so that a regression fails the test
    // instead of hanging it.
    use std::sync::mpsc;

    /// A call on a reader's or a writer's handle, its result dropped.
    type Call = fn(&Journal) -> Result<(), StoreError>;

    let (_root, layout) = new_root();
    let journal = new_journal(&layout);
    journal.append_frame(&[root_changeset()]).unwrap(); // bytes 0..84
    journal
        .append_frame(&[child_changeset(), main_created()])
        .unwrap(); // bytes 84..257
    drop(journal);
    set_len(layout.journal_off(), 1 << 40);
    let calls: [(&str, Call); 6] = [
        ("frame_count", |journal| journal.frame_count().map(drop)),
        ("frames_from", |journal| {
            journal.frames_from(0, 10).map(drop)
        }),
        ("frames_after", |journal| {
            journal.frames_after(1, 10).map(drop)
        }),
        ("tail_frames_from", |journal| {
            journal.tail_frames_from(0).map(drop)
        }),
        ("repair_tail", |journal| journal.repair_tail().map(drop)),
        ("append_frame", |journal| {
            journal.append_frame(&[compacted(1)]).map(drop)
        }),
    ];
    for (what, call) in calls {
        let (tx, rx) = mpsc::channel();
        let opened = layout.clone();
        thread::spawn(move || {
            let journal = Journal::open(&opened, real_fs()).unwrap();
            let _ = tx.send(call(&journal));
        });
        match rx.recv_timeout(Duration::from_secs(30)) {
            Ok(got) => assert_corrupt_at(got, 0, what),
            Err(e) => {
                panic!("{what} on a 1 TiB zero-filled journal.off did not return in 30 s: {e}")
            }
        }
    }
    assert_eq!(
        (
            file_len(layout.journal_off()),
            file_len(layout.journal_log())
        ),
        (1 << 40, 257),
        "nothing changed"
    );
}

#[test]
fn recovery_checks_every_listed_frame_before_it_writes() {
    // Recovery reads only the last listed frames to find the tail, but before it cuts
    // journal.log or lists a frame it walks journal.off whole: every listed frame must start
    // where the one before it ends (frame 0 at 0) and read whole. Garbage entries could
    // otherwise make it cut a committed frame, or list a frame under a wrong seq.

    // Crafted entries [0, 84, 116] over one 148-byte frame, whose parents hold a header at 84
    // announcing a frame that ends at 116, and at 116 a whole 19-byte frame, then a header cut
    // short at 135: the tail check alone would cut frame 0 there.
    let (_root, layout) = new_root();
    let journal = new_journal(&layout);
    let mut fake_header = [0_u8; 32];
    fake_header[0] = 24;
    let mut embedded = [0_u8; 32];
    embedded[..19].copy_from_slice(&frame_bytes(
        &[&[1, 0][..], &encoded(&compacted(5))].concat(),
    ));
    embedded[19..23].copy_from_slice(&[0xff, 0xff, 0xff, 0x7f]);
    journal
        .append_frame(&[Record::Changeset {
            id: id(0x11),
            generation: 1,
            delta_hash: [0xdd; 32],
            parents: vec![
                ChangesetId(ObjectId(fake_header)),
                ChangesetId(ObjectId(embedded)),
            ],
        }])
        .unwrap(); // bytes 0..148, its parents at 84..148
    append_bytes(layout.journal_off(), &84_u64.to_le_bytes());
    append_bytes(layout.journal_off(), &116_u64.to_le_bytes());
    assert_corrupt_at(
        journal.truncate_torn_tail(),
        84,
        "frame 0 ends at 148, not 84",
    );
    assert_eq!(file_len(layout.journal_log()), 148, "nothing was cut");
    assert_eq!(offsets_on_disk(&layout), [0, 84, 116], "nothing was listed");

    // Stale entries [0, 19, 0, 19]: frames 0 and 1 listed twice. With a torn tail to cut, or a
    // whole unlisted frame to list, entry 2 is found not to start where frame 1 ends (38).
    let unlisted = frame_bytes(&[&[1, 0][..], &encoded(&compacted(3))].concat());
    for (what, tail) in [
        ("a torn tail", b"garbage".to_vec()),
        ("a whole frame", unlisted),
    ] {
        let (_root, layout) = new_root();
        let journal = new_journal(&layout);
        journal.append_frame(&[compacted(1)]).unwrap(); // bytes 0..19
        journal.append_frame(&[compacted(2)]).unwrap(); // bytes 19..38
        append_bytes(layout.journal_off(), &0_u64.to_le_bytes());
        append_bytes(layout.journal_off(), &19_u64.to_le_bytes());
        append_bytes(layout.journal_log(), &tail);
        assert_corrupt_at(journal.truncate_torn_tail(), 0, what);
        assert_eq!(
            file_len(layout.journal_log()),
            38 + u64::try_from(tail.len()).unwrap(),
            "{what}: nothing was cut"
        );
        assert_eq!(
            offsets_on_disk(&layout),
            [0, 19, 0, 19],
            "{what}: nothing was listed"
        );
    }
}

#[test]
fn an_offset_entry_off_a_frame_boundary_is_journal_corrupt() {
    let (_root, layout) = new_root();
    let journal = new_journal(&layout);
    journal.append_frame(&[root_changeset()]).unwrap();
    // An entry that points into frame 0, whose bytes there read as a header announcing
    // far more than the file holds: dropping it and cutting there would destroy frame 0.
    append_bytes(layout.journal_off(), &50_u64.to_le_bytes());

    assert_corrupt_at(journal.truncate_torn_tail(), 50, "truncate_torn_tail");
    assert_eq!(file_len(layout.journal_log()), 84, "nothing was cut");
    assert_eq!(offsets_on_disk(&layout), [0, 50]);
    assert_corrupt_at(reopen(&layout).frames_after(0, 10), 50, "frames_after");
}

#[test]
fn offset_entries_out_of_order_are_journal_corrupt_not_a_panic() {
    // A third entry that does not start where frame 1 ends (257): one inside frame 0 (50),
    // and frame 1's entry again (84), which points at a whole frame, so only its place in
    // journal.off gives it away. (A zeroed one is what a zero-filled block leaves, which
    // recovery drops: `trailing_zeroed_entries_are_dropped_like_a_partial_entry`.)
    for bad in [50_u64, 84] {
        let what = format!("third entry {bad}");
        let (_root, layout) = new_root();
        let journal = new_journal(&layout);
        journal.append_frame(&[root_changeset()]).unwrap();
        journal
            .append_frame(&[child_changeset(), main_created()])
            .unwrap();
        append_bytes(layout.journal_off(), &bad.to_le_bytes());

        // Writers refuse it, so no frame is listed or numbered after a bad entry.
        assert_corrupt_at(journal.truncate_torn_tail(), bad, &what);
        assert_corrupt_at(journal.append_frame(&[compacted(9)]), bad, &what);
        assert_eq!(
            file_len(layout.journal_log()),
            257,
            "{what}: nothing written"
        );
        assert_eq!(offsets_on_disk(&layout), [0, 84, bad], "{what}");

        let reader = reopen(&layout);
        assert_corrupt_at(reader.frames_after(0, 1), 84, &what); // frame 1 cannot end there
        assert_corrupt_at(reader.frames_after(0, 10), bad, &what);
        assert_corrupt_at(reader.frames_after(1, 10), bad, &what);
    }

    // A zeroed third entry lists nothing: readers drop it, as recovery does, since no frame
    // but the first starts at 0 (see
    // `zeroed_entries_at_the_end_of_journal_off_are_dropped_by_readers_too`).
    let what = "third entry 0";
    let (_root, layout) = new_root();
    let journal = new_journal(&layout);
    journal.append_frame(&[root_changeset()]).unwrap();
    let second = journal
        .append_frame(&[child_changeset(), main_created()])
        .unwrap();
    append_bytes(layout.journal_off(), &0_u64.to_le_bytes());
    let reader = reopen(&layout);
    assert_eq!(
        reader.frames_after(0, 1).unwrap(),
        slice::from_ref(&second),
        "{what}"
    );
    assert_eq!(
        reader.frames_after(0, 10).unwrap(),
        slice::from_ref(&second),
        "{what}"
    );
    assert_eq!(
        reader.frames_after(1, 10).unwrap(),
        Vec::<Frame>::new(),
        "{what}"
    );
    assert_eq!(journal.truncate_torn_tail().unwrap(), None, "{what}");
    assert_eq!(offsets_on_disk(&layout), [0, 84], "{what}: dropped");
}

#[test]
fn truncate_torn_tail_syncs_journal_log_before_listing_a_frame() {
    // A writer that died inside append_durable, after its write and before its sync, left
    // frame 1 whole in the page cache but maybe not on disk. Listing it unsynced would let
    // a power loss revert journal.log under an entry, after replay materialized the frame.
    let entry = 84_u64.to_le_bytes();
    let cases = [
        (
            "entry missing",
            &[][..],
            vec![
                "file_len journal.log",
                "file_len journal.off",
                "sync_data journal.log",
                "append_durable journal.off",
            ],
        ),
        (
            "entry partly written",
            &entry[..3],
            vec![
                "file_len journal.log",
                "file_len journal.off",
                "truncate journal.off",
                "sync_data journal.log",
                "append_durable journal.off",
            ],
        ),
    ];
    for (what, partial_entry, want) in cases {
        let (_root, layout) = new_root();
        let journal = new_journal(&layout);
        journal.append_frame(&[root_changeset()]).unwrap();
        journal.append_frame(&[compacted(1)]).unwrap();
        set_len(layout.journal_off(), 8);
        append_bytes(layout.journal_off(), partial_entry);

        let (recovering, calls) = recording_journal(&layout);
        assert_eq!(recovering.truncate_torn_tail().unwrap(), None, "{what}");
        assert_eq!(calls(), want, "{what}");
        assert_eq!(offsets_on_disk(&layout), [0, 84], "{what}");
    }

    // With nothing to list there is nothing to sync: a clean tail costs the two lengths, a
    // torn one the cut as well.
    let (_root, layout) = new_root();
    new_journal(&layout)
        .append_frame(&[root_changeset()])
        .unwrap();
    let (recovering, calls) = recording_journal(&layout);
    assert_eq!(recovering.truncate_torn_tail().unwrap(), None);
    assert_eq!(calls(), ["file_len journal.log", "file_len journal.off"]);
    append_bytes(layout.journal_log(), b"garbage");
    assert_eq!(recovering.truncate_torn_tail().unwrap(), Some(84));
    assert_eq!(
        calls(),
        [
            "file_len journal.log",
            "file_len journal.off",
            "truncate journal.log"
        ]
    );
}

#[test]
fn a_journal_cut_below_its_last_listed_frame_is_journal_corrupt() {
    // Only the last listed frame's entry can dangle: a journal.log that ends inside an
    // earlier listed frame has lost committed frames.
    let (_root, layout) = new_root();
    let journal = new_journal(&layout);
    journal.append_frame(&[root_changeset()]).unwrap();
    journal
        .append_frame(&[child_changeset(), main_created()])
        .unwrap();
    journal.append_frame(&[compacted(2)]).unwrap();
    set_len(layout.journal_log(), 104); // inside frame 1 (84..257); frame 2 started at 257

    let reader = reopen(&layout);
    assert_corrupt_at(reader.frames_after(0, 10), 84, "frames_after");
    assert_corrupt_at(reader.frames_after(1, 10), 257, "frame 2 lies past the end");
    assert_corrupt_at(reader.tail_frames_from(0), 84, "tail_frames_from");
    assert_corrupt_at(journal.truncate_torn_tail(), 257, "truncate_torn_tail");
    assert_eq!(file_len(layout.journal_log()), 104, "nothing was cut");
    assert_eq!(offsets_on_disk(&layout), [0, 84, 257]);
}

#[test]
fn append_frame_checks_the_tail_before_writing() {
    let (_root, layout) = new_root();
    let journal = new_journal(&layout);
    let first = journal.append_frame(&[root_changeset()]).unwrap();

    // Another writer crashed mid-append after `journal` last saw the end at 84: the torn
    // bytes are cut before the next frame goes in, never written over or after.
    append_bytes(layout.journal_log(), b"garbage");
    let second = journal.append_frame(&[compacted(1)]).unwrap();
    assert_eq!((second.seq, second.offset), (1, 84));

    // A handle that never ran truncate_torn_tail checks the tail as well.
    append_bytes(layout.journal_log(), b"garbage");
    let other = reopen(&layout);
    let third = other.append_frame(&[compacted(2)]).unwrap();
    assert_eq!((third.seq, third.offset), (2, 103));

    // `journal` is stale now; its next frame still lands at the real end, in order.
    let fourth = journal.append_frame(&[compacted(3)]).unwrap();
    assert_eq!((fourth.seq, fourth.offset), (3, 122));
    assert_eq!(journal.len(), 141);
    assert_eq!(offsets_on_disk(&layout), [0, 84, 103, 122]);
    assert_eq!(
        reopen(&layout).tail_frames_from(0).unwrap(),
        [first, second, third, fourth]
    );
}

/// [`Record::encode`] refuses a length its `u8` field cannot hold before it writes anything:
/// `out`, holding what was encoded before it, is left as it was, for a ref name, an actor and a
/// parent count alike.
#[test]
fn a_refused_record_leaves_the_encoding_untouched() {
    let too_long = "x".repeat(256);
    let refused = [
        Record::RefMove {
            name: too_long.clone(),
            old: None,
            new: Some(id(1)),
            actor: "worker/1".into(),
            ts: 0,
        },
        Record::RefMove {
            name: "main".into(),
            old: None,
            new: Some(id(1)),
            actor: too_long,
            ts: 0,
        },
        Record::Changeset {
            id: id(2),
            generation: 2,
            delta_hash: [0; 32],
            parents: vec![id(1); 256],
        },
    ];
    for record in refused {
        let mut out = vec![0xaa, 0xbb];
        let got = record.encode(&mut out);
        assert!(matches!(got, Err(StoreError::Limit { .. })), "{got:?}");
        assert_eq!(out, [0xaa, 0xbb], "{record:?}");
    }
}

#[test]
fn append_frame_refuses_records_the_layout_cannot_hold() {
    let (_root, layout) = new_root();
    let journal = new_journal(&layout);
    let too_long = "x".repeat(256);
    // Each with the limit it goes past, as (what, max, got); none for a call no request makes.
    let refused = [
        ("no records", vec![], None),
        (
            "a 256-byte ref name",
            vec![Record::RefMove {
                name: too_long.clone(),
                old: None,
                new: Some(id(1)),
                actor: "worker/1".into(),
                ts: 0,
            }],
            Some((LimitKind::RefNameBytes, 255, 256)),
        ),
        (
            "a 256-byte actor",
            vec![Record::RefMove {
                name: "main".into(),
                old: None,
                new: Some(id(1)),
                actor: too_long,
                ts: 0,
            }],
            Some((LimitKind::ActorBytes, 255, 256)),
        ),
        (
            "256 parents",
            vec![Record::Changeset {
                id: id(2),
                generation: 2,
                delta_hash: [0; 32],
                parents: vec![id(1); 256],
            }],
            Some((LimitKind::Parents, 255, 256)),
        ),
        (
            "65,536 records",
            vec![compacted(0); 65_536],
            Some((LimitKind::Records, 65_535, 65_536)),
        ),
    ];
    for (what, records, limit) in refused {
        let got = journal.append_frame(&records);
        match (&got, limit) {
            (Err(StoreError::BadCall(_)), None) => {}
            (Err(StoreError::Limit { what, max, got }), Some(limit))
                if (*what, *max, *got) == limit => {}
            _ => panic!("{what}: expected {limit:?} (None: BadCall), got {got:?}"),
        }
    }
    assert_eq!(file_len(layout.journal_log()), 0, "nothing was written");

    let longest = "x".repeat(255);
    let at_the_limits = [
        Record::RefMove {
            name: longest.clone(),
            old: None,
            new: None,
            actor: longest,
            ts: 0,
        },
        Record::Changeset {
            id: id(2),
            generation: 2,
            delta_hash: [0; 32],
            parents: vec![id(1); 255],
        },
    ];
    let frame = journal.append_frame(&at_the_limits).unwrap();
    assert_eq!(reopen(&layout).tail_frames_from(0).unwrap(), [frame]);
}

#[test]
fn ckpt_roundtrip_and_default_zero() {
    let (_root, layout) = new_root();
    let journal = new_journal(&layout);
    assert_eq!(
        journal.ckpt().unwrap(),
        0,
        "no checkpoint file: nothing materialized"
    );
    assert!(!layout.journal_ckpt().exists());

    journal.set_ckpt(84).unwrap();
    assert_eq!(journal.ckpt().unwrap(), 84);
    assert_eq!(
        fs::read(layout.journal_ckpt()).unwrap(),
        84_u64.to_le_bytes()
    );
    journal.set_ckpt(u64::MAX).unwrap();
    assert_eq!(reopen(&layout).ckpt().unwrap(), u64::MAX);
    assert_eq!(fs::read(layout.journal_ckpt()).unwrap(), [0xff; 8]);
    assert_eq!(
        names_in(layout.meta_dir()),
        ["journal.ckpt", "journal.log", "journal.off"],
        "set_ckpt leaves no temp file behind"
    );

    fs::write(layout.journal_ckpt(), [1, 2, 3]).unwrap();
    let got = journal.ckpt();
    assert!(
        matches!(&got, Err(StoreError::Io(e)) if e.kind() == io::ErrorKind::InvalidData),
        "a damaged checkpoint is an error, not a guess: {got:?}"
    );
}

#[test]
fn a_huge_checkpoint_file_is_malformed_not_read_whole() {
    // journal.ckpt holds 8 bytes; one of any other length is damage. A crafted one of huge
    // length (a sparse file, which costs no disk) must not be read into a buffer of that length:
    // the read is capped, and the error is the file's own, at once.
    let (_root, layout) = new_root();
    let journal = new_journal(&layout);
    journal.set_ckpt(84).unwrap();
    set_len(layout.journal_ckpt(), 1 << 30);
    let started = Instant::now();
    let got = journal.ckpt();
    let Err(StoreError::Io(e)) = &got else {
        panic!("a 1 GiB checkpoint file: expected InvalidData, got {got:?}");
    };
    assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");
    assert!(
        e.to_string()
            .contains("holds more than 8 bytes, not a u64le"),
        "{e}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "read whole: {:?}",
        started.elapsed()
    );
    set_len(layout.journal_ckpt(), 9);
    let got = journal.ckpt();
    assert!(
        matches!(&got, Err(StoreError::Io(e)) if e.to_string().contains("holds more than 8 bytes")),
        "{got:?}"
    );
    set_len(layout.journal_ckpt(), 3);
    let got = journal.ckpt();
    assert!(
        matches!(&got, Err(StoreError::Io(e)) if e.to_string().contains("holds 3 bytes, not a u64le")),
        "{got:?}"
    );
}

#[test]
fn real_fs_appends_truncates_and_replaces_durably() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("f");
    assert_eq!(
        RealFs.file_len(&file).unwrap(),
        0,
        "a missing file has length 0"
    );
    assert_eq!(
        RealFs.append_durable(&file, b"abc").unwrap(),
        0,
        "creates the file"
    );
    assert_eq!(
        RealFs.append_durable(&file, b"de").unwrap(),
        3,
        "where the bytes start"
    );
    assert_eq!(RealFs.append_durable(&file, b"").unwrap(), 5);
    assert_eq!(fs::read(&file).unwrap(), b"abcde");
    assert_eq!(RealFs.file_len(&file).unwrap(), 5);

    RealFs.truncate(&file, 2).unwrap();
    assert_eq!(fs::read(&file).unwrap(), b"ab");
    RealFs.sync_data(&file).unwrap();
    let missing = RealFs.sync_data(&dir.path().join("missing"));
    assert!(
        matches!(&missing, Err(e) if e.kind() == io::ErrorKind::NotFound),
        "{missing:?}"
    );

    RealFs.atomic_replace(&file, b"replaced").unwrap();
    assert_eq!(fs::read(&file).unwrap(), b"replaced");
    let created = dir.path().join("g");
    RealFs.atomic_replace(&created, b"new").unwrap();
    assert_eq!(fs::read(&created).unwrap(), b"new");
    assert_eq!(
        names_in(dir.path()),
        ["f", "g"],
        "no temp file is left behind"
    );
}

/// One writer transaction in a child process, through a `CrashAtFs` that aborts on its
/// k-th FsOps call, under the order checker: check the tail, append a frame, advance the
/// checkpoint.
#[cfg(feature = "test-hooks")]
fn crash_child(root: OsString) {
    let k = std::env::var(CRASH_AT_ENV).unwrap().parse().unwrap();
    let layout = Layout::new(Utf8PathBuf::from_path_buf(root.into()).unwrap());
    let journal = Journal::open(&layout, checked(CrashAtFs::new(RealFs, k))).unwrap();
    journal.truncate_torn_tail().unwrap();
    journal.append_frame(&[compacted(7)]).unwrap();
    journal.set_ckpt(journal.len()).unwrap();
}

/// Runs the test named `test` again as a child process that runs [`crash_child`] on
/// `layout` with `SWARMOPS_TEST_CRASH_AT=k`; it must die by SIGABRT, or exit cleanly if `k`
/// is past its last FsOps call, `calls`.
#[cfg(feature = "test-hooks")]
fn run_crash_child(test: &str, layout: &Layout, k: u64, calls: u64) {
    let child = run(this_test(&test_name!(test))
        .env(CRASH_CHILD_ROOT, layout.root())
        .env(CRASH_AT_ENV, k.to_string()));
    let ended_as_expected = if k <= calls {
        child.status.signal() == Some(SIGABRT)
    } else {
        child.status.success()
    };
    assert!(
        ended_as_expected,
        "k={k}: {:?}\n--- child stdout ---\n{}\n--- child stderr ---\n{}",
        child.status,
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr),
    );
}

/// `journal.log`, `journal.off` and the checkpoint as a crash left them.
#[cfg(feature = "test-hooks")]
fn on_disk(layout: &Layout) -> (u64, u64, u64) {
    (
        file_len(layout.journal_log()),
        file_len(layout.journal_off()),
        reopen(layout).ckpt().unwrap(),
    )
}

#[test]
#[cfg(feature = "test-hooks")]
fn crash_at_any_journal_write_recovers_to_whole_frames() {
    if let Some(root) = std::env::var_os(CRASH_CHILD_ROOT) {
        crash_child(root);
        return;
    }
    // The child's FsOps calls: 1 file_len(journal.log), 2 file_len(journal.off),
    // 3 truncate(journal.log) cutting the torn tail, 4 append_durable(journal.log) the
    // frame, 5 append_durable(journal.off) its entry, 6 atomic_replace(journal.ckpt);
    // k = 7 is past the last call. Per k: journal.log, journal.off and ckpt as the crash
    // left them, then what recovery cuts.
    let crash_points: [(u64, u64, u64, u64, Option<u64>); 7] = [
        (1, 91, 8, 0, Some(84)),
        (2, 91, 8, 0, Some(84)),
        (3, 91, 8, 0, Some(84)),
        (4, 84, 8, 0, None),
        (5, 103, 8, 0, None),
        (6, 103, 16, 0, None),
        (7, 103, 16, 103, None),
    ];
    for (k, log, off, ckpt, cut) in crash_points {
        let (_root, layout) = new_root();
        let first = new_journal(&layout)
            .append_frame(&[root_changeset()])
            .unwrap();
        append_bytes(layout.journal_log(), b"garbage"); // an earlier writer crashed mid-append

        run_crash_child(
            "crash_at_any_journal_write_recovers_to_whole_frames",
            &layout,
            k,
            6,
        );
        assert_eq!(on_disk(&layout), (log, off, ckpt), "k={k}: after the crash");

        // Recovery keeps exactly the whole frames: the child's frame once its bytes were
        // durable (k >= 5), even though its journal.off entry may be missing.
        let journal = reopen_checked(&layout);
        assert_eq!(journal.truncate_torn_tail().unwrap(), cut, "k={k}");
        let mut want = vec![first.clone()];
        if k >= 5 {
            want.push(Frame {
                seq: 1,
                offset: 84,
                records: vec![compacted(7)],
            });
        }
        assert_eq!(journal.frames_after(0, 10).unwrap(), want[1..], "k={k}");
        assert_eq!(journal.tail_frames_from(0).unwrap(), want, "k={k}");
    }
}

#[test]
#[cfg(feature = "test-hooks")]
fn crash_while_recovery_lists_a_frame_recovers_to_whole_frames() {
    if let Some(root) = std::env::var_os(CRASH_CHILD_ROOT) {
        crash_child(root);
        return;
    }
    // Frame 1 is whole but unlisted: its writer died between its two appends. The child's
    // FsOps calls: 1 file_len(journal.log), 2 file_len(journal.off), 3 sync_data(journal.log)
    // and 4 append_durable(journal.off) listing frame 1, then 5 append_durable(journal.log)
    // its own frame, 6 append_durable(journal.off) that frame's entry,
    // 7 atomic_replace(journal.ckpt); k = 8 is past the last call. Per k: journal.log,
    // journal.off and ckpt as the crash left them.
    let crash_points: [(u64, u64, u64, u64); 8] = [
        (1, 103, 8, 0),
        (2, 103, 8, 0),
        (3, 103, 8, 0),
        (4, 103, 8, 0),
        (5, 103, 16, 0),
        (6, 122, 16, 0),
        (7, 122, 24, 0),
        (8, 122, 24, 122),
    ];
    for (k, log, off, ckpt) in crash_points {
        let (_root, layout) = new_root();
        let journal = new_journal(&layout);
        let first = journal.append_frame(&[root_changeset()]).unwrap();
        let second = journal.append_frame(&[compacted(1)]).unwrap();
        set_len(layout.journal_off(), 8);

        run_crash_child(
            "crash_while_recovery_lists_a_frame_recovers_to_whole_frames",
            &layout,
            k,
            7,
        );
        assert_eq!(on_disk(&layout), (log, off, ckpt), "k={k}: after the crash");

        // Frame 1 was committed all along; the child's frame is once its bytes were
        // durable (k >= 6).
        let journal = reopen_checked(&layout);
        assert_eq!(journal.truncate_torn_tail().unwrap(), None, "k={k}");
        let mut want = vec![first.clone(), second.clone()];
        let mut offsets = vec![0, 84];
        if k >= 6 {
            want.push(Frame {
                seq: 2,
                offset: 103,
                records: vec![compacted(7)],
            });
            offsets.push(103);
        }
        assert_eq!(journal.tail_frames_from(0).unwrap(), want, "k={k}");
        assert_eq!(offsets_on_disk(&layout), offsets, "k={k}");
    }
}

/// The Linux fsync-failure model ([`FsyncModelFs`]) keeps lost what a failed sync lost: the
/// bytes are in the file (the page cache) for every reader, but not durable, and a later sync
/// that succeeds does not make them so: it makes the file's length durable, and bytes written
/// after them, the lost ones reading as what the disk held before (zeros past its end). A cut
/// below them ends the loss. What it says is durable is what its image holds.
#[test]
fn the_fsync_failure_model_keeps_lost_bytes_lost() {
    let (_root, layout) = new_root();
    fs::create_dir_all(layout.meta_dir()).unwrap();
    let path = layout.journal_log().into_std_path_buf();
    // The model's sync-class calls: 1 the first append, 2 the second (it fails), 3 a sync, 4 the
    // third append, 5 the cut, 6 the last append.
    let model = FsyncModelFs::new().failing(|call| call.n == 2);
    model.append_durable(&path, b"durable ").unwrap();
    let failed = model.append_durable(&path, b"lost").unwrap_err();
    assert!(failed.to_string().contains("injected"), "{failed}");
    assert_eq!(
        fs::read(&path).unwrap(),
        b"durable lost",
        "every reader sees it"
    );
    assert_eq!(model.durable(&path).unwrap(), b"durable ");
    model.sync_data(&path).unwrap();
    assert_eq!(
        model.durable(&path).unwrap(),
        b"durable \0\0\0\0",
        "a later sync makes the length durable, not the lost bytes: zeros reach the disk there"
    );
    assert!(model.is_durable(&path, 0, 8));
    assert!(!model.is_durable(&path, 8, 12));
    model.append_durable(&path, b" after").unwrap();
    assert_eq!(model.durable(&path).unwrap(), b"durable \0\0\0\0 after");
    assert!(model.is_durable(&path, 12, 18) && !model.is_durable(&path, 0, 18));
    model.truncate(&path, 8).unwrap();
    model.append_durable(&path, b"again").unwrap();
    assert_eq!(
        model.durable(&path).unwrap(),
        b"durable again",
        "the cut ended the loss"
    );
    assert!(model.is_durable(&path, 0, 13));
    assert_eq!(model.failed().len(), 1);
    assert_eq!(model.syncs(), 6);

    let image = tempfile::tempdir().unwrap();
    let image_root = image.path().join("store");
    model.write_durable_image(layout.root().as_std_path(), &image_root);
    assert_eq!(
        fs::read(image_root.join("meta/journal.log")).unwrap(),
        b"durable again"
    );
}

/// One run of the fsync-failure sweep on `layout`, every call through `fs`: the journal
/// created, then five transactions, each a tail check (as every writer's starts) and a frame of
/// one record (`Compacted` with the transaction's number), the checkpoint moved after the third.
/// Returns the records of each frame acknowledged: appended, or committed with a failure after
/// its commit point ([`StoreError::PostCommit`]). A transaction whose tail check or append failed
/// otherwise acknowledged nothing. `None` if the journal could not be created: nothing ran.
fn fsync_failure_run(layout: &Layout, fs: Arc<dyn FsOps>) -> Option<Vec<Vec<Record>>> {
    let journal = Journal::create(layout, fs).ok()?;
    let mut acked = Vec::new();
    for n in 0..5 {
        if journal.truncate_torn_tail().is_err() {
            continue;
        }
        let records = vec![compacted(n)];
        match journal.append_frame(&records) {
            Ok(_) | Err(StoreError::PostCommit { .. }) => acked.push(records),
            Err(_) => {}
        }
        if n == 2 {
            let _ = journal.set_ckpt(journal.len());
        }
    }
    Some(acked)
}

/// Each frame `journal.off` at `layout` lists, as the range of `journal.log` it takes, by its
/// header: what a reader without a crash takes as committed.
fn listed_frames(layout: &Layout) -> Vec<(u64, u64)> {
    let log = fs::read(layout.journal_log()).unwrap();
    offsets_on_disk(layout)
        .into_iter()
        .map(|start| {
            let at = usize::try_from(start).unwrap();
            let len = u32::from_le_bytes(log[at..at + 4].try_into().unwrap());
            (start, start + 8 + u64::from(len))
        })
        .collect()
}

/// Linux's fsync-failure semantics, modelled ([`FsyncModelFs`]): a sync that fails leaves the
/// bytes it should have made durable not durable for good, and a later sync succeeds without
/// them, Linux having reported the failed writeback once. With the k-th sync-class call of five
/// transactions failing, for every k:
///
/// - every frame `journal.off` lists is durable in `journal.log`: a frame whose fdatasync failed
///   is never listed after a later sync succeeds (the fsyncgate pattern);
/// - the journal a power loss would leave (the model's durable image) recovers, without
///   damage, to exactly the acknowledged frames in order: none of them is lost, and none is
///   there that was not acknowledged.
///
/// Every run, the dry run's included, is under the order checker ([`checked`]), outside the
/// model, so that it judges the failed syncs as real ones.
#[test]
fn fsync_failure_semantics_never_list_a_frame_not_on_disk() {
    let (_root, layout) = new_root();
    let dry = Arc::new(FsyncModelFs::new());
    let all = fsync_failure_run(&layout, checked(Arc::clone(&dry))).unwrap();
    assert_eq!(
        all.len(),
        5,
        "without a failure every frame is acknowledged"
    );
    let syncs = dry.syncs();
    assert!(
        syncs >= 14,
        "create, five frames of two syncs, a checkpoint: {syncs}"
    );
    for k in 1..=syncs {
        let (_root, layout) = new_root();
        let model = Arc::new(FsyncModelFs::failing_sync(k));
        let Some(acked) = fsync_failure_run(&layout, checked(Arc::clone(&model))) else {
            continue; // the journal's creation failed: no frame to judge
        };
        let failed = model.failed();
        let log = layout.journal_log().into_std_path_buf();
        for (start, end) in listed_frames(&layout) {
            assert!(
                model.is_durable(&log, start, end),
                "k={k}, {failed:?} failed: the frame at {start}..{end} is listed, but a power \
                 loss would take it"
            );
        }
        let image_dir = tempfile::tempdir().unwrap();
        let image =
            Layout::new(Utf8PathBuf::from_path_buf(image_dir.path().join("store")).unwrap());
        model.write_durable_image(layout.root().as_std_path(), image.root().as_std_path());
        let journal = Journal::open(&image, real_fs()).unwrap();
        if let Err(e) = journal.truncate_torn_tail() {
            panic!("k={k}, {failed:?} failed: the journal a power loss leaves is damaged: {e}");
        }
        let recovered: Vec<Vec<Record>> = journal
            .tail_frames_from(0)
            .unwrap()
            .into_iter()
            .map(|frame| frame.records)
            .collect();
        assert_eq!(
            recovered, acked,
            "k={k}, {failed:?} failed: after a power loss, the acknowledged frames"
        );
    }
}

/// The records of each frame the journal that a power loss would leave at `layout` recovers
/// to (the model's durable image, its tail checked as the next writer checks it), in order.
#[track_caller]
fn recovered_after_a_power_loss<F: FsOps>(
    layout: &Layout,
    model: &FsyncModelFs<F>,
) -> Vec<Vec<Record>> {
    let image_dir = tempfile::tempdir().unwrap();
    let image = Layout::new(Utf8PathBuf::from_path_buf(image_dir.path().join("store")).unwrap());
    model.write_durable_image(layout.root().as_std_path(), image.root().as_std_path());
    let journal = Journal::open(&image, real_fs()).unwrap();
    if let Err(e) = journal.truncate_torn_tail() {
        panic!("the journal a power loss leaves is damaged: {e}");
    }
    let frames = journal.tail_frames_from(0).unwrap();
    frames.into_iter().map(|frame| frame.records).collect()
}

/// Every frame `journal.off` at `layout` lists is durable, as `model` says.
#[track_caller]
fn assert_listed_frames_durable<F: FsOps>(layout: &Layout, model: &FsyncModelFs<F>) {
    let log = layout.journal_log().into_std_path_buf();
    for (start, end) in listed_frames(layout) {
        assert!(
            model.is_durable(&log, start, end),
            "the frame at {start}..{end} is listed, but a power loss would take it"
        );
    }
}

/// A tail check whose sync of `journal.log` fails, before it lists the whole frames that
/// `journal.off` does not list yet, writes those frames afresh (cut back off the journal,
/// appended again and synced) and lists them then: Linux reports a failed writeback once, so no
/// later sync could vouch for them, and the next tail check's sync, succeeding without them,
/// would list a frame that is not on disk (the fsyncgate pattern, through the tail check). Under
/// the Linux fsync-failure model ([`FsyncModelFs`]) and the order checker (W1), its first sync
/// of `journal.log` failing:
///
/// - a frame a crashed writer wrote and never synced is listed only once it is durable;
/// - a frame that is durable but not listed (its listing failed after its sync: committed, with
///   a warning, [`StoreError::PostCommit`]) is never cut: it is listed, and survives a power
///   loss with the frames before it.
#[test]
fn a_tail_check_whose_sync_fails_never_lists_a_frame_not_on_disk() {
    // The first sync_data of journal.log fails, and with `off_listing` the append_durable of
    // journal.off that lists frame 1 (the third: the empty one of create, frame 0's, frame 1's).
    let model = |off_listing: bool| {
        let (log_syncs, off_appends) = (AtomicU64::new(0), AtomicU64::new(0));
        Arc::new(FsyncModelFs::new().failing(move |call| {
            let nth = |count: &AtomicU64| count.fetch_add(1, Ordering::SeqCst) + 1;
            match (
                call.method,
                call.path.file_name().and_then(|name| name.to_str()),
            ) {
                (FsMethod::SyncData, Some("journal.log")) => nth(&log_syncs) == 1,
                (FsMethod::AppendDurable, Some("journal.off")) => {
                    off_listing && nth(&off_appends) == 3
                }
                _ => false,
            }
        }))
    };

    // A crashed writer's frame: whole in journal.log, never synced, not listed.
    let (_root, layout) = new_root();
    let model_fs = model(false);
    let journal = Journal::create(&layout, checked(Arc::clone(&model_fs))).unwrap();
    journal.append_frame(&[compacted(0)]).unwrap();
    let log = layout.journal_log().into_std_path_buf();
    let frame = frame_bytes(&[&[1, 0][..], &encoded(&compacted(1))].concat());
    model_fs.append(&log, &frame).unwrap();
    let first = journal.truncate_torn_tail(); // its sync fails
    journal.truncate_torn_tail().unwrap();
    assert_eq!(offsets_on_disk(&layout).len(), 2, "frame 1 is listed");
    assert_listed_frames_durable(&layout, &model_fs);
    assert_eq!(
        recovered_after_a_power_loss(&layout, &model_fs),
        [[compacted(0)], [compacted(1)]],
        "the frame listed survives a power loss"
    );
    assert_eq!(model_fs.failed().len(), 1, "{:?}", model_fs.failed());
    assert_eq!(
        first.unwrap(),
        None,
        "the tail check wrote the frame afresh"
    );

    // A frame committed with a warning: durable in journal.log, its listing failed.
    let (_root, layout) = new_root();
    let model_fs = model(true);
    let journal = Journal::create(&layout, checked(Arc::clone(&model_fs))).unwrap();
    journal.append_frame(&[compacted(0)]).unwrap();
    match journal.append_frame(&[compacted(1)]) {
        Err(StoreError::PostCommit { frame_seq: 1, .. }) => {}
        other => panic!("expected PostCommit at frame 1, got {other:?}"),
    }
    assert_eq!(offsets_on_disk(&layout).len(), 1, "frame 1 is not listed");
    let first = journal.truncate_torn_tail(); // its sync fails
    journal.truncate_torn_tail().unwrap();
    assert_eq!(offsets_on_disk(&layout).len(), 2, "frame 1 is listed");
    assert_listed_frames_durable(&layout, &model_fs);
    assert_eq!(
        recovered_after_a_power_loss(&layout, &model_fs),
        [[compacted(0)], [compacted(1)]],
        "the frame committed with a warning survives"
    );
    assert_eq!(model_fs.failed().len(), 2, "{:?}", model_fs.failed());
    assert_eq!(
        first.unwrap(),
        None,
        "the tail check wrote the frame afresh"
    );
}

/// A journal at `layout` on `fs`: frame 0 listed, then `unlisted` frames a crashed writer left
/// whole in `journal.log` (seqs 1 on, never synced, not listed: written raw through `model`),
/// then `torn` bytes after them. Returns the journal and where frame 1 starts.
fn crashed_with_unlisted_frames(
    layout: &Layout,
    fs: Arc<dyn FsOps>,
    model: &dyn FsOps,
    unlisted: u64,
    torn: &[u8],
) -> (Journal, u64) {
    let journal = Journal::create(layout, fs).unwrap();
    journal.append_frame(&[compacted(0)]).unwrap();
    let log = layout.journal_log().into_std_path_buf();
    let first = file_len(layout.journal_log());
    for n in 1..=unlisted {
        let frame = frame_bytes(&[&[1, 0][..], &encoded(&compacted(n))].concat());
        model.append(&log, &frame).unwrap();
    }
    model.append(&log, torn).unwrap();
    (journal, first)
}

/// What a writer that died in the middle of its append leaves: a header announcing 100 bytes,
/// and 10 of them.
fn torn_tail() -> Vec<u8> {
    [&100_u32.to_le_bytes()[..], &[0; 4], &[0xee; 10]].concat()
}

/// A tail check whose sync of the unlisted frames fails writes them afresh: it cuts
/// `journal.log` back to the first of them, which also cuts the torn tail behind them, and appends
/// them again. It reports the torn tail as cut, and cuts nothing more: one `truncate
/// journal.log`, the rewrite's (a second cut to the length the rewrite left, and its sync, did
/// nothing). The frame is listed, and survives a power loss.
#[test]
fn a_rewrite_after_a_failed_tail_sync_cuts_the_torn_tail_once() {
    let (_root, layout) = new_root();
    let (recording, calls) = RecordingFs::new();
    let log_syncs = AtomicU64::new(0);
    let model_fs = Arc::new(FsyncModelFs::over(recording).failing(move |call| {
        call.method == FsMethod::SyncData
            && call.path.file_name().and_then(|name| name.to_str()) == Some("journal.log")
            && log_syncs.fetch_add(1, Ordering::SeqCst) == 0
    }));
    let fs = checked(Arc::clone(&model_fs));
    let (journal, first) = crashed_with_unlisted_frames(&layout, fs, &*model_fs, 1, &torn_tail());
    let torn_at = file_len(layout.journal_log()) - 18;
    calls();
    let repair = journal.repair_tail().unwrap();
    assert_eq!(
        repair,
        TailRepair {
            cut: Some(LogCut {
                at: torn_at,
                bytes: 18
            }),
            frames_listed: 1,
            frames_rewritten: 1,
            off_bytes_dropped: 0,
        }
    );
    assert!(first < torn_at, "frame 1 lies before the torn tail");
    assert_eq!(
        file_len(layout.journal_log()),
        torn_at,
        "the torn tail is gone"
    );
    let made = calls();
    let cuts = made.iter().filter(|call| *call == "truncate journal.log");
    assert_eq!(cuts.count(), 1, "one cut, the rewrite's: {made:?}");
    assert_eq!(model_fs.failed().len(), 1, "{:?}", model_fs.failed());
    assert_listed_frames_durable(&layout, &model_fs);
    assert_eq!(
        recovered_after_a_power_loss(&layout, &model_fs),
        [[compacted(0)], [compacted(1)]],
    );
}

/// When the rewrite of the unlisted frames fails too (its append to `journal.log`), they are cut
/// back off the journal, the writer's own transaction did not commit, and the error names the
/// frames it cut by seq: here frames 1 to 2. `journal.log` ends where frame 1 started, and
/// nothing was listed.
#[test]
fn a_failed_rewrite_names_the_frames_it_cut() {
    let (_root, layout) = new_root();
    let armed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let failing = Arc::clone(&armed);
    let model_fs = Arc::new(FsyncModelFs::new().failing(move |call| {
        let log = call.path.file_name().and_then(|name| name.to_str()) == Some("journal.log");
        let syncs = call.method == FsMethod::SyncData || call.method == FsMethod::AppendDurable;
        failing.load(Ordering::SeqCst) && log && syncs
    }));
    let fs = checked(Arc::clone(&model_fs));
    let (journal, first) = crashed_with_unlisted_frames(&layout, fs, &*model_fs, 2, &[]);
    let listed = file_len(layout.journal_off());
    armed.store(true, Ordering::SeqCst);
    let failure = match journal.repair_tail() {
        Err(StoreError::NotCommitted { source }) => source,
        other => panic!("expected NotCommitted, got {other:?}"),
    };
    armed.store(false, Ordering::SeqCst);
    let text = failure.to_string();
    assert!(text.contains("frames 1..=2"), "{text}");
    assert_eq!(
        (
            file_len(layout.journal_log()),
            file_len(layout.journal_off())
        ),
        (first, listed),
        "cut back to frame 1's start, and nothing listed"
    );
    assert_eq!(model_fs.failed().len(), 2, "{:?}", model_fs.failed());
}

/// Writes `junk` to the end of `journal.log` right before the next `append_durable` of it, once
/// armed: another writer's bytes landing between a cut and an append.
struct Intrude {
    armed: std::sync::atomic::AtomicBool,
    junk: Vec<u8>,
}

impl Watch for Intrude {
    fn before(&self, call: &Call<'_>) -> io::Result<()> {
        if call.named() == "append_durable journal.log" && self.armed.swap(false, Ordering::SeqCst)
        {
            append_bytes(call.path(), &self.junk);
        }
        Ok(())
    }
}

/// A rewrite of the unlisted frames that does not land where they were (something wrote to
/// `journal.log` between the rewrite's cut and its append: a writer without the lock, or a
/// tail state gone wrong) is cut back to where they started and refused as
/// [`StoreError::JournalCorrupt`] there, before anything is listed: listed at the offsets the
/// check computed, the entries would point into other bytes.
#[test]
fn a_rewrite_that_lands_elsewhere_is_cut_back_and_reported_corrupt() {
    let (_root, layout) = new_root();
    let intrude = Arc::new(Watched(
        Intrude {
            armed: std::sync::atomic::AtomicBool::new(false),
            junk: vec![0xab; 7],
        },
        RealFs,
    ));
    let log_syncs = AtomicU64::new(0);
    let model_fs = Arc::new(
        FsyncModelFs::over(Arc::clone(&intrude)).failing(move |call| {
            call.method == FsMethod::SyncData
                && call.path.file_name().and_then(|name| name.to_str()) == Some("journal.log")
                && log_syncs.fetch_add(1, Ordering::SeqCst) == 0
        }),
    );
    let (journal, first) =
        crashed_with_unlisted_frames(&layout, Arc::clone(&model_fs) as _, &*model_fs, 1, &[]);
    let listed = file_len(layout.journal_off());
    intrude.0.armed.store(true, Ordering::SeqCst);
    match journal.repair_tail() {
        Err(StoreError::JournalCorrupt { offset }) => assert_eq!(offset, first),
        other => panic!("expected JournalCorrupt at {first}, got {other:?}"),
    }
    assert_eq!(
        (
            file_len(layout.journal_log()),
            file_len(layout.journal_off())
        ),
        (first, listed),
        "cut back to where the frames started, and nothing listed"
    );
}

/// Set only in the child of [`a_rewrite_that_lands_elsewhere_and_cannot_be_cut_back_aborts`]:
/// the store root.
#[cfg(feature = "test-hooks")]
const LANDING_CHILD_ROOT: &str = "SWARMOPS_TEST_JOURNAL_LANDING_ROOT";

/// [`Intrude`]'s bytes before the rewrite's append, once armed; then every cut of `journal.log`
/// fails: the cut that would take the misplaced rewrite back.
#[cfg(feature = "test-hooks")]
struct IntrudeThenFailTheCut {
    armed: std::sync::atomic::AtomicBool,
    intruded: std::sync::atomic::AtomicBool,
}

#[cfg(feature = "test-hooks")]
impl Watch for IntrudeThenFailTheCut {
    fn before(&self, call: &Call<'_>) -> io::Result<()> {
        let named = call.named();
        if named == "append_durable journal.log" && self.armed.swap(false, Ordering::SeqCst) {
            append_bytes(call.path(), &[0xab; 7]);
            self.intruded.store(true, Ordering::SeqCst);
        } else if named == "truncate journal.log" && self.intruded.load(Ordering::SeqCst) {
            return Err(io::Error::other("injected: the cut back fails"));
        }
        Ok(())
    }
}

/// The child of [`a_rewrite_that_lands_elsewhere_and_cannot_be_cut_back_aborts`]: the rewrite
/// of an unlisted frame whose sync failed lands past another writer's bytes, and the cut that
/// would take it back fails. It must not return.
#[cfg(feature = "test-hooks")]
fn landing_cut_fails_child(layout: &Layout) {
    let watch = Arc::new(Watched(
        IntrudeThenFailTheCut {
            armed: std::sync::atomic::AtomicBool::new(false),
            intruded: std::sync::atomic::AtomicBool::new(false),
        },
        RealFs,
    ));
    let log_syncs = AtomicU64::new(0);
    let model_fs = Arc::new(FsyncModelFs::over(Arc::clone(&watch)).failing(move |call| {
        call.method == FsMethod::SyncData
            && call.path.file_name().and_then(|name| name.to_str()) == Some("journal.log")
            && log_syncs.fetch_add(1, Ordering::SeqCst) == 0
    }));
    let (journal, _) =
        crashed_with_unlisted_frames(layout, Arc::clone(&model_fs) as _, &*model_fs, 1, &[]);
    watch.0.armed.store(true, Ordering::SeqCst);
    let returned = journal.repair_tail();
    println!("the tail check returned: {returned:?}");
}

/// A rewrite that lands anywhere but where the frames were
/// ([`a_rewrite_that_lands_elsewhere_is_cut_back_and_reported_corrupt`]) whose cut back fails
/// too aborts the process, saying why on stderr: entries at the offsets the check computed would
/// point into other bytes, and nothing here can take those bytes back, so that no later write of
/// the process may list them.
#[cfg(feature = "test-hooks")]
#[test]
fn a_rewrite_that_lands_elsewhere_and_cannot_be_cut_back_aborts() {
    const TEST: &str = "a_rewrite_that_lands_elsewhere_and_cannot_be_cut_back_aborts";
    if let Some(root) = std::env::var_os(LANDING_CHILD_ROOT) {
        landing_cut_fails_child(&Layout::new(
            Utf8PathBuf::from_path_buf(root.into()).unwrap(),
        ));
        return;
    }
    let (_root, layout) = new_root();
    let child = run(this_test(&test_name!(TEST)).env(LANDING_CHILD_ROOT, layout.root()));
    let stderr = String::from_utf8_lossy(&child.stderr);
    assert_eq!(
        child.status.signal(),
        Some(SIGABRT),
        "{:?}\n{stderr}\n{}",
        child.status,
        String::from_utf8_lossy(&child.stdout)
    );
    assert!(
        stderr.contains("landed at") && stderr.contains("could not be cut back off it"),
        "the abort says why: {stderr}"
    );
}

/// The lengths of `journal.log` and `journal.off` at `layout`.
fn journal_lens(layout: &Layout) -> (u64, u64) {
    (
        file_len(layout.journal_log()),
        file_len(layout.journal_off()),
    )
}

/// [`Journal::repair_tail`] on `journal`, checked against the files: `journal.log` loses exactly
/// the torn tail the report names, which ran to the file's end (a rewrite puts back the bytes it
/// cut); `journal.off` loses the bytes the report says it dropped, then gains an entry per frame
/// listed; only listed frames are written afresh; and a second call repairs nothing. Returns the
/// report.
#[track_caller]
fn repaired(layout: &Layout, journal: &Journal) -> TailRepair {
    let (log, off) = journal_lens(layout);
    let repair = journal.repair_tail().unwrap();
    if let Some(cut) = repair.cut {
        assert_eq!(
            cut.at + cut.bytes,
            log,
            "the cut runs to the end: {repair:?}"
        );
    }
    let cut = repair.cut.map_or(0, |cut| cut.bytes);
    let listed = 8 * repair.frames_listed;
    assert_eq!(
        journal_lens(layout),
        (log - cut, off - repair.off_bytes_dropped + listed),
        "{repair:?}"
    );
    assert!(
        repair.frames_rewritten <= repair.frames_listed,
        "{repair:?}"
    );
    assert_eq!(
        journal.repair_tail().unwrap(),
        TailRepair::default(),
        "again, after {repair:?}"
    );
    repair
}

/// What [`Journal::repair_tail`] reports is what it changed, and only that: the torn tail it cut
/// off `journal.log` (where it started, how many bytes), the whole frames `journal.off` did not
/// list that it listed, and of those the ones it wrote afresh first (the sync before their
/// listing having failed), and the bytes it dropped from the end of `journal.off` (part of an
/// entry, zeroed entries). A clean journal reports nothing. Every case keeps the identities
/// [`repaired`] checks.
#[test]
fn repair_tail_reports_exactly_what_it_changed() {
    /// What a writer that died in the middle of its append leaves: a header announcing 100
    /// bytes, and 10 of them.
    fn torn() -> Vec<u8> {
        [&100_u32.to_le_bytes()[..], &[0; 4], &[0xee; 10]].concat()
    }
    /// What a case does to a journal of two whole, listed frames.
    type Damage = fn(&Layout);

    // Frame 0 at 0..84, frame 1 at 84..257.
    let two_frames = |layout: &Layout| {
        let journal = new_journal(layout);
        journal.append_frame(&[root_changeset()]).unwrap();
        journal
            .append_frame(&[child_changeset(), main_created()])
            .unwrap();
        journal
    };
    let torn_at_257 = Some(LogCut { at: 257, bytes: 18 });
    let report = |cut, frames_listed, off_bytes_dropped| TailRepair {
        cut,
        frames_listed,
        frames_rewritten: 0,
        off_bytes_dropped,
    };
    let cases: [(&str, Damage, TailRepair); 7] = [
        ("a clean journal", |_| {}, TailRepair::default()),
        (
            "a torn tail",
            |layout| append_bytes(layout.journal_log(), &torn()),
            report(torn_at_257, 0, 0),
        ),
        (
            "a whole frame journal.off does not list (a crash between its two appends)",
            |layout| set_len(layout.journal_off(), 8),
            report(None, 1, 0),
        ),
        (
            "a whole frame not listed, then a torn tail",
            |layout| {
                set_len(layout.journal_off(), 8);
                append_bytes(layout.journal_log(), &torn());
            },
            report(torn_at_257, 1, 0),
        ),
        (
            "part of an entry",
            |layout| append_bytes(layout.journal_off(), &[0x54; 3]),
            report(None, 0, 3),
        ),
        (
            "frame 1's entry zeroed by a power loss, its length kept",
            |layout| {
                set_len(layout.journal_off(), 8);
                append_bytes(layout.journal_off(), &[0; 8]);
            },
            report(None, 1, 8),
        ),
        (
            "zeroed entries past the last frame, and part of another",
            |layout| append_bytes(layout.journal_off(), &[0; 19]),
            report(None, 0, 19),
        ),
    ];
    for (what, damage, want) in cases {
        let (_root, layout) = new_root();
        let journal = two_frames(&layout);
        damage(&layout);
        assert_eq!(repaired(&layout, &journal), want, "{what}");
    }

    // The tail check's sync of a crashed writer's frame fails: it writes the frame afresh, then
    // lists it, and cuts a torn tail after it all the same.
    for torn_tail in [false, true] {
        let (_root, layout) = new_root();
        let log_syncs = AtomicU64::new(0);
        let model = Arc::new(FsyncModelFs::new().failing(move |call| {
            call.method == "sync_data"
                && call.path.file_name().and_then(|name| name.to_str()) == Some("journal.log")
                && log_syncs.fetch_add(1, Ordering::SeqCst) == 0
        }));
        let journal = Journal::create(&layout, checked(Arc::clone(&model))).unwrap();
        journal.append_frame(&[root_changeset()]).unwrap(); // bytes 0..84
        let log = layout.journal_log().into_std_path_buf();
        let frame = frame_bytes(
            &[
                &[2, 0][..],
                &encoded(&child_changeset()),
                &encoded(&main_created()),
            ]
            .concat(),
        );
        model.append(&log, &frame).unwrap(); // bytes 84..257, never synced
        if torn_tail {
            model.append(&log, &torn()).unwrap();
        }
        let cut = torn_tail.then_some(LogCut { at: 257, bytes: 18 });
        let want = TailRepair {
            frames_rewritten: 1,
            ..report(cut, 1, 0)
        };
        assert_eq!(repaired(&layout, &journal), want, "torn tail: {torn_tail}");
        assert_eq!(model.failed().len(), 1, "{:?}", model.failed());
        assert_eq!(offsets_on_disk(&layout), [0, 84], "torn tail: {torn_tail}");
    }
}

#[test]
fn lock_times_out_as_busy() {
    let (_root, layout) = new_root();
    fs::create_dir_all(layout.meta_dir()).unwrap();
    let held = MetaLock::acquire(&layout, Duration::ZERO).unwrap();

    let waiter = {
        let layout = layout.clone();
        thread::spawn(move || {
            let started = Instant::now();
            let got = MetaLock::acquire(&layout, Duration::from_millis(200));
            (got, started.elapsed())
        })
    };
    let (got, elapsed) = waiter.join().unwrap();
    assert!(matches!(got, Err(StoreError::Busy)), "{got:?}");
    assert!(
        elapsed >= Duration::from_millis(200),
        "gave up after {elapsed:?}, before the timeout"
    );
    assert!(elapsed < Duration::from_secs(5), "waited {elapsed:?}");
    drop(held);
}

#[test]
fn lock_is_released_on_drop() {
    let (_root, layout) = new_root();
    fs::create_dir_all(layout.meta_dir()).unwrap();
    let first = MetaLock::acquire(&layout, Duration::ZERO).unwrap();
    assert!(layout.meta_lock().exists(), "acquire creates meta/lock");
    drop(first);
    let second = MetaLock::acquire(&layout, Duration::ZERO).expect("free again once dropped");

    // A writer waiting for the lock gets it once the holder drops it.
    let waiter = {
        let layout = layout.clone();
        thread::spawn(move || MetaLock::acquire(&layout, Duration::from_secs(10)))
    };
    thread::sleep(Duration::from_millis(50));
    drop(second);
    waiter
        .join()
        .unwrap()
        .expect("the waiter gets the lock after the drop");
}
