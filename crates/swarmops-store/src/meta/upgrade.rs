//! The upgrades of a store: FORMAT 1 to FORMAT 2 (the M6 design §3), then FORMAT 2 to FORMAT 3
//! (the FORMAT 3 design §3), one after the other in one call. Each writes the next format's files
//! beside the old ones, poisons the old checkpoint, then replaces `FORMAT`, its commit point. An
//! open that finds a poison under the `FORMAT` before it finishes that upgrade
//! ([`finish_if_poisoned`]).
//!
//! ```text
//! maint.lock, meta/views.lock (waited for), then meta/lock, held throughout
//! FORMAT 1 to 2:
//! 1. journal.log     tail repaired                      FORMAT 1 still
//! 2. journal2.*      removed (an earlier run's), made empty
//!    journal2.log    every frame again, same payload, its place in its header; synced
//!    journal2.off    an entry per frame; synced
//! 3. refs2/<hash>    every ref file again, same target and version
//! 4. index files     synced
//!    journal2.ckpt   FORMAT 1's checkpoint, at the same frame
//!    read back       frames, refs and checkpoint as FORMAT 2 reads them, or the upgrade stops
//! 5. journal.ckpt    the poison: every FORMAT 1 reader and writer fails closed from here
//! 6. FORMAT          "swarmops-store 2\n": the commit point
//! FORMAT 2 to 3:
//! 1. journal2.*      tail repaired, frames past the checkpoint replayed, index synced,
//!                    checkpoint at the journal's end                 FORMAT 2 still
//! 2. FORMAT 3 files  removed (an earlier run's): sorted3/, journal3.ckpt, facts.col, features
//! 3. idx/facts.col   empty; no feature list
//!    sorted3/        one run of every index record, the manifest (generation 1) naming it
//!    journal3.ckpt   the journal's end, its frame count as the base seq, the records synced
//!    read back       as FORMAT 3 reads them, or the upgrade stops
//! 4. journal2.ckpt   the poison: every FORMAT 2 reader and writer fails closed from here
//! 5. FORMAT          "swarmops-store 3\n": the commit point
//! ```
//!
//! A crash before a poison leaves the store in the format before, whole: the next upgrade starts
//! that step over, and gc removes what it left. Between a poison and its `FORMAT` the next open
//! finishes it. FORMAT 1's checkpoint is carried over, not moved: the frames past it are replayed
//! by the next FORMAT 2 write as FORMAT 1's would have been, and the index files, shared by every
//! format, are not touched. FORMAT 2's journal is not rewritten at all: FORMAT 3 shares its frame,
//! offset and ref file layouts, so every operation id stays as it was.

use std::collections::BTreeMap;
use std::io::{self, Read};
use std::sync::Arc;
use std::time::Duration;

use camino::Utf8Path;
use swarmops_core::bytes;

use std::fs::{File, TryLockError};
use std::ops::ControlFlow;
use std::time::Instant;

use super::column::AppendOnlyColumn;
use super::frame::{self, HEADER_LEN, HEADER_LEN_V2};
use super::index::{DHS, IDS, Index};
use super::journal::{Ckpt3, ENTRY_LEN_V2, Journal};
use super::lock::{Create, MetaLock, open_flock_file};
use super::refs::{self, RefState};
use super::replay;
use super::runs::{self, Domain, Manifest, Run};
use crate::error::{FileOp, StoreError};
use crate::format::{self, StoreFormat, UPGRADE_POISON, UPGRADE_POISON_V3};
use crate::fs_ops::{self, FsOps, StoreFs};
use crate::gc;
use crate::layout::Layout;
use crate::maint::MaintLease;

/// How long an upgrade, and an open that finishes one, waits for `meta/lock`: a metadata store's
/// own wait.
const META_LOCK_TIMEOUT: Duration = Duration::from_secs(10);

/// How long an upgrade waits for `meta/views.lock`, which a view step holds while it builds: the
/// same wait (the FORMAT 3 design §3.1). A step never waits for it.
const VIEWS_LOCK_TIMEOUT: Duration = META_LOCK_TIMEOUT;

/// How often an upgrade waiting for `meta/views.lock` tries it again.
const VIEWS_LOCK_POLL: Duration = Duration::from_millis(10);

/// `meta/idx/`, where FORMAT 3's fact column lies beside the changeset index's.
const IDX_DIR: &str = "idx";

