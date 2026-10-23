//! The derived-value store (storage spec §3 and §5): values derived from immutable objects (a
//! changeset's first-parent ancestors, the changeset that last touched a path, …), each in an
//! immutable, checksummed file of its own under `meta/derived/`, named by its slot
//! ([`DerivedSlot`]): the derivation's name and version, the object the value is about, and a key.
//!
//! The files are a cache, outside the journal: no record and no other file names one, so a power
//! loss that takes a value back costs a recomputation, never a commit. A put writes the file to a
//! new temp file in `meta/derived/tmp/` and links it into place without replacing anything
//! ([`FsOps::link_noclobber`]), and syncs nothing: no file sync, no directory fsync. It takes no
//! lock: no write ever replaces a file, and puts racing on one slot each write a temp file of
//! their own, of which one is linked. A read checks what the file says against the slot it was
//! asked for, and a trailer, BLAKE3 over the slot and the value, against both; a file that fails
//! (torn, cut short, zero-filled, flipped, or another slot's file) is removed, best effort, and
//! reads as missing, so that the value is derived and stored again. No value holds more than an
//! object does ([`MAX_OBJECT_LEN`], 512 MiB): a put refuses a longer one, and a file whose header
//! records a longer one fails its checks before anything is decoded or allocated for it.
//!
//! A file is `"SWDV" | u8 1 | u8 enc | u16le 0 | u32le key_len | u64le value_len | key | payload
//! | trailer`: the payload is the value itself (`enc` 0) or, for a value over 4 KiB, its zstd
//! frame (`enc` 1; level 3, with zstd's content checksum), and the trailer is
//! `blake3("swarmops:derived:v1\0" ++ u8 name_len ++ name ++ u32le version ++ subject ++
//! u32le key_len ++ key ++ u64le value_len ++ value)`, over the value before compression. The
//! layout has a header of its own, outside the journal's `FORMAT`: a new layout gets a new
//! version byte.
//!
//! A check of the store lists a derivation's value files by their names ([`DerivedStore::list`])
//! and reads each with its key ([`DerivedStore::read_listed`]), removing nothing.

use std::borrow::Cow;
use std::hash::{BuildHasher, RandomState};
use std::io::{self, Read};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use camino::Utf8Path;
use swarmops_core::ObjectId;
use swarmops_core::bytes::{self, Reader};

use crate::error::{FileOp, StoreError};
use crate::external_ids;
use crate::fs_objects::MAX_OBJECT_LEN;
use crate::fs_ops::{FsOps, LinkOutcome, StoreFs};
use crate::layout::{self, Layout};

const MAGIC: [u8; 4] = *b"SWDV";
/// The version of the file layout: its fifth byte.
const LAYOUT_VERSION: u8 = 1;
const ENC_RAW: u8 = 0;
const ENC_ZSTD: u8 = 1;
/// `"SWDV" | u8 version | u8 enc | u16le 0 | u32le key_len | u64le value_len`.
const HEADER_LEN: usize = 20;
/// The trailer's length: a BLAKE3 hash.
const TRAILER_LEN: usize = 32;
/// The domain of the trailer's hash, so that it hashes nothing an object id hashes.
const DOMAIN: &[u8] = b"swarmops:derived:v1\0";
/// A value of more bytes than this is stored as a zstd frame.
const RAW_MAX: usize = 4 << 10;
const ZSTD_LEVEL: i32 = 3;
/// The most a zstd payload's buffer starts at; it grows past that only as the output arrives, so
/// that a header and a frame that both claim a size their bytes do not make cost no more than
/// this before they are found out.
const PREALLOC_MAX: usize = 1 << 20;
/// The longest name a derivation takes, in bytes.
const NAME_MAX: usize = 64;

