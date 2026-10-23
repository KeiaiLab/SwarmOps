//! git's objects converted into SwarmOps': a commit into the fields of a changeset, its trees
//! into trees and its blobs into blobs; and the fidelity check, that a commit converted without
//! loss renders back to its own git id (storage spec §7).
//!
//! The mapping. A tree entry of mode `100644` is a file, `100755` an executable, `120000` a
//! symlink (its blob holds the target) and `40000` a directory, and a blob's bytes are kept as
//! they are. What a changeset cannot hold as git has it belongs to a [`LossyClass`]: the
//! conversion refuses it ([`ConvertError::Lossy`], naming the commit, the path and the class)
//! unless the converter allows the class, and otherwise drops or normalizes it:
//!
//! - a gitlink, mode `160000` (a submodule's commit): dropped ([`LossyClass::Gitlinks`]);
//! - a name that is no path component (not UTF-8, `.`, `..`, empty, or holding a `/`): dropped
//!   ([`LossyClass::Paths`]);
//! - a mode of a file, a directory or a symlink written otherwise than git writes it, such as
//!   `100664`, `100600` or a zero-padded `040000`: taken by its file type, as git's `canon_mode`
//!   takes it, a regular file's an executable when its owner's execute bit is set and a file
//!   otherwise ([`LossyClass::Modes`]);
//! - a tree out of git's order, or holding a name twice: put in order, the first entry of a name
//!   kept ([`LossyClass::TreeOrder`]);
//! - an entry naming an empty tree: dropped ([`LossyClass::EmptyTrees`]);
//! - a commit whose fields render to other bytes than its own, or that lists a parent twice: as
//!   its fields hold it, each parent once ([`LossyClass::Nonstandard`]).
//!
//! An entry dropped counts under the class it is dropped for alone, and a directory that drops
//! leave empty is dropped with them, under their classes alone. Anything else is refused whatever
//! is allowed: a mode of no file, directory, symlink or gitlink ([`ConvertError::Mode`]), more
//! than 255 parents ([`ConvertError::Parents`]), a blob longer than an object holds
//! ([`ConvertError::TooLarge`], before it is read), a commit or a tree longer than that
//! ([`SourceError::TooLarge`], before it is read: each is read whole, to be parsed), bytes that
//! are no commit or tree, or an object of another kind than its name says
//! ([`ConvertError::Malformed`]), and what the source cannot read ([`ConvertError::Source`]).
//!
//! The fidelity check. Each commit converted is rendered back, every tree with [`tree_bytes`] and
//! the commit with [`commit_bytes`], and hashed: the hash is the commit's own id exactly when
//! nothing was lost ([`Converted::lossy`] empty), and any other outcome is [`ConvertError::Bug`].
//! A [`Converter`] converts each git tree and blob once, remembering what each became, and walks
//! trees with a stack of its own, never by recursion, however deep they are.
//!
//! [`tree_bytes`]: render::tree_bytes
//! [`commit_bytes`]: render::commit_bytes

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use swarmops_core::bytes;
use swarmops_core::{BlobId, LeafKind, ObjectKind, Signature, Tree, TreeEntry, TreeId};
use swarmops_store::MAX_OBJECT_LEN;

use crate::{
    GitOid, GitSource, LossyClass, LossySet, RawCommit, RawError, RawIdent, RawKind, RawTree,
    RawTreeEntry, SourceError,
};
use crate::{oid, render, source};

/// The metadata key of a converted changeset's git commit: its id, 40 lowercase hex digits.
pub const GIT_OID: &str = "git.oid";

/// The metadata key of the classes a converted changeset lost ([`LossySet::to_metadata`]), there
/// only when it lost any.
pub const GIT_LOSSY: &str = "git.lossy";

/// The most parents a changeset holds: its journal record counts them in one byte (issue 8).
const MAX_PARENTS: usize = 255;

/// The mode git writes for a directory.
const DIR_MODE: &[u8] = b"40000";

/// A git commit converted: the fields of the changeset it becomes, and what it lost.
///
/// The changeset's delta and its parents' changeset ids are the import's to make; the trees and
/// blobs its root names are in the `out` of [`Converter::commit`], or were made before.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Converted {
    /// The commit's git id.
    pub oid: GitOid,
    /// Its parents' git ids in their order, each once.
    pub parents: Vec<GitOid>,
    /// The tree its git tree became.
    pub root: TreeId,
    /// Its author, as its bytes hold it.
    pub author: Signature,
    /// Its committer, as its bytes hold it.
    pub committer: Signature,
    /// Its message, verbatim.
    pub message: Vec<u8>,
    /// Its header lines after the committer, in order (`encoding`, `gpgsig`, `mergetag`, …).
    pub extra_headers: Vec<(Vec<u8>, Vec<u8>)>,
    /// The classes of what it lost: empty exactly when it renders back to its own id.
    pub lossy: LossySet,
    /// The changeset's metadata, keys ascending: [`GIT_LOSSY`] when anything was lost, then
    /// [`GIT_OID`]. Nothing else, no `agent` among them: the same git commit is the same
    /// changeset in every store (the M3 plan's decision 15).
    pub metadata: Vec<(String, Vec<u8>)>,
}

