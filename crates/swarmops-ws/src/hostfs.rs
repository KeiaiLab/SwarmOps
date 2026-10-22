//! `HostFs` (§3.5 of the M5 design): the calls a workspace makes on its materialised tree, the
//! host's files the agent edits. The workspace's durable files go through the store's `FsOps`
//! instead; this layer has no sync, because the tree is a copy of what the store holds durably.
//!
//! ```text
//! materialize, status, commit, discard
//!          │                    │
//!          ▼                    ▼
//!      HostFs (the tree)     FsOps (workspace.json, intents, registry)
//!      RealHost              RealFs
//!      RecordingHost         CrashAtFs (test-hooks)
//!      CrashAtHost
//!      (both test-hooks)
//! ```
//!
//! The M5 plan's task W1 froze the trait; task W5 wrote `RealHost`, `RecordingHost` and
//! `CrashAtHost`, and added the two calls the frozen list lacked: [`HostFs::read_link`] (a
//! symlink's target, which status and the edit check compare) and [`HostFs::set_mode`] (the
//! executable bit, which a materialisation and the volume probe write).

use std::io;
#[cfg(feature = "test-hooks")]
use std::sync::Mutex;
#[cfg(feature = "test-hooks")]
use std::sync::atomic::{AtomicU64, Ordering};

use camino::Utf8Path;
#[cfg(feature = "test-hooks")]
use camino::Utf8PathBuf;

/// What a path is on the host, as `symlink_metadata` sees it: a symlink is never followed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HostKind {
    /// A regular file.
    File,
    /// A directory.
    Dir,
    /// A symlink.
    Symlink,
    /// Anything else: a FIFO, a socket, a device.
    Other,
}

/// A path's metadata, as status compares it with the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HostMeta {
    /// What the path is.
    pub kind: HostKind,
    /// Its size in bytes.
    pub size: u64,
    /// Its modification time, in nanoseconds since the Unix epoch.
    pub mtime_ns: i64,
    /// Its inode number; 0 where the platform has no stable one (Windows).
    pub inode: u64,
    /// Its permission bits (`st_mode & 0o7777`); 0 where the platform has none.
    pub mode: u32,
}

/// The calls a workspace makes on its materialised tree, and no others.
pub trait HostFs: Send + Sync {
    /// Writes `bytes` to `path`: a temp file in the same directory, then a rename over the path,
    /// so that a reader never sees half a file. No sync.
    fn write_file(&self, path: &Utf8Path, bytes: &[u8]) -> io::Result<()>;

    /// Makes a symlink at `path` holding `target`.
    fn symlink(&self, target: &str, path: &Utf8Path) -> io::Result<()>;

    /// `path`'s metadata, without following a symlink; `None` when nothing is there.
    fn symlink_metadata(&self, path: &Utf8Path) -> io::Result<Option<HostMeta>>;

    /// The bytes of the file at `path`.
    fn read_file(&self, path: &Utf8Path) -> io::Result<Vec<u8>>;

    /// The names in the directory at `path`, in no order.
    fn read_dir(&self, path: &Utf8Path) -> io::Result<Vec<String>>;

    /// Removes the file or symlink at `path`.
    fn remove_file(&self, path: &Utf8Path) -> io::Result<()>;

    /// Removes the directory at `path` if it is empty; whether it did.
    fn remove_dir_if_empty(&self, path: &Utf8Path) -> io::Result<bool>;

    /// Makes the directory at `path` and every missing parent.
    fn create_dir_all(&self, path: &Utf8Path) -> io::Result<()>;

    /// Renames `from` to `to`, replacing what `to` names.
    fn rename(&self, from: &Utf8Path, to: &Utf8Path) -> io::Result<()>;

    /// The target of the symlink at `path`, never followed.
    fn read_link(&self, path: &Utf8Path) -> io::Result<String>;

    /// Sets the permission bits of the file at `path` to `mode` (`0o755`, say); a no-op where
    /// the platform has none.
    fn set_mode(&self, path: &Utf8Path, mode: u32) -> io::Result<()>;
}

/// The suffix of the temp a [`RealHost::write_file`] writes beside its path: one name per path,
/// so that a write killed between the temp and the rename leaves a temp the next write of that
/// path reuses, rather than one more file.
const TEMP_SUFFIX: &str = ".swarmws-tmp";

