//! The scan of `ws status` (§4.1 of the M5 design): the index's tracked paths stat'd through
//! `HostFs`, the untracked additions found by one configured walker, and the files hashed with
//! at most [`HASH_BYTES_IN_FLIGHT`] bytes held at once.
//!
//! ```text
//!  index.bin (or the head's tree, rebuilt) ──▶ rows ──▶ symlink_metadata, in parallel
//!                                                         │ stat equal, mtime before the index
//!                                                         ├──────────────▶ unchanged
//!                                                         └─ else hashed ─▶ unchanged, row
//!                                                                           refreshed; or changed
//!  walker(root, prefixes) ──▶ files the rows lack and no ignore file names ──▶ added
//! ```

use std::collections::{BTreeMap, HashSet};
use std::io;
use std::sync::{Condvar, Mutex};

use camino::Utf8Path;
use ignore::{WalkBuilder, WalkState};
use rayon::prelude::*;
use swarmops_api::{FileKind, OpError, StoreOps, WsChangeKind};
use swarmops_core::{ObjectId, ObjectKind, hash_object};

use crate::hostfs::{HostFs, HostKind, HostMeta};
use crate::index::{self, Index, Row, UNCONFIRMED};
use crate::materialize::{Tree, covered, host_failed, items_under};
use crate::state::{SWARMWS_DIR, Volume};
use crate::volume::EXEC_BITS;
use crate::workspace::Workspace;
use crate::{Ctx, io_failed};

/// The most bytes of file content the hasher holds at once: a file is read only when its size
/// fits what is left, else it waits; a larger file runs alone.
pub const HASH_BYTES_IN_FLIGHT: u64 = 1 << 30;

/// The custom ignore file, read beside each `.gitignore`, in its syntax.
pub const SWARMIGNORE: &str = ".swarmignore";

/// A blob's id, as the index holds it.
pub(crate) type Blob = [u8; 32];

/// What a scan looks at.
#[derive(Debug, Clone, Default)]
pub(crate) struct Want {
    /// Only these repository paths and what is below them; every path when empty.
    pub(crate) paths: Vec<String>,
    /// Hash every tracked file, trusting no stat.
    pub(crate) full: bool,
    /// Paths outside the workspace's prefixes to look at too, each a file the host holds: a
    /// commit that names them.
    pub(crate) extra: Vec<String>,
}

/// One changed path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Change {
    /// The repository path.
    pub(crate) path: String,
    /// What changed.
    pub(crate) kind: WsChangeKind,
    /// What the head holds there, if anything.
    pub(crate) old: Option<(FileKind, Blob)>,
    /// What the host holds there, if anything.
    pub(crate) new: Option<HostFile>,
}

/// A file the host holds at a changed path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HostFile {
    /// Its kind, as the workspace records it (§3.3: a symlink a volume cannot make is a file
    /// holding its target; the mode of a volume without the bit is the index's).
    pub(crate) kind: FileKind,
    /// Its stat, taken during the scan.
    pub(crate) meta: HostMeta,
    /// Its blob, when the scan hashed it.
    pub(crate) blob: Option<Blob>,
}

/// What a scan found: the changes in path order, and the index refreshed with what it learned.
pub(crate) struct Scanned {
    /// The changes, by path.
    pub(crate) changes: Vec<Change>,
    /// The rows, refreshed where a hash found a file unchanged.
    pub(crate) index: Index,
    /// Whether `index` differs from the one read (rebuilt or refreshed).
    pub(crate) refreshed: bool,
    /// The temp file the next index is written through; its mtime is `index.written_ns`. None
    /// where `.swarmws/` takes no file: the index is not written then.
    pub(crate) temp: Option<tempfile::NamedTempFile>,
}

impl Scanned {
    /// Writes the index, bound to `binding`, if it changed and can be written; a cache, so a
    /// failure leaves the next scan to rebuild it.
    pub(crate) fn keep(self, dir: &Utf8Path, binding: &str, force: Keep) -> Vec<Change> {
        let Scanned {
            changes,
            index,
            refreshed,
            temp,
        } = self;
        if let Some(temp) = temp
            && (refreshed || force == Keep::Always)
        {
            let _ = index::write(dir, temp, binding, &index);
        }
        changes
    }
}

