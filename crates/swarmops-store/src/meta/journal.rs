//! The metadata journal (storage spec §2 rule 3, §3): `meta/journal.log` holds CRC-framed
//! frames, one per write transaction, and is the source of truth; `meta/journal.off` holds
//! each frame's start offset (`u64le`), so frame `seq` is found without a scan;
//! `meta/journal.ckpt` holds the offset up to which the metadata files are materialized.
//!
//! A frame is appended with one `append_durable` to `journal.log`, then one to
//! `journal.off`, so a crash leaves at most one torn frame at the tail, or one whole frame
//! that `journal.off` does not list yet; [`Journal::truncate_torn_tail`] repairs either,
//! syncing `journal.log` before it lists such a frame. So every frame `journal.off` lists was
//! whole and durable before it was listed, and damage to a listed frame is reported as
//! [`StoreError::JournalCorrupt`], never cut away.
//!
//! An append that fails leaves nothing a later sync could make look durable (storage spec §2
//! rule 3): Linux reports a failed writeback once, to the fdatasync that meets it, and may drop
//! the pages it could not write, so a later fdatasync can succeed without them. A frame whose
//! append fails is cut back off `journal.log`, under the lock, before the error returns
//! ([`StoreError::NotCommitted`]; the process aborts if even the cut fails), and an entry whose
//! append fails is cut back off `journal.off`: [`StoreError::PostCommit`] for the entry of the
//! caller's own frame, which is durable and committed, and which the next tail check lists
//! afresh; [`StoreError::NotCommitted`] for the tail check's listing of a frame left unlisted,
//! since the caller's transaction never began. When the tail check's sync of the frames it is
//! about to list fails, it writes them afresh (cut back off, appended again and synced) and
//! lists them then; if that append fails, they are cut back off too
//! ([`StoreError::NotCommitted`]), and if they land anywhere but where they were (another
//! writer's bytes came in between), they are cut back off and refused as
//! [`StoreError::JournalCorrupt`] before anything is listed; a cut that fails aborts.
//!
//! Writers hold [`crate::MetaLock`]; the journal itself takes no lock. Writes go through
//! [`FsOps`]. Reads take no lock: they measure `journal.off` before `journal.log` (with
//! [`FsOps::file_len`]) and never measure `journal.off` again once `journal.log` is measured;
//! one that finds `journal.off` holding fewer entries than it measured (a writer cut entries that
//! were never genuine: one whose own append failed, or zeroed or partial ones a crash left at its
//! end) starts over before it measures `journal.log`, up to three times. A frame listed in that
//! snapshot was synced before its entry was written, so it is whole in the `journal.log`
//! measured after it, and damage to it is [`StoreError::JournalCorrupt`] for readers too;
//! they skip only an unlisted torn tail (or an append still in progress) silently, and drop,
//! as recovery does, the zeroed entries at the end of `journal.off` that a power loss leaves of
//! an entry never synced (no frame but the first starts at 0): a 4 KiB block of them at most,
//! found with one read, and a longer run of zeros is damage. They read
//! with `read_at` on read-only handles, except that `journal.ckpt`, which
//! [`Journal::set_ckpt`] replaces by rename, is read by path.
//!
//! A FORMAT 2 store keeps the same protocol in `journal2.log`, `journal2.off` and
//! `journal2.ckpt` (the M6 design §2.2 to §2.4). Each frame's header names its place, its seq and
//! the payload CRC of the frame before it, behind a CRC of its own; each entry carries the
//! frame's payload CRC and a CRC of its seq, its offset and that; the checkpoint holds the seq of
//! the frame at its offset too. So a header that checks but announces a length no writer writes,
//! or a place not its own (a misplaced, stale or lost frame), is [`StoreError::JournalCorrupt`]
//! wherever it lies, never a torn tail; an entry that does not check, or names a payload CRC its
//! frame does not have, is too; and recovery checks the listed frames from the checkpoint's seq
//! on, not from frame 0. FORMAT 2 refuses a bad frame header before it reads a byte of its
//! payload. The calls through [`FsOps`], the syncs among them, are FORMAT 1's.
//!
//! Both formats read `journal.log` a frame at a time, its header before its payload, and
//! `journal.off` 4,096 entries at a time, so no read is sized by a file's length (the M6 design
//! §5): a read of entries stops after one whose offset is not past the one before it, which no
//! frame has, and a FORMAT 1 payload announced past the largest one this build decodes is
//! sorted by its CRC as it streams by, never held.

use std::fmt;
use std::fs::File;
use std::io::{self, Write};
use std::ops::RangeInclusive;
use std::os::unix::fs::FileExt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use camino::Utf8PathBuf;
use swarmops_core::bytes;

use super::READ_WINDOW;
use super::frame::{self, BadHeader, Frame, HEADER_LEN_V2, MAX_PAYLOAD_LEN, Parsed, Place, Record};
use crate::error::{FileOp, IoFailure, StoreError};
use crate::format::StoreFormat;
use crate::fs_ops::{self, FsMethod, FsOps, StoreFs};
use crate::layout::Layout;

/// The size of one `journal.off` entry.
const ENTRY_LEN: u64 = 8;

/// The size of one `journal2.off` entry (FORMAT 2, the M6 design §2.4): `u64le offset | u32le
/// payload_crc | u32le crc32(seq, offset, payload_crc)`.
pub(crate) const ENTRY_LEN_V2: u64 = 16;

/// The size of `journal2.ckpt` (FORMAT 2, the M6 design §2.2): `u64le offset | u64le seq |
/// u32le crc32(the 16 bytes before)`.
const CKPT_LEN_V2: usize = 20;

/// How many times [`Journal::frames_from`] and [`Journal::tail_frames_from`] start over when
/// `journal.off` holds fewer entries when they read them than when they measured it. Only a
/// writer cuts it, under the lock, and only entries that were never genuine (one whose own append
/// failed, or zeroed or partial ones a crash left at its end), so the next read nearly always
/// finds it still; a shrink every time is reported as damage.
const OFF_REREADS: u32 = 3;

/// The most zeroed entries at the end of `journal.off`, past frame 0, that readers and recovery
/// drop: a 4 KiB block of them. A power loss leaves zeroed only the entry being written, always
/// within one 512 B sector (storage spec §2 rule 3), so a longer run of zeros is damage,
/// [`StoreError::JournalCorrupt`] at 0 (the offset such an entry holds); and finding where the
/// entries that list frames end takes one read of at most the last 4 KiB of the file (and the
/// entry before them), however long it is.
const MAX_ZEROED: u64 = 512;

/// The longest read of `journal.log` or `journal.off`: one whole frame of the largest payload
/// this build decodes. A frame is read a frame at a time and entries a window at a time, so no
/// read is sized by a file's length (the M6 design §5).
const MAX_READ: u64 = (HEADER_LEN_V2 + MAX_PAYLOAD_LEN) as u64;

/// How many bytes of a FORMAT 1 payload longer than [`MAX_PAYLOAD_LEN`] one read takes while its
/// CRC streams by: its header has no CRC of its own to refuse the length with.
const CRC_CHUNK: u64 = 1 << 20;

/// A handle on one store's metadata journal.
#[non_exhaustive]
pub struct Journal {
    log_path: Utf8PathBuf,
    off_path: Utf8PathBuf,
    ckpt_path: Utf8PathBuf,
    /// Every durable write goes through here, and so do the readers' measurements of the
    /// files' lengths (`journal.off` first). It locates the failures of the reads on the
    /// handles too.
    fs: StoreFs,
    /// Read-only handle on `journal.log`.
    log: File,
    /// Read-only handle on `journal.off`.
    off: File,
    /// The store's format: what the files are named, and how frames, entries and the
    /// checkpoint are laid out.
    format: StoreFormat,
    tail: Mutex<Tail>,
}

impl fmt::Debug for Journal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Journal")
            .field("log", &self.log_path)
            .field("tail", &self.tail)
            .finish_non_exhaustive()
    }
}

/// A torn tail cut off `journal.log`: the bytes of a frame that never reached its commit point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct LogCut {
    /// Where the file was cut: where the torn tail started, and the file ends now.
    pub at: u64,
    /// How many bytes were cut, from `at` to where the file ended.
    pub bytes: u64,
}

impl LogCut {
    /// A cut at `at` of `bytes` bytes.
    pub fn new(at: u64, bytes: u64) -> LogCut {
        LogCut { at, bytes }
    }
}

/// What [`Journal::repair_tail`] changed to bring the journal to a clean end; all zero when it
/// ended cleanly.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct TailRepair {
    /// The torn tail cut off `journal.log`, if there was one.
    pub cut: Option<LogCut>,
    /// How many whole frames `journal.off` did not list were listed: transactions that committed
    /// and that their writer never listed (it crashed between a frame's two appends, or the
    /// listing failed), and frames whose entries a power loss zeroed.
    pub frames_listed: u64,
    /// How many of those were written afresh before they were listed, because the sync that would
    /// have made them durable failed (storage spec §2 rule 3 (c)).
    pub frames_rewritten: u64,
    /// How many bytes were dropped from the end of `journal.off`: a partial entry, and zeroed
    /// entries past frame 0.
    pub off_bytes_dropped: u64,
}

