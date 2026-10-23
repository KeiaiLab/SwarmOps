//! Reading a repository without a checkout (storage spec §4), in one snapshot of its metadata
//! ([`Repo::snapshot`], a [`RepoRead`]): a changeset's record and object
//! ([`RepoRead::changeset`]), revisions resolved to changesets ([`RepoRead::resolve`]), and
//! files and directories at a changeset ([`RepoRead::read_blob_at`], [`RepoRead::list_tree`],
//! [`RepoRead::entry_at`]), read by walking the trees from its root: one tree object per
//! directory on the path. [`Repo`]'s methods of the same names each read in a snapshot of their
//! own.
//!
//! A part of a file ([`RepoRead::read_file_span`]: from a byte offset, or a range of its lines)
//! keeps only that part of the blob while it verifies the whole; a listing to a depth
//! ([`RepoRead::list_tree_depth`]) walks the directories below one in pre-order, a page at a time,
//! with each leaf's size from its object's header.
//!
//! A revision (operations spec §0) is a ref name (`main`, `release/<x>`), a full id (64 hex
//! digits), or a unique id prefix of 12 to 63 hex digits, either case; followed by any number
//! of `~N` suffixes, each walking `N` first parents back (`main~2`, `main~1~1`), as
//! [`RepoRead::ancestor`] walks them.

use swarmops_core::bytes;
use swarmops_core::{
    BlobId, Changeset, ChangesetId, EntryTarget, LeafKind, ObjectId, ObjectKind, RepoPath, Tree,
    TreeEntry, TreeId,
};
use swarmops_store::{ChangesetMeta, MetaRead, ObjectStat, ObjectStore, StoreError, TailRepair};

use crate::error::RepoError;
use crate::repo::{self, Repo};

/// One consistent snapshot of a repository's metadata, and the reads of one request in it
/// ([`Repo::snapshot`]; storage spec §8): every revision and ref it resolves, it resolves in the
/// same view, which later commits and ref moves do not change, so that a request that names
/// several revisions reads them as of one moment. What it reads by id (changesets, trees, blobs)
/// is immutable. Two rules hold for it:
///
/// 1. Never read in it after the request's own write: it does not show that write. Reading what
///    the request wrote takes a new snapshot.
/// 2. Never hold it across requests. Besides going stale, a snapshot that counted index records
///    which a writer then cut back after a failed sync fails its lookups with an I/O error until
///    a new one is taken (storage spec §3).
#[non_exhaustive]
pub struct RepoRead<'r, O: ObjectStore> {
    pub(crate) repo: &'r Repo<O>,
    pub(crate) meta: MetaRead<'r>,
}

/// What [`RepoRead::blobs`] read.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct BlobsRead {
    /// The blobs found, each with its bytes, in the order asked.
    pub found: Vec<(BlobId, Vec<u8>)>,
    /// The ids the store holds no blob under, among those the read reached.
    pub missing: Vec<BlobId>,
    /// Whether the read stopped before the last id, to keep within the byte budget.
    pub truncated: bool,
}

impl<O: ObjectStore> std::fmt::Debug for RepoRead<'_, O> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RepoRead")
            .field("meta", &self.meta)
            .finish_non_exhaustive()
    }
}

/// A changeset as the store holds it: its record in the index, and its object.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct StoredChangeset {
    /// Its id.
    pub id: ChangesetId,
    /// Its place in the journal's total order of changesets, from 0; parents come first.
    pub seq: u64,
    /// 1 + the largest parent generation; 1 for a root.
    pub generation: u64,
    /// The changeset itself, decoded from its object.
    pub changeset: Changeset,
}

/// Which part of a file [`RepoRead::read_file_span`] reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FileSpan {
    /// The bytes from offset `start` to the file's end: none when `start` is at or past the end.
    Bytes {
        /// Where the span starts, in bytes from the file's start.
        start: u64,
    },
    /// Lines `first` to `last`, numbered from 1, both included: a line ends after its `\n`, and a
    /// last line without one is a line too (an empty file has none). The lines of the range that
    /// exist make the span: none when `first` is past the last line, or `last` before `first`.
    Lines {
        /// The range's first line.
        first: u64,
        /// The range's last line.
        last: u64,
    },
}