/// Whether [`Scanned::keep`] writes an index that did not change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Keep {
    /// Only when it changed.
    IfChanged,
    /// Always: it stands for something new.
    Always,
}

/// The walker of the untracked additions below `root`, under `prefixes` (the whole tree when
/// empty): the one configuration `status`, `--full` and the tests share (§4.1 (2)). The tree's
/// `.gitignore` files apply with no `.git` anywhere, `.swarmignore` files beside them; dotfiles
/// are listed; nothing above `root`, no `.ignore` and no host-wide exclude is read; no link is
/// followed; `.swarmws` at the root and every `.git` directory are skipped.
pub fn walker(root: &Utf8Path, prefixes: &[String]) -> WalkBuilder {
    // 1. Away from the defaults, which are git's inside a repository.
    let mut builder = WalkBuilder::new(root);
    builder
        .standard_filters(false)
        .git_ignore(true)
        .require_git(false)
        .add_custom_ignore_filename(SWARMIGNORE)
        .hidden(false)
        .parents(false)
        .git_global(false)
        .git_exclude(false)
        .ignore(false)
        .follow_links(false);

    // 2. The workspace's own state, nested repositories' state, and what lies off the prefixes.
    let root = root.to_path_buf();
    let prefixes = prefixes.to_vec();
    builder.filter_entry(move |entry| {
        let Some(rel) = relative(&root, entry.path()) else {
            return true;
        };
        let is_dir = entry.file_type().is_some_and(|kind| kind.is_dir());
        if rel == SWARMWS_DIR || (is_dir && entry.file_name() == ".git") {
            return false;
        }
        prefixes.is_empty() || covered(&rel, &prefixes) || leads_to(&rel, &prefixes)
    });
    builder
}

/// Scans workspace `ws` as `want` says: its changes against its head, and its index refreshed.
pub(crate) fn scan(ctx: &Ctx<'_>, ws: &Workspace, want: &Want) -> Result<Scanned, OpError> {
    // 1. The next index's temp first: its mtime is the filesystem's clock before any stat.
    let dir = &ws.dir;
    let temp = tempfile::Builder::new()
        .prefix("index.")
        .tempfile_in(dir.join(SWARMWS_DIR))
        .ok();
    let written_ns = temp.as_ref().and_then(stamp).unwrap_or(i64::MAX);

    // 2. The rows: the index, or the head's tree rebuilt; the named outside paths joined.
    let binding = binding_of(ws);
    let (mut index, mut refreshed) = match index::read(dir, &binding) {
        Some(index) => (index, false),
        None => (rebuild(ctx.store, ws)?, true),
    };
    let stale_ns = if refreshed {
        i64::MIN
    } else {
        index.written_ns
    };
    for path in &want.extra {
        refreshed |= join_rows(ctx.store, ws, &mut index, path)?;
    }
    index.written_ns = written_ns;

    // 3. The tracked rows, stat'd in parallel; those whose stat says nothing, hashed.
    let wanted = |path: &str| want.paths.is_empty() || covered(path, &want.paths);
    let host = ctx.host.as_ref();
    let looks: Vec<Look> = (index.rows.par_iter().enumerate())
        .filter(|(_, row)| wanted(&row.path))
        .map(|(at, row)| look(host, ws, row, at, stale_ns, want))
        .collect::<Result<_, _>>()?;
    let to_hash: Vec<(String, HostFile)> = (looks.iter())
        .filter(|look| look.hash)
        .filter_map(|look| Some((index.rows[look.at].path.clone(), look.file?)))
        .collect();
    let blobs = hash_all(host, dir, &to_hash, HASH_BYTES_IN_FLIGHT)?;

    // 4. Each tracked row's change, if any; a row found unchanged by its hash refreshed.
    let mut changes = BTreeMap::new();
    let mut next_blob = blobs.into_iter();
    for look in looks {
        let blob = if look.hash {
            next_blob.next().flatten()
        } else {
            None
        };
        let row = &mut index.rows[look.at];
        if let Some(change) = settle(row, look, blob, &mut refreshed) {
            changes.insert(change.path.clone(), change);
        }
    }

    // 5. The untracked additions under the prefixes.
    let tracked: HashSet<&str> = index.rows.iter().map(|row| row.path.as_str()).collect();
    for (path, meta) in untracked(ctx.host.as_ref(), ws, &want.extra, &tracked)? {
        if !wanted(&path) {
            continue;
        }
        let kind = host_kind(meta, None, ws.file.volume);
        let new = HostFile {
            kind,
            meta,
            blob: None,
        };
        changes.insert(
            path.clone(),
            Change {
                path,
                kind: WsChangeKind::Added,
                old: None,
                new: Some(new),
            },
        );
    }

    Ok(Scanned {
        changes: changes.into_values().collect(),
        index,
        refreshed,
        temp,
    })
}

