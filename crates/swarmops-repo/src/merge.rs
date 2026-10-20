//! The merge algorithms (`swarmops-merge`, re-exported here whole): pure functions over trees and
//! lines, with no I/O and no store, which the engine's operations call over the trees they read.
//! And the repository's half of a three-way merge (storage spec §4): [`Repo::merge_with`] merges
//! what one changeset, ours, changed since a base onto another, theirs, reading only the paths
//! ours changed, and returns the result as data, a [`MergeOutcome`], for its caller to commit or
//! to report; [`Repo::merge_trees_with`] does the same for three trees, over the paths it is
//! given. Neither writes anything. [`Repo::merge`] and [`Repo::merge_trees`] are the two with the
//! three-way text merge, diff3 ([`merge_content`]), which a land makes ([`crate::land`]).
//!
//! Each path ours changed is decided from the leaf the base, ours and theirs hold there
//! ([`plan_path`]); a path only theirs changed below one of them keeps theirs' leaf. A file
//! against a directory is a [`ConflictKind::TypeChange`] at ours' path: ours puts a leaf where
//! theirs holds none, and the merge keeps a leaf of theirs below it (theirs' directory there) or
//! above it (a file of theirs on its way, found with [`entry_in`]); or ours deleted the leaf,
//! holds nothing below it, and theirs holds a directory there. A leaf of theirs that the merge
//! deletes stands in no one's way: a directory ours turned into a file, whose leaves theirs left
//! as the base had them, and a file ours turned into a directory, which theirs left as it was,
//! merge cleanly. The contents both sides changed are merged by the caller's
//! [`ContentMerger`]; ours' unchanged paths are never read.
//!
//! A merge holds the hunks of the contents that conflict as the content merger makes them, the
//! first 128 of them in path order (`MAX_HELD_HUNKS`); a file's hunks past those are held as one
//! hunk that spans them, each side's lines from the first one's start to the last one's end, the
//! lines between them included, with their bytes. So what a merge holds of its conflicts grows
//! with the files that conflict, not with their lines, and each place where the two sides' edits
//! meet is still within a hunk it holds. Every conflicting path is there, whatever its hunks.
//!
//! [`plan_path`]: tree_merge::plan_path
//! [`entry_in`]: tree_merge::entry_in
//! [`ContentMerger`]: conflict::ContentMerger

pub use swarmops_merge::*;

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::ops::Range;

use swarmops_core::{
    BlobId, ChangesetId, DeltaEntry, DeltaOp, EntryTarget, LeafKind, ObjectKind, RepoPath, Tree,
    TreeId, hash_object,
};
use swarmops_merge::conflict::{ConflictHunk, ConflictKind, ContentMerge, ContentMerger};
use swarmops_merge::diff3::merge_content;
use swarmops_merge::tree_diff::{Leaf, PathFilter, PathTriple, TreeRead, diff_trees, three_way};
use swarmops_merge::tree_merge::{FileConflict, PathPlan, entry_in, plan_path};
use swarmops_store::ObjectStore;

use crate::error::RepoError;
use crate::repo::{self, Repo};

/// The most conflict hunks a merge holds as the content merger makes them, over all its files;
/// past them, each file's hunks are held as one that spans them ([the module documentation](self)).
const MAX_HELD_HUNKS: usize = 128;

/// What a three-way merge made ([`Repo::merge_with`]), for its caller to commit, or to report.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MergeOutcome {
    /// What turns theirs' root into the merged tree, sorted by path: a put of each leaf the
    /// merge holds that theirs does not, a delete of each leaf of theirs it drops. A path that
    /// conflicts has no entry: it keeps theirs' leaf.
    pub delta: Vec<DeltaEntry>,
    /// The merged contents the delta puts that neither ours nor theirs holds, by blob id: the
    /// caller stores them before it commits the delta.
    pub new_blobs: BTreeMap<BlobId, Vec<u8>>,
    /// The paths whose changes conflict, sorted by path; their hunks as
    /// [the module documentation](self) says a merge holds them.
    pub conflicts: Vec<FileConflict>,
    /// The paths both sides changed whose contents merged cleanly into a content neither had
    /// (`new_blobs` holds it), sorted.
    pub auto_merged: Vec<RepoPath>,
}

