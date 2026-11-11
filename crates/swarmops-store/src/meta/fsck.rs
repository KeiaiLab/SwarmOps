//! Rebuilding the projections of the journal (the M6 design §7): `journal.off`, the index and
//! its sorted views, the ref files. Each function takes `meta/lock` itself and needs no open
//! [`crate::MetaStore`]: an open fails on the damage they mend. None of them cuts the journal
//! but where a writer's tail check cuts it, at a torn tail.
//!
//! ```text
//! journal.log ──walk, frame by frame──▶ journal.off   rebuild_offsets
//!             ──normal write recovery──▶ index, views rebuild_index (checkpoint 0, views empty)
//!             ──every RefMove──────────▶ ref files    rebuild_refs
//! ```

use std::collections::BTreeMap;
use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use swarmops_core::bytes;

use super::READ_WINDOW;
use super::features;
use super::frame::{self, BadHeader, Base, MAX_PAYLOAD_LEN, Parsed, Place, Record};
use super::journal::{Ckpt3, ENTRY_LEN_V2, Journal};
use super::lock::{Create, MetaLock, open_flock_file};
use super::refs::{self, RefState};
use super::runs::{self, Manifest};
use super::sorted::VIEW_FILES;
use super::store::MetaStore;
use crate::error::{FileOp, StoreError};
use crate::format::StoreFormat;
use crate::fs_ops::{self, FsOps, LinkOutcome, StoreFs};
use crate::layout::Layout;

/// How long a rebuild waits for `meta/lock`: as long as a writer waits by default.
const LOCK_TIMEOUT: Duration = Duration::from_secs(10);

/// The size of one FORMAT 1 `journal.off` entry: the frame's offset, a `u64le`.
const ENTRY_LEN_V1: u64 = 8;

/// How many frames [`rebuild_refs`] reads with one call as it walks the journal.
const FRAME_BATCH: usize = 256;

/// How long [`rebuild_index`] waits between two tries of FORMAT 3's `meta/views.lock`.
const VIEWS_LOCK_POLL: Duration = Duration::from_millis(10);

/// The kind byte of a FORMAT 3 frame's nonce record, and the record's length (the FORMAT 3
/// design §2.7: `u8 11 | u64le nonce`).
const KIND_NONCE: u8 = 11;
const NONCE_LEN: usize = 9;

/// How much of `journal3.ckpt` [`rebuild_offsets`] reads: one byte past a checkpoint's 44.
const CKPT3_READ: u64 = 45;

/// What a rebuild wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct Rebuilt {
    /// How many entries, records or files it wrote.
    pub written: u64,
}

/// Writes `journal.off` afresh from the frames of `journal.log`, walked one at a time.
///
/// Under `meta/lock`, it first walks the log, reading nothing past one frame at a time: every
/// frame must be whole, at its place (FORMAT 2's seq and previous CRC), up to the end or to a
/// tail a torn write leaves (cut short, or failing its CRC with nothing after it), and the walk
/// must find at least as many frames as `journal.off` listed (its whole entries but the zeroed
/// ones at its end, which list nothing). Anything else is [`StoreError::JournalCorrupt`] at the
/// frame where the walk stopped, and nothing is written: the journal is the truth, and a frame
/// the rebuild cannot vouch for is never listed or listed away.
///
/// In FORMAT 3 the walk also holds each frame to the base seq (the FORMAT 3 design §2.3, §2.7):
/// frames before it are FORMAT 2's, without a nonce, and those from it on end in one. A
/// `journal3.ckpt` that is missing or no checkpoint has lost its base seq: the walk takes the
/// first frame that ends in a nonce as the base (0 for a journal of none), and a frame without
/// one after it is [`StoreError::JournalCorrupt`] at that frame (finding `base_seq_lost`),
/// nothing written.
///
/// Then it moves the checkpoint to 0 (FORMAT 2's names the seq the entries must agree with,
/// FORMAT 3's keeps the base seq; the next write moves it again), empties `journal.off` with one
/// `atomic_replace`, and lets the
/// journal's own tail check list every whole frame and cut the torn tail, as the next writer's
/// would, its entries appended without a sync each and synced once at the end: one sync per
/// frame would take hours on a large journal. A crash in between leaves a prefix of the entries
/// for the next tail check to finish, and a power loss before the last sync zeroed entries that
/// a rebuild run again lists. `written` is the frames listed.
///
/// The replace gives `journal.off` a new file: a handle opened on the store before it reads the
/// old one, and fails as damage until it is opened again.
pub fn rebuild_offsets(layout: &Layout, fs: Arc<dyn FsOps>) -> Result<Rebuilt, StoreError> {
    let _lock = MetaLock::acquire(layout, LOCK_TIMEOUT)?;
    let store_fs = StoreFs::new(Arc::clone(&fs), layout.root());

    // 1. The walk, which writes nothing.
    let walk = walk_log(layout, &store_fs)?;
    if walk.frames < listed_before(layout, &store_fs)? {
        return Err(StoreError::JournalCorrupt { offset: walk.end });
    }

    // 2. The checkpoint first: a crash after it leaves the old entries, and a store as damaged
    //    as before; one after the replace, entries the next tail check lists.
    match walk.base {
        Some(base) => reset_ckpt3(layout, &store_fs, base)?,
        None => Journal::open_in(layout, store_fs.clone())?.set_ckpt(0)?,
    }
    store_fs.atomic_replace(layout.journal_off(), &[])?;

    // 3. Every whole frame listed by the journal itself, then the entries synced.
    let listing = Arc::new(UnsyncedListing {
        inner: fs,
        off: layout.journal_off().into_std_path_buf(),
    });
    let journal = Journal::open_in(layout, StoreFs::new(listing, layout.root()))?;
    let repaired = journal.repair_tail()?;
    store_fs.sync_data(layout.journal_off())?;
    Ok(Rebuilt {
        written: repaired.frames_listed,
    })
}

