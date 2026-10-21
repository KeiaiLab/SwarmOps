//! The external id index (storage spec §7, M3): which changeset of the store an id of another
//! system names, a git commit's oid say, one small file per id under
//! `meta/external_ids/<system>/<first 2 hex>/<remaining hex>`, named by the key's lowercase hex.
//!
//! An entry is 44 bytes:
//!
//! ```text
//! "SXID" | u8 version = 1 | u8 flags (bit 0: faithful; the others 0) | u16le 0
//!        | changeset id [32] | u32le crc32 of the 40 bytes before it
//! ```
//!
//! An entry's file is read no further than a byte past an entry, 45 bytes, on a handle of the
//! index's own (`std::fs`, with no [`FsOps`] call: [`FsOps::read`] takes a whole file): a longer
//! file is no entry, however long, sparse or crafted, and costs no more to read than that.
//!
//! An entry never changes once written. [`ExternalIds::put_batch`] writes one as the object store
//! writes an object (storage spec §2 rule 1, through the same sequence): a temp file in
//! `objects/tmp/`, synced, linked into place without replacing anything; then it fsyncs, once
//! each, every fan-out directory it put an entry into and the parent of every directory it made,
//! before it returns. An entry already there with the same bytes is left as it is; one with
//! other bytes is left as it is too, and the call names its key as a conflict. Writers take no
//! lock: they never replace a file.
//!
//! The index is a cache outside the journal, not a projection of it. An entry is written only
//! once the journal frame that records its changeset is durable (W8, storage spec §2 rule 4),
//! so that no entry names a changeset a power loss can take back; an entry a crash loses is
//! written again by whatever wrote it (an import converts the commit again, finds its changeset
//! in the store, and records the entry); and the way back, from a changeset to its git oid, is the
//! changeset's own `git.oid` metadata. So `FORMAT` 1 does not change. `Repo::verify` checks each
//! entry: its file, that its changeset is in the store, and, for system `git`, that the
//! changeset's `git.oid` is its key.

use std::collections::BTreeSet;
use std::fs::FileType;
use std::io;
use std::sync::Arc;

use camino::{Utf8Path, Utf8PathBuf};
use rayon::ThreadPool;
use rayon::prelude::*;
use swarmops_core::bytes::to_u64;
use swarmops_core::portable::is_windows_device_name;
use swarmops_core::{ChangesetId, ObjectId};

use crate::error::{FileOp, StoreError};
use crate::fs_objects::{fsync_dirs, link_new_file};
use crate::fs_ops::{FsMethod, FsOps, LinkOutcome, StoreFs, open_regular};
use crate::layout::Layout;

/// The system of git commits: its keys are their SHA-1 oids, 20 bytes.
pub const SYSTEM_GIT: &str = "git";

/// The longest system name, in bytes.
const MAX_SYSTEM_LEN: usize = 64;

/// The shortest key, in bytes: its first byte names the fan-out directory, the rest the file.
const MIN_KEY_LEN: usize = 2;

/// The longest key, in bytes.
const MAX_KEY_LEN: usize = 64;

/// An entry's magic.
const MAGIC: [u8; 4] = *b"SXID";

/// The entry layout this build writes and reads.
const VERSION: u8 = 1;

/// The bit of an entry's flags that says it is faithful; version 1 defines no other.
const FAITHFUL: u8 = 1;

/// An entry's length.
const ENTRY_LEN: usize = 44;

/// Where an entry's CRC starts: it covers the bytes before it.
const CRC_AT: usize = 40;

/// What an external id names: a changeset of the store, and whether that changeset holds the
/// external object faithfully.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExternalEntry {
    /// The changeset.
    pub changeset: ChangesetId,
    /// Whether the changeset holds the external object faithfully: for system `git`, whether
    /// it renders back to the commit's own oid (it has no `git.lossy` metadata).
    pub faithful: bool,
}

