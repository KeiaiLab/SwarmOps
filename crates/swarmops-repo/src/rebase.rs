//! Rebase (M5; operations spec §3, the store-only `rebase`): [`Repo::rebase`] merges a change, the
//! first-parent chain from a base to a head, three-way onto a ref's head `T`, as a land does, and
//! records the result as one changeset `H'` whose one parent is `T`, moving no ref.
//!
//! It is a land's preparation ([`crate::land`], steps 1 to 4: the request checks, the merge, the
//! resolutions and the conflict report) with a commit as its last step ([`Repo::commit_in`], in
//! the snapshot the request was checked in), so that there is one merge:
//!
//! - A chain whose base is `T` sits on it already: [`RebaseStatus::UpToDate`], the head itself.
//! - A head on `T`'s first-parent chain, or a merge that leaves `T`'s tree as it is:
//!   [`RebaseStatus::Absorbed`], `T` itself.
//! - A conflict, or a stale resolution: [`RepoError::Conflict`] with the report a land of the same
//!   change gives, and nothing recorded.
//! - Otherwise `H'`: its delta the merge's, its message the request's or the head's, its author and
//!   committer the rebaser, its metadata the request's with `squashed_from`, the chain's ids, as a
//!   land records it. One frame holds its insert: [`RebaseStatus::Rebased`]. The same request on
//!   the same `T`, signed at the same time (a keyed rebase signs at its key's), is the same
//!   changeset, found under the lock without a frame: [`RebaseStatus::Replayed`].
//!
//! No retry loop: no ref is published, so no race can be lost. `H'` never carries `lander` or
//! `land_key`, so a changeset that has `lander` is one a land made.

use swarmops_core::{ChangesetId, DeltaOp, RepoPath, Signature, TreeId};
use swarmops_merge::tree_diff::{PathFilter, delta_of, diff_trees};
use swarmops_store::{InsertOutcome, ObjectStore, Recovered};

use crate::commit::{Change, ChangeOp, CommitRequest};
use crate::error::RepoError;
use crate::land::{LandRequest, Prep, Prepared, Resolutions};
use crate::repo::Repo;

/// The metadata key of the chain a rebased changeset squashed, as a land records it.
const SQUASHED_FROM: &str = "squashed_from";

/// A rebase to make ([`Repo::rebase`]): the change, the first-parent chain from `base` to `head`,
/// onto the head of the ref `onto`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebaseRequest {
    /// The change's head: the last changeset of the chain.
    pub head: ChangesetId,
    /// Where the change starts, as a land's base: a first-parent ancestor of `head`. `None`: the
    /// merge base of `head` and the ref's head, along first parents.
    pub base: Option<ChangesetId>,
    /// The ref to rebase onto: `main` or `release/<x>`, which must exist.
    pub onto: String,
    /// The recorded changeset's message; `None`: the head's.
    pub message: Option<Vec<u8>>,
    /// Who rebases: the recorded changeset's author and committer.
    pub rebaser: Signature,
    /// The recorded changeset's metadata, beside the `squashed_from` the rebase records: the
    /// rebaser's `agent` and `run_id`, and a keyed rebase's `commit_key`, say. Any order: the
    /// rebase sorts it. `lander` and `land_key` are a land's alone, and the caller gives neither.
    pub metadata: Vec<(String, Vec<u8>)>,
    /// What to make of some paths of the merge, as a land's ([`Resolutions`]); `None`: none.
    pub resolutions: Option<Resolutions>,
}

impl RebaseRequest {
    /// A rebase of `head` onto `onto` by `rebaser`: its base the merge base, the head's message,
    /// no metadata of the rebaser's, and no resolutions.
    pub fn new(head: ChangesetId, onto: &str, rebaser: Signature) -> RebaseRequest {
        RebaseRequest {
            head,
            base: None,
            onto: onto.to_owned(),
            message: None,
            rebaser,
            metadata: Vec::new(),
            resolutions: None,
        }
    }
}

/// What a rebase did ([`Repo::rebase`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebaseOutcome {
    /// How it ended.
    pub status: RebaseStatus,
    /// The changeset that holds the change on the ref's head: `H'`, the head when up to date, the
    /// ref's head when absorbed.
    pub changeset: ChangesetId,
    /// Its root tree.
    pub root: TreeId,
    /// The ref's head the change was merged onto: `H'`'s parent.
    pub onto: ChangesetId,
    /// The base the change was taken from.
    pub base: ChangesetId,
    /// How many paths `changeset` changes against `onto`.
    pub files_changed: usize,
    /// The paths both sides changed whose contents merged into content neither had, sorted.
    pub auto_merged: Vec<RepoPath>,
    /// The chain `H'` squashed, oldest first; empty when nothing was merged.
    pub squashed_from: Vec<ChangesetId>,
    /// Steps after the commit point that failed, one line each: `H'` is recorded all the same.
    pub warnings: Vec<String>,
    /// What the commit's write transaction repaired first, of what earlier writers left.
    pub recovered: Recovered,
}

