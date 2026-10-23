//! A land's resolutions (operations spec §3, resolution path (b); the M4 plan's Decision 9): what
//! a lander decided for some paths of its change's merge onto the target, applied to each
//! attempt's merge without a checkout.
//!
//! A resolution decides what `L`, the changeset the land makes, holds at its path and at every
//! path below it: what the change's head holds there ([`Side::Ours`]), what the target's head
//! holds there ([`Side::Theirs`]), one leaf of new content ([`Resolution::Put`],
//! [`Resolution::PutBlob`]), or nothing ([`Resolution::Delete`]). The resolutions are decided
//! against a changeset, `against`: the conflict report's `theirs`, the target's head the lander
//! read. In each attempt, after the merge and before its conflict check, a resolution applies to
//! its path when the target's head holds there what `against` holds (the same leaf, the same
//! directory, or nothing), whether the path conflicts or not, and resolves every conflict at or
//! below its path; a path the target's head holds otherwise is stale, since the target changed it
//! after the lander decided. A stale path, or a conflict no resolution resolved, fails the land
//! ([`RepoError::Conflict`], whose [`MergeConflicts::stale`] names the stale paths).
//!
//! A resolution that applies replaces what the merge made at and below its path (the merge's
//! delta entries there, its conflicts there, and the paths there it reported auto-merged) with
//! what turns the target's head into what the resolution says. Resolutions apply in path order,
//! so one below another decides its own path and those below it. What they make must be a tree:
//! a file that stays above a path one of them puts is a [`RepoError::TypeConflict`] when the land
//! writes `L`, and nothing is recorded.
//!
//! A content resolution's leaf is of the kind it gives, else of the kind of the leaf ours holds at
//! its path, else theirs', else a file. Its bytes are written with `L`, as the merged contents
//! are. A blob it names by id must be one the store holds ([`RepoError::MissingBlob`], before the
//! first attempt): the land neither reads nor writes it again, but retains it, so that the flush
//! of `L`'s objects, before any frame names `L`, makes its name durable (I1).
//!
//! [`MergeConflicts::stale`]: super::MergeConflicts::stale

use std::collections::BTreeMap;

use swarmops_core::{
    BlobId, ChangesetId, DeltaEntry, DeltaOp, EntryTarget, LeafKind, ObjectKind, RepoPath, TreeId,
};
use swarmops_merge::tree_diff::{self, Leaf, PathFilter};
use swarmops_merge::tree_merge;
use swarmops_store::{ObjectStat, ObjectStore};

use crate::error::RepoError;
use crate::merge::MergeOutcome;
use crate::read::RepoRead;
use crate::repo::Repo;

/// A land's resolutions ([`LandRequest::resolutions`](super::LandRequest::resolutions)): what to
/// make of some paths of the merge, decided against a changeset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolutions {
    /// The changeset they were decided against: the conflict report's `theirs`, a head of the
    /// target. A resolution applies while the target's head holds its path as this does.
    pub against: ChangesetId,
    /// What to make of each path, and of every path below it.
    pub files: BTreeMap<RepoPath, Resolution>,
}

impl Resolutions {
    /// Makes resolutions decided against `against`.
    ///
    /// Invariant: `against` is a head of the target. `new` does not check it: the land re-checks
    /// each resolution against the target's head it merges onto, and fails on a stale one
    /// ([`RepoError::Conflict`]).
    pub fn new(against: ChangesetId, files: BTreeMap<RepoPath, Resolution>) -> Resolutions {
        Resolutions { against, files }
    }
}

/// What a land makes of a path, and of every path below it ([`Resolutions`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// What this side holds there: a leaf, a directory, or nothing.
    Take(Side),
    /// One leaf holding `content`, nothing below it.
    Put {
        /// The leaf's kind: `None` for the kind of the leaf ours holds there, else theirs', else
        /// a file.
        kind: Option<LeafKind>,
        /// The bytes, which the land writes as a blob with the changeset it makes.
        content: Vec<u8>,
    },
    /// One leaf holding blob `id`, which the store holds already, nothing below it: as
    /// [`Resolution::Put`] of the blob's content, which the land neither reads nor writes again.
    /// An id the store holds no blob under is [`RepoError::MissingBlob`].
    PutBlob {
        /// The leaf's kind, as [`Resolution::Put`]'s.
        kind: Option<LeafKind>,
        /// The blob.
        id: BlobId,
    },
    /// Nothing: no leaf there, nor below.
    Delete,
}

