//! [`OrderCheckFs`] (feature `test-hooks`): an online check of the store's write-ordering
//! rules (storage spec §2 rule 4), in the manner of SQLite's journal-test VFS, which makes sure
//! that nothing reaches the database file that was not first written and synced to the
//! journal. It wraps the [`FsOps`] the stores write through, follows what each call does to
//! the files and directories, and panics, naming the rule and the call, at the first call that
//! breaks one. A crash sweep that runs under it checks every call it makes, not only the call
//! it crashes at: a sync left out, or made in the wrong place, shows even when nothing
//! crashes there.
//!
//! # The rules
//!
//! W1 to W8, which storage spec §2 rule 4 states: the write protocol of the spec's §2, §3 and §7,
//! restated as this checker judges it, call by call. They add no protocol of their own.
//!
//! - **W1, the journal.** A frame is listed in `journal.off` only once it is durable in
//!   `journal.log`: synced in this process after it was written, or listed already (each listed
//!   frame was durable before it was listed). And neither journal file is appended to while a
//!   failed sync has left some of its bytes not durable: Linux reports a failed writeback once,
//!   to the sync that meets it, and may drop the pages it could not write, so a later sync that
//!   succeeds does not make those bytes durable, and a frame or an entry written after them
//!   would outlive them in a power loss. Nor is either file cut below what the journal vouches
//!   for: `journal.log` into the frames `journal.off` lists, `journal.off` below an entry this
//!   process synced or found there when it first saw the file (the zeroed entries past frame 0
//!   that a power loss leaves at its end list no frame, and the tail check drops them, as it
//!   does a partial entry).
//! - **W2, the refs.** A ref file is replaced or removed only while every byte of `journal.log`
//!   is durable: the frame that moves the ref is durable before its projection changes.
//! - **W3, the checkpoint.** `journal.ckpt` moves to an offset past 0 only when the five index
//!   files (`parents.log`, `par.col`, `gen.col`, `dh.col`, `ids.col`) were synced in this process
//!   since they last changed, no failed sync coming between; the offset is within what
//!   `journal.log` holds durably; and no change of a ref file (a replace, a removal, a new
//!   directory) still waits for its directory's fsync. (A store without index files, as some
//!   journal tests lay out, has no index clause.)
//! - **W4, the sorted views.** A view file is renamed into place only from a temp file synced
//!   since it was written (W6), only while the five index files are synced since they last
//!   changed (a view must never cover records a power loss could take back), and only so that
//!   the four view files' entry counts never grow along `ids.key`, `ids.rec`, `dh.key`, `dh.rec`,
//!   the order a compaction renames them in, so that a crash between two renames leaves views a
//!   reader takes. The directory fsync after each rename is [`FsOps::rename_durable`]'s own.
//! - **W5, the objects.** An object file is linked into its fan-out directory only from data
//!   synced first, and a journal frame that records a changeset is appended only once every
//!   object name linked, and every fan-out directory made, in this process is durable (its
//!   directory fsynced since): the objects a changeset names are durable before its record.
//! - **W6, durable renames.** [`FsOps::rename_durable`] moves only durable data: a file synced in
//!   this process since it was last written, or a directory whose tree has no change waiting for
//!   a directory fsync and no data waiting for a sync.
//! - **W7, `FORMAT`.** A store's `FORMAT` file, the mark that the store is complete, is linked
//!   only from synced data, and only once the rest of the store is durable: no entry under the
//!   store root (or the root's own) waits for its directory's fsync, `objects/tmp/` and the
//!   staging `meta.tmp/` aside, and no file there holds data not synced. W7 judges durability,
//!   not completeness: it does not check that `meta/`, `REPO` and the fan-out directories exist.
//!   The pinned call sequence of `LocalRepo::create` and the create sweeps' oracle hold that
//!   they are made before `FORMAT`.
//! - **W8, the external ids.** An entry of the external id index (`meta/external_ids/`) is linked
//!   only from data synced first, and only while every byte this process wrote to `journal.log`
//!   is durable: the frame that records the entry's changeset is durable before the entry. W2
//!   asks more of a ref change, every byte of `journal.log`; a put of entries takes no lock, so
//!   another process's frame may be on its way meanwhile.
//!
//! # What it knows
//!
//! Each process has its own: what a file held when this process first changed it counts as
//! durable (another process made it so, or a crash left it for recovery to judge), except that
//! of `journal.log` only the frames `journal.off` lists do. A call that fails is judged by what
//! it left behind, read back from the files themselves: a failed sync leaves the bytes that it
//! did not cover not durable, for good (W1's lost writeback; the index files too, so that W3
//! catches a checkpoint past records a failed sync may have lost); a failed replace or rename
//! that took effect leaves its new name waiting for a directory fsync. A directory's later fsync
//! that succeeds covers every change made in it before, those a failed one left waiting among
//! them, as storage spec §2 rule 5 assumes (a failed directory fsync is fail-stop on the
//! filesystems the store supports); file data that a failed sync left not durable is never made
//! good by a later sync. It reads the files with `std::fs`, never through the [`FsOps`] it wraps,
//! so that it changes no call count, and it holds no lock while a call runs, so that calls made
//! at once stay at once.
//!
//! It knows the files of both formats under the same rules: FORMAT 1's `journal.log`,
//! `journal.off` (8-byte entries), `journal.ckpt` (8 bytes) and `refs/`, and FORMAT 2's
//! `journal2.log`, `journal2.off` (16-byte entries), `journal2.ckpt` (20 bytes, its offset first)
//! and `refs2/` (the M6 design §2.2). The poison an upgrade writes over FORMAT 1's checkpoint
//! (`UPGRADE_POISON`) moves no checkpoint, and W3 lets it pass.
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

