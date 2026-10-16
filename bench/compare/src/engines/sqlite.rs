//! SQLite through rusqlite (bundled), in WAL mode, `BEGIN IMMEDIATE` write transactions with
//! a 10 s `busy_timeout` (the native lock timeout), prepared statements cached per
//! connection, one connection per session. Everything else is SQLite's default (2 MB page
//! cache, no mmap, autocheckpoint at 1,000 pages).
//!
//! `synchronous=FULL`: in WAL mode every commit syncs the WAL before it returns (this build of
//! SQLite has no `HAVE_FDATASYNC`, so the sync is an `fsync`). `synchronous=NORMAL`, the
//! reference line, syncs only at checkpoints: a commit survives a process crash, not a power
//! loss.
//!
//! The schema mirrors the native index: rows in seq order (the rowid), a unique index by id and
//! an index by delta hash, the refs, and a reflog row per ref move (as the M0 SQLite schema had).

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, ErrorCode, OptionalExtension, TransactionBehavior, params};

use super::{
    Committed, Db, Engine, Error, Main, Result, Session, Snapshot, check_main, other, unix_now,
};
use crate::data::{Changeset, Id};

pub struct Sqlite {
    name: &'static str,
    synchronous: &'static str,
}

impl Sqlite {
    pub const FULL: Sqlite = Sqlite {
        name: "sqlite",
        synchronous: "FULL",
    };
    pub const NORMAL: Sqlite = Sqlite {
        name: "sqlite-normal",
        synchronous: "NORMAL",
    };
}

const SCHEMA: &str = "
    CREATE TABLE changesets(
        seq INTEGER PRIMARY KEY,
        id BLOB NOT NULL UNIQUE,
        generation INTEGER NOT NULL,
        delta_hash BLOB NOT NULL,
        parents BLOB NOT NULL
    );
    CREATE INDEX changesets_delta_hash ON changesets(delta_hash);
    CREATE TABLE refs(name TEXT PRIMARY KEY, target BLOB NOT NULL, version INTEGER NOT NULL)
        WITHOUT ROWID;
    CREATE TABLE reflog(
        seq INTEGER PRIMARY KEY,
        name TEXT NOT NULL,
        old BLOB,
        new BLOB,
        actor TEXT NOT NULL,
        ts INTEGER NOT NULL
    );
";

impl Engine for Sqlite {
    fn name(&self) -> &'static str {
        self.name
    }

    fn multi_process(&self) -> bool {
        true
    }

    fn durability(&self) -> &'static str {
        if self.synchronous == "FULL" {
            "WAL, synchronous=FULL: the WAL is fsync'd at every commit"
        } else {
            "WAL, synchronous=NORMAL: no sync at commit, only at checkpoints (NOT power-loss durable)"
        }
    }

    fn create(&self, dir: &Path) -> Result<Box<dyn Db>> {
        let db = SqliteDb {
            path: dir.join("db.sqlite"),
            synchronous: self.synchronous,
        };
        db.connect()?.execute_batch(SCHEMA)?;
        Ok(Box::new(db))
    }

    fn open(&self, dir: &Path) -> Result<Box<dyn Db>> {
        let path = dir.join("db.sqlite");
        if !path.exists() {
            return Err(other(format!("{} is missing", path.display())));
        }
        let db = SqliteDb {
            path,
            synchronous: self.synchronous,
        };
        // Open one connection now, as the other engines open their files, so that open time
        // counts it.
        drop(db.connect()?);
        Ok(Box::new(db))
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        match e.sqlite_error_code() {
            Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) => Error::Busy(e.to_string()),
            _ => Error::Other(e.to_string()),
        }
    }
}

struct SqliteDb {
    path: PathBuf,
    synchronous: &'static str,
}

impl SqliteDb {
    fn connect(&self) -> Result<Connection> {
        let conn = Connection::open(&self.path)?;
        conn.busy_timeout(Duration::from_secs(10))?;
        let mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(other(format!("journal_mode is {mode}, not wal")));
        }
        conn.pragma_update(None, "synchronous", self.synchronous)?;
        Ok(conn)
    }
}

impl Db for SqliteDb {
    fn session(&self) -> Result<Box<dyn Session + '_>> {
        Ok(Box::new(SqliteSession(self.connect()?)))
    }

    fn settle(&self) -> Result<()> {
        let conn = self.connect()?;
        let busy: i64 = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))?;
        if busy != 0 {
            return Err(Error::Busy("wal_checkpoint(TRUNCATE) was blocked".into()));
        }
        Ok(())
    }
}

struct SqliteSession(Connection);

fn to_id(bytes: &[u8]) -> Result<Id> {
    bytes
        .try_into()
        .map_err(|_| other("a stored id is not 32 bytes"))
}