/// What [`RepoRead::read_file_span`] read of a file.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct FileSpanRead {
    /// The file's kind.
    pub kind: LeafKind,
    /// Its blob.
    pub blob: BlobId,
    /// The whole file's size in bytes.
    pub size: u64,
    /// Where the span starts in the file: [`FileSpan::Bytes`]' `start` as given, past the end if it
    /// was; for [`FileSpan::Lines`], where the range's first line that exists starts, or the
    /// file's size when none does.
    pub start: u64,
    /// The span's bytes, its first `max` when it is longer.
    pub bytes: Vec<u8>,
    /// For [`FileSpan::Lines`]: the first and the last line of the range that exist, which the
    /// span holds; `None` when none exists, and for [`FileSpan::Bytes`].
    pub lines: Option<(u64, u64)>,
}

impl FileSpanRead {
    /// Makes one of the fields given.
    pub fn new(
        kind: LeafKind,
        blob: BlobId,
        size: u64,
        start: u64,
        bytes: Vec<u8>,
        lines: Option<(u64, u64)>,
    ) -> FileSpanRead {
        FileSpanRead {
            kind,
            blob,
            size,
            start,
            bytes,
            lines,
        }
    }
}

/// One entry of a listing below a directory ([`RepoRead::list_tree_depth`]).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ListedEntry {
    /// Its path below the listed directory: its name, for one of the directory's own entries.
    pub path: RepoPath,
    /// What it is: a leaf, with its kind and blob, or a directory, with its tree.
    pub target: EntryTarget,
    /// For a leaf, its size in bytes: its blob's length, as the blob's object header records it;
    /// `None` for a directory.
    pub size: Option<u64>,
}

impl ListedEntry {
    /// Makes one of the fields given.
    pub fn new(path: RepoPath, target: EntryTarget, size: Option<u64>) -> ListedEntry {
        ListedEntry { path, target, size }
    }
}

/// A page of a listing below a directory ([`RepoRead::list_tree_depth`]).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct TreePage {
    /// The page's entries, in pre-order.
    pub entries: Vec<ListedEntry>,
    /// Whether entries follow the page's last.
    pub truncated: bool,
}

impl<O: ObjectStore> Repo<O> {
    /// A snapshot of the metadata as committed now ([`swarmops_store::MetaStore::read`]), to make
    /// the reads of one request in: see [`RepoRead`] for its two rules.
    pub fn snapshot(&self) -> Result<RepoRead<'_, O>, RepoError> {
        Ok(RepoRead {
            repo: self,
            meta: self.meta.read()?,
        })
    }

    /// A snapshot of the committed metadata only, for the readers whose results leave the host
    /// (M8's git export, M11's `serve` and `push`; design-L107): a [`RepoRead`] built as
    /// [`Repo::snapshot`] builds one, from [`MetaStore::read_committed`], with what the journal's
    /// tail check under the lock repaired ([`TailRepair`]). It waits for the writer lock as a
    /// commit does ([`StoreError::Busy`] if the wait runs out) and lets it go before it returns,
    /// so that no frame a power loss or a failed sync's cut could take back is in it. The same two
    /// rules as [`Repo::snapshot`]'s hold for it.
    ///
    /// [`MetaStore::read_committed`]: swarmops_store::MetaStore::read_committed
    pub fn snapshot_committed(&self) -> Result<(RepoRead<'_, O>, TailRepair), RepoError> {
        let (meta, repaired) = self.meta.read_committed()?;
        Ok((RepoRead { repo: self, meta }, repaired))
    }

    /// Changeset `id`, if the store holds it, in a snapshot of its own
    /// ([`RepoRead::changeset`]).
    pub fn changeset(&self, id: ChangesetId) -> Result<Option<StoredChangeset>, RepoError> {
        self.snapshot()?.changeset(id)
    }

    /// The changeset revision `rev` names, in a snapshot of its own ([`RepoRead::resolve`]).
    pub fn resolve(&self, rev: &str) -> Result<ChangesetId, RepoError> {
        self.snapshot()?.resolve(rev)
    }

    /// The kind, the blob id and the bytes of the file at `path` in changeset `cs`'s tree, in a
    /// snapshot of its own ([`RepoRead::read_blob_at`]).
    pub fn read_blob_at(
        &self,
        cs: ChangesetId,
        path: &RepoPath,
    ) -> Result<Option<(LeafKind, BlobId, Vec<u8>)>, RepoError> {
        self.snapshot()?.read_blob_at(cs, path)
    }

    /// The entries of directory `dir` in changeset `cs`'s tree, in a snapshot of its own
    /// ([`RepoRead::list_tree`]).
    pub fn list_tree(
        &self,
        cs: ChangesetId,
        dir: Option<&RepoPath>,
    ) -> Result<Option<Vec<TreeEntry>>, RepoError> {
        self.snapshot()?.list_tree(cs, dir)
    }

    /// The changeset `meta` records, with its object, hash-checked against its id, which must
    /// name the parents `meta` does: a record that names others (a damaged or crafted
    /// `parents.log` or `par.col`) is [`StoreError::IndexCorrupt`], so that no read returns a
    /// changeset with a history its object does not have.
    pub(crate) fn stored(&self, meta: &ChangesetMeta) -> Result<StoredChangeset, RepoError> {
        let changeset = self.changeset_object(meta.id)?;
        if changeset.parents != meta.parents {
            return Err(StoreError::IndexCorrupt {
                what: "a changeset's record names other parents than its object",
            }
            .into());
        }
        Ok(StoredChangeset {
            id: meta.id,
            seq: meta.seq,
            generation: meta.generation,
            changeset,
        })
    }

    /// The tree of the directory that `dirs` lead to from tree `root`; `None` if one of them
    /// is missing or not a directory. Reads one tree per component.
    fn descend(&self, root: TreeId, dirs: &[&str]) -> Result<Option<TreeId>, RepoError> {
        let mut at = root;
        for &dir in dirs {
            let tree = repo::tree(&self.objects, at)?;
            match find(&tree, dir).map(|entry| entry.target) {
                Some(EntryTarget::Dir(tree)) => at = tree,
                Some(EntryTarget::Leaf(..)) | None => return Ok(None),
            }
        }
        Ok(Some(at))
    }
}

