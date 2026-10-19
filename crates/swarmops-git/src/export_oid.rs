//! Export oids (storage spec §5's `GitOid`, §7): the oid of the git commit each changeset exports
//! to, a derived value ([`ExportOid`], derived name `git_oid`), and the git oids of the trees and
//! blobs it renders, memoized per object in the same derived store ([`tree_oid`], [`blob_oid`]).
//!
//! An imported changeset whose lineage SwarmOps did not change exports to the commit it was
//! imported from, its [`GIT_OID`], taken without reading a tree: one without [`GIT_LOSSY`] whose
//! parents are all imported changesets that export to their own commits (the import maps a git
//! parent's oid only to the changeset it imported from that commit). Every other changeset is
//! rendered over its parents' export oids and its root's tree oid ([`render_commit`]), and
//! hashed: a native changeset; a lossy import, and an import whose parent was rendered anew,
//! which become new commits; and an import on a native parent (a commit made in git on an
//! export), whose rendering is its own commit again exactly when its parent's export is the
//! commit git built it on ([`ExportedOid::kept`]).
//!
//! A tree's oid is the hash of [`tree_bytes`] over its entries' oids, and a blob's the hash of its
//! bytes. Each is memoized in the derived store, the object's id as subject and the oid's 20 bytes
//! as value (`meta/derived/git_tree_oid/1/`, `meta/derived/git_blob_oid/1/`), so that a tree or a
//! blob that many changesets share is hashed once; trees are walked with a stack of their own,
//! never by recursion. As M2's engine does with its values, a memo whose put fails is dropped and
//! the oid returned all the same; a read of the derived store that fails is the error.
//!
//! Export oids read objects and derived values alone, never a ref, the journal or the clock, so a
//! changeset's export oid never changes. Version 1 fixes git's tree order and modes, the commit
//! layout, the signatures verbatim and the native trailer block ([`crate::native`]): a change to
//! any of them is a new version, which every branch exported before it meets as divergence.

use serde::{Deserialize, Serialize};
use swarmops_core::{BlobId, ChangesetId, EntryTarget, ObjectId, Tree, TreeEntry, TreeId};
use swarmops_repo::{Derivation, DeriveCtx, DeriveError, Repo, RepoError, RepoRead, Subject};
use swarmops_store::{DerivedSlot, DerivedStore, ObjectStore};

use crate::native::{metadata, render_commit};
use crate::render::tree_bytes;
use crate::{GIT_LOSSY, GIT_OID, GitOid, RawKind, git_hash};

/// The derived store's name for a tree's memoized git oid ([`tree_oid`]).
const TREE_MEMO: &str = "git_tree_oid";

/// The derived store's name for a blob's memoized git oid ([`blob_oid`]).
const BLOB_MEMO: &str = "git_blob_oid";

/// The version of both memos: a slot with no key, holding the oid's 20 bytes.
const MEMO_VERSION: u32 = 1;

/// The derivation of the git commit each changeset exports to (storage spec §5's `GitOid`,
/// derived name `git_oid`): see [the module documentation](self). Its key is `()`, its value an
/// [`ExportedOid`], and its dependencies every parent's value (the default
/// [`Derivation::deps`]). Not eager, so that no export oid is derived for a changeset no one
/// exports. [`register`] registers it.
#[derive(Debug, Clone, Copy, Default)]
pub struct ExportOid;

/// [`ExportOid`]'s value: the oid of the git commit a changeset exports to, and whether that
/// commit is the changeset's own.
///
/// Stored as postcard encodes it, the oid's 20 bytes, then one byte for `kept` (21 bytes): what
/// version 1 of [`ExportOid`] reads back, so any other layout takes a new version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ExportedOid {
    /// The commit's git oid.
    pub oid: [u8; 20],
    /// Whether the commit is the changeset's own: true for a native changeset (it has no
    /// [`GIT_OID`]), and for an imported one whose [`GIT_OID`] is `oid`. An import rendered anew,
    /// under a lossy lineage or on a parent whose export is not the commit git built it on,
    /// exports to a new commit, and is not kept.
    pub kept: bool,
}

impl ExportedOid {
    /// The commit's git oid, as a [`GitOid`].
    pub fn git_oid(&self) -> GitOid {
        GitOid(self.oid)
    }
}

impl Derivation for ExportOid {
    const NAME: &'static str = "git_oid";
    const VERSION: u32 = 1;
    const EAGER: bool = false;
    type Key = ();
    type Value = ExportedOid;

