//! redb, with its defaults: `Durability::Immediate` (set explicitly), a 1-phase commit with
//! checksums (one `fdatasync` per commit), no quick-repair (a crash makes the next open walk
//! the whole database to rebuild the allocator state), its default cache. One process only:
//! redb 4's multi-process mode is behind the `experimental-multiprocess` feature, not used.
//!
//! Tables: `changesets` id → seq, generation, delta hash, parents; `by_delta_hash`
//! (delta hash, seq) → id; `refs`; `reflog`; `meta` (the seq counters).

use std::path::Path;

use redb::{
    Database, Durability, ReadOnlyTable, ReadTransaction, ReadableDatabase, ReadableTable, Table,
    TableDefinition,
};

use super::{
    Committed, Db, Engine, Error, Main, Result, Session, Snapshot, check_main, decode_generation,
    decode_ref, delta_key, encode_changeset, encode_ref, encode_ref_move, other, unix_now,
};
use crate::data::{Changeset, Id};

const CHANGESETS: TableDefinition<&[u8; 32], &[u8]> = TableDefinition::new("changesets");
const BY_DELTA: TableDefinition<&[u8; 40], &[u8; 32]> = TableDefinition::new("by_delta_hash");
const REFS: TableDefinition<&str, &[u8; 40]> = TableDefinition::new("refs");
const REFLOG: TableDefinition<u64, &[u8]> = TableDefinition::new("reflog");
const META: TableDefinition<&str, u64> = TableDefinition::new("meta");

macro_rules! from_redb {
    ($($t:ty),*) => {
        $(impl From<$t> for Error {
            fn from(e: $t) -> Self {
                Error::Other(e.to_string())
            }
        })*
    };
}

from_redb!(
    redb::Error,
    redb::DatabaseError,
    redb::TransactionError,
    redb::TableError,
    redb::StorageError,
    redb::CommitError,
    redb::SetDurabilityError
);

pub struct Redb;

impl Engine for Redb {
    fn name(&self) -> &'static str {
        "redb"
    }

    fn multi_process(&self) -> bool {
        false
    }

    fn durability(&self) -> &'static str {
        "Durability::Immediate (default), 1-phase commit + checksums: one fdatasync per commit"
    }

    fn create(&self, dir: &Path) -> Result<Box<dyn Db>> {
        let db = Database::create(dir.join("db.redb"))?;
        let mut txn = db.begin_write()?;
        txn.set_durability(Durability::Immediate)?;
        txn.open_table(CHANGESETS)?;
        txn.open_table(BY_DELTA)?;
        txn.open_table(REFS)?;
        txn.open_table(REFLOG)?;
        txn.open_table(META)?;
        txn.commit()?;
        Ok(Box::new(RedbDb(db)))
    }

    fn open(&self, dir: &Path) -> Result<Box<dyn Db>> {
        Ok(Box::new(RedbDb(Database::open(dir.join("db.redb"))?)))
    }
}

struct RedbDb(Database);

impl Db for RedbDb {
    fn session(&self) -> Result<Box<dyn Session + '_>> {
        Ok(Box::new(RedbSession(&self.0)))
    }

    fn settle(&self) -> Result<()> {
        // Every commit is durable already; dropping the database records a clean shutdown.
        Ok(())
    }
}

struct RedbSession<'a>(&'a Database);

type Tables<'t> = (
    Table<'t, &'static [u8; 32], &'static [u8]>,
    Table<'t, &'static [u8; 40], &'static [u8; 32]>,
    Table<'t, &'static str, &'static [u8; 40]>,
    Table<'t, u64, &'static [u8]>,
    Table<'t, &'static str, u64>,
);

fn insert(t: &mut Tables<'_>, cs: &Changeset) -> Result<()> {
    if t.0.get(&cs.id)?.is_some() {
        return Err(other("changeset already present"));
    }
    let mut generation = 1;
    for p in &cs.parents {
        let parent = t.0.get(p)?.ok_or_else(|| other("missing parent"))?;
        generation = generation.max(decode_generation(parent.value())? + 1);
    }
    let seq = t.4.get("seq")?.map_or(0, |g| g.value());
    t.4.insert("seq", seq + 1)?;
    t.0.insert(&cs.id, encode_changeset(seq, generation, cs).as_slice())?;
    t.1.insert(&delta_key(&cs.delta_hash, seq), &cs.id)?;
    Ok(())
}

