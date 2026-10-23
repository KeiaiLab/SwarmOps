//! The tree diff (storage spec §4): the leaf-level changes between two trees, their canonical
//! delta, and the three-way path entries a merge starts from.
//!
//! Trees come through [`TreeRead`], so that the caller decides where they are read from: the
//! repository engine's snapshot of a store (M2's `diff`), or the trees M3's importer has just
//! built in memory. Nothing here does I/O of its own.
//!
//! [`diff_trees`] walks the two trees together, directory by directory, and short-circuits on
//! their Merkle ids: a subtree both sides share is never read, and neither is a directory the
//! [`PathFilter`] does not reach. Directories are expanded into their leaves, so a change is
//! always a leaf's ([`TreeChange`]), and the changes come sorted by path bytes, the order a
//! changeset's delta is in. [`delta_of`] turns them into that delta, and [`three_way`] pairs
//! two diffs from one base into the entries a three-way merge decides path by path (M4).

use std::cmp::Ordering;

use swarmops_core::{
    BlobId, DeltaEntry, DeltaOp, EntryTarget, LeafKind, RepoPath, Tree, TreeEntry, TreeId,
};

/// Where [`diff_trees`] and [`three_way`] read trees from.
pub trait TreeRead {
    /// Why a tree could not be read.
    type Error;

    /// The tree whose id is `id`.
    fn tree(&self, id: TreeId) -> Result<Tree, Self::Error>;
}

/// A leaf as a tree entry names it: a file, an executable or a symlink, and its blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct Leaf {
    /// File, executable or symlink.
    pub kind: LeafKind,
    /// The content (a symlink's: its target).
    pub blob: BlobId,
}

impl Leaf {
    /// The leaf of this kind and blob.
    pub fn new(kind: LeafKind, blob: BlobId) -> Self {
        Self { kind, blob }
    }
}

/// What happened to the leaf at one path, from one tree to the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ChangeKind {
    /// No leaf was there before (nothing, or a directory); `new` is there now.
    Added {
        /// The leaf there now.
        new: Leaf,
    },
    /// `old` was there; no leaf is now (nothing, or a directory).
    Deleted {
        /// The leaf that was there.
        old: Leaf,
    },
    /// The leaf's content or its exec bit changed: both leaves are files or executables, or
    /// both are symlinks.
    Modified {
        /// The leaf that was there.
        old: Leaf,
        /// The leaf there now.
        new: Leaf,
    },
    /// A symlink became a file or an executable, or one of those a symlink.
    TypeChanged {
        /// The leaf that was there.
        old: Leaf,
        /// The leaf there now.
        new: Leaf,
    },
}

impl ChangeKind {
    /// The leaf the path held before: `None` for an addition.
    pub fn before(self) -> Option<Leaf> {
        match self {
            Self::Added { .. } => None,
            Self::Deleted { old } | Self::Modified { old, .. } | Self::TypeChanged { old, .. } => {
                Some(old)
            }
        }
    }

    /// The leaf the path holds after: `None` for a deletion.
    pub fn after(self) -> Option<Leaf> {
        match self {
            Self::Deleted { .. } => None,
            Self::Added { new } | Self::Modified { new, .. } | Self::TypeChanged { new, .. } => {
                Some(new)
            }
        }
    }
}

/// The change of the leaf at one path.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct TreeChange {
    /// The leaf's full path from the root.
    pub path: RepoPath,
    /// What happened to it.
    pub kind: ChangeKind,
}

impl TreeChange {
    /// The change `kind` of the leaf at `path`.
    pub fn new(path: RepoPath, kind: ChangeKind) -> Self {
        Self { path, kind }
    }
}

/// The paths a diff looks at: every path ([`PathFilter::all`]), or those that equal or lie below
/// one of some prefixes ([`PathFilter::under`]).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PathFilter {
    /// `None` for every path; else the prefixes, sorted by bytes, each once.
    prefixes: Option<Vec<RepoPath>>,
}

impl PathFilter {
    /// Every path.
    pub fn all() -> Self {
        Self { prefixes: None }
    }

    /// The paths that equal one of `prefixes` or lie below one (`src` matches `src` and
    /// `src/a.rs`, never `src-old` or `srcs`); a prefix that is a file matches that file. With
    /// no prefix, no path matches.
    pub fn under(mut prefixes: Vec<RepoPath>) -> Self {
        prefixes.sort_unstable();
        prefixes.dedup();
        Self {
            prefixes: Some(prefixes),
        }
    }