/// Inserts `cs` in the open write transaction `conn`: its parents' generations, then the row.
fn insert(conn: &Connection, cs: &Changeset) -> Result<()> {
    let mut generation = 1_i64;
    let mut parent = conn.prepare_cached("SELECT generation FROM changesets WHERE id = ?1")?;
    for p in &cs.parents {
        let g: Option<i64> = parent.query_row([&p[..]], |r| r.get(0)).optional()?;
        generation = generation.max(g.ok_or_else(|| other("missing parent"))? + 1);
    }
    let mut stmt = conn.prepare_cached(
        "INSERT INTO changesets(id, generation, delta_hash, parents) VALUES (?1, ?2, ?3, ?4)",
    )?;
    match stmt.execute(params![
        &cs.id[..],
        generation,
        &cs.delta_hash[..],
        cs.parents.concat()
    ]) {
        Err(e) if e.sqlite_error_code() == Some(ErrorCode::ConstraintViolation) => {
            Err(other("changeset already present"))
        }
        r => r.map(drop).map_err(Error::from),
    }
}

/// Moves `main` to `new` in the open write transaction `conn` (a compare-and-swap against
/// what it names now, read in the same transaction), recording the move; returns what it named
/// before and whether that was not what `main` expected.
fn move_main(conn: &Connection, new: &Id, main: Main, actor: &str) -> Result<(Option<Id>, bool)> {
    let current: Option<(Vec<u8>, i64)> = conn
        .prepare_cached("SELECT target, version FROM refs WHERE name = 'main'")?
        .query_row([], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    let old = current.as_ref().map(|(t, _)| to_id(t)).transpose()?;
    let mismatched = check_main(main, old)?;
    match current {
        Some((_, version)) => conn
            .prepare_cached("UPDATE refs SET target = ?1, version = ?2 WHERE name = 'main'")?
            .execute(params![&new[..], version + 1])?,
        None => conn
            .prepare_cached("INSERT INTO refs(name, target, version) VALUES ('main', ?1, 1)")?
            .execute([&new[..]])?,
    };
    conn.prepare_cached(
        "INSERT INTO reflog(name, old, new, actor, ts) VALUES ('main', ?1, ?2, ?3, ?4)",
    )?
    .execute(params![
        old.as_ref().map(|o| &o[..]),
        &new[..],
        actor,
        unix_now()
    ])?;
    Ok((old, mismatched))
}

/// What `main` names, read in the open transaction `conn`.
fn read_main(conn: &Connection) -> Result<Option<Id>> {
    let target: Option<Vec<u8>> = conn
        .prepare_cached("SELECT target FROM refs WHERE name = 'main'")?
        .query_row([], |r| r.get(0))
        .optional()?;
    target.as_deref().map(to_id).transpose()
}

impl Session for SqliteSession {
    fn commit(&mut self, cs: &Changeset, main: Main, actor: &str) -> Result<Committed> {
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        insert(&tx, cs)?;
        let (old, mismatched) = move_main(&tx, &cs.id, main, actor)?;
        tx.commit()?;
        Ok(Committed { old, mismatched })
    }

    fn load(&mut self, batch: &[Changeset]) -> Result<()> {
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for cs in batch {
            insert(&tx, cs)?;
        }
        if let Some(last) = batch.last() {
            move_main(&tx, &last.id, Main::Current { guess: None }, "seed")?;
        }
        tx.commit()?;
        Ok(())
    }

    fn get(&mut self, id: &Id) -> Result<Option<u64>> {
        // Autocommit: the statement is a read transaction of its own.
        get(&self.0, id)
    }

    fn snapshot(&mut self) -> Result<Box<dyn Snapshot + '_>> {
        let tx = self.0.unchecked_transaction()?;
        // A deferred transaction takes its snapshot at its first read.
        read_main(&tx)?;
        Ok(Box::new(SqliteSnapshot(tx)))
    }
}

fn get(conn: &Connection, id: &Id) -> Result<Option<u64>> {
    let g: Option<i64> = conn
        .prepare_cached("SELECT generation FROM changesets WHERE id = ?1")?
        .query_row([&id[..]], |r| r.get(0))
        .optional()?;
    Ok(g.map(i64::unsigned_abs))
}

struct SqliteSnapshot<'a>(rusqlite::Transaction<'a>);

impl Snapshot for SqliteSnapshot<'_> {
    fn get(&self, id: &Id) -> Result<Option<u64>> {
        get(&self.0, id)
    }

    fn prefix(&self, prefix: [u8; 6]) -> Result<Vec<Id>> {
        let mut low = [0_u8; 32];
        let mut high = [0xff_u8; 32];
        low[..6].copy_from_slice(&prefix);
        high[..6].copy_from_slice(&prefix);
        let mut stmt = self
            .0
            .prepare_cached("SELECT id FROM changesets WHERE id BETWEEN ?1 AND ?2")?;
        let rows = stmt.query_map([&low[..], &high[..]], |r| r.get::<_, Vec<u8>>(0))?;
        rows.map(|r| to_id(&r?)).collect()
    }

    fn by_delta(&self, dh: &[u8; 32]) -> Result<Vec<Id>> {
        let mut stmt = self
            .0
            .prepare_cached("SELECT id FROM changesets WHERE delta_hash = ?1 ORDER BY seq")?;
        let rows = stmt.query_map([&dh[..]], |r| r.get::<_, Vec<u8>>(0))?;
        rows.map(|r| to_id(&r?)).collect()
    }
}
