//! Unsigned LEB128 varints, at most 10 bytes, minimal encoding only.

use super::codec::CodecError;

const MAX_BYTES: usize = 10;

/// Appends the minimal LEB128 encoding of `v`.
pub fn write(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let byte = u8::try_from(v & 0x7f).expect("masked to 7 bits");
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// Reads one minimal LEB128 varint at `*pos`, advancing it.
///
/// Errors with [`CodecError::Truncated`] when the buffer ends mid-varint and
/// [`CodecError::Overlong`] for more than 10 bytes, a value past `u64::MAX`, or a
/// non-minimal encoding (a trailing zero group).
pub fn read(buf: &[u8], pos: &mut usize) -> Result<u64, CodecError> {
    let mut value: u64 = 0;
    for i in 0..MAX_BYTES {
        let byte = *buf.get(*pos + i).ok_or(CodecError::Truncated)?;
        let group = u64::from(byte & 0x7f);
        if i == MAX_BYTES - 1 && group > 1 {
            return Err(CodecError::Overlong);
        }
        value |= group << (7 * i);
        if byte & 0x80 == 0 {
            if i > 0 && group == 0 {
                return Err(CodecError::Overlong);
            }
            *pos += i + 1;
            return Ok(value);
        }
    }
    Err(CodecError::Overlong)
}
