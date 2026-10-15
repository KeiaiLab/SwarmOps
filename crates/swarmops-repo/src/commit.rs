//! Commits from change lists (storage spec §2 rule 2, §4): [`Repo::commit`] applies a
//! [`CommitRequest`]'s changes to its first parent's tree and records the result.
//!
//! 1. Checks the request before it reads or writes anything: changes to make, unless
//!    `allow_empty` ([`RepoError::EmptyChanges`]), no directory put ([`RepoError::DirPut`]),
//!    each path once ([`RepoError::DuplicatePath`]), parents distinct and in the store
//!    ([`RepoError::UnknownParent`]).
//! 2. Hashes each put's content into its blob id, and applies the changes to the first
//!    parent's root tree in memory, the deletes first: a put below a file
//!    or where a directory is, or a delete of a directory, is a [`RepoError::TypeConflict`]; a
//!    delete of a path that is no file, a [`RepoError::PathNotFound`]. A put of what its path
//!    holds already changes nothing and stays out of the delta; a request of nothing else is
//!    [`RepoError::EmptyChanges`], unless `allow_empty`: then the changeset keeps its first
//!    parent's root (the empty tree for a root changeset), with an empty delta.
//! 3. Encodes the changed directories bottom-up, then the changeset: its delta is the changes
//!    that changed the tree, sorted by path.
//! 4. Writes the blobs the delta puts, the changed trees and the changeset object in one
//!    `put_batch`, then flushes: every object is durable before any metadata names it (I1). A
//!    flush error fails the commit, and nothing is recorded.
//! 5. Inserts the changeset in a write transaction of its own and commits that: its journal
//!    frame is the commit point. A failure after it ([`StoreError::PostCommit`]) leaves the
//!    changeset committed, and is reported as a warning; the next write to the store repairs
//!    what is missing.
//!
//! An identical request computes the same objects and the same changeset: the store has them
//! all, so nothing is written, and the insert finds the changeset
//! ([`InsertOutcome::AlreadyPresent`]) without appending a transaction.

use std::collections::{BTreeMap, BTreeSet};

use swarmops_core::{
    BlobId, Changeset, ChangesetId, DeltaEntry, DeltaOp, EntryKind, ObjectId, ObjectKind, RepoPath,
    Signature, TreeId, hash_object,
};
use swarmops_store::{ChangesetMeta, InsertOutcome, ObjectStore, StoreError};

use crate::error::RepoError;
use crate::repo::Repo;
use crate::tree_edit::{Encoded, TreeEdit};

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
        kind: EntryKind,
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
    /// SwarmOps-owned key/value pairs; keys strictly ascending.
    pub metadata: Vec<(String, Vec<u8>)>,
    /// Whether a changeset that changes nothing may be made (no changes, or only puts of what
    /// their paths hold already); otherwise that is [`RepoError::EmptyChanges`].
    pub allow_empty: bool,
}

/// What [`Repo::commit`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitOutcome {
    /// The changeset's id.
    pub id: ChangesetId,
    /// Its root tree.
    pub root: TreeId,
    /// How many blobs this call wrote that the store did not hold yet.
    pub blobs_written: usize,
    /// How many trees this call wrote that the store did not hold yet.
    pub trees_written: usize,
    /// What the insert did: [`InsertOutcome::AlreadyPresent`] when an identical request was
    /// committed before.
    pub insert: InsertOutcome,
    /// Steps after the commit point that failed, one line each: the changeset is committed
    /// all the same.
    pub warnings: Vec<String>,
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
        } = req;
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
        let bytes = changeset.encode()?;
        let id = ChangesetId(hash_object(ObjectKind::Changeset, &bytes));
        let meta = ChangesetMeta {
            id,
            seq: 0,
            generation: 0,
            delta_hash: changeset.delta_hash()?,
            parents: changeset.parents,
        };

        let blobs_written = self.count_missing(contents.keys().copied())?;
        let trees_written = self.count_missing(trees.iter().map(|(tree, _)| tree.0))?;
        let mut batch = Vec::with_capacity(contents.len() + trees.len() + 1);
        batch.extend(contents.into_values().map(|blob| (ObjectKind::Blob, blob)));
        batch.extend(trees.into_iter().map(|(_, tree)| (ObjectKind::Tree, tree)));
        batch.push((ObjectKind::Changeset, bytes));
        self.objects.put_batch(batch)?;
        self.objects.flush()?;

        let (insert, warnings) = self.record(meta)?;
        Ok(CommitOutcome {
            id,
            root,
            blobs_written,
            trees_written,
            insert,
            warnings,
        })
    }

    /// Refuses a parent listed twice ([`StoreError::DuplicateParent`]) or not in the store
    /// ([`RepoError::UnknownParent`]).
    fn check_parents(&self, parents: &[ChangesetId]) -> Result<(), RepoError> {
        for (at, parent) in parents.iter().enumerate() {
            if parents[..at].contains(parent) {
                return Err(StoreError::DuplicateParent(*parent).into());
            }
        }
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

    /// How many of `ids` the object store does not hold.
    fn count_missing(&self, ids: impl IntoIterator<Item = ObjectId>) -> Result<usize, RepoError> {
        let mut missing = 0;
        for id in ids {
            if !self.objects.contains(id)? {
                missing += 1;
            }
        }
        Ok(missing)
    }

    /// Inserts `meta` in a write transaction of its own and commits it, if the store does
    /// not hold the changeset yet. Returns what the insert did, and a warning if a step after
    /// the commit point failed.
    fn record(&self, meta: ChangesetMeta) -> Result<(InsertOutcome, Vec<String>), RepoError> {
        let mut write = self.meta.write()?;
        let insert = write.insert_changeset(meta).map_err(|e| match e {
            StoreError::MissingParent(parent) => RepoError::UnknownParent(parent),
            other => other.into(),
        })?;
        let mut warnings = Vec::new();
        // An insert that found the changeset queued nothing: dropping the write releases the
        // lock and appends nothing.
        if let InsertOutcome::Inserted { .. } = insert {
            match write.commit() {
                Ok(_) => {}
                Err(StoreError::PostCommit { frame_seq, source }) => warnings.push(format!(
                    "the changeset is committed (journal frame {frame_seq}), but a step after \
                     the commit point failed: {source}; the next write to the store repairs it"
                )),
                Err(e) => return Err(e.into()),
            }
        }
        Ok((insert, warnings))
    }
}

/// `changes` sorted by path, once checked: not empty (unless `allow_empty`), no directory
/// put, each path once.
fn sorted_changes(mut changes: Vec<Change>, allow_empty: bool) -> Result<Vec<Change>, RepoError> {
    if changes.is_empty() && !allow_empty {
        return Err(RepoError::EmptyChanges);
    }
    let dir_put = changes.iter().find(|change| {
        matches!(
            change.op,
            ChangeOp::Put {
                kind: EntryKind::Dir,
                ..
            }
        )
    });
    if let Some(change) = dir_put {
        return Err(RepoError::DirPut {
            path: change.path.clone(),
        });
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
/// with the contents by blob id.
fn candidate_delta(changes: Vec<Change>) -> (Vec<DeltaEntry>, BTreeMap<ObjectId, Vec<u8>>) {
    let mut contents = BTreeMap::new();
    let delta = changes
        .into_iter()
        .map(|Change { path, op }| {
            let op = match op {
                ChangeOp::Put { kind, content } => {
                    let id = hash_object(ObjectKind::Blob, &content);
                    contents.entry(id).or_insert(content);
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
