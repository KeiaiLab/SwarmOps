//! `swarmops serve --prepare` (the M17 design §4.3): `serve.git` made ready before a client
//! connects, so that the connection finds it current and exports nothing.
//!
//! ```text
//! 0 no memo key, main's head native? skipped (the export would walk the whole history)
//! 1 serve.lock: Zero ─ busy ─▶ git/serve.dirty marked, done (the holder runs again)
//!               Bounded ─ SERVE_LOCK_WAIT, then E_BUSY
//! 2 Seed::GitDir: the imported tip's objects fetched from it, kept as a pack, no ref written
//! 3 the refresh (freshness first)  ─▶ 4 objects written? commit-graph layer (gc --auto in 3)
//!   after a seed's first round: one full repack with a bitmap, and a commit-graph layer
//! 5 git/serve.dirty there? removed, 3-4 again; SERVE_REFRESH_ATTEMPTS rounds in all
//! ```
//!
//! The seed's export finds every lossless commit in the fetched pack and writes none. The full
//! repack waits for that export: before it no ref reaches the fetched objects, and `git repack
//! -a -d` drops what no ref reaches.
//!
//! A mark left after the last round, or made after the holder's last look, costs the next
//! connection one slow path, never a wrong advertisement: the connection's own freshness check
//! decides what it advertises.

use std::fs::OpenOptions;
use std::io;

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_api::OpError;
use swarmops_core::ChangesetId;
use swarmops_git::serve::repo::{ServeError, ServeLock, ServeRepo};
use swarmops_git::transport::{GitCli, TransportError};
use swarmops_git::{GIT_OID, GitOid};
use swarmops_repo::RepoRead;
use swarmops_store::FsObjectStore;

use super::refresh::{
    Refreshed, SERVE_LOCK_WAIT, SERVE_REFRESH_ATTEMPTS, Scope, refresh_locked, serve_error,
};
use crate::LocalOps;
use crate::error;

/// The mark a preparation that found `serve.lock` held leaves for its holder, in `git/`.
const DIRTY_FILE: &str = "serve.dirty";

/// The branch whose head decides whether a preparation without the memo key exports, and whose
/// imported tip a seed fetches.
const MAIN: &str = "main";

/// The name the texts give the serving repository: never its host path.
const SERVE_GIT: &str = "serve.git";

/// Where a preparation takes the history's objects from before it exports.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Seed {
    /// Nowhere: the refresh exports what `serve.git` lacks.
    None,
    /// The git repository an import read (`git import`'s source): the objects of `main`'s
    /// imported tip are fetched from it.
    GitDir(Utf8PathBuf),
}

/// How long a preparation waits for `serve.lock`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LockWait {
    /// Not at all: a held lock is marked dirty for its holder (the run after an operation).
    Zero,
    /// The serve lock wait (10 s, the store's writer lock wait), then `E_BUSY` (an operator's
    /// run).
    Bounded,
}

/// What a preparation did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Prepared {
    /// What its refreshes did, summed.
    pub refreshed: Refreshed,
    /// How many refreshes it ran: 0 when it found the lock held or was skipped.
    pub rounds: u32,
    /// How many tips it fetched from its seed: 0 or 1.
    pub seeded: u64,
    /// Why it exported nothing, when it did not try.
    pub skipped: Option<String>,
}

impl Prepared {
    /// A preparation that did nothing yet.
    pub fn new() -> Prepared {
        Prepared::default()
    }
}

/// The serving repository of the store at `root`, made when it is missing
/// ([`ServeRepo::open`]), its failures as the operations spec's errors.
pub fn serve_repo(root: &Utf8Path) -> Result<ServeRepo, OpError> {
    ServeRepo::open(root).map_err(serve_error)
}

/// Prepares `repo`, the serving repository of the store `ops` opened: steps 0 to 5 of [the
/// module documentation](self), seeded from `seed`, waiting for `serve.lock` as `wait` says.
pub fn prepare(
    ops: &LocalOps,
    repo: &ServeRepo,
    seed: &Seed,
    wait: LockWait,
) -> Result<Prepared, OpError> {
    let mut prepared = Prepared::new();

    // 0. Without the memo key a native head's export walks the whole history: left to the
    //    next connection, as before.
    let read = ops.repo.snapshot().map_err(error::op)?;
    let main = read
        .get_ref(MAIN)
        .map_err(error::op)?
        .map(|state| state.target);
    if ops.memo_key.is_none()
        && let Some(head) = main
        && imported_oid(&read, head)?.is_none()
    {
        prepared.skipped = Some(
            "no memo key, and main's head is no imported commit: its export would walk the \
             whole history, left to the next connection"
                .to_owned(),
        );
        return Ok(prepared);
    }

    // 1. serve.lock, or the mark for its holder.
    let lock = match lock(repo, wait) {
        Ok(lock) => lock,
        Err(ServeError::Busy) if wait == LockWait::Zero => {
            mark_dirty(repo).map_err(|e| io_error(DIRTY_FILE, &e))?;
            return Ok(prepared);
        }
        Err(e) => return Err(serve_error(e)),
    };

    // 2. The seed's objects in serve.git, as a pack: the export finds every lossless commit
    //    packed and writes none.
    if let (Seed::GitDir(source), Some(head)) = (seed, main)
        && let Some(tip) = imported_tip(&read, head)?
    {
        let git = GitCli::find().map_err(git_error)?;
        git.fetch_objects(repo.git_dir().as_std_path(), source.as_std_path(), tip)
            .map_err(git_error)?;
        prepared.seeded = 1;
    }
    drop(read);

    // 3-5. Rounds while a mark says another run found the lock held; after a seed's first,
    //      the full repack.
    loop {
        prepared.rounds += 1;
        let _ = clear_dirty(repo);
        let mut done = round(ops, repo, &lock)?;
        if prepared.rounds == 1 && prepared.seeded > 0 {
            done.warnings.extend(repack(repo));
        }
        prepared.refreshed.moved += done.moved;
        prepared.refreshed.removed += done.removed;
        prepared.refreshed.objects += done.objects;
        prepared.refreshed.warnings.extend(done.warnings);
        if prepared.rounds >= SERVE_REFRESH_ATTEMPTS || !dirty(repo) {
            return Ok(prepared);
        }
    }
}

