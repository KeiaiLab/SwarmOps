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
//!
//! Every write names a root and a path below it (the M16 design §1). On Unix `RealHost` walks
//! down from the root one directory at a time and never follows a link below it:
//!
//! ```text
//! root (followed) ─openat(NOFOLLOW)─▶ d1 ─openat(NOFOLLOW)─▶ d2 ─▶ temp, renameat │ unlinkat │ …
//!                       │ a link or a file: Swapped(path) → E_WS_DIRTY
//!                       └ another failure: Unopened(dir)  → E_IO naming the directory
//! ```
//!
//! Other platforms write by the full path after the caller's pre-write check.

use std::fmt;
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
#[non_exhaustive]
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
#[non_exhaustive]
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

/// The calls a workspace makes on its materialised tree, and no others. A write names the
/// workspace's `root` and a `rel` path below it (`/`-separated); a read names a full path.
pub trait HostFs: Send + Sync {
    /// Writes `bytes` to `rel` below `root`: a temp file in the same directory, then a rename
    /// over the path, so that a reader never sees half a file. Missing directories are made.
    /// No sync.
    fn write_file(&self, root: &Utf8Path, rel: &str, bytes: &[u8]) -> io::Result<()>;

    /// Makes a symlink at `rel` below `root` holding `target`; missing directories are made.
    fn symlink(&self, root: &Utf8Path, rel: &str, target: &str) -> io::Result<()>;

    /// `path`'s metadata, without following a symlink; `None` when nothing is there.
    fn symlink_metadata(&self, path: &Utf8Path) -> io::Result<Option<HostMeta>>;

    /// The bytes of the file at `path`.
    fn read_file(&self, path: &Utf8Path) -> io::Result<Vec<u8>>;

    /// The names in the directory at `path`, in no order.
    fn read_dir(&self, path: &Utf8Path) -> io::Result<Vec<String>>;

    /// Removes the file or symlink at `rel` below `root`.
    fn remove_file(&self, root: &Utf8Path, rel: &str) -> io::Result<()>;

    /// Removes the directory at `rel` below `root` if it is empty; whether it did.
    fn remove_dir_if_empty(&self, root: &Utf8Path, rel: &str) -> io::Result<bool>;

    /// Makes the directory at `rel` below `root` and every missing parent.
    fn create_dir_all(&self, root: &Utf8Path, rel: &str) -> io::Result<()>;

    /// Renames `from` to `to`, both below `root`, replacing what `to` names.
    fn rename(&self, root: &Utf8Path, from: &str, to: &str) -> io::Result<()>;

    /// The target of the symlink at `path`, never followed.
    fn read_link(&self, path: &Utf8Path) -> io::Result<String>;

    /// Sets the permission bits of the file at `rel` below `root` to `mode` (`0o755`, say); a
    /// no-op where the platform has none. On Unix a file that is not a regular one with one
    /// link is refused as swapped: a hard link would carry the mode outside the root.
    fn set_mode(&self, root: &Utf8Path, rel: &str, mode: u32) -> io::Result<()>;

    /// Writes `bytes` to `rel` below `root` as [`HostFs::write_file`] does, the file readable
    /// and writable by its owner alone (`0o600`): a file that names an identity, as the
    /// hold record does. [`RealHost`] makes it with that mode; this default writes, then sets it.
    fn write_private(&self, root: &Utf8Path, rel: &str, bytes: &[u8]) -> io::Result<()> {
        self.write_file(root, rel, bytes)?;
        self.set_mode(root, rel, PRIVATE_MODE)
    }
}

/// The permission bits of a file only its owner reads and writes, as the registry's entries.
const PRIVATE_MODE: u32 = 0o600;

/// A write that met, below the root, something other than a directory where it walks down, or
/// other than a regular file with one link where it sets a mode: the path below the root.
#[derive(Debug)]
struct Swapped(String);

impl fmt::Display for Swapped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} was replaced during the write", self.0)
    }
}

impl std::error::Error for Swapped {}

/// A directory a write walks through that would not open: its path below the root (`.` for
/// the root) and why.
#[derive(Debug)]
struct Unopened {
    /// The directory.
    dir: String,
    /// Why it would not open.
    source: io::Error,
}

