//! The loose object store: one self-verifying file per object (storage spec §2).
//!
//! An object file is `"SOB1" | kind u8 {0 blob, 1 tree, 2 changeset} | enc u8 {0 raw,
//! 1 zstd} | u16le 0 | u64le ulen | payload`; the id is always the hash of the decoded
//! payload, so every read is verified.
//!
//! A write hashes the payload, skips the work if the object file exists, writes and
//! `sync_all`s a temp file in `objects/tmp/`, and links it into `objects/<2hex>/`
//! without replacing anything. [`ObjectStore::flush`] then fsyncs each touched fan-out
//! directory once, plus `objects/` itself when a put created a fan-out directory, which
//! is what makes the new names survive power loss. It makes those fsyncs at once, on
//! `put_batch`'s pool: while any process has metadata pending, ext4 turns each directory
//! fsync into a journal commit of its own, and concurrent fsyncs share commits
//! (docs/perf.md).

use std::collections::BTreeSet;
use std::io;
use std::sync::{Mutex, MutexGuard, PoisonError};

use camino::{Utf8Path, Utf8PathBuf};
use rayon::ThreadPool;
use rayon::prelude::*;
use swarmops_core::{ObjectId, ObjectKind, hash_object};

use crate::error::StoreError;
use crate::fs_ops::{CrashAtFs, FsOps, RealFs};
use crate::layout::Layout;

const MAGIC: [u8; 4] = *b"SOB1";
const HEADER_LEN: usize = 16;
const ENC_RAW: u8 = 0;
const ENC_ZSTD: u8 = 1;
/// Shorter payloads are stored raw without trying zstd.
const MIN_COMPRESS_LEN: usize = 128;
const ZSTD_LEVEL: i32 = 3;

/// Content-addressed storage of blobs, trees and changesets (storage spec §8).
pub trait ObjectStore: Send + Sync {
    /// Stores one object and returns its id. Idempotent: an object that already exists
    /// is not written again. The file content is durable on return; its directory entry
    /// becomes durable at the next [`ObjectStore::flush`].
    fn put(&self, kind: ObjectKind, payload: &[u8]) -> Result<ObjectId, StoreError>;

    /// [`ObjectStore::put`] for many objects in parallel; the ids come back in input order.
    fn put_batch(&self, items: Vec<(ObjectKind, Vec<u8>)>) -> Result<Vec<ObjectId>, StoreError>;

    /// Reads one object, verified against `id`; `None` if it is not stored.
    fn get(&self, id: ObjectId) -> Result<Option<(ObjectKind, Vec<u8>)>, StoreError>;

    /// Whether an object file for `id` exists (not verified).
    fn contains(&self, id: ObjectId) -> Result<bool, StoreError>;

    /// Makes every object put since the last flush durable. An implementation may wait on a
    /// thread pool meanwhile, as [`FsObjectStore`]'s does: see there what that rules out.
    fn flush(&self) -> Result<(), StoreError>;

    /// The id of every object stored, in ascending order. Reads no object, so it verifies
    /// none: [`ObjectStore::get`] does.
    fn list(&self) -> Result<Vec<ObjectId>, StoreError>;
}

/// [`ObjectStore`] over loose files in a store's `objects/` directory.
///
/// Needs no lock between processes: no write ever replaces a file, and writers racing
/// on one id produce identical bytes.
///
/// `put_batch` and `flush` run their work on a rayon pool: the store's own once
/// [`FsObjectStore::set_parallelism`] is called, else rayon's global pool. `flush` holds a
/// lock of the store while it waits for its fsyncs on that pool, and a pool thread that waits
/// may run other tasks of the pool meanwhile. So never call `flush` (or anything that
/// flushes, such as a commit) from a task on that same pool: if such a task flushed the same
/// store while another flush waited, the two would deadlock.
pub struct FsObjectStore {
    layout: Layout,
    fs: Box<dyn FsOps>,
    /// Directories `flush` must fsync: the fan-out directory of every object put since
    /// the last flush, and `objects/` if one of those fan-out directories was created.
    touched: Mutex<BTreeSet<Utf8PathBuf>>,
    /// Held by `flush` from taking the touched set through its last fsync, so that no
    /// flush returns while an earlier one is still fsyncing directories the caller's
    /// objects rely on. Lock order: `flush_lock`, then `touched`; `put` takes only `touched`.
    flush_lock: Mutex<()>,
    /// The pool of `put_batch` and of `flush`'s fsyncs once
    /// [`FsObjectStore::set_parallelism`] is called; until then, rayon's global pool.
    pool: Option<ThreadPool>,
}

