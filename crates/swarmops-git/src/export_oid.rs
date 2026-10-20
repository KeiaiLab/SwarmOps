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
//! Neither the export, the import nor a deep verify takes a `git_oid` memo as the truth: each
//! derives every export oid it uses again, with the history below it, from the objects, the export
//! and the import checking each memo they meet against what they derive and refusing one that
//! disagrees, the verify reporting it ([`verify_export_oids`]). A deep verify checks the external
//! id entries of native changesets against their export oids the same way
//! ([`verify_external_ids`]).
//!
//! Export oids read objects and derived values alone, never a ref, the journal or the clock, so a
//! changeset's export oid never changes. Version 1 fixes git's tree order and modes, the commit
//! layout, the signatures verbatim and the native trailer block ([`crate::native`]): a change to
//! any of them is a new version, which every branch exported before it meets as divergence.

use std::cell::RefCell;
use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use swarmops_core::{
    BlobId, Changeset, ChangesetId, EntryTarget, ObjectId, Tree, TreeEntry, TreeId,
};
use swarmops_repo::{Derivation, DeriveCtx, DeriveError, Repo, RepoError, RepoRead, Subject};
use swarmops_store::{DerivedSlot, DerivedStore, ObjectStore, SYSTEM_GIT};

use crate::native::{metadata, render_commit};
use crate::render::tree_bytes;
use crate::{ExternalIdMismatch, GIT_LOSSY, GIT_OID, GitOid, RawKind, git_hash};

/// The derived store's name for a tree's memoized git oid ([`tree_oid`]).
const TREE_MEMO: &str = "git_tree_oid";

/// The derived store's name for a blob's memoized git oid ([`blob_oid`]).
const BLOB_MEMO: &str = "git_blob_oid";

/// The version of both memos: a slot with no key, holding the oid's 20 bytes.
const MEMO_VERSION: u32 = 1;

/// The length of a `git_oid` memo, an [`ExportedOid`] as postcard writes it: the oid's 20 bytes,
/// then one for `kept`.
const EXPORT_MEMO_LEN: usize = 21;

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
    /// and its root's real tree oid ([`real_tree_oid`], the memo checked against the store's own
    /// bytes, so a crafted tree or blob memo is refused here, before an export oid is stored over
    /// it, not trusted).
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
        let cache = OidCache::new();
        let tree = real_tree_oid(ctx, &cache, subject.id, cs.root)?;
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

/// An export's own cache of the real git oids of the objects it has read in this run, each hashed
/// once, so that a tree or a blob many of the commits it writes share is read once and the whole
/// export binds to the store's own bytes without re-reading them. Keyed by the object's id.
///
/// This is not the derived store's memo ([`tree_oid`], [`blob_oid`]): [`real_tree_oid`] and
/// [`real_blob_oid`] recompute from the store's bytes and only ever check a memo against what they
/// compute, so a memo a store file names cannot stand in for an object's content.
#[derive(Default)]
pub struct OidCache(RefCell<HashMap<ObjectId, GitOid>>);

impl OidCache {
    /// A cache with nothing in it.
    pub fn new() -> OidCache {
        OidCache::default()
    }

    fn get(&self, id: ObjectId) -> Option<GitOid> {
        self.0.borrow().get(&id).copied()
    }

    fn insert(&self, id: ObjectId, oid: GitOid) {
        self.0.borrow_mut().insert(id, oid);
    }
}

/// Whether a recomputation checks the memos it finds against what it computes: the export does
/// ([`Against`], naming the changeset for the error a wrong memo raises); `verify --deep`, which
/// reports every mismatch itself rather than stopping at the first, does not ([`Recompute`]).
///
/// [`Against`]: Memos::Against
/// [`Recompute`]: Memos::Recompute
#[derive(Clone, Copy)]
enum Memos {
    /// Check each memo, refusing a crafted one ([`check_memo`]); write one where none is there.
    Against(ChangesetId),
    /// Only recompute, reading or writing no memo.
    Recompute,
}

