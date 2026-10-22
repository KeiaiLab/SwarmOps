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
//!
//! The M13 plan's task C1 froze these items; task C4 writes their bodies, which until then
//! panic with the task that writes them.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use camino::Utf8Path;
use thiserror::Error;

use crate::oid::GitOid;
use crate::sink::SinkError;
use crate::source::SourceError;

/// The task that writes this module's bodies.
const PLANNED: &str = "planned in M13 (C4)";

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
    /// when it is absent; never `mirror.git`, which [`Mirror::ensure`] makes under the lock.
    pub fn open(_store_root: &Utf8Path) -> Result<Mirror, MirrorError> {
        unimplemented!("{PLANNED}")
    }

    /// Takes the mirror's lock, or fails at once with [`MirrorError::Busy`].
    pub fn lock(&self) -> Result<MirrorLock, MirrorError> {
        unimplemented!("{PLANNED}")
    }

    /// Makes `mirror.git`, or rebuilds it, under the lock.
    pub fn ensure(&self, _lock: &MirrorLock) -> Result<Rebuilt, MirrorError> {
        unimplemented!("{PLANNED}")
    }

    /// Removes the git lock files older than [`STALE_LOCK_AGE`], and gives their names as
    /// warnings; a younger one is [`MirrorError::GitLockBusy`].
    pub fn sweep_git_locks(&self, _lock: &MirrorLock) -> Result<Vec<String>, MirrorError> {
        unimplemented!("{PLANNED}")
    }

    /// `mirror.git`, the directory every git command runs in.
    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    /// What the last fetch saw of `remote`'s `branch`: `refs/remotes/<remote>/<branch>`.
    pub fn tracking(&self, _remote: &str, _branch: &str) -> Result<Option<GitOid>, MirrorError> {
        unimplemented!("{PLANNED}")
    }

    /// Moves `refs/remotes/<remote>/<branch>` from `old` to `new` by compare-and-swap.
    pub fn set_tracking(
        &self,
        _remote: &str,
        _branch: &str,
        _old: Option<GitOid>,
        _new: GitOid,
    ) -> Result<(), MirrorError> {
        unimplemented!("{PLANNED}")
    }

    /// Sets the staging branch `refs/heads/push/<remote>/<branch>` to `to`, or deletes it when
    /// `to` is `None`, durably; gives the branch's short name.
    pub fn seed_staging(
        &self,
        _remote: &str,
        _branch: &str,
        _to: Option<GitOid>,
    ) -> Result<String, MirrorError> {
        unimplemented!("{PLANNED}")
    }

    /// Removes the mirror's refs of `remote`, and gives how many.
    pub fn remove_remote(&self, _remote: &str) -> Result<u64, MirrorError> {
        unimplemented!("{PLANNED}")
    }
}
