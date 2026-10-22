//! The store's git mirror (M13): a bare repository SwarmOps owns at `<store>/git/mirror.git`,
//! which git fetches into and pushes from, and which the import reads and the export writes
//! (storage spec §2, §7).
//!
//! ```text
//! <store>/git/
//!   MIRROR                              "swarmops-git-mirror 1\n": the layout's version
//!   lock                                one fetch, push, sync or clone at a time
//!   mirror.git/                         bare, SHA-1, never written by people
//!     refs/remotes/<remote>/<branch>    what the last fetch saw of each remote branch
//!     refs/remote-tags/<remote>/<tag>   the remote's tags, never imported
//!     refs/heads/push/<remote>/<branch> the branch a push's export writes
//!     hooks-none/                       empty: git runs no hook here
//! ```
//!
//! The mirror is a cache: it holds nothing the store and the remote cannot give again, and one
//! that is missing, of an unknown version or that gix cannot open is set aside and made again by
//! the next fetch. Its lock is taken once per command and passed down by reference
//! ([`MirrorLock`]): it is not re-entrant, and a held lock is refused at once, never waited for.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_store::Layout;
use thiserror::Error;

use crate::gix_sink::{GixSink, remove_stale_locks};
use crate::oid::GitOid;
use crate::sink::{BranchUpdate, SinkError};
use crate::source::SourceError;

/// The mirror's directory in a store: `<store>/git`.
const GIT: &str = "git";

/// The marker file, in `git/`, and what it holds for this layout.
const MARKER_FILE: &str = "MIRROR";
const MARKER: &[u8] = b"swarmops-git-mirror 1\n";

/// The lock file, in `git/`.
const LOCK_FILE: &str = "lock";

/// The bare repository, in `git/`, and the prefix of the name a broken one is set aside as.
const MIRROR_GIT: &str = "mirror.git";
const BROKEN: &str = "mirror.git.broken-";

/// Where a remote's refs are in `mirror.git`: its branches as the last fetch saw them, its tags,
/// and the staging branches a push's export writes (under `refs/heads/`, a branch to the export).
const TRACKING: &str = "refs/remotes";
const REMOTE_TAGS: &str = "refs/remote-tags";
const STAGING: &str = "push";

/// How old a git lock file in the mirror must be, by its mtime, before a run takes it for a dead
/// process's and removes it: younger, it may belong to a git child still writing.
pub const STALE_LOCK_AGE: Duration = Duration::from_mins(10);

/// A store's mirror: `<store>/git/`.
#[derive(Debug)]
pub struct Mirror {
    /// `<store>/git/mirror.git`.
    git_dir: PathBuf,
}

/// The mirror's lock, `git/lock`, held until dropped: the proof a step runs under it.
#[derive(Debug)]
pub struct MirrorLock {
    /// The open lock file, which holds the lock.
    _file: File,
}

/// What [`Mirror::ensure`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rebuilt {
    /// The mirror was there and sound.
    Kept,
    /// There was none: it was made.
    Created,
    /// The one there was unsound: it was renamed aside, and an empty one made.
    SetAside {
        /// The name it was renamed to: `mirror.git.broken-<unix ms>`.
        name: String,
    },
}

