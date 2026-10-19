//! The lock holder's relocation of a land onto a target that moved (Pushrebase, Metalog, and op
//! heads' settling), and the line merge Metalog takes in the lock ([`toy_merge`]).

use std::collections::BTreeSet;

use swarmops_core::{
    BlobId, Changeset, ChangesetId, DeltaEntry, DeltaOp, EntryTarget, Hashed, LeafKind, ObjectKind,
    RepoPath, Tree, TreeId,
};
use swarmops_store::{MetaWrite, ObjectStore, StoreError};

use super::{MAX_MERGE_BYTES, MAX_WALK};
use crate::error::RepoError;
use crate::repo::{self, Repo};
use crate::tree_edit::{Encoded, TreeEdit};

/// What [`relocate`] made of a land.
pub(super) enum Relocation {
    /// `changeset` (whose id is `id`) is the land on the actual head, its objects durable; it
    /// is not queued yet.
    Relocated {
        /// Its id.
        id: ChangesetId,
        /// The relocated changeset.
        changeset: Box<Changeset>,
    },
    /// The actual head is not `onto` or a descendant of it within [`MAX_WALK`] first-parent
    /// steps.
    Moved,
    /// A path the land changes overlaps one the target gained, and was not merged.
    Overlap,
}

/// `l`, prepared on `onto`, made anew on `actual`, the target's head, under the lock `write`
/// holds: see [the module documentation](super). With `merge` (Metalog), overlapping files are
/// merged by [`toy_merge`]. The new blobs, trees and changeset object are written and flushed
/// before this returns, so before any frame names them (I1).
pub(super) fn relocate<O: ObjectStore>(
    repo: &Repo<O>,
    write: &MetaWrite<'_>,
    l: &Changeset,
    onto: ChangesetId,
    actual: ChangesetId,
    merge: bool,
) -> Result<Relocation, RepoError> {
    let Some(gained) = gained_since(write, onto, actual)? else {
        return Ok(Relocation::Moved);
    };
    let mut paths = BTreeSet::new();
    for id in gained {
        for entry in repo.changeset_object(id)?.delta {
            paths.insert(entry.path.as_str().to_owned());
        }
    }
    let actual_root = repo.changeset_object(actual)?.root;
    let onto_root = repo.changeset_object(onto)?.root;
    let mut delta = l.delta.clone();
    let mut blobs = Vec::new();
    for entry in &mut delta {
        if !overlaps_any(&entry.path, &paths) {
            continue;
        }
        if !merge {
            return Ok(Relocation::Overlap);
        }
        let DeltaOp::Put {
            kind: LeafKind::File,
            id: ours,
        } = entry.op
        else {
            return Ok(Relocation::Overlap);
        };
        let base = file_at(repo, onto_root, &entry.path)?;
        let theirs = file_at(repo, actual_root, &entry.path)?;
        let (Some(base), Some(theirs)) = (base, theirs) else {
            return Ok(Relocation::Overlap);
        };
        let ours = repo::object(&repo.objects, ours.0, ObjectKind::Blob)?;
        let small = [&base, &ours, &theirs]
            .iter()
            .all(|side| side.len() <= MAX_MERGE_BYTES);
        let merged = small.then(|| toy_merge(&base, &ours, &theirs)).flatten();
        let Some(merged) = merged else {
            return Ok(Relocation::Overlap);
        };
        let merged = Hashed::new(ObjectKind::Blob, merged);
        entry.op = DeltaOp::Put {
            kind: LeafKind::File,
            id: BlobId(merged.id()),
        };
        blobs.push(merged);
    }
    let mut edit = TreeEdit::new(&repo.objects, actual_root)?;
    let effective = edit.apply(&delta)?;
    let delta: Vec<DeltaEntry> = delta
        .into_iter()
        .zip(effective)
        .filter_map(|(entry, effective)| effective.then_some(entry))
        .collect();
    // A merge that left the actual head's files as they are has nothing to land there.
    if delta.is_empty() {
        return Ok(Relocation::Overlap);
    }
    let Encoded { root, changed } = edit.finish()?;
    let changeset = Changeset {
        parents: vec![actual],
        root,
        delta,
        ..l.clone()
    };
    let object = Hashed::new(ObjectKind::Changeset, changeset.encode()?);
    let id = ChangesetId(object.id());
    let mut batch = blobs;
    batch.extend(changed);
    batch.push(object);
    repo.objects.put_hashed_batch(batch)?;
    repo.objects.flush()?;
    Ok(Relocation::Relocated {
        id,
        changeset: Box::new(changeset),
    })
}

