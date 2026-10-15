//! Object identifiers: BLAKE3-256 with a per-kind domain separator.

use std::fmt;
use std::str::FromStr;

use crate::error::IdError;

/// A 32-byte BLAKE3 digest identifying one immutable object.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectId(pub [u8; 32]);

impl ObjectId {
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

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ObjectId({})", self.to_hex())
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
}

/// `blake3( domain(kind) ++ u64le(payload.len()) ++ payload )`.
pub fn hash_object(kind: ObjectKind, payload: &[u8]) -> ObjectId {
    let mut h = blake3::Hasher::new();
    h.update(kind.domain());
    let len = u64::try_from(payload.len()).expect("payload length fits in u64");
    h.update(&len.to_le_bytes());
    h.update(payload);
    ObjectId(*h.finalize().as_bytes())
}

macro_rules! typed_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        #[repr(transparent)]
        pub struct $name(pub ObjectId);

        impl $name {
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
