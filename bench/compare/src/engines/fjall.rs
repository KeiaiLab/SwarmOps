//! fjall (an LSM tree), through its single-writer transactional database: write transactions
//! are serialized by an in-process mutex, and each commits with
//! `durability(Some(PersistMode::SyncAll))`, the strongest mode it offers: the journal is
//! written and `fsync`'d before `commit` returns. Everything else is fjall's default (LZ4
//! blocks, its block cache, 64 MiB memtables, background flush and compaction workers). One
//! process only: the database directory is locked.
//!
//! Keyspaces: `changesets` id → seq, generation, delta hash, parents; `by_delta_hash`
//! (delta hash, seq) → id; `refs`; `reflog`; `meta` (the seq counters).
//!
//! # Settling a seeded store
//!
//! fjall keeps what it writes in memtables backed by its journal, and writes a memtable to a
//! table file only once it holds 64 MiB. A journal is dropped only when every keyspace has
//! flushed past it, and the small keyspaces (`meta`, `refs`, `reflog`) never fill a memtable,
//! so after a bulk load every journal stays until 512 MiB of them force those keyspaces to
//! flush: measured that way, W5 would count the journals beside the tables (216 of the 385 MiB
//! of a 1M store). So `fjall` (the row the comparison uses) settles as the others do, by moving
//! its log into its main structure, as SQLite's checkpoint does: it syncs the journal, then
//! flushes every keyspace's memtable into table files (`rotate_memtable_and_wait`, a public but
//! doc-hidden call fjall's own tests use), after which fjall deletes the journals no keyspace
//! needs (within 10 s, or the settle fails: an unsettled store is not measured). What stays is
//! the active journal: fjall rotates it only when it passes 64 MB at a flush, and an open
//! replays it whole, flushed or not. So the flush shrinks a large store and saves only the
//! first open after the load: opening an unflushed store replays every journal, flushes what
//! the load left in memtables and deletes the sealed journals (2.9 s against 0.55 s at 1M);
//! every later open replays the same active journal. A small store keeps its whole journal
//! beside the new tables. `fjall-unflushed` settles with the journal sync alone: the store as
//! the bulk load left it, measured in W3–W5 only, for the record (W5 sizes it as left, its first
//! open is timed at seeding, and W4 opens fresh copies of it as left, taken at seeding; W3 and
//! W4's recovery see it after that first open settled it).

use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use fjall::{
    KeyspaceCreateOptions, PersistMode, Readable, SingleWriterTxDatabase, SingleWriterTxKeyspace,
    SingleWriterWriteTx,
};

use super::{
    Committed, Db, Engine, Error, Main, Result, Session, Snapshot, check_main, decode_generation,
    decode_ref, delta_key, encode_changeset, encode_ref, encode_ref_move, other, unix_now,
};
use crate::data::{Changeset, Id};

impl From<fjall::Error> for Error {
    fn from(e: fjall::Error) -> Self {
        Error::Other(e.to_string())
    }
}

pub struct Fjall {
    name: &'static str,
    /// Whether [`Db::settle`] flushes the memtables into table files (see the module
    /// documentation).
    flush: bool,
}

impl Fjall {
    pub const FLUSHED: Fjall = Fjall {
        name: "fjall",
        flush: true,
    };
    pub const UNFLUSHED: Fjall = Fjall {
        name: "fjall-unflushed",
        flush: false,
    };
}

impl Engine for Fjall {
    fn name(&self) -> &'static str {
        self.name
    }

    fn multi_process(&self) -> bool {
        false
    }

    fn durability(&self) -> &'static str {
        if self.flush {
            "single-writer tx, durability(Some(PersistMode::SyncAll)): the journal is fsync'd at every commit; a seeded store's memtables flushed into tables when settled"
        } else {
            "as fjall, but a seeded store is settled with a journal sync alone: its data stays in the journals (W3–W5 only)"
        }
    }

    fn create(&self, dir: &Path) -> Result<Box<dyn Db>> {
        self.open(dir)
    }

    fn open(&self, dir: &Path) -> Result<Box<dyn Db>> {
        let db = SingleWriterTxDatabase::builder(dir.join("fjall")).open()?;
        let keyspace = |name| db.keyspace(name, KeyspaceCreateOptions::default);
        Ok(Box::new(FjallDb {
            changesets: keyspace("changesets")?,
            by_delta: keyspace("by_delta_hash")?,
            refs: keyspace("refs")?,
            reflog: keyspace("reflog")?,
            meta: keyspace("meta")?,
            db,
            flush: self.flush,
        }))
    }
}

struct FjallDb {
    db: SingleWriterTxDatabase,
    changesets: SingleWriterTxKeyspace,
    by_delta: SingleWriterTxKeyspace,
    refs: SingleWriterTxKeyspace,
    reflog: SingleWriterTxKeyspace,
    meta: SingleWriterTxKeyspace,
    flush: bool,
}

impl Db for FjallDb {
    fn session(&self) -> Result<Box<dyn Session + '_>> {
        Ok(Box::new(FjallSession(self)))
    }

    fn settle(&self) -> Result<()> {
        self.db.persist(PersistMode::SyncAll)?;
        if !self.flush {
            return Ok(());
        }
        let keyspaces = [
            &self.changesets,
            &self.by_delta,
            &self.refs,
            &self.reflog,
            &self.meta,
        ];
        for keyspace in keyspaces {
            AsRef::<fjall::Keyspace>::as_ref(keyspace).rotate_memtable_and_wait()?;
        }
        // The flush worker deletes the journals no keyspace needs right after its last flush.
        wait_settled(|| self.db.journal_count(), SETTLE_TIMEOUT)
    }
}

/// How long [`Db::settle`] waits for fjall to delete the journals its flush left unneeded.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(10);

