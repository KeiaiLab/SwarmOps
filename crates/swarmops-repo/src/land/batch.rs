//! Batched lands and commits (the M18 D2 design §3.3, slices D2-4 and D2-5): a publish or a
//! commit's insert that finds the store's lock taken waits as a request in the store's write
//! queue ([`MetaStore::take_turn`]); the next lock holder leads a batch of them ([`Repo::lead`],
//! [`Repo::lead_insert`]), each planned in a frame of its own over one write transaction and
//! committed with one sync of `journal.log`.
//!
//! ```text
//!   own request       ──▶ frame 0, planned as a lone publish or insert plans it
//!   request 1 (by id) ──▶ frame 1, on what frame 0 queued (its moves, its key)
//!   request 2 (by id) ──▶ frame 2, on what frames 0 and 1 queued
//!   claim ──▶ the staged objects, one put and one flush ──▶ one append of every frame
//!   answers: Landed or an insert that queued (a frame); Moved, Replayed, or an insert that
//!   queued nothing (none); a member the leader cannot plan is declined, and its writer makes
//!   it again alone
//! ```
//!
//! An insert's request is its changeset's index record and the ref it advances: the changeset's
//! objects are durable since its committer flushed them before it asked (I1).
//!
//! A request carries ids and small fields only: the leader reads the member's `L` (and `C`) by
//! id, durable since its lander flushed them before it asked (I1). Each member's changesets are
//! read and made through one [`Staged`] overlay, so that a member made anew on the member before
//! it reads that member's `L'`, which no store holds yet; the overlay's objects are put and
//! flushed once, after the claim and before the append (W5).
//!
//! The request and answer bodies are postcard over private mirror types, after a version byte:
//! a body of another version, or one that does not decode, declines its member (a request) or
//! fails closed (an answer).
//!
//! [`MetaStore::take_turn`]: swarmops_store::MetaStore::take_turn

use serde::{Deserialize, Serialize};
use swarmops_core::{Changeset, ChangesetId, ObjectId, ObjectKind, RepoPath};
use swarmops_store::{
    Answer, ChangesetMeta, FrameUse, InsertOutcome, Lead, MetaWrite, ObjectStore, Outcome, Planned,
    Recovered, StoreError,
};

use crate::commit::{self, Advance, Advanced, InsertPlan, Recorded};
use crate::error::RepoError;
use crate::oplog::OpId;
use crate::repo::{self, Repo};
use crate::staged::Staged;

use super::publish::{self, Publication, PublishPlan, Published, Queued, Relocate};

/// The version of the bodies this build encodes and decodes.
const VERSION: u8 = 1;

/// A changeset id on the wire.
type Id = [u8; 32];

/// A request body.
#[derive(Serialize, Deserialize)]
enum Request {
    /// A land's publish.
    Publish(PublishRequest),
    /// A commit's insert.
    Insert(InsertRequest),
}

/// A publish's request: every field of [`Publication`] but its two changesets, named by id.
#[derive(Serialize, Deserialize)]
struct PublishRequest {
    target: String,
    onto: Id,
    id: Id,
    actor: String,
    /// [`Relocate`]: 0 allow, 1 never.
    relocate: u8,
    first: Option<Id>,
    key: Option<String>,
    next_seq: u64,
    head: Id,
}

/// An insert's request: the changeset's index record but the numbers the store gives it, and
/// the ref it advances.
#[derive(Serialize, Deserialize)]
struct InsertRequest {
    id: Id,
    delta_hash: [u8; 32],
    parents: Vec<Id>,
    /// [`Advance`]: the ref and who moves it.
    advance: Option<(String, String)>,
}

/// An insert's answer body: what [`commit::plan_insert`] did.
#[derive(Serialize, Deserialize)]
struct InsertAnswer {
    /// The seq of [`InsertOutcome::Inserted`]; `None` for [`InsertOutcome::AlreadyPresent`].
    inserted: Option<u64>,
    advanced: Option<AdvancedWire>,
}