/// The host's own filesystem.
#[derive(Debug, Clone, Copy, Default)]
pub struct RealHost;

impl HostFs for RealHost {
    fn write_file(&self, path: &Utf8Path, bytes: &[u8]) -> io::Result<()> {
        // 1. The temp beside the path; one a killed write left is removed, never followed.
        let name = path.file_name().ok_or_else(|| no_name(path))?;
        let temp = path.with_file_name(format!(".{name}{TEMP_SUFFIX}"));
        match std::fs::remove_file(&temp) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }

        // 2. Made new (`O_EXCL`: a symlink put there in between is refused), written, renamed
        //    over the path. No sync: the store holds the bytes durably.
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        io::Write::write_all(&mut file, bytes)?;
        drop(file);
        std::fs::rename(&temp, path)
    }

    fn symlink(&self, target: &str, path: &Utf8Path) -> io::Result<()> {
        make_symlink(target, path)
    }

    fn symlink_metadata(&self, path: &Utf8Path) -> io::Result<Option<HostMeta>> {
        match std::fs::symlink_metadata(path) {
            Ok(meta) => Ok(Some(host_meta(&meta))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn read_file(&self, path: &Utf8Path) -> io::Result<Vec<u8>> {
        std::fs::read(path)
    }

    fn read_dir(&self, path: &Utf8Path) -> io::Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(path)? {
            let name = entry?.file_name();
            let name = name.into_string().map_err(|raw| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{path} holds a name that is not UTF-8: {}", raw.display()),
                )
            })?;
            names.push(name);
        }
        Ok(names)
    }

    fn remove_file(&self, path: &Utf8Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }

    fn remove_dir_if_empty(&self, path: &Utf8Path) -> io::Result<bool> {
        match std::fs::remove_dir(path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::DirectoryNotEmpty => Ok(false),
            Err(e) => Err(e),
        }
    }

    fn create_dir_all(&self, path: &Utf8Path) -> io::Result<()> {
        std::fs::create_dir_all(path)
    }

    fn rename(&self, from: &Utf8Path, to: &Utf8Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }

    fn read_link(&self, path: &Utf8Path) -> io::Result<String> {
        let target = std::fs::read_link(path)?;
        target.into_os_string().into_string().map_err(|raw| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "the symlink {path} holds a target that is not UTF-8: {}",
                    raw.display()
                ),
            )
        })
    }

    fn set_mode(&self, path: &Utf8Path, mode: u32) -> io::Result<()> {
        set_permissions(path, mode)
    }
}

/// The error of a path without a file name (`/`, `..`), which no file is written at.
fn no_name(path: &Utf8Path) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, format!("{path} names no file"))
}

/// A symlink at `path` holding `target`.
#[cfg(unix)]
fn make_symlink(target: &str, path: &Utf8Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, path)
}

/// A symlink at `path` holding `target`: a file symlink, which needs a privilege Windows does
/// not give by default (the volume probe finds out).
#[cfg(windows)]
fn make_symlink(target: &str, path: &Utf8Path) -> io::Result<()> {
    std::os::windows::fs::symlink_file(target, path)
}

/// No symlinks on this platform.
#[cfg(not(any(unix, windows)))]
fn make_symlink(_target: &str, _path: &Utf8Path) -> io::Result<()> {
    Err(io::Error::from(io::ErrorKind::Unsupported))
}

