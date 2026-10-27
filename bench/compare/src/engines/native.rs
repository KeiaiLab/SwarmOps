//! SwarmOps' own metadata store (`swarmops_store::MetaStore`), through its public API, with
//! its defaults: the 10 s lock timeout, index syncs from 5 threads.
//!
//! Durability: a commit appends one CRC-framed frame to `journal.log` and `sync_data`s it,
//! then its offset to `journal.off` and `sync_data`s that; a moved ref's file is replaced
//! atomically (temp file, `sync_all`, rename, directory fsync). The index columns are synced
//! in batches, every 32 frames, and compacted into the sorted views every 1,024 records.

use std::path::Path;

use camino::Utf8Path;
use swarmops_core::{ChangesetId, ObjectId};
use swarmops_store::{CasResult, ChangesetMeta, InsertOutcome, MetaRead, MetaStore, StoreError};

use super::{Committed, Db, Engine, Error, Main, Result, Session, Snapshot, check_main, other};
use crate::data::{Changeset, Id, hex};

pub struct Native;

impl Engine for Native {
    fn name(&self) -> &'static str {
        "native"
    }

    fn multi_process(&self) -> bool {
        true
    }

    fn durability(&self) -> &'static str {
        "journal frame + offset entry, each fdatasync'd; ref file replaced (fsync + rename + dir fsync)"
    }

    fn create(&self, dir: &Path) -> Result<Box<dyn Db>> {
        Ok(Box::new(NativeDb(MetaStore::create(utf8(dir)?)?)))
    }

    fn open(&self, dir: &Path) -> Result<Box<dyn Db>> {
        Ok(Box::new(NativeDb(MetaStore::open(utf8(dir)?)?)))
    }
}

fn utf8(dir: &Path) -> Result<&Utf8Path> {
    Utf8Path::from_path(dir).ok_or_else(|| other(format!("{} is not UTF-8", dir.display())))
}

impl From<StoreError> for Error {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::Busy => Error::Busy(e.to_string()),
            e => Error::Other(e.to_string()),
        }
    }
}

fn cid(id: Id) -> ChangesetId {
    ChangesetId::new(ObjectId::new(id))
}

fn meta(cs: &Changeset) -> ChangesetMeta {
    let parents = cs.parents.iter().copied().map(cid).collect();
    ChangesetMeta::new(cid(cs.id), 0, 0, cs.delta_hash, parents)
}

struct NativeDb(MetaStore);

impl Db for NativeDb {
    fn session(&self) -> Result<Box<dyn Session + '_>> {
        Ok(Box::new(NativeSession(&self.0)))
    }

    fn settle(&self) -> Result<()> {
        Ok(self.0.flush()?)
    }
}

struct NativeSession<'a>(&'a MetaStore);

impl Session for NativeSession<'_> {
    fn commit(&mut self, cs: &Changeset, main: Main, actor: &str) -> Result<Committed> {
        let mut write = self.0.write()?;
        match write.insert_changeset(meta(cs))? {
            InsertOutcome::Inserted { .. } => {}
            InsertOutcome::AlreadyPresent => return Err(other("changeset already present")),
            _ => return Err(other("an insert outcome this harness does not know")),
        }
        let new = Some(cid(cs.id));
        let first = match main {
            Main::Expect(expected) => expected,
            Main::Current { guess } => guess,
        };
        // A CAS against the value read inside this transaction: the file's (or what this
        // transaction moved it to), as `cas_ref` reads it under the lock.
        let (old, mismatched) = match write.cas_ref("main", first.map(cid), new, actor)? {
            CasResult::Applied { old } => (old, false),
            CasResult::Mismatch { actual } => {
                check_main(main, actual.map(|c| c.0.0))?;
                match write.cas_ref("main", actual, new, actor)? {
                    CasResult::Applied { old } => (old, true),
                    CasResult::Mismatch { .. } => return Err(other("main moved under the lock")),
                    _ => return Err(other("a CAS result this harness does not know")),
                }
            }
            _ => return Err(other("a CAS result this harness does not know")),
        };
        write.commit()?;
        Ok(Committed {
            old: old.map(|c| c.0.0),
            mismatched,
        })
    }

    fn load(&mut self, batch: &[Changeset]) -> Result<()> {
        let mut write = self.0.write()?;
        for cs in batch {
            write.insert_changeset(meta(cs))?;
        }
        if let Some(last) = batch.last() {
            let new = Some(cid(last.id));
            if let CasResult::Mismatch { actual } = write.cas_ref("main", None, new, "seed")? {
                write.cas_ref("main", actual, new, "seed")?;
            }
        }
        write.commit()?;
        Ok(())
    }

    fn get(&mut self, id: &Id) -> Result<Option<u64>> {
        Ok(self.0.read()?.changeset(cid(*id))?.map(|m| m.generation))
    }

    fn snapshot(&mut self) -> Result<Box<dyn Snapshot + '_>> {
        Ok(Box::new(NativeSnapshot(self.0.read()?)))
    }
}

struct NativeSnapshot<'a>(MetaRead<'a>);

impl Snapshot for NativeSnapshot<'_> {
    fn get(&self, id: &Id) -> Result<Option<u64>> {
        Ok(self.0.changeset(cid(*id))?.map(|m| m.generation))
    }

    fn prefix(&self, prefix: [u8; 6]) -> Result<Vec<Id>> {
        let ids = self.0.resolve_prefix(&hex(&prefix))?;
        Ok(ids.into_iter().map(|c| c.0.0).collect())
    }

    fn by_delta(&self, dh: &[u8; 32]) -> Result<Vec<Id>> {
        let ids = self.0.by_delta_hash(dh)?;
        Ok(ids.into_iter().map(|c| c.0.0).collect())
    }
}