impl fmt::Display for Unopened {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.dir, self.source)
    }
}

impl std::error::Error for Unopened {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// The error of a write that met a swapped `rel`.
fn swapped(rel: &str) -> io::Error {
    io::Error::other(Swapped(rel.to_owned()))
}

/// The error of directory `dir` that would not open, of `source`'s kind.
fn unopened(dir: &str, source: io::Error) -> io::Error {
    let kind = source.kind();
    io::Error::new(
        kind,
        Unopened {
            dir: dir.to_owned(),
            source,
        },
    )
}

/// The path below the root that `e` found swapped, if that is what `e` is.
pub(crate) fn swapped_path(e: &io::Error) -> Option<&str> {
    let swapped = e.get_ref()?.downcast_ref::<Swapped>()?;
    Some(&swapped.0)
}

/// The directory below the root that `e` could not open, if that is what `e` is.
pub(crate) fn unopened_dir(e: &io::Error) -> Option<&str> {
    let unopened = e.get_ref()?.downcast_ref::<Unopened>()?;
    Some(&unopened.dir)
}

/// `dir`'s parent and its last name: the root and the path a write that removes `dir` itself
/// names (a parent of `""` is `.`).
pub(crate) fn split_root(dir: &Utf8Path) -> io::Result<(&Utf8Path, &str)> {
    let name = dir.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{dir} names no directory"),
        )
    })?;
    let parent = match dir.parent() {
        Some(parent) if !parent.as_str().is_empty() => parent,
        _ => Utf8Path::new("."),
    };
    Ok((parent, name))
}

/// `rel`'s directories and its last name; an empty name, `.` or `..` is refused, as no write
/// below the root goes through one.
fn split(rel: &str) -> io::Result<(Vec<&str>, &str)> {
    let mut parts: Vec<&str> = rel.split('/').collect();
    if parts.iter().any(|part| matches!(*part, "" | "." | "..")) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{rel} is not a path below the workspace"),
        ));
    }
    let name = parts.pop().unwrap_or_default();
    Ok((parts, name))
}

/// The suffix of the temp a [`RealHost::write_file`] writes beside its path: one name per path,
/// so that a write killed between the temp and the rename leaves a temp the next write of that
/// path reuses, rather than one more file.
const TEMP_SUFFIX: &str = ".swarmws-tmp";

/// The host's own filesystem.
#[derive(Debug, Clone, Copy, Default)]
#[non_exhaustive]
pub struct RealHost;

impl HostFs for RealHost {
    fn write_file(&self, root: &Utf8Path, rel: &str, bytes: &[u8]) -> io::Result<()> {
        walk::write_file(root, rel, bytes)
    }