/// Replays every frame into the index and its sorted views again.
///
/// Under `meta/lock` it moves the checkpoint to 0, then cuts the four view files to nothing in
/// the reverse of the order a compaction renames them in, `ids.key` last: every count stays a
/// shape a reader takes, and from that last cut on no record counts as it is, so the next
/// write's recovery holds every record against the journal, field by field, cuts the index at
/// the first that differs, and replays the rest (storage spec §3, "색인의 보증"). Then it runs
/// that recovery itself, a [`MetaStore::flush`], which syncs the index, compacts it again past
/// [`super::Index::COMPACT_THRESHOLD`] and moves the checkpoint back to the end. A crash before
/// the last cut leaves the store as damaged as it was; after it, the next writer's recovery
/// finishes the rebuild. `written` is the records replayed.
///
/// It holds the lock while it reads the whole journal into memory and replays it (the M6 plan's
/// risk R-1): other writers meanwhile give up after their lock timeout with
/// [`StoreError::Busy`].
///
/// In FORMAT 3 (the FORMAT 3 design §2.3, §2.5) it first takes `meta/views.lock`, waiting as long
/// as for `meta/lock` (lock order: `views.lock`, then `meta/lock`), so that no step publishes a
/// manifest meanwhile. Under both it moves the checkpoint to 0 with its base seq kept (one a
/// missing or damaged `journal3.ckpt` lost is found again as [`rebuild_offsets`] finds it), then
/// replaces the manifest by one of no run, the commit point: every record and fact row is held
/// against the journal again. The replay leaves the runs to the next view step
/// ([`MetaStore::step_views`]), which also removes the runs no manifest names.
pub fn rebuild_index(layout: &Layout, fs: Arc<dyn FsOps>) -> Result<Rebuilt, StoreError> {
    // 1. The checkpoint to 0, then the views emptied, ids.key last (FORMAT 3: the manifest of
    //    no run): the commit point.
    {
        let store_fs = StoreFs::new(Arc::clone(&fs), layout.root());
        let _views = match layout.format() {
            StoreFormat::V1 | StoreFormat::V2 => None,
            StoreFormat::V3 => Some(views_lease(layout, &store_fs)?),
        };
        let _lock = MetaLock::acquire(layout, LOCK_TIMEOUT)?;
        match layout.format() {
            StoreFormat::V1 | StoreFormat::V2 => {
                Journal::open_in(layout, store_fs.clone())?.set_ckpt(0)?;
                let sorted = layout.meta_path("sorted");
                for view in VIEW_FILES.iter().rev() {
                    store_fs.truncate(sorted.join(view), 0)?;
                }
            }
            StoreFormat::V3 => empty_views(layout, &store_fs)?,
        }
    }

    // 2. The normal write recovery, one index file synced at a time, made durable.
    let mut store = MetaStore::open_in(layout.clone(), fs)?;
    store.set_parallelism(1);
    let replayed = store.write()?.recovered().records_replayed;
    store.flush()?;
    Ok(Rebuilt { written: replayed })
}