/// The permission bits of the file at `path` set to `mode`.
#[cfg(unix)]
fn set_permissions(path: &Utf8Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

/// No permission bits on this platform: nothing to set.
#[cfg(not(unix))]
fn set_permissions(_path: &Utf8Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

/// What `meta` says of its path.
fn host_meta(meta: &std::fs::Metadata) -> HostMeta {
    // 1. The kind, a symlink never followed.
    let file_type = meta.file_type();
    let kind = if file_type.is_symlink() {
        HostKind::Symlink
    } else if file_type.is_dir() {
        HostKind::Dir
    } else if file_type.is_file() {
        HostKind::File
    } else {
        HostKind::Other
    };

    // 2. The modification time in nanoseconds, before 1970 negative, clamped to i64.
    let mtime_ns = meta.modified().map_or(0, |time| {
        let ns = match time.duration_since(std::time::UNIX_EPOCH) {
            Ok(after) => i128::try_from(after.as_nanos()).unwrap_or(i128::MAX),
            Err(before) => -i128::try_from(before.duration().as_nanos()).unwrap_or(i128::MAX),
        };
        i64::try_from(ns.clamp(i128::from(i64::MIN), i128::from(i64::MAX))).unwrap_or(0)
    });

    // 3. The inode and the permission bits, where the platform has them.
    let (inode, mode) = inode_and_mode(meta);
    HostMeta {
        kind,
        size: meta.len(),
        mtime_ns,
        inode,
        mode,
    }
}

/// `meta`'s inode number and permission bits.
#[cfg(unix)]
fn inode_and_mode(meta: &std::fs::Metadata) -> (u64, u32) {
    use std::os::unix::fs::MetadataExt;
    (meta.ino(), meta.mode() & 0o7777)
}

/// No stable inode and no permission bits in std on this platform.
#[cfg(not(unix))]
fn inode_and_mode(_meta: &std::fs::Metadata) -> (u64, u32) {
    (0, 0)
}

/// A [`HostFs`] method, as [`RecordingHost`] records it (feature `test-hooks`).
#[cfg(feature = "test-hooks")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HostMethod {
    /// [`HostFs::write_file`].
    WriteFile,
    /// [`HostFs::symlink`].
    Symlink,
    /// [`HostFs::symlink_metadata`].
    SymlinkMetadata,
    /// [`HostFs::read_file`].
    ReadFile,
    /// [`HostFs::read_dir`].
    ReadDir,
    /// [`HostFs::remove_file`].
    RemoveFile,
    /// [`HostFs::remove_dir_if_empty`].
    RemoveDirIfEmpty,
    /// [`HostFs::create_dir_all`].
    CreateDirAll,
    /// [`HostFs::rename`].
    Rename,
    /// [`HostFs::read_link`].
    ReadLink,
    /// [`HostFs::set_mode`].
    SetMode,
}

/// One call [`RecordingHost`] saw: the method and the path it named (`from` for a rename).
#[cfg(feature = "test-hooks")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostCall {
    /// The method.
    pub method: HostMethod,
    /// The path.
    pub path: Utf8PathBuf,
}

/// A host that forwards every call to `inner` and records it, in order: what a test counts to
/// see that an update writes only what changed (feature `test-hooks`).
#[cfg(feature = "test-hooks")]
pub struct RecordingHost<H: HostFs = RealHost> {
    /// The host the calls go to.
    inner: H,
    /// The calls so far.
    calls: Mutex<Vec<HostCall>>,
}

#[cfg(feature = "test-hooks")]
impl<H: HostFs> RecordingHost<H> {
    /// Records the calls made on `inner`.
    pub fn new(inner: H) -> RecordingHost<H> {
        RecordingHost {
            inner,
            calls: Mutex::new(Vec::new()),
        }
    }

    /// Every call so far, in order.
    pub fn calls(&self) -> Vec<HostCall> {
        self.calls.lock().expect("no recorder panicked").clone()
    }

    /// The paths of the calls of `method` so far, in order.
    pub fn calls_of(&self, method: HostMethod) -> Vec<Utf8PathBuf> {
        (self.calls().into_iter())
            .filter(|call| call.method == method)
            .map(|call| call.path)
            .collect()
    }

    /// Forgets the calls so far.
    pub fn clear(&self) {
        self.calls.lock().expect("no recorder panicked").clear();
    }

    /// Records a call of `method` on `path`.
    fn record(&self, method: HostMethod, path: &Utf8Path) {
        let call = HostCall {
            method,
            path: path.to_owned(),
        };
        self.calls.lock().expect("no recorder panicked").push(call);
    }
}

#[cfg(feature = "test-hooks")]
impl<H: HostFs> HostFs for RecordingHost<H> {
    fn write_file(&self, path: &Utf8Path, bytes: &[u8]) -> io::Result<()> {
        self.record(HostMethod::WriteFile, path);
        self.inner.write_file(path, bytes)
    }

    fn symlink(&self, target: &str, path: &Utf8Path) -> io::Result<()> {
        self.record(HostMethod::Symlink, path);
        self.inner.symlink(target, path)
    }

    fn symlink_metadata(&self, path: &Utf8Path) -> io::Result<Option<HostMeta>> {
        self.record(HostMethod::SymlinkMetadata, path);
        self.inner.symlink_metadata(path)
    }

