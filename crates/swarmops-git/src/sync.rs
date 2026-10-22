//! The sync engine of the git remotes (M13): from `main` and a remote's head, the changes that
//! are unpublished, those the remote holds already, and the rest landed onto the remote's head
//! with no ref moved (umbrella spec §13, the M13 scope; operations spec §3).
//!
//! ```text
//!   main:   … ─ B ─ U1 ─ U2 ─ U3          B: the newest changeset on main's first-parent
//!                \                            chain that the remote's head reaches
//!   remote:       R1 ─ R2 (C_R)
//!
//!   plan       → B, U1..Un, how far behind
//!   recognise  → drop the Ui the remote holds (a trailer line, or a no-op merge)
//!   relocate   → land the rest onto C_R in order, one new changeset each, no ref moved
//! ```
//!
//! It lives here, beside the import, because it walks the import's ancestry
//! (`ancestry::reaches`). The operations layer takes the lock, fetches, moves `main` and pushes.
//!
//! The M13 plan's task C1 froze these items; task C8a writes their bodies, which until then
//! panic with the task that writes them.

use swarmops_core::{ChangesetId, Signature};
use swarmops_repo::{LocalRepo, MergeConflicts, RepoError};
use thiserror::Error;

/// The task that writes this module's bodies.
const PLANNED: &str = "planned in M13 (C8a)";

/// The most changesets a sync walks: `main`'s first parents, and the remote's history above the
/// base.
pub const MAX_SYNC_WALK: usize = 10_000;

/// Where `main` and the remote's head stand against each other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncPlan {
    /// The newest changeset on `main`'s first-parent chain that the remote's head reaches.
    pub base: ChangesetId,
    /// `main`'s first-parent chain above `base`, oldest first.
    pub unpublished: Vec<ChangesetId>,
    /// The length of the remote head's first-parent chain above `base`.
    pub behind: u64,
    /// Whether `behind` stopped at [`MAX_SYNC_WALK`].
    pub behind_truncated: bool,
}

/// Which unpublished changes the remote holds already, and which are left to land.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recognised {
    /// The changes the remote holds, and how that was found.
    pub dropped: Vec<(ChangesetId, DropReason)>,
    /// The changes left, oldest first.
    pub to_land: Vec<ChangesetId>,
}

/// How the remote was found to hold a change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    /// A commit the remote's head reaches names it in a trailer line.
    Trailer,
    /// It merges onto the remote's head as no change.
    NoOp,
}

/// What [`relocate`] landed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relocated {
    /// The last changeset landed, or the remote's head when none was.
    pub head: ChangesetId,
    /// Each change landed, and the changeset it was landed as.
    pub landed: Vec<(ChangesetId, ChangesetId)>,
    /// The changes that merged as no change, and were dropped.
    pub no_ops: Vec<ChangesetId>,
}

/// Why the sync engine stopped.
#[derive(Debug, Error)]
pub enum SyncError {
    /// `main` and the remote share no changeset within [`MAX_SYNC_WALK`].
    #[error("main and the remote's head share no changeset")]
    NoCommonBase,
    /// A change conflicts with what it lands onto.
    #[error("{change} conflicts with {onto}")]
    Conflict {
        /// The change.
        change: ChangesetId,
        /// What it was landing onto.
        onto: ChangesetId,
        /// The conflicts, as a land reports them.
        conflicts: Box<MergeConflicts>,
    },
    /// An unpublished change is a merge changeset, which the sync does not relocate.
    #[error("{0} is a merge changeset")]
    MergeChangeset(ChangesetId),
    /// The repository failed.
    #[error(transparent)]
    Repo(#[from] RepoError),
}

/// Where `main` (at `main`) and the remote's head (`remote_head`) stand.
pub fn plan(
    _repo: &LocalRepo,
    _main: ChangesetId,
    _remote_head: ChangesetId,
) -> Result<SyncPlan, SyncError> {
    unimplemented!("{PLANNED}")
}

/// Which of `plan`'s unpublished changes the remote at `remote_head` holds already.
pub fn recognise(
    _repo: &LocalRepo,
    _plan: &SyncPlan,
    _remote_head: ChangesetId,
) -> Result<Recognised, SyncError> {
    unimplemented!("{PLANNED}")
}

/// Lands `to_land` onto `onto` in order, each as one new changeset signed by `lander`, with no
/// ref moved.
pub fn relocate(
    _repo: &LocalRepo,
    _to_land: &[ChangesetId],
    _onto: ChangesetId,
    _lander: &Signature,
) -> Result<Relocated, SyncError> {
    unimplemented!("{PLANNED}")
}
