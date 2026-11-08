//! The group land (the M18 design §6.3): changes planned in order onto one head of a target,
//! as [`Repo::land_check`] plans them, the clean ones written as a chain and published in one
//! frame, k inserts and one move of the target.
//!
//! ```text
//!   attempt: snapshot ──plan──▶ L1 … Lk staged (each Li's parent Li-1, L1's the head read)
//!            ──one put, one flush (I1)──▶ one publish: k inserts, the move onto ─▶ Lk
//!            target moved since the plan ──▶ nothing recorded, the next attempt plans again
//! ```
//!
//! Each `Li` is the land its change would make on the one before: the head's author, message
//! (or the item's), `agent` and `run_id`; the lander its committer, its `lander`, the item's key
//! as `land_key`, the chain it squashed as `squashed_from`. The publish is never made anew under
//! the lock ([`super::Relocate::Never`]'s rule): what lands is the trees the plan made. A group
//! with nothing clean lands nothing, decided under the lock as a land's replay is: the target
//! still names the head the plan read, or the group plans again.

use std::collections::BTreeMap;
use std::thread;

use swarmops_core::{Changeset, ChangesetId, Signature, TreeId};
use swarmops_store::ObjectStore;

use crate::commit::{self, Draft};
use crate::error::RepoError;
use crate::read::RepoRead;
use crate::repo::{self, Repo};
use crate::staged::Staged;

use super::landing::{self, Recorded, backoff, stored};
use super::plan::{self, CleanPlan, PlanItem, PlanOutcome, Planned};
use super::publish::{GroupPublication, Published};

/// The attempts a group land makes by default ([`LandGroupRequest::new`]).
const MAX_ATTEMPTS: u32 = 8;

/// A group land to make ([`Repo::land_group`]): `items`, in order, onto `target`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct LandGroupRequest {
    /// The ref the changes land on: `main` or `release/<x>`, which must exist.
    pub target: String,
    /// The changes, in the order they land. The operations layer takes 1 to
    /// [`super::MAX_GROUP_CHANGES`].
    pub items: Vec<PlanItem>,
    /// Who lands: each landed changeset's committer, whose name it records as `lander`, and who
    /// moves the target.
    pub lander: Signature,
    /// More metadata for each landed changeset, as [`super::LandRequest::lander_metadata`].
    pub lander_metadata: Vec<(String, Vec<u8>)>,
    /// How many attempts it makes at most before [`RepoError::RetryExhausted`]; 0 counts as 1.
    /// The operations layer takes 1 to 32.
    pub max_attempts: u32,
}

impl LandGroupRequest {
    /// A group land of `items` onto `target` by `lander`: no metadata of the lander's, 8
    /// attempts at most.
    pub fn new(target: &str, items: Vec<PlanItem>, lander: Signature) -> LandGroupRequest {
        LandGroupRequest {
            target: target.to_owned(),
            items,
            lander,
            lander_metadata: Vec::new(),
            max_attempts: MAX_ATTEMPTS,
        }
    }
}

/// What a group land did ([`Repo::land_group`]).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct LandGroup {
    /// The target's head the last attempt planned on: the chain's first parent.
    pub onto: ChangesetId,
    /// The chain's publish; `None` when no change was clean, and nothing was recorded.
    pub landed: Option<GroupLanded>,
    /// Each item's outcome, in the request's order.
    pub items: Vec<GroupItem>,
    /// How many attempts it made.
    pub attempts: u32,
}

/// A group land's publish ([`LandGroup::landed`]).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct GroupLanded {
    /// The chain's last changeset: what the target names now.
    pub head: ChangesetId,
    /// The operation that recorded it: its frame's seq.
    pub op: u64,
    /// Steps after the commit point that failed, one line each: the chain is committed all the
    /// same.
    pub warnings: Vec<String>,
}

/// One item of a group land ([`LandGroup::items`]).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct GroupItem {
    /// The change's head.
    pub head: ChangesetId,
    /// What its plan came to, on the head the last attempt read.
    pub outcome: PlanOutcome,
    /// The changeset that landed it: its link of the chain, or for a replay the one a land with
    /// its key made before; `None` otherwise.
    pub landed: Option<ChangesetId>,
}

/// One attempt's end.
enum Attempt {
    /// The group is done.
    Done(LandGroup),
    /// The target names `.0`, not the head the attempt planned on.
    Moved(ChangesetId),
}

impl<O: ObjectStore> Repo<O> {
    /// Lands `req`'s changes, in order, as one operation (the M18 design §6.3):
    /// each clean one a changeset of a chain on the target's head, published in one frame with
    /// the target's move to its last. A conflicting change is reported and passed over; a
    /// replayed one or one with nothing to land lands nothing. The request fails as
    /// [`Repo::land_check`]'s does; a key's land on another ref found under the lock is
    /// [`RepoError::KeyReused`]; every attempt planned on a head the target no longer named,
    /// [`RepoError::RetryExhausted`]. Nothing is recorded unless the chain lands.
    pub fn land_group(&self, req: &LandGroupRequest) -> Result<LandGroup, RepoError> {
        self.check_ref_name(&req.target)?;
        let seed = match req.items.first() {
            Some(item) => item.head.0.0.to_vec(),
            None => Vec::new(),
        };

        // Each attempt from what the target names, after a wait from the fourth on.
        let max_attempts = req.max_attempts.max(1);
        let mut actual = None;
        for attempt in 1..=max_attempts {
            if attempt > 1 {
                thread::sleep(backoff(attempt, &seed));
            }
            match self.group_attempt(req, attempt)? {
                Attempt::Done(group) => return Ok(group),
                Attempt::Moved(now) => actual = Some(now),
            }
        }
        Err(RepoError::RetryExhausted {
            attempts: max_attempts,
            actual: actual.ok_or_else(|| RepoError::Internal("no attempt made".to_owned()))?,
        })
    }

