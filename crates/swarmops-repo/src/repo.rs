//! The repository handle: [`Repo`], over an object store and the native metadata store, and
//! [`LocalRepo`], the one on the local filesystem, which also owns the store's lifecycle.
//!
//! # A store's files (storage spec §2)
//!
//! ```text
//! <store>/FORMAT    "swarmops-store 1\n": written last by create, checked first by open
//! <store>/REPO      the repository's identity: its id (32 hex digits), then its name
//! <store>/meta/     the native metadata store (swarmops_store::MetaStore)
//! <store>/objects/  the 256 fan-out directories of the loose objects, and tmp/
//! ```
//!
//! # The `REPO` file
//!
//! `REPO` holds the repository's identity: a line with its id, 32 lowercase hex digits chosen
//! at random by create, then its name, verbatim (any UTF-8, line breaks included, at most 255
//! bytes) to the end of the file, with no line break added:
//!
//! ```text
//! 3f9c2d0a8e1b4c7f6a5d2e9b0c8f1a37
//! acme
//! ```
//!
//! Create writes it in step 4 below, before `meta/` and `FORMAT`, without replacing anything
//! (storage spec §2, rule 1: a temp file in `objects/tmp/`, synced, linked no-clobber, the root
//! synced), so a store with `FORMAT` always has it, and it never changes afterwards; a create
//! that finds one (a create racing it, or one a crash stopped) takes its identity. Open
//! requires it alongside `FORMAT`: after refusing a path that leads under
//! `/mnt/` as create does ([`StoreError::UnsupportedFs`]) and checking `FORMAT`, it reads
//! `REPO`, and refuses a store whose `REPO` is missing, longer than an id line and a 255-byte
//! name (measured before it is read, so a crafted one of huge length is never read into
//! memory), not UTF-8, or does not start with an id line ([`StoreError::Format`]).
//!
//! # Create
//!
//! [`LocalRepo::create`] lays a store out so that it is whole once `FORMAT` exists, and
//! resumes after a crash at any of its steps:
//!
//! 1. It works out where the store path leads (each symlink followed, `..` taken, missing
//!    directories kept as named) and refuses a path that leads under `/mnt/` (where WSL mounts
//!    Windows drives) before it creates anything. Then it makes the missing directories, syncs
//!    the parent of each one it made (or of the store directory alone if that was there), and
//!    checks again once the path is canonical.
//! 2. It refuses a directory that holds `FORMAT`: a store is there already
//!    ([`RepoError::Exists`]).
//! 3. It creates `objects/` with its 256 fan-out directories and `tmp/`, keeping those that
//!    exist already, and makes them durable: one fsync of `objects/`, one of the store root.
//! 4. It links `REPO` with a new repository id unless one is there (see above), syncs the store
//!    root, and takes an exclusive lock on `REPO` (std `File::lock`) for the rest of the
//!    create: a second create of the store waits there, and then finds `FORMAT`
//!    ([`RepoError::Exists`]) or, after a crash, finishes the store itself.
//! 5. It creates `meta/` with `MetaStore::create`, which is crash-atomic, or opens the whole one
//!    a crashed create left behind.
//! 6. Last, it links `FORMAT` into place without replacing anything, from a temp file in
//!    `objects/tmp/` that is synced first (so `FORMAT` is never seen half written), and syncs
//!    the store root. If that sync fails, it takes `FORMAT` back (removed, the removal synced)
//!    and fails: Linux reports a failed writeback once, so no later sync could tell whether the
//!    name reached the disk, and the next create completes the store. If even that fails, the
//!    process aborts, as a crash between the link and the sync would leave it.
//!
//! A crash before the link leaves no `FORMAT`: open refuses the directory, and the next create
//! picks up where the crashed one stopped. Of two creates of one store at once, one makes it and
//! the other fails with [`RepoError::Exists`], having replaced nothing.

use std::fs::File;
use std::hash::{BuildHasher, RandomState};
use std::io::{self, Write};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use camino::{Utf8Component, Utf8Path, Utf8PathBuf};
use swarmops_core::bytes;
use swarmops_core::{Changeset, ChangesetId, ObjectId, ObjectKind, Tree, TreeId};
use swarmops_store::{
    CasResult, DerivedStore, ExternalEntry, ExternalIds, ExternalPut, FileOp, FsObjectStore, FsOps,
    HookedFs, IoFailure, Layout, LinkOutcome, MetaStore, ObjectStore, RealFs, Recovered, RefState,
    StoreError, StoreFs,
};

use crate::derive::Registry;
use crate::error::RepoError;
use crate::read::RepoRead;

/// What a store's `FORMAT` file holds: the store layout this build writes and reads.
const FORMAT: &[u8] = b"swarmops-store 1\n";

/// The version number `FORMAT` names.
const STORE_VERSION: u32 = 1;

