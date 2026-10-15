//! Journal recovery and the journal readers never panic or hang on damaged files, and do what
//! the storage spec (§3) and its rulings say: recovery, under the lock, cuts exactly a torn tail
//! and lists exactly the whole frames that follow the listed ones, or reports damage and changes
//! nothing; readers return the frames on disk, stopping silently only where an append in
//! progress could leave the journal.
//!
//! A case is either the journal files verbatim (mode 0: `journal.log`, `journal.off` from `u64le`
//! entries, `journal.ckpt` if given), or a valid journal built with `Journal::create` and
//! `append_frame` from arbitrary records and then damaged by a patch (mode 1): bytes flipped, a
//! file cut, the last `journal.off` entry dropped (a crash between a frame's two appends),
//! garbage, zeros (blocks a filesystem exposed zero-filled after a crash) or a frame with a
//! correct CRC appended. Each case runs in a fresh `tempfile::tempdir()`. A typed error is
//! `JournalCorrupt`, or `Io` with `InvalidData`, `UnexpectedEof` or `InvalidInput`. A torn
//! tail is an unlisted frame cut short or failing its CRC, or unlisted bytes that are zero to
//! the end of `journal.log`. Readers measure `journal.off` before `journal.log`, so every frame
//! listed in their snapshot is whole in the `journal.log` they read: for them, as for recovery,
//! a listed frame cut short, missing or failing its CRC is damage.
//!
//! - (a) `open`, `frame_count` and `ckpt` succeed; `tail_frames_from` (from the checkpoint and
//!   from 0), `frames_from(0, 64)`, `frames_after(0, 64)` and `frames_from(0, usize::MAX)` return
//!   `Ok` or a typed error, before recovery and after it. A frame they return is the canonical
//!   encoding of its records at its offset (so its CRC matches), a listed one at its entry;
//!   frames returned together abut, readers agree where they overlap, and a reader stops early
//!   only at a torn tail, never before a listed frame. When `journal.off`'s entries are sound,
//!   readers from the start return exactly what the model says, before recovery and after it:
//!   the listed frames (and for `tail_frames_from(0)` the unlisted whole ones after them), or
//!   `JournalCorrupt` at the first listed frame that is not whole.
//! - (b) `truncate_torn_tail` does exactly what a model of recovery says: `JournalCorrupt` at
//!   the model's offset with no file changed, or `Ok` with the model's cut, `journal.log` the
//!   prefix up to it, and `journal.off` the whole entries it held, then one per frame listed.
//!   After an `Ok`, a second call returns `Ok(None)` and changes no file, and `journal.log` ends
//!   with the last listed frame, which reads whole (or is empty if nothing is listed).
//! - (c) So recovery only cuts a suffix, and only a torn tail: a listed frame, cut short or not,
//!   is never cut, nor is any listed entry dropped.
//! - (d) Mode 1: with the files undamaged, every appended frame reads back unchanged through
//!   every reader and recovery changes nothing. Damaged, the frames before the first damaged
//!   byte of `journal.log` still read back unchanged from the start, and through `frames_from`
//!   as far as their entries are undamaged.
//! - After a recovery, a new frame lands where `journal.log` ends and reads back.
//!
//! Every loop runs over the input's bytes, entries, frames or edits, so a case cannot hang.
#![no_main]

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use swarmops_core::{ChangesetId, ObjectId};
use swarmops_fuzz::{
    ENTRY_LEN, HEADER_LEN, Model, frame_around, frame_bytes, is_typed, model, to_u64, zero_filled,
};
use swarmops_store::{Frame, FsOps, Journal, Layout, LinkOutcome, RealFs, Record, StoreError};

/// The most frames mode 1 appends, and the most records in each, to keep a case quick.
const MAX_FRAMES: usize = 16;
const MAX_RECORDS: usize = 64;

/// The limit of the bounded reads.
const LIMIT: usize = 64;

#[derive(Arbitrary, Debug)]
struct Case {
    /// Even: mode 0, the files verbatim. Odd: mode 1, a valid journal and then a patch.
    mode: u8,
    /// Mode 0: `journal.log`.
    log: Vec<u8>,
    /// Mode 0: the entries of `journal.off`.
    off: Vec<u64>,
    /// Mode 0: `journal.ckpt`, no file if `None`. Mode 1: picks the frame boundary `set_ckpt`
    /// stores, no checkpoint if `None`.
    ckpt: Option<u64>,
    /// Mode 1: the frames appended, as their records; an empty one is skipped.
    frames: Vec<Vec<ArbRecord>>,
    /// Mode 1: the damage done to the files then, in order.
    patch: Vec<Edit>,
}