    /// Whether `path` is one of the paths this filter looks at.
    pub fn matches(&self, path: &RepoPath) -> bool {
        let Some(prefixes) = &self.prefixes else {
            return true;
        };
        // The path itself, then each directory above it.
        let mut at = path.as_str();
        loop {
            if prefixes.binary_search_by(|p| p.as_str().cmp(at)).is_ok() {
                return true;
            }
            match at.rfind('/') {
                Some(slash) => at = &at[..slash],
                None => return false,
            }
        }
    }

    /// The prefixes, sorted by bytes, each once; `None` for every path ([`PathFilter::all`]).
    pub fn prefixes(&self) -> Option<&[RepoPath]> {
        self.prefixes.as_deref()
    }
}

/// What a filter reaches of one directory.
#[derive(Debug, Clone, Copy)]
enum Scope<'f> {
    /// All of it: the directory equals or lies below a prefix, or the filter takes every path.
    All,
    /// The paths below the directory that equal or lie below one of `prefixes`. Every one of
    /// them starts with the directory's path and a `/` (nothing at the root): `skip` bytes, which
    /// the directory's entries do not have. They are sorted by bytes, and so is what follows
    /// those `skip` bytes.
    Below {
        prefixes: &'f [RepoPath],
        skip: usize,
    },
}

impl<'f> Scope<'f> {
    /// What `filter` reaches of the root; `None` for nothing (a filter with no prefix).
    fn root(filter: &'f PathFilter) -> Option<Self> {
        match filter.prefixes.as_deref() {
            None => Some(Self::All),
            Some([]) => None,
            Some(prefixes) => Some(Self::Below { prefixes, skip: 0 }),
        }
    }

    /// What this directory's scope reaches of its entry `name`: all of it (the entry's path
    /// matches, and so does every path below it), some of the paths below it, or nothing.
    fn entry(self, name: &str) -> Option<Self> {
        let Self::Below { prefixes, skip } = self else {
            return Some(Self::All);
        };
        let name = name.as_bytes();
        if prefixes
            .binary_search_by(|p| past(p, skip).cmp(name))
            .is_ok()
        {
            return Some(Self::All);
        }
        let start =
            prefixes.partition_point(|p| against_dir(past(p, skip), name) == Ordering::Less);
        let below = &prefixes[start..];
        let len = below.partition_point(|p| against_dir(past(p, skip), name) == Ordering::Equal);
        (len > 0).then(|| Self::Below {
            prefixes: &below[..len],
            skip: skip + name.len() + 1,
        })
    }
}

/// The bytes of `prefix` past its first `skip`.
fn past(prefix: &RepoPath, skip: usize) -> &[u8] {
    prefix.as_str().as_bytes().get(skip..).unwrap_or_default()
}

/// Where `rest` sorts against the paths below the directory `name`, those that start with
/// `name/`: before them (`Less`), among them (`Equal`) or after them (`Greater`).
fn against_dir(rest: &[u8], name: &[u8]) -> Ordering {
    let mut rest = rest.iter();
    for want in name.iter().chain(std::iter::once(&b'/')) {
        let Some(got) = rest.next() else {
            return Ordering::Less;
        };
        match got.cmp(want) {
            Ordering::Equal => {}
            other => return other,
        }
    }
    Ordering::Equal
}

