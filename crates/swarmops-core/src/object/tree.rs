//! One directory: `"SWTR" ver=1 | varint n | n × (kind u8 | varint len | name | id[32])`.

use super::codec::{CodecError, check_strictly_sorted, put_count, put_len_prefixed, reader};
use crate::id::{BlobId, ObjectId, ObjectKind, TreeId, hash_object};
use crate::path::check_component;

const MAGIC: &[u8; 4] = b"SWTR";
const VERSION: u8 = 1;

/// What a tree entry points at, as the canonical layout writes it: the entry's kind byte. A tree
/// entry names its target with an [`EntryTarget`], whose [`EntryTarget::kind`] gives this byte.
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

/// The kind of a leaf: a file, an executable or a symlink, never a directory. A tree entry's
/// [`EntryTarget::Leaf`] and a delta's put ([`super::DeltaOp::Put`]) carry one; its byte is the
/// [`EntryKind`] byte of the same name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum LeafKind {
    /// Regular file, mode 100644.
    File = 0,
    /// Executable file, mode 100755.
    Exec = 1,
    /// Symlink; the blob holds the target.
    Symlink = 2,
}

impl LeafKind {
    /// The on-disk byte: that of the [`EntryKind`] of the same name.
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    /// Parses the on-disk byte of a leaf: 3 (a directory) and up are [`CodecError::BadKind`].
    pub fn from_u8(b: u8) -> Result<Self, CodecError> {
        match b {
            0 => Ok(Self::File),
            1 => Ok(Self::Exec),
            2 => Ok(Self::Symlink),
            other => Err(CodecError::BadKind(other)),
        }
    }

    /// The entry kind of the same name.
    pub fn entry_kind(self) -> EntryKind {
        match self {
            Self::File => EntryKind::File,
            Self::Exec => EntryKind::Exec,
            Self::Symlink => EntryKind::Symlink,
        }
    }
}

/// What a tree entry names: a leaf's blob, with the leaf's kind, or a subdirectory's tree. The
/// type says which kind of object the id is, so a blob id is never taken for a tree's. The
/// canonical layout writes it as the entry's kind byte ([`EntryTarget::kind`]) and id
/// ([`EntryTarget::id`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntryTarget {
    /// A file, an executable or a symlink, and the blob that holds its content (a symlink's:
    /// its target).
    Leaf(LeafKind, BlobId),
    /// A subdirectory, and its tree.
    Dir(TreeId),
}

impl EntryTarget {
    /// The kind byte the canonical layout writes for it.
    pub fn kind(self) -> EntryKind {
        match self {
            Self::Leaf(kind, _) => kind.entry_kind(),
            Self::Dir(_) => EntryKind::Dir,
        }
    }

    /// The id the canonical layout writes for it: the blob's or the tree's.
    pub fn id(self) -> ObjectId {
        match self {
            Self::Leaf(_, blob) => blob.0,
            Self::Dir(tree) => tree.0,
        }
    }

    /// The target that the layout's kind byte, parsed into `kind`, and `id` name.
    pub(crate) fn from_parts(kind: EntryKind, id: ObjectId) -> Self {
        match kind {
            EntryKind::File => Self::Leaf(LeafKind::File, BlobId(id)),
            EntryKind::Exec => Self::Leaf(LeafKind::Exec, BlobId(id)),
            EntryKind::Symlink => Self::Leaf(LeafKind::Symlink, BlobId(id)),
            EntryKind::Dir => Self::Dir(TreeId(id)),
        }
    }
}

/// One entry of a tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    /// A single path component.
    pub name: String,
    /// What the entry names: a leaf's blob or a subdirectory's tree.
    pub target: EntryTarget,
}

impl TreeEntry {
    /// The entry `name` of a leaf of `kind` whose content is blob `blob`.
    pub fn leaf(name: impl Into<String>, kind: LeafKind, blob: BlobId) -> Self {
        Self {
            name: name.into(),
            target: EntryTarget::Leaf(kind, blob),
        }
    }

    /// The entry `name` of the subdirectory whose tree is `tree`.
    pub fn dir(name: impl Into<String>, tree: TreeId) -> Self {
        Self {
            name: name.into(),
            target: EntryTarget::Dir(tree),
        }
    }
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
            check_component(&e.name)?;
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
            out.push(e.target.kind().as_u8());
            put_len_prefixed(&mut out, e.name.as_bytes());
            out.extend_from_slice(&e.target.id().0);
        }
        out
    }

    /// Parses canonical bytes; an empty tree decodes successfully (it may be the root).
    pub fn decode(buf: &[u8]) -> Result<Self, CodecError> {
        let mut r = reader(buf);
        r.magic(*MAGIC, VERSION)?;
        let n = r.count()?;
        let mut entries = Vec::with_capacity(n.min(1024));
        for _ in 0..n {
            let kind = EntryKind::from_u8(r.u8()?)?;
            let name = r.utf8()?;
            let id = ObjectId(r.array()?);
            let target = EntryTarget::from_parts(kind, id);
            entries.push(TreeEntry { name, target });
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