fuzz_target!(|case: Case| {
    let dir = tempfile::tempdir().expect("create a temp dir");
    let root = dir.path().to_str().expect("the temp dir path is UTF-8");
    let layout = Layout::new(root.to_owned());
    let ops: Arc<dyn FsOps> = Arc::new(NoSyncFs);
    if case.mode.is_multiple_of(2) {
        fs::create_dir(layout.meta_dir()).expect("create meta/");
        let files = Files {
            log: case.log,
            off: case
                .off
                .iter()
                .flat_map(|entry| entry.to_le_bytes())
                .collect(),
        };
        files.write(&layout);
        if let Some(ckpt) = case.ckpt {
            fs::write(layout.journal_ckpt(), ckpt.to_le_bytes()).expect("write journal.ckpt");
        }
        check(&layout, &ops, case.ckpt.unwrap_or(0), None);
    } else {
        let built = Built::new(&layout, &ops, case.frames, case.ckpt);
        let mut files = built.files.clone();
        for edit in case.patch {
            edit.apply(&mut files);
        }
        files.write(&layout);
        check(&layout, &ops, built.ckpt, Some(&built));
    }
});

/// Reads the journal in `layout`, recovers it, reads it again and appends to it, checking the
/// properties. `ckpt` is what `journal.ckpt` holds (0 if it is missing); `built` is the valid
/// journal a mode 1 case damaged.
fn check(layout: &Layout, ops: &Arc<dyn FsOps>, ckpt: u64, built: Option<&Built>) {
    let before = Files::read(layout);
    let journal = Journal::open(layout, Arc::clone(ops)).expect("open: the journal files exist");
    assert_eq!(journal.len(), to_u64(before.log.len()), "len() after open");
    let seen = Seen::read(&journal, &before, ckpt);
    if let Some(built) = built {
        built.reads_back(&journal, &before, &seen);
    }
    // With sound entries, the model says what the readers from the start return; a reader
    // races appends, so it must not fail at a last listed frame cut short.
    let listed = listed_frames(&before);
    let from_start = match &listed {
        Some(Ok(listed)) => Some(read_from_start(&before, listed)),
        Some(Err(damaged)) => Some(Err(*damaged)),
        None => None,
    };
    match &listed {
        Some(Ok(listed)) => {
            let first = |skip: usize| listed.iter().skip(skip).take(LIMIT).cloned().collect();
            agrees(
                "frames_from(0, usize::MAX)",
                &seen.listed,
                &Ok(listed.clone()),
            );
            agrees("frames_from(0, 64)", &seen.first, &Ok(first(0)));
            agrees("frames_after(0, 64)", &seen.after_first, &Ok(first(1)));
        }
        Some(Err(damaged)) => agrees("frames_from(0, usize::MAX)", &seen.listed, &Err(*damaged)),
        None => {}
    }
    if let Some(want) = &from_start {
        agrees("tail_frames_from(0)", &seen.from_start, want);
    }

    let recovery = Recovery::of(&before);
    let cut = match journal.truncate_torn_tail() {
        Ok(cut) => cut,
        Err(e) => {
            assert!(is_typed(&e), "truncate_torn_tail: an untyped error: {e:?}");
            assert!(
                Files::read(layout) == before,
                "truncate_torn_tail changed the files, then failed with {e:?}"
            );
            assert!(
                matches!((&recovery, &e), (Recovery::Corrupt { offset }, StoreError::JournalCorrupt { offset: at }) if at == offset),
                "truncate_torn_tail failed with {e:?}, the model says {recovery:?}"
            );
            return;
        }
    };
    let after = Files::read(layout);
    let Recovery::Clean { entries, cut: want } = &recovery else {
        panic!("truncate_torn_tail returned {cut:?}, the model says {recovery:?}");
    };
    assert_eq!(
        cut, *want,
        "truncate_torn_tail cut elsewhere than the model says"
    );
    let kept = cut.map_or(before.log.len(), to_usize);
    assert!(
        after.log == before.log[..kept],
        "journal.log after recovery is not its first {kept} bytes"
    );
    assert!(
        after.off.len() == entries.len() * ENTRY_LEN && after.entries() == *entries,
        "journal.off after recovery lists {:?}, the model says {entries:?}",
        after.entries()
    );
    let again = journal.truncate_torn_tail();
    assert!(
        matches!(again, Ok(None)),
        "truncate_torn_tail returned {cut:?}, then {again:?}"
    );
    assert!(
        Files::read(layout) == after,
        "a second truncate_torn_tail changed the files"
    );
    ends_cleanly(&journal, &after);
    let seen_after = Seen::read(&journal, &after, ckpt);
    if let Some(Ok(frames)) = &from_start {
        // Every frame that read is listed now, and nothing follows them.
        assert!(
            seen_after.listed.as_ref().ok() == Some(frames),
            "after recovery, frames_from(0, usize::MAX) returned {:?}, not {frames:?}",
            seen_after.listed
        );
        agrees(
            "tail_frames_from(0) after recovery",
            &seen_after.from_start,
            &Ok(frames.clone()),
        );
    }
    if let Some(built) = built
        && before == built.files
    {
        assert_eq!(cut, None, "recovery cut an undamaged journal");
        assert!(after == before, "recovery changed an undamaged journal");
    }
    appends_at_the_end(&journal, layout, &after);
}

