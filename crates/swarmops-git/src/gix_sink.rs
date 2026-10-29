//! [`GixSink`], the [`GitSink`] of a repository on disk, written through gix: with
//! [`gix_source`], the crate's contact with gix (umbrella decision 3).
//!
//! gix writes the loose objects. This module makes them durable, which gix does not (its loose
//! writer renames a temporary file into place and syncs nothing), and moves a branch by git's own
//! lock protocol, written here so that it syncs: the lock file created exclusively, the new value
//! written to it and synced, the lock renamed onto the ref, and the ref's directory synced.

use std::collections::BTreeSet;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
#[cfg(feature = "test-hooks")]
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use gix::objs::{Exists as _, Find as _};
use gix::prelude::Write as _;
use rayon::prelude::*;

use crate::{BranchUpdate, GitOid, GitSink, ObjectFormat, RawKind, SinkError, SourceError, Stored};
use crate::{gix_source, oid};

/// How long the export tries a branch's lock before the branch is [`SinkError::RefLocked`]:
/// git's `core.filesRefLockTimeout` default, so that two exports racing to one branch take turns.
/// Counted as git counts it, by the pauses between attempts rather than by the clock, so that a
/// loaded host makes as many attempts as a quiet one.
const LOCK_TIMEOUT: Duration = Duration::from_millis(100);

/// Where a branch's ref is, by its name: `refs/heads/<name>`.
const HEADS: &str = "refs/heads/";

/// Where a tag's ref is, by its name: `refs/tags/<name>`.
const TAGS: &str = "refs/tags/";

/// The paths a [`GixSink`] opened with [`GixSink::open_recording_syncs`] fsynced, relative to the
/// git directory, in the order the syncs ended.
#[cfg(feature = "test-hooks")]
pub type SyncedPaths = Arc<Mutex<Vec<PathBuf>>>;

/// A [`GitSink`] over a git repository on disk, through gix (`=0.90.0`, its `sha1` feature alone).
///
/// It opens the repository as [`GixSource::open`](crate::GixSource::open) does, refusing what that
/// refuses. Objects are written by gix's loose writer: `objects/<2 hex>/<38 hex>`, zlib, as git
/// writes them. [`stored`] and [`read`] go by what the repository holds on disk: a loose file is
/// read (and checked against its id) before a pack, and gix's answer for the empty tree, which it
/// gives whether or not the tree is stored, is not taken: git's own object writer goes by the files
/// too, and writes the tree where it is not. [`sync`] fsyncs each file and directory on rayon's
/// global pool. A branch moves by git's lock protocol, and a tag as a branch does under
/// `refs/tags/`: `refs/heads/<name>.lock` created exclusively, tried again for up to 100 ms
/// (git's `core.filesRefLockTimeout`), the ref read and compared under it, the new value written
/// to the lock and synced, the lock renamed onto the ref, and the ref's directory synced, with
/// each directory the move made and its parent. A process that dies between the lock and the
/// rename leaves the lock, which, as with git, stays until it is removed; a failure to sync those
/// directories after the rename leaves the branch moved, perhaps not durably.
///
/// [`stored`]: GitSink::stored
/// [`read`]: GitSink::read
/// [`sync`]: GitSink::sync
#[non_exhaustive]
pub struct GixSink {
    /// The repository, as gix opened it.
    repo: gix::Repository,
    /// Its files, apart from gix: what the threads of a sync share.
    files: Files,
    /// Its loose objects alone, read before its packs.
    loose: gix::odb::loose::Store,
    /// The fan-out directories this sink made, by their byte, whose names in `objects/` are not
    /// durable until a sync of an object in them syncs `objects/`.
    made: BTreeSet<u8>,
    /// What the lock's retry loop runs between two attempts.
    #[cfg(feature = "test-hooks")]
    lock_retry: Option<Box<dyn FnMut() + Send>>,
}

/// The files of the repository a [`GixSink`] syncs and moves branches in, apart from gix.
struct Files {
    /// The repository's git directory, the common one of its work trees: where its objects and
    /// branches are, and what the paths of errors and records are relative to.
    git_dir: PathBuf,
    /// Its objects directory, where gix writes loose objects.
    objects: PathBuf,
    /// Each path synced, when the sink records them.
    #[cfg(feature = "test-hooks")]
    synced: Option<SyncedPaths>,
}

impl fmt::Debug for GixSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GixSink")
            .field("git_dir", &self.files.git_dir)
            .finish_non_exhaustive()
    }
}

impl GixSink {
    /// Opens the repository at `path`, a work tree or a git directory (a bare `repo.git`), as
    /// [`GixSource::open`](crate::GixSource::open) does: [`SinkError::NotARepo`] when gix opens no
    /// repository there, [`SinkError::Unsupported`] for one the import would not read.
    pub fn open(path: &Path) -> Result<GixSink, SinkError> {
        let repo = gix_source::open_repository(path).map_err(opening)?;
        let git_dir = repo.common_dir().to_owned();
        let objects = repo.objects.store_ref().path().to_owned();
        let loose = gix::odb::loose::Store::at(objects.clone(), gix::hash::Kind::Sha1);
        Ok(GixSink {
            repo,
            files: Files {
                git_dir,
                objects,
                #[cfg(feature = "test-hooks")]
                synced: None,
            },
            loose,
            made: BTreeSet::new(),
            #[cfg(feature = "test-hooks")]
            lock_retry: None,
        })
    }

