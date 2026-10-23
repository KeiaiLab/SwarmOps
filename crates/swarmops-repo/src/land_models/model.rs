//! The landing [`Model`]s and their publication against a target ref.

use std::collections::BTreeSet;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use swarmops_core::{
    Changeset, ChangesetId, DeltaEntry, DeltaOp, Hashed, LeafKind, ObjectId, ObjectKind, RepoPath,
    Signature,
};
use swarmops_store::{CasResult, ChangesetMeta, InsertOutcome, MetaWrite, ObjectStore, StoreError};

use crate::commit::{Change, ChangeOp, candidate_delta, sorted_changes};
use crate::error::RepoError;
use crate::repo::{Repo, ref_error};
use crate::tree_edit::{Encoded, TreeEdit};

use super::relocate::{Relocation, relocate};

/// The most first-parent steps a publish walks from the target's actual head back to a land's
/// `onto`: past them, the land goes back to its lander as [`Published::Moved`].
pub const MAX_WALK: usize = 256;

/// The most bytes Metalog merges in the lock, on each side of a file.
pub const MAX_MERGE_BYTES: usize = 1 << 20;

/// The metadata key under which an op-heads land names its op-head ref.
pub const OP_HEAD_KEY: &str = "oph";

/// What an op-head ref's name starts with: a land recorded and not settled yet.
pub const OP_HEAD_PREFIX: &str = "release/oph-";

/// What a conflicted op-head ref's name starts with: a land that did not settle, its changeset
/// kept for its lander to re-merge.
pub const CONFLICTED_PREFIX: &str = "release/ophc-";

/// The one target op heads land on.
const OP_HEADS_TARGET: &str = "main";

/// Who the moves a settle makes are recorded as.
const SETTLER: &str = "op-heads/settle";

/// The email of a prepared changeset's signature.
const EMAIL: &[u8] = b"lander@swarmops.local";

/// A landing model: how a publish handles a target that moved after the land read it (see
/// [the module documentation](super)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Model {
    /// The yardstick: insert and compare-and-swap; a moved target goes back to the lander.
    CasLoop,
    /// The lock holder relocates a change whose paths are disjoint from what the target gained.
    Pushrebase,
    /// Pushrebase, and overlapping files merged in the lock.
    Metalog,
    /// Lands recorded under refs of their own, settled onto the target by the next transaction.
    OpHeads,
}

impl Model {
    /// The four, in the order of the work they do under the lock (the CAS loop's least).
    pub const ALL: [Model; 4] = [
        Model::CasLoop,
        Model::Pushrebase,
        Model::Metalog,
        Model::OpHeads,
    ];

    /// The model's name, as the measurements print it: `cas_loop`, `pushrebase`, `metalog` or
    /// `op_heads`.
    pub fn name(self) -> &'static str {
        match self {
            Model::CasLoop => "cas_loop",
            Model::Pushrebase => "pushrebase",
            Model::Metalog => "metalog",
            Model::OpHeads => "op_heads",
        }
    }

    /// The model named `name` ([`Model::name`]).
    pub fn from_name(name: &str) -> Option<Model> {
        Model::ALL.into_iter().find(|model| model.name() == name)
    }
}

/// A land's changeset, prepared on the head it read ([`Repo::prepare`]) and ready to publish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Publication {
    /// The ref it lands on.
    pub target: String,
    /// The head it was prepared on: its one parent.
    pub onto: ChangesetId,
    /// Its id.
    pub id: ChangesetId,
    /// The changeset, whose objects are durable.
    pub changeset: Changeset,
    /// Who moves the target, as the reflog names them.
    pub actor: String,
}

/// What a publish did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Published {
    /// The changeset `id` is on the target, its head now; `onto` its parent. `relocated` when
    /// the lock holder made it from the land's changeset onto a head that had moved.
    Landed {
        /// The changeset the target names now.
        id: ChangesetId,
        /// The head it landed on.
        onto: ChangesetId,
        /// Whether the lock holder relocated it.
        relocated: bool,
    },
    /// The target moved to `actual` in a way the model does not relocate across: nothing was
    /// recorded.
    Moved {
        /// The target's head.
        actual: ChangesetId,
    },
    /// The target moved to `actual` and gained a path that overlaps the land's, which the model
    /// did not merge: nothing was recorded.
    Overlap {
        /// The target's head.
        actual: ChangesetId,
    },
    /// Op heads: the land is recorded under ref `op_head`, for a later transaction to settle.
    Pending {
        /// The op-head ref.
        op_head: String,
    },
}

