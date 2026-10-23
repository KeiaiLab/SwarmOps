//! The filesystem operations the object store and the metadata store (journal and index)
//! perform, behind a trait so tests can count them, crash the process at an exact point of a
//! write, fail them, or check the order they come in.
//!
//! Four test hooks reach a whole process through its environment, for tests of a binary
//! (`swarmops` itself), and [`HookedFs::from_env`] reads them; the stores open on what it gives
//! by default ([`crate::FsObjectStore::open_default`], and `swarmops-repo`'s `LocalRepo::open`
//! and `create`, which share one for both stores):
//!
//! - `SWARMOPS_TEST_CRASH_AT=<k>` aborts the process at its k-th call (`CrashAtFs`);
//! - `SWARMOPS_TEST_FS_CALLS=<file>` has it write how many calls it made, so that a dry run tells
//!   a sweep where it ends (`CrashAtFs::reporting_calls_to`);
//! - `SWARMOPS_TEST_FAIL_AT=<k>[:<mode>[:<errno>]]` fails its k-th call with an injected I/O
//!   error, and with `persistent` every mutating call after it too (`FailAtFs`);
//! - `SWARMOPS_TEST_ORDER_CHECK=1` checks the store's write-ordering rules on every call, and
//!   panics, naming the rule and the call, at the first that breaks one
//!   (`OrderCheckFs`).
//!
//! A fifth, `SWARMOPS_TEST_NOW=<unix seconds>`, has `swarmops-ops` sign changesets at that time:
//! a commit then writes the same objects in every run, so call k is the same call in every run.
//!
//! The hooks act only in a build with the `test-hooks` feature (this crate's, which the
//! `test-hooks` features of `swarmops-repo`, `swarmops-ops`, `swarmops-cli` and
//! `swarmops-testkit` turn on; the test kit's is not a default): any
//! other build, a release build among them, ignores the variables, and holds no fault double
//! at all: `CrashAtFs`, `FailAtFs` and `OrderCheckFs` compile only with the feature, so a
//! reference to one outside it does not compile. The workspace's gates build the tests with it.
//! [`HookedFs`] is in every build, since the stores ask it for their filesystem: without the
//! feature it has none to give.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use camino::{Utf8Component, Utf8Path, Utf8PathBuf};
#[cfg(feature = "test-hooks")]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(feature = "test-hooks")]
use std::sync::{Mutex, PoisonError};
use tempfile::NamedTempFile;

use crate::error::{FileOp, IoFailure};

/// Opens `path` read-only if it is a regular file, a symlink to one included; anything else
/// is refused before it is opened ([`not_a_regular_file`]). No store file is a FIFO, a device
/// or a socket, and opening a FIFO would block until something opened it for writing.
pub(crate) fn open_regular(path: impl AsRef<Path>) -> io::Result<File> {
    let path = path.as_ref();
    if !fs::metadata(path)?.is_file() {
        return Err(not_a_regular_file(path));
    }
    File::open(path)
}

/// The refusal of the store file at `path`, which is not a regular file: `InvalidData`, the
/// message naming the file by its name alone (`"journal.ckpt is not a regular file"`), since the
/// rest of the path is the store's root, a host path.
pub(crate) fn not_a_regular_file(path: &Path) -> io::Error {
    let name = path
        .file_name()
        .map_or_else(|| "a store file".into(), |name| name.to_string_lossy());
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("{name} is not a regular file"),
    )
}

/// The bytes of the regular file at `path`, opened as [`open_regular`] opens it.
pub(crate) fn read_regular(path: impl AsRef<Path>) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    io::Read::read_to_end(&mut open_regular(path)?, &mut bytes)?;
    Ok(bytes)
}

/// Test hook, in a build with the `test-hooks` feature: with `SWARMOPS_TEST_CRASH_AT=<k>` set
/// to a positive k, the filesystem a store opens on by default aborts the process on its k-th
/// call (`CrashAtFs`; see [`HookedFs::from_env`]).
pub const CRASH_AT_ENV: &str = "SWARMOPS_TEST_CRASH_AT";

/// Test hook, in a build with the `test-hooks` feature: with `SWARMOPS_TEST_FS_CALLS=<file>`
/// set, the filesystem a store opens on by default writes how many calls it made to `<file>`, in
/// decimal, when it is dropped (`CrashAtFs::reporting_calls_to`): a test's dry run, to count
/// the calls of a run it then crashes or fails at each of (see [`HookedFs::from_env`]). A run
/// that crashes writes nothing.
pub const FS_CALLS_ENV: &str = "SWARMOPS_TEST_FS_CALLS";

/// Test hook, in a build with the `test-hooks` feature: with
/// `SWARMOPS_TEST_FAIL_AT=<k>[:<mode>[:<errno>]]` set, k positive, the filesystem a store opens
/// on by default fails its k-th call with an injected I/O error (a `FailAtFs`; see
/// [`HookedFs::from_env`]), counting calls as `SWARMOPS_TEST_CRASH_AT` does. `<mode>` is `once`
/// (the default: that call alone) or `persistent` (that call and every mutating call after it);
/// `<errno>` is `eio` (the default) or `enospc`. Any other value is a mistake in the test, and
/// the store's opening panics on it. A failure during recovery (`during_recovery`) is no mode
/// but two runs: `SWARMOPS_TEST_CRASH_AT=<k1>` in one, then `SWARMOPS_TEST_FAIL_AT=<k2>` in the
/// next, which fails the k2-th call, counted from that run's first, of the open and write that
/// recover from the crash.
pub const FAIL_AT_ENV: &str = "SWARMOPS_TEST_FAIL_AT";

/// Test hook, in a build with the `test-hooks` feature: with `SWARMOPS_TEST_ORDER_CHECK=1` set,
/// the filesystem a store opens on by default checks the store's write-ordering rules on every
/// call (an `OrderCheckFs`; see [`HookedFs::from_env`]) and panics at the first call that
/// breaks one. Any other value leaves the check off.
pub const ORDER_CHECK_ENV: &str = "SWARMOPS_TEST_ORDER_CHECK";

