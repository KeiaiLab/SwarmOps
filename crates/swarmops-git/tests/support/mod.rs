//! What the git crate's tests share: [`MemSource`], a [`GitSource`] held in memory, for the
//! repositories no fixture holds (a tree 10,000 directories deep, a blob whose header claims more
//! than an object holds, an object of the wrong kind). A module directory, so that Cargo does not
//! take it for a test target of its own.
//!
//! [`MemSink`] (`mem_sink.rs`) is a [`GitSink`](swarmops_git::GitSink) held in memory, for the
//! export's tests: its log replays into the images a power loss could leave.

mod mem_sink;

pub use mem_sink::{Keep, MemImage, MemSink, SinkCall, reaches_only_kept};

use std::collections::HashMap;

use swarmops_git::{
    GitOid, GitSource, ObjectFormat, RawCommit, RawIdent, RawKind, RawTree, RawTreeEntry,
    SourceError, git_hash,
};

/// A [`GitSource`] held in memory. Each object it is given is rendered as git stores it and kept
/// under the id git gives its bytes ([`git_hash`]), so that what [`read`](GitSource::read)
/// returns hashes to the id asked for, as the trait requires. It has no refs: [`resolve`]
/// takes 40 hex digits of a commit it holds.
///
/// [`resolve`]: GitSource::resolve
#[derive(Debug, Default)]
pub struct MemSource {
    objects: HashMap<GitOid, (RawKind, Vec<u8>)>,
    /// Objects only a header describes, which a read must never reach.
    claims: HashMap<GitOid, (RawKind, u64)>,
}

impl MemSource {
    /// Keeps `bytes` as an object of `kind`; its id.
    pub fn put(&mut self, kind: RawKind, bytes: Vec<u8>) -> GitOid {
        let oid = git_hash(kind, &bytes);
        self.objects.insert(oid, (kind, bytes));
        oid
    }

    /// A blob of `bytes`.
    pub fn blob(&mut self, bytes: &[u8]) -> GitOid {
        self.put(RawKind::Blob, bytes.to_vec())
    }

    /// A tree of `entries`, each its mode's digits, its name and the id it names, in the order
    /// given, which need not be git's.
    pub fn tree(&mut self, entries: &[(&str, &[u8], GitOid)]) -> GitOid {
        let tree = RawTree {
            entries: entries
                .iter()
                .map(|&(mode, name, oid)| RawTreeEntry {
                    mode: mode.as_bytes().to_vec(),
                    name: name.to_vec(),
                    oid,
                })
                .collect(),
        };
        self.put(RawKind::Tree, tree.render())
    }

    /// A commit of `tree` and `parents`, written as git writes it, by one author and committer
    /// at one time, with `message`.
    pub fn commit(&mut self, tree: GitOid, parents: &[GitOid], message: &str) -> GitOid {
        let ident = RawIdent {
            name: b"Mem Source".to_vec(),
            email: b"mem@source.invalid".to_vec(),
            secs: 1_700_000_000,
            offset_secs: 0,
            negative_zero: false,
        };
        let commit = RawCommit {
            tree,
            parents: parents.to_vec(),
            author: ident.clone(),
            committer: ident,
            extra_headers: Vec::new(),
            message: message.as_bytes().to_vec(),
        };
        self.put(RawKind::Commit, commit.render())
    }

    /// An object `oid` that only its header describes, as a `kind` of `size` bytes: reading it
    /// panics, so that a test sees what is decided before a read.
    pub fn claim(&mut self, oid: GitOid, kind: RawKind, size: u64) {
        self.claims.insert(oid, (kind, size));
    }

    /// The object `oid` read as `bytes` of `kind`, which hash to another id: what a source that
    /// breaks [`GitSource::read`]'s promise returns, for a test of what notices.
    pub fn forge(&mut self, oid: GitOid, kind: RawKind, bytes: Vec<u8>) {
        self.objects.insert(oid, (kind, bytes));
    }
}

impl GitSource for MemSource {
    fn object_format(&self) -> ObjectFormat {
        ObjectFormat::Sha1
    }

    fn resolve(&self, name: &str) -> Result<GitOid, SourceError> {
        GitOid::from_hex(name.as_bytes())
            .filter(|oid| matches!(self.objects.get(oid), Some((RawKind::Commit, _))))
            .ok_or_else(|| SourceError::RefNotFound {
                name: name.to_owned(),
            })
    }

    fn header(&self, oid: GitOid) -> Result<(RawKind, u64), SourceError> {
        if let Some(&claim) = self.claims.get(&oid) {
            return Ok(claim);
        }
        let (kind, bytes) = self.objects.get(&oid).ok_or(SourceError::Missing { oid })?;
        Ok((*kind, u64::try_from(bytes.len()).unwrap()))
    }

    fn read(&self, oid: GitOid) -> Result<(RawKind, Vec<u8>), SourceError> {
        if let Some((_, size)) = self.claims.get(&oid) {
            panic!("object {oid} was read, whose header claims {size} bytes");
        }
        self.objects
            .get(&oid)
            .cloned()
            .ok_or(SourceError::Missing { oid })
    }
}
