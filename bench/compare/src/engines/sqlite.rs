//! SQLite through rusqlite (bundled), in WAL mode, `BEGIN IMMEDIATE` write transactions with
//! a 10 s `busy_timeout` (the native lock timeout), prepared statements cached per
//! connection, one connection per session. Everything else is SQLite's default (2 MB page
//! cache, no mmap, autocheckpoint at 1,000 pages).
//!
//! Opening (or creating) a store makes its first connection, and the first session gets that
//! connection: closing it would close the store's last connection, which checkpoints the WAL
//! into the database (syncing both) and deletes it, and the session's own connection would then
//! start a new WAL. W4 times the open and the first lookup or write, so neither may pay for
//! that.
//!
//! `synchronous=FULL`: in WAL mode every commit syncs the WAL before it returns (this build of
//! SQLite has no `HAVE_FDATASYNC`, so the sync is an `fsync`). `synchronous=NORMAL`, the
//! reference line, syncs only at checkpoints: a commit survives a process crash, not a power
//! loss.
//!
//! The schema mirrors the native index: rows in seq order (the rowid), a unique index by id and
//! an index by delta hash, the refs, and a reflog row per ref move (as the M0 SQLite schema had).

use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
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
        let path = dir.join("db.sqlite");
        let first = connect(&path, self.synchronous)?;
        first.execute_batch(SCHEMA)?;
        Ok(Box::new(SqliteDb::new(path, self.synchronous, first)))
    }

    fn open(&self, dir: &Path) -> Result<Box<dyn Db>> {
        let path = dir.join("db.sqlite");
        if !path.exists() {
            return Err(other(format!("{} is missing", path.display())));
        }
        // The first connection now, as the other engines open their files, so that the open's
        // time counts it (and, after a crash, the WAL's recovery); the first session gets it.
        let first = connect(&path, self.synchronous)?;
        Ok(Box::new(SqliteDb::new(path, self.synchronous, first)))
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
    /// The connection the open or the create made, until the first session takes it.
    first: Mutex<Option<Connection>>,
}

/// A connection to the database at `path`, in WAL mode at `synchronous`. The first one after
/// a crash recovers the WAL (it rebuilds the wal-index from it).
fn connect(path: &Path, synchronous: &str) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(Duration::from_secs(10))?;
    let mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(other(format!("journal_mode is {mode}, not wal")));
    }
    conn.pragma_update(None, "synchronous", synchronous)?;
    Ok(conn)
}

impl SqliteDb {
    fn new(path: PathBuf, synchronous: &'static str, first: Connection) -> SqliteDb {
        SqliteDb {
            path,
            synchronous,
            first: Mutex::new(Some(first)),
        }
    }

    fn connect(&self) -> Result<Connection> {
        connect(&self.path, self.synchronous)
    }
}

impl Db for SqliteDb {
    fn session(&self) -> Result<Box<dyn Session + '_>> {
        let first = self
            .first
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let conn = match first {
            Some(conn) => conn,
            None => self.connect()?,
        };
        Ok(Box::new(SqliteSession(conn)))
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

#[cfg(test)]
mod tests {
    use std::fs;

    use super::Sqlite;
    use crate::data::{chain, changeset, id};
    use crate::engines::{Engine, Main};

    /// The files a writer killed after `n` acknowledged commits leaves: the database and the
    /// WAL holding its commits, copied while its connection is open (so nothing checkpointed
    /// them or deleted the WAL), and no wal-index, which the next open rebuilds.
    fn killed_writer_store(n: u64) -> tempfile::TempDir {
        let writer = tempfile::tempdir().expect("a temp dir");
        let left = tempfile::tempdir().expect("a temp dir");
        let db = Sqlite::FULL.create(writer.path()).expect("create");
        let mut session = db.session().expect("a session");
        let mut main = None;
        for k in 0..n {
            let cs = changeset(chain::W4_WRITER, k, None);
            session
                .commit(&cs, Main::Expect(main), "test")
                .expect("commit");
            main = Some(cs.id);
        }
        for file in ["db.sqlite", "db.sqlite-wal"] {
            fs::copy(writer.path().join(file), left.path().join(file)).expect("copy");
        }
        left
    }

    #[test]
    fn open_leaves_a_killed_writers_wal_to_the_first_session() {
        let store = killed_writer_store(8);
        let wal = store.path().join("db.sqlite-wal");
        let left = fs::metadata(&wal).expect("the WAL").len();
        assert!(left > 0, "the killed writer's commits are in its WAL");

        let db = Sqlite::FULL.open(store.path()).expect("open");
        // Closing a store's last connection checkpoints the WAL into the database (with its
        // syncs) and deletes it: W4 would time that as the open after a kill.
        assert_eq!(
            fs::metadata(&wal).map(|m| m.len()).ok(),
            Some(left),
            "the open checkpointed or deleted the WAL the killed writer left"
        );

        let mut session = db.session().expect("a session");
        let last = id(chain::W4_WRITER, 7);
        let cs = changeset(chain::W4_RECOVER, 0, Some(last));
        session
            .commit(&cs, Main::Current { guess: None }, "test")
            .expect("commit");
        // The first session's commit went into that WAL, on the connection the open made.
        assert!(
            fs::metadata(&wal).expect("the WAL").len() > left,
            "the first commit did not append to the WAL the open recovered"
        );
        assert_eq!(session.get(&last).expect("get"), Some(8));
        assert_eq!(session.get(&cs.id).expect("get"), Some(9));
    }

    #[test]
    fn the_sessions_after_the_first_connect_anew() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let db = Sqlite::FULL.create(dir.path()).expect("create");
        let (mut first, mut second) = (db.session().expect("one"), db.session().expect("two"));
        let cs = changeset(chain::W1, 0, None);
        first
            .commit(&cs, Main::Expect(None), "test")
            .expect("commit");
        assert_eq!(second.get(&cs.id).expect("get"), Some(1));
        drop((first, second));
        let mut third = db.session().expect("three");
        assert_eq!(third.get(&cs.id).expect("get"), Some(1));
    }
}
