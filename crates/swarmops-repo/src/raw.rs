//! Changesets whose every field their caller gives (storage spec §7): the git import's writer.
//! [`Repo::commit_raw_batch`] records many of them in one journal frame, [`Repo::commit_raw`] one,
//! and a [`RawBatch`] ([`Repo::raw_batch`]) builds such a batch one changeset at a time, giving
//! each one's id as it comes, so that a later one can name it as a parent.
//!
//! Where [`Repo::commit`] builds a changeset's root from a change list, a raw changeset
//! ([`RawChangeset`]) is taken as given: its parents in their order (none for a root changeset,
//! which is made whatever refs exist), its signatures, extra headers, message and metadata
//! verbatim, and a root tree the caller stored already ([`Repo::put_objects`]). Its delta follows
//! from that root: the tree diff from its first parent's root (the empty tree for a root
//! changeset) to it, in the canonical form of [`delta_of`] (sorted by path, no rename), so that
//! the delta agrees with the root by construction, as [`Repo::verify`]'s root check requires.
//!
//! 1. Every raw is checked before anything is written: at most 255 parents
//!    ([`StoreError::Limit`]), none listed twice ([`StoreError::DuplicateParent`]), no metadata
//!    key that ends in `:b64` ([`RepoError::ReservedMetadataKey`], [`check_metadata`]), the keys
//!    strictly ascending ([`CodecError::UnsortedMetadata`]), and a root the store holds as a tree
//!    ([`RepoError::MissingObject`]). A batch holds at most [`MAX_RAW_BATCH`] changesets, what
//!    one journal frame holds ([`StoreError::Limit`], [`LimitKind::Records`]).
//! 2. Then, in order, each raw's parents must be in the store or earlier in the batch
//!    ([`RepoError::UnknownParent`]), and its delta is diffed from the trees the store holds: a
//!    parent earlier in the batch gives its root from the batch, one in the store from its
//!    changeset's object. A tree the diff reads that the store lacks is a
//!    [`RepoError::MissingObject`], a signature the codec refuses a [`RepoError::Codec`]. The
//!    changeset is encoded and hashed once. Still nothing is written.
//! 3. The batch's changeset objects are put in one `put_hashed_batch`, then flushed: the flush
//!    makes durable every object put on this handle since its last one too, so the trees and
//!    blobs the caller put with [`Repo::put_objects`] are durable before any metadata names them
//!    (storage spec §2 rule 2). Each root joins the flush even if another handle stored it
//!    ([`ObjectStore::retain`], in step 1).
//! 4. One write transaction inserts the batch's changesets
//!    ([`swarmops_store::MetaWrite::insert_changesets_batch`]) and commits them: one journal
//!    frame, the batch's commit point. A changeset the store holds, or the batch named before, is
//!    [`InsertOutcome::AlreadyPresent`]; a batch of nothing new appends no frame. A failure after
//!    the commit point ([`StoreError::PostCommit`]) leaves the changesets committed and is
//!    reported as a warning; a journal append that fails ([`StoreError::NotCommitted`]) is the
//!    error, the frame cut back off the journal, and the same batch made again records it, as for
//!    [`Repo::commit`]. What the transaction's recovery repaired first, of what earlier writers
//!    left, comes back in [`RawBatchOutcome::recovered`].
//!
//! [`delta_of`]: tree_diff::delta_of
//! [`check_metadata`]: commit::check_metadata

use std::collections::HashMap;

use swarmops_core::bytes;
use swarmops_core::{
    Changeset, ChangesetId, CodecError, Hashed, ObjectKind, Signature, Tree, TreeId,
};
use swarmops_merge::tree_diff::{self, PathFilter, TreeRead};
use swarmops_store::{
    ChangesetMeta, InsertOutcome, LimitKind, MetaRead, ObjectStat, ObjectStore, Recovered,
    StoreError,
};

use crate::commit;
use crate::error::RepoError;
use crate::repo::{self, Repo};

/// The most changesets a raw batch holds: the records of one journal frame (65,535), which holds
/// nothing else.
pub const MAX_RAW_BATCH: usize = 65_535;