/// The most bytes a repository's name may have: create refuses a longer one
/// ([`swarmops_store::LimitKind::RepoNameBytes`]).
const MAX_NAME_LEN: usize = 255;

/// The most bytes `REPO` may hold: the id line (32 hex digits and a line break) and a name of
/// at most [`MAX_NAME_LEN`] bytes. Open refuses a longer `REPO` by its length, unread.
const MAX_REPO_LEN: u64 = 32 + 1 + 255;

/// A repository: objects in `O`, metadata in the native [`MetaStore`].
#[non_exhaustive]
pub struct Repo<O: ObjectStore> {
    pub(crate) objects: O,
    pub(crate) meta: MetaStore,
    /// The store's paths: [`Repo::verify`] reads the journal and the index files itself.
    pub(crate) layout: Layout,
    /// What both stores write through; verify's own handles on the metadata use it too.
    pub(crate) fs: Arc<dyn FsOps>,
    /// The cache of derived values, `meta/derived/`, on the same filesystem ([`Repo::derived`]).
    pub(crate) derived: DerivedStore,
    /// The derivations whose values [`Repo::derived`] makes: the built-ins, and the ones
    /// [`Repo::register`] adds.
    pub(crate) derivations: Registry,
    /// The external id index (storage spec §7), on the same filesystem.
    pub(crate) external_ids: ExternalIds,
    identity: Identity,
}

/// The repository on the local filesystem: loose object files and the native metadata store.
pub type LocalRepo = Repo<FsObjectStore>;

/// What [`LocalRepo::info`] reports about a repository.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RepoInfo {
    /// The id [`LocalRepo::create`] chose: 32 lowercase hex digits, random.
    pub repo_id: String,
    /// The name the repository was created with.
    pub name: String,
    /// The store layout version, `FORMAT`'s number: 1.
    pub store_version: u32,
    /// How many changesets the store holds.
    pub changesets: u64,
    /// How many refs exist.
    pub refs: u64,
    /// What `main` names, if it exists.
    pub main: Option<ChangesetId>,
}

impl RepoInfo {
    /// Makes one of the fields given.
    pub fn new(
        repo_id: String,
        name: String,
        store_version: u32,
        changesets: u64,
        refs: u64,
        main: Option<ChangesetId>,
    ) -> RepoInfo {
        RepoInfo {
            repo_id,
            name,
            store_version,
            changesets,
            refs,
            main,
        }
    }
}

/// What [`Repo::cas_ref`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CasOutcome {
    /// What the store decided under the lock: [`CasResult::Applied`] when the ref named
    /// `expected` (it names `new` now; with `old` equal to `new` for a move to the target it
    /// expects, which is not recorded), [`CasResult::Mismatch`] with what it names otherwise.
    pub result: CasResult,
    /// Steps after the commit point that failed, one line each: the move is committed all
    /// the same.
    pub warnings: Vec<String>,
    /// What the write transaction's recovery repaired before the compare-and-swap, of what
    /// earlier writers left ([`swarmops_store::MetaWrite::recovered`]): on every outcome,
    /// [`CasResult::Mismatch`] included. It is no warning, and it is lost when the call fails
    /// after the repair.
    pub recovered: Recovered,
}

impl CasOutcome {
    /// Makes one of the fields given.
    pub fn new(result: CasResult, warnings: Vec<String>, recovered: Recovered) -> CasOutcome {
        CasOutcome {
            result,
            warnings,
            recovered,
        }
    }
}

/// What `REPO` holds.
struct Identity {
    repo_id: String,
    name: String,
}

impl LocalRepo {
    /// Creates a store at `store` (the directory is created if missing) for a repository
    /// named `name`, and opens it: see [the module documentation](self) for the steps.
    /// [`RepoError::Exists`] if the directory holds a store already (its `FORMAT` file);
    /// [`StoreError::UnsupportedFs`] for a path under `/mnt/`. A name of more than 255 bytes is
    /// refused with [`StoreError::Limit`]
    /// ([`RepoNameBytes`](swarmops_store::LimitKind::RepoNameBytes)) before anything is
    /// created; any other is kept verbatim.
    ///
    /// With `SWARMOPS_TEST_CRASH_AT=<k>` set (crash tests; only in a build with the store's
    /// `test-hooks` feature, which this crate's turns on), the object store and the metadata
    /// store share one `swarmops_store::CrashAtFs` whose k-th call aborts the process, and
    /// objects are written (and the directories and the index's files synced) one at a time, so
    /// that the k-th call is the same call in every run. With `SWARMOPS_TEST_FS_CALLS=<file>`
    /// set, the same holds, and that `CrashAtFs` writes how many calls both stores made to
    /// `<file>` when the repository is dropped: a dry run's count.
    /// `SWARMOPS_TEST_FAIL_AT=<k>[:<mode>[:<errno>]]` fails call k of the same count with an
    /// injected I/O error instead, and `SWARMOPS_TEST_ORDER_CHECK=1` checks the order of the
    /// stores' writes ([`HookedFs::from_env`]). A build without the feature ignores the
    /// variables.
    pub fn create(store: &Utf8Path, name: &str) -> Result<LocalRepo, RepoError> {
        let (fs, threads) = default_fs();
        Self::create_on(store, name, fs, threads)
    }