/// A publish's outcome, with how long it held the metadata lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timed<T> {
    /// The outcome.
    pub value: T,
    /// From `MetaStore::write` returning to the end of the write: its commit, or its drop.
    pub lock_hold: Duration,
}

impl<O: ObjectStore> Repo<O> {
    /// Prepares a land's changeset `L` on `onto`, outside the lock: `puts` and `deletes`
    /// applied to `onto`'s tree as a commit applies them, `metadata` recorded (sorted by key),
    /// a signature naming `actor` at the current time, and its blobs, trees and changeset
    /// object written and flushed. Nothing names `L` until a publish records it.
    pub fn prepare(
        &self,
        target: &str,
        onto: ChangesetId,
        puts: Vec<(RepoPath, LeafKind, Vec<u8>)>,
        deletes: Vec<RepoPath>,
        actor: &str,
        mut metadata: Vec<(String, Vec<u8>)>,
    ) -> Result<Publication, RepoError> {
        swarmops_store::check_ref_name(target).map_err(ref_error)?;
        let changes = puts
            .into_iter()
            .map(|(path, kind, content)| Change {
                path,
                op: ChangeOp::Put { kind, content },
            })
            .chain(deletes.into_iter().map(|path| Change {
                path,
                op: ChangeOp::Delete,
            }))
            .collect();
        let (candidates, mut contents) = candidate_delta(sorted_changes(changes, false)?);
        let mut edit = TreeEdit::new(&self.objects, self.changeset_object(onto)?.root)?;
        let effective = edit.apply(&candidates)?;
        let delta: Vec<DeltaEntry> = candidates
            .into_iter()
            .zip(effective)
            .filter_map(|(entry, effective)| effective.then_some(entry))
            .collect();
        if delta.is_empty() {
            return Err(RepoError::EmptyChanges);
        }
        let Encoded {
            root,
            changed: trees,
        } = edit.finish()?;
        let put: BTreeSet<ObjectId> = delta
            .iter()
            .filter_map(|entry| match entry.op {
                DeltaOp::Put { id, .. } => Some(id.0),
                DeltaOp::Delete => None,
            })
            .collect();
        contents.retain(|id, _| put.contains(id));
        metadata.sort_by(|a, b| a.0.cmp(&b.0));
        let signature = signature(actor);
        let changeset = Changeset {
            parents: vec![onto],
            root,
            delta,
            author: signature.clone(),
            committer: signature,
            message: format!("land by {actor}").into_bytes(),
            extra_headers: Vec::new(),
            metadata,
        };
        let object = Hashed::new(ObjectKind::Changeset, changeset.encode()?);
        let id = ChangesetId(object.id());
        let mut batch: Vec<Hashed> = contents.into_values().collect();
        batch.extend(trees);
        batch.push(object);
        self.objects.put_hashed_batch(batch)?;
        self.objects.flush()?;
        Ok(Publication {
            target: target.to_owned(),
            onto,
            id,
            changeset,
            actor: actor.to_owned(),
        })
    }

    /// Publishes `p` by `model`, in one write transaction: see [the module
    /// documentation](super).
    pub fn publish_with(
        &self,
        model: Model,
        p: &Publication,
    ) -> Result<Timed<Published>, RepoError> {
        let mut write = self.meta.write()?;
        let held = Instant::now();
        let (value, queued) = match model {
            Model::CasLoop => publish_cas(&mut write, p)?,
            Model::Pushrebase => self.publish_relocating(&mut write, p, false)?,
            Model::Metalog => self.publish_relocating(&mut write, p, true)?,
            Model::OpHeads => self.publish_op_head(&mut write, p)?,
        };
        // A publish that queued nothing drops the write: no frame, the lock released.
        if queued {
            write.commit()?;
        } else {
            drop(write);
        }
        Ok(Timed {
            value,
            lock_hold: held.elapsed(),
        })
    }

