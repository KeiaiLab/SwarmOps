//! The git export (storage spec §7, umbrella §12): the history behind a store ref written into a
//! branch of a git repository through [`GitSink`], every commit the branch's last export does not
//! reach, parents first; then the branch moved to the export of the ref's head by
//! compare-and-swap, once everything it reaches is durable. Run again, it writes only what is
//! new; it stops on a branch that diverged ([`ExportError::Diverged`]) rather than merge.
//!
//! [`export()`] goes in nine steps, each done before anything of the next is written.
//!
//! 1. The source ref's name is checked, a branch that a work tree has checked out is refused
//!    ([`ExportError::CheckedOut`]), and the source ref's head is read in a committed snapshot
//!    ([`Repo::snapshot_committed`]: taken under the writer lock after the journal's tail check,
//!    so that nothing the export sends off the host can be taken back by a power loss or by a
//!    failed sync's cut).
//! 2. The branch is read, and the head's export oid derived ([`ExportOid`]).
//! 3. A branch at the head's export is up to date: nothing is written, and it does not move.
//! 4. A branch that exists must name the export of the head or of an ancestor of it, over every
//!    parent: the base, found through the external id index (the changeset it names for the
//!    branch's commit, whose export oid is that commit), else along the head's first parents
//!    compared by export oid. Anything else is a divergence, which [`DivergedReason`] says
//!    (found through the index, or along the branch's first 1,000 first parents in the
//!    repository), and nothing is written; a request that forces the move goes on from the base
//!    the divergence found, if any.
//! 5. The commits to write: from the head over every parent, highest generation first, the base
//!    and its ancestors marked out (their exports are what the branch reaches); a commit the
//!    repository holds in a pack is trusted, as git trusts it, with everything it reaches. They
//!    come out parents first.
//! 6. For each, parents first: the trees and blobs its root reaches that its first parent's root
//!    does not hold at the same path (a root changeset's all of them), each once, then the commit
//!    itself. An object the repository lacks is written (a commit rendered over its parents'
//!    export oids, its rendering checked against its own: [`ExportError::Bug`] otherwise); a
//!    loose one is read back, and written again if it is damaged (an export cut short leaves
//!    loose objects, and a power loss can empty them); a packed one is trusted, and a tree there
//!    not descended.
//! 7. Every object written or read back loose is made durable ([`GitSink::sync`]).
//! 8. The native commits' oids are recorded in the external id index, each naming its changeset
//!    (W8 holds: the snapshot's changesets are committed); an imported changeset keeps its
//!    import's entry, and one rendered anew under a lossy lineage gets none.
//! 9. The branch moves from what step 2 read to the head's export, by compare-and-swap
//!    ([`GitSink::cas_branch`]): one that another export moved there meanwhile is up to date
//!    too, and any other move is [`DivergedReason::MovedDuringExport`], which leaves the
//!    objects and the entries written: loose objects the branch does not reach, and entries that
//!    are true.
//!
//! The export writes no journal frame. Its store-side writes are the derived store's values
//! (export oids, and the memoized git oids of trees and blobs) and the external id entries. Its
//! reads go by the committed snapshot: the head, the changesets' records and objects, the trees
//! and blobs; the ancestry check of step 4 reads the records of the head's ancestors by id
//! (`ancestry.rs`), which no snapshot changes.
//!
//! [`Repo::snapshot_committed`]: swarmops_repo::Repo::snapshot_committed

use std::collections::{BinaryHeap, HashMap, HashSet};

use swarmops_core::bytes::to_u64;
use swarmops_core::{BlobId, Changeset, ChangesetId, EntryTarget, Tree, TreeEntry, TreeId};
use swarmops_repo::{Derivation, LocalRepo, RepoError, RepoRead, StoreError};
use swarmops_store::{ExternalEntry, FsObjectStore, SYSTEM_GIT, TailRepair, check_ref_name};

use crate::ancestry;
use crate::native::{metadata, render_commit};
use crate::render::tree_bytes;
use crate::{
    BranchUpdate, ExportOid, ExportReader, ExportedOid, GIT_OID, GitOid, GitSink, ObjectFormat,
    OidCache, RawCommit, RawKind, SinkError, Stored, Unsupported, git_hash, real_blob_oid,
    real_tree_oid,
};

/// How many first parents of the branch's commit step 4 reads in the repository, looking for the
/// export of the head or of an ancestor of it, before it says the branch moved.
const BRANCH_WALK: usize = 1_000;

/// What to export, and into which branch ([`export()`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportRequest {
    /// The store ref whose history is exported, `main` or `release/<x>`: its head is the commit
    /// the branch ends at.
    pub source_ref: String,
    /// The git branch written, by its short name (`refs/heads/<branch>`), created if it does not
    /// exist.
    pub branch: String,
    /// Whether a branch that diverged moves all the same, by compare-and-swap against the commit
    /// the export read, so that a branch another writer moves meanwhile still stops it. It never
    /// moves a branch a work tree has checked out.
    pub force: bool,
}