    /// [`LocalRepo::create`] with every filesystem change going through `fs` (fault injection in
    /// tests); objects are written in parallel as usual.
    pub fn create_with_fs(
        store: &Utf8Path,
        name: &str,
        fs: Arc<dyn FsOps>,
    ) -> Result<LocalRepo, RepoError> {
        Self::create_on(store, name, fs, Threads::Pool)
    }

    /// Opens the store at `store`. Checks `FORMAT` first, then reads `REPO`:
    /// [`StoreError::Format`] if `FORMAT` is missing (no store there) or names another layout
    /// version, if `REPO` is missing, malformed or longer than an id line and a 255-byte name,
    /// or if `meta/` is missing; and, before any of that, [`StoreError::UnsupportedFs`] if the
    /// path leads under `/mnt/`, as `create` refuses it. Honors `SWARMOPS_TEST_CRASH_AT` and
    /// `SWARMOPS_TEST_FS_CALLS` as [`LocalRepo::create`] does.
    pub fn open(store: &Utf8Path) -> Result<LocalRepo, RepoError> {
        let (fs, threads) = default_fs();
        Self::open_on(store, fs, threads)
    }

    /// [`LocalRepo::open`] with every filesystem access of both stores going through `fs`.
    pub fn open_with_fs(store: &Utf8Path, fs: Arc<dyn FsOps>) -> Result<LocalRepo, RepoError> {
        Self::open_on(store, fs, Threads::Pool)
    }

    /// The repository's identity and counts, in a snapshot of its own ([`RepoRead::info`]).
    pub fn info(&self) -> Result<RepoInfo, RepoError> {
        self.snapshot()?.info()
    }

    fn create_on(
        store: &Utf8Path,
        name: &str,
        fs: Arc<dyn FsOps>,
        threads: Threads,
    ) -> Result<LocalRepo, RepoError> {
        // A name longer than open reads back from REPO is refused before anything is made, and
        // before the directory is checked for a store already there.
        if name.len() > MAX_NAME_LEN {
            return Err(StoreError::Limit {
                what: swarmops_store::LimitKind::RepoNameBytes,
                max: bytes::to_u64(MAX_NAME_LEN),
                got: bytes::to_u64(name.len()),
            }
            .into());
        }
        let root = native_root(store, &fs)?;
        let layout = Layout::new(root.clone());
        // Each call below names the file of the store it failed on.
        let fs = StoreFs::new(fs, &root);
        if fs.exists(layout.format_file().as_std_path())? {
            return Err(RepoError::Exists);
        }
        let objects = layout.objects_dir();
        for byte in 0..=u8::MAX {
            let fanout = objects.join(format!("{byte:02x}"));
            fs.create_dir_all(fanout.as_std_path())?;
        }
        fs.create_dir_all(layout.objects_tmp().as_std_path())?;
        fs.fsync_dir(objects.as_std_path())?;

        // 1. REPO, linked never replacing one there: the root's sync makes it and objects/
        //    durable together.
        let identity = Identity {
            repo_id: new_repo_id(),
            name: name.to_owned(),
        };
        let repo_file = format!("{}\n{}", identity.repo_id, identity.name);
        let temp =
            fs.write_durable_temp(layout.objects_tmp().as_std_path(), repo_file.as_bytes())?;
        let claimed = fs.link_noclobber(&temp, layout.repo_file().as_std_path())?;
        fs.fsync_dir(root.as_std_path())?;

        // 2. One there already is a create's racing this one, or a crashed one's: its identity
        //    is the store's, read before the lock (a Windows lock bars reads).
        let identity = match claimed {
            LinkOutcome::Placed => identity,
            LinkOutcome::AlreadyExists => read_identity(&layout, &fs)?,
            _ => return Err(RepoError::unknown_variant("LinkOutcome")),
        };

        // 3. One create at a time from here; one that waited finds FORMAT, linked by the
        //    create it waited for.
        let _held = lock_repo(&layout, &fs)?;
        if fs.exists(layout.format_file().as_std_path())? {
            return Err(RepoError::Exists);
        }

        // 4. MetaStore::create renames a whole meta/ into place, so one that exists is whole.
        let meta = if fs.exists(layout.meta_dir().as_std_path())? {
            MetaStore::open_with_fs(&root, Arc::clone(fs.ops()))?
        } else {
            MetaStore::create_with_fs(&root, Arc::clone(fs.ops()))?
        };

        // 5. FORMAT, last: the store is complete.
        let temp = fs.write_durable_temp(layout.objects_tmp().as_std_path(), FORMAT)?;
        let format_file = layout.format_file();
        // Another create that finished first.
        if fs.link_noclobber(&temp, format_file.as_std_path())? == LinkOutcome::AlreadyExists {
            return Err(RepoError::Exists);
        }
        if let Err(e) = fs.fsync_dir(root.as_std_path()) {
            take_format_back(&fs, &root, &e);
            return Err(e.into());
        }
        Ok(Self::assemble(
            layout,
            Arc::clone(fs.ops()),
            meta,
            identity,
            threads,
        ))
    }

