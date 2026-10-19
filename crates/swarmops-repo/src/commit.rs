//! Commits from change lists (storage spec §2 rule 2, §4): [`Repo::commit`] applies a
//! [`CommitRequest`]'s changes to its first parent's tree and records the result.
//!
//! 1. Checks the request before it reads or writes anything: changes to make, unless
//!    `allow_empty` ([`RepoError::EmptyChanges`]), each path once
//!    ([`RepoError::DuplicatePath`]), at most 255 parents (the metadata
//!    store's [`StoreError::Limit`], as it gives at insert), distinct and in the store
//!    ([`RepoError::UnknownParent`]), the name of the ref to advance, if any, a ref name
//!    ([`RepoError::RefName`]), and no metadata key that ends in `:b64`
//!    ([`RepoError::ReservedMetadataKey`], [`check_metadata`]).
//! 2. Hashes each put's content into its blob id, once, and applies the changes to the first
//!    parent's root tree in memory, the deletes first: a put below a file
//!    or where a directory is, or a delete of a directory, is a [`RepoError::TypeConflict`]; a
//!    delete of a path that is no file, a [`RepoError::PathNotFound`]. A put of what its path
//!    holds already changes nothing and stays out of the delta; a request of nothing else is
//!    [`RepoError::EmptyChanges`], unless `allow_empty`: then the changeset keeps its first
//!    parent's root (the empty tree for a root changeset), with an empty delta.
//! 3. Encodes the changed directories bottom-up, then the changeset: its delta is the changes
//!    that changed the tree, sorted by path. Each is encoded and hashed once.
//! 4. Writes the blobs the delta puts, the changed trees and the changeset object in one
//!    `put_hashed_batch`, each with the id it was hashed to, which the store does not compute
//!    again, then flushes: every object is durable before any metadata names it (I1). A
//!    flush error fails the commit, and nothing is recorded. The store says which objects this
//!    commit's writes placed, and the outcome counts them.
//! 5. Inserts the changeset in a write transaction of its own, beside the move of the ref that
//!    `advance` names if any (below), and commits that: its journal frame is the commit point.
//!    A failure after it ([`StoreError::PostCommit`]) leaves the changeset committed, and is
//!    reported as a warning; the next write to the store repairs
//!    what is missing. A journal append that fails ([`StoreError::NotCommitted`]) is returned
//!    as it is: the frame was cut back off the journal, so the changeset is not recorded, and
//!    the same request made again records it. What the transaction's recovery repaired first,
//!    of what earlier writers left, comes back in [`CommitOutcome::recovered`].
//!
//! An identical request computes the same objects and the same changeset: the store has them
//! all, so nothing is written, and the insert finds the changeset
//! ([`InsertOutcome::AlreadyPresent`]) without appending a transaction.
//!
//! A request with `advance` ([`Advance`]) publishes the changeset: the same transaction moves
//! that ref from the changeset's first parent to it (for a root changeset, creates the ref), so
//! the changeset and the move are one journal frame, one commit point, and recorded together
//! or not at all. The expectation is the first parent, never a value of its own, so the move
//! is always a one-step fast-forward along first parents. A ref that names anything else, or
//! for a root changeset exists, drops the transaction: [`RepoError::RefMoved`], and neither the
//! changeset nor the move is recorded (the objects step 4 flushed stay, unreferenced, for
//! GC). A ref that names the changeset already is an identical request made before: nothing
//! is written, and the outcome says so ([`Advanced::already_applied`]).

use std::collections::{BTreeMap, BTreeSet};

use swarmops_core::bytes::to_u64;
use swarmops_core::{
    BlobId, Changeset, ChangesetId, DeltaEntry, DeltaOp, Hashed, LeafKind, ObjectId, ObjectKind,
    RepoPath, Signature, TreeId,
};
use swarmops_store::{
    ChangesetMeta, InsertOutcome, LimitKind, ObjectStore, PutOutcome, Recovered, StoreError,
};

use crate::error::RepoError;
use crate::repo::{Repo, post_commit_warning, ref_error};
use crate::tree_edit::{Encoded, TreeEdit};

/// The most parents a changeset may have: what a journal record holds (its count is a byte).
const MAX_PARENTS: usize = 255;

/// The suffix no metadata key may end in ([`check_metadata`]): the operations' `show` gives a
/// metadata value that is not UTF-8 in base64, under its key with this appended, so that `k:b64`
/// always means `k`'s value in base64.
pub const RESERVED_METADATA_KEY_SUFFIX: &str = ":b64";

