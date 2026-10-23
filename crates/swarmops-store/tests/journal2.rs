//! Tests of FORMAT 2 frames, offsets and checkpoint (the M6 design §2.2 to §2.4).
//!
//! Frame sizes below are worked out by hand from the layout: a FORMAT 2 frame is `24 (len, crc,
//! seq, prev_crc, hcrc) + 2 (record count) + its records`, so a frame of one `Compacted` record
//! (9 bytes) is 35; a `journal2.off` entry is 16 bytes, `u64le offset | u32le payload_crc |
//! u32le crc32(seq, offset, payload_crc)`; `journal2.ckpt` is 20, `u64le offset | u64le seq |
//! u32le crc32(the 16 before)`.

use std::fmt::Debug;
use std::fs;
use std::io;
use std::sync::Arc;

use swarmops_core::{ChangesetId, ObjectId};
use swarmops_store::{
    ChangesetMeta, FsOps, Journal, Layout, MetaStore, MetaWrite, Record, StoreError, StoreFormat,
};
use swarmops_testkit::fs::{NoSyncFs, RecordingFs};
use swarmops_testkit::store::{append_bytes, file_len, new_root, set_len};
use tempfile::TempDir;

/// The size of a FORMAT 2 frame header.
const HEADER: usize = 24;

/// The size of a `journal2.off` entry.
const ENTRY: usize = 16;

/// The size of a FORMAT 2 frame of one `Compacted` record.
const COMPACTED_FRAME: u64 = 35;

/// The largest payload a frame holds: a count, then 65,535 changesets of 255 parents each.
const MAX_PAYLOAD: u32 = 539_615_192;

/// The most zeroed entries at the end of `journal2.off` that recovery drops.
const MAX_ZEROED: u64 = 512;

/// A fresh FORMAT 2 store root.
fn v2_root() -> (TempDir, Layout) {
    let (dir, layout) = new_root();
    (dir, Layout::with_format(layout.root(), StoreFormat::V2))
}

/// A FORMAT 2 journal of `n` frames of one `Compacted` record each (`sorted_count` = its seq),
/// on a filesystem that never syncs; its tail checked.
fn journal_of(layout: &Layout, n: u64) -> Journal {
    let journal = Journal::create(layout, Arc::new(NoSyncFs)).unwrap();
    journal.repair_tail().unwrap();
    for seq in 0..n {
        journal.append_frame(&[compacted(seq)]).unwrap();
    }
    journal
}

/// Another handle on the journal, as another process would open it.
fn reopen(layout: &Layout) -> Journal {
    Journal::open(layout, Arc::new(NoSyncFs)).unwrap()
}

fn compacted(sorted_count: u64) -> Record {
    Record::Compacted { sorted_count }
}

/// The payload of a frame of `records`: its count, then each record.
fn payload(records: &[Record]) -> Vec<u8> {
    let mut bytes = u16::try_from(records.len()).unwrap().to_le_bytes().to_vec();
    for record in records {
        record.encode(&mut bytes).unwrap();
    }
    bytes
}

/// The bytes of a FORMAT 2 frame of `payload` at `seq`, after a frame whose payload CRC is
/// `prev_crc`, its header CRC correct.
fn frame2(payload: &[u8], seq: u64, prev_crc: u32) -> Vec<u8> {
    let len = u32::try_from(payload.len()).unwrap();
    header2(len, crc32fast::hash(payload), seq, prev_crc)
        .into_iter()
        .chain(payload.iter().copied())
        .collect()
}

/// A FORMAT 2 frame header, its CRC correct.
fn header2(len: u32, crc: u32, seq: u64, prev_crc: u32) -> Vec<u8> {
    let mut header = len.to_le_bytes().to_vec();
    header.extend(crc.to_le_bytes());
    header.extend(seq.to_le_bytes());
    header.extend(prev_crc.to_le_bytes());
    let hcrc = crc32fast::hash(&header);
    header.extend(hcrc.to_le_bytes());
    header
}

/// A `journal2.off` entry for frame `seq` at `offset` whose payload CRC is `crc`.
fn entry2(seq: u64, offset: u64, crc: u32) -> Vec<u8> {
    let mut entry = offset.to_le_bytes().to_vec();
    entry.extend(crc.to_le_bytes());
    let mut covered = seq.to_le_bytes().to_vec();
    covered.extend(&entry);
    entry.extend(crc32fast::hash(&covered).to_le_bytes());
    entry
}

/// The header fields of the frame at `at` of `log`: `(len, crc, seq, prev_crc, hcrc)`.
fn header_at(log: &[u8], at: usize) -> (u32, u32, u64, u32, u32) {
    let u32_at = |i: usize| u32::from_le_bytes(log[at + i..at + i + 4].try_into().unwrap());
    let seq = u64::from_le_bytes(log[at + 8..at + 16].try_into().unwrap());
    (u32_at(0), u32_at(4), seq, u32_at(16), u32_at(20))
}

/// The entries of `journal2.off`, as `(offset, payload_crc, ecrc)`.
fn entries_on_disk(layout: &Layout) -> Vec<(u64, u32, u32)> {
    let bytes = fs::read(layout.journal_off()).unwrap();
    let (entries, partial) = bytes.as_chunks::<ENTRY>();
    assert!(partial.is_empty(), "journal2.off ends in a partial entry");
    entries
        .iter()
        .map(|entry| {
            (
                u64::from_le_bytes(entry[..8].try_into().unwrap()),
                u32::from_le_bytes(entry[8..12].try_into().unwrap()),
                u32::from_le_bytes(entry[12..].try_into().unwrap()),
            )
        })
        .collect()
}