/// How many frames of a journal are read at a time.
const FRAME_BATCH: usize = 64;

/// How many bytes of frames are gathered before they are appended to `journal2.log`.
const WRITE_CHUNK: usize = 1 << 20;

/// The domain a FORMAT 2 ref file's name is hashed under (the M6 design §2.5). The ref files'
/// own reader holds the recipe too; every file written here is read back through it before the
/// poison, so a recipe that differed would stop the upgrade with the store still FORMAT 1.
const NAME_HASH_DOMAIN: &[u8] = b"swarmops:refname:v1\0";

/// How many hex digits of the name's hash name a FORMAT 2 ref file.
const STEM_LEN: usize = 32;

/// What [`upgrade`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Upgraded {
    /// The format the store had.
    pub from: StoreFormat,
    /// The format it has now.
    pub to: StoreFormat,
    /// Whether it changed: false for a store that had the latest format already.
    pub upgraded: bool,
}

impl Upgraded {
    /// Takes every field.
    pub fn new(from: StoreFormat, to: StoreFormat, upgraded: bool) -> Upgraded {
        Upgraded { from, to, upgraded }
    }
}

/// Upgrades the store of `layout` to FORMAT 3, a FORMAT 1 one through FORMAT 2; nothing for a
/// FORMAT 3 store. Takes the maintenance lease ([`StoreError::MaintenanceBusy`] at once if it is
/// held), then `meta/views.lock` and `meta/lock` (waiting up to 10 s for each, then
/// [`StoreError::Busy`]), and holds all three throughout: writers and view steps wait meanwhile.
/// The steps are the module documentation's; `layout`'s format is not read, the store's `FORMAT`
/// is, under the lock. A store whose files do not read ([`StoreError::JournalCorrupt`], a damaged
/// ref or index file) is not upgraded past its format.
pub fn upgrade(layout: &Layout, fs: Arc<dyn FsOps>) -> Result<Upgraded, StoreError> {
    let fs = StoreFs::new(fs, layout.root());

    // 1. The lease, the view lease, then meta/lock (the FORMAT 3 design §3.1), all held to the
    //    end: the lock order of §2.5 rule 8.
    let lease = MaintLease::try_acquire(layout)?;
    let _views = views_lock(layout, &fs)?;
    let _lock = lease.meta_lock(layout, META_LOCK_TIMEOUT)?;

    // 2. What FORMAT says under the lock: FORMAT 3 is left alone, FORMAT 1 taken to 2 first.
    let from = format::read_format(layout, &fs)?;
    match from {
        StoreFormat::V3 => return Ok(Upgraded::new(from, from, false)),
        StoreFormat::V1 => to_v2(layout.root(), &fs)?,
        StoreFormat::V2 => {}
    }

    // 3. FORMAT 2 to 3.
    to_v3(layout.root(), &fs)?;
    Ok(Upgraded::new(from, StoreFormat::V3, true))
}

/// FORMAT 1 to 2, under the locks: the FORMAT 2 files, then the poison, unless a run that
/// stopped after its poison wrote both; then the commit point.
fn to_v2(root: &Utf8Path, fs: &StoreFs) -> Result<(), StoreError> {
    let v1 = Layout::with_format(root, StoreFormat::V1);
    if !poisoned(&v1.journal_ckpt(), UPGRADE_POISON) {
        write_v2(&v1, &Layout::with_format(root, StoreFormat::V2), fs)?;
        fs.atomic_replace(v1.journal_ckpt().as_std_path(), UPGRADE_POISON)?;
    }
    fs.atomic_replace(v1.format_file().as_std_path(), StoreFormat::V2.marker())?;
    Ok(())
}

/// FORMAT 2 to 3, under the locks: FORMAT 2 brought to its journal's end, FORMAT 3's files, then
/// the poison, unless a run that stopped after its poison wrote them all (W11); then the commit
/// point.
fn to_v3(root: &Utf8Path, fs: &StoreFs) -> Result<(), StoreError> {
    let v2 = Layout::with_format(root, StoreFormat::V2);
    let v3 = Layout::with_format(root, StoreFormat::V3);
    if !poisoned(&v2.journal_ckpt(), UPGRADE_POISON_V3) {
        let (journal, records) = flush_v2(&v2, fs)?;
        clear_v3(&v3, fs)?;
        write_v3(&v3, fs, &journal, records)?;
        fs.atomic_replace(v2.journal_ckpt().as_std_path(), UPGRADE_POISON_V3)?;
    }
    fs.atomic_replace(v3.format_file().as_std_path(), StoreFormat::V3.marker())?;
    Ok(())
}

