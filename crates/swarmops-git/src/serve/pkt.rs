//! git's pkt-line framing (`gitprotocol-common`): four hex digits of length, then the payload;
//! `0000` flush, `0001` delimiter. A refusal before any protocol byte is one `ERR` line.

use std::io::{self, Read, Write};

/// The most bytes a pkt-line holds, its four length digits included.
const MAX_PKT_LEN: usize = 65_520;

/// The length digits of a pkt-line.
const LEN_DIGITS: usize = 4;

/// What starts the payload of an error line.
const ERR_PREFIX: &str = "ERR ";

/// One pkt-line.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Pkt {
    /// A data line's payload.
    Data(Vec<u8>),
    /// `0000`.
    Flush,
    /// `0001`.
    Delim,
}

/// The next pkt-line of `input`; `None` at its end.
pub fn read(input: &mut impl Read) -> io::Result<Option<Pkt>> {
    let _ = input;
    Err(io::Error::other("pkt-line reading: not built"))
}

/// Writes `payload` as one data line; one longer than a pkt-line holds is refused.
pub fn write_data(output: &mut impl Write, payload: &[u8]) -> io::Result<()> {
    let len = payload.len() + LEN_DIGITS;
    if len > MAX_PKT_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("a pkt-line holds at most {MAX_PKT_LEN} bytes, not {len}"),
        ));
    }
    write!(output, "{len:04x}")?;
    output.write_all(payload)
}

/// Writes a flush line, `0000`.
pub fn write_flush(output: &mut impl Write) -> io::Result<()> {
    output.write_all(b"0000")
}

/// Writes `ERR <message>`, the line git prints as `remote error: <message>`.
pub fn write_err(output: &mut impl Write, message: &str) -> io::Result<()> {
    write_data(output, format!("{ERR_PREFIX}{message}").as_bytes())
}