/// `n` as an index.
fn us(n: u64) -> usize {
    usize::try_from(n).unwrap()
}

/// Writes `bytes` over `path` at `at`.
fn patch(path: impl AsRef<std::path::Path>, at: usize, bytes: &[u8]) {
    let mut file = fs::read(&path).unwrap();
    file[at..at + bytes.len()].copy_from_slice(bytes);
    fs::write(&path, file).unwrap();
}

#[track_caller]
fn assert_corrupt_at<T: Debug>(got: Result<T, StoreError>, offset: u64, what: &str) {
    match got {
        Err(StoreError::JournalCorrupt { offset: at }) => assert_eq!(at, offset, "{what}"),
        other => panic!("{what}: expected JournalCorrupt at {offset}, got {other:?}"),
    }
}

/// Where damage lies, for what reads it. Recovery reads, of a journal whose tail is clean, the
/// last listed frame and the headers of the two before it (storage spec §2 rule 3), so damage
/// further back it finds only when it has something to cut or list; [`Journal::frames_from`]
/// reads the listed frames only.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Recovery {
    /// Among the listed frames, where recovery reads: every reader reports it where it is.
    Sees,
    /// After the listed frames: `frames_from` does not read it.
    SeesUnlisted,
    /// Further back: recovery finds it once there is an unlisted frame to list, reporting the
    /// first place it reads that does not agree, at the offset given (the last listed frame,
    /// which it checks first, may be that place).
    ListsAt(u64),
}

/// Every reader and recovery report the journal corrupt at `offset`, and recovery changes no
/// file. With [`Recovery::ListsAt`] recovery runs after the last entry is cut off (a frame it
/// must list, so it checks the listed frames first).
#[track_caller]
fn assert_all_corrupt_at(layout: &Layout, offset: u64, what: &str, recovery: Recovery) {
    let journal = reopen(layout);
    assert_corrupt_at(journal.tail_frames_from(0), offset, what);
    if recovery != Recovery::SeesUnlisted {
        assert_corrupt_at(journal.frames_from(0, usize::MAX), offset, what);
    }
    let mut recovered_at = offset;
    if let Recovery::ListsAt(at) = recovery {
        set_len(
            layout.journal_off(),
            file_len(layout.journal_off()) - ENTRY as u64,
        );
        recovered_at = at;
    }
    let before = (
        fs::read(layout.journal_log()).unwrap(),
        fs::read(layout.journal_off()).unwrap(),
    );
    assert_corrupt_at(reopen(layout).repair_tail(), recovered_at, what);
    let after = (
        fs::read(layout.journal_log()).unwrap(),
        fs::read(layout.journal_off()).unwrap(),
    );
    assert!(before == after, "{what}: recovery changed the journal");
}

/// What `call` returns, with the number of `read` system calls this thread made during it and
/// the bytes they returned (`syscr`, `rchar` of `/proc/thread-self/io`), less the read of that
/// file itself.
fn reading<T>(call: impl FnOnce() -> T) -> (T, u64, u64) {
    fn io_counts() -> (u64, u64, u64) {
        let text = fs::read_to_string("/proc/thread-self/io").unwrap();
        let field = |name: &str| -> u64 {
            (text.lines())
                .find_map(|line| line.strip_prefix(name))
                .and_then(|count| count.trim().parse().ok())
                .unwrap()
        };
        (field("syscr:"), field("rchar:"), text.len() as u64)
    }
    let (calls_before, bytes_before, _) = io_counts();
    let value = call();
    let (calls_after, bytes_after, own) = io_counts();
    // The read of the counts itself: read_to_string's reads, up to the end of the file.
    let own_calls = 2;
    (
        value,
        calls_after - calls_before - own_calls,
        bytes_after - bytes_before - own,
    )
}

/// A FORMAT 2 frame holds its records as FORMAT 1 does, behind a header naming its length, its
/// payload's CRC, its seq, the payload CRC of the frame before it (0 for frame 0) and the CRC of
/// those 20 bytes; each entry its offset, the frame's payload CRC and a CRC of both and the seq.
#[test]
fn frames_carry_their_seq_and_the_crc_of_the_frame_before() {
    let (_dir, layout) = v2_root();
    let journal = journal_of(&layout, 0);
    let records = [vec![compacted(7)], vec![compacted(8), compacted(9)]];
    for (seq, records) in (0..).zip(&records) {
        let frame = journal.append_frame(records).unwrap();
        assert_eq!(frame.seq, seq);
    }
    assert_eq!(layout.journal_log().file_name(), Some("journal2.log"));
    assert!(
        !layout.meta_dir().join("journal.log").exists(),
        "no FORMAT 1 file"
    );

    let log = fs::read(layout.journal_log()).unwrap();
    let (mut at, mut prev_crc) = (0, 0);
    for (seq, records) in (0..).zip(&records) {
        let payload = payload(records);
        let (len, crc, got_seq, got_prev, hcrc) = header_at(&log, at);
        assert_eq!(len as usize, payload.len(), "frame {seq}: len");
        assert_eq!(crc, crc32fast::hash(&payload), "frame {seq}: crc");
        assert_eq!(got_seq, seq, "frame {seq}: seq");
        assert_eq!(got_prev, prev_crc, "frame {seq}: prev_crc");
        assert_eq!(
            hcrc,
            crc32fast::hash(&log[at..at + 20]),
            "frame {seq}: hcrc"
        );
        assert_eq!(&log[at + HEADER..at + HEADER + payload.len()], payload);
        assert_eq!(
            log[at..at + HEADER + payload.len()],
            frame2(&payload, seq, prev_crc)
        );
        let entry = entries_on_disk(&layout)[usize::try_from(seq).unwrap()];
        assert_eq!(
            entry2(seq, at as u64, crc),
            [
                entry.0.to_le_bytes().as_slice(),
                &entry.1.to_le_bytes(),
                &entry.2.to_le_bytes()
            ]
            .concat()
        );
        at += HEADER + payload.len();
        prev_crc = crc;
    }
    assert_eq!(at, log.len());

    let frames = reopen(&layout).frames_from(0, 10).unwrap();
    assert_eq!(frames.len(), 2);
    assert_eq!((frames[1].seq, frames[1].offset), (1, COMPACTED_FRAME));
    assert_eq!(frames[1].records, records[1]);
    assert_eq!(reopen(&layout).tail_frames_from(0).unwrap(), frames);
    assert_eq!(reopen(&layout).frame_count().unwrap(), 2);
}

