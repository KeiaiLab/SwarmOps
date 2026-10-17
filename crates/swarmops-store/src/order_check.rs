//! [`OrderCheckFs`] (feature `test-hooks`): an online check of the store's write-ordering
//! rules (storage spec §2–§3), in the manner of SQLite's journal-test VFS, which makes sure
//! that nothing reaches the database file that was not first written and synced to the
//! journal. It wraps the [`FsOps`] the stores write through, follows what each call does to
//! the files and directories, and panics, naming the rule and the call, at the first call that
//! breaks one. A crash sweep that runs under it checks every call it makes, not only the call
//! it crashes at: a sync left out, or made in the wrong place, shows even when nothing
//! crashes there.
//!
//! # The rules
//!
//! - **R1, the journal.** A frame is listed in `journal.off` only once it is durable in
//!   `journal.log`: synced in this process after it was written, or listed already (each listed
//!   frame was durable before it was listed). And neither journal file is appended to while a
//!   failed sync has left some of its bytes not durable: Linux reports a failed writeback once,
//!   to the sync that meets it, and may drop the pages it could not write, so a later sync that
//!   succeeds does not make those bytes durable, and a frame or an entry written after them
//!   would outlive them in a power loss.
//! - **R2, the refs.** A ref file is replaced or removed only while every byte of `journal.log`
//!   is durable: the frame that moves the ref is durable before its projection changes.
//! - **R3, the checkpoint.** `journal.ckpt` moves to an offset past 0 only when the five index
//!   files (`parents.log`, `par.col`, `gen.col`, `dh.col`, `ids.col`) were synced in this process
//!   since they last changed, no failed sync coming between; the offset is within what
//!   `journal.log` holds durably; and no change of a ref file (a replace, a removal, a new
//!   directory) still waits for its directory's fsync. (A store without index files, as some
//!   journal tests lay out, has no index clause.)
//! - **R4, the sorted views.** A view file is renamed into place only from a temp file synced
//!   since it was written (R6), only while the five index files are synced since they last
//!   changed (a view must never cover records a power loss could take back), and only so that
//!   the four view files' entry counts never grow along `ids.key`, `ids.rec`, `dh.key`, `dh.rec`,
//!   the order a compaction renames them in, so that a crash between two renames leaves views a
//!   reader takes. The directory fsync after each rename is [`FsOps::rename_durable`]'s own.
//! - **R5, the objects.** An object file is linked into its fan-out directory only from data
//!   synced first, and a journal frame that records a changeset is appended only once every
//!   object name linked, and every fan-out directory made, in this process is durable (its
//!   directory fsynced since): the objects a changeset names are durable before its record.
//! - **R6, durable renames.** [`FsOps::rename_durable`] moves only durable data: a file synced in
//!   this process since it was last written, or a directory whose tree has no change waiting for
//!   a directory fsync and no data waiting for a sync.
//! - **R7, `FORMAT`.** A store's `FORMAT` file, the mark that the store is complete, is linked
//!   only from synced data, and only once the rest of the store is durable: no entry under the
//!   store root (or the root's own) waits for its directory's fsync, `objects/tmp/` and the
//!   staging `meta.tmp/` aside, and no file there holds data not synced. R7 judges durability,
//!   not completeness: it does not check that `meta/`, `REPO` and the fan-out directories exist.
//!   The pinned call sequence of `LocalRepo::create` and the create sweeps' oracle hold that
//!   they are made before `FORMAT`.
//!
//! # What it knows
//!
//! Each process has its own: what a file held when this process first changed it counts as
//! durable (another process made it so, or a crash left it for recovery to judge), except that
//! of `journal.log` only the frames `journal.off` lists do. A call that fails is judged by what
//! it left behind, read back from the files themselves: a failed sync leaves the bytes that it
//! did not cover not durable, for good (R1's lost writeback; the index files too, so that R3
//! catches a checkpoint past records a failed sync may have lost); a failed replace or rename
//! that took effect leaves its new name waiting for a directory fsync. It reads the files with
//! `std::fs`, never through the [`FsOps`] it wraps, so that it changes no call count, and it
//! holds no lock while a call runs, so that calls made at once stay at once.
//!
//! It judges the order of the calls, and takes each at what [`FsOps`] documents for it, not at
//! what `RealFs` does inside it: that `atomic_replace` syncs its temp file before the rename and
//! the directory after it, that `append_durable`, `truncate` and `write_durable_temp` sync, that
//! `rename_durable` syncs the directory, and that `sync_data` and `fsync_dir` flush are read in
//! `fs_ops.rs`, not tested.

use std::collections::HashMap;
use std::fs::{self, File};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::{fmt, io};

use crate::fs_ops::{FsOps, LinkOutcome};
use crate::meta::frame::{Parsed, Record, parse_frame};

/// The size of one `journal.off` entry.
const ENTRY_LEN: u64 = 8;

/// The size of a frame's header: `u32le len | u32le crc`.
const HEADER_LEN: u64 = 8;

/// The four view files of `meta/sorted/`, in the order a compaction renames them, with the size
/// of one entry.
const VIEWS: [(&str, u64); 4] = [
    ("ids.key", 32),
    ("ids.rec", 8),
    ("dh.key", 32),
    ("dh.rec", 8),
];

/// The store's [`FsOps`] with its write-ordering rules checked on every call: see [the module
/// documentation](self). A call that breaks a rule panics before it runs (or, for a rename
/// that leaves the view files out of order, right after), with a message that starts
/// `order check: R<n> violated by <method> <paths>`.
pub struct OrderCheckFs<F: FsOps> {
    inner: F,
    state: Mutex<State>,
}