/// What `truncate_torn_tail` does with `files`, by the model. Under the lock the files are
/// quiescent, so the last listed frame must lie within `journal.log`, start where the header of
/// the frame listed before it says that one ends (frame 0 at 0), and read whole: cut short,
/// missing, failing its CRC or not decoding, it is damage, as is a header before it announcing
/// an empty payload. After it, unlisted whole frames are listed up to the end of the file or a
/// torn tail, which is cut; an unlisted frame no writer produces is damage. A partial entry is
/// dropped.
#[derive(Debug, PartialEq, Eq)]
enum Recovery {
    /// Recovery succeeds: `journal.off` lists `entries` after it, and `journal.log` is cut at
    /// `cut`, if anywhere.
    Clean { entries: Vec<u64>, cut: Option<u64> },
    /// Recovery fails with `JournalCorrupt` at `offset`, and changes nothing.
    Corrupt { offset: u64 },
}

impl Recovery {
    fn of(files: &Files) -> Recovery {
        let log_len = to_u64(files.log.len());
        let mut entries = files.entries();
        let mut at = 0;
        if let Some(&start) = entries.last() {
            if start > log_len {
                return Recovery::Corrupt { offset: start };
            }
            let previous_end = match entries.len().checked_sub(2) {
                Some(previous) => match files.header_end(entries[previous]) {
                    Ok(end) => end,
                    Err(offset) => return Recovery::Corrupt { offset },
                },
                None => Some(0),
            };
            if previous_end != Some(start) {
                return Recovery::Corrupt { offset: start };
            }
            match files.frame_at(start) {
                Model::Whole { size, .. } => at = start + to_u64(size),
                _ => return Recovery::Corrupt { offset: start },
            }
        }
        // Each pass moves `at` forward by a whole frame or ends the loop.
        while at < log_len {
            match files.frame_at(at) {
                Model::Whole { size, .. } => {
                    entries.push(at);
                    at += to_u64(size);
                }
                Model::Invalid if !zero_filled(&files.log[to_usize(at)..]) => {
                    return Recovery::Corrupt { offset: at };
                }
                Model::Short | Model::BadCrc | Model::Invalid => {
                    return Recovery::Clean {
                        entries,
                        cut: Some(at),
                    };
                }
            }
        }
        Recovery::Clean { entries, cut: None }
    }
}

/// The listed frames as readers must return them, by the model, if `journal.off`'s entries are
/// sound: frame 0 at 0, each listed frame where the one before it ends. A reader measures
/// `journal.off` before `journal.log`, so every frame listed then is whole in the `journal.log`
/// it reads: `Ok` with them all, or `Err` with the start of the first that is not (cut short,
/// missing, failing its CRC or not decoding), where frames_from and tail_frames_from fail with
/// `JournalCorrupt`. `None` if the entries are not sound up to there.
fn listed_frames(files: &Files) -> Option<Result<Vec<Frame>, u64>> {
    let entries = files.entries();
    let mut frames = Vec::new();
    let mut next = 0;
    for (seq, &entry) in (0..).zip(&entries) {
        if entry != next {
            return None;
        }
        match files.frame_at(entry) {
            Model::Whole { records, size } => {
                frames.push(Frame {
                    seq,
                    offset: entry,
                    records,
                });
                next = entry + to_u64(size);
            }
            _ => return Some(Err(entry)),
        }
    }
    Some(Ok(frames))
}

/// What `tail_frames_from(0)` returns, by the model, given `listed`, every listed frame, whole,
/// from [`listed_frames`]: they, then the unlisted whole frames up to the end or a torn tail;
/// or `Err(offset)` for `JournalCorrupt` at an unlisted frame no writer produces.
fn read_from_start(files: &Files, listed: &[Frame]) -> Result<Vec<Frame>, u64> {
    let log_len = to_u64(files.log.len());
    let mut frames = listed.to_vec();
    let mut at = listed.last().map_or(0, |frame| {
        frame.offset + to_u64(frame_bytes(&frame.records).len())
    });
    // Each pass moves `at` forward by a whole frame or ends the loop.
    while at < log_len {
        match files.frame_at(at) {
            Model::Whole { records, size } => {
                frames.push(Frame {
                    seq: to_u64(frames.len()),
                    offset: at,
                    records,
                });
                at += to_u64(size);
            }
            Model::Invalid if !zero_filled(&files.log[to_usize(at)..]) => return Err(at),
            Model::Short | Model::BadCrc | Model::Invalid => break,
        }
    }
    Ok(frames)
}

/// Asserts that a reader returned what the model says: those frames, or `JournalCorrupt` at
/// that offset.
fn agrees(what: &str, got: &Result<Vec<Frame>, StoreError>, want: &Result<Vec<Frame>, u64>) {
    let agrees = match (got, want) {
        (Ok(read), Ok(frames)) => read == frames,
        (Err(StoreError::JournalCorrupt { offset }), Err(at)) => offset == at,
        _ => false,
    };
    assert!(agrees, "{what} returned {got:?}, the model says {want:?}");
}

