//! [`GixSink`], the [`GitSink`] of a repository on disk, written through gix: with
//! [`gix_source`](crate::gix_source), the crate's contact with gix (umbrella decision 3).
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

use crate::gix_source::{from_gix, not_the_object, open_repository, raw_kind, reason, to_gix};
use crate::{
    BranchUpdate, GitOid, GitSink, ObjectFormat, RawKind, SinkError, SourceError, Stored, git_hash,
};

/// How long the export tries a branch's lock before the branch is [`SinkError::RefLocked`]:
/// git's `core.filesRefLockTimeout` default, so that two exports racing to one branch take turns.
/// Counted as git counts it, by the pauses between attempts rather than by the clock, so that a
/// loaded host makes as many attempts as a quiet one.
const LOCK_TIMEOUT: Duration = Duration::from_millis(100);

/// The paths a [`GixSink`] opened with [`GixSink::open_recording_syncs`] fsynced, relative to the
/// git directory, in the order the syncs ended.
#[cfg(feature = "test-hooks")]
pub type SyncedPaths = Arc<Mutex<Vec<PathBuf>>>;

/// A [`GitSink`] over a git repository on disk, through gix (`=0.89.0`, its `sha1` feature alone).
///
/// It opens the repository as [`GixSource::open`](crate::GixSource::open) does, refusing what that
/// refuses. Objects are written by gix's loose writer: `objects/<2 hex>/<38 hex>`, zlib, as git
/// writes them. [`stored`] and [`read`] go by what the repository holds on disk: a loose file is
/// read (and checked against its id) before a pack, and gix's answer for the empty tree, which it
/// gives whether or not the tree is stored, is not taken: git's own object writer goes by the files
/// too, and writes the tree where it is not. [`sync`] fsyncs each file and directory on rayon's
/// global pool. A branch moves by git's lock protocol: `refs/heads/<name>.lock` created
/// exclusively, tried again for up to 100 ms (git's `core.filesRefLockTimeout`), the ref read and
/// compared under it, the new value written to the lock and synced, the lock renamed onto the ref,
/// and the ref's directory synced, with each directory the move made and its parent. A process that
/// dies between the lock and the rename leaves the lock, which, as with git, stays until it is
/// removed; a failure to sync those directories after the rename leaves the branch moved, perhaps
/// not durably.
///
/// [`stored`]: GitSink::stored
/// [`read`]: GitSink::read
/// [`sync`]: GitSink::sync
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
        let repo = open_repository(path).map_err(opening)?;
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
        self.repo.objects.exists(&to_gix(oid))
    }

    /// The loose file of `oid`, read and checked against its id; `None` when there is no such
    /// file.
    fn read_loose(&self, oid: GitOid) -> Result<Option<(RawKind, Vec<u8>)>, SinkError> {
        let mut buffer = Vec::new();
        match self.loose.try_find(&to_gix(oid), &mut buffer) {
            Ok(Some(data)) => checked(oid, raw_kind(data.kind), data.data.to_vec()).map(Some),
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
                    reason: reason(&error),
                });
            }
        };
        match reference.target() {
            gix::refs::TargetRef::Object(id) => Ok(Some(from_gix(&id.to_owned()))),
            gix::refs::TargetRef::Symbolic(_) => Err(SinkError::RefConflict {
                name: full.to_owned(),
            }),
        }
    }

    /// Refuses the branch `full` when another ref is in the way: one whose name is a directory of
    /// `full`'s (its loose file, `packed-refs`, or a file that is no ref), or one under `full/`.
    /// git refuses both, and the filesystem alone would not see a packed one.
    fn refuse_conflicts(&self, full: &str) -> Result<(), SinkError> {
        let mut at = "refs/heads/".len();
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
            reason: reason(&error),
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
        let data = (self.repo.objects.try_find(&to_gix(oid), &mut buffer))
            .map_err(|error| damaged(oid, &error, format!("object {oid}")))?
            .ok_or(SinkError::Missing { oid })?;
        checked(oid, raw_kind(data.kind), data.data.to_vec())
    }

    fn write_object(&mut self, kind: RawKind, bytes: &[u8]) -> Result<GitOid, SinkError> {
        let oid = git_hash(kind, bytes);
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
                reason: reason(&error),
            })?;
        if fresh && fan_out.is_dir() {
            self.made.insert(oid.0[0]);
        }
        let written = from_gix(&written);
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
        self.read_ref(&branch_ref(name)?)
    }

    fn checked_out(&self) -> Result<Option<String>, SinkError> {
        if self.repo.is_bare() {
            return Ok(None);
        }
        let head = self.repo.head_name().map_err(|error| SinkError::Read {
            what: "HEAD".to_owned(),
            reason: reason(&error),
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
        let full = branch_ref(name)?;
        if !self.has(new) {
            return Err(SinkError::Missing { oid: new });
        }
        self.refuse_conflicts(&full)?;
        let (lock, path) = self.files.ref_paths(&full);
        let dir = path.parent().expect("a branch's file is in refs/heads/");
        let made = self.files.make_dirs(dir)?;
        let file = self.lock(&format!("{full}.lock"))?;
        let moved = self.move_locked(&full, file, expected, new);
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

/// The full name of the branch `name`, `refs/heads/<name>`, when git takes that as a ref name
/// (gix's reference-name rules); [`SinkError::BadBranch`] otherwise.
fn branch_ref(name: &str) -> Result<String, SinkError> {
    let full = format!("refs/heads/{name}");
    match gix::refs::FullName::try_from(full.as_str()) {
        Ok(_) => Ok(full),
        Err(_) => Err(SinkError::BadBranch {
            name: name.to_owned(),
        }),
    }
}

/// What [`open_repository`] refused, as a sink says it.
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
    match not_the_object(oid, kind, &bytes) {
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
            reason: reason(error),
        }
    } else {
        SinkError::Read {
            what,
            reason: reason(error),
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
