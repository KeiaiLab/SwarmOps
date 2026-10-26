//! The store formats and their `FORMAT` markers (the M6 design §2.1): the one place that names
//! them.
//!
//! ```text
//! <root>/FORMAT   "swarmops-store 1\n"   FORMAT 1: journal.*, refs/
//!                 "swarmops-store 2\n"   FORMAT 2: journal2.*, refs2/
//! ```
//!
//! Both markers are 17 bytes long, so a reader that refuses a longer `FORMAT` unread refuses
//! the same files for both.

use std::io::{self, Read};

use swarmops_core::bytes;

use crate::error::{FileOp, StoreError};
use crate::fs_ops::StoreFs;
use crate::layout::Layout;

/// The `FORMAT` marker of FORMAT 1.
const MARKER_V1: &[u8] = b"swarmops-store 1\n";

/// The `FORMAT` marker of FORMAT 2.
const MARKER_V2: &[u8] = b"swarmops-store 2\n";

/// The longest marker, in bytes: a `FORMAT` file longer than this names no format.
pub const MAX_MARKER_LEN: usize = 17;

/// A store's on-disk format: which journal, offset, checkpoint and ref files it has, and
/// which ref names it takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum StoreFormat {
    /// FORMAT 1: `journal.{log,off,ckpt}`, `refs/<name>`, refs `main` and `release/<x>`.
    V1,
    /// FORMAT 2: `journal2.{log,off,ckpt}`, `refs2/<hash>`, free-form branches, tags and
    /// remote-tracking refs.
    V2,
}

/// The format a create writes when the request names none.
pub const DEFAULT_INIT_FORMAT: StoreFormat = StoreFormat::V1;

/// The formats a create writes.
pub const INIT_FORMATS: &[StoreFormat] = &[StoreFormat::V1];

/// What an upgrade writes over FORMAT 1's `journal.ckpt` before it replaces `FORMAT`: not 8
/// bytes, so every FORMAT 1 reader and writer fails closed from then on.
pub const UPGRADE_POISON: &[u8] = b"swarmops-store 2\n";

impl StoreFormat {
    /// The version number `FORMAT` names: 1 or 2.
    pub fn version(self) -> u32 {
        match self {
            Self::V1 => 1,
            Self::V2 => 2,
        }
    }

    /// The format whose number is `version`; `None` for one this build does not know.
    pub fn from_version(version: u32) -> Option<StoreFormat> {
        match version {
            1 => Some(Self::V1),
            2 => Some(Self::V2),
            _ => None,
        }
    }

    /// The bytes of its `FORMAT` file.
    pub fn marker(self) -> &'static [u8] {
        match self {
            Self::V1 => MARKER_V1,
            Self::V2 => MARKER_V2,
        }
    }

    /// The format whose marker is `bytes`, exactly; `None` for any other bytes.
    pub fn from_marker(bytes: &[u8]) -> Option<StoreFormat> {
        [Self::V1, Self::V2]
            .into_iter()
            .find(|format| format.marker() == bytes)
    }
}

/// The format `layout`'s `FORMAT` file names, read through `fs`. [`StoreError::Format`] for a
/// missing file (no store there), one longer than [`MAX_MARKER_LEN`] (measured before it is
/// read, so a crafted file of huge length is never read into memory) and one that names no
/// format this build knows. The messages name the file relative to the store's root.
pub fn read_format(layout: &Layout, fs: &StoreFs) -> Result<StoreFormat, StoreError> {
    let file = layout.format_file();

    // 1. Refuse a long file unread; a missing one, or no regular file, reaches the read.
    if let Ok(meta) = std::fs::metadata(file.as_std_path())
        && meta.is_file()
        && meta.len() > bytes::to_u64(MAX_MARKER_LEN)
    {
        return Err(StoreError::Format(format!(
            "FORMAT is {} bytes long, not a store format this build reads",
            meta.len()
        )));
    }

    // 2. Read it and name its format.
    let Some(bytes) = fs.read(file.as_std_path())? else {
        return Err(StoreError::Format("no FORMAT file: not a store".to_owned()));
    };
    StoreFormat::from_marker(&bytes).ok_or_else(|| {
        StoreError::Format("FORMAT names a store format this build does not read".to_owned())
    })
}

/// [`read_format`], except that a missing `FORMAT` is FORMAT 1, what a metadata store opened
/// on its own (no repository around it, as the store's tests make) has always been; and that it
/// reads off the [`crate::FsOps`] seam, through `std::fs`, as the metadata's own reads do, so
/// that a metadata store's open makes the calls through the seam it always made. At most
/// [`MAX_MARKER_LEN`] + 1 bytes are read.
pub(crate) fn read_format_or_v1(layout: &Layout, fs: &StoreFs) -> Result<StoreFormat, StoreError> {
    let file = layout.format_file();
    let opened = match std::fs::File::open(file.as_std_path()) {
        Ok(opened) => opened,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(StoreFormat::V1),
        Err(e) => return Err(fs.locate(FileOp::Open, &file, e).into()),
    };
    let mut bytes = Vec::with_capacity(MAX_MARKER_LEN + 1);
    io::Read::take(opened, bytes::to_u64(MAX_MARKER_LEN + 1))
        .read_to_end(&mut bytes)
        .map_err(|e| fs.locate(FileOp::ReadAt, &file, e))?;
    StoreFormat::from_marker(&bytes).ok_or_else(|| {
        StoreError::Format("FORMAT names a store format this build does not read".to_owned())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_markers_round_trip() {
        for format in [StoreFormat::V1, StoreFormat::V2] {
            assert_eq!(StoreFormat::from_marker(format.marker()), Some(format));
            assert_eq!(StoreFormat::from_version(format.version()), Some(format));
            assert_eq!(format.marker().len(), MAX_MARKER_LEN);
        }
        assert_eq!(StoreFormat::V1.marker(), b"swarmops-store 1\n");
        assert_eq!(StoreFormat::V2.marker(), b"swarmops-store 2\n");
        assert_eq!(StoreFormat::from_marker(b"swarmops-store 3\n"), None);
        assert_eq!(StoreFormat::from_marker(b"swarmops-store 1"), None);
        assert_eq!(StoreFormat::from_version(3), None);
        assert_eq!(UPGRADE_POISON, StoreFormat::V2.marker());
    }
}