    fn symlink(&self, root: &Utf8Path, rel: &str, target: &str) -> io::Result<()> {
        walk::symlink(root, rel, target)
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

    fn remove_file(&self, root: &Utf8Path, rel: &str) -> io::Result<()> {
        walk::remove_file(root, rel)
    }

    fn remove_dir_if_empty(&self, root: &Utf8Path, rel: &str) -> io::Result<bool> {
        walk::remove_dir_if_empty(root, rel)
    }

    fn create_dir_all(&self, root: &Utf8Path, rel: &str) -> io::Result<()> {
        walk::create_dir_all(root, rel)
    }

    fn rename(&self, root: &Utf8Path, from: &str, to: &str) -> io::Result<()> {
        walk::rename(root, from, to)
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

    fn set_mode(&self, root: &Utf8Path, rel: &str, mode: u32) -> io::Result<()> {
        walk::set_mode(root, rel, mode)
    }

    fn write_private(&self, root: &Utf8Path, rel: &str, bytes: &[u8]) -> io::Result<()> {
        walk::write_private(root, rel, bytes)
    }
}

/// The writes of `RealHost`: rooted and never following a link below the root on Unix, by the
/// full path elsewhere.
#[cfg(unix)]
use nofollow as walk;

/// The writes of `RealHost`: rooted and never following a link below the root on Unix, by the
/// full path elsewhere.
#[cfg(not(unix))]
use fullpath as walk;

/// `RealHost`'s writes on Unix: each directory below the root opened with `O_NOFOLLOW` from the
/// one above it, the write made relative to the last one.
#[cfg(unix)]
mod nofollow {
    use std::io;
    use std::os::fd::OwnedFd;

    use camino::Utf8Path;
    use rustix::fs::{AtFlags, FileType, Mode, OFlags};
    use rustix::io::Errno;

    use super::{PRIVATE_MODE, TEMP_SUFFIX, split, swapped, unopened};

    /// How the root is opened: links above and at it followed.
    const ROOT_FLAGS: OFlags = OFlags::RDONLY
        .union(OFlags::DIRECTORY)
        .union(OFlags::CLOEXEC);

    /// How a directory below the root is opened: never through a link.
    const DIR_FLAGS: OFlags = ROOT_FLAGS.union(OFlags::NOFOLLOW);

    /// How a temp is made: new, never through a link.
    const TEMP_FLAGS: OFlags = OFlags::WRONLY
        .union(OFlags::CREATE)
        .union(OFlags::EXCL)
        .union(OFlags::NOFOLLOW)
        .union(OFlags::CLOEXEC);

    /// How a file is opened to set its mode: never through a link, never blocking on a FIFO.
    const MODE_FLAGS: OFlags = OFlags::RDONLY
        .union(OFlags::NOFOLLOW)
        .union(OFlags::NONBLOCK)
        .union(OFlags::CLOEXEC);

    /// The mode a new file or directory asks for, before the umask.
    const NEW_FILE_MODE: u32 = 0o666;

    /// The mode a new directory asks for, before the umask.
    const NEW_DIR_MODE: u32 = 0o777;

    /// What a walk does with a directory that is missing.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum Missing {
        /// Makes it: a write.
        Make,
        /// Fails with `NotFound`: a removal or a rename.
        Fail,
    }

    /// The directory `parents` names below `root`: the root opened as it is, then each of
    /// `parents` from the one above it, never through a link.
    pub(super) fn descend(
        root: &Utf8Path,
        parents: &[&str],
        missing: Missing,
    ) -> io::Result<OwnedFd> {
        // 1. The root, followed.
        let mut fd = rustix::fs::open(root.as_std_path(), ROOT_FLAGS, Mode::empty())
            .map_err(|e| unopened(".", e.into()))?;

        // 2. Each directory below it, never followed; a missing one made when asked.
        for (n, name) in parents.iter().enumerate() {
            let shown = parents[..=n].join("/");
            fd = open_dir(&fd, name, &shown, missing)?;
        }
        Ok(fd)
    }

    /// Directory `name` in `dir` (`shown` below the root), never through a link.
    fn open_dir(dir: &OwnedFd, name: &str, shown: &str, missing: Missing) -> io::Result<OwnedFd> {
        // 1. Opened as it is; a link or a file is a swap.
        match rustix::fs::openat(dir, name, DIR_FLAGS, Mode::empty()) {
            Ok(fd) => return Ok(fd),
            Err(Errno::NOENT) if missing == Missing::Make => {}
            Err(e) => return Err(dir_failed(shown, e)),
        }

        // 2. Missing: made (or made meanwhile by another write), then opened.
        match rustix::fs::mkdirat(dir, name, Mode::from_raw_mode(NEW_DIR_MODE)) {
            Ok(()) | Err(Errno::EXIST) => {}
            Err(e) => return Err(unopened(shown, e.into())),
        }
        rustix::fs::openat(dir, name, DIR_FLAGS, Mode::empty()).map_err(|e| dir_failed(shown, e))
    }

    /// Why directory `shown` would not open: swapped for a link (`ELOOP`, `EMLINK` on FreeBSD)
    /// or for a file (`ENOTDIR`), else unopened.
    fn dir_failed(shown: &str, e: Errno) -> io::Error {
        if is_swap(e) {
            return swapped(shown);
        }
        unopened(shown, e.into())
    }

    /// Whether an `O_NOFOLLOW` open failed on a link or on a file where a directory was.
    fn is_swap(e: Errno) -> bool {
        e == Errno::LOOP || e == Errno::MLINK || e == Errno::NOTDIR
    }

    /// The directory above `rel` below `root`, and `rel`'s last name.
    fn parent_of<'r>(
        root: &Utf8Path,
        rel: &'r str,
        missing: Missing,
    ) -> io::Result<(OwnedFd, &'r str)> {
        let (parents, name) = split(rel)?;
        Ok((descend(root, &parents, missing)?, name))
    }

