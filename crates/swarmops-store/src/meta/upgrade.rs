//! The upgrade of a FORMAT 1 store to FORMAT 2 (the M6 design §3): the FORMAT 2 files written
//! beside the FORMAT 1 ones, FORMAT 1's checkpoint poisoned ([`crate::format::UPGRADE_POISON`]),
//! then `FORMAT` replaced, the commit point. An open that finds the poison under a FORMAT 1
//! `FORMAT` finishes the upgrade ([`finish_if_poisoned`]).
//!
//! ```text
//! maint.lock, then meta/lock, held throughout
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
//! ```
//!
//! A crash before 5 leaves the store FORMAT 1, whole: the next upgrade starts over, and gc
//! removes what it left. Between 5 and 6 the next open finishes it. The checkpoint is carried
//! over, not moved: the frames past it are replayed by the next FORMAT 2 write as FORMAT 1's
//! would have been, and the index files, shared by both formats, are not touched.

use std::collections::BTreeMap;
use std::io::{self, Read};
use std::sync::Arc;
use std::time::Duration;

use camino::Utf8Path;
use swarmops_core::bytes;

use super::frame::{self, HEADER_LEN, HEADER_LEN_V2};
use super::index::Index;
use super::journal::{ENTRY_LEN_V2, Journal};
use super::lock::MetaLock;
use super::refs::{self, RefState};
use crate::error::{FileOp, StoreError};
use crate::format::{self, StoreFormat, UPGRADE_POISON};
use crate::fs_ops::{self, FsOps, StoreFs};
use crate::layout::Layout;
use crate::maint::MaintLease;

/// How long an upgrade, and an open that finishes one, waits for `meta/lock`: a metadata store's
/// own wait.
const META_LOCK_TIMEOUT: Duration = Duration::from_secs(10);

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

/// Upgrades the store of `layout` to FORMAT 2; nothing for a FORMAT 2 store. Takes the
/// maintenance lease ([`StoreError::MaintenanceBusy`] at once if it is held), then `meta/lock`
/// (waiting up to 10 s, then [`StoreError::Busy`]), and holds both throughout: other writers
/// wait meanwhile. The steps are the module documentation's; `layout`'s format is not read, the
/// store's `FORMAT` is, under the lock. A FORMAT 1 store whose files do not read
/// ([`StoreError::JournalCorrupt`], a damaged ref file) is not upgraded: it stays FORMAT 1.
pub fn upgrade(layout: &Layout, fs: Arc<dyn FsOps>) -> Result<Upgraded, StoreError> {
    let fs = StoreFs::new(fs, layout.root());

    // 1. The lease, then meta/lock (the M6 design §3), both held to the end.
    let lease = MaintLease::try_acquire(layout)?;
    let _lock = lease.meta_lock(layout, META_LOCK_TIMEOUT)?;

    // 2. What FORMAT says under the lock: only FORMAT 1 is upgraded.
    let from = format::read_format(layout, &fs)?;
    if from != StoreFormat::V1 {
        return Ok(Upgraded::new(from, from, false));
    }

    // 3. The FORMAT 2 files, then the poison; a run that stopped after its poison wrote both.
    let v1 = Layout::with_format(layout.root(), StoreFormat::V1);
    if !poisoned(&v1) {
        write_v2(
            &v1,
            &Layout::with_format(layout.root(), StoreFormat::V2),
            &fs,
        )?;
        fs.atomic_replace(v1.journal_ckpt().as_std_path(), UPGRADE_POISON)?;
    }

    // 4. The commit point.
    fs.atomic_replace(layout.format_file().as_std_path(), StoreFormat::V2.marker())?;
    Ok(Upgraded::new(StoreFormat::V1, StoreFormat::V2, true))
}

/// Finishes an upgrade of the FORMAT 1 store of `layout` that stopped between its poison and
/// its commit point: replaces `FORMAT` if FORMAT 1's checkpoint is the poison. Whether it did.
///
/// The checkpoint is looked at first through `std::fs`, no [`FsOps`] call: a store no upgrade
/// poisoned costs its open one small read. A poisoned one is finished under `meta/lock` (waiting
/// up to 10 s, then [`StoreError::Busy`]), without the maintenance lease, so that an open never
/// waits for a gc; the poison is written only once the FORMAT 2 files are durable, so `FORMAT`
/// is all that is left. Two opens at once both finish it: the second finds `FORMAT` replaced.
pub fn finish_if_poisoned(layout: &Layout, fs: Arc<dyn FsOps>) -> Result<bool, StoreError> {
    let v1 = Layout::with_format(layout.root(), StoreFormat::V1);
    if !poisoned(&v1) {
        return Ok(false);
    }

    // Under the lock, FORMAT again: another open may have finished it meanwhile.
    let fs = StoreFs::new(fs, layout.root());
    let _lock = MetaLock::acquire(&v1, META_LOCK_TIMEOUT)?;
    if format::read_format(layout, &fs)? == StoreFormat::V1 {
        fs.atomic_replace(layout.format_file().as_std_path(), StoreFormat::V2.marker())?;
    }
    Ok(true)
}

/// Whether FORMAT 1's checkpoint of `v1` is the poison, read through `std::fs` and capped one
/// byte past it. A checkpoint that does not read is not the poison: the store's own reads report
/// it, as they would without an upgrade.
fn poisoned(v1: &Layout) -> bool {
    let Ok(file) = fs_ops::open_regular(v1.journal_ckpt()) else {
        return false;
    };
    let mut bytes = Vec::with_capacity(UPGRADE_POISON.len() + 1);
    let read = file
        .take(bytes::to_u64(UPGRADE_POISON.len() + 1))
        .read_to_end(&mut bytes);
    read.is_ok() && bytes == UPGRADE_POISON
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