/// Where one derived value lives: the derivation that makes it, by name and version, the object
/// it is about, and a key that tells its values about that object apart (a path, say; empty for
/// one value per object). Its file is `meta/derived/<name>/<version>/<2 hex>/<62 hex>`, the
/// subject's hex digits, with `.<16 hex>` appended when the key is not empty: the first 8 bytes of
/// BLAKE3 of the key ([`Layout::derived_path`]). The file holds the whole key too, so that two
/// keys whose hashes share those bytes never read each other's value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DerivedSlot<'a> {
    /// The derivation's name, its directory under `meta/derived/`: 1 to 64 bytes of `[a-z0-9_]`,
    /// and not `tmp`, the directory of the temp files.
    pub name: &'a str,
    /// The derivation's version, at least 1: a new version is a directory of its own, so that
    /// values an older one derived are never read.
    pub version: u32,
    /// The object the value is about: any object id (a changeset's for the derivation engine's
    /// values, a tree's for a value per tree).
    pub subject: ObjectId,
    /// What tells the subject's values apart, as bytes; empty for one value per subject. At most
    /// `u32::MAX` bytes.
    pub key: &'a [u8],
}

impl<'a> DerivedSlot<'a> {
    /// The slot of derivation `name` at `version`, about `subject`, under `key`. Takes any
    /// value: a store call given a slot it does not take (see the fields) refuses it as
    /// [`StoreError::BadCall`].
    pub fn new(name: &'a str, version: u32, subject: ObjectId, key: &'a [u8]) -> DerivedSlot<'a> {
        DerivedSlot {
            name,
            version,
            subject,
            key,
        }
    }
}

impl DerivedSlot<'_> {
    /// The key's length, as the file records it, if the slot is one a store takes (see
    /// [`DerivedSlot`]'s fields); [`StoreError::BadCall`] otherwise.
    fn key_len(&self) -> Result<u32, StoreError> {
        check_derivation(self.name, self.version)?;
        u32::try_from(self.key.len()).map_err(|_| {
            StoreError::BadCall(format!(
                "derived value key of {} bytes: at most {}",
                self.key.len(),
                u32::MAX
            ))
        })
    }

    /// The trailer of the slot's file holding `value`: BLAKE3 over the domain, the slot and the
    /// value, each length-prefixed as the module documentation gives it. `key_len` is the key's
    /// length ([`DerivedSlot::key_len`]).
    fn trailer(&self, key_len: u32, value: &[u8]) -> [u8; 32] {
        let name_len = u8::try_from(self.name.len()).expect("a checked name has at most 64 bytes");
        let mut hasher = blake3::Hasher::new();
        hasher.update(DOMAIN);
        hasher.update(&[name_len]);
        hasher.update(self.name.as_bytes());
        hasher.update(&self.version.to_le_bytes());
        hasher.update(&self.subject.0);
        hasher.update(&key_len.to_le_bytes());
        hasher.update(self.key);
        hasher.update(&bytes::to_u64(value.len()).to_le_bytes());
        hasher.update(value);
        *hasher.finalize().as_bytes()
    }
}

/// [`StoreError::BadCall`] unless `name` and `version` are a derivation's that a store takes
/// ([`DerivedSlot::name`], [`DerivedSlot::version`]).
fn check_derivation(name: &str, version: u32) -> Result<(), StoreError> {
    if !is_derived_name(name) {
        return Err(StoreError::BadCall(format!(
            "derived value name {name:?}: not 1 to 64 bytes of [a-z0-9_], or tmp"
        )));
    }
    if version == 0 {
        return Err(StoreError::BadCall(format!(
            "derived value version 0 of {name}: versions start at 1"
        )));
    }
    Ok(())
}

/// A value file of a derivation's directory, as [`DerivedStore::list`] finds it by its name: the
/// subject the name gives, and the hash of the key it ends in, if any. [`DerivedStore::read_listed`]
/// reads the key and the value the file holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ListedValue {
    /// The object the value is about: its first byte the file's fan-out directory, the rest the
    /// name's first 62 hex digits.
    pub subject: ObjectId,
    /// The first 8 bytes of BLAKE3 of the value's key, which the file's name ends in
    /// (`.<16 hex>`); `None` for a name without them, a value whose key is empty.
    pub key_hash: Option<[u8; 8]>,
}

