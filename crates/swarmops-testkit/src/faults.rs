//! Fault doubles beyond the store's own (`FailAtFs` and `OrderCheckFs`, which the store crate
//! holds behind its `test-hooks` feature):
//!
//! - [`FsyncModelFs`]: the real filesystem with a model of what a power loss would leave of
//!   each file's data under Linux's fsync-failure semantics, failing the sync-class calls a
//!   predicate picks as Linux fails them. The power-loss images can take what it says is
//!   durable as their starting point ([`FsyncModelFs::durable`],
//!   [`FsyncModelFs::write_durable_image`]).
//! - [`checked`]: a filesystem under the order checker in a build that has it.
//!
//! The store's tests use them.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use swarmops_store::{FsMethod, FsOps, LinkOutcome, RealFs};

/// `fs` with the store's write-ordering rules checked on every call
/// (`swarmops_store::OrderCheckFs`, which panics at a call that breaks one), in a build with
/// this crate's `test-hooks` feature (which the store's, the repository's, the operations' and
/// the CLI's turn on, as `--all-features` does in the gates); `fs` as it is in any other.
#[cfg(feature = "test-hooks")]
pub fn checked(fs: impl FsOps + 'static) -> Arc<dyn FsOps> {
    Arc::new(swarmops_store::OrderCheckFs::new(fs))
}

/// `fs` as it is: a build without this crate's `test-hooks` feature has no order checker.
#[cfg(not(feature = "test-hooks"))]
pub fn checked(fs: impl FsOps + 'static) -> Arc<dyn FsOps> {
    Arc::new(fs)
}

/// One sync-class call, as an [`FsyncModelFs`] shows it to the predicate that picks the calls
/// it fails.
pub struct SyncCall<'a> {
    /// Its number among the model's sync-class calls, from 1.
    pub n: u64,
    /// The method, which compares equal to its name (`call.method == "append_durable"`): one
    /// that flushes ([`FsMethod::flushes`]).
    pub method: FsMethod,
    /// The path it acts on: a rename's target; a directory for `fsync_dir` and
    /// `write_durable_temp`.
    pub path: &'a Path,
}