/// [`Advanced`] on the wire.
#[derive(Serialize, Deserialize)]
struct AdvancedWire {
    name: String,
    old: Option<Id>,
    already_applied: bool,
}

/// An answer body: what a member's publish did, but the warnings only its commit gives and the
/// leader's recovery, which the leader alone reports.
#[derive(Serialize, Deserialize)]
enum PublishAnswer {
    /// [`Published::Landed`].
    Landed {
        id: Id,
        onto: Id,
        relocated: bool,
        merged_in_lock: Vec<String>,
        files_changed: u64,
    },
    /// [`Published::Moved`].
    Moved { actual: Id },
    /// [`Published::Replayed`].
    Replayed {
        id: Id,
        onto: Id,
        squashed_from: Vec<Id>,
        files_changed: u64,
    },
}

/// How a member's planning ended.
enum Member {
    /// A publish: its inserts and move queued in a frame of its own, closed.
    Queued(Queued),
    /// A publish with nothing to record: [`Published::Moved`] or [`Published::Replayed`].
    Done(Published),
    /// An insert: its frame closed if it queued anything ([`InsertPlan::queued`]), else
    /// dropped.
    Inserted(InsertPlan),
    /// It failed: its frame dropped.
    Failed(RepoError),
}

impl Member {
    /// Whether it holds a frame.
    fn framed(&self) -> bool {
        match self {
            Member::Queued(_) => true,
            Member::Inserted(plan) => plan.queued(),
            Member::Done(_) | Member::Failed(_) => false,
        }
    }
}

/// What a lead did for its own request: how its planning ended, and the batch's commit, if it
/// had anything to commit.
struct Led {
    mine: Member,
    committed: Option<Result<Answer, StoreError>>,
}

impl<O: ObjectStore> Repo<O> {
    /// Leads a batch with `lead`, whose own request is `own` (the [module documentation](self)):
    /// plans `own` first, then each waiting request [`Lead::may_plan`] grants, claims them,
    /// writes and flushes the objects they made, and commits. Returns what `own`'s publish did,
    /// as [`Repo::publish`] does; the others are answered through the queue.
    ///
    /// `own` is planned from the caller's publication, its changesets not read again: a lone
    /// publish makes the same filesystem calls in the same order as before the queue. An error of
    /// `own`'s planning is returned once the batch committed; a batch with nothing to commit is
    /// dropped, as a lone publish that records nothing drops its write.
    pub(crate) fn lead(&self, lead: Lead<'_>, own: &Publication) -> Result<Published, RepoError> {
        let Led { mine, committed } =
            self.lead_with(lead, |write, staged| plan_member(write, staged, own))?;
        let Some(committed) = committed else {
            return own_end(mine);
        };
        let Member::Queued(queued) = mine else {
            return own_end(mine);
        };
        match committed {
            Ok(_) => Ok(queued.landed(Vec::new())),
            Err(StoreError::PostCommit {
                frame_seq,
                frame_crc,
                source,
            }) => {
                let warning = queued.warning(own, OpId::new(frame_seq, frame_crc), &source);
                Ok(queued.landed(vec![warning]))
            }
            Err(e) => Err(e.into()),
        }
    }