/// What the readers returned for the files as they were.
struct Seen {
    /// `frames_from(0, usize::MAX)`: every listed frame, if they all read.
    listed: Result<Vec<Frame>, StoreError>,
    /// `tail_frames_from(0)`.
    from_start: Result<Vec<Frame>, StoreError>,
    /// `tail_frames_from(ckpt)`.
    from_ckpt: Result<Vec<Frame>, StoreError>,
    /// `frames_from(0, LIMIT)`.
    first: Result<Vec<Frame>, StoreError>,
    /// `frames_after(0, LIMIT)`.
    after_first: Result<Vec<Frame>, StoreError>,
}

impl Seen {
    /// Runs every reader once, checking each result and how they agree.
    fn read(journal: &Journal, files: &Files, ckpt: u64) -> Seen {
        let listed = to_u64(files.entries().len());
        assert_eq!(journal.frame_count().expect("frame_count"), listed);
        assert_eq!(
            journal
                .ckpt()
                .expect("ckpt: journal.ckpt is missing or holds 8 bytes"),
            ckpt
        );
        let seen = Seen {
            from_ckpt: tail_run(
                "tail_frames_from(ckpt)",
                journal.tail_frames_from(ckpt),
                ckpt,
                files,
            ),
            from_start: tail_run("tail_frames_from(0)", journal.tail_frames_from(0), 0, files),
            first: listed_run(
                "frames_from(0, 64)",
                journal.frames_from(0, LIMIT),
                0,
                LIMIT,
                files,
            ),
            after_first: listed_run(
                "frames_after(0, 64)",
                journal.frames_after(0, LIMIT),
                1,
                LIMIT,
                files,
            ),
            listed: listed_run(
                "frames_from(0, usize::MAX)",
                journal.frames_from(0, usize::MAX),
                0,
                usize::MAX,
                files,
            ),
        };
        // Where readers overlap, they return the same frames.
        if let Ok(start) = &seen.from_start {
            for (what, run) in [
                ("frames_from(0, 64)", &seen.first),
                ("frames_from(0, usize::MAX)", &seen.listed),
            ] {
                if let Ok(run) = run {
                    assert!(
                        start.starts_with(run),
                        "{what} disagrees with tail_frames_from(0)"
                    );
                }
            }
            if let Ok(after) = &seen.after_first {
                assert!(
                    start.get(1..).unwrap_or_default().starts_with(after),
                    "frames_after(0, 64) disagrees with tail_frames_from(0)"
                );
            }
            // A seq comes from the frame's place in journal.off, which garbage entries can
            // muddle; where each frame is and what it holds cannot differ.
            if let Ok(from_ckpt) = &seen.from_ckpt
                && let Some(i) = start.iter().position(|frame| frame.offset == ckpt)
            {
                assert!(
                    same_frames(from_ckpt, &start[i..]),
                    "tail_frames_from(ckpt) disagrees with tail_frames_from(0)"
                );
            }
        }
        if let (Ok(all), Ok(first)) = (&seen.listed, &seen.first) {
            assert!(
                all.starts_with(first),
                "frames_from(0, 64) disagrees with frames_from(0, usize::MAX)"
            );
        }
        seen
    }
}

/// Whether `a` and `b` hold the same frames at the same offsets, whatever their seqs.
fn same_frames(a: &[Frame], b: &[Frame]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(a, b)| a.offset == b.offset && a.records == b.records)
}

