//! Shared decoding error, and the codec's reads on the bounds-checked cursor.

use thiserror::Error;

use super::varint;
use crate::bytes;
use crate::error::PathError;

/// Why bytes are not a canonical object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum CodecError {
    /// The four magic bytes did not match the object kind.
    #[error("bad magic")]
    Magic,
    /// A layout version this build does not know.
    #[error("unsupported layout version {0}")]
    Version(u8),
    /// The buffer ended before the layout did.
    #[error("truncated input")]
    Truncated,
    /// A varint longer than 10 bytes, past `u64::MAX`, or not minimal.
    #[error("overlong or non-minimal varint")]
    Overlong,
    /// An entry kind byte outside 0..=3, or `Dir` where a leaf is required.
    #[error("bad entry kind {0}")]
    BadKind(u8),
    /// A delta op byte outside 0..=1.
    #[error("bad delta op {0}")]
    BadOp(u8),
    /// A flag byte outside 0..=1.
    #[error("bad flag byte {0}")]
    BadFlag(u8),
    /// Entries or delta paths are not in ascending byte order.
    #[error("entries are not sorted")]
    Unsorted,
    /// Two entries or delta paths are equal.
    #[error("duplicate entry")]
    Duplicate,
    /// A name or path failed [`crate::RepoPath`] validation.
    #[error("invalid path: {0}")]
    BadPath(#[from] PathError),
    /// A name, path, or metadata key is not UTF-8.
    #[error("invalid UTF-8")]
    Utf8,
    /// Metadata keys are not strictly ascending.
    #[error("metadata keys are not sorted and unique")]
    UnsortedMetadata,
    /// `Tree::new` was given no entries; only the root may be empty.
    #[error("a non-root tree cannot be empty")]
    EmptyNonRootTree,
    /// Bytes remained after the layout ended.
    #[error("trailing bytes after object")]
    TrailingBytes,
    /// `negative_zero` was set with a non-zero `offset_secs`.
    #[error("signature has the -0000 flag with a non-zero offset")]
    BadSignature,
    /// A `Delete` entry carries `renamed_from`, or `renamed_from` equals the entry's own path.
    #[error("delete cannot carry renamed_from, and renamed_from cannot equal path")]
    BadRename,
}

/// The cursor the codec decodes with: a read past the end is [`CodecError::Truncated`].
pub(crate) type Reader<'a> = bytes::Reader<'a, CodecError>;

/// A cursor at the start of `buf`.
pub(crate) fn reader(buf: &[u8]) -> Reader<'_> {
    Reader::new(buf, |_| CodecError::Truncated)
}

impl<'a> Reader<'a> {
    pub(crate) fn magic(&mut self, want: [u8; 4], version: u8) -> Result<(), CodecError> {
        if self.bytes(4)? != want {
            return Err(CodecError::Magic);
        }
        let v = self.u8()?;
        if v != version {
            return Err(CodecError::Version(v));
        }
        Ok(())
    }

    pub(crate) fn varint(&mut self) -> Result<u64, CodecError> {
        let mut len = 0;
        let value = varint::read(self.rest(), &mut len)?;
        self.bytes(len)?;
        Ok(value)
    }

    /// A varint count that must fit in `usize` and be no larger than the bytes left.
    pub(crate) fn count(&mut self) -> Result<usize, CodecError> {
        let n = usize::try_from(self.varint()?).map_err(|_| CodecError::Truncated)?;
        if n > self.rest().len() {
            return Err(CodecError::Truncated);
        }
        Ok(n)
    }

    pub(crate) fn len_prefixed(&mut self) -> Result<&'a [u8], CodecError> {
        let n = self.count()?;
        self.bytes(n)
    }

    pub(crate) fn utf8(&mut self) -> Result<String, CodecError> {
        let s = self.len_prefixed()?;
        String::from_utf8(s.to_vec()).map_err(|_| CodecError::Utf8)
    }

    pub(crate) fn flag(&mut self) -> Result<bool, CodecError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            b => Err(CodecError::BadFlag(b)),
        }
    }

    pub(crate) fn finish(self) -> Result<(), CodecError> {
        if self.rest().is_empty() {
            Ok(())
        } else {
            Err(CodecError::TrailingBytes)
        }
    }
}

/// Appends `varint(len) ++ bytes`.
pub(crate) fn put_len_prefixed(out: &mut Vec<u8>, bytes: &[u8]) {
    varint::write(out, u64::try_from(bytes.len()).expect("length fits in u64"));
    out.extend_from_slice(bytes);
}

/// Appends `varint(n)`.
pub(crate) fn put_count(out: &mut Vec<u8>, n: usize) {
    varint::write(out, u64::try_from(n).expect("count fits in u64"));
}

/// Checks that `items` is strictly ascending under `key`.
pub(crate) fn check_strictly_sorted<T, K: Ord + ?Sized>(
    items: &[T],
    key: impl Fn(&T) -> &K,
) -> Result<(), CodecError> {
    for pair in items.windows(2) {
        match key(&pair[0]).cmp(key(&pair[1])) {
            std::cmp::Ordering::Less => {}
            std::cmp::Ordering::Equal => return Err(CodecError::Duplicate),
            std::cmp::Ordering::Greater => return Err(CodecError::Unsorted),
        }
    }
    Ok(())
}