    /// Settles every recorded op-heads land onto `main` in one write transaction, recording
    /// none: how many it landed there (the conflicted ones keep their changesets under their
    /// `release/ophc-` refs).
    pub fn settle_op_heads(&self) -> Result<Timed<usize>, RepoError> {
        let mut write = self.meta.write()?;
        let held = Instant::now();
        let (landed, seen) = self.settle(&mut write)?;
        if seen > 0 {
            write.commit()?;
        } else {
            drop(write);
        }
        Ok(Timed {
            value: landed,
            lock_hold: held.elapsed(),
        })
    }

    /// Pushrebase's publish, and with `merge` Metalog's: see [the module documentation](super).
    fn publish_relocating(
        &self,
        write: &mut MetaWrite<'_>,
        p: &Publication,
        merge: bool,
    ) -> Result<(Published, bool), RepoError> {
        let actual = write
            .get_ref(&p.target)
            .map_err(ref_error)?
            .ok_or_else(|| missing_target(&p.target))?;
        if actual == p.onto {
            return publish_cas(write, p);
        }
        match relocate(self, write, &p.changeset, p.onto, actual, merge)? {
            Relocation::Relocated { id, changeset } => {
                insert(write, &changeset, id)?;
                move_ref(write, &p.target, Some(actual), Some(id), &p.actor)?;
                let landed = Published::Landed {
                    id,
                    onto: actual,
                    relocated: true,
                };
                Ok((landed, true))
            }
            Relocation::Moved => Ok((Published::Moved { actual }, false)),
            Relocation::Overlap => Ok((Published::Overlap { actual }, false)),
        }
    }

    /// Op heads' publish: settles what is recorded, then records `p` under its op head.
    fn publish_op_head(
        &self,
        write: &mut MetaWrite<'_>,
        p: &Publication,
    ) -> Result<(Published, bool), RepoError> {
        if p.target != OP_HEADS_TARGET {
            return Err(RepoError::RefName(p.target.clone()));
        }
        let op_head = op_head_of(&p.changeset)?;
        self.settle(write)?;
        insert(write, &p.changeset, p.id)?;
        move_ref(write, &op_head, None, Some(p.id), &p.actor)?;
        let conflicted = conflicted_name(&op_head);
        if let Some(old) = write.get_ref(&conflicted).map_err(ref_error)? {
            move_ref(write, &conflicted, Some(old), None, &p.actor)?;
        }
        Ok((Published::Pending { op_head }, true))
    }

    /// Settles every op head the store records, oldest record first, queuing the moves in
    /// `write`: each lands on `main` (its changeset, or the one Metalog's rule relocates onto
    /// `main`'s head), its op-head ref deleted; or, when it does not settle, its ref gives way
    /// to a conflicted one naming the same changeset. How many landed, and how many it saw.
    fn settle(&self, write: &mut MetaWrite<'_>) -> Result<(usize, usize), RepoError> {
        // A snapshot taken under the lock, after the write's replay: the refs as they are.
        let refs = self.meta.read()?.list_refs()?;
        let mut heads = Vec::new();
        for (name, state) in refs {
            if name.starts_with(OP_HEAD_PREFIX) {
                let meta = write
                    .changeset(state.target)?
                    .ok_or(StoreError::MissingParent(state.target))?;
                heads.push((meta.seq, name, state.target));
            }
        }
        heads.sort();
        let mut landed = 0;
        for (_, name, id) in &heads {
            let changeset = self.changeset_object(*id)?;
            let onto = *changeset
                .parents
                .first()
                .ok_or_else(|| RepoError::NotFound(format!("the parent of op head {name}")))?;
            let head = write
                .get_ref(OP_HEADS_TARGET)
                .map_err(ref_error)?
                .ok_or_else(|| missing_target(OP_HEADS_TARGET))?;
            let settled = if head == onto {
                Some(*id)
            } else {
                match relocate(self, write, &changeset, onto, head, true)? {
                    Relocation::Relocated {
                        id: relocated,
                        changeset,
                    } => {
                        insert(write, &changeset, relocated)?;
                        Some(relocated)
                    }
                    Relocation::Moved | Relocation::Overlap => None,
                }
            };
            if let Some(settled) = settled {
                move_ref(write, OP_HEADS_TARGET, Some(head), Some(settled), SETTLER)?;
                move_ref(write, name, Some(*id), None, SETTLER)?;
                landed += 1;
            } else {
                move_ref(write, name, Some(*id), None, SETTLER)?;
                let conflicted = conflicted_name(name);
                let old = write.get_ref(&conflicted).map_err(ref_error)?;
                move_ref(write, &conflicted, old, Some(*id), SETTLER)?;
            }
        }
        Ok((landed, heads.len()))
    }
}