    /// See `HostFs::write_file`.
    pub(super) fn write_file(root: &Utf8Path, rel: &str, bytes: &[u8]) -> io::Result<()> {
        write_mode(root, rel, bytes, NEW_FILE_MODE)
    }

    /// See `HostFs::write_private`: the temp made with [`PRIVATE_MODE`], so the path never
    /// holds the bytes with more.
    pub(super) fn write_private(root: &Utf8Path, rel: &str, bytes: &[u8]) -> io::Result<()> {
        write_mode(root, rel, bytes, PRIVATE_MODE)
    }

    /// Writes `bytes` to `rel` below `root`, a new file asking for `mode` before the umask.
    fn write_mode(root: &Utf8Path, rel: &str, bytes: &[u8], mode: u32) -> io::Result<()> {
        // 1. The temp beside the path; one a killed write left is removed, never followed.
        let (dir, name) = parent_of(root, rel, Missing::Make)?;
        let temp = format!(".{name}{TEMP_SUFFIX}");
        match rustix::fs::unlinkat(&dir, temp.as_str(), AtFlags::empty()) {
            Ok(()) | Err(Errno::NOENT) => {}
            Err(e) => return Err(e.into()),
        }

        // 2. Made new (`O_EXCL`, `O_NOFOLLOW`), written, renamed over the path in the same
        //    directory. No sync: the store holds the bytes durably.
        let fd = rustix::fs::openat(&dir, temp.as_str(), TEMP_FLAGS, Mode::from_raw_mode(mode))?;
        let mut file = std::fs::File::from(fd);
        io::Write::write_all(&mut file, bytes)?;
        drop(file);
        rustix::fs::renameat(&dir, temp.as_str(), &dir, name)?;
        Ok(())
    }

    /// See `HostFs::symlink`.
    pub(super) fn symlink(root: &Utf8Path, rel: &str, target: &str) -> io::Result<()> {
        let (dir, name) = parent_of(root, rel, Missing::Make)?;
        rustix::fs::symlinkat(target, &dir, name)?;
        Ok(())
    }

    /// See `HostFs::remove_file`.
    pub(super) fn remove_file(root: &Utf8Path, rel: &str) -> io::Result<()> {
        let (dir, name) = parent_of(root, rel, Missing::Fail)?;
        rustix::fs::unlinkat(&dir, name, AtFlags::empty())?;
        Ok(())
    }

    /// See `HostFs::remove_dir_if_empty`; a file or a link where the directory was is a swap.
    pub(super) fn remove_dir_if_empty(root: &Utf8Path, rel: &str) -> io::Result<bool> {
        let (dir, name) = parent_of(root, rel, Missing::Fail)?;
        match rustix::fs::unlinkat(&dir, name, AtFlags::REMOVEDIR) {
            Ok(()) => Ok(true),
            Err(Errno::NOTEMPTY | Errno::EXIST) => Ok(false),
            Err(Errno::NOTDIR) => Err(swapped(rel)),
            Err(e) => Err(e.into()),
        }
    }

    /// See `HostFs::create_dir_all`.
    pub(super) fn create_dir_all(root: &Utf8Path, rel: &str) -> io::Result<()> {
        let (mut parents, name) = split(rel)?;
        parents.push(name);
        descend(root, &parents, Missing::Make).map(drop)
    }

    /// See `HostFs::rename`.
    pub(super) fn rename(root: &Utf8Path, from: &str, to: &str) -> io::Result<()> {
        let (from_dir, from_name) = parent_of(root, from, Missing::Fail)?;
        let (to_dir, to_name) = parent_of(root, to, Missing::Fail)?;
        rustix::fs::renameat(&from_dir, from_name, &to_dir, to_name)?;
        Ok(())
    }

