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
//! ```
//!
//! The M5 plan's task W1 froze the trait; task W5 writes `RealHost`, `RecordingHost` and
//! `CrashAtHost`. Until then [`RealHost`] fails every call, as the operations it serves do.

use std::io;

use camino::Utf8Path;

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
}

/// The host's own filesystem. Until the M5 plan's task W5 writes it, every call fails with
/// [`io::ErrorKind::Unsupported`].
#[derive(Debug, Clone, Copy, Default)]
pub struct RealHost;

/// The error of every call of the unbuilt [`RealHost`].
fn not_built() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "RealHost is not built yet (the M5 plan's task W5)",
    )
}

impl HostFs for RealHost {
    fn write_file(&self, _path: &Utf8Path, _bytes: &[u8]) -> io::Result<()> {
        Err(not_built())
    }

    fn symlink(&self, _target: &str, _path: &Utf8Path) -> io::Result<()> {
        Err(not_built())
    }

    fn symlink_metadata(&self, _path: &Utf8Path) -> io::Result<Option<HostMeta>> {
        Err(not_built())
    }

    fn read_file(&self, _path: &Utf8Path) -> io::Result<Vec<u8>> {
        Err(not_built())
    }

    fn read_dir(&self, _path: &Utf8Path) -> io::Result<Vec<String>> {
        Err(not_built())
    }

    fn remove_file(&self, _path: &Utf8Path) -> io::Result<()> {
        Err(not_built())
    }

    fn remove_dir_if_empty(&self, _path: &Utf8Path) -> io::Result<bool> {
        Err(not_built())
    }

    fn create_dir_all(&self, _path: &Utf8Path) -> io::Result<()> {
        Err(not_built())
    }

    fn rename(&self, _from: &Utf8Path, _to: &Utf8Path) -> io::Result<()> {
        Err(not_built())
    }
}