/// Writes every ref file again from the journal's `RefMove` records.
///
/// Under `meta/lock`, after the journal's tail check (as a writer's), it walks every frame:
/// each ref a frame moves is left at its last move's target, at the next version if that
/// changes it (1 for a ref made, gone for one deleted), as the frame's writer left it. Then each
/// ref's file is written unless it holds that state already, and every other ref file is
/// removed. A name this store's format does not take is [`StoreError::JournalCorrupt`] at its
/// frame, before anything is written. `written` is the files written or removed.
pub fn rebuild_refs(layout: &Layout, fs: Arc<dyn FsOps>) -> Result<Rebuilt, StoreError> {
    let _lock = MetaLock::acquire(layout, LOCK_TIMEOUT)?;
    let store_fs = StoreFs::new(fs, layout.root());
    let journal = Journal::open_in(layout, store_fs.clone())?;
    journal.repair_tail()?;
    let states = ref_states(layout.format(), &journal)?;
    let written = refs::rebuild(layout, &store_fs, &states)?;
    Ok(Rebuilt { written })
}

/// Where a walk of `journal.log` stopped.
struct Walk {
    /// The whole frames it read.
    frames: u64,
    /// Where they end: the end of the file, or the start of a torn tail.
    end: u64,
    /// In FORMAT 3, the base seq the frames bear out: `journal3.ckpt`'s, or the one they show
    /// when that is lost; `None` in FORMAT 1 and 2.
    base: Option<u64>,
}

/// What lies at a frame boundary of `journal.log`, as [`rebuild_offsets`] judges it.
enum Step {
    /// A whole frame at its place, ending at `end`, its payload's CRC `crc`; in FORMAT 3, whether
    /// it ends in a nonce.
    Whole { end: u64, crc: u32, nonce: Nonce },
    /// What a torn write leaves at the tail, nothing after it that the walk can see.
    Torn,
    /// Damage.
    Corrupt,
}

/// Whether a FORMAT 3 frame ends in its nonce (the FORMAT 3 design §2.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Nonce {
    /// FORMAT 2's records, and no nonce: a frame before the base seq. (Every frame of FORMAT 1
    /// and 2.)
    Without,
    /// Records, then exactly one nonce, last: a frame from the base seq on.
    With,
}

/// Walks the frames of `journal.log` from 0 (see [`rebuild_offsets`]).
fn walk_log(layout: &Layout, fs: &StoreFs) -> Result<Walk, StoreError> {
    let path = layout.journal_log();
    let log = fs_ops::open_regular(&path).map_err(|e| fs.locate(FileOp::Open, &path, e))?;
    let log_len = (log.metadata())
        .map_err(|e| fs.locate(FileOp::Stat, &path, e))?
        .len();

    // 1. FORMAT 3's base seq, unless journal3.ckpt lost it: the walk finds it then.
    let mut base = match layout.format() {
        StoreFormat::V1 | StoreFormat::V2 => None,
        StoreFormat::V3 => ckpt3_of(layout, fs)?.map(|ckpt| ckpt.base_seq),
    };
    let known = base.is_some();

    // 2. Frame by frame, each at its place, and in FORMAT 3 on its side of the base.
    let mut place = Place {
        seq: 0,
        prev_crc: 0,
    };
    let mut at = 0;
    while at < log_len {
        let step = read_step(&log, layout.format(), at, log_len, place)
            .map_err(|e| fs.locate(FileOp::ReadAt, &path, e))?;
        match step {
            Step::Whole { end, crc, nonce } => {
                if nonce == Nonce::With && base.is_none() {
                    base = Some(place.seq);
                }
                let past_base = base.is_some_and(|base| place.seq >= base);
                let wanted = if past_base {
                    Nonce::With
                } else {
                    Nonce::Without
                };
                if layout.format() == StoreFormat::V3 && nonce != wanted {
                    return Err(StoreError::JournalCorrupt { offset: at });
                }
                at = end;
                place = Place {
                    seq: place.seq + 1,
                    prev_crc: crc,
                };
            }
            Step::Torn => break,
            Step::Corrupt => return Err(StoreError::JournalCorrupt { offset: at }),
        }
    }

    // 3. A lost base that no frame shows: a journal of none starts at 0, any other is refused.
    if layout.format() == StoreFormat::V3 && !known && base.is_none() {
        if place.seq > 0 {
            return Err(StoreError::JournalCorrupt { offset: 0 });
        }
        base = Some(0);
    }
    Ok(Walk {
        frames: place.seq,
        end: at,
        base,
    })
}