/// Checks what `tail_frames_from(from)` returned and passes it on. It is `Ok` or a typed error.
/// The frames start at `from` and abut, each the canonical encoding of its records, a listed
/// one at its entry; their seqs count up from `from`'s place in `journal.off` (or from the
/// listed count, if `from` is where the last listed frame ends). They stop at the end of
/// `journal.log` or at a torn tail (an unlisted frame cut short or failing its CRC, or unlisted
/// zeros to the end of the file), never before a listed frame: the reader measured
/// `journal.off` first, so every listed frame is whole in what it read.
fn tail_run(
    what: &str,
    got: Result<Vec<Frame>, StoreError>,
    from: u64,
    files: &Files,
) -> Result<Vec<Frame>, StoreError> {
    let frames = match &got {
        Ok(frames) => frames,
        Err(e) => {
            assert!(is_typed(e), "{what}: an untyped error: {e:?}");
            return got;
        }
    };
    let entries = files.entries();
    let listed = to_u64(entries.len());
    let mut at = from;
    for (i, frame) in frames.iter().enumerate() {
        assert_eq!(
            frame.offset, at,
            "{what}: frame {i} does not start where the one before ends"
        );
        if let Some(i) = i.checked_sub(1) {
            assert_eq!(frame.seq, frames[i].seq + 1, "{what}: seqs do not count up");
        }
        if let Some(&entry) = entries.get(to_usize(frame.seq)) {
            assert_eq!(
                entry, at,
                "{what}: listed frame {} read away from its entry",
                frame.seq
            );
        }
        at += canonical_size(what, frame, files);
    }
    // Every listed frame from `from` on was whole when the reader measured journal.log, so it
    // never stops before one.
    if let Some(next) = frames.last().map(|frame| frame.seq + 1) {
        assert!(
            next >= listed,
            "{what}: stopped at {at}, before listed frame {next} of {listed}"
        );
    }
    // An unlisted first frame takes the listed count as its seq, and with none read the read
    // started there too: where the last listed frame ends (0 with none listed).
    let unlisted_first = frames
        .first()
        .is_none_or(|first| entries.get(to_usize(first.seq)).is_none());
    if unlisted_first {
        if let Some(first) = frames.first() {
            assert_eq!(
                first.seq, listed,
                "{what}: seq {} is past the listed frames",
                first.seq
            );
        }
        let listed_end = entries
            .last()
            .map_or(Some(0), |&last| files.frame_end(last));
        assert_eq!(
            listed_end,
            Some(from),
            "{what}: read nothing listed from {from}, which is not where the listed frames end"
        );
        // And the last listed frame starts where the one listed before it ends (frame 0 at 0):
        // a zeroed entry there does not.
        if let Some((&last, before)) = entries.split_last() {
            let previous_end = before
                .last()
                .map_or(Some(0), |&previous| files.frame_end(previous));
            assert_eq!(
                previous_end,
                Some(last),
                "{what}: the last listed frame, at {last}, does not start where the one before it ends"
            );
        }
    }
    assert!(
        at <= to_u64(files.log.len()),
        "{what}: frames past the end of journal.log"
    );
    if at < to_u64(files.log.len()) {
        let next = frames.last().map(|frame| frame.seq + 1);
        match files.frame_at(at) {
            Model::Short => assert!(
                next.is_none_or(|seq| seq >= listed),
                "{what}: stopped at listed frame {next:?} of {listed}, which is cut short"
            ),
            Model::BadCrc => assert!(
                next.is_none_or(|seq| seq >= listed),
                "{what}: stopped at listed frame {next:?} of {listed}, which fails its CRC"
            ),
            Model::Invalid if zero_filled(&files.log[to_usize(at)..]) => assert!(
                next.is_none_or(|seq| seq >= listed),
                "{what}: stopped at listed frame {next:?} of {listed}, which is zeros"
            ),
            other => panic!("{what}: stopped at {at}, at a frame that reads as {other:?}"),
        }
    }
    got
}

/// Checks what `frames_from(first, limit)` returned and passes it on. It is `Ok` or a typed
/// error. The frames are the listed ones from seq `first` on, each at its entry and the
/// canonical encoding of its records; they abut, the first starts where the header of the frame
/// listed before it says that one ends, and the next entry (if one is listed) is where they end.
/// There are as many as are listed from `first` on, up to `limit`.
fn listed_run(
    what: &str,
    got: Result<Vec<Frame>, StoreError>,
    first: u64,
    limit: usize,
    files: &Files,
) -> Result<Vec<Frame>, StoreError> {
    let frames = match &got {
        Ok(frames) => frames,
        Err(e) => {
            assert!(is_typed(e), "{what}: an untyped error: {e:?}");
            return got;
        }
    };
    let entries = files.entries();
    let listed = to_u64(entries.len());
    let wanted = listed.saturating_sub(first).min(to_u64(limit));
    assert!(
        to_u64(frames.len()) <= wanted,
        "{what}: {} frames, more than {wanted}",
        frames.len()
    );
    let mut end = None;
    for (seq, frame) in (first..).zip(frames) {
        let entry = entries[to_usize(seq)];
        assert_eq!(
            (frame.seq, frame.offset),
            (seq, entry),
            "{what}: not the frame listed as {seq}"
        );
        if let Some(end) = end {
            assert_eq!(
                entry, end,
                "{what}: frame {seq} does not start where the one before ends"
            );
        } else if let Some(previous) = first.checked_sub(1) {
            let previous_end = files.frame_end(entries[to_usize(previous)]);
            assert_eq!(
                previous_end,
                Some(entry),
                "{what}: frame {seq} does not start where frame {previous} ends"
            );
        }
        end = Some(entry + canonical_size(what, frame, files));
    }
    let next = first + to_u64(frames.len());
    assert_eq!(
        to_u64(frames.len()),
        wanted,
        "{what}: {} frames, not the {wanted} listed and asked for",
        frames.len()
    );
    if let (Some(end), Some(&entry)) = (end, entries.get(to_usize(next))) {
        assert_eq!(
            entry, end,
            "{what}: the next listed frame does not start where these end"
        );
    }
    got
}

