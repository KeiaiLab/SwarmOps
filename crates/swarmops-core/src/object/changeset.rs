//! One write-ahead-log entry (storage-core spec §1 layout "SWCS").

use super::codec::{
    CodecError, Reader, check_strictly_sorted, put_count, put_len_prefixed, reader,
};
use super::tree::LeafKind;
use crate::id::{BlobId, ChangesetId, ObjectId, ObjectKind, TreeId, hash_object};
use crate::path::RepoPath;

const MAGIC: &[u8; 4] = b"SWCS";
const VERSION: u8 = 1;

/// Author or committer identity with a git-faithful timestamp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    /// Name bytes, verbatim.
    pub name: Vec<u8>,
    /// Email bytes, verbatim.
    pub email: Vec<u8>,
    /// Seconds since the Unix epoch.
    pub secs: i64,
    /// UTC offset in seconds.
    pub offset_secs: i32,
    /// Git's `-0000`: offset zero written with a minus sign. Only valid with `offset_secs == 0`.
    pub negative_zero: bool,
}

/// What happened to one path relative to the first parent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeltaOp {
    /// The path now holds this leaf: a file, an executable or a symlink, never a directory
    /// (a directory follows from the paths of the leaves below it).
    Put {
        /// File, Exec, or Symlink.
        kind: LeafKind,
        /// The content.
        id: BlobId,
    },
    /// The path no longer exists.
    Delete,
}

/// One line of a changeset's delta.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeltaEntry {
    /// Full path from the root.
    pub path: RepoPath,
    /// What happened.
    pub op: DeltaOp,
    /// For a detected rename, where the content came from. Only a `Put` can carry
    /// one, and it is never the entry's own `path`.
    pub renamed_from: Option<RepoPath>,
}

/// A changeset: the unit of work and the WAL entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Changeset {
    /// Parents; index 0 is the mainline parent the delta is relative to.
    pub parents: Vec<ChangesetId>,
    /// The full post-state root tree.
    pub root: TreeId,
    /// Changes relative to `parents[0]`, sorted by path bytes, unique.
    pub delta: Vec<DeltaEntry>,
    /// Who wrote the change.
    pub author: Signature,
    /// Who recorded it.
    pub committer: Signature,
    /// Message bytes, verbatim.
    pub message: Vec<u8>,
    /// Git headers preserved in order (gpgsig, encoding, mergetag, ...).
    pub extra_headers: Vec<(Vec<u8>, Vec<u8>)>,
    /// SwarmOps-owned key/value pairs, keys strictly ascending.
    pub metadata: Vec<(String, Vec<u8>)>,
}

impl Signature {
    fn validate(&self) -> Result<(), CodecError> {
        if self.negative_zero && self.offset_secs != 0 {
            return Err(CodecError::BadSignature);
        }
        Ok(())
    }

    fn encode(&self, out: &mut Vec<u8>) {
        put_len_prefixed(out, &self.name);
        put_len_prefixed(out, &self.email);
        out.extend_from_slice(&self.secs.to_le_bytes());
        out.extend_from_slice(&self.offset_secs.to_le_bytes());
        out.push(u8::from(self.negative_zero));
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self, CodecError> {
        let sig = Self {
            name: r.len_prefixed()?.to_vec(),
            email: r.len_prefixed()?.to_vec(),
            secs: r.i64_le()?,
            offset_secs: r.i32_le()?,
            negative_zero: r.flag()?,
        };
        sig.validate()?;
        Ok(sig)
    }
}

impl DeltaEntry {
    fn validate(&self) -> Result<(), CodecError> {
        if let Some(from) = &self.renamed_from
            && (matches!(self.op, DeltaOp::Delete) || *from == self.path)
        {
            return Err(CodecError::BadRename);
        }
        Ok(())
    }

