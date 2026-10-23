//! Checking a store (storage spec §2–§3): [`Repo::verify`] reads all of a store once, without
//! the writer lock and without writing, and reports what it finds damaged as
//! [`VerifyProblem`]s. It fails only on an I/O error it cannot get past. A metadata file it
//! cannot read as one (a journal file missing, `journal.ckpt` malformed or not a frame
//! boundary, index files that do not open) is a [`VerifyProblem::MetaCorrupt`], and the rest is
//! checked without it.
//!
//! It reads in this order, so that a commit running meanwhile never looks like damage (an entry
//! of `journal.off` that a failed listing cuts back meanwhile is read again, as the journal's
//! reads do): the checkpoint, the changeset index, the ref files and the external id index's
//! entries, then the journal (whatever the index, a ref file or an entry shows had its frame in the
//! journal before: an entry is linked only once the frame that records its changeset is durable,
//! W8), then the objects (every object a recorded changeset references was durable before its
//! frame).
//!
//! - The metadata. The index's records are compared with the journal's changeset records, place
//!   by place: a record unlike the journal's at its seq, one the journal does not hold, and a
//!   journal record below the checkpoint that the index lacks are each an
//!   [`VerifyProblem::IndexMismatch`]. (Past the checkpoint and the sorted views the index counts
//!   only the records the journal vouches for, `Index::open`; those the views cover count as
//!   they are, even past the checkpoint, synced before the views were written. One the journal
//!   does not vouch for, which a power loss can leave there, is no damage, and the next writer
//!   replays the journal's in its place. A record that no frame holds at all, past the
//!   journal's last changeset (what a writer that went around the journal would leave), is
//!   another matter: the index does not count it either while changeset frames follow the
//!   checkpoint, and the next writer would cut it without a word. So verify measures what the
//!   index files hold by their lengths (`Index::held`) before it reads the journal, and reports
//!   a mismatch at the journal's changeset count whenever they hold more and the journal reads
//!   whole: a writer appends a record only once its frame is durable and listed. With the
//!   journal damaged or missing, verify reports that instead.) So are sorted-view
//!   entries that disagree with the index columns (the advisory `Compacted` frames are never
//!   consulted); each names its [`MismatchCause`]. A frame the journal's reader finds corrupt is a
//!   [`VerifyProblem::JournalCorrupt`]: the comparison stops there, and the index's records
//!   past it are checked as the index holds them. A ref
//!   file that is not a ref file is a [`VerifyProblem::CorruptRef`], one that names no
//!   changeset a [`VerifyProblem::DanglingRef`]; one that cannot be read at all fails verify.
//!   A ref's target is checked only if the journal reads whole or the index opens: with
//!   neither, no changeset is known for certain, and the [`VerifyProblem::MetaCorrupt`] says
//!   that the refs' targets go unchecked.
//! - The lock file: a `meta/lock` that is not a regular file, which every write refuses before
//!   it opens it, is a [`VerifyProblem::MetaCorrupt`]. Verify only looks at it
//!   (`MetaLock::refuses`), and checks it first.
//! - Every changeset, in seq order: its object, its record against its object (parents, delta
//!   hash) and its parents' generations, its parents ([`VerifyProblem::MissingParent`]), every
//!   tree under its root, and its root against its first parent's root with its delta applied
//!   ([`VerifyProblem::RootMismatch`]).
//! - The external id index (storage spec §7), once the changesets are checked: an entry whose file
//!   is not one is a [`VerifyProblem::ExternalIdCorrupt`]; one that names a changeset the store
//!   does not hold a [`VerifyProblem::ExternalIdDangling`] (checked only if the journal reads
//!   whole or the index opens, as a ref's target is); and one of system `git` whose changeset has
//!   a `git.oid` that is not the entry's key a [`VerifyProblem::ExternalIdMismatch`]. A name
//!   under `meta/external_ids/` that is no entry's is passed over.
//! - The objects. A blob a tree names that no object file holds is a
//!   [`VerifyProblem::MissingObject`]. Objects live in the fan-out directories only
//!   (`ObjectStore::list`): the temp files a crash leaves in `objects/tmp/`, and any stray
//!   entry under `objects/`, are passed over.
//!
//! A deep check also re-hashes every object file, referenced or not, and reports one whose
//! content does not match its id as a [`VerifyProblem::CorruptObject`]; and it checks every value
//! the derived store holds of the built-in derivations, the first-parent skip lists and the last
//! touches of paths, against what the changesets' objects give, and reports one that is not that
//! value as a [`VerifyProblem::DerivedMismatch`] (the values are listed, and those of the last
//! touches read, before the changesets are checked, and checked after them). A shallow check reads
//! only what the structure needs: the changeset and tree objects, which are verified as they
//! are read (so one of those that is corrupt is reported all the same), while a blob is only
//! looked for, its bytes unread, and no derived value is read.
//!
//! A damaged object is reported once, against the first changeset (in seq order) that
//! references it.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::io;
use std::sync::Arc;