/// Where this handle last saw the journal end.
#[derive(Debug, Clone, Copy)]
struct Tail {
    /// The length of `journal.log`.
    len: u64,
    /// The number of whole entries in `journal.off`.
    frames: u64,
    /// Whether the journal was known to end cleanly here, every whole frame listed and no
    /// torn tail: set by a tail check and by each append, cleared before anything that
    /// could leave it wrong.
    checked: bool,
    /// The payload CRC of the last frame (0 for none), which the next FORMAT 2 frame's header
    /// names: right while `checked` is.
    last_crc: u32,
}

/// One entry of `journal.off` as a reader takes it: where its frame starts, and in FORMAT 2 the
/// frame's payload CRC (`None` in FORMAT 1, and for a zeroed entry, whose offset reads 0).
#[derive(Debug, Clone, Copy)]
struct Entry {
    offset: u64,
    crc: Option<u32>,
}

/// What a listed frame's header alone says: where the frame ends, and its payload CRC (0 in
/// FORMAT 1, where only the length is read).
#[derive(Debug, Clone, Copy)]
struct Head {
    end: u64,
    crc: u32,
}

/// What a read of `journal.off` entries does at one whose offset is not past the offset of the
/// entry before it, which no frame it lists has.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Disorder {
    /// Reads on: the entries may end in zeroed ones ([`Journal::last_entries`]).
    ReadOn,
    /// Stops after it: the frames read with the entries meet it, and report it, before any
    /// entry past it, which would be read only to be dropped.
    Stop,
}

/// A FORMAT 2 checkpoint: an offset, and the seq of the frame that starts there (the number of
/// frames before it).
#[derive(Debug, Clone, Copy)]
struct Ckpt {
    offset: u64,
    seq: u64,
}

/// What one read of the frames ([`Journal::frames_from`], [`Journal::tail_frames_from`]) gives
/// when it does not fail.
enum Attempt {
    /// The frames.
    Read(Vec<Frame>),
    /// `journal.off` held fewer entries when they were read than it did when measured: a writer
    /// has cut entries that were never genuine since the measurement, and the read starts over.
    /// Past [`OFF_REREADS`], [`StoreError::JournalCorrupt`] at `offset`.
    Shrunk {
        /// Where the read reports the journal corrupt once it gives up.
        offset: u64,
    },
}

impl Journal {
    /// Creates `meta/` and the journal files of `layout` where they are missing, makes their
    /// directory entries durable, and opens the journal. Existing files are kept as they are.
    pub fn create(layout: &Layout, fs: Arc<dyn FsOps>) -> Result<Journal, StoreError> {
        Self::create_in(layout, &StoreFs::new(fs, layout.root()))
    }

    /// [`Journal::create`], through `fs`, whose root may be another than `layout`'s: a staged
    /// create's files are named where they are, under `meta.tmp/`.
    pub(crate) fn create_in(layout: &Layout, fs: &StoreFs) -> Result<Journal, StoreError> {
        let meta = layout.meta_dir();
        fs.create_dir_all(meta.as_std_path())?;
        for path in [layout.journal_log(), layout.journal_off()] {
            // An empty append creates a missing file and never truncates an existing one.
            fs.append_durable(path.as_std_path(), &[])?;
        }
        fs.fsync_dir(meta.as_std_path())?;
        fs.fsync_dir(layout.root().as_std_path())?;
        Self::open_in(layout, fs.clone())
    }

    /// Opens the existing journal of `layout`, durable writes going through `fs`. Opening
    /// reads only the files' lengths; the tail is checked by the first
    /// [`Journal::append_frame`] or [`Journal::truncate_torn_tail`].
    pub fn open(layout: &Layout, fs: Arc<dyn FsOps>) -> Result<Journal, StoreError> {
        Self::open_in(layout, StoreFs::new(fs, layout.root()))
    }

    /// [`Journal::open`], through `fs`, whose root may be another than `layout`'s (see
    /// [`Journal::create_in`]).
    pub(crate) fn open_in(layout: &Layout, fs: StoreFs) -> Result<Journal, StoreError> {
        let log = fs_ops::open_regular(layout.journal_log())
            .map_err(|e| fs.locate(FileOp::Open, layout.journal_log(), e))?;
        let off = fs_ops::open_regular(layout.journal_off())
            .map_err(|e| fs.locate(FileOp::Open, layout.journal_off(), e))?;
        let tail = Tail {
            len: (log.metadata())
                .map_err(|e| fs.locate(FileOp::Stat, layout.journal_log(), e))?
                .len(),
            frames: (off.metadata())
                .map_err(|e| fs.locate(FileOp::Stat, layout.journal_off(), e))?
                .len()
                / entry_len(layout.format()),
            checked: false,
            last_crc: 0,
        };
        Ok(Self {
            log_path: layout.journal_log(),
            off_path: layout.journal_off(),
            ckpt_path: layout.journal_ckpt(),
            fs,
            log,
            off,
            format: layout.format(),
            tail: Mutex::new(tail),
        })
    }

    /// Appends `records` as one frame (one transaction) and returns the frame. Its bytes
    /// go to `journal.log` in one `append_durable`, then its offset to `journal.off` in
    /// another; the transaction is committed once the first returns.
    ///
    /// The caller must hold [`crate::MetaLock`]: this takes no lock. If the journal does
    /// not end where this handle last saw it end, or the handle never checked its tail,
    /// the tail is checked first, as [`Journal::truncate_torn_tail`] does, so that a frame
    /// never lands after a torn one; what that check repairs goes unreported. Under
    /// [`crate::MetaStore::write`], which checked the tail under the same lock, it finds nothing
    /// to repair. Records the layout cannot hold are refused before
    /// anything is written: more than 65,535, or a record whose lengths do not fit, with
    /// [`StoreError::Limit`]; none at all, with [`StoreError::BadCall`].
    ///
    /// When the first append fails, in its write or in its fdatasync, what reached
    /// `journal.log` of the frame is cut back off it first (the file truncated to where the
    /// frame starts, the cut synced), and the error is [`StoreError::NotCommitted`]: the
    /// transaction did not commit. Linux reports a failed writeback once, to the fdatasync that
    /// meets it, and may drop the pages it could not write: a frame left in the file after its
    /// sync failed could be listed by a later writer whose sync succeeds without it, and lost to
    /// a power loss after that. When even the cut fails, nothing here can keep a later write of
    /// this process from listing the frame: the process aborts ([`std::process::abort`]), saying
    /// why on stderr.
    ///
    /// When the second append fails, the frame being durable, the error is
    /// [`StoreError::PostCommit`] at the frame's seq: the transaction committed, and the next
    /// tail check lists the frame. What reached `journal.off` of the entry is cut back off it
    /// first: an entry whose sync failed must not stay listed while later entries are made
    /// durable after it, a hole a power loss would leave among the listed frames. A cut that
    /// fails too is reported beside the append's error.
    pub fn append_frame(&self, records: &[Record]) -> Result<Frame, StoreError> {
        let mut bytes = frame::encode_frame_in(self.format, records, 0, 0)?;
        let mut tail = self.tail();
        if !self.still_ends_at(&tail)? {
            tail.checked = false;
            // Unreported: see above.
            (*tail, _) = self.check_tail()?;
        }
        tail.checked = false; // a failed append can leave part of a frame or an entry behind

        // FORMAT 2: the header names the frame's place, which the tail check has found.
        if self.format == StoreFormat::V2 {
            frame::set_place(&mut bytes, tail.frames, tail.last_crc);
        }
        let crc = frame::payload_crc(&bytes);
        let offset = match self.fs.append_durable(self.log_path.as_std_path(), &bytes) {
            Ok(offset) => offset,
            Err(e) => return Err(self.cut_failed_frame(tail.len, e)),
        };
        // The commit point: the frame is whole and durable.
        let entry = self.entry_bytes(tail.frames, offset, crc);
        let entry_at = match self.fs.append_durable(self.off_path.as_std_path(), &entry) {
            Ok(entry_at) => entry_at,
            Err(e) => {
                let source = self.cut_failed_entries(tail.frames, e);
                return Err(StoreError::PostCommit {
                    frame_seq: tail.frames,
                    source: Box::new(source.into()),
                });
            }
        };
        let seq = entry_at / self.entry_len();
        *tail = Tail {
            len: offset + bytes::to_u64(bytes.len()),
            frames: seq + 1,
            checked: true,
            last_crc: crc,
        };
        Ok(Frame {
            seq,
            offset,
            records: records.to_vec(),
        })
    }

    /// Up to `limit` frames in journal order after frame `seq`: the frames `journal.off`
    /// lists whose seq is strictly greater than `seq`, read as [`Journal::frames_from`]
    /// reads them. Seqs count from 0, so frame 0 is never among them;
    /// [`Journal::frames_from`]`(0, ..)` reads from the very start.
    pub fn frames_after(&self, seq: u64, limit: usize) -> Result<Vec<Frame>, StoreError> {
        match seq.checked_add(1) {
            Some(first) => self.frames_from(first, limit),
            None => Ok(Vec::new()),
        }
    }

