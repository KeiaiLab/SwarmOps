//! FORMAT 2 journal recovery and readers never panic or hang on damaged files, and keep the
//! guarantees the M6 design §2.3, §2.4 and the storage spec §3 give: a reader returns frames that
//! are canonical at their place, recovery cuts only a suffix of `journal2.log`, and a recovered
//! journal reads whole and takes the next append.
//!
//! A case is either the journal files verbatim (mode 0: `journal2.log`, `journal2.off`,
//! `journal2.ckpt` if given), or a valid journal built with `Journal::create` and
//! `append_frame` from arbitrary records, with a checkpoint at a frame boundary, and then damaged
//! by a patch (mode 1): bytes flipped in any of the three files, a file cut, the last
//! `journal2.off` entry dropped, garbage or zeros appended, a frame with a correct header
//! appended at a seq and previous CRC the patch picks (right or wrong), or an earlier frame
//! copied to the end (a stale or misplaced frame). Each case runs in a fresh
//! `tempfile::tempdir()`.
//!
//! - Every reader and `ckpt` and `frame_count` return `Ok` or a typed error, before recovery and
//!   after it; the frames `tail_frames_from(0)` and `frames_from(0, usize::MAX)` return are
//!   numbered from 0, abut, and re-encode, at the place their predecessors make, to the bytes
//!   they were read from.
//! - Mode 1 with the files undamaged: the readers return the appended frames, `ckpt` the
//!   checkpoint set, and `repair_tail` reports nothing and changes no file.
//! - `repair_tail` fails with a typed error and changes no file, or succeeds and leaves
//!   `journal2.log` a prefix of what it was; a second call then reports nothing and changes no
//!   file, every frame the log holds is listed (the readers agree), and an append lands at the
//!   end of the log and reads back.
//!
//! Every loop runs over the input's bytes, frames or edits, so a case cannot hang.
#![no_main]

use std::fs;
use std::sync::Arc;

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use swarmops_fuzz::v2::{ArbRecord, ENTRY_LEN_V2, frame2_around, frame2_bytes, payload_crc};
use swarmops_fuzz::{is_typed, to_u64};
use swarmops_store::{Frame, FsOps, Journal, Layout, Record, StoreFormat, TailRepair};
use swarmops_testkit::fs::NoSyncFs;

/// The most frames mode 1 appends, and the most records in each, to keep a case quick.
const MAX_FRAMES: usize = 16;
const MAX_RECORDS: usize = 64;

/// The limit of the bounded read.
const LIMIT: usize = 64;

#[derive(Arbitrary, Debug)]
struct Case {
    /// Even: mode 0, the files verbatim. Odd: mode 1, a valid journal and then a patch.
    mode: u8,
    /// Mode 0: `journal2.log`.
    log: Vec<u8>,
    /// Mode 0: `journal2.off`.
    off: Vec<u8>,
    /// Mode 0: `journal2.ckpt`, no file if `None`. Mode 1: picks the frame boundary `set_ckpt`
    /// stores, no checkpoint if `None`.
    ckpt: Option<Vec<u8>>,
    /// Mode 1: picks the checkpoint's frame boundary, if `ckpt` holds anything.
    pick: u64,
    /// Mode 1: the frames appended, as their records; an empty one is skipped.
    frames: Vec<Vec<ArbRecord>>,
    /// Mode 1: the damage done to the files then, in order.
    patch: Vec<Edit>,
}

/// The contents of the three journal files; no checkpoint file is `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Files {
    log: Vec<u8>,
    off: Vec<u8>,
    ckpt: Option<Vec<u8>>,
}

impl Files {
    fn read(layout: &Layout) -> Files {
        Files {
            log: fs::read(layout.journal_log()).expect("read journal2.log"),
            off: fs::read(layout.journal_off()).expect("read journal2.off"),
            ckpt: fs::read(layout.journal_ckpt()).ok(),
        }
    }