/// A header whose CRC matches but whose length is 0, or past the largest payload this build
/// decodes, is damage wherever it lies: never a torn tail, and refused from its header, with
/// none of the bytes after it read.
#[test]
fn a_header_announcing_no_frame_a_writer_writes_is_corrupt_and_its_payload_unread() {
    for len in [0, MAX_PAYLOAD + 1, u32::MAX] {
        let (_dir, layout) = v2_root();
        let first = journal_of(&layout, 1).tail_frames_from(0).unwrap()[0].clone();
        let prev = crc32fast::hash(&payload(&first.records));
        append_bytes(layout.journal_log(), &header2(len, 0, 1, prev));
        // 1 MiB after it, which no read may take.
        append_bytes(layout.journal_log(), &vec![0xa5; 1 << 20]);

        let journal = reopen(&layout);
        let (got, _calls, bytes) = reading(|| journal.tail_frames_from(0));
        assert_corrupt_at(got, COMPACTED_FRAME, &format!("len {len}"));
        assert!(bytes < 4096, "len {len}: the read took {bytes} bytes");
        let (got, _calls, bytes) = reading(|| journal.repair_tail());
        assert_corrupt_at(got, COMPACTED_FRAME, &format!("len {len}: recovery"));
        assert!(bytes < 4096, "len {len}: recovery took {bytes} bytes");
        assert_eq!(
            file_len(layout.journal_log()),
            COMPACTED_FRAME + 24 + (1 << 20)
        );
    }
}

/// A frame whose header checks but whose seq is not its place's, or whose previous CRC is not
/// the payload CRC of the frame before it, is a misplaced, stale or lost frame: damage at any
/// position, the last unlisted one included, never a torn tail to cut.
#[test]
fn misplaced_stale_and_lost_frames_are_corrupt_at_any_position() {
    let frame_at = |seq: u64| seq * COMPACTED_FRAME;
    let crc_of = |seq: u64| crc32fast::hash(&payload(&[compacted(seq)]));
    let bytes_of = |layout: &Layout, seq: u64| {
        let log = fs::read(layout.journal_log()).unwrap();
        let at = usize::try_from(frame_at(seq)).unwrap();
        log[at..at + us(COMPACTED_FRAME)].to_vec()
    };

    // Misplaced: frames 1 and 2 swapped (each is where the other's entry says).
    for (first, second) in [(0, 1), (1, 2), (2, 3)] {
        let (_dir, layout) = v2_root();
        journal_of(&layout, 4);
        let (a, b) = (bytes_of(&layout, first), bytes_of(&layout, second));
        patch(layout.journal_log(), us(frame_at(first)), &b);
        patch(layout.journal_log(), us(frame_at(second)), &a);
        let what = format!("{first} and {second} swapped");
        let recovery = Recovery::ListsAt(frame_at(2));
        assert_all_corrupt_at(&layout, frame_at(first), &what, recovery);
    }

    // Stale: a frame written again over a later one (its seq repeated), or after the last.
    for (stale, over) in [(0, 1), (1, 3)] {
        let (_dir, layout) = v2_root();
        journal_of(&layout, 4);
        let bytes = bytes_of(&layout, stale);
        patch(layout.journal_log(), us(frame_at(over)), &bytes);
        let what = format!("{stale} over {over}");
        let recovery = if over == 3 {
            Recovery::Sees
        } else {
            Recovery::ListsAt(frame_at(2))
        };
        assert_all_corrupt_at(&layout, frame_at(over), &what, recovery);
    }
    let (_dir, layout) = v2_root();
    journal_of(&layout, 4);
    append_bytes(layout.journal_log(), &bytes_of(&layout, 3));
    assert_all_corrupt_at(
        &layout,
        frame_at(4),
        "3 after the last",
        Recovery::SeesUnlisted,
    );

    // Lost: a frame missing, its successors and their entries in its place (a seq gap); the
    // next frame after the last listed one naming a gap or the wrong previous CRC.
    let (_dir, layout) = v2_root();
    journal_of(&layout, 4);
    let mut log = fs::read(layout.journal_log()).unwrap();
    log.drain(us(frame_at(1))..us(frame_at(2)));
    fs::write(layout.journal_log(), &log).unwrap();
    let mut off = Vec::new();
    for (place, seq) in [(0, 0), (1, 2), (2, 3)] {
        off.extend(entry2(place, frame_at(place), crc_of(seq)));
    }
    fs::write(layout.journal_off(), off).unwrap();
    assert_all_corrupt_at(
        &layout,
        frame_at(1),
        "frame 1 lost",
        Recovery::ListsAt(frame_at(1)),
    );
    for (seq, prev) in [(5, crc_of(3)), (4, crc_of(2)), (4, 0)] {
        let (_dir, layout) = v2_root();
        journal_of(&layout, 4);
        append_bytes(
            layout.journal_log(),
            &frame2(&payload(&[compacted(4)]), seq, prev),
        );
        let what = format!("seq {seq} after the last");
        assert_all_corrupt_at(&layout, frame_at(4), &what, Recovery::SeesUnlisted);
    }
}

