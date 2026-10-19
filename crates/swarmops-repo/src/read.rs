//! Reading a repository without a checkout (storage spec §4), in one snapshot of its metadata
//! ([`Repo::snapshot`], a [`RepoRead`]): a changeset's record and object
//! ([`RepoRead::changeset`]), revisions resolved to changesets ([`RepoRead::resolve`]), and
//! files and directories at a changeset ([`RepoRead::read_blob_at`], [`RepoRead::list_tree`],
//! [`RepoRead::entry_at`]), read by walking the trees from its root: one tree object per
//! directory on the path. [`Repo`]'s methods of the same names each read in a snapshot of their
//! own.
//!
//! A revision (operations spec §0) is a ref name (`main`, `release/<x>`), a full id (64 hex
//! digits), or a unique id prefix of 12 to 63 hex digits, either case; followed by any number
//! of `~N` suffixes, each walking `N` first parents back (`main~2`, `main~1~1`).

use swarmops_core::{
    BlobId, Changeset, ChangesetId, EntryTarget, LeafKind, ObjectId, ObjectKind, RepoPath, Tree,
    TreeEntry, TreeId,
};
use swarmops_store::{ChangesetMeta, MetaRead, ObjectStore, StoreError};

use crate::error::RepoError;
use crate::repo::{self, Repo};

/// One consistent snapshot of a repository's metadata, and the reads of one request in it
/// ([`Repo::snapshot`]; storage spec §8): every revision and ref it resolves, it resolves in the
/// same view, which later commits and ref moves do not change, so that a request that names
/// several revisions reads them as of one moment. What it reads by id (changesets, trees, blobs)
/// is immutable. Two rules hold for it:
///
/// 1. Never read in it after the request's own write: it does not show that write. Reading what
///    the request wrote takes a new snapshot.
/// 2. Never hold it across requests. Besides going stale, a snapshot that counted index records
///    which a writer then cut back after a failed sync fails its lookups with an I/O error until
///    a new one is taken (storage spec §3).
pub struct RepoRead<'r, O: ObjectStore> {
    pub(crate) repo: &'r Repo<O>,
    pub(crate) meta: MetaRead<'r>,
}

impl<O: ObjectStore> std::fmt::Debug for RepoRead<'_, O> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RepoRead")
            .field("meta", &self.meta)
            .finish_non_exhaustive()
    }
}

/// A changeset as the store holds it: its record in the index, and its object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredChangeset {
    /// Its id.
    pub id: ChangesetId,
    /// Its place in the journal's total order of changesets, from 0; parents come first.
    pub seq: u64,
    /// 1 + the largest parent generation; 1 for a root.
    pub generation: u64,
    /// The changeset itself, decoded from its object.
    pub changeset: Changeset,
}

impl<O: ObjectStore> Repo<O> {
    /// A snapshot of the metadata as committed now ([`swarmops_store::MetaStore::read`]), to make
    /// the reads of one request in: see [`RepoRead`] for its two rules.
    pub fn snapshot(&self) -> Result<RepoRead<'_, O>, RepoError> {
        Ok(RepoRead {
            repo: self,
            meta: self.meta.read()?,
        })
    }

    /// Changeset `id`, if the store holds it, in a snapshot of its own
    /// ([`RepoRead::changeset`]).
    pub fn changeset(&self, id: ChangesetId) -> Result<Option<StoredChangeset>, RepoError> {
        self.snapshot()?.changeset(id)
    }

    /// The changeset revision `rev` names, in a snapshot of its own ([`RepoRead::resolve`]).
    pub fn resolve(&self, rev: &str) -> Result<ChangesetId, RepoError> {
        self.snapshot()?.resolve(rev)
    }

    /// The kind, the blob id and the bytes of the file at `path` in changeset `cs`'s tree, in a
    /// snapshot of its own ([`RepoRead::read_blob_at`]).
    pub fn read_blob_at(
        &self,
        cs: ChangesetId,
        path: &RepoPath,
    ) -> Result<Option<(LeafKind, BlobId, Vec<u8>)>, RepoError> {
        self.snapshot()?.read_blob_at(cs, path)
    }

    /// The entries of directory `dir` in changeset `cs`'s tree, in a snapshot of its own
    /// ([`RepoRead::list_tree`]).
    pub fn list_tree(
        &self,
        cs: ChangesetId,
        dir: Option<&RepoPath>,
    ) -> Result<Option<Vec<TreeEntry>>, RepoError> {
        self.snapshot()?.list_tree(cs, dir)
    }

    /// The changeset `meta` records, with its object.
    pub(crate) fn stored(&self, meta: &ChangesetMeta) -> Result<StoredChangeset, RepoError> {
        Ok(StoredChangeset {
            id: meta.id,
            seq: meta.seq,
            generation: meta.generation,
            changeset: self.changeset_object(meta.id)?,
        })
    }

    /// The tree of the directory that `dirs` lead to from tree `root`; `None` if one of them
    /// is missing or not a directory. Reads one tree per component.
    fn descend(&self, root: TreeId, dirs: &[&str]) -> Result<Option<TreeId>, RepoError> {
        let mut at = root;
        for &dir in dirs {
            let tree = repo::tree(&self.objects, at)?;
            match find(&tree, dir).map(|entry| entry.target) {
                Some(EntryTarget::Dir(tree)) => at = tree,
                Some(EntryTarget::Leaf(..)) | None => return Ok(None),
            }
        }
        Ok(Some(at))
    }
}

