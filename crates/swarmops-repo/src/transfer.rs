//! Objects that come from another store (the native sync design §2.3, §2.4): blobs taken as
//! stored bytes ([`Repo::put_blobs`]) and changesets adopted ([`Repo::adopt`]): recorded as the
//! bytes their store made, with no ref moved, so that a changeset keeps its id across stores.
//! Trees never cross: each adopted changeset's trees are rebuilt here from its first parent's
//! root and its delta, by the tree editor a commit uses, and the changeset is refused unless the
//! rebuilt root is its `root`.
//!
//! ```text
//!   put_blobs:  each blob's id rechecked ─▶ put ─▶ flush
//!   adopt:      per changeset, in order (parents first), nothing recorded yet:
//!                 V1 V2  kind, canonical bytes, id rechecked     else StoreError::Corrupt
//!                 held, or named before in the call            ─▶ present, no more checks
//!                 V3     each parent held or named before      else UnknownParent
//!                 V4     each blob the delta puts held          else MissingBlob
//!                 V5     root rebuilt == root, trees put        else StoreError::Corrupt
//!               per max_raw_batch(format) of the new ones:
//!                 changesets put ─▶ flush (trees, blobs too) ─▶ one journal frame
//! ```

use std::collections::HashMap;

use swarmops_core::{
    Changeset, ChangesetId, DeltaEntry, DeltaOp, Hashed, ObjectId, ObjectKind, TreeId, hash_object,
};
use swarmops_store::{
    ChangesetMeta, InsertOutcome, MetaRead, ObjectStat, ObjectStore, PutOutcome, StoreError,
};

use crate::error::RepoError;
use crate::oplog::OpId;
use crate::raw::max_raw_batch;
use crate::repo::{self, Repo};
use crate::tree_edit::{Encoded, TreeEdit};

/// What [`Repo::adopt`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct AdoptOutcome {
    /// The changesets it recorded, in the order given.
    pub adopted: Vec<ChangesetId>,
    /// The changesets the store held already, or the call named before.
    pub present: Vec<ChangesetId>,
    /// The journal frames it appended, one per batch of new changesets; none when every
    /// changeset was present.
    pub frames: Vec<u64>,
    /// Steps after a frame's commit point that failed, one line each: its changesets are
    /// recorded all the same.
    pub warnings: Vec<String>,
}

impl AdoptOutcome {
    /// Takes every field.
    pub fn new(
        adopted: Vec<ChangesetId>,
        present: Vec<ChangesetId>,
        frames: Vec<u64>,
        warnings: Vec<String>,
    ) -> AdoptOutcome {
        AdoptOutcome {
            adopted,
            present,
            frames,
            warnings,
        }
    }
}

impl<O: ObjectStore> Repo<O> {
    /// Stores `blobs`, another store's blobs as their bytes, and flushes them; says of each, in
    /// input order, whether this call placed its file. Each one's bytes are hashed again as a
    /// blob first: one that does not give the id it carries (an object of another kind) is
    /// [`StoreError::Corrupt`], refused before any blob of the call is written. A blob longer
    /// than the store takes is refused as [`ObjectStore::put_hashed_batch`] refuses it.
    pub fn put_blobs(&self, blobs: Vec<Hashed>) -> Result<Vec<PutOutcome>, RepoError> {
        // 1. Every id rechecked before anything is written.
        let wrong = blobs
            .iter()
            .find(|blob| hash_object(ObjectKind::Blob, blob.payload()) != blob.id());
        if let Some(blob) = wrong {
            return Err(StoreError::Corrupt { id: blob.id() }.into());
        }

        // 2. Written and flushed as they arrive: a later adopt's frame may name them.
        let outcomes = self.objects.put_hashed_batch(blobs)?;
        self.objects.flush()?;
        Ok(outcomes)
    }