/// The changesets on `actual`'s first-parent line down to `onto`, `actual` first and `onto`
/// left out, as `write` sees them; `None` past [`MAX_WALK`] of them, or when `onto` is not on
/// that line (a changeset at or below `onto`'s generation that is not `onto`, or a root).
fn gained_since(
    write: &MetaWrite<'_>,
    onto: ChangesetId,
    actual: ChangesetId,
) -> Result<Option<Vec<ChangesetId>>, RepoError> {
    let onto_generation = write
        .changeset(onto)?
        .ok_or_else(|| RepoError::NotFound(onto.to_string()))?
        .generation;
    let mut gained = Vec::new();
    let mut at = actual;
    while at != onto {
        if gained.len() == MAX_WALK {
            return Ok(None);
        }
        let meta = write.changeset(at)?.ok_or(StoreError::MissingParent(at))?;
        // Generations fall by at least one per first-parent step.
        if meta.generation <= onto_generation {
            return Ok(None);
        }
        gained.push(at);
        match meta.parents.first() {
            Some(&parent) => at = parent,
            None => return Ok(None),
        }
    }
    Ok(Some(gained))
}

/// Whether `path` overlaps one of `gained`: equals one, lies below one, or has one below it.
fn overlaps_any(path: &RepoPath, gained: &BTreeSet<String>) -> bool {
    let path = path.as_str();
    if gained.contains(path) {
        return true;
    }
    let mut above = path;
    while let Some((parent, _)) = above.rsplit_once('/') {
        if gained.contains(parent) {
            return true;
        }
        above = parent;
    }
    let below = format!("{path}/");
    gained
        .range(below.clone()..)
        .next()
        .is_some_and(|next| next.starts_with(&below))
}

/// The bytes of the regular file at `path` in tree `root`; `None` if no regular file is there.
fn file_at<O: ObjectStore>(
    repo: &Repo<O>,
    root: TreeId,
    path: &RepoPath,
) -> Result<Option<Vec<u8>>, RepoError> {
    let components: Vec<&str> = path.components().collect();
    let (&name, dirs) = components.split_last().expect("a path has a component");
    let mut tree = repo::tree(&repo.objects, root)?;
    for &dir in dirs {
        match entry(&tree, dir) {
            Some(EntryTarget::Dir(below)) => tree = repo::tree(&repo.objects, below)?,
            _ => return Ok(None),
        }
    }
    match entry(&tree, name) {
        Some(EntryTarget::Leaf(LeafKind::File, blob)) => {
            Ok(Some(repo::object(&repo.objects, blob.0, ObjectKind::Blob)?))
        }
        _ => Ok(None),
    }
}

/// What the entry named `name` of `tree` names: a binary search, the entries sorted by name.
fn entry(tree: &Tree, name: &str) -> Option<EntryTarget> {
    let entries = tree.entries();
    entries
        .binary_search_by(|entry| entry.name.as_bytes().cmp(name.as_bytes()))
        .ok()
        .map(|at| entries[at].target)
}

/// A three-way merge of `ours` and `theirs` from `base`, line by line, for the prototypes: equal
/// sides, or a side equal to `base`, decide at once; otherwise the three are split at each
/// `\n` and must have one line count, and each line is the side that changed it (both, if they
/// changed it alike). `None` when a line changed differently on both sides, or the counts
/// differ: a conflict. M4's real merge (diff3 over M2's line hunks) comes with Task 3b.
pub fn toy_merge(base: &[u8], ours: &[u8], theirs: &[u8]) -> Option<Vec<u8>> {
    if ours == theirs || theirs == base {
        return Some(ours.to_vec());
    }
    if ours == base {
        return Some(theirs.to_vec());
    }
    let (base, ours, theirs) = (lines(base), lines(ours), lines(theirs));
    if ours.len() != base.len() || theirs.len() != base.len() {
        return None;
    }
    let mut merged = Vec::new();
    for (at, ((base, ours), theirs)) in base.iter().zip(&ours).zip(&theirs).enumerate() {
        let line = if ours == theirs || theirs == base {
            ours
        } else if ours == base {
            theirs
        } else {
            return None;
        };
        if at > 0 {
            merged.push(b'\n');
        }
        merged.extend_from_slice(line);
    }
    Some(merged)
}

/// `bytes` split at each `\n`: the lines without their breaks, and what follows the last break.
fn lines(bytes: &[u8]) -> Vec<&[u8]> {
    bytes.split(|&b| b == b'\n').collect()
}