/// The leaves that differ between `from` and `to`, each path once, sorted by path bytes (not in
/// tree order: `a-c` comes before `a/b`), within `filter`.
///
/// `None` is the empty tree, which is never read. A subtree with the same id on both sides is
/// never read, nor is a directory `filter` does not reach. A leaf on both sides with another
/// kind or blob is [`ChangeKind::Modified`] when both are files or executables, or both
/// symlinks, and [`ChangeKind::TypeChanged`] when exactly one is a symlink. A leaf on one side
/// and a directory on the other is the leaf deleted (or added) and every leaf of the directory
/// added (or deleted): a file that becomes a directory `a` → `a/x`, `a/y` is `Deleted a`,
/// `Added a/x`, `Added a/y`.
///
/// The first error of a read is the error.
pub fn diff_trees<R: TreeRead>(
    read: &R,
    from: Option<TreeId>,
    to: Option<TreeId>,
    filter: &PathFilter,
) -> Result<Vec<TreeChange>, R::Error> {
    let mut changes = Vec::new();
    let Some(scope) = Scope::root(filter) else {
        return Ok(changes);
    };
    // The directories still to diff: an explicit stack, so that no depth of nesting recurses.
    let mut dirs = vec![Dir {
        path: String::new(),
        from,
        to,
        scope,
    }];
    while let Some(dir) = dirs.pop() {
        if dir.from == dir.to {
            // The same subtree on both sides (or none on either): nothing below it changed.
            continue;
        }
        let old = dir.from.map(|id| read.tree(id)).transpose()?;
        let new = dir.to.map(|id| read.tree(id)).transpose()?;
        let side_by_side = Pairs {
            old: old.as_ref().map_or(&[], Tree::entries),
            new: new.as_ref().map_or(&[], Tree::entries),
        };
        for (name, before, after) in side_by_side {
            if before == after {
                continue;
            }
            let Some(scope) = dir.scope.entry(name) else {
                continue;
            };
            // A leaf here changes only if the filter takes its own path: a scope that reaches
            // only some paths below this entry reaches no leaf at it.
            let change = match scope {
                Scope::All => leaf_change(leaf(before), leaf(after)),
                Scope::Below { .. } => None,
            };
            let (from, to) = (subtree(before), subtree(after));
            if from.is_none() && to.is_none() {
                if let Some(kind) = change {
                    let path = repo_path(join(&dir.path, name));
                    changes.push(TreeChange { path, kind });
                }
                continue;
            }
            let path = join(&dir.path, name);
            if let Some(kind) = change {
                let path = repo_path(path.clone());
                changes.push(TreeChange { path, kind });
            }
            dirs.push(Dir {
                path,
                from,
                to,
                scope,
            });
        }
    }
    changes.sort_unstable_by(|a, b| a.path.cmp(&b.path));
    Ok(changes)
}

/// One directory still to diff.
struct Dir<'f> {
    /// Its path; empty for the root.
    path: String,
    /// Its tree in `from`; `None` where `from` has no directory there.
    from: Option<TreeId>,
    /// Its tree in `to`; `None` where `to` has no directory there.
    to: Option<TreeId>,
    /// What the filter reaches of it.
    scope: Scope<'f>,
}

/// The entries of two directories side by side, by name bytes (the order of a tree's entries):
/// each name once, with what it names on each side, `None` on a side that lacks it.
struct Pairs<'t> {
    old: &'t [TreeEntry],
    new: &'t [TreeEntry],
}

impl<'t> Iterator for Pairs<'t> {
    type Item = (&'t str, Option<EntryTarget>, Option<EntryTarget>);

    fn next(&mut self) -> Option<Self::Item> {
        match (self.old.split_first(), self.new.split_first()) {
            (None, None) => None,
            (Some((a, old)), None) => {
                self.old = old;
                Some((&a.name, Some(a.target), None))
            }
            (None, Some((b, new))) => {
                self.new = new;
                Some((&b.name, None, Some(b.target)))
            }
            (Some((a, old)), Some((b, new))) => match a.name.as_bytes().cmp(b.name.as_bytes()) {
                Ordering::Less => {
                    self.old = old;
                    Some((&a.name, Some(a.target), None))
                }
                Ordering::Greater => {
                    self.new = new;
                    Some((&b.name, None, Some(b.target)))
                }
                Ordering::Equal => {
                    self.old = old;
                    self.new = new;
                    Some((&a.name, Some(a.target), Some(b.target)))
                }
            },
        }
    }
}

/// The leaf an entry names, if it names one.
fn leaf(target: Option<EntryTarget>) -> Option<Leaf> {
    match target? {
        EntryTarget::Leaf(kind, blob) => Some(Leaf { kind, blob }),
        EntryTarget::Dir(_) => None,
    }
}

/// The tree an entry names, if it names a directory.
fn subtree(target: Option<EntryTarget>) -> Option<TreeId> {
    match target? {
        EntryTarget::Dir(tree) => Some(tree),
        EntryTarget::Leaf(..) => None,
    }
}

/// The change from the leaf `before` to the leaf `after` at one path; `None` if they are the
/// same, or if neither is there.
fn leaf_change(before: Option<Leaf>, after: Option<Leaf>) -> Option<ChangeKind> {
    let is_symlink = |leaf: Leaf| leaf.kind == LeafKind::Symlink;
    match (before, after) {
        (None, None) => None,
        (None, Some(new)) => Some(ChangeKind::Added { new }),
        (Some(old), None) => Some(ChangeKind::Deleted { old }),
        (Some(old), Some(new)) if old == new => None,
        (Some(old), Some(new)) if is_symlink(old) == is_symlink(new) => {
            Some(ChangeKind::Modified { old, new })
        }
        (Some(old), Some(new)) => Some(ChangeKind::TypeChanged { old, new }),
    }
}

/// The path of the entry `name` of the directory at `dir` (empty for the root).
fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        return name.to_owned();
    }
    let mut path = String::with_capacity(dir.len() + 1 + name.len());
    path.push_str(dir);
    path.push('/');
    path.push_str(name);
    path
}

