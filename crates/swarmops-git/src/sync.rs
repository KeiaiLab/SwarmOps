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
//! Both walks over every parent are pruned by generation, as the import's ancestry walk is
//! (`ancestry::reaches`), and shared across their questions: the base is the first of `main`'s
//! first parents, newest first, that the remote's head reaches, so one walk down the remote's
//! history answers them all, each step taken once (`Reach`).

use std::collections::{BinaryHeap, HashSet};

use swarmops_core::{Changeset, ChangesetId, ObjectId, Signature};
use swarmops_repo::land::{DetachedLand, DetachedOutcome};
use swarmops_repo::{LocalRepo, MergeConflicts, RepoError};
use thiserror::Error;

use crate::native::TRAILERS;

/// The trailer every native changeset's commit carries first: its changeset's id.
const CHANGESET_TRAILER: &str = "SwarmOps-Changeset";

/// The metadata key of a landed changeset's idempotency key, and of its squashed chain.
const LAND_KEY: &str = "land_key";
const SQUASHED_FROM: &str = "squashed_from";

/// The metadata key a relocated changeset names its change by (in hex). The export renders no
/// trailer for it ([`TRAILERS`]), so version 1 of the export oid is untouched.
const SYNCED_FROM: &str = "synced_from";

/// The list bullets a trailer line may start with, as GitHub's squash description writes them.
const BULLETS: [&[u8]; 2] = [b"* ", b"- "];

/// The most changesets a sync walks: `main`'s first parents, and the remote's history above the
/// base.
pub const MAX_SYNC_WALK: usize = 10_000;

/// [`MAX_SYNC_WALK`] as a count of changesets behind.
const MAX_BEHIND: u64 = MAX_SYNC_WALK as u64;

/// Where `main` and the remote's head stand against each other.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
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

impl SyncPlan {
    /// The plan of `unpublished` above `base`, the remote `behind` by that many (stopped at
    /// [`MAX_SYNC_WALK`] when `behind_truncated`).
    pub fn new(
        base: ChangesetId,
        unpublished: Vec<ChangesetId>,
        behind: u64,
        behind_truncated: bool,
    ) -> SyncPlan {
        SyncPlan {
            base,
            unpublished,
            behind,
            behind_truncated,
        }
    }
}

/// Which unpublished changes the remote holds already, and which are left to land.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Recognised {
    /// The changes the remote holds, and how that was found.
    pub dropped: Vec<(ChangesetId, DropReason)>,
    /// The changes left, oldest first.
    pub to_land: Vec<ChangesetId>,
}

/// How the remote was found to hold a change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DropReason {
    /// A commit the remote's head reaches names it in a trailer line.
    Trailer,
    /// It merges onto the remote's head as no change.
    NoOp,
}

/// What [`relocate`] landed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
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
#[non_exhaustive]
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
///
/// The base is the newest of `main`'s first parents, at most [`MAX_SYNC_WALK`] of them, that
/// `remote_head` reaches over every parent: a change a merge commit on the remote took through
/// its second parent is published. [`SyncError::NoCommonBase`] when none is.
pub fn plan(
    repo: &LocalRepo,
    main: ChangesetId,
    remote_head: ChangesetId,
) -> Result<SyncPlan, SyncError> {
    // 1. Down main's first parents, newest first, until the remote's head reaches one.
    let mut remote = Reach::new(repo, remote_head)?;
    let mut unpublished = Vec::new();
    let mut at = Some(main);
    let mut base = None;
    while let Some(id) = at {
        if unpublished.len() >= MAX_SYNC_WALK {
            break;
        }
        let meta = record(repo, id)?;
        if remote.reaches(id, meta.generation)? {
            base = Some(id);
            break;
        }
        unpublished.push(id);
        at = meta.parents.first().copied();
    }
    let base = base.ok_or(SyncError::NoCommonBase)?;
    unpublished.reverse();

    // 2. Down the remote head's first parents until the base reaches one: what main lacks.
    let mut published = Reach::new(repo, base)?;
    let mut behind: u64 = 0;
    let mut behind_truncated = false;
    let mut at = Some(remote_head);
    while let Some(id) = at {
        let meta = record(repo, id)?;
        if published.reaches(id, meta.generation)? {
            break;
        }
        if behind >= MAX_BEHIND {
            behind_truncated = true;
            break;
        }
        behind += 1;
        at = meta.parents.first().copied();
    }

    Ok(SyncPlan {
        base,
        unpublished,
        behind,
        behind_truncated,
    })
}

