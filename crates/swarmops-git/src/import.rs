//! The git import (storage spec §7, umbrella §12): every commit a git ref reaches that the store
//! does not hold becomes a changeset, parents first, many to a journal frame; then a target ref
//! moves to the changeset of the ref's commit. Run again, it takes only what is new.
//!
//! [`import()`] goes in six steps.
//!
//! 1. The source ref is resolved to a commit, the tip ([`GitSource::resolve`]), and the target
//!    ref read. A target that exists has a base: the git commit its head changeset holds in its
//!    `git.oid` metadata, or for a native head, which holds none, the commit it exports to
//!    ([`ExportOid`](crate::ExportOid), M8 decision 7), so that commits made in git on top of an
//!    export come back on the changeset they were made on.
//! 2. The walk: from the tip over parents, first parents first, on a stack of its own, never by
//!    recursion. A commit is known when it is the base, which stands for the target's head, or
//!    when the external id index names a changeset for it that the store holds and that stands
//!    for it, holding it as its `git.oid` or exporting to it; the walk stops there. Any other
//!    entry (damaged, naming a changeset the store lacks, or one that stands for another commit)
//!    is a miss and a warning: its commit is converted again, so that no store file decides
//!    which changeset a commit is. The commits it does not know come out parents first; how many
//!    known ones it stopped at is [`ImportReport::skipped`].
//! 3. The base must be the tip, and then nothing is written, or an ancestor of it: met by the
//!    walk, or reached from a known commit it stopped at by a walk down the store's parents,
//!    pruned to the generations above the base's (a base reached only through a merge's second
//!    parent is no divergence). A head that holds a `git.oid` but exports to another commit
//!    (one imported with a loss, or above such a one: [`ExportedOid::kept`] is false) stands for
//!    that commit too, which a tip made on its export descends from: when the tip does not lead
//!    from the `git.oid` and the walk met that commit, the walk is made again with it as a second
//!    base, and the one the tip descends from is the base. Otherwise [`ImportError::Diverged`],
//!    naming the head's `git.oid`, else the commit a native head exports to, and the first entry
//!    the walk passed over whose changeset stands for another commit, and nothing is written.
//! 4. A base whose tree the head's root holds without loss (a `git.oid` without `git.lossy`, or
//!    the head's export, which is its root rendered) seeds the conversion: the head's root and
//!    the base commit's git tree are walked together, entries matched by name and kind, and a
//!    pair taken only where the store's own bytes render to the git object's oid (no memo is
//!    read), so that the trees and blobs the new commits share with the base are neither read
//!    nor converted again, and no git object is taken for a store object of other bytes. A base
//!    the walk did not meet, only reached, may be a commit the source lacks (one the tip
//!    descends from through the head's export instead): then nothing is seeded.
//! 5. The frames: each run of [`ImportRequest::frame_changesets`] commits is converted, its new
//!    trees and blobs stored ([`Repo::put_objects`]) whenever
//!    [`ImportRequest::object_batch_bytes`] of them accumulate and at its end, then recorded as
//!    one journal frame ([`Repo::raw_batch`]; a parent is a changeset of this run, the base or
//!    the index's), then its commits' external ids are recorded ([`Repo::record_external_ids`]),
//!    after the frame is durable (storage spec §2 rule 4, W8). A compaction runs at most once a
//!    frame, so about ⌈m/N⌉ times for m commits in frames of N.
//! 6. The metadata is flushed ([`Repo::flush_metadata`]: the checkpoint moves to the journal's
//!    end, so that no reader parses the import's frames again), and the target moved from its
//!    head to the tip's changeset by compare-and-swap ([`Repo::cas_ref`]). A swap that finds the
//!    target at the tip's changeset already succeeds without moving it; one that finds anything
//!    else is [`ImportError::CasMismatch`]. A move is then flushed too, so that its frame does
//!    not stay past the checkpoint either; that flush failing is a warning.
//!
//! An objects-only import ([`ImportRequest::objects_only`], a fetch's) has no target: step 1
//! reads no ref, so the walk knows a commit by its external id alone, step 3 is skipped and so is
//! step 4's seed, and step 6 flushes the metadata but moves no ref. Its report's `ref_moved` is
//! false, its `old_head` `None`.
//!
//! A store failure stops the import (M3 plan decision 6): [`StoreError::Busy`] is retried three
//! times, and any other failure, or a frame whose commit reports a failure after its commit
//! point, ends it after the last frame that committed ([`ImportError::Store`],
//! [`ImportError::PostCommit`]), saying how far it got. It never writes again after such a
//! failure: storage spec §2 rule 5's fail-stop assumption goes unchecked until M6. The same
//! import run again continues after the frames that committed. A commit the conversion refuses
//! ([`ImportError::Convert`]) ends the import before the frame that would hold it.
//!
//! The export oids of steps 1 to 3 are derived from the store's own objects, as the export
//! derives them: the import asks for one only where the target's head is native, or holds a git
//! commit the tip does not lead from, or the walk meets an entry whose changeset does not hold
//! the entry's commit, so a plain incremental import derives none. Deriving one
//! walks the whole history below the head, each changeset once, parents first, on a stack of its
//! own, and takes no `git_oid` memo of the derived store as a value: a memo it meets that
//! disagrees with what it derives is [`RepoError::DerivedCorrupt`] ([`ImportError::Store`]),
//! before anything is written, and one that is missing is written. The target, those export oids
//! and the trees and blobs that seed the conversion are read in one snapshot of the store.
//!
//! [`ExportedOid::kept`]: crate::ExportedOid::kept
//! [`Repo::put_objects`]: swarmops_repo::Repo::put_objects
//! [`Repo::raw_batch`]: swarmops_repo::Repo::raw_batch
//! [`Repo::record_external_ids`]: swarmops_repo::Repo::record_external_ids
//! [`Repo::flush_metadata`]: swarmops_repo::Repo::flush_metadata
//! [`Repo::cas_ref`]: swarmops_repo::Repo::cas_ref

use std::collections::{HashMap, HashSet};

use swarmops_core::bytes;
use swarmops_core::{
    BlobId, ChangesetId, EntryTarget, LeafKind, ObjectKind, RepoPath, TreeEntry, TreeId,
};
use swarmops_repo::{
    CasResult, LocalRepo, MAX_RAW_BATCH, RawChangeset, RepoError, RepoRead, StoreError,
};
use swarmops_store::{ExternalEntry, FsObjectStore, Recovered, SYSTEM_GIT};