/// A torn last frame is cut as in FORMAT 1: one cut short, one whose payload fails its CRC, one
/// whose header fails its own (zeros among them); readers skip it.
#[test]
fn a_torn_last_frame_is_cut_as_in_format_1() {
    let next = frame2(
        &payload(&[compacted(1)]),
        1,
        crc32fast::hash(&payload(&[compacted(0)])),
    );
    let mut bad_payload = next.clone();
    *bad_payload.last_mut().unwrap() ^= 1;
    let mut bad_header = next.clone();
    bad_header[9] ^= 1; // its seq
    let torn: [(&str, Vec<u8>); 6] = [
        ("half a header", next[..12].to_vec()),
        ("a header alone", next[..HEADER].to_vec()),
        ("half a payload", next[..HEADER + 5].to_vec()),
        ("a payload failing its crc", bad_payload),
        ("a header failing its crc", bad_header),
        ("zeros", vec![0; 4096]),
    ];
    for (what, tail) in torn {
        let (_dir, layout) = v2_root();
        journal_of(&layout, 1);
        append_bytes(layout.journal_log(), &tail);
        let journal = reopen(&layout);
        assert_eq!(journal.tail_frames_from(0).unwrap().len(), 1, "{what}");
        assert_eq!(journal.frames_from(0, 9).unwrap().len(), 1, "{what}");
        let repair = journal.repair_tail().unwrap();
        let cut = repair.cut.unwrap_or_else(|| panic!("{what}: no cut"));
        assert_eq!(
            (cut.at, cut.bytes),
            (COMPACTED_FRAME, tail.len() as u64),
            "{what}"
        );
        assert_eq!(file_len(layout.journal_log()), COMPACTED_FRAME, "{what}");
        // The journal takes the next frame where the torn one was.
        assert_eq!(
            journal.append_frame(&[compacted(1)]).unwrap().offset,
            COMPACTED_FRAME
        );
        assert_eq!(
            reopen(&layout).tail_frames_from(0).unwrap().len(),
            2,
            "{what}"
        );
    }
}

/// An unlisted whole frame is listed by recovery, its entry carrying its seq and payload CRC.
#[test]
fn recovery_lists_a_whole_unlisted_frame_with_its_checksum() {
    let (_dir, layout) = v2_root();
    journal_of(&layout, 3);
    set_len(layout.journal_off(), 2 * ENTRY as u64);
    let journal = reopen(&layout);
    assert_eq!(journal.tail_frames_from(0).unwrap().len(), 3);
    assert_eq!(journal.repair_tail().unwrap().frames_listed, 1);
    let crc = crc32fast::hash(&payload(&[compacted(2)]));
    let entries = entries_on_disk(&layout);
    let mut last = entries[2].0.to_le_bytes().to_vec();
    last.extend(entries[2].1.to_le_bytes());
    last.extend(entries[2].2.to_le_bytes());
    assert_eq!(last, entry2(2, 2 * COMPACTED_FRAME, crc));
}

/// Each entry's checksum covers its seq, its offset and the frame's payload CRC: a crafted
/// offset, an entry copied from another place, a payload CRC that is not its frame's, or a
/// zeroed entry before the end are damage for readers and recovery alike; zeroed entries at the
/// end are what a power loss leaves (512 at most), dropped and listed again.
#[test]
fn entry_checksums_catch_crafted_and_misplaced_entries() {
    let crc_of = |seq: u64| crc32fast::hash(&payload(&[compacted(seq)]));
    let entry_at = |seq: u64| usize::try_from(seq).unwrap() * ENTRY;

    // A crafted offset, its checksum left as it was: the entry no longer checks.
    let (_dir, layout) = v2_root();
    journal_of(&layout, 3);
    patch(
        layout.journal_off(),
        entry_at(1),
        &(2 * COMPACTED_FRAME).to_le_bytes(),
    );
    assert_all_corrupt_at(
        &layout,
        2 * COMPACTED_FRAME,
        "crafted offset",
        Recovery::ListsAt(2 * COMPACTED_FRAME),
    );

    // Entry 2 copied over entry 1: it checks only at seq 2.
    let (_dir, layout) = v2_root();
    journal_of(&layout, 3);
    let off = fs::read(layout.journal_off()).unwrap();
    patch(
        layout.journal_off(),
        entry_at(1),
        &off[entry_at(2)..entry_at(3)],
    );
    assert_all_corrupt_at(
        &layout,
        2 * COMPACTED_FRAME,
        "entry copied",
        Recovery::ListsAt(2 * COMPACTED_FRAME),
    );

    // An entry that checks but names a payload CRC its frame does not have.
    let (_dir, layout) = v2_root();
    journal_of(&layout, 3);
    patch(
        layout.journal_off(),
        entry_at(1),
        &entry2(1, COMPACTED_FRAME, crc_of(7)),
    );
    let what = "payload crc not its frame's";
    assert_all_corrupt_at(
        &layout,
        COMPACTED_FRAME,
        what,
        Recovery::ListsAt(COMPACTED_FRAME),
    );

    // An entry whose offset and payload CRC are its frame's, but whose own checksum is not.
    let (_dir, layout) = v2_root();
    journal_of(&layout, 3);
    let mut entry = entry2(1, COMPACTED_FRAME, crc_of(1));
    entry[ENTRY - 1] ^= 1;
    patch(layout.journal_off(), entry_at(1), &entry);
    let what = "entry checksum";
    assert_all_corrupt_at(
        &layout,
        COMPACTED_FRAME,
        what,
        Recovery::ListsAt(COMPACTED_FRAME),
    );

    // A zeroed entry before the end.
    let (_dir, layout) = v2_root();
    journal_of(&layout, 3);
    patch(layout.journal_off(), entry_at(1), &[0; ENTRY]);
    let journal = reopen(&layout);
    assert_corrupt_at(journal.frames_from(0, 9), 0, "zeroed entry mid-chain");
    assert_corrupt_at(journal.tail_frames_from(0), 0, "zeroed entry mid-chain");

    // Zeroed entries at the end: up to 512 dropped, the frames they would list listed again.
    for zeroed in [1, MAX_ZEROED] {
        let (_dir, layout) = v2_root();
        journal_of(&layout, 2);
        append_bytes(layout.journal_off(), &vec![0; ENTRY * (us(zeroed) - 1)]);
        patch(layout.journal_off(), entry_at(1), &[0; ENTRY]);
        let journal = reopen(&layout);
        assert_eq!(journal.frame_count().unwrap(), 1, "{zeroed} zeroed");
        assert_eq!(
            journal.tail_frames_from(0).unwrap().len(),
            2,
            "{zeroed} zeroed"
        );
        let repair = journal.repair_tail().unwrap();
        assert_eq!(repair.frames_listed, 1, "{zeroed} zeroed");
        assert_eq!(
            repair.off_bytes_dropped,
            zeroed * ENTRY as u64,
            "{zeroed} zeroed"
        );
        assert_eq!(entries_on_disk(&layout)[1].0, COMPACTED_FRAME);
    }
    let (_dir, layout) = v2_root();
    journal_of(&layout, 1);
    append_bytes(layout.journal_off(), &vec![0; ENTRY * (us(MAX_ZEROED) + 1)]);
    assert_all_corrupt_at(&layout, 0, "513 zeroed", Recovery::Sees);
}

