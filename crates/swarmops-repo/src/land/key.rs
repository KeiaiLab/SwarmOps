//! A land's walk down its target's first parents (operations spec §3 step (c)): the replay
//! search of a keyed land, for the changeset its key landed, and who last touched each path a land
//! conflicts on (the M4 plan's Decision 6), from one walk that reads each changeset of the target
//! once per land, however many attempts continue it.
//!
//! A key the target's walk does not find is looked for on the other refs a land can publish to
//! (`main`, `release/<x>` and branches; the M15 design §6): the same walk from each head, to the
//! same floor, over the same kept changesets, so a chain shared with the target is read once.
//! A changeset it finds there that the target's head reaches is the target's own, below its walk's
//! floor, and is passed over.
//!
//! The walk starts at the head an attempt read, and ends at the first changeset at or below the
//! base's generation: the base itself, when it is on the target; for a base off the target (M11's
//! form), the changeset of the target's chain where a land of the change could have landed at the
//! earliest, since a land lands above its base. Generations fall strictly along first parents, so
//! the walk reads at most one changeset per generation between the two.

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap};

use swarmops_core::{ChangesetId, ObjectId, RepoPath, TreeId};
use swarmops_store::{ObjectStore, RefKind, StoreError, ref_kind};

use super::landing::stored;
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
            .filter_map(|hex| ObjectId::from_hex(hex).ok().map(ChangesetId::new))
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

/// What a keyed land's change is, which a changeset carrying its key must have squashed to be
/// this land made before.
#[derive(Debug, Clone, Copy)]
pub(super) enum Ours {
    /// The chain ending at this head: the found land's `squashed_from` ends there.
    Chain(ChangesetId),
    /// A change committed on `head` with tree `root` (a land with changes, the M16 design §4):
    /// the found land's `squashed_from` ends at a changeset whose first parent is `head` and whose
    /// tree is `root`, however late it was signed.
    Commit {
        /// The land's head.
        head: ChangesetId,
        /// `C`'s tree.
        root: TreeId,
    },
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

/// Whether `last`, the last changeset a land carrying the key squashed, is `ours`: the head
/// itself, or a changeset on the head with `C`'s tree, as `read` holds it.
fn squashed_ours<O: ObjectStore>(
    read: &RepoRead<'_, O>,
    last: ChangesetId,
    ours: Ours,
) -> Result<bool, RepoError> {
    match ours {
        Ours::Chain(head) => Ok(last == head),
        Ours::Commit { head, root } => {
            let Some(made) = read.changeset(last)? else {
                return Ok(false);
            };
            let on_head = made.changeset.parents.first() == Some(&head);
            Ok(on_head && made.changeset.root == root)
        }
    }
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
        visit: impl FnMut(ChangesetId, &Walked) -> bool,
    ) -> Result<(), RepoError> {
        self.walk_from(read, top.id, Some(top), visit)
    }

    /// [`Self::walk`] from changeset `start`, whose object is `known` when the caller has read it:
    /// otherwise it is read here, unless the walk kept it.
    fn walk_from<O: ObjectStore>(
        &mut self,
        read: &RepoRead<'_, O>,
        start: ChangesetId,
        known: Option<&StoredChangeset>,
        mut visit: impl FnMut(ChangesetId, &Walked) -> bool,
    ) -> Result<(), RepoError> {
        let mut at = Some(start);
        while let Some(id) = at {
            if let Entry::Vacant(vacant) = self.walked.entry(id) {
                let walked = match known {
                    Some(top) if id == top.id => Walked::of(top),
                    _ if id == start => Walked::of(&stored(read, id)?),
                    // A first parent of a changeset the snapshot holds: one it lacks is a
                    // damaged index.
                    _ => {
                        let stored = read.changeset(id)?.ok_or(StoreError::MissingParent(id))?;
                        Walked::of(&stored)
                    }
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
    /// its `squashed_from` ends as `ours` says, else a key reused.
    pub(super) fn find_key<O: ObjectStore>(
        &mut self,
        read: &RepoRead<'_, O>,
        top: &StoredChangeset,
        key: &str,
        ours: Ours,
    ) -> Result<Option<KeyFound>, RepoError> {
        // 1. The newest changeset carrying the key.
        let mut hit = None;
        self.walk(read, top, |id, walked| {
            if walked.land_key.as_deref() != Some(key.as_bytes()) {
                return true;
            }
            hit = Some((id, walked.parent, walked.squashed(), walked.paths.len()));
            false
        })?;
        let Some((id, parent, squashed_from, files_changed)) = hit else {
            return Ok(None);
        };

        // 2. This land made before, or another change.
        let (Some(&last), Some(parent)) = (squashed_from.last(), parent) else {
            return Ok(Some(KeyFound::Reused(id)));
        };
        if !squashed_ours(read, last, ours)? {
            return Ok(Some(KeyFound::Reused(id)));
        }
        Ok(Some(KeyFound::Replay(Replayed {
            id,
            parent,
            squashed_from,
            files_changed,
        })))
    }

    /// The newest changeset whose `land_key` is `key` on the first-parent chain of a ref other
    /// than `target` that a land publishes to, with that ref's name; `theirs` is the target's head.
    /// The refs are visited by name, each walked from its head to the floor; a changeset the
    /// target's head reaches is passed over, with the rest of its chain. `None`: no other ref
    /// holds the key above the floor.
    pub(super) fn find_elsewhere<O: ObjectStore>(
        &mut self,
        read: &RepoRead<'_, O>,
        target: &str,
        key: &str,
        theirs: &StoredChangeset,
    ) -> Result<Option<(String, ChangesetId)>, RepoError> {
        for (name, state) in read.list_refs()? {
            // 1. Not the target, walked already, nor a ref no land publishes to: a tag never
            // moves and a remote-tracking ref only a fetch moves. Any other kind is walked.
            let unlandable = matches!(
                ref_kind(&name),
                Some(RefKind::Tag | RefKind::RemoteTracking)
            );
            if name == target || unlandable {
                continue;
            }

            // 2. The ref's chain, down to the floor, for the key; a head the walk kept is not
            // read again.
            let mut hit = None;
            self.walk_from(read, state.target, None, |id, walked| {
                if walked.land_key.as_deref() != Some(key.as_bytes()) {
                    return true;
                }
                hit = Some(id);
                false
            })?;
            let Some(landed) = hit else {
                continue;
            };

            // 3. A changeset the target's head reaches is not another target's.
            if read.is_ancestor(landed, theirs.id)? {
                continue;
            }
            return Ok(Some((name, landed)));
        }
        Ok(None)
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