/// `path` as a [`RepoPath`]: it joins the names of tree entries, each one valid path component
/// (a [`Tree`] holds no other), so it is a valid path.
fn repo_path(path: String) -> RepoPath {
    RepoPath::new(path).expect("the names of tree entries join into a valid path")
}

/// The delta that `changes` make: an addition, a modification or a type change puts the new
/// leaf, a deletion deletes, in the order given, never a rename. The changes of a
/// [`diff_trees`] give a canonical delta, sorted by path bytes, each path once:
/// `delta_of(&diff_trees(read, parent_root, Some(root), &PathFilter::all())?)` is the delta of a
/// changeset whose first parent's root is `parent_root`.
pub fn delta_of(changes: &[TreeChange]) -> Vec<DeltaEntry> {
    changes
        .iter()
        .map(|change| DeltaEntry {
            path: change.path.clone(),
            op: match change.kind.after() {
                Some(new) => DeltaOp::Put {
                    kind: new.kind,
                    id: new.blob,
                },
                None => DeltaOp::Delete,
            },
            renamed_from: None,
        })
        .collect()
}

/// A path that a three-way merge decides: the leaf each of the three trees holds there.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct PathTriple {
    /// The leaf's full path from the root.
    pub path: RepoPath,
    /// The base's leaf there; `None` for none (nothing, or a directory).
    pub base: Option<Leaf>,
    /// Ours' leaf there; `None` for none.
    pub ours: Option<Leaf>,
    /// Theirs' leaf there; `None` for none.
    pub theirs: Option<Leaf>,
}

impl PathTriple {
    /// The path and the leaf each of the three trees holds there.
    pub fn new(
        path: RepoPath,
        base: Option<Leaf>,
        ours: Option<Leaf>,
        theirs: Option<Leaf>,
    ) -> Self {
        Self {
            path,
            base,
            ours,
            theirs,
        }
    }
}

/// Every leaf path, within `filter`, that `base` → `ours` or `base` → `theirs` changes, with the
/// leaf each of the three holds there, sorted by path bytes. A path both sides changed the same
/// way is listed too: the merge decides it.
///
/// It reads what `diff_trees(read, base, ours, filter)` and `diff_trees(read, base, theirs,
/// filter)` read, and nothing else: a subtree neither side changed is never read.
pub fn three_way<R: TreeRead>(
    read: &R,
    base: Option<TreeId>,
    ours: Option<TreeId>,
    theirs: Option<TreeId>,
    filter: &PathFilter,
) -> Result<Vec<PathTriple>, R::Error> {
    let ours = diff_trees(read, base, ours, filter)?;
    let theirs = diff_trees(read, base, theirs, filter)?;
    // Both are sorted by path, each path once: merge them. A side that did not change a path
    // holds the base's leaf there.
    let mut triples = Vec::with_capacity(ours.len().max(theirs.len()));
    let mut ours = ours.into_iter().peekable();
    let mut theirs = theirs.into_iter().peekable();
    loop {
        let order = match (ours.peek(), theirs.peek()) {
            (None, None) => break,
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (Some(o), Some(t)) => o.path.cmp(&t.path),
        };
        let triple = match order {
            Ordering::Less => ours.next().map(|o| PathTriple {
                base: o.kind.before(),
                ours: o.kind.after(),
                theirs: o.kind.before(),
                path: o.path,
            }),
            Ordering::Greater => theirs.next().map(|t| PathTriple {
                base: t.kind.before(),
                ours: t.kind.before(),
                theirs: t.kind.after(),
                path: t.path,
            }),
            Ordering::Equal => ours.next().zip(theirs.next()).map(|(o, t)| PathTriple {
                base: o.kind.before(),
                ours: o.kind.after(),
                theirs: t.kind.after(),
                path: o.path,
            }),
        };
        triples.extend(triple);
    }
    Ok(triples)
}