/// The CAS loop's publish: `p`'s changeset and the target's move from `p.onto` to it, or
/// [`Published::Moved`] with nothing that counts queued. Whether it queued anything comes back
/// too.
fn publish_cas(write: &mut MetaWrite<'_>, p: &Publication) -> Result<(Published, bool), RepoError> {
    insert(write, &p.changeset, p.id)?;
    match write
        .cas_ref(&p.target, Some(p.onto), Some(p.id), &p.actor)
        .map_err(ref_error)?
    {
        CasResult::Applied { .. } => {
            let landed = Published::Landed {
                id: p.id,
                onto: p.onto,
                relocated: false,
            };
            Ok((landed, true))
        }
        // The insert stays queued, and is dropped with the write.
        CasResult::Mismatch {
            actual: Some(actual),
        } => Ok((Published::Moved { actual }, false)),
        CasResult::Mismatch { actual: None } => Err(missing_target(&p.target)),
    }
}

/// Queues `changeset`, whose id is `id`, in `write`.
fn insert(
    write: &mut MetaWrite<'_>,
    changeset: &Changeset,
    id: ChangesetId,
) -> Result<InsertOutcome, RepoError> {
    let meta = ChangesetMeta {
        id,
        seq: 0,
        generation: 0,
        delta_hash: changeset.delta_hash()?,
        parents: changeset.parents.clone(),
    };
    write.insert_changeset(meta).map_err(|e| match e {
        StoreError::MissingParent(parent) => RepoError::UnknownParent(parent),
        other => other.into(),
    })
}

/// Queues the move of ref `name` from `expected` to `new` in `write`, which must find it at
/// `expected`: this transaction read it there under the lock.
fn move_ref(
    write: &mut MetaWrite<'_>,
    name: &str,
    expected: Option<ChangesetId>,
    new: Option<ChangesetId>,
    actor: &str,
) -> Result<(), RepoError> {
    match write
        .cas_ref(name, expected, new, actor)
        .map_err(ref_error)?
    {
        CasResult::Applied { .. } => Ok(()),
        CasResult::Mismatch { actual } => Err(RepoError::RefMoved {
            name: name.to_owned(),
            actual,
            recovered: Box::new(write.recovered()),
        }),
    }
}

/// The op-head ref an op-heads land's changeset names under [`OP_HEAD_KEY`].
fn op_head_of(changeset: &Changeset) -> Result<String, RepoError> {
    let named = changeset
        .metadata
        .iter()
        .find(|(key, _)| key == OP_HEAD_KEY)
        .and_then(|(_, value)| String::from_utf8(value.clone()).ok());
    match named {
        Some(name) if name.starts_with(OP_HEAD_PREFIX) => Ok(name),
        Some(name) => Err(RepoError::RefName(name)),
        None => Err(RepoError::NotFound(format!(
            "the {OP_HEAD_KEY} metadata of an op-heads land"
        ))),
    }
}

/// The conflicted ref of op head `op_head`: `release/ophc-<rest>` for `release/oph-<rest>`.
fn conflicted_name(op_head: &str) -> String {
    let rest = op_head.strip_prefix(OP_HEAD_PREFIX).unwrap_or(op_head);
    format!("{CONFLICTED_PREFIX}{rest}")
}

/// The error of a target that does not exist.
fn missing_target(target: &str) -> RepoError {
    RepoError::NotFound(format!("ref {target}"))
}

/// A prepared changeset's author and committer: `actor`, now.
fn signature(actor: &str) -> Signature {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
        });
    Signature {
        name: actor.as_bytes().to_vec(),
        email: EMAIL.to_vec(),
        secs,
        offset_secs: 0,
        negative_zero: false,
    }
}