impl<F: FsOps> OrderCheckFs<F> {
    /// Wraps `inner`, knowing nothing yet of the files' durability.
    pub fn new(inner: F) -> Self {
        Self {
            inner,
            state: Mutex::new(State::default()),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        // A panic while the lock is held is a violation already reported; what it left is
        // still good for the threads that go on until the test ends.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl<F: FsOps> fmt::Debug for OrderCheckFs<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OrderCheckFs").finish_non_exhaustive()
    }
}

/// What the checker knows of the files and directories, in this process.
#[derive(Default)]
struct State {
    files: HashMap<PathBuf, FileState>,
    /// The changes of each directory's entries that no fsync of it has covered yet.
    dirs: HashMap<PathBuf, Vec<Pending>>,
    /// Counts the changes recorded, so that an fsync covers those made before it began.
    generation: u64,
}

/// What the checker knows of one file.
#[derive(Debug, Clone, Copy)]
struct FileState {
    /// Bytes `[0, durable_to)` are durable.
    durable_to: u64,
    /// The file's length after the last sync of it in this process that succeeded, if nothing
    /// was written to it since.
    synced_at: Option<u64>,
    /// Written in this process since the last sync.
    dirty: bool,
    /// From here on, the bytes are not durable whatever later syncs report: a failed sync left
    /// them so. A cut at or below it ends that.
    lost_from: Option<u64>,
}

impl FileState {
    /// A file whose `len` bytes are all durable, and synced in this process.
    fn durable(len: u64) -> FileState {
        FileState {
            durable_to: len,
            synced_at: Some(len),
            dirty: false,
            lost_from: None,
        }
    }

    /// Whether every byte of the file, `len` of them now, is durable and was synced in this
    /// process after it was last written.
    fn synced_whole(&self, len: u64) -> bool {
        self.lost_from.is_none() && !self.dirty && self.synced_at == Some(len)
    }
}

/// A change of a directory's entries that no fsync of the directory has covered yet.
struct Pending {
    /// The entry.
    path: PathBuf,
    /// Made (created, linked or renamed in), or else removed.
    added: bool,
    generation: u64,
}

/// One of the journal's three files.
#[derive(Clone, Copy, PartialEq, Eq)]
enum JournalFile {
    Log,
    Off,
    Ckpt,
}

impl State {
    /// The state of `path`, which the checker follows from now on. A file seen for the first
    /// time counts as durable as it is, but for `journal.log`, of which only the frames
    /// `journal.off` lists do.
    fn track(&mut self, path: &Path) -> &mut FileState {
        self.files.entry(path.to_path_buf()).or_insert_with(|| {
            let len = real_len(path);
            let durable_to = match journal_file(path) {
                Some((JournalFile::Log, meta)) => listed_end(meta).min(len),
                _ => len,
            };
            FileState {
                durable_to,
                synced_at: None,
                dirty: false,
                lost_from: None,
            }
        })
    }

    /// A sync of `path` that succeeded: what it holds is durable now, but for what a failed
    /// sync left not durable.
    fn synced(&mut self, path: &Path) {
        let len = real_len(path);
        let file = self.track(path);
        file.durable_to = file.lost_from.map_or(len, |lost| lost.min(len));
        file.dirty = false;
        file.synced_at = Some(len);
    }

    /// A sync of `path` that failed: what it did not cover stays not durable for good, as
    /// Linux marks the pages whose writeback failed clean.
    fn sync_failed(&mut self, path: &Path) {
        let len = real_len(path);
        let file = self.track(path);
        if len > file.durable_to {
            file.lost_from = Some(
                file.lost_from
                    .map_or(file.durable_to, |lost| lost.min(file.durable_to)),
            );
        }
        file.dirty = false;
        file.synced_at = None;
    }

    /// A plain write of `path`, which was `before` bytes long.
    fn written(&mut self, path: &Path, before: u64) {
        let len = real_len(path);
        let file = self.track(path);
        if len != before {
            file.dirty = true;
            file.synced_at = None;
        }
    }

    /// A cut of `path`, synced if it succeeded.
    fn cut(&mut self, path: &Path, ok: bool) {
        let len = real_len(path);
        let file = self.track(path);
        if ok {
            file.lost_from = file.lost_from.filter(|&lost| lost < len);
            file.durable_to = file.lost_from.unwrap_or(len);
            file.dirty = false;
            file.synced_at = Some(len);
        } else {
            // The cut may have been made: the bytes before it are durable still, but whatever
            // lies past what was durable is not, nor may the cut be.
            if len > file.durable_to {
                file.lost_from = Some(
                    file.lost_from
                        .map_or(file.durable_to, |lost| lost.min(file.durable_to)),
                );
            }
            file.durable_to = file.durable_to.min(len);
            file.dirty = false;
            file.synced_at = None;
        }
    }

    /// A change of `path`'s entry in its directory, waiting for the directory's fsync.
    fn pend(&mut self, path: &Path, added: bool) {
        let Some(dir) = path.parent() else {
            return;
        };
        self.generation += 1;
        let generation = self.generation;
        self.dirs
            .entry(dir.to_path_buf())
            .or_default()
            .push(Pending {
                path: path.to_path_buf(),
                added,
                generation,
            });
    }

    /// An fsync of `dir` that began once change `upto` was recorded: it covers that change and
    /// those before it.
    fn dir_synced(&mut self, dir: &Path, upto: u64) {
        if let Some(pending) = self.dirs.get_mut(dir) {
            pending.retain(|change| change.generation > upto);
        }
    }

