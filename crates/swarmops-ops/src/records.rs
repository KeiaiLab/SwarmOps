//! The store's line-per-record diagnostics files: `serve`'s (the M17 design §6) and the
//! operations' (the M21 design §2.2), one copy of their append and rotation.
//!
//! ```text
//!   append: one line < 4 KiB, one O_APPEND write, unsynced
//!           └─ past the bound: the writer that takes <lock> renames <current> to <older>
//! ```
//!
//! Diagnostics, not acknowledged state: nothing is synced and nothing reads them for
//! correctness.

use std::fs::{self, File, TryLockError};
use std::io::{self, Write as _};
use std::path::Path;

use serde::Serialize;

/// The longest line appended: one `write` of it is not interleaved with another's.
const MAX_LINE: usize = 4 << 10;

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
    let mut file = File::options().append(true).create(true).open(&path)?;
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
        .open(dir.join(files.lock))?;
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
