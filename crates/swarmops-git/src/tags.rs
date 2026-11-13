//! The tag intake (the annotated tags design §2.3): one function for serve, fetch and import that
//! takes a git tag, `name` at the object `oid`, into a store tag, or refuses it for that tag
//! alone.
//!
//! ```text
//!  oid ──read_bounded──▶ commit ────────────────────────────▶ lightweight, target = oid
//!                        tag    ──RawTag::parse──▶ format 3? ─▶ type commit? ─▶ annotated,
//!                        tree, blob ─▶ tag_target_kind              │            target = object
//!                                                                   └ tag ─▶ tag_of_tag
//!                                                                     tree, blob ─▶ tag_target_kind
//!  target ──objects-only import──▶ changeset T (external id) ──export oid of T == target?──▶
//!  Repo::tags_create, TAG_BATCH tags a frame (create-only: alike Unchanged, else immutable)
//! ```
//!
//! Intake accepts exactly what export writes back: the target commit is imported objects-only
//! (unless an external id names a changeset that exports to it already: a tag taken again writes
//! nothing), its changeset found by its external id, and the commit that changeset exports to must be the
//! target itself, otherwise the tag is refused as lossy and nothing is written for it. A FORMAT 2
//! store refuses an annotated tag before any import or blob put; the bytes of a tag object are
//! never rewritten: unknown header lines and a missing tagger are kept as they are.

use swarmops_core::ChangesetId;
use swarmops_repo::{
    Applied, LocalRepo, RefKind, RepoError, StoreError, StoreFormat, TAG_BATCH, TagObject, TagWant,
};
use swarmops_store::{Recovered, SYSTEM_GIT};

use crate::{
    ConvertError, GitOid, GitSource, ImportError, ImportReport, ImportRequest, RawError, RawKind,
    RawTag,
};
use crate::{export, import, source};

/// The feature an annotated tag needs (the FORMAT 3 design §2.6).
const FEATURE: &str = "tag_object";

/// What the intake did with a tag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum TagTaken {
    /// It created the store tag.
    Created,
    /// The store held the tag already, at the same target with the same object: nothing written.
    Unchanged,
}

/// Why the intake refuses a git tag: the store could not write it back as git has it. Each is
/// `E_INVALID_ARG` with its [`as_str`](TagRefusal::as_str) as `details.reason`.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum TagRefusal {
    /// The tag names another tag (`type tag`).
    #[error("a tag of a tag is refused: a store tag names a changeset")]
    Nested,
    /// The tag, or a lightweight tag's ref, names a tree or a blob.
    #[error("a tag of a {0} is refused: a store tag names a changeset")]
    TargetKind(RawKind),
    /// The commit is taken only lossily, or its changeset exports as another commit: the tag's
    /// `object` line could never be written back.
    #[error("a tag of a lossy commit is refused: its changeset exports as another commit")]
    Lossy,
    /// The tag object is no tag git's own checks take.
    #[error("the tag object is malformed: {0}")]
    Malformed(RawError),
}

impl TagRefusal {
    /// The refusal's name, as `details.reason` gives it: `tag_of_tag`, `tag_target_kind`,
    /// `tag_target_lossy` or `tag_malformed`.
    pub fn as_str(&self) -> &'static str {
        match self {
            TagRefusal::Nested => "tag_of_tag",
            TagRefusal::TargetKind(_) => "tag_target_kind",
            TagRefusal::Lossy => "tag_target_lossy",
            TagRefusal::Malformed(_) => "tag_malformed",
        }
    }
}