use crate::export_oid::{ExportOids, OnMemo};
use crate::{
    ConvertError, Converted, Converter, ExportReader, GIT_LOSSY, GIT_OID, GitOid, GitSource,
    LossySet, ObjectFormat, OidCache, RawCommit, RawError, RawKind, RawTree, SourceError,
    Unsupported,
};
use crate::{ancestry, export_oid, source};

/// How many changesets a frame of the import holds unless asked otherwise: issue 6 Stage 1's
/// N, past the index's compaction threshold, so that a compaction runs once a frame at most.
pub const DEFAULT_FRAME_CHANGESETS: usize = 8_192;

/// How many bytes of new trees and blobs the import gathers before it stores them, unless asked
/// otherwise: 64 MiB.
pub const DEFAULT_OBJECT_BATCH_BYTES: usize = 64 << 20;

/// How many lossy commits an [`ImportReport`] lists ([`ImportReport::lossy`]); it counts them
/// all ([`ImportReport::lossy_total`]).
pub const MAX_LOSSY_LISTED: usize = 100;

/// What to import, and into which ref ([`import()`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportRequest {
    /// The source ref: `HEAD`, a ref's name, or 40 hex digits ([`GitSource::resolve`]).
    pub git_ref: String,
    /// The target ref, `main` or `release/<x>`: created at the tip's changeset, or moved there
    /// from the changeset of an earlier import.
    pub target_ref: String,
    /// The classes of content the import may drop or normalize; it refuses the others
    /// ([`ConvertError::Lossy`]).
    pub lossy: LossySet,
    /// Who moves the target ref, as the reflog records it.
    pub actor: String,
    /// How many changesets a journal frame holds: 1 to 65,535
    /// ([`swarmops_repo::MAX_RAW_BATCH`]), a number outside taken as the nearer bound.
    pub frame_changesets: usize,
    /// How many bytes of new trees and blobs the import gathers before it stores them.
    pub object_batch_bytes: usize,
    /// Whether a target ref is read and moved: set by the constructor alone.
    mode: Mode,
}

/// What an import does with refs ([`ImportRequest::new`], [`ImportRequest::objects_only`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// The target ref is read for divergence and moved to the tip's changeset.
    IntoRef,
    /// No ref is read or moved: the commits become changesets with their external ids only.
    ObjectsOnly,
}

impl ImportRequest {
    /// A request to import `git_ref` into `target_ref` for `actor`, allowing no lossy class,
    /// [`DEFAULT_FRAME_CHANGESETS`] to a frame, objects stored every
    /// [`DEFAULT_OBJECT_BATCH_BYTES`].
    pub fn new(
        git_ref: impl Into<String>,
        target_ref: impl Into<String>,
        actor: impl Into<String>,
    ) -> ImportRequest {
        ImportRequest {
            git_ref: git_ref.into(),
            target_ref: target_ref.into(),
            lossy: LossySet::empty(),
            actor: actor.into(),
            frame_changesets: DEFAULT_FRAME_CHANGESETS,
            object_batch_bytes: DEFAULT_OBJECT_BATCH_BYTES,
            mode: Mode::IntoRef,
        }
    }

    /// A request to import what `git_ref` reaches without reading or moving any ref: no target,
    /// no agent (`target_ref` and `actor` empty, and unused), no lossy class allowed, the frames
    /// and batches of [`ImportRequest::new`]. A fetch imports a remote branch so: the remote's
    /// state is the mirror's ref, and the store needs none.
    pub fn objects_only(git_ref: impl Into<String>) -> ImportRequest {
        ImportRequest {
            mode: Mode::ObjectsOnly,
            ..ImportRequest::new(git_ref, "", "")
        }
    }

    /// Whether a target ref is read and moved.
    fn moves_ref(&self) -> bool {
        self.mode == Mode::IntoRef
    }
}

/// What an import did ([`import()`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportReport {
    /// The commits it recorded: each one the walk did not know, its changeset made (or found in
    /// the store already, as after a crash before its external id was written) and its external
    /// id written.
    pub imported: u64,
    /// The known commits the walk stopped at: the base, and commits the external id index
    /// names.
    pub skipped: u64,
    /// The changeset of the tip, which the target names.
    pub head: ChangesetId,
    /// The tip: the commit the source ref names.
    pub git_head: GitOid,
    /// The target ref.
    pub target_ref: String,
    /// What the target named before; `None` if it did not exist, or for an objects-only import.
    pub old_head: Option<ChangesetId>,
    /// Whether this import moved the target (or created it); `false` when it named the tip's
    /// changeset already, and for an objects-only import, which moves no ref.
    pub ref_moved: bool,
    /// The first [`MAX_LOSSY_LISTED`] commits recorded with a loss, in the order recorded.
    pub lossy: Vec<LossyCommit>,
    /// How many commits were recorded with a loss.
    pub lossy_total: u64,
    /// What went wrong without stopping the import, one line each: an external id entry found
    /// damaged, naming a changeset the store lacks or one that stands for another commit, one
    /// the index kept with other bytes, a step after the target's move that failed.
    pub warnings: Vec<String>,
    /// What the import's write transactions repaired before their own work, of what earlier
    /// writers left ([`swarmops_store::MetaWrite::recovered`]), added up.
    pub recovered: Recovered,
}

impl ImportReport {
    /// A report of every field, in their order.
    #[expect(
        clippy::too_many_arguments,
        reason = "one argument per field: a report has no field to default"
    )]
    pub fn new(
        imported: u64,
        skipped: u64,
        head: ChangesetId,
        git_head: GitOid,
        target_ref: String,
        old_head: Option<ChangesetId>,
        ref_moved: bool,
        lossy: Vec<LossyCommit>,
        lossy_total: u64,
        warnings: Vec<String>,
        recovered: Recovered,
    ) -> ImportReport {
        ImportReport {
            imported,
            skipped,
            head,
            git_head,
            target_ref,
            old_head,
            ref_moved,
            lossy,
            lossy_total,
            warnings,
            recovered,
        }
    }
}

/// A commit an import recorded with a loss.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LossyCommit {
    /// The commit.
    pub commit: GitOid,
    /// Its changeset, whose `git.lossy` metadata names the classes.
    pub changeset: ChangesetId,
    /// The classes of what it lost.
    pub classes: LossySet,
}

impl LossyCommit {
    /// `commit`, recorded as `changeset` with the loss of `classes`.
    pub fn new(commit: GitOid, changeset: ChangesetId, classes: LossySet) -> LossyCommit {
        LossyCommit {
            commit,
            changeset,
            classes,
        }
    }
}