/// The real git oid of blob `blob`, the hash of the store's own bytes as a git blob, never a memo:
/// the git oid an export reuses or writes is bound to the content the store holds. Checks the memo
/// the derived store holds for the blob against it, so a crafted memo is a typed
/// refusal, not a silent reuse of another object; writes the memo where none is there. `about` is
/// the changeset the export is on, for the error a wrong memo raises. Cached in `cache`.
pub fn real_blob_oid(
    objects: &dyn ExportObjects,
    cache: &OidCache,
    about: ChangesetId,
    blob: BlobId,
) -> Result<GitOid, RepoError> {
    blob_git_oid(objects, cache, Memos::Against(about), blob)
}

/// The real git oid of the tree `root` renders to, computed from the store's own bytes — its
/// blobs' real oids ([`real_blob_oid`]) and its subtrees' real oids — never a memo: the git oid an
/// export reuses or writes for a tree, and the child oids it renders into a tree, are the store's.
/// Checks the memo the derived store holds for each tree against what it computes,
/// so a crafted tree memo is a typed refusal, not a silent reuse; writes a memo where none is
/// there. Walks the trees below `root` with a stack of its own, never by recursion. `about` is the
/// changeset the export is on, for the error a wrong memo raises. Cached in `cache`.
pub fn real_tree_oid(
    objects: &dyn ExportObjects,
    cache: &OidCache,
    about: ChangesetId,
    root: TreeId,
) -> Result<GitOid, RepoError> {
    tree_git_oid(objects, cache, Memos::Against(about), root)
}

/// The git oid of the tree `root` renders to, from the store's own bytes alone, as
/// [`real_tree_oid`] computes it but reading and writing no memo of the derived store: what the
/// import compares a git tree with before it takes the tree as converted ([`crate::import()`]).
/// Cached in `cache`.
pub(crate) fn rendered_tree_oid(
    objects: &dyn ExportObjects,
    cache: &OidCache,
    root: TreeId,
) -> Result<GitOid, RepoError> {
    tree_git_oid(objects, cache, Memos::Recompute, root)
}

/// The git oid of blob `blob`, the hash of the store's own bytes, reading and writing no memo:
/// what the import compares a git blob with before it takes the blob as converted. Cached in
/// `cache`.
pub(crate) fn rendered_blob_oid(
    objects: &dyn ExportObjects,
    cache: &OidCache,
    blob: BlobId,
) -> Result<GitOid, RepoError> {
    blob_git_oid(objects, cache, Memos::Recompute, blob)
}

/// The git oid of blob `blob` from the store's own bytes, the memos handled as `memos` says.
fn blob_git_oid(
    objects: &dyn ExportObjects,
    cache: &OidCache,
    memos: Memos,
    blob: BlobId,
) -> Result<GitOid, RepoError> {
    if let Some(oid) = cache.get(blob.0) {
        return Ok(oid);
    }
    let real = git_hash(RawKind::Blob, &objects.blob(blob)?);
    if let Memos::Against(about) = memos {
        check_memo(objects.memo(), BLOB_MEMO, blob.0, real, about)?;
    }
    cache.insert(blob.0, real);
    Ok(real)
}