/// Asserts that `frame` sits in `journal.log` at its offset as the canonical encoding of its
/// records (its length, CRC, record count and records all as a writer writes them), and
/// returns its size.
fn canonical_size(what: &str, frame: &Frame, files: &Files) -> u64 {
    let bytes = frame_bytes(&frame.records);
    let at = to_usize(frame.offset);
    let on_disk = at
        .checked_add(bytes.len())
        .and_then(|end| files.log.get(at..end));
    assert!(
        on_disk == Some(&bytes[..]),
        "{what}: frame {} at {at} is not the canonical encoding of its records",
        frame.seq
    );
    to_u64(bytes.len())
}

/// After a successful recovery the journal ends cleanly: `journal.off` holds whole entries, the
/// last a frame that reads whole and ends where `journal.log` does; listing none, `journal.log`
/// is empty.
fn ends_cleanly(journal: &Journal, files: &Files) {
    assert_eq!(
        files.off.len() % ENTRY_LEN,
        0,
        "journal.off ends in part of an entry"
    );
    assert_eq!(
        journal.len(),
        to_u64(files.log.len()),
        "len() after recovery"
    );
    let Some(last) = to_u64(files.entries().len()).checked_sub(1) else {
        assert!(
            files.log.is_empty(),
            "nothing is listed, yet journal.log holds {} bytes",
            files.log.len()
        );
        return;
    };
    let read = listed_run(
        "frames_from(last, 1)",
        journal.frames_from(last, 1),
        last,
        1,
        files,
    );
    let frames = read.unwrap_or_else(|e| panic!("the last listed frame does not read: {e:?}"));
    let [frame] = frames.as_slice() else {
        panic!("the last listed frame is cut short");
    };
    let end = frame.offset + to_u64(frame_bytes(&frame.records).len());
    assert_eq!(
        end,
        to_u64(files.log.len()),
        "journal.log does not end with its last listed frame"
    );
}

/// After a recovery, a writer's next frame lands where `journal.log` ends, takes the next seq,
/// and reads back.
fn appends_at_the_end(journal: &Journal, layout: &Layout, files: &Files) {
    let frame = journal
        .append_frame(&[Record::Compacted { sorted_count: 7 }])
        .expect("append after a recovery");
    let seq = to_u64(files.entries().len());
    assert_eq!(
        (frame.seq, frame.offset),
        (seq, to_u64(files.log.len())),
        "the appended frame is not at the end"
    );
    let now = Files::read(layout);
    assert!(
        now.log.starts_with(&files.log) && now.off.starts_with(&files.off),
        "the append changed what was there"
    );
    let read = listed_run(
        "frames_from(seq, 1)",
        journal.frames_from(seq, 1),
        seq,
        1,
        &now,
    );
    assert_eq!(
        read.ok(),
        Some(vec![frame]),
        "the appended frame does not read back"
    );
}

/// The contents of `journal.log` and `journal.off`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Files {
    log: Vec<u8>,
    off: Vec<u8>,
}

impl Files {
    fn read(layout: &Layout) -> Files {
        Files {
            log: fs::read(layout.journal_log()).expect("read journal.log"),
            off: fs::read(layout.journal_off()).expect("read journal.off"),
        }
    }

    fn write(&self, layout: &Layout) {
        fs::write(layout.journal_log(), &self.log).expect("write journal.log");
        fs::write(layout.journal_off(), &self.off).expect("write journal.off");
    }

    /// The whole entries of `journal.off`: where each listed frame starts.
    fn entries(&self) -> Vec<u64> {
        let (entries, _partial) = self.off.as_chunks::<ENTRY_LEN>();
        entries
            .iter()
            .map(|entry| u64::from_le_bytes(*entry))
            .collect()
    }

    /// How the frame at `offset` reads; past the end of `journal.log` it is cut short.
    fn frame_at(&self, offset: u64) -> Model {
        let at = usize::try_from(offset).unwrap_or(usize::MAX);
        model(self.log.get(at..).unwrap_or_default())
    }

    /// Where the frame at `offset` ends by its header alone, as the journal judges it: `None`
    /// if the header, or the payload it announces, runs past the end of `journal.log`;
    /// `Err(offset)`, `JournalCorrupt` there, if it announces an empty payload.
    fn header_end(&self, offset: u64) -> Result<Option<u64>, u64> {
        let Some(header) = usize::try_from(offset)
            .ok()
            .and_then(|at| self.log.get(at..))
            .and_then(<[u8]>::first_chunk::<HEADER_LEN>)
        else {
            return Ok(None);
        };
        let len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
        if len == 0 {
            return Err(offset);
        }
        let end = offset + to_u64(HEADER_LEN) + u64::from(len);
        Ok((end <= to_u64(self.log.len())).then_some(end))
    }

    /// [`Files::header_end`], with an empty payload counted as no end.
    fn frame_end(&self, offset: u64) -> Option<u64> {
        self.header_end(offset).ok().flatten()
    }
}