impl<O: ObjectStore> Repo<O> {
    /// Merges what changeset `ours` changed since changeset `base` onto changeset `theirs` (see
    /// [the module documentation](self)): the paths of the diff from `base`'s tree to `ours`'
    /// ([`diff_trees`]), each decided against what theirs holds there, the contents both sides
    /// changed merged by `content`, which is called with the base's, ours' and theirs' contents
    /// in that order (an empty base where the base has no leaf). It reads the three changesets
    /// in a snapshot of its own, [`RepoError::NotFound`] if it does not hold one of them; then
    /// the trees on the paths ours changed, each tree once, and the blobs of the paths both
    /// sides changed. It writes nothing.
    pub fn merge_with(
        &self,
        base: ChangesetId,
        ours: ChangesetId,
        theirs: ChangesetId,
        content: ContentMerger<'_>,
    ) -> Result<MergeOutcome, RepoError> {
        let read = self.snapshot()?;
        let root = |cs: ChangesetId| -> Result<TreeId, RepoError> {
            match read.changeset(cs)? {
                Some(stored) => Ok(stored.changeset.root),
                None => Err(RepoError::NotFound(cs.to_string())),
            }
        };
        let roots = [root(base)?, root(ours)?, root(theirs)?];
        self.merge_roots(roots, content)
    }

    /// [`Repo::merge_with`] of three trees, the merge of what `ours` changed since `base` onto
    /// `theirs`, over `paths` and the paths below them only: the caller vouches that they hold
    /// every path ours changed, for a change of ours outside them is left out. So a land's
    /// re-merge merges its changeset's own delta paths again, after the target moved, without
    /// reading the rest of the trees. Each tree is read once; nothing is written.
    pub fn merge_trees_with(
        &self,
        base: TreeId,
        ours: TreeId,
        theirs: TreeId,
        paths: &[RepoPath],
        content: ContentMerger<'_>,
    ) -> Result<MergeOutcome, RepoError> {
        let reads = Reads::new(&self.objects);
        merge_trees(&reads, [base, ours, theirs], paths.to_vec(), content)
    }

    /// [`Repo::merge_with`] with the three-way text merge, [`merge_content`] (diff3): the merge a
    /// land makes of its change onto the target's head ([`crate::land`]).
    pub fn merge(
        &self,
        base: ChangesetId,
        ours: ChangesetId,
        theirs: ChangesetId,
    ) -> Result<MergeOutcome, RepoError> {
        self.merge_with(base, ours, theirs, &merge_content)
    }

    /// [`Repo::merge_trees_with`] with the three-way text merge, [`merge_content`] (diff3): a
    /// land's incremental re-merge, after the target moved, of what it merged onto what the target
    /// gained ([`crate::land`]).
    pub fn merge_trees(
        &self,
        base: TreeId,
        ours: TreeId,
        theirs: TreeId,
        paths: &[RepoPath],
    ) -> Result<MergeOutcome, RepoError> {
        self.merge_trees_with(base, ours, theirs, paths, &merge_content)
    }

    /// [`Repo::merge_with`] of the three roots `[base, ours, theirs]`, read by the caller already:
    /// over the paths of the diff from `base` to `ours`, each tree read once, the diff's and the
    /// merge's alike. Nothing is written.
    pub(crate) fn merge_roots(
        &self,
        roots: [TreeId; 3],
        content: ContentMerger<'_>,
    ) -> Result<MergeOutcome, RepoError> {
        let reads = Reads::new(&self.objects);
        let changed = diff_trees(&reads, Some(roots[0]), Some(roots[1]), &PathFilter::all())?;
        let paths = changed.into_iter().map(|change| change.path).collect();
        merge_trees(&reads, roots, paths, content)
    }
}

/// The merge of what the second of `roots` (ours) changed since the first (the base) onto the
/// third (theirs), over `paths` and the paths below them.
fn merge_trees<O: ObjectStore>(
    reads: &Reads<'_, O>,
    roots: [TreeId; 3],
    paths: Vec<RepoPath>,
    content: ContentMerger<'_>,
) -> Result<MergeOutcome, RepoError> {
    let [base, ours, theirs] = roots;
    let filter = PathFilter::under(paths);
    let triples = three_way(reads, Some(base), Some(ours), Some(theirs), &filter)?;
    let plans: Vec<PathPlan> = triples.iter().map(plan_path).collect();
    let type_changes = (0..triples.len())
        .map(|at| type_change(reads, theirs, &triples, &plans, at))
        .collect::<Result<Vec<bool>, RepoError>>()?;
    let mut outcome = MergeOutcome::default();
    let mut held = Held {
        left: MAX_HELD_HUNKS,
    };
    for ((triple, plan), type_change) in triples.iter().zip(plans).zip(type_changes) {
        let plan = if type_change {
            PathPlan::Conflict(ConflictKind::TypeChange)
        } else {
            plan
        };
        match plan {
            PathPlan::Theirs => {}
            PathPlan::Put(leaf) => outcome.delta.push(put(&triple.path, leaf)),
            PathPlan::Delete => outcome.delta.push(DeltaEntry {
                path: triple.path.clone(),
                op: DeltaOp::Delete,
                renamed_from: None,
            }),
            PathPlan::MergeContent { kind } => {
                merge_contents(reads, triple, kind, content, &mut held, &mut outcome)?;
            }
            PathPlan::Conflict(kind) => outcome.conflicts.push(conflict(triple, kind)),
        }
    }
    Ok(outcome)
}