impl ListedValue {
    /// The value file about `subject` whose name ends in `key_hash`, if any.
    pub fn new(subject: ObjectId, key_hash: Option<[u8; 8]>) -> ListedValue {
        ListedValue { subject, key_hash }
    }
}

/// What a whole value file holds, as [`DerivedStore::read_listed`] reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredValue {
    /// The key of the slot it is the file of, as the file records it.
    pub key: Vec<u8>,
    /// The value.
    pub value: Vec<u8>,
}

impl StoredValue {
    /// A file holding `value` under `key`.
    pub fn new(key: Vec<u8>, value: Vec<u8>) -> StoredValue {
        StoredValue { key, value }
    }
}

/// Whether `name` is one a derivation takes: 1 to 64 bytes of `[a-z0-9_]`, and not `tmp`, which
/// names the directory of the temp files.
pub(crate) fn is_derived_name(name: &str) -> bool {
    (1..=NAME_MAX).contains(&name.len())
        && name
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_'))
        && name != "tmp"
}

/// The store of derived values under `meta/derived/` (see [the module documentation](self)):
/// [`DerivedStore::get`] and [`DerivedStore::put`] by slot, through the store's [`FsOps`], with
/// no lock and no sync. Opening it touches nothing; a put makes the directories it needs.
#[derive(Debug, Clone)]
pub struct DerivedStore {
    layout: Layout,
    /// Every filesystem call goes through it, its failure located in the store.
    fs: StoreFs,
}

impl DerivedStore {
    /// The derived-value store of `layout`'s store, its files read and written through `fs`: the
    /// filesystem its object and metadata stores share, so that they count their calls together.
    /// Touches nothing.
    pub fn open(layout: Layout, fs: Arc<dyn FsOps>) -> DerivedStore {
        let fs = StoreFs::new(fs, layout.root());
        DerivedStore { layout, fs }
    }

    /// The value in `slot`, or `None`: none was stored, a power loss took it back, or its file is
    /// damaged. A file whose header, lengths, key or trailer does not check out against `slot` is
    /// removed, best effort, so that a put can store the value again; one the store cannot remove
    /// reads as missing all the same, every time (storage spec §3). A header that records a value
    /// longer than [`MAX_OBJECT_LEN`] is such a file, found out before its payload is decoded.
    /// Reads the file whole, once ([`FsOps::read`]).
    ///
    /// [`StoreError::BadCall`] for a slot no store takes ([`DerivedSlot`]), before any read; a
    /// failed read is [`StoreError::Io`], naming the file.
    pub fn get(&self, slot: &DerivedSlot<'_>) -> Result<Option<Vec<u8>>, StoreError> {
        let key_len = slot.key_len()?;
        let path = self.layout.derived_path(slot);
        let Some(file) = self.fs.read(&path)? else {
            return Ok(None);
        };
        let value = decode(slot, key_len, file);
        if value.is_none() {
            // Best effort: a file that cannot be removed reads as missing all the same.
            let _ = self.fs.remove(&path);
        }
        Ok(value)
    }

