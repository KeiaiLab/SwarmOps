//! git's object ids, the kinds of object, and the id git gives an object's bytes.

use std::fmt;
use std::str::FromStr;

/// A git object id: the SHA-1 of the object ([`git_hash`]), 20 bytes. It prints as 40 lowercase
/// hex digits and parses from 40 hex digits of either case.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub struct GitOid(pub [u8; 20]);

impl GitOid {
    /// The id no object has, forty zeros: git's "no object".
    pub const NULL: GitOid = GitOid([0; 20]);

    /// The id whose 20 bytes are `bytes`.
    pub const fn new(bytes: [u8; 20]) -> GitOid {
        GitOid(bytes)
    }

    /// The id `hex` writes: exactly 40 hex digits, of either case; `None` for anything else.
    pub fn from_hex(hex: &[u8]) -> Option<GitOid> {
        let digits: &[u8; 40] = hex.try_into().ok()?;
        let mut id = [0; 20];
        for (byte, pair) in id.iter_mut().zip(digits.as_chunks::<2>().0) {
            *byte = (nibble(pair[0])? << 4) | nibble(pair[1])?;
        }
        Some(GitOid(id))
    }
}

impl From<[u8; 20]> for GitOid {
    /// The id whose 20 bytes are `bytes` ([`GitOid::new`]).
    fn from(bytes: [u8; 20]) -> GitOid {
        GitOid::new(bytes)
    }
}

/// The value of one hex digit, of either case.
fn nibble(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        b'A'..=b'F' => Some(digit - b'A' + 10),
        _ => None,
    }
}

impl fmt::Display for GitOid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for GitOid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "GitOid({self})")
    }
}

impl FromStr for GitOid {
    type Err = ParseGitOidError;

    fn from_str(hex: &str) -> Result<GitOid, ParseGitOidError> {
        GitOid::from_hex(hex.as_bytes()).ok_or(ParseGitOidError)
    }
}

/// Text that is not a [`GitOid`]: not 40 hex digits.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("a git object id is 40 hex digits")]
#[non_exhaustive]
pub struct ParseGitOidError;

/// The hash a git repository names its objects by (`extensions.objectFormat`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ObjectFormat {
    /// SHA-1, 20 bytes: the format of every repository without the extension, and the only one
    /// the import reads.
    Sha1,
    /// SHA-256, 32 bytes.
    Sha256,
}

impl ObjectFormat {
    /// git's name for it, `sha1` or `sha256`.
    pub fn as_str(self) -> &'static str {
        match self {
            ObjectFormat::Sha1 => "sha1",
            ObjectFormat::Sha256 => "sha256",
        }
    }
}

/// The kind of a git object, as its header names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RawKind {
    /// A commit.
    Commit,
    /// A tree.
    Tree,
    /// A blob: a file's or a symlink's content.
    Blob,
    /// An annotated tag.
    Tag,
}

impl RawKind {
    /// git's name for it, as an object's header writes it: `commit`, `tree`, `blob` or `tag`.
    pub fn as_str(self) -> &'static str {
        match self {
            RawKind::Commit => "commit",
            RawKind::Tree => "tree",
            RawKind::Blob => "blob",
            RawKind::Tag => "tag",
        }
    }
}

impl fmt::Display for RawKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The id git gives an object of `kind` whose bytes are `bytes`: the SHA-1 of the header
/// `"<kind> <len>\0"` followed by the bytes, by gix's SHA-1 (`gix::hash`, in the crate's one
/// module that sees gix), which runs git's collision detection as git does. Bytes that detection
/// flags as a SHA-1 collision attack, an object git refuses to read, get [`GitOid::NULL`], which
/// no object has, so that a comparison with any object's id fails.
pub fn git_hash(kind: RawKind, bytes: &[u8]) -> GitOid {
    crate::gix_source::object_sha1(kind, bytes).unwrap_or(GitOid::NULL)
}