    fn write(&self, layout: &Layout) {
        fs::write(layout.journal_log(), &self.log).expect("write journal2.log");
        fs::write(layout.journal_off(), &self.off).expect("write journal2.off");
        match &self.ckpt {
            Some(ckpt) => fs::write(layout.journal_ckpt(), ckpt).expect("write journal2.ckpt"),
            None => {
                let _ = fs::remove_file(layout.journal_ckpt());
            }
        }
    }
}

fuzz_target!(|case: Case| {
    let dir = tempfile::tempdir().expect("create a temp dir");
    let root = dir.path().to_str().expect("the temp dir path is UTF-8");
    let layout = Layout::with_format(root.to_owned(), StoreFormat::V2);
    let ops: Arc<dyn FsOps> = Arc::new(NoSyncFs);
    if case.mode.is_multiple_of(2) {
        fs::create_dir(layout.meta_dir()).expect("create meta/");
        let files = Files {
            log: case.log,
            off: case.off,
            ckpt: case.ckpt,
        };
        files.write(&layout);
        check(&layout, &ops, None);
        return;
    }

    let built = Built::new(&layout, &ops, case.frames, case.ckpt.is_some(), case.pick);
    let mut files = built.files.clone();
    for edit in case.patch {
        edit.apply(&mut files, &built);
    }
    files.write(&layout);
    check(&layout, &ops, Some((&built, files == built.files)));
});

/// Reads the journal in `layout`, recovers it, reads it again and appends to it, checking the
/// properties. `built` is the valid journal a mode 1 case started from, and whether the patch
/// left it as built.
fn check(layout: &Layout, ops: &Arc<dyn FsOps>, built: Option<(&Built, bool)>) {
    let before = Files::read(layout);
    let journal = Journal::open(layout, Arc::clone(ops)).expect("open: the journal files exist");
    let intact = built.filter(|(_, intact)| *intact).map(|(built, _)| built);
    reads(&journal, &before, intact);

    let repair = match journal.repair_tail() {
        Ok(repair) => repair,
        Err(e) => {
            assert!(is_typed(&e), "repair_tail: an untyped error: {e:?}");
            assert!(
                intact.is_none(),
                "an undamaged journal failed recovery: {e:?}"
            );
            assert!(
                Files::read(layout) == before,
                "repair_tail changed the files, then failed with {e:?}"
            );
            return;
        }
    };
    let after = Files::read(layout);
    assert!(
        before.log.starts_with(&after.log),
        "recovery changed journal2.log other than by cutting a suffix"
    );
    if intact.is_some() {
        assert!(
            repair == TailRepair::default() && after == before,
            "recovery of an undamaged journal reported or changed something: {repair:?}"
        );
    }
    let again = journal.repair_tail();
    assert!(
        matches!(again, Ok(again) if again == TailRepair::default()),
        "repair_tail reported {repair:?}, then {again:?}"
    );
    assert!(
        Files::read(layout) == after,
        "a second repair_tail changed the files"
    );

    // Everything the log holds is listed now: the two reads agree.
    let listed = journal.frames_from(0, usize::MAX);
    let tail = journal.tail_frames_from(0);
    match (&listed, &tail) {
        (Ok(listed), Ok(tail)) => assert_eq!(listed, tail, "after recovery the reads differ"),
        _ => {
            for e in [listed.as_ref().err(), tail.as_ref().err()]
                .into_iter()
                .flatten()
            {
                assert!(is_typed(e), "a read after recovery: untyped error {e:?}");
            }
        }
    }
    reads(&journal, &after, None);
    appends_at_the_end(&journal, &after);
}