impl ExportRequest {
    /// A request to export `source_ref` into the branch `branch`, without `force`.
    pub fn new(source_ref: &str, branch: &str) -> ExportRequest {
        ExportRequest {
            source_ref: source_ref.to_owned(),
            branch: branch.to_owned(),
            force: false,
        }
    }
}

/// What an export did ([`export()`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportReport {
    /// The changeset the source ref named in the committed snapshot the export read: the head.
    pub head: ChangesetId,
    /// The commit the head exports to ([`ExportOid`]), which the branch names now.
    pub head_oid: GitOid,
    /// The branch, by its short name.
    pub branch: String,
    /// The commit the branch named when the export read it; `None` when there was no such
    /// branch, and the export created it.
    pub old: Option<GitOid>,
    /// The changeset whose export the branch named, which the export went on from: the head or
    /// an ancestor of it, whose history the branch reaches already. With `force`, the changeset a
    /// divergence found the branch at the export of, if any. `None` for a branch the export
    /// created, and for one forced from a commit that is no export of the store's.
    pub base: Option<ChangesetId>,
    /// Whether this export moved the branch, or created it; `false` when the branch named the
    /// head's export already, after an earlier export or a concurrent one.
    pub branch_moved: bool,
    /// How many commits it wrote.
    pub commits_written: u64,
    /// How many objects it wrote, commits, trees and blobs, damaged loose ones written again
    /// among them.
    pub objects_written: u64,
    /// How many loose objects the repository held already that it read back before it trusted
    /// them: what an export cut short left, and what a power loss may have emptied. A damaged one
    /// among them is written again, and counted in [`ExportReport::objects_written`] too.
    pub objects_checked: u64,
    /// How many of the commits it wrote or read back were imported from git and export to
    /// another commit than their own: a commit imported with a loss (`git.lossy`), and every
    /// commit above it, whose parent has another oid now. Their signature headers no longer
    /// verify, and a warning says how many they are.
    pub rewritten: u64,
    /// What the journal's tail check under the writer lock repaired before the snapshot was
    /// taken ([`swarmops_repo::Repo::snapshot_committed`]).
    pub recovered: TailRepair,
    /// What did not stop the export, one line each: commits rewritten, a damaged loose object
    /// written again, an external id entry that was no use, a forced divergence.
    pub warnings: Vec<String>,
}

/// Why an export failed. Nothing is written but where a variant says otherwise.
#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    /// The source ref does not exist. Nothing was read but the ref.
    #[error("the store has no ref {0}")]
    SourceRef(String),
    /// The source ref's name is no ref name: refs are `main` and `release/<x>`. Nothing was read.
    #[error("invalid source ref name {0:?}: a ref is main or release/<name>")]
    RefName(String),
    /// The branch is checked out in the repository's work tree: moving it would leave the work
    /// tree behind its `HEAD`, which `git push` refuses by default (`receive.denyCurrentBranch`).
    /// Refused before the store or the branch is read, `force` or not.
    #[error(
        "the branch {branch} is checked out in the repository's work tree, which an export does \
         not move"
    )]
    CheckedOut {
        /// The branch.
        branch: String,
    },
    /// The branch names no export of the source's head or of an ancestor of it: [`reason`]
    /// says what happened. Nothing was written, unless the branch moved during the export
    /// ([`DivergedReason::MovedDuringExport`]).
    ///
    /// [`reason`]: ExportError::Diverged::reason
    #[error(
        "the branch {branch} has diverged from {head} ({}): it names {git_head}",
        reason.as_str()
    )]
    Diverged {
        /// The branch.
        branch: String,
        /// The commit the branch names: the one the export read, or for
        /// [`DivergedReason::MovedDuringExport`] the one the compare-and-swap found
        /// ([`GitOid::NULL`] if it found no branch).
        git_head: GitOid,
        /// The source ref's head.
        head: ChangesetId,
        /// Why the branch is no export the source's head goes on from.
        reason: DivergedReason,
    },
    /// The repository could not be read or written: its objects, its branch, its lock. Objects
    /// written before the failure stay, loose, and the branch is as it was.
    #[error(transparent)]
    Sink(#[from] SinkError),
    /// The store failed: its snapshot, an object, a derived value, the external id index.
    /// Objects written before the failure stay, loose, and the branch is as it was.
    #[error(transparent)]
    Repo(#[from] RepoError),
    /// What can only be a bug: a commit whose rendering does not hash to its export oid, an
    /// object the repository wrote under another oid, an external id entry naming another
    /// changeset for a commit the export wrote. Nothing more is written, and the branch is not
    /// moved.
    #[error("an export bug: {reason}; the branch was not moved")]
    Bug {
        /// What was found.
        reason: String,
    },
}

/// Why a branch is no export the source's head goes on from ([`ExportError::Diverged`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DivergedReason {
    /// Commits were made in git on top of the export of the head itself: `git import` brings them
    /// into the store, and the export after it goes on.
    BranchAhead {
        /// The changeset the branch's history holds the export of: the head.
        exported: ChangesetId,
    },
    /// Commits were made in git on top of the export of an older ancestor of the head, which the
    /// store moved past meanwhile: two writers, and the export merges nothing.
    BothMoved {
        /// The changeset the branch's history holds the export of.
        exported: ChangesetId,
    },
    /// The branch names a commit that is no export of the head's history, and that has none
    /// among its first 1,000 first parents.
    BranchMoved,
    /// The branch names the export of a changeset that is neither the head nor an ancestor of
    /// it: the source ref moved back (an `undo`, a `ref set` backwards) or elsewhere.
    SourceMovedBack {
        /// The changeset the branch names the export of.
        exported: ChangesetId,
    },
    /// The branch names the commit a changeset was imported from, which that changeset no longer
    /// exports to: it was imported with a loss (`git.lossy`), or above one, and exports as a new
    /// commit, which no branch built on the original can take.
    LossyHistory {
        /// The changeset imported from the branch's commit.
        changeset: ChangesetId,
    },
    /// The branch moved while the export ran: the compare-and-swap found it elsewhere. The
    /// objects and the external id entries written stay: loose objects the branch does not reach,
    /// and entries that are true.
    MovedDuringExport {
        /// What the branch names: `None` when it no longer exists.
        actual: Option<GitOid>,
    },
}