/// Why the intake did not take a tag. Nothing is written for the tag, but the commits an
/// objects-only import recorded before a later refusal.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TagError {
    /// The store could not hold the tag as git has it: [`TagRefusal`] says why.
    #[error(transparent)]
    Refused(TagRefusal),
    /// The store holds the tag at another target or with another object: a tag never moves.
    #[error("tag {0:?} cannot move: delete it and create it again")]
    Immutable(String),
    /// The store's format holds no such tag: a tag below FORMAT 2 (`name` the ref), an annotated
    /// tag below FORMAT 3 (`name` the feature, `tag_object`). An upgrade makes it valid.
    #[error("{name} needs store format {needs}; this store is format {store}")]
    Format {
        /// The ref name, or the feature.
        name: String,
        /// The first format version that takes it.
        needs: u32,
        /// The store's format version.
        store: u32,
    },
    /// The git source or the import of the tag's commit failed.
    #[error(transparent)]
    Import(ImportError),
    /// The store failed, or the name is no tag name.
    #[error(transparent)]
    Repo(RepoError),
}

impl TagError {
    /// The store's refusal `e`: a tag move and a format the store lacks by their own variants.
    fn from_repo(e: RepoError) -> TagError {
        match e {
            RepoError::Store(StoreError::TagImmutable(name)) => TagError::Immutable(name),
            RepoError::Store(StoreError::FormatRequired { name, needs, store }) => {
                TagError::Format { name, needs, store }
            }
            other => TagError::Repo(other),
        }
    }
}

/// A tag the intake checked, ready for its create.
struct Wanted {
    /// The changeset it names.
    target: ChangesetId,
    /// What it carries.
    object: TagObject,
    /// The objects-only import of its commit.
    report: ImportReport,
}

/// One tag's outcome in a chunk, with the tag it checked.
type Taken = Result<(TagTaken, Wanted), TagError>;

/// Takes the git tag `name` (`tags/<path>`), which names the object `oid` of `source`, into
/// `repo` for `actor`: see [the module documentation](self). [`take_tags`] of one tag.
pub fn take_tag(
    repo: &LocalRepo,
    source: &dyn GitSource,
    name: &str,
    oid: GitOid,
    actor: &str,
) -> Result<TagTaken, TagError> {
    let tags = [(name.to_owned(), oid)];
    let taken = take_tags(repo, source, &tags, actor).map_err(TagError::Repo)?;
    taken
        .into_iter()
        .next()
        .unwrap_or_else(|| Err(TagError::Repo(missing_outcome())))
}

/// Takes each git tag of `tags`, a name and the object it names in `source`, into `repo` for
/// `actor`, each decided on its own, in their order: [`TAG_BATCH`] tags at a time, each checked
/// and its commit imported, then created in one frame, so that a run cut between frames keeps
/// the tags before and, run again, finds them [`TagTaken::Unchanged`]. A store failure of the
/// creates fails the call; the frames before it stay.
pub fn take_tags(
    repo: &LocalRepo,
    source: &dyn GitSource,
    tags: &[(String, GitOid)],
    actor: &str,
) -> Result<Vec<Result<TagTaken, TagError>>, RepoError> {
    let mut taken = Vec::with_capacity(tags.len());
    for chunk in tags.chunks(TAG_BATCH) {
        let decided = take_chunk(repo, source, chunk, actor)?;
        taken.extend(decided.into_iter().map(|one| one.map(|(taken, _)| taken)));
    }
    Ok(taken)
}

/// Imports into `repo`'s `tags/` target the annotated tag `tag` that `req.git_ref` names in
/// `source`, a FORMAT 3 store's: [`import()`](crate::import())'s path for it. The report is the
/// objects-only import's of the tag's commit, the target created at its changeset (`ref_moved`),
/// or found there with the same object.
pub(crate) fn import_tag(
    repo: &LocalRepo,
    source: &dyn GitSource,
    req: &ImportRequest,
    tag: GitOid,
) -> Result<ImportReport, ImportError> {
    let tags = [(req.target_ref.clone(), tag)];
    let decided = take_chunk(repo, source, &tags, &req.actor).map_err(stopped)?;
    let one = decided.into_iter().next();
    let (taken, wanted) = match one.unwrap_or_else(|| Err(TagError::Repo(missing_outcome()))) {
        Ok(one) => one,
        Err(TagError::Refused(refusal)) => {
            let target_ref = req.target_ref.clone();
            return Err(ImportError::Tag {
                target_ref,
                refusal,
            });
        }
        Err(TagError::Import(e)) => return Err(e),
        Err(TagError::Immutable(name)) => {
            return Err(stopped(RepoError::Store(StoreError::TagImmutable(name))));
        }
        Err(TagError::Format { name, needs, store }) => {
            let required = StoreError::FormatRequired { name, needs, store };
            return Err(stopped(RepoError::Store(required)));
        }
        Err(TagError::Repo(e)) => return Err(stopped(e)),
    };

    // The report of the commit's import, as the target's.
    let mut report = wanted.report;
    let created = taken == TagTaken::Created;
    report.head = wanted.target;
    report.target_ref.clone_from(&req.target_ref);
    report.ref_moved = created;
    report.old_head = (!created).then_some(wanted.target);
    Ok(report)
}