fn move_main(t: &mut Tables<'_>, new: &Id, main: Main, actor: &str) -> Result<(Option<Id>, bool)> {
    let current =
        t.2.get("main")?
            .map(|g| decode_ref(g.value()))
            .transpose()?;
    let old = current.map(|(target, _)| target);
    let mismatched = check_main(main, old)?;
    let version = current.map_or(0, |(_, v)| v) + 1;
    t.2.insert("main", &encode_ref(new, version))?;
    let r = t.4.get("reflog")?.map_or(0, |g| g.value());
    t.4.insert("reflog", r + 1)?;
    t.3.insert(
        r,
        encode_ref_move(old.as_ref(), new, actor, unix_now()).as_slice(),
    )?;
    Ok((old, mismatched))
}

impl RedbSession<'_> {
    fn write<T>(&self, f: impl FnOnce(&mut Tables<'_>) -> Result<T>) -> Result<T> {
        let mut txn = self.0.begin_write()?;
        txn.set_durability(Durability::Immediate)?;
        let out = {
            let mut tables = (
                txn.open_table(CHANGESETS)?,
                txn.open_table(BY_DELTA)?,
                txn.open_table(REFS)?,
                txn.open_table(REFLOG)?,
                txn.open_table(META)?,
            );
            f(&mut tables)?
        };
        txn.commit()?;
        Ok(out)
    }
}

impl Session for RedbSession<'_> {
    fn commit(&mut self, cs: &Changeset, main: Main, actor: &str) -> Result<Committed> {
        let (old, mismatched) = self.write(|t| {
            insert(t, cs)?;
            move_main(t, &cs.id, main, actor)
        })?;
        Ok(Committed { old, mismatched })
    }

    fn load(&mut self, batch: &[Changeset]) -> Result<()> {
        self.write(|t| {
            for cs in batch {
                insert(t, cs)?;
            }
            if let Some(last) = batch.last() {
                move_main(t, &last.id, Main::Current { guess: None }, "seed")?;
            }
            Ok(())
        })
    }

    fn get(&mut self, id: &Id) -> Result<Option<u64>> {
        let txn = self.0.begin_read()?;
        let table = txn.open_table(CHANGESETS)?;
        let found = table.get(id)?;
        found.map(|g| decode_generation(g.value())).transpose()
    }

    fn snapshot(&mut self) -> Result<Box<dyn Snapshot + '_>> {
        let txn = self.0.begin_read()?;
        let changesets = txn.open_table(CHANGESETS)?;
        let by_delta = txn.open_table(BY_DELTA)?;
        Ok(Box::new(RedbSnapshot {
            _txn: txn,
            changesets,
            by_delta,
        }))
    }
}

struct RedbSnapshot {
    _txn: ReadTransaction,
    changesets: ReadOnlyTable<&'static [u8; 32], &'static [u8]>,
    by_delta: ReadOnlyTable<&'static [u8; 40], &'static [u8; 32]>,
}

impl Snapshot for RedbSnapshot {
    fn get(&self, id: &Id) -> Result<Option<u64>> {
        let found = self.changesets.get(id)?;
        found.map(|g| decode_generation(g.value())).transpose()
    }

    fn prefix(&self, prefix: [u8; 6]) -> Result<Vec<Id>> {
        let mut low = [0_u8; 32];
        let mut high = [0xff_u8; 32];
        low[..6].copy_from_slice(&prefix);
        high[..6].copy_from_slice(&prefix);
        let mut ids = Vec::new();
        for entry in self.changesets.range::<&[u8; 32]>(&low..=&high)? {
            ids.push(*entry?.0.value());
        }
        Ok(ids)
    }

    fn by_delta(&self, dh: &[u8; 32]) -> Result<Vec<Id>> {
        let (low, high) = (delta_key(dh, 0), delta_key(dh, u64::MAX));
        let mut ids = Vec::new();
        for entry in self.by_delta.range::<&[u8; 40]>(&low..=&high)? {
            ids.push(*entry?.1.value());
        }
        Ok(ids)
    }
}