    /// One attempt of the group land `req`: the plan in a snapshot of its own, the chain
    /// written, and its publish.
    fn group_attempt(&self, req: &LandGroupRequest, attempt: u32) -> Result<Attempt, RepoError> {
        // 1. The plan onto the target's head, in one snapshot.
        let read = self.snapshot()?;
        let theirs = read.get_ref(&req.target)?;
        let theirs = theirs.ok_or_else(|| RepoError::NotFound(req.target.clone()))?;
        let onto = stored(&read, theirs.target)?;
        let staged = Staged::new(&self.objects);
        let planned = plan::plan(self, &read, &req.target, &onto, &req.items, &staged)?;

        // 2. The chain, staged beside the plan's trees: each clean item on the one before.
        let mut chain: Vec<(ChangesetId, Changeset)> = Vec::new();
        let mut keys = Vec::new();
        let mut landed = Vec::with_capacity(planned.len());
        for (item, planned) in req.items.iter().zip(&planned) {
            let PlanOutcome::Clean(clean) = &planned.outcome else {
                landed.push(replayed(planned));
                continue;
            };
            let parent = match chain.last() {
                Some((id, changeset)) => (*id, changeset.root),
                None => (onto.id, onto.changeset.root),
            };
            let (id, changeset) = link(&read, &staged, req, (item, clean), parent)?;
            if let Some(key) = &item.key {
                keys.push((key.clone(), item.head));
            }
            chain.push((id, changeset));
            landed.push(Some(id));
        }
        let next_seq = read.next_seq();
        drop(read);
        let items = group_items(planned, landed);

        // 3. Nothing clean: decided under the lock, the target still where the plan read it.
        if chain.is_empty() {
            drop(staged);
            let now = self.target_now(&req.target)?;
            if now != onto.id {
                return Ok(Attempt::Moved(now));
            }
            return Ok(Attempt::Done(LandGroup {
                onto: onto.id,
                landed: None,
                items,
                attempts: attempt,
            }));
        }

        // 4. Every object the chain names, one put and one flush, before the frame (I1).
        self.objects.put_hashed_batch(staged.into_objects())?;
        self.objects.flush()?;

        // 5. The publish: k inserts and the one move, or nothing on a moved target.
        let g = GroupPublication {
            target: req.target.clone(),
            onto: onto.id,
            chain,
            actor: String::from_utf8_lossy(&req.lander.name).into_owned(),
            keys,
            next_seq,
        };
        match self.publish_group(&g)? {
            (Published::Landed { id, warnings, .. }, Some(op)) => Ok(Attempt::Done(LandGroup {
                onto: onto.id,
                landed: Some(GroupLanded {
                    head: id,
                    op,
                    warnings,
                }),
                items,
                attempts: attempt,
            })),
            (Published::Moved { actual, .. }, _) => Ok(Attempt::Moved(actual)),
            _ => Err(RepoError::Internal(
                "a group publish that neither landed in a frame nor found its target moved"
                    .to_owned(),
            )),
        }
    }

    /// What ref `target` names under the store's lock, after the write's tail check: a write
    /// transaction taken for that alone, and dropped without a frame.
    fn target_now(&self, target: &str) -> Result<ChangesetId, RepoError> {
        let write = self.meta.write()?;
        let named = write.get_ref(target).map_err(repo::ref_error)?;
        named.ok_or_else(|| RepoError::NotFound(target.to_owned()))
    }
}

/// The changeset a group land of `req` makes of `item`, planned `clean`, on `parent` and
/// its root: as a land of the item would make it there, written into `staged`, which holds
/// the trees and merged contents the plan made, and not yet put. Its root is the plan's.
fn link<O: ObjectStore>(
    read: &RepoRead<'_, O>,
    staged: &Staged<'_, O>,
    req: &LandGroupRequest,
    (item, clean): (&PlanItem, &CleanPlan),
    (parent, parent_root): (ChangesetId, TreeId),
) -> Result<(ChangesetId, Changeset), RepoError> {
    // 1. What the land records: the head's attribution, the key, the lander, the chain.
    let head = stored(read, item.head)?.changeset;
    let recorded = Recorded {
        key: item.key.as_deref(),
        lander: &req.lander,
        extra: &req.lander_metadata,
        squashed: &clean.chain,
    };

    // 2. The plan's delta on the parent's tree, its blobs and trees in the overlay.
    let draft = Draft {
        onto: Some(parent_root),
        changes: clean.delta.clone(),
        contents: BTreeMap::new(),
        parents: vec![parent],
        author: head.author.clone(),
        committer: req.lander.clone(),
        message: item.message.clone().unwrap_or_else(|| head.message.clone()),
        metadata: landing::metadata(&head, recorded),
        allow_empty: false,
    };
    let written = commit::write_changeset_in(staged, draft)?;
    if written.changeset.root != clean.root {
        return Err(RepoError::Internal(
            "a group's changeset whose root is not its plan's".to_owned(),
        ));
    }
    Ok((written.id, written.changeset))
}

/// The changeset a plan that is not clean names as landing it: a replay's.
fn replayed(planned: &Planned) -> Option<ChangesetId> {
    match planned.outcome {
        PlanOutcome::Replayed(id) => Some(id),
        _ => None,
    }
}

/// The items of a group: each plan, with the changeset that landed it.
fn group_items(planned: Vec<Planned>, landed: Vec<Option<ChangesetId>>) -> Vec<GroupItem> {
    (planned.into_iter().zip(landed))
        .map(|(planned, landed)| GroupItem {
            head: planned.head,
            outcome: planned.outcome,
            landed,
        })
        .collect()
}