/// One change of a [`CommitRequest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// The path it changes.
    pub path: RepoPath,
    /// What it does there.
    pub op: ChangeOp,
}

/// What a [`Change`] does at its path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeOp {
    /// The path holds `content` as a `kind` leaf afterwards: a file, an executable or a symlink
    /// (whose content is its target), never a directory. Missing directories on the way are
    /// made.
    Put {
        /// File, Exec or Symlink.
        kind: LeafKind,
        /// The bytes.
        content: Vec<u8>,
    },
    /// The file at the path is gone afterwards, and so is each directory that leaves empty.
    Delete,
}

/// A commit to make: changes to the first parent's tree, and the changeset's other fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitRequest {
    /// The parents, the mainline parent first: the changes apply to its tree. None for a root
    /// changeset, whose changes apply to the empty tree.
    pub parents: Vec<ChangesetId>,
    /// The changes, in any order; each path at most once.
    pub changes: Vec<Change>,
    /// Who wrote the change.
    pub author: Signature,
    /// Who recorded it.
    pub committer: Signature,
    /// The message, verbatim.
    pub message: Vec<u8>,
    /// SwarmOps-owned key/value pairs; keys strictly ascending, none ending in `:b64`
    /// ([`check_metadata`]).
    pub metadata: Vec<(String, Vec<u8>)>,
    /// Whether a changeset that changes nothing may be made (no changes, or only puts of what
    /// their paths hold already); otherwise that is [`RepoError::EmptyChanges`].
    pub allow_empty: bool,
    /// A ref to move to the changeset in the same transaction, from the first parent (see [the
    /// module documentation](self)); `None` moves no ref.
    pub advance: Option<Advance>,
}

/// The ref a commit publishes its changeset on ([`CommitRequest::advance`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advance {
    /// The ref: `main` or `release/<x>`.
    pub name: String,
    /// Who moves it, as the journal's record of the move (the reflog) names them: at most 255
    /// bytes.
    pub actor: String,
}

/// The move of the ref a commit advanced ([`CommitOutcome::advanced`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advanced {
    /// The ref.
    pub name: String,
    /// What it named before the move: the changeset's first parent; `None` for a root changeset,
    /// whose move created the ref.
    pub old: Option<ChangesetId>,
    /// Whether the ref named the changeset already, so that nothing was written: an identical
    /// request committed it before (the insert is then [`InsertOutcome::AlreadyPresent`]).
    pub already_applied: bool,
}

/// What [`Repo::commit`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitOutcome {
    /// The changeset's id.
    pub id: ChangesetId,
    /// Its root tree.
    pub root: TreeId,
    /// How many paths the changeset changes: the length of its delta, which leaves out the
    /// puts of what a path held already.
    pub files_changed: usize,
    /// How many blobs this commit's own writes placed in the object store; blobs that were
    /// there already, or that a concurrent writer placed first, count 0.
    pub blobs_written: usize,
    /// How many trees this commit's own writes placed in the object store; trees that were
    /// there already, or that a concurrent writer placed first, count 0.
    pub trees_written: usize,
    /// What the insert did: [`InsertOutcome::AlreadyPresent`] when an identical request was
    /// committed before.
    pub insert: InsertOutcome,
    /// The move of the ref the request advanced; `None` without `advance`.
    pub advanced: Option<Advanced>,
    /// Steps after the commit point that failed, one line each: the changeset is committed
    /// all the same.
    pub warnings: Vec<String>,
    /// What the write transaction's recovery repaired before the insert, of what earlier writers
    /// left ([`swarmops_store::MetaWrite::recovered`]): on every outcome,
    /// [`InsertOutcome::AlreadyPresent`] included, which is what the same request made again
    /// after a crash or a warning finds. It is no warning, and it is lost when the call fails
    /// after the repair.
    pub recovered: Recovered,
}