    /// `from` (a file, or a directory and everything under it) is `to` now.
    fn moved(&mut self, from: &Path, to: &Path) {
        let rekey = |path: &Path| -> Option<PathBuf> {
            let rest = path.strip_prefix(from).ok()?;
            Some(if rest.as_os_str().is_empty() {
                to.to_path_buf()
            } else {
                to.join(rest)
            })
        };
        let files: Vec<PathBuf> = self
            .files
            .keys()
            .filter(|path| path.starts_with(from))
            .cloned()
            .collect();
        if !files.is_empty() {
            // A file renamed over `to` replaces it.
            self.files.remove(to);
        }
        for path in files {
            if let (Some(state), Some(new)) = (self.files.remove(&path), rekey(&path)) {
                self.files.insert(new, state);
            }
        }
        let dirs: Vec<PathBuf> = self
            .dirs
            .keys()
            .filter(|path| path.starts_with(from))
            .cloned()
            .collect();
        for path in dirs {
            if let (Some(mut pending), Some(new)) = (self.dirs.remove(&path), rekey(&path)) {
                for change in &mut pending {
                    if let Some(moved) = rekey(&change.path) {
                        change.path = moved;
                    }
                }
                self.dirs.insert(new, pending);
            }
        }
    }

    /// The bytes of `journal.log` in `meta` known to be durable: synced in this process, or
    /// listed in `journal.off`.
    fn durable_log_end(&self, meta: &Path) -> u64 {
        let synced = self
            .files
            .get(&meta.join("journal.log"))
            .map_or(0, |file| file.durable_to);
        synced.max(listed_end(meta))
    }

    /// Where a failed sync left `journal.log` in `meta` not durable, if it did.
    fn log_lost_from(&self, meta: &Path) -> Option<u64> {
        self.files
            .get(&meta.join("journal.log"))
            .and_then(|file| file.lost_from)
    }

    /// R1 for an append of `entries` to `off`, the `journal.off` of `meta`.
    fn listing(&self, off: &Path, meta: &Path, entries: &[u8]) -> Option<String> {
        if let Some(lost) = self.files.get(off).and_then(|file| file.lost_from) {
            return Some(format!(
                "journal.off holds bytes from {lost} on that a failed sync left not durable, and \
                 the new entries would follow them: a power loss would leave a hole among the \
                 listed frames"
            ));
        }
        let log = meta.join("journal.log");
        let durable = self.durable_log_end(meta);
        let lost = self.log_lost_from(meta);
        for entry in entries.chunks(8) {
            let Ok(entry) = <[u8; 8]>::try_from(entry) else {
                return Some(format!(
                    "it appends {} bytes, not whole entries",
                    entries.len()
                ));
            };
            let start = u64::from_le_bytes(entry);
            let Some(end) = frame_end(&log, start) else {
                return Some(format!(
                    "it lists a frame at {start}, where journal.log holds no frame header"
                ));
            };
            if let Some(lost) = lost
                && end > lost
            {
                return Some(format!(
                    "it lists the frame at {start}..{end}, whose bytes from {lost} on a failed \
                     sync left not durable: Linux reports a failed writeback once, so a later \
                     sync that succeeds does not make them durable"
                ));
            }
            if end > durable {
                return Some(format!(
                    "it lists the frame at {start}..{end} before the frame is durable: \
                     journal.log is durable to {durable} (synced in this process, or listed)"
                ));
            }
        }
        None
    }

    /// R1 and R5 for an append of `frame` to `log`, the `journal.log` of `meta`: the rule
    /// broken, and how.
    fn frame(&self, log: &Path, meta: &Path, frame: &[u8]) -> Option<(&'static str, String)> {
        if let Some(lost) = self.files.get(log).and_then(|file| file.lost_from) {
            return Some((
                "R1",
                format!(
                    "journal.log holds bytes from {lost} on that a failed sync left not durable, \
                     and the frame would follow them"
                ),
            ));
        }
        let Parsed::Whole { records, .. } = parse_frame(frame) else {
            return None;
        };
        if !records
            .iter()
            .any(|record| matches!(record, Record::Changeset { .. }))
        {
            return None;
        }
        let objects = meta.parent()?.join("objects");
        let tmp = objects.join("tmp");
        for (dir, pending) in &self.dirs {
            let objects_dir = *dir == objects || dir.parent() == Some(objects.as_path());
            if !objects_dir || *dir == tmp {
                continue;
            }
            if let Some(change) = pending
                .iter()
                .find(|change| change.added && change.path != tmp)
            {
                return Some((
                    "R5",
                    format!(
                        "the frame records a changeset while {} is not durable: no fsync of {} \
                         since it was made",
                        change.path.display(),
                        dir.display()
                    ),
                ));
            }
        }
        None
    }

    /// R2 for a change of a ref file of `meta`.
    fn ref_change(&self, meta: &Path) -> Option<String> {
        let len = real_len(&meta.join("journal.log"));
        if let Some(lost) = self.log_lost_from(meta)
            && len > lost
        {
            return Some(format!(
                "journal.log holds bytes from {lost} on that a failed sync left not durable"
            ));
        }
        let durable = self.durable_log_end(meta);
        (len > durable).then(|| {
            format!(
                "journal.log is durable to {durable} of its {len} bytes: the frame that moves \
                 the ref may not be durable yet"
            )
        })
    }

    /// R3 for `journal.ckpt` of `meta` set to `bytes`.
    fn checkpoint(&self, meta: &Path, bytes: &[u8]) -> Option<String> {
        let Ok(offset) = <[u8; 8]>::try_from(bytes) else {
            return Some(format!("journal.ckpt would hold {} bytes", bytes.len()));
        };
        let offset = u64::from_le_bytes(offset);
        if offset == 0 {
            return None;
        }
        if let Some(why) = self.index_unsynced(meta) {
            return Some(format!("it moves the checkpoint to {offset} while {why}"));
        }
        let durable = self.durable_log_end(meta);
        if offset > durable {
            return Some(format!(
                "it moves the checkpoint to {offset}, past the {durable} bytes of journal.log \
                 that are durable"
            ));
        }
        let refs = meta.join("refs");
        for (dir, pending) in &self.dirs {
            let change = if dir.starts_with(&refs) {
                pending.first()
            } else if dir == meta {
                pending.iter().find(|change| change.path == refs)
            } else {
                None
            };
            if let Some(change) = change {
                return Some(format!(
                    "it moves the checkpoint to {offset} while the change of {} waits for an \
                     fsync of {}: a power loss could undo a ref move the checkpoint has passed",
                    change.path.display(),
                    dir.display()
                ));
            }
        }
        None
    }

