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

use std::cell::RefCell;
use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use swarmops_core::{
    BlobId, Changeset, ChangesetId, EntryTarget, ObjectId, Tree, TreeEntry, TreeId,
};
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
    let mut expected = HashMap::new();
    for listed in store.list(ExportOid::NAME, ExportOid::VERSION)? {
        let id = ChangesetId(listed.subject);
        let Some(stored) = stored_export_oid(store, id)? else {
            continue;
        };
        if let Ok(got) = export_oid_expected(&snapshot, &reader, &cache, &mut expected, id)
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
    let Some(value) = store.get(&slot(ExportOid::NAME, id.0))? else {
        return Ok(None);
    };
    // 21 bytes, as postcard writes an `ExportedOid`: the oid's 20, then one for `kept`.
    if value.len() != 21 {
        return Ok(None);
    }
    let oid = <[u8; 20]>::try_from(&value[..20]).expect("20 bytes of 21");
    Ok(Some(ExportedOid {
        oid,
        kept: value[20] != 0,
    }))
}

/// The export oid changeset `id` renders to over its real oids, as [`ExportOid::derive`] computes
/// it but reading no memo as the truth ([`Memos::Recompute`]): its [`GIT_OID`] where it is an
/// unchanged import, otherwise the hash of its rendering over its parents' recomputed export oids
/// and its root's recomputed tree oid. Memoized in `expected` for the verify's run. Walks the
/// history below `id` parents first, with a stack of its own, never by recursion: no depth of
/// history overflows the thread's stack.
fn export_oid_expected<O: ObjectStore>(
    snapshot: &RepoRead<'_, O>,
    reader: &dyn ExportObjects,
    cache: &OidCache,
    expected: &mut HashMap<ChangesetId, ExportedOid>,
    id: ChangesetId,
) -> Result<ExportedOid, RepoError> {
    if let Some(&value) = expected.get(&id) {
        return Ok(value);
    }
    let mut stack = vec![PendingChangeset::read(snapshot, id)?];
    loop {
        let top = stack
            .last_mut()
            .expect("the stack holds a changeset until id's value is known");

        // The next parent's value: known, it joins the changeset's; else the parent goes on top.
        if let Some(&parent) = top.changeset.parents.get(top.parents.len()) {
            match expected.get(&parent) {
                Some(&value) => top.parents.push(value),
                None => stack.push(PendingChangeset::read(snapshot, parent)?),
            }
            continue;
        }

        // Every parent's value is known: the changeset's own is memoized, where the changeset
        // below finds it next.
        let done = stack.pop().expect("the changeset on top");
        let value = expected_of(snapshot, reader, cache, &done)?;
        expected.insert(done.id, value);
        if stack.is_empty() {
            return Ok(value);
        }
    }
}

/// A changeset [`export_oid_expected`] is re-deriving the export oid of: its id, its object, and
/// its parents' expected export oids so far, in the changeset's order.
struct PendingChangeset {
    id: ChangesetId,
    changeset: Changeset,
    parents: Vec<ExportedOid>,
}

impl PendingChangeset {
    /// Changeset `id` of `snapshot`, no parent's value known yet. [`RepoError::NotFound`] if the
    /// snapshot does not hold it.
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

/// The export oid of `done`, a changeset whose parents' expected export oids are all known, as
/// [`export_oid_expected`] says.
fn expected_of<O: ObjectStore>(
    snapshot: &RepoRead<'_, O>,
    reader: &dyn ExportObjects,
    cache: &OidCache,
    done: &PendingChangeset,
) -> Result<ExportedOid, RepoError> {
    let (id, cs, parents) = (done.id, &done.changeset, &done.parents);
    let recorded = metadata(cs, GIT_OID);
    let mut kept_import = metadata(cs, GIT_LOSSY).is_none() && recorded.is_some();
    for (&parent, value) in cs.parents.iter().zip(parents) {
        let imported = snapshot
            .changeset(parent)?
            .is_some_and(|p| metadata(&p.changeset, GIT_OID).is_some());
        kept_import = kept_import && value.kept && imported;
    }
    let value = if let Some(own) = recorded.and_then(lowercase_oid).filter(|_| kept_import) {
        ExportedOid {
            oid: own.0,
            kept: true,
        }
    } else {
        let parent_oids: Vec<GitOid> = parents.iter().map(ExportedOid::git_oid).collect();
        let tree = tree_git_oid(reader, cache, Memos::Recompute, cs.root)?;
        let oid = git_hash(RawKind::Commit, &render_commit(id, cs, &parent_oids, tree));
        let kept = recorded.is_none_or(|hex| hex == oid.to_string().as_bytes());
        ExportedOid { oid: oid.0, kept }
    };
    Ok(value)
}