    /// Stores `value` in `slot`, unless the slot's file exists: then this writes nothing, and the
    /// slot keeps its value, whatever `value` is (a slot is one derivation's value, at one version,
    /// for one subject and key: it never changes). `true` if this call placed the file; `false` if
    /// it found one, or a racing put placed its own first.
    ///
    /// The file is written to a new temp file in `meta/derived/tmp/` ([`FsOps::append`]) and
    /// linked into the slot without replacing anything ([`FsOps::link_noclobber`], which removes
    /// the temp file), after the directories it needs are made ([`FsOps::create_dir_all`]).
    /// Nothing is synced (storage spec §3): nothing durable names a derived value, and a power
    /// loss that takes one back, or leaves its file torn, costs a recomputation. A put that fails
    /// leaves no file in the slot, and removes its temp file, best effort; what a crash leaves in
    /// `meta/derived/tmp/` stays there.
    ///
    /// [`StoreError::BadCall`] for a slot no store takes ([`DerivedSlot`]), and for a value longer
    /// than an object holds ([`MAX_OBJECT_LEN`]), which no read would take back, before anything
    /// is written; a failed filesystem call is [`StoreError::Io`], naming the file.
    pub fn put(&self, slot: &DerivedSlot<'_>, value: &[u8]) -> Result<bool, StoreError> {
        let key_len = slot.key_len()?;
        if bytes::to_u64(value.len()) > MAX_OBJECT_LEN {
            return Err(StoreError::BadCall(format!(
                "a derived value of {} bytes: at most {MAX_OBJECT_LEN}",
                value.len()
            )));
        }
        let path = self.layout.derived_path(slot);
        if self.fs.exists(&path)? {
            return Ok(false);
        }
        let file =
            encode(slot, key_len, value).map_err(|e| self.fs.locate(FileOp::Compress, &path, e))?;
        let tmp_dir = self.layout.derived_tmp();
        self.fs.create_dir_all(&tmp_dir)?;
        self.fs
            .create_dir_all(path.parent().expect("a slot's file has a directory"))?;
        let temp = tmp_dir.join(temp_name());
        if let Err(err) = self.fs.append(&temp, &file) {
            // Best effort: what the append left of the temp file is harmless; its error is what
            // to report.
            let _ = self.fs.remove(&temp);
            return Err(err.into());
        }
        match self.fs.link_noclobber(&temp, &path) {
            Ok(LinkOutcome::Placed) => Ok(true),
            Ok(LinkOutcome::AlreadyExists) => Ok(false),
            Err(err) => {
                // Best effort, as above: a failed link may leave the temp file.
                let _ = self.fs.remove(&temp);
                Err(err.into())
            }
        }
    }
}

impl DerivedStore {
    /// Every value file of derivation `name` at `version`, sorted: each name under
    /// `meta/derived/<name>/<version>/` that [`Layout::derived_path`] gives a slot, a fan-out
    /// directory of two lowercase hex digits and in it a file of the other 62, ending in `.` and 16
    /// more where the slot's key is not empty. Anything else is passed over, as the object store's
    /// listing passes over strays. Lists the directories with `std::fs` (an [`FsOps`] has no
    /// listing), following a link to a directory as a read of a file below it does, and reads no
    /// file ([`DerivedStore::read_listed`] reads one); a listing that fails fails the call, naming
    /// the directory.
    ///
    /// [`StoreError::BadCall`] for a name or a version no store takes ([`DerivedSlot`]), before
    /// anything is listed.
    pub fn list(&self, name: &str, version: u32) -> Result<Vec<ListedValue>, StoreError> {
        check_derivation(name, version)?;
        let dir = self
            .layout
            .derived_dir()
            .join(name)
            .join(version.to_string());
        let mut listed = Vec::new();
        for fanout in self.names_in(&dir)? {
            let Some(first) = external_ids::from_hex(&fanout).filter(|first| first.len() == 1)
            else {
                continue;
            };
            for file in self.names_in(&dir.join(&fanout))? {
                if let Some(value) = listed_value(&first, &file) {
                    listed.push(value);
                }
            }
        }
        listed.sort_unstable();
        Ok(listed)
    }