/// What picks the sync-class calls an [`FsyncModelFs`] fails.
type FailWhen = dyn Fn(&SyncCall<'_>) -> bool + Send + Sync;

/// The real filesystem (or `F`, on real files), with a model of what a power loss would leave
/// of each file's data under Linux's fsync-failure semantics (Rebello et al., ATC'20: after a
/// failed fsync, "pages are always marked clean"), and the sync-class calls its predicate
/// picks failing as they fail on Linux:
///
/// - The sync-class calls are those that flush: `append_durable`, `sync_data`, `truncate`,
///   `atomic_replace`, `write_durable_temp`, `fsync_dir` and `rename_durable`. One that fails
///   runs all the same, then reports `EIO`: what it wrote is in the files (the page cache, for
///   every reader), but the bytes its sync should have made durable are not, and stay not
///   durable for good. A later sync that succeeds does not make them durable: Linux reports a
///   failed writeback once, to the sync that meets it, and marks the pages clean.
/// - Every other byte follows the calls: durable once a sync of its file covers it
///   (`append_durable`'s own, `sync_data`, `truncate`'s, the temp file's of `atomic_replace`
///   and `write_durable_temp`), volatile until then.
///
/// A file the model has seen no call on counts as durable as it is. It models file data only:
/// directory entries count as durable at once, as storage spec §2 rule 5 lets it (a failed
/// directory fsync is fail-stop on the filesystems the store supports, so no later sync succeeds
/// over an entry it left unsure; a power loss can take back entries no fsync of their directory
/// covered, which the power-loss images model), and so a failed `fsync_dir`, or a failed
/// `rename_durable` or `atomic_replace` once its data is synced, loses no data here.
pub struct FsyncModelFs<F: FsOps = RealFs> {
    /// The filesystem the calls run on.
    inner: F,
    /// The sync-class calls it fails.
    fail: Box<FailWhen>,
    /// What it knows.
    state: Mutex<Model>,
}

/// What the model knows.
#[derive(Default)]
struct Model {
    /// The files it follows, by path.
    files: HashMap<PathBuf, ModelFile>,
    /// The sync-class calls so far.
    syncs: u64,
    /// The calls it failed, as `"<method> <path>"`.
    failed: Vec<String>,
}

/// What the model knows of one file.
#[derive(Clone, Default)]
struct ModelFile {
    /// What a power loss would leave of it.
    durable: Vec<u8>,
    /// Its length when a sync last covered it, whether that sync succeeded or not: the bytes past
    /// it are written but not synced (appends only add to a file's end, and cuts sync).
    synced_len: u64,
    /// Byte ranges `[start, end)` that a failed sync left clean but not durable.
    lost: Vec<(u64, u64)>,
}

impl ModelFile {
    /// A file all of whose `bytes` are durable.
    fn durable(bytes: Vec<u8>) -> ModelFile {
        ModelFile {
            synced_len: swarmops_core::bytes::to_u64(bytes.len()),
            durable: bytes,
            lost: Vec::new(),
        }
    }

    /// Whether the byte at `at` is one a failed sync lost.
    fn is_lost(&self, at: u64) -> bool {
        self.lost
            .iter()
            .any(|&(start, end)| (start..end).contains(&at))
    }

    /// A sync that succeeded over `now`, the file's content: every byte is durable now but
    /// those a failed sync lost, which keep what was durable before (zeros past its end).
    fn synced(&mut self, now: &[u8]) {
        let durable = now
            .iter()
            .enumerate()
            .map(|(at, &byte)| {
                if self.is_lost(swarmops_core::bytes::to_u64(at)) {
                    self.durable.get(at).copied().unwrap_or(0)
                } else {
                    byte
                }
            })
            .collect();
        self.durable = durable;
        self.synced_len = swarmops_core::bytes::to_u64(now.len());
    }

    /// A sync that failed over a file `len` bytes long: the bytes written since the last sync
    /// are clean now, and not durable.
    fn sync_failed(&mut self, len: u64) {
        if len > self.synced_len {
            self.lost.push((self.synced_len, len));
        }
        self.synced_len = len;
    }

    /// The file cut to `len`.
    fn cut(&mut self, len: u64) {
        for range in &mut self.lost {
            range.1 = range.1.min(len);
        }
        self.lost.retain(|&(start, end)| start < end);
        self.synced_len = self.synced_len.min(len);
    }
}

impl FsyncModelFs<RealFs> {
    /// The model over the real filesystem, failing nothing.
    pub fn new() -> Self {
        Self::over(RealFs)
    }

    /// The model over the real filesystem, failing its `k`-th sync-class call, and no other.
    pub fn failing_sync(k: u64) -> Self {
        Self::new().failing(move |call| call.n == k)
    }
}

impl<F: FsOps> FsyncModelFs<F> {
    /// The model over `inner`, failing nothing.
    pub fn over(inner: F) -> Self {
        Self {
            inner,
            fail: Box::new(|_| false),
            state: Mutex::new(Model::default()),
        }
    }

    /// This model, failing the sync-class calls `fail` picks.
    #[must_use]
    pub fn failing(mut self, fail: impl Fn(&SyncCall<'_>) -> bool + Send + Sync + 'static) -> Self {
        self.fail = Box::new(fail);
        self
    }

    /// How many sync-class calls it has seen.
    pub fn syncs(&self) -> u64 {
        self.state().syncs
    }

    /// The calls it failed, as `"<method> <path>"`.
    pub fn failed(&self) -> Vec<String> {
        self.state().failed.clone()
    }

    /// What a power loss would leave of the file at `path`; `None` for one the model has seen
    /// no call on, which counts as durable as it is.
    pub fn durable(&self, path: &Path) -> Option<Vec<u8>> {
        self.state()
            .files
            .get(path)
            .map(|file| file.durable.clone())
    }

    /// Whether bytes `[start, end)` of the file at `path`, as it holds them now, would survive a
    /// power loss.
    pub fn is_durable(&self, path: &Path, start: u64, end: u64) -> bool {
        let state = self.state();
        let Some(file) = state.files.get(path) else {
            return true;
        };
        let now = fs::read(path).unwrap_or_default();
        let (Ok(from), Ok(to)) = (usize::try_from(start), usize::try_from(end)) else {
            return false;
        };
        to <= now.len()
            && to <= file.durable.len()
            && now[from..to] == file.durable[from..to]
            && !(start..end).any(|at| file.is_lost(at))
    }

    /// Copies the tree at `from` to `to` (which must not exist), each file the model follows
    /// holding what a power loss would leave of it, the others as they are.
    pub fn write_durable_image(&self, from: &Path, to: &Path) {
        fs::create_dir_all(to).expect("make a directory of the image");
        for entry in fs::read_dir(from).expect("read a directory") {
            let entry = entry.expect("a directory entry");
            let (source, target) = (entry.path(), to.join(entry.file_name()));
            if entry.file_type().expect("its type").is_dir() {
                self.write_durable_image(&source, &target);
            } else {
                let bytes = match self.durable(&source) {
                    Some(durable) => durable,
                    None => fs::read(&source).expect("read a file"),
                };
                fs::write(&target, bytes).expect("write a file of the image");
            }
        }
    }

    /// What it knows, locked (a test that panicked with it held leaves it as it was).
    fn state(&self) -> MutexGuard<'_, Model> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The model's file for `path`, following it from now on (as durable as it is now).
    fn track<'m>(model: &'m mut Model, path: &Path) -> &'m mut ModelFile {
        model
            .files
            .entry(path.to_path_buf())
            .or_insert_with(|| ModelFile::durable(fs::read(path).unwrap_or_default()))
    }

    /// Counts sync-class call `method` on `path`, and says whether it fails.
    fn fails(&self, method: FsMethod, path: &Path) -> bool {
        debug_assert!(method.flushes(), "{method} is no sync-class call");
        let mut state = self.state();
        state.syncs += 1;
        let call = SyncCall {
            n: state.syncs,
            method,
            path,
        };
        let fails = (self.fail)(&call);
        if fails {
            state.failed.push(format!("{method} {}", path.display()));
        }
        fails
    }

    /// Runs a sync-class call of `method` on `path` that syncs the data of `file` (the file it
    /// writes, or `None`), then fails it if the predicate says so. With the sync's outcome the
    /// model follows: what the call wrote is durable if it succeeded, lost if it failed.
    fn flush<T>(
        &self,
        method: FsMethod,
        path: &Path,
        file: Option<&Path>,
        call: impl FnOnce() -> io::Result<T>,
    ) -> io::Result<T> {
        if let Some(file) = file {
            Self::track(&mut self.state(), file);
        }
        let fails = self.fails(method, path);
        let result = call();
        let failed = fails || result.is_err();
        if let Some(file) = file {
            let mut state = self.state();
            let model = Self::track(&mut state, file);
            if failed {
                model.sync_failed(len_of(file));
            } else {
                model.synced(&fs::read(file).unwrap_or_default());
            }
        }
        match result {
            Ok(_) if fails => Err(injected(method, path)),
            result => result,
        }
    }
}