impl<O: ObjectStore> RepoRead<'_, O> {
    /// Changeset `id`, if the snapshot holds it.
    pub fn changeset(&self, id: ChangesetId) -> Result<Option<StoredChangeset>, RepoError> {
        match self.meta.changeset(id)? {
            Some(meta) => self.repo.stored(&meta).map(Some),
            None => Ok(None),
        }
    }

    /// The changeset revision `rev` names in the snapshot: see [the module documentation](self)
    /// for the grammar. [`RepoError::NotFound`] when it names none (a ref that does not exist or
    /// names a changeset the store does not hold, an id or a prefix of none, a `~N` past a root;
    /// the `~N` suffixes add up to one walk, [`RepoRead::ancestor`]'s),
    /// [`RepoError::Ambiguous`] for a prefix of more than one, [`RepoError::BadRev`] for anything
    /// that is not a revision. The errors carry `rev` as given (a dangling ref's error adds the
    /// changeset it names).
    pub fn resolve(&self, rev: &str) -> Result<ChangesetId, RepoError> {
        let bad = || RepoError::BadRev(rev.to_owned());
        let mut parts = rev.split('~');
        let base = parts.next().expect("split yields a first part");
        let mut steps: u64 = 0;
        for part in parts {
            if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
                return Err(bad());
            }
            let n: u64 = part.parse().map_err(|_| bad())?;
            steps = steps.checked_add(n).ok_or_else(bad)?;
        }
        let id = self.resolve_base(base, rev)?;
        if steps == 0 {
            return Ok(id);
        }
        self.ancestor(id, steps)?
            .ok_or_else(|| RepoError::NotFound(rev.to_owned()))
    }

    /// The kind, the blob id and the bytes of the file at `path` in changeset `cs`'s tree;
    /// `None` if no file is there (nothing, or a directory). [`RepoError::NotFound`] if the
    /// snapshot does not hold `cs`.
    pub fn read_blob_at(
        &self,
        cs: ChangesetId,
        path: &RepoPath,
    ) -> Result<Option<(LeafKind, BlobId, Vec<u8>)>, RepoError> {
        match self.entry_at(cs, path)? {
            Some(EntryTarget::Leaf(kind, blob)) => Ok(Some((kind, blob, self.blob(blob)?))),
            Some(EntryTarget::Dir(_)) | None => Ok(None),
        }
    }

    /// What `path` holds in changeset `cs`'s tree: a leaf (its kind and blob), a directory (its
    /// tree), or nothing (`None`, also below a file or a missing directory). Reads the tree of
    /// each directory on the way, the root's first. [`RepoError::NotFound`] if the snapshot does
    /// not hold `cs`.
    pub fn entry_at(
        &self,
        cs: ChangesetId,
        path: &RepoPath,
    ) -> Result<Option<EntryTarget>, RepoError> {
        let root = self.root_of(cs)?;
        let components: Vec<&str> = path.components().collect();
        let (&name, dirs) = components.split_last().expect("a path has a component");
        let Some(dir) = self.repo.descend(root, dirs)? else {
            return Ok(None);
        };
        Ok(find(&self.tree(dir)?, name).map(|entry| entry.target))
    }

    /// Tree `id`, decoded, its object verified against its id as every object read is.
    /// [`RepoError::MissingObject`] if the store lacks it, or holds an object of another kind
    /// under that id.
    pub fn tree(&self, id: TreeId) -> Result<Tree, RepoError> {
        repo::tree(&self.repo.objects, id)
    }

    /// The bytes of blob `id`, its object verified against its id as every object read is.
    /// [`RepoError::MissingObject`] if the store lacks it, or holds an object of another kind
    /// under that id.
    pub fn blob(&self, id: BlobId) -> Result<Vec<u8>, RepoError> {
        repo::object(&self.repo.objects, id.0, ObjectKind::Blob)
    }

    /// Blobs `ids`, in the order given, up to `max_bytes` of content: each blob's size comes
    /// from its object's header ([`ObjectStore::stat`]) before its payload is read, so a blob
    /// that does not fit is never read. The first blob found is returned whole even if it alone
    /// exceeds `max_bytes`, so that a batch always makes progress. An id the store holds no
    /// blob under, or holds an object of another kind under, is in `missing`. A blob that does
    /// not fit stops the read: it and every id after it are left for the next request
    /// ([`BlobsRead::truncated`]).
    pub fn blobs(&self, ids: &[BlobId], max_bytes: u64) -> Result<BlobsRead, RepoError> {
        let mut read = BlobsRead::default();
        let mut used = 0u64;

        for &id in ids {
            // 1. The header says what is there and how big, without reading the payload.
            let Some(ObjectStat {
                kind: ObjectKind::Blob,
                size,
            }) = self.repo.objects.stat(id.0)?
            else {
                read.missing.push(id);
                continue;
            };

            // 2. Past the budget: stop, leaving this blob and the rest for the next request.
            if !read.found.is_empty() && used.saturating_add(size) > max_bytes {
                read.truncated = true;
                break;
            }

            // 3. Read and verify the whole blob.
            read.found.push((id, self.blob(id)?));
            used = used.saturating_add(size);
        }

        Ok(read)
    }

    /// The entries of directory `dir` (the root for `None`) in changeset `cs`'s tree, in
    /// canonical order; `None` if no directory is there (nothing, or a file).
    /// [`RepoError::NotFound`] if the snapshot does not hold `cs`.
    pub fn list_tree(
        &self,
        cs: ChangesetId,
        dir: Option<&RepoPath>,
    ) -> Result<Option<Vec<TreeEntry>>, RepoError> {
        let root = self.root_of(cs)?;
        let components: Vec<&str> = dir
            .map(|dir| dir.components().collect())
            .unwrap_or_default();
        match self.repo.descend(root, &components)? {
            Some(tree) => Ok(Some(self.tree(tree)?.entries().to_vec())),
            None => Ok(None),
        }
    }

    /// The part `span` names of the file at `path` in changeset `cs`'s tree, its first `max` bytes
    /// at most, with the file's kind, blob and size; `None` if no file is there (nothing, or a
    /// directory). [`RepoError::NotFound`] if the snapshot does not hold `cs`.
    ///
    /// The blob's object file is read whole, as [`RepoRead::blob`] reads it, but its payload goes
    /// by in pieces ([`ObjectStore::read_chunks`]) of which only the span's first `max` bytes are
    /// kept: the memory is the file and those bytes, not a copy of the payload besides. The whole
    /// payload is verified all the same, so a blob whose bytes fail their id is
    /// [`StoreError::Corrupt`] wherever the damage lies, in the span or not; a blob the store
    /// lacks, or an object of another kind under its id, is [`RepoError::MissingObject`].
    pub fn read_file_span(
        &self,
        cs: ChangesetId,
        path: &RepoPath,
        span: FileSpan,
        max: u64,
    ) -> Result<Option<FileSpanRead>, RepoError> {
        let Some(EntryTarget::Leaf(kind, blob)) = self.entry_at(cs, path)? else {
            return Ok(None);
        };
        let mut kept = Kept::new(span, max);
        let stat = (self.repo.objects).read_chunks(blob.0, &mut |piece: &[u8]| kept.take(piece))?;
        match stat {
            Some(ObjectStat {
                kind: ObjectKind::Blob,
                size,
            }) => Ok(Some(kept.read(kind, blob, size))),
            _ => Err(RepoError::MissingObject {
                id: blob.0,
                kind: ObjectKind::Blob,
            }),
        }
    }

    /// A page of the entries below directory `dir` (the root for `None`) in changeset `cs`'s
    /// tree, down to `depth` levels below it (1: its own entries; 0 lists as 1 does), in
    /// pre-order: a directory, then the entries below it, each directory's entries by name bytes.
    /// The page holds the first `limit` entries after `after` in that order (from the first for
    /// `None`; `after` need not name an entry that exists), and says whether more follow. A leaf's
    /// size is its blob's length as the blob's object header records it ([`ObjectStore::stat`]):
    /// no payload is read. `None` if no directory is at `dir` (nothing, or a file);
    /// [`RepoError::NotFound`] if the snapshot does not hold `cs`; [`RepoError::MissingObject`]
    /// for a tree the walk enters, or a blob of the page, that the store lacks or holds as an
    /// object of another kind.
    ///
    /// Reads the trees on the way to `dir`, those of the directories on `after`'s path that the
    /// listing enters, and one tree for each directory the page enters, but none to learn that
    /// more entries follow: a directory's tree is never empty (only the root's may be).
    pub fn list_tree_depth(
        &self,
        cs: ChangesetId,
        dir: Option<&RepoPath>,
        depth: u32,
        after: Option<&RepoPath>,
        limit: usize,
    ) -> Result<Option<TreePage>, RepoError> {
        let root = self.root_of(cs)?;
        let components: Vec<&str> = dir
            .map(|dir| dir.components().collect())
            .unwrap_or_default();
        let Some(listed) = self.repo.descend(root, &components)? else {
            return Ok(None);
        };
        let mut levels = vec![self.level(None, 1, listed)?];
        if let Some(after) = after {
            self.skip_through(&mut levels, depth, after)?;
        }
        let mut entries = Vec::new();
        // The directory the page listed last, if the walk enters it: its path, how far below the
        // listed directory its entries are, and its tree.
        let mut enter: Option<(RepoPath, u32, TreeId)> = None;
        let truncated = loop {
            if let Some((path, below_by, tree)) = enter.take() {
                if entries.len() == limit {
                    break true;
                }
                levels.push(self.level(Some(path), below_by, tree)?);
            }
            let Some(level) = levels.last_mut() else {
                break false;
            };
            let Some(entry) = level.tree.entries().get(level.next).cloned() else {
                levels.pop();
                continue;
            };
            if entries.len() == limit {
                break true;
            }
            level.next += 1;
            let path = below(level.path.as_ref(), &entry.name);
            if let EntryTarget::Dir(tree) = entry.target
                && level.depth < depth
            {
                enter = Some((path.clone(), level.depth + 1, tree));
            }
            entries.push(ListedEntry {
                path,
                target: entry.target,
                size: None,
            });
        };
        for entry in &mut entries {
            if let EntryTarget::Leaf(_, blob) = entry.target {
                entry.size = Some(self.blob_size(blob)?);
            }
        }
        Ok(Some(TreePage { entries, truncated }))
    }

    /// Directory `tree`, at `path` below a listing's directory (`None` for that directory itself),
    /// with its entries `depth` levels below it, none of them listed yet.
    fn level(&self, path: Option<RepoPath>, depth: u32, tree: TreeId) -> Result<Level, RepoError> {
        Ok(Level {
            path,
            depth,
            tree: self.tree(tree)?,
            next: 0,
        })
    }

    /// Moves a listing's walk, `levels` (the listed directory's level alone), past `after` in
    /// pre-order: every entry up to `after` is passed over, and the walk resumes with the entry
    /// that follows it, the first below `after` when `after` is a directory the listing enters
    /// (`depth`). Reads the trees of the directories on `after`'s path that the listing enters.
    fn skip_through(
        &self,
        levels: &mut Vec<Level>,
        depth: u32,
        after: &RepoPath,
    ) -> Result<(), RepoError> {
        for name in after.components() {
            let level = levels
                .last_mut()
                .expect("a listing's walk starts in the listed directory");
            let entries = level.tree.entries();
            let at = entries.partition_point(|entry| entry.name.as_bytes() < name.as_bytes());
            // An entry above `after`, or `after` itself: passed over either way.
            let Some(target) = entries
                .get(at)
                .filter(|entry| entry.name == name)
                .map(|entry| entry.target)
            else {
                // No entry has the name: the walk resumes at the first entry past it.
                level.next = at;
                return Ok(());
            };
            level.next = at + 1;
            match target {
                EntryTarget::Dir(tree) if level.depth < depth => {
                    let path = below(level.path.as_ref(), name);
                    let next = self.level(Some(path), level.depth + 1, tree)?;
                    levels.push(next);
                }
                // A leaf, or a directory the listing does not enter: nothing below it is listed.
                EntryTarget::Dir(_) | EntryTarget::Leaf(..) => return Ok(()),
            }
        }
        Ok(())
    }

    /// The size of blob `id` in bytes, as its object's 16-byte header records it
    /// ([`ObjectStore::stat`]): its payload is neither read nor verified.
    /// [`RepoError::MissingObject`] if the store lacks it, or holds an object of another kind
    /// under that id.
    fn blob_size(&self, id: BlobId) -> Result<u64, RepoError> {
        match self.repo.objects.stat(id.0)? {
            Some(ObjectStat {
                kind: ObjectKind::Blob,
                size,
            }) => Ok(size),
            _ => Err(RepoError::MissingObject {
                id: id.0,
                kind: ObjectKind::Blob,
            }),
        }
    }

    /// The root tree of changeset `cs`; [`RepoError::NotFound`] if the snapshot does not hold
    /// it.
    fn root_of(&self, cs: ChangesetId) -> Result<TreeId, RepoError> {
        if self.meta.changeset(cs)?.is_none() {
            return Err(RepoError::NotFound(cs.to_string()));
        }
        Ok(self.repo.changeset_object(cs)?.root)
    }

    /// The changeset that `base`, a revision without its `~N` suffixes, names in the snapshot.
    /// The errors carry `rev`, the whole revision.
    fn resolve_base(&self, base: &str, rev: &str) -> Result<ChangesetId, RepoError> {
        let read = &self.meta;
        let not_found = || RepoError::NotFound(rev.to_owned());
        let hex = !base.is_empty() && base.bytes().all(|b| b.is_ascii_hexdigit());
        if !hex {
            // No ref name is all hex digits: main, or release/<x>.
            return match read.get_ref(base) {
                Ok(Some(state)) if read.changeset(state.target)?.is_some() => Ok(state.target),
                // A dangling ref (damage verify reports): the revision names nothing.
                Ok(Some(state)) => Err(RepoError::NotFound(format!(
                    "{rev} (ref {base} names {}, which the store does not hold)",
                    state.target
                ))),
                Ok(None) => Err(not_found()),
                Err(StoreError::RefName(_)) => Err(RepoError::BadRev(rev.to_owned())),
                Err(e) => Err(e.into()),
            };
        }
        match base.len() {
            64 => {
                let id = ChangesetId(ObjectId::from_hex(base).expect("64 hex digits"));
                match read.changeset(id)? {
                    Some(_) => Ok(id),
                    None => Err(not_found()),
                }
            }
            12..=63 => match read.resolve_prefix(base)?[..] {
                [] => Err(not_found()),
                [id] => Ok(id),
                _ => Err(RepoError::Ambiguous(rev.to_owned())),
            },
            _ => Err(RepoError::BadRev(rev.to_owned())),
        }
    }
}