impl DivergedReason {
    /// The reason's name, as `E_GIT_EXPORT_DIVERGED`'s `details.reason` gives it: `branch_ahead`,
    /// `both_moved`, `branch_moved`, `source_moved_back`, `lossy_history` or
    /// `moved_during_export`.
    pub fn as_str(&self) -> &'static str {
        match self {
            DivergedReason::BranchAhead { .. } => "branch_ahead",
            DivergedReason::BothMoved { .. } => "both_moved",
            DivergedReason::BranchMoved => "branch_moved",
            DivergedReason::SourceMovedBack { .. } => "source_moved_back",
            DivergedReason::LossyHistory { .. } => "lossy_history",
            DivergedReason::MovedDuringExport { .. } => "moved_during_export",
        }
    }
}

/// Exports the history behind `req.source_ref` in `repo` into the branch `req.branch` of the
/// repository `sink` writes, and moves the branch to the export of the ref's head: see [the
/// module documentation](self) for the steps and their errors. `repo` must have [`ExportOid`]
/// registered ([`crate::register`]); without it the export fails with [`RepoError::Derived`]
/// before it writes anything.
pub fn export(
    repo: &LocalRepo,
    sink: &mut dyn GitSink,
    req: &ExportRequest,
) -> Result<ExportReport, ExportError> {
    if check_ref_name(&req.source_ref).is_err() {
        return Err(ExportError::RefName(req.source_ref.clone()));
    }
    let format = sink.object_format();
    if format != ObjectFormat::Sha1 {
        let format = Unsupported::ObjectFormat(format.as_str().to_owned());
        return Err(SinkError::Unsupported(format).into());
    }
    if sink.checked_out()?.as_deref() == Some(req.branch.as_str()) {
        return Err(ExportError::CheckedOut {
            branch: req.branch.clone(),
        });
    }
    let (read, recovered) = repo.snapshot_committed()?;
    let Some(state) = read.get_ref(&req.source_ref)? else {
        return Err(ExportError::SourceRef(req.source_ref.clone()));
    };
    let head = state.target;
    let old = sink.branch(&req.branch)?;
    let head_oid = repo.derived::<ExportOid>(head, &())?.git_oid();
    let mut report = ExportReport {
        head,
        head_oid,
        branch: req.branch.clone(),
        old,
        base: None,
        branch_moved: false,
        commits_written: 0,
        objects_written: 0,
        objects_checked: 0,
        rewritten: 0,
        recovered,
        warnings: Vec::new(),
    };
    if old == Some(head_oid) {
        report.base = Some(head);
        return Ok(report);
    }

    let mut run = Run::new(repo, &read, sink, head);
    if let Some(branch) = old {
        report.base = match run.base(branch)? {
            Found::Base(base) => Some(base),
            Found::Diverged { reason, base } if req.force => {
                run.warnings.push(format!(
                    "the branch {} had diverged ({}), and the export moved it all the same",
                    req.branch,
                    reason.as_str()
                ));
                base
            }
            Found::Diverged { reason, .. } => {
                return Err(ExportError::Diverged {
                    branch: req.branch.clone(),
                    git_head: branch,
                    head,
                    reason,
                });
            }
        };
    }
    let commits = run.commits(report.base)?;
    run.write(&commits)?;
    if !run.pending.is_empty() {
        let pending = std::mem::take(&mut run.pending);
        run.sink.sync(&pending)?;
    }
    run.record(&commits)?;
    let rewritten = (commits.iter())
        .filter(|commit| run.nodes[&commit.id].imported && !commit.exported.kept)
        .count();
    report.rewritten = to_u64(rewritten);
    if report.rewritten > 0 {
        run.warnings.push(format!(
            "{} of the commits exported were imported from git and export to new commits: a \
             history imported with a loss renders anew, with every commit above it, and their \
             signature headers no longer verify",
            report.rewritten
        ));
    }
    report.branch_moved = match run.sink.cas_branch(&req.branch, old, head_oid)? {
        BranchUpdate::Applied => true,
        // Another export of the same head moved it first.
        BranchUpdate::Mismatch { actual } if actual == Some(head_oid) => false,
        BranchUpdate::Mismatch { actual } => {
            return Err(ExportError::Diverged {
                branch: req.branch.clone(),
                git_head: actual.unwrap_or(GitOid::NULL),
                head,
                reason: DivergedReason::MovedDuringExport { actual },
            });
        }
    };
    report.commits_written = run.commits_written;
    report.objects_written = run.objects_written;
    report.objects_checked = run.objects_checked;
    report.warnings = run.warnings;
    Ok(report)
}

