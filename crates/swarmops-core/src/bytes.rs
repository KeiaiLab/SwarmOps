//! A bounds-checked cursor for decoding byte layouts: every read checks that the bytes it
//! takes are there before it takes them, so input cut short or crafted gives an error, never a
//! panic. The object codec reads with it, and so do the store's journal records.
//!
//! [`to_u64`] turns a `usize` length, count or offset into the `u64` that the byte layouts,
//! the store and the API hold.

/// A cursor over an immutable buffer.
///
/// A read that would run past the end takes nothing and fails with `error_at(pos)`, `pos`
/// being where the read starts: each decoder says what that error is (the object codec's is
/// [`crate::CodecError::Truncated`] wherever it happens, a journal record's names the
/// position).
pub struct Reader<'a, E> {
    buf: &'a [u8],
    pos: usize,
    error_at: fn(usize) -> E,
}

impl<'a, E> Reader<'a, E> {
    /// A cursor at the start of `buf`, whose reads past the end fail with `error_at`.
    pub fn new(buf: &'a [u8], error_at: fn(usize) -> E) -> Self {
        Self {
            buf,
            pos: 0,
            error_at,
        }
    }

    /// Where the next read starts: how many bytes have been read.
    pub fn pos(&self) -> usize {
        self.pos
    }

    /// The bytes not read yet.
    pub fn rest(&self) -> &'a [u8] {
        self.buf.get(self.pos..).unwrap_or_default()
    }

    /// The error a read past the end starting at `pos` gives: for a decoder that finds bytes
    /// it cannot accept at a position it noted.
    pub fn error_at(&self, pos: usize) -> E {
        debug_assert!(
            pos <= self.buf.len(),
            "a decoder notes only positions inside the buffer it reads"
        );
        (self.error_at)(pos)
    }

    /// The next `n` bytes.
    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8], E> {
        let bytes = self
            .rest()
            .get(..n)
            .ok_or_else(|| self.error_at(self.pos))?;
        self.pos += n;
        Ok(bytes)
    }

    /// The next `N` bytes, as an array.
    pub fn array<const N: usize>(&mut self) -> Result<[u8; N], E> {
        let bytes = *self
            .rest()
            .first_chunk::<N>()
            .ok_or_else(|| self.error_at(self.pos))?;
        self.pos += N;
        Ok(bytes)
    }

    /// The next byte.
    pub fn u8(&mut self) -> Result<u8, E> {
        self.array().map(|[byte]| byte)
    }

    /// The next 8 bytes, as a little-endian `u64`.
    pub fn u64_le(&mut self) -> Result<u64, E> {
        self.array().map(u64::from_le_bytes)
    }

    /// The next 4 bytes, as a little-endian `i32`.
    pub fn i32_le(&mut self) -> Result<i32, E> {
        self.array().map(i32::from_le_bytes)
    }

    /// The next 8 bytes, as a little-endian `i64`.
    pub fn i64_le(&mut self) -> Result<i64, E> {
        self.array().map(i64::from_le_bytes)
    }
}

/// `n`, a length, count or offset held in a `usize`, as the `u64` the byte layouts, the
/// store and the API use. Lossless: no target Rust supports has a `usize` wider than 64 bits,
/// as the assertion below checks when the crate compiles, so this never panics.
// Inlined into the other crates, as the private helpers this replaced were in their own.
#[inline]
pub fn to_u64(n: usize) -> u64 {
    u64::try_from(n).expect("a usize fits in a u64")
}

const _: () = assert!(
    usize::BITS <= u64::BITS,
    "to_u64 is lossless only where a usize fits in a u64"
);

#[cfg(test)]
mod tests {
    use super::to_u64;

    #[test]
    fn to_u64_is_lossless() {
        assert_eq!(to_u64(0), 0);
        assert_eq!(to_u64(1 << 20), 1 << 20);
        assert_eq!(to_u64(usize::MAX), u64::try_from(usize::MAX).unwrap());
    }
}
