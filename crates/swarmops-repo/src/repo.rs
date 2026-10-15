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
//! at random by create, then its name, verbatim (any UTF-8, line breaks included) to the end
//! of the file, with no line break added:
//!
//! ```text
//! 3f9c2d0a8e1b4c7f6a5d2e9b0c8f1a37
//! acme
//! ```
//!
//! Create writes it in step 5 below, after `meta/` and before `FORMAT`, replacing it
//! atomically (a temp file in the store root, synced, renamed over it, the root synced), so a
//! store with `FORMAT` always has it; a create that resumes after a crash writes it again,
//! with a new id. Open requires it alongside `FORMAT`: after checking `FORMAT`, it reads
//! `REPO`, and refuses a store whose `REPO` is missing, not UTF-8, or does not start with an
//! id line ([`StoreError::Format`]).
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
//! 4. It creates `meta/` with `MetaStore::create`, which is crash-atomic, or opens the whole one
//!    a crashed create left behind.
//! 5. It writes `REPO` atomically, with a new repository id (see above).
//! 6. Last, it links `FORMAT` into place without replacing anything, from a temp file in
//!    `objects/tmp/` that is synced first (so `FORMAT` is never seen half written), and syncs
//!    the store root.
//!
//! A crash before the link leaves no `FORMAT`: open refuses the directory, and the next create
//! picks up where the crashed one stopped. Two creates of one store at once are not supported.

use std::hash::{BuildHasher, RandomState};
use std::io;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use camino::{Utf8Component, Utf8Path, Utf8PathBuf};
use swarmops_core::{Changeset, ChangesetId, ObjectId, ObjectKind, Tree, TreeId};
use swarmops_store::{
    CasResult, CrashAtFs, FsObjectStore, FsOps, Layout, LinkOutcome, MetaStore, ObjectStore,
    RealFs, RefState, StoreError, is_native_linux_fs,
};

use crate::error::RepoError;

/// What a store's `FORMAT` file holds: the store layout this build writes and reads.
const FORMAT: &[u8] = b"swarmops-store 1\n";

/// The version number `FORMAT` names.
const STORE_VERSION: u32 = 1;

/// A repository: objects in `O`, metadata in the native [`MetaStore`].
pub struct Repo<O: ObjectStore> {
    pub(crate) objects: O,
    pub(crate) meta: MetaStore,
    /// The store's paths: [`Repo::verify`] reads the journal and the index files itself.
    pub(crate) layout: Layout,
    /// What both stores write through; verify's own handles on the metadata use it too.
    pub(crate) fs: Arc<dyn FsOps>,
    identity: Identity,
}

/// The repository on the local filesystem: loose object files and the native metadata store.
pub type LocalRepo = Repo<FsObjectStore>;

/// What [`LocalRepo::info`] reports about a repository.
#[derive(Debug, Clone, PartialEq, Eq)]
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