/// A changeset to record with the fields its caller gives ([`Repo::commit_raw_batch`]); the
/// delta is the one field it does not give, as the module documentation says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawChangeset {
    /// The parents in their order, the mainline parent first: the delta is against its root.
    /// None for a root changeset, made whatever refs exist. At most 255, each once, each in the
    /// store or earlier in the batch.
    pub parents: Vec<ChangesetId>,
    /// The root tree, stored already, with the trees and blobs below it
    /// ([`Repo::put_objects`]).
    pub root: TreeId,
    /// Who wrote the change, verbatim.
    pub author: Signature,
    /// Who recorded it, verbatim.
    pub committer: Signature,
    /// The message, verbatim.
    pub message: Vec<u8>,
    /// Headers a git commit carries beyond its tree, parents and signatures (`encoding`,
    /// `gpgsig`, `mergetag`, …), in their order, verbatim.
    pub extra_headers: Vec<(Vec<u8>, Vec<u8>)>,
    /// SwarmOps-owned key/value pairs: keys strictly ascending, none ending in `:b64`
    /// ([`check_metadata`](commit::check_metadata)).
    pub metadata: Vec<(String, Vec<u8>)>,
}

/// What a raw batch did with one of its changesets ([`RawBatchOutcome::outcomes`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawOutcome {
    /// The changeset's id.
    pub id: ChangesetId,
    /// What the insert did: [`InsertOutcome::AlreadyPresent`] for a changeset the store held
    /// already, or that the batch named before.
    pub insert: InsertOutcome,
    /// How many paths the changeset changes: the length of its delta.
    pub files_changed: usize,
}

/// What [`Repo::commit_raw_batch`] (or [`RawBatch::commit`]) did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawBatchOutcome {
    /// One per changeset of the batch, in its order.
    pub outcomes: Vec<RawOutcome>,
    /// The journal frame that records the batch's changesets; `None` when the batch held none
    /// the store did not hold already (or none at all), and no frame was appended.
    pub frame_seq: Option<u64>,
    /// Steps after the commit point that failed, one line each: the changesets are committed
    /// all the same.
    pub warnings: Vec<String>,
    /// What the write transaction's recovery repaired before the insert, of what earlier writers
    /// left ([`swarmops_store::MetaWrite::recovered`]): on every outcome. It is no warning, and it
    /// is lost when the call fails after the repair. An empty batch opens no transaction, so it
    /// reports no repair.
    pub recovered: Recovered,
}

/// A batch of raw changesets in the making ([`Repo::raw_batch`]): each [`RawBatch::push`] checks
/// its raw and makes its changeset at once, as [`Repo::commit_raw_batch`] does, and gives its id,
/// which a raw pushed later can name as a parent; nothing is written until [`RawBatch::commit`]
/// writes them all, in one frame. Dropped uncommitted, it writes nothing.
pub struct RawBatch<'r, O: ObjectStore> {
    repo: &'r Repo<O>,
    /// A snapshot of the metadata, taken at the first parent the batch does not hold.
    read: Option<MetaRead<'r>>,
    /// The root of each changeset the batch holds, by id.
    roots: HashMap<ChangesetId, TreeId>,
    /// The changesets' objects, each once.
    objects: Vec<Hashed>,
    /// Their index records, in the batch's order: a changeset pushed twice is there twice.
    metas: Vec<ChangesetMeta>,
    /// The length of each one's delta, in the same order.
    changed: Vec<usize>,
}

impl<O: ObjectStore> Repo<O> {
    /// Records `raws` as changesets, in their order, in one journal frame: see [the module
    /// documentation](self) for the steps and their errors. Every raw is checked before anything
    /// is written, so a refusal leaves the store as it was; a failure once the objects are put
    /// (an I/O error, [`StoreError::Busy`], [`StoreError::NotCommitted`]) records nothing and
    /// leaves them, unreferenced, for GC, as [`Repo::commit`] does.
    pub fn commit_raw_batch(&self, raws: Vec<RawChangeset>) -> Result<RawBatchOutcome, RepoError> {
        if raws.len() > MAX_RAW_BATCH {
            return Err(too_many(raws.len()));
        }
        for raw in &raws {
            self.check_raw(raw)?;
        }
        let mut batch = self.raw_batch();
        for raw in raws {
            batch.make(raw)?;
        }
        batch.commit()
    }