/// Asserts that every read of the journal returns `Ok` or a typed error, that the frames from
/// offset 0 are canonical, and, with `intact` (the journal as built), that they are the frames
/// appended and the checkpoint is the one set.
fn reads(journal: &Journal, files: &Files, intact: Option<&Built>) {
    let tail = journal.tail_frames_from(0);
    let listed = journal.frames_from(0, usize::MAX);
    let first = journal.frames_from(0, LIMIT);
    let after = journal.frames_after(0, LIMIT);
    let count = journal.frame_count();
    let ckpt = journal.ckpt();
    for e in [
        tail.as_ref().err(),
        listed.as_ref().err(),
        first.as_ref().err(),
        after.as_ref().err(),
        count.as_ref().err(),
        ckpt.as_ref().err(),
    ]
    .into_iter()
    .flatten()
    {
        assert!(is_typed(e), "a read: an untyped error {e:?}");
    }
    for frames in [&tail, &listed].into_iter().flatten() {
        canonical(frames, &files.log);
    }
    if let Ok(first) = &first {
        canonical(first, &files.log);
    }
    let Some(built) = intact else {
        return;
    };
    assert_eq!(
        tail.ok().as_ref(),
        Some(&built.frames),
        "tail_frames_from(0)"
    );
    assert_eq!(
        listed.ok().as_ref(),
        Some(&built.frames),
        "frames_from(0, MAX)"
    );
    assert_eq!(count.ok(), Some(to_u64(built.frames.len())), "frame_count");
    assert_eq!(ckpt.ok(), Some(built.ckpt), "ckpt");
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

/// After a recovery, a writer's next frame lands where `journal2.log` ends, takes the next seq,
/// and reads back. A journal whose checkpoint is damaged may refuse the append, with a typed
/// error.
fn appends_at_the_end(journal: &Journal, files: &Files) {
    let seq = match journal.frame_count() {
        Ok(count) => count,
        Err(e) => {
            assert!(
                is_typed(&e),
                "frame_count after recovery: untyped error {e:?}"
            );
            return;
        }
    };
    let record = Record::Compacted { sorted_count: 7 };
    let frame = match journal.append_frame(std::slice::from_ref(&record)) {
        Ok(frame) => frame,
        Err(e) => {
            assert!(is_typed(&e), "append after recovery: untyped error {e:?}");
            return;
        }
    };
    assert_eq!(
        (frame.seq, frame.offset),
        (seq, to_u64(files.log.len())),
        "the appended frame is not at the end"
    );
    let read = journal.frames_from(seq, 1);
    assert_eq!(
        read.ok(),
        Some(vec![frame]),
        "the appended frame does not read back"
    );
}

/// A valid journal: the frames appended, the files just after, the checkpoint set.
struct Built {
    frames: Vec<Frame>,
    files: Files,
    /// The checkpoint stored, 0 if none.
    ckpt: u64,
    /// Where each frame starts in `journal2.log`, then where the last ends.
    bounds: Vec<usize>,
}

impl Built {
    fn new(
        layout: &Layout,
        ops: &Arc<dyn FsOps>,
        frames: Vec<Vec<ArbRecord>>,
        with_ckpt: bool,
        pick: u64,
    ) -> Built {
        let journal = Journal::create(layout, Arc::clone(ops)).expect("create a journal");
        let mut appended: Vec<Frame> = Vec::new();
        let mut bounds = vec![0];
        let mut prev_crc = 0;
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
            let seq = to_u64(appended.len());
            let bytes = frame2_bytes(&records, seq, prev_crc);
            let start = *bounds.last().expect("bounds start at 0");
            assert_eq!(
                (frame.seq, frame.offset),
                (seq, to_u64(start)),
                "append_frame returned another place than it appended at"
            );
            prev_crc = payload_crc(&bytes);
            bounds.push(start + bytes.len());
            appended.push(frame);
        }
        let ckpt = if with_ckpt {
            let at = bounds[usize::try_from(pick % to_u64(bounds.len())).expect("fits")];
            journal.set_ckpt(to_u64(at)).expect("set the checkpoint");
            to_u64(at)
        } else {
            0
        };
        Built {
            frames: appended,
            files: Files::read(layout),
            ckpt,
            bounds,
        }
    }
}