/// The import's failure `e`, before it recorded anything the report counts.
fn stopped(e: RepoError) -> ImportError {
    ImportError::Store {
        source: Box::new(e),
        imported: 0,
        last: None,
    }
}

/// What a chunk whose create gave no outcome for a tag says: a store bug.
fn missing_outcome() -> RepoError {
    RepoError::Internal("a tag create gave no outcome for a tag of its batch".to_owned())
}

/// The tags of `chunk`, at most [`TAG_BATCH`]: each checked and its commit imported, then the
/// ones that passed created in one call ([`swarmops_repo::Repo::tags_create`]), each outcome in
/// the chunk's order.
fn take_chunk(
    repo: &LocalRepo,
    source: &dyn GitSource,
    chunk: &[(String, GitOid)],
    actor: &str,
) -> Result<Vec<Taken>, RepoError> {
    // 1. Each tag on its own: refused, or its target known.
    let checked: Vec<Result<Wanted, TagError>> = (chunk.iter())
        .map(|(name, oid)| check(repo, source, name, *oid))
        .collect();

    // 2. The ones that passed, created together.
    let batch: Vec<TagWant> = (checked.iter().zip(chunk))
        .filter_map(|(checked, (name, _))| {
            let wanted = checked.as_ref().ok()?;
            Some((name.clone(), wanted.target, wanted.object.clone()))
        })
        .collect();
    let mut made = repo.tags_create(&batch, actor)?.into_iter();

    // 3. Each outcome, in the chunk's order.
    let mut taken = Vec::with_capacity(chunk.len());
    for checked in checked {
        let mut wanted = match checked {
            Ok(wanted) => wanted,
            Err(refused) => {
                taken.push(Err(refused));
                continue;
            }
        };
        let Some(outcome) = made.next() else {
            return Err(missing_outcome());
        };
        let made = match outcome {
            Ok(made) => made,
            Err(e) => {
                taken.push(Err(TagError::from_repo(e)));
                continue;
            }
        };
        let outcome = match made.applied {
            Applied::New => TagTaken::Created,
            Applied::Already => TagTaken::Unchanged,
            _ => {
                return Err(RepoError::Internal(format!(
                    "Applied: {}",
                    crate::UNKNOWN_VARIANT
                )));
            }
        };
        // Steps after the frame's commit point that failed: the tag is made all the same.
        wanted.report.warnings.extend(made.warnings);
        taken.push(Ok((outcome, wanted)));
    }
    Ok(taken)
}