/// One name [`ExternalIds::list`] found: the key it spells, with the entry its file holds, or
/// [`StoreError::ExternalIdCorrupt`] when what is there is no entry.
pub type ListedEntry = (Vec<u8>, Result<ExternalEntry, StoreError>);

/// What [`ExternalIds::put_batch`] did with the entries it was given.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExternalPut {
    /// How many it wrote.
    pub written: u64,
    /// How many it found there already, with the same bytes, and left as they were.
    pub existing: u64,
    /// The keys of those it found there already with other bytes, in the order given: the file
    /// there is left as it was, and the entry given is not written.
    pub conflicts: Vec<Vec<u8>>,
}

/// The external id index of a store: see [the module documentation](self).
///
/// A system's name is `[a-z0-9][a-z0-9-]{0,63}` (M11's `swarmops-<repo id>` among them) and a
/// key has 2 to 64 bytes; every method refuses anything else as [`StoreError::BadCall`] before
/// it makes a filesystem call.
pub struct ExternalIds {
    layout: Layout,
    /// Every filesystem call goes through it, its failure located in the store, but the reads of
    /// entries (`read_entry`) and the listings of directories (`read_dir`), located through it
    /// all the same.
    fs: StoreFs,
    /// The pool of [`ExternalIds::put_batch`]'s writes and fsyncs once
    /// [`ExternalIds::set_parallelism`] is called; until then, rayon's global pool.
    pool: Option<ThreadPool>,
}

/// What [`ExternalIds::put_batch`] found, or made, at one entry's name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Placed {
    /// It linked the entry there.
    Written,
    /// The same bytes were there.
    Existing,
    /// Other bytes were there.
    Conflict,
}

impl Placed {
    /// What was found there: the same bytes as `ours`, or others.
    fn found(there: Option<&[u8]>, ours: &[u8]) -> Placed {
        if there == Some(ours) {
            Placed::Existing
        } else {
            Placed::Conflict
        }
    }
}

impl ExternalIds {
    /// The index of the store `layout` lays out, every filesystem call going through `fs`.
    /// Touches nothing: the first put that needs a directory makes it.
    pub fn new(layout: Layout, fs: Arc<dyn FsOps>) -> ExternalIds {
        let fs = StoreFs::new(fs, layout.root());
        ExternalIds {
            layout,
            fs,
            pool: None,
        }
    }

    /// Runs [`ExternalIds::put_batch`]'s writes and fsyncs on a pool of its own with `threads`
    /// threads (0: one per available core). With 1, both go one at a time, the entries in the
    /// order given and the directories in path order, so that a test counting `FsOps` calls (a
    /// crash sweep's `CrashAtFs`) meets the same call at the same count in every run. Until this
    /// is called, both use rayon's global pool.
    ///
    /// # Panics
    ///
    /// If the threads cannot be spawned.
    pub fn set_parallelism(&mut self, threads: usize) {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .expect("spawn the external id index's thread pool");
        self.pool = Some(pool);
    }

    /// The entry of external id `key` of `system`; `None` if there is none. Reads at most 45
    /// bytes of its file, a byte past an entry, and makes no [`FsOps`] call (see
    /// [the module documentation](self)). A file there that is not an entry, a longer one among
    /// them, is [`StoreError::ExternalIdCorrupt`].
    pub fn get(&self, system: &str, key: &[u8]) -> Result<Option<ExternalEntry>, StoreError> {
        check_system(system)?;
        check_key(key)?;
        let path = self.layout.external_id_path(system, key);
        match self.read_entry(&path)? {
            Some(bytes) => decode(&bytes).map(Some).ok_or_else(|| corrupt(system, key)),
            None => Ok(None),
        }
    }

