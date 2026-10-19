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
//! append fails is cut back off `journal.off` ([`StoreError::PostCommit`]: its frame is durable
//! and committed, and the next tail check lists it afresh). When the tail check's sync of the
//! frames it is about to list fails, it writes them afresh (cut back off, appended again and
//! synced) and lists them then; if that append fails, they are cut back off too.
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
//! an entry never synced (no frame but the first starts at 0). They read
//! with `read_at` on read-only handles, except that `journal.ckpt`, which
//! [`Journal::set_ckpt`] replaces by rename, is read by path.

use std::fmt;
use std::fs::File;
use std::io::{self, Write};
use std::os::unix::fs::FileExt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use camino::Utf8PathBuf;

use super::frame::{Frame, HEADER_LEN, Parsed, Record, encode_frame, parse_frame};
use super::to_u64;
use crate::error::StoreError;
use crate::fs_ops::{FsOps, open_regular};
use crate::layout::Layout;

/// The size of one `journal.off` entry.
const ENTRY_LEN: u64 = 8;

/// How many times [`Journal::frames_from`] and [`Journal::tail_frames_from`] start over when
/// `journal.off` holds fewer entries when they read them than when they measured it. Only a
/// writer cuts it, under the lock, and only entries that were never genuine (one whose own append
/// failed, or zeroed or partial ones a crash left at its end), so the next read nearly always
/// finds it still; a shrink every time is reported as damage.
const OFF_REREADS: u32 = 3;

