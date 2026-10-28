//! git's `proc-receive` hook protocol, version 1 (`githooks(5)`): the version handshake, the
//! commands of a push, and a report per command (`ok`, with its `option` lines, or `ng`).

use std::io::{self, Read, Write};

use super::pkt::{self, Pkt};
use crate::oid::GitOid;

/// The protocol version spoken, the only one `githooks(5)` defines.
const VERSION: &str = "1";

/// What starts the version line.
const VERSION_KEY: &str = "version=";

/// Capabilities receive-pack may offer that this hook cannot honour: push options are not read,
/// and each ref has its own CAS, so a push cannot be atomic.
const REFUSED_CAPS: [&str; 2] = ["push-options", "atomic"];

/// One command of a push.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Command {
    /// What the client says the ref holds: `None` for one it creates.
    pub old: Option<GitOid>,
    /// What it moves the ref to: `None` for a deletion.
    pub new: Option<GitOid>,
    /// The ref, by its full name.
    pub refname: String,
}

impl Command {
    /// The command that moves `refname` from `old` to `new`.
    pub fn new(old: Option<GitOid>, new: Option<GitOid>, refname: impl Into<String>) -> Command {
        Command {
            old,
            new,
            refname: refname.into(),
        }
    }
}

/// The hook's report on one command.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Report {
    /// Done: `ok <refname>`, then an `option <key> <value>` line per option.
    Ok {
        /// The ref, by its full name.
        refname: String,
        /// `refname`, `old-oid`, `new-oid`, … in order.
        options: Vec<(String, String)>,
    },
    /// Refused: `ng <refname> <reason>`.
    Ng {
        /// The ref, by its full name.
        refname: String,
        /// Why, with no host path.
        reason: String,
    },
}

impl Report {
    /// `ok <refname>`, with no option.
    pub fn ok(refname: impl Into<String>) -> Report {
        Report::Ok {
            refname: refname.into(),
            options: Vec::new(),
        }
    }

    /// `ng <refname> <reason>`.
    pub fn ng(refname: impl Into<String>, reason: impl Into<String>) -> Report {
        Report::Ng {
            refname: refname.into(),
            reason: reason.into(),
        }
    }
}

/// The version handshake on `input` and `output`: receive-pack's version line and flush read,
/// then `version=1` answered with no capability, so receive-pack sends no push options. Another
/// version, or a receive-pack offering push options or asking for `atomic`, is `InvalidData` and
/// nothing is answered.
pub fn handshake(input: &mut impl Read, output: &mut impl Write) -> io::Result<()> {
    // 1. receive-pack's lines up to its flush; the first is the version line.
    let lines = data_until_flush(input)?;
    let first = lines
        .first()
        .ok_or_else(|| invalid("proc-receive: no version line".to_owned()))?;

    // 2. `version=1`, optionally a NUL and space-separated capabilities, the newline dropped.
    let line = text(first)?;
    let line = line.strip_suffix('\n').unwrap_or(line);
    let (version, caps) = line.split_once('\0').unwrap_or((line, ""));
    if version.strip_prefix(VERSION_KEY) != Some(VERSION) {
        return Err(invalid(format!("proc-receive: version line {version:?}")));
    }
    if let Some(cap) = caps.split(' ').find(|cap| REFUSED_CAPS.contains(cap)) {
        return Err(invalid(format!(
            "proc-receive: capability {cap:?} not served"
        )));
    }

    // 3. The answer: the same version, no capability, a flush.
    pkt::write_data(output, format!("{VERSION_KEY}{VERSION}\n").as_bytes())?;
    pkt::write_flush(output)?;
    output.flush()
}

/// The commands of the push, up to their flush: `<old> <new> <refname>` each, a zero id as
/// `None`. A malformed line or a delimiter is `InvalidData`; an end before the flush is
/// `UnexpectedEof`.
pub fn read_commands(input: &mut impl Read) -> io::Result<Vec<Command>> {
    data_until_flush(input)?
        .iter()
        .map(|line| command(text(line)?))
        .collect()
}

/// Writes `reports`, then a flush: `ok <refname>` and an `option <key> <value>` line per option
/// (`option <key>` for an empty value, as `forced-update`), or `ng <refname> <reason>`. A field
/// holding a newline would forge a line: `InvalidInput`, nothing written.
pub fn write_reports(output: &mut impl Write, reports: &[Report]) -> io::Result<()> {
    // 1. Every line formed first, so a refused field leaves `output` untouched.
    let mut lines = Vec::new();
    for report in reports {
        match report {
            Report::Ok { refname, options } => {
                lines.push(format!("ok {}", field(refname)?));
                for (key, value) in options {
                    let line = match value.as_str() {
                        "" => format!("option {}", field(key)?),
                        _ => format!("option {} {}", field(key)?, field(value)?),
                    };
                    lines.push(line);
                }
            }
            Report::Ng { refname, reason } => {
                lines.push(format!("ng {} {}", field(refname)?, field(reason)?));
            }
        }
    }

    // 2. One data line each, the flush.
    for line in lines {
        pkt::write_data(output, format!("{line}\n").as_bytes())?;
    }
    pkt::write_flush(output)?;
    output.flush()
}

/// The payloads of `input`'s data lines up to its flush.
fn data_until_flush(input: &mut impl Read) -> io::Result<Vec<Vec<u8>>> {
    let mut lines = Vec::new();
    loop {
        match pkt::read(input)? {
            Some(Pkt::Data(line)) => lines.push(line),
            Some(Pkt::Flush) => return Ok(lines),
            Some(Pkt::Delim) => return Err(invalid("proc-receive: a delimiter line".to_owned())),
            None => return Err(io::ErrorKind::UnexpectedEof.into()),
        }
    }
}

/// One command line: two ids and a ref name, a trailing newline allowed.
fn command(line: &str) -> io::Result<Command> {
    let line = line.strip_suffix('\n').unwrap_or(line);
    let malformed = || invalid(format!("proc-receive: command line {line:?}"));

    let mut parts = line.splitn(3, ' ');
    let (Some(old), Some(new), Some(refname)) = (parts.next(), parts.next(), parts.next()) else {
        return Err(malformed());
    };
    if refname.is_empty() {
        return Err(malformed());
    }
    let id = |hex: &str| hex.parse::<GitOid>().map_err(|_| malformed());
    Ok(Command::new(present(id(old)?), present(id(new)?), refname))
}

/// `oid`, unless it is git's zero id ("no object").
fn present(oid: GitOid) -> Option<GitOid> {
    (oid != GitOid::NULL).then_some(oid)
}

/// `line` as text.
fn text(line: &[u8]) -> io::Result<&str> {
    std::str::from_utf8(line).map_err(|_| invalid("proc-receive: a line not UTF-8".to_owned()))
}

/// `value`, refused if it holds a newline.
fn field(value: &str) -> io::Result<&str> {
    if value.contains('\n') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("proc-receive: a report field with a newline: {value:?}"),
        ));
    }
    Ok(value)
}

/// An `InvalidData` error saying `what`.
fn invalid(what: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what)
}