    /// Up to `limit` frames in journal order from frame `first` on: the frames `journal.off`
    /// lists whose seq is `first` or greater. Reads the entries (the previous frame's, the
    /// wanted frames', and the next one) 4,096 at a time, stopping after one whose offset is not
    /// past the one before it, the previous frame's header (frame 0 has none: it starts at 0),
    /// and the frames a frame at a time, each header before its payload.
    ///
    /// `journal.off` is measured, and the entries read, before `journal.log`: every frame
    /// listed then was whole and durable before its entry was written, so it is whole in
    /// `journal.log` as measured after. One that is cut short or missing, fails its CRC or
    /// does not decode is [`StoreError::JournalCorrupt`], as is an entry that does not point
    /// where the previous frame ends. A `journal.off` that holds fewer entries when they are read
    /// than it did when measured is no damage: a writer cut entries that were never genuine (one
    /// whose own append failed, or zeroed or partial ones a crash left at its end), and the read
    /// starts over, measuring `journal.off` again, up to three times; a shrink every time is
    /// [`StoreError::JournalCorrupt`] at the first wanted frame's start, or 0 if its entry is gone
    /// too. Zeroed entries at the end of `journal.off` list nothing and are dropped first, as
    /// recovery drops them, 512 at most (see [`Journal::tail_frames_from`]).
    pub fn frames_from(&self, first: u64, limit: usize) -> Result<Vec<Frame>, StoreError> {
        reread(|| self.frames_from_once(first, limit))
    }

    /// One read of [`Journal::frames_from`]: [`Attempt::Shrunk`] where `journal.off` holds fewer
    /// entries when they are read than it did when measured.
    fn frames_from_once(&self, first: u64, limit: usize) -> Result<Attempt, StoreError> {
        // journal.off cut since it was measured: entries of the snapshot are gone.
        let Some(listed) = self.listed()? else {
            return Ok(Attempt::Shrunk { offset: 0 });
        };
        let wanted = u64::try_from(limit)
            .unwrap_or(u64::MAX)
            .min(listed.saturating_sub(first));
        if wanted == 0 {
            return Ok(Attempt::Read(Vec::new()));
        }
        // The previous frame's entry (the first wanted frame starts where that frame ends),
        // the wanted frames' entries, and the next one (where the last wanted frame ends).
        let previous = first.checked_sub(1);
        let count = u64::from(previous.is_some()) + wanted + u64::from(first + wanted < listed);
        let entries = self.entries_of(previous.unwrap_or(first), count, Disorder::Stop)?;
        let skip = usize::from(previous.is_some());
        if bytes::to_u64(entries.len()) != count && !ends_in_disorder(&entries) {
            // journal.off was cut since it was measured: entries of the snapshot are gone.
            let offset = entries.get(skip).map_or(0, |entry| entry.offset);
            return Ok(Attempt::Shrunk { offset });
        }
        let previous = previous.map(|_| entries[0].offset);
        let listing = &entries[skip..];
        let wanted = usize::try_from(wanted).map_or(listing.len(), |n| n.min(listing.len()));
        let start = listing[0].offset;
        // Only now journal.log, which holds whole every frame listed above.
        let log_len = self.fs.file_len(self.log_path.as_std_path())?;
        let mut prev_crc = self.check_starts_after(previous, start, log_len)?;
        let end = listing
            .get(wanted)
            .map_or(log_len, |next| next.offset.min(log_len));
        if start > end {
            return Err(StoreError::JournalCorrupt { offset: start });
        }

        // Each listed frame where the one before it ends, whole, at its own place, with the
        // payload CRC its entry names.
        let mut frames = Vec::with_capacity(wanted);
        let mut at = start;
        for (frame_seq, entry) in (first..).zip(&listing[..wanted]) {
            let offset = entry.offset;
            if offset != at {
                return Err(StoreError::JournalCorrupt { offset });
            }
            let place = Place {
                seq: frame_seq,
                prev_crc,
            };
            match self.parse_at(end, offset, place)? {
                Parsed::Whole { records, size, crc } if entry.lists(crc) => {
                    frames.push(Frame {
                        seq: frame_seq,
                        offset,
                        records,
                    });
                    at += bytes::to_u64(size);
                    prev_crc = crc;
                }
                _ => return Err(StoreError::JournalCorrupt { offset }),
            }
        }
        if let Some(next) = listing.get(wanted)
            && next.offset != at
        {
            return Err(StoreError::JournalCorrupt {
                offset: next.offset,
            });
        }
        Ok(Attempt::Read(frames))
    }

    /// Every frame from `offset` to the end of `journal.log`, read a frame at a time, each header
    /// before its payload: what a reader applies past [`Journal::ckpt`]. `offset` must be a
    /// frame boundary: 0, a listed frame's start, or the end of the last listed frame; the
    /// entries from there on are read 4,096 at a time. Zeroed entries at the end of
    /// `journal.off`, past frame 0, are not listings: a power loss between an entry's append
    /// and its sync may keep the file's length and lose the bytes. They are dropped first, as
    /// recovery drops them, and the frames they would list read as whole frames not listed yet.
    /// A power loss leaves zeroed only the entry being written, so more than 512 of them (a
    /// 4 KiB block) are damage, [`StoreError::JournalCorrupt`] at 0: finding where they start
    /// reads at most the last 4 KiB of `journal.off`, however long it is.
    ///
    /// `journal.off` is measured, and the entries read, before `journal.log`: every frame
    /// listed then was whole and durable before its entry was written, so it is whole in
    /// `journal.log` as measured after, where its entry says. What the read meets is one of:
    ///
    /// - a whole frame, listed or not, which it returns. Whole frames after the listed ones are
    ///   an append racing the read, which lists them later, or a crash between a frame's two
    ///   appends;
    /// - a torn tail, where it stops silently: an unlisted frame cut short (an append in
    ///   progress looks the same), failing its CRC, or whose length reads 0 (zeros to the end
    ///   of the file among them; see [`Journal::truncate_torn_tail`]);
    /// - damage, [`StoreError::JournalCorrupt`]: a listed frame that is cut short, missing,
    ///   failing its CRC, not decoding, or away from its entry, as a zeroed or stale entry puts
    ///   it; a listed frame just before the read (the last listed one, if the read starts past
    ///   them all) that does not start where the one listed before it ends (frame 0 at 0), as
    ///   recovery checks it too; an `offset` that is not a frame boundary; anything else.
    ///
    /// A `journal.off` that holds fewer entries when they are read than it did when measured is
    /// no damage, as [`Journal::frames_from`] says: the read starts over, up to three times, and
    /// a shrink every time is reported at `offset`.
    pub fn tail_frames_from(&self, offset: u64) -> Result<Vec<Frame>, StoreError> {
        reread(|| self.tail_frames_once(offset))
    }

    /// [`Journal::tail_frames_from`] read once: `None` where `journal.off` holds fewer entries
    /// when they are read than it did when measured, for a caller that counts such a shrink
    /// itself ([`crate::MetaStore::read`] takes another of its rounds).
    pub(crate) fn tail_frames_from_unless_shrunk(
        &self,
        offset: u64,
    ) -> Result<Option<Vec<Frame>>, StoreError> {
        match self.tail_frames_once(offset)? {
            Attempt::Read(frames) => Ok(Some(frames)),
            Attempt::Shrunk { .. } => Ok(None),
        }
    }

    /// One read of [`Journal::tail_frames_from`]: [`Attempt::Shrunk`] where `journal.off` holds
    /// fewer entries when they are read than it did when measured.
    fn tail_frames_once(&self, offset: u64) -> Result<Attempt, StoreError> {
        // journal.off cut since it was measured: entries of the snapshot are gone.
        let shrunk = || Ok(Attempt::Shrunk { offset });
        let Some(listed) = self.listed()? else {
            return shrunk();
        };
        let Some(first) = unless_cut(self.first_at_or_after(offset, listed))? else {
            return shrunk();
        };
        // Where each listed frame from `first` on starts, and where the listed frame just
        // before the read and the one before that start.
        let entries = self.entries_of(first, listed - first, Disorder::Stop)?;
        if bytes::to_u64(entries.len()) != listed - first && !ends_in_disorder(&entries) {
            return shrunk();
        }
        let before = match first.checked_sub(1) {
            Some(before) => {
                let previous = unless_cut(self.entry_before(before))?;
                let (Some(previous), Some(start)) = (previous, unless_cut(self.entry(before))?)
                else {
                    return shrunk();
                };
                Some((previous, start))
            }
            None => None,
        };
        // Only now journal.log, which holds whole every frame listed above.
        let log_len = self.fs.file_len(self.log_path.as_std_path())?;
        if offset > log_len {
            return Err(StoreError::JournalCorrupt { offset });
        }
        // The listed frame just before the read (the last listed one, if the read starts past
        // them all) starts where the one listed before it ends, which a zeroed or stale entry
        // does not, and ends where the read starts. With none before it, the read starts at
        // frame 0, at 0. Its payload CRC is what the first frame read names as the one before.
        let (follows, mut prev_crc) = if let Some((previous, before)) = before {
            self.check_starts_after(previous, before, log_len)?;
            match self.frame_head(before, log_len)? {
                Some(head) => (head.end == offset, head.crc),
                None => (false, 0),
            }
        } else {
            if let Some(entry) = entries.first() {
                self.check_starts_after(None, entry.offset, log_len)?;
            }
            (offset == 0, 0)
        };
        let is_boundary = follows && entries.first().is_none_or(|entry| entry.offset == offset);
        if !is_boundary {
            return Err(StoreError::JournalCorrupt { offset });
        }
        let listed_at = |seq: u64| {
            usize::try_from(seq - first)
                .ok()
                .and_then(|i| entries.get(i).copied())
        };

        let mut frames = Vec::new();
        let mut seq = first;
        let mut at = offset;
        while at < log_len {
            match self.parse_at(log_len, at, Place { seq, prev_crc })? {
                Parsed::Whole { records, size, crc } => {
                    if let Some(entry) = listed_at(seq)
                        && (entry.offset != at || !entry.lists(crc))
                    {
                        return Err(StoreError::JournalCorrupt {
                            offset: entry.offset,
                        });
                    }
                    frames.push(Frame {
                        seq,
                        offset: at,
                        records,
                    });
                    seq += 1;
                    at += bytes::to_u64(size);
                    prev_crc = crc;
                }
                parsed if may_be_torn(&parsed, seq, listed) => break,
                _ => return Err(StoreError::JournalCorrupt { offset: at }),
            }
        }
        // journal.log ended before a listed frame: that frame is missing.
        if let Some(entry) = listed_at(seq) {
            return Err(StoreError::JournalCorrupt {
                offset: entry.offset,
            });
        }
        Ok(Attempt::Read(frames))
    }

