//! sled 0.34.7, the last stable release (2021; unmaintained since, its 1.0 rewrite still in
//! alpha), with its defaults (1 GB cache, a background flush every 500 ms). A transaction over
//! the five trees, then `Db::flush`, which sled documents as "flushes all dirty IO buffers and
//! calls fsync"; on Linux it writes each buffer with `pwrite` and makes it stable with
//! `sync_file_range(WAIT_BEFORE | WRITE | WAIT_AFTER)`, which writes no metadata and does not
//! flush the device's write cache: weaker than the others' `fsync`/`fdatasync` (the sync-call
//! count shows it). sled's transactions are optimistic: a conflict reruns the closure, and
//! those reruns are counted. One process only: the database is locked.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use sled::Transactional;
use sled::transaction::{ConflictableTransactionError, TransactionError, TransactionalTree, abort};

use super::{
    Committed, Db, Engine, Error, Main, Result, Session, Snapshot, check_main, decode_generation,
    decode_ref, delta_key, encode_changeset, encode_ref, encode_ref_move, other, unix_now,
};
use crate::data::{Changeset, Id};

impl From<sled::Error> for Error {
    fn from(e: sled::Error) -> Self {
        Error::Other(e.to_string())
    }
}

pub struct Sled;

impl Engine for Sled {
    fn name(&self) -> &'static str {
        "sled"
    }

    fn multi_process(&self) -> bool {
        false
    }

    fn durability(&self) -> &'static str {
        "transaction, then Db::flush(): pwrite + sync_file_range (no metadata, no device cache flush)"
    }

    fn create(&self, dir: &Path) -> Result<Box<dyn Db>> {
        self.open(dir)
    }

    fn open(&self, dir: &Path) -> Result<Box<dyn Db>> {
        let db = sled::open(dir.join("sled"))?;
        Ok(Box::new(SledDb {
            changesets: db.open_tree("changesets")?,
            by_delta: db.open_tree("by_delta_hash")?,
            refs: db.open_tree("refs")?,
            reflog: db.open_tree("reflog")?,
            meta: db.open_tree("meta")?,
            db,
            attempts: AtomicU64::new(0),
            commits: AtomicU64::new(0),
        }))
    }
}

struct SledDb {
    db: sled::Db,
    changesets: sled::Tree,
    by_delta: sled::Tree,
    refs: sled::Tree,
    reflog: sled::Tree,
    meta: sled::Tree,
    /// How many times a transaction closure ran, and how many transactions committed: the
    /// difference is sled's conflict reruns.
    attempts: AtomicU64,
    commits: AtomicU64,
}

impl Db for SledDb {
    fn session(&self) -> Result<Box<dyn Session + '_>> {
        Ok(Box::new(SledSession(self)))
    }

    fn settle(&self) -> Result<()> {
        self.db.flush()?;
        Ok(())
    }
}

struct SledSession<'a>(&'a SledDb);

type Tx<T> = std::result::Result<T, ConflictableTransactionError<Error>>;

/// The five trees, as one transaction sees them.
struct Trees<'a> {
    changesets: &'a TransactionalTree,
    by_delta: &'a TransactionalTree,
    refs: &'a TransactionalTree,
    reflog: &'a TransactionalTree,
    meta: &'a TransactionalTree,
}

fn counter(meta: &TransactionalTree, name: &str) -> Tx<u64> {
    match meta.get(name)? {
        Some(v) => match <[u8; 8]>::try_from(&*v) {
            Ok(bytes) => Ok(u64::from_le_bytes(bytes)),
            Err(_) => abort(other("short counter")),
        },
        None => Ok(0),
    }
}

fn insert(t: &Trees<'_>, cs: &Changeset) -> Tx<()> {
    let Trees {
        changesets,
        by_delta,
        meta,
        ..
    } = t;
    if changesets.get(cs.id)?.is_some() {
        return abort(other("changeset already present"));
    }
    let mut generation = 1;
    for p in &cs.parents {
        let Some(parent) = changesets.get(p)? else {
            return abort(other("missing parent"));
        };
        match decode_generation(&parent) {
            Ok(g) => generation = generation.max(g + 1),
            Err(e) => return abort(e),
        }
    }
    let seq = counter(meta, "seq")?;
    meta.insert("seq", &(seq + 1).to_le_bytes()[..])?;
    changesets.insert(&cs.id[..], encode_changeset(seq, generation, cs))?;
    by_delta.insert(&delta_key(&cs.delta_hash, seq)[..], &cs.id[..])?;
    Ok(())
}

