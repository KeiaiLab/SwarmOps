//! git's object ids, the kinds of object, and the id git gives an object's bytes.

use std::fmt;
use std::str::FromStr;

/// A git object id: the SHA-1 of the object ([`GitOid::of`]), 20 bytes. It prints as 40 lowercase
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

    /// The id git gives an object of `kind` whose bytes are `bytes`: the SHA-1 of the header
    /// `"<kind> <len>\0"` followed by the bytes, by gix's SHA-1 (`gix::hash`, in the crate's one
    /// module that sees gix), which runs git's collision detection as git does. `None` for bytes
    /// that detection flags as a SHA-1 collision attack: git refuses to read such an object, so
    /// it has no id to keep, write or compare.
    pub fn of(kind: RawKind, bytes: &[u8]) -> Option<GitOid> {
        checked_hash(kind, bytes)
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

/// [`GitOid::of`] for a comparison: the id git gives an object of `kind` whose bytes are `bytes`,
/// and [`GitOid::NULL`], which no object has, for bytes git's collision detection flags, so that
/// a comparison with any object's id fails. What keeps or writes an id (the export's oids and
/// their memos, [`GixSink`](crate::GixSink), the import's renderings) takes [`GitOid::of`]'s
/// `None` and refuses such bytes instead.
pub fn git_hash(kind: RawKind, bytes: &[u8]) -> GitOid {
    checked_hash(kind, bytes).unwrap_or(GitOid::NULL)
}

/// [`GitOid::of`], for the crate: `None` for bytes git's collision detection flags.
pub(crate) fn checked_hash(kind: RawKind, bytes: &[u8]) -> Option<GitOid> {
    #[cfg(test)]
    if flagged::holds(kind, bytes) {
        return None;
    }
    crate::gix_source::object_sha1(kind, bytes)
}

/// The objects the crate's unit tests have git's collision detection flag. No test can make
/// bytes it flags: a collision attack's blocks hold only after the prefix they were computed
/// for, and git's header `"<kind> <len>\0"` comes first here. So a test names the object, and
/// [`checked_hash`] takes it as flagged, on that test's thread alone.
#[cfg(test)]
pub(crate) mod flagged {
    use std::cell::RefCell;

    use crate::RawKind;

    thread_local! {
        /// The objects flagged on this thread: their kinds and bytes.
        static FLAGGED: RefCell<Vec<(RawKind, Vec<u8>)>> = const { RefCell::new(Vec::new()) };
    }

    /// Has [`checked_hash`](super::checked_hash) take the object of `kind` and `bytes` as
    /// flagged, on this thread.
    pub(crate) fn flag(kind: RawKind, bytes: &[u8]) {
        FLAGGED.with(|flagged| flagged.borrow_mut().push((kind, bytes.to_vec())));
    }

    /// Whether the object of `kind` and `bytes` is flagged on this thread.
    pub(super) fn holds(kind: RawKind, bytes: &[u8]) -> bool {
        FLAGGED.with(|flagged| {
            (flagged.borrow().iter()).any(|(of, held)| *of == kind && held.as_slice() == bytes)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The content of the blob the test has git's collision detection flag.
    const CONTENT: &[u8] = b"a\n";
    /// The oid git gives a blob of [`CONTENT`] (`git hash-object`).
    const CONTENT_OID: &str = "78981922613b2afb6025042ff6bd878ac1994e85";

    /// [`GitOid::of`] gives an object its oid, and none to one git's collision detection flags,
    /// where [`git_hash`] gives the oid of no object.
    #[test]
    fn a_flagged_object_has_no_oid() {
        // 1. An object git reads: its oid, the same from both.
        let oid = GitOid::of(RawKind::Blob, CONTENT);
        let hex = oid.map(|oid| oid.to_string());
        assert_eq!(hex.as_deref(), Some(CONTENT_OID), "the blob's oid");
        assert_eq!(oid, Some(git_hash(RawKind::Blob, CONTENT)), "git_hash");

        // 2. The same bytes flagged as a blob: no oid, and the null one.
        flagged::flag(RawKind::Blob, CONTENT);
        assert_eq!(GitOid::of(RawKind::Blob, CONTENT), None, "a flagged blob");
        assert_eq!(git_hash(RawKind::Blob, CONTENT), GitOid::NULL, "git_hash");

        // 3. The flag is the object's: the bytes as a tag still have an oid.
        let tag = GitOid::of(RawKind::Tag, CONTENT);
        assert!(tag.is_some(), "the bytes as a tag, which no one flagged");
    }
}