/// Why a commit was not converted.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConvertError {
    /// The source could not read an object the commit needs: it lacks it, its bytes are damaged,
    /// or reading failed; or the commit, or a tree it needs, is longer than an object holds
    /// ([`SourceError::TooLarge`]), and was not read.
    #[error(transparent)]
    Source(#[from] SourceError),
    /// An object is no commit or tree the conversion can read, or is of another kind than its
    /// name says: a file's entry naming a tree, a commit's id a blob.
    #[error("git object {oid} is malformed: {what}")]
    Malformed {
        /// The object.
        oid: GitOid,
        /// What is wrong, in a few words.
        what: &'static str,
    },
    /// The commit holds what a changeset cannot hold as git has it, of a class the converter
    /// does not allow.
    #[error(
        "commit {commit} holds {class} content{}: {detail}; allowing the class drops or \
         normalizes it",
        at_path(.path)
    )]
    Lossy {
        /// The commit.
        commit: GitOid,
        /// The class.
        class: LossyClass,
        /// The entry's path, its names joined by `/`, each its bytes as git has them; empty for
        /// what is wrong with the commit's own bytes (`nonstandard`).
        path: Vec<u8>,
        /// What is wrong, for a person.
        detail: String,
    },
    /// The commit has more parents than a changeset holds (255).
    #[error("commit {commit} has {parents} parents, and a changeset holds at most 255")]
    Parents {
        /// The commit.
        commit: GitOid,
        /// How many parents it lists, each once.
        parents: usize,
    },
    /// A tree entry's mode is of no file, directory, symlink or gitlink.
    #[error(
        "commit {commit}: {} has the mode {}, of no file, directory, symlink or gitlink",
        .path.escape_ascii(),
        .mode.escape_ascii()
    )]
    Mode {
        /// The commit.
        commit: GitOid,
        /// The entry's path, as in [`ConvertError::Lossy`].
        path: Vec<u8>,
        /// The mode's digits, as stored.
        mode: Vec<u8>,
    },
    /// A blob is longer than an object holds ([`MAX_OBJECT_LEN`]), as its header says before
    /// it is read.
    #[error(
        "commit {commit}: the blob at {} is {size} bytes, and an object holds at most {max}",
        .path.escape_ascii(),
        max = MAX_OBJECT_LEN
    )]
    TooLarge {
        /// The commit.
        commit: GitOid,
        /// The entry's path, as in [`ConvertError::Lossy`].
        path: Vec<u8>,
        /// The blob's length.
        size: u64,
    },
    /// The commit lost nothing and yet does not render back to its own id, or lost something
    /// and does: a bug in the conversion, never a property of the commit.
    #[error(
        "commit {commit} does not render back as the conversion found it would: a bug in the \
         conversion"
    )]
    Bug {
        /// The commit.
        commit: GitOid,
    },
}

/// Converts the commits of a [`GitSource`], one call each, into the fields of changesets and the
/// trees and blobs they name.
///
/// It converts each git tree and blob once, and remembers what each became: a commit's
/// conversion reads only the trees and blobs no commit before it named, and appends to its
/// `out` only the trees and blobs not made before. It refuses what the classes it allows do not
/// cover (the [module](crate::convert) says how).
pub struct Converter<'s> {
    source: &'s dyn GitSource,
    allowed: LossySet,
    /// What each git tree converted to.
    trees: HashMap<GitOid, TreeMemo>,
    /// The blob each git blob converted to.
    blobs: HashMap<GitOid, BlobId>,
    /// The trees made: appended to an `out`, or held by the store ([`Converter::remember_tree`]).
    made: HashSet<TreeId>,
    /// The id of the empty root, which a commit whose tree keeps nothing has.
    empty_root: TreeId,
}

/// What a git tree converted to.
#[derive(Clone, Copy, Debug)]
struct TreeMemo {
    /// The tree it became; `None` when it keeps no entry (an empty tree in git, or one whose
    /// entries were all dropped), which its parent drops and a commit holds as the empty root.
    tree: Option<TreeId>,
    /// The id of the git tree it renders back to: its own when nothing in it was lost.
    rendered: GitOid,
    /// The classes of what was lost in it, and under it.
    classes: LossySet,
}

