//! The derived values a deep [`Repo::verify`] checks (storage spec §5): every value the derived
//! store holds of the two built-in derivations, [`FirstParentSkipList`] and [`LastModified`], at
//! the versions this build reads, against what the changesets' objects give. A value file someone
//! placed in a store, whole and with its trailer right, is taken as valid where a reader's own
//! checks cannot tell it from the value its derivation gives; this pass finds every such value.
//!
//! The values of `last_modified` are read before the changesets are checked, for the paths they
//! are about. The changesets' check then notes each changeset whose object decodes, its first
//! parent, and which of those paths its delta touches. After it, the first-parent forest is laid
//! out (each changeset's distance from the root of its first-parent chain, and a span of the
//! changesets above it on their first-parent chains), and each value is checked against it:
//!
//! - a skip list has one entry for each `i` with `2^i` at most the changeset's distance from its
//!   root, and entry `i` is the changeset `2^i` first-parent steps below it;
//! - a last touch names the nearest changeset of the first-parent chain, the changeset itself
//!   included, whose delta touches the path, or none where none does.
//!
//! A value about a changeset verify does not know whole is not checked: one committed after the
//! journal was read, or whose object, or one on its first-parent chain, is missing or damaged
//! (which verify reports on its own). Neither is a file that is not whole (torn by a power loss,
//! which the cache is not synced against, or damaged): a read of its slot removes it and derives
//! the value again. A key that is no path names a slot no reader asks for, and is passed over.
//! Values of other derivations or versions, and the oids the git export memoizes, are not read.

use std::collections::HashMap;

use serde::de::DeserializeOwned;
use swarmops_core::{Changeset, ChangesetId};
use swarmops_store::{ListedValue, ObjectStore, StoredValue};

use super::VerifyProblem;
use crate::derive::{Derivation, FirstParentSkipList, LastModified, LastTouch, PathKey, SkipList};
use crate::error::RepoError;
use crate::repo::Repo;

/// What the derived-value pass has read and noted: see [the module documentation](self).
pub(super) struct Derived {
    /// The files of the skip lists, by name: read after the changesets are checked.
    skip_lists: Vec<ListedValue>,
    /// The values of `last_modified`, read before the changesets are checked.
    touches: Vec<Touch>,
    /// The paths the values of `last_modified` are about, each by its place in `touched_by`.
    paths: HashMap<String, usize>,
    /// For each path of `paths`, the changesets whose delta touches it, by their place in
    /// `nodes`, in seq order.
    touched_by: Vec<Vec<usize>>,
    /// Each changeset checked whose object decodes, in seq order: its id, and its first parent.
    nodes: Vec<(ChangesetId, Option<ChangesetId>)>,
    /// Where each changeset of `nodes` is in it.
    at: HashMap<ChangesetId, usize>,
}

/// A value of `last_modified`, as its file holds it.
struct Touch {
    /// The changeset it is about.
    subject: ChangesetId,
    /// Its key's bytes, a path's.
    key: Vec<u8>,
    /// Its path, by its place in [`Derived::touched_by`].
    path: usize,
    /// The value; `None` if its bytes do not decode as one.
    value: Option<LastTouch>,
}

impl Derived {
    /// Lists the values the derived store of `repo` holds of the two built-in derivations, and
    /// reads those of `last_modified`; `None` if it holds none.
    pub(super) fn read<O: ObjectStore>(repo: &Repo<O>) -> Result<Option<Derived>, RepoError> {
        let store = &repo.derived;
        let skip_lists = store.list(FirstParentSkipList::NAME, FirstParentSkipList::VERSION)?;
        let (name, version) = (LastModified::NAME, LastModified::VERSION);
        let mut touches = Vec::new();
        let mut paths: HashMap<String, usize> = HashMap::new();
        for listed in store.list(name, version)? {
            let Some(StoredValue { key, value }) = store.read_listed(name, version, &listed)?
            else {
                continue;
            };
            let Some(PathKey(path)) = decoded::<PathKey>(&key) else {
                continue;
            };
            let next = paths.len();
            let path = *paths.entry(path.as_str().to_owned()).or_insert(next);
            touches.push(Touch {
                subject: ChangesetId(listed.subject),
                key,
                path,
                value: decoded(&value),
            });
        }
        if skip_lists.is_empty() && touches.is_empty() {
            return Ok(None);
        }
        Ok(Some(Derived {
            skip_lists,
            touches,
            touched_by: vec![Vec::new(); paths.len()],
            paths,
            nodes: Vec::new(),
            at: HashMap::new(),
        }))
    }

