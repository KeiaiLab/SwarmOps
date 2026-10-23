//! git's commit and tree bytes, parsed into their fields and rendered back.
//!
//! A commit is a line `tree <id>`, a line `parent <id>` for each parent, `author <ident>`,
//! `committer <ident>`, any other header lines, a blank line, and the message to the end. A header
//! whose value has more than one line goes on in continuation lines, each starting with one space.
//! An ident is `name SP '<' email '>' SP secs SP (+|-)HHMM`. A tree is a run of entries, each
//! `mode SP name NUL` followed by the 20 bytes of the entry's id.
//!
//! [`RawCommit::parse`] is lenient wherever the fields can still hold something, and
//! [`RawCommit::render`] writes what the fields hold the one way git writes it: a commit git wrote
//! renders back to its own bytes, and one written another way renders to other bytes, which the
//! import classes `nonstandard`. Only what the fields cannot hold at all is
//! [`RawError::Malformed`]. A tree is kept exactly as stored, the digits of its modes and the
//! order of its entries included, so every tree that parses renders back to its bytes.

use crate::GitOid;

/// A git commit's fields, as its bytes hold them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawCommit {
    /// The commit's tree.
    pub tree: GitOid,
    /// Its parents, in their order, a parent listed twice twice.
    pub parents: Vec<GitOid>,
    /// Who wrote the change, and when.
    pub author: RawIdent,
    /// Who made the commit, and when.
    pub committer: RawIdent,
    /// Every header line after the committer, in order, each as its key and its value: the value
    /// is everything after the key's space, with the lines of a multi-line value (its continuation
    /// lines, without their leading space) joined by newlines (`encoding`, `gpgsig`, `mergetag`
    /// and any other).
    pub extra_headers: Vec<(Vec<u8>, Vec<u8>)>,
    /// Everything after the blank line that ends the headers, verbatim.
    pub message: Vec<u8>,
}

/// An author or a committer: `name SP '<' email '>' SP secs SP (+|-)HHMM`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawIdent {
    /// The name, without the white space git's ident parse trims before the `<`.
    pub name: Vec<u8>,
    /// The e-mail, between `<` and `>`.
    pub email: Vec<u8>,
    /// Seconds since the Unix epoch, negative before 1970.
    pub secs: i64,
    /// The offset from UTC in seconds, west negative: `+0530` is 19,800.
    pub offset_secs: i32,
    /// git's `-0000`: offset zero written with a minus sign (only with `offset_secs == 0`).
    pub negative_zero: bool,
}

/// A git tree's entries, as its bytes hold them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawTree {
    /// The entries, in their stored order, whether or not it is git's.
    pub entries: Vec<RawTreeEntry>,
}

/// One entry of a [`RawTree`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawTreeEntry {
    /// The mode's octal digits as stored: `100644`, `100755`, `120000`, `40000` and `160000` as git
    /// writes them, and anything else a tree holds, such as `100664` or a zero-padded `040000`.
    pub mode: Vec<u8>,
    /// The entry's name, any bytes but NUL.
    pub name: Vec<u8>,
    /// The id of the blob, tree or commit (a gitlink) it names.
    pub oid: GitOid,
}

impl RawTreeEntry {
    /// An entry of mode digits `mode`, `name` and `oid`.
    pub fn new(mode: Vec<u8>, name: Vec<u8>, oid: GitOid) -> RawTreeEntry {
        RawTreeEntry { mode, name, oid }
    }
}

/// Bytes that are no commit or tree the fields can hold.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RawError {
    /// The bytes cannot be held at all: a commit whose first line is not its tree, an id that is
    /// not 40 hex digits, no author or committer after the parents, a `parent` after the author;
    /// a tree entry without its mode's octal digits, its name's NUL or its id's 20 bytes.
    #[error("not a git object this crate can hold: {what}")]
    Malformed {
        /// What is wrong, in a few words.
        what: &'static str,
    },
}

fn malformed(what: &'static str) -> RawError {
    RawError::Malformed { what }
}

/// The white space git's ident parse trims from the end of a name (git's `isspace`).
const IDENT_SPACE: &[u8] = b" \t\n\r";

impl RawCommit {
    /// A commit of these fields: `tree`, `parents` in order, `author`, `committer`, the
    /// `extra_headers` after the committer, and `message`.
    pub fn new(
        tree: GitOid,
        parents: Vec<GitOid>,
        author: RawIdent,
        committer: RawIdent,
        extra_headers: Vec<(Vec<u8>, Vec<u8>)>,
        message: Vec<u8>,
    ) -> RawCommit {
        RawCommit {
            tree,
            parents,
            author,
            committer,
            extra_headers,
            message,
        }
    }