    /// Records `changesets`, another store's changeset objects, parents first, with their ids
    /// and no ref moved (the native sync design §2.4). Each one's bytes must be a canonical
    /// changeset that hashes to the id it carries, else [`StoreError::Corrupt`]; one the store
    /// holds, or the call named before, is present and checked no further. Each parent must be
    /// held or named before ([`RepoError::UnknownParent`]), each blob its delta puts held
    /// ([`RepoError::MissingBlob`]), and its root rebuilt from its first parent's root and its
    /// delta must be its `root` ([`StoreError::Corrupt`]). Every changeset
    /// is checked, and its rebuilt trees put, before any is recorded; then the new ones are
    /// recorded [`max_raw_batch`] of the store's format to a journal frame, each frame whole or
    /// not at all, its trees and blobs durable before it (storage spec §2 rule 2). A refusal
    /// records nothing and leaves the trees put so far, unreferenced, for GC, as
    /// [`Repo::commit`] does; a failure at a later frame leaves the frames before it recorded.
    pub fn adopt(&self, changesets: Vec<Hashed>) -> Result<AdoptOutcome, RepoError> {
        let mut out = AdoptOutcome::default();
        let mut adopting = Adopting::default();

        // 1. Every changeset checked, its trees put.
        let read = self.meta.read()?;
        for object in changesets {
            self.check_adopt(&read, &mut adopting, &mut out, object)?;
        }
        drop(read);

        // 2. The new ones, a frame per batch.
        let most = max_raw_batch(self.format());
        let mut rest = adopting.new;
        while !rest.is_empty() {
            let tail = rest.split_off(rest.len().min(most));
            let batch = std::mem::replace(&mut rest, tail);
            self.adopt_frame(batch, &mut out)?;
        }
        Ok(out)
    }

    /// Object `id` and its kind, verified against its id; `None` if the store does not hold it
    /// ([`ObjectStore::get`]).
    pub fn object(&self, id: ObjectId) -> Result<Option<(ObjectKind, Vec<u8>)>, RepoError> {
        Ok(self.objects.get(id)?)
    }

    /// Step 1 of [`Repo::adopt`] for `object`, on the snapshot `read`: V1 to V5, its rebuilt
    /// trees put, and the changeset added to `adopting`; or, held or named before, to
    /// `out.present`.
    fn check_adopt(
        &self,
        read: &MetaRead<'_>,
        adopting: &mut Adopting,
        out: &mut AdoptOutcome,
        object: Hashed,
    ) -> Result<(), RepoError> {
        // 1. V1, V2: a changeset whose canonical bytes hash to the id the object carries.
        let id = ChangesetId::new(object.id());
        let corrupt = || RepoError::from(StoreError::Corrupt { id: object.id() });
        if object.kind() != ObjectKind::Changeset {
            return Err(corrupt());
        }
        let changeset = Changeset::decode(object.payload())?;
        let (encoded, delta_hash) = changeset.encode_hashed()?;
        if encoded.id() != object.id() {
            return Err(corrupt());
        }

        // 2. Held, or named before: checked when it was recorded.
        if adopting.roots.contains_key(&id) || read.changeset(id)?.is_some() {
            out.present.push(id);
            return Ok(());
        }

        // 3. V3: each parent held or named before.
        for &parent in &changeset.parents {
            if !adopting.roots.contains_key(&parent) && read.changeset(parent)?.is_none() {
                return Err(RepoError::UnknownParent(parent));
            }
        }

        // 4. V4: each blob the delta puts held; it joins the next flush, whoever stored it.
        for entry in &changeset.delta {
            let DeltaOp::Put { id: blob, .. } = entry.op else {
                continue;
            };
            match self.objects.retain(blob.0)? {
                Some(ObjectStat {
                    kind: ObjectKind::Blob,
                    ..
                }) => {}
                _ => {
                    return Err(RepoError::MissingBlob {
                        path: entry.path.clone(),
                        id: blob,
                    });
                }
            }
        }

        // 5. V5: the root rebuilt from the first parent's and the delta is `root`; its trees
        //    are put, durable at the frame's flush.
        let base = match changeset.parents.first() {
            Some(&first) => Some(self.adopted_root(adopting, first)?),
            None => None,
        };
        let rebuilt = self.rebuild(base, &changeset.delta);
        let trees = match rebuilt {
            Ok(encoded) if encoded.root == changeset.root => encoded.changed,
            Ok(_) | Err(RepoError::TypeConflict { .. } | RepoError::PathNotFound { .. }) => {
                return Err(corrupt());
            }
            Err(e) => return Err(e),
        };
        self.objects.put_hashed_batch(trees)?;

        adopting.roots.insert(id, changeset.root);
        let meta = ChangesetMeta::new(id, 0, 0, delta_hash, changeset.parents);
        adopting.new.push((object, meta));
        Ok(())
    }

