//! The three-way tree merge's decisions (storage spec §4): what a merge makes of one path from
//! the leaf the base, ours and theirs each hold there ([`plan_path`]), what a tree holds at a path
//! ([`entry_in`]), and a path whose merge conflicts ([`FileConflict`]).
//!
//! A merge decides only the paths that ours changed since the base: [`three_way`] gives each with
//! its three leaves, and a path only theirs changed below one of them stays as theirs has it. A
//! single path's leaves cannot show a file against a directory, which takes the paths around it:
//! the repository engine (`swarmops_repo::Repo::merge_with`) finds those with [`entry_in`] and the
//! other paths' plans, merges the contents that both sides changed with a
//! [`ContentMerger`](crate::conflict::ContentMerger), and returns the result as data.
//!
//! [`three_way`]: crate::tree_diff::three_way

use swarmops_core::{EntryTarget, LeafKind, RepoPath, Tree, TreeId};

use crate::conflict::ConflictKind;
use crate::tree_diff::{Leaf, PathTriple, TreeRead};

/// What a three-way merge makes of one path, from its three leaves ([`plan_path`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PathPlan {
    /// The path keeps what theirs holds there, a leaf or nothing: ours left it as the base had
    /// it, or made the same change as theirs.
    Theirs,
    /// The path holds this leaf, which theirs does not: ours', where only ours changed the
    /// path, or one side's kind with the other side's content, where each changed one of them
    /// (an executable bit and a text: a symlink against a file conflicts instead).
    Put(Leaf),
    /// The path holds nothing: ours deleted the leaf, which theirs left as the base had it.
    Delete,
    /// Both sides changed the content, differently: the path holds a leaf of this kind (the
    /// side's that changed the kind, or the base's) whose content is the three contents merged.
    MergeContent {
        /// The merged leaf's kind.
        kind: LeafKind,
    },
    /// The two sides' changes of the path conflict.
    Conflict(ConflictKind),
}

/// Decides one path of a three-way merge from the leaf the base, ours and theirs hold there, in
/// this order:
///
/// 1. ours as the base had it: [`PathPlan::Theirs`] (a path only theirs changed);
/// 2. theirs as the base had it: ours' leaf ([`PathPlan::Put`]), or [`PathPlan::Delete`] where
///    ours has none;
/// 3. ours and theirs alike: [`PathPlan::Theirs`] (the same change);
/// 4. ours deleted the leaf, which theirs changed: [`ConflictKind::DeleteModify`]; theirs
///    deleted it, and ours changed it: [`ConflictKind::ModifyDelete`];
/// 5. both hold a leaf, each changed: the kind and the content are decided apart, each the
///    side's that changed it, or the change both made alike. Two different kinds are
///    [`ConflictKind::Mode`]. A symlink on one side against a file on the other is
///    [`ConflictKind::Binary`]: one side changed the kind and the other the target or the
///    content, whose bytes would mean another thing under the other kind (git's "distinct
///    types"). Two different contents are [`PathPlan::MergeContent`], unless the base's, ours'
///    or theirs' leaf is a symlink, which is not merged by lines ([`ConflictKind::Binary`]).
///    With no base (both added the path) a kind or a content both sides have differently counts
///    as changed by both.
///
/// A leaf it decides on that is theirs is [`PathPlan::Theirs`]. It looks at nothing but the
/// three leaves: whether a file stands against a directory (a [`ConflictKind::TypeChange`]) takes
/// the trees around the path, which its caller reads.
pub fn plan_path(triple: &PathTriple) -> PathPlan {
    let (base, ours, theirs) = (triple.base, triple.ours, triple.theirs);
    if ours == base {
        return PathPlan::Theirs;
    }
    if theirs == base {
        return ours.map_or(PathPlan::Delete, PathPlan::Put);
    }
    if theirs == ours {
        return PathPlan::Theirs;
    }
    let (ours, theirs) = match (ours, theirs) {
        (None, _) => return PathPlan::Conflict(ConflictKind::DeleteModify),
        (_, None) => return PathPlan::Conflict(ConflictKind::ModifyDelete),
        (Some(ours), Some(theirs)) => (ours, theirs),
    };
    let Some(kind) = changed_by_one(base.map(|b| b.kind), ours.kind, theirs.kind) else {
        return PathPlan::Conflict(ConflictKind::Mode {
            ours: ours.kind,
            theirs: theirs.kind,
        });
    };
    // One side changed the kind, to or from a symlink, and the other the bytes: decided apart,
    // they would make a file of a link's target, or a link to a file's text.
    if ours.kind != theirs.kind && [ours.kind, theirs.kind].contains(&LeafKind::Symlink) {
        return PathPlan::Conflict(ConflictKind::Binary);
    }
    let Some(blob) = changed_by_one(base.map(|b| b.blob), ours.blob, theirs.blob) else {
        let symlink = [base.map(|b| b.kind), Some(ours.kind), Some(theirs.kind)]
            .contains(&Some(LeafKind::Symlink));
        return if symlink {
            PathPlan::Conflict(ConflictKind::Binary)
        } else {
            PathPlan::MergeContent { kind }
        };
    };
    let leaf = Leaf { kind, blob };
    if leaf == theirs {
        PathPlan::Theirs
    } else {
        PathPlan::Put(leaf)
    }
}