/// Damage done to a valid journal's files.
#[derive(Arbitrary, Debug)]
enum Edit {
    /// XOR `mask` (0 taken as 1) into the `journal2.log` byte at `at`, modulo its length.
    FlipLog { at: u16, mask: u8 },
    /// The same in `journal2.off`.
    FlipOff { at: u16, mask: u8 },
    /// The same in `journal2.ckpt`.
    FlipCkpt { at: u8, mask: u8 },
    /// Cut `journal2.log` to `len` bytes, modulo its length + 1.
    CutLog { len: u16 },
    /// Cut `journal2.off` to `len` bytes, modulo its length + 1: maybe inside an entry.
    CutOff { len: u16 },
    /// Drop `journal2.ckpt`.
    DropCkpt,
    /// Drop the last whole entry of `journal2.off`, and a partial one after it: a crash between
    /// a frame's two appends.
    DropLastEntry,
    /// Append bytes to `journal2.log`: a torn frame, or garbage.
    AppendLog(Vec<u8>),
    /// Append bytes to `journal2.off`: entries, or part of one.
    AppendOff(Vec<u8>),
    /// Append 1 to 256 zero bytes to `journal2.log`.
    AppendZeros(u8),
    /// Append 1 to 4 zeroed entries to `journal2.off`: what a power loss leaves.
    ZeroEntries(u8),
    /// Append a frame whose header checks, around `payload`, at the next seq plus `skew` and with
    /// the previous payload CRC, flipped by `crc_flip`: right if both are 0.
    AppendFrame {
        payload: Vec<u8>,
        skew: i8,
        crc_flip: u32,
    },
    /// Append a copy of frame `pick` (modulo the count) to `journal2.log`: stale or misplaced.
    CopyFrame { pick: u8 },
}

impl Edit {
    fn apply(self, files: &mut Files, built: &Built) {
        match self {
            Self::FlipLog { at, mask } => flip(&mut files.log, at.into(), mask),
            Self::FlipOff { at, mask } => flip(&mut files.off, at.into(), mask),
            Self::FlipCkpt { at, mask } => {
                if let Some(ckpt) = &mut files.ckpt {
                    flip(ckpt, at.into(), mask);
                }
            }
            Self::CutLog { len } => cut(&mut files.log, len),
            Self::CutOff { len } => cut(&mut files.off, len),
            Self::DropCkpt => files.ckpt = None,
            Self::DropLastEntry => {
                let entries = files.off.len() / ENTRY_LEN_V2;
                files.off.truncate(entries.saturating_sub(1) * ENTRY_LEN_V2);
            }
            Self::AppendLog(bytes) => files.log.extend(bytes),
            Self::AppendOff(bytes) => files.off.extend(bytes),
            Self::AppendZeros(n) => files.log.resize(files.log.len() + usize::from(n) + 1, 0),
            Self::ZeroEntries(n) => {
                let entries = usize::from(n % 4) + 1;
                files
                    .off
                    .resize(files.off.len() + entries * ENTRY_LEN_V2, 0);
            }
            Self::AppendFrame {
                payload,
                skew,
                crc_flip,
            } => {
                let seq = to_u64(built.frames.len()).wrapping_add_signed(skew.into());
                let prev = built.frames.last().map_or(0, |_| last_payload_crc(built));
                files
                    .log
                    .extend(frame2_around(&payload, seq, prev ^ crc_flip));
            }
            Self::CopyFrame { pick } => {
                let count = built.frames.len();
                if count > 0 {
                    let i = usize::from(pick) % count;
                    let copy = built.files.log[built.bounds[i]..built.bounds[i + 1]].to_vec();
                    files.log.extend(copy);
                }
            }
        }
    }
}

/// The payload CRC of the last frame `built` appended.
fn last_payload_crc(built: &Built) -> u32 {
    let start = built.bounds[built.bounds.len() - 2];
    payload_crc(&built.files.log[start..])
}

fn flip(bytes: &mut [u8], at: usize, mask: u8) {
    if !bytes.is_empty() {
        let at = at % bytes.len();
        bytes[at] ^= mask.max(1);
    }
}

fn cut(bytes: &mut Vec<u8>, len: u16) {
    bytes.truncate(usize::from(len) % (bytes.len() + 1));
}