/// `journal3.ckpt` of `layout`, read one byte past a checkpoint's length; `None` if it is
/// missing or holds no checkpoint: its base seq is lost then (the FORMAT 3 design §2.3).
fn ckpt3_of(layout: &Layout, fs: &StoreFs) -> Result<Option<Ckpt3>, StoreError> {
    let path = layout.journal_ckpt();
    let file = match fs_ops::open_regular(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(fs.locate(FileOp::Open, &path, e).into()),
    };
    let mut bytes = Vec::new();
    io::Read::read_to_end(&mut io::Read::take(file, CKPT3_READ), &mut bytes)
        .map_err(|e| fs.locate(FileOp::ReadAt, &path, e))?;
    Ok(Ckpt3::decode(&bytes).ok())
}

/// Writes `journal3.ckpt` of `layout` at offset 0, no record or fact row passed, its base seq
/// `base`: atomically and durably.
fn reset_ckpt3(layout: &Layout, fs: &StoreFs, base: u64) -> Result<(), StoreError> {
    let ckpt = Ckpt3 {
        base_seq: base,
        ..Ckpt3::default()
    };
    fs.atomic_replace(layout.journal_ckpt(), &ckpt.encode())?;
    Ok(())
}

/// [`rebuild_index`]'s commit point in FORMAT 3, under `views.lock` and `meta/lock`: the
/// checkpoint at 0 with its base seq (found again in the journal if `journal3.ckpt` lost it),
/// then the manifest of no run, a generation past the one it replaces.
fn empty_views(layout: &Layout, fs: &StoreFs) -> Result<(), StoreError> {
    // 1. The checkpoint, its base kept.
    let base = match ckpt3_of(layout, fs)? {
        Some(ckpt) => ckpt.base_seq,
        None => walk_log(layout, fs)?.base.unwrap_or(0),
    };
    reset_ckpt3(layout, fs, base)?;

    // 2. The manifest of no run.
    let generation = match runs::read_manifest(layout, fs) {
        Ok((_, manifest)) => manifest.generation + 1,
        Err(StoreError::IndexCorrupt { .. }) => 1,
        Err(e) => return Err(e),
    };
    let empty = Manifest {
        generation,
        ..Manifest::empty()
    };
    let dir = layout.views_dir();
    if !fs.exists(dir.as_std_path())? {
        fs.create_dir_all(dir.as_std_path())?;
        fs.fsync_dir(layout.meta_dir().as_std_path())?;
    }
    fs.atomic_replace(dir.join(runs::MANIFEST), &empty.encode())?;
    Ok(())
}

/// `meta/views.lock` of `layout`, held: tried every [`VIEWS_LOCK_POLL`] for as long as a writer
/// waits for `meta/lock`, then [`StoreError::Busy`].
fn views_lease(layout: &Layout, fs: &StoreFs) -> Result<File, StoreError> {
    let path = layout.views_lock();
    let file = open_flock_file(path.as_std_path(), Create::IfMissing)
        .map_err(|e| fs.locate(FileOp::Open, &path, e))?;
    let deadline = std::time::Instant::now() + LOCK_TIMEOUT;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                std::thread::sleep(VIEWS_LOCK_POLL);
            }
            Err(std::fs::TryLockError::WouldBlock) => return Err(StoreError::Busy),
            Err(std::fs::TryLockError::Error(e)) => {
                return Err(fs.locate(FileOp::Lock, &path, e).into());
            }
        }
    }
}

