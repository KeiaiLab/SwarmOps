//! The engines under test, behind one interface: the native `MetaStore`, SQLite (rusqlite,
//! bundled), redb, fjall and sled. Each implements the same transactions on the same data, in
//! the layout a competent user of that engine would choose, with every committed transaction
//! durable before `commit` returns (SQLite `synchronous=NORMAL` excepted: a reference line).
//!
//! What one W1 transaction does, on every engine:
//!
//! 1. checks that the changeset is new and looks up each parent (an error if one is missing),
//!    computing the generation as 1 + the largest parent generation;
//! 2. stores the changeset under its id, numbered in insertion order (`seq`), and indexes it
//!    by delta hash;
//! 3. moves `main` to it by compare-and-swap (and records the move: the native store's
//!    journal frame holds it, the others write a reflog entry, as the M0 SQLite schema did);
//! 4. commits durably.

pub mod fjall;
pub mod native;
pub mod redb;
pub mod sled;
pub mod sqlite;

use std::fmt;
use std::path::Path;

use crate::data::{Changeset, Id};

/// A failed operation: `Busy` (a writer lock not taken in time, retried by the workloads) or
/// anything else.
#[derive(Debug)]
pub enum Error {
    Busy(String),
    Other(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Busy(e) => write!(f, "busy: {e}"),
            Error::Other(e) => f.write_str(e),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Other(e.to_string())
    }
}

/// `Error::Other` from anything printable.
pub fn other(e: impl fmt::Display) -> Error {
    Error::Other(e.to_string())
}

pub type Result<T> = std::result::Result<T, Error>;

/// What a transaction expects `main` to name before it moves it.
#[derive(Debug, Clone, Copy)]
pub enum Main {
    /// Exactly this (a single writer knows): anything else is an error.
    Expect(Option<Id>),
    /// Whatever it names when the transaction reads it, `guess` tried first: the
    /// compare-and-swap then compares against the value read inside the same transaction
    /// (many writers moving one ref).
    Current { guess: Option<Id> },
}

/// What a committed transaction found.
#[derive(Debug, Clone, Copy)]
pub struct Committed {
    /// `main` before the move.
    pub old: Option<Id>,
    /// Whether `main` named something other than the guess ([`Main::Current`]).
    pub mismatched: bool,
}

/// One engine: how to create and open its stores.
pub trait Engine: Send + Sync {
    /// The name the tables and the command line use.
    fn name(&self) -> &'static str;
    /// Whether several processes may open one store at once.
    fn multi_process(&self) -> bool;
    /// One line on what makes a commit durable, for the report.
    fn durability(&self) -> &'static str;
    /// Creates a store in the empty directory `dir` and opens it.
    fn create(&self, dir: &Path) -> Result<Box<dyn Db>>;
    /// Opens the store in `dir`.
    fn open(&self, dir: &Path) -> Result<Box<dyn Db>>;
}

/// An open store, shared by the threads of one process.
pub trait Db: Send + Sync {
    /// A handle for one thread (SQLite: a connection of its own).
    fn session(&self) -> Result<Box<dyn Session + '_>>;
    /// Makes everything durable and the files what a clean close leaves (a checkpoint, a
    /// flush), before sizes are measured.
    fn settle(&self) -> Result<()>;
}

/// One thread's handle on a store.
pub trait Session {
    /// One durable W1 transaction: inserts `cs` and moves `main` to it.
    fn commit(&mut self, cs: &Changeset, main: Main, actor: &str) -> Result<Committed>;
    /// Seeding: inserts `batch` in one transaction, then moves `main` to its last changeset.
    /// Not timed.
    fn load(&mut self, batch: &[Changeset]) -> Result<()>;
    /// A fresh read transaction (snapshot), then the generation of changeset `id`.
    fn get(&mut self, id: &Id) -> Result<Option<u64>>;
    /// A read transaction (snapshot) to run many lookups on.
    fn snapshot(&mut self) -> Result<Box<dyn Snapshot + '_>>;
    /// How many times a transaction ran again after a conflict (sled's optimistic
    /// transactions; the others serialize their writers and never do).
    fn retries(&self) -> u64 {
        0
    }
}