    /// Writes the entries of `system` not there yet, as [the module documentation](self) says,
    /// and makes every name it relies on durable before it returns. Every key is checked before
    /// any call. The caller writes an entry only once the changeset it names is committed, its
    /// journal frame durable (W8). On an error, the entries written before it stay, whole, and the
    /// same put again completes the call.
    pub fn put_batch(
        &self,
        system: &str,
        entries: &[(Vec<u8>, ExternalEntry)],
    ) -> Result<ExternalPut, StoreError> {
        check_system(system)?;
        for (key, _) in entries {
            check_key(key)?;
        }
        if entries.is_empty() {
            return Ok(ExternalPut::default());
        }
        let system_dir = self.layout.external_ids_dir().join(system);
        let paths: Vec<Utf8PathBuf> = entries
            .iter()
            .map(|(key, _)| self.layout.external_id_path(system, key))
            .collect();
        let fanouts: BTreeSet<&Utf8Path> = paths.iter().map(|path| parent(path)).collect();
        let mut touched = self.make_dirs(&system_dir, &fanouts)?;
        let tmp_dir = self.layout.objects_tmp();
        let place = |(path, (_, entry)): (&Utf8PathBuf, &(Vec<u8>, ExternalEntry))| {
            self.place(&tmp_dir, path, &encode(entry))
        };
        let work: Vec<_> = paths.iter().zip(entries).collect();
        let threads = self
            .pool
            .as_ref()
            .map_or_else(rayon::current_num_threads, ThreadPool::current_num_threads);
        let placed: Vec<Placed> = if threads <= 1 || work.len() <= 1 {
            work.into_iter().map(place).collect::<Result<_, _>>()?
        } else {
            let place_all = || work.into_par_iter().map(place).collect::<Result<_, _>>();
            match &self.pool {
                Some(pool) => pool.install(place_all)?,
                None => place_all()?,
            }
        };
        let mut put = ExternalPut::default();
        for ((path, (key, _)), placed) in paths.iter().zip(entries).zip(placed) {
            match placed {
                Placed::Written => put.written += 1,
                Placed::Existing => put.existing += 1,
                Placed::Conflict => {
                    put.conflicts.push(key.clone());
                    continue;
                }
            }
            // An entry found there is fsynced too: another writer may have linked it and not
            // fsynced its directory yet, and the caller is about to rely on it (as for objects,
            // storage spec §2 rules 1 and 5).
            touched.insert(parent(path).to_owned());
        }
        let dirs: Vec<Utf8PathBuf> = touched.into_iter().collect();
        if let Some(Err(e)) = fsync_dirs(&self.fs, self.pool.as_ref(), &dirs)
            .into_iter()
            .find(Result::is_err)
        {
            return Err(e.into());
        }
        Ok(put)
    }

    /// The systems the index has a directory for, by name: what [`ExternalIds::list`] can list.
    /// A directory or a file not named as a system is passed over. Lists `meta/external_ids/`
    /// with `std::fs` (an [`FsOps`] has no listing), and makes no [`FsOps`] call.
    pub fn systems(&self) -> Result<Vec<String>, StoreError> {
        let mut systems: Vec<String> = self
            .read_dir(&self.layout.external_ids_dir())?
            .into_iter()
            .filter(|(name, kind)| kind.is_dir() && is_system(name))
            .map(|(name, _)| name)
            .collect();
        systems.sort_unstable();
        Ok(systems)
    }