/// Which of `plan`'s unpublished changes the remote at `remote_head` holds already.
///
/// 1. By trailer: a change whose id, or whose `land_key`, a trailer line of a changeset the
///    remote's head reaches above the base names (`trailers`).
/// 2. By applying as a no-op: of those left, `V1..Vm`, the largest prefix `V1..Vk` whose merge
///    onto the remote's head (base `plan.base`, theirs `Vk`) leaves its tree as it is; the scan
///    goes from `k = m` down and stops at the first that holds, a conflict counting as not held.
///
/// The dropped changes are listed in `plan`'s order.
pub fn recognise(
    repo: &LocalRepo,
    plan: &SyncPlan,
    remote_head: ChangesetId,
) -> Result<Recognised, SyncError> {
    // 1. What the remote's trailers name, and the changes they hold.
    let named = trailers(repo, plan.base, remote_head)?;
    let mut by_trailer = HashSet::new();
    let mut left = Vec::new();
    for &id in &plan.unpublished {
        let cs = object(repo, id)?;
        let key = value(&cs, LAND_KEY);
        if named.ids.contains(&id) || key.is_some_and(|key| named.keys.contains(key)) {
            by_trailer.insert(id);
            continue;
        }
        left.push(id);
    }

    // 2. The largest prefix of what is left that merges onto the remote's head as no change.
    let mut held = 0;
    for k in (1..=left.len()).rev() {
        let merged = repo.merge(plan.base, left[k - 1], remote_head)?;
        if merged.conflicts.is_empty() && merged.delta.is_empty() {
            held = k;
            break;
        }
    }
    let by_no_op: HashSet<ChangesetId> = left[..held].iter().copied().collect();

    // 3. Each change dropped with its reason, in main's order; the rest to land.
    let dropped = (plan.unpublished.iter())
        .filter_map(|id| {
            if by_trailer.contains(id) {
                return Some((*id, DropReason::Trailer));
            }
            by_no_op.contains(id).then_some((*id, DropReason::NoOp))
        })
        .collect();
    Ok(Recognised {
        dropped,
        to_land: left.split_off(held),
    })
}

/// Lands `to_land` onto `onto` in order, each as one new changeset signed by `lander`, with no
/// ref moved.
///
/// Each change `Vi` lands as a detached land of `(parent(Vi), Vi]` onto the head so far
/// ([`swarmops_repo::Repo::land_detached`]): `Vi`'s author, message, `agent` and `run_id`, its
/// `land_key` and `squashed_from` copied (its own id when it has no chain), and `synced_from`,
/// its id. A change that merges as no change is listed in `no_ops`. A merge changeset among them
/// is refused before anything lands ([`SyncError::MergeChangeset`]); a conflict stops the
/// relocation ([`SyncError::Conflict`]) with what landed before it left unreferenced.
pub fn relocate(
    repo: &LocalRepo,
    to_land: &[ChangesetId],
    onto: ChangesetId,
    lander: &Signature,
) -> Result<Relocated, SyncError> {
    // 1. Every change read first: a merge changeset refuses the whole relocation.
    let mut changes = Vec::with_capacity(to_land.len());
    for &id in to_land {
        let cs = object(repo, id)?;
        let [parent] = cs.parents[..] else {
            return Err(SyncError::MergeChangeset(id));
        };
        changes.push((id, parent, cs));
    }

    // 2. Each lands on the one before, the first on `onto`.
    let mut relocated = Relocated {
        head: onto,
        landed: Vec::new(),
        no_ops: Vec::new(),
    };
    for (id, parent, cs) in changes {
        let land = DetachedLand {
            base: parent,
            head: id,
            onto: relocated.head,
            lander: lander.clone(),
            key: value(&cs, LAND_KEY).and_then(|key| String::from_utf8(key.to_vec()).ok()),
            squashed_from: value(&cs, SQUASHED_FROM).and_then(chain),
            message: None,
            extra_metadata: vec![(SYNCED_FROM.to_owned(), id.to_string().into_bytes())],
        };
        match repo.land_detached(land) {
            Ok(DetachedOutcome::Landed(made)) => {
                relocated.landed.push((id, made));
                relocated.head = made;
            }
            Ok(DetachedOutcome::NoChanges) => relocated.no_ops.push(id),
            Err(RepoError::Conflict(conflicts)) => {
                return Err(SyncError::Conflict {
                    change: id,
                    onto: relocated.head,
                    conflicts,
                });
            }
            Err(RepoError::MergeChangeset(at)) => return Err(SyncError::MergeChangeset(at)),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(relocated)
}

/// What the trailer lines of a set of commit messages name.
#[derive(Debug, Default)]
struct Named {
    /// The ids of `SwarmOps-Changeset` and `SwarmOps-Squashed-From` lines.
    ids: HashSet<ChangesetId>,
    /// The keys of `SwarmOps-Land-Key` lines.
    keys: HashSet<Vec<u8>>,
}

/// What the trailer lines of the changesets `remote_head` reaches above `base` name: every parent,
/// pruned at `base`'s generation, at most [`MAX_SYNC_WALK`] changesets read.
fn trailers(
    repo: &LocalRepo,
    base: ChangesetId,
    remote_head: ChangesetId,
) -> Result<Named, SyncError> {
    let floor = record(repo, base)?.generation;
    let mut named = Named::default();
    let mut stack = vec![remote_head];
    let mut seen = HashSet::new();
    while let Some(at) = stack.pop() {
        if at == base || !seen.insert(at) {
            continue;
        }
        if seen.len() > MAX_SYNC_WALK {
            break;
        }

        // A changeset at or below the base's generation is not above it.
        let stored = repo
            .changeset(at)?
            .ok_or_else(|| RepoError::NotFound(at.to_string()))?;
        if stored.generation <= floor {
            continue;
        }
        read_trailers(&stored.changeset.message, &mut named);
        stack.extend(stored.changeset.parents);
    }
    Ok(named)
}

/// Adds what the trailer lines of `message` name to `named`. A line counts with its leading
/// spaces and tabs ignored and then one list bullet stripped:
///
/// ```text
///   SwarmOps-Changeset: <id>            → ids
///   * SwarmOps-Squashed-From: <id> <id> → ids
///     - SwarmOps-Land-Key: key-1        → keys
/// ```
fn read_trailers(message: &[u8], named: &mut Named) {
    let squashed_token = token(SQUASHED_FROM);
    let key_token = token(LAND_KEY);
    for line in message.split(|&b| b == b'\n') {
        // 1. The indentation, then one bullet.
        let start = line
            .iter()
            .position(|&b| b != b' ' && b != b'\t')
            .unwrap_or(line.len());
        let mut line = &line[start..];
        if let Some(bullet) = BULLETS.iter().find(|bullet| line.starts_with(bullet)) {
            line = &line[bullet.len()..];
        }

        // 2. `<token>: <value>`, the value's surrounding white space dropped.
        let Some(colon) = line.iter().position(|&b| b == b':') else {
            continue;
        };
        let (name, value) = (&line[..colon], line[colon + 1..].trim_ascii());
        if name == CHANGESET_TRAILER.as_bytes() || name == squashed_token.as_bytes() {
            let ids = value.split(u8::is_ascii_whitespace).filter_map(parse_id);
            named.ids.extend(ids);
            continue;
        }
        if name == key_token.as_bytes() && !value.is_empty() {
            named.keys.insert(value.to_vec());
        }
    }
}

/// The trailer token the export renders metadata `key` as ([`TRAILERS`]).
fn token(key: &str) -> &'static str {
    TRAILERS
        .iter()
        .find(|(k, _)| *k == key)
        .map_or("", |(_, token)| *token)
}

/// A changeset id from its 64 hex digits; `None` for anything else.
fn parse_id(hex: &[u8]) -> Option<ChangesetId> {
    let hex = std::str::from_utf8(hex).ok()?;
    ObjectId::from_hex(hex).ok().map(ChangesetId)
}

/// A `squashed_from` value as ids, oldest first; `None` when one is not an id.
fn chain(value: &[u8]) -> Option<Vec<ChangesetId>> {
    (value.split(|&b| b == b' ')).map(parse_id).collect()
}

/// The value of metadata `key` of `cs`.
fn value<'a>(cs: &'a Changeset, key: &str) -> Option<&'a [u8]> {
    (cs.metadata.iter())
        .find(|(k, _)| k == key)
        .map(|(_, value)| value.as_slice())
}