    fn open_on(
        store: &Utf8Path,
        fs: Arc<dyn FsOps>,
        threads: Threads,
    ) -> Result<LocalRepo, RepoError> {
        let root = absolute(store)?;
        // Where the path leads (symlinks followed whether or not their target exists, `..` as
        // the kernel takes it), as `create` resolves it: a store that leads under /mnt/ is
        // refused on open as on create, whether it was moved, copied or linked there.
        let (target, _) = resolve(&root)?;
        if !swarmops_store::is_native_linux_fs(&target) {
            return Err(StoreError::UnsupportedFs(target).into());
        }
        let layout = Layout::new(root.clone());
        // A FORMAT longer than the layout's names no layout this build reads, however long it
        // is: measured before it is read (through `std::fs`, no `FsOps` call), so that a
        // crafted file of huge length is never read into memory. A missing one, and one that is
        // no regular file (whose size says nothing of its content), reach the read, which
        // refuses the latter as such.
        if let Ok(meta) = std::fs::metadata(layout.format_file().as_std_path())
            && meta.is_file()
            && meta.len() > bytes::to_u64(FORMAT.len())
        {
            return Err(StoreError::Format(format!(
                "{root}/FORMAT is {} bytes long, not a store layout this build reads",
                meta.len()
            ))
            .into());
        }
        // Each call below names the file of the store it failed on.
        let store_fs = StoreFs::new(Arc::clone(&fs), &root);
        match store_fs.read(layout.format_file().as_std_path())? {
            Some(format) if format == FORMAT => {}
            Some(_) => {
                return Err(StoreError::Format(format!(
                    "{root}/FORMAT names a store layout this build does not read"
                ))
                .into());
            }
            None => {
                return Err(
                    StoreError::Format(format!("{root} is not a store: no FORMAT file")).into(),
                );
            }
        }
        let identity = read_identity(&layout, &store_fs)?;
        let meta = MetaStore::open_with_fs(&root, Arc::clone(&fs))?;
        Ok(Self::assemble(layout, fs, meta, identity, threads))
    }

    fn assemble(
        layout: Layout,
        fs: Arc<dyn FsOps>,
        mut meta: MetaStore,
        identity: Identity,
        threads: Threads,
    ) -> LocalRepo {
        let mut objects = FsObjectStore::open_shared(layout.clone(), Arc::clone(&fs));
        let mut external_ids = ExternalIds::new(layout.clone(), Arc::clone(&fs));
        if threads == Threads::One {
            objects.set_parallelism(1);
            meta.set_parallelism(1);
            external_ids.set_parallelism(1);
        }
        let derived = DerivedStore::open(layout.clone(), Arc::clone(&fs));
        Repo {
            objects,
            meta,
            layout,
            fs,
            derived,
            derivations: Registry::builtins(),
            external_ids,
            identity,
        }
    }
}

impl RepoRead<'_, FsObjectStore> {
    /// The repository's identity, and its counts as the snapshot shows them.
    pub fn info(&self) -> Result<RepoInfo, RepoError> {
        let identity = &self.repo.identity;
        Ok(RepoInfo {
            repo_id: identity.repo_id.clone(),
            name: identity.name.clone(),
            store_version: STORE_VERSION,
            changesets: self.meta.count_changesets()?,
            refs: bytes::to_u64(self.meta.list_refs()?.len()),
            main: self.meta.get_ref("main")?.map(|state| state.target),
        })
    }
}

impl<O: ObjectStore> RepoRead<'_, O> {
    /// Ref `name`, as the snapshot shows it; `None` if it does not exist.
    /// [`RepoError::RefName`] for a name that is not `main` or `release/<x>`.
    pub fn get_ref(&self, name: &str) -> Result<Option<RefState>, RepoError> {
        self.meta.get_ref(name).map_err(ref_error)
    }

    /// Every ref the snapshot shows, by name.
    pub fn list_refs(&self) -> Result<Vec<(String, RefState)>, RepoError> {
        Ok(self.meta.list_refs()?)
    }
}

impl<O: ObjectStore> Repo<O> {
    /// The store's filesystem handle, rooted at the store, for the files that live beside its
    /// objects and metadata (a store's `remotes.toml`). A write through it goes through the
    /// same [`FsOps`] as the store's own, so the test hooks and the fault doubles see it, and a
    /// failure names its file relative to the store's root.
    pub fn store_fs(&self) -> StoreFs {
        StoreFs::new(Arc::clone(&self.fs), self.layout.root())
    }