/// What the FORMAT 3 frame at `offset` of `layout`'s `journal2.log`, which must sit at `place`,
/// holds if it is whole and its payload checks: records and no nonce, or records and their
/// nonce; `None` for anything else. What [`MetaStore::check`] names a frame the journal's reader
/// refused by.
pub(super) fn nonce_at(
    layout: &Layout,
    fs: &StoreFs,
    offset: u64,
    place: Place,
) -> Result<Option<Nonce>, StoreError> {
    let path = layout.journal_log();
    let log = fs_ops::open_regular(&path).map_err(|e| fs.locate(FileOp::Open, &path, e))?;
    let log_len = (log.metadata())
        .map_err(|e| fs.locate(FileOp::Stat, &path, e))?
        .len();
    let step = read_step(&log, StoreFormat::V3, offset, log_len, place)
        .map_err(|e| fs.locate(FileOp::ReadAt, &path, e))?;
    Ok(match step {
        Step::Whole { nonce, .. } => Some(nonce),
        Step::Torn | Step::Corrupt => None,
    })
}

/// Judges the frame at `at` of `log` (`log_len` long), which must sit at `place`: its header
/// first, then its payload, which no length past what a writer writes makes it read.
fn read_step(
    log: &File,
    format: StoreFormat,
    at: u64,
    log_len: u64,
    place: Place,
) -> io::Result<Step> {
    // 1. The header: cut short, failing its own CRC (FORMAT 2) or of length 0 (FORMAT 1) is a
    //    torn write's; one that checks and names a length no writer writes, or another place,
    //    is damage.
    let header_len = frame::header_len(format);
    let payload_at = at + bytes::to_u64(header_len);
    if payload_at > log_len {
        return Ok(Step::Torn);
    }
    let mut header = vec![0; header_len];
    log.read_exact_at(&mut header, at)?;
    let header = match frame::decode_header_in(format, &header) {
        Ok(Some(header)) => header,
        Ok(None) | Err(BadHeader::Crc) => return Ok(Step::Torn),
        Err(BadHeader::Len) => return Ok(Step::Corrupt),
    };
    if header.len == 0 {
        return Ok(Step::Torn);
    }
    if !header.sits_at(place) {
        return Ok(Step::Corrupt);
    }

    // 2. The payload: cut short is torn; failing its CRC torn only at the very end.
    let end = payload_at + u64::from(header.len);
    let len = usize::try_from(header.len).unwrap_or(usize::MAX);
    if end > log_len {
        return Ok(Step::Torn);
    }
    if len > MAX_PAYLOAD_LEN {
        // FORMAT 1 only: FORMAT 2 refused the length with the header.
        return Ok(if end == log_len {
            Step::Torn
        } else {
            Step::Corrupt
        });
    }
    let mut payload = vec![0; len];
    log.read_exact_at(&mut payload, payload_at)?;
    let whole = |crc, nonce| Step::Whole { end, crc, nonce };
    Ok(
        match frame::parse_payload(header_len, header.crc, &payload) {
            Parsed::Whole { crc, .. } => whole(crc, Nonce::Without),
            Parsed::BadCrc if end == log_len => Step::Torn,
            // FORMAT 3 from the base on: the records, then the nonce.
            Parsed::Invalid { .. } if format == StoreFormat::V3 && ends_in_nonce(&payload) => {
                whole(header.crc, Nonce::With)
            }
            _ => Step::Corrupt,
        },
    )
}

/// Whether `payload`, whose CRC checks, holds a FORMAT 3 frame's records past the base seq and
/// then its nonce (the FORMAT 3 design §2.7): a count of 2 at least, that many records less one,
/// each one this build decodes there, then `u8 11 | u64le nonce` and nothing after it.
fn ends_in_nonce(payload: &[u8]) -> bool {
    let Some((count, mut rest)) = payload.split_first_chunk::<2>() else {
        return false;
    };
    let records = usize::from(u16::from_le_bytes(*count));
    if records < 2 {
        return false;
    }
    let known = features::known_kinds();
    for _ in 1..records {
        let decoded = Record::decode_in(StoreFormat::V3, Base::After, &known, rest);
        let Some(next) = decoded.ok().and_then(|(_, used)| rest.get(used..)) else {
            return false;
        };
        rest = next;
    }
    rest.len() == NONCE_LEN && rest[0] == KIND_NONCE
}