    /// Why the five index files of `meta` are not all synced since they last changed in this
    /// process, if they are not; `None` too for a `meta` without index files.
    fn index_unsynced(&self, meta: &Path) -> Option<String> {
        let files = index_files(meta);
        if !files.iter().any(|path| path.exists()) {
            return None;
        }
        files.iter().find_map(|path| {
            let len = real_len(path);
            match self.files.get(path) {
                Some(file) if file.lost_from.is_some() => Some(format!(
                    "{} holds bytes that a failed sync left not durable",
                    path.display()
                )),
                Some(file) if file.synced_whole(len) => None,
                _ => Some(format!(
                    "{} is not synced in this process since it last changed",
                    path.display()
                )),
            }
        })
    }

    /// R6 for a rename of `from`, a directory or else a file.
    fn rename(&self, from: &Path, from_is_dir: bool) -> Option<String> {
        if !from_is_dir {
            return match self.files.get(from) {
                Some(file) if file.lost_from.is_some() => {
                    Some("a failed sync left some of its data not durable".to_owned())
                }
                Some(file) if file.synced_whole(real_len(from)) => None,
                _ => Some("its data is not synced since it was last written".to_owned()),
            };
        }
        for (dir, pending) in &self.dirs {
            if dir.starts_with(from)
                && let Some(change) = pending.first()
            {
                return Some(format!(
                    "the change of {} waits for an fsync of {}",
                    change.path.display(),
                    dir.display()
                ));
            }
        }
        self.files.iter().find_map(|(path, file)| {
            (path.starts_with(from) && (file.dirty || file.lost_from.is_some()))
                .then(|| format!("{} holds data not synced", path.display()))
        })
    }

    /// R5 and R7: the data of `from`, about to be linked into place, must be durable.
    fn linked_data(&self, from: &Path) -> Option<String> {
        match self.files.get(from) {
            Some(file) if file.synced_whole(real_len(from)) => None,
            _ => Some(format!(
                "{} is linked into place before its data is synced",
                from.display()
            )),
        }
    }

    /// R7 for the link of `root`'s `FORMAT`.
    fn store_complete(&self, root: &Path) -> Option<String> {
        let staging = [root.join("objects/tmp"), root.join("meta.tmp")];
        let staged = |path: &Path| staging.iter().any(|dir| path.starts_with(dir));
        for (dir, pending) in &self.dirs {
            let change = pending.iter().find(|change| {
                change.added
                    && !staged(&change.path)
                    && (change.path.starts_with(root) || root.starts_with(&change.path))
            });
            if let Some(change) = change {
                return Some(format!(
                    "{} is not durable: no fsync of {} since it was made",
                    change.path.display(),
                    dir.display()
                ));
            }
        }
        self.files.iter().find_map(|(path, file)| {
            (path.starts_with(root) && !staged(path) && (file.dirty || file.lost_from.is_some()))
                .then(|| format!("{} holds data not synced", path.display()))
        })
    }
}

/// Panics if `violation` holds one: rule `rule` is broken by `method` on `paths`.
fn verdict(rule: &str, method: &str, paths: &[&Path], violation: Option<String>) {
    if let Some(why) = violation {
        let paths: Vec<String> = paths
            .iter()
            .map(|path| path.display().to_string())
            .collect();
        panic!(
            "order check: {rule} violated by {method} {}: {why}",
            paths.join(" ")
        );
    }
}

/// The length of the file at `path` now; 0 if there is none.
fn real_len(path: &Path) -> u64 {
    fs::metadata(path).map_or(0, |meta| meta.len())
}

/// `len` bytes of the file at `path` from `offset`, if it holds them.
fn read_at(path: &Path, offset: u64, len: usize) -> Option<Vec<u8>> {
    let file = File::open(path).ok()?;
    let mut buf = vec![0; len];
    file.read_exact_at(&mut buf, offset).ok()?;
    Some(buf)
}

/// Where the frame whose header starts at `offset` of `log` ends, by its header; `None` if the
/// header is not there.
fn frame_end(log: &Path, offset: u64) -> Option<u64> {
    let len = read_at(log, offset, 4)?;
    let len = u32::from_le_bytes(len.try_into().ok()?);
    Some(offset + HEADER_LEN + u64::from(len))
}

/// Where the last frame `journal.off` of `meta` lists ends, as the files are now; 0 if it lists
/// none, or its last entry or that frame's header cannot be read.
fn listed_end(meta: &Path) -> u64 {
    let off = meta.join("journal.off");
    let listed = real_len(&off) / ENTRY_LEN;
    let Some(last) = listed.checked_sub(1) else {
        return 0;
    };
    read_at(&off, last * ENTRY_LEN, 8)
        .and_then(|entry| <[u8; 8]>::try_from(entry).ok())
        .and_then(|entry| frame_end(&meta.join("journal.log"), u64::from_le_bytes(entry)))
        .unwrap_or(0)
}

/// The name of `path`'s last component, if it is UTF-8.
fn name(path: &Path) -> Option<&str> {
    path.file_name()?.to_str()
}

/// Which journal file `path` is, and the `meta/` directory it lies in.
fn journal_file(path: &Path) -> Option<(JournalFile, &Path)> {
    let meta = path.parent()?;
    if name(meta)? != "meta" {
        return None;
    }
    let file = match name(path)? {
        "journal.log" => JournalFile::Log,
        "journal.off" => JournalFile::Off,
        "journal.ckpt" => JournalFile::Ckpt,
        _ => return None,
    };
    Some((file, meta))
}

/// The five index files of the metadata store in `meta`, in the order `Index::append` writes
/// them.
fn index_files(meta: &Path) -> [PathBuf; 5] {
    [
        meta.join("parents.log"),
        meta.join("idx/par.col"),
        meta.join("idx/gen.col"),
        meta.join("idx/dh.col"),
        meta.join("idx/ids.col"),
    ]
}

/// The `meta/` directory of `path`, if it is one of the view files of `meta/sorted/`.
fn view_meta(path: &Path) -> Option<&Path> {
    let sorted = path.parent()?;
    let meta = sorted.parent()?;
    let is_view = name(sorted)? == "sorted"
        && name(meta)? == "meta"
        && VIEWS.iter().any(|(view, _)| Some(*view) == name(path));
    is_view.then_some(meta)
}

/// The `meta/` directory of `path`, if it lies under `meta/refs/`.
fn ref_meta(path: &Path) -> Option<&Path> {
    path.ancestors()
        .skip(1)
        .find(|dir| name(dir) == Some("refs") && dir.parent().and_then(name) == Some("meta"))
        .and_then(Path::parent)
}

/// Whether `path` is an object file, `objects/<2 hex>/<name>`.
fn is_object(path: &Path) -> bool {
    let Some(fanout) = path.parent() else {
        return false;
    };
    let is_fanout = name(fanout).is_some_and(|fanout| {
        fanout.len() == 2
            && fanout
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    });
    is_fanout && fanout.parent().and_then(name) == Some("objects")
}

/// `path` and those of its ancestors that do not exist, outermost first: what a
/// `create_dir_all` of it makes.
fn missing_dirs(path: &Path) -> Vec<PathBuf> {
    let mut missing: Vec<PathBuf> = path
        .ancestors()
        .take_while(|dir| !dir.as_os_str().is_empty() && !dir.exists())
        .map(Path::to_path_buf)
        .collect();
    missing.reverse();
    missing
}

impl<F: FsOps> FsOps for OrderCheckFs<F> {
    fn create_dir_all(&self, p: &Path) -> io::Result<()> {
        let missing = missing_dirs(p);
        let result = self.inner.create_dir_all(p);
        let mut state = self.state();
        for dir in missing.iter().filter(|dir| dir.exists()) {
            state.pend(dir, true);
        }
        result
    }