/// What one commit's conversion made: the converter takes it once the conversion succeeds, so a
/// refused commit leaves the converter and its `out` as they were.
#[derive(Default)]
struct Pending {
    trees: HashMap<GitOid, TreeMemo>,
    blobs: HashMap<GitOid, BlobId>,
    made: HashSet<TreeId>,
    objects: Vec<(ObjectKind, Vec<u8>)>,
}

/// A git tree the walk is in: the entries it has not looked at, and what it keeps so far.
struct Frame {
    oid: GitOid,
    entries: std::vec::IntoIter<RawTreeEntry>,
    /// The entries kept, each with the git id it renders to.
    kept: Vec<(TreeEntry, GitOid)>,
    /// The classes of what it lost so far.
    classes: LossySet,
    /// The entry of its parent that names it; `None` for the commit's tree.
    named: Option<Named>,
}

/// A directory entry, while the walk is in the tree it names.
struct Named {
    name: String,
    /// Its mode's digits, as stored.
    mode: Vec<u8>,
    /// The length of the walk's path in the parent, which it goes back to.
    parent_path: usize,
}

/// What a tree entry's mode names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Leaf(LeafKind),
    Dir,
    Gitlink,
}

/// Where in a commit a refusal is: the commit, and the entry's directory and name (an empty name
/// for the directory itself, both empty for the commit's own bytes).
#[derive(Clone, Copy)]
struct At<'p> {
    commit: GitOid,
    dir: &'p [u8],
    name: &'p [u8],
}

impl At<'_> {
    /// The commit's own bytes.
    fn commit(commit: GitOid) -> At<'static> {
        At {
            commit,
            dir: &[],
            name: &[],
        }
    }

    /// The path a refusal names: the directory and the name joined by `/`.
    fn path(self) -> Vec<u8> {
        let mut path = self.dir.to_vec();
        push_name(&mut path, self.name);
        path
    }
}