    /// Notes changeset `id`, checked in seq order, whose object decodes as `changeset`: its first
    /// parent, and the paths of the values of `last_modified` that its delta touches (a path
    /// where its delta has an entry, or above one).
    pub(super) fn note(&mut self, id: ChangesetId, changeset: &Changeset) {
        let node = self.nodes.len();
        self.nodes.push((id, changeset.parents.first().copied()));
        self.at.insert(id, node);
        if self.paths.is_empty() {
            return;
        }
        let mut touched: Vec<usize> = Vec::new();
        for entry in &changeset.delta {
            let mut at = entry.path.as_str();
            loop {
                if let Some(&path) = self.paths.get(at) {
                    touched.push(path);
                }
                match at.rfind('/') {
                    Some(slash) => at = &at[..slash],
                    None => break,
                }
            }
        }
        touched.sort_unstable();
        touched.dedup();
        for path in touched {
            self.touched_by[path].push(node);
        }
    }

    /// Checks every value read or listed against the changesets noted, reading the skip lists
    /// one at a time from the derived store of `repo`: the values that are not the ones the
    /// objects give, by derivation, changeset and key.
    pub(super) fn check<O: ObjectStore>(
        self,
        repo: &Repo<O>,
    ) -> Result<Vec<VerifyProblem>, RepoError> {
        let forest = Forest::of(&self.nodes, &self.at);
        // Each wrong value's derivation, changeset and key.
        let mut wrong: Vec<(&str, ChangesetId, Vec<u8>)> = Vec::new();
        let (name, version) = (FirstParentSkipList::NAME, FirstParentSkipList::VERSION);
        for listed in &self.skip_lists {
            let subject = ChangesetId(listed.subject);
            let Some(&node) = self.at.get(&subject) else {
                continue;
            };
            if forest.depth[node].is_none() {
                continue;
            }
            let Some(StoredValue { key, value }) =
                repo.derived.read_listed(name, version, listed)?
            else {
                continue;
            };
            // Its key is `()`, whose postcard bytes are none: another key is no skip list's slot.
            if !key.is_empty() {
                continue;
            }
            let right = decoded::<SkipList>(&value)
                .is_some_and(|list| self.is_skip_list(&forest, node, &list));
            if !right {
                wrong.push((name, subject, key));
            }
        }
        for touch in self.wrong_touches(&forest) {
            let touch = &self.touches[touch];
            wrong.push((LastModified::NAME, touch.subject, touch.key.clone()));
        }
        wrong.sort_unstable();
        let problems =
            wrong
                .into_iter()
                .map(|(name, changeset, key)| VerifyProblem::DerivedMismatch {
                    name: name.to_owned(),
                    changeset,
                    key,
                });
        Ok(problems.collect())
    }

    /// Whether `list` is the skip list of the changeset at `node` in `forest`: one entry for each
    /// level `i` with `2^i` at most its distance from its root, entry `i` the changeset of its
    /// first-parent chain `2^i` steps below it.
    fn is_skip_list(&self, forest: &Forest, node: usize, list: &[ChangesetId]) -> bool {
        let Some(depth) = forest.depth[node] else {
            return false;
        };
        // ⌊log2 depth⌋ + 1 levels, none at a root.
        let levels = usize::try_from(depth.bit_width()).expect("at most 64");
        list.len() == levels
            && list.iter().enumerate().all(|(level, entry)| {
                let Some(&below) = self.at.get(entry) else {
                    return false;
                };
                let steps = 1_u64 << level;
                depth.checked_sub(steps).is_some()
                    && forest.depth[below] == depth.checked_sub(steps)
                    && forest.holds(below, node)
            })
    }