    /// [`Repo::lead`] for a commit's insert of `meta`, with `advance`: what
    /// [`commit::plan_insert`] did, committed, with the warning of a step after the commit point
    /// that failed and what the lead's recovery repaired. A lone insert makes the same
    /// filesystem calls in the same order as before the queue.
    pub(crate) fn lead_insert(
        &self,
        lead: Lead<'_>,
        meta: ChangesetMeta,
        advance: Option<Advance>,
    ) -> Result<Recorded, RepoError> {
        let recovered = lead.recovered();
        let Led { mine, committed } =
            self.lead_with(lead, |write, _| plan_insert_member(write, meta, advance))?;
        let plan = match mine {
            Member::Inserted(plan) => plan,
            Member::Failed(e) => return Err(e),
            Member::Queued(_) | Member::Done(_) => {
                return Err(RepoError::Internal(
                    "an insert's lead planned a publish".to_owned(),
                ));
            }
        };

        // Nothing queued: nothing of the batch's commit is the insert's.
        let mut warnings = Vec::new();
        match committed {
            Some(Err(StoreError::PostCommit {
                frame_seq,
                frame_crc,
                source,
            })) if plan.queued() => warnings.push(repo::post_commit_warning(
                &plan.what(),
                OpId::new(frame_seq, frame_crc),
                &source,
            )),
            Some(Err(e)) if plan.queued() => return Err(e.into()),
            _ => {}
        }
        Ok((plan.insert, plan.advanced, warnings, recovered))
    }