/// `meta/views.lock` of `layout`, held: opened as `meta/lock` is (made if missing), then tried
/// every [`VIEWS_LOCK_POLL`] until [`VIEWS_LOCK_TIMEOUT`] has passed, then [`StoreError::Busy`].
/// A view step holds it for one build and merge; it never waits for it.
fn views_lock(layout: &Layout, fs: &StoreFs) -> Result<File, StoreError> {
    let path = layout.views_lock();
    let file = open_flock_file(path.as_std_path(), Create::IfMissing)
        .map_err(|e| fs.locate(FileOp::Open, &path, e))?;
    let started = Instant::now();
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(TryLockError::WouldBlock) if started.elapsed() < VIEWS_LOCK_TIMEOUT => {
                std::thread::sleep(VIEWS_LOCK_POLL);
            }
            Err(TryLockError::WouldBlock) => return Err(StoreError::Busy),
            Err(TryLockError::Error(e)) => return Err(fs.locate(FileOp::Lock, &path, e).into()),
        }
    }
}

/// Finishes an upgrade of the store of `layout`, opened in the format `FORMAT` names, that
/// stopped between its poison and its commit point: replaces `FORMAT` if that format's
/// checkpoint is the next one's poison (FORMAT 1's [`UPGRADE_POISON`], FORMAT 2's
/// [`UPGRADE_POISON_V3`]). Whether it did.
///
/// The checkpoint is looked at first through `std::fs`, no [`FsOps`] call: a store no upgrade
/// poisoned costs its open one small read, a FORMAT 3 one none. A poisoned one is finished under
/// `meta/lock` (waiting up to 10 s, then [`StoreError::Busy`]), without the maintenance lease, so
/// that an open never waits for a gc; a poison is written only once the next format's files are
/// durable, so `FORMAT` is all that is left. Two opens at once both finish it: the second finds
/// `FORMAT` replaced.
pub fn finish_if_poisoned(layout: &Layout, fs: Arc<dyn FsOps>) -> Result<bool, StoreError> {
    let Some(next) = poison_of(layout) else {
        return Ok(false);
    };
    if !poisoned(&layout.journal_ckpt(), next) {
        return Ok(false);
    }

    // Under the lock, FORMAT again: another open may have finished it meanwhile. Each commit
    // point whose poison is down is passed.
    let fs = StoreFs::new(fs, layout.root());
    let _lock = MetaLock::acquire(layout, META_LOCK_TIMEOUT)?;
    loop {
        let format = format::read_format(layout, &fs)?;
        let at = Layout::with_format(layout.root(), format);
        let Some(poison) = poison_of(&at) else {
            return Ok(true);
        };
        if !poisoned(&at.journal_ckpt(), poison) {
            return Ok(true);
        }
        let next = match format {
            StoreFormat::V1 => StoreFormat::V2,
            StoreFormat::V2 | StoreFormat::V3 => StoreFormat::V3,
        };
        fs.atomic_replace(layout.format_file().as_std_path(), next.marker())?;
    }
}

/// What an upgrade from `layout`'s format writes over its checkpoint; none for FORMAT 3, the
/// latest.
fn poison_of(layout: &Layout) -> Option<&'static [u8]> {
    match layout.format() {
        StoreFormat::V1 => Some(UPGRADE_POISON),
        StoreFormat::V2 => Some(UPGRADE_POISON_V3),
        StoreFormat::V3 => None,
    }
}

/// Whether the checkpoint at `ckpt` is `poison`, read through `std::fs` and capped one byte past
/// it. A checkpoint that does not read is not the poison: the store's own reads report it, as
/// they would without an upgrade.
fn poisoned(ckpt: &Utf8Path, poison: &[u8]) -> bool {
    let Ok(file) = fs_ops::open_regular(ckpt) else {
        return false;
    };
    let mut bytes = Vec::with_capacity(poison.len() + 1);
    let read = file
        .take(bytes::to_u64(poison.len() + 1))
        .read_to_end(&mut bytes);
    read.is_ok() && bytes == poison
}