impl FsObjectStore {
    /// The object store of `layout`, doing all filesystem access through `fs`. Opening
    /// touches nothing; directories are created by the first write that needs them.
    pub fn open(layout: Layout, fs: Box<dyn FsOps>) -> Self {
        Self {
            layout,
            fs,
            touched: Mutex::new(BTreeSet::new()),
            flush_lock: Mutex::new(()),
            pool: None,
        }
    }

    /// [`FsObjectStore::open`] on the real filesystem, wrapped in a [`CrashAtFs`] when a test
    /// hook asks for one ([`CrashAtFs::from_env`]: `SWARMOPS_TEST_CRASH_AT=<k>`,
    /// `SWARMOPS_TEST_FS_CALLS=<file>`, in a build with the `test-hooks` feature only). Under
    /// either hook the store runs with parallelism 1 ([`FsObjectStore::set_parallelism`]):
    /// `put_batch` then writes one object at a time and `flush` fsyncs one directory at a time,
    /// so that call k is the same call in every run.
    pub fn open_default(layout: Layout) -> Self {
        match CrashAtFs::from_env() {
            Some(fs) => {
                let mut store = Self::open(layout, Box::new(fs));
                store.set_parallelism(1);
                store
            }
            None => Self::open(layout, Box::new(RealFs)),
        }
    }

    /// Runs [`ObjectStore::put_batch`], and the directory fsyncs of [`ObjectStore::flush`], on
    /// a pool of its own with `threads` threads (0: one per available core). With 1 both go
    /// one at a time, `flush` on the calling thread and in path order, so that a test counting
    /// [`FsOps`] calls (a crash sweep's [`CrashAtFs`]) meets the same call at the same count in
    /// every run. Until this is called, both use rayon's global pool, which by default also
    /// has one thread per available core.
    ///
    /// # Panics
    ///
    /// If the threads cannot be spawned.
    pub fn set_parallelism(&mut self, threads: usize) {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .expect("spawn the object store's thread pool");
        self.pool = Some(pool);
    }

    /// One `fsync_dir` of each of `dirs`, every one made whatever the others return; the
    /// results in `dirs`' order. One after another on this thread when the pool has one
    /// thread (or there is one dir); otherwise on the pool, as many at once as it has threads.
    fn fsync_dirs(&self, dirs: &[Utf8PathBuf]) -> Vec<io::Result<()>> {
        let fsync = |dir: &Utf8PathBuf| self.fs.fsync_dir(dir.as_std_path());
        let threads = self
            .pool
            .as_ref()
            .map_or_else(rayon::current_num_threads, ThreadPool::current_num_threads);
        if threads <= 1 || dirs.len() <= 1 {
            return dirs.iter().map(fsync).collect();
        }
        // `flush` holds `flush_lock` meanwhile, and nothing on the pool takes it.
        let fsync_all = || dirs.par_iter().map(fsync).collect();
        match &self.pool {
            Some(pool) => pool.install(fsync_all),
            None => fsync_all(),
        }
    }

    fn touched(&self) -> MutexGuard<'_, BTreeSet<Utf8PathBuf>> {
        // Every update is a single insert or take, so the set is valid even if a holder
        // panicked; poisoning carries no information here.
        self.touched.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Writes the file for an object that is not stored yet: temp file, `sync_all`,
    /// no-clobber link into its fan-out directory `fanout`.
    fn write_new(
        &self,
        kind: ObjectKind,
        payload: &[u8],
        path: &Utf8Path,
        fanout: &Utf8Path,
    ) -> Result<(), StoreError> {
        let file = encode(kind, payload)?;
        let tmp_dir = self.layout.objects_tmp();
        self.fs.create_dir_all(tmp_dir.as_std_path())?;
        self.ensure_fanout(fanout)?;
        let temp = self.fs.write_durable_temp(tmp_dir.as_std_path(), &file)?;
        // `AlreadyExists` counts as success: a racing writer placed the same id, which in a
        // content-addressed store means the same bytes.
        if let Err(err) = self.fs.link_noclobber(&temp, path.as_std_path()) {
            // Best effort: a leftover temp file is harmless; the link error is what to report.
            let _ = self.fs.remove(&temp);
            return Err(err.into());
        }
        Ok(())
    }