    /// Every entry of `system`, by key: each key with its entry, or with
    /// [`StoreError::ExternalIdCorrupt`] for a file that does not read as one or anything under an
    /// entry's name that is not a regular file. What is not named as an entry is (a fan-out
    /// directory named by other than two lowercase hex digits, a file in it named by other than
    /// an even count of 2 to 126 of them) is passed over, as the object store's listing passes
    /// over strays. Lists the directories with `std::fs` (an [`FsOps`] has no listing) and reads
    /// each entry as [`ExternalIds::get`] does, no further than a byte past an entry; a listing
    /// or a read that fails fails the call.
    pub fn list(&self, system: &str) -> Result<Vec<ListedEntry>, StoreError> {
        check_system(system)?;
        let system_dir = self.layout.external_ids_dir().join(system);
        let mut entries = Vec::new();
        for (fanout, kind) in self.read_dir(&system_dir)? {
            let Some(first) = from_hex(&fanout).filter(|first| first.len() == 1) else {
                continue;
            };
            if !kind.is_dir() {
                continue;
            }
            let dir = system_dir.join(&fanout);
            for (name, kind) in self.read_dir(&dir)? {
                let Some(rest) = from_hex(&name)
                    .filter(|rest| (MIN_KEY_LEN - 1..MAX_KEY_LEN).contains(&rest.len()))
                else {
                    continue;
                };
                let key = [first.as_slice(), &rest].concat();
                let entry = if kind.is_file() {
                    match self.read_entry(&dir.join(&name))? {
                        Some(bytes) => decode(&bytes).ok_or_else(|| corrupt(system, &key)),
                        // Gone since the listing: nothing removes an entry but a person.
                        None => continue,
                    }
                } else {
                    Err(corrupt(system, &key))
                };
                entries.push((key, entry));
            }
        }
        entries.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));
        Ok(entries)
    }

    /// Makes the directories the fan-out directories `fanouts` of the system directory
    /// `system_dir` need that are missing. Returns the directories whose entries it changed:
    /// the parent of each directory it made, `meta/` when it made `meta/external_ids/` (a store
    /// has `meta/` from its creation).
    fn make_dirs(
        &self,
        system_dir: &Utf8Path,
        fanouts: &BTreeSet<&Utf8Path>,
    ) -> Result<BTreeSet<Utf8PathBuf>, StoreError> {
        let mut changed = BTreeSet::new();
        if !self.fs.exists(system_dir.as_std_path())? {
            let ids_dir = self.layout.external_ids_dir();
            if !self.fs.exists(ids_dir.as_std_path())? {
                changed.insert(self.layout.meta_dir());
            }
            self.fs.create_dir_all(system_dir.as_std_path())?;
            changed.insert(ids_dir);
        }
        for fanout in fanouts {
            if !self.fs.exists(fanout.as_std_path())? {
                self.fs.create_dir_all(fanout.as_std_path())?;
                changed.insert(system_dir.to_owned());
            }
        }
        Ok(changed)
    }

    /// Places the entry `bytes` at `path`, unless a file is there: then what is there decides,
    /// read as [`ExternalIds::get`] reads it (a file longer than an entry is other bytes).
    fn place(
        &self,
        tmp_dir: &Utf8Path,
        path: &Utf8Path,
        bytes: &[u8],
    ) -> Result<Placed, StoreError> {
        if let Some(there) = self.read_entry(path)? {
            return Ok(Placed::found(Some(there.as_slice()), bytes));
        }
        self.fs.create_dir_all(tmp_dir.as_std_path())?;
        match link_new_file(&self.fs, tmp_dir, bytes, path)? {
            LinkOutcome::Placed => Ok(Placed::Written),
            // Another writer linked the name since the read: what it linked decides.
            LinkOutcome::AlreadyExists => {
                let there = self.read_entry(path)?;
                Ok(Placed::found(there.as_deref(), bytes))
            }
        }
    }

    /// The bytes of the entry file at `path`, no more than a byte past an entry
    /// (`ENTRY_LEN + 1`), or `None` if there is none: a file that holds more is no entry, and the
    /// rest of it is never read, however long it is. The file is read on a handle of its own
    /// (`std::fs`, no [`FsOps`] call, as `journal.ckpt` is), and a path that is no regular file
    /// is refused before it is opened (`open_regular`, as [`FsOps::read`] refuses it); a failure
    /// is that of a `read` of the file, named by its place in the store, as [`FsOps::read`]'s is.
    fn read_entry(&self, path: &Utf8Path) -> Result<Option<Vec<u8>>, StoreError> {
        let failed = |e| self.fs.locate(FileOp::Fs(FsMethod::Read), path, e);
        let file = match open_regular(path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(failed(e).into()),
        };
        let mut bytes = Vec::with_capacity(ENTRY_LEN + 1);
        let mut capped = io::Read::take(file, to_u64(ENTRY_LEN + 1));
        io::Read::read_to_end(&mut capped, &mut bytes).map_err(failed)?;
        Ok(Some(bytes))
    }

    /// The names in `dir` and their types, `dir` read with `std::fs`; none if `dir` does not
    /// exist.
    fn read_dir(&self, dir: &Utf8Path) -> Result<Vec<(String, FileType)>, StoreError> {
        let listing_failed = |e| self.fs.locate(FileOp::ReadDir, dir, e);
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(listing_failed(e).into()),
        };
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry.map_err(listing_failed)?;
            // A name that is not UTF-8 is no name the index gives.
            if let Ok(name) = entry.file_name().into_string() {
                names.push((name, entry.file_type().map_err(listing_failed)?));
            }
        }
        Ok(names)
    }
}