/// What an index of workspace `ws` stands for.
pub(crate) fn binding_of(ws: &Workspace) -> String {
    index::binding(&ws.file.head, &ws.file.prefixes, ws.file.all)
}

/// What a tracked row's stat said.
#[derive(Debug, Clone, Copy)]
struct Look {
    /// The row's place in the index.
    at: usize,
    /// The file there, as the workspace records it; none when nothing (or a directory, or
    /// something that is no file) is there.
    file: Option<HostFile>,
    /// Whether its kind is the row's: a symlink both, or a file (any mode) both.
    same_kind: bool,
    /// Whether its bytes must be hashed to know whether they changed.
    hash: bool,
}

/// The stat of `row`, the `at`-th of the index of workspace `ws`, written at `stale_ns` (the
/// racy rule): whether it is there, of the row's kind, and whether to hash it (always, when
/// `want` is full).
fn look(
    host: &dyn HostFs,
    ws: &Workspace,
    row: &Row,
    at: usize,
    stale_ns: i64,
    want: &Want,
) -> Result<Look, OpError> {
    // 1. The stat; nothing, a directory or no file at all reads as gone.
    let meta = host
        .symlink_metadata(&ws.dir.join(&row.path))
        .map_err(|e| host_failed(&row.path, &e))?;
    let Some(meta) = meta.filter(|meta| matches!(meta.kind, HostKind::File | HostKind::Symlink))
    else {
        return Ok(Look {
            at,
            file: None,
            same_kind: false,
            hash: false,
        });
    };

    // 2. Its kind as recorded; another kind needs no hash to be a change, but a commit needs one.
    let kind = host_kind(meta, Some(row.mode), ws.file.volume);
    let same_kind = (kind == FileKind::Symlink) == (row.mode == FileKind::Symlink);
    let file = Some(HostFile {
        kind,
        meta,
        blob: None,
    });

    // 3. Unchanged by its stat only when the index saw this very stat before it was written.
    let known = row.mtime_ns != UNCONFIRMED
        && meta.size == row.size
        && meta.mtime_ns == row.mtime_ns
        && meta.inode == row.inode
        && meta.mtime_ns < stale_ns;
    let hash = same_kind && (want.full || !known);
    Ok(Look {
        at,
        file,
        same_kind,
        hash,
    })
}

/// The change at `row` that `look` and its `blob` (when hashed) show, if any; the row's stat
/// refreshed when the hash finds its bytes unchanged.
fn settle(row: &mut Row, look: Look, blob: Option<Blob>, refreshed: &mut bool) -> Option<Change> {
    // 1. Gone, or another kind.
    let change = |row: &Row, kind, new| {
        Some(Change {
            path: row.path.clone(),
            kind,
            old: Some((row.mode, row.blob)),
            new,
        })
    };
    let Some(mut file) = look.file else {
        return change(row, WsChangeKind::Deleted, None);
    };
    if !look.same_kind {
        return change(row, WsChangeKind::TypeChanged, Some(file));
    }

    // 2. The bytes: changed, or the same as the row's (then its stat is the one to trust).
    if look.hash {
        file.blob = blob;
        if blob != Some(row.blob) {
            return change(row, WsChangeKind::Modified, Some(file));
        }
        let seen = (file.meta.size, file.meta.mtime_ns, file.meta.inode);
        if (row.size, row.mtime_ns, row.inode) != seen {
            (row.size, row.mtime_ns, row.inode) = seen;
            *refreshed = true;
        }
    } else {
        file.blob = Some(row.blob);
    }

    // 3. The same bytes: the mode alone may differ.
    if file.kind != row.mode {
        return change(row, WsChangeKind::ModeChanged, Some(file));
    }
    None
}