    /// Leads a batch with `lead`, its own request planned by `own` as frame 0: then each waiting
    /// request [`Lead::may_plan`] grants, the claim, the objects the members made written and
    /// flushed, and the commit. A batch of the own request alone that records nothing is
    /// dropped, its commit none.
    fn lead_with<F>(&self, mut lead: Lead<'_>, own: F) -> Result<Led, RepoError>
    where
        F: FnOnce(&mut MetaWrite<'_>, &Staged<'_, O>) -> Member,
    {
        let staged = Staged::new(&self.objects);

        // 1. The own request, frame 0.
        let own_rid = lead.requests()[0].rid.clone();
        let mine = own(lead.write(), &staged);
        let own_framed = mine.framed();
        let own_frame = if own_framed {
            FrameUse::Frame(0)
        } else {
            FrameUse::NoFrame
        };
        let mut plans = vec![Planned::new(own_rid, own_frame, Vec::new())];

        // 2. The waiting requests, oldest first, each on what the ones before it queued.
        let mut frames = usize::from(own_framed);
        while lead.may_plan() {
            let request = lead.requests()[plans.len()].clone();
            let member = match decode_request(&request.body) {
                Ok(Request::Publish(wire)) => match publication(&staged, wire) {
                    Ok(p) => plan_member(lead.write(), &staged, &p),
                    Err(e) => Member::Failed(e),
                },
                Ok(Request::Insert(wire)) => {
                    let (meta, advance) = insert_of(wire);
                    plan_insert_member(lead.write(), meta, advance)
                }
                Err(e) => Member::Failed(e),
            };
            let has_frame = member.framed();
            let body = match member {
                Member::Queued(queued) => {
                    Some(encode_answer(&wire_of(&queued.landed(Vec::new())))?)
                }
                Member::Done(published) => Some(encode_answer(&wire_of(&published))?),
                Member::Inserted(plan) => Some(encode(&insert_answer(&plan)?)?),
                Member::Failed(_) => None,
            };
            let (frame, body) = match body {
                Some(body) if has_frame => {
                    frames += 1;
                    (FrameUse::Frame(frames - 1), body)
                }
                Some(body) => (FrameUse::NoFrame, body),
                None => (FrameUse::Declined, Vec::new()),
            };
            plans.push(Planned::new(request.rid, frame, body));
        }

        // 3. The claim; a batch of the own request alone that records nothing is dropped.
        let held = lead.claim(plans)?;
        if held == 1 && !own_framed {
            drop(lead);
            return Ok(Led {
                mine,
                committed: None,
            });
        }

        // 4. The objects every frame names, durable before the append (I1, W5).
        let objects = staged.into_objects();
        if !objects.is_empty() {
            self.objects.put_hashed_batch(objects)?;
            self.objects.flush()?;
        }

        // 5. The commit point of every member.
        Ok(Led {
            mine,
            committed: Some(lead.commit()),
        })
    }

    /// What `answer`, a leader's answer to the publish of `p`, says the publish did. The frame's
    /// CRC a post-commit warning names is read from a snapshot, lock-free.
    pub(crate) fn answered(
        &self,
        answer: &Answer,
        p: &Publication,
    ) -> Result<Published, RepoError> {
        let wire = decode_answer(&answer.body)?;
        match (&answer.outcome, wire) {
            (Outcome::Committed { .. }, landed @ PublishAnswer::Landed { .. }) => {
                published(landed, Vec::new())
            }
            (Outcome::PostCommit { frame_seq, warning }, landed @ PublishAnswer::Landed { .. }) => {
                let what = publish::landed_what(answer_id(&landed), &p.target);
                let warning = self.member_warning(&what, *frame_seq, warning);
                published(landed, vec![warning])
            }
            (
                Outcome::NoFrame,
                done @ (PublishAnswer::Moved { .. } | PublishAnswer::Replayed { .. }),
            ) => published(done, Vec::new()),
            _ => Err(RepoError::Internal(
                "a publish's answer whose outcome does not match its body".to_owned(),
            )),
        }
    }

    /// What `answer`, a leader's answer to a commit's insert, says the insert did; the recovery
    /// is the leader's to report, none here.
    pub(crate) fn answered_insert(&self, answer: &Answer) -> Result<Recorded, RepoError> {
        let plan = plan_of(decode(&answer.body, "answer")?);
        let warnings = match (&answer.outcome, plan.queued()) {
            (Outcome::Committed { .. }, true) | (Outcome::NoFrame, false) => Vec::new(),
            (Outcome::PostCommit { frame_seq, warning }, true) => {
                vec![self.member_warning(&plan.what(), *frame_seq, warning)]
            }
            _ => {
                return Err(RepoError::Internal(
                    "an insert's answer whose outcome does not match its body".to_owned(),
                ));
            }
        };
        Ok((plan.insert, plan.advanced, warnings, Recovered::default()))
    }

    /// The request body of the insert of `meta` with `advance`: its version, then the index
    /// record's fields but the numbers the store gives it, and the ref.
    pub(crate) fn insert_body(
        meta: &ChangesetMeta,
        advance: Option<&Advance>,
    ) -> Result<Vec<u8>, RepoError> {
        let wire = Request::Insert(InsertRequest {
            id: bytes(meta.id),
            delta_hash: meta.delta_hash,
            parents: meta.parents.iter().map(|id| bytes(*id)).collect(),
            advance: advance.map(|a| (a.name.clone(), a.actor.clone())),
        });
        encode(&wire)
    }

    /// The warning of a member's land (`what`) committed in frame `seq`, whose step after the
    /// commit point failed as `source` says: [`repo::post_commit_warning`]'s words, the
    /// operation's CRC read from a snapshot (none if the frame is not listed yet).
    fn member_warning(&self, what: &str, seq: u64, source: &str) -> String {
        let crc = (self.meta.read().ok())
            .and_then(|read| read.frame(seq).ok().flatten())
            .map(|frame| frame.crc);
        let op = crc.map_or_else(|| seq.to_string(), |crc| OpId::new(seq, crc).to_string());
        format!(
            "{what} is committed (operation {op}), but a step after the commit point failed: \
             {source}; the next write to the store repairs it"
        )
    }
}

/// Plans `p` in a frame of its own of `write`, its objects read from and written to `objects`:
/// queued, its frame closed; or nothing to record, or failed, its frame dropped.
fn plan_member<S: ObjectStore>(write: &mut MetaWrite<'_>, objects: &S, p: &Publication) -> Member {
    match publish::plan_publish(write, objects, p) {
        // Closed now, so that the next member's key check sees it (D12).
        Ok(PublishPlan::Queued(queued)) => match write.begin_frame() {
            Ok(()) => Member::Queued(queued),
            Err(e) => {
                write.drop_frame();
                Member::Failed(e.into())
            }
        },
        Ok(PublishPlan::Done(published)) => {
            write.drop_frame();
            Member::Done(published)
        }
        Err(e) => {
            write.drop_frame();
            Member::Failed(e)
        }
    }
}

/// Plans the insert of `meta`, with `advance`, in a frame of its own of `write`: closed if it
/// queued anything, else dropped, as on a failure.
fn plan_insert_member(
    write: &mut MetaWrite<'_>,
    meta: ChangesetMeta,
    advance: Option<Advance>,
) -> Member {
    match commit::plan_insert(write, meta, advance) {
        // Closed now, so that the next member sees it.
        Ok(plan) if plan.queued() => match write.begin_frame() {
            Ok(()) => Member::Inserted(plan),
            Err(e) => {
                write.drop_frame();
                Member::Failed(e.into())
            }
        },
        Ok(plan) => {
            write.drop_frame();
            Member::Inserted(plan)
        }
        Err(e) => {
            write.drop_frame();
            Member::Failed(e)
        }
    }
}

/// The insert `wire` asks for: the index record, numbered by the store, and the advance.
fn insert_of(wire: InsertRequest) -> (ChangesetMeta, Option<Advance>) {
    let parents = wire.parents.into_iter().map(id_of).collect();
    let meta = ChangesetMeta::new(id_of(wire.id), 0, 0, wire.delta_hash, parents);
    let advance = wire
        .advance
        .map(|(name, actor)| Advance::new(&name, &actor));
    (meta, advance)
}

/// The answer body of `plan`.
fn insert_answer(plan: &InsertPlan) -> Result<InsertAnswer, RepoError> {
    let inserted = match plan.insert {
        InsertOutcome::Inserted { seq } => Some(seq),
        InsertOutcome::AlreadyPresent => None,
        // Fail closed: an outcome this build cannot answer ends the lead, nothing claimed.
        _ => return Err(RepoError::unknown_variant("InsertOutcome")),
    };
    let advanced = plan.advanced.as_ref().map(|a| AdvancedWire {
        name: a.name.clone(),
        old: a.old.map(bytes),
        already_applied: a.already_applied,
    });
    Ok(InsertAnswer { inserted, advanced })
}

/// The plan `wire` answers.
fn plan_of(wire: InsertAnswer) -> InsertPlan {
    let insert = match wire.inserted {
        Some(seq) => InsertOutcome::Inserted { seq },
        None => InsertOutcome::AlreadyPresent,
    };
    let advanced =
        (wire.advanced).map(|a| Advanced::new(&a.name, a.old.map(id_of), a.already_applied));
    InsertPlan { insert, advanced }
}

/// The own publish's end, when it committed no frame of its own.
fn own_end(mine: Member) -> Result<Published, RepoError> {
    match mine {
        Member::Done(published) => Ok(published),
        Member::Failed(e) => Err(e),
        Member::Queued(_) => Err(RepoError::Internal(
            "a queued publish ended without its commit".to_owned(),
        )),
        Member::Inserted(_) => Err(RepoError::Internal(
            "a publish's lead planned an insert".to_owned(),
        )),
    }
}

/// `p`'s request body: its version, then its fields but the changesets'.
pub(crate) fn encode_publish(p: &Publication) -> Result<Vec<u8>, RepoError> {
    let relocate = match p.relocate {
        Relocate::Allow => 0,
        Relocate::Never => 1,
    };
    let wire = Request::Publish(PublishRequest {
        target: p.target.clone(),
        onto: bytes(p.onto),
        id: bytes(p.id),
        actor: p.actor.clone(),
        relocate,
        first: p.first.as_ref().map(|(id, _)| bytes(*id)),
        key: p.key.clone(),
        next_seq: p.next_seq,
        head: bytes(p.head),
    });
    encode(&wire)
}

/// The request `body` encodes, of this version.
fn decode_request(body: &[u8]) -> Result<Request, RepoError> {
    decode(body, "request")
}

/// The publication `wire` names, its changesets read by id from `objects`.
fn publication<S: ObjectStore>(
    objects: &S,
    wire: PublishRequest,
) -> Result<Publication, RepoError> {
    let relocate = match wire.relocate {
        0 => Relocate::Allow,
        1 => Relocate::Never,
        _ => return Err(RepoError::unknown_variant("Relocate")),
    };
    let first = match wire.first {
        Some(id) => Some((id_of(id), changeset_in(objects, id_of(id))?)),
        None => None,
    };
    Ok(Publication {
        target: wire.target,
        onto: id_of(wire.onto),
        id: id_of(wire.id),
        changeset: changeset_in(objects, id_of(wire.id))?,
        actor: wire.actor,
        relocate,
        first,
        key: wire.key,
        next_seq: wire.next_seq,
        head: id_of(wire.head),
    })
}

/// Changeset `id`, decoded from `objects`.
fn changeset_in<S: ObjectStore>(objects: &S, id: ChangesetId) -> Result<Changeset, RepoError> {
    let payload = repo::object(objects, id.0, ObjectKind::Changeset)?;
    Ok(Changeset::decode(&payload)?)
}

/// The answer body of `published`, without its warnings and its recovery.
fn wire_of(published: &Published) -> PublishAnswer {
    match published {
        Published::Landed {
            id,
            onto,
            relocated,
            merged_in_lock,
            files_changed,
            ..
        } => PublishAnswer::Landed {
            id: bytes(*id),
            onto: bytes(*onto),
            relocated: *relocated,
            merged_in_lock: merged_in_lock
                .iter()
                .map(|p| p.as_str().to_owned())
                .collect(),
            files_changed: u64::try_from(*files_changed).unwrap_or(u64::MAX),
        },
        Published::Moved { actual, .. } => PublishAnswer::Moved {
            actual: bytes(*actual),
        },
        Published::Replayed {
            id,
            onto,
            squashed_from,
            files_changed,
        } => PublishAnswer::Replayed {
            id: bytes(*id),
            onto: bytes(*onto),
            squashed_from: squashed_from.iter().map(|id| bytes(*id)).collect(),
            files_changed: u64::try_from(*files_changed).unwrap_or(u64::MAX),
        },
    }
}

/// The publish `wire` answers, with `warnings`; a waiter's recovery is none.
fn published(wire: PublishAnswer, warnings: Vec<String>) -> Result<Published, RepoError> {
    let count = |n: u64| {
        usize::try_from(n).map_err(|_| RepoError::Internal("a count past usize".to_owned()))
    };
    Ok(match wire {
        PublishAnswer::Landed {
            id,
            onto,
            relocated,
            merged_in_lock,
            files_changed,
        } => Published::Landed {
            id: id_of(id),
            onto: id_of(onto),
            relocated,
            merged_in_lock: (merged_in_lock.iter())
                .map(RepoPath::new)
                .collect::<Result<_, _>>()
                .map_err(|e| RepoError::Internal(format!("an answer's path: {e}")))?,
            files_changed: count(files_changed)?,
            warnings,
        },
        PublishAnswer::Moved { actual } => Published::Moved {
            actual: id_of(actual),
            recovered: swarmops_store::Recovered::default(),
        },
        PublishAnswer::Replayed {
            id,
            onto,
            squashed_from,
            files_changed,
        } => Published::Replayed {
            id: id_of(id),
            onto: id_of(onto),
            squashed_from: squashed_from.into_iter().map(id_of).collect(),
            files_changed: count(files_changed)?,
        },
    })
}

/// The id of the changeset `wire` names: the landed, the moved-to or the replayed one.
fn answer_id(wire: &PublishAnswer) -> ChangesetId {
    match wire {
        PublishAnswer::Landed { id, .. }
        | PublishAnswer::Moved { actual: id }
        | PublishAnswer::Replayed { id, .. } => id_of(*id),
    }
}

/// `wire`'s answer body: its version, then its fields.
fn encode_answer(wire: &PublishAnswer) -> Result<Vec<u8>, RepoError> {
    encode(wire)
}

/// The answer `body` encodes, of this version.
fn decode_answer(body: &[u8]) -> Result<PublishAnswer, RepoError> {
    decode(body, "answer")
}

/// [`VERSION`], then `value`'s postcard bytes.
fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, RepoError> {
    let mut body = vec![VERSION];
    let encoded = postcard::to_stdvec(value)
        .map_err(|e| RepoError::Internal(format!("a queue body does not encode: {e}")))?;
    body.extend(encoded);
    Ok(body)
}

/// What `body`, every byte of it, encodes after [`VERSION`]; a body of another version, or one
/// that does not decode whole, fails closed. `what` names it in the error.
fn decode<T: for<'de> Deserialize<'de>>(body: &[u8], what: &str) -> Result<T, RepoError> {
    let bad = |why: &str| Err(RepoError::Internal(format!("a queue {what}: {why}")));
    let Some((&version, rest)) = body.split_first() else {
        return bad("empty");
    };
    if version != VERSION {
        return bad("of a version this build does not read");
    }
    match postcard::take_from_bytes(rest) {
        Ok((value, [])) => Ok(value),
        Ok(_) => bad("bytes left after it"),
        Err(_) => bad("does not decode"),
    }
}

/// `id`'s bytes.
fn bytes(id: ChangesetId) -> Id {
    id.0.0
}

/// The id of `bytes`.
fn id_of(bytes: Id) -> ChangesetId {
    ChangesetId::new(ObjectId::new(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A publication of no changeset worth reading: its fields only.
    fn publication_of(key: Option<&str>) -> Publication {
        let id = |n: u8| id_of([n; 32]);
        let changeset = Changeset::new(
            vec![id(1)],
            swarmops_core::TreeId::new(ObjectId::new([2; 32])),
            Vec::new(),
            swarmops_testkit::repo::signature(),
            swarmops_testkit::repo::signature(),
            b"m".to_vec(),
            Vec::new(),
            Vec::new(),
        )
        .unwrap();
        Publication {
            target: "main".to_owned(),
            onto: id(1),
            id: id(3),
            changeset,
            actor: "test/actor".to_owned(),
            relocate: Relocate::Never,
            first: None,
            key: key.map(str::to_owned),
            next_seq: 7,
            head: id(4),
        }
    }

    #[test]
    fn a_publish_request_decodes_to_its_fields() {
        let p = publication_of(Some("key-1"));
        let Request::Publish(wire) = decode_request(&encode_publish(&p).unwrap()).unwrap() else {
            panic!("not a publish's request");
        };
        assert_eq!(
            (wire.target.as_str(), wire.onto, wire.id, wire.relocate),
            ("main", [1; 32], [3; 32], 1)
        );
        assert_eq!(
            (wire.key.as_deref(), wire.next_seq, wire.head),
            (Some("key-1"), 7, [4; 32])
        );
        assert_eq!(wire.first, None);
    }

    #[test]
    fn a_body_of_another_version_fails_closed() {
        let mut body = encode_publish(&publication_of(None)).unwrap();
        body[0] = VERSION + 1;
        assert!(matches!(decode_request(&body), Err(RepoError::Internal(_))));
        body[0] = VERSION;
        body.push(0);
        assert!(matches!(decode_request(&body), Err(RepoError::Internal(_))));
        assert!(matches!(decode_answer(&[]), Err(RepoError::Internal(_))));
    }

    #[test]
    fn an_answer_round_trips() {
        let landed = Published::Landed {
            id: id_of([5; 32]),
            onto: id_of([6; 32]),
            relocated: true,
            merged_in_lock: vec![RepoPath::new("a/b").unwrap()],
            files_changed: 2,
            warnings: Vec::new(),
        };
        let body = encode_answer(&wire_of(&landed)).unwrap();
        assert_eq!(
            published(decode_answer(&body).unwrap(), Vec::new()).unwrap(),
            landed
        );
    }
}