    /// The fields of the commit whose bytes, without git's `commit <len>\0` header, are `bytes`.
    ///
    /// Lenient where the fields can still hold something, as [`render`](RawCommit::render) then
    /// writes other bytes: an ident or a date it cannot read becomes zero seconds at offset zero
    /// (with what it can read of the name and the e-mail), a header line without a space a key
    /// with an empty value, and bytes with no blank line after the headers an empty message.
    pub fn parse(bytes: &[u8]) -> Result<RawCommit, RawError> {
        let (headers, message) = split_headers(bytes);
        let mut headers = headers.into_iter().peekable();
        let tree = match headers.next() {
            Some((key, value)) if key == b"tree" => {
                GitOid::from_hex(&value).ok_or(malformed("a tree id is not 40 hex digits"))?
            }
            _ => return Err(malformed("the first line is not the tree")),
        };
        let mut parents = Vec::new();
        while let Some((_, value)) = headers.next_if(|(key, _)| *key == b"parent") {
            parents.push(
                GitOid::from_hex(&value).ok_or(malformed("a parent id is not 40 hex digits"))?,
            );
        }
        let author = match headers.next() {
            Some((key, value)) if key == b"author" => RawIdent::parse(&value),
            _ => return Err(malformed("no author after the tree and the parents")),
        };
        let committer = match headers.next() {
            Some((key, value)) if key == b"committer" => RawIdent::parse(&value),
            Some((key, _)) if key == b"parent" => {
                return Err(malformed("a parent after the author"));
            }
            _ => return Err(malformed("no committer after the author")),
        };
        let mut extra_headers = Vec::new();
        for (key, value) in headers {
            if key == b"parent" {
                return Err(malformed("a parent after the author"));
            }
            extra_headers.push((key.to_vec(), value));
        }
        Ok(RawCommit {
            tree,
            parents,
            author,
            committer,
            extra_headers,
            message: message.to_vec(),
        })
    }

    /// The commit's bytes as git writes these fields, without git's `commit <len>\0` header:
    /// [`parse`](RawCommit::parse) gives the fields back unchanged, and a commit git wrote
    /// renders back to its own bytes. A value's newlines become continuation lines; an offset
    /// renders as whole minutes.
    pub fn render(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(256 + self.message.len());
        header(&mut out, b"tree", self.tree.to_string().as_bytes());
        for parent in &self.parents {
            header(&mut out, b"parent", parent.to_string().as_bytes());
        }
        header(&mut out, b"author", &self.author.render());
        header(&mut out, b"committer", &self.committer.render());
        for (key, value) in &self.extra_headers {
            header(&mut out, key, value);
        }
        out.push(b'\n');
        out.extend_from_slice(&self.message);
        out
    }
}

/// Writes the header line `key SP value`, each newline of the value followed by the space that
/// starts a continuation line.
fn header(out: &mut Vec<u8>, key: &[u8], value: &[u8]) {
    out.extend_from_slice(key);
    out.push(b' ');
    for &byte in value {
        out.push(byte);
        if byte == b'\n' {
            out.push(b' ');
        }
    }
    out.push(b'\n');
}

/// A commit's header lines, each as its key and its value.
type Headers<'a> = Vec<(&'a [u8], Vec<u8>)>;

/// A commit's header lines, each as its key (the bytes before its first space, or the whole line)
/// and its value (the bytes after it, then each continuation line's after a newline), and the
/// message after the blank line that ends them: empty when no blank line does. A continuation
/// line before any header is a header of an empty key.
fn split_headers(bytes: &[u8]) -> (Headers<'_>, &[u8]) {
    let mut headers = Headers::new();
    let mut rest = bytes;
    while !rest.is_empty() {
        let (line, after) = match rest.iter().position(|&byte| byte == b'\n') {
            Some(end) => (&rest[..end], &rest[end + 1..]),
            None => (rest, &rest[rest.len()..]),
        };
        if line.is_empty() {
            return (headers, after);
        }
        if let (Some(continued), Some((_, value))) = (line.strip_prefix(b" "), headers.last_mut()) {
            value.push(b'\n');
            value.extend_from_slice(continued);
        } else {
            let (key, value) = match line.iter().position(|&byte| byte == b' ') {
                Some(space) => (&line[..space], &line[space + 1..]),
                None => (line, &line[line.len()..]),
            };
            headers.push((key, value.to_vec()));
        }
        rest = after;
    }
    (headers, &[])
}

impl RawIdent {
    /// An ident of `name` and `email` at `secs`, `offset_secs` from UTC; `negative_zero` writes
    /// offset zero as `-0000`.
    pub fn new(
        name: Vec<u8>,
        email: Vec<u8>,
        secs: i64,
        offset_secs: i32,
        negative_zero: bool,
    ) -> RawIdent {
        RawIdent {
            name,
            email,
            secs,
            offset_secs,
            negative_zero,
        }
    }

    /// The ident an `author` or `committer` header's value writes, read as git reads it: the name
    /// is everything before the first `<`, its trailing white space trimmed, and the e-mail runs
    /// to the first `>` after it. The date must be exactly ` secs (+|-)HHMM`, the seconds an
    /// optional minus and decimal digits: anything else, and a value without its `<` or `>`, is
    /// zero seconds at offset zero, so that [`render`](RawIdent::render) writes other bytes.
    pub fn parse(value: &[u8]) -> RawIdent {
        let mut ident = RawIdent {
            name: Vec::new(),
            email: Vec::new(),
            secs: 0,
            offset_secs: 0,
            negative_zero: false,
        };
        let Some(open) = value.iter().position(|&byte| byte == b'<') else {
            ident.name = trim_end(value).to_vec();
            return ident;
        };
        ident.name = trim_end(&value[..open]).to_vec();
        let after = &value[open + 1..];
        let Some(close) = after.iter().position(|&byte| byte == b'>') else {
            ident.email = after.to_vec();
            return ident;
        };
        ident.email = after[..close].to_vec();
        if let Some((secs, offset_secs, negative_zero)) = parse_date(&after[close + 1..]) {
            ident.secs = secs;
            ident.offset_secs = offset_secs;
            ident.negative_zero = negative_zero;
        }
        ident
    }