impl<O: ObjectStore> Repo<O> {
    /// Applies `req`'s changes to its first parent's tree and records the result as a
    /// changeset: see [the module documentation](self) for the steps and their errors.
    pub fn commit(&self, req: CommitRequest) -> Result<CommitOutcome, RepoError> {
        let CommitRequest {
            parents,
            changes,
            author,
            committer,
            message,
            metadata,
            allow_empty,
            advance,
        } = req;
        if let Some(Advance { name, .. }) = &advance {
            swarmops_store::check_ref_name(name).map_err(ref_error)?;
        }
        check_metadata(&metadata)?;
        let changes = sorted_changes(changes, allow_empty)?;
        self.check_parents(&parents)?;
        let (candidates, mut contents) = candidate_delta(changes);
        let mut edit = match parents.first() {
            Some(&first) => TreeEdit::new(&self.objects, self.changeset_object(first)?.root)?,
            None => TreeEdit::empty(&self.objects),
        };
        let effective = edit.apply(&candidates)?;
        let delta: Vec<DeltaEntry> = candidates
            .into_iter()
            .zip(effective)
            .filter_map(|(entry, effective)| effective.then_some(entry))
            .collect();
        if delta.is_empty() && !allow_empty {
            return Err(RepoError::EmptyChanges);
        }
        let files_changed = delta.len();
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
        let changeset = Changeset {
            parents,
            root,
            delta,
            author,
            committer,
            message,
            extra_headers: Vec::new(),
            metadata,
        };
        let (object, delta_hash) = changeset.encode_hashed()?;
        let id = ChangesetId(object.id());
        let meta = ChangesetMeta {
            id,
            seq: 0,
            generation: 0,
            delta_hash,
            parents: changeset.parents,
        };

        let (blob_count, tree_count) = (contents.len(), trees.len());
        let mut batch = Vec::with_capacity(blob_count + tree_count + 1);
        batch.extend(contents.into_values());
        batch.extend(trees);
        batch.push(object);
        let outcomes = self.objects.put_hashed_batch(batch)?;
        let placed = |outcomes: &[PutOutcome]| outcomes.iter().filter(|o| o.written).count();
        let (blobs, trees) = outcomes.split_at(blob_count);
        let (blobs_written, trees_written) = (placed(blobs), placed(&trees[..tree_count]));
        self.objects.flush()?;

        let (insert, advanced, warnings, recovered) = self.record(meta, advance)?;
        Ok(CommitOutcome {
            id,
            root,
            files_changed,
            blobs_written,
            trees_written,
            insert,
            advanced,
            warnings,
            recovered,
        })
    }

    /// Refuses more parents than a journal record holds ([`StoreError::Limit`], as the
    /// metadata store gives at insert), a parent listed twice ([`StoreError::DuplicateParent`]),
    /// or one not in the store ([`RepoError::UnknownParent`]).
    fn check_parents(&self, parents: &[ChangesetId]) -> Result<(), RepoError> {
        check_parent_list(parents)?;
        if parents.is_empty() {
            return Ok(());
        }
        let read = self.meta.read()?;
        for &parent in parents {
            if read.changeset(parent)?.is_none() {
                return Err(RepoError::UnknownParent(parent));
            }
        }
        Ok(())
    }

