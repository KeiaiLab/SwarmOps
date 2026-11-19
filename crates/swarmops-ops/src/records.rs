//! The line-per-record diagnostics files: `serve`'s (the M17 design §6), the operations' (the
//! M21 design §2.2) and the check runs' run lines (M31-11, `check::lines`), one copy of their
//! append, rotation and read.
//!
//! ```text
//!   append: one line < 4 KiB, one O_APPEND write, unsynced; the file a regular one, a link
//!           not followed and a FIFO not waited on (the lock too)
//!           └─ past the bound: the writer that takes <lock> renames <current> to <older>
//!   lines:  <older>, then <current>; a torn last line is the reader's to skip
//! ```
//!
//! Diagnostics, not acknowledged state: nothing is synced and nothing reads them for
//! correctness.

use std::fs::{self, File, TryLockError};
use std::io::{self, BufRead as _, BufReader, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;

use serde::Serialize;

/// The longest line appended: one `write` of it is not interleaved with another's.
pub(crate) const MAX_LINE: usize = 4 << 10;

/// How a file written here is opened: a link is not followed and a FIFO does not block the open,
/// since the directory may be one several accounts write.
const OPEN_FLAGS: i32 = libc::O_NOFOLLOW | libc::O_NONBLOCK;

/// The files of one kind of record, in their directory.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Files {
    /// The file appended to.
    pub(crate) current: &'static str,
    /// The one older file, which a rotation replaces.
    pub(crate) older: &'static str,
    /// The lock of the rotation.
    pub(crate) lock: &'static str,
}

/// The serve records' directory, below the store's root: its git directory.
pub(crate) const SERVE_DIR: &str = "git";

/// The serve records' files, in [`SERVE_DIR`].
pub(crate) const SERVE: Files = Files {
    current: "serve-records.jsonl",
    older: "serve-records.1.jsonl",
    lock: "serve-records.lock",
};

/// Appends `record` as one line to `files`' current file in `dir`, in one `write`; past
/// `bound` bytes, renamed to the older file by the writer that takes the rotation's lock.
pub(crate) fn append<T: Serialize>(
    dir: &Path,
    files: &Files,
    record: &T,
    bound: u64,
) -> io::Result<()> {
    // 1. The line, short enough that one write of it is whole.
    let mut line = serde_json::to_vec(record).map_err(io::Error::other)?;
    line.push(b'\n');
    if line.len() > MAX_LINE {
        return Err(io::Error::other("a record past 4 KiB"));
    }

    // 2. Appended, unsynced: diagnostics.
    let path = dir.join(files.current);
    let mut file = File::options()
        .append(true)
        .create(true)
        .custom_flags(OPEN_FLAGS)
        .open(&path)?;
    regular(&file)?;
    file.write_all(&line)?;
    if file.metadata()?.len() <= bound {
        return Ok(());
    }

    // 3. Rotated by the writer that takes the lock, no wait; past the bound still, since
    //    another may have rotated first.
    rotate(dir, files, bound)
}

/// Renames `files`' current file in `dir` to the older file when it is past `bound`, holding
/// the rotation's lock; another holder's lock leaves it to them.
pub(crate) fn rotate(dir: &Path, files: &Files, bound: u64) -> io::Result<()> {
    let lock = File::options()
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags(OPEN_FLAGS)
        .open(dir.join(files.lock))?;
    regular(&lock)?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Ok(()),
        Err(TryLockError::Error(e)) => return Err(e),
    }
    let path = dir.join(files.current);
    let len = fs::metadata(&path).map_or(0, |meta| meta.len());
    if len > bound {
        fs::rename(&path, dir.join(files.older))?;
    }
    Ok(())
}

/// An error unless `file`, opened to write, is a regular file: nothing is written to a device
/// or a FIFO left where a records file belongs.
fn regular(file: &File) -> io::Result<()> {
    if file.metadata()?.is_file() {
        return Ok(());
    }
    Err(io::Error::other("not a regular file"))
}

/// The lines of `files` in `dir`, streamed: the older file's, then the current one's. A file
/// that is not there has none; one that does not open is one error.
pub(crate) fn lines(dir: &Path, files: &Files) -> impl Iterator<Item = io::Result<String>> + use<> {
    // Both opened first, so a rotation between the two reads no file twice.
    let older = open(&dir.join(files.older));
    let current = open(&dir.join(files.current));
    [older, current].into_iter().flat_map(|opened| {
        let (reader, error) = match opened {
            Ok(file) => (file.map(|f| BufReader::new(f).lines()), None),
            Err(e) => (None, Some(Err(e))),
        };
        error.into_iter().chain(reader.into_iter().flatten())
    })
}

/// The file at `path` for reading: none when it is not there.
fn open(path: &Path) -> io::Result<Option<File>> {
    match File::open(path) {
        Ok(file) => Ok(Some(file)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}