    fn encode(&self, out: &mut Vec<u8>) {
        put_len_prefixed(out, self.path.as_str().as_bytes());
        match &self.op {
            DeltaOp::Put { kind, id } => {
                out.push(0);
                out.push(kind.as_u8());
                out.extend_from_slice(&id.0.0);
            }
            DeltaOp::Delete => out.push(1),
        }
        match &self.renamed_from {
            Some(from) => {
                out.push(1);
                put_len_prefixed(out, from.as_str().as_bytes());
            }
            None => out.push(0),
        }
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self, CodecError> {
        let path = RepoPath::new(r.utf8()?)?;
        let op = match r.u8()? {
            // A kind byte of 3 (a directory) or more is `BadKind`: a put names a leaf.
            0 => DeltaOp::Put {
                kind: LeafKind::from_u8(r.u8()?)?,
                id: BlobId(ObjectId(r.array()?)),
            },
            1 => DeltaOp::Delete,
            b => return Err(CodecError::BadOp(b)),
        };
        let renamed_from = if r.flag()? {
            Some(RepoPath::new(r.utf8()?)?)
        } else {
            None
        };
        let entry = Self {
            path,
            op,
            renamed_from,
        };
        entry.validate()?;
        Ok(entry)
    }
}

impl Changeset {
    /// Checks the invariants neither the layout nor the types express: sorted unique delta
    /// paths, renames only on puts and never from the entry's own path, sorted unique metadata
    /// keys, consistent signatures. (That a put names a leaf, the type says: [`LeafKind`].)
    pub fn validate(&self) -> Result<(), CodecError> {
        for d in &self.delta {
            d.validate()?;
        }
        check_strictly_sorted(&self.delta, |d| d.path.as_str().as_bytes())?;
        self.author.validate()?;
        self.committer.validate()?;
        check_strictly_sorted(&self.metadata, |(k, _)| k.as_bytes())
            .map_err(|_| CodecError::UnsortedMetadata)
    }

    /// Canonical bytes plus the byte range of the delta section inside them.
    fn encode_with_delta_range(&self) -> Result<(Vec<u8>, std::ops::Range<usize>), CodecError> {
        self.validate()?;
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        out.push(VERSION);
        put_count(&mut out, self.parents.len());
        for p in &self.parents {
            out.extend_from_slice(&p.0.0);
        }
        out.extend_from_slice(&self.root.0.0);
        let delta_start = out.len();
        put_count(&mut out, self.delta.len());
        for d in &self.delta {
            d.encode(&mut out);
        }
        let delta_end = out.len();
        self.author.encode(&mut out);
        self.committer.encode(&mut out);
        put_len_prefixed(&mut out, &self.message);
        put_count(&mut out, self.extra_headers.len());
        for (k, v) in &self.extra_headers {
            put_len_prefixed(&mut out, k);
            put_len_prefixed(&mut out, v);
        }
        put_count(&mut out, self.metadata.len());
        for (k, v) in &self.metadata {
            put_len_prefixed(&mut out, k.as_bytes());
            put_len_prefixed(&mut out, v);
        }
        Ok((out, delta_start..delta_end))
    }

    /// The canonical bytes, or why this value is not canonical.
    pub fn encode(&self) -> Result<Vec<u8>, CodecError> {
        Ok(self.encode_with_delta_range()?.0)
    }

    /// Parses canonical bytes.
    pub fn decode(buf: &[u8]) -> Result<Self, CodecError> {
        let mut r = reader(buf);
        r.magic(*MAGIC, VERSION)?;
        let np = r.count()?;
        let mut parents = Vec::with_capacity(np.min(16));
        for _ in 0..np {
            parents.push(ChangesetId(ObjectId(r.array()?)));
        }
        let root = TreeId(ObjectId(r.array()?));
        let m = r.count()?;
        let mut delta = Vec::with_capacity(m.min(1024));
        for _ in 0..m {
            delta.push(DeltaEntry::decode(&mut r)?);
        }
        let author = Signature::decode(&mut r)?;
        let committer = Signature::decode(&mut r)?;
        let message = r.len_prefixed()?.to_vec();
        let k = r.count()?;
        let mut extra_headers = Vec::with_capacity(k.min(16));
        for _ in 0..k {
            extra_headers.push((r.len_prefixed()?.to_vec(), r.len_prefixed()?.to_vec()));
        }
        let j = r.count()?;
        let mut metadata = Vec::with_capacity(j.min(16));
        for _ in 0..j {
            metadata.push((r.utf8()?, r.len_prefixed()?.to_vec()));
        }
        r.finish()?;
        let cs = Self {
            parents,
            root,
            delta,
            author,
            committer,
            message,
            extra_headers,
            metadata,
        };
        cs.validate()?;
        Ok(cs)
    }

    /// `hash_object(Changeset, encode())`.
    pub fn id(&self) -> Result<ChangesetId, CodecError> {
        Ok(ChangesetId(hash_object(
            ObjectKind::Changeset,
            &self.encode()?,
        )))
    }

    /// BLAKE3 of the delta section alone: equal for the same change on a different base.
    pub fn delta_hash(&self) -> Result<[u8; 32], CodecError> {
        let (bytes, range) = self.encode_with_delta_range()?;
        Ok(*blake3::hash(&bytes[range]).as_bytes())
    }
}
