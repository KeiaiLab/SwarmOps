//! `swarmops serve --prepare` (the M17 design §4.3): `serve.git` made ready before a client
//! connects, so that the connection finds it current and exports nothing.
//!
//! ```text
//! 0 no memo key, main's head native? skipped (the export would walk the whole history)
//! 1 serve.lock: Zero ─ busy ─▶ git/serve.dirty marked, done (the holder runs again)
//!               Bounded ─ SERVE_LOCK_WAIT, then E_BUSY
//! 2 Seed::GitDir: the imported tip's objects fetched from it, kept as a pack, no ref written;
//!   a fetch that fails is a warning, and the refresh exports instead
//! 3 the refresh (gc --auto in it), serve.lock let go around its committed snapshot
//!   after a seed's first round: one full repack with a bitmap, and a commit-graph layer
//!   serve.lock let go at the round's end
//! 4 git/serve.dirty there? serve.lock again, zero wait (busy ─▶ marked, done), removed, 3
//!   again; SERVE_REFRESH_ATTEMPTS rounds in all
//! 5 objects written? one commit-graph layer, under serve.lock taken at once (busy: skipped,
//!   the next run's layer holds the commits)
//! ```
//!
//! The seed's export finds every lossless commit in the fetched pack and writes none. The full
//! repack waits for that export: before it no ref reaches the fetched objects, and `git repack
//! -a -d` drops what no ref reaches.
//!
//! A mark left after the last round, or made after the holder's last look, costs the next
//! connection one slow path, never a wrong advertisement: the connection's own freshness check
//! decides what it advertises. Between rounds, connections and pushes take `serve.lock`.

use std::fs::OpenOptions;
use std::io;
use std::time::Duration;

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

/// The warning of a seed whose objects could not be fetched.
const SEED_FAILED: &str = "seed_failed";

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

/// What [`claim`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Claim {
    /// `serve.lock` was free, and is again: a preparation may start.
    Free,
    /// `serve.lock` was held: `git/serve.dirty` is marked for its holder.
    Marked,
}

/// Tries `serve.lock` of `repo` once, without waiting: held, the mark is left for its holder
/// ([`Claim::Marked`]); free, it is let go again ([`Claim::Free`]).
pub fn claim(repo: &ServeRepo) -> Result<Claim, OpError> {
    match repo.lock(Duration::ZERO) {
        Ok(lock) => {
            drop(lock);
            Ok(Claim::Free)
        }
        Err(ServeError::Busy) => {
            mark_dirty(repo).map_err(|e| io_error(DIRTY_FILE, &e))?;
            Ok(Claim::Marked)
        }
        Err(e) => Err(serve_error(e)),
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
    let held = match lock(repo, wait) {
        Ok(held) => held,
        Err(ServeError::Busy) if wait == LockWait::Zero => {
            mark_dirty(repo).map_err(|e| io_error(DIRTY_FILE, &e))?;
            return Ok(prepared);
        }
        Err(e) => return Err(serve_error(e)),
    };

    // 2. The seed's objects in serve.git, as a pack: the export finds every lossless commit
    //    packed and writes none. A fetch that failed leaves the export to write them.
    if let (Seed::GitDir(source), Some(head)) = (seed, main)
        && let Some(tip) = imported_tip(&read, head)?
    {
        let fetched = GitCli::find().and_then(|git| {
            git.fetch_objects(repo.git_dir().as_std_path(), source.as_std_path(), tip)
        });
        match fetched {
            Ok(()) => prepared.seeded = 1,
            Err(e) => {
                let failed = format!("{SEED_FAILED}: {}", git_error(e).message);
                prepared.refreshed.warnings.push(failed);
            }
        }
    }
    drop(read);

    // 3-4. Rounds while a mark says another run found the lock held, each with the lock of its
    //      own: round 1's, then one taken at once; after a seed's first, the full repack.
    let mut first = Some(held);
    loop {
        let held = match first.take() {
            Some(held) => held,
            None => match lock(repo, LockWait::Zero) {
                Ok(held) => held,
                Err(ServeError::Busy) => {
                    mark_dirty(repo).map_err(|e| io_error(DIRTY_FILE, &e))?;
                    break;
                }
                Err(e) => return Err(serve_error(e)),
            },
        };
        prepared.rounds += 1;
        let _ = clear_dirty(repo);
        let (held, mut done) = refresh_locked(ops, repo, held, &Scope::All)?;
        if prepared.rounds == 1 && prepared.seeded > 0 {
            done.warnings.extend(repack(repo));
        }
        drop(held);
        prepared.refreshed.moved += done.moved;
        prepared.refreshed.removed += done.removed;
        prepared.refreshed.objects += done.objects;
        prepared.refreshed.warnings.extend(done.warnings);
        if prepared.rounds >= SERVE_REFRESH_ATTEMPTS || !dirty(repo) {
            break;
        }
    }

    // 5. One commit-graph layer for every round's objects.
    if prepared.refreshed.objects > 0 {
        prepared.refreshed.warnings.extend(graph(repo));
    }
    Ok(prepared)
}

/// Step 5: a commit-graph layer under `serve.lock` taken at once; held, none (the next run's
/// layer holds the commits). What failed, as a warning.
fn graph(repo: &ServeRepo) -> Option<String> {
    let held = lock(repo, LockWait::Zero).ok()?;
    let graph = GitCli::find().and_then(|git| git.write_commit_graph(repo.git_dir().as_std_path()));
    drop(held);
    let e = graph.err()?;
    Some(format!("commit_graph_failed: {}", git_error(e).message))
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
        LockWait::Zero => repo.lock(Duration::ZERO),
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use camino::Utf8PathBuf;

    use super::{Claim, claim, dirty, serve_repo};
    use crate::LocalOps;

    /// `claim` finds a free `serve.lock` free and leaves it so, with no mark; a held one it
    /// marks for its holder.
    #[test]
    fn claim_marks_a_held_lock() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).expect("UTF-8");
        LocalOps::init_as(&root, "acme", Some(2)).expect("init");
        let repo = serve_repo(&root).expect("open serve.git");

        // 1. Free: no mark, and the lock free again.
        assert_eq!(claim(&repo).expect("claim"), Claim::Free);
        assert!(!dirty(&repo));
        let held = repo.lock(Duration::ZERO).expect("serve.lock free again");

        // 2. Held: the mark.
        assert_eq!(claim(&repo).expect("claim"), Claim::Marked);
        assert!(dirty(&repo));
        drop(held);
    }
}