/// The tag `name` at the object `oid` of `source`, checked and its commit imported objects-only:
/// what its create writes, or why it is refused.
fn check(
    repo: &LocalRepo,
    source: &dyn GitSource,
    name: &str,
    oid: GitOid,
) -> Result<Wanted, TagError> {
    // 1. A tag name the store's format takes.
    repo.check_ref_name(name).map_err(TagError::from_repo)?;
    if swarmops_store::ref_kind(name) != Some(RefKind::Tag) {
        return Err(TagError::Repo(RepoError::RefName(name.to_owned())));
    }

    // 2. The object, through the size gate: a commit is a lightweight tag, a tag object an
    //    annotated one.
    let read = source::read_bounded(source, oid);
    let (kind, bytes) = read.map_err(|e| TagError::Import(ImportError::Source(e)))?;
    let (commit, object) = match kind {
        RawKind::Commit => (oid, TagObject::Lightweight),
        RawKind::Tag => (tagged(repo, &bytes)?, TagObject::Annotated(bytes)),
        other => return Err(TagError::Refused(TagRefusal::TargetKind(other))),
    };

    // 3. A commit the store holds already, by an external id whose changeset exports to it:
    //    nothing to import, nothing written (a tag taken again).
    if let Some(target) = known(repo, commit)? {
        let report = ImportReport::new(
            0,
            1,
            target,
            commit,
            String::new(),
            None,
            false,
            Vec::new(),
            0,
            Vec::new(),
            Recovered::default(),
        );
        return Ok(Wanted {
            target,
            object,
            report,
        });
    }

    // 4. Else the commit imported objects-only, its changeset found by its external id.
    let req = ImportRequest::objects_only(commit.to_string());
    let report = match import(repo, source, &req) {
        Ok(report) => report,
        Err(ImportError::Convert(ConvertError::Lossy { .. })) => {
            return Err(TagError::Refused(TagRefusal::Lossy));
        }
        Err(e) => return Err(TagError::Import(e)),
    };
    let entry = repo.external_id(SYSTEM_GIT, &commit.0);
    let target = entry
        .map_err(TagError::Repo)?
        .map_or(report.head, |e| e.changeset);

    // 5. Taken only if the export writes the commit back: the derivation the export makes.
    if export_of(repo, target)? != commit {
        return Err(TagError::Refused(TagRefusal::Lossy));
    }
    Ok(Wanted {
        target,
        object,
        report,
    })
}

/// The changeset the external id index names for `commit`, if the store holds it and it exports
/// to `commit`; `None` otherwise, an entry that is damaged or dangling included, which the import
/// passes over in turn.
fn known(repo: &LocalRepo, commit: GitOid) -> Result<Option<ChangesetId>, TagError> {
    let entry = match repo.external_id(SYSTEM_GIT, &commit.0) {
        Ok(Some(entry)) => entry,
        Ok(None) | Err(RepoError::Store(StoreError::ExternalIdCorrupt { .. })) => return Ok(None),
        Err(e) => return Err(TagError::Repo(e)),
    };
    let read = repo.snapshot().map_err(TagError::Repo)?;
    if read
        .changeset(entry.changeset)
        .map_err(TagError::Repo)?
        .is_none()
    {
        return Ok(None);
    }
    let exported = export::derived_oid(repo, &read, None, entry.changeset);
    let exported = exported.map_err(TagError::Repo)?;
    Ok((exported == commit).then_some(entry.changeset))
}

/// The commit changeset `id` exports to, derived as the export derives it.
fn export_of(repo: &LocalRepo, id: ChangesetId) -> Result<GitOid, TagError> {
    let read = repo.snapshot().map_err(TagError::Repo)?;
    export::derived_oid(repo, &read, None, id).map_err(TagError::Repo)
}

/// The commit the tag object `bytes` names: its head parsed, the store's format read before
/// anything is imported or put (FORMAT 3, feature `tag_object`), and its `type` a commit.
fn tagged(repo: &LocalRepo, bytes: &[u8]) -> Result<GitOid, TagError> {
    let parsed = RawTag::parse(bytes).map_err(|e| TagError::Refused(TagRefusal::Malformed(e)))?;
    let format = repo.format();
    if format != StoreFormat::V3 {
        return Err(TagError::Format {
            name: FEATURE.to_owned(),
            needs: StoreFormat::V3.version(),
            store: format.version(),
        });
    }
    match parsed.kind() {
        RawKind::Commit => Ok(parsed.object()),
        RawKind::Tag => Err(TagError::Refused(TagRefusal::Nested)),
        other => Err(TagError::Refused(TagRefusal::TargetKind(other))),
    }
}