/// Recovery checks the listed frames from the checkpoint's seq on, not from frame 0: each entry
/// carries its own checksum. Counted by this thread's `read` calls against a journal of 3,000
/// frames, ten past the checkpoint, the last of them unlisted.
#[test]
fn recovery_reads_only_the_entries_from_the_checkpoints_seq() {
    let (_dir, layout) = v2_root();
    let journal = journal_of(&layout, 2990);
    journal.set_ckpt(journal.len()).unwrap();
    for seq in 2990..3000 {
        journal.append_frame(&[compacted(seq)]).unwrap();
    }
    drop(journal);
    set_len(layout.journal_off(), 2999 * ENTRY as u64);

    let journal = reopen(&layout);
    let (repair, calls, _bytes) = reading(|| journal.repair_tail().unwrap());
    assert_eq!(repair.frames_listed, 1);
    assert!(calls < 100, "recovery made {calls} reads");
    assert_eq!(journal.frame_count().unwrap(), 3000);
    assert_eq!(journal.frames_from(2995, 9).unwrap().len(), 5);
}

/// The checkpoint holds the offset and the seq of the frame it stops before, behind its CRC:
/// absent it is 0; a file of another length or failing its CRC is malformed (`InvalidData`); an
/// offset its seq's entry does not hold is `JournalCorrupt`.
#[test]
fn the_checkpoint_holds_offset_and_seq_and_its_entry_must_agree() {
    let (_dir, layout) = v2_root();
    let journal = journal_of(&layout, 3);
    assert_eq!(layout.journal_ckpt().file_name(), Some("journal2.ckpt"));
    assert_eq!(journal.ckpt().unwrap(), 0);
    for (offset, seq) in [(0, 0), (COMPACTED_FRAME, 1), (3 * COMPACTED_FRAME, 3)] {
        journal.set_ckpt(offset).unwrap();
        assert_eq!(reopen(&layout).ckpt().unwrap(), offset);
        let mut want = offset.to_le_bytes().to_vec();
        want.extend(u64::to_le_bytes(seq));
        want.extend(crc32fast::hash(&want).to_le_bytes());
        assert_eq!(
            fs::read(layout.journal_ckpt()).unwrap(),
            want,
            "at {offset}"
        );
    }
    // A zeroed entry after the checkpoint's frame (the next frame's, which a power loss took)
    // lists nothing: the checkpoint at the end of the listed frames still agrees.
    append_bytes(layout.journal_off(), &[0; ENTRY]);
    assert_eq!(reopen(&layout).ckpt().unwrap(), 3 * COMPACTED_FRAME);
    set_len(layout.journal_off(), 3 * ENTRY as u64);

    // An offset that is no frame boundary journal2.off lists.
    assert!(matches!(
        journal.set_ckpt(COMPACTED_FRAME + 1),
        Err(StoreError::BadCall(_))
    ));

    let ckpt = |offset: u64, seq: u64| {
        let mut bytes = offset.to_le_bytes().to_vec();
        bytes.extend(seq.to_le_bytes());
        bytes.extend(crc32fast::hash(&bytes).to_le_bytes());
        bytes
    };
    let mismatched = [
        ("seq one past", ckpt(COMPACTED_FRAME, 2)),
        ("seq one before", ckpt(2 * COMPACTED_FRAME, 1)),
        ("seq past the entries", ckpt(3 * COMPACTED_FRAME, 4)),
        ("offset of no frame", ckpt(COMPACTED_FRAME + 1, 1)),
        ("seq 0 past 0", ckpt(COMPACTED_FRAME, 0)),
    ];
    for (what, bytes) in mismatched {
        fs::write(layout.journal_ckpt(), &bytes).unwrap();
        let offset = u64::from_le_bytes(bytes[..8].try_into().unwrap());
        assert_corrupt_at(reopen(&layout).ckpt(), offset, what);
    }
    let mut bad_crc = ckpt(COMPACTED_FRAME, 1);
    bad_crc[19] ^= 1;
    for (what, bytes) in [
        ("bad crc", bad_crc),
        ("8 bytes", COMPACTED_FRAME.to_le_bytes().to_vec()),
        ("21 bytes", vec![0; 21]),
    ] {
        fs::write(layout.journal_ckpt(), &bytes).unwrap();
        match reopen(&layout).ckpt() {
            Err(StoreError::Io(e)) => assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{what}"),
            other => panic!("{what}: {other:?}"),
        }
    }
}

