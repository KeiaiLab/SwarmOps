//! git's `proc-receive` hook protocol, version 1 (`githooks(5)`): the version handshake, the
//! commands of a push, and a report per command (`ok`, with its `option` lines, or `ng`).

use std::io::{self, Read, Write};

use crate::oid::GitOid;

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

/// The version handshake on `input` and `output`.
pub fn handshake(input: &mut impl Read, output: &mut impl Write) -> io::Result<()> {
    let _ = (input, output);
    Err(io::Error::other("proc-receive: not built"))
}

/// The commands of the push, up to their flush.
pub fn read_commands(input: &mut impl Read) -> io::Result<Vec<Command>> {
    let _ = input;
    Err(io::Error::other("proc-receive: not built"))
}

/// Writes `reports`, then a flush.
pub fn write_reports(output: &mut impl Write, reports: &[Report]) -> io::Result<()> {
    let _ = (output, reports);
    Err(io::Error::other("proc-receive: not built"))
}
