//! The filesystem operations the object store and the metadata store (journal and index)
//! perform, behind a trait so tests can count them or crash the process at an exact point of
//! a write.
//!
//! Two test hooks reach a whole process through its environment, for crash tests of a binary
//! (`swarmops` itself): `SWARMOPS_TEST_CRASH_AT=<k>` aborts it at its k-th call, and
//! `SWARMOPS_TEST_FS_CALLS=<file>` has it write how many calls it made, so that a dry run tells
//! a crash sweep where it ends. [`CrashAtFs::from_env`] reads both; the stores open on what it
//! gives by default ([`crate::FsObjectStore::open_default`], and `swarmops-repo`'s
//! `LocalRepo::open` and `create`, which share one for both stores). A third, test-only too,
//! `SWARMOPS_TEST_NOW=<unix seconds>`, has `swarmops-ops` sign changesets at that time: a commit
//! then writes the same objects in every run, so call k is the same call in every run.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Test hook: with `SWARMOPS_TEST_CRASH_AT=<k>` set to a positive k, the filesystem a store
/// opens on by default is a [`CrashAtFs`] that aborts the process on its k-th call (see
/// [`CrashAtFs::from_env`]).
pub const CRASH_AT_ENV: &str = "SWARMOPS_TEST_CRASH_AT";

/// Test hook: with `SWARMOPS_TEST_FS_CALLS=<file>` set, the filesystem a store opens on by
/// default is a [`CrashAtFs`] that writes how many calls it made to `<file>`, in decimal, when
/// it is dropped: a crash test's dry run, to count the calls of a run it then crashes at each
/// of (see [`CrashAtFs::from_env`]). A run that crashes writes nothing.
pub const FS_CALLS_ENV: &str = "SWARMOPS_TEST_FS_CALLS";

/// What [`FsOps::link_noclobber`] found at its target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkOutcome {
    /// The target did not exist; it now names the source file.
    Placed,
    /// The target already existed and was left untouched.
    AlreadyExists,
}

/// The filesystem operations of the object store and the metadata store.
///
/// Implementations are called from many threads at once, and many processes may
/// operate on the same store concurrently.
pub trait FsOps: Send + Sync {
    /// Creates `p` and any missing parents; succeeds if `p` already exists.
    fn create_dir_all(&self, p: &Path) -> io::Result<()>;

    /// Creates a new, uniquely named file in `dir`, writes `bytes`, flushes the file to
    /// stable storage (`sync_all`) and returns its path. The file's directory entry is
    /// not made durable.
    fn write_durable_temp(&self, dir: &Path, bytes: &[u8]) -> io::Result<PathBuf>;

    /// Gives the file at `from` the name `to` unless `to` already exists (never replacing
    /// it), then removes `from` in both cases. The new name is durable only after
    /// [`FsOps::fsync_dir`] on its directory. On error `from` may remain.
    fn link_noclobber(&self, from: &Path, to: &Path) -> io::Result<LinkOutcome>;

    /// Flushes directory `p` to stable storage, making the entries created in it durable.
    fn fsync_dir(&self, p: &Path) -> io::Result<()>;

    /// The whole content of file `p`, or `None` if it does not exist.
    fn read(&self, p: &Path) -> io::Result<Option<Vec<u8>>>;

    /// Whether `p` exists.
    fn exists(&self, p: &Path) -> io::Result<bool>;

    /// Removes file `p`.
    fn remove(&self, p: &Path) -> io::Result<()>;

    /// Appends `bytes` to file `path` (created if missing) and flushes the file's data to
    /// stable storage (`sync_data`). Returns the file's length before the append: the
    /// offset at which `bytes` now start, provided nobody else appends at the same time.
    /// A new file's directory entry is not made durable.
    fn append_durable(&self, path: &Path, bytes: &[u8]) -> io::Result<u64>;