/// What step 4 of [the module documentation](self) found.
enum Found {
    /// The changeset whose export the branch names: the head or an ancestor of it.
    Base(ChangesetId),
    /// A divergence, and the base a forced export goes on from: the changeset it found the
    /// branch's history at the export of, if any.
    Diverged {
        reason: DivergedReason,
        base: Option<ChangesetId>,
    },
}

/// What the export keeps of a changeset its walks meet: its record and the fields of its object
/// that the steps read.
struct Node {
    /// 1 + the largest parent generation; 1 for a root.
    generation: u64,
    /// Its parents, in their order.
    parents: Vec<ChangesetId>,
    /// Its root.
    root: TreeId,
    /// Whether it was imported from git: it has [`GIT_OID`] metadata.
    imported: bool,
}

/// A commit of step 5: one the repository does not hold in a pack.
struct Commit {
    /// Its changeset.
    id: ChangesetId,
    /// Its export oid.
    exported: ExportedOid,
    /// How the repository held it when the walk met it: [`Stored::Absent`] or [`Stored::Loose`].
    stored: Stored,
}

/// Whether the walk of step 5 writes a changeset's commit (in), or leaves it out, with every
/// ancestor of it: the base's history.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mark {
    In,
    Out,
}

/// An export as it runs, from step 4 on.
struct Run<'a, 'r> {
    repo: &'a LocalRepo,
    read: &'a RepoRead<'r, FsObjectStore>,
    sink: &'a mut dyn GitSink,
    head: ChangesetId,
    /// The changesets the walks met, by id.
    nodes: HashMap<ChangesetId, Node>,
    /// The objects this run wrote, read back or found packed: each is handled once.
    seen: HashSet<GitOid>,
    /// The objects written or read back loose, in that order: what step 7 makes durable.
    pending: Vec<GitOid>,
    /// The real git oid of every object this run has read, each computed from the store's own
    /// bytes once ([`OidCache`]): what the export binds its reuse, its writes and the trees it
    /// renders to, never a memo of the derived store a store file could have placed.
    cache: OidCache,
    /// The changeset the export is writing, for the error a wrong memo raises.
    current: ChangesetId,
    commits_written: u64,
    objects_written: u64,
    objects_checked: u64,
    warnings: Vec<String>,
}

impl<'a, 'r> Run<'a, 'r> {
    fn new(
        repo: &'a LocalRepo,
        read: &'a RepoRead<'r, FsObjectStore>,
        sink: &'a mut dyn GitSink,
        head: ChangesetId,
    ) -> Run<'a, 'r> {
        Run {
            repo,
            read,
            sink,
            head,
            nodes: HashMap::new(),
            seen: HashSet::new(),
            pending: Vec::new(),
            cache: OidCache::new(),
            current: head,
            commits_written: 0,
            objects_written: 0,
            objects_checked: 0,
            warnings: Vec::new(),
        }
    }