use crate::format::{StoreFormat, UPGRADE_POISON};
use crate::fs_ops::{FsMethod, FsOps, LinkOutcome};
use crate::meta::frame::{self, Parsed, Place, Record};

/// The size of one `journal.off` entry (FORMAT 1).
const ENTRY_LEN: u64 = 8;

/// The size of one `journal2.off` entry (FORMAT 2).
const ENTRY_LEN_V2: u64 = 16;

/// The size of a frame's header: `u32le len | u32le crc` (FORMAT 1).
const HEADER_LEN: u64 = 8;

/// The size of a FORMAT 2 frame's header: `len`, `crc`, `seq`, `prev_crc`, `hcrc`.
const HEADER_LEN_V2: u64 = 24;

/// The size of `journal2.ckpt`: its offset, its seq and their CRC.
const CKPT_LEN_V2: usize = 20;

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
/// `order check: W<n> violated by <method> <paths>`.
#[non_exhaustive]
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

/// How a cut of a file ended: [`Cut::Done`] syncs it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Cut {
    Done,
    Failed,
}

/// What happened to a path's entry in its directory.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Entry {
    Added,
    Removed,
}

/// What a path names on disk.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Node {
    File,
    Dir,
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
                Some((JournalFile::Log, meta, format)) => listed_end(meta, format).min(len),
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
    fn cut(&mut self, path: &Path, cut: Cut) {
        let len = real_len(path);
        let file = self.track(path);
        if cut == Cut::Done {
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
    fn pend(&mut self, path: &Path, entry: Entry) {
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
                added: entry == Entry::Added,
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

    /// The bytes of `journal.log` of `format` in `meta` known to be durable: synced in this
    /// process, or listed in `journal.off`.
    fn durable_log_end(&self, meta: &Path, format: StoreFormat) -> u64 {
        let synced = self
            .files
            .get(&journal_path(meta, format, JournalFile::Log))
            .map_or(0, |file| file.durable_to);
        synced.max(listed_end(meta, format))
    }

    /// Where a failed sync left `journal.log` of `format` in `meta` not durable, if it did.
    fn log_lost_from(&self, meta: &Path, format: StoreFormat) -> Option<u64> {
        self.files
            .get(&journal_path(meta, format, JournalFile::Log))
            .and_then(|file| file.lost_from)
    }

    /// W1 for an append of `entries` to `off`, the `journal.off` of `format` in `meta`.
    fn listing(
        &self,
        off: &Path,
        meta: &Path,
        format: StoreFormat,
        entries: &[u8],
    ) -> Option<String> {
        if let Some(lost) = self.files.get(off).and_then(|file| file.lost_from) {
            return Some(format!(
                "journal.off holds bytes from {lost} on that a failed sync left not durable, and \
                 the new entries would follow them: a power loss would leave a hole among the \
                 listed frames"
            ));
        }
        let log = journal_path(meta, format, JournalFile::Log);
        let durable = self.durable_log_end(meta, format);
        let lost = self.log_lost_from(meta, format);
        let entry_len = usize::try_from(entry_len(format)).unwrap_or(usize::MAX);
        for entry in entries.chunks(entry_len) {
            let Some(start) = (entry.len() == entry_len)
                .then(|| entry.first_chunk::<8>())
                .flatten()
            else {
                return Some(format!(
                    "it appends {} bytes, not whole entries",
                    entries.len()
                ));
            };
            let start = u64::from_le_bytes(*start);
            let Some(end) = frame_end(&log, start, format) else {
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

    /// W1 for a cut to `len` of `path`, the journal file `file` of `meta`: `journal.log` is
    /// never cut into the frames `journal.off` lists, nor `journal.off` below an entry this
    /// process synced or found there when it first saw the file, but for the zeroed entries past
    /// frame 0 at its end, which list no frame.
    fn journal_cut(
        &self,
        (file, meta, format): (JournalFile, &Path, StoreFormat),
        path: &Path,
        len: u64,
    ) -> Option<String> {
        match file {
            JournalFile::Log => {
                let listed = listed_end(meta, format);
                (len < listed).then(|| {
                    format!(
                        "it cuts journal.log to {len}, into the frames journal.off lists (to \
                         {listed}): a committed frame would be lost"
                    )
                })
            }
            JournalFile::Off => {
                let synced = self.files.get(path).map_or(0, |file| file.durable_to);
                let listing = listing_entries_end(path, synced, format);
                (len < listing).then(|| {
                    format!(
                        "it cuts journal.off to {len}, below the entries synced or found there \
                         (to {listing}): a committed frame would no longer be listed"
                    )
                })
            }
            JournalFile::Ckpt => None,
        }
    }

    /// W1 and W5 for an append of `frame` to `log`, the `journal.log` of `meta`: the rule
    /// broken, and how.
    fn frame(
        &self,
        log: &Path,
        meta: &Path,
        format: StoreFormat,
        frame: &[u8],
    ) -> Option<(Rule, String)> {
        if let Some(lost) = self.files.get(log).and_then(|file| file.lost_from) {
            return Some((
                Rule::W1,
                format!(
                    "journal.log holds bytes from {lost} on that a failed sync left not durable, \
                     and the frame would follow them"
                ),
            ));
        }
        let Parsed::Whole { records, .. } = parse_any(format, frame) else {
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
                    Rule::W5,
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

    /// W8 for the link of an external id entry of `meta`: every byte this process wrote to its
    /// `journal.log` is durable (written by `append`, it is not until a sync covers it; a failed
    /// sync leaves it not durable for good), so the frame that records the entry's changeset is.
    /// The journal of either format, an upgrade's among them.
    fn external_id(&self, meta: &Path) -> Option<String> {
        [StoreFormat::V1, StoreFormat::V2]
            .iter()
            .find_map(|&format| {
                let log = journal_path(meta, format, JournalFile::Log);
                let file = self.files.get(&log)?;
                if let Some(lost) = file.lost_from
                    && real_len(&log) > lost
                {
                    return Some(format!(
                        "journal.log holds bytes from {lost} on that a failed sync left not durable"
                    ));
                }
                file.dirty.then(|| {
                    "journal.log holds bytes this process wrote and has not synced: the frame that \
                 records the entry's changeset may not be durable yet"
                        .to_owned()
                })
            })
    }

    /// W2 for a change of a ref file of `meta`, of `format`.
    fn ref_change(&self, (meta, format): (&Path, StoreFormat)) -> Option<String> {
        let len = real_len(&journal_path(meta, format, JournalFile::Log));
        if let Some(lost) = self.log_lost_from(meta, format)
            && len > lost
        {
            return Some(format!(
                "journal.log holds bytes from {lost} on that a failed sync left not durable"
            ));
        }
        let durable = self.durable_log_end(meta, format);
        (len > durable).then(|| {
            format!(
                "journal.log is durable to {durable} of its {len} bytes: the frame that moves \
                 the ref may not be durable yet"
            )
        })
    }

    /// W3 for `journal.ckpt` of `format` in `meta` set to `bytes`.
    fn checkpoint(&self, meta: &Path, format: StoreFormat, bytes: &[u8]) -> Option<String> {
        if format == StoreFormat::V1 && bytes == UPGRADE_POISON {
            return None;
        }
        let len = match format {
            StoreFormat::V1 => 8,
            StoreFormat::V2 => CKPT_LEN_V2,
        };
        let Some(offset) = (bytes.len() == len)
            .then(|| bytes.first_chunk::<8>())
            .flatten()
        else {
            return Some(format!("journal.ckpt would hold {} bytes", bytes.len()));
        };
        let offset = u64::from_le_bytes(*offset);
        if offset == 0 {
            return None;
        }
        if let Some(why) = self.index_unsynced(meta) {
            return Some(format!("it moves the checkpoint to {offset} while {why}"));
        }
        let durable = self.durable_log_end(meta, format);
        if offset > durable {
            return Some(format!(
                "it moves the checkpoint to {offset}, past the {durable} bytes of journal.log \
                 that are durable"
            ));
        }
        let refs = meta.join(refs_name(format));
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

    /// W6 for a rename of `from`, a directory or else a file.
    fn rename(&self, from: &Path, node: Node) -> Option<String> {
        if node == Node::File {
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

    /// W5, W7 and W8: the data of `from`, about to be linked into place, must be durable.
    fn linked_data(&self, from: &Path) -> Option<String> {
        match self.files.get(from) {
            Some(file) if file.synced_whole(real_len(from)) => None,
            _ => Some(format!(
                "{} is linked into place before its data is synced",
                from.display()
            )),
        }
    }

    /// W7 for the link of `root`'s `FORMAT`.
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

/// Declares [`Rule`] from one list of the rules, each with its documentation: the ids the
/// panics name and, for the tests, `Rule::ALL` come from the same list, so that a rule cannot be
/// left out of either.
macro_rules! rules {
    ($($(#[$attr:meta])* $rule:ident,)+) => {
        /// A write-ordering rule, as [the module documentation](self) states it; a violation's
        /// panic names it by its id.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        enum Rule {
            $($(#[$attr])* $rule,)+
        }

        #[cfg(test)]
        impl Rule {
            /// Every rule, in order.
            const ALL: [Rule; [$(stringify!($rule)),+].len()] = [$(Rule::$rule),+];
        }

        impl fmt::Display for Rule {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(match self {
                    $(Rule::$rule => stringify!($rule),)+
                })
            }
        }
    };
}

rules! {
    /// The journal.
    W1,
    /// The refs.
    W2,
    /// The checkpoint.
    W3,
    /// The sorted views.
    W4,
    /// The objects.
    W5,
    /// Durable renames.
    W6,
    /// `FORMAT`.
    W7,
    /// The external ids.
    W8,
}

/// Panics if `violation` holds one: rule `rule` is broken by `method` on `paths`.
fn verdict(rule: Rule, method: FsMethod, paths: &[&Path], violation: Option<String>) {
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

/// Where the frame of `format` whose header starts at `offset` of `log` ends, by its header;
/// `None` if the header is not there.
fn frame_end(log: &Path, offset: u64, format: StoreFormat) -> Option<u64> {
    let len = read_at(log, offset, 4)?;
    let len = u32::from_le_bytes(len.try_into().ok()?);
    let header_len = match format {
        StoreFormat::V1 => HEADER_LEN,
        StoreFormat::V2 => HEADER_LEN_V2,
    };
    Some(offset + header_len + u64::from(len))
}

/// The records of `frame`, a frame of `format` (its place, in FORMAT 2, what its header names).
fn parse_any(format: StoreFormat, frame: &[u8]) -> Parsed {
    let place = match frame::decode_header_in(format, frame) {
        Ok(Some(header)) => Place {
            seq: header.seq.unwrap_or(0),
            prev_crc: header.prev_crc.unwrap_or(0),
        },
        _ => return Parsed::Short,
    };
    frame::parse_frame_in(format, frame, place)
}

/// The size of one entry of `journal.off` in `format`.
fn entry_len(format: StoreFormat) -> u64 {
    match format {
        StoreFormat::V1 => ENTRY_LEN,
        StoreFormat::V2 => ENTRY_LEN_V2,
    }
}

/// Where the last frame `journal.off` of `format` in `meta` lists ends, as the files are now
/// (the zeroed entries past frame 0 at its end list none: [`listing_entries_end`]); 0 if it
/// lists none, or its last entry or that frame's header cannot be read.
fn listed_end(meta: &Path, format: StoreFormat) -> u64 {
    let off = journal_path(meta, format, JournalFile::Off);
    let entry_len = entry_len(format);
    let listed = listing_entries_end(&off, real_len(&off), format) / entry_len;
    let Some(last) = listed.checked_sub(1) else {
        return 0;
    };
    let log = journal_path(meta, format, JournalFile::Log);
    read_at(&off, last * entry_len, 8)
        .and_then(|entry| <[u8; 8]>::try_from(entry).ok())
        .and_then(|entry| frame_end(&log, u64::from_le_bytes(entry), format))
        .unwrap_or(0)
}

/// Where the last entry of `off`, a `journal.off` of `format`, among its first `upto` bytes that
/// lists a frame ends: a partial entry lists none, nor do the zeroed entries past frame 0 at the
/// end (what a zero-filled block leaves: no frame but the first starts at 0), which the tail
/// check drops; entry 0 lists frame 0 whatever it holds. 0 if there is no entry.
fn listing_entries_end(off: &Path, upto: u64, format: StoreFormat) -> u64 {
    let entry_len = entry_len(format);
    let len = usize::try_from(entry_len).unwrap_or(usize::MAX);
    let whole = upto.min(real_len(off)) / entry_len;
    let last = (1..whole)
        .rev()
        .find(|&n| {
            read_at(off, n * entry_len, len).is_none_or(|entry| entry.iter().any(|&b| b != 0))
        })
        .unwrap_or(0);
    whole.min(last + 1) * entry_len
}

/// The name of `path`'s last component, if it is UTF-8.
fn name(path: &Path) -> Option<&str> {
    path.file_name()?.to_str()
}

/// Which journal file `path` is, the `meta/` directory it lies in, and the format it is of.
fn journal_file(path: &Path) -> Option<(JournalFile, &Path, StoreFormat)> {
    let meta = path.parent()?;
    if name(meta)? != "meta" {
        return None;
    }
    let (stem, ext) = name(path)?.split_once('.')?;
    let format = match stem {
        "journal" => StoreFormat::V1,
        "journal2" => StoreFormat::V2,
        _ => return None,
    };
    let file = match ext {
        "log" => JournalFile::Log,
        "off" => JournalFile::Off,
        "ckpt" => JournalFile::Ckpt,
        _ => return None,
    };
    Some((file, meta, format))
}

/// The journal file `file` of `format` in `meta`.
fn journal_path(meta: &Path, format: StoreFormat, file: JournalFile) -> PathBuf {
    let stem = match format {
        StoreFormat::V1 => "journal",
        StoreFormat::V2 => "journal2",
    };
    let ext = match file {
        JournalFile::Log => "log",
        JournalFile::Off => "off",
        JournalFile::Ckpt => "ckpt",
    };
    meta.join(format!("{stem}.{ext}"))
}

/// The name of the ref files' directory in `meta/` in `format`.
fn refs_name(format: StoreFormat) -> &'static str {
    match format {
        StoreFormat::V1 => "refs",
        StoreFormat::V2 => "refs2",
    }
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

/// The `meta/` directory of `path`, if it lies under `meta/refs/` (FORMAT 1) or `meta/refs2/`
/// (FORMAT 2), and that format.
fn ref_meta(path: &Path) -> Option<(&Path, StoreFormat)> {
    path.ancestors().skip(1).find_map(|dir| {
        let format = match name(dir)? {
            "refs" => StoreFormat::V1,
            "refs2" => StoreFormat::V2,
            _ => return None,
        };
        let meta = dir.parent()?;
        (name(meta)? == "meta").then_some((meta, format))
    })
}

/// The `meta/` directory of `path`, if it is an entry of the external id index,
/// `meta/external_ids/<system>/<fan-out>/<name>`.
fn external_id_meta(path: &Path) -> Option<&Path> {
    let ids = path.parent()?.parent()?.parent()?;
    let meta = ids.parent()?;
    (name(ids)? == "external_ids" && name(meta)? == "meta").then_some(meta)
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
            state.pend(dir, Entry::Added);
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
            state.pend(path, Entry::Added);
        }
        result
    }

    fn link_noclobber(&self, from: &Path, to: &Path) -> io::Result<LinkOutcome> {
        let violation = {
            let state = self.state();
            if name(to) == Some("FORMAT") {
                let root = to.parent().unwrap_or(Path::new("."));
                state
                    .linked_data(from)
                    .or_else(|| state.store_complete(root))
                    .map(|why| (Rule::W7, why))
            } else if let Some(meta) = external_id_meta(to) {
                state
                    .linked_data(from)
                    .or_else(|| state.external_id(meta))
                    .map(|why| (Rule::W8, why))
            } else if is_object(to) {
                state.linked_data(from).map(|why| (Rule::W5, why))
            } else {
                None
            }
        };
        if let Some((rule, why)) = violation {
            verdict(rule, FsMethod::LinkNoclobber, &[from, to], Some(why));
        }
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
            state.pend(to, Entry::Added);
        }
        if !from.exists() {
            state.pend(from, Entry::Removed);
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
        verdict(Rule::W2, FsMethod::Remove, &[p], violation);
        let result = self.inner.remove(p);
        if result.is_ok() {
            let mut state = self.state();
            state.files.remove(p);
            state.pend(p, Entry::Removed);
        }
        result
    }

    fn append_durable(&self, path: &Path, bytes: &[u8]) -> io::Result<u64> {
        let violation = {
            let mut state = self.state();
            state.track(path);
            match journal_file(path) {
                Some((JournalFile::Off, meta, format)) => state
                    .listing(path, meta, format, bytes)
                    .map(|why| (Rule::W1, why)),
                Some((JournalFile::Log, meta, format)) => state.frame(path, meta, format, bytes),
                _ => None,
            }
        };
        if let Some((rule, why)) = violation {
            verdict(rule, FsMethod::AppendDurable, &[path], Some(why));
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
            state.pend(path, Entry::Added);
        }
        result
    }

    fn file_len(&self, path: &Path) -> io::Result<u64> {
        self.inner.file_len(path)
    }

    fn truncate(&self, path: &Path, len: u64) -> io::Result<()> {
        let violation = {
            let mut state = self.state();
            state.track(path);
            journal_file(path).and_then(|file| state.journal_cut(file, path, len))
        };
        verdict(Rule::W1, FsMethod::Truncate, &[path], violation);
        let result = self.inner.truncate(path, len);
        let cut = if result.is_ok() {
            Cut::Done
        } else {
            Cut::Failed
        };
        self.state().cut(path, cut);
        result
    }

    fn atomic_replace(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        let violation = {
            let state = self.state();
            if let Some((JournalFile::Ckpt, meta, format)) = journal_file(path) {
                state
                    .checkpoint(meta, format, bytes)
                    .map(|why| (Rule::W3, why))
            } else if let Some(meta) = ref_meta(path) {
                state.ref_change(meta).map(|why| (Rule::W2, why))
            } else {
                None
            }
        };
        if let Some((rule, why)) = violation {
            verdict(rule, FsMethod::AtomicReplace, &[path], Some(why));
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
                state.pend(path, Entry::Added);
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
            state.pend(path, Entry::Added);
        }
        result
    }

    fn rename_durable(&self, from: &Path, to: &Path) -> io::Result<()> {
        let node = if from.is_dir() { Node::Dir } else { Node::File };
        let view = view_meta(to);
        let violation = {
            let state = self.state();
            match state.rename(from, node) {
                Some(why) => Some((Rule::W6, why)),
                None => view.and_then(|meta| state.index_unsynced(meta)).map(|why| {
                    (
                        Rule::W4,
                        format!(
                            "a sorted view is renamed into place while {why}: it could \
                                 cover records a power loss takes back"
                        ),
                    )
                }),
            }
        };
        if let Some((rule, why)) = violation {
            verdict(rule, FsMethod::RenameDurable, &[from, to], Some(why));
        }
        let upto = self.state().generation;
        let result = self.inner.rename_durable(from, to);
        {
            let mut state = self.state();
            let moved = result.is_ok() || (to.exists() && !from.exists());
            if moved {
                state.moved(from, to);
                if from.parent() != to.parent() {
                    state.pend(from, Entry::Removed);
                }
            }
            match (&result, to.parent()) {
                (Ok(()), Some(dir)) => state.dir_synced(dir, upto),
                (Err(_), _) if moved => state.pend(to, Entry::Added),
                _ => {}
            }
        }
        if result.is_ok()
            && let Some(meta) = view
        {
            verdict(
                Rule::W4,
                FsMethod::RenameDurable,
                &[from, to],
                view_counts(meta),
            );
        }
        result
    }
}

/// W4 after a view file of `meta` was renamed into place: the four files' entry counts must
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
mod tests;