/// Steps 3 and 4: the refresh, then a commit-graph layer once it wrote objects (a failure is a
/// warning).
fn round(ops: &LocalOps, repo: &ServeRepo, lock: &ServeLock) -> Result<Refreshed, OpError> {
    let mut done = refresh_locked(ops, repo, lock, &Scope::All)?;
    if done.objects == 0 {
        return Ok(done);
    }
    let graph = GitCli::find().and_then(|git| git.write_commit_graph(repo.git_dir().as_std_path()));
    if let Err(e) = graph {
        done.warnings
            .push(format!("commit_graph_failed: {}", git_error(e).message));
    }
    Ok(done)
}

/// One full repack with a bitmap, then a commit-graph layer, once the seed's export moved the
/// refs onto the fetched objects: what failed, as warnings (the refs are right without them).
fn repack(repo: &ServeRepo) -> Vec<String> {
    let git_dir = repo.git_dir().as_std_path();
    let packed = GitCli::find().and_then(|git| {
        git.repack_full(git_dir)?;
        git.write_commit_graph(git_dir)
    });
    match packed {
        Ok(()) => Vec::new(),
        Err(e) => vec![format!("repack_failed: {}", git_error(e).message)],
    }
}

/// `serve.lock`, waiting as `wait` says.
fn lock(repo: &ServeRepo, wait: LockWait) -> Result<ServeLock, ServeError> {
    match wait {
        LockWait::Zero => repo.lock(std::time::Duration::ZERO),
        LockWait::Bounded => repo.lock(SERVE_LOCK_WAIT),
    }
}

/// The commit changeset `id` was imported from, if it was imported from git.
fn imported_oid(
    read: &RepoRead<'_, FsObjectStore>,
    id: ChangesetId,
) -> Result<Option<GitOid>, OpError> {
    let Some(stored) = read.changeset(id).map_err(error::op)? else {
        return Ok(None);
    };
    let metadata = stored.changeset.metadata.iter();
    let value = metadata
        .filter(|(key, _)| key == GIT_OID)
        .map(|(_, value)| value.as_slice())
        .next();
    Ok(value.and_then(GitOid::from_hex))
}

/// The newest imported commit along `head`'s first parents: `head`'s own when it was imported.
fn imported_tip(
    read: &RepoRead<'_, FsObjectStore>,
    head: ChangesetId,
) -> Result<Option<GitOid>, OpError> {
    for step in read.first_parents(head) {
        let id = step.map_err(error::op)?;
        if let Some(oid) = imported_oid(read, id)? {
            return Ok(Some(oid));
        }
    }
    Ok(None)
}

/// `git/serve.dirty`.
fn dirty_file(repo: &ServeRepo) -> Utf8PathBuf {
    let git_dir = repo.git_dir();
    git_dir.parent().unwrap_or(git_dir).join(DIRTY_FILE)
}

/// Leaves the mark, unsynced: one already there is enough.
fn mark_dirty(repo: &ServeRepo) -> io::Result<()> {
    let made = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dirty_file(repo).as_std_path());
    match made {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => Err(e),
        _ => Ok(()),
    }
}

/// Whether the mark is there.
fn dirty(repo: &ServeRepo) -> bool {
    dirty_file(repo).as_std_path().exists()
}

/// Removes the mark, if it is there.
fn clear_dirty(repo: &ServeRepo) -> io::Result<()> {
    std::fs::remove_file(dirty_file(repo).as_std_path())
}

/// The error of a file of `git/` that could not be written.
fn io_error(what: &str, e: &io::Error) -> OpError {
    serve_error(ServeError::Io {
        what: what.to_owned(),
        reason: e.kind().to_string(),
    })
}

/// The error of a git that failed on `serve.git`.
fn git_error(e: TransportError) -> OpError {
    error::transport_error(e, SERVE_GIT, error::Phase::Fetch)
}