    /// [`GixSink::open`], with a record of each path the sink fsyncs (and still fsyncs), relative
    /// to the git directory, in the order the syncs end.
    #[cfg(feature = "test-hooks")]
    pub fn open_recording_syncs(path: &Path) -> Result<(GixSink, SyncedPaths), SinkError> {
        let mut sink = GixSink::open(path)?;
        let synced = Arc::new(Mutex::new(Vec::new()));
        sink.files.synced = Some(Arc::clone(&synced));
        Ok((sink, synced))
    }

    /// Runs `hook` in the ref lock's retry loop, between two attempts to take a branch's lock,
    /// so that a test lets the lock go at a known point rather than by a timer.
    #[cfg(feature = "test-hooks")]
    pub fn on_lock_retry(&mut self, hook: impl FnMut() + Send + 'static) {
        self.lock_retry = Some(Box::new(hook));
    }

    /// Whether the loose file of `oid` is there.
    fn is_loose(&self, oid: GitOid) -> Result<bool, SinkError> {
        let path = self.files.loose(oid);
        match fs::metadata(&path) {
            Ok(metadata) => Ok(metadata.is_file()),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                Ok(false)
            }
            Err(error) => Err(self.files.failed_read(&path, &error)),
        }
    }

    /// Whether the repository holds `oid` in a pack or an alternate (or as a loose file): what
    /// gix finds, without its answer for the empty tree.
    fn has(&self, oid: GitOid) -> bool {
        self.repo.objects.exists(&gix_source::to_gix(oid))
    }

    /// The loose file of `oid`, read and checked against its id; `None` when there is no such
    /// file, or no regular one: the open of a FIFO would block, and gix's own lookups pass such a
    /// file over too.
    fn read_loose(&self, oid: GitOid) -> Result<Option<(RawKind, Vec<u8>)>, SinkError> {
        if !self.is_loose(oid)? {
            return Ok(None);
        }
        let mut buffer = Vec::new();
        match self.loose.try_find(&gix_source::to_gix(oid), &mut buffer) {
            Ok(Some(data)) => {
                checked(oid, gix_source::raw_kind(data.kind), data.data.to_vec()).map(Some)
            }
            Ok(None) => Ok(None),
            Err(error) => Err(damaged(
                oid,
                &error,
                self.files.named(&self.files.loose(oid)),
            )),
        }
    }

    /// What the ref `full` names, as git reads it (its loose file, else `packed-refs`): `None`
    /// when there is no such ref, [`SinkError::RefConflict`] for a symbolic one.
    fn read_ref(&self, full: &str) -> Result<Option<GitOid>, SinkError> {
        let reference = match self.repo.try_find_reference(full) {
            Ok(Some(reference)) if reference.name().as_bstr() == full.as_bytes() => reference,
            // No such ref; the lookup goes on to names the full name is no ref of
            // (`refs/tags/<full>` and the like), which are not this one.
            Ok(_) => return Ok(None),
            Err(error) => {
                return Err(SinkError::Read {
                    what: full.to_owned(),
                    reason: gix_source::reason(&error),
                });
            }
        };
        match reference.target() {
            gix::refs::TargetRef::Object(id) => Ok(Some(gix_source::from_gix(&id.to_owned()))),
            gix::refs::TargetRef::Symbolic(_) => Err(SinkError::RefConflict {
                name: full.to_owned(),
            }),
        }
    }

    /// Refuses the branch or tag `full` (`refs/heads/…`, `refs/tags/…`) when another ref is in
    /// the way: one whose name is a directory of `full`'s below its namespace (its loose file,
    /// `packed-refs`, or a file that is no ref), or one under `full/`. git refuses both, and the
    /// filesystem alone would not see a packed one.
    fn refuse_conflicts(&self, full: &str) -> Result<(), SinkError> {
        let namespace = full.match_indices('/').nth(1);
        let mut at = namespace.map_or(full.len(), |(slash, _)| slash + 1);
        while let Some(slash) = full[at..].find('/') {
            let directory = &full[..at + slash];
            if !matches!(self.read_ref(directory), Ok(None)) {
                return Err(SinkError::RefConflict {
                    name: directory.to_owned(),
                });
            }
            at += slash + 1;
        }
        let under = format!("{full}/");
        let listing = |error: gix::Error| SinkError::Read {
            what: under.clone(),
            reason: gix_source::reason(&error),
        };
        let platform = self.repo.references().map_err(listing)?;
        let mut refs = platform.prefixed(under.as_str()).map_err(listing)?;
        let found = refs.find_map(|found| match found {
            Ok(reference) => {
                let name = reference.name().as_bstr();
                name.starts_with(under.as_bytes()).then(|| name.to_string())
            }
            // Something under the name that does not read as a ref is in the way all the same.
            Err(_) => Some(full.to_owned()),
        });
        match found {
            Some(name) => Err(SinkError::RefConflict { name }),
            None => Ok(()),
        }
    }

    /// Takes the lock file `lock` (relative to the git directory), as git does: created
    /// exclusively, and tried again while another writer holds it, after a pause of 1 ms that
    /// doubles each time, until the pauses add up to [`LOCK_TIMEOUT`].
    fn lock(&mut self, lock: &str) -> Result<File, SinkError> {
        let path = self.files.git_dir.join(lock);
        let mut paused = Duration::ZERO;
        let mut pause = Duration::from_millis(1);
        loop {
            let error = match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => return Ok(file),
                Err(error) => error,
            };
            match error.kind() {
                io::ErrorKind::AlreadyExists if !path.is_dir() => {}
                io::ErrorKind::AlreadyExists | io::ErrorKind::NotADirectory => {
                    return Err(SinkError::RefConflict {
                        name: lock.to_owned(),
                    });
                }
                _ => return Err(self.files.failed_write(&path, &error)),
            }
            if paused >= LOCK_TIMEOUT {
                return Err(SinkError::RefLocked {
                    lock: lock.to_owned(),
                });
            }
            #[cfg(feature = "test-hooks")]
            if let Some(hook) = &mut self.lock_retry {
                hook();
            }
            let wait = pause.min(LOCK_TIMEOUT.saturating_sub(paused));
            thread::sleep(wait);
            paused += wait;
            pause *= 2;
        }
    }

    /// With the lock of the branch `full` held as `file`: the branch compared with `expected`,
    /// then `new` written to the lock, synced, and the lock renamed onto the ref.
    fn move_locked(
        &self,
        full: &str,
        mut file: File,
        expected: Option<GitOid>,
        new: GitOid,
    ) -> Result<BranchUpdate, SinkError> {
        let actual = self.read_ref(full)?;
        if actual != expected {
            return Ok(BranchUpdate::Mismatch { actual });
        }
        let (lock, path) = self.files.ref_paths(full);
        (file.write_all(format!("{new}\n").as_bytes()))
            .map_err(|error| self.files.failed_write(&lock, &error))?;
        self.files.sync_file(&file, &lock)?;
        drop(file);
        fs::rename(&lock, &path).map_err(|error| match error.kind() {
            io::ErrorKind::IsADirectory | io::ErrorKind::DirectoryNotEmpty => {
                SinkError::RefConflict {
                    name: full.to_owned(),
                }
            }
            _ => self.files.failed_write(&path, &error),
        })?;
        Ok(BranchUpdate::Applied)
    }
}