/// The kind the workspace records for a host file of `meta`, at a path whose row (if any) has
/// kind `row`, on a volume that holds `volume` (§3.3).
pub(crate) fn host_kind(meta: HostMeta, row: Option<FileKind>, volume: Volume) -> FileKind {
    // 1. A symlink is one; a file standing for a symlink where the volume makes none is one too.
    if meta.kind == HostKind::Symlink {
        return FileKind::Symlink;
    }
    if row == Some(FileKind::Symlink) && !volume.symlinks {
        return FileKind::Symlink;
    }

    // 2. A file: its bit where the volume keeps it, else the index's mode.
    if volume.exec_bit {
        return if meta.mode & EXEC_BITS == 0 {
            FileKind::File
        } else {
            FileKind::Exec
        };
    }
    match row {
        Some(FileKind::Exec) => FileKind::Exec,
        _ => FileKind::File,
    }
}

/// The rows of workspace `ws` rebuilt from its head's tree under its prefixes, none confirmed:
/// each is hashed against the head's blob.
fn rebuild(store: &dyn StoreOps, ws: &Workspace) -> Result<Index, OpError> {
    // 1. The head's files under the prefixes.
    let tree = Tree::new(store, &ws.file.head);
    let mut items = Vec::new();
    if ws.file.all {
        items.extend(items_under(store, &tree, None)?.unwrap_or_default());
    } else {
        for prefix in &ws.file.prefixes {
            items.extend(items_under(store, &tree, Some(prefix))?.unwrap_or_default());
        }
    }

    // 2. One row each, sorted.
    let mut rows = items
        .into_iter()
        .map(|item| unconfirmed(&item.path, item.kind, &item.blob))
        .collect::<Result<Vec<_>, _>>()?;
    rows.sort_by(|a, b| a.path.cmp(&b.path));
    rows.dedup_by(|a, b| a.path == b.path);
    Ok(Index {
        written_ns: 0,
        rows,
    })
}

/// Adds to `index` the head's entry at `path`, outside the prefixes of workspace `ws`, if the
/// head has one there and the index lacks it; whether it did.
fn join_rows(
    store: &dyn StoreOps,
    ws: &Workspace,
    index: &mut Index,
    path: &str,
) -> Result<bool, OpError> {
    let tree = Tree::new(store, &ws.file.head);
    let mut joined = false;
    for item in items_under(store, &tree, Some(path))?.unwrap_or_default() {
        let Err(at) = index
            .rows
            .binary_search_by(|row| row.path.as_str().cmp(&item.path))
        else {
            continue;
        };
        index
            .rows
            .insert(at, unconfirmed(&item.path, item.kind, &item.blob)?);
        joined = true;
    }
    Ok(joined)
}

/// A row for `path` holding blob `blob` (hex) of `kind`, its stat unconfirmed.
fn unconfirmed(path: &str, kind: FileKind, blob: &str) -> Result<Row, OpError> {
    Ok(Row {
        path: path.to_owned(),
        size: 0,
        mtime_ns: UNCONFIRMED,
        inode: 0,
        mode: kind,
        blob: blob_of(blob)?,
    })
}

/// Blob id `hex` as the index holds it.
pub(crate) fn blob_of(hex: &str) -> Result<Blob, OpError> {
    ObjectId::from_hex(hex).map(|id| id.0).map_err(|e| {
        OpError::new(
            swarmops_api::ErrorCode::Internal,
            format!("the store gave {hex:?} as a blob id: {e}"),
        )
    })
}