impl Default for FsyncModelFs<RealFs> {
    fn default() -> Self {
        Self::new()
    }
}

/// The error a failed sync-class call reports.
fn injected(method: FsMethod, path: &Path) -> io::Error {
    io::Error::other(format!(
        "injected EIO: the writeback of {method} {} failed",
        path.display()
    ))
}

/// The length of the file at `path`; 0 if there is none.
fn len_of(path: &Path) -> u64 {
    fs::metadata(path).map_or(0, |meta| meta.len())
}

impl<F: FsOps> FsOps for FsyncModelFs<F> {
    fn create_dir_all(&self, p: &Path) -> io::Result<()> {
        self.inner.create_dir_all(p)
    }

    fn write_durable_temp(&self, dir: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
        let fails = self.fails(FsMethod::WriteDurableTemp, dir);
        let path = self.inner.write_durable_temp(dir, bytes)?;
        let mut state = self.state();
        let mut model = ModelFile::durable(bytes.to_vec());
        if fails {
            // The temp file's one sync failed: none of it is durable.
            model = ModelFile::default();
            model.sync_failed(swarmops_core::bytes::to_u64(bytes.len()));
        }
        state.files.insert(path.clone(), model);
        drop(state);
        if fails {
            Err(injected(FsMethod::WriteDurableTemp, dir))
        } else {
            Ok(path)
        }
    }