/// Whether the path of `triples[at]` is a file against a directory, which its three leaves do
/// not show ([the module documentation](self)): ours put a leaf where theirs holds none, and the
/// merge keeps a leaf of theirs below it or above it; or ours deleted the leaf, holds nothing
/// below it, and theirs holds a directory there. `plans` are the triples' plans.
///
/// The leaves below a path ours changed are all among the triples, theirs' as ours': the filter
/// takes the path and every path below it. A leaf of theirs above one is found in theirs' tree,
/// and the merge keeps it unless ours deleted it while theirs left it as the base had it.
fn type_change<O: ObjectStore>(
    reads: &Reads<'_, O>,
    theirs: TreeId,
    triples: &[PathTriple],
    plans: &[PathPlan],
    at: usize,
) -> Result<bool, RepoError> {
    let triple = &triples[at];
    if triple.ours == triple.base || triple.theirs.is_some() {
        return Ok(false);
    }
    let below = below(triples, &triple.path);
    let (below_triples, below_plans) = (&triples[below.clone()], &plans[below]);
    if triple.ours.is_none() {
        return Ok(below_triples.iter().all(|t| t.ours.is_none())
            && below_triples.iter().any(|t| t.theirs.is_some()));
    }
    let kept_below = below_triples
        .iter()
        .zip(below_plans)
        .any(|(t, plan)| t.theirs.is_some() && *plan != PathPlan::Delete);
    if kept_below {
        return Ok(true);
    }
    let mut above = triple.path.parent();
    while let Some(dir) = above {
        match entry_in(reads, theirs, &dir)? {
            Some(EntryTarget::Dir(_)) => return Ok(false),
            Some(EntryTarget::Leaf(..)) => {
                let deleted = triples
                    .binary_search_by(|t| t.path.cmp(&dir))
                    .is_ok_and(|at| plans[at] == PathPlan::Delete);
                return Ok(!deleted);
            }
            // Nothing there: a directory to make, or a file higher up.
            None => above = dir.parent(),
        }
    }
    Ok(false)
}

/// Where the triples of the paths below `path` lie in `triples`, which are sorted by path bytes:
/// those paths start with `path` and a `/`, and sort before `path` and a `0`, the byte after `/`.
fn below(triples: &[PathTriple], path: &RepoPath) -> Range<usize> {
    let (from, to) = (format!("{path}/"), format!("{path}0"));
    let start = triples.partition_point(|t| t.path.as_str() < from.as_str());
    let end = triples.partition_point(|t| t.path.as_str() < to.as_str());
    start..end
}

/// Merges the contents of `triple`'s path, which both sides changed, with `content`, into
/// `outcome`: clean contents make a leaf of `kind`, put unless theirs holds it, and a new blob and
/// an auto-merged path unless ours or theirs holds those contents; a conflict between texts is
/// [`ConflictKind::Content`] (or [`ConflictKind::AddAdd`], over the empty base of a path the base
/// lacks), its hunks as `held` holds them, and contents that are not all text
/// [`ConflictKind::Binary`].
fn merge_contents<O: ObjectStore>(
    reads: &Reads<'_, O>,
    triple: &PathTriple,
    kind: LeafKind,
    content: ContentMerger<'_>,
    held: &mut Held,
    outcome: &mut MergeOutcome,
) -> Result<(), RepoError> {
    let read =
        |leaf: Option<Leaf>| leaf.map_or_else(|| Ok(Vec::new()), |leaf| reads.blob(leaf.blob));
    let (base, ours, theirs) = (read(triple.base)?, read(triple.ours)?, read(triple.theirs)?);
    match content(&base, &ours, &theirs) {
        ContentMerge::Clean(merged) => {
            let blob = BlobId(hash_object(ObjectKind::Blob, &merged));
            let leaf = Leaf { kind, blob };
            if Some(leaf) == triple.theirs {
                return Ok(());
            }
            outcome.delta.push(put(&triple.path, leaf));
            let sides = [triple.ours, triple.theirs].map(|side| side.map(|leaf| leaf.blob));
            if !sides.contains(&Some(blob)) {
                outcome.new_blobs.insert(blob, merged);
                outcome.auto_merged.push(triple.path.clone());
            }
        }
        ContentMerge::Conflicted(hunks) => {
            let hunks = held.hold(hunks, [&base, &ours, &theirs]);
            let kind = if triple.base.is_none() {
                ConflictKind::AddAdd { hunks }
            } else {
                ConflictKind::Content { hunks }
            };
            outcome.conflicts.push(conflict(triple, kind));
        }
        ContentMerge::Binary => outcome
            .conflicts
            .push(conflict(triple, ConflictKind::Binary)),
    }
    Ok(())
}