    /// [`Repo::commit_raw_batch`] of `raw` alone.
    pub fn commit_raw(&self, raw: RawChangeset) -> Result<RawBatchOutcome, RepoError> {
        self.commit_raw_batch(vec![raw])
    }

    /// An empty [`RawBatch`] on this repository. It touches nothing.
    pub fn raw_batch(&self) -> RawBatch<'_, O> {
        RawBatch {
            repo: self,
            read: None,
            roots: HashMap::new(),
            objects: Vec::new(),
            metas: Vec::new(),
            changed: Vec::new(),
        }
    }

    /// Step 1 of [the module documentation](self) for `raw`: what it must be before anything is
    /// read for it. Its root joins the next flush ([`ObjectStore::retain`]): the changeset will
    /// name it, so its name must be durable before the frame, whoever stored it.
    fn check_raw(&self, raw: &RawChangeset) -> Result<(), RepoError> {
        commit::check_parent_list(&raw.parents)?;
        commit::check_metadata(&raw.metadata)?;
        if raw.metadata.windows(2).any(|pair| pair[0].0 >= pair[1].0) {
            return Err(CodecError::UnsortedMetadata.into());
        }
        match self.objects.retain(raw.root.0)? {
            Some(ObjectStat {
                kind: ObjectKind::Tree,
                ..
            }) => Ok(()),
            _ => Err(RepoError::MissingObject {
                id: raw.root.0,
                kind: ObjectKind::Tree,
            }),
        }
    }
}