    /// See `HostFs::set_mode`: the file opened without following a link, checked to be a
    /// regular file with one link, then its mode set through the descriptor.
    pub(super) fn set_mode(root: &Utf8Path, rel: &str, mode: u32) -> io::Result<()> {
        // 1. The file itself, never a link.
        let (dir, name) = parent_of(root, rel, Missing::Fail)?;
        let fd = match rustix::fs::openat(&dir, name, MODE_FLAGS, Mode::empty()) {
            Ok(fd) => fd,
            Err(e) if is_swap(e) => return Err(swapped(rel)),
            Err(e) => return Err(e.into()),
        };

        // 2. A regular file with one link: else the mode would reach another path.
        let stat = rustix::fs::fstat(&fd)?;
        let regular = FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile;
        if !regular || stat.st_nlink != 1 {
            return Err(swapped(rel));
        }
        rustix::fs::fchmod(&fd, Mode::from_raw_mode(mode))?;
        Ok(())
    }
}

/// `RealHost`'s writes where the rooted walk is not built: by the full path, after the
/// caller's pre-write check (the M16 design §1).
#[cfg(not(unix))]
mod fullpath {
    use std::io;

    use camino::Utf8Path;

    use super::{TEMP_SUFFIX, split};

    /// `rel` below `root`, checked to name a path below it.
    fn full(root: &Utf8Path, rel: &str) -> io::Result<camino::Utf8PathBuf> {
        split(rel)?;
        Ok(root.join(rel))
    }

    /// See `HostFs::write_file`.
    pub(super) fn write_file(root: &Utf8Path, rel: &str, bytes: &[u8]) -> io::Result<()> {
        // 1. The temp beside the path; one a killed write left is removed.
        let path = full(root, rel)?;
        let (_, name) = split(rel)?;
        let temp = path.with_file_name(format!(".{name}{TEMP_SUFFIX}"));
        match std::fs::remove_file(&temp) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }

        // 2. Made new, written, renamed over the path. No sync.
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        io::Write::write_all(&mut file, bytes)?;
        drop(file);
        std::fs::rename(&temp, path)
    }

    /// See `HostFs::symlink`.
    pub(super) fn symlink(root: &Utf8Path, rel: &str, target: &str) -> io::Result<()> {
        make_symlink(target, &full(root, rel)?)
    }

    /// See `HostFs::remove_file`.
    pub(super) fn remove_file(root: &Utf8Path, rel: &str) -> io::Result<()> {
        std::fs::remove_file(full(root, rel)?)
    }

    /// See `HostFs::remove_dir_if_empty`.
    pub(super) fn remove_dir_if_empty(root: &Utf8Path, rel: &str) -> io::Result<bool> {
        match std::fs::remove_dir(full(root, rel)?) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::DirectoryNotEmpty => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// See `HostFs::create_dir_all`.
    pub(super) fn create_dir_all(root: &Utf8Path, rel: &str) -> io::Result<()> {
        std::fs::create_dir_all(full(root, rel)?)
    }

    /// See `HostFs::rename`.
    pub(super) fn rename(root: &Utf8Path, from: &str, to: &str) -> io::Result<()> {
        std::fs::rename(full(root, from)?, full(root, to)?)
    }

    /// See `HostFs::write_private`: no permission bits on these platforms, the plain write.
    pub(super) fn write_private(root: &Utf8Path, rel: &str, bytes: &[u8]) -> io::Result<()> {
        write_file(root, rel, bytes)
    }

    /// See `HostFs::set_mode`: no permission bits on these platforms, nothing to set.
    pub(super) fn set_mode(root: &Utf8Path, rel: &str, _mode: u32) -> io::Result<()> {
        full(root, rel).map(drop)
    }

    /// A symlink at `path` holding `target`: a file symlink, which needs a privilege Windows
    /// does not give by default (the volume probe finds out).
    #[cfg(windows)]
    fn make_symlink(target: &str, path: &Utf8Path) -> io::Result<()> {
        std::os::windows::fs::symlink_file(target, path)
    }

    /// No symlinks on this platform.
    #[cfg(not(windows))]
    fn make_symlink(_target: &str, _path: &Utf8Path) -> io::Result<()> {
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }
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
#[non_exhaustive]
pub enum HostMethod {
    /// [`HostFs::write_file`] and [`HostFs::write_private`].
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

/// One call [`RecordingHost`] saw: the method and the path it named, a write's root joined to
/// its path below it (`from` for a rename).
#[cfg(feature = "test-hooks")]
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct HostCall {
    /// The method.
    pub method: HostMethod,
    /// The path.
    pub path: Utf8PathBuf,
}

/// A host that forwards every call to `inner` and records it, in order: what a test counts to
/// see that an update writes only what changed (feature `test-hooks`).
#[cfg(feature = "test-hooks")]
#[non_exhaustive]
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
    fn write_file(&self, root: &Utf8Path, rel: &str, bytes: &[u8]) -> io::Result<()> {
        self.record(HostMethod::WriteFile, &root.join(rel));
        self.inner.write_file(root, rel, bytes)
    }

    fn symlink(&self, root: &Utf8Path, rel: &str, target: &str) -> io::Result<()> {
        self.record(HostMethod::Symlink, &root.join(rel));
        self.inner.symlink(root, rel, target)
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

    fn remove_file(&self, root: &Utf8Path, rel: &str) -> io::Result<()> {
        self.record(HostMethod::RemoveFile, &root.join(rel));
        self.inner.remove_file(root, rel)
    }

    fn remove_dir_if_empty(&self, root: &Utf8Path, rel: &str) -> io::Result<bool> {
        self.record(HostMethod::RemoveDirIfEmpty, &root.join(rel));
        self.inner.remove_dir_if_empty(root, rel)
    }

    fn create_dir_all(&self, root: &Utf8Path, rel: &str) -> io::Result<()> {
        self.record(HostMethod::CreateDirAll, &root.join(rel));
        self.inner.create_dir_all(root, rel)
    }

    fn rename(&self, root: &Utf8Path, from: &str, to: &str) -> io::Result<()> {
        self.record(HostMethod::Rename, &root.join(from));
        self.inner.rename(root, from, to)
    }

    fn read_link(&self, path: &Utf8Path) -> io::Result<String> {
        self.record(HostMethod::ReadLink, path);
        self.inner.read_link(path)
    }

    fn set_mode(&self, root: &Utf8Path, rel: &str, mode: u32) -> io::Result<()> {
        self.record(HostMethod::SetMode, &root.join(rel));
        self.inner.set_mode(root, rel, mode)
    }

    fn write_private(&self, root: &Utf8Path, rel: &str, bytes: &[u8]) -> io::Result<()> {
        self.record(HostMethod::WriteFile, &root.join(rel));
        self.inner.write_private(root, rel, bytes)
    }
}

/// Fault injection for crash tests (feature `test-hooks`), as the store's `CrashAtFs`: forwards
/// to `inner`, except that the k-th call (counting every [`HostFs`] method, from 1) aborts the
/// process with [`std::process::abort`] instead of running, as a crash at that exact point
/// would. It counts every call ([`CrashAtHost::calls`]). A [`HostFs::write_private`] runs as the
/// trait's default, a write then a mode: two calls, as through a wrapper that does not forward
/// it.
#[cfg(feature = "test-hooks")]
#[non_exhaustive]
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
    fn write_file(&self, root: &Utf8Path, rel: &str, bytes: &[u8]) -> io::Result<()> {
        self.tick();
        self.inner.write_file(root, rel, bytes)
    }

    fn symlink(&self, root: &Utf8Path, rel: &str, target: &str) -> io::Result<()> {
        self.tick();
        self.inner.symlink(root, rel, target)
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

    fn remove_file(&self, root: &Utf8Path, rel: &str) -> io::Result<()> {
        self.tick();
        self.inner.remove_file(root, rel)
    }

    fn remove_dir_if_empty(&self, root: &Utf8Path, rel: &str) -> io::Result<bool> {
        self.tick();
        self.inner.remove_dir_if_empty(root, rel)
    }

    fn create_dir_all(&self, root: &Utf8Path, rel: &str) -> io::Result<()> {
        self.tick();
        self.inner.create_dir_all(root, rel)
    }

    fn rename(&self, root: &Utf8Path, from: &str, to: &str) -> io::Result<()> {
        self.tick();
        self.inner.rename(root, from, to)
    }

    fn read_link(&self, path: &Utf8Path) -> io::Result<String> {
        self.tick();
        self.inner.read_link(path)
    }

    fn set_mode(&self, root: &Utf8Path, rel: &str, mode: u32) -> io::Result<()> {
        self.tick();
        self.inner.set_mode(root, rel, mode)
    }
}