/// Why an import failed. Each says how far it got where it wrote anything.
#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    /// The source could not open, resolve or read what the import needs: the ref names nothing,
    /// the repository is one the import does not read, an object is missing or damaged.
    #[error(transparent)]
    Source(#[from] SourceError),
    /// The conversion refused a commit: content of a class the request does not allow, or
    /// content no class covers. The frames before the one that would hold it are recorded.
    #[error(transparent)]
    Convert(#[from] ConvertError),
    /// The target is no ref name: refs are `main` and `release/<x>`. Nothing was read or
    /// written.
    #[error("invalid target ref name {0:?}: a ref is main or release/<name>")]
    RefName(String),
    /// The target does not lead to the tip: the git commit its head stands for, the one it holds
    /// or for a native head the one it exports to, is not an ancestor of the tip (the source was
    /// rewritten, or is behind, or a native changeset nobody exported is on the target), nor is
    /// the head's export, where it exports to another commit than the one it holds. Nothing was
    /// written.
    #[error(
        "{target_ref} has diverged from the git history: its head {target_head} {}, which the \
         tip {git_head} does not reach{}",
        holding(*.base),
        passed_over(.external_id.as_deref())
    )]
    Diverged {
        /// The target ref.
        target_ref: String,
        /// The changeset it names.
        target_head: ChangesetId,
        /// The git commit the import tried first: the one that changeset holds (`git.oid`), or
        /// for a native changeset the one it exports to ([`ExportOid`](crate::ExportOid)).
        /// [`import()`] always names one since a native head goes on from its export (M8
        /// decision 7); `None` would say the head stands for no git commit.
        base: Option<GitOid>,
        /// The tip.
        git_head: GitOid,
        /// The first external id entry the walk passed over because its changeset stands for
        /// another commit, if any: a file someone placed in the store, which can be what the
        /// divergence comes from (`swarmops verify --deep` reports each such entry). Boxed, so
        /// that it does not make every result of the import larger.
        external_id: Option<Box<ExternalIdMismatch>>,
    },
    /// The target moved while the import ran: the compare-and-swap found it naming another
    /// changeset. Every changeset and external id is recorded; the same import run again moves
    /// the target if the move leaves it a descendant.
    #[error(
        "{target_ref} moved during the import: it names {}",
        .actual.map_or_else(|| "nothing".to_owned(), |id| id.to_string())
    )]
    CasMismatch {
        /// The target ref.
        target_ref: String,
        /// What it names; `None` if it does not exist.
        actual: Option<ChangesetId>,
    },
    /// The store failed, and the import stopped: the frames before the failure are recorded,
    /// and the same import run again continues after them.
    #[error(
        "the store failed after {imported} commits were recorded{}: {source}",
        .last.map_or_else(String::new, |last| format!(", the last {last}"))
    )]
    Store {
        /// The failure, boxed: a [`RepoError`] beside the import's progress would make every
        /// result of the import as large as both.
        source: Box<RepoError>,
        /// How many commits the frames that committed recorded.
        imported: u64,
        /// The last commit of the last frame that committed; `None` if none did.
        last: Option<GitOid>,
    },
    /// A frame committed, but a step after its commit point failed (its warnings say which,
    /// and the frame), and the import stopped after it, before its external ids: it never
    /// writes again after a failed sync. The next write repairs the store, and the same import
    /// run again continues.
    #[error(
        "the import stopped after {imported} commits were recorded{}: {}",
        .last.map_or_else(String::new, |last| format!(", the last {last}")),
        .warnings.join("; ")
    )]
    PostCommit {
        /// The steps that failed, one line each, each naming the frame.
        warnings: Vec<String>,
        /// How many commits the frames that committed recorded, that frame's included.
        imported: u64,
        /// The last commit of that frame.
        last: Option<GitOid>,
    },
}

/// What [`ImportError::Diverged`] says the target's head stands for: the git commit it holds, or
/// the one a native head exports to.
fn holding(base: Option<GitOid>) -> String {
    base.map_or_else(
        || "holds no git commit".to_owned(),
        |base| format!("stands for git commit {base}"),
    )
}

/// What [`ImportError::Diverged`] says of the external id entry the walk passed over, if any.
fn passed_over(entry: Option<&ExternalIdMismatch>) -> String {
    entry.map_or_else(String::new, |entry| {
        format!(
            "; the external id entry of git commit {} names changeset {}, which stands for \
             another commit, and was passed over",
            entry.commit, entry.changeset
        )
    })
}

/// An external id entry of system `git` whose changeset stands for another commit than the
/// entry's: it neither holds the commit as its `git.oid` nor exports to it. No import or export
/// writes one; the import passes over one it meets ([`ImportError::Diverged::external_id`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExternalIdMismatch {
    /// The commit whose entry it is: the entry's key.
    pub commit: GitOid,
    /// The changeset the entry names.
    pub changeset: ChangesetId,
}

impl ExternalIdMismatch {
    /// The entry of `commit` that names `changeset`.
    pub fn new(commit: GitOid, changeset: ChangesetId) -> ExternalIdMismatch {
        ExternalIdMismatch { commit, changeset }
    }
}

/// How many times a store call that found the writer lock held for its whole wait
/// ([`StoreError::Busy`]) is made again.
const BUSY_RETRIES: usize = 3;

/// The mode git writes for a directory.
const DIR_MODE: &[u8] = b"40000";