impl GitSink for GixSink {
    fn object_format(&self) -> ObjectFormat {
        // `open` refuses every other format.
        ObjectFormat::Sha1
    }

    fn stored(&self, oid: GitOid) -> Result<Stored, SinkError> {
        Ok(if self.is_loose(oid)? {
            Stored::Loose
        } else if self.has(oid) {
            Stored::Packed
        } else {
            Stored::Absent
        })
    }

    fn read(&self, oid: GitOid) -> Result<(RawKind, Vec<u8>), SinkError> {
        if let Some(found) = self.read_loose(oid)? {
            return Ok(found);
        }
        let mut buffer = Vec::new();
        let data = (self
            .repo
            .objects
            .try_find(&gix_source::to_gix(oid), &mut buffer))
        .map_err(|error| damaged(oid, &error, format!("object {oid}")))?
        .ok_or(SinkError::Missing { oid })?;
        checked(oid, gix_source::raw_kind(data.kind), data.data.to_vec())
    }

    fn write_object(&mut self, kind: RawKind, bytes: &[u8]) -> Result<GitOid, SinkError> {
        // Bytes git's collision detection flags are no object git reads: refused before a write.
        let Some(oid) = oid::checked_hash(kind, bytes) else {
            return Err(SinkError::Write {
                what: self.files.named(&self.files.objects),
                reason: format!(
                    "git's collision detection flags the {kind}'s bytes as a SHA-1 collision attack"
                ),
            });
        };
        let path = self.files.loose(oid);
        // gix writes nothing for an id it finds, a loose file of that name among them, so one
        // that does not read back as the object goes first. One that does is the object.
        if self.is_loose(oid)? {
            match self.read_loose(oid) {
                Ok(Some(_)) => return Ok(oid),
                Ok(None) => {}
                Err(SinkError::Corrupt { .. }) => match fs::remove_file(&path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(self.files.failed_write(&path, &error)),
                },
                Err(error) => return Err(error),
            }
        }
        let fan_out = path
            .parent()
            .expect("a loose object's file is in its fan-out directory");
        let fresh = !fan_out.is_dir();
        let written =
            (self.repo.write_buf(gix_kind(kind), bytes)).map_err(|error| SinkError::Write {
                what: self.files.named(&path),
                reason: gix_source::reason(&error),
            })?;
        if fresh && fan_out.is_dir() {
            self.made.insert(oid.0[0]);
        }
        let written = gix_source::from_gix(&written);
        if written != oid {
            return Err(SinkError::Write {
                what: self.files.named(&path),
                reason: format!("gix wrote the object as {written}"),
            });
        }
        Ok(oid)
    }