    fn write_durable_temp(&self, dir: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
        let result = self.inner.write_durable_temp(dir, bytes);
        if let Ok(path) = &result {
            let mut state = self.state();
            state
                .files
                .insert(path.clone(), FileState::durable(real_len(path)));
            state.pend(path, true);
        }
        result
    }

    fn link_noclobber(&self, from: &Path, to: &Path) -> io::Result<LinkOutcome> {
        let (rule, violation) = {
            let state = self.state();
            if name(to) == Some("FORMAT") {
                let root = to.parent().unwrap_or(Path::new("."));
                let why = state
                    .linked_data(from)
                    .or_else(|| state.store_complete(root));
                ("R7", why)
            } else if is_object(to) {
                ("R5", state.linked_data(from))
            } else {
                ("", None)
            }
        };
        verdict(rule, "link_noclobber", &[from, to], violation);
        let existed = to.exists();
        let result = self.inner.link_noclobber(from, to);
        let mut state = self.state();
        let placed = match &result {
            Ok(LinkOutcome::Placed) => true,
            Ok(LinkOutcome::AlreadyExists) => false,
            Err(_) => !existed && to.exists(),
        };
        if placed {
            state.moved(from, to);
        } else if !from.exists() {
            state.files.remove(from);
        }
        if to.exists() {
            // Placed, or found there: either way its name waits for the fsync that makes it
            // durable, as the object store's flush does for both.
            state.pend(to, true);
        }
        if !from.exists() {
            state.pend(from, false);
        }
        result
    }

    fn fsync_dir(&self, p: &Path) -> io::Result<()> {
        let upto = self.state().generation;
        let result = self.inner.fsync_dir(p);
        if result.is_ok() {
            self.state().dir_synced(p, upto);
        }
        result
    }

    fn read(&self, p: &Path) -> io::Result<Option<Vec<u8>>> {
        self.inner.read(p)
    }

    fn exists(&self, p: &Path) -> io::Result<bool> {
        self.inner.exists(p)
    }

    fn remove(&self, p: &Path) -> io::Result<()> {
        let violation = ref_meta(p).and_then(|meta| self.state().ref_change(meta));
        verdict("R2", "remove", &[p], violation);
        let result = self.inner.remove(p);
        if result.is_ok() {
            let mut state = self.state();
            state.files.remove(p);
            state.pend(p, false);
        }
        result
    }

    fn append_durable(&self, path: &Path, bytes: &[u8]) -> io::Result<u64> {
        let violation = {
            let mut state = self.state();
            state.track(path);
            match journal_file(path) {
                Some((JournalFile::Off, meta)) => {
                    state.listing(path, meta, bytes).map(|why| ("R1", why))
                }
                Some((JournalFile::Log, meta)) => state.frame(path, meta, bytes),
                _ => None,
            }
        };
        if let Some((rule, why)) = violation {
            verdict(rule, "append_durable", &[path], Some(why));
        }
        let existed = path.exists();
        let result = self.inner.append_durable(path, bytes);
        let mut state = self.state();
        if result.is_ok() {
            state.synced(path);
        } else {
            state.sync_failed(path);
        }
        if !existed && path.exists() {
            state.pend(path, true);
        }
        result
    }

    fn file_len(&self, path: &Path) -> io::Result<u64> {
        self.inner.file_len(path)
    }

    fn truncate(&self, path: &Path, len: u64) -> io::Result<()> {
        self.state().track(path);
        let result = self.inner.truncate(path, len);
        self.state().cut(path, result.is_ok());
        result
    }