    fn read_file(&self, path: &Utf8Path) -> io::Result<Vec<u8>> {
        self.record(HostMethod::ReadFile, path);
        self.inner.read_file(path)
    }

    fn read_dir(&self, path: &Utf8Path) -> io::Result<Vec<String>> {
        self.record(HostMethod::ReadDir, path);
        self.inner.read_dir(path)
    }

    fn remove_file(&self, path: &Utf8Path) -> io::Result<()> {
        self.record(HostMethod::RemoveFile, path);
        self.inner.remove_file(path)
    }

    fn remove_dir_if_empty(&self, path: &Utf8Path) -> io::Result<bool> {
        self.record(HostMethod::RemoveDirIfEmpty, path);
        self.inner.remove_dir_if_empty(path)
    }

    fn create_dir_all(&self, path: &Utf8Path) -> io::Result<()> {
        self.record(HostMethod::CreateDirAll, path);
        self.inner.create_dir_all(path)
    }

    fn rename(&self, from: &Utf8Path, to: &Utf8Path) -> io::Result<()> {
        self.record(HostMethod::Rename, from);
        self.inner.rename(from, to)
    }

    fn read_link(&self, path: &Utf8Path) -> io::Result<String> {
        self.record(HostMethod::ReadLink, path);
        self.inner.read_link(path)
    }

    fn set_mode(&self, path: &Utf8Path, mode: u32) -> io::Result<()> {
        self.record(HostMethod::SetMode, path);
        self.inner.set_mode(path, mode)
    }
}

/// Fault injection for crash tests (feature `test-hooks`), as the store's `CrashAtFs`: forwards
/// to `inner`, except that the k-th call (counting every [`HostFs`] method, from 1) aborts the
/// process with [`std::process::abort`] instead of running, as a crash at that exact point
/// would. It counts every call ([`CrashAtHost::calls`]).
#[cfg(feature = "test-hooks")]
pub struct CrashAtHost<H: HostFs = RealHost> {
    /// The host the calls go to.
    inner: H,
    /// Calls left until the abort, the fatal one included; 0 never aborts.
    crash_at: AtomicU64,
    /// Every call so far, the fatal one included.
    calls: AtomicU64,
}

#[cfg(feature = "test-hooks")]
impl<H: HostFs> CrashAtHost<H> {
    /// Wraps `inner` so that call number `k` aborts; `k == 0` never aborts.
    pub fn new(inner: H, k: u64) -> CrashAtHost<H> {
        CrashAtHost {
            inner,
            crash_at: AtomicU64::new(k),
            calls: AtomicU64::new(0),
        }
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
impl<H: HostFs> HostFs for CrashAtHost<H> {
    fn write_file(&self, path: &Utf8Path, bytes: &[u8]) -> io::Result<()> {
        self.tick();
        self.inner.write_file(path, bytes)
    }

    fn symlink(&self, target: &str, path: &Utf8Path) -> io::Result<()> {
        self.tick();
        self.inner.symlink(target, path)
    }

    fn symlink_metadata(&self, path: &Utf8Path) -> io::Result<Option<HostMeta>> {
        self.tick();
        self.inner.symlink_metadata(path)
    }

    fn read_file(&self, path: &Utf8Path) -> io::Result<Vec<u8>> {
        self.tick();
        self.inner.read_file(path)
    }

    fn read_dir(&self, path: &Utf8Path) -> io::Result<Vec<String>> {
        self.tick();
        self.inner.read_dir(path)
    }

    fn remove_file(&self, path: &Utf8Path) -> io::Result<()> {
        self.tick();
        self.inner.remove_file(path)
    }

    fn remove_dir_if_empty(&self, path: &Utf8Path) -> io::Result<bool> {
        self.tick();
        self.inner.remove_dir_if_empty(path)
    }

    fn create_dir_all(&self, path: &Utf8Path) -> io::Result<()> {
        self.tick();
        self.inner.create_dir_all(path)
    }

    fn rename(&self, from: &Utf8Path, to: &Utf8Path) -> io::Result<()> {
        self.tick();
        self.inner.rename(from, to)
    }

    fn read_link(&self, path: &Utf8Path) -> io::Result<String> {
        self.tick();
        self.inner.read_link(path)
    }

    fn set_mode(&self, path: &Utf8Path, mode: u32) -> io::Result<()> {
        self.tick();
        self.inner.set_mode(path, mode)
    }
}