    /// The ident as git writes it: `name SP '<' email '>' SP secs SP (+|-)HHMM`, the offset in
    /// whole minutes, `-` west of UTC and for git's `-0000`.
    pub fn render(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.name.len() + self.email.len() + 32);
        out.extend_from_slice(&self.name);
        out.extend_from_slice(b" <");
        out.extend_from_slice(&self.email);
        out.extend_from_slice(b"> ");
        out.extend_from_slice(self.secs.to_string().as_bytes());
        let west = self.offset_secs < 0 || (self.offset_secs == 0 && self.negative_zero);
        let offset = self.offset_secs.unsigned_abs();
        let zone = format!(
            " {}{:02}{:02}",
            if west { '-' } else { '+' },
            offset / 3600,
            offset % 3600 / 60
        );
        out.extend_from_slice(zone.as_bytes());
        out
    }
}

/// `value` without the white space git's ident parse trims from the end of a name.
fn trim_end(value: &[u8]) -> &[u8] {
    let end = value
        .iter()
        .rposition(|byte| !IDENT_SPACE.contains(byte))
        .map_or(0, |last| last + 1);
    &value[..end]
}

/// The date after an ident's `>`: exactly ` secs (+|-)HHMM`, the seconds an optional minus and
/// decimal digits; the seconds, the offset in seconds and whether it is git's `-0000`.
fn parse_date(date: &[u8]) -> Option<(i64, i32, bool)> {
    let date = date.strip_prefix(b" ")?;
    let space = date.iter().position(|&byte| byte == b' ')?;
    let (secs, zone) = (&date[..space], &date[space + 1..]);
    let digits = secs.strip_prefix(b"-").unwrap_or(secs);
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let secs: i64 = std::str::from_utf8(secs).ok()?.parse().ok()?;
    let &[sign, h1, h2, m1, m2] = zone else {
        return None;
    };
    if !matches!(sign, b'+' | b'-') || ![h1, h2, m1, m2].iter().all(u8::is_ascii_digit) {
        return None;
    }
    let [h1, h2, m1, m2] = [h1, h2, m1, m2].map(|digit| i32::from(digit - b'0'));
    let offset = (h1 * 10 + h2) * 3600 + (m1 * 10 + m2) * 60;
    let west = sign == b'-';
    Some((
        secs,
        if west { -offset } else { offset },
        west && offset == 0,
    ))
}

impl RawTree {
    /// A tree of `entries`, in their stored order.
    pub fn new(entries: Vec<RawTreeEntry>) -> RawTree {
        RawTree { entries }
    }

    /// The entries of the tree whose bytes, without git's `tree <len>\0` header, are `bytes`, as
    /// stored: each a mode of octal digits, a space, a name ended by NUL, and 20 bytes of id.
    pub fn parse(bytes: &[u8]) -> Result<RawTree, RawError> {
        let mut entries = Vec::new();
        let mut rest = bytes;
        while !rest.is_empty() {
            let space = rest
                .iter()
                .position(|&byte| byte == b' ')
                .ok_or(malformed("a tree entry has no space after its mode"))?;
            let mode = &rest[..space];
            if mode.is_empty() || !mode.iter().all(|byte| (b'0'..=b'7').contains(byte)) {
                return Err(malformed("a tree entry's mode is not octal digits"));
            }
            let after = &rest[space + 1..];
            let nul = after
                .iter()
                .position(|&byte| byte == 0)
                .ok_or(malformed("a tree entry's name has no NUL after it"))?;
            let id: [u8; 20] = after
                .get(nul + 1..nul + 21)
                .and_then(|id| id.try_into().ok())
                .ok_or(malformed("a tree entry's id is cut short"))?;
            entries.push(RawTreeEntry {
                mode: mode.to_vec(),
                name: after[..nul].to_vec(),
                oid: GitOid(id),
            });
            rest = &after[nul + 21..];
        }
        Ok(RawTree { entries })
    }

    /// The tree's bytes, without git's `tree <len>\0` header: each entry as `mode SP name NUL`
    /// and its id's 20 bytes, in the entries' order. [`parse`](RawTree::parse) of what it writes
    /// gives the entries back, and a tree that parses renders back to its own bytes.
    pub fn render(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for entry in &self.entries {
            out.extend_from_slice(&entry.mode);
            out.push(b' ');
            out.extend_from_slice(&entry.name);
            out.push(0);
            out.extend_from_slice(&entry.oid.0);
        }
        out
    }
}