/// The git oid of the tree `root` renders to from the store's own bytes, the memos handled as
/// `memos` says. Walks the trees below `root` with a stack of its own, never by recursion.
fn tree_git_oid(
    objects: &dyn ExportObjects,
    cache: &OidCache,
    memos: Memos,
    root: TreeId,
) -> Result<GitOid, RepoError> {
    if let Some(oid) = cache.get(root.0) {
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
            Some(EntryTarget::Leaf(_, blob)) => blob_git_oid(objects, cache, memos, blob)?,
            Some(EntryTarget::Dir(dir)) => {
                if let Some(oid) = cache.get(dir.0) {
                    oid
                } else {
                    stack.push(Pending::new(dir, objects.tree(dir)?));
                    continue;
                }
            }
            None => {
                // Every entry's oid is known: the tree renders, its oid is checked and cached.
                let done = stack.pop().expect("the tree on top");
                let bytes = tree_bytes(&done.tree, |entry| done.oid_of(entry));
                let oid = git_hash(RawKind::Tree, &bytes);
                if let Memos::Against(about) = memos {
                    check_memo(objects.memo(), TREE_MEMO, done.id.0, oid, about)?;
                }
                cache.insert(done.id.0, oid);
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

/// Checks the git oid the derived store memoized for `subject` under `name` against `real`, the oid
/// the store's own bytes give: a memo that is another oid is [`RepoError::DerivedCorrupt`] (the git
/// export's memo of `about`), so whoever can place a store file cannot make a reader take one
/// object's content for another's; `verify --deep` names every such memo. A memo that agrees, or
/// none (then `real` is written, as [`memoize`]), is no error.
fn check_memo(
    memo: &DerivedStore,
    name: &str,
    subject: ObjectId,
    real: GitOid,
    about: ChangesetId,
) -> Result<(), RepoError> {
    match memoized(memo, name, subject)? {
        Some(stored) if stored != real => Err(RepoError::DerivedCorrupt {
            name: name.to_owned(),
            changeset: about,
            why: format!(
                "the git oid memoized for object {subject} is {stored}, but the store's bytes \
                 render to {real}"
            ),
        }),
        Some(_) => Ok(()),
        None => {
            memoize(memo, name, subject, real);
            Ok(())
        }
    }
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

/// A git-export oid the derived store memoized (`git_tree_oid`, `git_blob_oid` or `git_oid`) that
/// is not the one the store's own objects give, as a deep verify re-derives it ([`verify_export_oids`]):
/// a file someone placed in the store, or a cache a faulty build wrote. Removing the file, or all
/// of `meta/derived/`, has it derived again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitMemoMismatch {
    /// The memo's derivation name: `git_tree_oid`, `git_blob_oid` or `git_oid`.
    pub memo: String,
    /// The object the memo is about: a tree, a blob, or (for `git_oid`) a changeset.
    pub subject: ObjectId,
    /// The git oid the memo holds.
    pub stored: GitOid,
    /// The git oid the store's own objects give.
    pub real: GitOid,
}

/// Re-derives every git-export oid the derived store holds against the store's own objects, and
/// reports each memo that is not what they give ([`GitMemoMismatch`]): the `git_tree_oid` and
/// `git_blob_oid` of every tree and blob, recomputed from the store's bytes, and the `git_oid` of
/// every changeset, rendered over oids recomputed that way. A deep `verify` calls this (storage
/// spec §5, §7), so that a crafted or torn git memo is found as the other derived values' are; it
/// reads no `meta/derived/` memo as the truth and writes none (unlike an export, it reports every
/// mismatch rather than refusing at the first). A memo about an object the store lacks, or that
/// does not decode, is passed over (the deep verify reports the missing or corrupt object on its
/// own). [`RepoError`] only where a read cannot go on.
pub fn verify_export_oids<O: ObjectStore>(
    repo: &Repo<O>,
) -> Result<Vec<GitMemoMismatch>, RepoError> {
    let snapshot = repo.snapshot()?;
    let reader = ExportReader {
        read: &snapshot,
        store: repo.derived_store(),
    };
    let store = repo.derived_store();
    let cache = OidCache::new();
    let mut found = Vec::new();

    // Each blob's and each tree's memoized git oid, against the oid its bytes give. A memo about
    // an object the store lacks or holds damaged is passed over (`Err`): the deep verify reports
    // that object itself.
    for (name, is_tree) in [(BLOB_MEMO, false), (TREE_MEMO, true)] {
        for listed in store.list(name, MEMO_VERSION)? {
            let subject = listed.subject;
            let Some(stored) = listed_oid(store, name, subject)? else {
                continue;
            };
            let recomputed = if is_tree {
                tree_git_oid(&reader, &cache, Memos::Recompute, TreeId(subject))
            } else {
                blob_git_oid(&reader, &cache, Memos::Recompute, BlobId(subject))
            };
            if let Ok(got) = recomputed
                && got != stored
            {
                found.push(GitMemoMismatch {
                    memo: name.to_owned(),
                    subject,
                    stored,
                    real: got,
                });
            }
        }
    }

    // Each changeset's memoized export oid, against the one it renders to over real oids.
    let mut expected = ExportOids::new(OnMemo::Skip);
    for listed in store.list(ExportOid::NAME, ExportOid::VERSION)? {
        let id = ChangesetId(listed.subject);
        let Some(stored) = stored_export_oid(store, id)? else {
            continue;
        };
        if let Ok(got) = expected.of(&snapshot, &reader, &cache, id)
            && (got.oid != stored.oid || got.kept != stored.kept)
        {
            found.push(GitMemoMismatch {
                memo: ExportOid::NAME.to_owned(),
                subject: id.0,
                stored: stored.git_oid(),
                real: got.git_oid(),
            });
        }
    }
    Ok(found)
}

/// Checks each entry of system `git` in the external id index that names a native changeset, one
/// without [`GIT_OID`], against that changeset's export oid, re-derived from the store's own
/// objects as [`verify_export_oids`] derives it, and reports each entry whose commit is not the one
/// its changeset exports to ([`ExternalIdMismatch`]): an entry the git import passes over, which
/// no import or export writes. A deep `verify` calls this beside [`verify_export_oids`] (storage
/// spec §7). Every other entry is the store's own verify's to check ([`Repo::verify`]): one that
/// is damaged, one naming a changeset the store does not hold, and one naming a changeset with a
/// [`GIT_OID`], which it checks against that, so that each entry is reported once. An entry whose
/// changeset's export oid does not derive (an object missing or damaged below it, which the deep
/// verify reports on its own) is passed over. It reads no memo of the derived store and writes
/// none. [`RepoError`] only where a read cannot go on.
pub fn verify_external_ids<O: ObjectStore>(
    repo: &Repo<O>,
) -> Result<Vec<ExternalIdMismatch>, RepoError> {
    // The entries before the snapshot: an entry is written only once its changeset is (W8).
    let entries = repo.list_external_ids(SYSTEM_GIT)?;
    let snapshot = repo.snapshot()?;
    let reader = ExportReader {
        read: &snapshot,
        store: repo.derived_store(),
    };
    let cache = OidCache::new();
    let mut exports = ExportOids::new(OnMemo::Skip);
    let mut found = Vec::new();
    for (key, entry) in entries {
        let (Ok(entry), Ok(commit)) = (entry, <[u8; 20]>::try_from(key.as_slice())) else {
            continue;
        };
        let commit = GitOid(commit);
        // A changeset the snapshot lacks, or whose object does not read, is the store's own
        // verify's to report, as one that holds a `git.oid` is its to check.
        let Ok(Some(stored)) = snapshot.changeset(entry.changeset) else {
            continue;
        };
        if metadata(&stored.changeset, GIT_OID).is_some() {
            continue;
        }
        if let Ok(exported) = exports.of(&snapshot, &reader, &cache, entry.changeset)
            && exported.git_oid() != commit
        {
            found.push(ExternalIdMismatch {
                commit,
                changeset: entry.changeset,
            });
        }
    }
    Ok(found)
}

/// The git oid a `git_tree_oid`/`git_blob_oid` memo (key `()`) holds for `subject`, if the store
/// has one of an oid's 20 bytes; `None` for none, or a value of another length (a deep verify
/// reports a torn or malformed value no further).
fn listed_oid(
    store: &DerivedStore,
    name: &str,
    subject: ObjectId,
) -> Result<Option<GitOid>, RepoError> {
    let Some(value) = store.get(&slot(name, subject))? else {
        return Ok(None);
    };
    Ok(<[u8; 20]>::try_from(value.as_slice()).ok().map(GitOid))
}

/// The [`ExportedOid`] a `git_oid` memo holds for changeset `id`, as its 21 bytes record it (the
/// oid's 20, then one for `kept`); `None` for none, or a value of another length.
fn stored_export_oid(
    store: &DerivedStore,
    id: ChangesetId,
) -> Result<Option<ExportedOid>, RepoError> {
    let Some(value) = store.get(&export_slot(id))? else {
        return Ok(None);
    };
    if value.len() != EXPORT_MEMO_LEN {
        return Ok(None);
    }
    let oid = <[u8; 20]>::try_from(&value[..20]).expect("20 bytes of 21");
    Ok(Some(ExportedOid {
        oid,
        kept: value[20] != 0,
    }))
}

/// The derived store's slot of changeset `id`'s `git_oid` memo: [`ExportOid`]'s, whose key is
/// `()`, of no bytes.
fn export_slot(id: ChangesetId) -> DerivedSlot<'static> {
    DerivedSlot {
        name: ExportOid::NAME,
        version: ExportOid::VERSION,
        subject: id.0,
        key: &[],
    }
}

/// An [`ExportedOid`] as its `git_oid` memo holds it, as postcard writes one: the oid's 20 bytes,
/// then one for `kept`.
fn export_memo(value: ExportedOid) -> [u8; EXPORT_MEMO_LEN] {
    let mut bytes = [0; EXPORT_MEMO_LEN];
    bytes[..20].copy_from_slice(&value.oid);
    bytes[20] = u8::from(value.kept);
    bytes
}

/// Checks the export oid the derived store memoized for changeset `id` (its `git_oid` memo)
/// against `real`, the one its history gives ([`ExportOids`]): a memo that is another value is
/// [`RepoError::DerivedCorrupt`], so that no store file someone placed decides the parent a commit
/// names or the commit a branch moves to; `verify --deep` names every such memo. A memo that
/// agrees is no error, nor is none: then `real` is written, a put that fails dropped (as
/// [`memoize`]). A value of another length than an export oid's is no memo the export reads, and
/// is passed over.
fn check_export_memo(
    memo: &DerivedStore,
    id: ChangesetId,
    real: ExportedOid,
) -> Result<(), RepoError> {
    match stored_export_oid(memo, id)? {
        Some(stored) if stored != real => Err(RepoError::DerivedCorrupt {
            name: ExportOid::NAME.to_owned(),
            changeset: id,
            why: format!(
                "the export oid memoized for it is {} (kept: {}), but its history gives {} \
                 (kept: {})",
                stored.git_oid(),
                stored.kept,
                real.git_oid(),
                real.kept
            ),
        }),
        Some(_) => Ok(()),
        None => {
            let _ = memo.put(&export_slot(id), &export_memo(real));
            Ok(())
        }
    }
}

/// What re-deriving export oids does with the memos of the derived store it meets
/// ([`ExportOids`]).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum OnMemo {
    /// Checks each against what the store's objects give, a memo that disagrees refused
    /// ([`check_memo`], [`check_export_memo`]), and writes one where none is: the export's and
    /// the import's.
    Check,
    /// Reads and writes none: a deep verify's, which compares the memos with the values itself
    /// and reports every mismatch ([`verify_export_oids`]).
    Skip,
}

/// The export oids of a snapshot's changesets, each re-derived from the store's own objects as
/// [`ExportOid::derive`] computes it, but over its parents' re-derived values, never over a
/// `git_oid` memo: an unchanged import's [`GIT_OID`], any other changeset's rendering over its
/// parents' export oids and its root's real tree oid ([`tree_git_oid`]). What the export names as
/// a commit's parents, compares a branch with and moves a branch to, and what the import takes as
/// the commit a target's head stands for, so that no memo a store file could have placed decides
/// any of them (with [`OnMemo::Check`]); what a deep verify compares the memos with (with
/// [`OnMemo::Skip`]).
///
/// Each changeset is derived once, its value kept for the run: the history below one is walked
/// parents first, with a stack of its own, never by recursion, so that no depth of history
/// overflows the thread's stack.
///
/// ```text
///   h        of(h): the stack is h, b, p2; p2 is derived first (a root), then b over p2's
///  / \       value; then h's second parent, c, goes on top and is derived over p2's; then h
/// b   c      over b's and c's. Each value comes from the objects and the values below it,
///  \ /       never from a memo, and each changeset is derived once.
///   p2
/// ```
pub(crate) struct ExportOids {
    on_memo: OnMemo,
    known: HashMap<ChangesetId, Known>,
}

/// What [`ExportOids`] keeps of a changeset it derived: its export oid, and whether it was
/// imported (it has [`GIT_OID`]), which a child's check for an unchanged import reads.
#[derive(Clone, Copy)]
struct Known {
    value: ExportedOid,
    imported: bool,
}

impl ExportOids {
    /// Export oids re-derived as `on_memo` says, none yet.
    pub(crate) fn new(on_memo: OnMemo) -> ExportOids {
        ExportOids {
            on_memo,
            known: HashMap::new(),
        }
    }

    /// Changeset `id`'s export oid in `snapshot`, re-derived with every one below it not derived
    /// yet ([`ExportOids`]); its trees and blobs read through `objects`, their real git oids
    /// cached in `cache`. With [`OnMemo::Check`], the memos of each changeset it derives, its
    /// `git_oid` and its trees' and blobs', are checked against what it computes:
    /// [`RepoError::DerivedCorrupt`] for one that disagrees. [`RepoError::NotFound`] for a
    /// changeset the snapshot does not hold; a read that fails is the error, and what was derived
    /// before it is kept.
    pub(crate) fn of<O: ObjectStore>(
        &mut self,
        snapshot: &RepoRead<'_, O>,
        objects: &dyn ExportObjects,
        cache: &OidCache,
        id: ChangesetId,
    ) -> Result<ExportedOid, RepoError> {
        if let Some(known) = self.known.get(&id) {
            return Ok(known.value);
        }
        let mut stack = vec![PendingChangeset::read(snapshot, id)?];
        loop {
            let top = stack
                .last_mut()
                .expect("the stack holds a changeset until id's value is known");

            // The next parent's value: known, it joins the changeset's; else the parent goes on
            // top.
            if let Some(&parent) = top.changeset.parents.get(top.parents.len()) {
                match self.known.get(&parent) {
                    Some(&known) => top.parents.push(known),
                    None => stack.push(PendingChangeset::read(snapshot, parent)?),
                }
                continue;
            }

            // Every parent's value is known: the changeset's own is kept, where the changeset
            // below finds it next.
            let done = stack.pop().expect("the changeset on top");
            let known = self.derive(objects, cache, &done)?;
            self.known.insert(done.id, known);
            if stack.is_empty() {
                return Ok(known.value);
            }
        }
    }

    /// The export oid of `done`, a changeset whose parents' values are all known, as
    /// [`ExportOids::of`] says; its memos checked with [`OnMemo::Check`].
    fn derive(
        &self,
        objects: &dyn ExportObjects,
        cache: &OidCache,
        done: &PendingChangeset,
    ) -> Result<Known, RepoError> {
        let (id, cs) = (done.id, &done.changeset);
        let recorded = metadata(cs, GIT_OID);
        let unchanged = metadata(cs, GIT_LOSSY).is_none()
            && (done.parents.iter()).all(|parent| parent.imported && parent.value.kept);
        let value = if let Some(own) = recorded.and_then(lowercase_oid).filter(|_| unchanged) {
            ExportedOid {
                oid: own.0,
                kept: true,
            }
        } else {
            let memos = match self.on_memo {
                OnMemo::Check => Memos::Against(id),
                OnMemo::Skip => Memos::Recompute,
            };
            let parents: Vec<GitOid> = (done.parents.iter())
                .map(|parent| parent.value.git_oid())
                .collect();
            let tree = tree_git_oid(objects, cache, memos, cs.root)?;
            let oid = git_hash(RawKind::Commit, &render_commit(id, cs, &parents, tree));
            let kept = recorded.is_none_or(|hex| hex == oid.to_string().as_bytes());
            ExportedOid { oid: oid.0, kept }
        };
        if self.on_memo == OnMemo::Check {
            check_export_memo(objects.memo(), id, value)?;
        }
        Ok(Known {
            value,
            imported: recorded.is_some(),
        })
    }
}

/// A changeset [`ExportOids::of`] is re-deriving the export oid of: its id, its object, and what
/// it knows of its parents so far, in the changeset's order.
struct PendingChangeset {
    id: ChangesetId,
    changeset: Changeset,
    parents: Vec<Known>,
}

impl PendingChangeset {
    /// Changeset `id` of `snapshot`, nothing of its parents known yet. [`RepoError::NotFound`] if
    /// the snapshot does not hold it.
    fn read<O: ObjectStore>(
        snapshot: &RepoRead<'_, O>,
        id: ChangesetId,
    ) -> Result<PendingChangeset, RepoError> {
        let stored = snapshot
            .changeset(id)?
            .ok_or_else(|| RepoError::NotFound(id.to_string()))?;
        let parents = Vec::with_capacity(stored.changeset.parents.len());
        Ok(PendingChangeset {
            id,
            changeset: stored.changeset,
            parents,
        })
    }
}