    /// Brings the journal to a clean end after a crash, and returns where `journal.log`
    /// was cut, if it was:
    ///
    /// - an unlisted frame at the end that is cut short, fails its CRC or has a length of 0 is a
    ///   torn write: `journal.log` is cut where it starts. No writer writes a length of 0, and
    ///   an append's sync makes its header durable, so a length of 0 is a header that never
    ///   reached the disk: unlisted bytes that are zero to the end of the file (blocks a
    ///   filesystem exposed zero-filled after a crash), or the first sector of an append that a
    ///   power loss lost while it kept a later one;
    /// - whole frames that `journal.off` does not list (a crash between a frame's two
    ///   appends) are committed: `journal.log` is synced, then their entries are appended;
    /// - before it cuts `journal.log` or lists a frame, it checks every listed frame: each
    ///   must start where the one listed before it ends (frame 0 at 0) and read whole, or
    ///   garbage in `journal.off` could make the cut or the new entries destroy or misnumber
    ///   committed frames;
    /// - a partial entry at the end of `journal.off` is dropped, and so are zeroed entries
    ///   past frame 0 before it, up to 512 of them (what a power loss leaves of an entry never
    ///   synced: no frame but the first starts at 0); the frame then left last is checked as
    ///   any last listed frame is;
    /// - anything else is [`StoreError::JournalCorrupt`] and changes nothing: more than 512
    ///   zeroed entries at the end of `journal.off` (at 0: more than a power loss leaves); a
    ///   last listed frame, or the frame listed before it, that does not start where the frame
    ///   listed before it ends; a last listed frame that is cut short, missing (`journal.log` ends
    ///   where it starts), fails its CRC or does not decode (a listed frame's bytes were
    ///   durable before it was listed, so its loss is a filesystem's, never a torn write); a
    ///   listed frame that fails the check before a cut or a listing; an unlisted frame that
    ///   matches its CRC but does not decode (a newer version may have written it);
    /// - a listing that fails is cut back off `journal.off` (what reached it of the entry), and
    ///   is [`StoreError::NotCommitted`]: nothing of the caller's is committed, and the next call
    ///   lists the frame; so is a failed rewrite of the frames whose sync failed (storage spec §2
    ///   rule 3 (b), (c)).
    ///
    /// Only the last listed frame, the headers of the two frames before it, and what follows
    /// are read, unless there is something to cut or list: then every listed frame is.
    /// `journal.off` is repaired before `journal.log` is cut, so a crash in between leaves a torn
    /// frame the next call cuts. The caller must hold [`crate::MetaLock`].
    ///
    /// [`Journal::repair_tail`] does the same, and reports everything it changed.
    pub fn truncate_torn_tail(&self) -> Result<Option<u64>, StoreError> {
        Ok(self.repair_tail()?.cut.map(|cut| cut.at))
    }

    /// [`Journal::truncate_torn_tail`], returning what it changed: the torn tail it cut off
    /// `journal.log`, the whole frames it listed (and of those, the ones it wrote afresh first),
    /// and the bytes it dropped from the end of `journal.off`. [`TailRepair::default`] when the
    /// journal ended cleanly. The report is made of what the check knows already: it makes no
    /// filesystem call of its own. On an error the repairs made before it stay made, and the
    /// report is lost. The caller must hold [`crate::MetaLock`].
    pub fn repair_tail(&self) -> Result<TailRepair, StoreError> {
        let mut tail = self.tail();
        tail.checked = false;
        let (checked, repair) = self.check_tail()?;
        *tail = checked;
        Ok(repair)
    }

    /// The length of `journal.log` as this handle last saw it: when it opened, then after
    /// each [`Journal::append_frame`] (the end of that frame) and
    /// [`Journal::truncate_torn_tail`].
    pub fn len(&self) -> u64 {
        self.tail().len
    }

    /// Whether [`Journal::len`] is 0.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// How many frames `journal.off` lists now, as readers count them: the seq the next listed
    /// frame takes. Its whole entries, measured on this handle (one `fstat`, no [`FsOps`]
    /// call), less the zeroed ones at its end past frame 0, which a power loss leaves of
    /// entries never synced, and which [`Journal::frames_from`], [`Journal::tail_frames_from`]
    /// and recovery drop: one `read_at` of at most the last 513 entries. More than 512 zeroed
    /// entries there are more than a power loss leaves: [`StoreError::JournalCorrupt`] at 0, as
    /// for those readers and recovery. An entry cut since the measurement lists nothing either:
    /// a writer cuts only such zeroed or partial entries, and one whose own append failed.
    pub fn frame_count(&self) -> Result<u64, StoreError> {
        let off_len = self
            .off
            .metadata()
            .map_err(|e| self.off_failed(FileOp::Stat, e))?;
        let whole = off_len.len() / self.entry_len();
        let (from, entries) = self.last_entries(whole)?;
        listing(whole, from, &entries)
    }

    /// The offset up to which the journal is materialized: what [`Journal::set_ckpt`] last
    /// stored, or 0 if it never ran. A file of any length but 8 is `InvalidData`, a failed read
    /// of `meta/journal.ckpt` (named by its place in the store, never by the store's root, a
    /// host path), as is every other failure to read it; the read is capped at 9 bytes, so a
    /// crafted file of huge length is never read into memory.
    ///
    /// FORMAT 2's `journal2.ckpt` holds the seq of the frame at that offset too, and a CRC of
    /// both: a file of any length but 20, or failing its CRC, is `InvalidData`; an offset that
    /// the seq's entry of `journal2.off` does not hold (nor, for the seq past the last entry,
    /// the end of the last listed frame) is [`StoreError::JournalCorrupt`] at the offset.
    pub fn ckpt(&self) -> Result<u64, StoreError> {
        match self.format {
            StoreFormat::V1 => self.ckpt_v1(),
            StoreFormat::V2 => Ok(self.ckpt_v2()?.offset),
        }
    }

    /// [`Journal::ckpt`] in FORMAT 1.
    fn ckpt_v1(&self) -> Result<u64, StoreError> {
        let Some(bytes) = self.read_ckpt(9)? else {
            return Ok(0);
        };
        let offset = <[u8; 8]>::try_from(bytes.as_slice()).map_err(|_| {
            let length = if bytes.len() > 8 {
                "more than 8".to_owned()
            } else {
                bytes.len().to_string()
            };
            self.ckpt_malformed(format!("holds {length} bytes, not a u64le"))
        })?;
        Ok(u64::from_le_bytes(offset))
    }

    /// The FORMAT 2 checkpoint, checked against `journal2.off` (see [`Journal::ckpt`]).
    fn ckpt_v2(&self) -> Result<Ckpt, StoreError> {
        let Some(bytes) = self.read_ckpt(bytes::to_u64(CKPT_LEN_V2 + 1))? else {
            return Ok(Ckpt { offset: 0, seq: 0 });
        };
        let Ok(bytes) = <[u8; CKPT_LEN_V2]>::try_from(bytes.as_slice()) else {
            let length = if bytes.len() > CKPT_LEN_V2 {
                format!("more than {CKPT_LEN_V2}")
            } else {
                bytes.len().to_string()
            };
            let why = format!("holds {length} bytes, not an offset, a seq and their CRC");
            return Err(self.ckpt_malformed(why).into());
        };
        let (fields, crc) = bytes.split_at(16);
        if crc32fast::hash(fields) != u32::from_le_bytes(crc.try_into().expect("4 bytes")) {
            return Err(self.ckpt_malformed("fails its CRC".to_owned()).into());
        }
        let (offset, seq) = fields.split_at(8);
        let ckpt = Ckpt {
            offset: u64::from_le_bytes(offset.try_into().expect("8 bytes")),
            seq: u64::from_le_bytes(seq.try_into().expect("8 bytes")),
        };

        // The entry of the checkpoint's seq holds its offset; past the last listed one, the
        // last listed frame ends there. The zeroed entries a power loss leaves at the end of
        // journal2.off list nothing ([`Journal::frame_count`]): the next frame's, never synced,
        // may be one. Measured on the handles: no FsOps call.
        let listed = self.frame_count()?;
        let agrees = match ckpt.seq.checked_sub(1) {
            None => ckpt.offset == 0,
            Some(_) if ckpt.seq < listed => self.entry(ckpt.seq)? == ckpt.offset,
            Some(last) if ckpt.seq == listed => {
                let log_len = (self.log.metadata())
                    .map_err(|e| self.log_failed(FileOp::Stat, e))?
                    .len();
                self.frame_end(self.entry(last)?, log_len)? == Some(ckpt.offset)
            }
            Some(_) => false,
        };
        if !agrees {
            return Err(StoreError::JournalCorrupt {
                offset: ckpt.offset,
            });
        }
        Ok(ckpt)
    }