/// Imports into `repo` every commit `source`'s `req.git_ref` reaches that `repo` does not hold,
/// and moves `req.target_ref` to the tip's changeset, unless the request is objects-only: see
/// [the module documentation](self) for the steps and their errors.
///
/// Where the target's head is native, or holds a git commit the tip does not lead from, the
/// import derives the commit the head exports to from the store's objects itself, whether or not
/// `repo` has [`ExportOid`](crate::ExportOid) registered: a `git_oid` memo that disagrees with what
/// it derives fails the import there with [`RepoError::DerivedCorrupt`] ([`ImportError::Store`])
/// before it writes anything.
pub fn import(
    repo: &LocalRepo,
    source: &dyn GitSource,
    req: &ImportRequest,
) -> Result<ImportReport, ImportError> {
    if req.moves_ref() && swarmops_store::check_ref_name(&req.target_ref).is_err() {
        return Err(ImportError::RefName(req.target_ref.clone()));
    }
    let format = source.object_format();
    if format != ObjectFormat::Sha1 {
        let format = Unsupported::ObjectFormat(format.as_str().to_owned());
        return Err(SourceError::Unsupported(format).into());
    }
    let tip = source.resolve(&req.git_ref)?;
    let mut oids = Oids::new(repo)?;
    let mut base = Base::of(&mut oids, req)?;
    let mut walk = Walk::from_tip(repo, &mut oids, source, tip, base.as_ref())?;
    let old_head = base.as_ref().map(|base| base.head);
    // The commit that seeds the conversion, and whether the walk met it.
    let mut seeds = None;
    if let Some(base) = &mut base {
        let mut leads = base.leads(repo, tip, &walk)?;
        if leads.is_none() && base.add_export(&mut oids, &walk)? {
            // The tip may descend from the head's export, which the walk met: the walk again,
            // knowing it.
            walk = Walk::from_tip(repo, &mut oids, source, tip, Some(base))?;
            leads = base.leads(repo, tip, &walk)?;
        }
        match leads {
            None => {
                return Err(ImportError::Diverged {
                    target_ref: req.target_ref.clone(),
                    target_head: base.head,
                    base: Some(base.commits[0].oid),
                    git_head: tip,
                    external_id: walk.mismatched.map(Box::new),
                });
            }
            Some(Leads::Tip) => {
                return Ok(ImportReport {
                    imported: 0,
                    skipped: bytes::to_u64(walk.known.len()),
                    head: base.head,
                    git_head: tip,
                    target_ref: req.target_ref.clone(),
                    old_head,
                    ref_moved: false,
                    lossy: Vec::new(),
                    lossy_total: 0,
                    warnings: walk.warnings,
                    recovered: Recovered::default(),
                });
            }
            Some(Leads::Met(commit)) => seeds = commit.seeds.then_some((commit.oid, Meeting::Met)),
            Some(Leads::Reached) => {
                let first = base.commits[0];
                seeds = first.seeds.then_some((first.oid, Meeting::Unmet));
            }
        }
    }
    let mut run = Run::new(repo, source, req, walk.known);
    run.skipped = bytes::to_u64(run.ids.len());
    run.warnings = walk.warnings;
    run.flagged = walk.flagged;
    if let Some(base) = &base
        && let Some((commit, met)) = seeds
    {
        seed(&oids, source, &mut run.converter, base, commit, met)?;
    }
    // Nothing is read in the snapshot past here: the frames' own writes follow.
    drop(oids);
    for commits in walk
        .unknown
        .chunks(req.frame_changesets.clamp(1, MAX_RAW_BATCH))
    {
        run.frame(commits)?;
    }
    run.finish(tip, old_head)
}

/// The target's head when the import starts, and the git commits it stands for: what the import
/// goes on from.
struct Base {
    /// The changeset the target names.
    head: ChangesetId,
    /// Its generation: no changeset of a generation as low or lower has it as an ancestor.
    generation: u64,
    /// Its root.
    root: TreeId,
    /// Whether it holds a git commit (`git.oid`): an imported changeset, which may export to
    /// another commit than its own.
    imported: bool,
    /// The git commits it stands for, the first tried first: the one it holds, or for a native
    /// head the one it exports to; then, where the walk met it, the other commit an imported head
    /// exports to ([`Base::add_export`]).
    commits: Vec<BaseCommit>,
}

/// A git commit the target's head stands for.
#[derive(Clone, Copy)]
struct BaseCommit {
    /// The commit.
    oid: GitOid,
    /// Whether the head's root holds its tree without loss, so that it seeds the conversion: the
    /// commit an imported head holds unless the head has `git.lossy`, and the head's export,
    /// which is its root rendered.
    seeds: bool,
}

/// How the tip leads from the target's head (step 3 of [the module documentation](self)).
enum Leads {
    /// The tip is one of the commits the head stands for: there is nothing to import.
    Tip,
    /// The walk met this commit the head stands for: a parent of a commit it did not know.
    Met(BaseCommit),
    /// The walk met none of them, but stopped at a known commit whose changeset descends from the
    /// head in the store.
    Reached,
}

impl Base {
    /// The base of `req.target_ref` as `oids`' snapshot shows it; `None` when the target does not
    /// exist. For a native head, the commit it exports to, derived from the store's objects
    /// ([`Oids::export_of`]).
    fn of(oids: &mut Oids<'_>, req: &ImportRequest) -> Result<Option<Base>, ImportError> {
        // An objects-only import reads no ref: it has no base.
        if !req.moves_ref() {
            return Ok(None);
        }

        let at_start = |source| stopped(source, 0, None);
        let Some(state) = oids.read.get_ref(&req.target_ref).map_err(at_start)? else {
            return Ok(None);
        };
        let head = state.target;
        let Some(stored) = oids.read.changeset(head).map_err(at_start)? else {
            return Err(at_start(RepoError::NotFound(head.to_string())));
        };
        let metadata = &stored.changeset.metadata;
        let own = value(metadata, GIT_OID).and_then(GitOid::from_hex);
        let first = match own {
            Some(oid) => BaseCommit {
                oid,
                seeds: value(metadata, GIT_LOSSY).is_none(),
            },
            None => BaseCommit {
                oid: oids.export_of(head)?,
                seeds: true,
            },
        };
        Ok(Some(Base {
            head,
            generation: stored.generation,
            root: stored.changeset.root,
            imported: own.is_some(),
            commits: vec![first],
        }))
    }

    /// Whether `oid` is one of the git commits the head stands for.
    fn stands_for(&self, oid: GitOid) -> bool {
        self.commits.iter().any(|commit| commit.oid == oid)
    }

    /// How the tip leads from the head, as `walk` from `tip` found it; `None` when it does not.
    fn leads(
        &self,
        repo: &LocalRepo,
        tip: GitOid,
        walk: &Walk,
    ) -> Result<Option<Leads>, ImportError> {
        if self.stands_for(tip) {
            return Ok(Some(Leads::Tip));
        }
        let mut commits = self.commits.iter();
        if let Some(&met) = commits.find(|commit| walk.known.contains_key(&commit.oid)) {
            return Ok(Some(Leads::Met(met)));
        }
        let stops = walk.known.values().copied();
        let reached = ancestry::reaches(repo, stops, self.head, self.generation);
        let reached = reached.map_err(|source| stopped(source, 0, None))?;
        Ok(reached.then_some(Leads::Reached))
    }

    /// Adds the commit an imported head exports to, when that is another commit than the one it
    /// holds (it was imported with a loss, or above such a one) and `walk` met it: `true` then.
    /// A native head's export is its first commit already. Derives the head's export oid
    /// ([`Oids::export_of`]).
    fn add_export(&mut self, oids: &mut Oids<'_>, walk: &Walk) -> Result<bool, ImportError> {
        if !self.imported {
            return Ok(false);
        }
        let exported = oids.export_of(self.head)?;
        if self.stands_for(exported) || !walk.met(exported) {
            return Ok(false);
        }
        self.commits.push(BaseCommit {
            oid: exported,
            seeds: true,
        });
        Ok(true)
    }
}