fn move_main(t: &Trees<'_>, new: &Id, main: Main, actor: &str) -> Tx<(Option<Id>, bool)> {
    let Trees {
        refs, reflog, meta, ..
    } = t;
    let current = match refs.get("main")?.map(|v| decode_ref(&v)).transpose() {
        Ok(current) => current,
        Err(e) => return abort(e),
    };
    let old = current.map(|(target, _)| target);
    let mismatched = match check_main(main, old) {
        Ok(m) => m,
        Err(e) => return abort(e),
    };
    let version = current.map_or(0, |(_, v)| v) + 1;
    refs.insert("main", &encode_ref(new, version)[..])?;
    let r = counter(meta, "reflog")?;
    meta.insert("reflog", &(r + 1).to_le_bytes()[..])?;
    reflog.insert(
        &r.to_be_bytes()[..],
        encode_ref_move(old.as_ref(), new, actor, unix_now()),
    )?;
    Ok((old, mismatched))
}

impl SledSession<'_> {
    fn transact<T>(&self, f: impl Fn(&Trees<'_>) -> Tx<T>) -> Result<T> {
        let db = self.0;
        let trees = (&db.changesets, &db.by_delta, &db.refs, &db.reflog, &db.meta);
        let out = trees.transaction(|(changesets, by_delta, refs, reflog, meta)| {
            db.attempts.fetch_add(1, Ordering::Relaxed);
            f(&Trees {
                changesets,
                by_delta,
                refs,
                reflog,
                meta,
            })
        });
        let out = match out {
            Ok(out) => out,
            Err(TransactionError::Abort(e)) => return Err(e),
            Err(TransactionError::Storage(e)) => return Err(e.into()),
        };
        db.commits.fetch_add(1, Ordering::Relaxed);
        // What makes the transaction durable.
        db.db.flush()?;
        Ok(out)
    }
}

impl Session for SledSession<'_> {
    fn commit(&mut self, cs: &Changeset, main: Main, actor: &str) -> Result<Committed> {
        let (old, mismatched) = self.transact(|t| {
            insert(t, cs)?;
            move_main(t, &cs.id, main, actor)
        })?;
        Ok(Committed { old, mismatched })
    }

    fn load(&mut self, batch: &[Changeset]) -> Result<()> {
        self.transact(|t| {
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
        get(self.0, id)
    }

    fn snapshot(&mut self) -> Result<Box<dyn Snapshot + '_>> {
        // sled has no read snapshots: every read sees the latest state.
        Ok(Box::new(SledSnapshot(self.0)))
    }

    fn retries(&self) -> u64 {
        let db = self.0;
        db.attempts.load(Ordering::Relaxed) - db.commits.load(Ordering::Relaxed)
    }
}

fn get(db: &SledDb, id: &Id) -> Result<Option<u64>> {
    db.changesets
        .get(id)?
        .map(|v| decode_generation(&v))
        .transpose()
}

fn to_id(bytes: &[u8]) -> Result<Id> {
    bytes
        .try_into()
        .map_err(|_| other("a stored id is not 32 bytes"))
}

struct SledSnapshot<'a>(&'a SledDb);

impl Snapshot for SledSnapshot<'_> {
    fn get(&self, id: &Id) -> Result<Option<u64>> {
        get(self.0, id)
    }

    fn prefix(&self, prefix: [u8; 6]) -> Result<Vec<Id>> {
        let mut ids = Vec::new();
        for entry in self.0.changesets.scan_prefix(prefix) {
            ids.push(to_id(&entry?.0)?);
        }
        Ok(ids)
    }

    fn by_delta(&self, dh: &[u8; 32]) -> Result<Vec<Id>> {
        let mut ids = Vec::new();
        for entry in self.0.by_delta.scan_prefix(dh) {
            ids.push(to_id(&entry?.1)?);
        }
        Ok(ids)
    }
}
