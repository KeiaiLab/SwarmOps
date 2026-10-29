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
//! the oid returned all the same; a read of the derived store that fails is the error. A blob, a
//! tree or a commit whose bytes git's collision detection flags as a SHA-1 collision attack has
//! no oid: [`RepoError::Derived`], before any memo of it or of what holds it is written.
//!
//! Neither the export, the import nor a deep verify takes a `git_oid` memo as the truth: each
//! derives every export oid it uses again, with the history below it, from the objects, the export
//! and the import checking each memo they meet against what they derive and refusing one that
//! disagrees, the verify reporting it ([`verify_export_oids`]). A deep verify checks the external
//! id entries of native changesets against their export oids the same way
//! ([`verify_external_ids`]).
//!
//! Given a [`MemoKey`], a key kept outside the store, the export and the import trust a memo
//! that it authenticates (derived names `git_oid_mac`, `git_tree_oid_mac`, `git_blob_oid_mac`,
//! version 1, the key's id as the slot key): the value, then a MAC over the name, the subject and
//! the value ([`MemoKey`]). Such a memo stops the walk: the history below a changeset whose
//! `git_oid_mac` checks is not read, nor is a tree or a blob whose memo checks. One whose MAC does
//! not check is [`RepoError::DerivedCorrupt`], never derived again in silence. Every value a run
//! with a key derives, from objects or from memos that check, is written under the key too, so
//! that a value the key authenticates comes from the store's objects by induction. Without a key
//! no `_mac` memo is read or written.
//!
//! Export oids read objects and derived values alone, never a ref, the journal or the clock, so a
//! changeset's export oid never changes. Version 1 fixes git's tree order and modes, the commit
//! layout, the signatures verbatim and the native trailer block ([`crate::native`]): a change to
//! any of them is a new version, which every branch exported before it meets as divergence.
//!
//! [`render_commit`]: native::render_commit
//! [`tree_bytes`]: render::tree_bytes

use std::cell::RefCell;
use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use swarmops_core::{
    BlobId, Changeset, ChangesetId, EntryTarget, ObjectId, Tree, TreeEntry, TreeId,
};
use swarmops_repo::{Derivation, DeriveCtx, DeriveError, Repo, RepoError, RepoRead, Subject};
use swarmops_store::{DerivedSlot, DerivedStore, ObjectStore, SYSTEM_GIT};

use crate::memo_key::{MAC_LEN, MEMO_KEY_ID_LEN};
use crate::{ExternalIdMismatch, GIT_LOSSY, GIT_OID, GitOid, MemoKey, RawKind};
use crate::{native, oid, render};

/// The derived store's name for a tree's memoized git oid ([`tree_oid`]).
const TREE_MEMO: &str = "git_tree_oid";

/// The derived store's name for a blob's memoized git oid ([`blob_oid`]).
const BLOB_MEMO: &str = "git_blob_oid";

/// The version of both memos: a slot with no key, holding the oid's 20 bytes.
const MEMO_VERSION: u32 = 1;

/// The length of a `git_oid` memo, an [`ExportedOid`] as postcard writes it: the oid's 20 bytes,
/// then one for `kept`.
const EXPORT_MEMO_LEN: usize = 21;

/// The derived store's name for a changeset's export oid under a [`MemoKey`].
const EXPORT_MAC_MEMO: &str = "git_oid_mac";

/// The derived store's name for a tree's git oid under a [`MemoKey`].
const TREE_MAC_MEMO: &str = "git_tree_oid_mac";

/// The derived store's name for a blob's git oid under a [`MemoKey`].
const BLOB_MAC_MEMO: &str = "git_blob_oid_mac";

/// The version of the three memos under a key: a slot keyed by the key's id ([`MemoKey::id`]).
const MAC_MEMO_VERSION: u32 = 1;

/// The bytes of a `git_oid_mac` memo before its MAC: the oid's 20, one for `kept`, one for
/// `imported`.
const EXPORT_MAC_VALUE_LEN: usize = 22;

/// The length of a `git_oid_mac` memo: its value, then the MAC.
const EXPORT_MAC_MEMO_LEN: usize = EXPORT_MAC_VALUE_LEN + MAC_LEN;

/// The length of a `git_tree_oid_mac` or `git_blob_oid_mac` memo: the oid's 20 bytes, then the
/// MAC.
const OID_MAC_MEMO_LEN: usize = 20 + MAC_LEN;