    fn atomic_replace(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        let violation = {
            let state = self.state();
            if let Some((JournalFile::Ckpt, meta)) = journal_file(path) {
                state.checkpoint(meta, bytes).map(|why| ("R3", why))
            } else if let Some(meta) = ref_meta(path) {
                state.ref_change(meta).map(|why| ("R2", why))
            } else {
                None
            }
        };
        if let Some((rule, why)) = violation {
            verdict(rule, "atomic_replace", &[path], Some(why));
        }
        let upto = self.state().generation;
        let result = self.inner.atomic_replace(path, bytes);
        let mut state = self.state();
        match &result {
            Ok(()) => {
                state
                    .files
                    .insert(path.to_path_buf(), FileState::durable(real_len(path)));
                if let Some(dir) = path.parent() {
                    state.dir_synced(dir, upto);
                }
            }
            // It took effect, its directory's fsync failing: the new name is not durable yet.
            Err(_) if fs::read(path).is_ok_and(|now| now == bytes) => {
                state
                    .files
                    .insert(path.to_path_buf(), FileState::durable(real_len(path)));
                state.pend(path, true);
            }
            Err(_) => {}
        }
        result
    }

    fn sync_data(&self, path: &Path) -> io::Result<()> {
        self.state().track(path);
        let result = self.inner.sync_data(path);
        if result.is_ok() {
            self.state().synced(path);
        } else {
            self.state().sync_failed(path);
        }
        result
    }

    fn append(&self, path: &Path, bytes: &[u8]) -> io::Result<u64> {
        let before = {
            let mut state = self.state();
            state.track(path);
            real_len(path)
        };
        let existed = path.exists();
        let result = self.inner.append(path, bytes);
        let mut state = self.state();
        state.written(path, before);
        if !existed && path.exists() {
            state.pend(path, true);
        }
        result
    }

    fn rename_durable(&self, from: &Path, to: &Path) -> io::Result<()> {
        let from_is_dir = from.is_dir();
        let view = view_meta(to);
        let violation = {
            let state = self.state();
            match state.rename(from, from_is_dir) {
                Some(why) => Some(("R6", why)),
                None => view.and_then(|meta| state.index_unsynced(meta)).map(|why| {
                    (
                        "R4",
                        format!(
                            "a sorted view is renamed into place while {why}: it could \
                                 cover records a power loss takes back"
                        ),
                    )
                }),
            }
        };
        if let Some((rule, why)) = violation {
            verdict(rule, "rename_durable", &[from, to], Some(why));
        }
        let upto = self.state().generation;
        let result = self.inner.rename_durable(from, to);
        {
            let mut state = self.state();
            let moved = result.is_ok() || (to.exists() && !from.exists());
            if moved {
                state.moved(from, to);
                if from.parent() != to.parent() {
                    state.pend(from, false);
                }
            }
            match (&result, to.parent()) {
                (Ok(()), Some(dir)) => state.dir_synced(dir, upto),
                (Err(_), _) if moved => state.pend(to, true),
                _ => {}
            }
        }
        if result.is_ok()
            && let Some(meta) = view
        {
            verdict("R4", "rename_durable", &[from, to], view_counts(meta));
        }
        result
    }
}

/// R4 after a view file of `meta` was renamed into place: the four files' entry counts must
/// not grow along the order a compaction renames them in, nor end in a partial entry.
fn view_counts(meta: &Path) -> Option<String> {
    let sorted = meta.join("sorted");
    let mut counts = Vec::with_capacity(VIEWS.len());
    for (view, width) in VIEWS {
        let len = real_len(&sorted.join(view));
        if !len.is_multiple_of(width) {
            return Some(format!("sorted/{view} ends in a partial entry"));
        }
        counts.push(len / width);
    }
    counts.windows(2).any(|pair| pair[0] < pair[1]).then(|| {
        format!(
            "the view files' entry counts {counts:?} grow along ids.key, ids.rec, dh.key, \
             dh.rec, the order a compaction renames them in: a crash now would leave views no \
             reader opens"
        )
    })
}

#[cfg(test)]
mod tests {
    //! Each rule, broken on purpose by a call sequence the store never makes, panics with its
    //! id; kept, the same steps pass. The stores' own sweeps run under the checker elsewhere.

    use std::panic::{self, AssertUnwindSafe};

    use swarmops_core::{ChangesetId, ObjectId};

    use super::*;
    use crate::fs_ops::{FailAt, FailAtFs, FailMode, InjectedError, RealFs};
    use crate::meta::frame::encode_frame;

    /// A store root in a temp dir, with `meta/`, empty journal files, `meta/refs/` and an
    /// `objects/` fan-out directory, all durable.
    fn root() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("store");
        for sub in [
            "meta/refs",
            "meta/idx",
            "meta/sorted",
            "objects/ab",
            "objects/tmp",
        ] {
            fs::create_dir_all(root.join(sub)).unwrap();
        }
        for file in ["meta/journal.log", "meta/journal.off"] {
            File::create(root.join(file)).unwrap();
        }
        (dir, root)
    }

    fn frame(records: &[Record]) -> Vec<u8> {
        encode_frame(records).unwrap()
    }

    fn compacted() -> Vec<u8> {
        frame(&[Record::Compacted { sorted_count: 0 }])
    }

    fn changeset() -> Vec<u8> {
        frame(&[Record::Changeset {
            id: ChangesetId(ObjectId([1; 32])),
            generation: 1,
            delta_hash: [2; 32],
            parents: Vec::new(),
        }])
    }

    /// The rule `run` breaks, from its panic; `None` if it does not panic.
    fn broken(run: impl FnOnce()) -> Option<String> {
        let payload = panic::catch_unwind(AssertUnwindSafe(run)).err()?;
        let message = payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
            .unwrap_or_default();
        let rule = message.strip_prefix("order check: ")?.split(' ').next()?;
        Some(rule.to_owned())
    }