/// One property of a leaf both sides changed (its kind, or its content), merged: theirs' where
/// ours left it as the base had it, ours' where theirs did, the value both gave it alike; `None`
/// when both changed it, differently.
fn changed_by_one<T: PartialEq + Copy>(base: Option<T>, ours: T, theirs: T) -> Option<T> {
    if Some(ours) == base || ours == theirs {
        Some(theirs)
    } else if Some(theirs) == base {
        Some(ours)
    } else {
        None
    }
}

/// What the tree `root` holds at `path`: a leaf (its kind and blob), a directory (its tree), or
/// nothing (`None`, also below a file and below a directory that is not there). For any root,
/// a recorded changeset's or not. Reads the tree of each directory on the way, the root's first,
/// and stops where the way ends; the first error of a read is the error.
pub fn entry_in<R: TreeRead>(
    read: &R,
    root: TreeId,
    path: &RepoPath,
) -> Result<Option<EntryTarget>, R::Error> {
    let mut at = EntryTarget::Dir(root);
    for name in path.components() {
        let Some(dir) = at.dir() else {
            // Below a file.
            return Ok(None);
        };
        match find(&read.tree(dir)?, name) {
            Some(target) => at = target,
            None => return Ok(None),
        }
    }
    Ok(Some(at))
}

/// What the entry named `name` of `tree` names: a binary search, the entries being sorted by
/// name bytes.
fn find(tree: &Tree, name: &str) -> Option<EntryTarget> {
    let entries = tree.entries();
    entries
        .binary_search_by(|entry| entry.name.as_bytes().cmp(name.as_bytes()))
        .ok()
        .map(|at| entries[at].target)
}

/// A path whose merge conflicts: why, and the leaf the base, ours and theirs each hold there.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct FileConflict {
    /// The path.
    pub path: RepoPath,
    /// Why it conflicts.
    pub kind: ConflictKind,
    /// The base's leaf there; `None` for none (nothing, or a directory).
    pub base: Option<Leaf>,
    /// Ours' leaf there; `None` for none.
    pub ours: Option<Leaf>,
    /// Theirs' leaf there; `None` for none.
    pub theirs: Option<Leaf>,
}

impl FileConflict {
    /// The conflict at `path`, why, and the three leaves there.
    pub fn new(
        path: RepoPath,
        kind: ConflictKind,
        base: Option<Leaf>,
        ours: Option<Leaf>,
        theirs: Option<Leaf>,
    ) -> Self {
        Self {
            path,
            kind,
            base,
            ours,
            theirs,
        }
    }
}
