//! One append-only file of the changeset index: entries of `W` bytes, entry `i` at byte
//! `i × W`. It is read with `read_at` on a read-only handle, and changed only through
//! [`FsOps`](crate::FsOps), by way of the index's [`StoreFs`] (which names a failure's file):
//! appends at its end, and cuts of its tail.
//!
//! The four columns hold one entry per record (`W` = 32 or 8). `parents.log`, whose records'
//! parents `par.col` finds by byte offset, is a column of bytes (`W` = 1).

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_core::bytes;

use super::sorted;
use crate::error::{FileOp, IoFailure, StoreError};
use crate::fs_ops::{self, StoreFs};

/// See the module documentation.
pub(crate) struct AppendOnlyColumn<const W: usize> {
    path: Utf8PathBuf,
    /// Read-only.
    file: File,
    /// What the column changes the file through, and locates its failures with.
    fs: StoreFs,
}

impl<const W: usize> AppendOnlyColumn<W> {
    /// Opens the file at `path` read-only, changes going through `fs`. A missing one is
    /// [`StoreError::IndexCorrupt`], `missing` saying which: a store has every index file from
    /// its creation.
    pub(crate) fn open(
        fs: &StoreFs,
        path: Utf8PathBuf,
        missing: &'static str,
    ) -> Result<Self, StoreError> {
        let file = fs_ops::open_regular(&path)
            .map_err(|e| sorted::index_file_error(fs, &path, e, missing))?;
        Ok(Self {
            path,
            file,
            fs: fs.clone(),
        })
    }

    pub(crate) fn path(&self) -> &Utf8Path {
        &self.path
    }

    /// The file's length in bytes (one `fstat`), a partial entry at its end included.
    pub(crate) fn len(&self) -> Result<u64, IoFailure> {
        let meta = (self.file.metadata()).map_err(|e| self.failed(FileOp::Stat, e))?;
        Ok(meta.len())
    }

    /// How many whole entries `len` bytes of the file hold.
    pub(crate) fn whole(len: u64) -> u64 {
        len / bytes::to_u64(W)
    }

    /// Whether the file holds exactly `entries` entries.
    pub(crate) fn holds(&self, entries: u64) -> Result<bool, IoFailure> {
        Ok(self.len()? == entries * bytes::to_u64(W))
    }

    /// Entry `i`, read with one `read_at`.
    pub(crate) fn entry(&self, i: u64) -> Result<[u8; W], IoFailure> {
        let mut entry = [0; W];
        self.read_at(i, &mut entry)?;
        Ok(entry)
    }

    /// Fills `buf`, a whole number of entries, with the entries from entry `first` on, with one
    /// `read_at` (none for an empty `buf`).
    pub(crate) fn read_at(&self, first: u64, buf: &mut [u8]) -> Result<(), IoFailure> {
        if buf.is_empty() {
            return Ok(());
        }
        (self.file.read_exact_at(buf, first * bytes::to_u64(W)))
            .map_err(|e| self.failed(FileOp::ReadAt, e))
    }

    /// The entries `[from, to)`, read with one `read_at` (none for no entries). A range that
    /// starts past its end is [`StoreError::IndexCorrupt`]: the index scans its columns from
    /// where a sorted view ends, and a view never covers more records than the index counts.
    pub(crate) fn entries(&self, from: u64, to: u64) -> Result<Vec<u8>, StoreError> {
        let Some(count) = to.checked_sub(from) else {
            return Err(StoreError::IndexCorrupt {
                what: "a sorted view covers records past those the index counts",
            });
        };
        let len = usize::try_from(count * bytes::to_u64(W))
            .map_err(|_| self.failed(FileOp::ReadAt, io::ErrorKind::OutOfMemory.into()))?;
        let mut bytes = vec![0; len];
        self.read_at(from, &mut bytes)?;
        Ok(bytes)
    }

    /// Appends `entries`, a whole number of them, with one [`crate::FsOps::append`]. The file
    /// must hold exactly `at` entries until then, as the caller counts them: one that does not
    /// changed under the caller, [`StoreError::IndexCorrupt`].
    pub(crate) fn append(&self, entries: &[u8], at: u64) -> Result<(), StoreError> {
        if self.fs.append(self.path.as_std_path(), entries)? == at * bytes::to_u64(W) {
            Ok(())
        } else {
            Err(StoreError::IndexCorrupt {
                what: "an index file changed during an append: a writer without meta/lock?",
            })
        }
    }

    /// Cuts the file, `len` bytes long as the caller measured it, back to its first `keep`
    /// entries with one [`crate::FsOps::truncate`] (which syncs the cut), if it is longer; returns
    /// how many entries were cut, a partial one counting as one.
    pub(crate) fn truncate(&self, len: u64, keep: u64) -> Result<u64, StoreError> {
        let keep_len = keep * bytes::to_u64(W);
        if len <= keep_len {
            return Ok(0);
        }
        self.fs.truncate(self.path.as_std_path(), keep_len)?;
        Ok(len.div_ceil(bytes::to_u64(W)) - keep)
    }

    /// The failure `e` of `op` on this file, located; the index's own refusals on it too.
    pub(crate) fn failed(&self, op: FileOp, e: io::Error) -> IoFailure {
        self.fs.locate(op, &self.path, e)
    }
}
