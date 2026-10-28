//! The serving repository, `<store>/git/serve.git` (M14 design §0.2): bare, SHA-1, written by
//! SwarmOps and host git alone, apart from the M13 mirror, with its own lock and the one
//! `proc-receive` hook in `<store>/git/serve-hooks/`. A cache: one that is missing is made again
//! and exported again.

use std::collections::BTreeMap;
use std::time::Duration;

use camino::{Utf8Path, Utf8PathBuf};

use crate::oid::GitOid;
use crate::sink::BranchUpdate;

/// The bytes of `<store>/git/SERVE`, the serving repository's layout and its version.
pub const SERVE_MARKER: &[u8] = b"swarmops-git-serve 1\n";

/// The one hook file, `<store>/git/serve-hooks/proc-receive`. git runs a hook as a file, so this
/// one is POSIX `sh`: it hands the hook to the binary that serves, which the parent named.
pub const HOOK_SCRIPT: &str = "#!/bin/sh
# git runs hooks as files: this one hands proc-receive to the swarmops that serves.
exec \"$SWARMOPS_SERVE_EXE\" serve --hook proc-receive
";

/// A store's serving repository.
#[derive(Debug)]
#[non_exhaustive]
pub struct ServeRepo {
    /// `<store>/git/serve.git`.
    git_dir: Utf8PathBuf,
    /// `<store>/git/serve-hooks`.
    hooks_dir: Utf8PathBuf,
}

/// The serving repository's lock, held until dropped: the proof a ref moves under it.
#[derive(Debug)]
#[non_exhaustive]
pub struct ServeLock {}

/// Why the serving repository failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ServeError {
    /// The part of `serve` this is has not been built yet.
    #[error("not built")]
    NotBuilt,
}

impl ServeRepo {
    /// The serving repository of the store at `store_root`, made when it is missing.
    pub fn open(store_root: &Utf8Path) -> Result<ServeRepo, ServeError> {
        let _ = store_root;
        Err(ServeError::NotBuilt)
    }

    /// `<store>/git/serve.git`.
    pub fn git_dir(&self) -> &Utf8Path {
        &self.git_dir
    }

    /// `<store>/git/serve-hooks`, its `core.hooksPath`.
    pub fn hooks_dir(&self) -> &Utf8Path {
        &self.hooks_dir
    }

    /// Every ref it holds, by its full name.
    pub fn refs(&self) -> Result<BTreeMap<String, GitOid>, ServeError> {
        Err(ServeError::NotBuilt)
    }

    /// Takes its lock, waiting at most `wait`.
    pub fn lock(&self, wait: Duration) -> Result<ServeLock, ServeError> {
        let _ = wait;
        Err(ServeError::NotBuilt)
    }

    /// Moves the ref `name` from `expect` to `new` by compare-and-swap (`None`: absent), under
    /// `lock`.
    pub fn set_ref(
        &self,
        lock: &ServeLock,
        name: &str,
        expect: Option<GitOid>,
        new: Option<GitOid>,
    ) -> Result<BranchUpdate, ServeError> {
        let _ = (lock, name, expect, new);
        Err(ServeError::NotBuilt)
    }
}
