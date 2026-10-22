//! The detached land (operations spec §3, the detached form): [`Repo::land_detached`] merges a
//! change, the first-parent chain `(base, head]`, three-way onto a changeset, `onto`, and writes
//! the result as one new changeset, `L`, whose one parent is `onto`, with no ref moved. A sync
//! uses it to move changes not yet published onto a remote's head before it moves `main`.
//!
//! It is a land's first attempt without what a ref brings: no key walk and no race, so no retry.
//! It shares the land's steps ([`super`]): the chain, the merge, the conflict report and the one
//! writer of `L`. Then `L`'s insert alone is committed, in a journal frame that moves no ref:
//!
//! ```text
//!   base ── … ── head            the change
//!     \
//!      … ── onto ── L            L = merge(base → head onto onto), no ref names it
//! ```
//!
//! `L` records the head's `agent` and `run_id`, the key given as `land_key`, the lander, the
//! caller's metadata and `squashed_from`: the chain given, or the chain's own ids.

use swarmops_core::{ChangesetId, Signature};
use swarmops_store::{InsertOutcome, ObjectStore, StoreError};

use super::key::TargetWalk;
use super::{
    NothingToLandReason, Recorded, Sides, Squash, chain, conflict_report, merge_chain, metadata,
    stored, write_squash,
};
use crate::commit::{WrittenChangeset, check_metadata};
use crate::error::RepoError;
use crate::repo::Repo;

/// A detached land to make ([`Repo::land_detached`]): the change `(base, head]` onto `onto`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetachedLand {
    /// Where the change starts: a first-parent ancestor of `head`, whose own changes are not part
    /// of it.
    pub base: ChangesetId,
    /// The change's head: the last changeset of the chain the land squashes.
    pub head: ChangesetId,
    /// The changeset to land on: `L`'s one parent. No ref need name it.
    pub onto: ChangesetId,
    /// Who lands: `L`'s committer, whose name it records as `lander`.
    pub lander: Signature,
    /// The key recorded as `land_key`, as it is: the land looks for no land made with it.
    pub key: Option<String>,
    /// The chain recorded as `squashed_from`, oldest first; `None`: the chain `(base, head]`.
    pub squashed_from: Option<Vec<ChangesetId>>,
    /// `L`'s message; `None`: the head's.
    pub message: Option<Vec<u8>>,
    /// More metadata for `L`, beside what the land records itself (`agent`, `run_id`,
    /// `land_key`, `lander` and `squashed_from`): a sync's `synced_from`, say. Any order: the land
    /// sorts it. A key the land records too makes a changeset it cannot encode:
    /// [`RepoError::Codec`], before anything is written.
    pub extra_metadata: Vec<(String, Vec<u8>)>,
}

/// What a detached land did ([`Repo::land_detached`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetachedOutcome {
    /// `L`, written and inserted: in the index, named by no ref.
    Landed(ChangesetId),
    /// The merge leaves `onto`'s tree as it is: `onto` holds the change already. Nothing is
    /// recorded.
    NoChanges,
}

impl<O: ObjectStore> Repo<O> {
    /// Lands `req` detached: merges the chain `(req.base, req.head]` three-way onto `req.onto` and
    /// writes the result as one changeset on it, recorded in one journal frame that moves no ref.
    ///
    /// [`RepoError::NotFound`] for a changeset the store does not hold;
    /// [`RepoError::BaseNotAncestor`] for a base off the head's first-parent chain;
    /// [`RepoError::MergeChangeset`] for a chain that holds a merge; [`RepoError::Conflict`], the
    /// land's report with `onto` as `theirs`, for a merge that conflicts; and nothing is recorded
    /// then. A failure after the commit point leaves `L` committed: the next write to the store
    /// repairs what is missing.
    pub fn land_detached(&self, req: DetachedLand) -> Result<DetachedOutcome, RepoError> {
        // 1. The request, in one snapshot: three changesets the store holds, the base on the
        // head's first-parent chain, the caller's keys no reserved ones.
        check_metadata(&req.extra_metadata)?;
        let read = self.snapshot()?;
        let head = stored(&read, req.head)?;
        let base = stored(&read, req.base)?;
        let onto = stored(&read, req.onto)?;
        if !read.is_ancestor(req.base, req.head)? {
            return Err(RepoError::BaseNotAncestor {
                base: req.base,
                head: req.head,
            });
        }

        // 2. The chain L squashes, which holds no merge.
        let squashed = chain(self, base.id, head.id)?;

        // 3. The merge onto `onto`: a conflict is the land's report, `onto` its `theirs`.
        let merged = merge_chain(self, &base, &head, &onto)?;
        if !merged.conflicts.is_empty() {
            let sides = Sides {
                base: base.id,
                ours: head.id,
                theirs: &onto,
            };
            let mut walk = TargetWalk::new(base.generation);
            return Err(conflict_report(
                &mut walk,
                &read,
                sides,
                merged,
                Vec::new(),
            )?);
        }
        if merged.delta.is_empty() {
            return Ok(DetachedOutcome::NoChanges);
        }

        // The snapshot is not read past the land's own write.
        drop(read);

        // 4. L, written and flushed by the land's writer (I1).
        let recorded = Recorded {
            key: req.key.as_deref(),
            lander: &req.lander,
            extra: &req.extra_metadata,
            squashed: req.squashed_from.as_deref().unwrap_or(&squashed),
        };
        let metadata = metadata(&head.changeset, recorded);
        let squash = Squash {
            theirs: &onto,
            head: &head.changeset,
            delta: merged.delta,
            new_blobs: merged.new_blobs,
            lander: req.lander,
            message: req.message,
            metadata,
        };
        let written = match write_squash(self, squash) {
            Ok(written) => written,
            Err(RepoError::NothingToLand(NothingToLandReason::NoChanges)) => {
                return Ok(DetachedOutcome::NoChanges);
            }
            Err(e) => return Err(e),
        };

        // 5. Its insert, the frame's one record.
        self.insert_detached(&written)?;
        Ok(DetachedOutcome::Landed(written.id))
    }

    /// Inserts `written` in a write transaction of its own, and commits it if the insert queued
    /// anything: a frame that moves no ref. A changeset the index holds already (the same land
    /// made again) queues nothing, and no frame is appended. A parent the store no longer holds (a
    /// frame a failed sync took back) is [`RepoError::UnknownParent`]; a failure after the commit
    /// point leaves the insert committed, and is no error.
    fn insert_detached(&self, written: &WrittenChangeset) -> Result<(), RepoError> {
        let mut write = self.meta.write()?;
        let insert = write
            .insert_changeset(written.meta())
            .map_err(|e| match e {
                StoreError::MissingParent(parent) => RepoError::UnknownParent(parent),
                other => other.into(),
            })?;
        if !matches!(insert, InsertOutcome::Inserted { .. }) {
            return Ok(());
        }
        match write.commit() {
            // Committed; the next write to the store repairs a step after the commit point.
            Ok(_) | Err(StoreError::PostCommit { .. }) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}