    /// The bytes of the checkpoint file, at most `cap` of them; `None` if there is none.
    fn read_ckpt(&self, cap: u64) -> Result<Option<Vec<u8>>, IoFailure> {
        let failed = |e| {
            self.fs
                .locate(FileOp::Fs(FsMethod::Read), &self.ckpt_path, e)
        };
        let mut bytes = Vec::new();
        match fs_ops::open_regular(&self.ckpt_path) {
            Ok(file) => {
                io::Read::read_to_end(&mut io::Read::take(file, cap), &mut bytes).map_err(failed)?
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(failed(e)),
        };
        Ok(Some(bytes))
    }

    /// The checkpoint file is no checkpoint, for the reason `why`: `InvalidData`, a failed read.
    fn ckpt_malformed(&self, why: String) -> IoFailure {
        let e = io::Error::new(io::ErrorKind::InvalidData, why);
        self.fs
            .locate(FileOp::Fs(FsMethod::Read), &self.ckpt_path, e)
    }

    /// Stores `offset` as the checkpoint, replacing the old one atomically and durably.
    ///
    /// FORMAT 2 stores the seq of the frame at `offset` with it: the one past the last frame
    /// this handle appended or listed, when `offset` is where it saw the journal end; else the
    /// frame `journal2.off` lists there. An offset that is no frame boundary it lists (where a
    /// listed frame starts, or where the last one ends) is [`StoreError::BadCall`].
    pub fn set_ckpt(&self, offset: u64) -> Result<(), StoreError> {
        let bytes = match self.format {
            StoreFormat::V1 => offset.to_le_bytes().to_vec(),
            StoreFormat::V2 => ckpt_bytes(offset, self.seq_at(offset)?),
        };
        self.fs
            .atomic_replace(self.ckpt_path.as_std_path(), &bytes)?;
        Ok(())
    }

    /// The seq of the frame at `offset`, a frame boundary: see [`Journal::set_ckpt`].
    fn seq_at(&self, offset: u64) -> Result<u64, StoreError> {
        if offset == 0 {
            return Ok(0);
        }
        let tail = *self.tail();
        if tail.checked && tail.len == offset {
            return Ok(tail.frames);
        }

        // By journal.off: the listed frame that starts at `offset`, or the end of the last one.
        let listed = self.frame_count()?;
        let seq = self.first_at_or_after(offset, listed)?;
        let at_boundary = match listed.checked_sub(1) {
            _ if seq < listed => self.entry(seq)? == offset,
            Some(last) => {
                let log_len = (self.log.metadata())
                    .map_err(|e| self.log_failed(FileOp::Stat, e))?
                    .len();
                self.frame_end(self.entry(last)?, log_len)? == Some(offset)
            }
            None => false,
        };
        if !at_boundary {
            return Err(StoreError::BadCall(format!(
                "a checkpoint at {offset}, where no frame the journal lists starts or ends"
            )));
        }
        Ok(seq)
    }

    /// What [`Journal::append_frame`] returns when the append of its frame to `journal.log`,
    /// which ended at `start`, fails with `e`: [`StoreError::NotCommitted`], once the file is
    /// cut back to `start` and the cut synced, unless it still ends there (nothing of the frame
    /// reached it: measured on this handle's own descriptor, with no [`FsOps`] call). If the cut
    /// fails, the process aborts: see [`Journal::append_frame`].
    fn cut_failed_frame(&self, start: u64, e: IoFailure) -> StoreError {
        let untouched = matches!(self.log.metadata(), Ok(meta) if meta.len() == start);
        if !untouched && let Err(cut) = self.fs.truncate(self.log_path.as_std_path(), start) {
            // A write that fails (a closed pipe, a full log) must not panic: the abort is the
            // point, and `swarmops mcp` would catch the panic and serve on.
            let _ = writeln!(
                io::stderr(),
                "swarmops: fatal: appending a frame to {} failed ({e}), and the frame could not \
                 be cut back off it ({cut}): aborting, so that no later write of this process \
                 lists a frame that may not be on disk",
                self.log_path
            );
            std::process::abort();
        }
        StoreError::NotCommitted { source: e }
    }

    /// The failure `e` of an append to `journal.off`, which held `listed` entries before it,
    /// once whatever of the entry reached the file is cut back off it and the cut synced (unless
    /// the file still ends there, measured on this handle's own descriptor): an entry whose sync
    /// failed must not stay listed while later entries are made durable after it. A cut that
    /// fails too is told beside `e`, as what else failed.
    fn cut_failed_entries(&self, listed: u64, e: IoFailure) -> IoFailure {
        let end = listed * self.entry_len();
        let untouched = matches!(self.off.metadata(), Ok(meta) if meta.len() == end);
        let cut = if untouched {
            Ok(())
        } else {
            self.fs.truncate(self.off_path.as_std_path(), end)
        };
        match cut {
            Ok(()) => e,
            Err(cut) => e.with_also(format!("cutting back what reached it failed too: {cut}")),
        }
    }

    /// What the tail check does when the sync that would make the whole frames `journal.off`
    /// does not list yet durable (`frames`, the frames `seqs`, from offset `from` to the end of
    /// the last of them) fails with `e`. Linux reports a failed writeback once, to the sync that
    /// meets it, and may drop the pages it could not write: no later sync can vouch for those
    /// frames, and one that succeeds would let them be listed while they are not on disk. Nor
    /// may they simply be cut: one may be a committed frame whose listing failed
    /// ([`StoreError::PostCommit`]), durable since its own sync, or frames whose entries a power
    /// loss zeroed. So they are written afresh, from the bytes the tail check read:
    /// `journal.log` is cut back to `from` (the cut synced; it takes a torn tail behind them too)
    /// and they are appended again and synced, at the offsets they had, and the tail check lists
    /// them then. If that append fails, they are cut back off as a failed frame append is, and
    /// the error is [`StoreError::NotCommitted`], naming them by seq: the writer's own
    /// transaction did not commit. If they land anywhere but `from` (another writer's bytes came
    /// in between: one without the lock, or a tail state gone wrong), they are cut back to
    /// `from` and the error is [`StoreError::JournalCorrupt`] at `from`, before anything is
    /// listed: entries at the offsets the check computed would point into other bytes. A cut
    /// that fails aborts the process, saying why on stderr.
    fn write_afresh(
        &self,
        from: u64,
        seqs: RangeInclusive<u64>,
        frames: &[u8],
        e: &IoFailure,
    ) -> Result<(), StoreError> {
        if let Err(cut) = self.fs.truncate(self.log_path.as_std_path(), from) {
            let _ = writeln!(
                io::stderr(),
                "swarmops: fatal: syncing the frames of {} that journal.off does not list yet \
                 failed ({e}), and they could not be cut back off it to be written afresh \
                 ({cut}): aborting, so that no later write of this process lists a frame that \
                 may not be on disk",
                self.log_path
            );
            std::process::abort();
        }
        match self.fs.append_durable(self.log_path.as_std_path(), frames) {
            Ok(landed) if landed == from => Ok(()),
            Ok(landed) => {
                if let Err(cut) = self.fs.truncate(self.log_path.as_std_path(), from) {
                    let _ = writeln!(
                        io::stderr(),
                        "swarmops: fatal: the frames of {} that journal.off does not list yet, \
                         written afresh, landed at {landed} and not at {from}, and they could not \
                         be cut back off it ({cut}): aborting, so that no later write of this \
                         process lists a frame where it is not",
                        self.log_path
                    );
                    std::process::abort();
                }
                Err(StoreError::JournalCorrupt { offset: from })
            }
            Err(again) => {
                let (first, last) = seqs.into_inner();
                let again = again.with_also(format!(
                    "it was writing afresh frames {first}..={last}, which journal.off does not \
                     list yet, after syncing them failed: {e}; they were cut back off the journal"
                ));
                Err(self.cut_failed_frame(from, again))
            }
        }
    }

    fn tail(&self) -> MutexGuard<'_, Tail> {
        // `checked` is cleared before every write that could leave the tail stale, so the
        // value a panicking holder leaves behind is still safe to use.
        self.tail.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether `tail` was checked and the files still end exactly there.
    fn still_ends_at(&self, tail: &Tail) -> Result<bool, IoFailure> {
        let log = || {
            self.log
                .metadata()
                .map_err(|e| self.log_failed(FileOp::Stat, e))
        };
        let off = || {
            self.off
                .metadata()
                .map_err(|e| self.off_failed(FileOp::Stat, e))
        };
        Ok(tail.checked
            && log()?.len() == tail.len
            && off()?.len() == tail.frames * self.entry_len())
    }

    /// The work of [`Journal::repair_tail`]: returns the journal's new end and what it changed.
    fn check_tail(&self) -> Result<(Tail, TailRepair), StoreError> {
        let log_len = self.fs.file_len(self.log_path.as_std_path())?;
        let off_len = self.fs.file_len(self.off_path.as_std_path())?;
        // Zeroed entries past frame 0 at the end are what a power loss leaves of an entry never
        // synced (no frame but the first starts at 0): dropped like a partial entry, up to
        // MAX_ZEROED of them (more is damage, and nothing changes), and the frame left last is
        // checked as the last listed one. Under the lock nothing cuts journal.off: an entry gone
        // since it was measured fails the check.
        let entry_len = self.entry_len();
        let whole = off_len / entry_len;
        let (from, entries) = self.last_entries(whole)?;
        if bytes::to_u64(entries.len()) < whole - from {
            let gone = io::Error::from(io::ErrorKind::UnexpectedEof);
            return Err(self.off_failed(FileOp::ReadAt, gone).into());
        }
        let listed = listing(whole, from, &entries)?;
        // Read from the last listed frame's start: that frame is checked again, and the
        // frames `journal.off` does not list yet follow it.
        let last_entry = match listed.checked_sub(1) {
            Some(last) => self.entry_of(last)?,
            None => Entry {
                offset: 0,
                crc: None,
            },
        };
        let start = last_entry.offset;
        if start > log_len {
            return Err(StoreError::JournalCorrupt { offset: start });
        }
        // Where the unlisted frames begin, and the payload CRC of the frame before them.
        let (mut at, mut prev_crc) = match listed.checked_sub(1) {
            Some(last) => self.check_last_listed(last, last_entry, log_len)?,
            None => (0, 0),
        };
        let mut unlisted = Vec::new(); // each unlisted frame's offset and payload CRC
        let mut cut = None;
        while at < log_len {
            let place = Place {
                seq: listed + bytes::to_u64(unlisted.len()),
                prev_crc,
            };
            match self.parse_at(log_len, at, place)? {
                Parsed::Whole { size, crc, .. } => {
                    unlisted.push((at, crc));
                    at += bytes::to_u64(size);
                    prev_crc = crc;
                }
                parsed if is_torn_unlisted(&parsed) => {
                    cut = Some(at);
                    break;
                }
                _ => return Err(StoreError::JournalCorrupt { offset: at }),
            }
        }

        if cut.is_some() || !unlisted.is_empty() {
            // About to cut journal.log or list frames: garbage in journal.off could make that
            // destroy a committed frame or list one under a wrong seq, so first every listed
            // frame is checked (in FORMAT 2, from the checkpoint's on). Only a crash's repair
            // pays for this.
            self.check_listed_chain(listed, log_len)?;
        }
        if off_len != listed * entry_len {
            self.fs
                .truncate(self.off_path.as_std_path(), listed * entry_len)?;
        }
        let mut rewritten = false;
        if let Some(&(from, _)) = unlisted.first() {
            // The frames' bytes, read before the sync: a failed one may drop the pages.
            let frames = self.read_frames(&unlisted, at)?;
            // Their writer may have died after its write and before its sync, so their bytes
            // may still be only in the page cache: a listed frame must be durable. If the sync
            // fails, no later one can vouch for them: they are written afresh.
            if let Err(e) = self.fs.sync_data(self.log_path.as_std_path()) {
                let seqs = listed..=listed + bytes::to_u64(unlisted.len()) - 1;
                self.write_afresh(from, seqs, &frames, &e)?;
                rewritten = true;
            }
        }
        for (seq, &(offset, crc)) in (listed..).zip(&unlisted) {
            let entry = self.entry_bytes(seq, offset, crc);
            if let Err(e) = self.fs.append_durable(self.off_path.as_std_path(), &entry) {
                // The caller's own transaction never began: nothing of it is committed, and the
                // next tail check lists the frame (storage spec §2 rule 3 (b)).
                return Err(StoreError::NotCommitted {
                    source: self.cut_failed_entries(seq, e),
                });
            }
        }
        // A rewrite cut journal.log back to the first unlisted frame, and the torn tail behind
        // them with it: then there is nothing left to cut, though the report still says the
        // torn tail went.
        if let Some(cut) = cut
            && !rewritten
        {
            self.fs.truncate(self.log_path.as_std_path(), cut)?;
        }
        let tail = Tail {
            len: at,
            frames: listed + bytes::to_u64(unlisted.len()),
            checked: true,
            last_crc: prev_crc,
        };
        let frames_listed = bytes::to_u64(unlisted.len());
        let repair = TailRepair {
            cut: cut.map(|cut| LogCut {
                at: cut,
                bytes: log_len - cut,
            }),
            frames_listed,
            frames_rewritten: if rewritten { frames_listed } else { 0 },
            off_bytes_dropped: off_len - listed * entry_len,
        };
        Ok((tail, repair))
    }

    /// The tail check's check of the last listed frame, `last`, listed by `entry`: it starts
    /// where the frame listed before it ends, and that one where the one before ends; it reads
    /// whole within the `log_len` bytes of `journal.log`. Returns where it ends, and its payload
    /// CRC.
    fn check_last_listed(
        &self,
        last: u64,
        entry: Entry,
        log_len: u64,
    ) -> Result<(u64, u32), StoreError> {
        let start = entry.offset;
        let previous_start = self.entry_before(last)?;
        let prev_crc = self.check_starts_after(previous_start, start, log_len)?;

        // And the frame before it starts where the one listed before that ends (frame 0 at 0),
        // as readers check the frame just before where they read: a zeroed or stale entry there
        // would otherwise wait, behind a clean tail, for a repair to walk journal.off whole.
        if let Some(previous_start) = previous_start {
            self.check_starts_after(self.entry_before(last - 1)?, previous_start, log_len)?;
        }

        // It was whole and durable before it was listed: cut short, missing, failing its CRC,
        // not decoding or away from its place, it is damage, and nothing is cut.
        let place = Place {
            seq: last,
            prev_crc,
        };
        match self.parse_at(log_len, start, place)? {
            Parsed::Whole { size, crc, .. } if entry.lists(crc) => {
                Ok((start + bytes::to_u64(size), crc))
            }
            _ => Err(StoreError::JournalCorrupt { offset: start }),
        }
    }

    /// Checks that the first `listed` entries list sound frames: each starts where the one
    /// listed before it ends (frame 0 at 0) and reads whole, its CRC included (in FORMAT 2, at
    /// its own place, with the payload CRC its entry names). A failure is
    /// [`StoreError::JournalCorrupt`] at that entry's offset. Reads every listed frame, a frame
    /// at a time (its header first, and again with its payload), and the entries 4,096 at a
    /// time; FORMAT 2, whose entries carry checksums of their own, only those from the
    /// checkpoint's seq on.
    fn check_listed_chain(&self, listed: u64, log_len: u64) -> Result<(), StoreError> {
        let from = match self.format {
            StoreFormat::V1 => 0,
            StoreFormat::V2 => self.ckpt_v2()?.seq.min(listed),
        };
        let (mut next, mut prev_crc) = match from.checked_sub(1) {
            None => (0, 0),
            Some(before) => {
                let start = self.entry(before)?;
                match self.frame_head(start, log_len)? {
                    Some(head) => (head.end, head.crc),
                    None => return Err(StoreError::JournalCorrupt { offset: start }),
                }
            }
        };
        let entries = self.entries_of(from, listed - from, Disorder::Stop)?;
        for (seq, entry) in (from..).zip(entries) {
            let start = entry.offset;
            if start != next {
                return Err(StoreError::JournalCorrupt { offset: start });
            }
            let Some(end) = self.frame_end(start, log_len)? else {
                return Err(StoreError::JournalCorrupt { offset: start });
            };
            match self.parse_at(end, start, Place { seq, prev_crc })? {
                Parsed::Whole { size, crc, .. }
                    if bytes::to_u64(size) == end - start && entry.lists(crc) =>
                {
                    prev_crc = crc;
                }
                _ => return Err(StoreError::JournalCorrupt { offset: start }),
            }
            next = end;
        }
        Ok(())
    }

    /// How many frames `journal.off` lists, as a reader counts them: its whole entries,
    /// measured with [`FsOps::file_len`], less the zeroed ones at its end past frame 0, which
    /// recovery drops too. No frame but the first starts at 0: a zeroed entry there is what a
    /// power loss leaves of an entry appended and never synced (the file's length kept, its
    /// bytes lost), and the frame it would list reads as one whole frame not listed yet. More
    /// than [`MAX_ZEROED`] of them is more than a power loss leaves: [`StoreError::JournalCorrupt`]
    /// at 0. One `read_at` of at most the last 513 entries; `None` where it finds fewer than
    /// were measured: a writer cut the file since.
    fn listed(&self) -> Result<Option<u64>, StoreError> {
        let whole = self.fs.file_len(self.off_path.as_std_path())? / self.entry_len();
        let (from, entries) = self.last_entries(whole)?;
        if bytes::to_u64(entries.len()) < whole - from {
            return Ok(None);
        }
        listing(whole, from, &entries).map(Some)
    }

    /// The entries of the first `whole` of `journal.off` that may be zeroed ones to drop (past
    /// frame 0, the last [`MAX_ZEROED`] + 1 at most), and where they start: [`listing`] reads
    /// how many of the `whole` list frames from them. One `read_at`, of 4 KiB and 8 bytes at
    /// most; fewer entries where the file ends before them.
    fn last_entries(&self, whole: u64) -> Result<(u64, Vec<u64>), StoreError> {
        let from = whole - whole.saturating_sub(1).min(MAX_ZEROED + 1);
        Ok((from, self.entries(from, whole - from)?))
    }

    /// Where the frame listed before frame `seq` starts; `None` for frame 0.
    fn entry_before(&self, seq: u64) -> Result<Option<u64>, StoreError> {
        seq.checked_sub(1)
            .map(|previous| self.entry(previous))
            .transpose()
    }

    /// Checks that the listed frame at `start` starts where the frame listed before it, at
    /// `previous_start`, ends (frame 0, with none before it, at 0), and returns that frame's
    /// payload CRC (0 for none, and in FORMAT 1). An entry that does not (zeroed, stale, or
    /// pointing into a frame) is garbage: frames listed or numbered after it would be wrong, and
    /// cutting at it could destroy a whole frame.
    fn check_starts_after(
        &self,
        previous_start: Option<u64>,
        start: u64,
        log_len: u64,
    ) -> Result<u32, StoreError> {
        let previous = match previous_start {
            Some(previous_start) => self.frame_head(previous_start, log_len)?,
            None => Some(Head { end: 0, crc: 0 }),
        };
        match previous {
            Some(head) if head.end == start => Ok(head.crc),
            _ => Err(StoreError::JournalCorrupt { offset: start }),
        }
    }

    /// The size of one entry of `journal.off` in this store's format.
    fn entry_len(&self) -> u64 {
        entry_len(self.format)
    }

    /// The bytes of the entry listing frame `seq` at `offset`, whose payload CRC is `crc` (which
    /// FORMAT 1 does not hold).
    fn entry_bytes(&self, seq: u64, offset: u64, crc: u32) -> Vec<u8> {
        let mut entry = offset.to_le_bytes().to_vec();
        if self.format == StoreFormat::V2 {
            entry.extend_from_slice(&crc.to_le_bytes());
            entry.extend_from_slice(&entry_crc(seq, offset, crc).to_le_bytes());
        }
        entry
    }

    /// Entry `index` of `journal.off`: where frame `index` starts.
    fn entry(&self, index: u64) -> Result<u64, StoreError> {
        Ok(self.entry_of(index)?.offset)
    }

    /// Entry `index` of `journal.off`, as [`Journal::entries_of`] decodes it.
    fn entry_of(&self, index: u64) -> Result<Entry, StoreError> {
        let mut bytes = vec![0; usize::try_from(self.entry_len()).expect("8 or 16")];
        (self.off.read_exact_at(&mut bytes, index * self.entry_len()))
            .map_err(|e| self.off_failed(FileOp::ReadAt, e))?;
        decode_entry(self.format, index, &bytes)
    }

    /// Up to `count` entries of `journal.off` from entry `first`, 4,096 to a `read_at`.
    fn entries(&self, first: u64, count: u64) -> Result<Vec<u64>, StoreError> {
        let entries = self.entries_of(first, count, Disorder::ReadOn)?;
        Ok(entries.iter().map(|entry| entry.offset).collect())
    }

    /// [`Journal::entries`], with each entry's payload CRC (FORMAT 2), read on past an entry
    /// whose offset is not past the one before it or not, as `disorder` says. An entry that does
    /// not match its own checksum is [`StoreError::JournalCorrupt`] at the offset it holds; a
    /// zeroed one reads as offset 0, as in FORMAT 1. Fewer entries where the file ends first.
    fn entries_of(
        &self,
        first: u64,
        count: u64,
        disorder: Disorder,
    ) -> Result<Vec<Entry>, StoreError> {
        let len = self.entry_len();
        let width = usize::try_from(len).expect("8 or 16");
        let end = first.saturating_add(count);
        let mut entries = Vec::new();
        let mut at = first;
        while at < end {
            // 1. One window of entries: a file of any length is never read whole.
            let wanted = (end - at).min(READ_WINDOW);
            let bytes = read_upto(&self.off, at * len, wanted * len)
                .map_err(|e| self.off_failed(FileOp::ReadAt, e))?;

            // 2. Decoded, up to the first out of order when the caller stops there.
            for (seq, entry) in (at..).zip(bytes.chunks_exact(width)) {
                entries.push(decode_entry(self.format, seq, entry)?);
                if disorder == Disorder::Stop && ends_in_disorder(&entries) {
                    return Ok(entries);
                }
            }

            // 3. The file ends within the window: a writer cut it since it was measured.
            if bytes::to_u64(bytes.len()) < wanted * len {
                break;
            }
            at += wanted;
        }
        Ok(entries)
    }

    /// Where the frame at `offset` ends, judged by its header alone; `None` if the header,
    /// or the payload it announces, runs past `log_len`.
    fn frame_end(&self, offset: u64, log_len: u64) -> Result<Option<u64>, StoreError> {
        Ok(self.frame_head(offset, log_len)?.map(|head| head.end))
    }

    /// [`Journal::frame_end`], with the frame's payload CRC (FORMAT 2; 0 in FORMAT 1, which
    /// reads only the length). A listed frame's header, so one with a length of 0 is
    /// [`StoreError::JournalCorrupt`], and in FORMAT 2 one that fails its CRC or announces a
    /// length no writer writes.
    fn frame_head(&self, offset: u64, log_len: u64) -> Result<Option<Head>, StoreError> {
        let header_len = bytes::to_u64(frame::header_len(self.format));
        let header_end = offset.checked_add(header_len);
        if header_end.is_none_or(|header_end| header_end > log_len) {
            return Ok(None);
        }
        let corrupt = StoreError::JournalCorrupt { offset };
        let (len, crc) = match self.format {
            StoreFormat::V1 => {
                let mut len = [0; 4];
                (self.log.read_exact_at(&mut len, offset))
                    .map_err(|e| self.log_failed(FileOp::ReadAt, e))?;
                (u32::from_le_bytes(len), 0)
            }
            StoreFormat::V2 => {
                let mut header = [0; HEADER_LEN_V2];
                (self.log.read_exact_at(&mut header, offset))
                    .map_err(|e| self.log_failed(FileOp::ReadAt, e))?;
                match frame::decode_header_in(self.format, &header) {
                    Ok(Some(header)) => (header.len, header.crc),
                    _ => return Err(corrupt),
                }
            }
        };
        if len == 0 {
            return Err(corrupt);
        }
        let end = offset + header_len + u64::from(len);
        Ok((end <= log_len).then_some(Head { end, crc }))
    }

    /// The bytes of the whole frames `unlisted` names (each one's offset and payload CRC, in
    /// order), the last of them ending at `end`: read a frame at a time, and all there, as the
    /// tail check found them under the lock.
    fn read_frames(&self, unlisted: &[(u64, u32)], end: u64) -> Result<Vec<u8>, IoFailure> {
        let ends = unlisted
            .iter()
            .skip(1)
            .map(|&(offset, _)| offset)
            .chain([end]);
        let mut bytes = Vec::new();
        for (&(start, _), stop) in unlisted.iter().zip(ends) {
            let frame = self.read_log(start, stop - start)?;
            if bytes::to_u64(frame.len()) < stop - start {
                let gone = io::Error::from(io::ErrorKind::UnexpectedEof);
                return Err(self.log_failed(FileOp::ReadAt, gone));
            }
            bytes.extend(frame);
        }
        Ok(bytes)
    }

    /// Parses the frame at `offset`, a frame boundary before `end`, the end of `journal.log` as
    /// measured, which must sit at `place`. Reads the frame itself, its header first: one that
    /// says the frame runs past `end` leaves the payload unread, and so does, in FORMAT 2, a
    /// header that fails its CRC or announces a length no writer writes or a place not its own.
    /// A FORMAT 1 header has no CRC of its own: a payload it announces past the largest one this
    /// build decodes is sorted by its CRC as it streams by ([`Journal::stream_crc`]).
    fn parse_at(&self, end: u64, offset: u64, place: Place) -> Result<Parsed, IoFailure> {
        // 1. The header, which says how long the payload is, and in FORMAT 2 where the frame
        //    belongs. A FORMAT 1 length of 0 is a header that never reached the disk.
        let header_len = frame::header_len(self.format);
        let payload_at = offset.saturating_add(bytes::to_u64(header_len));
        if payload_at > end {
            return Ok(Parsed::Short);
        }
        let header = self.read_log(offset, bytes::to_u64(header_len))?;
        let header = match frame::decode_header_in(self.format, &header) {
            Ok(Some(header)) => header,
            Ok(None) => return Ok(Parsed::Short),
            Err(BadHeader::Crc) => return Ok(Parsed::BadCrc),
            Err(BadHeader::Len) => return Ok(Parsed::Corrupt),
        };
        if header.len == 0 {
            return Ok(Parsed::Invalid { zero_len: true });
        }
        if !header.sits_at(place) {
            return Ok(Parsed::Corrupt);
        }

        // 2. The payload, if the file holds it all: whole, or streamed past the bound.
        let len = u64::from(header.len);
        if payload_at.saturating_add(len) > end {
            return Ok(Parsed::Short);
        }
        if len > bytes::to_u64(MAX_PAYLOAD_LEN) {
            return self.stream_crc(payload_at, len, header.crc);
        }

        // 3. FORMAT 1: the whole frame, its header again, parsed by its codec as ever.
        if self.format == StoreFormat::V1 {
            let frame = self.read_log(offset, payload_at - offset + len)?;
            return Ok(frame::parse_frame(&frame));
        }
        let payload = self.read_log(payload_at, len)?;
        if bytes::to_u64(payload.len()) < len {
            return Ok(Parsed::Short);
        }
        Ok(frame::parse_payload(header_len, header.crc, &payload))
    }

    /// What the FORMAT 1 payload of `len` bytes at `offset`, longer than any this build decodes,
    /// is by its CRC, `crc` by its header, read [`CRC_CHUNK`] bytes at a time: matching, a payload
    /// a writer wrote that does not decode ([`Parsed::Invalid`]); failing, [`Parsed::BadCrc`];
    /// [`Parsed::Short`] if the file ends first.
    fn stream_crc(&self, offset: u64, len: u64, crc: u32) -> Result<Parsed, IoFailure> {
        let mut hasher = crc32fast::Hasher::new();
        let mut done = 0;
        while done < len {
            let wanted = (len - done).min(CRC_CHUNK);
            let chunk = self.read_log(offset + done, wanted)?;
            if bytes::to_u64(chunk.len()) < wanted {
                return Ok(Parsed::Short);
            }
            hasher.update(&chunk);
            done += wanted;
        }
        if hasher.finalize() != crc {
            return Ok(Parsed::BadCrc);
        }
        Ok(Parsed::Invalid { zero_len: false })
    }

    /// The index of the first of the `listed` entries that is `offset` or more (`listed` if
    /// none is), by binary search: the seq of the frame starting at `offset`, or of the frame
    /// after the listed ones if `offset` is where they end.
    fn first_at_or_after(&self, offset: u64, listed: u64) -> Result<u64, StoreError> {
        let (mut low, mut high) = (0, listed);
        while low < high {
            let mid = low + (high - low) / 2;
            if self.entry(mid)? < offset {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        Ok(low)
    }

    /// Up to `len` bytes of `journal.log` from `offset`, as [`read_upto`] reads them.
    fn read_log(&self, offset: u64, len: u64) -> Result<Vec<u8>, IoFailure> {
        read_upto(&self.log, offset, len).map_err(|e| self.log_failed(FileOp::ReadAt, e))
    }

    /// The failure `e` of `op` on this handle's `journal.log`, located.
    fn log_failed(&self, op: FileOp, e: io::Error) -> IoFailure {
        self.fs.locate(op, &self.log_path, e)
    }

    /// The failure `e` of `op` on this handle's `journal.off`, located.
    fn off_failed(&self, op: FileOp, e: io::Error) -> IoFailure {
        self.fs.locate(op, &self.off_path, e)
    }
}

/// How many of the first `whole` entries of `journal.off` list frames, given `entries`, what it
/// holds from entry `from` on ([`Journal::last_entries`]; an entry gone since `whole` was
/// measured is missing there, and lists nothing): `whole`, less the zeroed entries at its end
/// past frame 0, which are dropped (entry 0 lists frame 0, whatever it holds). More than
/// [`MAX_ZEROED`] of them is [`StoreError::JournalCorrupt`] at 0.
fn listing(whole: u64, from: u64, entries: &[u64]) -> Result<u64, StoreError> {
    match entries.iter().rposition(|&entry| entry != 0) {
        Some(last) => Ok(from + bytes::to_u64(last) + 1),
        None if whole - from <= MAX_ZEROED => Ok(from),
        None => Err(StoreError::JournalCorrupt { offset: 0 }),
    }
}

/// The frames `read` gives, read again while it finds `journal.off` shrunk, at most
/// [`OFF_REREADS`] times more; a shrink past that is [`StoreError::JournalCorrupt`] at the
/// offset the last one names. Each read starts with its own measurement of `journal.off`, and
/// finds the shrink before it measures `journal.log`.
fn reread(read: impl Fn() -> Result<Attempt, StoreError>) -> Result<Vec<Frame>, StoreError> {
    let mut rereads = 0;
    loop {
        match read()? {
            Attempt::Read(frames) => return Ok(frames),
            Attempt::Shrunk { offset } if rereads == OFF_REREADS => {
                return Err(StoreError::JournalCorrupt { offset });
            }
            Attempt::Shrunk { .. } => rereads += 1,
        }
    }
}

/// What a read of `journal.off` entries gave, or `None` where it met the end of the file before
/// an entry it was measured to hold (`UnexpectedEof`): a writer cut it since.
fn unless_cut<T>(read: Result<T, StoreError>) -> Result<Option<T>, StoreError> {
    match read {
        Ok(read) => Ok(Some(read)),
        Err(StoreError::Io(e)) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(None),
        Err(e) => Err(e),
    }
}

/// Whether a frame that did not parse whole may be a torn write rather than damage, given
/// its seq, how many frames `journal.off` listed when it was measured (before `journal.log`),
/// and `rest`, the bytes from its start to the end of `journal.log`. Only a frame not listed
/// then may: a listed one was whole and durable before its entry was written, so it is whole
/// in any `journal.log` measured after.
fn may_be_torn(parsed: &Parsed, seq: u64, listed: u64) -> bool {
    seq >= listed && is_torn_unlisted(parsed)
}

/// Whether unlisted bytes that do not parse as a whole frame are a torn tail: a frame cut
/// short by the end of the file (the append a crash tore, or one still in progress), one
/// that fails its CRC, or one whose length reads 0. No writer writes a frame of length 0, and
/// the sync an append ends with makes its header durable: a length of 0 is a header that never
/// reached the disk, so its frame never reached its commit point and nothing a writer committed
/// lies after it. Zeros a filesystem exposes after a crash in place of blocks it never wrote
/// (to the end of the file) read so, and so does the first sector of an append that a power
/// loss lost while it kept a later one (bytes that are not zero after the header). A frame that
/// matches its CRC but does not decode is no torn write: a writer wrote it, a newer version
/// perhaps. In FORMAT 2 a header that fails its own CRC is torn too (zeros among them), and one
/// that matches it is never torn: FORMAT 2 has no header of length 0, and a header that checks
/// cannot carry a seq or a previous CRC a torn write gave it ([`Parsed::Corrupt`]).
fn is_torn_unlisted(parsed: &Parsed) -> bool {
    match parsed {
        Parsed::Short | Parsed::BadCrc => true,
        // With its header whole: a length of 0, or a payload that matches its CRC and does not
        // decode.
        Parsed::Invalid { zero_len } => *zero_len,
        Parsed::Whole { .. } | Parsed::Corrupt => false,
    }
}

/// Up to `len` bytes of `file` from `offset`; fewer if the file ends first (another
/// process may have cut it since its length was read). A read of more than [`MAX_READ`] bytes,
/// which no caller makes, is refused (`InvalidInput`) before anything is held.
fn read_upto(file: &File, offset: u64, len: u64) -> io::Result<Vec<u8>> {
    if len > MAX_READ {
        let why = format!("a read of {len} bytes, past the longest frame");
        return Err(io::Error::new(io::ErrorKind::InvalidInput, why));
    }
    let len = usize::try_from(len).map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
    let mut buf = vec![0; len];
    let mut filled = 0;
    while filled < len {
        match file.read_at(&mut buf[filled..], offset + bytes::to_u64(filled)) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    buf.truncate(filled);
    Ok(buf)
}

/// Whether the last of `entries` holds an offset that is not past the one before it: no frame
/// it lists has that offset, since each starts where the one before it ends.
fn ends_in_disorder(entries: &[Entry]) -> bool {
    match entries {
        [.., before, last] => last.offset <= before.offset,
        _ => false,
    }
}

/// The size of one entry of `journal.off` in `format`.
fn entry_len(format: StoreFormat) -> u64 {
    match format {
        StoreFormat::V1 => ENTRY_LEN,
        StoreFormat::V2 => ENTRY_LEN_V2,
    }
}

/// The checksum of a FORMAT 2 entry: the CRC of `u64le seq | u64le offset | u32le crc`.
fn entry_crc(seq: u64, offset: u64, crc: u32) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(&seq.to_le_bytes());
    hasher.update(&offset.to_le_bytes());
    hasher.update(&crc.to_le_bytes());
    hasher.finalize()
}

/// Entry `seq` of a `journal.off` of `format`, its bytes `bytes` (8 or 16 of them). A FORMAT 2
/// entry of zeros reads as offset 0, a zeroed entry; one that does not match its checksum is
/// [`StoreError::JournalCorrupt`] at the offset it holds.
fn decode_entry(format: StoreFormat, seq: u64, bytes: &[u8]) -> Result<Entry, StoreError> {
    let u64_at = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().expect("8 bytes"));
    let u32_at = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().expect("4 bytes"));
    let offset = u64_at(0);
    if format == StoreFormat::V1 || bytes.iter().all(|&b| b == 0) {
        return Ok(Entry { offset, crc: None });
    }
    let crc = u32_at(8);
    if entry_crc(seq, offset, crc) != u32_at(12) {
        return Err(StoreError::JournalCorrupt { offset });
    }
    Ok(Entry {
        offset,
        crc: Some(crc),
    })
}

/// The bytes of a FORMAT 2 checkpoint at `offset`, the start of frame `seq`.
fn ckpt_bytes(offset: u64, seq: u64) -> Vec<u8> {
    let mut bytes = offset.to_le_bytes().to_vec();
    bytes.extend_from_slice(&seq.to_le_bytes());
    let crc = crc32fast::hash(&bytes);
    bytes.extend_from_slice(&crc.to_le_bytes());
    bytes
}

impl Entry {
    /// Whether the entry lists a frame whose payload CRC is `crc`: always in FORMAT 1, and for a
    /// zeroed entry, whose offset is checked instead.
    fn lists(&self, crc: u32) -> bool {
        self.crc.is_none_or(|listed| listed == crc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No read of a journal file is longer than one frame of the largest payload: a longer one,
    /// which no caller makes, is refused before a byte is held.
    #[test]
    fn a_read_past_the_longest_frame_is_refused() {
        let file = tempfile::tempfile().unwrap();
        file.set_len(MAX_READ + 1).unwrap();
        let refused = read_upto(&file, 0, MAX_READ + 1).map(|bytes| bytes.len());
        assert!(
            matches!(&refused, Err(e) if e.kind() == io::ErrorKind::InvalidInput),
            "{refused:?}"
        );
        assert_eq!(read_upto(&file, MAX_READ, 1).unwrap(), [0]);
    }
}