/// The entry named `name` of `tree`: a binary search, the entries being sorted by name bytes.
fn find<'t>(tree: &'t Tree, name: &str) -> Option<&'t TreeEntry> {
    let entries = tree.entries();
    entries
        .binary_search_by(|entry| entry.name.as_bytes().cmp(name.as_bytes()))
        .ok()
        .map(|at| &entries[at])
}

/// A directory a listing walks ([`RepoRead::list_tree_depth`]), and how far through its entries
/// the walk is.
struct Level {
    /// Its path below the listed directory; `None` for the listed directory itself.
    path: Option<RepoPath>,
    /// How far below the listed directory its entries are: 1 for the listed directory's own.
    depth: u32,
    /// Its tree.
    tree: Tree,
    /// The index of its next entry to list.
    next: usize,
}

/// The path of entry `name` of the directory at `parent` below a listing's directory: `name`
/// itself for one of the listed directory's own entries (`parent` `None`).
fn below(parent: Option<&RepoPath>, name: &str) -> RepoPath {
    let path = match parent {
        Some(parent) => format!("{parent}/{name}"),
        None => name.to_owned(),
    };
    RepoPath::new(path).expect("a tree entry's name is one path component")
}

/// What [`RepoRead::read_file_span`] keeps of a payload as its pieces go by: the span's first
/// `max` bytes, and, for lines, where the span starts and which lines it holds.
struct Kept {
    /// The span.
    span: FileSpan,
    /// The most bytes to keep.
    max: usize,
    /// The bytes kept.
    bytes: Vec<u8>,
    /// How many bytes of the payload have gone by.
    seen: u64,
    /// For [`FileSpan::Lines`]: the line the next byte is on, from 1.
    line: u64,
    /// For [`FileSpan::Lines`]: where the range's first line that exists starts.
    start: Option<u64>,
    /// For [`FileSpan::Lines`]: the range's first and last line seen so far.
    lines: Option<(u64, u64)>,
}