/// Steps 1 to 4 of the module documentation: the FORMAT 2 files of `v2` from the FORMAT 1 files
/// of `v1`, durable and read back. The caller holds `meta/lock`.
fn write_v2(v1: &Layout, v2: &Layout, fs: &StoreFs) -> Result<(), StoreError> {
    // 1. FORMAT 1's journal brought to a clean end, as a writer's recovery brings it.
    let old = Journal::open_in(v1, fs.clone())?;
    old.repair_tail()?;
    let old_ckpt = old.ckpt()?;
    let refs = refs::read_all(v1, fs)?.states;

    // 2. FORMAT 2's journal, from nothing.
    clear_v2(v2, fs)?;
    drop(Journal::create_in(v2, fs)?);
    let copied = copy_frames(&old, old_ckpt, v2, fs)?;

    // 3. The ref files.
    write_refs(v2, fs, &refs)?;

    // 4. The checkpoint, at the frame FORMAT 1's is at, once the index files it vouches for
    //    are synced (they are both formats'); then everything read back as FORMAT 2 reads it.
    let mut index = Index::open_in(v1, fs.clone())?;
    index.set_parallelism(1);
    index.sync()?;
    let new = Journal::open_in(v2, fs.clone())?;
    new.set_ckpt(copied.ckpt)?;
    check_copy(&old, &new, &copied)?;
    if refs::read_all(v2, fs)?.states != refs {
        return Err(unequal("its ref files"));
    }
    Ok(())
}

/// Removes what an earlier upgrade that stopped before its poison left of FORMAT 2's files: the
/// ref files first, then the journal's, each directory synced after.
fn clear_v2(v2: &Layout, fs: &StoreFs) -> Result<(), StoreError> {
    // 1. Every file of refs2/: in a FORMAT 1 store an upgrade writes them all.
    let dir = v2.refs_dir();
    let listing_failed = |e| fs.locate(FileOp::ReadDir, &dir, e);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries
            .collect::<Result<Vec<_>, _>>()
            .map_err(listing_failed)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(listing_failed(e).into()),
    };
    let files: Vec<_> = (entries.iter())
        .filter(|entry| entry.file_type().is_ok_and(|kind| !kind.is_dir()))
        .map(std::fs::DirEntry::path)
        .collect();
    for file in &files {
        fs.remove(file)?;
    }
    if !files.is_empty() {
        fs.fsync_dir(dir.as_std_path())?;
    }

    // 2. The journal's files.
    let mut removed = false;
    for file in [v2.journal_off(), v2.journal_log(), v2.journal_ckpt()] {
        if std::fs::symlink_metadata(&file).is_ok() {
            fs.remove(file.as_std_path())?;
            removed = true;
        }
    }
    if removed {
        fs.fsync_dir(v2.meta_dir().as_std_path())?;
    }
    Ok(())
}

/// What [`copy_frames`] wrote.
struct Copied {
    /// How many frames.
    frames: u64,
    /// Where in `journal2.log` the frame at FORMAT 1's checkpoint starts (or the last ends).
    ckpt: u64,
}

/// Writes every frame of `old` to `v2`'s `journal2.log`, empty, with the same payload behind a
/// FORMAT 2 header, and synced; then an entry for each to `journal2.off`, synced. Returns how
/// many, and where FORMAT 1's checkpoint `old_ckpt` falls among them.
fn copy_frames(
    old: &Journal,
    old_ckpt: u64,
    v2: &Layout,
    fs: &StoreFs,
) -> Result<Copied, StoreError> {
    let log = v2.journal_log();
    let mut pending = Vec::new(); // frames not appended yet
    let mut entries = Vec::new();
    let mut ckpt = None;
    let mut old_end = 0; // where the last frame read ends in journal.log
    let mut new_end = 0; // and in journal2.log, once `pending` is appended
    let mut prev_crc = 0;
    let mut next = 0;
    loop {
        let batch = old.frames_from(next, FRAME_BATCH)?;
        if batch.is_empty() {
            break;
        }
        for frame in batch {
            // 1. The frame again, in place: its payload is the one it had, so it is as long.
            let bytes =
                frame::encode_frame_in(StoreFormat::V2, &frame.records, frame.seq, prev_crc)?;
            let len = bytes::to_u64(bytes.len());
            if frame.offset != old_end {
                return Err(unequal("a frame's payload"));
            }
            if frame.offset == old_ckpt {
                ckpt = Some(new_end);
            }
            old_end += len - bytes::to_u64(HEADER_LEN_V2 - HEADER_LEN);
            prev_crc = frame::payload_crc(&bytes);
            entries.extend_from_slice(&entry_v2(frame.seq, new_end, prev_crc));
            new_end += len;
            next = frame.seq + 1;
            pending.extend_from_slice(&bytes);

            // 2. Appended a chunk at a time.
            if pending.len() >= WRITE_CHUNK {
                append(fs, &log, &pending, new_end)?;
                pending.clear();
            }
        }
    }
    append(fs, &log, &pending, new_end)?;
    if old_end != old.len() {
        return Err(unequal("its frames"));
    }
    if old_ckpt == old_end {
        ckpt = Some(new_end);
    }
    let ckpt = ckpt.ok_or(StoreError::JournalCorrupt { offset: old_ckpt })?;

    // 3. The frames durable before an entry lists them, then the entries.
    fs.sync_data(log.as_std_path())?;
    let off = v2.journal_off();
    append(fs, &off, &entries, bytes::to_u64(entries.len()))?;
    fs.sync_data(off.as_std_path())?;
    Ok(Copied { frames: next, ckpt })
}

