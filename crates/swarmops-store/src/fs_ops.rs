//! The filesystem operations the object store and the metadata journal perform, behind a
//! trait so tests can count them or crash the process at an exact point of a write.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Test hook: with `SWARMOPS_TEST_CRASH_AT=<k>` set, [`crate::FsObjectStore::open_default`]
/// wraps the real filesystem in a [`CrashAtFs`] that aborts the process on its k-th call.
pub const CRASH_AT_ENV: &str = "SWARMOPS_TEST_CRASH_AT";

/// What [`FsOps::link_noclobber`] found at its target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkOutcome {
    /// The target did not exist; it now names the source file.
    Placed,
    /// The target already existed and was left untouched.
    AlreadyExists,
}

/// The filesystem operations of the object store and the metadata journal.
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
        let dir = match path.parent() {
            Some(dir) if !dir.as_os_str().is_empty() => dir,
            _ => Path::new("."),
        };
        let mut temp = tempfile::Builder::new().tempfile_in(dir)?;
        temp.write_all(bytes)?;
        temp.as_file().sync_all()?;
        // On failure the temp file is removed when the returned `NamedTempFile` drops.
        temp.persist(path).map_err(|e| e.error)?;
        self.fsync_dir(dir)
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
}

/// Fault injection for crash tests: forwards to `inner`, except that the k-th call
/// (counting every [`FsOps`] method, from 1) aborts the process with
/// [`std::process::abort`] instead of running, as a crash at that exact point would.
pub struct CrashAtFs<F: FsOps> {
    inner: F,
    /// Calls left until the abort, the fatal one included; 0 never aborts.
    crash_at: AtomicU64,
}

impl<F: FsOps> CrashAtFs<F> {
    /// Wraps `inner` so that call number `k` aborts; `k == 0` never aborts.
    pub fn new(inner: F, k: u64) -> Self {
        Self {
            inner,
            crash_at: AtomicU64::new(k),
        }
    }

    /// Counts one call, and aborts if it is the k-th.
    fn tick(&self) {
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
}

/// The k of `SWARMOPS_TEST_CRASH_AT=<k>`; `None` unless set to a positive integer.
pub(crate) fn crash_at_from_env() -> Option<u64> {
    std::env::var(CRASH_AT_ENV)
        .ok()?
        .parse()
        .ok()
        .filter(|&k| k > 0)
}