/// One side of a land's merge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    /// The change: its head.
    Ours,
    /// The target: its head in the attempt.
    Theirs,
}

/// A land's resolutions, read once for all its attempts: what `against` and ours hold at each
/// path, each content hashed.
pub(super) struct Resolving {
    /// The root of `against`'s tree.
    against: TreeId,
    /// The root of the change's head's tree.
    ours: TreeId,
    /// The resolutions, in path order.
    paths: Vec<Prepared>,
}

/// One resolution, with what the land reads for it before its first attempt.
struct Prepared {
    /// Its path.
    path: RepoPath,
    /// What it makes there.
    makes: Makes,
    /// What `against` holds at the path.
    against: Option<EntryTarget>,
    /// The leaf ours holds at the path, if it holds one there.
    ours: Option<Leaf>,
}

/// What a resolution makes of its path, its content hashed.
enum Makes {
    /// What a side holds there.
    Take(Side),
    /// One leaf of blob `blob`, of `kind` if the resolution gives one; `bytes` are the blob's
    /// content when the land writes it, `None` for a blob the store holds.
    Put {
        kind: Option<LeafKind>,
        blob: BlobId,
        bytes: Option<Vec<u8>>,
    },
    /// Nothing.
    Delete,
}

impl Resolving {
    /// `resolutions`, prepared in `read` for a land whose head's root is `ours`: what `against`
    /// and ours hold at each path, each content hashed, and each blob named by id retained
    /// ([`retain_blob`]). [`RepoError::NotFound`] if `read` does not hold `against`.
    pub(super) fn new<O: ObjectStore>(
        repo: &Repo<O>,
        read: &RepoRead<'_, O>,
        resolutions: Resolutions,
        ours: TreeId,
    ) -> Result<Resolving, RepoError> {
        let Resolutions { against, files } = resolutions;
        let against = (read.changeset(against)?)
            .ok_or_else(|| RepoError::NotFound(against.to_string()))?
            .changeset
            .root;
        let mut paths = Vec::with_capacity(files.len());
        for (path, resolution) in files {
            let makes = match resolution {
                Resolution::Take(side) => Makes::Take(side),
                Resolution::Put { kind, content } => Makes::Put {
                    kind,
                    blob: BlobId(swarmops_core::hash_object(ObjectKind::Blob, &content)),
                    bytes: Some(content),
                },
                Resolution::PutBlob { kind, id } => {
                    retain_blob(repo, &path, id)?;
                    Makes::Put {
                        kind,
                        blob: id,
                        bytes: None,
                    }
                }
                Resolution::Delete => Makes::Delete,
            };
            paths.push(Prepared {
                against: tree_merge::entry_in(read, against, &path)?,
                ours: leaf(tree_merge::entry_in(read, ours, &path)?),
                path,
                makes,
            });
        }
        Ok(Resolving {
            against,
            ours,
            paths,
        })
    }

