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
//! Writers hold [`crate::MetaLock`]; the journal itself takes no lock. Writes go through
//! [`FsOps`]. Reads take no lock and skip a torn tail (or an append still in progress)
//! silently; they use `read_at` on read-only handles, except that `journal.ckpt`, which
//! [`Journal::set_ckpt`] replaces by rename, is read by path.

use std::fs::{self, File};
use std::io;
use std::os::unix::fs::FileExt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use camino::Utf8PathBuf;

use super::frame::{Frame, HEADER_LEN, Parsed, Record, encode_frame, parse_frame};
use crate::error::StoreError;
use crate::fs_ops::FsOps;
use crate::layout::Layout;

/// The size of one `journal.off` entry.
const ENTRY_LEN: u64 = 8;

/// A handle on one store's metadata journal.
pub struct Journal {
    log_path: Utf8PathBuf,
    off_path: Utf8PathBuf,
    ckpt_path: Utf8PathBuf,
    /// Every durable write goes through here.
    fs: Arc<dyn FsOps>,
    /// Read-only handle on `journal.log`.
    log: File,
    /// Read-only handle on `journal.off`.
    off: File,
    tail: Mutex<Tail>,
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
        let log = File::open(layout.journal_log())?;
        let off = File::open(layout.journal_off())?;
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
    /// never lands after a torn one. Records the layout cannot hold are refused with
    /// `InvalidInput` before anything is written. After any other error the frame may or
    /// may not be committed, as after a crash; the next tail check settles it.
    pub fn append_frame(&self, records: &[Record]) -> Result<Frame, StoreError> {
        let bytes = encode_frame(records)?;
        let mut tail = self.tail();
        if !self.still_ends_at(&tail)? {
            tail.checked = false;
            (*tail, _) = self.check_tail()?;
        }
        tail.checked = false; // a failed append can leave part of a frame or an entry behind
        let offset = self
            .fs
            .append_durable(self.log_path.as_std_path(), &bytes)?;
        let entry_at = self
            .fs
            .append_durable(self.off_path.as_std_path(), &offset.to_le_bytes())?;
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
    /// Every listed frame was whole once, so one that fails its CRC or does not decode is
    /// [`StoreError::JournalCorrupt`], as is an entry that does not point where the previous
    /// frame ends. Only the last listed frame may turn out cut short or missing: a reader
    /// takes no lock, so it stops there silently and returns the frames before it (under
    /// the lock, [`Journal::truncate_torn_tail`] reports it).
    pub fn frames_from(&self, first: u64, limit: usize) -> Result<Vec<Frame>, StoreError> {
        let listed = self.off.metadata()?.len() / ENTRY_LEN;
        let wanted = u64::try_from(limit)
            .unwrap_or(u64::MAX)
            .min(listed.saturating_sub(first));
        if wanted == 0 {
            return Ok(Vec::new());
        }
        // The previous frame's entry (the first wanted frame starts where that frame ends),
        // the wanted frames' entries, and the next one (where the last wanted frame ends).
        let previous = first.checked_sub(1);
        let entries = self.entries(
            previous.unwrap_or(first),
            u64::from(previous.is_some()) + wanted + u64::from(first + wanted < listed),
        )?;
        let (previous, offsets) = match (previous, entries.split_first()) {
            (None, _) => (None, entries.as_slice()),
            (Some(_), Some((&previous, offsets))) => (Some(previous), offsets),
            // journal.off was cut since its length was read.
            (Some(_), None) => return Ok(Vec::new()),
        };
        let wanted = usize::try_from(wanted).map_or(offsets.len(), |n| n.min(offsets.len()));
        let Some(&start) = offsets.first() else {
            return Ok(Vec::new());
        };
        let log_len = self.log.metadata()?.len();
        let previous_end = match previous {
            Some(previous) => self.frame_end(previous, log_len)?,
            None => Some(0),
        };
        if previous_end != Some(start) {
            return Err(StoreError::JournalCorrupt { offset: start });
        }
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
                parsed if may_be_torn(&parsed, frame_seq, listed, &buf[at..]) => break,
                _ => return Err(StoreError::JournalCorrupt { offset }),
            }
        }
        if let Some(&next) = offsets.get(wanted)
            && frames.len() == wanted
            && next != start + to_u64(at)
        {
            return Err(StoreError::JournalCorrupt { offset: next });
        }
        Ok(frames)
    }

    /// Every frame from `offset` to the end of `journal.log`, read with one `read_at`: what
    /// a reader applies past [`Journal::ckpt`]. `offset` must be a frame boundary: 0, a
    /// listed frame's start, or the end of the last listed frame.
    ///
    /// Stops silently at a torn tail: a frame cut short by the end of the file (an append
    /// in progress looks the same; of the listed frames, only the last), an unlisted frame
    /// that fails its CRC, or unlisted bytes that are zero to the end of the file. Whole
    /// frames that `journal.off` does not list yet are included. The length of `journal.log`
    /// is read before that of `journal.off`, so an append racing the read can list frames
    /// past the end read: the read stops before them silently, provided the first of them
    /// starts where it stops. Every listed frame read must sit where its entry says, so a
    /// zeroed or stale entry is [`StoreError::JournalCorrupt`]; so is anything else, an
    /// `offset` that is not a frame boundary included.
    pub fn tail_frames_from(&self, offset: u64) -> Result<Vec<Frame>, StoreError> {
        let log_len = self.log.metadata()?.len();
        if offset >= log_len {
            return if offset == log_len {
                Ok(Vec::new())
            } else {
                Err(StoreError::JournalCorrupt { offset })
            };
        }
        let listed = self.off.metadata()?.len() / ENTRY_LEN;
        let first = self.seq_at(offset, listed, log_len)?;
        let mut seq = first;
        // Where each listed frame from `first` on starts, by its entry.
        let entries = self.entries(first, listed - first)?;
        let listed_at = |seq: u64| {
            usize::try_from(seq - first)
                .ok()
                .and_then(|i| entries.get(i).copied())
        };
        let buf = read_upto(&self.log, offset, log_len - offset)?;

        let mut frames = Vec::new();
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
        // Stopping before a listed frame: it must start right here (an append racing this
        // read listed it after the length of journal.log was read).
        if let Some(entry) = listed_at(seq)
            && entry != offset + to_u64(at)
        {
            return Err(StoreError::JournalCorrupt { offset: entry });
        }
        Ok(frames)
    }

    /// Brings the journal to a clean end after a crash, and returns where `journal.log`
    /// was cut, if it was:
    ///
    /// - an unlisted frame at the end that is cut short or fails its CRC is a torn write,
    ///   and so are unlisted bytes that are zero to the end of the file (blocks a filesystem
    ///   exposed zero-filled after a crash): `journal.log` is cut where they start;
    /// - whole frames that `journal.off` does not list (a crash between a frame's two
    ///   appends) are committed: `journal.log` is synced, then their entries are appended;
    /// - a partial entry at the end of `journal.off` is dropped;
    /// - anything else is [`StoreError::JournalCorrupt`] and changes nothing: a last listed
    ///   frame that does not start where the frame before it ends, or that is cut short,
    ///   missing (`journal.log` ends where it starts), fails its CRC or does not decode (a
    ///   listed frame's bytes were durable before it was listed, so its loss is a
    ///   filesystem's, never a torn write); an unlisted empty payload (unless only zeros
    ///   follow it), or an unlisted frame that matches its CRC but does not decode (a newer
    ///   version may have written it).
    ///
    /// Only the last listed frame, the header of the frame before it, and what follows are
    /// read. `journal.off` is repaired before `journal.log` is cut, so a crash in between
    /// leaves a torn frame the next call cuts. The caller must hold [`crate::MetaLock`]: with
    /// no writer appending, a last listed frame cut short is damage. Readers, which race
    /// appends, stop there silently instead.
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

    /// How many frames `journal.off` lists now (one `fstat`): the seq the next listed frame
    /// takes.
    pub fn frame_count(&self) -> Result<u64, StoreError> {
        Ok(self.off.metadata()?.len() / ENTRY_LEN)
    }

    /// The offset up to which the journal is materialized: what [`Journal::set_ckpt`] last
    /// stored, or 0 if it never ran.
    pub fn ckpt(&self) -> Result<u64, StoreError> {
        let bytes = match fs::read(&self.ckpt_path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e.into()),
        };
        let offset = <[u8; 8]>::try_from(bytes.as_slice()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{} holds {} bytes, not a u64le",
                    self.ckpt_path,
                    bytes.len()
                ),
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
        let listed = off_len / ENTRY_LEN;
        // Read from the last listed frame's start: that frame is checked again, and the
        // frames `journal.off` does not list yet follow it.
        let start = match listed.checked_sub(1) {
            Some(last) => self.entry(last)?,
            None => 0,
        };
        if start > log_len {
            return Err(StoreError::JournalCorrupt { offset: start });
        }
        let buf = read_upto(&self.log, start, log_len - start)?;
        let mut at = 0; // where in `buf` the unlisted frames begin
        if let Some(last) = listed.checked_sub(1) {
            self.check_starts_after(self.entry_before(last)?, start, log_len)?;
            // It was whole and durable before it was listed: cut short, missing, failing its
            // CRC or not decoding, it is damage, and nothing is cut.
            match parse_frame(&buf) {
                Parsed::Whole { size, .. } => at = size,
                _ => return Err(StoreError::JournalCorrupt { offset: start }),
            }
        }
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

        if off_len != listed * ENTRY_LEN {
            self.fs
                .truncate(self.off_path.as_std_path(), listed * ENTRY_LEN)?;
        }
        if !unlisted.is_empty() {
            // Their writer may have died after its write and before its sync, so their bytes
            // may still be only in the page cache: a listed frame must be durable.
            self.fs.sync_data(self.log_path.as_std_path())?;
        }
        for offset in &unlisted {
            self.fs
                .append_durable(self.off_path.as_std_path(), &offset.to_le_bytes())?;
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

    /// The seq of the frame starting at `offset`: its index in `journal.off` (found by
    /// binary search), or `listed` if `offset` is where the last listed frame ends.
    fn seq_at(&self, offset: u64, listed: u64, log_len: u64) -> Result<u64, StoreError> {
        let (mut low, mut high) = (0, listed);
        while low < high {
            let mid = low + (high - low) / 2;
            if self.entry(mid)? < offset {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        let is_boundary = if low < listed {
            self.entry(low)? == offset
        } else {
            let listed_end = match listed.checked_sub(1) {
                Some(last) => self.frame_end(self.entry(last)?, log_len)?,
                None => Some(0),
            };
            listed_end == Some(offset)
        };
        if is_boundary {
            Ok(low)
        } else {
            Err(StoreError::JournalCorrupt { offset })
        }
    }
}

/// Whether a frame that did not parse whole may be a torn write rather than damage, as a
/// reader judges it: given its seq, how many frames `journal.off` lists, and `rest`, the
/// bytes read from its start on (for an unlisted frame, to the end of `journal.log`). A
/// listed frame was whole and durable before its entry was written, but a reader takes no
/// lock, so an append racing it can leave the last listed frame looking cut short.
fn may_be_torn(parsed: &Parsed, seq: u64, listed: u64, rest: &[u8]) -> bool {
    if seq >= listed {
        return is_torn_unlisted(parsed, rest);
    }
    matches!(parsed, Parsed::Short) && seq + 1 >= listed
}

/// Whether unlisted bytes that do not parse as a whole frame are a torn tail: a frame cut
/// short by the end of the file (the append a crash tore, or one still in progress), one
/// that fails its CRC, or zeros all the way to the end of the file, which a filesystem may
/// expose after a crash in place of blocks it never wrote. No frame starts with `len == 0`,
/// so zeros hold nothing a writer committed. `rest` runs from where the frame would start to
/// the end of `journal.log`.
fn is_torn_unlisted(parsed: &Parsed, rest: &[u8]) -> bool {
    match parsed {
        Parsed::Short | Parsed::BadCrc => true,
        Parsed::Invalid => !rest.is_empty() && rest.iter().all(|&byte| byte == 0),
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

fn to_u64(n: usize) -> u64 {
    u64::try_from(n).expect("usize fits in u64")
}
