//! `Arbitrary` implementations that only produce canonical values, so fuzzing
//! and property tests exercise the codec rather than its validation errors.

use arbitrary::{Arbitrary, Result, Unstructured};

use super::changeset::{Changeset, DeltaEntry, DeltaOp, Signature};
use super::tree::{EntryKind, EntryTarget, LeafKind, Tree, TreeEntry};
use crate::id::{BlobId, ChangesetId, ObjectId, TreeId};
use crate::path::RepoPath;

const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789._-";

fn component(u: &mut Unstructured<'_>) -> Result<String> {
    loop {
        let len = u.int_in_range(1..=8u8)?;
        let mut s = String::with_capacity(usize::from(len));
        for _ in 0..len {
            s.push(char::from(*u.choose(ALPHABET)?));
        }
        if s != "." && s != ".." {
            return Ok(s);
        }
    }
}

impl<'a> Arbitrary<'a> for ObjectId {
    fn arbitrary(u: &mut Unstructured<'a>) -> Result<Self> {
        Ok(Self(<[u8; 32]>::arbitrary(u)?))
    }
}

impl<'a> Arbitrary<'a> for RepoPath {
    fn arbitrary(u: &mut Unstructured<'a>) -> Result<Self> {
        let n = u.int_in_range(1..=4u8)?;
        let parts: Vec<String> = (0..n).map(|_| component(u)).collect::<Result<_>>()?;
        Self::new(parts.join("/")).map_err(|_| arbitrary::Error::IncorrectFormat)
    }
}

impl<'a> Arbitrary<'a> for EntryKind {
    fn arbitrary(u: &mut Unstructured<'a>) -> Result<Self> {
        Ok(*u.choose(&[Self::File, Self::Exec, Self::Symlink, Self::Dir])?)
    }
}

impl<'a> Arbitrary<'a> for TreeEntry {
    fn arbitrary(u: &mut Unstructured<'a>) -> Result<Self> {
        // The draws of the `(kind, id)` pair a target replaced, in the same order (the name,
        // one of the four kinds, the id), so that existing fuzz inputs keep decoding to the
        // same shapes.
        let name = component(u)?;
        let kind = EntryKind::arbitrary(u)?;
        let id = ObjectId::arbitrary(u)?;
        Ok(Self {
            name,
            target: EntryTarget::from_parts(kind, id),
        })
    }
}

impl<'a> Arbitrary<'a> for Tree {
    fn arbitrary(u: &mut Unstructured<'a>) -> Result<Self> {
        let n = u.int_in_range(1..=6u8)?;
        let mut entries: Vec<TreeEntry> = (0..n)
            .map(|_| TreeEntry::arbitrary(u))
            .collect::<Result<_>>()?;
        entries.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
        entries.dedup_by(|a, b| a.name == b.name);
        Self::new(entries).map_err(|_| arbitrary::Error::IncorrectFormat)
    }
}

impl<'a> Arbitrary<'a> for Signature {
    fn arbitrary(u: &mut Unstructured<'a>) -> Result<Self> {
        let offset_secs = u.int_in_range(-50_400..=50_400i32)?;
        Ok(Self {
            name: Vec::<u8>::arbitrary(u)?,
            email: Vec::<u8>::arbitrary(u)?,
            secs: i64::arbitrary(u)?,
            offset_secs,
            negative_zero: offset_secs == 0 && bool::arbitrary(u)?,
        })
    }
}

impl<'a> Arbitrary<'a> for DeltaOp {
    fn arbitrary(u: &mut Unstructured<'a>) -> Result<Self> {
        if bool::arbitrary(u)? {
            Ok(Self::Put {
                kind: *u.choose(&[LeafKind::File, LeafKind::Exec, LeafKind::Symlink])?,
                id: BlobId(ObjectId::arbitrary(u)?),
            })
        } else {
            Ok(Self::Delete)
        }
    }
}

impl<'a> Arbitrary<'a> for DeltaEntry {
    fn arbitrary(u: &mut Unstructured<'a>) -> Result<Self> {
        let path = RepoPath::arbitrary(u)?;
        let op = DeltaOp::arbitrary(u)?;
        // Drawn for every entry so existing fuzz inputs keep decoding to the same
        // shapes, then dropped where the canonical rules forbid a rename: a `Delete`
        // has none, and a `Put` never renames from its own path.
        let from = Option::<RepoPath>::arbitrary(u)?;
        let renamed_from = match op {
            DeltaOp::Put { .. } => from.filter(|source| *source != path),
            DeltaOp::Delete => None,
        };
        Ok(Self {
            path,
            op,
            renamed_from,
        })
    }
}

impl<'a> Arbitrary<'a> for Changeset {
    fn arbitrary(u: &mut Unstructured<'a>) -> Result<Self> {
        let np = u.int_in_range(0..=3u8)?;
        let parents = (0..np)
            .map(|_| Ok(ChangesetId(ObjectId::arbitrary(u)?)))
            .collect::<Result<_>>()?;
        let nd = u.int_in_range(0..=5u8)?;
        let mut delta: Vec<DeltaEntry> = (0..nd)
            .map(|_| DeltaEntry::arbitrary(u))
            .collect::<Result<_>>()?;
        delta.sort_by(|a, b| a.path.as_str().as_bytes().cmp(b.path.as_str().as_bytes()));
        delta.dedup_by(|a, b| a.path == b.path);
        let nh = u.int_in_range(0..=3u8)?;
        let extra_headers = (0..nh)
            .map(|_| Ok((Vec::<u8>::arbitrary(u)?, Vec::<u8>::arbitrary(u)?)))
            .collect::<Result<_>>()?;
        let nm = u.int_in_range(0..=3u8)?;
        let mut metadata: Vec<(String, Vec<u8>)> = (0..nm)
            .map(|_| Ok((component(u)?, Vec::<u8>::arbitrary(u)?)))
            .collect::<Result<_>>()?;
        metadata.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        metadata.dedup_by(|a, b| a.0 == b.0);
        Ok(Self {
            parents,
            root: TreeId(ObjectId::arbitrary(u)?),
            delta,
            author: Signature::arbitrary(u)?,
            committer: Signature::arbitrary(u)?,
            message: Vec::<u8>::arbitrary(u)?,
            extra_headers,
            metadata,
        })
    }
}