    /// Creates fan-out directory `fanout` if it is missing. A new directory's own entry
    /// lives in `objects/`, so `objects/` then joins the touched set as well.
    ///
    /// The set stays locked from the check to the mark: no concurrent flush can fsync
    /// `objects/` between the mkdir and the mark, and no other put can see the new
    /// directory before the mark is in.
    fn ensure_fanout(&self, fanout: &Utf8Path) -> Result<(), StoreError> {
        let mut touched = self.touched();
        if !self.fs.exists(fanout.as_std_path())? {
            self.fs.create_dir_all(fanout.as_std_path())?;
            touched.insert(self.layout.objects_dir());
        }
        Ok(())
    }
}

impl ObjectStore for FsObjectStore {
    fn put(&self, kind: ObjectKind, payload: &[u8]) -> Result<ObjectId, StoreError> {
        let id = hash_object(kind, payload);
        let path = self.layout.object_path(id);
        let fanout = path
            .parent()
            .expect("an object path has a fan-out directory");
        if !self.fs.exists(path.as_std_path())? {
            self.write_new(kind, payload, &path, fanout)?;
        }
        // Touched even when the object was already there: another process may have linked
        // it without having fsynced the directory yet, and the caller is about to rely on it.
        self.touched().insert(fanout.to_owned());
        Ok(id)
    }

    fn put_batch(&self, items: Vec<(ObjectKind, Vec<u8>)>) -> Result<Vec<ObjectId>, StoreError> {
        let put_all = || {
            items
                .into_par_iter()
                .map(|(kind, payload)| self.put(kind, &payload))
                .collect()
        };
        match &self.pool {
            Some(pool) => pool.install(put_all),
            None => put_all(),
        }
    }

    fn get(&self, id: ObjectId) -> Result<Option<(ObjectKind, Vec<u8>)>, StoreError> {
        match self.fs.read(self.layout.object_path(id).as_std_path())? {
            Some(file) => decode(id, &file).map(Some),
            None => Ok(None),
        }
    }

    fn contains(&self, id: ObjectId) -> Result<bool, StoreError> {
        Ok(self.fs.exists(self.layout.object_path(id).as_std_path())?)
    }