/// The files and symlinks below the root of workspace `ws`, under its prefixes and `extra`,
/// that `tracked` lacks and no ignore file names, with their stat; paths that are not UTF-8 are
/// passed over (a repository path is UTF-8).
fn untracked(
    host: &dyn HostFs,
    ws: &Workspace,
    extra: &[String],
    tracked: &HashSet<&str>,
) -> Result<Vec<(String, HostMeta)>, OpError> {
    // 1. The walk, in parallel, keeping what no row names.
    let prefixes = if ws.file.all {
        Vec::new()
    } else {
        ws.file.prefixes.iter().chain(extra).cloned().collect()
    };
    let found = Mutex::new(Vec::new());
    let failed = Mutex::new(None);
    walker(&ws.dir, &prefixes).build_parallel().run(|| {
        Box::new(|entry| {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    failed
                        .lock()
                        .expect("no walker panicked")
                        .get_or_insert(e.to_string());
                    return WalkState::Quit;
                }
            };
            let is_file =
                (entry.file_type()).is_some_and(|kind| kind.is_file() || kind.is_symlink());
            if let Some(rel) = relative(&ws.dir, entry.path())
                && is_file
                && !tracked.contains(rel.as_str())
            {
                found.lock().expect("no walker panicked").push(rel);
            }
            WalkState::Continue
        })
    });
    if let Some(e) = failed.into_inner().expect("no walker panicked") {
        return Err(io_failed(
            swarmops_store::FsMethod::Read,
            ".",
            &io::Error::other(e),
        ));
    }

    // 2. Each one's stat, through the host layer; one gone since is not there.
    let mut found = found.into_inner().expect("no walker panicked");
    found.sort();
    let mut out = Vec::with_capacity(found.len());
    for path in found {
        let meta = host
            .symlink_metadata(&ws.dir.join(&path))
            .map_err(|e| host_failed(&path, &e))?;
        if let Some(meta) = meta.filter(|m| matches!(m.kind, HostKind::File | HostKind::Symlink)) {
            out.push((path, meta));
        }
    }
    Ok(out)
}