/// What the store's own objects say of the target's head, read in one snapshot of the store: the
/// commit a changeset exports to, derived with every export oid below it ([`ExportOids`], which
/// checks each `git_oid` memo it meets against what it derives), and the git oids that the head's
/// trees and blobs render to, from their bytes alone ([`rendered_tree_oid`],
/// [`rendered_blob_oid`]): no memo of the derived store is taken for a value. Each object's git
/// oid is computed once ([`OidCache`]).
///
/// [`rendered_tree_oid`]: export_oid::rendered_tree_oid
/// [`rendered_blob_oid`]: export_oid::rendered_blob_oid
struct Oids<'r> {
    repo: &'r LocalRepo,
    /// The snapshot the target, its head's history and its objects are read in.
    read: RepoRead<'r, FsObjectStore>,
    cache: OidCache,
    exports: ExportOids,
}

impl<'r> Oids<'r> {
    /// A snapshot of `repo`, nothing derived yet.
    fn new(repo: &'r LocalRepo) -> Result<Oids<'r>, ImportError> {
        let read = repo.snapshot().map_err(|source| stopped(source, 0, None))?;
        Ok(Oids {
            repo,
            read,
            cache: OidCache::new(),
            exports: ExportOids::new(OnMemo::Check),
        })
    }

    /// The snapshot's trees and blobs, with the derived store whose memos are checked.
    fn reader(&self) -> ExportReader<'_, FsObjectStore> {
        ExportReader {
            read: &self.read,
            store: self.repo.derived_store(),
        }
    }

    /// The commit changeset `id` exports to, derived from the store's objects with every export
    /// oid below it, each once ([`ExportOids`]): never a memo, so that no store file someone placed
    /// decides what the head stands for. A memo met on the way (a changeset's `git_oid`, a
    /// rendered tree's or blob's git oid) that disagrees is [`RepoError::DerivedCorrupt`], and one
    /// that is missing is written.
    fn export_of(&mut self, id: ChangesetId) -> Result<GitOid, ImportError> {
        let reader = ExportReader {
            read: &self.read,
            store: self.repo.derived_store(),
        };
        let exported = self.exports.of(&self.read, &reader, &self.cache, id);
        Ok(exported
            .map_err(|source| stopped(source, 0, None))?
            .git_oid())
    }

    /// Whether changeset `id` exports to commit `oid` ([`Oids::export_of`]); `false` for a
    /// changeset the snapshot does not hold, one committed since it was taken.
    fn exports_to(&mut self, id: ChangesetId, oid: GitOid) -> Result<bool, ImportError> {
        let held = self.read.changeset(id);
        if held.map_err(|source| stopped(source, 0, None))?.is_none() {
            return Ok(false);
        }
        Ok(self.export_of(id)? == oid)
    }

    /// The git oid that tree `tree` renders to, from the store's own bytes.
    fn tree_oid(&self, tree: TreeId) -> Result<GitOid, ImportError> {
        let oid = export_oid::rendered_tree_oid(&self.reader(), &self.cache, tree);
        oid.map_err(|source| stopped(source, 0, None))
    }

    /// The git oid of blob `blob`, the hash of the store's own bytes.
    fn blob_oid(&self, blob: BlobId) -> Result<GitOid, ImportError> {
        let oid = export_oid::rendered_blob_oid(&self.reader(), &self.cache, blob);
        oid.map_err(|source| stopped(source, 0, None))
    }
}

/// The value of metadata key `key`.
fn value<'m>(metadata: &'m [(String, Vec<u8>)], key: &str) -> Option<&'m [u8]> {
    (metadata.iter())
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.as_slice())
}

/// Whether a changeset of `metadata` holds git commit `oid`: its `git.oid` is the commit's 40
/// lowercase hex digits, as the import records it.
fn holds(metadata: &[(String, Vec<u8>)], oid: GitOid) -> bool {
    value(metadata, GIT_OID) == Some(oid.to_string().as_bytes())
}

/// What the walk from the tip found (step 2 of [the module documentation](self)).
struct Walk {
    /// The commits it did not know, parents first.
    unknown: Vec<GitOid>,
    /// The known commits it stopped at, with their changesets.
    known: HashMap<GitOid, ChangesetId>,
    /// One line for each external id entry it found damaged, naming a changeset the store
    /// lacks, or naming one that stands for another commit.
    warnings: Vec<String>,
    /// The commits of those entries.
    flagged: HashSet<GitOid>,
    /// The first of them that names a changeset standing for another commit.
    mismatched: Option<ExternalIdMismatch>,
}

/// A commit on the walk's stack: the parents it has not looked at yet, first parents first.
struct Visit {
    oid: GitOid,
    parents: std::vec::IntoIter<GitOid>,
}

impl Walk {
    /// Walks from `tip` over parents, on a stack of its own, stopping at each known commit. An
    /// entry's changeset is checked against its commit in `oids`' snapshot.
    fn from_tip(
        repo: &LocalRepo,
        oids: &mut Oids<'_>,
        source: &dyn GitSource,
        tip: GitOid,
        base: Option<&Base>,
    ) -> Result<Walk, ImportError> {
        let mut walk = Walk {
            unknown: Vec::new(),
            known: HashMap::new(),
            warnings: Vec::new(),
            flagged: HashSet::new(),
            mismatched: None,
        };
        let mut seen = HashSet::from([tip]);
        let mut stack = Vec::new();
        walk.reach(repo, oids, source, base, tip, &mut stack)?;
        while let Some(top) = stack.last_mut() {
            if let Some(parent) = top.parents.next() {
                if seen.insert(parent) {
                    walk.reach(repo, oids, source, base, parent, &mut stack)?;
                }
            } else if let Some(done) = stack.pop() {
                // Every parent of it is known or out already: it comes out after them.
                walk.unknown.push(done.oid);
            }
        }
        Ok(walk)
    }

    /// Commit `oid`, met for the first time: a stop when it is known, and otherwise on the
    /// stack, its parents to be walked next.
    fn reach(
        &mut self,
        repo: &LocalRepo,
        oids: &mut Oids<'_>,
        source: &dyn GitSource,
        base: Option<&Base>,
        oid: GitOid,
        stack: &mut Vec<Visit>,
    ) -> Result<(), ImportError> {
        if let Some(changeset) = self.known_as(repo, oids, base, oid)? {
            self.known.insert(oid, changeset);
        } else {
            let parents = read_commit(source, oid)?.parents.into_iter();
            stack.push(Visit { oid, parents });
        }
        Ok(())
    }