/// Appends `bytes` to `path`, which must end at `end` once they are there.
fn append(fs: &StoreFs, path: &Utf8Path, bytes: &[u8], end: u64) -> Result<(), StoreError> {
    if bytes.is_empty() {
        return Ok(());
    }
    let at = fs.append(path.as_std_path(), bytes)?;
    if at + bytes::to_u64(bytes.len()) != end {
        return Err(StoreError::JournalCorrupt { offset: at });
    }
    Ok(())
}

/// The `journal2.off` entry of frame `seq`, at `offset`, of payload CRC `crc` (the M6 design
/// §2.4): `u64le offset | u32le crc | u32le crc32(u64le seq | u64le offset | u32le crc)`. The
/// journal's reader checks each, so [`check_copy`] reads them all back.
fn entry_v2(seq: u64, offset: u64, crc: u32) -> [u8; 16] {
    const _: () = assert!(ENTRY_LEN_V2 == 16, "a FORMAT 2 entry is 16 bytes");
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(&seq.to_le_bytes());
    hasher.update(&offset.to_le_bytes());
    hasher.update(&crc.to_le_bytes());
    let mut entry = [0; 16];
    entry[..8].copy_from_slice(&offset.to_le_bytes());
    entry[8..12].copy_from_slice(&crc.to_le_bytes());
    entry[12..].copy_from_slice(&hasher.finalize().to_le_bytes());
    entry
}

/// Writes the file of every ref of `refs` in `v2`'s `refs2/`, at the target and version it has:
/// `<name>\n<64 hex>\n<version>\n`, under the hash of its name. `refs2/` is made and its entry
/// synced first, even if an earlier run made it.
fn write_refs(
    v2: &Layout,
    fs: &StoreFs,
    refs: &BTreeMap<String, RefState>,
) -> Result<(), StoreError> {
    let dir = v2.refs_dir();
    fs.create_dir_all(dir.as_std_path())?;
    fs.fsync_dir(v2.meta_dir().as_std_path())?;
    for (name, state) in refs {
        refs::check_ref_name_in(StoreFormat::V2, name)?;
        let bytes = format!("{name}\n{}\n{}\n", state.target, state.version);
        fs.atomic_replace(dir.join(file_stem(name)).as_std_path(), bytes.as_bytes())?;
    }
    Ok(())
}

/// The FORMAT 2 file name of ref `name`: `hex(blake3(NAME_HASH_DOMAIN ‖ name)[0..16])`.
fn file_stem(name: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(NAME_HASH_DOMAIN);
    hasher.update(name.as_bytes());
    hasher.finalize().to_hex()[..STEM_LEN].to_owned()
}