impl<'s> Converter<'s> {
    /// A converter of `source`'s commits that drops or normalizes the classes `allowed` holds
    /// and refuses the others.
    pub fn new(source: &'s dyn GitSource, allowed: LossySet) -> Converter<'s> {
        Converter {
            source,
            allowed,
            trees: HashMap::new(),
            blobs: HashMap::new(),
            made: HashSet::new(),
            empty_root: Tree::empty_root().id(),
        }
    }

    /// Converts the commit `oid`: the changeset fields it becomes. The blobs and trees its tree
    /// becomes that this converter has not made before are appended to `out`, each once, children
    /// before the trees that name them; those made before are not.
    ///
    /// A refused commit appends nothing, and leaves the converter as it was.
    pub fn commit(
        &mut self,
        oid: GitOid,
        out: &mut Vec<(ObjectKind, Vec<u8>)>,
    ) -> Result<Converted, ConvertError> {
        let mut pending = Pending::default();
        let converted = self.convert(&mut pending, oid)?;
        self.trees.extend(pending.trees);
        self.blobs.extend(pending.blobs);
        self.made.extend(pending.made);
        out.append(&mut pending.objects);
        Ok(converted)
    }

    /// Takes the git tree `git` to have converted without loss to `tree`, which the store holds:
    /// a commit naming it is converted without reading it, and `tree` is never appended to an
    /// `out`. The import tells it so of what a faithful changeset of the store holds.
    pub fn remember_tree(&mut self, git: GitOid, tree: TreeId) {
        // The empty tree keeps no entry: a commit holds it as its root, a parent drops it.
        let kept = (tree != self.empty_root).then_some(tree);
        let memo = TreeMemo {
            tree: kept,
            rendered: git,
            classes: LossySet::empty(),
        };
        self.trees.insert(git, memo);
        self.made.insert(tree);
    }

    /// Takes the git blob `git` to be the blob `blob`, which the store holds: an entry naming it
    /// is converted without reading it, and the blob is never appended to an `out`.
    pub fn remember_blob(&mut self, git: GitOid, blob: BlobId) {
        self.blobs.insert(git, blob);
    }

    fn convert(&self, pending: &mut Pending, oid: GitOid) -> Result<Converted, ConvertError> {
        let (kind, raw) = source::read_bounded(self.source, oid)?;
        if kind != RawKind::Commit {
            return Err(ConvertError::Malformed {
                oid,
                what: "named as a commit, but it is no commit",
            });
        }
        let commit = RawCommit::parse(&raw).map_err(|error| malformed(oid, &error))?;
        let parents = each_once(&commit.parents);
        if parents.len() > MAX_PARENTS {
            return Err(ConvertError::Parents {
                commit: oid,
                parents: parents.len(),
            });
        }
        let mut lossy = LossySet::empty();
        let own = At::commit(oid);
        if parents.len() < commit.parents.len() {
            let detail = || "a parent listed twice".to_owned();
            self.allow(&mut lossy, LossyClass::Nonstandard, own, detail)?;
        } else {
            let rendered = commit.render();
            if rendered != raw {
                let detail = || unlike(&raw, &rendered);
                self.allow(&mut lossy, LossyClass::Nonstandard, own, detail)?;
            }
        }
        let tree = self.tree(pending, oid, commit.tree)?;
        lossy = lossy.union(tree.classes);
        let root = tree.tree.unwrap_or_else(|| {
            self.add_tree(pending, self.empty_root, Tree::empty_root().encode());
            self.empty_root
        });
        let RawCommit {
            author,
            committer,
            extra_headers,
            message,
            ..
        } = commit;
        let (author, committer) = (signature(author), signature(committer));
        let bytes = render::commit_bytes(
            tree.rendered,
            &parents,
            &author,
            &committer,
            &extra_headers,
            &message,
        );
        if (oid::git_hash(RawKind::Commit, &bytes) == oid) != lossy.is_empty() {
            return Err(ConvertError::Bug { commit: oid });
        }
        let mut metadata = Vec::with_capacity(2);
        if !lossy.is_empty() {
            metadata.push((GIT_LOSSY.to_owned(), lossy.to_metadata()));
        }
        metadata.push((GIT_OID.to_owned(), oid.to_string().into_bytes()));
        Ok(Converted {
            oid,
            parents,
            root,
            author,
            committer,
            message,
            extra_headers,
            lossy,
            metadata,
        })
    }

    /// What the git tree `root` of `commit` converts to, every tree under it converted first:
    /// depth first, on a stack of its own, each tree converted before taken as it converted then.
    fn tree(
        &self,
        pending: &mut Pending,
        commit: GitOid,
        root: GitOid,
    ) -> Result<TreeMemo, ConvertError> {
        if let Some(memo) = self.known_tree(pending, root) {
            return Ok(memo);
        }
        // The path of the tree on top of the stack, its names joined by `/`.
        let mut path = Vec::new();
        let mut stack = vec![self.open(commit, root, &path, None)?];
        loop {
            let top = stack
                .last_mut()
                .expect("the walk ends when it closes the root");
            let Some(entry) = top.entries.next() else {
                let frame = stack.pop().expect("a frame is on top");
                let memo = self.close(pending, commit, frame.oid, frame.kept, frame.classes)?;
                let Some(named) = frame.named else {
                    return Ok(memo);
                };
                let parent = stack
                    .last_mut()
                    .expect("a tree an entry names has a parent");
                let at = At {
                    commit,
                    dir: &path,
                    name: &[],
                };
                self.add_dir(parent, at, named.name, &named.mode, memo)?;
                path.truncate(named.parent_path);
                continue;
            };
            let at = At {
                commit,
                dir: &path,
                name: &entry.name,
            };
            let Some((kind, canonical)) = entry_kind(&entry.mode) else {
                let mode = entry.mode.clone();
                return Err(ConvertError::Mode {
                    commit,
                    path: at.path(),
                    mode,
                });
            };
            match kind {
                Kind::Gitlink => {
                    let detail = || format!("a gitlink, to commit {}", entry.oid);
                    self.allow(&mut top.classes, LossyClass::Gitlinks, at, detail)?;
                }
                Kind::Leaf(leaf) => {
                    let Some(name) = self.held_name(&mut top.classes, at, &entry.name)? else {
                        continue;
                    };
                    if !canonical {
                        let detail = || odd_mode(&entry.mode, kind);
                        self.allow(&mut top.classes, LossyClass::Modes, at, detail)?;
                    }
                    let blob = self.blob(pending, entry.oid, at)?;
                    top.kept
                        .push((TreeEntry::leaf(name, leaf, blob), entry.oid));
                }
                Kind::Dir => {
                    let Some(name) = self.held_name(&mut top.classes, at, &entry.name)? else {
                        continue;
                    };
                    let name = name.to_owned();
                    if let Some(memo) = self.known_tree(pending, entry.oid) {
                        self.add_dir(top, at, name, &entry.mode, memo)?;
                    } else {
                        let parent_path = path.len();
                        push_name(&mut path, &entry.name);
                        let mode = entry.mode;
                        let named = Named {
                            name,
                            mode,
                            parent_path,
                        };
                        stack.push(self.open(commit, entry.oid, &path, Some(named))?);
                    }
                }
            }
        }
    }

    /// The frame of the git tree `oid` at `path`, which `named` names in its parent: read once
    /// its header lets it through the size gate ([`read_bounded`]), parsed, and classed
    /// `tree_order` when it is not in git's order.
    ///
    /// [`read_bounded`]: source::read_bounded
    fn open(
        &self,
        commit: GitOid,
        oid: GitOid,
        path: &[u8],
        named: Option<Named>,
    ) -> Result<Frame, ConvertError> {
        let (kind, bytes) = source::read_bounded(self.source, oid)?;
        if kind != RawKind::Tree {
            return Err(ConvertError::Malformed {
                oid,
                what: "named as a tree, but it is no tree",
            });
        }
        let tree = RawTree::parse(&bytes).map_err(|error| malformed(oid, &error))?;
        let mut classes = LossySet::empty();
        if let Some((first, detail)) = disorder(&tree.entries) {
            let at = At {
                commit,
                dir: path,
                name: &tree.entries[first].name,
            };
            self.allow(&mut classes, LossyClass::TreeOrder, at, || detail)?;
        }
        Ok(Frame {
            oid,
            entries: tree.entries.into_iter(),
            kept: Vec::new(),
            classes,
            named,
        })
    }

    /// What the git tree `oid` converts to, once each of its entries is kept or dropped: the tree
    /// of its kept entries in byte order, the first kept of a name, made unless made before, and
    /// the git tree it renders back to.
    fn close(
        &self,
        pending: &mut Pending,
        commit: GitOid,
        oid: GitOid,
        mut kept: Vec<(TreeEntry, GitOid)>,
        classes: LossySet,
    ) -> Result<TreeMemo, ConvertError> {
        // A name held twice is `tree_order`, allowed by now: the first entry of it stays.
        kept.sort_by(|a, b| a.0.name.cmp(&b.0.name));
        kept.dedup_by(|later, earlier| later.0.name == earlier.0.name);
        let memo = if kept.is_empty() {
            TreeMemo {
                tree: None,
                rendered: oid::git_hash(RawKind::Tree, &[]),
                classes,
            }
        } else {
            let git: Vec<GitOid> = kept.iter().map(|&(_, git)| git).collect();
            let entries = kept.into_iter().map(|(entry, _)| entry).collect();
            // The entries are path components, sorted and unique: a tree holds them.
            let tree = Tree::new(entries).map_err(|_| ConvertError::Bug { commit })?;
            let child = |entry: &TreeEntry| {
                let at = (tree.entries()).binary_search_by(|kept| kept.name.cmp(&entry.name));
                // Every entry is found; were one not, the rendering would not hash to the id it
                // should, which the commit's check reports as the bug it is.
                at.map_or(GitOid::NULL, |at| git[at])
            };
            let rendered = oid::git_hash(RawKind::Tree, &render::tree_bytes(&tree, child));
            let bytes = tree.encode();
            let id = TreeId(swarmops_core::hash_object(ObjectKind::Tree, &bytes));
            self.add_tree(pending, id, bytes);
            TreeMemo {
                tree: Some(id),
                rendered,
                classes,
            }
        };
        pending.trees.insert(oid, memo);
        Ok(memo)
    }

    /// Adds to `frame` its entry `name` of mode `mode`, at `at`, naming a git tree that converted
    /// to `memo`: kept when anything of that tree is, dropped otherwise.
    fn add_dir(
        &self,
        frame: &mut Frame,
        at: At<'_>,
        name: String,
        mode: &[u8],
        memo: TreeMemo,
    ) -> Result<(), ConvertError> {
        let Some(tree) = memo.tree else {
            // Nothing of it is kept. An empty tree in git is dropped as one; a tree its own drops
            // emptied goes with them, under their classes alone.
            if memo.classes.is_empty() {
                let detail = || "an empty tree".to_owned();
                return self.allow(&mut frame.classes, LossyClass::EmptyTrees, at, detail);
            }
            frame.classes = frame.classes.union(memo.classes);
            return Ok(());
        };
        if mode != DIR_MODE {
            let detail = || odd_mode(mode, Kind::Dir);
            self.allow(&mut frame.classes, LossyClass::Modes, at, detail)?;
        }
        frame.classes = frame.classes.union(memo.classes);
        frame.kept.push((TreeEntry::dir(name, tree), memo.rendered));
        Ok(())
    }

    /// The blob the git blob `oid` at `at` converts to: its bytes, read once its header says it
    /// is a blob an object holds, and appended to the objects unless converted before.
    fn blob(&self, pending: &mut Pending, oid: GitOid, at: At<'_>) -> Result<BlobId, ConvertError> {
        if let Some(&id) = (pending.blobs.get(&oid)).or_else(|| self.blobs.get(&oid)) {
            return Ok(id);
        }
        let not_a_blob = ConvertError::Malformed {
            oid,
            what: "named as a file's or a symlink's content, but it is no blob",
        };
        let too_large = |size| ConvertError::TooLarge {
            commit: at.commit,
            path: at.path(),
            size,
        };
        let (kind, size) = self.source.header(oid)?;
        if kind != RawKind::Blob {
            return Err(not_a_blob);
        }
        if size > MAX_OBJECT_LEN {
            return Err(too_large(size));
        }
        let (kind, bytes) = self.source.read(oid)?;
        if kind != RawKind::Blob {
            return Err(not_a_blob);
        }
        // Past the limit after all, from a source whose header said less.
        if bytes::to_u64(bytes.len()) > MAX_OBJECT_LEN {
            return Err(too_large(bytes::to_u64(bytes.len())));
        }
        let id = BlobId(swarmops_core::hash_object(ObjectKind::Blob, &bytes));
        pending.blobs.insert(oid, id);
        pending.objects.push((ObjectKind::Blob, bytes));
        Ok(id)
    }

    /// `name`, as the name of an entry a tree holds; `None` when no path holds it, and the entry
    /// is dropped as `paths` (if allowed).
    fn held_name<'n>(
        &self,
        classes: &mut LossySet,
        at: At<'_>,
        name: &'n [u8],
    ) -> Result<Option<&'n str>, ConvertError> {
        match component(name) {
            Ok(name) => Ok(Some(name)),
            Err(why) => {
                let detail = || why.to_owned();
                self.allow(classes, LossyClass::Paths, at, detail)?;
                Ok(None)
            }
        }
    }

    /// Puts `class` in `classes` when the converter allows it; otherwise the commit's refusal,
    /// at `at`, saying what `detail` gives.
    fn allow(
        &self,
        classes: &mut LossySet,
        class: LossyClass,
        at: At<'_>,
        detail: impl FnOnce() -> String,
    ) -> Result<(), ConvertError> {
        if !self.allowed.contains(class) {
            return Err(ConvertError::Lossy {
                commit: at.commit,
                class,
                path: at.path(),
                detail: detail(),
            });
        }
        classes.insert(class);
        Ok(())
    }

    /// What the git tree `oid` converted to before, in this commit's conversion or an earlier
    /// one.
    fn known_tree(&self, pending: &Pending, oid: GitOid) -> Option<TreeMemo> {
        (pending.trees.get(&oid))
            .or_else(|| self.trees.get(&oid))
            .copied()
    }

    /// Appends the tree whose id is `id` and whose bytes are `bytes` to the objects the commit's
    /// conversion makes, unless it was made before.
    fn add_tree(&self, pending: &mut Pending, id: TreeId, bytes: Vec<u8>) {
        if !self.made.contains(&id) && pending.made.insert(id) {
            pending.objects.push((ObjectKind::Tree, bytes));
        }
    }
}