/// A handle on one store's metadata journal.
pub struct Journal {
    log_path: Utf8PathBuf,
    off_path: Utf8PathBuf,
    ckpt_path: Utf8PathBuf,
    /// Every durable write goes through here, and so do the readers' measurements of the
    /// files' lengths (`journal.off` first).
    fs: Arc<dyn FsOps>,
    /// Read-only handle on `journal.log`.
    log: File,
    /// Read-only handle on `journal.off`.
    off: File,
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
        let meta = layout.meta_dir();
        fs.create_dir_all(meta.as_std_path())?;
        for path in [layout.journal_log(), layout.journal_off()] {
            // An empty append creates a missing file and never truncates an existing one.
            fs.append_durable(path.as_std_path(), &[])?;
        }
        fs.fsync_dir(meta.as_std_path())?;
        fs.fsync_dir(layout.root().as_std_path())?;
        Self::open(layout, fs)
    }

    /// Opens the existing journal of `layout`, durable writes going through `fs`. Opening
    /// reads only the files' lengths; the tail is checked by the first
    /// [`Journal::append_frame`] or [`Journal::truncate_torn_tail`].
    pub fn open(layout: &Layout, fs: Arc<dyn FsOps>) -> Result<Journal, StoreError> {
        let log = open_regular(layout.journal_log())?;
        let off = open_regular(layout.journal_off())?;
        let tail = Tail {
            len: log.metadata()?.len(),
            frames: off.metadata()?.len() / ENTRY_LEN,
            checked: false,
        };
        Ok(Self {
            log_path: layout.journal_log(),
            off_path: layout.journal_off(),
            ckpt_path: layout.journal_ckpt(),
            fs,
            log,
            off,
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
    /// never lands after a torn one. Records the layout cannot hold are refused before
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
        let bytes = encode_frame(records)?;
        let mut tail = self.tail();
        if !self.still_ends_at(&tail)? {
            tail.checked = false;
            (*tail, _) = self.check_tail()?;
        }
        tail.checked = false; // a failed append can leave part of a frame or an entry behind
        let offset = match self.fs.append_durable(self.log_path.as_std_path(), &bytes) {
            Ok(offset) => offset,
            Err(e) => return Err(self.cut_failed_frame(tail.len, e)),
        };
        // The commit point: the frame is whole and durable.
        let entry = offset.to_le_bytes();
        let entry_at = match self.fs.append_durable(self.off_path.as_std_path(), &entry) {
            Ok(entry_at) => entry_at,
            Err(e) => {
                let source = self.cut_failed_entries(tail.frames, &e);
                return Err(StoreError::PostCommit {
                    frame_seq: tail.frames,
                    source: Box::new(source.into()),
                });
            }
        };
        let seq = entry_at / ENTRY_LEN;
        *tail = Tail {
            len: offset + to_u64(bytes.len()),
            frames: seq + 1,
            checked: true,
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
    /// wanted frames', and the next one) with one `read_at`, the previous frame's header with
    /// another (frame 0 has none: it starts at 0), and the frames' bytes with a third.
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
    /// recovery drops them (see [`Journal::tail_frames_from`]).
    pub fn frames_from(&self, first: u64, limit: usize) -> Result<Vec<Frame>, StoreError> {
        reread(|| self.frames_from_once(first, limit))
    }

    /// One read of [`Journal::frames_from`]: [`Attempt::Shrunk`] where `journal.off` holds fewer
    /// entries when they are read than it did when measured.
    fn frames_from_once(&self, first: u64, limit: usize) -> Result<Attempt, StoreError> {
        // journal.off cut since it was measured: entries of the snapshot are gone.
        let Some(listed) = unless_cut(self.listed())? else {
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
        let entries = self.entries(previous.unwrap_or(first), count)?;
        let skip = usize::from(previous.is_some());
        if to_u64(entries.len()) != count {
            // journal.off was cut since it was measured: entries of the snapshot are gone.
            let offset = entries.get(skip).copied().unwrap_or(0);
            return Ok(Attempt::Shrunk { offset });
        }
        let previous = previous.map(|_| entries[0]);
        let offsets = &entries[skip..];
        let wanted = usize::try_from(wanted).map_or(offsets.len(), |n| n.min(offsets.len()));
        let start = offsets[0];
        // Only now journal.log, which holds whole every frame listed above.
        let log_len = self.fs.file_len(self.log_path.as_std_path())?;
        self.check_starts_after(previous, start, log_len)?;
        let end = offsets
            .get(wanted)
            .map_or(log_len, |&next| next.min(log_len));
        if start > end {
            return Err(StoreError::JournalCorrupt { offset: start });
        }
        let buf = read_upto(&self.log, start, end - start)?;

        let mut frames = Vec::with_capacity(wanted);
        let mut at = 0;
        for (frame_seq, &offset) in (first..).zip(&offsets[..wanted]) {
            if offset != start + to_u64(at) {
                return Err(StoreError::JournalCorrupt { offset });
            }
            match parse_frame(&buf[at..]) {
                Parsed::Whole { records, size } => {
                    frames.push(Frame {
                        seq: frame_seq,
                        offset,
                        records,
                    });
                    at += size;
                }
                _ => return Err(StoreError::JournalCorrupt { offset }),
            }
        }
        if let Some(&next) = offsets.get(wanted)
            && next != start + to_u64(at)
        {
            return Err(StoreError::JournalCorrupt { offset: next });
        }
        Ok(Attempt::Read(frames))
    }

    /// Every frame from `offset` to the end of `journal.log`, read with one `read_at`: what
    /// a reader applies past [`Journal::ckpt`]. `offset` must be a frame boundary: 0, a
    /// listed frame's start, or the end of the last listed frame. The listed frame just before
    /// the read (the last listed one, if the read starts past them all) must start where the
    /// one listed before it ends (frame 0 at 0), as recovery checks it too: a zeroed or stale
    /// entry there is [`StoreError::JournalCorrupt`]. Zeroed entries at the end of
    /// `journal.off`, past frame 0, are not listings: a power loss between an entry's append
    /// and its sync may keep the file's length and lose the bytes. They are dropped first, as
    /// recovery drops them, and the frames they would list read as whole frames not listed yet.
    ///
    /// `journal.off` is measured, and the entries read, before `journal.log`: every frame
    /// listed then was whole and durable before its entry was written, so it is whole in
    /// `journal.log` as measured after, where its entry says. One that is not (cut short,
    /// missing, failing its CRC, not decoding, or away from its entry, as a zeroed or stale
    /// entry puts it) is [`StoreError::JournalCorrupt`]. Whole frames after the listed ones
    /// are included: an append racing the read, which lists them later, or a crash between a
    /// frame's two appends. The read stops silently at a torn tail: an unlisted frame cut
    /// short (an append in progress looks the same), failing its CRC, or whose length reads 0
    /// (zeros to the end of the file among them; see [`Journal::truncate_torn_tail`]). Anything
    /// else, an `offset` that is not a frame boundary included, is
    /// [`StoreError::JournalCorrupt`]. A `journal.off` that holds fewer entries when they are
    /// read than it did when measured is no damage, as [`Journal::frames_from`] says: the read
    /// starts over, up to three times, and a shrink every time is reported at `offset`.
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
        let Some(listed) = unless_cut(self.listed())? else {
            return shrunk();
        };
        let Some(first) = unless_cut(self.first_at_or_after(offset, listed))? else {
            return shrunk();
        };
        // Where each listed frame from `first` on starts, and where the listed frame just
        // before the read and the one before that start.
        let entries = self.entries(first, listed - first)?;
        if to_u64(entries.len()) != listed - first {
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
        // frame 0, at 0.
        let follows = if let Some((previous, before)) = before {
            self.check_starts_after(previous, before, log_len)?;
            self.frame_end(before, log_len)? == Some(offset)
        } else {
            if let Some(&start) = entries.first() {
                self.check_starts_after(None, start, log_len)?;
            }
            offset == 0
        };
        let is_boundary = follows && entries.first().is_none_or(|&start| start == offset);
        if !is_boundary {
            return Err(StoreError::JournalCorrupt { offset });
        }
        let listed_at = |seq: u64| {
            usize::try_from(seq - first)
                .ok()
                .and_then(|i| entries.get(i).copied())
        };
        let buf = read_upto(&self.log, offset, log_len - offset)?;

        let mut frames = Vec::new();
        let mut seq = first;
        let mut at = 0;
        while at < buf.len() {
            let frame_offset = offset + to_u64(at);
            match parse_frame(&buf[at..]) {
                Parsed::Whole { records, size } => {
                    if let Some(entry) = listed_at(seq)
                        && entry != frame_offset
                    {
                        return Err(StoreError::JournalCorrupt { offset: entry });
                    }
                    frames.push(Frame {
                        seq,
                        offset: frame_offset,
                        records,
                    });
                    seq += 1;
                    at += size;
                }
                parsed if may_be_torn(&parsed, seq, listed, &buf[at..]) => break,
                _ => {
                    return Err(StoreError::JournalCorrupt {
                        offset: frame_offset,
                    });
                }
            }
        }
        // journal.log ended before a listed frame: that frame is missing.
        if let Some(entry) = listed_at(seq) {
            return Err(StoreError::JournalCorrupt { offset: entry });
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
    ///   past frame 0 before it (a zero-filled block: no frame but the first starts at 0);
    ///   the frame then left last is checked as any last listed frame is;
    /// - anything else is [`StoreError::JournalCorrupt`] and changes nothing: a last listed
    ///   frame, or the frame listed before it, that does not start where the frame listed
    ///   before it ends; a last listed frame that is cut short, missing (`journal.log` ends
    ///   where it starts), fails its CRC or does not decode (a listed frame's bytes were
    ///   durable before it was listed, so its loss is a filesystem's, never a torn write); a
    ///   listed frame that fails the check before a cut or a listing; an unlisted frame that
    ///   matches its CRC but does not decode (a newer version may have written it).
    ///
    /// Only the last listed frame, the headers of the two frames before it, and what follows
    /// are read, unless there is something to cut or list: then every listed frame is.
    /// `journal.off` is repaired before `journal.log` is cut, so a crash in between leaves a torn
    /// frame the next call cuts. The caller must hold [`crate::MetaLock`].
    pub fn truncate_torn_tail(&self) -> Result<Option<u64>, StoreError> {
        let mut tail = self.tail();
        tail.checked = false;
        let (checked, cut) = self.check_tail()?;
        *tail = checked;
        Ok(cut)
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
    /// and recovery drop: one `read_at` of the last entry, and one more per zeroed one. An
    /// entry cut since the measurement lists nothing either: a writer cuts only such zeroed or
    /// partial entries, and one whose own append failed.
    pub fn frame_count(&self) -> Result<u64, StoreError> {
        let mut listed = self.off.metadata()?.len() / ENTRY_LEN;
        while listed > 1 {
            match self.entry(listed - 1) {
                Ok(0) => listed -= 1,
                Ok(_) => break,
                Err(StoreError::Io(e)) if e.kind() == io::ErrorKind::UnexpectedEof => listed -= 1,
                Err(e) => return Err(e),
            }
        }
        Ok(listed)
    }

    /// The offset up to which the journal is materialized: what [`Journal::set_ckpt`] last
    /// stored, or 0 if it never ran. A file of any length but 8 is `InvalidData`, its message
    /// naming `meta/journal.ckpt` by its place in the store (never the store's root, a host
    /// path); the read is capped at 9 bytes, so a crafted file of huge length is never read
    /// into memory.
    pub fn ckpt(&self) -> Result<u64, StoreError> {
        let mut bytes = Vec::with_capacity(9);
        match open_regular(&self.ckpt_path) {
            Ok(file) => io::Read::read_to_end(&mut io::Read::take(file, 9), &mut bytes)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e.into()),
        };
        let offset = <[u8; 8]>::try_from(bytes.as_slice()).map_err(|_| {
            let length = if bytes.len() > 8 {
                "more than 8".to_owned()
            } else {
                bytes.len().to_string()
            };
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("meta/journal.ckpt holds {length} bytes, not a u64le"),
            )
        })?;
        Ok(u64::from_le_bytes(offset))
    }

    /// Stores `offset` as the checkpoint, replacing the old one atomically and durably.
    pub fn set_ckpt(&self, offset: u64) -> Result<(), StoreError> {
        self.fs
            .atomic_replace(self.ckpt_path.as_std_path(), &offset.to_le_bytes())?;
        Ok(())
    }

    /// What [`Journal::append_frame`] returns when the append of its frame to `journal.log`,
    /// which ended at `start`, fails with `e`: [`StoreError::NotCommitted`], once the file is
    /// cut back to `start` and the cut synced, unless it still ends there (nothing of the frame
    /// reached it: measured on this handle's own descriptor, with no [`FsOps`] call). If the cut
    /// fails, the process aborts: see [`Journal::append_frame`].
    fn cut_failed_frame(&self, start: u64, e: io::Error) -> StoreError {
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

    /// The error of a failed append `e` to `journal.off`, which held `listed` entries before
    /// it, once whatever of the entry reached the file is cut back off it and the cut synced
    /// (unless the file still ends there, measured on this handle's own descriptor): an entry
    /// whose sync failed must not stay listed while later entries are made durable after it. A
    /// cut that fails too is told beside `e`.
    fn cut_failed_entries(&self, listed: u64, e: &io::Error) -> io::Error {
        let end = listed * ENTRY_LEN;
        let untouched = matches!(self.off.metadata(), Ok(meta) if meta.len() == end);
        let cut = if untouched {
            Ok(())
        } else {
            self.fs.truncate(self.off_path.as_std_path(), end)
        };
        let what = format!("listing a frame in journal.off failed: {e}");
        match cut {
            Ok(()) => io::Error::new(e.kind(), what),
            Err(cut) => io::Error::new(
                e.kind(),
                format!("{what}; cutting back what reached journal.off failed too: {cut}"),
            ),
        }
    }

    /// What the tail check does when the sync that would make the whole frames `journal.off`
    /// does not list yet durable (`frames`, from offset `from` to the end of the last of them)
    /// fails with `e`. Linux reports a failed writeback once, to the sync that meets it, and may
    /// drop the pages it could not write: no later sync can vouch for those frames, and one
    /// that succeeds would let them be listed while they are not on disk. Nor may they simply
    /// be cut: one may be a committed frame whose listing failed ([`StoreError::PostCommit`]),
    /// durable since its own sync, or frames whose entries a power loss zeroed. So they are
    /// written afresh, from the bytes the tail check read: `journal.log` is cut back to `from`
    /// (the cut synced) and they are appended again and synced, at the offsets they had, and
    /// the tail check lists them then. If that append fails, they are cut back off as a failed
    /// frame append is, and the error is [`StoreError::NotCommitted`]: the writer's own
    /// transaction did not commit. A cut that fails aborts the process, saying why on stderr.
    fn write_afresh(&self, from: u64, frames: &[u8], e: &io::Error) -> Result<(), StoreError> {
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
            Ok(_) => Ok(()),
            Err(again) => {
                let again = io::Error::new(
                    again.kind(),
                    format!(
                        "syncing the frames journal.off does not list yet failed ({e}), and so \
                         did writing them afresh: {again}"
                    ),
                );
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
    fn still_ends_at(&self, tail: &Tail) -> io::Result<bool> {
        Ok(tail.checked
            && self.log.metadata()?.len() == tail.len
            && self.off.metadata()?.len() == tail.frames * ENTRY_LEN)
    }

    /// The work of [`Journal::truncate_torn_tail`]: returns the journal's new end and
    /// where `journal.log` was cut.
    fn check_tail(&self) -> Result<(Tail, Option<u64>), StoreError> {
        let log_len = self.fs.file_len(self.log_path.as_std_path())?;
        let off_len = self.fs.file_len(self.off_path.as_std_path())?;
        let mut listed = off_len / ENTRY_LEN;
        // Read from the last listed frame's start: that frame is checked again, and the
        // frames `journal.off` does not list yet follow it.
        let mut start = match listed.checked_sub(1) {
            Some(last) => self.entry(last)?,
            None => 0,
        };
        // Zeroed entries past frame 0 at the end are what a zero-filled block leaves (no
        // frame but the first starts at 0): dropped like a partial entry, and the frame left
        // last is checked as the last listed one.
        while start == 0 && listed > 1 {
            listed -= 1;
            start = self.entry(listed - 1)?;
        }
        if start > log_len {
            return Err(StoreError::JournalCorrupt { offset: start });
        }
        let buf = read_upto(&self.log, start, log_len - start)?;
        let mut at = 0; // where in `buf` the unlisted frames begin
        if let Some(last) = listed.checked_sub(1) {
            let previous_start = self.entry_before(last)?;
            self.check_starts_after(previous_start, start, log_len)?;
            // And the frame before it starts where the one listed before that ends (frame 0
            // at 0), as readers check the frame just before where they read: a zeroed or
            // stale entry there would otherwise wait, behind a clean tail, for a repair to walk
            // journal.off whole.
            if let Some(previous_start) = previous_start {
                self.check_starts_after(self.entry_before(last - 1)?, previous_start, log_len)?;
            }
            // It was whole and durable before it was listed: cut short, missing, failing its
            // CRC or not decoding, it is damage, and nothing is cut.
            match parse_frame(&buf) {
                Parsed::Whole { size, .. } => at = size,
                _ => return Err(StoreError::JournalCorrupt { offset: start }),
            }
        }
        let unlisted_at = at;
        let mut unlisted = Vec::new();
        let mut cut = None;
        while at < buf.len() {
            let offset = start + to_u64(at);
            match parse_frame(&buf[at..]) {
                Parsed::Whole { size, .. } => {
                    unlisted.push(offset);
                    at += size;
                }
                parsed if is_torn_unlisted(&parsed, &buf[at..]) => {
                    cut = Some(offset);
                    break;
                }
                _ => return Err(StoreError::JournalCorrupt { offset }),
            }
        }

        if cut.is_some() || !unlisted.is_empty() {
            // About to cut journal.log or list frames: garbage in journal.off could make that
            // destroy a committed frame or list one under a wrong seq, so first every listed
            // frame is checked. Only a crash's repair pays for this.
            self.check_listed_chain(listed, log_len)?;
        }
        if off_len != listed * ENTRY_LEN {
            self.fs
                .truncate(self.off_path.as_std_path(), listed * ENTRY_LEN)?;
        }
        if let Some(&from) = unlisted.first() {
            // Their writer may have died after its write and before its sync, so their bytes
            // may still be only in the page cache: a listed frame must be durable. If the sync
            // fails, no later one can vouch for them: they are written afresh.
            if let Err(e) = self.fs.sync_data(self.log_path.as_std_path()) {
                self.write_afresh(from, &buf[unlisted_at..at], &e)?;
            }
        }
        for (at, offset) in (listed..).zip(&unlisted) {
            let entry = offset.to_le_bytes();
            if let Err(e) = self.fs.append_durable(self.off_path.as_std_path(), &entry) {
                return Err(self.cut_failed_entries(at, &e).into());
            }
        }
        if let Some(cut) = cut {
            self.fs.truncate(self.log_path.as_std_path(), cut)?;
        }
        let tail = Tail {
            len: start + to_u64(at),
            frames: listed + to_u64(unlisted.len()),
            checked: true,
        };
        Ok((tail, cut))
    }

    /// Checks that the first `listed` entries list sound frames: each starts where the one
    /// listed before it ends (frame 0 at 0) and reads whole, its CRC included. A failure is
    /// [`StoreError::JournalCorrupt`] at that entry's offset. Reads every listed frame, one
    /// `read_at` each (and its header with another).
    fn check_listed_chain(&self, listed: u64, log_len: u64) -> Result<(), StoreError> {
        let mut next = 0;
        for start in self.entries(0, listed)? {
            if start != next {
                return Err(StoreError::JournalCorrupt { offset: start });
            }
            let Some(end) = self.frame_end(start, log_len)? else {
                return Err(StoreError::JournalCorrupt { offset: start });
            };
            let bytes = read_upto(&self.log, start, end - start)?;
            if !matches!(parse_frame(&bytes), Parsed::Whole { size, .. } if size == bytes.len()) {
                return Err(StoreError::JournalCorrupt { offset: start });
            }
            next = end;
        }
        Ok(())
    }

    /// How many frames `journal.off` lists, as a reader counts them: its whole entries,
    /// measured with [`FsOps::file_len`], less the zeroed ones at its end past frame 0, which
    /// recovery drops too. No frame but the first starts at 0: a zeroed entry there is what a
    /// power loss leaves of an entry appended and never synced (the file's length kept, its
    /// bytes lost), and the frame it would list reads as one whole frame not listed yet. One
    /// `read_at` of the last entry, and one more per zeroed entry.
    fn listed(&self) -> Result<u64, StoreError> {
        let mut listed = self.fs.file_len(self.off_path.as_std_path())? / ENTRY_LEN;
        while listed > 1 && self.entry(listed - 1)? == 0 {
            listed -= 1;
        }
        Ok(listed)
    }

    /// Where the frame listed before frame `seq` starts; `None` for frame 0.
    fn entry_before(&self, seq: u64) -> Result<Option<u64>, StoreError> {
        seq.checked_sub(1)
            .map(|previous| self.entry(previous))
            .transpose()
    }

    /// Checks that the listed frame at `start` starts where the frame listed before it, at
    /// `previous_start`, ends (frame 0, with none before it, at 0). An entry that does not
    /// (zeroed, stale, or pointing into a frame) is garbage: frames listed or numbered after
    /// it would be wrong, and cutting at it could destroy a whole frame.
    fn check_starts_after(
        &self,
        previous_start: Option<u64>,
        start: u64,
        log_len: u64,
    ) -> Result<(), StoreError> {
        let previous_end = match previous_start {
            Some(previous_start) => self.frame_end(previous_start, log_len)?,
            None => Some(0),
        };
        if previous_end == Some(start) {
            Ok(())
        } else {
            Err(StoreError::JournalCorrupt { offset: start })
        }
    }

    /// Entry `index` of `journal.off`: where frame `index` starts.
    fn entry(&self, index: u64) -> Result<u64, StoreError> {
        let mut bytes = [0; 8];
        self.off.read_exact_at(&mut bytes, index * ENTRY_LEN)?;
        Ok(u64::from_le_bytes(bytes))
    }

    /// Up to `count` entries of `journal.off` from entry `first`, in one `read_at`.
    fn entries(&self, first: u64, count: u64) -> Result<Vec<u64>, StoreError> {
        let bytes = read_upto(&self.off, first * ENTRY_LEN, count * ENTRY_LEN)?;
        let (entries, _partial) = bytes.as_chunks::<8>();
        Ok(entries
            .iter()
            .map(|entry| u64::from_le_bytes(*entry))
            .collect())
    }

    /// Where the frame at `offset` ends, judged by its header alone; `None` if the header,
    /// or the payload it announces, runs past `log_len`.
    fn frame_end(&self, offset: u64, log_len: u64) -> Result<Option<u64>, StoreError> {
        let header_end = offset.checked_add(to_u64(HEADER_LEN));
        if header_end.is_none_or(|header_end| header_end > log_len) {
            return Ok(None);
        }
        let mut len = [0; 4];
        self.log.read_exact_at(&mut len, offset)?;
        let len = u32::from_le_bytes(len);
        if len == 0 {
            return Err(StoreError::JournalCorrupt { offset });
        }
        let end = offset + to_u64(HEADER_LEN) + u64::from(len);
        Ok((end <= log_len).then_some(end))
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
fn may_be_torn(parsed: &Parsed, seq: u64, listed: u64, rest: &[u8]) -> bool {
    seq >= listed && is_torn_unlisted(parsed, rest)
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
/// perhaps. `rest` runs from where the frame would start to the end of `journal.log`.
fn is_torn_unlisted(parsed: &Parsed, rest: &[u8]) -> bool {
    match parsed {
        Parsed::Short | Parsed::BadCrc => true,
        // With its header whole: a length of 0, or a payload that matches its CRC and does not
        // decode.
        Parsed::Invalid => rest.starts_with(&[0; 4]),
        Parsed::Whole { .. } => false,
    }
}

/// Up to `len` bytes of `file` from `offset`; fewer if the file ends first (another
/// process may have cut it since its length was read).
fn read_upto(file: &File, offset: u64, len: u64) -> io::Result<Vec<u8>> {
    let len = usize::try_from(len).map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
    let mut buf = vec![0; len];
    let mut filled = 0;
    while filled < len {
        match file.read_at(&mut buf[filled..], offset + to_u64(filled)) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    buf.truncate(filled);
    Ok(buf)
}
