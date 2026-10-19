//! Editing a tree in memory, the heart of a commit (storage spec §4): a delta is applied to a
//! base root tree, and the directories it changed are encoded back into tree objects.
//!
//! A directory is read from its tree object the first time a change reaches into it, and is
//! edited as a sorted map of its entries; directories no change reaches stay unread, named by
//! their tree id. The edited directories live in an arena, the root first. A directory joins
//! the arena when a change first reaches into it, always after its parent, so encoding the
//! arena from its end encodes children before parents ([`TreeEdit::finish`]).
//!
//! The deletes of a delta apply first, then its puts ([`TreeEdit::apply`]), so that one delta
//! can turn a file into a directory or a directory back into a file. A delete that leaves a
//! directory empty removes that directory from its parent at once, and so on up: a tree is
//! never empty, except the root's.

use std::collections::{BTreeMap, HashSet};

use swarmops_core::{
    BlobId, DeltaEntry, DeltaOp, EntryTarget, Hashed, LeafKind, ObjectKind, RepoPath, Tree,
    TreeEntry, TreeId,
};
use swarmops_store::ObjectStore;

use crate::error::RepoError;
use crate::repo;

/// A tree being edited: the base root tree with changes applied, in memory.
pub(crate) struct TreeEdit<'o, O: ObjectStore> {
    objects: &'o O,
    /// The directories changes have reached into; the root is `dirs[0]`.
    dirs: Vec<Dir>,
}

/// What [`TreeEdit::finish`] encoded.
pub(crate) struct Encoded {
    /// The root tree's id.
    pub(crate) root: TreeId,
    /// The trees that differ from the ones they were read from, each once, children first:
    /// their bytes with the ids they hash to.
    pub(crate) changed: Vec<Hashed>,
}

/// One directory a change has reached into.
struct Dir {
    /// Its entries by name; a `BTreeMap` of `String`s orders them by name bytes, as a tree.
    entries: BTreeMap<String, Slot>,
    /// The tree it was read from; `None` for a directory the changes made.
    read_from: Option<TreeId>,
}

/// One entry of a [`Dir`].
#[derive(Debug, Clone, Copy)]
enum Slot {
    /// A file, an executable or a symlink.
    Leaf { kind: LeafKind, id: BlobId },
    /// A directory no change has reached into: its tree, unread.
    Unread(TreeId),
    /// A directory a change has reached into: its place in the arena.
    Edited(usize),
}

impl<'o, O: ObjectStore> TreeEdit<'o, O> {
    /// An edit of tree `root`, read from `objects` (and its directories, as changes reach
    /// them).
    pub(crate) fn new(objects: &'o O, root: TreeId) -> Result<Self, RepoError> {
        let root = Dir::read(objects, root)?;
        Ok(Self {
            objects,
            dirs: vec![root],
        })
    }

    /// An edit of the empty tree: the base of a root changeset. Its root counts as made, not
    /// read, so [`TreeEdit::finish`] returns the root tree among the changed ones even if it
    /// stays empty: the store need not hold the empty tree yet.
    pub(crate) fn empty(objects: &'o O) -> Self {
        Self {
            objects,
            dirs: vec![Dir::made()],
        }
    }

    /// Applies `delta`, whose paths must be unique: every delete in order, then every put in
    /// order. Returns, for each entry, whether it changed the tree: a put of what its path
    /// holds already (the same kind and blob) does not.
    ///
    /// [`RepoError::TypeConflict`] for a put below a file, a put where a directory is, or a
    /// delete of a directory; [`RepoError::PathNotFound`] for a delete of a path that is no
    /// entry. An error leaves the edit half done: throw it away.
    pub(crate) fn apply(&mut self, delta: &[DeltaEntry]) -> Result<Vec<bool>, RepoError> {
        let mut changed = vec![false; delta.len()];
        for (at, entry) in delta.iter().enumerate() {
            if entry.op == DeltaOp::Delete {
                self.delete(&entry.path)?;
                changed[at] = true;
            }
        }
        for (at, entry) in delta.iter().enumerate() {
            if let DeltaOp::Put { kind, id } = entry.op {
                changed[at] = self.put(&entry.path, kind, id)?;
            }
        }
        Ok(changed)
    }

