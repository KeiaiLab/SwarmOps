//! `gc` (the M6 design §8): removes a store's leftovers, under its maintenance lease, and
//! counts the objects no changeset reaches. It removes no object and no changeset.

use std::collections::HashSet;
use std::time::{Duration, SystemTime};

use swarmops_core::{EntryTarget, ObjectId};
use swarmops_store::{LeftoverKind, MaintLease, ObjectStore, Record, gc};

use crate::derive::{Derivation, FirstParentSkipList, LastModified};
use crate::error::RepoError;
use crate::repo::{self, Repo};

/// How long gc waits for `meta/lock` once it holds the lease: the metadata store's own wait,
/// which a repository keeps.
const META_LOCK_TIMEOUT: Duration = Duration::from_secs(10);

/// The derivations whose superseded versions gc removes: the ones every repository of this
/// build registers. One a caller registers is not known here, and its versions are kept.
const BUILTINS: [(&str, u32); 2] = [
    (FirstParentSkipList::NAME, FirstParentSkipList::VERSION),
    (LastModified::NAME, LastModified::VERSION),
];

/// How many frames a count of the orphan objects reads at a time.
const FRAME_BATCH: usize = 4096;

/// What [`Repo::gc`] is asked to do.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct GcRequest {
    /// List what it would remove, and remove nothing.
    pub dry_run: bool,
}

impl GcRequest {
    /// A gc that removes, or with `dry_run` only lists.
    pub fn new(dry_run: bool) -> GcRequest {
        GcRequest { dry_run }
    }
}

/// What [`Repo::gc`] removed, or would remove for a dry run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct GcReport {
    /// How many leftovers of each kind, kinds with none omitted.
    pub removed: Vec<(LeftoverKind, u64)>,
    /// The bytes of their files.
    pub bytes: u64,
    /// How many object files no indexed changeset reaches: counted, never removed.
    pub orphan_objects: u64,
}

impl<O: ObjectStore> Repo<O> {
    /// Removes the store's leftovers ([`swarmops_store::gc`]), or for a dry run only lists
    /// them, then counts the object files no indexed changeset reaches. Takes the maintenance
    /// lease first ([`swarmops_store::StoreError::MaintenanceBusy`] at once if it is held), and
    /// `meta/lock` after it while it lists and removes; it holds only the lease while it counts.
    pub fn gc(&self, req: GcRequest) -> Result<GcReport, RepoError> {
        // 1. The lease, then meta/lock (the M6 design §3).
        let lease = MaintLease::try_acquire(&self.layout)?;
        let meta_lock = lease.meta_lock(&self.layout, META_LOCK_TIMEOUT)?;

        // 2. The leftovers, listed and, unless dry, removed: removal follows the listing's
        //    order, which puts a poisoned checkpoint last.
        let leftovers = gc::list_leftovers_of(&self.layout, SystemTime::now(), &BUILTINS)?;
        if !req.dry_run {
            let fs = self.store_fs();
            for leftover in &leftovers {
                gc::remove(&fs, leftover)?;
            }
        }
        drop(meta_lock);

        // 3. The report: counts by kind, in kind order (the listing's), and the orphans.
        let mut removed: Vec<(LeftoverKind, u64)> = Vec::new();
        for leftover in &leftovers {
            match removed.last_mut() {
                Some((kind, count)) if *kind == leftover.kind => *count += 1,
                _ => removed.push((leftover.kind, 1)),
            }
        }
        let bytes = leftovers.iter().map(|leftover| leftover.bytes).sum();
        let orphan_objects = self.orphan_objects()?;
        Ok(GcReport {
            removed,
            bytes,
            orphan_objects,
        })
    }

    /// How many object files no changeset of the journal reaches through its object, its trees
    /// and their blobs. A changeset or a tree whose object is missing or does not decode reaches
    /// nothing further: verify reports it; gc only counts.
    fn orphan_objects(&self) -> Result<u64, RepoError> {
        // 1. The changesets, newest frame first.
        let read = self.meta.read()?;
        let mut changesets = Vec::new();
        let mut before = None;
        loop {
            let frames = read.frames_before(before, FRAME_BATCH)?;
            let Some(oldest) = frames.last() else {
                break;
            };
            before = Some(oldest.seq);
            for record in frames.iter().flat_map(|frame| &frame.records) {
                if let Record::Changeset { id, .. } = record {
                    changesets.push(*id);
                }
            }
        }

        // 2. What they reach.
        let mut reached: HashSet<ObjectId> = HashSet::new();
        let mut trees = Vec::new();
        for id in changesets {
            reached.insert(id.0);
            if let Ok(cs) = self.changeset_object(id) {
                trees.push(cs.root);
            }
        }
        while let Some(id) = trees.pop() {
            if !reached.insert(id.0) {
                continue;
            }
            let Ok(tree) = repo::tree(&self.objects, id) else {
                continue;
            };
            for entry in tree.entries() {
                match entry.target {
                    EntryTarget::Dir(dir) => trees.push(dir),
                    EntryTarget::Leaf(_, blob) => {
                        reached.insert(blob.0);
                    }
                    _ => return Err(RepoError::unknown_variant("EntryTarget")),
                }
            }
        }

        // 3. The object files outside it.
        let listed = self.objects.list()?;
        let orphans = listed.iter().filter(|id| !reached.contains(id)).count();
        Ok(swarmops_core::bytes::to_u64(orphans))
    }
}