    fn sync(&mut self, oids: &[GitOid]) -> Result<(), SinkError> {
        let mut paths = Vec::new();
        let mut fan_outs = BTreeSet::new();
        for &oid in &oids.iter().copied().collect::<BTreeSet<GitOid>>() {
            if self.is_loose(oid)? {
                paths.push(self.files.loose(oid));
                fan_outs.insert(oid.0[0]);
            } else if !self.has(oid) {
                return Err(SinkError::Missing { oid });
            }
        }
        let objects = &self.files.objects;
        paths.extend(
            fan_outs
                .iter()
                .map(|byte| objects.join(format!("{byte:02x}"))),
        );
        let made = fan_outs.iter().any(|byte| self.made.contains(byte));
        if made {
            paths.push(objects.clone());
        }
        self.files.sync_all(&paths)?;
        self.made.retain(|byte| !fan_outs.contains(byte));
        Ok(())
    }

    fn branch(&self, name: &str) -> Result<Option<GitOid>, SinkError> {
        self.read_ref(&short_ref(HEADS, name)?)
    }

    fn checked_out(&self) -> Result<Option<String>, SinkError> {
        if self.repo.is_bare() {
            return Ok(None);
        }
        let head = self.repo.head_name().map_err(|error| SinkError::Read {
            what: "HEAD".to_owned(),
            reason: gix_source::reason(&error),
        })?;
        Ok(head.and_then(|name| {
            let branch = name.as_bstr().strip_prefix(b"refs/heads/".as_slice())?;
            Some(String::from_utf8_lossy(branch).into_owned())
        }))
    }

    fn cas_branch(
        &mut self,
        name: &str,
        expected: Option<GitOid>,
        new: GitOid,
    ) -> Result<BranchUpdate, SinkError> {
        self.cas_short(&short_ref(HEADS, name)?, expected, new)
    }

    fn tag(&self, name: &str) -> Result<Option<GitOid>, SinkError> {
        self.read_ref(&short_ref(TAGS, name)?)
    }

    fn cas_tag(
        &mut self,
        name: &str,
        expected: Option<GitOid>,
        new: GitOid,
    ) -> Result<BranchUpdate, SinkError> {
        self.cas_short(&short_ref(TAGS, name)?, expected, new)
    }
}

impl GixSink {
    /// [`cas_branch`](GitSink::cas_branch) and [`cas_tag`](GitSink::cas_tag) of the ref `full`
    /// (`refs/heads/<name>`, `refs/tags/<name>`): another ref in the way refused, then git's lock
    /// protocol, then the directories the move made synced.
    fn cas_short(
        &mut self,
        full: &str,
        expected: Option<GitOid>,
        new: GitOid,
    ) -> Result<BranchUpdate, SinkError> {
        if !self.has(new) {
            return Err(SinkError::Missing { oid: new });
        }
        self.refuse_conflicts(full)?;
        let (lock, path) = self.files.ref_paths(full);
        let dir = path
            .parent()
            .expect("a branch's or a tag's file is in refs/");
        let made = self.files.make_dirs(dir)?;
        let file = self.lock(&format!("{full}.lock"))?;
        let moved = self.move_locked(full, file, expected, new);
        if moved != Ok(BranchUpdate::Applied) {
            // A mismatch or a failure leaves the branch as it was, and the lock goes with it.
            return match (moved, fs::remove_file(&lock)) {
                (Ok(_), Err(error)) if error.kind() != io::ErrorKind::NotFound => {
                    Err(self.files.failed_write(&lock, &error))
                }
                (moved, _) => moved,
            };
        }
        // The rename is durable once the ref's directory is synced, and each directory the move
        // made once its parent is.
        let mut dirs = BTreeSet::from([dir.to_owned()]);
        for made in made {
            dirs.extend(made.parent().map(Path::to_owned));
            dirs.insert(made);
        }
        self.files.sync_all(&dirs.into_iter().collect::<Vec<_>>())?;
        Ok(BranchUpdate::Applied)
    }
}

impl Files {
    /// `path` relative to the git directory.
    fn relative(&self, path: &Path) -> PathBuf {
        path.strip_prefix(&self.git_dir).unwrap_or(path).to_owned()
    }

    /// `path`, relative to the git directory, as an error names it.
    fn named(&self, path: &Path) -> String {
        self.relative(path).display().to_string()
    }

    /// The loose file of `oid`.
    fn loose(&self, oid: GitOid) -> PathBuf {
        let hex = oid.to_string();
        self.objects.join(&hex[..2]).join(&hex[2..])
    }

    /// The lock file and the loose file of the ref `full`.
    fn ref_paths(&self, full: &str) -> (PathBuf, PathBuf) {
        let path = self.git_dir.join(full);
        (self.git_dir.join(format!("{full}.lock")), path)
    }

    fn failed_read(&self, path: &Path, error: &io::Error) -> SinkError {
        SinkError::Read {
            what: self.named(path),
            reason: error.to_string(),
        }
    }

    fn failed_write(&self, path: &Path, error: &io::Error) -> SinkError {
        SinkError::Write {
            what: self.named(path),
            reason: error.to_string(),
        }
    }