/// What a tree entry's mode digits name, and whether git writes the mode so (`100644`,
/// `100755`, `120000`, `40000`, `160000`); `None` for a mode of no file, directory, symlink or
/// gitlink.
fn entry_kind(mode: &[u8]) -> Option<(Kind, bool)> {
    let kind = match mode {
        b"100644" => Kind::Leaf(LeafKind::File),
        b"100755" => Kind::Leaf(LeafKind::Exec),
        b"120000" => Kind::Leaf(LeafKind::Symlink),
        b"40000" => Kind::Dir,
        b"160000" => Kind::Gitlink,
        _ => return odd_kind(mode).map(|kind| (kind, false)),
    };
    Some((kind, true))
}

/// What a mode git does not write names, by its file type, as git's `canon_mode` takes it: a
/// regular file's an executable when its owner's execute bit is set, a file otherwise. `None`
/// for other file types, and for digits that are no octal number of 16 bits.
fn odd_kind(mode: &[u8]) -> Option<Kind> {
    let value = mode.iter().try_fold(0_u32, |value, &digit| {
        let digit = digit.checked_sub(b'0').filter(|&digit| digit < 8)?;
        value.checked_mul(8)?.checked_add(u32::from(digit))
    })?;
    if value > 0o177_777 {
        return None;
    }
    match value & 0o170_000 {
        0o100_000 if value & 0o100 == 0 => Some(Kind::Leaf(LeafKind::File)),
        0o100_000 => Some(Kind::Leaf(LeafKind::Exec)),
        0o120_000 => Some(Kind::Leaf(LeafKind::Symlink)),
        0o040_000 => Some(Kind::Dir),
        0o160_000 => Some(Kind::Gitlink),
        _ => None,
    }
}