    fn link_noclobber(&self, from: &Path, to: &Path) -> io::Result<LinkOutcome> {
        Self::track(&mut self.state(), from);
        let outcome = self.inner.link_noclobber(from, to)?;
        let mut state = self.state();
        let model = state.files.remove(from);
        if let (LinkOutcome::Placed, Some(model)) = (outcome, model) {
            state.files.insert(to.to_path_buf(), model);
        }
        Ok(outcome)
    }

    fn fsync_dir(&self, p: &Path) -> io::Result<()> {
        self.flush(FsMethod::FsyncDir, p, None, || self.inner.fsync_dir(p))
    }

    fn read(&self, p: &Path) -> io::Result<Option<Vec<u8>>> {
        self.inner.read(p)
    }

    fn exists(&self, p: &Path) -> io::Result<bool> {
        self.inner.exists(p)
    }

    fn remove(&self, p: &Path) -> io::Result<()> {
        self.inner.remove(p)?;
        self.state().files.remove(p);
        Ok(())
    }

    fn append_durable(&self, path: &Path, bytes: &[u8]) -> io::Result<u64> {
        self.flush(FsMethod::AppendDurable, path, Some(path), || {
            self.inner.append_durable(path, bytes)
        })
    }

    fn file_len(&self, path: &Path) -> io::Result<u64> {
        self.inner.file_len(path)
    }

    fn truncate(&self, path: &Path, len: u64) -> io::Result<()> {
        Self::track(&mut self.state(), path);
        let fails = self.fails(FsMethod::Truncate, path);
        let result = self.inner.truncate(path, len);
        let mut state = self.state();
        let model = Self::track(&mut state, path);
        let now = len_of(path);
        model.cut(now);
        if fails || result.is_err() {
            // The cut is made, its sync failed: what was durable stays so, the cut included.
            model.sync_failed(now);
        } else {
            model.synced(&fs::read(path).unwrap_or_default());
        }
        drop(state);
        match result {
            Ok(()) if fails => Err(injected(FsMethod::Truncate, path)),
            result => result,
        }
    }

    fn atomic_replace(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        let fails = self.fails(FsMethod::AtomicReplace, path);
        self.inner.atomic_replace(path, bytes)?;
        // The new content was synced before its rename: whatever the directory's fsync did,
        // the data is durable.
        self.state()
            .files
            .insert(path.to_path_buf(), ModelFile::durable(bytes.to_vec()));
        if fails {
            Err(injected(FsMethod::AtomicReplace, path))
        } else {
            Ok(())
        }
    }

    fn sync_data(&self, path: &Path) -> io::Result<()> {
        self.flush(FsMethod::SyncData, path, Some(path), || {
            self.inner.sync_data(path)
        })
    }

    fn append(&self, path: &Path, bytes: &[u8]) -> io::Result<u64> {
        Self::track(&mut self.state(), path);
        self.inner.append(path, bytes)
    }

    fn rename_durable(&self, from: &Path, to: &Path) -> io::Result<()> {
        let fails = self.fails(FsMethod::RenameDurable, to);
        self.inner.rename_durable(from, to)?;
        // The data moves with the name (a directory's files with it): a failed fsync of the
        // directory loses no data here.
        let mut state = self.state();
        let moved: Vec<PathBuf> = state
            .files
            .keys()
            .filter(|path| path.starts_with(from))
            .cloned()
            .collect();
        for path in moved {
            if let (Some(model), Ok(rest)) = (state.files.remove(&path), path.strip_prefix(from)) {
                let new = if rest.as_os_str().is_empty() {
                    to.to_path_buf()
                } else {
                    to.join(rest)
                };
                state.files.insert(new, model);
            }
        }
        drop(state);
        if fails {
            Err(injected(FsMethod::RenameDurable, to))
        } else {
            Ok(())
        }
    }
}
