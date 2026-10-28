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

/// The length a flush line, `0000`, gives.
const FLUSH: usize = 0;

/// The length a delimiter line, `0001`, gives.
const DELIM: usize = 1;

/// The next pkt-line of `input`; `None` at its end. A length under 4 other than flush and
/// delimiter, over 65,520 or not four hex digits is `InvalidData`; a line cut short is
/// `UnexpectedEof`.
pub fn read(input: &mut impl Read) -> io::Result<Option<Pkt>> {
    // 1. The length digits: none at all is the end of the input.
    let mut digits = [0; LEN_DIGITS];
    let got = fill(input, &mut digits)?;
    if got == 0 {
        return Ok(None);
    }
    if got < LEN_DIGITS {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }

    // 2. The two special lines, then the bounds of a data line.
    let len = length(digits)?;
    match len {
        FLUSH => return Ok(Some(Pkt::Flush)),
        DELIM => return Ok(Some(Pkt::Delim)),
        _ => {}
    }
    if !(LEN_DIGITS..=MAX_PKT_LEN).contains(&len) {
        return Err(invalid(&format!("a pkt-line length of {len}")));
    }

    // 3. The payload, whole.
    let mut payload = vec![0; len - LEN_DIGITS];
    input.read_exact(&mut payload)?;
    Ok(Some(Pkt::Data(payload)))
}

/// Reads into `buf` until it is full or `input` ends; how many bytes it got.
fn fill(input: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut got = 0;
    while got < buf.len() {
        match input.read(&mut buf[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(got)
}

/// The length that four hex digits, of either case, write; any other byte (`+`, `g`) refused.
fn length(digits: [u8; LEN_DIGITS]) -> io::Result<usize> {
    digits.iter().try_fold(0, |len, &digit| {
        let value = match digit {
            b'0'..=b'9' => digit - b'0',
            b'a'..=b'f' => digit - b'a' + 10,
            b'A'..=b'F' => digit - b'A' + 10,
            _ => return Err(invalid(&format!("a pkt-line length digit {digit:#04x}"))),
        };
        Ok(len << 4 | usize::from(value))
    })
}

/// An `InvalidData` error saying `what` is not a pkt-line.
fn invalid(what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("{what} is not a pkt-line"),
    )
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
