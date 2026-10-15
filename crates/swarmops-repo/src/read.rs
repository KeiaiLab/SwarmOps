//! Reading a repository without a checkout (storage spec §4): a changeset's record and object
//! ([`Repo::changeset`]), revisions resolved to changesets ([`Repo::resolve`]), and files and
//! directories at a changeset ([`Repo::read_blob_at`], [`Repo::list_tree`]), read by walking
//! the trees from its root: one tree object per directory on the path.
//!
//! A revision (operations spec §0) is a ref name (`main`, `release/<x>`), a full id (64 hex
//! digits), or a unique id prefix of 12 to 63 hex digits, either case; followed by any number
//! of `~N` suffixes, each walking `N` first parents back (`main~2`, `main~1~1`).

use swarmops_core::{
    Changeset, ChangesetId, EntryKind, ObjectId, ObjectKind, RepoPath, Tree, TreeEntry, TreeId,
};
use swarmops_store::{ChangesetMeta, MetaRead, ObjectStore, StoreError};

use crate::error::RepoError;
use crate::log::first_parent;
use crate::repo::{self, Repo};

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
    /// Changeset `id`, if the store holds it.
    pub fn changeset(&self, id: ChangesetId) -> Result<Option<StoredChangeset>, RepoError> {
        match self.meta.read()?.changeset(id)? {
            Some(meta) => self.stored(&meta).map(Some),
            None => Ok(None),
        }
    }

    /// The changeset revision `rev` names, in one snapshot of the metadata: see
    /// [the module documentation](self) for the grammar. [`RepoError::NotFound`] when it names
    /// none (a ref that does not exist, an id or a prefix of none, a `~N` past a root),
    /// [`RepoError::Ambiguous`] for a prefix of more than one, [`RepoError::BadRev`] for
    /// anything that is not a revision. The errors carry `rev` as given.
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
        let read = self.meta.read()?;
        let mut id = resolve_base(&read, base, rev)?;
        for _ in 0..steps {
            id = first_parent(&read, id)?.ok_or_else(|| RepoError::NotFound(rev.to_owned()))?;
        }
        Ok(id)
    }

    /// The kind and bytes of the file at `path` in changeset `cs`'s tree; `None` if no file
    /// is there (nothing, or a directory). [`RepoError::NotFound`] if the store does not hold
    /// `cs`.
    pub fn read_blob_at(
        &self,
        cs: ChangesetId,
        path: &RepoPath,
    ) -> Result<Option<(EntryKind, Vec<u8>)>, RepoError> {
        let root = self.root_of(cs)?;
        let components: Vec<&str> = path.components().collect();
        let (&name, dirs) = components.split_last().expect("a path has a component");
        let Some(dir) = self.descend(root, dirs)? else {
            return Ok(None);
        };
        let tree = repo::tree(&self.objects, dir)?;
        match find(&tree, name) {
            Some(entry) if entry.kind != EntryKind::Dir => {
                let content = repo::object(&self.objects, entry.id, ObjectKind::Blob)?;
                Ok(Some((entry.kind, content)))
            }
            _ => Ok(None),
        }
    }

    /// The entries of directory `dir` (the root for `None`) in changeset `cs`'s tree, in
    /// canonical order; `None` if no directory is there (nothing, or a file).
    /// [`RepoError::NotFound`] if the store does not hold `cs`.
    pub fn list_tree(
        &self,
        cs: ChangesetId,
        dir: Option<&RepoPath>,
    ) -> Result<Option<Vec<TreeEntry>>, RepoError> {
        let root = self.root_of(cs)?;
        let components: Vec<&str> = dir
            .map(|dir| dir.components().collect())
            .unwrap_or_default();
        match self.descend(root, &components)? {
            Some(tree) => Ok(Some(repo::tree(&self.objects, tree)?.entries().to_vec())),
            None => Ok(None),
        }
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

    /// The root tree of changeset `cs`; [`RepoError::NotFound`] if the store does not hold it.
    fn root_of(&self, cs: ChangesetId) -> Result<TreeId, RepoError> {
        if self.meta.read()?.changeset(cs)?.is_none() {
            return Err(RepoError::NotFound(cs.to_string()));
        }
        Ok(self.changeset_object(cs)?.root)
    }

    /// The tree of the directory that `dirs` lead to from tree `root`; `None` if one of them
    /// is missing or not a directory. Reads one tree per component.
    fn descend(&self, root: TreeId, dirs: &[&str]) -> Result<Option<TreeId>, RepoError> {
        let mut at = root;
        for &dir in dirs {
            let tree = repo::tree(&self.objects, at)?;
            match find(&tree, dir) {
                Some(entry) if entry.kind == EntryKind::Dir => at = TreeId(entry.id),
                _ => return Ok(None),
            }
        }
        Ok(Some(at))
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

/// The changeset that `base`, a revision without its `~N` suffixes, names in snapshot `read`.
/// The errors carry `rev`, the whole revision.
fn resolve_base(read: &MetaRead<'_>, base: &str, rev: &str) -> Result<ChangesetId, RepoError> {
    let not_found = || RepoError::NotFound(rev.to_owned());
    let hex = !base.is_empty() && base.bytes().all(|b| b.is_ascii_hexdigit());
    if !hex {
        // No ref name is all hex digits: main, or release/<x>.
        return match read.get_ref(base) {
            Ok(Some(state)) => Ok(state.target),
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