    /// The values of `last_modified` (by their place in `touches`) that name another changeset
    /// than the nearest of their subject's first-parent chain, the subject included, whose delta
    /// touches their path, or that do not decode. Values about a changeset without a depth in the
    /// forest are passed over.
    fn wrong_touches(&self, forest: &Forest) -> Vec<usize> {
        let mut by_path: Vec<Vec<(usize, usize)>> = vec![Vec::new(); self.paths.len()];
        for (index, touch) in self.touches.iter().enumerate() {
            let Some(&node) = self.at.get(&touch.subject) else {
                continue;
            };
            if forest.depth[node].is_some() {
                by_path[touch.path].push((forest.enter[node], index));
            }
        }
        let mut wrong = Vec::new();
        for (path, mut queries) in by_path.into_iter().enumerate() {
            queries.sort_unstable();
            let mut touching: Vec<usize> = self.touched_by[path]
                .iter()
                .copied()
                .filter(|&node| forest.depth[node].is_some())
                .collect();
            touching.sort_unstable_by_key(|&node| forest.enter[node]);
            // The changesets touching the path entered by the place swept to, in the order they
            // were entered, less those left before it: the spans of those left on top of the
            // first one left over hold the place, each inside the one below it, so the top is the
            // nearest toucher on the chain of a changeset at that place.
            let mut open: Vec<usize> = Vec::new();
            let mut next = 0;
            for (place, index) in queries {
                while let Some(&node) = touching.get(next) {
                    if forest.enter[node] > place {
                        break;
                    }
                    open.push(node);
                    next += 1;
                }
                while open.last().is_some_and(|&top| forest.leave[top] < place) {
                    open.pop();
                }
                let nearest = open.last().map(|&node| self.nodes[node].0);
                if self.touches[index].value != Some(LastTouch(nearest)) {
                    wrong.push(index);
                }
            }
        }
        wrong
    }
}

/// The first-parent forest of the changesets noted: each one's first parent is its parent in it.
struct Forest {
    /// Each changeset's first-parent steps from the root of its chain; `None` where its chain
    /// leaves the changesets noted (a missing or damaged object on it).
    depth: Vec<Option<u64>>,
    /// Where a walk of the forest, depth first, enters each changeset with a depth: the changesets
    /// above one on their chains are entered after it, and before it is left.
    enter: Vec<usize>,
    /// The last place entered below each changeset with a depth, itself included.
    leave: Vec<usize>,
}

impl Forest {
    /// The forest of `nodes`, in seq order, which puts every first parent before its child;
    /// `at` says where each is.
    fn of(
        nodes: &[(ChangesetId, Option<ChangesetId>)],
        at: &HashMap<ChangesetId, usize>,
    ) -> Forest {
        let n = nodes.len();
        let mut depth: Vec<Option<u64>> = Vec::with_capacity(n);
        let mut parent: Vec<Option<usize>> = Vec::with_capacity(n);
        for (node, (_, first)) in nodes.iter().enumerate() {
            let (up, d) = match first {
                None => (None, Some(0)),
                Some(first) => match at.get(first).copied().filter(|&up| up < node) {
                    Some(up) => (Some(up), depth[up].map(|d: u64| d + 1)),
                    None => (None, None),
                },
            };
            parent.push(up.filter(|_| d.is_some()));
            depth.push(d);
        }
        // The children of each changeset, packed: those of `node` are
        // `children[first_child[node]..first_child[node + 1]]`.
        let mut first_child = vec![0_usize; n + 1];
        for up in parent.iter().flatten() {
            first_child[up + 1] += 1;
        }
        for node in 0..n {
            first_child[node + 1] += first_child[node];
        }
        let mut filled = first_child.clone();
        let mut children = vec![0_usize; first_child[n]];
        for (node, up) in parent.iter().enumerate() {
            if let Some(up) = *up {
                children[filled[up]] = node;
                filled[up] += 1;
            }
        }
        let mut enter = vec![0_usize; n];
        let mut leave = vec![0_usize; n];
        let mut place = 0;
        let mut stack: Vec<(usize, usize)> = Vec::new();
        for root in (0..n).filter(|&node| depth[node] == Some(0)) {
            enter[root] = place;
            place += 1;
            stack.push((root, first_child[root]));
            while let Some((node, child)) = stack.last_mut() {
                if *child < first_child[*node + 1] {
                    let next = children[*child];
                    *child += 1;
                    enter[next] = place;
                    place += 1;
                    stack.push((next, first_child[next]));
                } else {
                    leave[*node] = place - 1;
                    stack.pop();
                }
            }
        }
        Forest {
            depth,
            enter,
            leave,
        }
    }

    /// Whether the changeset at `below` is on the first-parent chain of the one at `node`, `node`
    /// itself included; both have a depth.
    fn holds(&self, below: usize, node: usize) -> bool {
        self.enter[below] <= self.enter[node] && self.enter[node] <= self.leave[below]
    }
}

/// What `bytes` decode as, every one of them, as postcard writes a key or a value; `None` if they
/// do not.
fn decoded<T: DeserializeOwned>(bytes: &[u8]) -> Option<T> {
    match postcard::take_from_bytes(bytes) {
        Ok((value, [])) => Some(value),
        _ => None,
    }
}