/// The derivation of the git commit each changeset exports to (storage spec §5's `GitOid`,
/// derived name `git_oid`): see [the module documentation](self). Its key is `()`, its value an
/// [`ExportedOid`], and its dependencies every parent's value (the default
/// [`Derivation::deps`]). Not eager, so that no export oid is derived for a changeset no one
/// exports. [`register`] registers it.
#[derive(Debug, Clone, Copy, Default)]
#[non_exhaustive]
pub struct ExportOid;

/// [`ExportOid`]'s value: the oid of the git commit a changeset exports to, and whether that
/// commit is the changeset's own.
///
/// Stored as postcard encodes it, the oid's 20 bytes, then one byte for `kept` (21 bytes): what
/// version 1 of [`ExportOid`] reads back, so any other layout takes a new version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
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
    /// The value naming `oid`, the changeset's own commit when `kept`.
    pub fn new(oid: [u8; 20], kept: bool) -> ExportedOid {
        ExportedOid { oid, kept }
    }

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
        let recorded = native::metadata(cs, GIT_OID);
        let mut parents = Vec::with_capacity(cs.parents.len());
        for &parent in &cs.parents {
            parents.push(ctx.value::<ExportOid>(parent, &())?);
        }
        if native::metadata(cs, GIT_LOSSY).is_none()
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
        let bytes = native::render_commit(subject.id, cs, &parents, tree);
        let oid = hashed(RawKind::Commit, &bytes, ExportOid::NAME, subject.id.0)?;
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
#[non_exhaustive]
pub struct ExportReader<'a, O: ObjectStore> {
    /// The snapshot the trees and blobs are read in.
    pub read: &'a RepoRead<'a, O>,
    /// The derived store the oids are memoized in.
    pub store: &'a DerivedStore,
}