    /// The changeset's [`GIT_OID`], with no tree read, where it is an unchanged import (see [the
    /// module documentation](self)); otherwise the hash of its rendering over its parents' values
    /// and its root's [`tree_oid`].
    fn derive(
        &self,
        ctx: &DeriveCtx<'_>,
        subject: Subject<'_>,
        (): &(),
    ) -> Result<ExportedOid, DeriveError> {
        let cs = subject.changeset;
        let recorded = metadata(cs, GIT_OID);
        let mut parents = Vec::with_capacity(cs.parents.len());
        for &parent in &cs.parents {
            parents.push(ctx.value::<ExportOid>(parent, &())?);
        }
        if metadata(cs, GIT_LOSSY).is_none()
            && let Some(own) = recorded.and_then(lowercase_oid)
            && imported_and_kept(ctx, &cs.parents, &parents)?
        {
            return Ok(ExportedOid {
                oid: own.0,
                kept: true,
            });
        }
        let parents: Vec<GitOid> = parents.iter().map(ExportedOid::git_oid).collect();
        let tree = tree_oid(ctx, cs.root)?;
        let oid = git_hash(
            RawKind::Commit,
            &render_commit(subject.id, cs, &parents, tree),
        );
        let kept = recorded.is_none_or(|hex| hex == oid.to_string().as_bytes());
        Ok(ExportedOid { oid: oid.0, kept })
    }
}

/// Registers [`ExportOid`] with `repo` ([`Repo::register`]), so that [`Repo::derived`] makes
/// export oids: once per handle, before the export or the import asks for one. A second call on
/// the same handle is [`RepoError::Derived`], as is any name registered already.
pub fn register<O: ObjectStore>(repo: &mut Repo<O>) -> Result<(), RepoError> {
    repo.register(ExportOid)
}

/// What [`tree_oid`] and [`blob_oid`] read: a store's trees and blobs, and the derived store their
/// oids are memoized in. [`ExportOid`] reads through its [`DeriveCtx`], a reader outside a
/// derivation (the export) through an [`ExportReader`].
pub trait ExportObjects {
    /// Tree `id`, decoded. [`RepoError::MissingObject`] if the store lacks it.
    fn tree(&self, id: TreeId) -> Result<Tree, RepoError>;

    /// The bytes of blob `id`. [`RepoError::MissingObject`] if the store lacks it.
    fn blob(&self, id: BlobId) -> Result<Vec<u8>, RepoError>;

    /// The derived store the oids are memoized in.
    fn memo(&self) -> &DerivedStore;
}

/// A derivation's objects, and the derived store of its run ([`DeriveCtx::derived_store`]).
impl ExportObjects for DeriveCtx<'_> {
    fn tree(&self, id: TreeId) -> Result<Tree, RepoError> {
        DeriveCtx::tree(self, id).map_err(repo_error)
    }

    fn blob(&self, id: BlobId) -> Result<Vec<u8>, RepoError> {
        DeriveCtx::blob(self, id).map_err(repo_error)
    }

    fn memo(&self) -> &DerivedStore {
        self.derived_store()
    }
}

/// The trees and blobs of a snapshot, and a derived store to memoize their oids in: what a reader
/// outside a derivation reads through, such as the export, which reads the snapshot it exports
/// from and memoizes in its repository's derived store ([`Repo::derived_store`]).
pub struct ExportReader<'a, O: ObjectStore> {
    /// The snapshot the trees and blobs are read in.
    pub read: &'a RepoRead<'a, O>,
    /// The derived store the oids are memoized in.
    pub store: &'a DerivedStore,
}

impl<O: ObjectStore> ExportObjects for ExportReader<'_, O> {
    fn tree(&self, id: TreeId) -> Result<Tree, RepoError> {
        self.read.tree(id)
    }

    fn blob(&self, id: BlobId) -> Result<Vec<u8>, RepoError> {
        self.read.blob(id)
    }

    fn memo(&self) -> &DerivedStore {
        self.store
    }
}

/// The git oid of the tree `root` renders to ([`tree_bytes`], hashed as a git tree), memoized per
/// tree in `objects`' derived store, as each of its blobs' is ([`blob_oid`]): a tree whose oid is
/// memoized is not read. Walks the trees below `root` with a stack of its own, never by
/// recursion. [`RepoError::MissingObject`] if the store lacks a tree or a blob it reads; a failed
/// read of the derived store is [`RepoError::Store`].
pub fn tree_oid(objects: &dyn ExportObjects, root: TreeId) -> Result<GitOid, RepoError> {
    let memo = objects.memo();
    if let Some(oid) = memoized(memo, TREE_MEMO, root.0)? {
        return Ok(oid);
    }
    let mut stack = vec![Pending::new(root, objects.tree(root)?)];
    loop {
        let top = stack
            .last()
            .expect("the stack holds a tree until the root's oid is known");
        let next = top
            .tree
            .entries()
            .get(top.oids.len())
            .map(|entry| entry.target);
        let oid = match next {
            Some(EntryTarget::Leaf(_, blob)) => blob_oid(objects, blob)?,
            Some(EntryTarget::Dir(dir)) => {
                if let Some(oid) = memoized(memo, TREE_MEMO, dir.0)? {
                    oid
                } else {
                    stack.push(Pending::new(dir, objects.tree(dir)?));
                    continue;
                }
            }
            None => {
                // Every entry's oid is known: the tree renders, and its oid goes to its parent's.
                let done = stack.pop().expect("the tree on top");
                let bytes = tree_bytes(&done.tree, |entry| done.oid_of(entry));
                let oid = git_hash(RawKind::Tree, &bytes);
                memoize(memo, TREE_MEMO, done.id.0, oid);
                if stack.is_empty() {
                    return Ok(oid);
                }
                oid
            }
        };
        let top = stack.last_mut().expect("the tree the oid is an entry of");
        top.oids.push(oid);
    }
}