/// How a rebase ended ([`RebaseOutcome::status`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RebaseStatus {
    /// It recorded `H'` now.
    Rebased,
    /// The store held `H'` already: the same request made it before, and nothing was written.
    Replayed,
    /// The chain sits on the ref's head already: nothing was written.
    UpToDate,
    /// The ref's head holds the change already: nothing was written.
    Absorbed,
}

impl<O: ObjectStore> Repo<O> {
    /// Rebases `req`'s change onto its ref's head and records the result, moving no ref: see [the
    /// module documentation](self). The request's errors are a land's: [`RepoError::RefName`],
    /// [`RepoError::NotFound`], [`RepoError::BaseNotAncestor`], [`RepoError::NoMergeBase`],
    /// [`RepoError::MergeChangeset`], [`RepoError::Conflict`]; the commit's are a commit's.
    pub fn rebase(&self, req: RebaseRequest) -> Result<RebaseOutcome, RepoError> {
        let RebaseRequest {
            head,
            base,
            onto,
            message,
            rebaser,
            mut metadata,
            resolutions,
        } = req;

        // 1. The request checked as a land's, in one snapshot.
        let land = LandRequest {
            base,
            max_attempts: 1,
            resolutions,
            ..LandRequest::new(head, &onto, rebaser.clone())
        };
        let started = self.start(land)?;
        let (base, theirs) = (started.base(), started.theirs());

        // 2. A chain on the ref's head already: the head, nothing written.
        if base == theirs {
            let changeset = started.read().changeset(head)?;
            let root = changeset
                .ok_or_else(|| RepoError::NotFound(head.to_string()))?
                .changeset
                .root;
            let onto_root = self.changeset_object(theirs)?.root;
            let paths = PathFilter::all();
            let changes = diff_trees(started.read(), Some(onto_root), Some(root), &paths)?;
            return Ok(RebaseOutcome {
                files_changed: delta_of(&changes).len(),
                ..unwritten(RebaseStatus::UpToDate, head, root, theirs, base)
            });
        }

        // 3. The merge onto it, as a land's attempt prepares it.
        let (read, prep) = started.prepare()?;
        let prepared = match prep {
            Prep::Merged(prepared) => *prepared,
            Prep::Nothing(seen, _) => {
                let root = self.changeset_object(seen)?.root;
                return Ok(unwritten(RebaseStatus::Absorbed, seen, root, seen, base));
            }
            // A land without a key never finds a replay, and a rebase's has none.
            Prep::Replay(..) => unreachable!("a rebase's land request has no key"),
        };
        let Prepared {
            draft,
            theirs,
            base,
            chain,
        } = prepared;

        // 4. H': the merge committed on the ref's head, in the snapshot the request read.
        let listed: Vec<String> = chain.iter().map(ToString::to_string).collect();
        metadata.push((SQUASHED_FROM.to_owned(), listed.join(" ").into_bytes()));
        metadata.sort_by(|a, b| a.0.cmp(&b.0));
        let message = match message {
            Some(message) => message,
            None => head_message(&read, head)?,
        };
        let mut new_blobs = draft.new_blobs;
        let changes: Vec<Change> = (draft.delta.into_iter())
            .map(|entry| Change {
                op: match entry.op {
                    DeltaOp::Put { kind, id } => match new_blobs.remove(&id) {
                        Some(content) => ChangeOp::Put { kind, content },
                        None => ChangeOp::PutBlob { kind, id },
                    },
                    DeltaOp::Delete => ChangeOp::Delete,
                },
                path: entry.path,
            })
            .collect();
        let commit = CommitRequest {
            parents: vec![theirs.id],
            changes,
            author: rebaser.clone(),
            committer: rebaser,
            message,
            metadata,
            allow_empty: false,
            advance: None,
        };
        let committed = self.commit_in(&read, commit)?;

        // 5. What it recorded, or found recorded.
        let status = match committed.insert {
            InsertOutcome::AlreadyPresent => RebaseStatus::Replayed,
            InsertOutcome::Inserted { .. } => RebaseStatus::Rebased,
        };
        Ok(RebaseOutcome {
            status,
            changeset: committed.id,
            root: committed.root,
            onto: theirs.id,
            base,
            files_changed: committed.files_changed,
            auto_merged: draft.auto_merged,
            squashed_from: chain,
            warnings: committed.warnings,
            recovered: committed.recovered,
        })
    }
}

/// The message of changeset `head`, as `read` holds it.
fn head_message<O: ObjectStore>(
    read: &crate::read::RepoRead<'_, O>,
    head: ChangesetId,
) -> Result<Vec<u8>, RepoError> {
    let stored = read.changeset(head)?;
    let stored = stored.ok_or_else(|| RepoError::NotFound(head.to_string()))?;
    Ok(stored.changeset.message)
}

/// A rebase that wrote nothing: `changeset`, whose root is `root`, on `onto`, from `base`.
fn unwritten(
    status: RebaseStatus,
    changeset: ChangesetId,
    root: TreeId,
    onto: ChangesetId,
    base: ChangesetId,
) -> RebaseOutcome {
    RebaseOutcome {
        status,
        changeset,
        root,
        onto,
        base,
        files_changed: 0,
        auto_merged: Vec::new(),
        squashed_from: Vec::new(),
        warnings: Vec::new(),
        recovered: Recovered::default(),
    }
}