impl<'a, O: ObjectStore> ExportReader<'a, O> {
    /// A reader of `read`'s trees and blobs that memoizes their oids in `store`.
    pub fn new(read: &'a RepoRead<'a, O>, store: &'a DerivedStore) -> ExportReader<'a, O> {
        ExportReader { read, store }
    }
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
/// read of the derived store is [`RepoError::Store`]; bytes git's collision detection flags are
/// [`RepoError::Derived`].
///
/// [`tree_bytes`]: render::tree_bytes
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
            Some(_) => return Err(unknown_variant("EntryTarget")),
            None => {
                // Every entry's oid is known: the tree renders, and its oid goes to its parent's.
                let done = stack.pop().expect("the tree on top");
                let bytes = render::tree_bytes(&done.tree, |entry| done.oid_of(entry))
                    .ok_or_else(|| unknown_variant("EntryTarget"))?;
                let oid = hashed(RawKind::Tree, &bytes, TREE_MEMO, done.id.0)?;
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
/// [`RepoError::Store`]; bytes git's collision detection flags are [`RepoError::Derived`].
pub fn blob_oid(objects: &dyn ExportObjects, blob: BlobId) -> Result<GitOid, RepoError> {
    let memo = objects.memo();
    if let Some(oid) = memoized(memo, BLOB_MEMO, blob.0)? {
        return Ok(oid);
    }
    let oid = hashed(RawKind::Blob, &objects.blob(blob)?, BLOB_MEMO, blob.0)?;
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
#[non_exhaustive]
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
/// ([`Against`], naming the changeset for the error a wrong memo raises), and given a key also
/// trusts the memos the key authenticates ([`Trusted`]); `verify --deep`, which reports every
/// mismatch itself rather than stopping at the first, does not ([`Recompute`]).
///
/// [`Against`]: Memos::Against
/// [`Trusted`]: Memos::Trusted
/// [`Recompute`]: Memos::Recompute
#[derive(Clone, Copy)]
enum Memos<'k> {
    /// Check each memo, refusing a crafted one ([`check_memo`]); write one where none is there.
    Against(ChangesetId),
    /// Take a tree's or a blob's oid from its memo under the key where the MAC checks, refusing
    /// one whose MAC does not ([`trusted_oid`]); check and write the rest as [`Memos::Against`]
    /// does, and write each oid computed under the key too.
    Trusted(ChangesetId, &'k MemoKey),
    /// Only recompute, reading or writing no memo.
    Recompute,
}

impl Memos<'_> {
    /// The oid memo `name` of `subject` holds under the key, if there is one and its MAC checks;
    /// `None` without a key.
    fn trusted(self, objects: &dyn ExportObjects, name: &str, subject: ObjectId) -> Trusted {
        match self {
            Memos::Trusted(about, key) => trusted_oid(objects.memo(), name, key, subject, about),
            Memos::Against(_) | Memos::Recompute => Ok(None),
        }
    }

    /// Checks `real`, the oid of `subject` computed from the store's bytes, against its v1 memo
    /// `name`, writing it there where none is; and with a key writes it under the key, as memo
    /// `mac_name`. Nothing for [`Memos::Recompute`].
    fn computed(
        self,
        objects: &dyn ExportObjects,
        (name, mac_name): (&str, &str),
        subject: ObjectId,
        real: GitOid,
    ) -> Result<(), RepoError> {
        match self {
            Memos::Against(about) => check_memo(objects.memo(), name, subject, real, about),
            Memos::Trusted(about, key) => {
                check_memo(objects.memo(), name, subject, real, about)?;
                memoize_mac(objects.memo(), mac_name, key, subject, &real.0);
                Ok(())
            }
            Memos::Recompute => Ok(()),
        }
    }
}

/// What [`Memos::trusted`] gives.
type Trusted = Result<Option<GitOid>, RepoError>;

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

/// [`real_blob_oid`], or with `key` the oid its memo under the key holds where the MAC checks,
/// the blob unread; every oid computed written under the key too. The export's.
pub(crate) fn keyed_blob_oid(
    objects: &dyn ExportObjects,
    cache: &OidCache,
    (about, key): (ChangesetId, Option<&MemoKey>),
    blob: BlobId,
) -> Result<GitOid, RepoError> {
    blob_git_oid(objects, cache, checked(about, key), blob)
}

/// [`real_tree_oid`], or with `key` taking each tree's and blob's oid from its memo under the key
/// where the MAC checks, without reading below it; every oid computed written under the key too.
/// The export's.
pub(crate) fn keyed_tree_oid(
    objects: &dyn ExportObjects,
    cache: &OidCache,
    (about, key): (ChangesetId, Option<&MemoKey>),
    root: TreeId,
) -> Result<GitOid, RepoError> {
    tree_git_oid(objects, cache, checked(about, key), root)
}

/// The memos a check of `about`'s objects reads: under `key` if there is one.
fn checked(about: ChangesetId, key: Option<&MemoKey>) -> Memos<'_> {
    match key {
        Some(key) => Memos::Trusted(about, key),
        None => Memos::Against(about),
    }
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
    memos: Memos<'_>,
    blob: BlobId,
) -> Result<GitOid, RepoError> {
    if let Some(oid) = cache.get(blob.0) {
        return Ok(oid);
    }

    // A memo the key authenticates stands for the blob's bytes, which are not read.
    if let Some(oid) = memos.trusted(objects, BLOB_MAC_MEMO, blob.0)? {
        cache.insert(blob.0, oid);
        return Ok(oid);
    }
    let real = hashed(RawKind::Blob, &objects.blob(blob)?, BLOB_MEMO, blob.0)?;
    memos.computed(objects, (BLOB_MEMO, BLOB_MAC_MEMO), blob.0, real)?;
    cache.insert(blob.0, real);
    Ok(real)
}

/// The git oid of the tree `root` renders to from the store's own bytes, the memos handled as
/// `memos` says. Walks the trees below `root` with a stack of its own, never by recursion.
fn tree_git_oid(
    objects: &dyn ExportObjects,
    cache: &OidCache,
    memos: Memos<'_>,
    root: TreeId,
) -> Result<GitOid, RepoError> {
    if let Some(oid) = known_tree(objects, cache, memos, root)? {
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
                if let Some(oid) = known_tree(objects, cache, memos, dir)? {
                    oid
                } else {
                    stack.push(Pending::new(dir, objects.tree(dir)?));
                    continue;
                }
            }
            Some(_) => return Err(unknown_variant("EntryTarget")),
            None => {
                // Every entry's oid is known: the tree renders, its oid is checked and cached.
                let done = stack.pop().expect("the tree on top");
                let bytes = render::tree_bytes(&done.tree, |entry| done.oid_of(entry))
                    .ok_or_else(|| unknown_variant("EntryTarget"))?;
                let oid = hashed(RawKind::Tree, &bytes, TREE_MEMO, done.id.0)?;
                memos.computed(objects, (TREE_MEMO, TREE_MAC_MEMO), done.id.0, oid)?;
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

/// The oid of tree `tree` known without reading it: cached in this run, or held by its memo
/// under the key, whose MAC checks (then cached).
fn known_tree(
    objects: &dyn ExportObjects,
    cache: &OidCache,
    memos: Memos<'_>,
    tree: TreeId,
) -> Trusted {
    if let Some(oid) = cache.get(tree.0) {
        return Ok(Some(oid));
    }
    let trusted = memos.trusted(objects, TREE_MAC_MEMO, tree.0)?;
    if let Some(oid) = trusted {
        cache.insert(tree.0, oid);
    }
    Ok(trusted)
}

/// The slot of `subject`'s memo `name` under `key`: version 1, the key's id as the slot key.
fn mac_slot<'a>(name: &'a str, subject: ObjectId, id: &'a [u8]) -> DerivedSlot<'a> {
    DerivedSlot::new(name, MAC_MEMO_VERSION, subject, id)
}

/// The git oid the tree or blob memo `name` of `subject` holds under `key`, if the derived store
/// holds one; one whose length or MAC does not check is [`RepoError::DerivedCorrupt`] (the
/// export's or the import's memo of `about`), so that no memo the key did not make is taken.
fn trusted_oid(
    memo: &DerivedStore,
    name: &str,
    key: &MemoKey,
    subject: ObjectId,
    about: ChangesetId,
) -> Trusted {
    let id = key.id();
    let Some(value) = memo.get(&mac_slot(name, subject, &id))? else {
        return Ok(None);
    };
    if value.len() != OID_MAC_MEMO_LEN || !key.checks(name, subject, &value[..20], &value[20..]) {
        return Err(mac_refused(name, about, subject, value.len()));
    }
    let oid = <[u8; 20]>::try_from(&value[..20]).expect("20 bytes of 52");
    Ok(Some(GitOid(oid)))
}

/// The refusal of memo `name` of `subject` under the key, of `len` bytes, whose MAC does not
/// check: [`RepoError::DerivedCorrupt`] naming `about`.
fn mac_refused(name: &str, about: ChangesetId, subject: ObjectId, len: usize) -> RepoError {
    RepoError::DerivedCorrupt {
        name: name.to_owned(),
        changeset: about,
        why: format!(
            "the memo of object {subject} under the memo key ({len} bytes) does not check \
             against its MAC"
        ),
    }
}

/// Memoizes `value` for `subject` under `key`, as memo `name`: the value, then its MAC. A put
/// that fails is dropped, as [`memoize`] drops one.
fn memoize_mac(memo: &DerivedStore, name: &str, key: &MemoKey, subject: ObjectId, value: &[u8]) {
    let mut bytes = value.to_vec();
    bytes.extend_from_slice(key.mac(name, subject, value).as_bytes());
    let _ = memo.put(&mac_slot(name, subject, &key.id()), &bytes);
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
        if !value.kept || native::metadata(&ctx.changeset(id)?, GIT_OID).is_none() {
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
    DerivedSlot::new(name, MEMO_VERSION, subject, &[])
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

/// The git oid of `bytes`, object `subject` rendered as a git `kind`, for memo `name`.
/// [`RepoError::Derived`] when git's collision detection flags the bytes as a SHA-1 collision
/// attack: git refuses to read such an object, so it has no oid to memoize or render into a
/// tree or a commit.
fn hashed(kind: RawKind, bytes: &[u8], name: &str, subject: ObjectId) -> Result<GitOid, RepoError> {
    oid::checked_hash(kind, bytes).ok_or_else(|| RepoError::Derived {
        name: name.to_owned(),
        why: format!(
            "git's collision detection flags the {kind} of object {subject} as a SHA-1 collision \
             attack, an object git refuses to read"
        ),
    })
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
        _ => unknown_variant("DeriveError"),
    }
}

/// The [`RepoError::Internal`] of a `match` on enum `ty` of another crate that found a variant
/// newer than this build.
fn unknown_variant(ty: &str) -> RepoError {
    RepoError::Internal(format!("{ty}: {}", crate::UNKNOWN_VARIANT))
}

/// A git-export oid the derived store memoized (`git_tree_oid`, `git_blob_oid` or `git_oid`, or
/// one of them under a memo key: `git_tree_oid_mac`, `git_blob_oid_mac`, `git_oid_mac`) that is
/// not the one the store's own objects give, as a deep verify re-derives it
/// ([`verify_export_oids`]), or, given the key, a memo under it whose MAC does not check
/// ([`verify_export_oids_with`]): a file someone placed in the store, or a cache a faulty build
/// wrote. Removing the file, or all of `meta/derived/`, has it derived again.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct GitMemoMismatch {
    /// The memo's derivation name: `git_tree_oid`, `git_blob_oid`, `git_oid`, or one of them
    /// followed by `_mac`.
    pub memo: String,
    /// The object the memo is about: a tree, a blob, or (for `git_oid` and `git_oid_mac`) a
    /// changeset.
    pub subject: ObjectId,
    /// The memo's slot key: the id of the key a `_mac` memo was made under
    /// ([`MemoKey::id`]); empty for the others.
    pub key: Vec<u8>,
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
    verify_export_oids_with(repo, None)
}

/// [`verify_export_oids`], which also compares the memos made under a key (`git_tree_oid_mac`,
/// `git_blob_oid_mac`, `git_oid_mac`, under any key) with what the objects give, and given `key`
/// reports each memo under it whose MAC does not check.
pub fn verify_export_oids_with<O: ObjectStore>(
    repo: &Repo<O>,
    key: Option<&MemoKey>,
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
                tree_git_oid(&reader, &cache, Memos::Recompute, TreeId::new(subject))
            } else {
                blob_git_oid(&reader, &cache, Memos::Recompute, BlobId::new(subject))
            };
            if let Ok(got) = recomputed
                && got != stored
            {
                found.push(GitMemoMismatch {
                    memo: name.to_owned(),
                    subject,
                    key: Vec::new(),
                    stored,
                    real: got,
                });
            }
        }
    }

    // Each changeset's memoized export oid, against the one it renders to over real oids.
    let mut expected = ExportOids::new(OnMemo::Skip);
    for listed in store.list(ExportOid::NAME, ExportOid::VERSION)? {
        let id = ChangesetId::new(listed.subject);
        let Some(stored) = stored_export_oid(store, id)? else {
            continue;
        };
        if let Ok(got) = expected.of(&snapshot, &reader, &cache, id)
            && (got.oid != stored.oid || got.kept != stored.kept)
        {
            found.push(GitMemoMismatch {
                memo: ExportOid::NAME.to_owned(),
                subject: id.0,
                key: Vec::new(),
                stored: stored.git_oid(),
                real: got.git_oid(),
            });
        }
    }

    // The memos under a key, under any key: their values against what the objects give, and
    // their MACs under `key`, if given.
    let macs = MacCheck {
        store,
        key,
        key_id: key.map(MemoKey::id),
    };
    for (name, is_tree) in [(BLOB_MAC_MEMO, false), (TREE_MAC_MEMO, true)] {
        for (subject, slot_key, value) in macs.listed(name)? {
            let Some(stored) = macs.oid(name, subject, &slot_key, &value) else {
                continue;
            };
            let recomputed = if is_tree {
                tree_git_oid(&reader, &cache, Memos::Recompute, TreeId::new(subject))
            } else {
                blob_git_oid(&reader, &cache, Memos::Recompute, BlobId::new(subject))
            };
            if let Ok(got) = recomputed
                && (got != stored.0 || !stored.1)
            {
                found.push(GitMemoMismatch {
                    memo: name.to_owned(),
                    subject,
                    key: slot_key,
                    stored: stored.0,
                    real: got,
                });
            }
        }
    }
    for (subject, slot_key, value) in macs.listed(EXPORT_MAC_MEMO)? {
        let Some(stored) = decode_export_mac(&value) else {
            continue;
        };
        let checks = macs.checks(EXPORT_MAC_MEMO, subject, &slot_key, &value);
        let id = ChangesetId::new(subject);
        if let Ok(got) = expected.known(&snapshot, &reader, &cache, id)
            && (got != stored || !checks)
        {
            found.push(GitMemoMismatch {
                memo: EXPORT_MAC_MEMO.to_owned(),
                subject,
                key: slot_key,
                stored: stored.value.git_oid(),
                real: got.value.git_oid(),
            });
        }
    }
    Ok(found)
}

/// A memo under a key as a deep verify lists it: its subject, its slot key and its value.
type ListedMemo = (ObjectId, Vec<u8>, Vec<u8>);

/// What a deep verify reads of the memos under a key ([`verify_export_oids_with`]).
struct MacCheck<'a> {
    store: &'a DerivedStore,
    /// The key whose memos' MACs it checks, if given.
    key: Option<&'a MemoKey>,
    key_id: Option<[u8; MEMO_KEY_ID_LEN]>,
}