    /// Encodes the directories changes reached, children before parents, leaving out those
    /// that are empty (the root is encoded even then).
    pub(crate) fn finish(self) -> Result<Encoded, RepoError> {
        // A pruned directory stays in the arena, but its parent no longer names it.
        let mut reachable = vec![false; self.dirs.len()];
        reachable[0] = true;
        for (at, dir) in self.dirs.iter().enumerate() {
            if reachable[at] {
                for slot in dir.entries.values() {
                    if let Slot::Edited(child) = *slot {
                        reachable[child] = true;
                    }
                }
            }
        }
        let mut ids: Vec<Option<TreeId>> = vec![None; self.dirs.len()];
        let mut changed = Vec::new();
        let mut seen = HashSet::new();
        for (at, dir) in self.dirs.iter().enumerate().rev() {
            if !reachable[at] {
                continue;
            }
            let entries: Vec<TreeEntry> = dir
                .entries
                .iter()
                .filter_map(|(name, slot)| {
                    let target = match *slot {
                        Slot::Leaf { kind, id } => EntryTarget::Leaf(kind, id),
                        Slot::Unread(tree) => EntryTarget::Dir(tree),
                        // Children come later in the arena: encoded already, or left out.
                        Slot::Edited(child) => EntryTarget::Dir(ids[child]?),
                    };
                    let name = name.clone();
                    Some(TreeEntry { name, target })
                })
                .collect();
            let tree = match (entries.is_empty(), at) {
                (true, 0) => Tree::empty_root(),
                (true, _) => continue,
                (false, _) => Tree::new(entries)?,
            };
            let object = Hashed::new(ObjectKind::Tree, tree.encode());
            let id = TreeId(object.id());
            if dir.read_from != Some(id) && seen.insert(id) {
                changed.push(object);
            }
            ids[at] = Some(id);
        }
        let root = ids[0].expect("the root is always encoded");
        Ok(Encoded { root, changed })
    }

    /// Puts leaf `kind`/`id` at `path`, making the directories on the way that are missing.
    /// Returns whether that changed anything.
    fn put(&mut self, path: &RepoPath, kind: LeafKind, id: BlobId) -> Result<bool, RepoError> {
        let conflict = || RepoError::TypeConflict { path: path.clone() };
        let components: Vec<&str> = path.components().collect();
        let (&name, dirs) = components.split_last().expect("a path has a component");
        let mut at = 0;
        for &dir in dirs {
            at = match self.dirs[at].entries.get(dir).copied() {
                Some(Slot::Leaf { .. }) => return Err(conflict()),
                Some(Slot::Edited(child)) => child,
                Some(Slot::Unread(tree)) => {
                    let read = Dir::read(self.objects, tree)?;
                    self.add(at, dir, read)
                }
                None => self.add(at, dir, Dir::made()),
            };
        }
        let entries = &mut self.dirs[at].entries;
        match entries.get(name).copied() {
            Some(Slot::Unread(_) | Slot::Edited(_)) => Err(conflict()),
            Some(Slot::Leaf { kind: k, id: i }) if (k, i) == (kind, id) => Ok(false),
            _ => {
                entries.insert(name.to_owned(), Slot::Leaf { kind, id });
                Ok(true)
            }
        }
    }

    /// Deletes the leaf at `path`, then the directories that leaves empty, deepest first.
    fn delete(&mut self, path: &RepoPath) -> Result<(), RepoError> {
        let not_found = || RepoError::PathNotFound { path: path.clone() };
        let components: Vec<&str> = path.components().collect();
        let (&name, dirs) = components.split_last().expect("a path has a component");
        // Each directory on the way, as (its parent, its name there).
        let mut chain = Vec::with_capacity(dirs.len());
        let mut at = 0;
        for &dir in dirs {
            let child = match self.dirs[at].entries.get(dir).copied() {
                Some(Slot::Edited(child)) => child,
                Some(Slot::Unread(tree)) => {
                    let read = Dir::read(self.objects, tree)?;
                    self.add(at, dir, read)
                }
                Some(Slot::Leaf { .. }) | None => return Err(not_found()),
            };
            chain.push((at, dir));
            at = child;
        }
        match self.dirs[at].entries.get(name) {
            Some(Slot::Leaf { .. }) => {
                self.dirs[at].entries.remove(name);
            }
            Some(Slot::Unread(_) | Slot::Edited(_)) => {
                return Err(RepoError::TypeConflict { path: path.clone() });
            }
            None => return Err(not_found()),
        }
        for (parent, dir) in chain.into_iter().rev() {
            if !self.dirs[at].entries.is_empty() {
                break;
            }
            self.dirs[parent].entries.remove(dir);
            at = parent;
        }
        Ok(())
    }

    /// Adds `dir` to the arena as entry `name` of directory `parent`; returns its place.
    fn add(&mut self, parent: usize, name: &str, dir: Dir) -> usize {
        let child = self.dirs.len();
        self.dirs.push(dir);
        self.dirs[parent]
            .entries
            .insert(name.to_owned(), Slot::Edited(child));
        child
    }
}

impl Dir {
    /// A directory the changes make, empty until a put fills it.
    fn made() -> Self {
        Self {
            entries: BTreeMap::new(),
            read_from: None,
        }
    }

    /// Directory `tree`, read from `objects`.
    fn read<O: ObjectStore>(objects: &O, tree: TreeId) -> Result<Self, RepoError> {
        let entries = repo::tree(objects, tree)?
            .entries()
            .iter()
            .map(|entry| {
                let slot = match entry.target {
                    EntryTarget::Leaf(kind, id) => Slot::Leaf { kind, id },
                    EntryTarget::Dir(tree) => Slot::Unread(tree),
                };
                (entry.name.clone(), slot)
            })
            .collect();
        Ok(Self {
            entries,
            read_from: Some(tree),
        })
    }
}