/// How many more conflict hunks a merge holds as they are made ([`MAX_HELD_HUNKS`] at first).
struct Held {
    /// The hunks that still go in as they are.
    left: usize,
}

impl Held {
    /// `hunks`, one file's in file order, as the merge holds them: the first while it holds fewer
    /// than [`MAX_HELD_HUNKS`] in all, then one spanning the rest, over the base's, ours' and
    /// theirs' contents `sides` ([`span`]). A single hunk left over spans itself.
    fn hold(&mut self, mut hunks: Vec<ConflictHunk>, sides: [&[u8]; 3]) -> Vec<ConflictHunk> {
        let kept = hunks.len().min(self.left);
        self.left -= kept;
        // Two or more past the budget: one hunk in their place, and their bytes freed.
        if let [first, .., last] = &hunks[kept..] {
            let spanning = span(first, last, sides);
            hunks.truncate(kept);
            hunks.push(spanning);
            hunks.shrink_to_fit();
        }
        hunks
    }
}

/// The hunk that spans the hunks from `first` to `last` of a file, in file order, over the
/// base's, ours' and theirs' contents `sides`: each side's lines from `first`'s start to `last`'s
/// end, with their bytes. Three edits a `}` apart, say, the last two past the budget:
///
/// ```text
/// line   0   1  2   3  4
/// base   l0  }  l1  }  l2      held: l0 | o0 | t0 as made,
/// ours   o0  }  o1  }  o2            then lines 2..5 of each side: "l1 } l2", "o1 } o2", "t1 } t2"
/// theirs t0  }  t1  }  t2
/// ```
fn span(first: &ConflictHunk, last: &ConflictHunk, sides: [&[u8]; 3]) -> ConflictHunk {
    let [base, ours, theirs] = [
        first.base.start..last.base.end,
        first.ours.start..last.ours.end,
        first.theirs.start..last.theirs.end,
    ];
    let [base_bytes, ours_bytes, theirs_bytes] = sides;
    ConflictHunk {
        base_text: lines_text(base_bytes, &base),
        ours_text: lines_text(ours_bytes, &ours),
        theirs_text: lines_text(theirs_bytes, &theirs),
        base,
        ours,
        theirs,
    }
}

/// The bytes of `content`'s lines `lines`, numbered from 0, each up to and including its `\n` (a
/// last line may have none), as [`split_lines`](line_diff::split_lines) splits them; a line past
/// the content's end has none.
fn lines_text(content: &[u8], lines: &Range<u32>) -> Vec<u8> {
    let mut each = content.split_inclusive(|&byte| byte == b'\n');
    let before: usize = (each.by_ref().take(lines.start as usize))
        .map(<[u8]>::len)
        .sum();
    let within: usize = each.take(lines.len()).map(<[u8]>::len).sum();
    content[before..before + within].to_vec()
}

/// The delta entry that puts `leaf` at `path`.
fn put(path: &RepoPath, leaf: Leaf) -> DeltaEntry {
    DeltaEntry {
        path: path.clone(),
        op: DeltaOp::Put {
            kind: leaf.kind,
            id: leaf.blob,
        },
        renamed_from: None,
    }
}

/// The conflict of `kind` at `triple`'s path, with its three leaves.
fn conflict(triple: &PathTriple, kind: ConflictKind) -> FileConflict {
    FileConflict {
        path: triple.path.clone(),
        kind,
        base: triple.base,
        ours: triple.ours,
        theirs: triple.theirs,
    }
}