    /// Stores `objects`, each a kind and its canonical payload, in one batch, and returns their
    /// ids in input order; an object the store holds already is not written again
    /// ([`ObjectStore::put_batch`]). The payloads are stored as given: nothing here checks that a
    /// tree or a changeset decodes. Each object's file is durable on return, its name once this
    /// handle next flushes its objects, which a commit does before it records its changeset
    /// (storage spec §2 rule 2): until then no metadata may name the objects, since a power loss
    /// could take their names back.
    pub fn put_objects(
        &self,
        objects: Vec<(ObjectKind, Vec<u8>)>,
    ) -> Result<Vec<ObjectId>, RepoError> {
        Ok(self.objects.put_batch(objects)?)
    }

    /// Whether the store holds an object file for `id`. The file is not read, so neither its
    /// kind nor its bytes are checked ([`ObjectStore::contains`]); [`Repo::verify`] with `deep`
    /// checks them.
    pub fn contains_object(&self, id: ObjectId) -> Result<bool, RepoError> {
        Ok(self.objects.contains(id)?)
    }

    /// Changeset `id`'s record in the metadata, from a snapshot of its own: its seq, its
    /// generation, the hash of its delta and its parents in their order; `None` if the store
    /// does not hold it. No object is read, where [`Repo::changeset`] also reads and decodes the
    /// changeset's object.
    pub fn changeset_meta(
        &self,
        id: ChangesetId,
    ) -> Result<Option<swarmops_store::ChangesetMeta>, RepoError> {
        Ok(self.meta.read()?.changeset(id)?)
    }

    /// Makes the metadata's projections durable now ([`MetaStore::flush`]): takes the writer
    /// lock as a commit does ([`StoreError::Busy`] if the wait runs out), repairs and replays what
    /// earlier writers left, and, if a journal frame lies past the checkpoint, syncs the index
    /// files and moves the checkpoint to the end of the journal. It records no changeset and
    /// moves no ref, so a failure leaves what is committed as it was. A commit moves the
    /// checkpoint only every 32 frames, so a bulk writer flushes once it is done: until the
    /// checkpoint passes its frames, every snapshot reads them again.
    pub fn flush_metadata(&self) -> Result<(), RepoError> {
        Ok(self.meta.flush()?)
    }

    /// Ref `name`, in a snapshot of its own ([`RepoRead::get_ref`]).
    pub fn get_ref(&self, name: &str) -> Result<Option<RefState>, RepoError> {
        self.snapshot()?.get_ref(name)
    }

    /// Every ref, by name, in a snapshot of its own ([`RepoRead::list_refs`]).
    pub fn list_refs(&self) -> Result<Vec<(String, RefState)>, RepoError> {
        self.snapshot()?.list_refs()
    }