    /// Applies the resolutions to `merged`, an attempt's merge onto the target's head whose root
    /// is `theirs`, read in `read` (see [the module documentation](self)): each whose path
    /// `theirs` holds as `against` does replaces what the merge made at and below its path. The
    /// paths of the others, which are stale, in path order.
    pub(super) fn apply<O: ObjectStore>(
        &self,
        read: &RepoRead<'_, O>,
        theirs: TreeId,
        merged: &mut MergeOutcome,
    ) -> Result<Vec<RepoPath>, RepoError> {
        let mut delta: BTreeMap<RepoPath, DeltaEntry> = (std::mem::take(&mut merged.delta))
            .into_iter()
            .map(|entry| (entry.path.clone(), entry))
            .collect();
        let mut stale = Vec::new();
        let mut applied: Vec<&RepoPath> = Vec::new();
        for at in &self.paths {
            let held = if theirs == self.against {
                at.against
            } else {
                tree_merge::entry_in(read, theirs, &at.path)?
            };
            if held != at.against {
                stale.push(at.path.clone());
                continue;
            }
            delta.retain(|path, _| !at_or_below(path, &at.path));
            let under = PathFilter::under(vec![at.path.clone()]);
            // What turns what theirs holds at and below the path into what the resolution says:
            // theirs' own (nothing to change), ours', one leaf, or nothing.
            let entries = match &at.makes {
                Makes::Take(Side::Theirs) => Vec::new(),
                Makes::Take(Side::Ours) => tree_diff::delta_of(&tree_diff::diff_trees(
                    read,
                    Some(theirs),
                    Some(self.ours),
                    &under,
                )?),
                Makes::Delete => {
                    tree_diff::delta_of(&tree_diff::diff_trees(read, Some(theirs), None, &under)?)
                }
                Makes::Put { kind, blob, bytes } => {
                    let held = leaf(held);
                    let kind = (kind.or(at.ours.map(|leaf| leaf.kind)))
                        .or(held.map(|leaf| leaf.kind))
                        .unwrap_or(LeafKind::File);
                    // Theirs' leaves below the path go; the leaf at it is replaced, unless it is
                    // the one the resolution puts.
                    let mut entries: Vec<DeltaEntry> = tree_diff::delta_of(&tree_diff::diff_trees(
                        read,
                        Some(theirs),
                        None,
                        &under,
                    )?)
                    .into_iter()
                    .filter(|entry| entry.path != at.path)
                    .collect();
                    let put = Leaf { kind, blob: *blob };
                    if held != Some(put) {
                        entries.push(DeltaEntry {
                            path: at.path.clone(),
                            op: DeltaOp::Put { kind, id: *blob },
                            renamed_from: None,
                        });
                        if let Some(bytes) = bytes {
                            merged.new_blobs.insert(*blob, bytes.clone());
                        }
                    }
                    entries
                }
            };
            delta.extend(entries.into_iter().map(|entry| (entry.path.clone(), entry)));
            applied.push(&at.path);
        }
        merged.delta = delta.into_values().collect();
        let resolved = |path: &RepoPath| applied.iter().any(|at| at_or_below(path, at));
        merged
            .conflicts
            .retain(|conflict| !resolved(&conflict.path));
        merged.auto_merged.retain(|path| !resolved(path));
        Ok(stale)
    }
}

/// The leaf `entry` names, if it names one: not a directory, nor nothing.
fn leaf(entry: Option<EntryTarget>) -> Option<Leaf> {
    match entry {
        Some(EntryTarget::Leaf(kind, blob)) => Some(Leaf { kind, blob }),
        Some(EntryTarget::Dir(_)) | None => None,
    }
}

/// Whether `path` is `at` or a path below it.
fn at_or_below(path: &RepoPath, at: &RepoPath) -> bool {
    path == at
        || (path.as_str().strip_prefix(at.as_str())).is_some_and(|rest| rest.starts_with('/'))
}

/// Retains blob `id`, which the resolution of `path` names ([`ObjectStore::retain`]): its header
/// read, and its fan-out directory among the next flush's fsyncs, which the land's write of `L`
/// makes before `L` is published. [`RepoError::MissingBlob`] unless the store holds a blob under
/// `id` (no object, or a tree's or a changeset's).
fn retain_blob<O: ObjectStore>(
    repo: &Repo<O>,
    path: &RepoPath,
    id: BlobId,
) -> Result<(), RepoError> {
    match repo.objects.retain(id.0)? {
        Some(ObjectStat {
            kind: ObjectKind::Blob,
            ..
        }) => Ok(()),
        _ => Err(RepoError::MissingBlob {
            path: path.clone(),
            id,
        }),
    }
}