use swarmops_core::bytes;
use swarmops_core::{Changeset, ChangesetId, EntryTarget, ObjectId, ObjectKind, Tree, TreeId};
use swarmops_store::external_ids;
use swarmops_store::{
    ChangesetMeta, ExternalEntry, Frame, Index, Journal, ListedEntry, ObjectStore, Record, RefFile,
    SYSTEM_GIT, SortedView, StoreError,
};

use crate::derive::{LastModified, PathKey};
use crate::error::RepoError;
use crate::repo::Repo;
use crate::tree_edit::TreeEdit;

mod derived;

/// How many journal frames verify reads with one call.
const FRAME_BATCH: usize = 4096;

/// What [`Repo::verify`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyReport {
    /// How many objects it examined. Deep: every object file, re-hashed. Shallow: the
    /// changeset and tree objects it read, and the blobs their trees name, looked for.
    pub objects_checked: u64,
    /// How many changesets it checked.
    pub changesets_checked: u64,
    /// What is damaged: metadata files that do not read first, then the journal, the index
    /// records, the refs, the changesets in seq order, the objects, the external id index's
    /// entries, by system and key, and last (deep only) the derived values, by derivation,
    /// changeset and key. Empty for a sound store.
    pub problems: Vec<VerifyProblem>,
}

/// One thing [`Repo::verify`] found damaged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyProblem {
    /// An object file whose content does not hash to its id or whose header is malformed, or a
    /// changeset or tree object whose bytes do not decode.
    CorruptObject(ObjectId),
    /// An object a changeset references (its own, a tree under its root, or a blob one of those
    /// names) that the store lacks, or holds as another kind.
    MissingObject {
        /// The first changeset, in seq order, that references it.
        referenced_by: ChangesetId,
        /// The object.
        id: ObjectId,
    },
    /// A changeset's object names a parent that the store does not hold.
    MissingParent {
        /// The changeset.
        child: ChangesetId,
        /// The parent it names.
        parent: ChangesetId,
    },
    /// A changeset's root is not its first parent's root (the empty tree for a root changeset)
    /// with its delta applied.
    RootMismatch(ChangesetId),
    /// A ref file names no changeset in the store; carries the ref's name.
    DanglingRef(String),
    /// A ref file that does not read as one (`<64 hex digits>\n<version>\n`); carries the
    /// ref's name.
    CorruptRef(String),
    /// The metadata of changeset record `seq` is wrong, as `cause` says.
    IndexMismatch {
        /// The record's place among the changesets, from 0.
        seq: u64,
        /// What disagrees with what.
        cause: MismatchCause,
    },
    /// A journal frame that the journal's reader finds corrupt starts at this offset of
    /// `journal.log`; nothing after it is read.
    JournalCorrupt {
        /// Where the frame starts.
        offset: u64,
    },
    /// A metadata file that verify cannot read as one, so that what depends on it goes
    /// unchecked: a journal file missing, `journal.ckpt` malformed or not a frame boundary, or
    /// index files that do not open. Carries what is damaged and what is not checked for it.
    /// A `meta/lock` that is not a regular file, which every write refuses, is one too; it then
    /// carries what to do.
    MetaCorrupt(String),
    /// An entry of the external id index (storage spec §7) that is not one: its file is not 44
    /// bytes of a version-1 entry with a CRC that matches, or not a regular file.
    ExternalIdCorrupt {
        /// The entry's system.
        system: String,
        /// The entry's key, in lowercase hex.
        key: String,
    },
    /// An entry of the external id index that names a changeset the store does not hold.
    ExternalIdDangling {
        /// The entry's system.
        system: String,
        /// The entry's key, in lowercase hex.
        key: String,
        /// The changeset it names.
        changeset: ChangesetId,
    },
    /// An entry of system `git` whose changeset's `git.oid` metadata is another oid than the
    /// entry's key.
    ExternalIdMismatch {
        /// The entry's system: `git`.
        system: String,
        /// The entry's key, in lowercase hex: a git commit's oid.
        key: String,
        /// The changeset it names.
        changeset: ChangesetId,
    },
    /// A value the derived store holds of a built-in derivation (storage spec §5), its file whole
    /// and its trailer right, that is not the value the changesets' objects give: a first-parent
    /// skip list whose entries are not the changesets 1, 2, 4, … first-parent steps below its
    /// changeset, or a last touch of a path that is not the nearest changeset of the first-parent
    /// chain that touches it. No crash or power loss writes one: someone placed the file, or a
    /// faulty build wrote it. A reader takes it as valid where its own checks cannot tell; removing
    /// the file, or all of `meta/derived/`, has the value derived again. Found by a deep check.
    DerivedMismatch {
        /// The derivation: `first_parent_skip_list` or `last_modified`.
        name: String,
        /// The changeset the value is about.
        changeset: ChangesetId,
        /// The value's key, as its file records it: a path's postcard bytes for `last_modified`,
        /// none for a skip list.
        key: Vec<u8>,
    },
}

/// What an [`VerifyProblem::IndexMismatch`] found disagreeing, which says what can mend it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MismatchCause {
    /// The index's record is not the journal's at that place, the index holds a record the
    /// journal does not, or it lacks one the checkpoint has passed. The journal is the source
    /// of truth: the index can be rebuilt from it.
    Journal,
    /// A sorted view's entry for the record disagrees with the index columns: its key is not
    /// the record's, it is out of order, or the view does not name the record once. The views
    /// can be rebuilt from the columns.
    SortedView,
    /// The changeset's metadata record (the journal's, or the index's where the journal
    /// cannot be read) disagrees with its object (its parents or its delta hash) or with its
    /// parents' generations: the record was written wrong, and must be derived from the object
    /// again.
    Object,
}