/// The changeset `n`, a root, as `insert_changeset` takes it.
fn changeset(n: u64) -> ChangesetMeta {
    let mut id = [0; 32];
    id[..8].copy_from_slice(&n.to_le_bytes());
    id[31] = 0xc5;
    ChangesetMeta::new(
        ChangesetId::new(ObjectId::new(id)),
        0,
        0,
        [n.to_le_bytes()[0]; 32],
        Vec::new(),
    )
}

/// Commits changeset `n` in a transaction of its own.
fn commit(store: &MetaStore, n: u64) -> Result<(), StoreError> {
    let mut write = store.write()?;
    write.insert_changeset(changeset(n))?;
    write.commit().map(|_| ())
}

/// A transaction on a FORMAT 2 store makes the calls one on a FORMAT 1 store makes, its syncs
/// among them (journal 2 per transaction), on the files of its own format: create, a commit, a
/// commit that moves the checkpoint, a flush.
#[test]
fn a_transaction_makes_the_calls_of_format_1_on_format_2_files() {
    let calls_of = |format: StoreFormat| {
        let (_dir, layout) = new_root();
        let layout = Layout::with_format(layout.root(), format);
        let (fs, calls) = RecordingFs::new();
        let fs: Arc<dyn FsOps> = Arc::new(fs);
        let mut store = MetaStore::create_in(layout, fs).unwrap();
        store.set_parallelism(1);
        for n in 0..33 {
            commit(&store, n).unwrap();
        }
        store.flush().unwrap();
        let calls: Vec<String> = calls()
            .into_iter()
            .map(|call| {
                call.replace("journal2.", "journal.")
                    .replace("refs2", "refs")
            })
            .collect();
        calls
    };
    let (v1, v2) = (calls_of(StoreFormat::V1), calls_of(StoreFormat::V2));
    assert_eq!(v1, v2);
    let syncs = |calls: &[String], file: &str| {
        calls
            .iter()
            .filter(|call| call.ends_with(file) && call.starts_with("append_durable"))
            .count()
    };
    // The create's empty appends, then one per commit.
    assert_eq!(syncs(&v2, "journal.log"), 1 + 33);
    assert_eq!(syncs(&v2, "journal.off"), 1 + 33);
}

/// A FORMAT 2 store keeps what it committed across a reopen, its frames read past the
/// checkpoint and before it.
#[test]
fn a_format_2_store_reads_back_what_it_committed() {
    let (_dir, layout) = v2_root();
    let store = MetaStore::create_in(layout.clone(), Arc::new(NoSyncFs)).unwrap();
    for n in 0..40 {
        commit(&store, n).unwrap();
    }
    drop(store);
    // No FORMAT file: FORMAT 1 is assumed, whose journal is not there.
    let assumed = MetaStore::open_with_fs(layout.root(), Arc::new(NoSyncFs));
    assert!(matches!(assumed, Err(StoreError::Format(_))), "{assumed:?}");
    let store = MetaStore::open_in(layout.clone(), Arc::new(NoSyncFs)).unwrap();
    store.write().and_then(MetaWrite::commit).unwrap();
    let read = store.read().unwrap();
    assert_eq!(read.count_changesets().unwrap(), 40);
    for n in [0, 20, 39] {
        assert!(read.changeset(changeset(n).id).unwrap().is_some(), "{n}");
    }
    // 40 commits and the empty write's frame-less recovery: 40 frames, read past the checkpoint
    // (the last 8) and before it.
    assert_eq!(read.journal_after(None, 100).unwrap().len(), 40);
    assert_eq!(read.frames_before(None, 100).unwrap().len(), 40);
}

#[cfg(feature = "test-hooks")]
mod hooks {
    //! What needs the test hooks: the order checker on FORMAT 2 files, and the store-level
    //! crash and power-loss sweeps on a FORMAT 2 store.

    use std::ffi::OsString;
    use std::os::unix::process::ExitStatusExt;
    use std::panic::{self, AssertUnwindSafe};
    use std::path::Path;

    use camino::Utf8PathBuf;
    use swarmops_store::{CRASH_AT_ENV, CrashAtFs, OrderCheckFs, RealFs};
    use swarmops_testkit::child::{SIGABRT, run, this_test};
    use swarmops_testkit::faults::checked;
    use swarmops_testkit::power::{ImageRecordingFs, RecordedCall, Seeded, Timeline};
    use swarmops_testkit::test_name;

    use super::*;

    /// Set only in the children of the crash sweep: the store root.
    const CRASH_ROOT: &str = "SWARMOPS_TEST_JOURNAL2_CRASH_ROOT";