/// Why the mirror failed.
#[derive(Debug, Error)]
pub enum MirrorError {
    /// Another command holds the mirror's lock (`E_BUSY`, `lock: "git_mirror"`).
    #[error("another command holds the mirror's lock")]
    Busy,
    /// A git lock file younger than [`STALE_LOCK_AGE`] is in the mirror (`E_BUSY`, `lock:
    /// "git_mirror_file"`).
    #[error("{file} is held: a git process may still be writing it")]
    GitLockBusy {
        /// The lock file, relative to `mirror.git`.
        file: String,
    },
    /// The filesystem refused an operation on the mirror.
    #[error("the mirror: {0}")]
    Io(#[from] io::Error),
    /// Writing the mirror failed.
    #[error(transparent)]
    Sink(#[from] SinkError),
    /// Reading the mirror failed.
    #[error(transparent)]
    Source(#[from] SourceError),
}

impl Mirror {
    /// The mirror of the store at `store_root`: makes `git/` and `git/lock`, and writes `MIRROR`
    /// when it is absent; never `mirror.git`, which [`Mirror::ensure`] makes under the lock. A
    /// `MIRROR` that is there, known or not, is left for `ensure`.
    pub fn open(store_root: &Utf8Path) -> Result<Mirror, MirrorError> {
        let root = store_root.as_std_path();
        let git = root.join(GIT);

        // 1. `git/`, durable in the store's root.
        if make_dir(&git)? {
            sync_dir(root)?;
        }

        // 2. The lock file and the marker, each durable in `git/` when made.
        let mut made = false;
        let lock = git.join(LOCK_FILE);
        if !exists(&lock)? {
            OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .open(&lock)?;
            made = true;
        }
        if !exists(&git.join(MARKER_FILE))? {
            write_marker(&git)?;
            made = true;
        }
        if made {
            sync_dir(&git)?;
        }
        Ok(Mirror {
            git_dir: git.join(MIRROR_GIT),
        })
    }

    /// Takes the mirror's lock, or fails at once with [`MirrorError::Busy`].
    pub fn lock(&self) -> Result<MirrorLock, MirrorError> {
        let path = self.git().join(LOCK_FILE);
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        match file.try_lock() {
            Ok(()) => Ok(MirrorLock { _file: file }),
            Err(TryLockError::WouldBlock) => Err(MirrorError::Busy),
            Err(TryLockError::Error(error)) => Err(MirrorError::Io(error)),
        }
    }

    /// Makes `mirror.git`, or rebuilds it, under the lock: one that is there, under the known
    /// marker, and that gix opens is kept; one under a missing or unknown marker, or that gix
    /// cannot open, is renamed aside as `mirror.git.broken-<unix ms>` (never deleted), and an
    /// empty one is made, then the marker written.
    pub fn ensure(&self, _lock: &MirrorLock) -> Result<Rebuilt, MirrorError> {
        let git = self.git();
        let marked = read_marker(git)?;
        let there = exists(&self.git_dir)?;

        // 1. Kept: marked, there, and gix opens it.
        if marked && there && GixSink::open(&self.git_dir).is_ok() {
            return Ok(Rebuilt::Kept);
        }

        // 2. Set aside, or absent.
        let rebuilt = if there {
            self.set_aside()?
        } else {
            Rebuilt::Created
        };

        // 3. Made again, then marked: a crash before the marker leaves one the next run sets
        // aside again.
        GixSink::create_bare(&self.git_dir)?;
        if !marked {
            write_marker(git)?;
            sync_dir(git)?;
        }
        Ok(rebuilt)
    }

    /// Removes the git lock files older than [`STALE_LOCK_AGE`], and gives their names as
    /// warnings; a younger one is [`MirrorError::GitLockBusy`], and then none is removed.
    pub fn sweep_git_locks(&self, _lock: &MirrorLock) -> Result<Vec<String>, MirrorError> {
        if !exists(&self.git_dir)? {
            return Ok(Vec::new());
        }
        match remove_stale_locks(&self.git_dir, STALE_LOCK_AGE) {
            Ok(removed) => Ok(removed),
            Err(SinkError::RefLocked { lock }) => Err(MirrorError::GitLockBusy { file: lock }),
            Err(error) => Err(error.into()),
        }
    }

    /// `mirror.git`, the directory every git command runs in.
    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    /// What the last fetch saw of `remote`'s `branch`: `refs/remotes/<remote>/<branch>`.
    pub fn tracking(&self, remote: &str, branch: &str) -> Result<Option<GitOid>, MirrorError> {
        let sink = GixSink::open(&self.git_dir)?;
        Ok(sink.reference(&tracking_ref(remote, branch))?)
    }

    /// Moves `refs/remotes/<remote>/<branch>` from `old` to `new` by compare-and-swap. A ref that
    /// names something else is left as it is, and is [`SinkError::Write`]: under the mirror's lock
    /// nothing else moves it.
    pub fn set_tracking(
        &self,
        remote: &str,
        branch: &str,
        old: Option<GitOid>,
        new: GitOid,
    ) -> Result<(), MirrorError> {
        let full = tracking_ref(remote, branch);
        let mut sink = GixSink::open(&self.git_dir)?;
        match sink.cas_ref(&full, old, Some(new))? {
            BranchUpdate::Applied => Ok(()),
            BranchUpdate::Mismatch { actual } => Err(moved(full, old, actual)),
        }
    }

    /// Sets the staging branch `refs/heads/push/<remote>/<branch>` to `to`, or deletes it when
    /// `to` is `None`, durably; gives the branch's short name.
    pub fn seed_staging(
        &self,
        remote: &str,
        branch: &str,
        to: Option<GitOid>,
    ) -> Result<String, MirrorError> {
        let name = format!("{STAGING}/{remote}/{branch}");
        let full = format!("refs/heads/{name}");
        let mut sink = GixSink::open(&self.git_dir)?;

        // Under the mirror's lock the branch is ours: moved from whatever it names.
        let actual = sink.reference(&full)?;
        if actual == to {
            return Ok(name);
        }
        match sink.cas_ref(&full, actual, to)? {
            BranchUpdate::Applied => Ok(name),
            BranchUpdate::Mismatch { actual: now } => Err(moved(full, actual, now)),
        }
    }

    /// Removes the mirror's refs of `remote` (its tracking refs, its tags and its staging
    /// branches), and gives how many.
    pub fn remove_remote(&self, remote: &str) -> Result<u64, MirrorError> {
        let mut sink = GixSink::open(&self.git_dir)?;
        let prefixes = [
            format!("{TRACKING}/{remote}/"),
            format!("{REMOTE_TAGS}/{remote}/"),
            format!("refs/heads/{STAGING}/{remote}/"),
        ];

        // Each ref under the three prefixes, deleted from the value it names.
        let mut removed = 0;
        for prefix in &prefixes {
            for (full, oid) in sink.references_under(prefix)? {
                match sink.cas_ref(&full, Some(oid), None)? {
                    BranchUpdate::Applied => removed += 1,
                    BranchUpdate::Mismatch { actual } => {
                        return Err(moved(full, Some(oid), actual));
                    }
                }
            }
        }
        Ok(removed)
    }

    /// `git/`, which holds the marker, the lock and `mirror.git`.
    fn git(&self) -> &Path {
        self.git_dir.parent().expect("mirror.git is in git/")
    }

    /// Renames `mirror.git` aside as `mirror.git.broken-<unix ms>`, a name not yet taken.
    fn set_aside(&self) -> Result<Rebuilt, MirrorError> {
        let git = self.git();
        let since = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO);
        let mut ms = since.as_millis();

        // The first free name from now on.
        let mut name = format!("{BROKEN}{ms}");
        while exists(&git.join(&name))? {
            ms += 1;
            name = format!("{BROKEN}{ms}");
        }
        fs::rename(&self.git_dir, git.join(&name))?;
        sync_dir(git)?;
        Ok(Rebuilt::SetAside { name })
    }
}

/// Whether `path` is inside a store's `git/` directory, the mirror's, which only the remote
/// commands write under the mirror's lock: one of its ancestors (`path` itself included, after
/// symlinks are resolved) is a directory named `git` whose parent holds a store's `FORMAT`. A
/// path that does not resolve (it does not exist) is not.
pub fn inside_store_git(path: &Path) -> bool {
    let Ok(resolved) = fs::canonicalize(path) else {
        return false;
    };
    resolved.ancestors().any(|dir| {
        let named_git = dir.file_name().is_some_and(|name| name == GIT);
        let store = dir
            .parent()
            .map(Path::to_owned)
            .map(Utf8PathBuf::from_path_buf);
        match (named_git, store) {
            (true, Some(Ok(store))) => Layout::new(store).format_file().is_file(),
            _ => false,
        }
    })
}

/// `refs/remotes/<remote>/<branch>`.
fn tracking_ref(remote: &str, branch: &str) -> String {
    format!("{TRACKING}/{remote}/{branch}")
}

/// The error for a ref `full` of the mirror that named `actual` where `expected` was.
fn moved(full: String, expected: Option<GitOid>, actual: Option<GitOid>) -> MirrorError {
    let show =
        |oid: Option<GitOid>| oid.map_or_else(|| "nothing".to_owned(), |oid| oid.to_string());
    MirrorError::Sink(SinkError::Write {
        reason: format!(
            "it names {}, not {}: something other than SwarmOps moved it",
            show(actual),
            show(expected)
        ),
        what: full,
    })
}

/// Whether `path` is there (a symlink is not followed).
fn exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Makes the directory `path`; whether it made it.
fn make_dir(path: &Path) -> io::Result<bool> {
    match fs::create_dir(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists && path.is_dir() => Ok(false),
        Err(error) => Err(error),
    }
}

/// Whether `git/MIRROR` holds this layout's marker; a missing one does not.
fn read_marker(git: &Path) -> io::Result<bool> {
    match fs::read(git.join(MARKER_FILE)) {
        Ok(marker) => Ok(marker == MARKER),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Writes `git/MIRROR`: to a temporary file, synced, then renamed onto it. `git/` is synced by
/// the caller.
fn write_marker(git: &Path) -> io::Result<()> {
    let temp = git.join(format!("{MARKER_FILE}.tmp"));
    let mut file = File::create(&temp)?;
    file.write_all(MARKER)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temp, git.join(MARKER_FILE))
}

/// fsyncs the directory `path`.
fn sync_dir(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}
