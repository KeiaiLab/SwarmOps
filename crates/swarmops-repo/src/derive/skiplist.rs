//! The first-parent skip list, the first built-in derivation (storage spec §5): for each
//! changeset, its first-parent ancestors 1, 2, 4, … steps back, so that a walk to the ancestor
//! any number of first-parent steps back can take one jump per bit of that number, not a step per
//! changeset.

use std::ops::Deref;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use swarmops_core::{ChangesetId, ObjectId};

use super::{Derivation, DeriveCtx, DeriveError, Subject};

/// The derivation of each changeset's [`SkipList`], built into every repository
/// ([`super::Registry::builtins`]): `first_parent_skip_list`, version 1, keyed by `()`, and eager,
/// so that `swarmops derive` derives it for each new changeset.
///
/// Its one dependency is the first parent's list. `derive` builds the list entry by entry: the
/// ancestor `2^i` steps back is the one `2^(i-1)` steps back from the ancestor `2^(i-1)` steps
/// back, that is entry `i - 1` of the list of entry `i - 1`. The engine's depth-first order has
/// derived that list by then; one the store has lost since is `Missing`, and derived on the way.
#[derive(Debug, Clone, Copy, Default)]
#[non_exhaustive]
pub struct FirstParentSkipList;

impl Derivation for FirstParentSkipList {
    const NAME: &'static str = "first_parent_skip_list";
    const VERSION: u32 = 1;
    const EAGER: bool = true;
    type Key = ();
    type Value = SkipList;

    fn deps(&self, subject: Subject<'_>, (): &()) -> Vec<(ChangesetId, ())> {
        let first = subject.changeset.parents.first();
        first.map(|&parent| (parent, ())).into_iter().collect()
    }

    fn derive(
        &self,
        ctx: &DeriveCtx<'_>,
        subject: Subject<'_>,
        (): &(),
    ) -> Result<SkipList, DeriveError> {
        let Some(&parent) = subject.changeset.parents.first() else {
            return Ok(SkipList(Vec::new()));
        };
        let mut ancestors = vec![parent];
        loop {
            // Entry `i` is the furthest so far, `2^i` steps back: its own entry `i` is twice as far.
            let i = ancestors.len() - 1;
            let further = ctx.value::<FirstParentSkipList>(ancestors[i], &())?;
            match further.get(i) {
                Some(&next) => ancestors.push(next),
                None => return Ok(SkipList(ancestors)),
            }
        }
    }
}

/// A changeset's first-parent ancestors at 1, 2, 4, … steps back, the value of
/// [`FirstParentSkipList`]: entry `i` is the ancestor `2^i` first-parent steps back, for every `i`
/// that has one: a root's list is empty, and a changeset `d` first-parent steps from the root of
/// its first-parent chain has `⌊log2 d⌋ + 1` entries. It reads as the slice of those ids, nearest
/// first.
///
/// Stored, as postcard encodes it, as the number of ids, then each id's 32 bytes in order: what
/// version 1 of [`FirstParentSkipList`] reads back, so any other layout takes a new version.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct SkipList(Vec<ChangesetId>);

impl SkipList {
    /// The ancestors, nearest first.
    pub fn into_vec(self) -> Vec<ChangesetId> {
        self.0
    }
}

/// The ancestors, nearest first: entry `i` is the one `2^i` first-parent steps back.
impl Deref for SkipList {
    type Target = [ChangesetId];

    fn deref(&self) -> &[ChangesetId] {
        &self.0
    }
}

/// A sequence of each id's 32 bytes, nearest first.
impl Serialize for SkipList {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.0.iter().map(|id| id.0.0))
    }
}

/// The ids of a sequence of 32-byte arrays, as [`SkipList`]'s `Serialize` writes them.
impl<'de> Deserialize<'de> for SkipList {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let ids = Vec::<[u8; 32]>::deserialize(deserializer)?;
        let ids = ids.into_iter().map(|id| ChangesetId(ObjectId(id)));
        Ok(SkipList(ids.collect()))
    }
}
