//! Object identifiers: BLAKE3-256 with a per-kind domain separator.

use std::fmt;
use std::str::FromStr;

use crate::bytes;
use crate::error::IdError;

/// A 32-byte BLAKE3 digest identifying one immutable object.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub struct ObjectId(pub [u8; 32]);

impl ObjectId {
    /// The id whose digest is `bytes`.
    pub fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Parses 64 hex characters (either case).
    pub fn from_hex(s: &str) -> Result<Self, IdError> {
        if s.len() != 64 {
            return Err(IdError::Length(s.len()));
        }
        let mut out = [0u8; 32];
        hex::decode_to_slice(s, &mut out).map_err(|_| IdError::NonHex)?;
        Ok(Self(out))
    }

    /// Lowercase hex, 64 characters.
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }
}

/// The 64 lowercase hex digits, written from a buffer on the stack (no `String` per id);
/// formatting flags such as a width are ignored, as `write_str` ignores them.
impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut buf = [0u8; 64];
        hex::encode_to_slice(self.0, &mut buf).map_err(|_| fmt::Error)?;
        f.write_str(std::str::from_utf8(&buf).map_err(|_| fmt::Error)?)
    }
}

impl From<[u8; 32]> for ObjectId {
    fn from(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl fmt::Debug for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ObjectId({self})")
    }
}

impl FromStr for ObjectId {
    type Err = IdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_hex(s)
    }
}

/// The three hashed object kinds. The kind is part of the hash input, not of the id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ObjectKind {
    /// Raw file (or symlink target) bytes.
    Blob,
    /// One directory.
    Tree,
    /// One write-ahead-log entry.
    Changeset,
}

impl ObjectKind {
    /// The domain-separation prefix fed to the hasher before the payload length.
    pub fn domain(self) -> &'static [u8] {
        match self {
            Self::Blob => b"swarmops:blob:v1\0",
            Self::Tree => b"swarmops:tree:v1\0",
            Self::Changeset => b"swarmops:changeset:v1\0",
        }
    }

    /// The kind's byte in a stored object file's header (storage spec §2): 0 blob, 1 tree,
    /// 2 changeset. Part of the on-disk layout, so it never changes in place.
    pub fn tag(self) -> u8 {
        match self {
            Self::Blob => 0,
            Self::Tree => 1,
            Self::Changeset => 2,
        }
    }

    /// The kind whose [`ObjectKind::tag`] is `tag`, or `None` for a byte no kind has.
    pub fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            0 => Some(Self::Blob),
            1 => Some(Self::Tree),
            2 => Some(Self::Changeset),
            _ => None,
        }
    }
}

/// `blake3( domain(kind) ++ u64le(payload.len()) ++ payload )`.
pub fn hash_object(kind: ObjectKind, payload: &[u8]) -> ObjectId {
    let mut hasher = ObjectHasher::new(kind, bytes::to_u64(payload.len()));
    hasher.update(payload);
    hasher.finish()
}

/// [`hash_object`]'s recipe fed the payload in pieces, the one place the recipe is written: `len`
/// goes into the hash before any piece, so the caller states the payload's length up front (an
/// object file's header records it) and checks that the pieces add up to it. Pieces of
/// `payload`, in order, give `hash_object(kind, payload)` however it is cut.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ObjectHasher(blake3::Hasher);

impl ObjectHasher {
    /// The hash of a payload of `kind` and `len` bytes, before any of them.
    pub fn new(kind: ObjectKind, len: u64) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(kind.domain());
        hasher.update(&len.to_le_bytes());
        Self(hasher)
    }

    /// Feeds the payload's next piece.
    pub fn update(&mut self, piece: &[u8]) {
        self.0.update(piece);
    }

    /// The object's id, once every piece is fed.
    pub fn finish(&self) -> ObjectId {
        ObjectId(*self.0.finalize().as_bytes())
    }
}

/// An object's payload with its id, which only hashing makes: the id is always
/// `hash_object(kind, payload)`. The object store takes an object with its id only as a `Hashed`
/// (storage spec §2 rule 1, §8), so a caller that hashed an object once hands the store the id it
/// computed, and no caller can hand it an id of other bytes. There is no other constructor, and no
/// way to change the payload in place.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Hashed {
    kind: ObjectKind,
    id: ObjectId,
    payload: Vec<u8>,
}

impl Hashed {
    /// Hashes `payload` as an object of `kind`, once, and keeps it (no copy).
    pub fn new(kind: ObjectKind, payload: Vec<u8>) -> Hashed {
        let id = hash_object(kind, &payload);
        Hashed { kind, id, payload }
    }

    /// The object's kind.
    pub fn kind(&self) -> ObjectKind {
        self.kind
    }

    /// The object's id: `hash_object(self.kind(), self.payload())`.
    pub fn id(&self) -> ObjectId {
        self.id
    }

    /// The object's payload.
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    /// The payload, given back as it was given to [`Hashed::new`].
    pub fn into_payload(self) -> Vec<u8> {
        self.payload
    }
}

/// The kind, the id and the payload's length; not the payload, which may be large.
impl fmt::Debug for Hashed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Hashed")
            .field("kind", &self.kind)
            .field("id", &self.id)
            .field("len", &self.payload.len())
            .finish()
    }
}

macro_rules! typed_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        #[repr(transparent)]
        #[non_exhaustive]
        pub struct $name(pub ObjectId);

        impl $name {
            /// The typed id of `id`.
            pub fn new(id: ObjectId) -> Self {
                Self(id)
            }

            /// The untyped id.
            pub fn as_object_id(&self) -> ObjectId {
                self.0
            }
        }

        impl From<ObjectId> for $name {
            fn from(id: ObjectId) -> Self {
                Self(id)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }
    };
}

typed_id!(
    /// Id of a blob.
    BlobId
);
typed_id!(
    /// Id of a tree.
    TreeId
);
typed_id!(
    /// Id of a changeset.
    ChangesetId
);