    /// The length of file `path` in bytes; 0 if it does not exist.
    fn file_len(&self, path: &Path) -> io::Result<u64>;

    /// Cuts the existing file `path` down to `len` bytes and flushes it to stable storage
    /// (`sync_data`).
    fn truncate(&self, path: &Path, len: u64) -> io::Result<()>;

    /// Replaces the content of `path` (created if missing) with `bytes`, atomically and
    /// durably: a temp file in the same directory is written, `sync_all`ed and renamed over
    /// `path`, then the directory is fsynced. Readers see the old content or the new one.
    fn atomic_replace(&self, path: &Path, bytes: &[u8]) -> io::Result<()>;

    /// Flushes the data of the existing file `path` to stable storage (`sync_data`),
    /// whichever process wrote it.
    fn sync_data(&self, path: &Path) -> io::Result<()>;

    /// Appends `bytes` to file `path` (created if missing) without flushing them to stable
    /// storage: a later [`FsOps::sync_data`] does that, once for many appends. Returns the
    /// file's length before the append, as [`FsOps::append_durable`] does. A new file's
    /// directory entry is not made durable.
    fn append(&self, path: &Path, bytes: &[u8]) -> io::Result<u64>;

    /// Renames `from` to `to`, replacing `to` if it exists (atomically: readers see the old
    /// file or the new one), then flushes `to`'s directory to stable storage, making the new
    /// name durable. `from`'s data must be durable already, or `to` may lose it in a crash.
    fn rename_durable(&self, from: &Path, to: &Path) -> io::Result<()>;
}

/// The real filesystem.
#[derive(Debug, Clone, Copy, Default)]
pub struct RealFs;

impl FsOps for RealFs {
    fn create_dir_all(&self, p: &Path) -> io::Result<()> {
        fs::create_dir_all(p)
    }

    fn write_durable_temp(&self, dir: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
        let mut temp = tempfile::Builder::new().tempfile_in(dir)?;
        temp.write_all(bytes)?;
        temp.as_file().sync_all()?;
        let (_file, path) = temp.keep().map_err(|e| e.error)?;
        Ok(path)
    }

    fn link_noclobber(&self, from: &Path, to: &Path) -> io::Result<LinkOutcome> {
        let outcome = match fs::hard_link(from, to) {
            Ok(()) => LinkOutcome::Placed,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => LinkOutcome::AlreadyExists,
            Err(e) => return Err(e),
        };
        self.remove(from)?;
        Ok(outcome)
    }

    fn fsync_dir(&self, p: &Path) -> io::Result<()> {
        File::open(p)?.sync_all()
    }

    fn read(&self, p: &Path) -> io::Result<Option<Vec<u8>>> {
        match fs::read(p) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn exists(&self, p: &Path) -> io::Result<bool> {
        fs::exists(p)
    }

    fn remove(&self, p: &Path) -> io::Result<()> {
        fs::remove_file(p)
    }

    fn append_durable(&self, path: &Path, bytes: &[u8]) -> io::Result<u64> {
        let mut file = OpenOptions::new().append(true).create(true).open(path)?;
        let offset = file.metadata()?.len();
        file.write_all(bytes)?;
        file.sync_data()?;
        Ok(offset)
    }

    fn file_len(&self, path: &Path) -> io::Result<u64> {
        match fs::metadata(path) {
            Ok(meta) => Ok(meta.len()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(0),
            Err(e) => Err(e),
        }
    }

    fn truncate(&self, path: &Path, len: u64) -> io::Result<()> {
        let file = OpenOptions::new().write(true).open(path)?;
        file.set_len(len)?;
        file.sync_data()
    }

    fn atomic_replace(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        let dir = parent_dir(path);
        let mut temp = tempfile::Builder::new().tempfile_in(dir)?;
        temp.write_all(bytes)?;
        temp.as_file().sync_all()?;
        // On failure the temp file is removed when the returned `NamedTempFile` drops.
        temp.persist(path).map_err(|e| e.error)?;
        self.fsync_dir(dir)
    }

    fn sync_data(&self, path: &Path) -> io::Result<()> {
        File::open(path)?.sync_data()
    }

    fn append(&self, path: &Path, bytes: &[u8]) -> io::Result<u64> {
        let mut file = OpenOptions::new().append(true).create(true).open(path)?;
        let offset = file.metadata()?.len();
        file.write_all(bytes)?;
        Ok(offset)
    }

    fn rename_durable(&self, from: &Path, to: &Path) -> io::Result<()> {
        fs::rename(from, to)?;
        self.fsync_dir(parent_dir(to))
    }
}

/// The directory that holds `path`; `.` for a bare file name.
fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    }
}