/// The directory `path` is in.
fn parent(path: &Utf8Path) -> &Utf8Path {
    path.parent()
        .expect("an entry's path has a fan-out directory")
}

/// Whether `system` is a system's name: `[a-z0-9][a-z0-9-]{0,63}`, and not a Windows device
/// name, which no directory on Windows can have (storage-core design, §2.1 P8).
fn is_system(system: &str) -> bool {
    let name = system.as_bytes();
    let starts = name
        .first()
        .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    let rest = name
        .iter()
        .all(|&b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    starts && rest && name.len() <= MAX_SYSTEM_LEN && !is_windows_device_name(system)
}

/// [`StoreError::BadCall`] unless `system` is a system's name.
fn check_system(system: &str) -> Result<(), StoreError> {
    if is_system(system) {
        Ok(())
    } else {
        Err(StoreError::BadCall(format!(
            "external id system {system:?}: a system's name is [a-z0-9][a-z0-9-]{{0,63}}, and \
             not a Windows device name"
        )))
    }
}

/// [`StoreError::BadCall`] unless `key` has 2 to 64 bytes.
fn check_key(key: &[u8]) -> Result<(), StoreError> {
    if (MIN_KEY_LEN..=MAX_KEY_LEN).contains(&key.len()) {
        Ok(())
    } else {
        Err(StoreError::BadCall(format!(
            "an external id key of {} bytes: a key has {MIN_KEY_LEN} to {MAX_KEY_LEN}",
            key.len()
        )))
    }
}

/// [`StoreError::ExternalIdCorrupt`] for the entry of `key` of `system`.
fn corrupt(system: &str, key: &[u8]) -> StoreError {
    StoreError::ExternalIdCorrupt {
        system: system.to_owned(),
        key: lower_hex(key),
    }
}

/// The 44 bytes of `entry`.
fn encode(entry: &ExternalEntry) -> [u8; ENTRY_LEN] {
    let mut bytes = [0; ENTRY_LEN];
    bytes[..4].copy_from_slice(&MAGIC);
    bytes[4] = VERSION;
    bytes[5] = if entry.faithful { FAITHFUL } else { 0 };
    // Bytes 6 and 7 are reserved: 0.
    bytes[8..CRC_AT].copy_from_slice(&entry.changeset.0.0);
    let crc = crc32fast::hash(&bytes[..CRC_AT]);
    bytes[CRC_AT..].copy_from_slice(&crc.to_le_bytes());
    bytes
}

/// The entry `bytes` hold, if they are one: 44 bytes of magic `SXID`, version 1, flags of no
/// bit but `faithful`'s, reserved 0, and a CRC that matches.
fn decode(bytes: &[u8]) -> Option<ExternalEntry> {
    let bytes: &[u8; ENTRY_LEN] = bytes.try_into().ok()?;
    let (head, crc) = bytes.split_at(CRC_AT);
    let whole = head[..4] == MAGIC
        && head[4] == VERSION
        && head[5] & !FAITHFUL == 0
        && head[6..8] == [0, 0]
        && crc32fast::hash(head).to_le_bytes() == crc;
    let id: [u8; 32] = head[8..].try_into().ok()?;
    whole.then_some(ExternalEntry {
        changeset: ChangesetId(ObjectId(id)),
        faithful: head[5] & FAITHFUL != 0,
    })
}

/// The lowercase hex digits of `bytes`, two to a byte: how an entry's key names its file, and how
/// [`StoreError::ExternalIdCorrupt`] and verify name a key.
pub fn lower_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut hex = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        hex.push(char::from(DIGITS[usize::from(byte >> 4)]));
        hex.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    hex
}

