//! Landing (operations spec §3, the store-only block): [`Repo::land`] merges a change, the
//! first-parent chain from a base to a head, three-way onto a ref's head, and publishes the result
//! as one new changeset, `L`, the change squashed, in one write transaction with the ref's move to
//! it.
//!
//! A land makes attempts, at most [`LandRequest::max_attempts`], each reading in a snapshot of
//! its own ([`Repo::snapshot`]):
//!
//! 1. Before the first, the request: the target is `main` or `release/<x>`
//!    ([`RepoError::RefName`]) and exists ([`RepoError::NotFound`]); a base it gives is a
//!    first-parent ancestor of the head ([`RepoError::BaseNotAncestor`]), on the target or not (a
//!    base off the target is M11's form); without one, the base is the merge base of the head and
//!    the target's head, along first parents ([`RepoError::NoMergeBase`] when they share none).
//! 2. An attempt reads the target's head, `theirs`. A head on the target's first-parent chain has
//!    nothing to land ([`NothingToLandReason::OnTarget`]). The chain from the base (exclusive) to
//!    the head, which `L` squashes, holds no changeset of two parents or more
//!    ([`RepoError::MergeChangeset`]).
//! 3. A keyed land walks the target's first parents from `theirs` down to the base, or, for a
//!    base off the target, down to the first changeset at or below the base's generation (a land of
//!    the change lands above its base), for a changeset whose `land_key` metadata is the key: one
//!    whose `squashed_from` ends at the head is the land, made before, and the land is a replay
//!    ([`LandStatus::Replayed`]); any other means the key landed another change
//!    ([`RepoError::KeyReused`]). The walk reads each changeset once per land, so a later attempt
//!    reads only what the target gained.
//! 4. It merges three-way (storage spec §4): the first attempt what the chain changed onto
//!    `theirs` ([`Repo::merge`] of the base, the head and `theirs`, over the paths the chain
//!    touches); a later one what it merged before, `L`, onto what the target gained since, over
//!    `L`'s delta paths alone ([`Repo::merge_trees`] of the head `L` was made on, `L` and the head
//!    now: the incremental re-merge), and, if that conflicts, the whole merge again, whose result
//!    stands. The request's resolutions apply to the merge before its conflicts are counted
//!    ([`Resolutions`]): each, while `theirs` holds its path as the resolutions' `against` does,
//!    replaces what the merge made at and below its path, conflicts included; a path `theirs`
//!    holds otherwise is stale. Conflicts left, or a stale path, end the land, recording nothing
//!    ([`RepoError::Conflict`], with who last touched each conflicting path on the target since
//!    the base, found on the same walk, and the stale paths). A merge that leaves `theirs`' tree
//!    as it is has nothing to land ([`NothingToLandReason::NoChanges`]).
//! 5. It writes `L` outside the lock: its one parent `theirs`, its delta the merge's, its message
//!    the request's or the head's, its author the head's, its committer the lander; its metadata
//!    the head's `agent` and `run_id`, the key as `land_key`, the lander's name as `lander`, the
//!    request's `lander_metadata`, and `squashed_from`, the chain's ids, oldest first, in hex, a
//!    space between each two. Its blobs, trees and object are written and flushed before anything
//!    names them (I1).
//! 6. It publishes `L` in one write transaction, one frame: `L`'s insert and the target's move
//!    from `theirs` to it, by compare-and-swap. A target that moved since is read again under the
//!    lock, and the lock holder makes the land anew on its head, `L'`, by the landing model M4
//!    measured (metalog merge, umbrella decision 10): when the head is at most 256 first parents
//!    above `theirs` and the paths it gained since do not overlap `L`'s, or the overlapping ones
//!    are regular files of at most 1 MiB that merge cleanly (diff3), `L'` is `L`'s delta (those
//!    files merged) on that head, written and flushed under the lock before the frame that names
//!    it, and it lands in the same attempt ([`LandOutcome::relocated`]). Otherwise the write is
//!    dropped, and the next attempt starts from what the target names, after a wait from the
//!    fourth attempt on ([`backoff`]). A failure after the commit point is a warning; a journal
//!    append that fails (`NotCommitted`), a lock wait that times out (`Busy`) and any other error
//!    end the land as they are.
//!
//! A land that finds its change on the target, landed by a land with its key or there in any
//! other way, decides so under the store's lock, as a keyed commit's replay is decided: it takes a
//! write transaction, whose tail check makes durable the frames its snapshot saw (or takes back
//! those a failed sync left), reads the target there, and drops the transaction without a frame.
//! A target that names something else then makes another attempt. So a land never answers that it
//! landed, or had nothing to land, on the strength of a frame a failed sync could still take back.
//!
//! When the last attempt lost its race, the land fails with [`RepoError::RetryExhausted`]. A lost
//! attempt leaves nothing in the journal: the objects it flushed stay, unreferenced, for GC.
//!
//! A land with [`LandRequest::expect_target`] (M5) makes one attempt. After step 3, a target that
//! does not name the expected changeset ends it with [`RepoError::CasMismatch`], before the
//! merge; so does a publish that loses its race, which never merges again and is never made
//! anew on the moved head, so that what lands is the tree its lander checked. Its replay check
//! runs first, so a land whose key landed its change replays it whatever the target names.
//!
//! Steps 1 to 4 are the land's preparation, which a rebase shares ([`crate::rebase`]): step 1
//! starts a land (`Repo::start`), and steps 2 to 4 prepare an attempt's merge
//! (`Started::prepare`, `Prepared`). A land writes and publishes what they prepared (steps 5
//! and 6); a rebase commits it.
//!
//! The detached land ([`Repo::land_detached`], operations spec §3, the detached form) is the
//! first attempt without the key walk and the publication: the same chain walk (step 2), merge
//! (step 4), conflict report and writer of `L` (step 5), then `L`'s insert alone, moving no ref.

mod detached;
mod key;
mod landing;
mod publish;
mod resolve;

pub use detached::{DetachedLand, DetachedOutcome};
use landing::{LAND_KEY, SQUASHED_FROM};
pub use landing::{
    LandOutcome, LandRequest, LandStatus, MergeConflicts, NothingToLandReason, backoff,
};
pub(crate) use landing::{Prep, Prepared};
pub use publish::{Publication, Published, Relocate};
pub use resolve::{Resolution, Resolutions, Side};

// The module documentation's links only.
#[cfg(doc)]
use crate::{error::RepoError, repo::Repo};