impl Kept {
    /// Nothing kept yet of `span`, of which `max` bytes at most are to be.
    fn new(span: FileSpan, max: u64) -> Self {
        Self {
            span,
            max: usize::try_from(max).unwrap_or(usize::MAX),
            bytes: Vec::new(),
            seen: 0,
            line: 1,
            start: None,
            lines: None,
        }
    }

    /// Takes the payload's next piece, keeping what of it is in the span while there is room.
    fn take(&mut self, piece: &[u8]) {
        let at = self.seen;
        self.seen += bytes::to_u64(piece.len());
        match self.span {
            FileSpan::Bytes { start } => {
                // Where the span starts in this piece: at its start once the span has begun.
                if let Ok(from) = usize::try_from(start.saturating_sub(at))
                    && from < piece.len()
                {
                    self.keep(&piece[from..]);
                }
            }
            // Past the range: nothing more to keep, or to count.
            FileSpan::Lines { last, .. } if self.line > last => {}
            FileSpan::Lines { first, last } => {
                let mut offset = at;
                for part in piece.split_inclusive(|&byte| byte == b'\n') {
                    if self.line > last {
                        break;
                    }
                    if self.line >= first {
                        self.start.get_or_insert(offset);
                        let from = self.lines.map_or(self.line, |(from, _)| from);
                        self.lines = Some((from, self.line));
                        self.keep(part);
                    }
                    offset += bytes::to_u64(part.len());
                    if part.ends_with(b"\n") {
                        self.line += 1;
                    }
                }
            }
        }
    }

    /// Keeps what there is room for of `bytes`.
    fn keep(&mut self, bytes: &[u8]) {
        let room = self.max - self.bytes.len();
        self.bytes
            .extend_from_slice(&bytes[..bytes.len().min(room)]);
    }

    /// What was read of the file of `kind` and `blob`, `size` bytes long, its payload gone by.
    fn read(self, kind: LeafKind, blob: BlobId, size: u64) -> FileSpanRead {
        let start = match self.span {
            FileSpan::Bytes { start } => start,
            FileSpan::Lines { .. } => self.start.unwrap_or(size),
        };
        FileSpanRead {
            kind,
            blob,
            size,
            start,
            bytes: self.bytes,
            lines: self.lines,
        }
    }
}