    /// Makes `dir` and each directory above it that is missing, and returns those it made, the
    /// outermost first. A file where a directory goes is a [`SinkError::RefConflict`] naming it.
    fn make_dirs(&self, dir: &Path) -> Result<Vec<PathBuf>, SinkError> {
        let mut missing = Vec::new();
        let mut at = dir;
        loop {
            match fs::metadata(at) {
                Ok(metadata) if metadata.is_dir() => break,
                Ok(_) => {
                    return Err(SinkError::RefConflict {
                        name: self.named(at),
                    });
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                    ) && at != self.git_dir =>
                {
                    missing.push(at.to_owned());
                    at = at.parent().unwrap_or(&self.git_dir);
                }
                Err(error) => return Err(self.failed_read(at, &error)),
            }
        }
        let mut made = Vec::new();
        for dir in missing.into_iter().rev() {
            match fs::create_dir(&dir) {
                Ok(()) => made.push(dir),
                // Another writer made it meanwhile.
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists && dir.is_dir() => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    return Err(SinkError::RefConflict {
                        name: self.named(&dir),
                    });
                }
                Err(error) => return Err(self.failed_write(&dir, &error)),
            }
        }
        Ok(made)
    }

    /// fsyncs each of `paths`, files and directories: every one, whatever the others return, on
    /// rayon's global pool when there are several (fsyncs made at once share ext4's journal
    /// commits, as the store's flush does); the first failure, in `paths`' order.
    fn sync_all(&self, paths: &[PathBuf]) -> Result<(), SinkError> {
        let sync = |path: &PathBuf| {
            let file = File::open(path).map_err(|error| self.failed_write(path, &error))?;
            self.sync_file(&file, path)
        };
        let results: Vec<Result<(), SinkError>> = if paths.len() <= 1 {
            paths.iter().map(sync).collect()
        } else {
            paths.par_iter().map(sync).collect()
        };
        results.into_iter().collect()
    }

    /// fsyncs `file`, open at `path`, and records `path` when the sink records its syncs.
    fn sync_file(&self, file: &File, path: &Path) -> Result<(), SinkError> {
        file.sync_all()
            .map_err(|error| self.failed_write(path, &error))?;
        #[cfg(feature = "test-hooks")]
        if let Some(synced) = &self.synced {
            let mut synced = synced.lock().unwrap_or_else(PoisonError::into_inner);
            synced.push(self.relative(path));
        }
        Ok(())
    }
}

/// The full name of the branch or tag `name` under `namespace` ([`HEADS`], [`TAGS`]):
/// `refs/heads/<name>`, `refs/tags/<name>`, when git takes that as a ref name (gix's
/// reference-name rules); [`SinkError::BadBranch`] otherwise.
fn short_ref(namespace: &str, name: &str) -> Result<String, SinkError> {
    let full = format!("{namespace}{name}");
    match gix::refs::FullName::try_from(full.as_str()) {
        Ok(_) => Ok(full),
        Err(_) => Err(SinkError::BadBranch {
            name: name.to_owned(),
        }),
    }
}

/// What [`open_repository`] refused, as a sink says it.
///
/// [`open_repository`]: gix_source::open_repository
fn opening(error: SourceError) -> SinkError {
    match error {
        SourceError::NotARepo { reason } => SinkError::NotARepo { reason },
        SourceError::Unsupported(unsupported) => SinkError::Unsupported(unsupported),
        SourceError::Io { reason } => SinkError::Read {
            what: "the repository".to_owned(),
            reason,
        },
        other => SinkError::Read {
            what: "the repository".to_owned(),
            reason: other.to_string(),
        },
    }
}

/// `bytes`, read as the object `oid` of `kind`, when they hash to `oid`; [`SinkError::Corrupt`]
/// otherwise.
fn checked(oid: GitOid, kind: RawKind, bytes: Vec<u8>) -> Result<(RawKind, Vec<u8>), SinkError> {
    match gix_source::not_the_object(oid, kind, &bytes) {
        None => Ok((kind, bytes)),
        Some(reason) => Err(SinkError::Corrupt { oid, reason }),
    }
}

/// A failure to read the object `oid` from `what`: [`SinkError::Corrupt`] when gix classes it as
/// damaged data (a loose file emptied by a power loss among them), [`SinkError::Read`] otherwise.
fn damaged(oid: GitOid, error: &gix::Error, what: String) -> SinkError {
    if error.is_corrupted() {
        SinkError::Corrupt {
            oid,
            reason: gix_source::reason(error),
        }
    } else {
        SinkError::Read {
            what,
            reason: gix_source::reason(error),
        }
    }
}

fn gix_kind(kind: RawKind) -> gix::objs::Kind {
    match kind {
        RawKind::Commit => gix::objs::Kind::Commit,
        RawKind::Tree => gix::objs::Kind::Tree,
        RawKind::Blob => gix::objs::Kind::Blob,
        RawKind::Tag => gix::objs::Kind::Tag,
    }
}

/// The directory a mirror's `core.hooksPath` names, relative to its git directory: empty, so that
/// git runs no hook there.
pub(crate) const HOOKS_NONE: &str = "hooks-none";

/// The git lock files in a git directory's top level that a dead git process can leave (with
/// every `*.lock` under `refs/`).
const TOP_LOCKS: [&str; 3] = ["packed-refs.lock", "index.lock", "shallow.lock"];

/// The suffix of a git lock file.
const LOCK_SUFFIX: &str = ".lock";

/// The configuration a bare repository [`GixSink::create_bare`] makes is given, over gix's own
/// (`core.<key>`, value).
const BARE_CONFIG: [(&str, &str); 3] = [
    ("repositoryformatversion", "0"),
    ("logallrefupdates", "false"),
    ("hooksPath", HOOKS_NONE),
];

