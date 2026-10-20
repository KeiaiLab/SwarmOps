//! A land's walk down its target's first parents (operations spec §3 step (c)): the replay
//! search of a keyed land, for the changeset its key landed, and who last touched each path a land
//! conflicts on (the M4 plan's Decision 6), from one walk that reads each changeset of the target
//! once per land, however many attempts continue it.
//!
//! The walk starts at the head an attempt read, and ends at the first changeset at or below the
//! base's generation: the base itself, when it is on the target; for a base off the target (M11's
//! form), the changeset of the target's chain where a land of the change could have landed at the
//! earliest, since a land lands above its base. Generations fall strictly along first parents, so
//! the walk reads at most one changeset per generation between the two.

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap};

use swarmops_core::{ChangesetId, ObjectId, RepoPath};
use swarmops_store::{ObjectStore, StoreError};

use super::{LAND_KEY, SQUASHED_FROM};
use crate::error::RepoError;
use crate::read::{RepoRead, StoredChangeset};

/// The metadata key of who made a change: what a conflict report names for the changeset that
/// last touched a path.
const AGENT: &str = "agent";

/// What a land's walk keeps of each changeset of the target's chain it read.
struct Walked {
    /// Its first parent; `None` for a root.
    parent: Option<ChangesetId>,
    /// Its generation.
    generation: u64,
    /// Its `agent` metadata, lossily UTF-8; empty when it names none.
    agent: String,
    /// The paths its delta changes.
    paths: Vec<RepoPath>,
    /// Its `land_key` metadata, if it has one.
    land_key: Option<Vec<u8>>,
    /// Its `squashed_from` metadata, if it has one.
    squashed_from: Option<Vec<u8>>,
}

impl Walked {
    /// What the walk keeps of `stored`.
    fn of(stored: &StoredChangeset) -> Walked {
        let changeset = &stored.changeset;
        let value = |key: &str| {
            (changeset.metadata.iter())
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.clone())
        };
        Walked {
            parent: changeset.parents.first().copied(),
            generation: stored.generation,
            agent: value(AGENT).map_or_else(String::new, |agent| {
                String::from_utf8_lossy(&agent).into_owned()
            }),
            paths: (changeset.delta.iter())
                .map(|entry| entry.path.clone())
                .collect(),
            land_key: value(LAND_KEY),
            squashed_from: value(SQUASHED_FROM),
        }
    }

    /// The changesets its `squashed_from` lists, in its order: ids in hex, a space between each
    /// two. Empty when it has none; a word that is no id is passed over.
    fn squashed(&self) -> Vec<ChangesetId> {
        let listed = self.squashed_from.as_deref().unwrap_or_default();
        (String::from_utf8_lossy(listed).split(' '))
            .filter_map(|hex| ObjectId::from_hex(hex).ok().map(ChangesetId))
            .collect()
    }

    /// Whether it touches `path`: its delta changes the path, a path below it, or one above it.
    fn touches(&self, path: &RepoPath) -> bool {
        let below = |a: &RepoPath, b: &RepoPath| {
            (a.as_str().strip_prefix(b.as_str())).is_some_and(|rest| rest.starts_with('/'))
        };
        (self.paths.iter())
            .any(|changed| changed == path || below(changed, path) || below(path, changed))
    }
}

/// A changeset a keyed land's walk found carrying its key.
pub(super) enum KeyFound {
    /// It was squashed from the land's head: the land, made before.
    Replay(Replayed),
    /// It was squashed from another head: the key landed another change.
    Reused(ChangesetId),
}

/// The land a replay found.
pub(super) struct Replayed {
    /// The changeset.
    pub(super) id: ChangesetId,
    /// Its parent: the head it landed on.
    pub(super) parent: ChangesetId,
    /// The chain it records it squashed, oldest first.
    pub(super) squashed_from: Vec<ChangesetId>,
    /// How many paths it changes.
    pub(super) files_changed: usize,
}

/// A land's walk down its target's first parents, which each attempt continues from the head it
/// read: the changesets read so far, kept, so that each is read once per land.
pub(super) struct TargetWalk {
    /// The base's generation: the walk ends at the first changeset at or below it.
    floor: u64,
    /// The changesets read so far.
    walked: HashMap<ChangesetId, Walked>,
}

impl TargetWalk {
    /// A walk that ends at the first changeset at or below generation `floor`, the base's.
    pub(super) fn new(floor: u64) -> TargetWalk {
        TargetWalk {
            floor,
            walked: HashMap::new(),
        }
    }

    /// Visits the target's first-parent chain from `top`, the head an attempt read (whose object
    /// it has read already), down to the floor, newest first, each changeset with what the walk
    /// keeps of it; it reads those no attempt read before. `visit` returns whether to go on.
    fn walk<O: ObjectStore>(
        &mut self,
        read: &RepoRead<'_, O>,
        top: &StoredChangeset,
        mut visit: impl FnMut(ChangesetId, &Walked) -> bool,
    ) -> Result<(), RepoError> {
        let mut at = Some(top.id);
        while let Some(id) = at {
            if let Entry::Vacant(vacant) = self.walked.entry(id) {
                let walked = if id == top.id {
                    Walked::of(top)
                } else {
                    // A first parent of a changeset the snapshot holds: one it lacks is a
                    // damaged index.
                    let stored = read.changeset(id)?.ok_or(StoreError::MissingParent(id))?;
                    Walked::of(&stored)
                };
                vacant.insert(walked);
            }
            let walked = &self.walked[&id];
            if !visit(id, walked) || walked.generation <= self.floor {
                return Ok(());
            }
            at = walked.parent;
        }
        Ok(())
    }

    /// The newest changeset on the walk from `top` whose `land_key` is `key`, if any: a replay if
    /// its `squashed_from` ends at `head`, else a key reused.
    pub(super) fn find_key<O: ObjectStore>(
        &mut self,
        read: &RepoRead<'_, O>,
        top: &StoredChangeset,
        key: &str,
        head: ChangesetId,
    ) -> Result<Option<KeyFound>, RepoError> {
        let mut found = None;
        self.walk(read, top, |id, walked| {
            if walked.land_key.as_deref() != Some(key.as_bytes()) {
                return true;
            }
            let squashed_from = walked.squashed();
            found = Some(match (squashed_from.last(), walked.parent) {
                (Some(&last), Some(parent)) if last == head => KeyFound::Replay(Replayed {
                    id,
                    parent,
                    squashed_from,
                    files_changed: walked.paths.len(),
                }),
                _ => KeyFound::Reused(id),
            });
            false
        })?;
        Ok(found)
    }

    /// For each of `paths`, the newest changeset on the walk from `top`, above the floor (since
    /// the base), that touches it, with its agent. A path none of them touches is left out.
    pub(super) fn last_touched<O: ObjectStore>(
        &mut self,
        read: &RepoRead<'_, O>,
        top: &StoredChangeset,
        paths: &[RepoPath],
    ) -> Result<BTreeMap<RepoPath, (ChangesetId, String)>, RepoError> {
        let floor = self.floor;
        let mut touched = BTreeMap::new();
        self.walk(read, top, |id, walked| {
            if walked.generation > floor {
                for path in paths {
                    if !touched.contains_key(path) && walked.touches(path) {
                        touched.insert(path.clone(), (id, walked.agent.clone()));
                    }
                }
            }
            touched.len() < paths.len()
        })?;
        Ok(touched)
    }
}