    /// Moves ref `name` from `expected` to `new` (`None` deletes it; `expected` `None` creates
    /// it) for `actor`, in a write transaction of its own, if the ref names `expected` now;
    /// otherwise [`CasResult::Mismatch`] with what it names, and nothing changes. The move is
    /// recorded in the journal (the reflog). [`RepoError::RefName`] for a bad name,
    /// [`RepoError::NotFound`] for a `new` that is not a changeset in the store.
    ///
    /// A move to the target it expects (`expected == new`) changes nothing and is never
    /// recorded: it is compared under the lock, after the write's tail check, as any move is
    /// ([`CasResult::Applied`] with `old` equal to `new` where the ref names that target,
    /// [`CasResult::Mismatch`] where it names another), and the write is dropped uncommitted, so
    /// no frame is written and the ref's version stays. That holds for a deletion of a ref it
    /// expects absent (`None` to `None`) too.
    ///
    /// A move whose journal frame is durable is applied even if a step after that failed
    /// ([`StoreError::PostCommit`], its listing in `journal.off` among them): the outcome
    /// carries a warning, and the next write to the store repairs what is missing. A journal
    /// append that fails ([`StoreError::NotCommitted`]) is returned as it is: the frame was cut
    /// back off the journal, the move is not applied, and the same move made again applies it.
    pub fn cas_ref(
        &self,
        name: &str,
        expected: Option<ChangesetId>,
        new: Option<ChangesetId>,
        actor: &str,
    ) -> Result<CasOutcome, RepoError> {
        let mut write = self.meta.write()?;
        let recovered = write.recovered();
        let result = write
            .cas_ref(name, expected, new, actor)
            .map_err(ref_error)?;
        let mut warnings = Vec::new();
        // A mismatch queued nothing, and a move to the target it expects queued one that changes
        // nothing: either way dropping the write releases the lock and commits nothing.
        if let CasResult::Applied { .. } = result
            && expected != new
        {
            match write.commit() {
                Ok(_) => {}
                Err(StoreError::PostCommit { frame_seq, source }) => {
                    warnings.push(post_commit_warning("the ref move", frame_seq, &source));
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(CasOutcome {
            result,
            warnings,
            recovered,
        })
    }

    /// What external id `key` of `system` names in the external id index (storage spec §7): a
    /// changeset, and whether it holds the external object faithfully; `None` if the index has
    /// no entry for it. [`StoreError::ExternalIdCorrupt`] for a file there that is no entry;
    /// [`StoreError::BadCall`] for a system name or a key the index does not take
    /// ([`ExternalIds`]).
    pub fn external_id(
        &self,
        system: &str,
        key: &[u8],
    ) -> Result<Option<ExternalEntry>, RepoError> {
        Ok(self.external_ids.get(system, key)?)
    }

    /// Records `entries` of `system` in the external id index ([`ExternalIds::put_batch`]): each
    /// not there yet is written by the object rules, every name it relies on is durable when it
    /// returns, and one there with other bytes is left as it is and named in
    /// [`ExternalPut::conflicts`]. Every changeset an entry names must be committed already, its
    /// journal frame durable (storage spec §2 rule 4, W8): record the entries after the commit
    /// that records their changesets returns, never before.
    pub fn record_external_ids(
        &self,
        system: &str,
        entries: &[(Vec<u8>, ExternalEntry)],
    ) -> Result<ExternalPut, RepoError> {
        Ok(self.external_ids.put_batch(system, entries)?)
    }

    /// Every entry of `system` in the external id index, by key ([`ExternalIds::list`]): each
    /// key with its entry, or with [`StoreError::ExternalIdCorrupt`] for a file that is no entry.
    /// For a check of the entries beside [`Repo::verify`]'s own, such as the git side's deep one
    /// of the entries its export writes.
    pub fn list_external_ids(
        &self,
        system: &str,
    ) -> Result<Vec<swarmops_store::ListedEntry>, RepoError> {
        Ok(self.external_ids.list(system)?)
    }

    /// The derived store, `meta/derived/`, where [`Repo::derived`] keeps its values: for a reader
    /// outside a derivation that memoizes values of its own there per object (the git export's
    /// tree and blob oids, M8), under names no derivation registers, as a derivation does through
    /// [`crate::DeriveCtx::derived_store`].
    pub fn derived_store(&self) -> &DerivedStore {
        &self.derived
    }

    /// Changeset `id`'s object, decoded.
    pub(crate) fn changeset_object(&self, id: ChangesetId) -> Result<Changeset, RepoError> {
        let payload = object(&self.objects, id.0, ObjectKind::Changeset)?;
        Ok(Changeset::decode(&payload)?)
    }
}

/// The warning for a transaction (`what`) committed at journal frame `frame_seq` whose step
/// after the commit point failed with `source`.
pub(crate) fn post_commit_warning(what: &str, frame_seq: u64, source: &StoreError) -> String {
    format!(
        "{what} is committed (journal frame {frame_seq}), but a step after the commit point \
         failed: {source}; the next write to the store repairs it"
    )
}

/// The payload of `kind` object `id`, verified against its id. [`RepoError::MissingObject`] if
/// `objects` lacks it, or holds an object of another kind under that id.
pub(crate) fn object<O: ObjectStore + ?Sized>(
    objects: &O,
    id: ObjectId,
    kind: ObjectKind,
) -> Result<Vec<u8>, RepoError> {
    match objects.get(id)? {
        Some((found, payload)) if found == kind => Ok(payload),
        _ => Err(RepoError::MissingObject { id, kind }),
    }
}

/// Tree `id`, decoded.
pub(crate) fn tree<O: ObjectStore + ?Sized>(objects: &O, id: TreeId) -> Result<Tree, RepoError> {
    Ok(Tree::decode(&object(objects, id.0, ObjectKind::Tree)?)?)
}

/// A store error of a ref operation, its ref-specific variants lifted to the repo's own.
pub(crate) fn ref_error(e: StoreError) -> RepoError {
    match e {
        StoreError::RefName(name) => RepoError::RefName(name),
        StoreError::UnknownRefTarget(id) => RepoError::NotFound(id.to_string()),
        other => other.into(),
    }
}

/// The exclusive lock on `REPO` a create holds from its link of `REPO` to its end, so that two
/// creates of one store run one after the other: std `File::lock`, released when the returned
/// file closes (a crash releases it too). `REPO` is never replaced, so both lock one file.
fn lock_repo(layout: &Layout, fs: &StoreFs) -> Result<File, RepoError> {
    let path = layout.repo_file();
    let file = File::open(&path).map_err(|e| fs.locate(FileOp::Open, &path, e))?;
    file.lock().map_err(|e| fs.locate(FileOp::Lock, &path, e))?;
    Ok(file)
}

/// What a create does when the sync of the store root that makes `FORMAT`'s name durable fails
/// with `e`: `FORMAT` makes the store complete for every reader at once, but Linux reports a
/// failed writeback once, so no later sync could tell whether its name reached the disk. So it
/// is removed again, the removal synced, and the create fails: the store is not complete, and
/// the next create makes it so (storage spec §2). If that fails too, the process aborts
/// ([`std::process::abort`]), saying why on stderr, as a crash between the link and the sync
/// would leave it: the next `open` finds a store or none, and `create` finishes it.
fn take_format_back(fs: &StoreFs, root: &Utf8Path, e: &IoFailure) {
    let format = Layout::new(root).format_file();
    let undone = fs
        .remove(format.as_std_path())
        .and_then(|()| fs.fsync_dir(root.as_std_path()));
    if let Err(undo) = undone {
        // A write that fails (a closed pipe, a full log) must not panic: the abort is the point.
        let _ = writeln!(
            io::stderr(),
            "swarmops: fatal: syncing {root} after linking {format} failed ({e}), and \
             {format} could not be taken back ({undo}): aborting, as a crash there would"
        );
        std::process::abort();
    }
}

/// The filesystem a store opens on: the real one; or, when a test hook asks for it
/// ([`HookedFs::from_env`], in a build with the `test-hooks` feature only:
/// `SWARMOPS_TEST_CRASH_AT=<k>` to abort at call k, `SWARMOPS_TEST_FS_CALLS=<file>` to write the
/// call count when the repository is dropped, `SWARMOPS_TEST_FAIL_AT=<k>[:<mode>[:<errno>]]` to
/// fail call k, `SWARMOPS_TEST_ORDER_CHECK=1` to check the order of the writes), one filesystem
/// for both stores, so that they count their calls together. [`Threads::One`] with a hook that
/// counts calls (all but the order check): objects are then written, and the directories and
/// the index's files synced, one at a time, so that call k is the same call in every run.
fn default_fs() -> (Arc<dyn FsOps>, Threads) {
    match HookedFs::from_env() {
        Some(hooked) => {
            let counts_calls = hooked.counts_calls();
            let threads = if counts_calls {
                Threads::One
            } else {
                Threads::Pool
            };
            (Arc::from(hooked.into_fs()), threads)
        }
        None => (Arc::new(RealFs::new()), Threads::Pool),
    }
}

/// The threads the stores write and sync with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Threads {
    /// The stores' own parallelism.
    Pool,
    /// One at a time, so that a counted filesystem call is the same call in every run.
    One,
}

/// `store` as a canonical path on a native Linux filesystem, created (through `fs`) if
/// missing, each new directory's entry made durable. Where the path leads is worked out before
/// anything is created ([`resolve`]): a path that leads under `/mnt/` is refused then, and,
/// should anything have swapped in a symlink meanwhile, again once the directory exists.
///
/// When the store directory exists already, perhaps made by a create that crashed, only its
/// parent is synced: the directories above it that the crashed create may have made are not
/// known any more. That is enough on the filesystems a store lives on (ext4, XFS): their
/// journal is one ordered log for the whole filesystem, so the parent's sync commits every
/// metadata change made before it, those directories' entries among them. Not on btrfs, whose
/// fsync logs what it syncs alone: there a power loss after a resumed create may take back the
/// directories the crashed one made above the store, and the store with them; creating it
/// again mends that, as nothing but a fresh store is at stake.
///
/// A failure names the store directory as `.` and the directories above it as no file of the
/// store; one of working out where the path leads is [`FileOp::Resolve`], with no file.
fn native_root(store: &Utf8Path, fs: &Arc<dyn FsOps>) -> Result<Utf8PathBuf, RepoError> {
    let (target, missing) = resolve(&absolute(store)?)?;
    if !swarmops_store::is_native_linux_fs(&target) {
        return Err(StoreError::UnsupportedFs(target).into());
    }
    let fs = StoreFs::new(Arc::clone(fs), &target);
    fs.create_dir_all(target.as_std_path())?;
    // A directory's entry lives in its parent: sync the parent of each directory just made,
    // outermost first, or of the root alone if it was there (a crashed create may have made it).
    let made = if missing.is_empty() {
        std::slice::from_ref(&target)
    } else {
        &missing[..]
    };
    for dir in made {
        if let Some(parent) = dir.parent() {
            fs.fsync_dir(parent.as_std_path())?;
        }
    }
    let canonical = target.canonicalize_utf8().map_err(resolving)?;
    if !swarmops_store::is_native_linux_fs(&canonical) {
        return Err(StoreError::UnsupportedFs(canonical).into());
    }
    Ok(canonical)
}

/// `path` made absolute, against the working directory if it is relative; a failure is one of
/// working out where the path leads ([`FileOp::Resolve`], no file).
fn absolute(path: &Utf8Path) -> Result<Utf8PathBuf, IoFailure> {
    camino::absolute_utf8(path).map_err(resolving)
}

/// The failure `e` of working out where a store's path leads: on no file of the store.
fn resolving(e: io::Error) -> IoFailure {
    IoFailure::new(FileOp::Resolve, None, e)
}

/// Where absolute path `path` leads once its missing directories are made, resolved as the
/// kernel will resolve it then, with the directories in it that do not exist yet, outermost
/// first. One component at a time: `..` takes back the one before it (resolved already, so no
/// symlink); a symlink gives way to its target, whether that exists or not (a directory made
/// through it lands where it points); a name that does not exist is kept as it is. A failure is
/// [`FileOp::Resolve`]'s, with no file; a symlink loop names `path` in its text: the store's path
/// made absolute (against the working directory, if the caller gave it relative).
fn resolve(path: &Utf8Path) -> Result<(Utf8PathBuf, Vec<Utf8PathBuf>), IoFailure> {
    // Linux's MAXSYMLINKS: more symlinks than that on one path is a loop.
    const MAX_LINKS: usize = 40;
    let mut target = Utf8PathBuf::from("/");
    let mut pending = components(path);
    let mut links = 0;
    while let Some(name) = pending.pop() {
        if name == ".." {
            target.pop();
            continue;
        }
        let next = target.join(&name);
        match std::fs::symlink_metadata(&next) {
            Ok(meta) if meta.file_type().is_symlink() => {
                links += 1;
                if links > MAX_LINKS {
                    return Err(resolving(io::Error::other(format!(
                        "{path}: too many levels of symbolic links"
                    ))));
                }
                let link = Utf8PathBuf::try_from(std::fs::read_link(&next).map_err(resolving)?)
                    .map_err(|e| resolving(io::Error::new(io::ErrorKind::InvalidData, e)))?;
                if link.is_absolute() {
                    target = Utf8PathBuf::from("/");
                }
                pending.extend(components(&link));
            }
            Ok(_) => target = next,
            Err(e) if e.kind() == io::ErrorKind::NotFound => target = next,
            Err(e) => return Err(resolving(e)),
        }
    }
    // No symlink is left in it, so the directories it lacks are its last ones.
    let mut missing: Vec<Utf8PathBuf> = target
        .ancestors()
        .take_while(|dir| std::fs::symlink_metadata(dir).is_err())
        .map(Utf8Path::to_path_buf)
        .collect();
    missing.reverse();
    Ok((target, missing))
}

/// The names and `..`s of `path`, the last one first: a stack whose top is the next.
fn components(path: &Utf8Path) -> Vec<String> {
    path.components()
        .rev()
        .filter_map(|component| match component {
            Utf8Component::Normal(name) => Some(name.to_owned()),
            Utf8Component::ParentDir => Some("..".to_owned()),
            Utf8Component::RootDir | Utf8Component::CurDir | Utf8Component::Prefix(_) => None,
        })
        .collect()
}

/// The identity `REPO` holds: a line with the id, then the name to the end of the file.
fn read_identity(layout: &Layout, fs: &StoreFs) -> Result<Identity, RepoError> {
    let path = layout.repo_file();
    let bad = |what: &str| RepoError::from(StoreError::Format(format!("{path} {what}")));
    // A REPO longer than an id line and the longest name create writes holds no identity this
    // build reads, however long it is: measured before it is read (through `std::fs`, no
    // `FsOps` call), so that a crafted file of huge length is never read into memory. A missing
    // one, and one that is no regular file (whose size says nothing of its content), reach the
    // read, which refuses the latter as such.
    if let Ok(meta) = std::fs::metadata(path.as_std_path())
        && meta.is_file()
        && meta.len() > MAX_REPO_LEN
    {
        return Err(bad(&format!(
            "is {} bytes long, more than a repository id and a name of at most {MAX_NAME_LEN} \
             bytes",
            meta.len()
        )));
    }
    let bytes = fs
        .read(path.as_std_path())?
        .ok_or_else(|| bad("is missing"))?;
    let text = String::from_utf8(bytes).map_err(|_| bad("is not UTF-8"))?;
    let (repo_id, name) = text
        .split_once('\n')
        .filter(|(repo_id, _)| is_repo_id(repo_id))
        .ok_or_else(|| bad("does not start with a repository id"))?;
    Ok(Identity {
        repo_id: repo_id.to_owned(),
        name: name.to_owned(),
    })
}

fn is_repo_id(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// A new repository id: 128 bits from the standard library's randomly keyed hasher (its keys
/// come from the operating system's randomness), fed the time and the process id, as 32
/// lowercase hex digits.
fn new_repo_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    let pid = std::process::id();
    let high = RandomState::new().hash_one((nanos, pid, 0_u8));
    let low = RandomState::new().hash_one((nanos, pid, 1_u8));
    format!("{high:016x}{low:016x}")
}
