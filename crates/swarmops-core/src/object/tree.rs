//! One directory: `"SWTR" ver=1 | varint n | n × (kind u8 | varint len | name | id[32])`.

use super::codec::{CodecError, Reader, check_strictly_sorted, put_count, put_len_prefixed};
use crate::id::{ObjectId, ObjectKind, TreeId, hash_object};
use crate::path::RepoPath;

const MAGIC: &[u8; 4] = b"SWTR";
const VERSION: u8 = 1;

/// What a tree entry points at and how it is laid out on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum EntryKind {
    /// Regular file, mode 100644.
    File = 0,
    /// Executable file, mode 100755.
    Exec = 1,
    /// Symlink; the blob holds the target.
    Symlink = 2,
    /// Subdirectory; the id is a tree id.
    Dir = 3,
}

impl EntryKind {
    /// The on-disk byte.
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    /// Parses the on-disk byte.
    pub fn from_u8(b: u8) -> Result<Self, CodecError> {
        match b {
            0 => Ok(Self::File),
            1 => Ok(Self::Exec),
            2 => Ok(Self::Symlink),
            3 => Ok(Self::Dir),
            other => Err(CodecError::BadKind(other)),
        }
    }
}

/// One entry of a tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    /// A single path component.
    pub name: String,
    /// What the entry is.
    pub kind: EntryKind,
    /// Blob id for leaves, tree id for `Dir`.
    pub id: ObjectId,
}

/// A directory with entries sorted by raw name bytes and unique names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tree {
    entries: Vec<TreeEntry>,
}

impl Tree {
    /// Wraps `entries`, which must be non-empty, sorted by name bytes, unique,
    /// and single valid path components.
    pub fn new(entries: Vec<TreeEntry>) -> Result<Self, CodecError> {
        if entries.is_empty() {
            return Err(CodecError::EmptyNonRootTree);
        }
        Self::validate(&entries)?;
        Ok(Self { entries })
    }

    /// The only tree allowed to be empty: the root of an empty repository.
    pub fn empty_root() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// The entries, in canonical order.
    pub fn entries(&self) -> &[TreeEntry] {
        &self.entries
    }

    fn validate(entries: &[TreeEntry]) -> Result<(), CodecError> {
        for e in entries {
            if e.name.contains('/') {
                return Err(CodecError::BadPath(crate::error::PathError::EmptyComponent));
            }
            RepoPath::new(e.name.as_str())?;
        }
        check_strictly_sorted(entries, |e| e.name.as_bytes())
    }

    /// The canonical bytes.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + self.entries.len() * 48);
        out.extend_from_slice(MAGIC);
        out.push(VERSION);
        put_count(&mut out, self.entries.len());
        for e in &self.entries {
            out.push(e.kind.as_u8());
            put_len_prefixed(&mut out, e.name.as_bytes());
            out.extend_from_slice(&e.id.0);
        }
        out
    }

    /// Parses canonical bytes; an empty tree decodes successfully (it may be the root).
    pub fn decode(buf: &[u8]) -> Result<Self, CodecError> {
        let mut r = Reader::new(buf);
        r.magic(*MAGIC, VERSION)?;
        let n = r.count()?;
        let mut entries = Vec::with_capacity(n.min(1024));
        for _ in 0..n {
            let kind = EntryKind::from_u8(r.u8()?)?;
            let name = r.utf8()?;
            let id = ObjectId(r.array32()?);
            entries.push(TreeEntry { name, kind, id });
        }
        r.finish()?;
        Self::validate(&entries)?;
        Ok(Self { entries })
    }

    /// `hash_object(Tree, encode())`.
    pub fn id(&self) -> TreeId {
        TreeId(hash_object(ObjectKind::Tree, &self.encode()))
    }
}
