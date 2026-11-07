//! An in-memory overlay of an object store (the D2 group-commit design §3.3): a lock holder that
//! plans several lands in one batch relocates each onto the one before, whose `L'` is not
//! written yet. [`Staged`] keeps every put in memory and serves reads and stats from those puts
//! first, then from the store beneath; nothing reaches that store until its caller puts
//! [`Staged::into_objects`] there and flushes, once for the whole batch (I1: before any frame
//! names them).
//!
//! ```text
//!   get / stat / contains ──▶ puts kept in memory ──(miss)──▶ the store beneath
//!   put ────────────────────▶ puts kept in memory             (untouched)
//!   flush ──────────────────▶ nothing: into_objects, then the caller's put and flush
//!   list / retain ──────────▶ StoreError::BadCall (fail closed)
//! ```
//!
//! `retain` promises that the next flush makes an object's name durable, and `list` that every
//! stored object is named: an overlay whose flush writes nothing keeps neither promise, so both
//! fail closed.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use swarmops_core::{Hashed, ObjectId, ObjectKind, bytes};
use swarmops_store::{LimitKind, MAX_OBJECT_LEN, ObjectStat, ObjectStore, PutOutcome, StoreError};

/// Puts kept in memory over `objects` (the [module documentation](self)).
pub(crate) struct Staged<'o, O: ObjectStore> {
    /// The store beneath, read on a miss and never written.
    objects: &'o O,
    /// The objects put, in put order, each once.
    puts: Mutex<Puts>,
}

/// The objects a [`Staged`] holds.
#[derive(Default)]
struct Puts {
    /// Each object put, once, in the order of its first put.
    order: Vec<Hashed>,
    /// Where each id is in `order`.
    at: HashMap<ObjectId, usize>,
}

impl Puts {
    /// The object put under `id`, if any.
    fn get(&self, id: ObjectId) -> Option<&Hashed> {
        self.at.get(&id).map(|&at| &self.order[at])
    }

    /// Keeps `object` unless it is held already; whether it was new.
    fn keep(&mut self, object: Hashed) -> bool {
        if self.at.contains_key(&object.id()) {
            return false;
        }
        self.at.insert(object.id(), self.order.len());
        self.order.push(object);
        true
    }
}

impl<'o, O: ObjectStore> Staged<'o, O> {
    /// Nothing put yet over `objects`.
    pub(crate) fn new(objects: &'o O) -> Self {
        Self {
            objects,
            puts: Mutex::new(Puts::default()),
        }
    }

    /// Every object put, each once, in the order of its first put: for the caller to put into
    /// the store beneath and flush.
    pub(crate) fn into_objects(self) -> Vec<Hashed> {
        self.puts
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner)
            .order
    }

    /// The puts, locked.
    fn puts(&self) -> MutexGuard<'_, Puts> {
        // A put either kept its object whole or did not: poisoning carries no information here.
        self.puts.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Refuses `payload` if it is longer than an object holds, as the store beneath would.
fn within_object_limit(payload: &[u8]) -> Result<(), StoreError> {
    let got = bytes::to_u64(payload.len());
    if got > MAX_OBJECT_LEN {
        return Err(StoreError::Limit {
            what: LimitKind::ObjectBytes,
            max: MAX_OBJECT_LEN,
            got,
        });
    }
    Ok(())
}

/// The error of a call a [`Staged`] cannot keep the promise of.
fn refused(call: &str) -> StoreError {
    StoreError::BadCall(format!("{call} on objects staged in memory"))
}