impl GixSink {
    /// Makes an empty bare SHA-1 repository at `path` (absent or an empty directory), through gix,
    /// with `core.repositoryformatversion = 0`, `core.logAllRefUpdates = false` and
    /// `core.hooksPath` naming its empty `hooks-none/`, a path relative to the repository so that
    /// no host path is written in it; every file and directory it made is fsynced, and `path`'s
    /// parent.
    pub fn create_bare(path: &Path) -> Result<(), SinkError> {
        let files = Files {
            git_dir: path.to_owned(),
            objects: path.join("objects"),
            #[cfg(feature = "test-hooks")]
            synced: None,
        };

        // 1. The repository as gix makes it. Its errors name it by its own name (`mirror.git`):
        // gix's text carries the host path, so only the io error under it is kept.
        let name = path
            .file_name()
            .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
        let options = gix::create::Options::default();
        gix::create::into(path, gix::create::Kind::Bare, options).map_err(|error| {
            SinkError::Write {
                what: name.clone(),
                reason: unpathed(&error, path, &name),
            }
        })?;

        // 2. Its configuration, replaced by git's lock protocol, and the empty hooks directory.
        files.configure(&BARE_CONFIG)?;
        let hooks = path.join(HOOKS_NONE);
        fs::create_dir(&hooks).map_err(|error| files.failed_write(&hooks, &error))?;

        // 3. Everything made, durable: the files and directories inside, then the repository's
        // own entry in its parent, which is outside the git directory and so named by the
        // repository's name.
        let made = files.tree(path)?;
        files.sync_all(&made)?;
        let Some(parent) = path.parent() else {
            return Ok(());
        };
        let synced = File::open(parent).and_then(|dir| dir.sync_all());
        synced.map_err(|error| SinkError::Write {
            what: format!("the directory holding {name}"),
            reason: error.to_string(),
        })
    }

    /// Moves the ref `full` (a full name: `refs/remotes/<remote>/<branch>`,
    /// `refs/heads/push/<remote>/<branch>`) from `expected` to `new` by compare-and-swap, durably
    /// as [`cas_branch`](GitSink::cas_branch) moves a branch, which it calls for a name under
    /// `refs/heads/`; `new` `None` deletes it, from `packed-refs` too. [`SinkError::BadBranch`]
    /// names a full name git takes for no ref.
    pub fn cas_ref(
        &mut self,
        full: &str,
        expected: Option<GitOid>,
        new: Option<GitOid>,
    ) -> Result<BranchUpdate, SinkError> {
        full_ref(full)?;
        match (new, full.strip_prefix("refs/heads/")) {
            (None, _) => self.delete_ref(full, expected),
            (Some(new), Some(branch)) => self.cas_branch(branch, expected, new),
            (Some(new), None) => self.set_ref(full, expected, new),
        }
    }

    /// What the ref `full` names: `None` when there is no such ref.
    pub(crate) fn reference(&self, full: &str) -> Result<Option<GitOid>, SinkError> {
        full_ref(full)?;
        self.read_ref(full)
    }

    /// The refs whose full names start with `prefix` (which ends with `/`), loose and packed, and
    /// what each names; symbolic ones are passed over.
    pub(crate) fn references_under(
        &self,
        prefix: &str,
    ) -> Result<Vec<(String, GitOid)>, SinkError> {
        let listing = |error: gix::Error| SinkError::Read {
            what: prefix.to_owned(),
            reason: gix_source::reason(&error),
        };
        let platform = self.repo.references().map_err(listing)?;
        let refs = platform.prefixed(prefix).map_err(listing)?;

        // Each ref under the prefix that names an object.
        let mut found = Vec::new();
        for reference in refs {
            let reference = reference.map_err(|error| SinkError::Read {
                what: prefix.to_owned(),
                reason: error.to_string(),
            })?;
            let name = reference.name().as_bstr().to_string();
            if !name.starts_with(prefix) {
                continue;
            }
            if let gix::refs::TargetRef::Object(id) = reference.target() {
                found.push((name, gix_source::from_gix(&id.to_owned())));
            }
        }
        Ok(found)
    }

    /// [`cas_ref`](GixSink::cas_ref)'s move of a ref outside `refs/heads/`: as `cas_branch`, by
    /// the ref's lock, with no look for a packed ref in the way (git names these refs itself).
    fn set_ref(
        &mut self,
        full: &str,
        expected: Option<GitOid>,
        new: GitOid,
    ) -> Result<BranchUpdate, SinkError> {
        if !self.has(new) {
            return Err(SinkError::Missing { oid: new });
        }

        // 1. The ref's directories, then its lock, then the compare and the move.
        let (lock, path) = self.files.ref_paths(full);
        let dir = path.parent().expect("a ref's file is in refs/");
        let made = self.files.make_dirs(dir)?;
        let file = self.lock(&format!("{full}{LOCK_SUFFIX}"))?;
        let moved = self.move_locked(full, file, expected, new);

        // 2. A mismatch or a failure leaves the ref as it was, and the lock goes with it.
        if moved != Ok(BranchUpdate::Applied) {
            return match (moved, fs::remove_file(&lock)) {
                (Ok(_), Err(error)) if error.kind() != io::ErrorKind::NotFound => {
                    Err(self.files.failed_write(&lock, &error))
                }
                (moved, _) => moved,
            };
        }

        // 3. The rename is durable once the ref's directory is synced, and each directory the
        // move made once its parent is.
        let mut dirs = BTreeSet::from([dir.to_owned()]);
        for made in made {
            dirs.extend(made.parent().map(Path::to_owned));
            dirs.insert(made);
        }
        self.files.sync_all(&dirs.into_iter().collect::<Vec<_>>())?;
        Ok(BranchUpdate::Applied)
    }