/// One set of operations shared by several stores, so that they share one [`CrashAtFs`]
/// call count.
impl<F: FsOps + ?Sized> FsOps for Arc<F> {
    fn create_dir_all(&self, p: &Path) -> io::Result<()> {
        (**self).create_dir_all(p)
    }

    fn write_durable_temp(&self, dir: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
        (**self).write_durable_temp(dir, bytes)
    }

    fn link_noclobber(&self, from: &Path, to: &Path) -> io::Result<LinkOutcome> {
        (**self).link_noclobber(from, to)
    }

    fn fsync_dir(&self, p: &Path) -> io::Result<()> {
        (**self).fsync_dir(p)
    }

    fn read(&self, p: &Path) -> io::Result<Option<Vec<u8>>> {
        (**self).read(p)
    }

    fn exists(&self, p: &Path) -> io::Result<bool> {
        (**self).exists(p)
    }

    fn remove(&self, p: &Path) -> io::Result<()> {
        (**self).remove(p)
    }

    fn append_durable(&self, path: &Path, bytes: &[u8]) -> io::Result<u64> {
        (**self).append_durable(path, bytes)
    }

    fn file_len(&self, path: &Path) -> io::Result<u64> {
        (**self).file_len(path)
    }

    fn truncate(&self, path: &Path, len: u64) -> io::Result<()> {
        (**self).truncate(path, len)
    }

    fn atomic_replace(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        (**self).atomic_replace(path, bytes)
    }

    fn sync_data(&self, path: &Path) -> io::Result<()> {
        (**self).sync_data(path)
    }

    fn append(&self, path: &Path, bytes: &[u8]) -> io::Result<u64> {
        (**self).append(path, bytes)
    }

    fn rename_durable(&self, from: &Path, to: &Path) -> io::Result<()> {
        (**self).rename_durable(from, to)
    }
}

/// Fault injection for crash tests: forwards to `inner`, except that the k-th call
/// (counting every [`FsOps`] method, from 1) aborts the process with
/// [`std::process::abort`] instead of running, as a crash at that exact point would. It
/// counts every call ([`CrashAtFs::calls`]), and can write the count to a file when it is
/// dropped ([`CrashAtFs::reporting_calls_to`]).
pub struct CrashAtFs<F: FsOps> {
    inner: F,
    /// Calls left until the abort, the fatal one included; 0 never aborts.
    crash_at: AtomicU64,
    /// Every call so far, the fatal one included.
    calls: AtomicU64,
    /// Where to write `calls` when dropped, if anywhere.
    report: Option<PathBuf>,
}

impl CrashAtFs<RealFs> {
    /// The filesystem the test hooks ask for, if they ask for one: the real filesystem in a
    /// [`CrashAtFs`] that aborts at call k when `SWARMOPS_TEST_CRASH_AT=<k>` is set to a
    /// positive k, and reports its call count to `<file>` when `SWARMOPS_TEST_FS_CALLS=<file>`
    /// is set (either, or both). `None` when neither is set, or both are empty.
    pub fn from_env() -> Option<CrashAtFs<RealFs>> {
        let crash_at = crash_at_from_env();
        let report = std::env::var_os(FS_CALLS_ENV).filter(|file| !file.is_empty());
        if crash_at.is_none() && report.is_none() {
            return None;
        }
        let fs = CrashAtFs::new(RealFs, crash_at.unwrap_or(0));
        Some(match report {
            Some(file) => fs.reporting_calls_to(file.into()),
            None => fs,
        })
    }
}