/// `path` below `root` as a repository path (`/`-separated); none for `root` itself or a path
/// that is not UTF-8 or not below it.
fn relative(root: &Utf8Path, path: &std::path::Path) -> Option<String> {
    let rest = path.strip_prefix(root).ok()?;
    let parts = (rest.components())
        .map(|part| part.as_os_str().to_str())
        .collect::<Option<Vec<&str>>>()?;
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// Whether `dir` is a directory above one of `prefixes`, which a walk must enter to reach it.
fn leads_to(dir: &str, prefixes: &[String]) -> bool {
    (prefixes.iter()).any(|prefix| {
        prefix
            .strip_prefix(dir)
            .is_some_and(|rest| rest.starts_with('/'))
    })
}

/// The mtime of `temp`, just made: the filesystem's clock now, in Unix nanoseconds.
fn stamp(temp: &tempfile::NamedTempFile) -> Option<i64> {
    let modified = temp.as_file().metadata().ok()?.modified().ok()?;
    let since = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    i64::try_from(since.as_nanos()).ok()
}

/// The blob ids of `files`, each a repository path below `dir` with what the host holds there,
/// hashed in parallel with at most `budget` bytes read and not yet hashed at once (a file larger
/// than the budget alone runs alone); none for a file gone since its stat.
pub(crate) fn hash_all(
    host: &dyn HostFs,
    dir: &Utf8Path,
    files: &[(String, HostFile)],
    budget: u64,
) -> Result<Vec<Option<Blob>>, OpError> {
    let held = Budget::new(budget);
    (files.par_iter())
        .map(|(path, file)| {
            let size = file.meta.size;
            held.acquire(size);
            let blob = hash_one(host, &dir.join(path), file.meta.kind);
            held.release(size);
            match blob {
                Ok(blob) => Ok(Some(blob)),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(host_failed(path, &e)),
            }
        })
        .collect()
}

/// The blob id of what the host holds at `abs`: a file's bytes, or a symlink's target.
fn hash_one(host: &dyn HostFs, abs: &Utf8Path, kind: HostKind) -> io::Result<Blob> {
    let bytes = content(host, abs, kind)?;
    Ok(blob_id(&bytes))
}

/// The id of the blob holding `bytes`.
pub(crate) fn blob_id(bytes: &[u8]) -> Blob {
    hash_object(ObjectKind::Blob, bytes).0
}

/// What the host holds at `abs`, as a blob holds it: a file's bytes, or a symlink's target.
pub(crate) fn content(host: &dyn HostFs, abs: &Utf8Path, kind: HostKind) -> io::Result<Vec<u8>> {
    if kind == HostKind::Symlink {
        return host.read_link(abs).map(String::into_bytes);
    }
    host.read_file(abs)
}

/// The bytes the hasher holds, bounded.
struct Budget {
    /// The most bytes held at once.
    limit: u64,
    /// The bytes held now.
    used: Mutex<u64>,
    /// Signalled whenever bytes are let go.
    freed: Condvar,
}

impl Budget {
    /// A budget of `limit` bytes.
    fn new(limit: u64) -> Budget {
        Budget {
            limit,
            used: Mutex::new(0),
            freed: Condvar::new(),
        }
    }

    /// Takes `size` bytes, waiting until they fit; with nothing held, any size fits.
    fn acquire(&self, size: u64) {
        let mut used = self.used.lock().expect("no hasher panicked");
        while *used > 0 && used.saturating_add(size) > self.limit {
            used = self.freed.wait(used).expect("no hasher panicked");
        }
        *used += size;
    }

    /// Lets `size` bytes go.
    fn release(&self, size: u64) {
        let mut used = self.used.lock().expect("no hasher panicked");
        *used -= size;
        self.freed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use super::*;
    use crate::hostfs::RealHost;

    /// A host that reads slowly and records the most bytes being read at once.
    #[derive(Default)]
    struct SlowReads {
        /// The bytes being read now.
        now: AtomicU64,
        /// The most at once.
        peak: AtomicU64,
    }

    impl HostFs for SlowReads {
        fn write_file(&self, path: &Utf8Path, bytes: &[u8]) -> io::Result<()> {
            RealHost.write_file(path, bytes)
        }
        fn symlink(&self, target: &str, path: &Utf8Path) -> io::Result<()> {
            RealHost.symlink(target, path)
        }
        fn symlink_metadata(&self, path: &Utf8Path) -> io::Result<Option<HostMeta>> {
            RealHost.symlink_metadata(path)
        }
        fn read_file(&self, path: &Utf8Path) -> io::Result<Vec<u8>> {
            let size = std::fs::metadata(path)?.len();
            let now = self.now.fetch_add(size, Ordering::SeqCst) + size;
            self.peak.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(30));
            let bytes = RealHost.read_file(path);
            self.now.fetch_sub(size, Ordering::SeqCst);
            bytes
        }
        fn read_dir(&self, path: &Utf8Path) -> io::Result<Vec<String>> {
            RealHost.read_dir(path)
        }
        fn remove_file(&self, path: &Utf8Path) -> io::Result<()> {
            RealHost.remove_file(path)
        }
        fn remove_dir_if_empty(&self, path: &Utf8Path) -> io::Result<bool> {
            RealHost.remove_dir_if_empty(path)
        }
        fn create_dir_all(&self, path: &Utf8Path) -> io::Result<()> {
            RealHost.create_dir_all(path)
        }
        fn rename(&self, from: &Utf8Path, to: &Utf8Path) -> io::Result<()> {
            RealHost.rename(from, to)
        }
        fn read_link(&self, path: &Utf8Path) -> io::Result<String> {
            RealHost.read_link(path)
        }
        fn set_mode(&self, path: &Utf8Path, mode: u32) -> io::Result<()> {
            RealHost.set_mode(path, mode)
        }
    }

    /// The hasher reads no more than its budget at once, however many files there are and
    /// however many threads could read them; a file larger than the budget is read alone. Each
    /// file still gets its blob's id.
    #[test]
    fn hashing_holds_at_most_hash_bytes_in_flight() {
        // 1. Eight files of 40 bytes and one of 150, summing well past a budget of 100.
        let tmp = tempfile::tempdir().unwrap();
        let dir = Utf8Path::from_path(tmp.path()).unwrap();
        let mut files = Vec::new();
        for n in 0..9u8 {
            let size = if n == 8 { 150 } else { 40 };
            let path = format!("f{n}");
            std::fs::write(dir.join(&path), vec![n; size]).unwrap();
            let meta = RealHost
                .symlink_metadata(&dir.join(&path))
                .unwrap()
                .unwrap();
            let file = HostFile {
                kind: FileKind::File,
                meta,
                blob: None,
            };
            files.push((path, file));
        }

        // 2. Hashed under the budget: at most 100 bytes read at once, but the big file's 150.
        let host = SlowReads::default();
        let blobs = hash_all(&host, dir, &files, 100).unwrap();
        let peak = host.peak.load(Ordering::SeqCst);
        assert!(peak == 150 || peak <= 100, "{peak} bytes read at once");
        let small = SlowReads::default();
        hash_all(&small, dir, &files[..8], 100).unwrap();
        assert!(small.peak.load(Ordering::SeqCst) <= 100);

        // 3. Each blob is its file's.
        for ((path, _), blob) in files.iter().zip(blobs) {
            let bytes = std::fs::read(dir.join(path)).unwrap();
            assert_eq!(blob, Some(hash_object(ObjectKind::Blob, &bytes).0));
        }
    }
}
