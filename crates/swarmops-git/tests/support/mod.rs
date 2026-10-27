//! What the git crate's tests share: [`MemSource`], a [`GitSource`] held in memory, for the
//! repositories no fixture holds (a tree 10,000 directories deep, a blob, a tree or a commit whose
//! header claims more than an object holds, an object of the wrong kind). A module directory, so
//! that Cargo does not take it for a test target of its own.
//!
//! [`MemSink`] (`mem_sink.rs`) is a [`GitSink`](swarmops_git::GitSink) held in memory, for the
//! export's tests: its log replays into the images a power loss could leave.

mod mem_sink;

pub use mem_sink::{Keep, MemImage, MemSink, SinkCall, reaches_only_kept};

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use camino::Utf8PathBuf;
use swarmops_repo::LocalRepo;
use swarmops_store::FsMethod;
use swarmops_testkit::fs::{Call, NoSyncFs, Watch, Watched};
use tempfile::TempDir;

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
    /// The lengths the headers of held objects say instead of their own.
    stated: HashMap<GitOid, u64>,
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
        let tree = RawTree::new(
            entries
                .iter()
                .map(|&(mode, name, oid)| {
                    RawTreeEntry::new(mode.as_bytes().to_vec(), name.to_vec(), oid)
                })
                .collect(),
        );
        self.put(RawKind::Tree, tree.render())
    }

    /// A commit of `tree` and `parents`, written as git writes it, by one author and committer
    /// at one time, with `message`.
    pub fn commit(&mut self, tree: GitOid, parents: &[GitOid], message: &str) -> GitOid {
        let ident = RawIdent::new(
            b"Mem Source".to_vec(),
            b"mem@source.invalid".to_vec(),
            1_700_000_000,
            0,
            false,
        );
        let commit = RawCommit::new(
            tree,
            parents.to_vec(),
            ident.clone(),
            ident,
            Vec::new(),
            message.as_bytes().to_vec(),
        );
        self.put(RawKind::Commit, commit.render())
    }

    /// An object `oid` that only its header describes, as a `kind` of `size` bytes: reading it
    /// panics, so that a test sees what is decided before a read.
    pub fn claim(&mut self, oid: GitOid, kind: RawKind, size: u64) {
        self.claims.insert(oid, (kind, size));
    }

    /// The header of the object `oid`, which it holds, says it is `size` bytes long, whatever
    /// its own length: a read still gives its bytes, so that a test sees what a header lets by.
    pub fn state_length(&mut self, oid: GitOid, size: u64) {
        assert!(self.objects.contains_key(&oid), "no object {oid} is held");
        self.stated.insert(oid, size);
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
        let size = self.stated.get(&oid).copied();
        Ok((
            *kind,
            size.unwrap_or_else(|| u64::try_from(bytes.len()).unwrap()),
        ))
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

/// Counts the reads of a store's object files (`objects/`): how many changesets, trees and blobs
/// a call read, which the memos a key authenticates spare.
pub struct ObjectReads {
    /// The store's `objects/`, as the store names its files.
    dir: PathBuf,
    reads: AtomicU64,
}

impl ObjectReads {
    /// How many object files were read since the last call, and none counted from here on.
    pub fn take(&self) -> u64 {
        self.reads.swap(0, Ordering::Relaxed)
    }
}

impl Watch for ObjectReads {
    fn before(&self, call: &Call<'_>) -> io::Result<()> {
        if call.method == FsMethod::Read && call.path().starts_with(&self.dir) {
            self.reads.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }
}

/// A store of its own on a filesystem that never flushes ([`NoSyncFs`]), whose object reads are
/// counted, `ExportOid` registered: its temp dir, its root, the store and the counter.
pub fn counted_store() -> (TempDir, Utf8PathBuf, LocalRepo, Arc<ObjectReads>) {
    let dir = tempfile::tempdir().expect("a temp dir");
    let base = dir.path().canonicalize().expect("the temp dir");
    let root = Utf8PathBuf::from_path_buf(base.join("store")).expect("a UTF-8 temp dir");
    let reads = Arc::new(ObjectReads {
        dir: base.join("store").join("objects"),
        reads: AtomicU64::new(0),
    });
    let fs = Arc::new(Watched(Arc::clone(&reads), NoSyncFs));
    let mut repo = LocalRepo::create_with_fs(&root, "test", fs).expect("create");
    swarmops_git::register(&mut repo).expect("register ExportOid");
    (dir, root, repo, reads)
}