/// What a refusal of an odd `mode` of `kind` says.
fn odd_mode(mode: &[u8], kind: Kind) -> String {
    let written = match kind {
        Kind::Leaf(LeafKind::File) => "100644",
        Kind::Leaf(LeafKind::Exec) => "100755",
        Kind::Leaf(LeafKind::Symlink) => "120000",
        Kind::Dir => "40000",
        Kind::Gitlink => "160000",
    };
    format!(
        "the mode {}, which git writes {written}",
        mode.escape_ascii()
    )
}

/// Whether a tree entry's mode digits name a directory, as git's order compares it.
fn is_dir(mode: &[u8]) -> bool {
    matches!(entry_kind(mode), Some((Kind::Dir, _)))
}

/// `name` as a path component, the name of an entry a tree holds; otherwise why it is none. (A
/// git tree's names hold no NUL.)
fn component(name: &[u8]) -> Result<&str, &'static str> {
    let name = std::str::from_utf8(name).map_err(|_| "a name that is not UTF-8")?;
    match name {
        "" => Err("an empty name"),
        "." | ".." => Err("a name of the directory itself or of its parent"),
        _ if name.contains('/') => Err("a name holding a `/`"),
        _ => Ok(name),
    }
}

/// The first of a tree's `entries` that breaks git's order, and how: one that does not come
/// after the entry before it, or that holds a name an earlier entry holds. `None` for a tree in
/// git's order.
fn disorder(entries: &[RawTreeEntry]) -> Option<(usize, String)> {
    for (before, pair) in entries.windows(2).enumerate() {
        match render::git_order(order_key(&pair[0]), order_key(&pair[1])) {
            Ordering::Less => {}
            Ordering::Equal => return Some((before + 1, "a name the tree holds twice".to_owned())),
            Ordering::Greater => {
                let detail = format!("out of git's order, after {}", pair[0].name.escape_ascii());
                return Some((before + 1, detail));
            }
        }
    }
    // In git's order a file and a directory may still share a name: git compares the
    // directory's as if it ended in `/`, so other names can sort between the two.
    let mut names: Vec<(&[u8], usize)> = (entries.iter().enumerate())
        .map(|(at, entry)| (entry.name.as_slice(), at))
        .collect();
    names.sort_unstable();
    let twice = names.windows(2).filter(|pair| pair[0].0 == pair[1].0);
    let second = twice.map(|pair| pair[1].1).min()?;
    let detail = "a name the tree holds twice, a file's and a directory's".to_owned();
    Some((second, detail))
}

