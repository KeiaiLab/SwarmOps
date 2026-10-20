//! A land's publish (operations spec §3 step (f)): the changeset it made, `L`, inserted and the
//! target moved to it in one write transaction, one journal frame, by the landing model's rule
//! for a target that moved after the land read it (umbrella decision 10). This is the baseline,
//! the frame a commit with `advance` writes: the target's compare-and-swap from the head `L` was
//! made on, which hands a moved target back to the lander. The model the M4 plan's measurement
//! chooses replaces its rule (Task 5), and keeps its types.

use swarmops_core::{Changeset, ChangesetId, RepoPath};
use swarmops_store::{CasResult, ChangesetMeta, ObjectStore, StoreError};

use crate::error::RepoError;
use crate::repo::{Repo, post_commit_warning, ref_error};

/// A land's changeset, written and flushed, to publish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Publication {
    /// The ref it lands on.
    pub target: String,
    /// The target's head it was made on, its one parent: what the publish expects the target to
    /// name.
    pub onto: ChangesetId,
    /// Its id.
    pub id: ChangesetId,
    /// The changeset, whose objects are durable.
    pub changeset: Changeset,
    /// Who moves the target, as the reflog names them: at most 255 bytes.
    pub actor: String,
}

/// What a publish did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Published {
    /// The target names `id` now, on `onto`: the changeset and the target's move to it are
    /// committed in one frame.
    Landed {
        /// The changeset the target names: the publication's, or one the lock holder made from it
        /// on the head the target had moved to.
        id: ChangesetId,
        /// Its parent: the head it landed on.
        onto: ChangesetId,
        /// Whether the lock holder made it anew on a head the target had moved to; never in the
        /// baseline.
        relocated: bool,
        /// The paths the lock holder merged under the lock to make it; none in the baseline.
        merged_in_lock: Vec<RepoPath>,
        /// Steps after the commit point that failed, one line each: the frame is committed all
        /// the same.
        warnings: Vec<String>,
    },
    /// The target names `actual`, not the head the publication was made on: nothing was recorded.
    Moved {
        /// What the target names.
        actual: ChangesetId,
    },
}

impl<O: ObjectStore> Repo<O> {
    /// Publishes `p` in one write transaction: queues `p`'s changeset (found there already, it
    /// queues nothing) and the move of its target from `p.onto` to it, by compare-and-swap, and
    /// commits both as one frame. A target that names another changeset drops the write, which
    /// appends nothing: [`Published::Moved`]. A target that does not exist is
    /// [`RepoError::NotFound`]; a parent the store no longer holds (a frame a failed sync took
    /// back), [`RepoError::UnknownParent`]. A failure after the commit point is a warning; a
    /// journal append that fails ([`StoreError::NotCommitted`]) is returned as it is.
    pub(crate) fn publish(&self, p: &Publication) -> Result<Published, RepoError> {
        // Encoded outside the lock, as everything but the frame is.
        let meta = ChangesetMeta {
            id: p.id,
            seq: 0,
            generation: 0,
            delta_hash: p.changeset.delta_hash()?,
            parents: p.changeset.parents.clone(),
        };
        let mut write = self.meta.write()?;
        write.insert_changeset(meta).map_err(|e| match e {
            StoreError::MissingParent(parent) => RepoError::UnknownParent(parent),
            other => other.into(),
        })?;
        match write
            .cas_ref(&p.target, Some(p.onto), Some(p.id), &p.actor)
            .map_err(ref_error)?
        {
            CasResult::Applied { .. } => {}
            // Returning drops the write: the queued insert with it, no frame, the lock released.
            CasResult::Mismatch {
                actual: Some(actual),
            } => return Ok(Published::Moved { actual }),
            CasResult::Mismatch { actual: None } => {
                return Err(RepoError::NotFound(p.target.clone()));
            }
        }
        let mut warnings = Vec::new();
        match write.commit() {
            Ok(_) => {}
            Err(StoreError::PostCommit { frame_seq, source }) => {
                let what = format!(
                    "the landed changeset {} and the move of ref {}",
                    p.id, p.target
                );
                warnings.push(post_commit_warning(&what, frame_seq, &source));
            }
            Err(e) => return Err(e.into()),
        }
        Ok(Published::Landed {
            id: p.id,
            onto: p.onto,
            relocated: false,
            merged_in_lock: Vec::new(),
            warnings,
        })
    }
}