impl fmt::Display for VerifyProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CorruptObject(id) => write!(f, "object {id} is corrupt"),
            Self::MissingObject { referenced_by, id } => {
                write!(
                    f,
                    "object {id} is missing; changeset {referenced_by} references it"
                )
            }
            Self::MissingParent { child, parent } => write!(
                f,
                "changeset {child} names parent {parent}, which the store does not hold"
            ),
            Self::RootMismatch(id) => write!(
                f,
                "changeset {id}: its root is not its first parent's root with its delta applied"
            ),
            Self::DanglingRef(name) => write!(f, "ref {name} names no changeset in the store"),
            Self::CorruptRef(name) => write!(f, "the file of ref {name} is not a ref file"),
            Self::IndexMismatch { seq, cause } => match cause {
                MismatchCause::Journal => write!(
                    f,
                    "changeset record {seq}: the index disagrees with the journal; rebuild the \
                     index from the journal"
                ),
                MismatchCause::SortedView => write!(
                    f,
                    "changeset record {seq}: a sorted view disagrees with the index columns; \
                     rebuild the sorted views from the columns"
                ),
                MismatchCause::Object => write!(
                    f,
                    "changeset record {seq}: its metadata record (the journal's, or the index's \
                     where the journal cannot be read) disagrees with its object (parents, delta \
                     hash) or with its parents' generations; derive the record from the object \
                     again"
                ),
            },
            Self::JournalCorrupt { offset } => {
                write!(f, "the journal is corrupt at offset {offset}")
            }
            Self::MetaCorrupt(what) => write!(f, "damaged metadata: {what}"),
            Self::ExternalIdCorrupt { system, key } => {
                write!(
                    f,
                    "the entry of external id {key} of system {system} is corrupt"
                )
            }
            Self::ExternalIdDangling {
                system,
                key,
                changeset,
            } => write!(
                f,
                "external id {key} of system {system} names changeset {changeset}, which the \
                 store does not hold"
            ),
            Self::ExternalIdMismatch {
                system,
                key,
                changeset,
            } => write!(
                f,
                "external id {key} of system {system} names changeset {changeset}, whose git.oid \
                 is another oid"
            ),
            Self::DerivedMismatch {
                name,
                changeset,
                key,
            } => write!(
                f,
                "the derived value {name} of changeset {changeset}{} is not the one the store's \
                 objects give; remove its file under meta/derived/{name}/, or all of \
                 meta/derived/, to have it derived again",
                key_text(name, key)
            ),
        }
    }
}

/// How a [`VerifyProblem::DerivedMismatch`] line names the value's key: a path for
/// `last_modified`, lowercase hex for any other key, nothing for none.
fn key_text(name: &str, key: &[u8]) -> String {
    if key.is_empty() {
        return String::new();
    }
    if name == <LastModified as crate::derive::Derivation>::NAME
        && let Ok((PathKey(path), [])) = postcard::take_from_bytes::<PathKey>(key)
    {
        return format!(" under path {path}");
    }
    format!(" under key {}", external_ids::lower_hex(key))
}

/// How much of a store [`Repo::verify`] reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Depth {
    /// The structure only: the changeset and tree objects, with every blob looked for, its
    /// bytes unread, and no derived value read.
    Shallow,
    /// The structure, every object file re-hashed, and the built-in derived values checked.
    Deep,
}

impl<O: ObjectStore> Repo<O> {
    /// Checks the whole store, as [the module documentation](self) says, and reports what is
    /// damaged: a sound store gives no problems. [`Depth::Deep`] re-hashes every object file as
    /// well; [`Depth::Shallow`] covers the structure, and looks for blobs without reading them.
    /// Errors only where it cannot read on.
    pub fn verify(&self, depth: Depth) -> Result<VerifyReport, RepoError> {
        let mut check = Check::new(self, depth);
        check.lock_file();
        let changesets = check.metadata()?;
        if depth == Depth::Deep {
            check.derived = derived::Derived::read(self)?;
        }
        for meta in &changesets {
            check.changeset(meta)?;
        }
        check.external_entries()?;
        check.objects()?;
        if let Some(derived) = check.derived.take() {
            check.derived_problems = derived.check(self)?;
        }
        Ok(check.report())
    }
}