impl MacCheck<'_> {
    /// Every whole memo `name` under any key: its subject, its slot key and its value.
    fn listed(&self, name: &str) -> Result<Vec<ListedMemo>, RepoError> {
        let mut memos = Vec::new();
        for listed in self.store.list(name, MAC_MEMO_VERSION)? {
            if let Some(stored) = self.store.read_listed(name, MAC_MEMO_VERSION, &listed)? {
                memos.push((listed.subject, stored.key, stored.value));
            }
        }
        Ok(memos)
    }

    /// Whether memo `name` of `subject` under slot key `slot_key`, of `value`, checks: its MAC,
    /// when the key it was made under is the one given; true for one under another key, whose
    /// MAC no one here can check.
    fn checks(&self, name: &str, subject: ObjectId, slot_key: &[u8], value: &[u8]) -> bool {
        let (Some(key), Some(id)) = (self.key, self.key_id) else {
            return true;
        };
        if slot_key != id {
            return true;
        }
        let at = value.len().saturating_sub(MAC_LEN);
        key.checks(name, subject, &value[..at], &value[at..])
    }

    /// The oid a tree or blob memo of `value` holds, and whether its MAC checks
    /// ([`MacCheck::checks`]); `None` for a value of another length.
    fn oid(
        &self,
        name: &str,
        subject: ObjectId,
        slot_key: &[u8],
        value: &[u8],
    ) -> Option<(GitOid, bool)> {
        if value.len() != OID_MAC_MEMO_LEN {
            return None;
        }
        let oid = <[u8; 20]>::try_from(&value[..20]).expect("20 bytes of 52");
        Some((GitOid(oid), self.checks(name, subject, slot_key, value)))
    }
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
        if native::metadata(&stored.changeset, GIT_OID).is_some() {
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
    DerivedSlot::new(ExportOid::NAME, ExportOid::VERSION, id.0, &[])
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
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum OnMemo {
    /// Checks each against what the store's objects give, a memo that disagrees refused
    /// ([`check_memo`], [`check_export_memo`]), and writes one where none is: the export's and
    /// the import's.
    Check,
    /// As [`OnMemo::Check`], and takes a changeset's, a tree's or a blob's value from its memo
    /// under the key where the MAC checks, reading nothing below it; a memo under the key whose
    /// MAC does not check is refused. Every value derived is written under the key too: the
    /// export's and the import's, given a key.
    Trusted(MemoKey),
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
#[derive(Clone, Copy, PartialEq, Eq)]
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
        Ok(self.known(snapshot, objects, cache, id)?.value)
    }

    /// [`ExportOids::of`], with whether the changeset was imported.
    fn known<O: ObjectStore>(
        &mut self,
        snapshot: &RepoRead<'_, O>,
        objects: &dyn ExportObjects,
        cache: &OidCache,
        id: ChangesetId,
    ) -> Result<Known, RepoError> {
        if let Some(&known) = self.known.get(&id) {
            return Ok(known);
        }
        if let Some(known) = self.trusted(objects.memo(), id)? {
            self.known.insert(id, known);
            return Ok(known);
        }
        let mut stack = vec![PendingChangeset::read(snapshot, id)?];
        loop {
            let top = stack
                .last_mut()
                .expect("the stack holds a changeset until id's value is known");

            // The next parent's value: known, or held by a memo the key authenticates, it joins
            // the changeset's; else the parent goes on top.
            if let Some(&parent) = top.changeset.parents.get(top.parents.len()) {
                if let Some(&known) = self.known.get(&parent) {
                    top.parents.push(known);
                } else if let Some(known) = self.trusted(objects.memo(), parent)? {
                    self.known.insert(parent, known);
                    top.parents.push(known);
                } else {
                    stack.push(PendingChangeset::read(snapshot, parent)?);
                }
                continue;
            }

            // Every parent's value is known: the changeset's own is kept, where the changeset
            // below finds it next.
            let done = stack.pop().expect("the changeset on top");
            let known = self.derive(objects, cache, &done)?;
            self.known.insert(done.id, known);
            if stack.is_empty() {
                return Ok(known);
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
        let recorded = native::metadata(cs, GIT_OID);
        let unchanged = native::metadata(cs, GIT_LOSSY).is_none()
            && (done.parents.iter()).all(|parent| parent.imported && parent.value.kept);
        let value = if let Some(own) = recorded.and_then(lowercase_oid).filter(|_| unchanged) {
            ExportedOid {
                oid: own.0,
                kept: true,
            }
        } else {
            let memos = match &self.on_memo {
                OnMemo::Check => Memos::Against(id),
                OnMemo::Trusted(key) => Memos::Trusted(id, key),
                OnMemo::Skip => Memos::Recompute,
            };
            let parents: Vec<GitOid> = (done.parents.iter())
                .map(|parent| parent.value.git_oid())
                .collect();
            let tree = tree_git_oid(objects, cache, memos, cs.root)?;
            let bytes = native::render_commit(id, cs, &parents, tree);
            let oid = hashed(RawKind::Commit, &bytes, ExportOid::NAME, id.0)?;
            let kept = recorded.is_none_or(|hex| hex == oid.to_string().as_bytes());
            ExportedOid { oid: oid.0, kept }
        };
        let known = Known {
            value,
            imported: recorded.is_some(),
        };
        match &self.on_memo {
            OnMemo::Check => check_export_memo(objects.memo(), id, value)?,
            OnMemo::Trusted(key) => {
                check_export_memo(objects.memo(), id, value)?;
                let mac_value = export_mac_value(known);
                memoize_mac(objects.memo(), EXPORT_MAC_MEMO, key, id.0, &mac_value);
            }
            OnMemo::Skip => {}
        }
        Ok(known)
    }

    /// Changeset `id`'s value from its `git_oid_mac` memo under the key, where the MAC checks;
    /// `None` without a key or without such a memo. One whose length, bytes or MAC does not check
    /// is [`RepoError::DerivedCorrupt`].
    fn trusted(&self, memo: &DerivedStore, id: ChangesetId) -> Result<Option<Known>, RepoError> {
        let OnMemo::Trusted(key) = &self.on_memo else {
            return Ok(None);
        };
        let key_id = key.id();
        let Some(bytes) = memo.get(&mac_slot(EXPORT_MAC_MEMO, id.0, &key_id))? else {
            return Ok(None);
        };
        let decoded = decode_export_mac(&bytes).filter(|_| {
            let (value, mac) = bytes.split_at(EXPORT_MAC_VALUE_LEN);
            key.checks(EXPORT_MAC_MEMO, id.0, value, mac)
        });
        match decoded {
            Some(known) => Ok(Some(known)),
            None => Err(mac_refused(EXPORT_MAC_MEMO, id, id.0, bytes.len())),
        }
    }
}

/// A changeset's value as its `git_oid_mac` memo holds it before the MAC: the oid's 20 bytes,
/// then one for `kept` and one for `imported`.
fn export_mac_value(known: Known) -> [u8; EXPORT_MAC_VALUE_LEN] {
    let mut bytes = [0; EXPORT_MAC_VALUE_LEN];
    bytes[..20].copy_from_slice(&known.value.oid);
    bytes[20] = u8::from(known.value.kept);
    bytes[21] = u8::from(known.imported);
    bytes
}

/// The value a `git_oid_mac` memo of `bytes` holds, its MAC unchecked; `None` for one of another
/// length, or whose `kept` or `imported` byte is neither 0 nor 1.
fn decode_export_mac(bytes: &[u8]) -> Option<Known> {
    if bytes.len() != EXPORT_MAC_MEMO_LEN {
        return None;
    }
    let flag = |b: u8| match b {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    };
    let oid = <[u8; 20]>::try_from(&bytes[..20]).expect("20 bytes of 54");
    Some(Known {
        value: ExportedOid {
            oid,
            kept: flag(bytes[20])?,
        },
        imported: flag(bytes[21])?,
    })
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

#[cfg(test)]
mod tests {
    use std::fmt;

    use swarmops_repo::LocalRepo;
    use swarmops_testkit::repo::{blob, new_repo, put, request};
    use tempfile::TempDir;

    use super::*;
    use crate::oid::flagged;

    /// The content of the file the tests have git's collision detection flag.
    const FLAGGED: &[u8] = b"flagged\n";

    /// A store with [`ExportOid`] registered, holding one changeset that puts [`FLAGGED`] at
    /// `f`: the store and the changeset.
    fn flagged_store() -> (TempDir, LocalRepo, ChangesetId) {
        let (dir, _, mut repo) = new_repo();
        register(&mut repo).unwrap();
        let id = repo
            .commit(request(&[], vec![put("f", FLAGGED)], "f\n"))
            .unwrap()
            .id;
        (dir, repo, id)
    }

    /// The memo `name` of `subject` under the slot key `key` (empty for a memo without a key),
    /// if `repo`'s derived store holds one.
    fn memo(repo: &LocalRepo, name: &str, subject: ObjectId, key: &[u8]) -> Option<Vec<u8>> {
        let slot = DerivedSlot::new(name, 1, subject, key);
        repo.derived_store().get(&slot).unwrap()
    }

    /// Checks that `got` is the refusal of an object git's collision detection flags, as memo
    /// `name`'s.
    #[track_caller]
    fn refused<T: fmt::Debug>(got: Result<T, RepoError>, name: &str) {
        match got {
            Err(RepoError::Derived { name: of, why }) => {
                assert_eq!(of, name, "{why}");
                assert!(why.contains("collision detection"), "{why}");
            }
            other => panic!("a flagged object's oid as {name}'s: {other:?}"),
        }
    }

    /// A blob git's collision detection flags has no git oid: each way of asking for its oid,
    /// its tree's or its changeset's export oid refuses, and writes no memo, with a key or not.
    #[test]
    fn a_flagged_blob_has_no_oid_and_no_memo() {
        let (_dir, repo, id) = flagged_store();
        flagged::flag(RawKind::Blob, FLAGGED);
        let read = repo.snapshot().unwrap();
        let reader = ExportReader::new(&read, repo.derived_store());
        let (file, root) = (
            blob(FLAGGED),
            read.changeset(id).unwrap().unwrap().changeset.root,
        );
        let key = MemoKey::from_bytes([1; 32]);

        // 1. The blob's oid, memoized, checked against its memo, and under a key.
        refused(blob_oid(&reader, file), BLOB_MEMO);
        refused(
            real_blob_oid(&reader, &OidCache::new(), id, file),
            BLOB_MEMO,
        );
        let keyed = keyed_blob_oid(&reader, &OidCache::new(), (id, Some(&key)), file);
        refused(keyed, BLOB_MEMO);

        // 2. The tree that holds it, and the changeset's export oid, derived and re-derived.
        refused(tree_oid(&reader, root), BLOB_MEMO);
        refused(
            real_tree_oid(&reader, &OidCache::new(), id, root),
            BLOB_MEMO,
        );
        refused(repo.derived::<ExportOid>(id, &()), BLOB_MEMO);
        let mut exports = ExportOids::new(OnMemo::Trusted(key.clone()));
        refused(exports.of(&read, &reader, &OidCache::new(), id), BLOB_MEMO);

        // 3. No memo of any of them.
        for (name, subject) in [
            (BLOB_MEMO, file.0),
            (TREE_MEMO, root.0),
            (ExportOid::NAME, id.0),
        ] {
            assert_eq!(memo(&repo, name, subject, &[]), None, "{name}");
        }
        for (name, subject) in [
            (BLOB_MAC_MEMO, file.0),
            (TREE_MAC_MEMO, root.0),
            (EXPORT_MAC_MEMO, id.0),
        ] {
            assert_eq!(memo(&repo, name, subject, &key.id()), None, "{name}");
        }
    }

    /// A tree whose rendering git's collision detection flags, and a commit whose rendering it
    /// flags, have no git oid either: refused as the tree's and the changeset's, no memo written.
    #[test]
    fn a_flagged_tree_or_commit_has_no_oid_and_no_memo() {
        let (_dir, repo, id) = flagged_store();
        let read = repo.snapshot().unwrap();
        let reader = ExportReader::new(&read, repo.derived_store());
        let cs = read.changeset(id).unwrap().unwrap().changeset;
        let tree = rendered_tree_oid(&reader, &OidCache::new(), cs.root).unwrap();
        let commit = native::render_commit(id, &cs, &[], tree);

        // 1. The commit's rendering: its export oid is refused, the tree's is not.
        flagged::flag(RawKind::Commit, &commit);
        refused(repo.derived::<ExportOid>(id, &()), ExportOid::NAME);
        let mut exports = ExportOids::new(OnMemo::Check);
        refused(
            exports.of(&read, &reader, &OidCache::new(), id),
            ExportOid::NAME,
        );
        assert_eq!(memo(&repo, ExportOid::NAME, id.0, &[]), None);
        assert_eq!(tree_oid(&reader, cs.root).unwrap(), tree);

        // 2. The tree's rendering, a store of its own: its oid is refused, memoized nowhere.
        let (_dir, repo, id) = flagged_store();
        let read = repo.snapshot().unwrap();
        let reader = ExportReader::new(&read, repo.derived_store());
        // The one file `f`, as git writes a tree.
        let entry = [
            b"100644 f\0".as_slice(),
            &oid::git_hash(RawKind::Blob, FLAGGED).0,
        ];
        flagged::flag(RawKind::Tree, &entry.concat());
        refused(tree_oid(&reader, cs.root), TREE_MEMO);
        refused(
            real_tree_oid(&reader, &OidCache::new(), id, cs.root),
            TREE_MEMO,
        );
        refused(repo.derived::<ExportOid>(id, &()), TREE_MEMO);
        assert_eq!(memo(&repo, TREE_MEMO, cs.root.0, &[]), None);
        assert_eq!(memo(&repo, ExportOid::NAME, id.0, &[]), None);
    }
}