impl<F: FsOps> CrashAtFs<F> {
    /// Wraps `inner` so that call number `k` aborts; `k == 0` never aborts.
    pub fn new(inner: F, k: u64) -> Self {
        Self {
            inner,
            crash_at: AtomicU64::new(k),
            calls: AtomicU64::new(0),
            report: None,
        }
    }

    /// This wrapper, writing how many calls it made to `file` (in decimal, replacing what is
    /// there) when it is dropped. A failure to write is not reported: the file is missing
    /// then, which the test that asked for it finds out.
    #[must_use]
    pub fn reporting_calls_to(mut self, file: PathBuf) -> Self {
        self.report = Some(file);
        self
    }

    /// How many calls it has seen.
    pub fn calls(&self) -> u64 {
        self.calls.load(Ordering::SeqCst)
    }

    /// Counts one call, and aborts if it is the k-th.
    fn tick(&self) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let before = self
            .crash_at
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            });
        if before == Ok(1) {
            std::process::abort();
        }
    }
}

impl<F: FsOps> Drop for CrashAtFs<F> {
    fn drop(&mut self) {
        if let Some(file) = &self.report {
            let _ = fs::write(file, self.calls().to_string());
        }
    }
}

impl<F: FsOps> FsOps for CrashAtFs<F> {
    fn create_dir_all(&self, p: &Path) -> io::Result<()> {
        self.tick();
        self.inner.create_dir_all(p)
    }

    fn write_durable_temp(&self, dir: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
        self.tick();
        self.inner.write_durable_temp(dir, bytes)
    }

    fn link_noclobber(&self, from: &Path, to: &Path) -> io::Result<LinkOutcome> {
        self.tick();
        self.inner.link_noclobber(from, to)
    }

    fn fsync_dir(&self, p: &Path) -> io::Result<()> {
        self.tick();
        self.inner.fsync_dir(p)
    }

    fn read(&self, p: &Path) -> io::Result<Option<Vec<u8>>> {
        self.tick();
        self.inner.read(p)
    }

    fn exists(&self, p: &Path) -> io::Result<bool> {
        self.tick();
        self.inner.exists(p)
    }

    fn remove(&self, p: &Path) -> io::Result<()> {
        self.tick();
        self.inner.remove(p)
    }

    fn append_durable(&self, path: &Path, bytes: &[u8]) -> io::Result<u64> {
        self.tick();
        self.inner.append_durable(path, bytes)
    }

    fn file_len(&self, path: &Path) -> io::Result<u64> {
        self.tick();
        self.inner.file_len(path)
    }

    fn truncate(&self, path: &Path, len: u64) -> io::Result<()> {
        self.tick();
        self.inner.truncate(path, len)
    }

    fn atomic_replace(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        self.tick();
        self.inner.atomic_replace(path, bytes)
    }

    fn sync_data(&self, path: &Path) -> io::Result<()> {
        self.tick();
        self.inner.sync_data(path)
    }

    fn append(&self, path: &Path, bytes: &[u8]) -> io::Result<u64> {
        self.tick();
        self.inner.append(path, bytes)
    }

    fn rename_durable(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.tick();
        self.inner.rename_durable(from, to)
    }
}

/// The k of `SWARMOPS_TEST_CRASH_AT=<k>`; `None` unless set to a positive integer.
fn crash_at_from_env() -> Option<u64> {
    std::env::var(CRASH_AT_ENV)
        .ok()?
        .parse()
        .ok()
        .filter(|&k| k > 0)
}