    /// The root tree of `parent`, which `adopting` names or the store holds.
    fn adopted_root(&self, adopting: &Adopting, parent: ChangesetId) -> Result<TreeId, RepoError> {
        match adopting.roots.get(&parent) {
            Some(&root) => Ok(root),
            None => Ok(self.changeset_object(parent)?.root),
        }
    }

    /// The trees of `delta` applied to the root `base` (the empty tree for `None`), by the
    /// editor a commit uses.
    fn rebuild(&self, base: Option<TreeId>, delta: &[DeltaEntry]) -> Result<Encoded, RepoError> {
        let mut edit = match base {
            Some(root) => TreeEdit::new(&self.objects, root)?,
            None => TreeEdit::empty(&self.objects),
        };
        edit.apply(delta)?;
        edit.finish()
    }

    /// Step 2 of [`Repo::adopt`] for one `batch`: its changesets' objects put, then one flush,
    /// which makes the trees and blobs before them durable too, then one journal frame.
    fn adopt_frame(
        &self,
        batch: Vec<(Hashed, ChangesetMeta)>,
        out: &mut AdoptOutcome,
    ) -> Result<(), RepoError> {
        let (objects, metas): (Vec<Hashed>, Vec<ChangesetMeta>) = batch.into_iter().unzip();
        let ids: Vec<ChangesetId> = metas.iter().map(|meta| meta.id).collect();

        // 1. Objects durable before the frame names them (W5).
        self.objects.put_hashed_batch(objects)?;
        self.objects.flush()?;

        // 2. One frame; a batch another writer recorded meanwhile appends none.
        let mut write = self.meta.write()?;
        let inserts = write.insert_changesets_batch(metas).map_err(|e| match e {
            StoreError::MissingParent(parent) => RepoError::UnknownParent(parent),
            other => other.into(),
        })?;
        let inserted = |insert: &InsertOutcome| matches!(insert, InsertOutcome::Inserted { .. });
        if inserts.iter().any(inserted) {
            let frame_seq = match write.commit() {
                Ok(info) => info.frame_seq,
                Err(StoreError::PostCommit {
                    frame_seq,
                    frame_crc,
                    source,
                }) => {
                    let what = "the adopted changesets";
                    let op = OpId::new(frame_seq, frame_crc);
                    out.warnings
                        .push(repo::post_commit_warning(what, op, &source));
                    frame_seq
                }
                Err(e) => return Err(e.into()),
            };
            out.frames.push(frame_seq);
        }

        // 3. Each one adopted, or present.
        for (id, insert) in ids.into_iter().zip(&inserts) {
            if inserted(insert) {
                out.adopted.push(id);
            } else {
                out.present.push(id);
            }
        }
        Ok(())
    }
}

/// What [`Repo::adopt`] has checked and not yet recorded.
#[derive(Default)]
struct Adopting {
    /// The root of each changeset the call named and the store did not hold, by id.
    roots: HashMap<ChangesetId, TreeId>,
    /// Those changesets' objects and index records, in the call's order.
    new: Vec<(Hashed, ChangesetMeta)>,
}