/// Lookups on one read transaction.
pub trait Snapshot {
    /// The generation of changeset `id`.
    fn get(&self, id: &Id) -> Result<Option<u64>>;
    /// The ids that start with `prefix`.
    fn prefix(&self, prefix: [u8; 6]) -> Result<Vec<Id>>;
    /// The ids of the changesets with delta hash `dh`, in seq order.
    fn by_delta(&self, dh: &[u8; 32]) -> Result<Vec<Id>>;
}

/// Every engine, in table order.
pub fn all() -> Vec<Box<dyn Engine>> {
    vec![
        Box::new(native::Native),
        Box::new(sqlite::Sqlite::FULL),
        Box::new(sqlite::Sqlite::NORMAL),
        Box::new(redb::Redb),
        Box::new(fjall::Fjall::FLUSHED),
        Box::new(fjall::Fjall::UNFLUSHED),
        Box::new(sled::Sled),
    ]
}

/// The engine named `name`.
pub fn by_name(name: &str) -> Option<Box<dyn Engine>> {
    all().into_iter().find(|e| e.name() == name)
}

/// The value the KV engines (redb, fjall, sled) store under a changeset id:
/// `seq u64le | generation u64le | delta_hash[32] | parents[32]…`.
pub fn encode_changeset(seq: u64, generation: u64, cs: &Changeset) -> Vec<u8> {
    let mut v = Vec::with_capacity(48 + 32 * cs.parents.len());
    v.extend_from_slice(&seq.to_le_bytes());
    v.extend_from_slice(&generation.to_le_bytes());
    v.extend_from_slice(&cs.delta_hash);
    for p in &cs.parents {
        v.extend_from_slice(p);
    }
    v
}

/// The generation in a value [`encode_changeset`] made.
pub fn decode_generation(v: &[u8]) -> Result<u64> {
    let bytes: [u8; 8] = v
        .get(8..16)
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| other("short changeset value"))?;
    Ok(u64::from_le_bytes(bytes))
}

/// A KV engine's reflog entry: `name_len u8 | name | old[32] or nothing | new[32] | ts i64le |
/// actor`.
pub fn encode_ref_move(old: Option<&Id>, new: &Id, actor: &str, ts: i64) -> Vec<u8> {
    let mut v = Vec::with_capacity(1 + 4 + 64 + 8 + actor.len());
    v.push(4);
    v.extend_from_slice(b"main");
    if let Some(old) = old {
        v.extend_from_slice(old);
    }
    v.extend_from_slice(new);
    v.extend_from_slice(&ts.to_le_bytes());
    v.extend_from_slice(actor.as_bytes());
    v
}

/// A KV engine's ref value: `target[32] | version u64le`.
pub fn encode_ref(target: &Id, version: u64) -> [u8; 40] {
    let mut v = [0; 40];
    v[..32].copy_from_slice(target);
    v[32..].copy_from_slice(&version.to_le_bytes());
    v
}

/// The target and version in a ref value [`encode_ref`] made.
pub fn decode_ref(v: &[u8]) -> Result<(Id, u64)> {
    let target: Id = v
        .get(..32)
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| other("short ref value"))?;
    let version: [u8; 8] = v
        .get(32..40)
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| other("short ref value"))?;
    Ok((target, u64::from_le_bytes(version)))
}

/// The by-delta-hash key of the KV engines: `delta_hash[32] | seq u64be`, so that one delta
/// hash's changesets sort in seq order.
pub fn delta_key(dh: &[u8; 32], seq: u64) -> [u8; 40] {
    let mut k = [0; 40];
    k[..32].copy_from_slice(dh);
    k[32..].copy_from_slice(&seq.to_be_bytes());
    k
}

/// Now, in Unix seconds, as the native store's ref moves record it.
pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// The compare-and-swap decision every engine makes once it has read `main` (`current`)
/// inside its transaction: `Ok(mismatched)` to go ahead, an error if a single writer's
/// expectation failed.
pub fn check_main(main: Main, current: Option<Id>) -> Result<bool> {
    match main {
        Main::Expect(expected) if expected == current => Ok(false),
        Main::Expect(expected) => Err(other(format!(
            "main names {:?}, not the expected {:?}",
            current.map(|c| crate::data::hex(&c[..4])),
            expected.map(|c| crate::data::hex(&c[..4]))
        ))),
        Main::Current { guess } => Ok(guess != current),
    }
}
