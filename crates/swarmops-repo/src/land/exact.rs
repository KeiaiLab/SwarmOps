//! The landing queue's exact publish (the M22 core design §2 I-1, I-2 and I-8, §5 step 8): a
//! round's green prefix, its chain `L1 … Lg` on the target's head H, published in one write
//! transaction, one frame.
//!
//! ```text
//!   H ── L1 ── L2 ── … ── Lg          cas_ref(target, H → Lg)
//!        │     │          │           Landed(L1) on entry 1, … Landed(Lg) on entry g,
//!        e1    e2         eg          each at the version its caller read (Expect::Version)
//! ```
//!
//! Under the lock, before anything is queued, `Lg`'s root tree must have a passed verdict in the
//! target's queue: what lands is a tree its check passed. Then the target's move from H, by
//! compare-and-swap, and each entry's Landed fact, by version compare-and-swap. A target that
//! names another head ([`ExactPublished::Moved`]) or an entry whose facts moved past its version,
//! as a withdraw moves them ([`ExactPublished::Mismatch`]), drops the transaction: nothing is
//! recorded, and the next round builds again. The publish is never made anew on a moved head
//! (`Relocate::Never`'s promise), so there is no in-flight state to recover: a crash leaves the
//! frame committed whole or not at all.

use swarmops_core::{ChangesetId, TreeId};
use swarmops_store::{
    CasResult, CommitInfo, Expect, Fact, FactKind, ObjectStore, StoreError, ref_name_key,
};

use crate::error::RepoError;
use crate::oplog::OpId;
use crate::queue::{Entry, EntryId, VerdictState};
use crate::repo::{Repo, post_commit_warning, ref_error};

use super::MAX_GROUP_CHANGES;

/// The Queue fact type of a Landed (the M22 core design §3 table): `u32 version, id landed`.
const LANDED: u8 = 3;

/// A round's green prefix to publish ([`Repo::publish_exact`]).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ExactPublish {
    /// The target.
    pub target: String,
    /// The target's head the chain was built on: the target moves only from it.
    pub on_head: ChangesetId,
    /// The prefix's last changeset, `Lg`: the target's new head.
    pub tip: ChangesetId,
    /// The prefix's entries in enqueue order, each with the version its caller read: entry i
    /// lands as the chain's i-th changeset. 1 to [`MAX_GROUP_CHANGES`] of them.
    pub landed: Vec<(EntryId, u32)>,
    /// Who moves the target and writes the facts: at most 255 bytes.
    pub actor: String,
}

impl ExactPublish {
    /// A publish of `tip`, on `on_head`, onto `target` by `actor`, landing no entry yet.
    pub fn new(target: &str, on_head: ChangesetId, tip: ChangesetId, actor: &str) -> ExactPublish {
        ExactPublish {
            target: target.to_owned(),
            on_head,
            tip,
            landed: Vec::new(),
            actor: actor.to_owned(),
        }
    }
}

/// What an exact publish did.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ExactPublished {
    /// The target's move and every Landed fact are committed in one frame.
    Published {
        /// Its frame's seq: the operation.
        op: u64,
        /// Steps after the commit point that failed, one line each.
        warnings: Vec<String>,
    },
    /// The target names `actual`, not the chain's head: nothing was recorded.
    Moved {
        /// What the target names; `None` when it is gone.
        actual: Option<ChangesetId>,
    },
    /// Entry `entry`'s facts are past the version given, or it is no open entry of the target's
    /// queue: nothing was recorded.
    Mismatch {
        /// The entry.
        entry: EntryId,
    },
}