/// One run of [`Repo::verify`]: what it has found, and what it has read.
struct Check<'r, O: ObjectStore> {
    repo: &'r Repo<O>,
    /// Whether every object file is re-hashed.
    depth: Depth,
    /// Metadata files that do not read.
    meta: Vec<VerifyProblem>,
    journal_corrupt: Option<u64>,
    /// The records found wrong, with what disagreed, each pair once.
    mismatches: BTreeSet<(u64, MismatchCause)>,
    /// The refs' problems, in the order of their names.
    refs: Vec<VerifyProblem>,
    /// The changesets' and the objects' problems, in the order found.
    problems: Vec<VerifyProblem>,
    /// The generation of every changeset the metadata records.
    generations: HashMap<ChangesetId, u64>,
    /// The root of every changeset checked whose object decoded.
    roots: HashMap<ChangesetId, TreeId>,
    /// The objects re-hashed so far, whole or not.
    hashed: HashSet<ObjectId>,
    /// The objects reported missing or corrupt, each reported once.
    reported: HashSet<ObjectId>,
    /// The trees walked so far, read or not.
    walked: HashSet<ObjectId>,
    /// Every blob a tree names, with the first changeset whose tree does.
    blobs: BTreeMap<ObjectId, ChangesetId>,
    changesets_checked: u64,
    /// Whether the journal read whole or the index opened: what tells which changesets the store
    /// holds, so that what names another (a ref, an external id) is dangling.
    targets_known: bool,
    /// Every entry of the external id index, system by system, listed before the journal is
    /// read.
    external: Vec<(String, Vec<ListedEntry>)>,
    /// Whether the changesets' `git.oid` values are wanted: the index has entries of system
    /// `git`.
    wants_git_oids: bool,
    /// The `git.oid` metadata of each changeset checked whose object decoded and has one.
    git_oids: HashMap<ChangesetId, Vec<u8>>,
    /// The external id index's problems, by system and key.
    external_problems: Vec<VerifyProblem>,
    /// The derived values a deep check reads and checks, and the changesets it notes for them;
    /// `None` for a shallow check, and for a store that holds none.
    derived: Option<derived::Derived>,
    /// The derived values' problems, by derivation, changeset and key.
    derived_problems: Vec<VerifyProblem>,
}

impl<'r, O: ObjectStore> Check<'r, O> {
    fn new(repo: &'r Repo<O>, depth: Depth) -> Self {
        Self {
            repo,
            depth,
            meta: Vec::new(),
            journal_corrupt: None,
            mismatches: BTreeSet::new(),
            refs: Vec::new(),
            problems: Vec::new(),
            generations: HashMap::new(),
            roots: HashMap::new(),
            hashed: HashSet::new(),
            reported: HashSet::new(),
            walked: HashSet::new(),
            blobs: BTreeMap::new(),
            changesets_checked: 0,
            targets_known: false,
            external: Vec::new(),
            wants_git_oids: false,
            git_oids: HashMap::new(),
            external_problems: Vec::new(),
            derived: None,
            derived_problems: Vec::new(),
        }
    }