    /// [`cas_ref`](GixSink::cas_ref)'s delete: under the ref's lock, the ref compared with
    /// `expected`, then taken out of `packed-refs` and its loose file removed, in git's order (a
    /// crash between the two leaves the loose value, never an older packed one).
    fn delete_ref(
        &mut self,
        full: &str,
        expected: Option<GitOid>,
    ) -> Result<BranchUpdate, SinkError> {
        // 1. An absent ref is already deleted: no lock, no directory made.
        if self.read_ref(full)?.is_none() {
            return Ok(match expected {
                None => BranchUpdate::Applied,
                Some(_) => BranchUpdate::Mismatch { actual: None },
            });
        }

        // 2. Under its lock: compare, unpack, unlink.
        let (lock, path) = self.files.ref_paths(full);
        let file = self.lock(&format!("{full}{LOCK_SUFFIX}"))?;
        let deleted = self.delete_locked(full, expected, &path);
        drop(file);
        let unlocked = fs::remove_file(&lock);
        let deleted = deleted?;
        if let Err(error) = unlocked
            && error.kind() != io::ErrorKind::NotFound
        {
            return Err(self.files.failed_write(&lock, &error));
        }
        if deleted != BranchUpdate::Applied {
            return Ok(deleted);
        }

        // 3. Durable: the ref's directory and the git directory (`packed-refs`' rename).
        let dir = path.parent().expect("a ref's file is in refs/");
        let dirs = BTreeSet::from([dir.to_owned(), self.files.git_dir.clone()]);
        self.files.sync_all(&dirs.into_iter().collect::<Vec<_>>())?;
        Ok(BranchUpdate::Applied)
    }

    /// With the lock of `full` held: the ref compared with `expected`, then deleted from
    /// `packed-refs` and its loose file at `path` removed.
    fn delete_locked(
        &mut self,
        full: &str,
        expected: Option<GitOid>,
        path: &Path,
    ) -> Result<BranchUpdate, SinkError> {
        let actual = self.read_ref(full)?;
        if actual != expected {
            return Ok(BranchUpdate::Mismatch { actual });
        }
        self.unpack(full)?;
        match fs::remove_file(path) {
            Ok(()) => Ok(BranchUpdate::Applied),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(BranchUpdate::Applied),
            Err(error) => Err(self.files.failed_write(path, &error)),
        }
    }

    /// Rewrites `packed-refs` without `full` (its line and the peeled `^` line after it), by
    /// git's lock protocol, synced before the rename; nothing when it does not list `full`.
    fn unpack(&mut self, full: &str) -> Result<(), SinkError> {
        let packed = self.files.git_dir.join("packed-refs");
        let text = match fs::read(&packed) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(self.files.failed_read(&packed, &error)),
        };

        // 1. The lines that stay. `packed-refs` lists `<oid> <name>`, each perhaps followed by
        // `^<oid>`, the object an annotated tag peels to.
        let mut kept = Vec::with_capacity(text.len());
        let mut dropping = false;
        for line in text.split_inclusive(|&byte| byte == b'\n') {
            if dropping && line.starts_with(b"^") {
                continue;
            }
            let body = line.strip_suffix(b"\n").unwrap_or(line);
            let name = body.splitn(2, |&byte| byte == b' ').nth(1);
            dropping = !body.starts_with(b"#") && name == Some(full.as_bytes());
            if !dropping {
                kept.extend_from_slice(line);
            }
        }
        if kept.len() == text.len() {
            return Ok(());
        }

        // 2. Written to its lock, synced, renamed onto it.
        let lock_name = format!("packed-refs{LOCK_SUFFIX}");
        let lock = self.files.git_dir.join(&lock_name);
        let mut file = self.lock(&lock_name)?;
        let written = (file.write_all(&kept))
            .map_err(|error| self.files.failed_write(&lock, &error))
            .and_then(|()| self.files.sync_file(&file, &lock));
        drop(file);
        let renamed = written.and_then(|()| {
            fs::rename(&lock, &packed).map_err(|error| self.files.failed_write(&packed, &error))
        });
        if renamed.is_err() {
            let _ = fs::remove_file(&lock);
        }
        renamed
    }
}