    /// Inserts `meta` in a write transaction of its own, with `advance` beside the move of that
    /// ref from the changeset's first parent to it, and commits it if it queued anything: see
    /// [the module documentation](self). Returns what the insert did, the move, a warning if a
    /// step after the commit point failed, and what the transaction's recovery repaired.
    fn record(
        &self,
        meta: ChangesetMeta,
        advance: Option<Advance>,
    ) -> Result<(InsertOutcome, Option<Advanced>, Vec<String>, Recovered), RepoError> {
        use swarmops_store::CasResult;

        // The move expects the first parent, never a value of its own: an expectation of
        // another head would drop that head from the ref's first-parent history.
        let (id, expected) = (meta.id, meta.parents.first().copied());
        let mut write = self.meta.write()?;
        let recovered = write.recovered();
        let insert = write.insert_changeset(meta).map_err(|e| match e {
            StoreError::MissingParent(parent) => RepoError::UnknownParent(parent),
            other => other.into(),
        })?;
        let advanced = match advance {
            None => None,
            Some(Advance { name, actor }) => {
                let moved = write
                    .cas_ref(&name, expected, Some(id), &actor)
                    .map_err(ref_error)?;
                let already_applied = match moved {
                    CasResult::Applied { .. } => false,
                    // The ref names the changeset already: an identical request committed it,
                    // so the insert found it too.
                    CasResult::Mismatch { actual } if actual == Some(id) => true,
                    // Another move came first. Returning drops the write, which appends nothing
                    // and releases the lock: neither the changeset nor the move is recorded.
                    CasResult::Mismatch { actual } => {
                        return Err(RepoError::RefMoved {
                            name,
                            actual,
                            recovered: Box::new(recovered),
                        });
                    }
                };
                Some(Advanced {
                    name,
                    old: expected,
                    already_applied,
                })
            }
        };
        let inserted = matches!(insert, InsertOutcome::Inserted { .. });
        let moved = advanced
            .as_ref()
            .filter(|advanced| !advanced.already_applied);
        let mut warnings = Vec::new();
        // An insert that found the changeset, and a move the ref had made already, queued
        // nothing: then dropping the write releases the lock and appends nothing.
        if inserted || moved.is_some() {
            match write.commit() {
                Ok(_) => {}
                Err(StoreError::PostCommit { frame_seq, source }) => {
                    let what = match moved {
                        Some(moved) if inserted => {
                            format!("the changeset and the move of ref {}", moved.name)
                        }
                        Some(moved) => format!("the move of ref {}", moved.name),
                        None => "the changeset".to_owned(),
                    };
                    warnings.push(post_commit_warning(&what, frame_seq, &source));
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok((insert, advanced, warnings, recovered))
    }
}

/// Refuses `metadata` with a key that ends in [`RESERVED_METADATA_KEY_SUFFIX`]:
/// [`RepoError::ReservedMetadataKey`], naming the first such key. The rule every writer of
/// changesets runs before it writes anything, [`Repo::commit`] among them, so that a writer below
/// the operations (the git import) keeps it as a commit through them does: `show` gives a value
/// that is not UTF-8 under its key with that suffix appended, which a stored key ending in it
/// would make ambiguous.
pub fn check_metadata(metadata: &[(String, Vec<u8>)]) -> Result<(), RepoError> {
    match metadata
        .iter()
        .find(|(key, _)| key.ends_with(RESERVED_METADATA_KEY_SUFFIX))
    {
        Some((key, _)) => Err(RepoError::ReservedMetadataKey { key: key.clone() }),
        None => Ok(()),
    }
}

/// Refuses more parents than a journal record holds ([`StoreError::Limit`], as the metadata
/// store gives at insert) or a parent listed twice ([`StoreError::DuplicateParent`]): what every
/// writer of changesets checks of a parent list before it looks a parent up, [`Repo::commit`] and
/// [`Repo::commit_raw_batch`] among them.
pub(crate) fn check_parent_list(parents: &[ChangesetId]) -> Result<(), RepoError> {
    if parents.len() > MAX_PARENTS {
        return Err(StoreError::Limit {
            what: LimitKind::Parents,
            max: to_u64(MAX_PARENTS),
            got: to_u64(parents.len()),
        }
        .into());
    }
    for (at, parent) in parents.iter().enumerate() {
        if parents[..at].contains(parent) {
            return Err(StoreError::DuplicateParent(*parent).into());
        }
    }
    Ok(())
}

/// `changes` sorted by path, once checked: not empty (unless `allow_empty`), each path once.
fn sorted_changes(mut changes: Vec<Change>, allow_empty: bool) -> Result<Vec<Change>, RepoError> {
    if changes.is_empty() && !allow_empty {
        return Err(RepoError::EmptyChanges);
    }
    changes.sort_by(|a, b| a.path.cmp(&b.path));
    if let Some(pair) = changes.windows(2).find(|pair| pair[0].path == pair[1].path) {
        return Err(RepoError::DuplicatePath {
            path: pair[0].path.clone(),
        });
    }
    Ok(changes)
}

/// The delta `changes` (sorted, checked) would make, each put naming the blob of its content,
/// with the contents as blob objects by id: each content hashed once.
fn candidate_delta(changes: Vec<Change>) -> (Vec<DeltaEntry>, BTreeMap<ObjectId, Hashed>) {
    let mut contents = BTreeMap::new();
    let delta = changes
        .into_iter()
        .map(|Change { path, op }| {
            let op = match op {
                ChangeOp::Put { kind, content } => {
                    let blob = Hashed::new(ObjectKind::Blob, content);
                    let id = blob.id();
                    contents.entry(id).or_insert(blob);
                    DeltaOp::Put {
                        kind,
                        id: BlobId(id),
                    }
                }
                ChangeOp::Delete => DeltaOp::Delete,
            };
            DeltaEntry {
                path,
                op,
                renamed_from: None,
            }
        })
        .collect();
    (delta, contents)
}