impl<O: ObjectStore> RawBatch<'_, O> {
    /// Checks `raw` and makes its changeset as [`Repo::commit_raw_batch`] does (steps 1 and 2 of
    /// [the module documentation](self)), adds it to the batch, and returns its id. Nothing is
    /// written. A raw refused leaves the batch as it was; one past [`MAX_RAW_BATCH`] is
    /// [`StoreError::Limit`] ([`LimitKind::Records`]).
    pub fn push(&mut self, raw: RawChangeset) -> Result<ChangesetId, RepoError> {
        if self.len() == MAX_RAW_BATCH {
            return Err(too_many(MAX_RAW_BATCH + 1));
        }
        self.repo.check_raw(&raw)?;
        self.make(raw)
    }

    /// How many changesets the batch holds.
    pub fn len(&self) -> usize {
        self.metas.len()
    }

    /// Whether the batch holds no changeset.
    pub fn is_empty(&self) -> bool {
        self.metas.is_empty()
    }

    /// Records the batch's changesets in one journal frame (steps 3 and 4 of [the module
    /// documentation](self)). An empty batch writes nothing and takes no lock.
    pub fn commit(mut self) -> Result<RawBatchOutcome, RepoError> {
        // The snapshot only told which parents the store held.
        self.read = None;
        if self.is_empty() {
            return Ok(RawBatchOutcome {
                outcomes: Vec::new(),
                frame_seq: None,
                warnings: Vec::new(),
                recovered: Recovered::default(),
            });
        }
        let repo = self.repo;
        repo.objects.put_hashed_batch(self.objects)?;
        repo.objects.flush()?;

        let ids: Vec<ChangesetId> = self.metas.iter().map(|meta| meta.id).collect();
        let mut write = repo.meta.write()?;
        let recovered = write.recovered();
        let inserts = write
            .insert_changesets_batch(self.metas)
            .map_err(|e| match e {
                StoreError::MissingParent(parent) => RepoError::UnknownParent(parent),
                other => other.into(),
            })?;
        let mut frame_seq = None;
        let mut warnings = Vec::new();
        // A batch of changesets the store holds queued nothing: dropping the write then releases
        // the lock and appends nothing.
        if inserts
            .iter()
            .any(|insert| matches!(insert, InsertOutcome::Inserted { .. }))
        {
            frame_seq = Some(match write.commit() {
                Ok(info) => info.frame_seq,
                Err(StoreError::PostCommit { frame_seq, source }) => {
                    let what = "the batch of changesets";
                    warnings.push(repo::post_commit_warning(what, frame_seq, &source));
                    frame_seq
                }
                Err(e) => return Err(e.into()),
            });
        }
        let outcomes = (ids.into_iter().zip(inserts).zip(self.changed))
            .map(|((id, insert), files_changed)| RawOutcome {
                id,
                insert,
                files_changed,
            })
            .collect();
        Ok(RawBatchOutcome {
            outcomes,
            frame_seq,
            warnings,
            recovered,
        })
    }

    /// Step 2 of [the module documentation](self) for `raw`, checked already: its parents
    /// found, its delta diffed, its changeset made and added to the batch; its id. A raw refused
    /// here leaves the batch as it was.
    fn make(&mut self, raw: RawChangeset) -> Result<ChangesetId, RepoError> {
        let RawChangeset {
            parents,
            root,
            author,
            committer,
            message,
            extra_headers,
            metadata,
        } = raw;
        for &parent in &parents {
            self.check_known(parent)?;
        }
        let first = match parents.first() {
            Some(&first) => Some(self.root_of(first)?),
            None => None,
        };
        let trees = StoredTrees(&self.repo.objects);
        let delta = tree_diff::delta_of(&tree_diff::diff_trees(
            &trees,
            first,
            Some(root),
            &PathFilter::all(),
        )?);
        let changeset = Changeset {
            parents,
            root,
            delta,
            author,
            committer,
            message,
            extra_headers,
            metadata,
        };
        let (object, delta_hash) = changeset.encode_hashed()?;
        let id = ChangesetId(object.id());
        // A changeset pushed twice is put once; the insert finds it the second time.
        if self.roots.insert(id, root).is_none() {
            self.objects.push(object);
        }
        self.changed.push(changeset.delta.len());
        self.metas.push(ChangesetMeta {
            id,
            seq: 0,
            generation: 0,
            delta_hash,
            parents: changeset.parents,
        });
        Ok(id)
    }

    /// [`RepoError::UnknownParent`] unless the batch or the store holds `parent`; the store's
    /// metadata is read through one snapshot, taken the first time the batch lacks a parent.
    fn check_known(&mut self, parent: ChangesetId) -> Result<(), RepoError> {
        if self.roots.contains_key(&parent) {
            return Ok(());
        }
        if self.read.is_none() {
            self.read = Some(self.repo.meta.read()?);
        }
        if let Some(read) = &self.read
            && read.changeset(parent)?.is_some()
        {
            return Ok(());
        }
        Err(RepoError::UnknownParent(parent))
    }

    /// The root of `parent`, which the batch or the store holds: the batch's, or its
    /// changeset's object's.
    fn root_of(&self, parent: ChangesetId) -> Result<TreeId, RepoError> {
        match self.roots.get(&parent) {
            Some(&root) => Ok(root),
            None => Ok(self.repo.changeset_object(parent)?.root),
        }
    }
}

/// The trees of an object store, as the delta's diff reads them: each verified against its id
/// and decoded; one the store lacks, or holds as another kind, is a [`RepoError::MissingObject`].
struct StoredTrees<'o, O>(&'o O);

impl<O: ObjectStore> TreeRead for StoredTrees<'_, O> {
    type Error = RepoError;

    fn tree(&self, id: TreeId) -> Result<Tree, RepoError> {
        crate::repo::tree(self.0, id)
    }
}

/// The refusal of a batch of `got` changesets, more than one frame holds.
fn too_many(got: usize) -> RepoError {
    StoreError::Limit {
        what: LimitKind::Records,
        max: bytes::to_u64(MAX_RAW_BATCH),
        got: bytes::to_u64(got),
    }
    .into()
}