    /// The changeset commit `oid` is known as: the base's head, for a commit it stands for, or the
    /// one its external id entry names, if the store holds it and it stands for the commit,
    /// holding it as its `git.oid` or exporting to it ([`Oids::exports_to`]). Any other entry
    /// (damaged, naming a changeset the store lacks, or one that stands for another commit) is a
    /// miss and a warning, whatever its file says.
    fn known_as(
        &mut self,
        repo: &LocalRepo,
        oids: &mut Oids<'_>,
        base: Option<&Base>,
        oid: GitOid,
    ) -> Result<Option<ChangesetId>, ImportError> {
        if let Some(base) = base
            && base.stands_for(oid)
        {
            return Ok(Some(base.head));
        }
        let entry = match repo.external_id(SYSTEM_GIT, &oid.0) {
            Ok(entry) => entry,
            Err(RepoError::Store(damaged @ StoreError::ExternalIdCorrupt { .. })) => {
                self.flag(oid, &damaged.to_string());
                return Ok(None);
            }
            Err(source) => return Err(stopped(source, 0, None)),
        };
        let Some(ExternalEntry { changeset, .. }) = entry else {
            return Ok(None);
        };
        // Read in the store as it is now, as the entry is: another import's entries name the
        // changesets it committed since the snapshot (W8).
        let held = repo.changeset(changeset);
        let Some(held) = held.map_err(|source| stopped(source, 0, None))? else {
            let why = format!("it names changeset {changeset}, which the store does not hold");
            self.flag(oid, &why);
            return Ok(None);
        };
        if holds(&held.changeset.metadata, oid) || oids.exports_to(changeset, oid)? {
            return Ok(Some(changeset));
        }
        let why = format!(
            "it names changeset {changeset}, which neither holds that commit nor exports to it"
        );
        self.flag(oid, &why);
        let mismatched = ExternalIdMismatch {
            commit: oid,
            changeset,
        };
        self.mismatched.get_or_insert(mismatched);
        Ok(None)
    }

    /// Whether the walk met commit `oid`: a commit it did not know, or a known one it stopped at.
    fn met(&self, oid: GitOid) -> bool {
        self.known.contains_key(&oid) || self.unknown.contains(&oid)
    }

    /// Warns that the external id entry of commit `oid` is no use, for the reason `why`.
    fn flag(&mut self, oid: GitOid, why: &str) {
        self.flagged.insert(oid);
        self.warnings.push(format!(
            "the external id entry of git commit {oid} is left as it is and the commit imported \
             again: {why}"
        ));
    }
}

/// The commit `oid`'s fields, read from `source` once its header lets it through the size gate
/// ([`read_bounded`]).
///
/// [`read_bounded`]: source::read_bounded
fn read_commit(source: &dyn GitSource, oid: GitOid) -> Result<RawCommit, ImportError> {
    let (kind, bytes) = source::read_bounded(source, oid)?;
    if kind != RawKind::Commit {
        let what = "named as a commit, but it is no commit";
        return Err(ConvertError::Malformed { oid, what }.into());
    }
    RawCommit::parse(&bytes).map_err(|error| malformed(oid, &error))
}

/// The object `oid`, whose bytes do not parse, as `error` says.
fn malformed(oid: GitOid, error: &RawError) -> ImportError {
    match *error {
        RawError::Malformed { what } => ConvertError::Malformed { oid, what }.into(),
    }
}

/// Whether the import's walk met the commit that seeds the conversion: one it did not meet
/// may be missing from the source.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Meeting {
    /// The walk met it.
    Met,
    /// The walk did not reach it.
    Unmet,
}