/// What git's order compares of a tree entry: its name, and whether it is a directory.
fn order_key(entry: &RawTreeEntry) -> (&[u8], bool) {
    (&entry.name, is_dir(&entry.mode))
}

/// `ids` in their order, each once.
fn each_once(ids: &[GitOid]) -> Vec<GitOid> {
    let mut seen = HashSet::with_capacity(ids.len());
    ids.iter().copied().filter(|&id| seen.insert(id)).collect()
}

/// An ident as a changeset holds it: the same fields.
fn signature(ident: RawIdent) -> Signature {
    Signature {
        name: ident.name,
        email: ident.email,
        secs: ident.secs,
        offset_secs: ident.offset_secs,
        negative_zero: ident.negative_zero,
    }
}

/// What in a commit's `raw` bytes git writes otherwise: the first line that `rendered`, what its
/// fields render to, does not hold as it is.
fn unlike(raw: &[u8], rendered: &[u8]) -> String {
    let same = (raw.iter().zip(rendered))
        .take_while(|(a, b)| a == b)
        .count();
    if same == raw.len() {
        return "no blank line after its headers".to_owned();
    }
    let start = raw[..same]
        .iter()
        .rposition(|&byte| byte == b'\n')
        .map_or(0, |end| end + 1);
    let end = raw[same..]
        .iter()
        .position(|&byte| byte == b'\n')
        .map_or(raw.len(), |len| same + len);
    format!(
        "git writes its line `{}` otherwise",
        raw[start..end].escape_ascii()
    )
}

/// The object `oid`, whose bytes do not parse, as `error` says.
fn malformed(oid: GitOid, error: &RawError) -> ConvertError {
    match *error {
        RawError::Malformed { what } => ConvertError::Malformed { oid, what },
    }
}