impl<O: ObjectStore> ObjectStore for Staged<'_, O> {
    fn put(&self, kind: ObjectKind, payload: &[u8]) -> Result<ObjectId, StoreError> {
        within_object_limit(payload)?;
        let object = Hashed::new(kind, payload.to_vec());
        let id = object.id();
        self.puts().keep(object);
        Ok(id)
    }

    fn put_batch(&self, items: Vec<(ObjectKind, Vec<u8>)>) -> Result<Vec<ObjectId>, StoreError> {
        for (_, payload) in &items {
            within_object_limit(payload)?;
        }
        let mut puts = self.puts();
        let ids = items
            .into_iter()
            .map(|(kind, payload)| {
                let object = Hashed::new(kind, payload);
                let id = object.id();
                puts.keep(object);
                id
            })
            .collect();
        Ok(ids)
    }

    /// Each outcome's `written` says whether this call kept the object in memory: it was not
    /// put here before. The store beneath is not asked.
    fn put_hashed_batch(&self, items: Vec<Hashed>) -> Result<Vec<PutOutcome>, StoreError> {
        for object in &items {
            within_object_limit(object.payload())?;
        }
        let mut puts = self.puts();
        let outcomes = items
            .into_iter()
            .map(|object| {
                let id = object.id();
                PutOutcome::new(id, puts.keep(object))
            })
            .collect();
        Ok(outcomes)
    }

    fn get(&self, id: ObjectId) -> Result<Option<(ObjectKind, Vec<u8>)>, StoreError> {
        if let Some(object) = self.puts().get(id) {
            return Ok(Some((object.kind(), object.payload().to_vec())));
        }
        self.objects.get(id)
    }

    /// A put object goes to `visit` in one piece; `visit` must not call this overlay.
    fn read_chunks(
        &self,
        id: ObjectId,
        visit: &mut dyn FnMut(&[u8]),
    ) -> Result<Option<ObjectStat>, StoreError> {
        if let Some(object) = self.puts().get(id) {
            visit(object.payload());
            let size = bytes::to_u64(object.payload().len());
            return Ok(Some(ObjectStat::new(object.kind(), size)));
        }
        self.objects.read_chunks(id, visit)
    }

    fn contains(&self, id: ObjectId) -> Result<bool, StoreError> {
        if self.puts().get(id).is_some() {
            return Ok(true);
        }
        self.objects.contains(id)
    }

    fn stat(&self, id: ObjectId) -> Result<Option<ObjectStat>, StoreError> {
        if let Some(object) = self.puts().get(id) {
            let size = bytes::to_u64(object.payload().len());
            return Ok(Some(ObjectStat::new(object.kind(), size)));
        }
        self.objects.stat(id)
    }

    fn retain(&self, _id: ObjectId) -> Result<Option<ObjectStat>, StoreError> {
        Err(refused("retain"))
    }

    /// Nothing: the objects put reach the store beneath through [`Staged::into_objects`].
    fn flush(&self) -> Result<(), StoreError> {
        Ok(())
    }

    fn list(&self) -> Result<Vec<ObjectId>, StoreError> {
        Err(refused("list"))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use swarmops_core::{ChangesetId, DeltaEntry, DeltaOp, LeafKind, RepoPath, Signature, Tree};
    use swarmops_store::{FsObjectStore, Layout, RealFs};

    use super::*;
    use crate::commit::{self, Draft};

    /// An empty object store in a new temporary directory, kept alive by the guard.
    fn store() -> (tempfile::TempDir, FsObjectStore) {
        let dir = tempfile::tempdir().unwrap();
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let objects = FsObjectStore::open(Layout::new(root), Box::new(RealFs::new()));
        (dir, objects)
    }

    /// A put is read back from memory, the store beneath untouched; an object of the store
    /// beneath is read through.
    #[test]
    fn staged_reads_its_puts_first() {
        let (_dir, base) = store();
        let below = base.put(ObjectKind::Blob, b"below").unwrap();
        base.flush().unwrap();
        let staged = Staged::new(&base);

        let id = staged.put(ObjectKind::Blob, b"staged").unwrap();
        assert_eq!(
            staged.get(id).unwrap(),
            Some((ObjectKind::Blob, b"staged".to_vec())),
            "a put reads back"
        );
        assert!(staged.contains(id).unwrap(), "a put is held");
        let mut read = Vec::new();
        let stat = staged.read_chunks(id, &mut |piece| read.extend_from_slice(piece));
        assert_eq!(stat.unwrap(), Some(ObjectStat::new(ObjectKind::Blob, 6)));
        assert_eq!(read, b"staged", "a put's chunks are its payload");
        assert_eq!(base.get(id).unwrap(), None, "the store beneath lacks it");

        assert_eq!(
            staged.get(below).unwrap(),
            Some((ObjectKind::Blob, b"below".to_vec())),
            "the store beneath is read through"
        );
    }

    /// A put's stat is its kind and payload length, without the store beneath.
    #[test]
    fn staged_stat_has_kind_and_size() {
        let (_dir, base) = store();
        let staged = Staged::new(&base);
        let tree = Tree::empty_root().encode();
        let tree_id = staged.put(ObjectKind::Tree, &tree).unwrap();
        let blob_id = staged.put(ObjectKind::Blob, &[7; 300]).unwrap();

        let tree_len = bytes::to_u64(tree.len());
        assert_eq!(
            staged.stat(tree_id).unwrap(),
            Some(ObjectStat::new(ObjectKind::Tree, tree_len)),
            "a staged tree's stat"
        );
        assert_eq!(
            staged.stat(blob_id).unwrap(),
            Some(ObjectStat::new(ObjectKind::Blob, 300)),
            "a staged blob's stat"
        );
        assert_eq!(
            base.stat(blob_id).unwrap(),
            None,
            "the store beneath lacks it"
        );
    }

    /// `list` and `retain` fail, for a staged object and one of the store beneath alike.
    #[test]
    fn staged_list_and_retain_fail_closed() {
        let (_dir, base) = store();
        let below = base.put(ObjectKind::Blob, b"below").unwrap();
        let staged = Staged::new(&base);
        let id = staged.put(ObjectKind::Blob, b"staged").unwrap();

        assert!(
            matches!(staged.list(), Err(StoreError::BadCall(_))),
            "list fails closed"
        );
        for id in [id, below] {
            assert!(
                matches!(staged.retain(id), Err(StoreError::BadCall(_))),
                "retain fails closed"
            );
        }
    }

    /// A changeset written over the overlay, flush included, leaves the store beneath as it was;
    /// its objects reach that store when they are put there.
    #[test]
    fn staging_writes_nothing_until_put() {
        let (_dir, base) = store();
        let staged = Staged::new(&base);
        let path = RepoPath::new("a/b.txt").unwrap();
        let blob = Hashed::new(ObjectKind::Blob, b"content\n".to_vec());
        let put = DeltaOp::Put {
            kind: LeafKind::File,
            id: swarmops_core::BlobId::new(blob.id()),
        };
        let sig = Signature::new(b"a".to_vec(), b"a@x".to_vec(), 0, 0, false).unwrap();
        let draft = Draft {
            onto: None,
            changes: vec![DeltaEntry::new(path, put, None).unwrap()],
            contents: BTreeMap::from([(blob.id(), blob)]),
            parents: Vec::new(),
            author: sig.clone(),
            committer: sig,
            message: b"m".to_vec(),
            metadata: Vec::new(),
            allow_empty: false,
        };

        let written = commit::write_changeset_in(&staged, draft).unwrap();
        let id: ChangesetId = written.id;
        assert!(
            staged.contains(id.0).unwrap(),
            "the overlay holds the changeset"
        );
        assert_eq!(
            base.list().unwrap(),
            Vec::new(),
            "nothing reached the store"
        );

        let objects = staged.into_objects();
        assert_eq!(objects.len(), 4, "a blob, two trees and the changeset");
        base.put_hashed_batch(objects).unwrap();
        base.flush().unwrap();
        assert!(base.contains(id.0).unwrap(), "the put reached the store");
        assert_eq!(
            base.list().unwrap().len(),
            4,
            "every object reached the store"
        );
    }
}