impl Files {
    /// Sets each `core.<key>` of `values` in the repository's `config`, the rest of it as it is:
    /// the new text written to `config.lock`, synced, and renamed onto `config`.
    fn configure(&self, values: &[(&str, &str)]) -> Result<(), SinkError> {
        let path = self.git_dir.join("config");
        let text = fs::read_to_string(&path).map_err(|error| self.failed_read(&path, &error))?;
        let mut config =
            gix::config::File::try_from(text.as_str()).map_err(|error| SinkError::Read {
                what: "config".to_owned(),
                reason: error.to_string(),
            })?;

        // 1. The values.
        for (key, value) in values {
            let set = config.set_raw_value_by("core", None, *key, *value);
            set.map_err(|error| SinkError::Write {
                what: "config".to_owned(),
                reason: error.to_string(),
            })?;
        }

        // 2. Written by git's lock protocol.
        let lock = self.git_dir.join(format!("config{LOCK_SUFFIX}"));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock)
            .map_err(|error| self.failed_write(&lock, &error))?;
        (file.write_all(&config.to_bstring())).map_err(|error| self.failed_write(&lock, &error))?;
        self.sync_file(&file, &lock)?;
        drop(file);
        fs::rename(&lock, &path).map_err(|error| self.failed_write(&path, &error))
    }

    /// `dir` and every file and directory under it, symlinks not followed.
    fn tree(&self, dir: &Path) -> Result<Vec<PathBuf>, SinkError> {
        let mut found = vec![dir.to_owned()];
        let mut at = 0;
        while at < found.len() {
            let path = found[at].clone();
            at += 1;
            let metadata =
                fs::symlink_metadata(&path).map_err(|error| self.failed_read(&path, &error))?;
            if !metadata.is_dir() {
                continue;
            }
            let entries = fs::read_dir(&path).map_err(|error| self.failed_read(&path, &error))?;
            for entry in entries {
                let entry = entry.map_err(|error| self.failed_read(&path, &error))?;
                found.push(entry.path());
            }
        }
        Ok(found)
    }
}

/// What failed in gix's `error` about `path`, without the host path: the io error under it, or
/// gix's text with `path` replaced by `name` when there is none (e.g. a non-empty directory).
fn unpathed(error: &gix::Error, path: &Path, name: &str) -> String {
    if let Some(io) = error.downcast_any_ref::<io::Error>() {
        return io.to_string();
    }
    gix_source::reason(error).replace(&path.display().to_string(), name)
}

/// Checks that git takes `full` as a ref name; [`SinkError::BadBranch`] names it otherwise.
fn full_ref(full: &str) -> Result<(), SinkError> {
    match gix::refs::FullName::try_from(full) {
        Ok(_) => Ok(()),
        Err(_) => Err(SinkError::BadBranch {
            name: full.to_owned(),
        }),
    }
}

/// Removes the git lock files a dead git process left in the git directory `git_dir` (every
/// `*.lock` under `refs/`, and `packed-refs.lock`, `index.lock`, `shallow.lock`), each older than
/// `age` by its mtime, and gives their names relative to `git_dir`, sorted. A younger one may be
/// a live git process's: then nothing is removed, and the first such, by name, is
/// [`SinkError::RefLocked`].
pub(crate) fn remove_stale_locks(git_dir: &Path, age: Duration) -> Result<Vec<String>, SinkError> {
    let files = Files {
        git_dir: git_dir.to_owned(),
        objects: git_dir.join("objects"),
        #[cfg(feature = "test-hooks")]
        synced: None,
    };

    // 1. The lock files: those of `refs/`, and the top level's.
    let mut locks = BTreeSet::new();
    let refs = git_dir.join("refs");
    if refs.is_dir() {
        for path in files.tree(&refs)? {
            let named = path.file_name().and_then(|name| name.to_str());
            if named.is_some_and(|name| name.ends_with(LOCK_SUFFIX)) && path.is_file() {
                locks.insert(files.named(&path));
            }
        }
    }
    for name in TOP_LOCKS {
        if git_dir.join(name).is_file() {
            locks.insert(name.to_owned());
        }
    }

    // 2. None young: a young one stops the sweep before anything is removed.
    let now = std::time::SystemTime::now();
    for name in &locks {
        let path = git_dir.join(name);
        let metadata = fs::metadata(&path).map_err(|error| files.failed_read(&path, &error))?;
        let modified = metadata
            .modified()
            .map_err(|error| files.failed_read(&path, &error))?;
        let old = now.duration_since(modified).unwrap_or(Duration::ZERO);
        if old < age {
            return Err(SinkError::RefLocked { lock: name.clone() });
        }
    }

    // 3. Each removed.
    for name in &locks {
        let path = git_dir.join(name);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(files.failed_write(&path, &error)),
        }
    }
    Ok(locks.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bytes git's collision detection flags are no object git reads: the sink refuses them
    /// before it writes, and the repository holds them under no id.
    #[test]
    fn a_flagged_object_is_not_written() {
        let dir = tempfile::tempdir().unwrap();
        GixSink::create_bare(dir.path()).unwrap();
        let mut sink = GixSink::open(dir.path()).unwrap();
        let bytes = b"flagged\n";
        let real = oid::git_hash(RawKind::Blob, bytes);
        oid::flagged::flag(RawKind::Blob, bytes);

        let got = sink.write_object(RawKind::Blob, bytes);
        assert!(
            matches!(&got, Err(SinkError::Write { what, reason })
                if what == "objects" && reason.contains("collision detection")),
            "{got:?}"
        );
        for oid in [real, GitOid::NULL] {
            assert_eq!(sink.stored(oid), Ok(Stored::Absent), "{oid}");
        }
    }
}