impl<O: ObjectStore> RepoRead<'_, O> {
    /// Changeset `id`, if the snapshot holds it.
    pub fn changeset(&self, id: ChangesetId) -> Result<Option<StoredChangeset>, RepoError> {
        match self.meta.changeset(id)? {
            Some(meta) => self.repo.stored(&meta).map(Some),
            None => Ok(None),
        }
    }

    /// The changeset revision `rev` names in the snapshot: see [the module documentation](self)
    /// for the grammar. [`RepoError::NotFound`] when it names none (a ref that does not exist or
    /// names a changeset the store does not hold, an id or a prefix of none, a `~N` past a root),
    /// [`RepoError::Ambiguous`] for a prefix of more than one, [`RepoError::BadRev`] for anything
    /// that is not a revision. The errors carry `rev` as given (a dangling ref's error adds the
    /// changeset it names).
    pub fn resolve(&self, rev: &str) -> Result<ChangesetId, RepoError> {
        let bad = || RepoError::BadRev(rev.to_owned());
        let mut parts = rev.split('~');
        let base = parts.next().expect("split yields a first part");
        let mut steps: u64 = 0;
        for part in parts {
            if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
                return Err(bad());
            }
            let n: u64 = part.parse().map_err(|_| bad())?;
            steps = steps.checked_add(n).ok_or_else(bad)?;
        }
        let mut id = self.resolve_base(base, rev)?;
        for _ in 0..steps {
            id = self
                .first_parent(id)?
                .ok_or_else(|| RepoError::NotFound(rev.to_owned()))?;
        }
        Ok(id)
    }

    /// The kind, the blob id and the bytes of the file at `path` in changeset `cs`'s tree;
    /// `None` if no file is there (nothing, or a directory). [`RepoError::NotFound`] if the
    /// snapshot does not hold `cs`.
    pub fn read_blob_at(
        &self,
        cs: ChangesetId,
        path: &RepoPath,
    ) -> Result<Option<(LeafKind, BlobId, Vec<u8>)>, RepoError> {
        match self.entry_at(cs, path)? {
            Some(EntryTarget::Leaf(kind, blob)) => Ok(Some((kind, blob, self.blob(blob)?))),
            Some(EntryTarget::Dir(_)) | None => Ok(None),
        }
    }

    /// What `path` holds in changeset `cs`'s tree: a leaf (its kind and blob), a directory (its
    /// tree), or nothing (`None`, also below a file or a missing directory). Reads the tree of
    /// each directory on the way, the root's first. [`RepoError::NotFound`] if the snapshot does
    /// not hold `cs`.
    pub fn entry_at(
        &self,
        cs: ChangesetId,
        path: &RepoPath,
    ) -> Result<Option<EntryTarget>, RepoError> {
        let root = self.root_of(cs)?;
        let components: Vec<&str> = path.components().collect();
        let (&name, dirs) = components.split_last().expect("a path has a component");
        let Some(dir) = self.repo.descend(root, dirs)? else {
            return Ok(None);
        };
        Ok(find(&self.tree(dir)?, name).map(|entry| entry.target))
    }

    /// Tree `id`, decoded, its object verified against its id as every object read is.
    /// [`RepoError::MissingObject`] if the store lacks it, or holds an object of another kind
    /// under that id.
    pub fn tree(&self, id: TreeId) -> Result<Tree, RepoError> {
        repo::tree(&self.repo.objects, id)
    }

    /// The bytes of blob `id`, its object verified against its id as every object read is.
    /// [`RepoError::MissingObject`] if the store lacks it, or holds an object of another kind
    /// under that id.
    pub fn blob(&self, id: BlobId) -> Result<Vec<u8>, RepoError> {
        repo::object(&self.repo.objects, id.0, ObjectKind::Blob)
    }

    /// The entries of directory `dir` (the root for `None`) in changeset `cs`'s tree, in
    /// canonical order; `None` if no directory is there (nothing, or a file).
    /// [`RepoError::NotFound`] if the snapshot does not hold `cs`.
    pub fn list_tree(
        &self,
        cs: ChangesetId,
        dir: Option<&RepoPath>,
    ) -> Result<Option<Vec<TreeEntry>>, RepoError> {
        let root = self.root_of(cs)?;
        let components: Vec<&str> = dir
            .map(|dir| dir.components().collect())
            .unwrap_or_default();
        match self.repo.descend(root, &components)? {
            Some(tree) => Ok(Some(self.tree(tree)?.entries().to_vec())),
            None => Ok(None),
        }
    }

    /// The root tree of changeset `cs`; [`RepoError::NotFound`] if the snapshot does not hold
    /// it.
    fn root_of(&self, cs: ChangesetId) -> Result<TreeId, RepoError> {
        if self.meta.changeset(cs)?.is_none() {
            return Err(RepoError::NotFound(cs.to_string()));
        }
        Ok(self.repo.changeset_object(cs)?.root)
    }

    /// The changeset that `base`, a revision without its `~N` suffixes, names in the snapshot.
    /// The errors carry `rev`, the whole revision.
    fn resolve_base(&self, base: &str, rev: &str) -> Result<ChangesetId, RepoError> {
        let read = &self.meta;
        let not_found = || RepoError::NotFound(rev.to_owned());
        let hex = !base.is_empty() && base.bytes().all(|b| b.is_ascii_hexdigit());
        if !hex {
            // No ref name is all hex digits: main, or release/<x>.
            return match read.get_ref(base) {
                Ok(Some(state)) if read.changeset(state.target)?.is_some() => Ok(state.target),
                // A dangling ref (damage verify reports): the revision names nothing.
                Ok(Some(state)) => Err(RepoError::NotFound(format!(
                    "{rev} (ref {base} names {}, which the store does not hold)",
                    state.target
                ))),
                Ok(None) => Err(not_found()),
                Err(StoreError::RefName(_)) => Err(RepoError::BadRev(rev.to_owned())),
                Err(e) => Err(e.into()),
            };
        }
        match base.len() {
            64 => {
                let id = ChangesetId(ObjectId::from_hex(base).expect("64 hex digits"));
                match read.changeset(id)? {
                    Some(_) => Ok(id),
                    None => Err(not_found()),
                }
            }
            12..=63 => match read.resolve_prefix(base)?[..] {
                [] => Err(not_found()),
                [id] => Ok(id),
                _ => Err(RepoError::Ambiguous(rev.to_owned())),
            },
            _ => Err(RepoError::BadRev(rev.to_owned())),
        }
    }
}

/// The entry named `name` of `tree`: a binary search, the entries being sorted by name bytes.
fn find<'t>(tree: &'t Tree, name: &str) -> Option<&'t TreeEntry> {
    let entries = tree.entries();
    entries
        .binary_search_by(|entry| entry.name.as_bytes().cmp(name.as_bytes()))
        .ok()
        .map(|at| &entries[at])
}