/// The bytes `hex` spells, if it is an even count of lowercase hex digits: how the external id
/// index and the derived store read the names their files are given.
pub(crate) fn from_hex(hex: &str) -> Option<Vec<u8>> {
    let digit = |b: u8| match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    };
    let (pairs, odd) = hex.as_bytes().as_chunks::<2>();
    if !odd.is_empty() {
        return None;
    }
    pairs
        .iter()
        .map(|&[high, low]| Some((digit(high)? << 4) | digit(low)?))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `bytes` with their CRC computed again, so that only the field changed is wrong.
    fn with_crc(mut bytes: [u8; ENTRY_LEN]) -> [u8; ENTRY_LEN] {
        let crc = crc32fast::hash(&bytes[..CRC_AT]);
        bytes[CRC_AT..].copy_from_slice(&crc.to_le_bytes());
        bytes
    }

    #[test]
    fn an_entry_is_44_bytes_and_reads_back() {
        let entry = ExternalEntry {
            changeset: ChangesetId(ObjectId([7; 32])),
            faithful: true,
        };
        let bytes = encode(&entry);
        assert_eq!(&bytes[..8], b"SXID\x01\x01\x00\x00");
        assert_eq!(bytes[8..40], [7; 32]);
        assert_eq!(bytes[40..], crc32fast::hash(&bytes[..40]).to_le_bytes());
        assert_eq!(decode(&bytes), Some(entry));
        let lossy = ExternalEntry {
            faithful: false,
            ..entry
        };
        assert_eq!(encode(&lossy)[5], 0);
        assert_eq!(decode(&encode(&lossy)), Some(lossy));

        // Each damage reads as no entry; past the CRC's, each with its CRC made to match.
        let changed = |at: usize, byte: u8| {
            let mut changed = bytes;
            changed[at] = byte;
            changed
        };
        let damaged = [
            ("a flipped CRC byte", changed(41, bytes[41] ^ 0x01).to_vec()),
            ("a wrong magic", with_crc(changed(3, b'X')).to_vec()),
            ("version 2", with_crc(changed(4, 2)).to_vec()),
            ("version 0", with_crc(changed(4, 0)).to_vec()),
            (
                "a flag version 1 does not define",
                with_crc(changed(5, 0b10)).to_vec(),
            ),
            ("a reserved byte not 0", with_crc(changed(6, 1)).to_vec()),
            (
                "the other reserved byte not 0",
                with_crc(changed(7, 1)).to_vec(),
            ),
            ("cut short", bytes[..43].to_vec()),
            ("a byte too many", [&bytes[..], &[0]].concat()),
            ("empty", Vec::new()),
        ];
        for (what, bytes) in damaged {
            assert_eq!(decode(&bytes), None, "{what}");
        }
    }

    #[test]
    fn keys_are_named_by_their_lowercase_hex() {
        assert_eq!(lower_hex(&[0x00, 0xab, 0x7f, 0xff]), "00ab7fff");
        assert_eq!(from_hex("00ab7fff"), Some(vec![0x00, 0xab, 0x7f, 0xff]));
        for not_lowercase_hex in ["0", "ABCD", "abc", "xy", "0g", "+1"] {
            assert_eq!(from_hex(not_lowercase_hex), None, "{not_lowercase_hex}");
        }
        assert_eq!(from_hex(""), Some(Vec::new()));
    }
}