    /// Reads and compares the metadata; returns the changesets to check, in seq order.
    fn metadata(&mut self) -> Result<Vec<ChangesetMeta>, RepoError> {
        let (journal, journal_damage) = match self.open_journal()? {
            Ok(journal) => (Some(journal), None),
            Err(what) => (None, Some(what)),
        };
        let ckpt = match &journal {
            Some(journal) => self.checkpoint(journal)?,
            None => None,
        };
        let (index, index_damage) = match self.open_index()? {
            Ok(index) => (Some(index), None),
            Err(what) => (None, Some(what)),
        };
        // Before the journal is read: a writer appends a record only once its frame is durable
        // and listed, so every record the index files hold now has its frame in what is read.
        let held = match &index {
            Some(index) => index.held()?,
            None => 0,
        };
        let ref_files = self.repo.meta.ref_files()?;
        // Before the journal too: an entry is linked only once the frame that records its
        // changeset is durable (W8), so an entry listed now that names a changeset the store
        // holds names one of a frame read below.
        self.external = self.external_ids()?;
        self.wants_git_oids = (self.external.iter())
            .any(|(system, entries)| system == SYSTEM_GIT && !entries.is_empty());
        let (frames, whole) = match &journal {
            Some(journal) => {
                let (frames, corrupt) = read_frames(journal)?;
                self.journal_corrupt = corrupt;
                (frames, corrupt.is_none())
            }
            None => (Vec::new(), false),
        };
        // Every snapshot and write transaction reads the frames past the checkpoint: a
        // checkpoint that is no frame boundary fails them all, so verify must say so.
        let ckpt = match (&journal, ckpt) {
            (Some(journal), Some(ckpt)) if whole => self.checkpoint_boundary(journal, ckpt)?,
            _ => ckpt,
        };

        // What the damaged files leave unchecked depends on which of the others read.
        if let Some(what) = journal_damage {
            let rest = if index.is_some() {
                ", and the index is checked as it is"
            } else {
                ""
            };
            self.meta.push(VerifyProblem::MetaCorrupt(format!(
                "{what}: the journal is not checked{rest}"
            )));
        }
        if let Some(what) = index_damage {
            let rest = if whole {
                ", and the journal's changesets are"
            } else if journal.is_none() {
                "; with no journal either, no changeset is known, so neither changesets nor the \
                 targets of refs are checked"
            } else {
                "; with the journal read only up to its damage, neither the changesets past it \
                 nor the targets of refs are checked"
            };
            self.meta.push(VerifyProblem::MetaCorrupt(format!(
                "the changeset index does not open ({what}): it is not checked{rest}"
            )));
        }
        // Without them, a ref's target could be a changeset no readable file lists.
        let targets_known = whole || index.is_some();
        self.targets_known = targets_known;

        let journaled = journal_changesets(&frames);
        let mut records = Vec::new();
        if let Some(index) = &index {
            records = self.index_records(index)?;
            let frames = if whole { Frames::Whole } else { Frames::Cut };
            self.compare(&journaled, &records, ckpt, frames);
            // A record past the journal's last changeset, which no frame holds, whether the index
            // counts it or not.
            if whole && held > bytes::to_u64(journaled.len()) {
                self.mismatches
                    .insert((bytes::to_u64(journaled.len()), MismatchCause::Journal));
            }
            self.check_view(index.ids_view(), &records, |meta| meta.id.0.0, Keys::One)?;
            self.check_view(
                index.dh_view(),
                &records,
                |meta| meta.delta_hash,
                Keys::Many,
            )?;
        }

        let journaled_count = journaled.len();
        let mut changesets: Vec<ChangesetMeta> =
            journaled.into_iter().map(|(meta, _)| meta).collect();
        if !whole {
            // The journal stops at damage, or is gone: past it, the index's records are what
            // there is to check.
            changesets.extend(records.into_iter().skip(journaled_count).flatten());
        }
        self.generations = changesets
            .iter()
            .map(|meta| (meta.id, meta.generation))
            .collect();
        for RefFile { name, state } in ref_files {
            match state {
                Ok(state) if targets_known && !self.generations.contains_key(&state.target) => {
                    self.refs.push(VerifyProblem::DanglingRef(name));
                }
                Ok(_) => {}
                // A file that is no ref file; anything else that fails is no damage to report.
                Err(StoreError::Io(e)) if e.kind() == io::ErrorKind::InvalidData => {
                    self.refs.push(VerifyProblem::CorruptRef(name));
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(changesets)
    }

    /// Every entry of the external id index, system by system: what
    /// [`Check::external_entries`] checks once the changesets are.
    fn external_ids(&self) -> Result<Vec<(String, Vec<ListedEntry>)>, RepoError> {
        let index = &self.repo.external_ids;
        let mut listed = Vec::new();
        for system in index.systems()? {
            let entries = index.list(&system)?;
            listed.push((system, entries));
        }
        Ok(listed)
    }

    /// Reports a `meta/lock` that every write refuses before it opens it (one that is not a
    /// regular file), looked at without opening it.
    fn lock_file(&mut self) {
        if swarmops_store::MetaLock::refuses(&self.repo.layout) {
            self.meta.push(VerifyProblem::MetaCorrupt(
                "meta/lock is not a regular file: every write fails on it until it is removed, \
                 and the next write makes a new one"
                    .to_owned(),
            ));
        }
    }

    /// A handle on the journal, or what is wrong with it if one of its files is missing: the
    /// failed open names the file, relative to the store's root.
    fn open_journal(&self) -> Result<Result<Journal, String>, RepoError> {
        let layout = &self.repo.layout;
        match Journal::open(layout, Arc::clone(&self.repo.fs)) {
            Ok(journal) => Ok(Ok(journal)),
            Err(StoreError::Io(e)) if e.kind() == io::ErrorKind::NotFound => {
                Ok(Err(match &e.file {
                    Some(file) => format!("{file} is missing"),
                    None => format!("the journal does not open ({e})"),
                }))
            }
            Err(e) => Err(e.into()),
        }
    }

    /// The journal's checkpoint; `None`, reported, if `journal.ckpt` is malformed.
    fn checkpoint(&mut self, journal: &Journal) -> Result<Option<u64>, RepoError> {
        match journal.ckpt() {
            Ok(ckpt) => Ok(Some(ckpt)),
            Err(StoreError::Io(e)) if e.kind() == io::ErrorKind::InvalidData => {
                self.meta.push(VerifyProblem::MetaCorrupt(
                    "meta/journal.ckpt does not hold a u64le offset: the records the checkpoint \
                     passed are not checked against the index"
                        .to_owned(),
                ));
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Whether the checkpoint is a frame boundary (0, a listed frame's start, or the end of the
    /// last listed frame), read as every snapshot and write transaction reads the frames past
    /// it; reported, and `None`, if not. Checked only when the journal read whole: otherwise
    /// the damage reported is the journal's.
    fn checkpoint_boundary(
        &mut self,
        journal: &Journal,
        ckpt: u64,
    ) -> Result<Option<u64>, RepoError> {
        match journal.tail_frames_from(ckpt) {
            Ok(_) => Ok(Some(ckpt)),
            Err(StoreError::JournalCorrupt { .. }) => {
                self.meta.push(VerifyProblem::MetaCorrupt(format!(
                    "meta/journal.ckpt holds {ckpt}, which is not a frame boundary: reads and \
                     writes fail on it, and the records the checkpoint passed are not checked \
                     against the index"
                )));
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// A handle on the changeset index, or what is wrong with its files if they are too
    /// damaged to open.
    fn open_index(&self) -> Result<Result<Index, &'static str>, RepoError> {
        match Index::open(&self.repo.layout, Arc::clone(&self.repo.fs)) {
            Ok(index) => Ok(Ok(index)),
            Err(StoreError::IndexCorrupt { what }) => Ok(Err(what)),
            Err(e) => Err(e.into()),
        }
    }

    /// The index's records, by rec; `None` for one that does not read (a mismatch).
    fn index_records(&mut self, index: &Index) -> Result<Vec<Option<ChangesetMeta>>, RepoError> {
        let mut records = Vec::new();
        for rec in 0..index.len() {
            match index.get(rec) {
                Ok(meta) => records.push(Some(meta)),
                Err(StoreError::IndexCorrupt { .. }) => {
                    self.mismatches.insert((rec, MismatchCause::Journal));
                    records.push(None);
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(records)
    }

    /// Compares the index's records with the journal's changeset records (each with its
    /// frame's offset), place by place.
    fn compare(
        &mut self,
        journaled: &[(ChangesetMeta, u64)],
        records: &[Option<ChangesetMeta>],
        ckpt: Option<u64>,
        frames: Frames,
    ) {
        for (rec, record) in records.iter().enumerate() {
            let Some(record) = record else { continue };
            let wrong = match journaled.get(rec) {
                Some((want, _)) => want != record,
                // A record no committed transaction holds; past damage in the journal, or with
                // no journal, there is no telling.
                None => frames == Frames::Whole,
            };
            if wrong {
                self.mismatches
                    .insert((bytes::to_u64(rec), MismatchCause::Journal));
            }
        }
        // The checkpoint passes a record only once the index holds it durably.
        let Some(ckpt) = ckpt else { return };
        for (seq, (_, offset)) in journaled.iter().enumerate().skip(records.len()) {
            if *offset < ckpt {
                self.mismatches
                    .insert((bytes::to_u64(seq), MismatchCause::Journal));
            }
        }
    }

    /// Checks a sorted view against the index columns: each entry's key must be the key of the
    /// record it names, the entries ascending (strictly by key for the id view, by key and then
    /// record for the delta-hash view), and a view of m entries must name each of the records
    /// 0 to m - 1 once. A record named wrongly, or not at all, is a mismatch.
    fn check_view(
        &mut self,
        view: &SortedView<32>,
        records: &[Option<ChangesetMeta>],
        key_of: impl Fn(&ChangesetMeta) -> [u8; 32],
        keys: Keys,
    ) -> Result<(), RepoError> {
        // The index opened only views that cover no more records than it holds.
        let mut named = vec![false; to_usize(view.len())];
        let mut previous: Option<([u8; 32], u64)> = None;
        for at in 0..view.len() {
            let (key, rec) = (view.key_at(at)?, view.rec_at(at)?);
            let slot = usize::try_from(rec).ok().filter(|&slot| slot < named.len());
            // A record named twice is named once under a key that is not its own, or out of
            // order: the coverage below finds the record it pushed out.
            let keyed = slot.is_some_and(|slot| {
                records[slot]
                    .as_ref()
                    .is_some_and(|meta| key_of(meta) == key)
            });
            let ordered = previous.is_none_or(|before| match keys {
                Keys::One => before.0 < key,
                Keys::Many => before < (key, rec),
            });
            if let Some(slot) = slot {
                named[slot] = true;
            }
            if !(keyed && ordered) {
                self.mismatches.insert((rec, MismatchCause::SortedView));
            }
            previous = Some((key, rec));
        }
        for (rec, named) in named.iter().enumerate() {
            if !named {
                self.mismatches
                    .insert((bytes::to_u64(rec), MismatchCause::SortedView));
            }
        }
        Ok(())
    }

    /// Checks one changeset: its object, its record against the object and its parents'
    /// generations, its parents, the trees under its root, and its root.
    fn changeset(&mut self, meta: &ChangesetMeta) -> Result<(), RepoError> {
        self.changesets_checked += 1;
        let id = meta.id;
        let Some(payload) = self.read(id.0, ObjectKind::Changeset, id)? else {
            return Ok(());
        };
        let Ok(cs) = Changeset::decode(&payload) else {
            self.corrupt(id.0);
            return Ok(());
        };
        if self.wants_git_oids
            && let Some((_, oid)) = cs.metadata.iter().find(|(key, _)| key == "git.oid")
        {
            self.git_oids.insert(id, oid.clone());
        }
        if let Some(derived) = &mut self.derived {
            derived.note(id, &cs);
        }
        // 1 + the largest parent generation, if the metadata holds every parent.
        let generation = meta
            .parents
            .iter()
            .try_fold(0, |max: u64, parent| {
                self.generations.get(parent).map(|&g| max.max(g))
            })
            .map(|max| max.saturating_add(1));
        if meta.parents != cs.parents
            || meta.delta_hash != cs.delta_hash()?
            || generation.is_some_and(|want| meta.generation != want)
        {
            self.mismatches.insert((meta.seq, MismatchCause::Object));
        }
        for &parent in &cs.parents {
            if !self.generations.contains_key(&parent) {
                self.problems
                    .push(VerifyProblem::MissingParent { child: id, parent });
            }
        }
        self.walk(cs.root, id)?;
        self.check_root(id, &cs)?;
        self.roots.insert(id, cs.root);
        Ok(())
    }

    /// Walks the trees under `root` that no earlier walk reached: reports the missing and
    /// corrupt ones (against `by`), and notes the blobs the others name.
    fn walk(&mut self, root: TreeId, by: ChangesetId) -> Result<(), RepoError> {
        let mut stack = vec![root.0];
        while let Some(id) = stack.pop() {
            if !self.walked.insert(id) {
                continue;
            }
            let Some(payload) = self.read(id, ObjectKind::Tree, by)? else {
                continue;
            };
            let Ok(tree) = Tree::decode(&payload) else {
                self.corrupt(id);
                continue;
            };
            for entry in tree.entries() {
                match entry.target {
                    EntryTarget::Dir(tree) => stack.push(tree.0),
                    EntryTarget::Leaf(_, blob) => {
                        self.blobs.entry(blob.0).or_insert(by);
                    }
                }
            }
        }
        Ok(())
    }

    /// Reports a [`VerifyProblem::RootMismatch`] if applying `cs`'s delta to its first parent's
    /// root (the empty tree for a root changeset) does not give its root. Skipped when that
    /// cannot be told: the first parent's object, or a tree the delta reaches into, is missing
    /// or corrupt (each reported on its own).
    fn check_root(&mut self, id: ChangesetId, cs: &Changeset) -> Result<(), RepoError> {
        let repo = self.repo;
        let edit = match cs.parents.first() {
            None => Ok(TreeEdit::empty(&repo.objects)),
            Some(parent) => match self.roots.get(parent) {
                Some(&base) => TreeEdit::new(&repo.objects, base),
                None => return Ok(()),
            },
        };
        let applied = edit.and_then(|mut edit| {
            edit.apply(&cs.delta)?;
            edit.finish()
        });
        match applied {
            Ok(encoded) if encoded.root == cs.root => Ok(()),
            Ok(_) | Err(RepoError::TypeConflict { .. } | RepoError::PathNotFound { .. }) => {
                self.problems.push(VerifyProblem::RootMismatch(id));
                Ok(())
            }
            Err(
                RepoError::MissingObject { .. }
                | RepoError::Codec(_)
                | RepoError::Store(StoreError::Corrupt { .. } | StoreError::BadHeader { .. }),
            ) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Checks each entry of the external id index listed before the journal was read: its file
    /// ([`VerifyProblem::ExternalIdCorrupt`]); that the store holds its changeset
    /// ([`VerifyProblem::ExternalIdDangling`]), when the journal or the index tells which it
    /// holds; and, for system `git`, that the changeset's `git.oid`, if it has one, is the
    /// entry's key ([`VerifyProblem::ExternalIdMismatch`]).
    fn external_entries(&mut self) -> Result<(), RepoError> {
        for (system, entries) in std::mem::take(&mut self.external) {
            for (key, entry) in entries {
                let key = external_ids::lower_hex(&key);
                let problem = match entry {
                    Err(StoreError::ExternalIdCorrupt { .. }) => VerifyProblem::ExternalIdCorrupt {
                        system: system.clone(),
                        key,
                    },
                    Err(e) => return Err(e.into()),
                    Ok(ExternalEntry { changeset, .. })
                        if self.targets_known && !self.generations.contains_key(&changeset) =>
                    {
                        VerifyProblem::ExternalIdDangling {
                            system: system.clone(),
                            key,
                            changeset,
                        }
                    }
                    Ok(ExternalEntry { changeset, .. })
                        if system == SYSTEM_GIT
                            && (self.git_oids.get(&changeset))
                                .is_some_and(|oid| *oid != key.as_bytes()) =>
                    {
                        VerifyProblem::ExternalIdMismatch {
                            system: system.clone(),
                            key,
                            changeset,
                        }
                    }
                    Ok(_) => continue,
                };
                self.external_problems.push(problem);
            }
        }
        Ok(())
    }

    /// Reports the blobs the trees name that no object file holds; a deep check re-hashes
    /// every object file not re-hashed yet first.
    fn objects(&mut self) -> Result<(), RepoError> {
        let listed = self.repo.objects.list()?;
        // Deep: the blobs, as their files' headers say. Shallow: whatever has a file.
        let present = if self.depth == Depth::Deep {
            self.rehash(listed)?
        } else {
            listed.into_iter().collect()
        };
        let missing: Vec<(ObjectId, ChangesetId)> = self
            .blobs
            .iter()
            .filter(|(id, _)| !present.contains(*id))
            .map(|(&id, &by)| (id, by))
            .collect();
        for (id, by) in missing {
            self.missing(by, id);
        }
        Ok(())
    }

    /// Re-hashes the objects of `listed` not re-hashed yet, reporting the corrupt ones;
    /// returns those that are blobs.
    fn rehash(&mut self, listed: Vec<ObjectId>) -> Result<HashSet<ObjectId>, RepoError> {
        let repo = self.repo;
        let mut blobs = HashSet::new();
        for id in listed {
            if self.hashed.contains(&id) {
                continue;
            }
            match repo.objects.get(id) {
                Ok(Some((kind, _))) => {
                    self.hashed.insert(id);
                    if kind == ObjectKind::Blob {
                        blobs.insert(id);
                    }
                }
                // Gone since the listing (nothing removes objects): not there to check.
                Ok(None) => {}
                Err(StoreError::Corrupt { .. } | StoreError::BadHeader { .. }) => {
                    self.hashed.insert(id);
                    self.corrupt(id);
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(blobs)
    }

    /// The payload of object `id`, re-hashed, if it is a `kind` object; otherwise `None`, once
    /// it is reported: missing (against `referenced_by`), or corrupt.
    fn read(
        &mut self,
        id: ObjectId,
        kind: ObjectKind,
        referenced_by: ChangesetId,
    ) -> Result<Option<Vec<u8>>, RepoError> {
        match self.repo.objects.get(id) {
            Ok(Some((found, payload))) => {
                self.hashed.insert(id);
                if found == kind {
                    return Ok(Some(payload));
                }
                self.missing(referenced_by, id);
            }
            Ok(None) => self.missing(referenced_by, id),
            Err(StoreError::Corrupt { .. } | StoreError::BadHeader { .. }) => {
                self.hashed.insert(id);
                self.corrupt(id);
            }
            Err(e) => return Err(e.into()),
        }
        Ok(None)
    }

    fn missing(&mut self, referenced_by: ChangesetId, id: ObjectId) {
        if self.reported.insert(id) {
            self.problems
                .push(VerifyProblem::MissingObject { referenced_by, id });
        }
    }

    fn corrupt(&mut self, id: ObjectId) {
        if self.reported.insert(id) {
            self.problems.push(VerifyProblem::CorruptObject(id));
        }
    }

    fn report(self) -> VerifyReport {
        // Shallow, the blobs were looked for, not read.
        let looked_for = if self.depth == Depth::Deep {
            0
        } else {
            let unread = self.blobs.keys().filter(|id| !self.hashed.contains(*id));
            unread.count()
        };
        let mut problems = self.meta;
        problems.extend(
            self.journal_corrupt
                .map(|offset| VerifyProblem::JournalCorrupt { offset }),
        );
        problems.extend(
            self.mismatches
                .iter()
                .map(|&(seq, cause)| VerifyProblem::IndexMismatch { seq, cause }),
        );
        problems.extend(self.refs);
        problems.extend(self.problems);
        problems.extend(self.external_problems);
        problems.extend(self.derived_problems);
        VerifyReport {
            objects_checked: bytes::to_u64(self.hashed.len() + looked_for),
            changesets_checked: self.changesets_checked,
            problems,
        }
    }
}

/// Every committed frame of `journal`, in order, read without a lock: the frames
/// `journal.off` lists, each of which must read whole (the last one too), then the whole
/// frames after them that a crash left unlisted. With where the journal is corrupt, if it is;
/// the frames are then the ones before that point.
fn read_frames(journal: &Journal) -> Result<(Vec<Frame>, Option<u64>), RepoError> {
    let mut frames: Vec<Frame> = Vec::new();
    let mut batch = FRAME_BATCH;
    loop {
        match journal.frames_from(bytes::to_u64(frames.len()), batch) {
            Ok(read) if read.is_empty() => break,
            Ok(read) => frames.extend(read),
            Err(StoreError::JournalCorrupt { offset }) if batch == 1 => {
                return Ok((frames, Some(offset)));
            }
            // The damage lies in this batch: read on a frame at a time, keeping those before.
            Err(StoreError::JournalCorrupt { .. }) => batch = 1,
            Err(e) => return Err(e.into()),
        }
    }
    // From the last listed frame's start, a frame boundary: it comes again, then the whole
    // frames after it, listed since or never.
    let again = usize::from(!frames.is_empty());
    let start = frames.last().map_or(0, |frame| frame.offset);
    match journal.tail_frames_from(start) {
        Ok(tail) => {
            frames.extend(tail.into_iter().skip(again));
            Ok((frames, None))
        }
        Err(StoreError::JournalCorrupt { offset }) => Ok((frames, Some(offset))),
        Err(e) => Err(e.into()),
    }
}

/// The changeset records of `frames`, in order, each as the index should hold it (its seq its
/// place among them), with the offset of its frame.
fn journal_changesets(frames: &[Frame]) -> Vec<(ChangesetMeta, u64)> {
    let mut changesets = Vec::new();
    for frame in frames {
        for record in &frame.records {
            if let Record::Changeset {
                id,
                generation,
                delta_hash,
                parents,
            } = record
            {
                let meta = ChangesetMeta {
                    id: *id,
                    seq: bytes::to_u64(changesets.len()),
                    generation: *generation,
                    delta_hash: *delta_hash,
                    parents: parents.clone(),
                };
                changesets.push((meta, frame.offset));
            }
        }
    }
    changesets
}

/// The journal's frames verify read: all of them, or only up to damage (or none, with no
/// journal).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Frames {
    Whole,
    Cut,
}

/// The records a sorted view's key names: one (the id view), or many, which then ascend by
/// record (the delta-hash view).
#[derive(Debug, Clone, Copy)]
enum Keys {
    One,
    Many,
}

/// `n` as a `usize`; the counts verify converts are lengths of files it has open.
fn to_usize(n: u64) -> usize {
    usize::try_from(n).expect("a count of entries in an open file fits in usize")
}