/// Reads `new`, the FORMAT 2 journal [`copy_frames`] wrote, back against `old`: as many frames,
/// each with the same records, and the checkpoint where it was put. Every entry and header is
/// checked by the reads themselves.
fn check_copy(old: &Journal, new: &Journal, copied: &Copied) -> Result<(), StoreError> {
    if new.frame_count()? != copied.frames || new.ckpt()? != copied.ckpt {
        return Err(unequal("its journal"));
    }
    let mut next = 0;
    loop {
        let (was, is) = (
            old.frames_from(next, FRAME_BATCH)?,
            new.frames_from(next, FRAME_BATCH)?,
        );
        let same = was.len() == is.len()
            && (was.iter().zip(&is))
                .all(|(was, is)| was.seq == is.seq && was.records == is.records);
        if !same {
            return Err(unequal("its frames"));
        }
        let Some(last) = was.last() else {
            return Ok(());
        };
        next = last.seq + 1;
    }
}

/// The error of an upgrade whose FORMAT 2 copy of `what` does not read back as FORMAT 1's: a
/// defect of this build, found before the poison, so the store is FORMAT 1 still.
fn unequal(what: &str) -> StoreError {
    StoreError::Internal(format!(
        "the upgrade's FORMAT 2 copy of {what} does not read back as FORMAT 1's; the store is \
         FORMAT 1 still"
    ))
}

/// Step 1 of FORMAT 2 to 3: FORMAT 2's journal brought to a clean end, the frames past its
/// checkpoint replayed into the index and the ref files, the index synced and the checkpoint
/// moved to the journal's end, as a writer's recovery and a flush do. The caller holds
/// `meta/lock`. Returns the journal and how many index records it now holds, all synced.
fn flush_v2(v2: &Layout, fs: &StoreFs) -> Result<(Journal, u64), StoreError> {
    // 1. The journal's tail, then what lies past the checkpoint.
    let journal = Journal::open_in(v2, fs.clone())?;
    journal.repair_tail()?;
    let ckpt = journal.ckpt()?;
    let past = || journal.tail_frames_from(ckpt);
    let (mut index, frames) = Index::open_with_past(v2, fs.clone(), 0, past)?;
    index.set_parallelism(1);
    index.repair_tail_with(&frames)?;
    replay::replay(&mut index, v2, fs, &frames)?;

    // 2. Durable, and passed by the checkpoint.
    index.sync()?;
    journal.set_ckpt(journal.len())?;
    Ok((journal, index.len()))
}

/// Removes what an earlier upgrade to FORMAT 3 that stopped before its poison left of FORMAT 3's
/// files (the FORMAT 3 design §3.1 step 4: a rerun starts afresh), each removal durable.
fn clear_v3(v3: &Layout, fs: &StoreFs) -> Result<(), StoreError> {
    let files = [
        v3.views_dir(),
        v3.journal_ckpt(),
        v3.facts_col(),
        v3.features_file(),
    ];
    for path in files {
        let rel = path
            .strip_prefix(v3.root())
            .expect("a store's file is under its root");
        gc::remove_at(fs, rel)?;
    }
    Ok(())
}

/// Step 3 of FORMAT 2 to 3: FORMAT 3's own files beside FORMAT 2's journal of `journal`, whose
/// `records` index records are all synced; each durable (W4′, W11), then read back.
fn write_v3(v3: &Layout, fs: &StoreFs, journal: &Journal, records: u64) -> Result<(), StoreError> {
    // 1. No fact yet, and no feature: an empty column, no list.
    let facts = v3.facts_col();
    fs.append(facts.as_std_path(), &[])?;
    fs.sync_data(facts.as_std_path())?;

    // 2. One run of every changeset record, then the manifest that names it.
    let views = v3.views_dir();
    fs.create_dir_all(views.join(runs::TMP).as_std_path())?;
    let run = (records > 0).then_some(Run {
        domain: Domain::Changesets,
        from: 0,
        to: records,
    });
    let built = match &run {
        Some(run) => Some((*run, write_run(v3, fs, run)?)),
        None => None,
    };
    let manifest = Manifest {
        generation: 1,
        runs: run.into_iter().collect(),
    };
    fs.atomic_replace(views.join(runs::MANIFEST).as_std_path(), &manifest.encode())?;

    // 3. The checkpoint at the journal's end: every frame before it is FORMAT 2's, without a
    //    nonce, so the base seq is their count.
    let frames = journal.frame_count()?;
    let ckpt = Ckpt3 {
        offset: journal.len(),
        seq: frames,
        base_seq: frames,
        records,
        fact_rows: 0,
    };
    fs.atomic_replace(v3.journal_ckpt().as_std_path(), &ckpt.encode())?;

    // 4. The entries of the new files durable, then all read back.
    for dir in [v3.meta_dir(), views, v3.meta_path(IDX_DIR)] {
        fs.fsync_dir(dir.as_std_path())?;
    }
    check_v3(v3, fs, ckpt, &manifest, built)
}

