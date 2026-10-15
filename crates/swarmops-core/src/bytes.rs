//! A bounds-checked cursor for decoding byte layouts: every read checks that the bytes it
//! takes are there before it takes them, so input cut short or crafted gives an error, never a
//! panic. The object codec reads with it, and so do the store's journal records.

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
        debug_assert!(pos <= self.buf.len());
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