    #[track_caller]
    fn assert_breaks(rule: &str, run: impl FnOnce()) {
        assert_eq!(broken(run).as_deref(), Some(rule));
    }

    /// `RealFs` whose `k`-th call fails with `EIO` once: a flushing call does its work first.
    fn eio_at(k: u64) -> FailAtFs<RealFs> {
        let fail = FailAt {
            at: k,
            mode: FailMode::Once,
            error: InjectedError::Eio,
        };
        FailAtFs::new(RealFs, fail)
    }

    #[test]
    fn r1_a_frame_is_listed_only_once_it_is_durable() {
        let (_dir, root) = root();
        let (log, off) = (root.join("meta/journal.log"), root.join("meta/journal.off"));
        let fs = OrderCheckFs::new(RealFs);
        // Written, never synced: listing it breaks R1.
        let at = fs.append(&log, &compacted()).unwrap();
        assert_breaks("R1", || {
            fs.append_durable(&off, &at.to_le_bytes()).unwrap();
        });
        // Synced first, it may be listed.
        let fs = OrderCheckFs::new(RealFs);
        fs.sync_data(&log).unwrap();
        fs.append_durable(&off, &at.to_le_bytes()).unwrap();
        let at = fs.append_durable(&log, &compacted()).unwrap();
        fs.append_durable(&off, &at.to_le_bytes()).unwrap();
    }

    /// The fsyncgate pattern: a frame whose sync failed, then listed after a later sync that
    /// succeeds, breaks R1 (Linux reports a failed writeback once); and so does any append to a
    /// journal file after bytes a failed sync left behind, until a cut takes them off.
    #[test]
    fn r1_a_failed_sync_is_not_made_good_by_a_later_one() {
        let (_dir, root) = root();
        let (log, off) = (root.join("meta/journal.log"), root.join("meta/journal.off"));
        let fs = OrderCheckFs::new(eio_at(1));
        let start: u64 = 0;
        fs.append_durable(&log, &compacted()).unwrap_err();
        fs.sync_data(&log).unwrap();
        assert_breaks("R1", || {
            fs.append_durable(&off, &start.to_le_bytes()).unwrap();
        });
        assert_breaks("R1", || {
            fs.append_durable(&log, &compacted()).unwrap();
        });
        // Cut back to where the frame started, the journal takes appends again.
        fs.truncate(&log, start).unwrap();
        let at = fs.append_durable(&log, &compacted()).unwrap();
        fs.append_durable(&off, &at.to_le_bytes()).unwrap();

        // An entry whose sync failed: the next entry would follow it.
        let fs = OrderCheckFs::new(eio_at(2));
        let at = fs.append_durable(&log, &compacted()).unwrap();
        fs.append_durable(&off, &at.to_le_bytes()).unwrap_err();
        let next = fs.append_durable(&log, &compacted()).unwrap();
        assert_breaks("R1", || {
            fs.append_durable(&off, &next.to_le_bytes()).unwrap();
        });
    }

    #[test]
    fn r2_a_ref_moves_only_once_its_frame_is_durable() {
        let (_dir, root) = root();
        let (log, main) = (root.join("meta/journal.log"), root.join("meta/refs/main"));
        let fs = OrderCheckFs::new(RealFs);
        fs.append(&log, &compacted()).unwrap();
        assert_breaks("R2", || fs.atomic_replace(&main, b"x").unwrap());
        fs.sync_data(&log).unwrap();
        fs.atomic_replace(&main, b"x").unwrap();
        fs.append(&log, &compacted()).unwrap();
        assert_breaks("R2", || fs.remove(&main).unwrap());
    }

    #[test]
    fn r3_the_checkpoint_waits_for_the_index_the_journal_and_the_refs() {
        let (_dir, root) = root();
        let meta = root.join("meta");
        let (log, ckpt) = (meta.join("journal.log"), meta.join("journal.ckpt"));
        let fs = OrderCheckFs::new(RealFs);
        let end = fs.append_durable(&log, &compacted()).unwrap() + to_u64(compacted().len());
        for path in index_files(&meta) {
            fs.append(&path, &[0; 8]).unwrap();
        }
        let ckpt_at = |offset: u64| {
            fs.atomic_replace(&ckpt, &offset.to_le_bytes()).unwrap();
        };
        assert_breaks("R3", || ckpt_at(end));
        ckpt_at(0); // covers nothing
        for path in index_files(&meta) {
            fs.sync_data(&path).unwrap();
        }
        assert_breaks("R3", || ckpt_at(end + 1));
        ckpt_at(end);
        // A ref file's removal not yet made durable.
        let old = meta.join("refs/old");
        fs.atomic_replace(&old, b"x").unwrap();
        fs.remove(&old).unwrap();
        assert_breaks("R3", || ckpt_at(end));
        fs.fsync_dir(&meta.join("refs")).unwrap();
        ckpt_at(end);
        // Synced, then written again: not synced since it last changed.
        fs.append(&meta.join("idx/ids.col"), &[0; 32]).unwrap();
        assert_breaks("R3", || ckpt_at(end));
        fs.sync_data(&meta.join("idx/ids.col")).unwrap();
        ckpt_at(end);
        // A failed index sync leaves records not durable, whatever later syncs say.
        let fs = OrderCheckFs::new(eio_at(2));
        let ids = meta.join("idx/ids.col");
        fs.append(&ids, &[0; 32]).unwrap();
        fs.sync_data(&ids).unwrap_err();
        for path in index_files(&meta) {
            fs.sync_data(&path).unwrap();
        }
        let checkpoint = || {
            fs.atomic_replace(&ckpt, &end.to_le_bytes()).unwrap();
        };
        assert_breaks("R3", checkpoint);
    }