/// What [`FsOps::link_noclobber`] found at its target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LinkOutcome {
    /// The target did not exist; it now names the source file.
    Placed,
    /// The target already existed and was left untouched.
    AlreadyExists,
}

/// Declares [`FsMethod`] from one list of the methods, each with its documentation and its
/// name: [`FsMethod::ALL`] and [`FsMethod::as_str`] come from the same list, so that a method
/// cannot be left out of either.
macro_rules! fs_methods {
    ($($(#[$attr:meta])* $method:ident = $name:literal,)+) => {
        /// One method of [`FsOps`], by name: the seam's vocabulary, which every double classifies
        /// calls with ([`FsMethod::is_mutating`], [`FsMethod::flushes`]) so that one list names
        /// the 14 methods. It compares equal to the method's name ([`FsMethod::as_str`]), so a
        /// test's `call.method == "append"` reads as before.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        #[non_exhaustive]
        pub enum FsMethod {
            $($(#[$attr])* $method,)+
        }

        impl FsMethod {
            /// Every method, in the order the trait declares them.
            pub const ALL: [FsMethod; [$($name),+].len()] = [$(FsMethod::$method),+];

            /// The method's name, as the trait spells it: `"append_durable"`, say.
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(FsMethod::$method => $name,)+
                }
            }
        }
    };
}

fs_methods! {
    /// [`FsOps::create_dir_all`].
    CreateDirAll = "create_dir_all",
    /// [`FsOps::write_durable_temp`].
    WriteDurableTemp = "write_durable_temp",
    /// [`FsOps::link_noclobber`].
    LinkNoclobber = "link_noclobber",
    /// [`FsOps::fsync_dir`].
    FsyncDir = "fsync_dir",
    /// [`FsOps::read`].
    Read = "read",
    /// [`FsOps::exists`].
    Exists = "exists",
    /// [`FsOps::remove`].
    Remove = "remove",
    /// [`FsOps::append_durable`].
    AppendDurable = "append_durable",
    /// [`FsOps::file_len`].
    FileLen = "file_len",
    /// [`FsOps::truncate`].
    Truncate = "truncate",
    /// [`FsOps::atomic_replace`].
    AtomicReplace = "atomic_replace",
    /// [`FsOps::sync_data`].
    SyncData = "sync_data",
    /// [`FsOps::append`].
    Append = "append",
    /// [`FsOps::rename_durable`].
    RenameDurable = "rename_durable",
}

impl FsMethod {
    /// Whether the method may change a file or a directory: every one but the three reads,
    /// `read`, `exists` and `file_len`.
    pub const fn is_mutating(self) -> bool {
        !matches!(self, FsMethod::Read | FsMethod::Exists | FsMethod::FileLen)
    }

    /// Whether the method flushes to stable storage, as [`FsOps`] documents it:
    /// `append_durable`, `truncate`, `sync_data`, `fsync_dir`, `atomic_replace`,
    /// `write_durable_temp` and `rename_durable`.
    pub const fn flushes(self) -> bool {
        matches!(
            self,
            FsMethod::AppendDurable
                | FsMethod::Truncate
                | FsMethod::SyncData
                | FsMethod::FsyncDir
                | FsMethod::AtomicReplace
                | FsMethod::WriteDurableTemp
                | FsMethod::RenameDurable
        )
    }
}

/// The method's name ([`FsMethod::as_str`]).
impl fmt::Display for FsMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A method is equal to its name ([`FsMethod::as_str`]).
impl PartialEq<&str> for FsMethod {
    fn eq(&self, name: &&str) -> bool {
        self.as_str() == *name
    }
}

#[cfg(test)]
mod fs_method_tests {
    use super::FsMethod;

    /// The method names, as the trait declares them, and the two classes the fault doubles
    /// sorted them into with string lists before `FsMethod` (`FailAtFs::call`): the calls that
    /// change nothing, and those that flush to stable storage.
    #[test]
    fn fs_method_classifies_as_the_doubles_did() {
        let names: Vec<&str> = FsMethod::ALL.iter().map(|method| method.as_str()).collect();
        assert_eq!(
            names,
            [
                "create_dir_all",
                "write_durable_temp",
                "link_noclobber",
                "fsync_dir",
                "read",
                "exists",
                "remove",
                "append_durable",
                "file_len",
                "truncate",
                "atomic_replace",
                "sync_data",
                "append",
                "rename_durable",
            ]
        );
        let mut distinct = names.clone();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(distinct.len(), 14, "14 distinct names");
        let reading: Vec<&str> = (FsMethod::ALL.iter())
            .filter(|method| !method.is_mutating())
            .map(|method| method.as_str())
            .collect();
        assert_eq!(reading, ["read", "exists", "file_len"]);
        let flushing: Vec<&str> = (FsMethod::ALL.iter())
            .filter(|method| method.flushes())
            .map(|method| method.as_str())
            .collect();
        assert_eq!(
            flushing,
            [
                "write_durable_temp",
                "fsync_dir",
                "append_durable",
                "truncate",
                "atomic_replace",
                "sync_data",
                "rename_durable",
            ]
        );
        for method in FsMethod::ALL {
            assert!(method == method.as_str(), "{method} is its name");
            assert_eq!(method.to_string(), method.as_str());
        }
        assert!(FsMethod::AppendDurable != "append");
    }
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

    /// The whole content of file `p`, or `None` if it does not exist. A `p` that is not a
    /// regular file (a FIFO, a device, a socket, a directory) is `InvalidData`, refused before
    /// it is opened.
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
#[non_exhaustive]
pub struct RealFs;

impl RealFs {
    /// The real filesystem.
    pub const fn new() -> RealFs {
        RealFs
    }
}

impl FsOps for RealFs {
    fn create_dir_all(&self, p: &Path) -> io::Result<()> {
        fs::create_dir_all(p)
    }

    fn write_durable_temp(&self, dir: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
        let mut temp = temp_file_in(dir)?;
        temp.as_file_mut().write_all(bytes)?;
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
        match read_regular(p) {
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
        let mut temp = temp_file_in(dir)?;
        temp.as_file_mut().write_all(bytes)?;
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

/// A new temp file in `dir`, named and made as tempfile names and makes one (`.tmp` and six
/// random characters, created exclusively, mode 0600, removed when dropped unless kept or
/// persisted), but opened by the caller's own `OpenOptions`: tempfile's own constructors, and
/// writes through its `NamedTempFile`, wrap a failure with the temp file's absolute path and
/// drop its errno, where this returns the operating system's error as it is. (Writes go to
/// [`NamedTempFile::as_file_mut`], the file itself, for the same reason.)
fn temp_file_in(dir: &Path) -> io::Result<NamedTempFile> {
    tempfile::Builder::new().make_in(dir, |path| {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
    })
}

/// The directory that holds `path`; `.` for a bare file name.
fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    }
}

/// The store's view of an [`FsOps`]: each call forwarded exactly once and as it is (so the call
/// counts, the injected failures and the order checker see what they saw before), its error
/// located as an [`IoFailure`] that names the method and the path relative to the store's root:
/// `link_noclobber` and `rename_durable` name their target, `write_durable_temp` its directory.
/// Nothing is computed on success. [`StoreFs::locate`] does the same for the calls the store
/// makes on its own handles and paths, off the seam (storage spec §8).
///
/// A store makes one on its real root, and its handles keep clones of it (two reference counts
/// each): a create's staged files are named under `meta.tmp/`, where they are.
#[derive(Clone)]
#[non_exhaustive]
pub struct StoreFs {
    fs: Arc<dyn FsOps>,
    root: Arc<Utf8Path>,
}

impl StoreFs {
    /// `fs`, its failures located relative to the store rooted at `root`.
    pub fn new(fs: Arc<dyn FsOps>, root: &Utf8Path) -> StoreFs {
        StoreFs {
            fs,
            root: Arc::from(root),
        }
    }

    /// The operations it forwards to.
    pub fn ops(&self) -> &Arc<dyn FsOps> {
        &self.fs
    }

    /// The store's root, which it names paths relative to.
    pub fn root(&self) -> &Utf8Path {
        &self.root
    }

    /// The failure `source` of `op` on `path`, located: `path` relative to the store's root
    /// ([`store_relative`]). For the store's own calls, off the seam; the forwarded calls locate
    /// theirs.
    pub fn locate(&self, op: FileOp, path: impl AsRef<Path>, source: io::Error) -> IoFailure {
        IoFailure::new(op, store_relative(&self.root, path.as_ref()), source)
    }

    /// The failure `source` of the forwarded call `method` on `path`, located.
    fn failed(&self, method: FsMethod, path: &Path, source: io::Error) -> IoFailure {
        self.locate(FileOp::Fs(method), path, source)
    }

    /// [`FsOps::create_dir_all`].
    pub fn create_dir_all(&self, p: impl AsRef<Path>) -> Result<(), IoFailure> {
        let p = p.as_ref();
        (self.fs.create_dir_all(p)).map_err(|e| self.failed(FsMethod::CreateDirAll, p, e))
    }

    /// [`FsOps::write_durable_temp`]; a failure names `dir`.
    pub fn write_durable_temp(
        &self,
        dir: impl AsRef<Path>,
        bytes: &[u8],
    ) -> Result<PathBuf, IoFailure> {
        let dir = dir.as_ref();
        (self.fs.write_durable_temp(dir, bytes))
            .map_err(|e| self.failed(FsMethod::WriteDurableTemp, dir, e))
    }

    /// [`FsOps::link_noclobber`]; a failure names `to`.
    pub fn link_noclobber(
        &self,
        from: impl AsRef<Path>,
        to: impl AsRef<Path>,
    ) -> Result<LinkOutcome, IoFailure> {
        let to = to.as_ref();
        (self.fs.link_noclobber(from.as_ref(), to))
            .map_err(|e| self.failed(FsMethod::LinkNoclobber, to, e))
    }

    /// [`FsOps::fsync_dir`].
    pub fn fsync_dir(&self, p: impl AsRef<Path>) -> Result<(), IoFailure> {
        let p = p.as_ref();
        (self.fs.fsync_dir(p)).map_err(|e| self.failed(FsMethod::FsyncDir, p, e))
    }

    /// [`FsOps::read`].
    pub fn read(&self, p: impl AsRef<Path>) -> Result<Option<Vec<u8>>, IoFailure> {
        let p = p.as_ref();
        (self.fs.read(p)).map_err(|e| self.failed(FsMethod::Read, p, e))
    }

    /// [`FsOps::exists`].
    pub fn exists(&self, p: impl AsRef<Path>) -> Result<bool, IoFailure> {
        let p = p.as_ref();
        (self.fs.exists(p)).map_err(|e| self.failed(FsMethod::Exists, p, e))
    }

    /// [`FsOps::remove`].
    pub fn remove(&self, p: impl AsRef<Path>) -> Result<(), IoFailure> {
        let p = p.as_ref();
        (self.fs.remove(p)).map_err(|e| self.failed(FsMethod::Remove, p, e))
    }

    /// [`FsOps::append_durable`].
    pub fn append_durable(&self, path: impl AsRef<Path>, bytes: &[u8]) -> Result<u64, IoFailure> {
        let path = path.as_ref();
        (self.fs.append_durable(path, bytes))
            .map_err(|e| self.failed(FsMethod::AppendDurable, path, e))
    }

    /// [`FsOps::file_len`].
    pub fn file_len(&self, path: impl AsRef<Path>) -> Result<u64, IoFailure> {
        let path = path.as_ref();
        (self.fs.file_len(path)).map_err(|e| self.failed(FsMethod::FileLen, path, e))
    }

    /// [`FsOps::truncate`].
    pub fn truncate(&self, path: impl AsRef<Path>, len: u64) -> Result<(), IoFailure> {
        let path = path.as_ref();
        (self.fs.truncate(path, len)).map_err(|e| self.failed(FsMethod::Truncate, path, e))
    }

    /// [`FsOps::atomic_replace`].
    pub fn atomic_replace(&self, path: impl AsRef<Path>, bytes: &[u8]) -> Result<(), IoFailure> {
        let path = path.as_ref();
        (self.fs.atomic_replace(path, bytes))
            .map_err(|e| self.failed(FsMethod::AtomicReplace, path, e))
    }

    /// [`FsOps::sync_data`].
    pub fn sync_data(&self, path: impl AsRef<Path>) -> Result<(), IoFailure> {
        let path = path.as_ref();
        (self.fs.sync_data(path)).map_err(|e| self.failed(FsMethod::SyncData, path, e))
    }

    /// [`FsOps::append`].
    pub fn append(&self, path: impl AsRef<Path>, bytes: &[u8]) -> Result<u64, IoFailure> {
        let path = path.as_ref();
        (self.fs.append(path, bytes)).map_err(|e| self.failed(FsMethod::Append, path, e))
    }

    /// [`FsOps::rename_durable`]; a failure names `to`.
    pub fn rename_durable(
        &self,
        from: impl AsRef<Path>,
        to: impl AsRef<Path>,
    ) -> Result<(), IoFailure> {
        let to = to.as_ref();
        (self.fs.rename_durable(from.as_ref(), to))
            .map_err(|e| self.failed(FsMethod::RenameDurable, to, e))
    }
}

impl fmt::Debug for StoreFs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoreFs")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

/// `path` relative to `root`, worked out from their text alone, with no system call: `.` for the
/// root itself; `None` for a path outside the root, one that leaves it with a `..`, or one that
/// is not UTF-8. A `.` in it is passed over.
pub fn store_relative(root: &Utf8Path, path: &Path) -> Option<Utf8PathBuf> {
    let rest = Utf8Path::from_path(path)?.strip_prefix(root).ok()?;
    let mut relative = Utf8PathBuf::new();
    for part in rest.components() {
        match part {
            Utf8Component::Normal(name) => relative.push(name),
            Utf8Component::CurDir => {}
            Utf8Component::ParentDir | Utf8Component::RootDir | Utf8Component::Prefix(_) => {
                return None;
            }
        }
    }
    if relative.as_str().is_empty() {
        relative.push(".");
    }
    Some(relative)
}

/// One set of operations shared by several stores, so that they share one `CrashAtFs` call
/// count.
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

/// Operations chosen at run time: the test hooks stack their wrappers so.
impl<F: FsOps + ?Sized> FsOps for Box<F> {
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

/// Fault injection for crash tests (feature `test-hooks`): forwards to `inner`, except that the
/// k-th call (counting every [`FsOps`] method, from 1) aborts the process with
/// [`std::process::abort`] instead of running, as a crash at that exact point would. It
/// counts every call ([`CrashAtFs::calls`]), and can write the count to a file when it is
/// dropped ([`CrashAtFs::reporting_calls_to`]).
#[cfg(feature = "test-hooks")]
#[non_exhaustive]
pub struct CrashAtFs<F: FsOps> {
    inner: F,
    /// Calls left until the abort, the fatal one included; 0 never aborts.
    crash_at: AtomicU64,
    /// Every call so far, the fatal one included.
    calls: AtomicU64,
    /// Where to write `calls` when dropped, if anywhere.
    report: Option<PathBuf>,
}

/// The filesystem the test hooks ask a store to open on: see [`HookedFs::from_env`].
#[non_exhaustive]
pub struct HookedFs {
    fs: Box<dyn FsOps>,
    counts_calls: bool,
}

impl HookedFs {
    /// The filesystem the test hooks ask for, if they ask for one, in a build with the
    /// `test-hooks` feature: the real filesystem, wrapped from the inside out in
    ///
    /// 1. a `FailAtFs` when `SWARMOPS_TEST_FAIL_AT=<k>[:<mode>[:<errno>]]` is set (see
    ///    [`FAIL_AT_ENV`]); a malformed value panics, as a mistake in the test;
    /// 2. a `CrashAtFs` when `SWARMOPS_TEST_CRASH_AT=<k>` is set to a positive k, or
    ///    `SWARMOPS_TEST_FS_CALLS=<file>` is set: it aborts at call k, and reports its call
    ///    count to `<file>`. Being outside the `FailAtFs`, it counts the calls that fails too;
    /// 3. an `OrderCheckFs` when `SWARMOPS_TEST_ORDER_CHECK=1` is set: outermost, so
    ///    that it sees every call, the injected failures included, and judges them as real ones.
    ///
    /// `None` when none of them is set (or each is empty), and always `None` in a build without
    /// the `test-hooks` feature, which ignores the variables.
    ///
    /// The stores open on it by default: [`crate::FsObjectStore::open_default`], and
    /// `swarmops-repo`'s `LocalRepo::open` and `create`, which hand the same filesystem to
    /// `MetaStore::create_with_fs` or `open_with_fs` too, so that the two stores count their
    /// calls together; on the real filesystem when it is `None`. With a hook that counts calls
    /// ([`HookedFs::counts_calls`]) both stores run at parallelism 1, the object writes, the
    /// flush's directory fsyncs and the index syncs one at a time, and with `SWARMOPS_TEST_NOW`
    /// fixing the time `swarmops-ops` signs at, call k is the same call in every run.
    ///
    /// The gates (`just check`, `scripts/check.sh` and `just check-slow`) build the tests with
    /// `--all-features`, the hooks with them. A test that needs one is marked
    /// `#[cfg(feature = "test-hooks")]` (the CLI marks its `crash` and `faults` modules so in
    /// its `tests/all.rs`), so that a plain `cargo test` skips it, and `just check` runs
    /// `swarmops-cli`'s `test_hooks` in a release build without the feature, which ignores the
    /// variables and holds none of the doubles (CONTRIBUTING.md, "Tests").
    pub fn from_env() -> Option<HookedFs> {
        hooked_fs()
    }

    /// Whether one of the hooks counts calls (`SWARMOPS_TEST_CRASH_AT`,
    /// `SWARMOPS_TEST_FS_CALLS`, `SWARMOPS_TEST_FAIL_AT`): a store must then make its calls one
    /// at a time (parallelism 1), so that call k is the same call in every run. The order check
    /// alone counts nothing, and leaves the parallelism as it is.
    pub fn counts_calls(&self) -> bool {
        self.counts_calls
    }

    /// The filesystem.
    pub fn into_fs(self) -> Box<dyn FsOps> {
        self.fs
    }
}

/// [`HookedFs::from_env`] in a build with the `test-hooks` feature.
#[cfg(feature = "test-hooks")]
fn hooked_fs() -> Option<HookedFs> {
    let crash_at = crash_at_from_env();
    let report = std::env::var_os(FS_CALLS_ENV).filter(|file| !file.is_empty());
    let fail_at = std::env::var(FAIL_AT_ENV).ok().and_then(|spec| {
        FailAt::parse(&spec).unwrap_or_else(|e| panic!("{FAIL_AT_ENV}={spec:?}: {e}"))
    });
    let order_check = std::env::var_os(ORDER_CHECK_ENV).is_some_and(|value| value == "1");
    let counts_calls = crash_at.is_some() || report.is_some() || fail_at.is_some();
    if !counts_calls && !order_check {
        return None;
    }
    let mut fs: Box<dyn FsOps> = Box::new(RealFs);
    if let Some(fail) = fail_at {
        fs = Box::new(FailAtFs::new(fs, fail));
    }
    if crash_at.is_some() || report.is_some() {
        let crash = CrashAtFs::new(fs, crash_at.unwrap_or(0));
        fs = Box::new(match report {
            Some(file) => crash.reporting_calls_to(file.into()),
            None => crash,
        });
    }
    if order_check {
        fs = Box::new(crate::OrderCheckFs::new(fs));
    }
    Some(HookedFs { fs, counts_calls })
}

/// [`HookedFs::from_env`] in a build without the `test-hooks` feature: the hooks' variables
/// are ignored.
#[cfg(not(feature = "test-hooks"))]
fn hooked_fs() -> Option<HookedFs> {
    None
}

#[cfg(feature = "test-hooks")]
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

#[cfg(feature = "test-hooks")]
impl<F: FsOps> Drop for CrashAtFs<F> {
    fn drop(&mut self) {
        if let Some(file) = &self.report {
            let _ = fs::write(file, self.calls().to_string());
        }
    }
}

#[cfg(feature = "test-hooks")]
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
#[cfg(feature = "test-hooks")]
fn crash_at_from_env() -> Option<u64> {
    std::env::var(CRASH_AT_ENV)
        .ok()?
        .parse()
        .ok()
        .filter(|&k| k > 0)
}

/// What [`FailAtFs`] fails, as `SWARMOPS_TEST_FAIL_AT=<k>[:<mode>[:<errno>]]` says it.
#[cfg(feature = "test-hooks")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct FailAt {
    /// The call that fails (or fails first), counting every [`FsOps`] call from 1.
    pub at: u64,
    /// Whether the calls after it fail too.
    pub mode: FailMode,
    /// The error the failing calls return.
    pub error: InjectedError,
}

/// Which calls a [`FailAtFs`] fails.
#[cfg(feature = "test-hooks")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FailMode {
    /// Call k, and no other: a transient error (`once`).
    Once,
    /// Call k, whatever it is, and every mutating call after it: a device gone bad
    /// (`persistent`). Reads (`read`, `exists`, `file_len`) after call k still succeed, as
    /// reads from the page cache do.
    Persistent,
}

/// The error a [`FailAtFs`] injects, and where in the call it strikes.
#[cfg(feature = "test-hooks")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum InjectedError {
    /// `EIO` (`eio`). A call that flushes to stable storage (`append_durable`, `truncate`,
    /// `sync_data`, `fsync_dir`, `atomic_replace`, `write_durable_temp`, `rename_durable`) runs,
    /// and then fails, as when the writeback its flush waits for fails: whatever it wrote stays
    /// written (in the page cache, as far as the caller can tell), and the caller is told it
    /// failed. Any other call fails before it does anything.
    Eio,
    /// `ENOSPC` (`enospc`): the call fails before it does anything, as a write the filesystem
    /// has no room for does.
    Enospc,
}

#[cfg(feature = "test-hooks")]
impl InjectedError {
    /// The OS error, as the kernel reports it (Linux's errno values).
    pub fn os_error(self) -> io::Error {
        io::Error::from_raw_os_error(match self {
            InjectedError::Eio => 5,
            InjectedError::Enospc => 28,
        })
    }
}

#[cfg(feature = "test-hooks")]
impl FailAt {
    /// Fail call `at`, as `mode` says, with `error`.
    pub fn new(at: u64, mode: FailMode, error: InjectedError) -> FailAt {
        FailAt { at, mode, error }
    }

    /// The failure `spec` names: `<k>[:<mode>[:<errno>]]`, k positive, `<mode>` `once` (the
    /// default) or `persistent`, `<errno>` `eio` (the default) or `enospc`, each of either case.
    /// `Ok(None)` for an empty spec or k = 0, which fail nothing; an error for anything else.
    pub fn parse(spec: &str) -> Result<Option<FailAt>, String> {
        let mut parts = spec.split(':');
        let at = parts.next().unwrap_or_default();
        if at.is_empty() {
            return Ok(None);
        }
        let at: u64 = at
            .parse()
            .map_err(|_| format!("{at:?} is not a call number"))?;
        let mode = match parts.next().map(str::to_ascii_lowercase).as_deref() {
            None | Some("once") => FailMode::Once,
            Some("persistent") => FailMode::Persistent,
            Some(other) => return Err(format!("{other:?} is not a mode: once or persistent")),
        };
        let error = match parts.next().map(str::to_ascii_lowercase).as_deref() {
            None | Some("eio") => InjectedError::Eio,
            Some("enospc") => InjectedError::Enospc,
            Some(other) => return Err(format!("{other:?} is not an errno: eio or enospc")),
        };
        if let Some(extra) = parts.next() {
            return Err(format!(
                "{extra:?} follows the errno: <k>[:<mode>[:<errno>]]"
            ));
        }
        Ok((at > 0).then_some(FailAt { at, mode, error }))
    }
}

/// Fault injection for error tests: forwards to `inner`, except that the calls [`FailAt`]
/// names fail with an injected I/O error ([`InjectedError`] says where in the call): call k
/// (counting every [`FsOps`] method, from 1, as [`CrashAtFs`] counts them), and with
/// [`FailMode::Persistent`] every mutating call after it too. It counts every call
/// ([`FailAtFs::calls`]) and every failure ([`FailAtFs::failures`]), and keeps which calls it
/// failed ([`FailAtFs::failed_calls`]).
#[cfg(feature = "test-hooks")]
#[non_exhaustive]
pub struct FailAtFs<F: FsOps> {
    inner: F,
    fail: FailAt,
    /// Every call so far, the failed ones included.
    calls: AtomicU64,
    /// The calls failed so far.
    failures: AtomicU64,
    /// The calls failed so far, in order: each one's method and the path it acted on.
    failed: Mutex<Vec<(FsMethod, PathBuf)>>,
}

/// Where a [`FailAtFs`] fails a call.
#[cfg(feature = "test-hooks")]
#[derive(Clone, Copy)]
enum Strike {
    /// The call does nothing, and fails.
    Before,
    /// The call runs, and then fails (unless it failed on its own).
    After,
}

#[cfg(feature = "test-hooks")]
impl<F: FsOps> FailAtFs<F> {
    /// Wraps `inner` so that the calls `fail` names fail.
    pub fn new(inner: F, fail: FailAt) -> Self {
        Self {
            inner,
            fail,
            calls: AtomicU64::new(0),
            failures: AtomicU64::new(0),
            failed: Mutex::new(Vec::new()),
        }
    }

    /// How many calls it has seen.
    pub fn calls(&self) -> u64 {
        self.calls.load(Ordering::SeqCst)
    }

    /// How many calls it has failed.
    pub fn failures(&self) -> u64 {
        self.failures.load(Ordering::SeqCst)
    }

    /// The calls it has failed, in order: each one's method and the path it acted on, as
    /// [`StoreFs`] names a failure (a link's and a rename's target, a temp file's directory).
    pub fn failed_calls(&self) -> Vec<(FsMethod, PathBuf)> {
        self.failed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Counts one call of `method` on `path`, and runs it as [`FailAt`] says: `call` runs unless
    /// the call fails before it, and an injected error replaces its result if the call fails.
    fn call<T>(
        &self,
        method: FsMethod,
        path: &Path,
        call: impl FnOnce() -> io::Result<T>,
    ) -> io::Result<T> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        let fails = n == self.fail.at
            || (self.fail.mode == FailMode::Persistent && n > self.fail.at && method.is_mutating());
        if !fails {
            return call();
        }
        self.failures.fetch_add(1, Ordering::SeqCst);
        (self.failed.lock().unwrap_or_else(PoisonError::into_inner))
            .push((method, path.to_path_buf()));
        let strike = match self.fail.error {
            InjectedError::Eio if method.flushes() => Strike::After,
            InjectedError::Eio | InjectedError::Enospc => Strike::Before,
        };
        match strike {
            Strike::Before => Err(self.fail.error.os_error()),
            Strike::After => {
                call()?;
                Err(self.fail.error.os_error())
            }
        }
    }
}

#[cfg(feature = "test-hooks")]
impl<F: FsOps> FsOps for FailAtFs<F> {
    fn create_dir_all(&self, p: &Path) -> io::Result<()> {
        self.call(FsMethod::CreateDirAll, p, || self.inner.create_dir_all(p))
    }

    fn write_durable_temp(&self, dir: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
        self.call(FsMethod::WriteDurableTemp, dir, || {
            self.inner.write_durable_temp(dir, bytes)
        })
    }

    fn link_noclobber(&self, from: &Path, to: &Path) -> io::Result<LinkOutcome> {
        self.call(FsMethod::LinkNoclobber, to, || {
            self.inner.link_noclobber(from, to)
        })
    }

    fn fsync_dir(&self, p: &Path) -> io::Result<()> {
        self.call(FsMethod::FsyncDir, p, || self.inner.fsync_dir(p))
    }

    fn read(&self, p: &Path) -> io::Result<Option<Vec<u8>>> {
        self.call(FsMethod::Read, p, || self.inner.read(p))
    }

    fn exists(&self, p: &Path) -> io::Result<bool> {
        self.call(FsMethod::Exists, p, || self.inner.exists(p))
    }

    fn remove(&self, p: &Path) -> io::Result<()> {
        self.call(FsMethod::Remove, p, || self.inner.remove(p))
    }

    fn append_durable(&self, path: &Path, bytes: &[u8]) -> io::Result<u64> {
        self.call(FsMethod::AppendDurable, path, || {
            self.inner.append_durable(path, bytes)
        })
    }

    fn file_len(&self, path: &Path) -> io::Result<u64> {
        self.call(FsMethod::FileLen, path, || self.inner.file_len(path))
    }

    fn truncate(&self, path: &Path, len: u64) -> io::Result<()> {
        self.call(FsMethod::Truncate, path, || self.inner.truncate(path, len))
    }

    fn atomic_replace(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        self.call(FsMethod::AtomicReplace, path, || {
            self.inner.atomic_replace(path, bytes)
        })
    }

    fn sync_data(&self, path: &Path) -> io::Result<()> {
        self.call(FsMethod::SyncData, path, || self.inner.sync_data(path))
    }

    fn append(&self, path: &Path, bytes: &[u8]) -> io::Result<u64> {
        self.call(FsMethod::Append, path, || self.inner.append(path, bytes))
    }

    fn rename_durable(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.call(FsMethod::RenameDurable, to, || {
            self.inner.rename_durable(from, to)
        })
    }
}

#[cfg(all(test, feature = "test-hooks"))]
mod tests {
    use super::*;

    /// A store file is a regular file: a device or a directory where one is expected is refused
    /// before it is opened (a FIFO would block the open), a symlink to a regular file is
    /// followed, and a missing file is still `NotFound`.
    #[test]
    #[cfg(unix)]
    fn only_regular_files_are_opened() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        fs::write(&file, b"bytes").unwrap();
        assert_eq!(read_regular(&file).unwrap(), b"bytes");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert_eq!(read_regular(&link).unwrap(), b"bytes");
        let null = dir.path().join("null");
        std::os::unix::fs::symlink("/dev/null", &null).unwrap();
        for (path, what) in [
            (null, "a symlink to a device"),
            (dir.path().to_path_buf(), "a directory"),
        ] {
            let e = open_regular(&path).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{what}: {e}");
            assert!(
                e.to_string().ends_with("is not a regular file"),
                "{what}: {e}"
            );
        }
        let e = read_regular(dir.path().join("missing")).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::NotFound, "{e}");
    }

    /// A temp file that `RealFs` cannot create, for an object write or a replacement, fails
    /// with the operating system's own error: its errno kept, and no path in its text (a
    /// store's temp files lie under its root, a host path). Missing directories make the
    /// failure, so that it fails as root too. Where the directory is there, the temp file is
    /// made as tempfile makes one: a fresh `.tmp` name, mode 0600, the bytes in it.
    #[test]
    fn real_fs_temp_files_fail_with_the_os_own_error() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let shown = dir.path().to_str().unwrap();
        let missing = dir.path().join("missing");
        let e = RealFs.write_durable_temp(&missing, b"bytes").unwrap_err();
        assert_eq!(e.raw_os_error(), Some(2), "write_durable_temp: {e}");
        assert!(!e.to_string().contains(shown), "write_durable_temp: {e}");
        let e = RealFs
            .atomic_replace(&missing.join("f"), b"bytes")
            .unwrap_err();
        assert_eq!(e.raw_os_error(), Some(2), "atomic_replace: {e}");
        assert!(!e.to_string().contains(shown), "atomic_replace: {e}");

        let temp = RealFs.write_durable_temp(dir.path(), b"bytes").unwrap();
        assert_eq!(temp.parent(), Some(dir.path()));
        let name = temp.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with(".tmp"), "{name}");
        assert_eq!(fs::read(&temp).unwrap(), b"bytes");
        let mode = fs::metadata(&temp).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "{mode:o}");
        let file = dir.path().join("f");
        RealFs.atomic_replace(&file, b"one").unwrap();
        RealFs.atomic_replace(&file, b"two").unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"two");
        let left: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(
            left.len(),
            2,
            "the replacements leave no temp file: {left:?}"
        );
    }

    #[test]
    fn fail_at_specs_parse_as_documented() {
        let fail = |at, mode, error| Ok(Some(FailAt { at, mode, error }));
        assert_eq!(
            FailAt::parse("7"),
            fail(7, FailMode::Once, InjectedError::Eio)
        );
        assert_eq!(
            FailAt::parse("7:persistent"),
            fail(7, FailMode::Persistent, InjectedError::Eio)
        );
        assert_eq!(
            FailAt::parse("7:once:ENOSPC"),
            fail(7, FailMode::Once, InjectedError::Enospc)
        );
        assert_eq!(FailAt::parse(""), Ok(None));
        assert_eq!(FailAt::parse("0:persistent"), Ok(None));
        for bad in ["x", "-1", "3:sometimes", "3:once:ebusy", "3:once:eio:more"] {
            assert!(FailAt::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn injected_errors_are_the_kernels() {
        assert_eq!(InjectedError::Eio.os_error().raw_os_error(), Some(5));
        let full = InjectedError::Enospc.os_error();
        assert_eq!(
            (full.raw_os_error(), full.kind()),
            (Some(28), io::ErrorKind::StorageFull)
        );
    }

    /// An `EIO` lets a flushing call do its work, then fails it; a call that does not flush, and
    /// any call under `ENOSPC`, fails before it does anything. `once` fails call k alone,
    /// `persistent` every mutating call from k on, reads aside.
    #[test]
    fn calls_fail_where_the_injected_error_strikes() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        let failing = |at, mode, error| FailAtFs::new(RealFs, FailAt { at, mode, error });

        let eio = failing(2, FailMode::Once, InjectedError::Eio);
        eio.append_durable(&file, b"ab").unwrap();
        let err = eio.append_durable(&file, b"cd").unwrap_err();
        assert_eq!(err.raw_os_error(), Some(5));
        assert_eq!(fs::read(&file).unwrap(), b"abcd", "the work was done");
        eio.append(&file, b"ef").unwrap();
        assert_eq!((eio.calls(), eio.failures()), (3, 1));

        let full = failing(1, FailMode::Persistent, InjectedError::Enospc);
        let err = full.append_durable(&file, b"gh").unwrap_err();
        assert_eq!(err.raw_os_error(), Some(28));
        assert_eq!(fs::read(&file).unwrap(), b"abcdef", "nothing was done");
        assert_eq!(full.file_len(&file).unwrap(), 6, "reads still succeed");
        assert!(
            full.append(&file, b"gh").is_err(),
            "every mutating call fails"
        );
        assert_eq!((full.calls(), full.failures()), (3, 2));

        let eio = failing(1, FailMode::Once, InjectedError::Eio);
        assert_eq!(
            eio.append(&file, b"ij").unwrap_err().raw_os_error(),
            Some(5)
        );
        assert_eq!(fs::read(&file).unwrap(), b"abcdef", "append does not flush");
    }

    /// `StoreFs` forwards each call once, as it is (a counter around the failing filesystem
    /// counts one call per call), and locates the failure: the method, and the path relative to
    /// the store's root (a link's and a rename's target, a temp file's directory; `.` for the
    /// root, nothing for a path outside it), with the injected errno. What it names is what
    /// `FailAtFs` records it failed. Locating a failure of the store's own makes no call.
    #[test]
    fn store_fs_forwards_each_call_once_and_locates_its_failure() {
        use crate::error::FileOp;

        let dir = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(dir.path()).unwrap();
        fs::create_dir_all(root.join("meta")).unwrap();
        fs::create_dir_all(root.join("objects/tmp")).unwrap();
        fs::write(root.join("meta/journal.off"), b"").unwrap();
        fs::write(root.join("meta/b.tmp"), b"").unwrap();
        // Every mutating call fails with EIO: before it runs, or, for one that flushes, after.
        let fail = FailAt {
            at: 1,
            mode: FailMode::Persistent,
            error: InjectedError::Eio,
        };
        let failing = Arc::new(FailAtFs::new(RealFs, fail));
        let counter = Arc::new(CrashAtFs::new(Arc::clone(&failing), 0));
        let store_fs = StoreFs::new(Arc::clone(&counter) as Arc<dyn FsOps>, root);
        let object = root.join("objects/ab").join("cd".repeat(31));
        let named =
            |failure: &IoFailure| (failure.op, failure.file.clone(), failure.raw_os_error());
        let failures = [
            store_fs
                .sync_data(root.join("meta/journal.off"))
                .unwrap_err(),
            (store_fs.link_noclobber(root.join("objects/tmp/.tmp1"), &object)).unwrap_err(),
            (store_fs.write_durable_temp(root.join("objects/tmp"), b"x")).unwrap_err(),
            (store_fs.rename_durable(root.join("meta/b.tmp"), root.join("meta/b"))).unwrap_err(),
            store_fs.create_dir_all(root).unwrap_err(),
            store_fs
                .fsync_dir(dir.path().parent().unwrap())
                .unwrap_err(),
        ];
        let want = [
            (FsMethod::SyncData, Some("meta/journal.off".to_owned())),
            (
                FsMethod::LinkNoclobber,
                Some(format!("objects/ab/{}", "cd".repeat(31))),
            ),
            (FsMethod::WriteDurableTemp, Some("objects/tmp".to_owned())),
            (FsMethod::RenameDurable, Some("meta/b".to_owned())),
            (FsMethod::CreateDirAll, Some(".".to_owned())),
            (FsMethod::FsyncDir, None),
        ];
        for (failure, (method, file)) in failures.iter().zip(want) {
            assert_eq!(
                named(failure),
                (FileOp::Fs(method), file.map(Utf8PathBuf::from), Some(5)),
                "{failure}"
            );
        }
        assert_eq!(counter.calls(), 6, "one call each");
        let recorded: Vec<(FileOp, Option<Utf8PathBuf>)> = (failing.failed_calls().iter())
            .map(|(method, path)| ((*method).into(), store_relative(root, path)))
            .collect();
        let located: Vec<(FileOp, Option<Utf8PathBuf>)> = (failures.iter())
            .map(|failure| (failure.op, failure.file.clone()))
            .collect();
        assert_eq!(recorded, located, "named as FailAtFs failed them");

        // A read after the failure succeeds (reads aside), forwarded once.
        assert!(store_fs.exists(root.join("meta/journal.off")).unwrap());
        assert_eq!(counter.calls(), 7);
        let eof = io::Error::from(io::ErrorKind::UnexpectedEof);
        let failure = store_fs.locate(FileOp::ReadAt, root.join("meta/journal.log"), eof);
        assert_eq!(
            failure.to_string(),
            "read_at meta/journal.log: unexpected end of file"
        );
        assert_eq!(counter.calls(), 7, "locating makes no call");
        assert_eq!(store_fs.root(), root);
    }

    /// `StoreFs::exists`, which the test above runs only where it succeeds, names its failure as
    /// every forwarded call does: the method, and the path relative to the store's root.
    #[test]
    fn store_fs_names_a_failed_exists() {
        use crate::error::FileOp;

        let dir = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(dir.path()).unwrap();
        let fail = FailAt {
            at: 1,
            mode: FailMode::Once,
            error: InjectedError::Eio,
        };
        let store_fs = StoreFs::new(Arc::new(FailAtFs::new(RealFs, fail)), root);
        let failure = store_fs.exists(root.join("meta/journal.off")).unwrap_err();
        assert_eq!(
            (failure.op, failure.file.as_deref(), failure.raw_os_error()),
            (
                FileOp::Fs(FsMethod::Exists),
                Some(Utf8Path::new("meta/journal.off")),
                Some(5)
            )
        );
    }
}

#[cfg(test)]
mod store_fs_tests {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    use camino::Utf8Path;

    /// A store-relative path is worked out from the path's text alone, with no system call: `.`
    /// for the root; nothing for a path outside the root, or one that leaves it with `..`, or
    /// one that is not UTF-8.
    #[test]
    fn store_relative_is_lexical_and_stays_in_the_root() {
        let root = Utf8Path::new("/r");
        let relative = |path: &str| super::store_relative(root, Path::new(path));
        assert_eq!(relative("/r").as_deref(), Some(Utf8Path::new(".")));
        assert_eq!(
            relative("/r/meta/journal.off").as_deref(),
            Some(Utf8Path::new("meta/journal.off"))
        );
        assert_eq!(
            relative("/r/./meta/lock").as_deref(),
            Some(Utf8Path::new("meta/lock"))
        );
        assert_eq!(relative("/elsewhere/x"), None);
        assert_eq!(relative("/rx/meta"), None, "a name that merely starts so");
        assert_eq!(relative("/r/../x"), None);
        assert_eq!(relative("/r/meta/../../x"), None);
        assert_eq!(
            relative("r/meta"),
            None,
            "a relative path under an absolute root"
        );
        let not_utf8 = Path::new(OsStr::from_bytes(b"/r/meta/\xff"));
        assert_eq!(super::store_relative(root, not_utf8), None);
    }
}