/// What [`Repo::cas_ref`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CasOutcome {
    /// Whether the ref moved, as the store decided.
    pub result: CasResult,
    /// Steps after the commit point that failed, one line each: the move is committed all
    /// the same.
    pub warnings: Vec<String>,
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
    /// [`StoreError::UnsupportedFs`] for a path under `/mnt/`. Any name is kept verbatim.
    ///
    /// With `SWARMOPS_TEST_CRASH_AT=<k>` set (crash tests), the object store and the metadata
    /// store share one [`CrashAtFs`] whose k-th call aborts the process, and objects are
    /// written (and the directories and the index's files synced) one at a time, so that the
    /// k-th call is the same call in every run. With `SWARMOPS_TEST_FS_CALLS=<file>` set, the
    /// same holds, and that [`CrashAtFs`] writes how many calls both stores made to `<file>`
    /// when the repository is dropped: a dry run's count.
    pub fn create(store: &Utf8Path, name: &str) -> Result<LocalRepo, RepoError> {
        let (fs, one_thread) = default_fs();
        Self::create_on(store, name, fs, one_thread)
    }

    /// [`LocalRepo::create`] with every filesystem change going through `fs` (fault injection in
    /// tests); objects are written in parallel as usual.
    pub fn create_with_fs(
        store: &Utf8Path,
        name: &str,
        fs: Arc<dyn FsOps>,
    ) -> Result<LocalRepo, RepoError> {
        Self::create_on(store, name, fs, false)
    }

    /// Opens the store at `store`. Checks `FORMAT` first, then reads `REPO`:
    /// [`StoreError::Format`] if `FORMAT` is missing (no store there) or names another layout
    /// version, if `REPO` is missing or malformed, or if `meta/` is missing. Honors
    /// `SWARMOPS_TEST_CRASH_AT` and `SWARMOPS_TEST_FS_CALLS` as [`LocalRepo::create`] does.
    pub fn open(store: &Utf8Path) -> Result<LocalRepo, RepoError> {
        let (fs, one_thread) = default_fs();
        Self::open_on(store, fs, one_thread)
    }

    /// [`LocalRepo::open`] with every filesystem access of both stores going through `fs`.
    pub fn open_with_fs(store: &Utf8Path, fs: Arc<dyn FsOps>) -> Result<LocalRepo, RepoError> {
        Self::open_on(store, fs, false)
    }

    /// The repository's identity and counts, from a snapshot of its metadata.
    pub fn info(&self) -> Result<RepoInfo, RepoError> {
        let read = self.meta.read()?;
        Ok(RepoInfo {
            repo_id: self.identity.repo_id.clone(),
            name: self.identity.name.clone(),
            store_version: STORE_VERSION,
            changesets: read.count_changesets()?,
            refs: u64::try_from(read.list_refs()?.len()).expect("usize fits in u64"),
            main: read.get_ref("main")?.map(|state| state.target),
        })
    }

    fn create_on(
        store: &Utf8Path,
        name: &str,
        fs: Arc<dyn FsOps>,
        one_thread: bool,
    ) -> Result<LocalRepo, RepoError> {
        let root = native_root(store, &*fs)?;
        let layout = Layout::new(root.clone());
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
        fs.fsync_dir(root.as_std_path())?;
        // MetaStore::create renames a whole meta/ into place, so one that exists is whole.
        let meta = if fs.exists(layout.meta_dir().as_std_path())? {
            MetaStore::open_with_fs(&root, Arc::clone(&fs))?
        } else {
            MetaStore::create_with_fs(&root, Arc::clone(&fs))?
        };
        let identity = Identity {
            repo_id: new_repo_id(),
            name: name.to_owned(),
        };
        let repo_file = format!("{}\n{}", identity.repo_id, identity.name);
        fs.atomic_replace(layout.repo_file().as_std_path(), repo_file.as_bytes())?;
        let temp = fs.write_durable_temp(layout.objects_tmp().as_std_path(), FORMAT)?;
        let format_file = layout.format_file();
        // Another create that finished first.
        if fs.link_noclobber(&temp, format_file.as_std_path())? == LinkOutcome::AlreadyExists {
            return Err(RepoError::Exists);
        }
        fs.fsync_dir(root.as_std_path())?;
        Ok(Self::assemble(layout, fs, meta, identity, one_thread))
    }

    fn open_on(
        store: &Utf8Path,
        fs: Arc<dyn FsOps>,
        one_thread: bool,
    ) -> Result<LocalRepo, RepoError> {
        let root = camino::absolute_utf8(store)?;
        let layout = Layout::new(root.clone());
        match fs.read(layout.format_file().as_std_path())? {
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
        let identity = read_identity(&layout, &*fs)?;
        let meta = MetaStore::open_with_fs(&root, Arc::clone(&fs))?;
        Ok(Self::assemble(layout, fs, meta, identity, one_thread))
    }

    fn assemble(
        layout: Layout,
        fs: Arc<dyn FsOps>,
        mut meta: MetaStore,
        identity: Identity,
        one_thread: bool,
    ) -> LocalRepo {
        let mut objects = FsObjectStore::open(layout.clone(), Box::new(Arc::clone(&fs)));
        if one_thread {
            objects.set_parallelism(1);
            meta.set_parallelism(1);
        }
        Repo {
            objects,
            meta,
            layout,
            fs,
            identity,
        }
    }
}

impl<O: ObjectStore> Repo<O> {
    /// Ref `name`, as a snapshot of the metadata shows it; `None` if it does not exist.
    /// [`RepoError::RefName`] for a name that is not `main` or `release/<x>`.
    pub fn get_ref(&self, name: &str) -> Result<Option<RefState>, RepoError> {
        self.meta.read()?.get_ref(name).map_err(ref_error)
    }

    /// Every ref, by name.
    pub fn list_refs(&self) -> Result<Vec<(String, RefState)>, RepoError> {
        Ok(self.meta.read()?.list_refs()?)
    }