    /// What the file `listed` names in derivation `name`'s `version` directory holds
    /// ([`DerivedStore::list`]), if it is whole: the key the file records, whose hash must be
    /// the one its name ends in, and the value, checked as [`DerivedStore::get`] checks the file of
    /// the slot of that key. `None` if the file is gone, is not a regular file (nor a link to one),
    /// or does not check out. Unlike [`DerivedStore::get`] it removes nothing, so that a check of
    /// a store changes nothing in it. One [`FsOps::read`].
    ///
    /// [`StoreError::BadCall`] for a name or a version no store takes ([`DerivedSlot`]), before
    /// any read; a read that fails otherwise is [`StoreError::Io`], naming the file.
    pub fn read_listed(
        &self,
        name: &str,
        version: u32,
        listed: &ListedValue,
    ) -> Result<Option<StoredValue>, StoreError> {
        check_derivation(name, version)?;
        let path = self
            .layout
            .derived_file(name, version, listed.subject, listed.key_hash);
        let file = match self.fs.read(&path) {
            Ok(Some(file)) => file,
            Ok(None) => return Ok(None),
            // No value file: a read of its slot fails on it as on any store file that is not a
            // regular file.
            Err(e) if e.kind() == io::ErrorKind::InvalidData => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let Some(key) = recorded_key(&file).map(<[u8]>::to_vec) else {
            return Ok(None);
        };
        if layout::derived_key_hash(&key) != listed.key_hash {
            return Ok(None);
        }
        let slot = DerivedSlot {
            name,
            version,
            subject: listed.subject,
            key: &key,
        };
        let key_len = slot.key_len()?;
        Ok(decode(&slot, key_len, file).map(|value| StoredValue { key, value }))
    }

    /// The names in `dir`, read with `std::fs`; none if `dir` does not exist or is not a
    /// directory. A name that is not UTF-8 is no name the store gives, and is passed over.
    fn names_in(&self, dir: &Utf8Path) -> Result<Vec<String>, StoreError> {
        let listing_failed = |e| self.fs.locate(FileOp::ReadDir, dir, e);
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                return Ok(Vec::new());
            }
            Err(e) => return Err(listing_failed(e).into()),
        };
        let mut names = Vec::new();
        for entry in entries {
            if let Ok(name) = entry.map_err(listing_failed)?.file_name().into_string() {
                names.push(name);
            }
        }
        Ok(names)
    }
}

/// The value file `file` names in the fan-out directory of subject byte `first`, if `file` is
/// one's name: 62 lowercase hex digits, the subject's other 31 bytes, then `.` and 16 more where
/// the slot's key is not empty, the first 8 bytes of BLAKE3 of the key.
fn listed_value(first: &[u8], file: &str) -> Option<ListedValue> {
    let (rest, key_hash) = match file.split_once('.') {
        Some((rest, hash)) => {
            let hash: [u8; 8] = external_ids::from_hex(hash)?.try_into().ok()?;
            (rest, Some(hash))
        }
        None => (file, None),
    };
    let rest = external_ids::from_hex(rest).filter(|rest| rest.len() == 31)?;
    let subject: [u8; 32] = [first, &rest].concat().try_into().ok()?;
    Some(ListedValue {
        subject: ObjectId(subject),
        key_hash,
    })
}

/// The key a value file records, if its header and its lengths leave room for one: the
/// `key_len` bytes after the header, before the trailer.
fn recorded_key(file: &[u8]) -> Option<&[u8]> {
    let key_len: [u8; 4] = file.get(8..12)?.try_into().ok()?;
    let key_len = usize::try_from(u32::from_le_bytes(key_len)).ok()?;
    let end = HEADER_LEN.checked_add(key_len)?;
    if end.checked_add(TRAILER_LEN)? > file.len() {
        return None;
    }
    file.get(HEADER_LEN..end)
}

/// A name for a new temp file that no other put takes, in this process or another:
/// `<pid>-<counter>-<16 hex digits>`, the counter the process's own and the hex digits 64 random
/// bits (the standard library's randomly keyed hasher over the time, the pid and the counter, as
/// a repository's id is made), so that an append never meets a file it did not make.
fn temp_name() -> String {
    static PUTS: AtomicU64 = AtomicU64::new(0);
    let counter = PUTS.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    let pid = std::process::id();
    let random = RandomState::new().hash_one((nanos, pid, counter));
    format!("{pid}-{counter}-{random:016x}")
}