/// Waits, polling every 10 ms, until `journals` (fjall's `journal_count`, which counts the
/// active journal too) is down to one; an error after `timeout`. A store that keeps more is not
/// settled: W3–W5 would time and size another store than the one the report names, so the
/// seeding fails, and the report lists the error, rather than measure it.
fn wait_settled(journals: impl Fn() -> usize, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let count = journals();
        if count <= 1 {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(other(format!(
                "fjall still holds {count} journals {timeout:?} after flushing every keyspace: \
                 the store is not settled"
            )));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

struct FjallSession<'a>(&'a FjallDb);

fn counter(tx: &SingleWriterWriteTx<'_>, db: &FjallDb, name: &str) -> Result<u64> {
    match tx.get(&db.meta, name)? {
        Some(v) => Ok(u64::from_le_bytes(
            (*v).try_into().map_err(|_| other("short counter"))?,
        )),
        None => Ok(0),
    }
}

fn insert(tx: &mut SingleWriterWriteTx<'_>, db: &FjallDb, cs: &Changeset) -> Result<()> {
    if tx.get(&db.changesets, cs.id)?.is_some() {
        return Err(other("changeset already present"));
    }
    let mut generation = 1;
    for p in &cs.parents {
        let parent = tx
            .get(&db.changesets, p)?
            .ok_or_else(|| other("missing parent"))?;
        generation = generation.max(decode_generation(&parent)? + 1);
    }
    let seq = counter(tx, db, "seq")?;
    tx.insert(&db.meta, "seq", (seq + 1).to_le_bytes().to_vec());
    tx.insert(
        &db.changesets,
        cs.id.to_vec(),
        encode_changeset(seq, generation, cs),
    );
    tx.insert(
        &db.by_delta,
        delta_key(&cs.delta_hash, seq).to_vec(),
        cs.id.to_vec(),
    );
    Ok(())
}

fn move_main(
    tx: &mut SingleWriterWriteTx<'_>,
    db: &FjallDb,
    new: &Id,
    main: Main,
    actor: &str,
) -> Result<(Option<Id>, bool)> {
    let current = tx
        .get(&db.refs, "main")?
        .map(|v| decode_ref(&v))
        .transpose()?;
    let old = current.map(|(target, _)| target);
    let mismatched = check_main(main, old)?;
    let version = current.map_or(0, |(_, v)| v) + 1;
    tx.insert(&db.refs, "main", encode_ref(new, version).to_vec());
    let r = counter(tx, db, "reflog")?;
    tx.insert(&db.meta, "reflog", (r + 1).to_le_bytes().to_vec());
    tx.insert(
        &db.reflog,
        r.to_be_bytes().to_vec(),
        encode_ref_move(old.as_ref(), new, actor, unix_now()),
    );
    Ok((old, mismatched))
}

impl Session for FjallSession<'_> {
    fn commit(&mut self, cs: &Changeset, main: Main, actor: &str) -> Result<Committed> {
        let db = self.0;
        let mut tx = db.db.write_tx().durability(Some(PersistMode::SyncAll));
        insert(&mut tx, db, cs)?;
        let (old, mismatched) = move_main(&mut tx, db, &cs.id, main, actor)?;
        tx.commit()?;
        Ok(Committed { old, mismatched })
    }

    fn load(&mut self, batch: &[Changeset]) -> Result<()> {
        let db = self.0;
        let mut tx = db.db.write_tx().durability(Some(PersistMode::SyncAll));
        for cs in batch {
            insert(&mut tx, db, cs)?;
        }
        if let Some(last) = batch.last() {
            move_main(&mut tx, db, &last.id, Main::Current { guess: None }, "seed")?;
        }
        tx.commit()?;
        Ok(())
    }

    fn get(&mut self, id: &Id) -> Result<Option<u64>> {
        let snapshot = self.0.db.read_tx();
        let found = snapshot.get(&self.0.changesets, id)?;
        found.map(|v| decode_generation(&v)).transpose()
    }

    fn snapshot(&mut self) -> Result<Box<dyn Snapshot + '_>> {
        Ok(Box::new(FjallSnapshot {
            snapshot: self.0.db.read_tx(),
            db: self.0,
        }))
    }
}

struct FjallSnapshot<'a> {
    snapshot: fjall::Snapshot,
    db: &'a FjallDb,
}

fn to_id(bytes: &[u8]) -> Result<Id> {
    bytes
        .try_into()
        .map_err(|_| other("a stored id is not 32 bytes"))
}

impl Snapshot for FjallSnapshot<'_> {
    fn get(&self, id: &Id) -> Result<Option<u64>> {
        let found = self.snapshot.get(&self.db.changesets, id)?;
        found.map(|v| decode_generation(&v)).transpose()
    }

    fn prefix(&self, prefix: [u8; 6]) -> Result<Vec<Id>> {
        let mut ids = Vec::new();
        for guard in self.snapshot.prefix(&self.db.changesets, prefix) {
            ids.push(to_id(&guard.key()?)?);
        }
        Ok(ids)
    }

    fn by_delta(&self, dh: &[u8; 32]) -> Result<Vec<Id>> {
        let mut ids = Vec::new();
        for guard in self.snapshot.prefix(&self.db.by_delta, dh) {
            ids.push(to_id(&guard.value()?)?);
        }
        Ok(ids)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::wait_settled;

    #[test]
    fn settling_fails_when_the_unneeded_journals_stay() {
        let timeout = Duration::from_millis(30);
        let error = wait_settled(|| 2, timeout).expect_err("two journals stay");
        assert!(
            error.to_string().contains("still holds 2 journals"),
            "{error}"
        );
        wait_settled(|| 1, timeout).expect("the active journal alone");
    }
}