    /// The files of the 256 fan-out directories whose names are object ids (62 lowercase hex
    /// digits). Everything else under `objects/` is passed over: `objects/tmp/`, where the
    /// temp files a crash left behind lie, and stray entries. Lists the directories with
    /// `std::fs` (an [`FsOps`] has no listing), and runs no [`FsOps`] call.
    fn list(&self) -> Result<Vec<ObjectId>, StoreError> {
        let mut ids = Vec::new();
        for byte in 0..=u8::MAX {
            let fanout = format!("{byte:02x}");
            let entries = match std::fs::read_dir(self.layout.objects_dir().join(&fanout)) {
                Ok(entries) => entries,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            for entry in entries {
                let entry = entry?;
                let name = entry.file_name();
                let Some(rest) = name.to_str().filter(|rest| is_object_name(rest)) else {
                    continue;
                };
                if entry.file_type()?.is_file() {
                    let id = ObjectId::from_hex(&format!("{fanout}{rest}"))
                        .expect("two and 62 hex digits make an id");
                    ids.push(id);
                }
            }
        }
        ids.sort_unstable();
        Ok(ids)
    }

    /// Fsyncs every touched directory (see [`FsObjectStore::set_parallelism`] for how many at
    /// once), all of them even when some fail. The ones that fail stay touched, so that a
    /// later flush retries them, and the error of the first of those in path order comes back.
    ///
    /// It holds the store's flush lock until the last fsync is done, waiting on the store's
    /// pool: do not call it from a task on that pool (see [`FsObjectStore`]).
    fn flush(&self) -> Result<(), StoreError> {
        // `()` carries no state that a panicking holder could have left inconsistent.
        let _flushing = self
            .flush_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let pending: Vec<Utf8PathBuf> = std::mem::take(&mut *self.touched()).into_iter().collect();
        let results = self.fsync_dirs(&pending);
        let mut first_error = None;
        for (dir, result) in pending.into_iter().zip(results) {
            if let Err(err) = result {
                self.touched().insert(dir);
                first_error.get_or_insert(err);
            }
        }
        match first_error {
            Some(err) => Err(err.into()),
            None => Ok(()),
        }
    }
}

/// Whether `name` is what an object file in a fan-out directory is named: the last 62 hex
/// digits of its id, lowercase.
fn is_object_name(name: &str) -> bool {
    name.len() == 62 && name.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn kind_byte(kind: ObjectKind) -> u8 {
    match kind {
        ObjectKind::Blob => 0,
        ObjectKind::Tree => 1,
        ObjectKind::Changeset => 2,
    }
}

fn kind_from_byte(byte: u8) -> Option<ObjectKind> {
    match byte {
        0 => Some(ObjectKind::Blob),
        1 => Some(ObjectKind::Tree),
        2 => Some(ObjectKind::Changeset),
        _ => None,
    }
}

/// The object file for `payload`: zstd when the payload has at least 128 bytes and
/// compresses smaller, raw otherwise.
fn encode(kind: ObjectKind, payload: &[u8]) -> io::Result<Vec<u8>> {
    let compressed = if payload.len() >= MIN_COMPRESS_LEN {
        Some(compress(payload)?).filter(|c| c.len() < payload.len())
    } else {
        None
    };
    let (enc, body) = match &compressed {
        Some(c) => (ENC_ZSTD, c.as_slice()),
        None => (ENC_RAW, payload),
    };
    let ulen = u64::try_from(payload.len()).expect("payload length fits in u64");
    let mut file = Vec::with_capacity(HEADER_LEN + body.len());
    file.extend_from_slice(&MAGIC);
    file.push(kind_byte(kind));
    file.push(enc);
    file.extend_from_slice(&0u16.to_le_bytes());
    file.extend_from_slice(&ulen.to_le_bytes());
    file.extend_from_slice(body);
    Ok(file)
}

/// zstd level 3 with the frame content checksum on (storage spec §0).
fn compress(payload: &[u8]) -> io::Result<Vec<u8>> {
    let mut compressor = zstd::bulk::Compressor::new(ZSTD_LEVEL)?;
    compressor.include_checksum(true)?;
    compressor.compress(payload)
}

/// Parses the object file read for `id` and verifies that its payload hashes to `id`.
fn decode(id: ObjectId, file: &[u8]) -> Result<(ObjectKind, Vec<u8>), StoreError> {
    let bad_header = || StoreError::BadHeader { id };
    let corrupt = || StoreError::Corrupt { id };
    let (header, body) = file
        .split_first_chunk::<HEADER_LEN>()
        .ok_or_else(bad_header)?;
    if header[..4] != MAGIC || header[6..8] != [0, 0] {
        return Err(bad_header());
    }
    let kind = kind_from_byte(header[4]).ok_or_else(bad_header)?;
    let ulen = u64::from_le_bytes(header[8..].try_into().expect("8 bytes"));
    let ulen = usize::try_from(ulen).map_err(|_| bad_header())?;
    let payload = match header[5] {
        ENC_RAW if body.len() == ulen => body.to_vec(),
        ENC_ZSTD => {
            // Every frame this store writes records its decompressed size (one without it
            // is damaged). It must equal ulen, which then bounds the output buffer.
            let frame_len = zstd::bulk::Decompressor::upper_bound(body).ok_or_else(corrupt)?;
            if frame_len != ulen {
                return Err(bad_header());
            }
            zstd::bulk::decompress(body, ulen).map_err(|_| corrupt())?
        }
        _ => return Err(bad_header()),
    };
    if hash_object(kind, &payload) != id {
        return Err(corrupt());
    }
    Ok((kind, payload))
}