    /// The view files of `meta`, `entries` each, written through `fs` to temp files and synced.
    fn temp_views(fs: &dyn FsOps, meta: &Path, entries: usize) {
        for (view, width) in VIEWS {
            let temp = meta.join(format!("sorted/{view}.tmp"));
            fs.append(&temp, &vec![0; entries * to_usize(width)])
                .unwrap();
            fs.sync_data(&temp).unwrap();
        }
    }

    /// A store root with one index record in every index file, synced unless `synced` is
    /// false, the view temp files of one entry each written and synced, and what renames a
    /// view's temp file into place.
    fn compacting(
        synced: bool,
    ) -> (
        tempfile::TempDir,
        PathBuf,
        OrderCheckFs<RealFs>,
        impl Fn(&OrderCheckFs<RealFs>, &str),
    ) {
        let (dir, root) = root();
        let meta = root.join("meta");
        let fs = OrderCheckFs::new(RealFs);
        for path in index_files(&meta) {
            fs.append(&path, &[0; 32]).unwrap();
            if synced {
                fs.sync_data(&path).unwrap();
            }
        }
        temp_views(&fs, &meta, 1);
        let sorted = meta.join("sorted");
        let rename = move |fs: &OrderCheckFs<RealFs>, view: &str| {
            let (temp, to) = (sorted.join(format!("{view}.tmp")), sorted.join(view));
            fs.rename_durable(&temp, &to).unwrap();
        };
        (dir, root, fs, rename)
    }

    #[test]
    fn r4_views_are_renamed_synced_after_the_index_and_in_order() {
        let (_dir, _root, fs, rename) = compacting(false);
        assert_breaks("R4", || rename(&fs, "ids.key")); // the index is not synced
        let (_dir, _root, fs, rename) = compacting(true);
        assert_breaks("R4", || rename(&fs, "dh.key")); // the counts grow along the order
        let (_dir, root, fs, rename) = compacting(true);
        for view in ["ids.key", "ids.rec", "dh.key", "dh.rec"] {
            rename(&fs, view);
        }
        // A temp file written since its sync breaks R6.
        temp_views(&fs, &root.join("meta"), 2);
        fs.append(&root.join("meta/sorted/ids.key.tmp"), &[0; 32])
            .unwrap();
        assert_breaks("R6", || rename(&fs, "ids.key"));
        // The index synced, then written again: not synced since it last changed (R4), even
        // with the temp synced (R6 kept).
        fs.sync_data(&root.join("meta/sorted/ids.key.tmp")).unwrap();
        fs.append(&root.join("meta/idx/ids.col"), &[0; 32]).unwrap();
        assert_breaks("R4", || rename(&fs, "ids.key"));
        fs.sync_data(&root.join("meta/idx/ids.col")).unwrap();
        rename(&fs, "ids.key");
    }

    #[test]
    fn r5_objects_are_durable_before_the_frame_that_names_them() {
        let (_dir, root) = root();
        let (log, staging) = (root.join("meta/journal.log"), root.join("objects/tmp"));
        let object = root.join("objects/ab/cdef");
        let fs = OrderCheckFs::new(RealFs);
        let unsynced = staging.join("unsynced");
        fs.append(&unsynced, b"object").unwrap();
        assert_breaks("R5", || {
            fs.link_noclobber(&unsynced, &object).unwrap();
        });
        let temp = fs.write_durable_temp(&staging, b"object").unwrap();
        fs.link_noclobber(&temp, &object).unwrap();
        fs.append_durable(&log, &compacted()).unwrap(); // records no changeset
        assert_breaks("R5", || {
            fs.append_durable(&log, &changeset()).unwrap();
        });
        fs.fsync_dir(&root.join("objects/ab")).unwrap();
        fs.append_durable(&log, &changeset()).unwrap();
    }

    #[test]
    fn r6_a_directory_is_renamed_only_once_its_tree_is_durable() {
        let (_dir, root) = root();
        let staged = root.join("meta.tmp/meta");
        let fs = OrderCheckFs::new(RealFs);
        fs.create_dir_all(&staged.join("idx")).unwrap();
        fs.append(&staged.join("idx/ids.col"), &[0; 8]).unwrap();
        fs.sync_data(&staged.join("idx/ids.col")).unwrap();
        let to = root.join("staged");
        assert_breaks("R6", || fs.rename_durable(&staged, &to).unwrap());
        fs.fsync_dir(&staged).unwrap(); // `idx` is durable, the entry `ids.col` in it is not
        assert_breaks("R6", || fs.rename_durable(&staged, &to).unwrap());
        fs.fsync_dir(&staged.join("idx")).unwrap();
        // Every entry durable, the file written since its sync.
        fs.append(&staged.join("idx/ids.col"), &[0; 8]).unwrap();
        assert_breaks("R6", || fs.rename_durable(&staged, &to).unwrap());
        fs.sync_data(&staged.join("idx/ids.col")).unwrap();
        fs.rename_durable(&staged, &to).unwrap();
    }

    #[test]
    fn r7_format_is_linked_last() {
        let (_dir, root) = root();
        let fs = OrderCheckFs::new(RealFs);
        fs.create_dir_all(&root.join("objects/cd")).unwrap();
        let format = root.join("FORMAT");
        let link = |fs: &OrderCheckFs<RealFs>| {
            let temp = fs
                .write_durable_temp(&root.join("objects/tmp"), b"1\n")
                .unwrap();
            fs.link_noclobber(&temp, &format).unwrap();
        };
        assert_breaks("R7", || link(&fs));
        fs.fsync_dir(&root.join("objects")).unwrap();
        link(&fs);
    }

    fn to_u64(n: usize) -> u64 {
        u64::try_from(n).unwrap()
    }

    fn to_usize(n: u64) -> usize {
        usize::try_from(n).unwrap()
    }
}