/// Appends `name` to `path`, after a `/` unless either is empty.
fn push_name(path: &mut Vec<u8>, name: &[u8]) {
    if !path.is_empty() && !name.is_empty() {
        path.push(b'/');
    }
    path.extend_from_slice(name);
}

/// ` at <path>`, or nothing for an empty path: where a [`ConvertError::Lossy`] is.
fn at_path(path: &[u8]) -> String {
    if path.is_empty() {
        String::new()
    } else {
        format!(" at {}", path.escape_ascii())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A mode's digits, and what they name.
    type ModeCase = (&'static [u8], Option<(Kind, bool)>);

    #[test]
    fn a_mode_names_its_kind_by_its_file_type() {
        let cases: [ModeCase; 14] = [
            (b"100644", Some((Kind::Leaf(LeafKind::File), true))),
            (b"100755", Some((Kind::Leaf(LeafKind::Exec), true))),
            (b"120000", Some((Kind::Leaf(LeafKind::Symlink), true))),
            (b"40000", Some((Kind::Dir, true))),
            (b"160000", Some((Kind::Gitlink, true))),
            // Written otherwise: by file type, a regular file's by its owner's execute bit.
            (b"100664", Some((Kind::Leaf(LeafKind::File), false))),
            (b"100700", Some((Kind::Leaf(LeafKind::Exec), false))),
            (b"0100644", Some((Kind::Leaf(LeafKind::File), false))),
            (b"040000", Some((Kind::Dir, false))),
            (b"0160000", Some((Kind::Gitlink, false))),
            // No file, directory, symlink or gitlink: a block device, a socket, no type at all,
            // a value past 16 bits.
            (b"60000", None),
            (b"140000", None),
            (b"644", None),
            (b"1100644", None),
        ];
        for (mode, kind) in cases {
            assert_eq!(entry_kind(mode), kind, "{}", mode.escape_ascii());
        }
        // Not octal at all, which a parsed tree never holds.
        assert_eq!(entry_kind(b"10064x"), None);
        assert_eq!(entry_kind(b"99999999999999999999999"), None);
    }

    #[test]
    fn a_name_is_held_when_it_is_a_path_component() {
        for name in ["a", "a.b", ".git", "caf\u{e9}", "with space", "-"] {
            assert_eq!(component(name.as_bytes()), Ok(name));
            Tree::new(vec![TreeEntry::dir(
                name,
                TreeId(swarmops_core::ObjectId([0; 32])),
            )])
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        }
        for name in [&b"caf\xe9"[..], b"", b".", b"..", b"a/b"] {
            assert!(component(name).is_err(), "{}", name.escape_ascii());
        }
    }

    #[test]
    fn disorder_finds_the_first_entry_out_of_git_order_or_named_twice() {
        let entry = |mode: &str, name: &str| RawTreeEntry {
            mode: mode.as_bytes().to_vec(),
            name: name.as_bytes().to_vec(),
            oid: GitOid::NULL,
        };
        let ordered = [
            entry("100644", "a.b"),
            entry("40000", "a"),
            entry("100644", "b"),
        ];
        assert_eq!(disorder(&ordered), None);
        let byte_order = [entry("40000", "a"), entry("100644", "a.b")];
        assert_eq!(disorder(&byte_order).map(|(at, _)| at), Some(1));
        let twice = [
            entry("100644", "a"),
            entry("100644", "b"),
            entry("100644", "b"),
        ];
        assert_eq!(disorder(&twice).map(|(at, _)| at), Some(2));
        // A file and a directory of one name, in git's order with `a.b` between them.
        let shared = [
            entry("100644", "a"),
            entry("100644", "a.b"),
            entry("40000", "a"),
        ];
        assert_eq!(disorder(&shared).map(|(at, _)| at), Some(2));
    }

    #[test]
    fn unlike_names_the_first_line_git_writes_otherwise() {
        let raw = b"tree t\nauthor A  <a> 1 +0000\ncommitter C <c> 1 +0000\n\nm\n";
        let rendered = b"tree t\nauthor A <a> 1 +0000\ncommitter C <c> 1 +0000\n\nm\n";
        assert_eq!(
            unlike(raw, rendered),
            "git writes its line `author A  <a> 1 +0000` otherwise"
        );
        let headers = b"tree t\nauthor A <a> 1 +0000\ncommitter C <c> 1 +0000\n";
        let mut whole = headers.to_vec();
        whole.push(b'\n');
        assert_eq!(unlike(headers, &whole), "no blank line after its headers");
    }
}