/// A valid journal, as `Journal::create` and `append_frame` wrote it.
struct Built {
    /// The frames, as `append_frame` returned them.
    frames: Vec<Frame>,
    /// The files just after.
    files: Files,
    /// The checkpoint stored, 0 if none.
    ckpt: u64,
    /// How many frames lie before the checkpoint.
    ckpt_frames: usize,
}

impl Built {
    fn new(
        layout: &Layout,
        ops: &Arc<dyn FsOps>,
        frames: Vec<Vec<ArbRecord>>,
        ckpt: Option<u64>,
    ) -> Built {
        let journal = Journal::create(layout, Arc::clone(ops)).expect("create a journal");
        assert_eq!(
            journal
                .truncate_torn_tail()
                .expect("check a new journal's tail"),
            None
        );
        let mut appended: Vec<Frame> = Vec::new();
        let mut end = 0;
        for records in frames.into_iter().take(MAX_FRAMES) {
            let records: Vec<Record> = records
                .into_iter()
                .take(MAX_RECORDS)
                .map(ArbRecord::into_record)
                .collect();
            if records.is_empty() {
                continue;
            }
            let frame = journal
                .append_frame(&records)
                .expect("append a frame the layout holds");
            assert_eq!(
                (frame.seq, frame.offset, &frame.records),
                (to_u64(appended.len()), end, &records),
                "append_frame returned another frame than it appended"
            );
            end += to_u64(frame_bytes(&records).len());
            assert_eq!(journal.len(), end, "len() after an append");
            appended.push(frame);
        }
        let ckpt_frames = ckpt.map_or(0, |pick| to_usize(pick % to_u64(appended.len() + 1)));
        let ckpt = ckpt.map_or(0, |_| {
            let at = appended.get(ckpt_frames).map_or(end, |frame| frame.offset);
            journal.set_ckpt(at).expect("set the checkpoint");
            at
        });
        Built {
            frames: appended,
            files: Files::read(layout),
            ckpt,
            ckpt_frames,
        }
    }

    /// Undamaged (`files` as built), every frame reads back unchanged through every reader.
    /// Damaged, the frames before the first damaged byte of `journal.log` still read back from
    /// the start, and, if their entries are undamaged too, through `frames_from`.
    fn reads_back(&self, journal: &Journal, files: &Files, seen: &Seen) {
        if *files == self.files {
            let all = Some(&self.frames);
            assert_eq!(
                seen.listed.as_ref().ok(),
                all,
                "frames_from(0, usize::MAX), undamaged"
            );
            assert_eq!(
                seen.from_start.as_ref().ok(),
                all,
                "tail_frames_from(0), undamaged"
            );
            assert_eq!(
                seen.first.as_ref().ok(),
                all,
                "frames_from(0, 64), undamaged"
            );
            assert_eq!(
                seen.after_first.as_ref().ok().map(Vec::as_slice),
                Some(self.frames.get(1..).unwrap_or_default()),
                "frames_after(0, 64), undamaged"
            );
            assert_eq!(
                seen.from_ckpt.as_ref().ok().map(Vec::as_slice),
                Some(&self.frames[self.ckpt_frames..]),
                "tail_frames_from(ckpt), undamaged"
            );
            return;
        }
        let log_ok = to_u64(common_prefix(&self.files.log, &files.log));
        let in_log = self
            .frames
            .iter()
            .take_while(|frame| frame.offset + to_u64(frame_bytes(&frame.records).len()) <= log_ok)
            .count();
        if let Ok(frames) = &seen.from_start {
            assert!(
                frames.get(..in_log) == Some(&self.frames[..in_log]),
                "tail_frames_from(0) lost an undamaged frame"
            );
        }
        // frames_from(0, n) also reads entry n, where the frames read must end.
        let entries_ok = common_prefix(&self.files.off, &files.off) / ENTRY_LEN;
        let mut intact = in_log.min(entries_ok);
        if intact == entries_ok && intact < files.entries().len() {
            intact = intact.saturating_sub(1);
        }
        let got = journal.frames_from(0, intact);
        assert!(
            got.as_ref().ok().map(Vec::as_slice) == Some(&self.frames[..intact]),
            "frames_from(0, {intact}) lost an undamaged frame: {got:?}"
        );
    }
}

/// How many leading bytes `a` and `b` share.
fn common_prefix(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).take_while(|(a, b)| a == b).count()
}

/// A journal record, before its lengths are clamped to what the layout holds.
#[derive(Arbitrary, Debug)]
enum ArbRecord {
    Changeset {
        id: [u8; 32],
        generation: u64,
        delta_hash: [u8; 32],
        parents: Vec<[u8; 32]>,
    },
    RefMove {
        name: String,
        old: Option<[u8; 32]>,
        new: Option<[u8; 32]>,
        actor: String,
        ts: i64,
    },
    Compacted {
        sorted_count: u64,
    },
}