/// Changeset `id`'s index record: [`RepoError::NotFound`] if the store does not hold it.
fn record(repo: &LocalRepo, id: ChangesetId) -> Result<swarmops_store::ChangesetMeta, SyncError> {
    let meta = repo.changeset_meta(id)?;
    Ok(meta.ok_or_else(|| RepoError::NotFound(id.to_string()))?)
}

/// Changeset `id` itself: [`RepoError::NotFound`] if the store does not hold it.
fn object(repo: &LocalRepo, id: ChangesetId) -> Result<Changeset, SyncError> {
    let stored = repo.changeset(id)?;
    let stored = stored.ok_or_else(|| RepoError::NotFound(id.to_string()))?;
    Ok(stored.changeset)
}

/// A walk down the ancestry of one changeset over every parent, highest generation first, that
/// answers "is `x` an ancestor (or the start)?" for questions asked in falling generation: before
/// a question about generation `g` it expands every changeset above `g`, so `x` is reached
/// exactly when the walk has seen it. Each changeset is expanded once over all the questions.
struct Reach<'a> {
    repo: &'a LocalRepo,
    /// The changesets seen but not expanded, by generation.
    frontier: BinaryHeap<(u64, ChangesetId)>,
    /// Every changeset seen.
    seen: HashSet<ChangesetId>,
}

impl<'a> Reach<'a> {
    /// A walk from `start`.
    fn new(repo: &'a LocalRepo, start: ChangesetId) -> Result<Reach<'a>, SyncError> {
        let generation = record(repo, start)?.generation;
        Ok(Reach {
            repo,
            frontier: BinaryHeap::from([(generation, start)]),
            seen: HashSet::from([start]),
        })
    }

    /// Whether the start reaches `id`, of generation `generation`, which is no higher than any
    /// asked before.
    fn reaches(&mut self, id: ChangesetId, generation: u64) -> Result<bool, SyncError> {
        while let Some(&(top, at)) = self.frontier.peek() {
            if top <= generation {
                break;
            }
            self.frontier.pop();

            // Parents are seen once each, with the generation their records hold.
            for parent in record(self.repo, at)?.parents {
                if !self.seen.insert(parent) {
                    continue;
                }
                let parent_generation = record(self.repo, parent)?.generation;
                self.frontier.push((parent_generation, parent));
            }
        }
        Ok(self.seen.contains(&id))
    }
}