impl<O: ObjectStore> Repo<O> {
    /// Publishes `req`'s green prefix in one frame (the M22 core design §5 step 8): the target
    /// from `on_head` to `tip`, and a Landed fact for each entry, the i-th naming the chain's
    /// i-th changeset. A tip whose root tree has no passed verdict in the target's queue, a
    /// chain from `tip` that does not reach `on_head` in as many first parents as entries, or
    /// no entry or more than [`MAX_GROUP_CHANGES`], is [`StoreError::BadCall`]; a target moved,
    /// [`ExactPublished::Moved`]; an entry's version passed, [`ExactPublished::Mismatch`]. Each
    /// of these records nothing.
    pub fn publish_exact(&self, req: &ExactPublish) -> Result<ExactPublished, RepoError> {
        // 1. The request: a target name, 1 to 32 entries.
        self.check_ref_name(&req.target)?;
        let count = req.landed.len();
        if count == 0 || count > MAX_GROUP_CHANGES {
            return Err(bad_call(format!(
                "an exact publish of {count} entries: 1 to {MAX_GROUP_CHANGES}"
            )));
        }

        // 2. Lock-free: the chain, Lg back to L1, whose first parent is the head.
        let chain = self.exact_chain(req)?;

        // 3. Under the lock: the tip's tree passed, in the queue as committed.
        let mut write = self.meta.write()?;
        let fold = self.snapshot()?.queue(&req.target)?;
        let passed = (fold.verdict(chain.tree)).is_some_and(|v| v.state == VerdictState::Passed);
        if !passed {
            return Err(bad_call(format!(
                "tip {} of {}: its tree has no passed verdict",
                req.tip, req.target
            )));
        }

        // 4. The target's move from the head the chain is on.
        let moved = write
            .cas_ref(&req.target, Some(req.on_head), Some(req.tip), &req.actor)
            .map_err(ref_error)?;
        if let CasResult::Mismatch { actual } = moved {
            return Ok(ExactPublished::Moved { actual });
        }

        // 5. Each entry landed as its prefix's changeset, at the version its caller read.
        for (&(entry, version), &landed) in req.landed.iter().zip(&chain.links) {
            let open = fold.entry(entry).is_some_and(Entry::is_open);
            if !open {
                return Ok(ExactPublished::Mismatch { entry });
            }
            let fact = landed_fact(entry, &req.target, version, landed, &req.actor)?;
            if let CasResult::Mismatch { .. } = write.put_fact(fact, Expect::Version(version))? {
                return Ok(ExactPublished::Mismatch { entry });
            }
        }

        // 6. The frame: the answer only once it is durable.
        match write.commit() {
            Ok(CommitInfo { frame_seq, .. }) => Ok(ExactPublished::Published {
                op: frame_seq,
                warnings: Vec::new(),
            }),
            Err(StoreError::PostCommit {
                frame_seq,
                frame_crc,
                source,
            }) => {
                let id = OpId::new(frame_seq, frame_crc);
                let warning = post_commit_warning("the publish", id, &source);
                Ok(ExactPublished::Published {
                    op: frame_seq,
                    warnings: vec![warning],
                })
            }
            Err(e) => Err(e.into()),
        }
    }

    /// `req`'s chain, read lock-free (changesets never change): its links `L1 … Lg`, oldest
    /// first, found by first parents from `tip` down to `on_head`, and `tip`'s root tree.
    fn exact_chain(&self, req: &ExactPublish) -> Result<Chain, RepoError> {
        let read = self.snapshot()?;
        let mut links = Vec::with_capacity(req.landed.len());
        let mut at = req.tip;
        let mut tree = None;

        // 1. One first parent per entry, from the tip down.
        for _ in 0..req.landed.len() {
            let stored =
                (read.changeset(at)?).ok_or_else(|| RepoError::NotFound(at.to_string()))?;
            tree.get_or_insert(stored.changeset.root);
            links.push(at);
            at = *(stored.changeset.parents.first()).ok_or_else(|| {
                bad_call(format!("chain changeset {at} of {} has no parent", req.tip))
            })?;
        }

        // 2. The walk ends at the head the chain was built on.
        if at != req.on_head {
            return Err(bad_call(format!(
                "tip {} is not {} changesets above {}",
                req.tip,
                req.landed.len(),
                req.on_head
            )));
        }
        links.reverse();
        let tree = tree.ok_or_else(|| RepoError::Internal("an empty chain".to_owned()))?;
        Ok(Chain { links, tree })
    }
}

/// An exact publish's chain.
struct Chain {
    /// `L1 … Lg`, oldest first.
    links: Vec<ChangesetId>,
    /// `Lg`'s root tree.
    tree: TreeId,
}

/// The Landed fact of `entry` of `target` at `version`, naming `landed`, by `actor`.
fn landed_fact(
    entry: EntryId,
    target: &str,
    version: u32,
    landed: ChangesetId,
    actor: &str,
) -> Result<Fact, RepoError> {
    let mut body = version.to_le_bytes().to_vec();
    body.extend_from_slice(&landed.0.0);
    let fact = Fact::new(
        FactKind::Queue,
        LANDED,
        entry.0,
        ref_name_key(target),
        unix_now(),
        actor.to_owned(),
        body,
    )?;
    Ok(fact)
}

/// A caller's error: [`StoreError::BadCall`] with `why`.
fn bad_call(why: String) -> RepoError {
    StoreError::BadCall(why).into()
}

/// The time now, in Unix seconds.
fn unix_now() -> i64 {
    let since = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
    since.map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}