/// Step 4 of [the module documentation](self): the trees and blobs of `base`'s head, taken by
/// `converter` as converted already, by walking the git tree of `commit`, a commit the head
/// stands for whose tree its root holds without loss, and the root together, entries matched by
/// name and kind, each pair taken only where the store's own bytes render to the git object's oid
/// ([`Oids`]): whatever the store says of the head, no git object is taken for a store object of
/// other bytes. A commit the walk did not meet ([`Meeting::Unmet`]) that `source` lacks seeds
/// nothing.
fn seed(
    oids: &Oids<'_>,
    source: &dyn GitSource,
    converter: &mut Converter<'_>,
    base: &Base,
    commit: GitOid,
    met: Meeting,
) -> Result<(), ImportError> {
    let git_root = match read_commit(source, commit) {
        Ok(commit) => commit.tree,
        Err(ImportError::Source(SourceError::Missing { .. })) if met == Meeting::Unmet => {
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    if oids.tree_oid(base.root)? == git_root {
        converter.remember_tree(git_root, base.root);
    }
    // Each directory to walk: its path in the base's tree (`None` for the root) and its git tree.
    let mut dirs: Vec<(Option<RepoPath>, GitOid)> = vec![(None, git_root)];
    // The git trees walked or about to be: a tree held at several paths is walked once.
    let mut walked = HashSet::from([git_root]);
    while let Some((dir, git)) = dirs.pop() {
        let (kind, bytes) = source::read_bounded(source, git)?;
        if kind != RawKind::Tree {
            let what = "named as a tree, but it is no tree";
            return Err(ConvertError::Malformed { oid: git, what }.into());
        }
        let tree = RawTree::parse(&bytes).map_err(|error| malformed(git, &error))?;
        let listed = oids.read.list_tree(base.head, dir.as_ref());
        let Some(held) = listed.map_err(|source| stopped(source, 0, None))? else {
            continue;
        };
        for entry in tree.entries {
            let Some(target) = held_target(&held, &entry.name) else {
                continue;
            };
            match (entry.mode.as_slice(), target) {
                (b"100644", EntryTarget::Leaf(LeafKind::File, blob))
                | (b"100755", EntryTarget::Leaf(LeafKind::Exec, blob))
                | (b"120000", EntryTarget::Leaf(LeafKind::Symlink, blob)) => {
                    if oids.blob_oid(blob)? == entry.oid {
                        converter.remember_blob(entry.oid, blob);
                    }
                }
                (DIR_MODE, EntryTarget::Dir(tree)) => {
                    if oids.tree_oid(tree)? == entry.oid {
                        converter.remember_tree(entry.oid, tree);
                    }
                    if walked.insert(entry.oid)
                        && let Some(path) = child_path(dir.as_ref(), &entry.name)
                    {
                        dirs.push((Some(path), entry.oid));
                    }
                }
                // Not as the conversion would have made it: left for it to convert.
                _ => {}
            }
        }
    }
    Ok(())
}

/// What the entry named `name` of `entries`, a tree's in its order, names.
fn held_target(entries: &[TreeEntry], name: &[u8]) -> Option<EntryTarget> {
    let at = entries.binary_search_by(|entry| entry.name.as_bytes().cmp(name));
    at.ok().map(|at| entries[at].target)
}

/// The path of the entry `name` of directory `dir` (the root for `None`); `None` when it is no
/// path.
fn child_path(dir: Option<&RepoPath>, name: &[u8]) -> Option<RepoPath> {
    let name = std::str::from_utf8(name).ok()?;
    let path = match dir {
        Some(dir) => format!("{}/{name}", dir.as_str()),
        None => name.to_owned(),
    };
    RepoPath::new(path).ok()
}

/// An import as it runs, from the frames on (steps 5 and 6 of [the module
/// documentation](self)).
struct Run<'a, 's> {
    repo: &'a LocalRepo,
    req: &'a ImportRequest,
    converter: Converter<'s>,
    /// The changeset of each commit the import knows: the walk's known ones, then each one the
    /// frames record.
    ids: HashMap<GitOid, ChangesetId>,
    /// The commits whose external id entries the walk warned of.
    flagged: HashSet<GitOid>,
    skipped: u64,
    imported: u64,
    /// The last commit of the last frame that committed.
    last: Option<GitOid>,
    lossy: Vec<LossyCommit>,
    lossy_total: u64,
    warnings: Vec<String>,
    recovered: Recovered,
}

impl<'a, 's> Run<'a, 's> {
    fn new(
        repo: &'a LocalRepo,
        source: &'s dyn GitSource,
        req: &'a ImportRequest,
        known: HashMap<GitOid, ChangesetId>,
    ) -> Run<'a, 's> {
        Run {
            repo,
            req,
            converter: Converter::new(source, req.lossy),
            ids: known,
            flagged: HashSet::new(),
            skipped: 0,
            imported: 0,
            last: None,
            lossy: Vec::new(),
            lossy_total: 0,
            warnings: Vec::new(),
            recovered: Recovered::default(),
        }
    }

    /// The import's failure `source`, which stops it after the frames that committed.
    fn stopped(&self, source: RepoError) -> ImportError {
        stopped(source, self.imported, self.last)
    }

    /// One frame: `commits` converted, their objects stored, their changesets recorded in one
    /// journal frame, then their external ids.
    fn frame(&mut self, commits: &[GitOid]) -> Result<(), ImportError> {
        let converted = self.convert(commits)?;
        let repo = self.repo;
        let mut batch = repo.raw_batch();
        let mut raws = Vec::with_capacity(converted.len());
        let mut entries = Vec::with_capacity(converted.len());
        for commit in converted {
            let Converted {
                oid,
                parents,
                root,
                author,
                committer,
                message,
                extra_headers,
                lossy,
                metadata,
            } = commit;
            let raw = RawChangeset {
                parents: self.changesets(oid, &parents)?,
                root,
                author,
                committer,
                message,
                extra_headers,
                metadata,
            };
            let id = batch
                .push(raw.clone())
                .map_err(|source| self.stopped(source))?;
            self.ids.insert(oid, id);
            self.note_lossy(oid, id, lossy);
            let faithful = lossy.is_empty();
            let entry = ExternalEntry {
                changeset: id,
                faithful,
            };
            entries.push((oid.0.to_vec(), entry));
            raws.push(raw);
        }
        // A batch is gone once it commits: a retry records the same raws again.
        let mut batch = Some(batch);
        let outcome = retry_busy(|| match batch.take() {
            Some(batch) => batch.commit(),
            None => repo.commit_raw_batch(raws.clone()),
        })
        .map_err(|source| self.stopped(source))?;
        absorb(&mut self.recovered, outcome.recovered);
        self.imported += bytes::to_u64(commits.len());
        self.last = commits.last().copied();
        if !outcome.warnings.is_empty() {
            return Err(ImportError::PostCommit {
                warnings: outcome.warnings,
                imported: self.imported,
                last: self.last,
            });
        }
        let put = repo.record_external_ids(SYSTEM_GIT, &entries);
        let put = put.map_err(|source| self.stopped(source))?;
        for key in put.conflicts {
            self.conflict(&key);
        }
        Ok(())
    }

    /// `commits` converted, in order; their new trees and blobs stored whenever
    /// [`ImportRequest::object_batch_bytes`] of them gather, and at the end.
    fn convert(&mut self, commits: &[GitOid]) -> Result<Vec<Converted>, ImportError> {
        let mut converted = Vec::with_capacity(commits.len());
        let mut objects: Vec<(ObjectKind, Vec<u8>)> = Vec::new();
        let mut gathered = 0;
        for &oid in commits {
            let made = objects.len();
            converted.push(self.converter.commit(oid, &mut objects)?);
            gathered += objects[made..]
                .iter()
                .map(|(_, bytes)| bytes.len())
                .sum::<usize>();
            if gathered >= self.req.object_batch_bytes {
                self.put(std::mem::take(&mut objects))?;
                gathered = 0;
            }
        }
        self.put(objects)?;
        Ok(converted)
    }

    /// Stores `objects`, if any.
    fn put(&self, objects: Vec<(ObjectKind, Vec<u8>)>) -> Result<(), ImportError> {
        if objects.is_empty() {
            return Ok(());
        }
        let put = self.repo.put_objects(objects);
        put.map(drop).map_err(|source| self.stopped(source))
    }

    /// The changesets of `parents`, the parents of commit `oid`: each recorded by this import
    /// or known to it, as the walk put parents first. One that is neither would be a commit
    /// whose bytes the source gave the walk otherwise than the conversion: a bug.
    fn changesets(&self, oid: GitOid, parents: &[GitOid]) -> Result<Vec<ChangesetId>, ImportError> {
        (parents.iter())
            .map(|parent| self.ids.get(parent).copied())
            .collect::<Option<Vec<ChangesetId>>>()
            .ok_or(ImportError::Convert(ConvertError::Bug { commit: oid }))
    }

    /// Counts commit `commit`, recorded as `changeset`, among the lossy ones if `classes` holds
    /// any, and lists it while the list is short.
    fn note_lossy(&mut self, commit: GitOid, changeset: ChangesetId, classes: LossySet) {
        if classes.is_empty() {
            return;
        }
        self.lossy_total += 1;
        if self.lossy.len() < MAX_LOSSY_LISTED {
            self.lossy.push(LossyCommit {
                commit,
                changeset,
                classes,
            });
        }
    }

    /// Warns of the entry `key` that the index kept with other bytes, unless the walk warned of
    /// it already.
    fn conflict(&mut self, key: &[u8]) {
        let Ok(oid) = <[u8; 20]>::try_from(key).map(GitOid) else {
            return;
        };
        if self.flagged.contains(&oid) {
            return;
        }
        if let Some(changeset) = self.ids.get(&oid) {
            self.warnings.push(format!(
                "the external id index holds another entry for git commit {oid} than its \
                 changeset {changeset}: the entry is left as it is"
            ));
        }
    }

    /// Step 6 of [the module documentation](self): the metadata flushed, the target moved from
    /// `old_head` to the changeset of `tip`, and the move flushed; the report.
    fn finish(
        mut self,
        tip: GitOid,
        old_head: Option<ChangesetId>,
    ) -> Result<ImportReport, ImportError> {
        let repo = self.repo;
        let req = self.req;
        // The tip is known, or recorded by a frame: every commit the walk did not know is.
        let head = self.changesets(tip, &[tip])?[0];
        retry_busy(|| repo.flush_metadata()).map_err(|source| self.stopped(source))?;

        // An objects-only import moves no ref.
        if !req.moves_ref() {
            return Ok(self.report(tip, head, None, false));
        }

        let moved = retry_busy(|| repo.cas_ref(&req.target_ref, old_head, Some(head), &req.actor));
        let moved = moved.map_err(|source| self.stopped(source))?;
        absorb(&mut self.recovered, moved.recovered);
        let ref_moved = match moved.result {
            CasResult::Applied { .. } => true,
            // Another import of the same history moved it first.
            CasResult::Mismatch { actual } if actual == Some(head) => false,
            CasResult::Mismatch { actual } => {
                let target_ref = req.target_ref.clone();
                return Err(ImportError::CasMismatch { target_ref, actual });
            }
        };
        // A move whose step after its commit point failed is not followed by another write.
        let flush_again = ref_moved && moved.warnings.is_empty();
        self.warnings.extend(moved.warnings);
        if flush_again && let Err(error) = retry_busy(|| repo.flush_metadata()) {
            self.warnings.push(format!(
                "the target moved, but the flush after its move failed: {error}; its frame stays \
                 past the checkpoint until a later one"
            ));
        }
        Ok(self.report(tip, head, old_head, ref_moved))
    }

    /// The report of the import that recorded `head` for `tip`, the target moved from `old_head`
    /// if `ref_moved`.
    fn report(
        self,
        tip: GitOid,
        head: ChangesetId,
        old_head: Option<ChangesetId>,
        ref_moved: bool,
    ) -> ImportReport {
        ImportReport {
            imported: self.imported,
            skipped: self.skipped,
            head,
            git_head: tip,
            target_ref: self.req.target_ref.clone(),
            old_head,
            ref_moved,
            lossy: self.lossy,
            lossy_total: self.lossy_total,
            warnings: self.warnings,
            recovered: self.recovered,
        }
    }
}

/// The import's failure `source`, after `imported` commits, the last of them `last`.
fn stopped(source: RepoError, imported: u64, last: Option<GitOid>) -> ImportError {
    ImportError::Store {
        source: Box::new(source),
        imported,
        last,
    }
}

/// `call`, made again while it fails with [`StoreError::Busy`], [`BUSY_RETRIES`] times at most.
fn retry_busy<T>(mut call: impl FnMut() -> Result<T, RepoError>) -> Result<T, RepoError> {
    let mut retries = 0;
    loop {
        match call() {
            Err(RepoError::Store(StoreError::Busy)) if retries < BUSY_RETRIES => retries += 1,
            outcome => return outcome,
        }
    }
}

/// Adds `more`, what one more write transaction repaired, to `total`: the counts summed, the
/// views rebuilt if either rebuilt them, the first journal cut kept.
fn absorb(total: &mut Recovered, more: Recovered) {
    let Recovered {
        log_cut,
        frames_listed,
        frames_rewritten,
        off_bytes_dropped,
        index_records_dropped,
        views_rebuilt,
        records_replayed,
        refs_replayed,
    } = more;
    total.log_cut = total.log_cut.or(log_cut);
    total.frames_listed += frames_listed;
    total.frames_rewritten += frames_rewritten;
    total.off_bytes_dropped += off_bytes_dropped;
    total.index_records_dropped += index_records_dropped;
    total.views_rebuilt |= views_rebuilt;
    total.records_replayed += records_replayed;
    total.refs_replayed += refs_replayed;
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use swarmops_store::LogCut;

    use super::*;

    /// A call that finds the writer lock held is made again three times, then its `Busy` is the
    /// outcome; one that succeeds on a retry is through; any other failure is not retried.
    #[test]
    fn busy_is_retried_three_times() {
        let calls = Cell::new(0);
        let busy = retry_busy(|| {
            calls.set(calls.get() + 1);
            Err::<(), _>(RepoError::Store(StoreError::Busy))
        });
        assert!(matches!(busy, Err(RepoError::Store(StoreError::Busy))));
        assert_eq!(calls.get(), 1 + BUSY_RETRIES);

        let calls = Cell::new(0);
        let through = retry_busy(|| {
            calls.set(calls.get() + 1);
            if calls.get() <= BUSY_RETRIES {
                Err(RepoError::Store(StoreError::Busy))
            } else {
                Ok(calls.get())
            }
        });
        assert_eq!(through.unwrap(), 1 + BUSY_RETRIES);

        let calls = Cell::new(0);
        let other = retry_busy(|| {
            calls.set(calls.get() + 1);
            Err::<(), _>(RepoError::NotFound("x".to_owned()))
        });
        assert!(matches!(other, Err(RepoError::NotFound(_))));
        assert_eq!(calls.get(), 1);
    }

    /// What several write transactions repaired adds up: counts summed, the views rebuilt if
    /// any rebuilt them, the first journal cut kept.
    #[test]
    fn what_the_transactions_repaired_adds_up() {
        let first = LogCut { at: 10, bytes: 3 };
        let mut total = Recovered::default();
        absorb(
            &mut total,
            Recovered {
                log_cut: Some(first),
                frames_listed: 1,
                records_replayed: 2,
                ..Recovered::default()
            },
        );
        absorb(
            &mut total,
            Recovered {
                log_cut: Some(LogCut { at: 20, bytes: 1 }),
                frames_listed: 2,
                frames_rewritten: 1,
                off_bytes_dropped: 8,
                index_records_dropped: 4,
                views_rebuilt: true,
                refs_replayed: 1,
                ..Recovered::default()
            },
        );
        let expected = Recovered {
            log_cut: Some(first),
            frames_listed: 3,
            frames_rewritten: 1,
            off_bytes_dropped: 8,
            index_records_dropped: 4,
            views_rebuilt: true,
            records_replayed: 2,
            refs_replayed: 1,
        };
        assert_eq!(total, expected);
    }
}