    /// The panic message of `run`, if it panics.
    fn panic_message(run: impl FnOnce()) -> Option<String> {
        let hook = panic::take_hook();
        panic::set_hook(Box::new(|_| {}));
        let result = panic::catch_unwind(AssertUnwindSafe(run));
        panic::set_hook(hook);
        let payload = result.err()?;
        Some(
            payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                .unwrap_or_default(),
        )
    }

    /// The order checker knows FORMAT 2's files under the rules it knows FORMAT 1's by: a whole
    /// store's life passes it, journal, entries of 16 bytes, checkpoint of 20 and all.
    #[test]
    fn a_format_2_store_passes_the_order_check() {
        let (_dir, layout) = v2_root();
        let mut store = MetaStore::create_in(layout.clone(), checked(RealFs::new())).unwrap();
        store.set_parallelism(1);
        for n in 0..33 {
            commit(&store, n).unwrap();
        }
        // Recovery after a crash between a frame's two appends lists the frame.
        drop(store);
        let off = file_len(layout.journal_off());
        set_len(layout.journal_off(), off - ENTRY as u64);
        let journal = Journal::open(&layout, checked(RealFs::new())).unwrap();
        assert_eq!(journal.repair_tail().unwrap().frames_listed, 1);
        let store = MetaStore::open_in(layout, checked(RealFs::new())).unwrap();
        store.flush().unwrap();
    }

    /// And it catches FORMAT 2 writes out of order: a frame listed before it is durable (W1), a
    /// checkpoint past the durable journal (W3).
    #[test]
    fn the_order_check_catches_format_2_writes_out_of_order() {
        let (_dir, layout) = v2_root();
        Journal::create(&layout, Arc::new(RealFs::new())).unwrap();
        let fs = OrderCheckFs::new(RealFs::new());
        let (log, off) = (layout.journal_log(), layout.journal_off());
        let frame = frame2(&payload(&[compacted(0)]), 0, 0);
        fs.append(log.as_std_path(), &frame).unwrap();
        let crc = crc32fast::hash(&payload(&[compacted(0)]));
        let listed = panic_message(|| {
            let _ = fs.append_durable(off.as_std_path(), &entry2(0, 0, crc));
        });
        let listed = listed.expect("listing a frame not durable panics");
        assert!(listed.starts_with("order check: W1 violated"), "{listed}");

        let mut ckpt = COMPACTED_FRAME.to_le_bytes().to_vec();
        ckpt.extend(1u64.to_le_bytes());
        ckpt.extend(crc32fast::hash(&ckpt).to_le_bytes());
        let moved = panic_message(|| {
            let _ = fs.atomic_replace(layout.journal_ckpt().as_std_path(), &ckpt);
        });
        let moved = moved.expect("a checkpoint past the durable journal panics");
        assert!(moved.starts_with("order check: W3 violated"), "{moved}");

        // Synced, the same frame is listed, and the checkpoint moves past it.
        fs.sync_data(log.as_std_path()).unwrap();
        fs.append_durable(off.as_std_path(), &entry2(0, 0, crc))
            .unwrap();
        fs.atomic_replace(layout.journal_ckpt().as_std_path(), &ckpt)
            .unwrap();
    }

    /// One transaction in a child process, on the FORMAT 2 store at `root`, through a
    /// `CrashAtFs` that aborts on its k-th call, under the order checker.
    fn crash_child(root: OsString) {
        let k = std::env::var(CRASH_AT_ENV).unwrap().parse().unwrap();
        let root = Utf8PathBuf::from_path_buf(root.into()).unwrap();
        let layout = Layout::with_format(&root, StoreFormat::V2);
        let fs = checked(CrashAtFs::new(NoSyncFs, k));
        let mut store = MetaStore::open_in(layout, fs).unwrap();
        store.set_parallelism(1);
        commit(&store, 31).unwrap();
    }

    /// A FORMAT 2 store of 31 changesets, the next commit moving the checkpoint, on a
    /// filesystem that never syncs (a process crash keeps the page cache).
    fn crash_template() -> (TempDir, Layout) {
        let (dir, layout) = v2_root();
        let store = MetaStore::create_in(layout.clone(), Arc::new(NoSyncFs)).unwrap();
        for n in 0..31 {
            commit(&store, n).unwrap();
        }
        append_bytes(layout.journal_log(), b"torn"); // an earlier writer crashed mid-append
        (dir, layout)
    }

    /// A crash at any call of a transaction that moves the checkpoint leaves a FORMAT 2 store
    /// that recovers to every changeset committed before it, the crashed one's once its frame
    /// was durable.
    #[test]
    fn a_crash_at_any_call_of_a_transaction_recovers() {
        if let Some(root) = std::env::var_os(CRASH_ROOT) {
            crash_child(root);
            return;
        }
        let test = "a_crash_at_any_call_of_a_transaction_recovers";
        let calls = {
            let (_dir, layout) = crash_template();
            let counter = Arc::new(CrashAtFs::new(NoSyncFs, 0));
            let mut store =
                MetaStore::open_in(layout, Arc::clone(&counter) as Arc<dyn FsOps>).unwrap();
            store.set_parallelism(1);
            commit(&store, 31).unwrap();
            counter.calls()
        };
        assert!(calls > 10, "{calls} calls");
        for k in 1..=calls + 1 {
            let (_dir, layout) = crash_template();
            let child = run(this_test(&test_name!(test))
                .env(CRASH_ROOT, layout.root())
                .env(CRASH_AT_ENV, k.to_string()));
            let ended = if k <= calls {
                child.status.signal() == Some(SIGABRT)
            } else {
                child.status.success()
            };
            assert!(
                ended,
                "k={k}: {:?}\n{}",
                child.status,
                String::from_utf8_lossy(&child.stderr)
            );
            let mut store = MetaStore::open_in(layout.clone(), checked(NoSyncFs)).unwrap();
            store.set_parallelism(1);
            store.write().and_then(MetaWrite::commit).unwrap();
            store.flush().unwrap();
            let read = store.read().unwrap();
            let count = read.count_changesets().unwrap();
            assert!(count == 31 || count == 32, "k={k}: {count} changesets");
            for n in 0..count {
                assert!(
                    read.changeset(changeset(n).id).unwrap().is_some(),
                    "k={k}: {n}"
                );
            }
            assert_eq!(
                Journal::open(&layout, Arc::new(NoSyncFs))
                    .unwrap()
                    .ckpt()
                    .unwrap(),
                file_len(layout.journal_log()),
                "k={k}"
            );
            // The next transaction commits.
            commit(&store, 40).unwrap();
        }
    }