/// The objects one merge reads: each tree once, however often its walks pass it (the second diff
/// of ours in [`three_way`], the walks up theirs' paths), each verified against its id as every
/// object read is; and the blobs of the paths both sides changed.
struct Reads<'o, O: ObjectStore> {
    objects: &'o O,
    trees: RefCell<HashMap<TreeId, Tree>>,
}

impl<'o, O: ObjectStore> Reads<'o, O> {
    /// Nothing read yet from `objects`.
    fn new(objects: &'o O) -> Self {
        Self {
            objects,
            trees: RefCell::new(HashMap::new()),
        }
    }

    /// The bytes of blob `id`, verified; [`RepoError::MissingObject`] if the store lacks it, or
    /// holds an object of another kind under that id.
    fn blob(&self, id: BlobId) -> Result<Vec<u8>, RepoError> {
        repo::object(self.objects, id.0, ObjectKind::Blob)
    }
}

/// Each tree read once, then kept; [`RepoError::MissingObject`] for a tree the store lacks.
impl<O: ObjectStore> TreeRead for Reads<'_, O> {
    type Error = RepoError;

    fn tree(&self, id: TreeId) -> Result<Tree, RepoError> {
        let kept = self.trees.borrow().get(&id).cloned();
        if let Some(tree) = kept {
            return Ok(tree);
        }
        let tree = repo::tree(self.objects, id)?;
        self.trees.borrow_mut().insert(id, tree.clone());
        Ok(tree)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One hunk at line `at` of each side, its texts `side` and `at` on a line.
    fn hunk(at: u32) -> ConflictHunk {
        let text = |side: &str| format!("{side}{at}\n").into_bytes();
        ConflictHunk {
            base: at..at + 1,
            ours: at..at + 1,
            theirs: at..at + 1,
            base_text: text("l"),
            ours_text: text("o"),
            theirs_text: text("t"),
        }
    }

    /// `n` lines of `side`, each `side` and its number.
    fn content(side: &str, n: u32) -> Vec<u8> {
        (0..n)
            .flat_map(|at| format!("{side}{at}\n").into_bytes())
            .collect()
    }

    /// A merge holds its first [`MAX_HELD_HUNKS`] hunks as they were made, over its files: the
    /// file that passes them holds one spanning the rest, a later file one spanning all of its
    /// own, and a later file of one hunk that hunk.
    #[test]
    fn a_merge_holds_its_first_hunks_then_spans() {
        let n = u32::try_from(MAX_HELD_HUNKS).unwrap() + 2;
        let [base, ours, theirs] = ["l", "o", "t"].map(|side| content(side, n));
        let sides = [&base[..], &ours, &theirs];
        let mut held = Held {
            left: MAX_HELD_HUNKS,
        };

        let first = held.hold((0..n).map(hunk).collect(), sides);
        let spanned = ConflictHunk {
            base: n - 2..n,
            ours: n - 2..n,
            theirs: n - 2..n,
            base_text: lines_text(&base, &(n - 2..n)),
            ours_text: lines_text(&ours, &(n - 2..n)),
            theirs_text: lines_text(&theirs, &(n - 2..n)),
        };
        let mut expected: Vec<ConflictHunk> = (0..n - 2).map(hunk).collect();
        expected.push(spanned);
        assert_eq!(first, expected);
        assert_eq!(
            first[first.len() - 1].ours_text,
            format!("o{}\no{}\n", n - 2, n - 1).into_bytes()
        );

        let later = held.hold((0..3).map(hunk).collect(), sides);
        assert_eq!(later.len(), 1);
        assert_eq!(
            (later[0].base.clone(), later[0].theirs_text.as_slice()),
            (0..3, b"t0\nt1\nt2\n".as_slice())
        );
        assert_eq!(held.hold(vec![hunk(5)], sides), [hunk(5)]);
    }

    /// The bytes of lines: a last line without its `\n` is a line, and lines past the end, or
    /// none, have no bytes.
    #[test]
    fn lines_text_takes_whole_lines() {
        let content = b"a\nbb\nccc";
        assert_eq!(lines_text(content, &(0..1)), b"a\n");
        assert_eq!(lines_text(content, &(1..3)), b"bb\nccc");
        assert_eq!(lines_text(content, &(2..9)), b"ccc");
        assert_eq!(lines_text(content, &(3..5)), b"");
        assert_eq!(lines_text(content, &(1..1)), b"");
        assert_eq!(lines_text(b"", &(0..2)), b"");
    }
}