/// `slot`'s file holding `value`; `key_len` is the key's length ([`DerivedSlot::key_len`]). The
/// one error is zstd's, compressing a value over 4 KiB.
fn encode(slot: &DerivedSlot<'_>, key_len: u32, value: &[u8]) -> io::Result<Vec<u8>> {
    let (enc, payload) = if value.len() > RAW_MAX {
        (ENC_ZSTD, Cow::Owned(compress(value)?))
    } else {
        (ENC_RAW, Cow::Borrowed(value))
    };
    let mut file = Vec::with_capacity(HEADER_LEN + slot.key.len() + payload.len() + TRAILER_LEN);
    file.extend_from_slice(&MAGIC);
    file.push(LAYOUT_VERSION);
    file.push(enc);
    file.extend_from_slice(&0_u16.to_le_bytes());
    file.extend_from_slice(&key_len.to_le_bytes());
    file.extend_from_slice(&bytes::to_u64(value.len()).to_le_bytes());
    file.extend_from_slice(slot.key);
    file.extend_from_slice(&payload);
    file.extend_from_slice(&slot.trailer(key_len, value));
    Ok(file)
}

/// zstd level 3 with the frame content checksum on, as an object file's.
fn compress(value: &[u8]) -> io::Result<Vec<u8>> {
    let mut compressor = zstd::bulk::Compressor::new(ZSTD_LEVEL)?;
    compressor.include_checksum(true)?;
    compressor.compress(value)
}

/// The value `file` holds, if it is `slot`'s file and whole: its header is this layout's, its key
/// is the slot's, its value's length is at most [`MAX_OBJECT_LEN`] (checked before the payload is
/// decoded, so that a header cannot size what a read allocates), its lengths add up to the
/// file's, its payload decodes to the value's length, and its trailer is the slot's and the
/// value's. `None` if anything fails. A raw value comes back in `file` itself, the rest cut off
/// around it.
fn decode(slot: &DerivedSlot<'_>, key_len: u32, mut file: Vec<u8>) -> Option<Vec<u8>> {
    let mut reader = Reader::new(&file, |_| ());
    let header: [u8; 8] = reader.array().ok()?;
    if header[..4] != MAGIC || header[4] != LAYOUT_VERSION || header[6..] != [0, 0] {
        return None;
    }
    let enc = header[5];
    if reader.array().map(u32::from_le_bytes).ok()? != key_len {
        return None;
    }
    let value_len = reader.u64_le().ok()?;
    // No value holds more than an object does: refused before a buffer is sized by it.
    if value_len > MAX_OBJECT_LEN {
        return None;
    }
    let value_len = usize::try_from(value_len).ok()?;
    if reader.bytes(slot.key.len()).ok()? != slot.key {
        return None;
    }
    let start = reader.pos();
    let payload_len = reader.rest().len().checked_sub(TRAILER_LEN)?;
    let payload = reader.bytes(payload_len).ok()?;
    let trailer = reader.rest();
    match enc {
        ENC_RAW if payload_len == value_len => {
            if trailer != slot.trailer(key_len, payload) {
                return None;
            }
            file.truncate(start + value_len);
            file.drain(..start);
            Some(file)
        }
        ENC_ZSTD => {
            let value = decompress(payload, value_len)?;
            (trailer == slot.trailer(key_len, &value)).then_some(value)
        }
        _ => None,
    }
}

/// `payload`, a zstd frame that records `len` bytes, decoded: `None` unless the frame records
/// `len` bytes and makes exactly that many. Its buffer starts at no more than [`PREALLOC_MAX`]
/// bytes and grows as the output arrives, and no more than `len + 1` bytes are read, so that a
/// frame never sizes its own allocation.
fn decompress(payload: &[u8], len: usize) -> Option<Vec<u8>> {
    if zstd::bulk::Decompressor::upper_bound(payload)? != len {
        return None;
    }
    let mut value = Vec::with_capacity(len.min(PREALLOC_MAX));
    zstd::stream::read::Decoder::with_buffer(payload)
        .ok()?
        .take(bytes::to_u64(len).saturating_add(1))
        .read_to_end(&mut value)
        .ok()?;
    (value.len() == len).then_some(value)
}