/// Writes `run`'s file from the index's id and delta-hash columns, read a window at a time and
/// sorted: in `tmp/`, synced, then renamed into place (W4′). Returns its bytes.
fn write_run(v3: &Layout, fs: &StoreFs, run: &Run) -> Result<Vec<u8>, StoreError> {
    // 1. The two sections, each sorted; an id twice is damage.
    let mut ids = column_entries(v3, fs, IDS, run.to)?;
    let mut dh = column_entries(v3, fs, DHS, run.to)?;
    ids.sort_unstable();
    dh.sort_unstable();
    if ids.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        return Err(StoreError::IndexCorrupt {
            what: "the index holds an id twice",
        });
    }

    // 2. Written in tmp/, synced, its entry there too (the rename syncs only sorted3/), renamed.
    let bytes = runs::encode_run(run, [&ids, &dh]);
    let tmp = runs::tmp_path(v3, &run.name());
    fs.append(tmp.as_std_path(), &bytes)?;
    fs.sync_data(tmp.as_std_path())?;
    fs.fsync_dir(v3.views_dir().join(runs::TMP).as_std_path())?;
    let path = runs::run_path(v3, run);
    fs.rename_durable(tmp.as_std_path(), path.as_std_path())?;
    Ok(bytes)
}

/// The (key, record number) entries of the first `count` records of index column `rel`.
fn column_entries(
    v3: &Layout,
    fs: &StoreFs,
    rel: &str,
    count: u64,
) -> Result<Vec<([u8; 32], u64)>, StoreError> {
    let column = AppendOnlyColumn::<32>::open(fs, v3.meta_path(rel), "an index column is missing")?;
    let mut entries = Vec::with_capacity(usize::try_from(count).unwrap_or(0));
    column.scan(0, count, |first, keys| {
        entries.extend(keys.iter().zip(first..).map(|(key, rec)| (*key, rec)));
        ControlFlow::<()>::Continue(())
    })?;
    if bytes::to_u64(entries.len()) != count {
        return Err(StoreError::IndexCorrupt {
            what: "an index column is shorter than the index counts",
        });
    }
    Ok(entries)
}

/// Reads FORMAT 3's files back as FORMAT 3 reads them, against what [`write_v3`] wrote: the
/// checkpoint (checked against `journal2.off` by its read), the manifest, the run's bytes, an
/// empty fact column, and an index that opens on them and counts the records.
fn check_v3(
    v3: &Layout,
    fs: &StoreFs,
    ckpt: Ckpt3,
    manifest: &Manifest,
    built: Option<(Run, Vec<u8>)>,
) -> Result<(), StoreError> {
    // 1. The checkpoint and the manifest.
    if Journal::open_in(v3, fs.clone())?.ckpt3()? != ckpt {
        return Err(unequal_v3("its checkpoint"));
    }
    if &runs::read_manifest(v3, fs)?.1 != manifest {
        return Err(unequal_v3("its manifest"));
    }

    // 2. The run, byte for byte, and the fact column, empty.
    if let Some((run, bytes)) = built {
        let path = runs::run_path(v3, &run);
        let read = std::fs::read(&path).map_err(|e| fs.locate(FileOp::ReadAt, &path, e))?;
        if read != bytes {
            return Err(unequal_v3("its run"));
        }
    }
    let facts = v3.facts_col();
    let facts_len = std::fs::metadata(&facts)
        .map_err(|e| fs.locate(FileOp::Stat, &facts, e))?
        .len();
    if facts_len != 0 {
        return Err(unequal_v3("its fact column"));
    }

    // 3. The index, opened on the runs.
    if Index::open_in(v3, fs.clone())?.len() != ckpt.records {
        return Err(unequal_v3("its index"));
    }
    Ok(())
}

/// The error of an upgrade whose FORMAT 3 copy of `what` does not read back as it was written: a
/// defect of this build, found before the poison, so the store is FORMAT 2 still.
fn unequal_v3(what: &str) -> StoreError {
    StoreError::Internal(format!(
        "the upgrade's FORMAT 3 copy of {what} does not read back as written; the store is \
         FORMAT 2 still"
    ))
}