impl ArbRecord {
    /// The record, with at most 255 parents, and a ref name and an actor of at most 255 bytes.
    fn into_record(self) -> Record {
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
    ChangesetId(ObjectId(bytes))
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

/// Damage done to a valid journal's files.
#[derive(Arbitrary, Debug)]
enum Edit {
    /// XOR `mask` (0 taken as 1) into the `journal.log` byte at `at`, modulo its length.
    FlipLog { at: u16, mask: u8 },
    /// XOR `mask` (0 taken as 1) into the `journal.off` byte at `at`, modulo its length.
    FlipOff { at: u16, mask: u8 },
    /// Cut `journal.log` to `len` bytes, modulo its length + 1.
    CutLog { len: u16 },
    /// Cut `journal.off` to `len` bytes, modulo its length + 1: maybe inside an entry.
    CutOff { len: u16 },
    /// Drop the last whole entry of `journal.off`, and a partial one after it: a crash between
    /// a frame's two appends.
    DropLastEntry,
    /// Append bytes to `journal.log`: a torn frame, or garbage.
    AppendLog(Vec<u8>),
    /// Append a frame with a correct length and CRC around the payload to `journal.log`,
    /// unlisted: a whole frame if the payload decodes, one no writer produces if it does not.
    AppendFrame(Vec<u8>),
    /// Append bytes to `journal.off`: entries, or part of one.
    AppendOff(Vec<u8>),
    /// Append 1 to 256 zero bytes to `journal.log`: blocks a filesystem exposed zero-filled
    /// after a crash.
    AppendZeros(u8),
}

impl Edit {
    fn apply(self, files: &mut Files) {
        match self {
            Self::FlipLog { at, mask } => flip(&mut files.log, at, mask),
            Self::FlipOff { at, mask } => flip(&mut files.off, at, mask),
            Self::CutLog { len } => cut(&mut files.log, len),
            Self::CutOff { len } => cut(&mut files.off, len),
            Self::DropLastEntry => {
                let entries = files.off.len() / ENTRY_LEN;
                files.off.truncate(entries.saturating_sub(1) * ENTRY_LEN);
            }
            Self::AppendLog(bytes) => files.log.extend(bytes),
            Self::AppendFrame(payload) => files.log.extend(frame_around(&payload)),
            Self::AppendOff(bytes) => files.off.extend(bytes),
            Self::AppendZeros(n) => files.log.resize(files.log.len() + usize::from(n) + 1, 0),
        }
    }
}

fn flip(bytes: &mut [u8], at: u16, mask: u8) {
    if !bytes.is_empty() {
        let at = usize::from(at) % bytes.len();
        bytes[at] ^= mask.max(1);
    }
}

fn cut(bytes: &mut Vec<u8>, len: u16) {
    bytes.truncate(usize::from(len) % (bytes.len() + 1));
}

fn to_usize(n: u64) -> usize {
    usize::try_from(n).expect("u64 fits in usize")
}

/// The real filesystem without the syncs: they decide what survives a crash, which this target
/// does not simulate, and each costs milliseconds on a real disk. Every effect on the files'
/// contents is `RealFs`'s.
struct NoSyncFs;

impl FsOps for NoSyncFs {
    fn create_dir_all(&self, p: &Path) -> io::Result<()> {
        RealFs.create_dir_all(p)
    }

    fn write_durable_temp(&self, dir: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
        RealFs.write_durable_temp(dir, bytes)
    }

    fn link_noclobber(&self, from: &Path, to: &Path) -> io::Result<LinkOutcome> {
        RealFs.link_noclobber(from, to)
    }

    fn fsync_dir(&self, p: &Path) -> io::Result<()> {
        File::open(p).map(drop)
    }

    fn read(&self, p: &Path) -> io::Result<Option<Vec<u8>>> {
        RealFs.read(p)
    }

    fn exists(&self, p: &Path) -> io::Result<bool> {
        RealFs.exists(p)
    }

    fn remove(&self, p: &Path) -> io::Result<()> {
        RealFs.remove(p)
    }

    fn append_durable(&self, path: &Path, bytes: &[u8]) -> io::Result<u64> {
        RealFs.append(path, bytes)
    }

    fn file_len(&self, path: &Path) -> io::Result<u64> {
        RealFs.file_len(path)
    }

    fn truncate(&self, path: &Path, len: u64) -> io::Result<()> {
        OpenOptions::new().write(true).open(path)?.set_len(len)
    }

    fn atomic_replace(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        let dir = path.parent().unwrap_or(Path::new("."));
        let mut temp = tempfile::Builder::new().tempfile_in(dir)?;
        temp.write_all(bytes)?;
        temp.persist(path).map(drop).map_err(|e| e.error)
    }

    fn sync_data(&self, path: &Path) -> io::Result<()> {
        File::open(path).map(drop)
    }

    fn append(&self, path: &Path, bytes: &[u8]) -> io::Result<u64> {
        RealFs.append(path, bytes)
    }

    fn rename_durable(&self, from: &Path, to: &Path) -> io::Result<()> {
        fs::rename(from, to)
    }
}