/// The git oid of blob `blob`, the hash of its bytes as a git blob, memoized per blob in
/// `objects`' derived store: a blob whose oid is memoized is not read.
/// [`RepoError::MissingObject`] if the store lacks it; a failed read of the derived store is
/// [`RepoError::Store`].
pub fn blob_oid(objects: &dyn ExportObjects, blob: BlobId) -> Result<GitOid, RepoError> {
    let memo = objects.memo();
    if let Some(oid) = memoized(memo, BLOB_MEMO, blob.0)? {
        return Ok(oid);
    }
    let oid = git_hash(RawKind::Blob, &objects.blob(blob)?);
    memoize(memo, BLOB_MEMO, blob.0, oid);
    Ok(oid)
}

/// A tree [`tree_oid`] is rendering: its id, its object, and its entries' oids so far, in the
/// tree's order.
struct Pending {
    id: TreeId,
    tree: Tree,
    oids: Vec<GitOid>,
}

impl Pending {
    fn new(id: TreeId, tree: Tree) -> Pending {
        let oids = Vec::with_capacity(tree.entries().len());
        Pending { id, tree, oids }
    }

    /// The oid of `entry`, an entry of the tree once every entry's oid is known: found by its
    /// name, as a tree's names are unique and sorted by their bytes.
    fn oid_of(&self, entry: &TreeEntry) -> GitOid {
        let name = entry.name.as_bytes();
        let at = (self.tree.entries())
            .binary_search_by(|other| other.name.as_bytes().cmp(name))
            .expect("tree_bytes gives the tree's own entries");
        self.oids[at]
    }
}

/// Whether every parent (`ids`, with their values `values`) is an imported changeset, one with
/// [`GIT_OID`], that exports to its own commit: the oids an import's commit names its parents by
/// are then their export oids. Reads each parent's changeset, up to the first that is not.
fn imported_and_kept(
    ctx: &DeriveCtx<'_>,
    ids: &[ChangesetId],
    values: &[ExportedOid],
) -> Result<bool, DeriveError> {
    for (&id, value) in ids.iter().zip(values) {
        if !value.kept || metadata(&ctx.changeset(id)?, GIT_OID).is_none() {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The git oid that `hex` writes as 40 lowercase hex digits, as the import records [`GIT_OID`];
/// `None` for anything else.
fn lowercase_oid(hex: &[u8]) -> Option<GitOid> {
    if hex.iter().all(|&b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        GitOid::from_hex(hex)
    } else {
        None
    }
}

/// The derived store's slot of `subject`'s memo `name`: version 1, no key.
fn slot(name: &str, subject: ObjectId) -> DerivedSlot<'_> {
    DerivedSlot {
        name,
        version: MEMO_VERSION,
        subject,
        key: &[],
    }
}

/// The git oid memoized for `subject` under `name`, if the derived store holds one. A value of
/// another length than an oid's 20 bytes is [`RepoError::Derived`], as a derived value that does
/// not decode is.
fn memoized(
    memo: &DerivedStore,
    name: &str,
    subject: ObjectId,
) -> Result<Option<GitOid>, RepoError> {
    let Some(value) = memo.get(&slot(name, subject))? else {
        return Ok(None);
    };
    match <[u8; 20]>::try_from(value.as_slice()) {
        Ok(oid) => Ok(Some(GitOid(oid))),
        Err(_) => Err(RepoError::Derived {
            name: name.to_owned(),
            why: format!("a memo of {} bytes, where a git oid has 20", value.len()),
        }),
    }
}

/// Memoizes `oid` for `subject` under `name`. A put that fails is dropped: the oid is the same,
/// and a later call computes it again.
fn memoize(memo: &DerivedStore, name: &str, subject: ObjectId, oid: GitOid) {
    let _ = memo.put(&slot(name, subject), &oid.0);
}

/// The [`RepoError`] of a [`DeriveError`] that a read of an object returned. Such a read never
/// returns [`DeriveError::Missing`], which only a read of a derived value does: one would be a
/// bug, reported as [`RepoError::Derived`].
fn repo_error(e: DeriveError) -> RepoError {
    match e {
        DeriveError::Repo(e) => e,
        DeriveError::Missing(need) => RepoError::Derived {
            name: ExportOid::NAME.to_owned(),
            why: need.to_string(),
        },
    }
}