    /// Moves ref `name` from `expected` to `new` (`None` deletes it; `expected` `None` creates
    /// it) for `actor`, in a write transaction of its own, if the ref names `expected` now;
    /// otherwise [`CasResult::Mismatch`] with what it names, and nothing changes. The move is
    /// recorded in the journal (the reflog). [`RepoError::RefName`] for a bad name,
    /// [`RepoError::NotFound`] for a `new` that is not a changeset in the store.
    ///
    /// A move whose journal frame is durable is applied even if a step after that failed
    /// ([`StoreError::PostCommit`]): the outcome carries a warning, and the next write to the
    /// store repairs the ref file.
    pub fn cas_ref(
        &self,
        name: &str,
        expected: Option<ChangesetId>,
        new: Option<ChangesetId>,
        actor: &str,
    ) -> Result<CasOutcome, RepoError> {
        let mut write = self.meta.write()?;
        let result = write
            .cas_ref(name, expected, new, actor)
            .map_err(ref_error)?;
        let mut warnings = Vec::new();
        // A mismatch queued nothing: dropping the write releases the lock and commits nothing.
        if let CasResult::Applied { .. } = result {
            match write.commit() {
                Ok(_) => {}
                Err(StoreError::PostCommit { frame_seq, source }) => {
                    warnings.push(post_commit_warning("the ref move", frame_seq, &source));
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(CasOutcome { result, warnings })
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
pub(crate) fn object<O: ObjectStore>(
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
pub(crate) fn tree<O: ObjectStore>(objects: &O, id: TreeId) -> Result<Tree, RepoError> {
    Ok(Tree::decode(&object(objects, id.0, ObjectKind::Tree)?)?)
}

/// A store error of a ref operation, its ref-specific variants lifted to the repo's own.
fn ref_error(e: StoreError) -> RepoError {
    match e {
        StoreError::RefName(name) => RepoError::RefName(name),
        StoreError::UnknownRefTarget(id) => RepoError::NotFound(id.to_string()),
        other => other.into(),
    }
}

/// The filesystem a store opens on: the real one; or, when a crash test's hook asks for it
/// ([`CrashAtFs::from_env`]: `SWARMOPS_TEST_CRASH_AT=<k>` to abort at call k,
/// `SWARMOPS_TEST_FS_CALLS=<file>` to write the call count when the repository is dropped), one
/// [`CrashAtFs`] for both stores, so that they count their calls together. `true` with the
/// latter: objects are then written, and the directories and the index's files synced, one at
/// a time, so that call k is the same call in every run.
fn default_fs() -> (Arc<dyn FsOps>, bool) {
    match CrashAtFs::from_env() {
        Some(fs) => (Arc::new(fs), true),
        None => (Arc::new(RealFs), false),
    }
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
fn native_root(store: &Utf8Path, fs: &dyn FsOps) -> Result<Utf8PathBuf, RepoError> {
    let (target, missing) = resolve(&camino::absolute_utf8(store)?)?;
    if !is_native_linux_fs(&target) {
        return Err(StoreError::UnsupportedFs(target).into());
    }
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
    let canonical = target.canonicalize_utf8()?;
    if !is_native_linux_fs(&canonical) {
        return Err(StoreError::UnsupportedFs(canonical).into());
    }
    Ok(canonical)
}

/// Where absolute path `path` leads once its missing directories are made, resolved as the
/// kernel will resolve it then, with the directories in it that do not exist yet, outermost
/// first. One component at a time: `..` takes back the one before it (resolved already, so no
/// symlink); a symlink gives way to its target, whether that exists or not (a directory made
/// through it lands where it points); a name that does not exist is kept as it is.
fn resolve(path: &Utf8Path) -> io::Result<(Utf8PathBuf, Vec<Utf8PathBuf>)> {
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
                    return Err(io::Error::other(format!(
                        "{path}: too many levels of symbolic links"
                    )));
                }
                let link = Utf8PathBuf::try_from(std::fs::read_link(&next)?)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                if link.is_absolute() {
                    target = Utf8PathBuf::from("/");
                }
                pending.extend(components(&link));
            }
            Ok(_) => target = next,
            Err(e) if e.kind() == io::ErrorKind::NotFound => target = next,
            Err(e) => return Err(e),
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
fn read_identity(layout: &Layout, fs: &dyn FsOps) -> Result<Identity, RepoError> {
    let path = layout.repo_file();
    let bad = |what: &str| RepoError::from(StoreError::Format(format!("{path} {what}")));
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