/// How many frames `journal.off` lists before the rebuild: its whole entries, less the zeroed
/// ones at its end past entry 0 (entry 0 lists frame 0, whatever it holds). Read from the end in
/// windows of [`READ_WINDOW`] entries, each entry only as bytes: the damage being mended may be
/// in any of them.
fn listed_before(layout: &Layout, fs: &StoreFs) -> Result<u64, StoreError> {
    let path = layout.journal_off();
    let failed = |op, e| StoreError::from(fs.locate(op, &path, e));
    let off = fs_ops::open_regular(&path).map_err(|e| failed(FileOp::Open, e))?;
    let entry_len = match layout.format() {
        StoreFormat::V1 => ENTRY_LEN_V1,
        StoreFormat::V2 | StoreFormat::V3 => ENTRY_LEN_V2,
    };
    let whole = (off.metadata()).map_err(|e| failed(FileOp::Stat, e))?.len() / entry_len;
    let mut end = whole;
    while end > 1 {
        let start = end.saturating_sub(READ_WINDOW).max(1);
        let mut window = vec![0; usize::try_from((end - start) * entry_len).unwrap_or(usize::MAX)];
        (off.read_exact_at(&mut window, start * entry_len))
            .map_err(|e| failed(FileOp::ReadAt, e))?;
        let width = usize::try_from(entry_len).unwrap_or(usize::MAX);
        if let Some(last) = window
            .chunks_exact(width)
            .rposition(|entry| entry.iter().any(|&b| b != 0))
        {
            return Ok(start + bytes::to_u64(last) + 1);
        }
        end = start;
    }
    Ok(whole.min(1))
}

/// The state every ref is left in by the journal's frames, read [`FRAME_BATCH`] at a time:
/// each frame one transaction, its refs at their last move's targets ([`refs::moved`]).
fn ref_states(
    format: StoreFormat,
    journal: &Journal,
) -> Result<BTreeMap<String, RefState>, StoreError> {
    let mut states = BTreeMap::new();
    let mut next = 0;
    loop {
        let frames = journal.frames_from(next, FRAME_BATCH)?;
        let Some(last) = frames.last() else {
            return Ok(states);
        };
        next = last.seq + 1;
        for frame in &frames {
            let mut targets = BTreeMap::new();
            for record in &frame.records {
                if let Record::RefMove { name, new, .. } = record {
                    refs::check_ref_name_in(format, name).map_err(|_| {
                        StoreError::JournalCorrupt {
                            offset: frame.offset,
                        }
                    })?;
                    targets.insert(name.as_str(), *new);
                }
            }
            for (name, target) in targets {
                match refs::moved(states.get(name).copied(), target) {
                    Some(state) => states.insert(name.to_owned(), state),
                    None => states.remove(name),
                };
            }
        }
    }
}

/// The filesystem [`rebuild_offsets`] lets the journal's tail check list the frames through:
/// `inner`, but for the appends to `journal.off` (`off`), made without a sync each, since the
/// rebuild syncs the file once when they are all made.
struct UnsyncedListing {
    inner: Arc<dyn FsOps>,
    off: PathBuf,
}

impl FsOps for UnsyncedListing {
    fn create_dir_all(&self, p: &Path) -> io::Result<()> {
        self.inner.create_dir_all(p)
    }

    fn write_durable_temp(&self, dir: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
        self.inner.write_durable_temp(dir, bytes)
    }

    fn link_noclobber(&self, from: &Path, to: &Path) -> io::Result<LinkOutcome> {
        self.inner.link_noclobber(from, to)
    }

    fn fsync_dir(&self, p: &Path) -> io::Result<()> {
        self.inner.fsync_dir(p)
    }

    fn read(&self, p: &Path) -> io::Result<Option<Vec<u8>>> {
        self.inner.read(p)
    }

    fn exists(&self, p: &Path) -> io::Result<bool> {
        self.inner.exists(p)
    }

    fn remove(&self, p: &Path) -> io::Result<()> {
        self.inner.remove(p)
    }

    fn append_durable(&self, path: &Path, bytes: &[u8]) -> io::Result<u64> {
        if path == self.off {
            return self.inner.append(path, bytes);
        }
        self.inner.append_durable(path, bytes)
    }

    fn file_len(&self, path: &Path) -> io::Result<u64> {
        self.inner.file_len(path)
    }

    fn truncate(&self, path: &Path, len: u64) -> io::Result<()> {
        self.inner.truncate(path, len)
    }

    fn atomic_replace(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        self.inner.atomic_replace(path, bytes)
    }

    fn sync_data(&self, path: &Path) -> io::Result<()> {
        self.inner.sync_data(path)
    }

    fn append(&self, path: &Path, bytes: &[u8]) -> io::Result<u64> {
        self.inner.append(path, bytes)
    }

    fn rename_durable(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.inner.rename_durable(from, to)
    }
}