    /// The snapshot's trees and blobs, with the derived store their git oids are memoized in.
    fn reader(&self) -> ExportReader<'_, FsObjectStore> {
        ExportReader {
            read: self.read,
            store: self.repo.derived_store(),
        }
    }

    /// The real git oid of tree `tree`, from the store's own bytes, the memo checked against it
    /// ([`real_tree_oid`]): what the export reuses, writes or renders for the tree, never a memo
    /// alone. Cached for the run.
    fn tree_git_oid(&self, tree: TreeId) -> Result<GitOid, ExportError> {
        Ok(real_tree_oid(
            &self.reader(),
            &self.cache,
            self.current,
            tree,
        )?)
    }

    /// The real git oid of blob `blob`, from the store's own bytes, the memo checked against it
    /// ([`real_blob_oid`]). Cached for the run.
    fn blob_git_oid(&self, blob: BlobId) -> Result<GitOid, ExportError> {
        Ok(real_blob_oid(
            &self.reader(),
            &self.cache,
            self.current,
            blob,
        )?)
    }

    /// Reads changeset `id`'s record and object into [`Run::nodes`], once; `false` when the
    /// snapshot does not hold it.
    fn load(&mut self, id: ChangesetId) -> Result<bool, ExportError> {
        if self.nodes.contains_key(&id) {
            return Ok(true);
        }
        let Some(stored) = self.read.changeset(id)? else {
            return Ok(false);
        };
        let imported = metadata(&stored.changeset, GIT_OID).is_some();
        let Changeset { parents, root, .. } = stored.changeset;
        let node = Node {
            generation: stored.generation,
            parents,
            root,
            imported,
        };
        self.nodes.insert(id, node);
        Ok(true)
    }

    /// Changeset `id`'s node, which the snapshot holds: the head, a parent of a changeset it
    /// holds, or one the external id index names that [`Run::load`] found.
    fn node(&mut self, id: ChangesetId) -> Result<&Node, ExportError> {
        if !self.load(id)? {
            return Err(RepoError::NotFound(id.to_string()).into());
        }
        Ok(&self.nodes[&id])
    }

    /// Step 4: the base the branch's commit `branch` (not the head's export) gives, or the
    /// divergence it is.
    fn base(&mut self, branch: GitOid) -> Result<Found, ExportError> {
        if let Some((changeset, exported)) = self.indexed(branch)? {
            if exported.git_oid() != branch {
                let reason = DivergedReason::LossyHistory { changeset };
                return Ok(Found::Diverged { reason, base: None });
            }
            if self.reaches_head(changeset)? {
                return Ok(Found::Base(changeset));
            }
            let reason = DivergedReason::SourceMovedBack {
                exported: changeset,
            };
            return Ok(Found::Diverged {
                reason,
                base: Some(changeset),
            });
        }
        // No entry: the head's first parents, compared by export oid.
        let mut chain = HashMap::new();
        for at in self.read.first_parents(self.head) {
            let at = at?;
            let exported = self.repo.derived::<ExportOid>(at, &())?.git_oid();
            if exported == branch {
                return Ok(Found::Base(at));
            }
            chain.entry(exported).or_insert(at);
        }
        // The branch's first parents in the repository: is one the export of the head, or of an
        // ancestor of it?
        let mut at = branch;
        for _ in 0..BRANCH_WALK {
            let Some(parent) = self.first_parent_in_repository(at)? else {
                break;
            };
            at = parent;
            let exported = match chain.get(&at) {
                Some(&changeset) => Some(changeset),
                None => self.indexed_ancestor(at)?,
            };
            if let Some(exported) = exported {
                let reason = if exported == self.head {
                    DivergedReason::BranchAhead { exported }
                } else {
                    DivergedReason::BothMoved { exported }
                };
                return Ok(Found::Diverged {
                    reason,
                    base: Some(exported),
                });
            }
        }
        let reason = DivergedReason::BranchMoved;
        Ok(Found::Diverged { reason, base: None })
    }

    /// The changeset the external id index names for commit `oid`, with its export oid, if the
    /// snapshot holds it. An entry that is damaged, or names a changeset the snapshot lacks, is
    /// passed over, with a warning.
    fn indexed(&mut self, oid: GitOid) -> Result<Option<(ChangesetId, ExportedOid)>, ExportError> {
        let entry = match self.repo.external_id(SYSTEM_GIT, &oid.0) {
            Ok(entry) => entry,
            Err(RepoError::Store(damaged @ StoreError::ExternalIdCorrupt { .. })) => {
                self.warnings.push(format!(
                    "the external id entry of git commit {oid} is passed over: {damaged}"
                ));
                return Ok(None);
            }
            Err(error) => return Err(error.into()),
        };
        let Some(entry) = entry else {
            return Ok(None);
        };
        if !self.load(entry.changeset)? {
            self.warnings.push(format!(
                "the external id entry of git commit {oid} is passed over: it names changeset {}, \
                 which the store does not hold",
                entry.changeset
            ));
            return Ok(None);
        }
        let exported = self.repo.derived::<ExportOid>(entry.changeset, &())?;
        Ok(Some((entry.changeset, exported)))
    }

    /// The changeset commit `oid` is the export of, through the external id index, when it is
    /// the head or an ancestor of it.
    fn indexed_ancestor(&mut self, oid: GitOid) -> Result<Option<ChangesetId>, ExportError> {
        match self.indexed(oid)? {
            Some((changeset, exported))
                if exported.git_oid() == oid && self.reaches_head(changeset)? =>
            {
                Ok(Some(changeset))
            }
            _ => Ok(None),
        }
    }

    /// Whether `changeset`, which the snapshot holds, is the head or an ancestor of it, over
    /// every parent ([`ancestry::reaches`]).
    fn reaches_head(&mut self, changeset: ChangesetId) -> Result<bool, ExportError> {
        if changeset == self.head {
            return Ok(true);
        }
        let generation = self.node(changeset)?.generation;
        Ok(ancestry::reaches(
            self.repo,
            [self.head],
            changeset,
            generation,
        )?)
    }

    /// The first parent of commit `commit` in the repository; `None` for a root commit, and for
    /// an object the repository lacks or that is no commit.
    fn first_parent_in_repository(&self, commit: GitOid) -> Result<Option<GitOid>, ExportError> {
        let (kind, bytes) = match self.sink.read(commit) {
            Ok(object) => object,
            Err(SinkError::Missing { .. }) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if kind != RawKind::Commit {
            return Ok(None);
        }
        let parsed = RawCommit::parse(&bytes).ok();
        Ok(parsed.and_then(|parsed| parsed.parents.first().copied()))
    }

    /// Step 5: the commits to write, parents first: from the head over every parent, highest
    /// generation first, `base` and its ancestors left out; a commit the repository holds in a
    /// pack is trusted with everything it reaches, and its parents are not walked from it.
    fn commits(&mut self, base: Option<ChangesetId>) -> Result<Vec<Commit>, ExportError> {
        let mut walk = Walk::default();
        self.mark(&mut walk, self.head, Mark::In)?;
        if let Some(base) = base {
            self.mark(&mut walk, base, Mark::Out)?;
        }
        let mut commits = Vec::new();
        // Every changeset is taken after its children, as a parent's generation is lower than
        // any child's: a mark is final when it is taken.
        while walk.wanted > 0 {
            let (_, id) = walk.heap.pop().expect("a changeset marked in is waiting");
            let mark = walk.marks[&id];
            let parents = self.nodes[&id].parents.clone();
            if mark == Mark::Out {
                for parent in parents {
                    self.mark(&mut walk, parent, Mark::Out)?;
                }
                continue;
            }
            walk.wanted -= 1;
            self.current = id;
            let exported = self.repo.derived::<ExportOid>(id, &())?;
            let stored = self.sink.stored(exported.git_oid())?;
            if stored == Stored::Packed {
                // Trust the pack for this commit and its whole history only once the commit is the
                // one the changeset renders to over its real oids: a crafted git_oid memo naming a
                // packed commit does not have the branch move onto it, nor its ancestors go
                // unwritten. The render also checks the trees it reaches ([`Run::rendered_commit`]
                // through [`Run::tree_git_oid`]).
                let commit = Commit {
                    id,
                    exported,
                    stored,
                };
                self.rendered_commit(&commit, &HashMap::new())?;
                self.seen.insert(exported.git_oid());
                continue;
            }
            commits.push(Commit {
                id,
                exported,
                stored,
            });
            for parent in parents {
                self.mark(&mut walk, parent, Mark::In)?;
            }
        }
        commits.reverse();
        Ok(commits)
    }

    /// Marks changeset `id` `mark` in `walk`: a changeset met for the first time waits for its
    /// turn, and one marked in already is marked out if `mark` is.
    fn mark(&mut self, walk: &mut Walk, id: ChangesetId, mark: Mark) -> Result<(), ExportError> {
        match walk.marks.get(&id) {
            None => {
                let generation = self.node(id)?.generation;
                walk.marks.insert(id, mark);
                walk.heap.push((generation, id));
                if mark == Mark::In {
                    walk.wanted += 1;
                }
            }
            Some(Mark::In) if mark == Mark::Out => {
                walk.marks.insert(id, Mark::Out);
                walk.wanted -= 1;
            }
            Some(_) => {}
        }
        Ok(())
    }

    /// Step 6: each commit of `commits`, parents first, with the trees and blobs its root holds
    /// that its first parent's root does not.
    fn write(&mut self, commits: &[Commit]) -> Result<(), ExportError> {
        let exported: HashMap<ChangesetId, ExportedOid> = (commits.iter())
            .map(|commit| (commit.id, commit.exported))
            .collect();
        for commit in commits {
            self.current = commit.id;
            let node = &self.nodes[&commit.id];
            let (root, first) = (node.root, node.parents.first().copied());
            let against = match first {
                Some(parent) => Some(self.node(parent)?.root),
                None => None,
            };
            self.trees(root, against)?;
            self.commit(commit, &exported)?;
        }
        Ok(())
    }

    /// The trees and blobs tree `root` reaches that tree `against` does not hold at the same path
    /// (an entry the same in both is not descended; `None`: all of them), each once: a packed tree
    /// is not descended, an absent one written, a loose one read back.
    fn trees(&mut self, root: TreeId, against: Option<TreeId>) -> Result<(), ExportError> {
        if against == Some(root) {
            return Ok(());
        }
        let mut stack = vec![(root, against)];
        while let Some((tree, against)) = stack.pop() {
            let oid = self.tree_git_oid(tree)?;
            if !self.seen.insert(oid) {
                continue;
            }
            let stored = self.sink.stored(oid)?;
            if stored == Stored::Packed {
                continue;
            }
            let held = self.read.tree(tree)?;
            if stored == Stored::Loose {
                self.check(RawKind::Tree, oid, |run| run.rendered_tree(&held))?;
            } else {
                let bytes = self.rendered_tree(&held)?;
                self.put(RawKind::Tree, &bytes, oid)?;
            }
            let theirs = match against {
                Some(against) => Some(self.read.tree(against)?),
                None => None,
            };
            for entry in held.entries() {
                let other = (theirs.as_ref())
                    .and_then(|theirs| entry_named(theirs, entry.name.as_bytes()))
                    .map(|other| other.target);
                if other == Some(entry.target) {
                    continue;
                }
                match entry.target {
                    EntryTarget::Dir(sub) => {
                        let sub_against = match other {
                            Some(EntryTarget::Dir(theirs)) => Some(theirs),
                            _ => None,
                        };
                        stack.push((sub, sub_against));
                    }
                    EntryTarget::Leaf(_, blob) => self.blob(blob)?,
                }
            }
        }
        Ok(())
    }

    /// Blob `blob`, once: packed, trusted; absent, written; loose, read back. Its oid is the hash
    /// of the store's own bytes ([`Run::blob_git_oid`]), so what the destination holds at that oid
    /// is the store's content, and a packed one is trusted as git trusts it.
    fn blob(&mut self, blob: BlobId) -> Result<(), ExportError> {
        let oid = self.blob_git_oid(blob)?;
        if !self.seen.insert(oid) {
            return Ok(());
        }
        match self.sink.stored(oid)? {
            Stored::Packed => {}
            Stored::Loose => {
                self.check(RawKind::Blob, oid, |run| Ok(run.read.blob(blob)?))?;
            }
            Stored::Absent => {
                let bytes = self.read.blob(blob)?;
                self.put(RawKind::Blob, &bytes, oid)?;
            }
        }
        Ok(())
    }

    /// `commit`'s own object: rendered over real oids and verified against its memoized export oid
    /// first ([`Run::rendered_commit`], so a crafted `git_oid` memo is refused before the branch
    /// could move onto its commit), then absent, written; loose, read back. `exported` holds the
    /// export oids of the commits of step 5.
    fn commit(
        &mut self,
        commit: &Commit,
        exported: &HashMap<ChangesetId, ExportedOid>,
    ) -> Result<(), ExportError> {
        let bytes = self.rendered_commit(commit, exported)?;
        let oid = commit.exported.git_oid();
        self.seen.insert(oid);
        let written = if commit.stored == Stored::Loose {
            self.check(RawKind::Commit, oid, |_| Ok(bytes.clone()))?
        } else {
            self.put(RawKind::Commit, &bytes, oid)?;
            true
        };
        if written {
            self.commits_written += 1;
        }
        Ok(())
    }

    /// The bytes of `tree` as git holds it ([`tree_bytes`]), each entry's oid the real git oid of
    /// its object, from the store's own bytes ([`Run::tree_git_oid`], [`Run::blob_git_oid`]), so a
    /// tree the export writes never names a memo a store file could have placed.
    fn rendered_tree(&self, tree: &Tree) -> Result<Vec<u8>, ExportError> {
        let mut oids = Vec::with_capacity(tree.entries().len());
        for entry in tree.entries() {
            oids.push(match entry.target {
                EntryTarget::Leaf(_, blob) => self.blob_git_oid(blob)?,
                EntryTarget::Dir(dir) => self.tree_git_oid(dir)?,
            });
        }
        Ok(tree_bytes(tree, |entry| {
            let at = (tree.entries())
                .binary_search_by(|other| other.name.as_bytes().cmp(entry.name.as_bytes()))
                .expect("tree_bytes gives the tree's own entries");
            oids[at]
        }))
    }

    /// The bytes of `commit`'s git commit ([`render_commit`]) over its parents' export oids and
    /// its root's real tree oid ([`Run::tree_git_oid`]), which must hash to the export oid the
    /// derived store memoized for the changeset: storage spec §7 (5)'s check. A memo that is
    /// another oid (a store file someone placed, a cache a faulty build wrote) is
    /// [`RepoError::DerivedCorrupt`] of `git_oid`, so the branch never moves to a commit the
    /// export did not render; `verify --deep` names it. The oid is the changeset's own, not the
    /// cached value, so no crafted `git_oid` memo stands in for it.
    fn rendered_commit(
        &self,
        commit: &Commit,
        exported: &HashMap<ChangesetId, ExportedOid>,
    ) -> Result<Vec<u8>, ExportError> {
        let stored = (self.read.changeset(commit.id)?)
            .ok_or_else(|| RepoError::NotFound(commit.id.to_string()))?;
        let cs = stored.changeset;
        let mut parents = Vec::with_capacity(cs.parents.len());
        for parent in &cs.parents {
            let value = match exported.get(parent) {
                Some(&value) => value,
                None => self.repo.derived::<ExportOid>(*parent, &())?,
            };
            parents.push(value.git_oid());
        }
        let tree = self.tree_git_oid(cs.root)?;
        let bytes = render_commit(commit.id, &cs, &parents, tree);
        let rendered = git_hash(RawKind::Commit, &bytes);
        let expected = commit.exported.git_oid();
        if rendered != expected {
            return Err(RepoError::DerivedCorrupt {
                name: ExportOid::NAME.to_owned(),
                changeset: commit.id,
                why: format!(
                    "the changeset renders to git commit {rendered}, but its memoized export oid \
                     is {expected}"
                ),
            }
            .into());
        }
        Ok(bytes)
    }

    /// The loose object `oid` of `kind`, read back: whole, it is made durable as it is; damaged
    /// (or gone since), it is written again from `bytes`, with a warning. Whether it was written.
    fn check(
        &mut self,
        kind: RawKind,
        oid: GitOid,
        bytes: impl FnOnce(&Self) -> Result<Vec<u8>, ExportError>,
    ) -> Result<bool, ExportError> {
        self.objects_checked += 1;
        match self.sink.read(oid) {
            Ok(_) => {
                self.pending.push(oid);
                Ok(false)
            }
            Err(SinkError::Corrupt { reason, .. }) => {
                self.warnings.push(format!(
                    "the loose {kind} {oid} was damaged ({reason}), and is written again"
                ));
                let bytes = bytes(self)?;
                self.put(kind, &bytes, oid)?;
                Ok(true)
            }
            Err(SinkError::Missing { .. }) => {
                let bytes = bytes(self)?;
                self.put(kind, &bytes, oid)?;
                Ok(true)
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Writes `bytes` as an object of `kind`, which must be the object `oid`.
    fn put(&mut self, kind: RawKind, bytes: &[u8], oid: GitOid) -> Result<(), ExportError> {
        let written = self.sink.write_object(kind, bytes)?;
        if written != oid {
            return Err(ExportError::Bug {
                reason: format!("the {kind} {oid} was written as {written}"),
            });
        }
        self.objects_written += 1;
        self.pending.push(oid);
        Ok(())
    }

    /// Step 8: the oid of each native commit of `commits` recorded in the external id index,
    /// faithful, naming its changeset. An entry there already for the same commit naming another
    /// changeset is [`ExportError::Bug`].
    fn record(&mut self, commits: &[Commit]) -> Result<(), ExportError> {
        let entries: Vec<(Vec<u8>, ExternalEntry)> = (commits.iter())
            .filter(|commit| !self.nodes[&commit.id].imported)
            .map(|commit| {
                let entry = ExternalEntry {
                    changeset: commit.id,
                    faithful: true,
                };
                (commit.exported.oid.to_vec(), entry)
            })
            .collect();
        if entries.is_empty() {
            return Ok(());
        }
        let put = self.repo.record_external_ids(SYSTEM_GIT, &entries)?;
        for key in put.conflicts {
            let Some((_, ours)) = entries.iter().find(|(ours, _)| *ours == key) else {
                continue;
            };
            let commit = <[u8; 20]>::try_from(key.as_slice()).map_or(GitOid::NULL, GitOid);
            match self.repo.external_id(SYSTEM_GIT, &key) {
                Ok(Some(there)) if there.changeset != ours.changeset => {
                    return Err(ExportError::Bug {
                        reason: format!(
                            "the external id index names changeset {} for git commit {commit}, \
                             which changeset {} exports to",
                            there.changeset, ours.changeset
                        ),
                    });
                }
                Ok(_) | Err(RepoError::Store(StoreError::ExternalIdCorrupt { .. })) => {
                    self.warnings.push(format!(
                        "the external id index holds another entry for git commit {commit} than \
                         its changeset {}: the entry is left as it is",
                        ours.changeset
                    ));
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }
}

/// The walk of step 5 ([`Run::commits`]): each changeset met and its mark, the changesets waiting
/// for their turn by generation, and how many of those are marked in.
#[derive(Default)]
struct Walk {
    marks: HashMap<ChangesetId, Mark>,
    heap: BinaryHeap<(u64, ChangesetId)>,
    wanted: usize,
}

/// The entry named `name` of `tree`, whose entries are sorted by their names' bytes.
fn entry_named<'t>(tree: &'t Tree, name: &[u8]) -> Option<&'t TreeEntry> {
    let entries = tree.entries();
    let at = entries.binary_search_by(|entry| entry.name.as_bytes().cmp(name));
    at.ok().map(|at| &entries[at])
}