    /// What the power-loss sweep records: a FORMAT 2 store made, then commits, each returned
    /// at the step its mark names.
    struct Recorded {
        timeline: Timeline,
        /// The step at which each commit returned.
        returned: Vec<usize>,
        /// The step of each commit's frame append.
        begun: Vec<usize>,
        /// The step at which the create returned.
        created: usize,
    }

    /// The commits the power-loss sweep makes; a flush after the third moves the checkpoint.
    const POWER_COMMITS: u64 = 5;

    fn record() -> (TempDir, Recorded) {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let root = Utf8PathBuf::from_path_buf(base.join("store")).unwrap();
        let layout = Layout::with_format(&root, StoreFormat::V2);
        let fs = Arc::new(ImageRecordingFs::new());
        // The store root made durable first, as a repository's init makes it.
        fs.create_dir_all(root.as_std_path()).unwrap();
        fs.fsync_dir(&base).unwrap();
        let mut store =
            MetaStore::create_in(layout.clone(), Arc::clone(&fs) as Arc<dyn FsOps>).unwrap();
        store.set_parallelism(1);
        fs.mark("created");
        for n in 0..POWER_COMMITS {
            commit(&store, n).unwrap();
            fs.mark(&format!("commit {n}"));
            if n == 2 {
                store.flush().unwrap();
            }
        }
        let calls = fs.calls();
        let timeline = Timeline::new(&base, &calls);
        let log = layout.journal_log().into_std_path_buf();
        let begun = (calls.iter().enumerate())
            .filter(|(_, call)| {
                matches!(call, RecordedCall::AppendDurable { path, bytes }
                    if *path == log && !bytes.is_empty())
            })
            .map(|(at, _)| timeline.steps_of(at).start)
            .collect();
        let returned = (0..POWER_COMMITS)
            .map(|n| timeline.mark(&format!("commit {n}")))
            .collect();
        let created = timeline.mark("created");
        (
            dir,
            Recorded {
                timeline,
                returned,
                begun,
                created,
            },
        )
    }

    /// The changesets a power-loss image of the store recovers to, or why it does not.
    fn recovered(base: &Path) -> Result<u64, String> {
        let root = Utf8PathBuf::from_path_buf(base.join("store")).unwrap();
        let layout = Layout::with_format(&root, StoreFormat::V2);
        let mut store = MetaStore::open_in(layout.clone(), checked(NoSyncFs))
            .map_err(|e| format!("open: {e}"))?;
        store.set_parallelism(1);
        (store.write().and_then(MetaWrite::commit)).map_err(|e| format!("recovery: {e}"))?;
        store.flush().map_err(|e| format!("flush: {e}"))?;
        let read = store.read().map_err(|e| format!("read: {e}"))?;
        let count = read.count_changesets().map_err(|e| format!("count: {e}"))?;
        for n in 0..count {
            if read
                .changeset(changeset(n).id)
                .map_err(|e| e.to_string())?
                .is_none()
            {
                return Err(format!("{count} changesets but {n} missing"));
            }
        }
        drop(read);
        commit(&store, 100).map_err(|e| format!("the next commit: {e}"))?;
        Ok(count)
    }

    /// Checks the images of a power failure after every step from the create's return on, for
    /// the seeds `seeds`: each recovers to a prefix of the commits holding every one that
    /// returned and none that had not begun.
    fn sweep(seeds: &[u64]) {
        let (_dir, rec) = record();
        let mut failures = Vec::new();
        for cut in rec.created..=rec.timeline.steps() {
            for &seed in seeds {
                let image = rec.timeline.image(cut, &mut Seeded::new(seed, cut));
                let dir = tempfile::tempdir().unwrap();
                image.materialize(dir.path()).unwrap();
                let acked = rec.returned.iter().filter(|&&at| at <= cut).count() as u64;
                let begun = rec.begun.iter().filter(|&&at| at < cut).count() as u64;
                match recovered(dir.path()) {
                    Ok(count) if (acked..=begun).contains(&count) => {}
                    Ok(count) => failures.push(format!(
                        "cut {cut}, seed {seed}: {count} changesets, {acked} acknowledged, \
                         {begun} begun"
                    )),
                    Err(why) => failures.push(format!("cut {cut}, seed {seed}: {why}")),
                }
            }
        }
        assert!(
            failures.is_empty(),
            "{} images failed; the first:\n{}",
            failures.len(),
            failures[..failures.len().min(5)].join("\n")
        );
    }

    /// The images of a power failure at every step: the least and the most the disk can hold
    /// (seeds 0 and 1), and random shapes of loss (2 to 15).
    #[test]
    fn power_loss_images_of_a_format_2_store_recover_every_acknowledged_commit() {
        sweep(&(0..16).collect::<Vec<u64>>());
    }
}
