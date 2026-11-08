//! The write queue of `meta/queue/` (the M18 D2 design §3): writers that find `meta/lock` taken
//! leave a small request and wait; the next of them to hold the lock leads a batch of their
//! transactions, a frame each, committed with one sync of `journal.log`, and answers each.
//!
//! ```text
//!  waiter                               meta/queue/                 leader (holds meta/lock)
//!  try meta/lock: free ------------------------------------------> leads, its request in memory
//!     | taken
//!  .tmp-<rid> written, flocked, renamed  <rid>.req
//!  waits on the lock, polls <rid>.ans                               recovery, settlement, janitor
//!                                                                   own request file closed
//!                                                                   own request planned (frame 0),
//!                                                                   then .req oldest first
//!                                       <H>.batch  <-------------   manifest, before any claim
//!                                       <rid>.com  <-------------   claims (.req renamed)
//!                                                                   objects, one log sync, entries
//!  reads <rid>.ans without the lock     <rid>.ans  <-------------   answers after the commit point
//!  removes its request file, then .ans                              manifest removed, lock released
//! ```
//!
//! [`MetaStore::take_turn`] is the one entry point: it returns the answer a leader wrote
//! ([`Turn::Answered`]) or the lead ([`Turn::Lead`]), which the caller plans over its
//! [`Lead::write`] and commits ([`Lead::claim`], [`Lead::commit`]). The queue's files are
//! ephemeral IPC: never synced, read by nothing but the queue; liveness is the waiter's flock on
//! its request file. Every queue step is preceded by a [`crate::FsOps::crash_point`], so that a
//! crash test can end the process there. A queue failure never surfaces as a new error: it
//! serves fewer requests, and the next leader settles what it left.

use std::cell::Cell;
use std::collections::hash_map::RandomState;
use std::fmt;
use std::fs::{self, File, TryLockError};
use std::hash::BuildHasher;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use swarmops_core::bytes::{self, Reader};

use super::frame::{self, Record};
use super::lock::{self, Create, LockWait, MetaLock, Started};
use super::store::{CommitInfo, MetaStore, MetaWrite, Recovered};
use crate::error::{FileOp, IoFailure, StoreError};
use crate::fs_ops::{self, FsMethod};

/// The most members a batch holds, the leader's own included: a batch's FORMAT 2 entries then
/// touch at most two 512 B sectors of `journal.off`.
pub const MAX_BATCH: usize = 32;

/// How long a leader plans other members, from the end of its recovery: it plans no further
/// member once it has spent it.
pub const PLAN_BUDGET: Duration = Duration::from_millis(50);

/// The most bytes a request record takes; a longer one is never written, and its writer leads
/// alone ([`Serve::OwnOnly`]).
pub const REQUEST_MAX: usize = 64 * 1024;

/// The most bytes an answer record takes, its fixed bytes included.
pub const ANSWER_MAX: usize = 1024 * 1024;

/// The most bytes of a post-commit warning an answer carries: a longer one is cut to it.
pub const WARNING_MAX: usize = 4 * 1024;

/// How often a waiter looks for its answer.
pub const ANSWER_POLL: Duration = Duration::from_millis(1);

/// The bytes of an answer record that are not its warning or its body: magic, version, outcome,
/// frame seq, the two lengths and the CRC.
pub const ANSWER_FIXED: usize = MAGIC_LEN + 1 + 1 + 8 + 4 + 4 + CRC_LEN;

/// The most bytes a manifest record takes: per other member at most an answer and 64 bytes.
pub const MANIFEST_MAX: usize = (MAX_BATCH - 1) * (ANSWER_MAX + 64) + 64;

/// The longest answer body a member may have when it is claimed: room is left for the warning
/// only the commit can give, so no answer written after the commit point passes [`ANSWER_MAX`].
const BODY_MAX: usize = ANSWER_MAX - WARNING_MAX - ANSWER_FIXED;

/// A queue record's magic, then its version byte; its CRC32 closes it.
const MAGIC_LEN: usize = 4;
const CRC_LEN: usize = 4;
const VERSION: u8 = 1;
const REQUEST_MAGIC: [u8; MAGIC_LEN] = *b"SOQR";
const ANSWER_MAGIC: [u8; MAGIC_LEN] = *b"SOQA";
const MANIFEST_MAGIC: [u8; MAGIC_LEN] = *b"SOQM";

/// The bytes of a request record that are not its body.
const REQUEST_FIXED: usize = MAGIC_LEN + 1 + 4 + CRC_LEN;

/// How long the lead-wait hook waits for its requests at most.
const LEAD_WAIT_LIMIT: Duration = Duration::from_secs(10);

/// The most files a janitor pass removes.
const JANITOR_MAX: usize = 256;

/// What a cut warning ends with.
const ELLIPSIS: &str = "...";

/// The prefix of a temp file's name.
const TMP: &str = ".tmp-";

/// Which writers a turn serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Serve {
    /// The normal case: wait in the queue, or lead it.
    Queue,
    /// Wait for the lock as [`MetaStore::write`] does, lead only this request, serve no queue.
    OwnOnly,
}

/// A request's id in the queue: 16 hex digits of its submission time in ns, its process id
/// and 8 random hex digits. Ids order as their requests were submitted.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub struct RequestId(String);

impl RequestId {
    /// A fresh id, of now and this process.
    fn new() -> RequestId {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| {
                u64::try_from(since.as_nanos()).unwrap_or(u64::MAX)
            });
        let nonce = RandomState::new().hash_one(COUNTER.fetch_add(1, Ordering::Relaxed)) >> 32;
        RequestId(format!("{ns:016x}-{}-{nonce:08x}", std::process::id()))
    }

    /// The id `text` spells, if it is in the grammar.
    fn parse(text: &str) -> Option<RequestId> {
        let mut parts = text.split('-');
        let (time, pid, nonce) = (parts.next()?, parts.next()?, parts.next()?);
        let hex = |part: &str, len: usize| {
            part.len() == len && part.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        };
        let pid_ok = (1..=10).contains(&pid.len()) && pid.bytes().all(|b| b.is_ascii_digit());
        let ok = parts.next().is_none() && hex(time, 16) && pid_ok && hex(nonce, 8);
        ok.then(|| RequestId(text.to_owned()))
    }

    /// The id as its files spell it.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Whose request a lead plans.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Whose {
    /// The leader's own.
    Own,
    /// A waiter's.
    Other,
}

/// A request a lead plans: its id, the caller's body, and whose it is.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Request {
    /// Its id.
    pub rid: RequestId,
    /// What its writer passed to [`MetaStore::take_turn`].
    pub body: Vec<u8>,
    /// Whose it is.
    pub whose: Whose,
}

/// What a planned member uses of the transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FrameUse {
    /// The frame of this index in the transaction (frames counted from 0, in plan order).
    Frame(usize),
    /// No frame: its answer records nothing (a move refused, a changeset already present).
    NoFrame,
    /// Declined: its waiter runs it again alone ([`Serve::OwnOnly`]).
    Declined,
}

/// A member as its lead planned it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Planned {
    /// The request's id.
    pub rid: RequestId,
    /// What it uses of the transaction.
    pub frame: FrameUse,
    /// The body of its answer.
    pub body: Vec<u8>,
}

impl Planned {
    /// Request `rid`, planned to use `frame`, answered with `body`.
    pub fn new(rid: RequestId, frame: FrameUse, body: Vec<u8>) -> Planned {
        Planned { rid, frame, body }
    }
}

/// How a member's transaction ended.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Outcome {
    /// Its frame is durable at `frame_seq`.
    Committed {
        /// The seq of its frame.
        frame_seq: u64,
    },
    /// Its frame is durable at `frame_seq`; what came after failed, as `warning` says, and the
    /// next write repairs it ([`StoreError::PostCommit`]).
    PostCommit {
        /// The seq of its frame.
        frame_seq: u64,
        /// What failed after the commit point, cut to [`WARNING_MAX`] bytes.
        warning: String,
    },
    /// It recorded nothing.
    NoFrame,
    /// It was declined, nothing of it recorded: its writer runs it again alone.
    Declined,
}

/// A member's answer: how it ended and the body its lead planned.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Answer {
    /// How it ended.
    pub outcome: Outcome,
    /// The body its lead planned ([`Planned::body`]).
    pub body: Vec<u8>,
}

impl Answer {
    /// An answer of `outcome` and `body`.
    pub fn new(outcome: Outcome, body: Vec<u8>) -> Answer {
        Answer { outcome, body }
    }
}

/// What [`MetaStore::take_turn`] gave.
#[derive(Debug)]
#[non_exhaustive]
// A turn is made once per write and taken apart at once: boxing the lead would only add an
// allocation.
#[allow(clippy::large_enum_variant)]
pub enum Turn<'a> {
    /// A leader committed the request and answered it; never [`Outcome::Declined`], which
    /// the turn runs again alone.
    Answered(Answer),
    /// This writer holds the lock and leads.
    Lead(Lead<'a>),
}

/// A lead of the queue: the write transaction, the requests to plan over it, and the claim and
/// commit that answer them. Dropped without [`Lead::commit`], it commits nothing, and every
/// member it claimed is put back in the queue.
#[non_exhaustive]
pub struct Lead<'a> {
    store: &'a MetaStore,
    write: Option<MetaWrite<'a>>,
    requests: Vec<Request>,
    /// Requests of another version: claimed after the planned members, answered `Declined`.
    declined: Vec<RequestId>,
    serve: Serve,
    /// How many members the lead may plan so far, its own included ([`Lead::may_plan`]).
    granted: Cell<usize>,
    /// When the planning budget started.
    started: Instant,
    seams: Seams,
    /// The own member as claimed.
    own: Option<Planned>,
    /// What the claim did, once made.
    batch: Option<Batch>,
}

/// A claimed batch.
struct Batch {
    /// The manifest's file, if one was written.
    manifest: Option<PathBuf>,
    /// The other members claimed, in order.
    members: Vec<Planned>,
    /// Whether the manifest must stay: a claim failed in a way that leaves its member unknown.
    keep: bool,
}

// ---- The file-op seam ----

/// The queue's file operations, other than through [`crate::FsOps`]: a seam, so that tests fail
/// them.
pub(crate) trait QueueOps: Send + Sync {
    /// Makes the file at `path` (missing before), open for reading and writing.
    fn create(&self, path: &Path) -> io::Result<File>;

    /// Opens the existing file at `path`, making none.
    fn open(&self, path: &Path) -> io::Result<File>;

    /// Writes `bytes` to `file`.
    fn write(&self, file: &mut File, bytes: &[u8]) -> io::Result<()>;

    /// Tries `file`'s flock without waiting.
    fn try_lock(&self, file: &File) -> Result<(), TryLockError>;

    /// Renames `from` to `to`; `NotFound` if `from` is gone.
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;

    /// Removes the file at `path`.
    fn remove(&self, path: &Path) -> io::Result<()>;

    /// The names in directory `dir`.
    fn list(&self, dir: &Path) -> io::Result<Vec<String>>;
}

/// The real file operations.
struct RealQueue;

impl QueueOps for RealQueue {
    fn create(&self, path: &Path) -> io::Result<File> {
        // A temp a dead writer left under the same name is written afresh.
        let file = lock::open_flock_file(path, Create::IfMissing)?;
        file.set_len(0)?;
        Ok(file)
    }

    fn open(&self, path: &Path) -> io::Result<File> {
        lock::open_flock_file(path, Create::Never)
    }

    fn write(&self, file: &mut File, bytes: &[u8]) -> io::Result<()> {
        file.write_all(bytes)
    }

    fn try_lock(&self, file: &File) -> Result<(), TryLockError> {
        file.try_lock()
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        fs::rename(from, to)
    }

    fn remove(&self, path: &Path) -> io::Result<()> {
        fs::remove_file(path)
    }

    fn list(&self, dir: &Path) -> io::Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in fs::read_dir(dir)? {
            if let Ok(name) = entry?.file_name().into_string() {
                names.push(name);
            }
        }
        Ok(names)
    }
}

/// What a turn runs on: the file operations, the clock and the lead-wait count.
#[derive(Clone)]
pub(crate) struct Seams {
    pub(crate) ops: Arc<dyn QueueOps>,
    pub(crate) clock: Arc<dyn Fn() -> Instant + Send + Sync>,
    /// How many other requests a `Serve::Queue` leader waits for; `None` reads the test hook.
    pub(crate) lead_wait: Option<usize>,
}

impl Seams {
    /// The real ones.
    pub(crate) fn real() -> Seams {
        Seams {
            ops: Arc::new(RealQueue),
            clock: Arc::new(Instant::now),
            lead_wait: None,
        }
    }

    fn now(&self) -> Instant {
        (self.clock)()
    }
}

/// `SWARMOPS_TEST_LEAD_WAIT=<n>` in a build with the test hooks: panics on a malformed value.
#[cfg(feature = "test-hooks")]
fn lead_wait_from_env() -> Option<usize> {
    let value = std::env::var(fs_ops::LEAD_WAIT_ENV).ok()?;
    let n = value
        .parse()
        .unwrap_or_else(|e| panic!("{}={value:?}: {e}", fs_ops::LEAD_WAIT_ENV));
    Some(n)
}

/// A build without the test hooks ignores `SWARMOPS_TEST_LEAD_WAIT`.
#[cfg(not(feature = "test-hooks"))]
fn lead_wait_from_env() -> Option<usize> {
    None
}

// ---- The queue's files ----

/// What a queue file's name says it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Req,
    Com,
    Ans,
    Batch,
    Temp,
    TempAns,
    TempBatch,
}

impl Kind {
    /// The file of `rid` of this kind, in `dir`.
    fn path(self, dir: &Path, rid: &RequestId) -> PathBuf {
        let name = match self {
            Kind::Req => format!("{rid}.req"),
            Kind::Com => format!("{rid}.com"),
            Kind::Ans => format!("{rid}.ans"),
            Kind::Batch => format!("{rid}.batch"),
            Kind::Temp => format!("{TMP}{rid}"),
            Kind::TempAns => format!("{TMP}{rid}.ans"),
            Kind::TempBatch => format!("{TMP}{rid}.batch"),
        };
        dir.join(name)
    }

    /// The kind and id a queue file's `name` spells; `None` for any other name.
    fn of(name: &str) -> Option<(Kind, RequestId)> {
        let (temp, rest) = match name.strip_prefix(TMP) {
            Some(rest) => (true, rest),
            None => (false, name),
        };
        let (stem, suffix) = rest.split_once('.').unwrap_or((rest, ""));
        let kind = match (temp, suffix) {
            (true, "") => Kind::Temp,
            (true, "ans") => Kind::TempAns,
            (true, "batch") => Kind::TempBatch,
            (false, "req") => Kind::Req,
            (false, "com") => Kind::Com,
            (false, "ans") => Kind::Ans,
            (false, "batch") => Kind::Batch,
            _ => return None,
        };
        Some((kind, RequestId::parse(stem)?))
    }
}

/// Whether `path` exists, as `lstat` sees it.
fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

/// Whether `dir` may hold a queue: a directory, or missing (made on first use). A symlink, or
/// anything else, serves no queue.
fn usable(dir: &Path) -> bool {
    match fs::symlink_metadata(dir) {
        Ok(found) => found.is_dir(),
        Err(e) => e.kind() == io::ErrorKind::NotFound,
    }
}

/// The bytes of the regular file at `path` (opened as `meta/lock` is), if it is at most `max`
/// bytes long: `None` past it, checked before a byte is read.
fn read_bounded(ops: &dyn QueueOps, path: &Path, max: usize) -> io::Result<Option<Vec<u8>>> {
    let file = ops.open(path)?;
    read_file(&file, max)
}

/// [`read_bounded`] of an open file.
fn read_file(file: &File, max: usize) -> io::Result<Option<Vec<u8>>> {
    if file.metadata()?.len() > bytes::to_u64(max) {
        return Ok(None);
    }
    let mut buf = Vec::new();
    file.take(bytes::to_u64(max) + 1).read_to_end(&mut buf)?;
    Ok((buf.len() <= max).then_some(buf))
}

/// Writes `bytes` to the temp file `temp`, then renames it to `to`, a crash point before the
/// rename. The temp is removed if anything fails.
fn publish(seams: &Seams, store: &MetaStore, temp: &Path, to: &Path, bytes: &[u8]) -> bool {
    let ops = &*seams.ops;
    let written = ops
        .create(temp)
        .and_then(|mut file| ops.write(&mut file, bytes));
    let renamed = written.and_then(|()| {
        crash_point(store);
        ops.rename(temp, to)
    });
    if renamed.is_err() {
        let _ = ops.remove(temp);
    }
    renamed.is_ok()
}

/// The point before a queue step at which a crash test may end the process.
fn crash_point(store: &MetaStore) {
    store.fs.ops().crash_point();
}

/// Whether the file at `path`'s flock is held by a live process: its open, or a `try_lock`, that
/// errors counts as alive (only a free flock is dead). `Gone` if it is missing.
fn liveness(ops: &dyn QueueOps, path: &Path) -> Liveness {
    let file = match ops.open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Liveness::Gone,
        Err(_) => return Liveness::Alive,
    };
    match ops.try_lock(&file) {
        Ok(()) => Liveness::Dead,
        Err(TryLockError::WouldBlock | TryLockError::Error(_)) => Liveness::Alive,
    }
}

/// What [`liveness`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Liveness {
    Gone,
    Alive,
    Dead,
}

// ---- Records ----

/// Why a queue record does not read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unread {
    /// Short, its CRC wrong, or not the record it should be.
    Torn,
    /// A record of another version.
    OtherVersion,
}

/// `magic`, the version, `payload`, then the CRC of all of it.
fn seal(magic: [u8; MAGIC_LEN], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(MAGIC_LEN + 1 + payload.len() + CRC_LEN);
    out.extend_from_slice(&magic);
    out.push(VERSION);
    out.extend_from_slice(payload);
    let crc = crc32fast::hash(&out);
    out.extend_from_slice(&crc.to_le_bytes());
    out
}

/// The payload of a record `seal` made with `magic`.
fn unseal(magic: [u8; MAGIC_LEN], record: &[u8]) -> Result<&[u8], Unread> {
    let Some(body_len) = record.len().checked_sub(MAGIC_LEN + 1 + CRC_LEN) else {
        return Err(Unread::Torn);
    };
    let (sealed, crc) = record.split_at(record.len() - CRC_LEN);
    if crc32fast::hash(sealed).to_le_bytes() != crc || sealed[..MAGIC_LEN] != magic {
        return Err(Unread::Torn);
    }
    if sealed[MAGIC_LEN] != VERSION {
        return Err(Unread::OtherVersion);
    }
    Ok(&sealed[MAGIC_LEN + 1..MAGIC_LEN + 1 + body_len])
}

/// A reader of a record's payload, every error [`Unread::Torn`].
fn reader(payload: &[u8]) -> Reader<'_, Unread> {
    Reader::new(payload, |_| Unread::Torn)
}

fn u32_of(reader: &mut Reader<'_, Unread>) -> Result<u32, Unread> {
    reader.array().map(u32::from_le_bytes)
}

fn len_of(reader: &mut Reader<'_, Unread>) -> Result<usize, Unread> {
    usize::try_from(u32_of(reader)?).map_err(|_| Unread::Torn)
}

/// `n` as the `u32le` a record holds; every length here is bounded far below it.
fn u32_le(n: usize) -> [u8; 4] {
    u32::try_from(n).unwrap_or(u32::MAX).to_le_bytes()
}

/// A request record: its body.
fn encode_request(body: &[u8]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(4 + body.len());
    payload.extend_from_slice(&u32_le(body.len()));
    payload.extend_from_slice(body);
    seal(REQUEST_MAGIC, &payload)
}

fn decode_request(record: &[u8]) -> Result<Vec<u8>, Unread> {
    let mut r = reader(unseal(REQUEST_MAGIC, record)?);
    let len = len_of(&mut r)?;
    let body = r.bytes(len)?.to_vec();
    r.rest().is_empty().then_some(body).ok_or(Unread::Torn)
}

/// The outcome tags of an answer record.
const COMMITTED: u8 = 1;
const POST_COMMIT: u8 = 2;
const NO_FRAME: u8 = 3;
const DECLINED: u8 = 4;

/// An answer record.
fn encode_answer(answer: &Answer) -> Vec<u8> {
    let (tag, seq, warning) = match &answer.outcome {
        Outcome::Committed { frame_seq } => (COMMITTED, *frame_seq, ""),
        Outcome::PostCommit { frame_seq, warning } => (POST_COMMIT, *frame_seq, warning.as_str()),
        Outcome::NoFrame => (NO_FRAME, 0, ""),
        Outcome::Declined => (DECLINED, 0, ""),
    };
    let mut payload = Vec::with_capacity(ANSWER_FIXED + warning.len() + answer.body.len());
    payload.push(tag);
    payload.extend_from_slice(&seq.to_le_bytes());
    payload.extend_from_slice(&u32_le(warning.len()));
    payload.extend_from_slice(warning.as_bytes());
    payload.extend_from_slice(&u32_le(answer.body.len()));
    payload.extend_from_slice(&answer.body);
    seal(ANSWER_MAGIC, &payload)
}

fn decode_answer(record: &[u8]) -> Result<Answer, Unread> {
    let mut r = reader(unseal(ANSWER_MAGIC, record)?);
    let tag = r.u8()?;
    let frame_seq = r.u64_le()?;
    let len = len_of(&mut r)?;
    let warning = String::from_utf8(r.bytes(len)?.to_vec()).map_err(|_| Unread::Torn)?;
    let len = len_of(&mut r)?;
    let body = r.bytes(len)?.to_vec();
    let outcome = match tag {
        COMMITTED => Outcome::Committed { frame_seq },
        POST_COMMIT => Outcome::PostCommit { frame_seq, warning },
        NO_FRAME => Outcome::NoFrame,
        DECLINED => Outcome::Declined,
        _ => return Err(Unread::Torn),
    };
    r.rest()
        .is_empty()
        .then_some(Answer { outcome, body })
        .ok_or(Unread::Torn)
}

/// A member a manifest names: its id, its frame's index and payload hash if it has one, and
/// the body of its answer.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Named {
    rid: RequestId,
    frame: Option<(u32, [u8; 32])>,
    body: Vec<u8>,
}

/// A manifest: the seq of the batch's first frame and the other members.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Manifest {
    first_seq: u64,
    members: Vec<Named>,
}

fn encode_manifest(manifest: &Manifest) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&manifest.first_seq.to_le_bytes());
    payload.push(u8::try_from(manifest.members.len()).unwrap_or(u8::MAX));
    for member in &manifest.members {
        let rid = member.rid.as_str().as_bytes();
        payload.push(u8::try_from(rid.len()).unwrap_or(u8::MAX));
        payload.extend_from_slice(rid);
        let (has, index, hash) = match member.frame {
            Some((index, hash)) => (1, index, hash),
            None => (0, 0, [0; 32]),
        };
        payload.push(has);
        payload.extend_from_slice(&index.to_le_bytes());
        payload.extend_from_slice(&hash);
        payload.extend_from_slice(&u32_le(member.body.len()));
        payload.extend_from_slice(&member.body);
    }
    seal(MANIFEST_MAGIC, &payload)
}

fn decode_manifest(record: &[u8]) -> Result<Manifest, Unread> {
    let mut r = reader(unseal(MANIFEST_MAGIC, record)?);
    let first_seq = r.u64_le()?;
    let count = r.u8()?;
    let mut members = Vec::with_capacity(usize::from(count));
    for _ in 0..count {
        let len = usize::from(r.u8()?);
        let rid = std::str::from_utf8(r.bytes(len)?).map_err(|_| Unread::Torn)?;
        let rid = RequestId::parse(rid).ok_or(Unread::Torn)?;
        let has = r.u8()?;
        let index = u32_of(&mut r)?;
        let hash = r.array::<32>()?;
        let len = len_of(&mut r)?;
        let body = r.bytes(len)?.to_vec();
        let frame = match has {
            0 => None,
            1 => Some((index, hash)),
            _ => return Err(Unread::Torn),
        };
        members.push(Named { rid, frame, body });
    }
    r.rest()
        .is_empty()
        .then_some(Manifest { first_seq, members })
        .ok_or(Unread::Torn)
}

/// `warning` cut to at most [`WARNING_MAX`] bytes, at a char boundary, [`ELLIPSIS`] appended
/// when cut.
fn cut_warning(warning: &str) -> String {
    if warning.len() <= WARNING_MAX {
        return warning.to_owned();
    }
    let mut end = WARNING_MAX - ELLIPSIS.len();
    while !warning.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{ELLIPSIS}", &warning[..end])
}

/// The blake3 of the payload of a frame of `records` in `store`'s format, as it is appended at
/// any place (the place is no part of the payload).
fn payload_hash(store: &MetaStore, records: &[Record]) -> Result<[u8; 32], StoreError> {
    let format = store.layout.format();
    let encoded = frame::encode_frame_in(format, records, 0, 0)?;
    Ok(*blake3::hash(&encoded[frame::header_len(format)..]).as_bytes())
}

/// A queue file that does not read as it must: [`StoreError::Io`] naming it.
fn unreadable(store: &MetaStore, op: FileOp, path: &Path, why: &str) -> StoreError {
    let file = fs_ops::store_relative(store.layout.root(), path);
    IoFailure::new(
        op,
        file,
        io::Error::new(io::ErrorKind::InvalidData, why.to_owned()),
    )
    .into()
}

// ---- The waiter ----

/// A waiter's request file: open and flocked, under `<rid>.req` or, once claimed, `.com`.
struct OwnFile {
    _file: File,
}

impl MetaStore {
    /// Takes a turn at the write queue with `body`, a request of the caller's own encoding: see
    /// the module documentation and the M18 D2 design §3.2. With [`Serve::Queue`], a writer that
    /// finds the lock free leads at once, writing no queue file; one that finds it taken leaves
    /// a request in `meta/queue/` and waits, for the lock or for its answer. A request past
    /// [`REQUEST_MAX`], or a `meta/queue` that is not a directory, takes [`Serve::OwnOnly`]:
    /// wait for the lock as [`MetaStore::write`] does and lead alone.
    ///
    /// [`StoreError::Busy`] when the request is still unclaimed at the deadline
    /// ([`MetaStore::with_lock_timeout`]): nothing of it was recorded. A claimed request waits
    /// past the deadline for its answer. A declined request runs again alone, waiting only for
    /// what is left of the first deadline. An answer that cannot be read is [`StoreError::Io`]
    /// naming its file, and the request is never run again.
    pub fn take_turn(&self, body: &[u8], serve: Serve) -> Result<Turn<'_>, StoreError> {
        self.take_turn_with(body, serve, Seams::real())
    }

    /// [`MetaStore::take_turn`] on `seams`.
    pub(crate) fn take_turn_with(
        &self,
        body: &[u8],
        serve: Serve,
        seams: Seams,
    ) -> Result<Turn<'_>, StoreError> {
        let deadline = seams.now() + self.lock_timeout();
        let own = RequestId::new();
        let dir = self.layout.queue_dir();
        let queued = serve == Serve::Queue
            && REQUEST_FIXED + body.len() <= REQUEST_MAX
            && usable(dir.as_std_path());
        if !queued {
            return self.own_only(own, body, None, deadline, seams);
        }

        // 1. The zero-timeout try: an uncontended writer leads with its request in memory.
        let wait = match MetaLock::start(&self.layout)? {
            Started::Held(lock) => return self.lead_queue(lock, own, body, None, seams),
            Started::Waiting(wait) => wait,
        };

        // 2. The request in place, or else a wait of its own.
        let Some(file) = self.enqueue(&own, &encode_request(body), &seams) else {
            let lock = locked(&wait, remaining(&seams, deadline))?;
            let write = self.write_locked(lock)?;
            return Ok(Turn::Lead(Lead::new(
                self,
                write,
                own,
                body,
                Serve::OwnOnly,
                seams,
            )));
        };
        self.await_answer(wait, own, body, file, deadline, seams)
    }

    /// A turn that serves no queue: waits for the lock until `deadline`, on `wait` if it has
    /// one (a declined rerun's first wait: a new one would leave its helper parked), then leads
    /// alone.
    fn own_only(
        &self,
        own: RequestId,
        body: &[u8],
        wait: Option<LockWait>,
        deadline: Instant,
        seams: Seams,
    ) -> Result<Turn<'_>, StoreError> {
        let lock = match wait {
            Some(wait) => locked(&wait, remaining(&seams, deadline))?,
            None => match MetaLock::start(&self.layout)? {
                Started::Held(lock) => lock,
                Started::Waiting(wait) => locked(&wait, remaining(&seams, deadline))?,
            },
        };
        let write = self.write_locked(lock)?;
        Ok(Turn::Lead(Lead::new(
            self,
            write,
            own,
            body,
            Serve::OwnOnly,
            seams,
        )))
    }

    /// Writes the request `record` as `.tmp-<rid>`, flocks it and renames it `<rid>.req`;
    /// `None`, the temp removed, if any of that fails.
    fn enqueue(&self, own: &RequestId, record: &[u8], seams: &Seams) -> Option<OwnFile> {
        let dir = self.layout.queue_dir();
        let dir = dir.as_std_path();
        if let Err(e) = fs::create_dir(dir)
            && e.kind() != io::ErrorKind::AlreadyExists
        {
            return None;
        }
        if !usable(dir) {
            return None;
        }
        let ops = &*seams.ops;
        let temp = Kind::Temp.path(dir, own);
        let placed = ops.create(&temp).and_then(|mut file| {
            ops.try_lock(&file).map_err(io::Error::from)?;
            ops.write(&mut file, record)?;
            crash_point(self);
            ops.rename(&temp, &Kind::Req.path(dir, own))?;
            Ok(file)
        });
        if placed.is_err() {
            let _ = ops.remove(&temp);
        }
        placed.ok().map(|file| OwnFile { _file: file })
    }

    /// Waits for the lock or for the answer to request `own` (steps 5, 15–17 of §3.1).
    fn await_answer(
        &self,
        wait: LockWait,
        own: RequestId,
        body: &[u8],
        file: OwnFile,
        deadline: Instant,
        seams: Seams,
    ) -> Result<Turn<'_>, StoreError> {
        let dir = self.layout.queue_dir();
        let dir = dir.as_std_path();
        loop {
            // 1. The lock: lead, or settle the leader that claimed this request.
            if let Some(got) = wait.wait(ANSWER_POLL) {
                let lock = got.inspect_err(|_| self.withdraw(&own, &seams))?;
                return self.lead_queue(lock, own, body, Some((file, deadline)), seams);
            }

            // 2. The answer, read without the lock; a declined rerun waits on this wait.
            if exists(&Kind::Ans.path(dir, &own)) {
                return self.answered(&own, body, file, Some(wait), deadline, seams);
            }

            // 3. The deadline: withdrawn while unclaimed, or claimed and waited for.
            if seams.now() >= deadline {
                crash_point(self);
                if seams.ops.remove(&Kind::Req.path(dir, &own)).is_ok() {
                    return Err(StoreError::Busy);
                }
            }
        }
    }

    /// Removes request `own`'s file, wherever it is, best effort.
    fn withdraw(&self, own: &RequestId, seams: &Seams) {
        let dir = self.layout.queue_dir();
        for kind in [Kind::Req, Kind::Com] {
            crash_point(self);
            if seams.ops.remove(&kind.path(dir.as_std_path(), own)).is_ok() {
                return;
            }
        }
    }

    /// Reads the answer to request `own` without the lock, removes the request file, closes
    /// it, then removes the answer (step 15); a declined request runs again alone, on `wait`
    /// if the request still waits for the lock.
    fn answered(
        &self,
        own: &RequestId,
        body: &[u8],
        file: OwnFile,
        wait: Option<LockWait>,
        deadline: Instant,
        seams: Seams,
    ) -> Result<Turn<'_>, StoreError> {
        let dir = self.layout.queue_dir();
        let path = Kind::Ans.path(dir.as_std_path(), own);
        let read = read_bounded(&*seams.ops, &path, ANSWER_MAX);

        // 1. The request file first, closed, then the answer (D11).
        self.withdraw(own, &seams);
        drop(file);
        crash_point(self);
        let _ = seams.ops.remove(&path);

        // 2. What it says.
        let op = FileOp::Fs(FsMethod::Read);
        let answer = match read {
            Ok(Some(record)) => decode_answer(&record)
                .map_err(|_| unreadable(self, op, &path, "not an answer this build reads"))?,
            Ok(None) => return Err(unreadable(self, op, &path, "longer than an answer")),
            Err(e) => {
                let file = fs_ops::store_relative(self.layout.root(), &path);
                return Err(IoFailure::new(op, file, e).into());
            }
        };
        if answer.outcome == Outcome::Declined {
            return self.own_only(own.clone(), body, wait, deadline, seams);
        }
        Ok(Turn::Answered(answer))
    }

    /// Leads with `lock` held: recovery, settlement, the own request file closed (or the answer
    /// settlement gave it), the janitor, then the requests to plan (steps 6–9 of §3.1).
    fn lead_queue(
        &self,
        lock: MetaLock,
        own: RequestId,
        body: &[u8],
        waiter: Option<(OwnFile, Instant)>,
        seams: Seams,
    ) -> Result<Turn<'_>, StoreError> {
        let dir = self.layout.queue_dir();
        let dir = dir.as_std_path();
        let write = match self.write_locked(lock) {
            Ok(write) => write,
            Err(e) => {
                if waiter.is_some() {
                    self.withdraw(&own, &seams);
                }
                return Err(e);
            }
        };

        // 1. What a dead leader left (step 6).
        settle(self, &write, &seams);

        // 2. The own request file: answered by that settlement, or removed and closed (step 7).
        if let Some((file, deadline)) = waiter {
            if exists(&Kind::Ans.path(dir, &own)) {
                drop(write);
                return self.answered(&own, body, file, None, deadline, seams);
            }
            let req = Kind::Req.path(dir, &own);
            if !exists(&req) {
                let com = Kind::Com.path(dir, &own);
                let (at, why) = if exists(&com) {
                    (com, "claimed, and no readable manifest names it")
                } else {
                    (req, "the request is under no name")
                };
                return Err(unreadable(self, FileOp::Stat, &at, why));
            }
            crash_point(self);
            let _ = seams.ops.remove(&req);
            drop(file);
        }

        // 3. The janitor, then the requests: none if a manifest is left (step 8, D10).
        janitor(self, &seams);
        let mut lead = Lead::new(self, write, own, body, Serve::Queue, seams);
        if manifests(&*lead.seams.ops, dir).is_empty() {
            lead.wait_hook();
            lead.gather();
        }
        lead.started = lead.seams.now();
        Ok(Turn::Lead(lead))
    }
}

/// What is left of `deadline`.
fn remaining(seams: &Seams, deadline: Instant) -> Duration {
    deadline.saturating_duration_since(seams.now())
}

/// The lock from `wait` within `limit`, else [`StoreError::Busy`].
fn locked(wait: &LockWait, limit: Duration) -> Result<MetaLock, StoreError> {
    wait.wait(limit).unwrap_or(Err(StoreError::Busy))
}

/// The manifests in `dir`, oldest first.
fn manifests(ops: &dyn QueueOps, dir: &Path) -> Vec<RequestId> {
    let mut found: Vec<RequestId> = (ops.list(dir).unwrap_or_default().iter())
        .filter_map(|name| Kind::of(name))
        .filter(|(kind, _)| *kind == Kind::Batch)
        .map(|(_, rid)| rid)
        .collect();
    found.sort();
    found
}

// ---- Settlement and janitor ----

/// Settles every manifest a dead or failed leader left (§3.5), under the lock, after recovery:
/// each member answered from it, put back as `.req`, or left; a manifest whose members are all
/// settled is removed.
fn settle(store: &MetaStore, write: &MetaWrite<'_>, seams: &Seams) {
    let dir = store.layout.queue_dir();
    let dir = dir.as_std_path();
    for leader in manifests(&*seams.ops, dir) {
        let path = Kind::Batch.path(dir, &leader);
        let Ok(Some(record)) = read_bounded(&*seams.ops, &path, MANIFEST_MAX) else {
            continue;
        };
        let Ok(manifest) = decode_manifest(&record) else {
            continue;
        };
        let mut settled = true;
        for member in &manifest.members {
            settled &= settle_member(store, write, seams, manifest.first_seq, member);
        }
        if settled {
            crash_point(store);
            let _ = seams.ops.remove(&path);
        }
    }
}

/// Settles one member a manifest names; whether it is settled.
fn settle_member(
    store: &MetaStore,
    write: &MetaWrite<'_>,
    seams: &Seams,
    first_seq: u64,
    member: &Named,
) -> bool {
    let dir = store.layout.queue_dir();
    let dir = dir.as_std_path();
    let (com, rid) = (Kind::Com.path(dir, &member.rid), &member.rid);
    if exists(&Kind::Ans.path(dir, rid)) {
        return true;
    }
    if !exists(&com) {
        // `.req`: never claimed, left alone; no file: gone.
        return true;
    }

    // 1. Committed: its frame listed, its payload the one the manifest names.
    if let Some((index, hash)) = member.frame {
        let seq = first_seq + u64::from(index);
        let listed = write
            .listed_frames(seq, 1)
            .ok()
            .and_then(|mut frames| frames.pop());
        let matches = listed
            .filter(|frame| frame.seq == seq)
            .and_then(|frame| payload_hash(store, &frame.records).ok())
            .is_some_and(|found| found == hash);
        if matches {
            let answer = Answer::new(Outcome::Committed { frame_seq: seq }, member.body.clone());
            let temp = Kind::TempAns.path(dir, rid);
            let to = Kind::Ans.path(dir, rid);
            return publish(seams, store, &temp, &to, &encode_answer(&answer));
        }
    }

    // 2. Else nothing of it was recorded: back in the queue.
    crash_point(store);
    match seams.ops.rename(&com, &Kind::Req.path(dir, rid)) {
        Ok(()) => true,
        Err(e) => e.kind() == io::ErrorKind::NotFound,
    }
}

/// Removes what dead writers and leaders left in `meta/queue/` (§3.5), at most [`JANITOR_MAX`]
/// files a pass: names in the queue's grammar only.
fn janitor(store: &MetaStore, seams: &Seams) {
    let dir = store.layout.queue_dir();
    let dir = dir.as_std_path();
    let ops = &*seams.ops;
    let Ok(names) = ops.list(dir) else {
        return;
    };
    let files: Vec<(Kind, RequestId)> = names.iter().filter_map(|name| Kind::of(name)).collect();
    let alive =
        |kind: Kind, rid: &RequestId| liveness(ops, &kind.path(dir, rid)) == Liveness::Alive;
    let owned = |rid: &RequestId| alive(Kind::Req, rid) || alive(Kind::Com, rid);
    let waited_on = files.iter().any(|(kind, rid)| {
        *kind == Kind::Com && !exists(&Kind::Ans.path(dir, rid)) && alive(Kind::Com, rid)
    });
    let mut removed = 0;
    for (kind, rid) in &files {
        if removed == JANITOR_MAX {
            return;
        }
        let path = kind.path(dir, rid);
        let dead = match kind {
            Kind::Temp | Kind::Req | Kind::Com => liveness(ops, &path) == Liveness::Dead,
            Kind::TempAns | Kind::TempBatch => true,
            Kind::Ans => !owned(rid),
            Kind::Batch => !waited_on && unreadable_manifest(ops, &path),
        };
        if dead {
            crash_point(store);
            removed += usize::from(ops.remove(&path).is_ok());
        }
    }
}

/// Whether the manifest at `path` cannot be read or settled.
fn unreadable_manifest(ops: &dyn QueueOps, path: &Path) -> bool {
    match read_bounded(ops, path, MANIFEST_MAX) {
        Ok(Some(record)) => decode_manifest(&record).is_err(),
        Ok(None) => true,
        Err(e) => e.kind() != io::ErrorKind::NotFound,
    }
}

// ---- The lead ----

impl<'a> Lead<'a> {
    fn new(
        store: &'a MetaStore,
        write: MetaWrite<'a>,
        own: RequestId,
        body: &[u8],
        serve: Serve,
        seams: Seams,
    ) -> Lead<'a> {
        let started = seams.now();
        Lead {
            store,
            write: Some(write),
            requests: vec![Request {
                rid: own,
                body: body.to_vec(),
                whose: Whose::Own,
            }],
            declined: Vec::new(),
            serve,
            granted: Cell::new(1),
            started,
            seams,
            own: None,
            batch: None,
        }
    }

    /// The test hook: waits up to 10 s for as many other pending requests as it asks for.
    fn wait_hook(&self) {
        let Some(wanted) = self.seams.lead_wait.or_else(lead_wait_from_env) else {
            return;
        };
        let dir = self.store.layout.queue_dir();
        let limit = Instant::now() + LEAD_WAIT_LIMIT;
        while Instant::now() < limit {
            let pending = (self
                .seams
                .ops
                .list(dir.as_std_path())
                .unwrap_or_default()
                .iter())
            .filter(|name| matches!(Kind::of(name), Some((Kind::Req, _))))
            .count();
            if pending >= wanted {
                return;
            }
            thread::sleep(ANSWER_POLL);
        }
    }

    /// The pending requests, oldest first, after the own one: alive, readable and of this
    /// version; a dead one removed, never planned; one of another version to be declined.
    fn gather(&mut self) {
        let dir = self.store.layout.queue_dir();
        let dir = dir.as_std_path();
        let ops = Arc::clone(&self.seams.ops);
        let mut pending: Vec<RequestId> = (ops.list(dir).unwrap_or_default().iter())
            .filter_map(|name| Kind::of(name))
            .filter(|(kind, rid)| *kind == Kind::Req && *rid != self.requests[0].rid)
            .map(|(_, rid)| rid)
            .collect();
        pending.sort();
        for rid in pending {
            if self.requests.len() + self.declined.len() >= MAX_BATCH {
                return;
            }
            let path = Kind::Req.path(dir, &rid);
            let Ok(file) = ops.open(&path) else {
                continue;
            };
            if ops.try_lock(&file).is_ok() {
                // Dead: its writer's flock is gone.
                crash_point(self.store);
                let _ = ops.remove(&path);
                continue;
            }
            match read_file(&file, REQUEST_MAX)
                .ok()
                .flatten()
                .map(|r| decode_request(&r))
            {
                Some(Ok(body)) => self.requests.push(Request {
                    rid,
                    body,
                    whose: Whose::Other,
                }),
                Some(Err(Unread::OtherVersion)) => self.declined.push(rid),
                Some(Err(Unread::Torn)) | None => {}
            }
        }
    }

    /// The requests to plan, the own one first, then the waiters' oldest first.
    pub fn requests(&self) -> &[Request] {
        &self.requests
    }

    /// Whether the lead may plan the next request of [`Lead::requests`] after those it has
    /// planned: while the batch has room ([`MAX_BATCH`]) and the planning budget
    /// ([`PLAN_BUDGET`]) lasts. Each `true` grants one more; the own request needs none.
    pub fn may_plan(&self) -> bool {
        let granted = self.granted.get();
        let room = granted < self.requests.len() && granted < MAX_BATCH;
        let time = self.seams.now().saturating_duration_since(self.started) < PLAN_BUDGET;
        if !(room && time) {
            return false;
        }
        self.granted.set(granted + 1);
        true
    }

    /// The write transaction to plan the members over, a frame each
    /// ([`MetaWrite::begin_frame`]).
    pub fn write(&mut self) -> &mut MetaWrite<'a> {
        self.write
            .as_mut()
            .expect("a lead holds its write until it commits")
    }

    /// What the recovery that opened the lead repaired.
    pub fn recovered(&self) -> Recovered {
        (self.write.as_ref()).map_or_else(Recovered::default, MetaWrite::recovered)
    }

    /// Claims the members `plans` names (§3.1 step 10): the own request first, then the
    /// others in [`Lead::requests`]'s order, each with the frame it uses (frames numbered in
    /// order, from 0) and its answer body. A member whose body passes what an answer leaves room
    /// for is declined, and with it every later member if it had a frame. The open frame is
    /// closed; then, if any other member is left, the manifest is written and each member
    /// claimed in order, the batch ending at the first that is gone (withdrawn). The frames of
    /// the members left out are dropped ([`MetaWrite::keep_frames`]). Returns how many members
    /// the batch holds, the own included. [`StoreError::BadCall`] for plans that do not match
    /// the requests or the frames.
    pub fn claim(&mut self, plans: Vec<Planned>) -> Result<usize, StoreError> {
        let mut plans = self.check(plans)?;
        decline_oversized(&mut plans);

        // 1. Every frame closed; the declined members' dropped.
        let frames = plans
            .iter()
            .filter(|p| matches!(p.frame, FrameUse::Frame(_)))
            .count();
        let write = self.write();
        write.keep_frames(frames)?;
        if write.closed_frames() != frames {
            return Err(StoreError::BadCall(format!(
                "a lead planned {} frames, its plans name {frames}",
                write.closed_frames()
            )));
        }
        let own = plans.remove(0);
        let own_frames = usize::from(matches!(own.frame, FrameUse::Frame(_)));
        self.own = Some(own);
        let mut others = plans;
        let declined = std::mem::take(&mut self.declined);
        others.extend(
            declined
                .into_iter()
                .map(|rid| Planned::new(rid, FrameUse::Declined, Vec::new())),
        );
        if others.is_empty() || self.serve == Serve::OwnOnly {
            self.write().keep_frames(own_frames)?;
            return Ok(1);
        }

        // 2. The manifest, before the first claim.
        let Some(manifest) = self.write_manifest(&others)? else {
            self.write().keep_frames(own_frames)?;
            return Ok(1);
        };

        // 3. The claims, in order, to the first that fails.
        let dir = self.store.layout.queue_dir();
        let dir = dir.as_std_path();
        let mut batch = Batch {
            manifest: Some(manifest),
            members: Vec::new(),
            keep: false,
        };
        for member in others {
            crash_point(self.store);
            let claimed = self.seams.ops.rename(
                &Kind::Req.path(dir, &member.rid),
                &Kind::Com.path(dir, &member.rid),
            );
            if let Err(e) = claimed {
                batch.keep = e.kind() != io::ErrorKind::NotFound;
                break;
            }
            batch.members.push(member);
        }
        let kept = own_frames
            + (batch.members.iter())
                .filter(|m| matches!(m.frame, FrameUse::Frame(_)))
                .count();
        let held = 1 + batch.members.len();
        self.batch = Some(batch);
        self.write().keep_frames(kept)?;
        Ok(held)
    }

    /// `plans` checked against the requests and the frames: the own first, the rest in order,
    /// no more than were granted, frame indices in order from 0.
    fn check(&self, plans: Vec<Planned>) -> Result<Vec<Planned>, StoreError> {
        let bad = |why: &str| Err(StoreError::BadCall(format!("a lead's plans: {why}")));
        if self.batch.is_some() || self.own.is_some() {
            return bad("claimed already");
        }
        if plans.is_empty() || plans.len() > self.granted.get() {
            return bad("not one per granted request");
        }
        if (plans.iter().zip(&self.requests)).any(|(plan, request)| plan.rid != request.rid) {
            return bad("not in the requests' order");
        }
        let mut next = 0;
        for plan in &plans {
            if let FrameUse::Frame(index) = plan.frame {
                if index != next {
                    return bad("frames out of order");
                }
                next += 1;
            }
        }
        Ok(plans)
    }

    /// Writes the manifest of the batch of `others`, if `meta/queue/` holds none; its path, or
    /// `None` if it holds one or the write failed (the lead then serves no queue).
    fn write_manifest(&mut self, others: &[Planned]) -> Result<Option<PathBuf>, StoreError> {
        let write = self
            .write
            .as_ref()
            .expect("a lead holds its write until it commits");
        let first_seq = write.next_seq()?;
        let frames = write.frames_from(first_seq)?;
        let mut members = Vec::with_capacity(others.len());
        for member in others {
            let frame = match member.frame {
                FrameUse::Frame(index) => {
                    let Some(frame) = frames.get(index) else {
                        return Err(StoreError::BadCall(format!("no frame {index}")));
                    };
                    let index = u32::try_from(index).unwrap_or(u32::MAX);
                    Some((index, payload_hash(self.store, &frame.records)?))
                }
                FrameUse::NoFrame | FrameUse::Declined => None,
            };
            members.push(Named {
                rid: member.rid.clone(),
                frame,
                body: member.body.clone(),
            });
        }
        let record = encode_manifest(&Manifest { first_seq, members });
        let dir = self.store.layout.queue_dir();
        let dir = dir.as_std_path();
        if !manifests(&*self.seams.ops, dir).is_empty() {
            return Ok(None);
        }
        let own = &self
            .own
            .as_ref()
            .expect("the own member is claimed first")
            .rid;
        let (temp, path) = (Kind::TempBatch.path(dir, own), Kind::Batch.path(dir, own));
        Ok(publish(&self.seams, self.store, &temp, &path, &record).then_some(path))
    }

    /// Commits the batch (§3.1 steps 12–14): the frames, with one append, then an answer to each
    /// other member, the manifest removed once every one is written, and the lock released.
    /// Returns the own answer, or the commit's error: [`StoreError::NotCommitted`] (every other
    /// member is then declined) or [`StoreError::PostCommit`].
    pub fn commit(mut self) -> Result<Answer, StoreError> {
        let mut write = self
            .write
            .take()
            .expect("a lead holds its write until it commits");
        let lock = write.take_lock();
        let committed = write.commit_frames();

        // 1. Each other member's answer, after the commit point.
        let batch = self.batch.take();
        if let Some(batch) = &batch {
            self.answer_all(batch, &committed);
        }
        drop(lock);

        // 2. The own.
        let infos = committed?;
        let own = self.own.take();
        let (frame, body) = own.map_or((FrameUse::Frame(0), Vec::new()), |own| {
            (own.frame, own.body)
        });
        let outcome = match frame {
            FrameUse::Frame(index) => infos
                .get(index)
                .filter(|info| info.frame_crc.is_some())
                .map_or(Outcome::NoFrame, |info| Outcome::Committed {
                    frame_seq: info.frame_seq,
                }),
            FrameUse::NoFrame => Outcome::NoFrame,
            FrameUse::Declined => Outcome::Declined,
        };
        Ok(Answer::new(outcome, body))
    }

    /// Writes every member's answer from `committed`; then removes the manifest, unless an
    /// answer failed or the claim kept it.
    fn answer_all(&self, batch: &Batch, committed: &Result<Vec<CommitInfo>, StoreError>) {
        let dir = self.store.layout.queue_dir();
        let dir = dir.as_std_path();
        let mut answered = true;
        for member in &batch.members {
            let outcome = outcome_of(member.frame, committed);
            let body = match outcome {
                Outcome::Declined => Vec::new(),
                _ => member.body.clone(),
            };
            let record = encode_answer(&Answer::new(outcome, body));
            let (temp, to) = (
                Kind::TempAns.path(dir, &member.rid),
                Kind::Ans.path(dir, &member.rid),
            );
            answered &= publish(&self.seams, self.store, &temp, &to, &record);
        }
        if let Some(manifest) = &batch.manifest
            && answered
            && !batch.keep
        {
            crash_point(self.store);
            let _ = self.seams.ops.remove(manifest);
        }
    }
}

/// A member's outcome once the batch's commit gave `committed`.
fn outcome_of(frame: FrameUse, committed: &Result<Vec<CommitInfo>, StoreError>) -> Outcome {
    match (frame, committed) {
        (FrameUse::Frame(index), Ok(infos)) => {
            infos
                .get(index)
                .map_or(Outcome::Declined, |info| Outcome::Committed {
                    frame_seq: info.frame_seq,
                })
        }
        (
            FrameUse::Frame(index),
            Err(StoreError::PostCommit {
                frame_seq, source, ..
            }),
        ) => Outcome::PostCommit {
            frame_seq: frame_seq + bytes::to_u64(index),
            warning: cut_warning(&source.to_string()),
        },
        (FrameUse::NoFrame, Ok(_) | Err(StoreError::PostCommit { .. })) => Outcome::NoFrame,
        // Declined, or not committed: nothing of the member was recorded (D5).
        (FrameUse::Declined, _) | (FrameUse::Frame(_) | FrameUse::NoFrame, Err(_)) => {
            Outcome::Declined
        }
    }
}

/// Declines the members after the own whose answer body passes [`BODY_MAX`]: such a member
/// with a frame takes every later member with it, since they were planned on its frame.
fn decline_oversized(plans: &mut [Planned]) {
    let mut cut = false;
    for plan in plans.iter_mut().skip(1) {
        let over = plan.body.len() > BODY_MAX;
        cut |= over && matches!(plan.frame, FrameUse::Frame(_));
        if over || cut {
            plan.frame = FrameUse::Declined;
            plan.body = Vec::new();
        }
    }
}

impl Drop for Lead<'_> {
    /// A lead dropped without its commit committed nothing: every member it claimed goes back to
    /// the queue, and the manifest is removed if every one did.
    fn drop(&mut self) {
        let Some(batch) = self.batch.take() else {
            return;
        };
        let dir = self.store.layout.queue_dir();
        let dir = dir.as_std_path();
        let mut back = !batch.keep;
        for member in &batch.members {
            crash_point(self.store);
            let renamed = self.seams.ops.rename(
                &Kind::Com.path(dir, &member.rid),
                &Kind::Req.path(dir, &member.rid),
            );
            back &= match renamed {
                Ok(()) => true,
                Err(e) => e.kind() == io::ErrorKind::NotFound,
            };
        }
        if let Some(manifest) = &batch.manifest
            && back
        {
            crash_point(self.store);
            let _ = self.seams.ops.remove(manifest);
        }
    }
}

impl fmt::Debug for Lead<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Lead")
            .field("requests", &self.requests.len())
            .field("serve", &self.serve)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    //! The queue's protocol, in one process: each thread with a store handle of its own is a
    //! writer, its flocks its own. A test that must hold the lock takes [`MetaLock`] itself and
    //! leads with it ([`MetaStore::lead_queue`]), so that it knows who leads.

    use std::sync::mpsc;
    use std::sync::{Mutex, PoisonError};

    use camino::Utf8PathBuf;
    use swarmops_core::{ChangesetId, ObjectKind, hash_object};

    use super::*;
    use crate::fs_ops::{FsOps, LinkOutcome, RealFs};
    use crate::layout::Layout;
    use crate::meta::index::ChangesetMeta;
    use crate::meta::store::{CasResult, InsertOutcome};

    pub(super) const ACTOR: &str = "test/queue";

    /// How long a test waits for a file or a thread before it fails.
    pub(super) const PATIENCE: Duration = Duration::from_secs(10);

    /// The id of changeset `n`.
    pub(super) fn id(n: u64) -> ChangesetId {
        ChangesetId::new(hash_object(ObjectKind::Changeset, &n.to_le_bytes()))
    }

    /// What a test request asks its leader to plan.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum Ask {
        /// Insert changeset n on `main` and move `main` to it: a frame.
        Frame(u64),
        /// Nothing to record.
        NoFrame,
        /// A planning error: declined.
        Fail,
        /// As `Frame`, with an answer body past what a claim takes.
        Big(u64),
    }

    impl Ask {
        pub(super) fn body(self) -> Vec<u8> {
            match self {
                Ask::Frame(n) => [&b"F"[..], &n.to_le_bytes()].concat(),
                Ask::NoFrame => b"N".to_vec(),
                Ask::Fail => b"X".to_vec(),
                Ask::Big(n) => [&b"B"[..], &n.to_le_bytes()].concat(),
            }
        }

        fn of(body: &[u8]) -> Ask {
            let n = || u64::from_le_bytes(body[1..9].try_into().unwrap());
            match body[0] {
                b'F' => Ask::Frame(n()),
                b'B' => Ask::Big(n()),
                b'N' => Ask::NoFrame,
                _ => Ask::Fail,
            }
        }
    }

    /// Plans request `i` of `lead` in a frame of its own, `next` the index its frame takes.
    fn plan(lead: &mut Lead<'_>, i: usize, next: &mut usize) -> Planned {
        let request = lead.requests()[i].clone();
        let write = lead.write();
        write.begin_frame().unwrap();
        let ask = Ask::of(&request.body);
        let n = match ask {
            Ask::Frame(n) | Ask::Big(n) => n,
            Ask::NoFrame | Ask::Fail => {
                write.drop_frame();
                let frame = if ask == Ask::Fail {
                    FrameUse::Declined
                } else {
                    FrameUse::NoFrame
                };
                return Planned::new(request.rid, frame, b"none".to_vec());
            }
        };
        let parent = write.get_ref("main").unwrap();
        let delta = hash_object(ObjectKind::Blob, &n.to_le_bytes()).0;
        let meta = ChangesetMeta::new(id(n), 0, 0, delta, parent.into_iter().collect());
        if !matches!(
            write.insert_changeset(meta).unwrap(),
            InsertOutcome::Inserted { .. }
        ) {
            write.drop_frame();
            return Planned::new(request.rid, FrameUse::NoFrame, b"present".to_vec());
        }
        let moved = write.cas_ref("main", parent, Some(id(n)), ACTOR).unwrap();
        assert_eq!(moved, CasResult::Applied { old: parent });
        let body = match ask {
            Ask::Big(_) => vec![0; BODY_MAX + 1],
            _ => n.to_le_bytes().to_vec(),
        };
        let frame = FrameUse::Frame(*next);
        *next += 1;
        Planned::new(request.rid, frame, body)
    }

    /// What a lead did: how many requests it saw and claimed, and the own answer.
    #[derive(Debug)]
    pub(super) struct Led {
        pub(super) requests: usize,
        pub(super) held: usize,
        pub(super) answer: Result<Answer, StoreError>,
    }

    /// Plans every request `lead` may plan, claims them and commits.
    pub(super) fn lead_all(mut lead: Lead<'_>) -> Led {
        let requests = lead.requests().len();
        let mut next = 0;
        let mut plans = vec![plan(&mut lead, 0, &mut next)];
        while lead.may_plan() {
            let i = plans.len();
            plans.push(plan(&mut lead, i, &mut next));
        }
        let held = lead.claim(plans).unwrap();
        let answer = lead.commit();
        Led {
            requests,
            held,
            answer,
        }
    }

    /// How a turn ended: answered by a leader, or led (and how).
    #[derive(Debug)]
    pub(super) enum Ended {
        Answered(Answer),
        Led(Led),
    }

    impl Ended {
        /// The answer, by whoever gave it.
        pub(super) fn answer(&self) -> &Answer {
            match self {
                Ended::Answered(answer) => answer,
                Ended::Led(led) => led.answer.as_ref().unwrap(),
            }
        }
    }

    /// A turn of `ask` on `store` through `seams`, led to its end if it leads.
    pub(super) fn turn_with(
        store: &MetaStore,
        ask: Ask,
        seams: Seams,
    ) -> Result<Ended, StoreError> {
        match store.take_turn_with(&ask.body(), Serve::Queue, seams)? {
            Turn::Answered(answer) => Ok(Ended::Answered(answer)),
            Turn::Lead(lead) => Ok(Ended::Led(lead_all(lead))),
        }
    }

    pub(super) fn turn(store: &MetaStore, ask: Ask) -> Result<Ended, StoreError> {
        turn_with(store, ask, Seams::real())
    }

    /// A fresh store.
    pub(super) fn new_store() -> (tempfile::TempDir, Utf8PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
        fs::create_dir(&root).unwrap();
        MetaStore::create(&root).unwrap();
        (dir, root)
    }

    /// A handle of its own on the store at `root`: one writer.
    pub(super) fn open(root: &Utf8PathBuf) -> MetaStore {
        let mut store = MetaStore::open(root).unwrap();
        store.set_parallelism(1);
        store
    }

    pub(super) fn queue(root: &Utf8PathBuf) -> PathBuf {
        root.join("meta/queue").into_std_path_buf()
    }

    /// The queue's files of `kind`.
    pub(super) fn files(root: &Utf8PathBuf, kind: Kind) -> Vec<RequestId> {
        let mut found: Vec<RequestId> = (fs::read_dir(queue(root)).into_iter().flatten())
            .filter_map(|entry| Kind::of(entry.ok()?.file_name().to_str()?))
            .filter(|(k, _)| *k == kind)
            .map(|(_, rid)| rid)
            .collect();
        found.sort();
        found
    }

    /// Waits until the queue holds `n` files of `kind`.
    pub(super) fn wait_files(root: &Utf8PathBuf, kind: Kind, n: usize) -> Vec<RequestId> {
        let deadline = Instant::now() + PATIENCE;
        loop {
            let found = files(root, kind);
            if found.len() >= n {
                return found;
            }
            assert!(
                Instant::now() < deadline,
                "{kind:?}: {} of {n}",
                found.len()
            );
            thread::sleep(Duration::from_millis(1));
        }
    }

    /// The lock of `store`, held by the test.
    pub(super) fn hold(store: &MetaStore) -> MetaLock {
        MetaLock::acquire(&store.layout, Duration::ZERO).unwrap()
    }

    /// Leads on `lock` with `ask` and `seams`, as a writer that found the lock free.
    pub(super) fn lead_on(store: &MetaStore, lock: MetaLock, ask: Ask, seams: Seams) -> Lead<'_> {
        let turn = store.lead_queue(lock, RequestId::new(), &ask.body(), None, seams);
        let Turn::Lead(lead) = turn.unwrap() else {
            panic!("a lock holder leads");
        };
        lead
    }

    /// How many times `main`'s log moved it to changeset `n`.
    pub(super) fn landed(store: &MetaStore, n: u64) -> usize {
        let read = store.read().unwrap();
        let log = read.reflog("main", 10_000).unwrap();
        log.iter().filter(|entry| entry.new == Some(id(n))).count()
    }

    /// Writes a request record of `ask` under `rid`'s `kind` name; the file, open, if `alive`
    /// holds its flock.
    pub(super) fn seed(
        root: &Utf8PathBuf,
        kind: Kind,
        ask: Ask,
        alive: bool,
    ) -> (RequestId, Option<File>) {
        let dir = queue(root);
        fs::create_dir_all(&dir).unwrap();
        let rid = RequestId::new();
        let path = kind.path(&dir, &rid);
        fs::write(&path, encode_request(&ask.body())).unwrap();
        let file = alive.then(|| {
            let file = lock::open_flock_file(&path, Create::Never).unwrap();
            file.try_lock().unwrap();
            file
        });
        (rid, file)
    }

    /// Writes `answer` as `rid`'s answer, renamed into place as a leader's `publish` does: a
    /// waiter polls the name and reads without the lock, so it must never see it half written.
    fn seed_answer(root: &Utf8PathBuf, rid: &RequestId, answer: &Answer) {
        let dir = queue(root);
        let temp = Kind::TempAns.path(&dir, rid);
        fs::write(&temp, encode_answer(answer)).unwrap();
        fs::rename(&temp, Kind::Ans.path(&dir, rid)).unwrap();
    }

    /// Writes `manifest` as `leader`'s.
    fn seed_manifest(root: &Utf8PathBuf, leader: &RequestId, manifest: &Manifest) {
        fs::write(
            Kind::Batch.path(&queue(root), leader),
            encode_manifest(manifest),
        )
        .unwrap();
    }

    /// A clock the test moves: the real one's start, plus what the test added.
    fn fake_clock() -> (Arc<AtomicU64>, Arc<dyn Fn() -> Instant + Send + Sync>) {
        let start = Instant::now();
        let added = Arc::new(AtomicU64::new(0));
        let read = Arc::clone(&added);
        let clock = move || start + Duration::from_millis(read.load(Ordering::SeqCst));
        (added, Arc::new(clock))
    }

    /// Which queue file operation [`Failing`] fails.
    #[derive(Clone, Copy, PartialEq, Eq)]
    pub(super) enum Op {
        Create,
        TryLock,
        Rename,
        Remove,
    }

    /// The real queue operations, but those `fails` names, on a file whose name contains the
    /// given text, fail with `EIO`.
    pub(super) struct Failing {
        fails: Mutex<Vec<(Op, String)>>,
    }

    impl Failing {
        pub(super) fn seams(fails: &[(Op, &str)]) -> Seams {
            let fails = fails.iter().map(|(op, text)| (*op, (*text).to_owned()));
            let ops = Failing {
                fails: Mutex::new(fails.collect()),
            };
            Seams {
                ops: Arc::new(ops),
                ..Seams::real()
            }
        }

        fn check(&self, op: Op, path: &Path) -> io::Result<()> {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let fails = self.fails.lock().unwrap_or_else(PoisonError::into_inner);
            if fails
                .iter()
                .any(|(o, text)| *o == op && name.contains(text.as_str()))
            {
                return Err(io::Error::from_raw_os_error(libc::EIO));
            }
            Ok(())
        }
    }

    impl QueueOps for Failing {
        fn create(&self, path: &Path) -> io::Result<File> {
            self.check(Op::Create, path)?;
            RealQueue.create(path)
        }

        fn open(&self, path: &Path) -> io::Result<File> {
            RealQueue.open(path)
        }

        fn write(&self, file: &mut File, bytes: &[u8]) -> io::Result<()> {
            RealQueue.write(file, bytes)
        }

        fn try_lock(&self, file: &File) -> Result<(), TryLockError> {
            let fails = self.fails.lock().unwrap_or_else(PoisonError::into_inner);
            if fails.iter().any(|(op, _)| *op == Op::TryLock) {
                return Err(TryLockError::Error(io::Error::from_raw_os_error(libc::EIO)));
            }
            drop(fails);
            RealQueue.try_lock(file)
        }

        fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.check(Op::Rename, to)?;
            RealQueue.rename(from, to)
        }

        fn remove(&self, path: &Path) -> io::Result<()> {
            self.check(Op::Remove, path)?;
            RealQueue.remove(path)
        }

        fn list(&self, dir: &Path) -> io::Result<Vec<String>> {
            RealQueue.list(dir)
        }
    }

    /// What a [`Spy`] asks before each call.
    pub(super) type Hook = dyn Fn(FsMethod, &Path) -> io::Result<()> + Send + Sync;

    /// The real filesystem, `hook` asked before each call (an error fails it), each crash point
    /// counted.
    pub(super) struct Spy {
        pub(super) hook: Box<Hook>,
        pub(super) points: AtomicU64,
    }

    impl Spy {
        pub(super) fn new(
            hook: impl Fn(FsMethod, &Path) -> io::Result<()> + Send + Sync + 'static,
        ) -> Arc<Spy> {
            Arc::new(Spy {
                hook: Box::new(hook),
                points: AtomicU64::new(0),
            })
        }
    }

    impl FsOps for Spy {
        fn create_dir_all(&self, p: &Path) -> io::Result<()> {
            (self.hook)(FsMethod::CreateDirAll, p)?;
            RealFs.create_dir_all(p)
        }

        fn write_durable_temp(&self, dir: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
            (self.hook)(FsMethod::WriteDurableTemp, dir)?;
            RealFs.write_durable_temp(dir, bytes)
        }

        fn link_noclobber(&self, from: &Path, to: &Path) -> io::Result<LinkOutcome> {
            (self.hook)(FsMethod::LinkNoclobber, to)?;
            RealFs.link_noclobber(from, to)
        }

        fn fsync_dir(&self, p: &Path) -> io::Result<()> {
            (self.hook)(FsMethod::FsyncDir, p)?;
            RealFs.fsync_dir(p)
        }

        fn read(&self, p: &Path) -> io::Result<Option<Vec<u8>>> {
            (self.hook)(FsMethod::Read, p)?;
            RealFs.read(p)
        }

        fn exists(&self, p: &Path) -> io::Result<bool> {
            (self.hook)(FsMethod::Exists, p)?;
            RealFs.exists(p)
        }

        fn remove(&self, p: &Path) -> io::Result<()> {
            (self.hook)(FsMethod::Remove, p)?;
            RealFs.remove(p)
        }

        fn append_durable(&self, path: &Path, bytes: &[u8]) -> io::Result<u64> {
            (self.hook)(FsMethod::AppendDurable, path)?;
            RealFs.append_durable(path, bytes)
        }

        fn file_len(&self, path: &Path) -> io::Result<u64> {
            (self.hook)(FsMethod::FileLen, path)?;
            RealFs.file_len(path)
        }

        fn truncate(&self, path: &Path, len: u64) -> io::Result<()> {
            (self.hook)(FsMethod::Truncate, path)?;
            RealFs.truncate(path, len)
        }

        fn atomic_replace(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
            (self.hook)(FsMethod::AtomicReplace, path)?;
            RealFs.atomic_replace(path, bytes)
        }

        fn sync_data(&self, path: &Path) -> io::Result<()> {
            (self.hook)(FsMethod::SyncData, path)?;
            RealFs.sync_data(path)
        }

        fn append(&self, path: &Path, bytes: &[u8]) -> io::Result<u64> {
            (self.hook)(FsMethod::Append, path)?;
            RealFs.append(path, bytes)
        }

        fn rename_durable(&self, from: &Path, to: &Path) -> io::Result<()> {
            (self.hook)(FsMethod::RenameDurable, to)?;
            RealFs.rename_durable(from, to)
        }

        fn crash_point(&self) {
            self.points.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// A store handle on `fs`.
    fn open_on(root: &Utf8PathBuf, fs: Arc<dyn FsOps>) -> MetaStore {
        let mut store = MetaStore::open_with_fs(root, fs).unwrap();
        store.set_parallelism(1);
        store
    }

    /// Runs `waiters` turns of `asks` in threads of their own, each on a handle of its own,
    /// once the test holds the lock; `lead` runs on the test's lock once they are all queued.
    /// Returns the waiters' ends, in order, and what `lead` returned.
    fn with_waiters<T: Send>(
        root: &Utf8PathBuf,
        asks: &[Ask],
        leader: impl FnOnce(MetaLock) -> T + Send,
    ) -> (Vec<Result<Ended, StoreError>>, T) {
        let holder = open(root);
        let lock = hold(&holder);
        thread::scope(|s| {
            let mut handles = Vec::new();
            for (i, ask) in asks.iter().enumerate() {
                let ask = *ask;
                handles.push(s.spawn(move || turn(&open(root), ask)));
                // One at a time, so that their order is their submission order.
                wait_files(root, Kind::Req, i + 1);
            }
            let led = leader(lock);
            let ends = handles.into_iter().map(|h| h.join().unwrap()).collect();
            (ends, led)
        })
    }

    #[test]
    fn the_lock_holder_answers_a_queued_request() {
        let (_dir, root) = new_store();
        let (ends, led) = with_waiters(&root, &[Ask::Frame(2)], |lock| {
            let store = open(&root);
            lead_all(lead_on(&store, lock, Ask::Frame(1), Seams::real()))
        });
        assert_eq!((led.requests, led.held), (2, 2), "{led:?}");
        let own = led.answer.unwrap();
        assert_eq!(own.outcome, Outcome::Committed { frame_seq: 0 });
        let Ok(Ended::Answered(answer)) = &ends[0] else {
            panic!("the waiter was answered: {ends:?}");
        };
        assert_eq!(answer.outcome, Outcome::Committed { frame_seq: 1 });
        assert_eq!(answer.body, 2u64.to_le_bytes());
        let store = open(&root);
        assert_eq!((landed(&store, 1), landed(&store, 2)), (1, 1));
        assert!(
            fs::read_dir(queue(&root)).unwrap().next().is_none(),
            "queue left empty"
        );
    }

    #[test]
    fn an_answer_is_read_without_the_lock() {
        let (_dir, root) = new_store();
        let holder = open(&root);
        let lock = hold(&holder);
        let (tx, rx) = mpsc::channel();
        thread::scope(|s| {
            s.spawn(|| tx.send(turn(&open(&root), Ask::Frame(5))).unwrap());
            let rid = wait_files(&root, Kind::Req, 1).remove(0);

            // Claimed and answered by hand, the lock held throughout.
            let dir = queue(&root);
            fs::rename(Kind::Req.path(&dir, &rid), Kind::Com.path(&dir, &rid)).unwrap();
            let answer = Answer::new(Outcome::Committed { frame_seq: 7 }, b"seven".to_vec());
            seed_answer(&root, &rid, &answer);
            let got = rx
                .recv_timeout(PATIENCE)
                .expect("answered while the lock is held");
            let Ok(Ended::Answered(read)) = got else {
                panic!("{got:?}");
            };
            assert_eq!(read, answer);
            assert!(
                files(&root, Kind::Ans).is_empty(),
                "the waiter removed its answer"
            );
            assert!(files(&root, Kind::Com).is_empty(), "and its request");
            drop(lock);
        });
    }

    #[test]
    fn a_queued_request_at_its_deadline_is_busy() {
        let (_dir, root) = new_store();
        let holder = open(&root);
        let _lock = hold(&holder);
        let store = open(&root).with_lock_timeout(Duration::from_millis(100));
        let started = Instant::now();
        let busy = turn(&store, Ask::Frame(1));
        assert!(matches!(busy, Err(StoreError::Busy)), "{busy:?}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        assert!(files(&root, Kind::Req).is_empty(), "withdrawn");
    }

    #[test]
    fn a_claimed_request_waits_past_its_deadline() {
        let (_dir, root) = new_store();
        let holder = open(&root);
        let lock = hold(&holder);
        let (tx, rx) = mpsc::channel();
        thread::scope(|s| {
            s.spawn(|| {
                let store = open(&root).with_lock_timeout(Duration::from_millis(50));
                tx.send(turn(&store, Ask::Frame(1))).unwrap();
            });
            let rid = wait_files(&root, Kind::Req, 1).remove(0);
            let dir = queue(&root);
            fs::rename(Kind::Req.path(&dir, &rid), Kind::Com.path(&dir, &rid)).unwrap();
            thread::sleep(Duration::from_millis(300));
            assert!(
                rx.try_recv().is_err(),
                "a claimed request waits past its deadline"
            );
            seed_answer(&root, &rid, &Answer::new(Outcome::NoFrame, Vec::new()));
            let got = rx.recv_timeout(PATIENCE).unwrap();
            assert!(matches!(got, Ok(Ended::Answered(_))), "{got:?}");
            drop(lock);
        });
    }

    #[test]
    fn a_dead_waiters_request_is_dropped() {
        let (_dir, root) = new_store();
        let (dead, _) = seed(&root, Kind::Req, Ask::Frame(9), false);
        let store = open(&root);
        let lead = lead_on(&store, hold(&store), Ask::Frame(1), Seams::real());
        assert_eq!(lead.requests().len(), 1, "never planned");
        assert!(!exists(&Kind::Req.path(&queue(&root), &dead)), "removed");
        drop(lead);
    }

    #[test]
    fn a_torn_request_is_never_taken() {
        let (_dir, root) = new_store();
        let (rid, _alive) = seed(&root, Kind::Req, Ask::Frame(9), true);
        let path = Kind::Req.path(&queue(&root), &rid);
        let mut torn = fs::read(&path).unwrap();
        torn.pop();
        fs::write(&path, torn).unwrap();
        let store = open(&root);
        let led = lead_all(lead_on(&store, hold(&store), Ask::Frame(1), Seams::real()));
        assert_eq!((led.requests, led.held), (1, 1), "{led:?}");
        assert!(exists(&path), "left as it is, unclaimed");
        assert_eq!(landed(&store, 9), 0);
    }

    #[test]
    fn requests_are_led_in_submission_order() {
        let (_dir, root) = new_store();
        let asks = [Ask::Frame(10), Ask::Frame(11), Ask::Frame(12)];
        let (ends, bodies) = with_waiters(&root, &asks, |lock| {
            let store = open(&root);
            let lead = lead_on(&store, lock, Ask::Frame(1), Seams::real());
            let bodies: Vec<Vec<u8>> = lead.requests().iter().map(|r| r.body.clone()).collect();
            lead_all(lead);
            bodies
        });
        let want: Vec<Vec<u8>> = [Ask::Frame(1)]
            .iter()
            .chain(&asks)
            .map(|a| a.body())
            .collect();
        assert_eq!(bodies, want);
        let seqs: Vec<Outcome> = ends
            .iter()
            .map(|e| e.as_ref().unwrap().answer().outcome.clone())
            .collect();
        let want: Vec<Outcome> = (1..4)
            .map(|frame_seq| Outcome::Committed { frame_seq })
            .collect();
        assert_eq!(seqs, want);
    }

    #[test]
    fn the_own_request_is_planned_first() {
        let (_dir, root) = new_store();
        let (ends, (whose, led)) = with_waiters(&root, &[Ask::Frame(2)], |lock| {
            let store = open(&root);
            let lead = lead_on(&store, lock, Ask::Frame(1), Seams::real());
            let whose: Vec<Whose> = lead.requests().iter().map(|r| r.whose).collect();
            (whose, lead_all(lead))
        });
        assert_eq!(whose, [Whose::Own, Whose::Other]);
        assert_eq!(
            led.answer.unwrap().outcome,
            Outcome::Committed { frame_seq: 0 },
            "frame 0"
        );
        let waiter = ends[0].as_ref().unwrap().answer();
        assert_eq!(waiter.outcome, Outcome::Committed { frame_seq: 1 });
    }

    #[test]
    fn a_batch_takes_at_most_32() {
        let (_dir, root) = new_store();
        let asks: Vec<Ask> = (10..50).map(Ask::Frame).collect();
        let (ends, led) = with_waiters(&root, &asks, |lock| {
            let store = open(&root);
            lead_all(lead_on(&store, lock, Ask::Frame(1), Seams::real()))
        });
        assert_eq!((led.requests, led.held), (MAX_BATCH, MAX_BATCH), "{led:?}");
        let answered = ends
            .iter()
            .filter(|e| matches!(e, Ok(Ended::Answered(_))))
            .count();
        assert!(answered >= MAX_BATCH - 1, "{answered}");
        let store = open(&root);
        for n in 10..50 {
            assert_eq!(landed(&store, n), 1, "{n}");
        }
    }

    #[test]
    fn planning_stops_at_the_budget() {
        let (_dir, root) = new_store();
        let asks = [Ask::Frame(10), Ask::Frame(11), Ask::Frame(12)];
        let (ends, held) = with_waiters(&root, &asks, |lock| {
            let store = open(&root);
            let (added, clock) = fake_clock();
            let seams = Seams {
                clock,
                ..Seams::real()
            };
            let mut lead = lead_on(&store, lock, Ask::Frame(1), seams);
            let mut next = 0;
            let mut plans = vec![plan(&mut lead, 0, &mut next)];
            assert!(lead.may_plan(), "within the budget");
            plans.push(plan(&mut lead, 1, &mut next));
            added.store(
                u64::try_from(PLAN_BUDGET.as_millis()).unwrap(),
                Ordering::SeqCst,
            );
            assert!(!lead.may_plan(), "the budget is spent");
            let held = lead.claim(plans).unwrap();
            lead.commit().unwrap();
            held
        });
        assert_eq!(held, 2);
        assert!(ends.iter().all(Result::is_ok), "{ends:?}");
        let store = open(&root);
        assert!(
            (10..13).all(|n| landed(&store, n) == 1),
            "the rest led later"
        );
    }

    #[test]
    fn a_failed_batch_declines_the_others() {
        let (_dir, root) = new_store();
        let (ends, led) = with_waiters(&root, &[Ask::Frame(2)], |lock| {
            let log = Layout::new(root.clone()).journal_log();
            let spy = Spy::new(move |method, path| {
                let failed = method == FsMethod::AppendDurable && path == log.as_std_path();
                if failed {
                    return Err(io::Error::from_raw_os_error(libc::EIO));
                }
                Ok(())
            });
            let store = open_on(&root, spy);
            lead_all(lead_on(&store, lock, Ask::Frame(1), Seams::real()))
        });
        assert_eq!(led.held, 2);
        assert!(
            matches!(led.answer, Err(StoreError::NotCommitted { .. })),
            "{led:?}"
        );
        // The waiter was declined, ran again alone and led itself.
        let Ok(Ended::Led(alone)) = &ends[0] else {
            panic!("{ends:?}");
        };
        assert_eq!(alone.requests, 1);
        let store = open(&root);
        assert_eq!((landed(&store, 1), landed(&store, 2)), (0, 1));
    }

    #[test]
    fn no_answer_before_the_commit_point() {
        let (_dir, root) = new_store();
        let early = Arc::new(AtomicU64::new(0));
        let (ends, led) = with_waiters(&root, &[Ask::Frame(2), Ask::NoFrame], |lock| {
            let log = Layout::new(root.clone()).journal_log();
            let dir = queue(&root);
            let seen = Arc::clone(&early);
            let spy = Spy::new(move |method, path| {
                if method == FsMethod::AppendDurable && path == log.as_std_path() {
                    let answers = (fs::read_dir(&dir).unwrap())
                        .filter(|e| {
                            matches!(
                                Kind::of(e.as_ref().unwrap().file_name().to_str().unwrap()),
                                Some((Kind::Ans | Kind::TempAns, _))
                            )
                        })
                        .count();
                    seen.fetch_add(bytes::to_u64(answers), Ordering::SeqCst);
                }
                Ok(())
            });
            let store = open_on(&root, spy);
            lead_all(lead_on(&store, lock, Ask::Frame(1), Seams::real()))
        });
        assert_eq!(led.held, 3);
        assert_eq!(
            early.load(Ordering::SeqCst),
            0,
            "an answer before the log append"
        );
        assert!(
            ends.iter().all(|e| matches!(e, Ok(Ended::Answered(_)))),
            "{ends:?}"
        );
    }

    /// A batch whose answers to the members named in `unanswered` fail: a leader that failed, or
    /// died, after the commit point.
    fn unanswered_batch(root: &Utf8PathBuf, asks: &[Ask], unanswered: usize) -> Vec<RequestId> {
        let holder = open(root);
        let lock = hold(&holder);
        let mut alive = Vec::new();
        for ask in asks {
            alive.push(seed(root, Kind::Req, *ask, true));
        }
        let rids: Vec<RequestId> = alive.iter().map(|(rid, _)| rid.clone()).collect();
        let fails: Vec<(Op, &str)> = rids[asks.len() - unanswered..]
            .iter()
            .map(|rid| (Op::Create, rid.as_str()))
            .collect();
        let store = open(root);
        let led = lead_all(lead_on(&store, lock, Ask::NoFrame, Failing::seams(&fails)));
        assert_eq!(led.held, asks.len() + 1);
        drop(holder);
        // The waiters stay alive, waiting.
        std::mem::forget(alive);
        rids
    }

    #[test]
    fn a_dead_leaders_committed_members_are_answered() {
        let (_dir, root) = new_store();
        let rids = unanswered_batch(&root, &[Ask::Frame(2)], 1);
        assert_eq!(files(&root, Kind::Com), rids, "claimed, unanswered");
        assert_eq!(files(&root, Kind::Batch).len(), 1, "the manifest kept");
        let store = open(&root);
        lead_all(lead_on(&store, hold(&store), Ask::NoFrame, Seams::real()));
        let read = read_bounded(
            &RealQueue,
            &Kind::Ans.path(&queue(&root), &rids[0]),
            ANSWER_MAX,
        );
        let answer = decode_answer(&read.unwrap().unwrap()).unwrap();
        assert_eq!(answer.outcome, Outcome::Committed { frame_seq: 0 });
        assert_eq!(answer.body, 2u64.to_le_bytes());
        assert!(files(&root, Kind::Batch).is_empty(), "settled");
        assert_eq!(landed(&store, 2), 1);
    }

    #[test]
    fn a_dead_leaders_partial_answers_are_completed() {
        let (_dir, root) = new_store();
        let rids = unanswered_batch(&root, &[Ask::Frame(2), Ask::Frame(3)], 1);
        assert_eq!(files(&root, Kind::Ans), rids[..1], "the first answered");
        let store = open(&root);
        lead_all(lead_on(&store, hold(&store), Ask::NoFrame, Seams::real()));
        assert_eq!(files(&root, Kind::Ans), rids, "both answered");
        let path = Kind::Ans.path(&queue(&root), &rids[1]);
        let answer = decode_answer(&fs::read(path).unwrap()).unwrap();
        assert_eq!(answer.outcome, Outcome::Committed { frame_seq: 1 });
        assert_eq!(files(&root, Kind::Batch), [], "no manifest left");
    }

    #[test]
    fn a_claimed_waiter_that_gets_the_lock_settles_its_leader() {
        let (_dir, root) = new_store();
        let holder = open(&root);
        let lock = hold(&holder);
        thread::scope(|s| {
            let waiter = s.spawn(|| turn(&open(&root), Ask::Frame(2)));
            let rid = wait_files(&root, Kind::Req, 1).remove(0);
            let store = open(&root);
            let seams = Failing::seams(&[(Op::Create, rid.as_str())]);
            let led = lead_all(lead_on(&store, lock, Ask::Frame(1), seams));
            assert_eq!(led.held, 2);
            drop(holder);
            let ended = waiter.join().unwrap().unwrap();
            let Ended::Answered(answer) = ended else {
                panic!("settled, not led: {ended:?}");
            };
            assert_eq!(answer.outcome, Outcome::Committed { frame_seq: 1 });
            assert_eq!(landed(&store, 2), 1);
        });
        assert_eq!(files(&root, Kind::Batch), [], "no manifest left");
    }

    /// The manifest of a leader that died before its log append, naming `rids` with a frame
    /// each (whose hashes no frame has) at the store's next seq.
    fn dead_manifest(root: &Utf8PathBuf, rids: &[RequestId]) -> RequestId {
        let store = open(root);
        let first_seq = store.write().unwrap().next_seq().unwrap();
        let members = (rids.iter().zip(0u32..))
            .map(|(rid, index)| Named {
                rid: rid.clone(),
                frame: Some((index, [7; 32])),
                body: b"never".to_vec(),
            })
            .collect();
        let leader = RequestId::new();
        seed_manifest(root, &leader, &Manifest { first_seq, members });
        leader
    }

    #[test]
    fn a_dead_leaders_uncommitted_members_are_requeued() {
        let (_dir, root) = new_store();
        let (rid, _alive) = seed(&root, Kind::Com, Ask::Frame(2), true);
        dead_manifest(&root, std::slice::from_ref(&rid));
        let store = open(&root);
        let led = lead_all(lead_on(&store, hold(&store), Ask::NoFrame, Seams::real()));
        assert_eq!(led.held, 2, "requeued, then planned");
        assert_eq!(landed(&store, 2), 1);
        assert_eq!(files(&root, Kind::Batch), [], "settled");
        assert_eq!(files(&root, Kind::Ans), [rid], "answered by this batch");
    }

    #[test]
    fn a_reused_seq_requeues_the_member() {
        let (_dir, root) = new_store();
        let (rid, _alive) = seed(&root, Kind::Com, Ask::Frame(2), true);
        dead_manifest(&root, std::slice::from_ref(&rid));
        // A plain writer's frame, which settles nothing, takes the seq the manifest names.
        let store = open(&root);
        let mut write = store.write().unwrap();
        let delta = hash_object(ObjectKind::Blob, b"other").0;
        write
            .insert_changeset(ChangesetMeta::new(id(1), 0, 0, delta, Vec::new()))
            .unwrap();
        write.commit().unwrap();
        let led = lead_all(lead_on(&store, hold(&store), Ask::NoFrame, Seams::real()));
        assert_eq!(led.held, 2, "requeued, then planned");
        let answer = decode_answer(&fs::read(Kind::Ans.path(&queue(&root), &rid)).unwrap());
        assert_eq!(answer.unwrap().outcome, Outcome::Committed { frame_seq: 1 });
    }

    #[test]
    fn a_partial_claim_is_requeued() {
        let (_dir, root) = new_store();
        let (claimed, _a) = seed(&root, Kind::Com, Ask::Frame(2), true);
        let (unclaimed, _b) = seed(&root, Kind::Req, Ask::Frame(3), true);
        let leader = RequestId::new();
        let members = [&claimed, &unclaimed]
            .iter()
            .map(|rid| Named {
                rid: (*rid).clone(),
                frame: None,
                body: Vec::new(),
            })
            .collect();
        seed_manifest(
            &root,
            &leader,
            &Manifest {
                first_seq: 0,
                members,
            },
        );
        let store = open(&root);
        let lead = lead_on(&store, hold(&store), Ask::NoFrame, Seams::real());
        assert_eq!(files(&root, Kind::Req), {
            let mut both = vec![claimed, unclaimed];
            both.sort();
            both
        });
        assert!(files(&root, Kind::Batch).is_empty(), "settled");
        assert_eq!(lead.requests().len(), 3);
        drop(lead);
    }

    #[test]
    fn an_answered_member_is_never_requeued() {
        let (_dir, root) = new_store();
        let (rid, _alive) = seed(&root, Kind::Com, Ask::Frame(2), true);
        dead_manifest(&root, std::slice::from_ref(&rid));
        let answer = Answer::new(Outcome::Committed { frame_seq: 9 }, b"kept".to_vec());
        seed_answer(&root, &rid, &answer);
        let store = open(&root);
        let lead = lead_on(&store, hold(&store), Ask::NoFrame, Seams::real());
        assert!(files(&root, Kind::Req).is_empty(), "not requeued");
        assert_eq!(files(&root, Kind::Com), std::slice::from_ref(&rid));
        assert_eq!(lead.requests().len(), 1);
        assert!(files(&root, Kind::Batch).is_empty(), "settled");
        let read = decode_answer(&fs::read(Kind::Ans.path(&queue(&root), &rid)).unwrap());
        assert_eq!(read.unwrap(), answer, "the answer stands");
        drop(lead);
    }

    #[test]
    fn a_leftover_manifest_with_every_answer_is_removed() {
        let (_dir, root) = new_store();
        let (answered, _a) = seed(&root, Kind::Com, Ask::Frame(2), true);
        seed_answer(
            &root,
            &answered,
            &Answer::new(Outcome::Committed { frame_seq: 0 }, Vec::new()),
        );
        // A frameless member, gone (its waiter read its answer and left).
        let gone = RequestId::new();
        let members = vec![
            Named {
                rid: answered,
                frame: Some((0, [1; 32])),
                body: Vec::new(),
            },
            Named {
                rid: gone,
                frame: None,
                body: Vec::new(),
            },
        ];
        seed_manifest(
            &root,
            &RequestId::new(),
            &Manifest {
                first_seq: 0,
                members,
            },
        );
        let store = open(&root);
        drop(lead_on(&store, hold(&store), Ask::NoFrame, Seams::real()));
        assert!(files(&root, Kind::Batch).is_empty(), "removed");
        assert!(files(&root, Kind::Req).is_empty(), "nothing requeued");
    }

    #[test]
    fn a_kept_manifest_stops_claims() {
        let (_dir, root) = new_store();
        let (_waiting, _a) = seed(&root, Kind::Req, Ask::Frame(2), true);
        let (_claimed, _b) = seed(&root, Kind::Com, Ask::Frame(3), true);
        let leader = RequestId::new();
        fs::write(Kind::Batch.path(&queue(&root), &leader), b"no manifest").unwrap();
        let store = open(&root);
        let led = lead_all(lead_on(&store, hold(&store), Ask::Frame(1), Seams::real()));
        assert_eq!((led.requests, led.held), (1, 1), "{led:?}");
        assert_eq!(files(&root, Kind::Batch), [leader], "kept");
        assert_eq!(landed(&store, 1), 1, "the own request led alone");
    }

    #[test]
    fn a_leftover_own_request_is_never_planned() {
        let (_dir, root) = new_store();
        let holder = open(&root);
        let lock = hold(&holder);
        thread::scope(|s| {
            let waiter = s.spawn(|| {
                let seams = Failing::seams(&[(Op::Remove, ".req")]);
                turn_with(&open(&root), Ask::Frame(2), seams)
            });
            let own = wait_files(&root, Kind::Req, 1).remove(0);
            drop(lock);
            let ended = waiter.join().unwrap().unwrap();
            let Ended::Led(led) = ended else {
                panic!("{ended:?}");
            };
            assert_eq!(led.requests, 1, "its own leftover file is not planned");
            let path = Kind::Req.path(&queue(&root), &own);
            let left = lock::open_flock_file(&path, Create::Never).unwrap();
            assert!(left.try_lock().is_ok(), "left closed: it reads as dead");
        });
        assert_eq!(landed(&open(&root), 2), 1);
    }

    #[test]
    fn a_waiter_dead_after_its_claim_commits_once() {
        let (_dir, root) = new_store();
        let (rid, alive) = seed(&root, Kind::Req, Ask::Frame(2), true);
        let store = open(&root);
        let mut lead = lead_on(&store, hold(&store), Ask::Frame(1), Seams::real());
        let mut next = 0;
        let mut plans = vec![plan(&mut lead, 0, &mut next)];
        assert!(lead.may_plan());
        plans.push(plan(&mut lead, 1, &mut next));
        assert_eq!(lead.claim(plans).unwrap(), 2);
        drop(alive);
        lead.commit().unwrap();
        assert_eq!(files(&root, Kind::Ans), std::slice::from_ref(&rid));
        let again = lead_all(lead_on(&store, hold(&store), Ask::NoFrame, Seams::real()));
        assert_eq!(again.requests, 1, "never planned again");
        assert!(files(&root, Kind::Ans).is_empty() && files(&root, Kind::Com).is_empty());
        assert_eq!(landed(&store, 2), 1);
    }

    #[test]
    fn a_failed_rename_back_keeps_the_manifest() {
        let (_dir, root) = new_store();
        let (rid, _alive) = seed(&root, Kind::Req, Ask::Frame(2), true);
        let store = open(&root);
        let seams = Failing::seams(&[(Op::Rename, ".req")]);
        let mut lead = lead_on(&store, hold(&store), Ask::Frame(1), seams);
        let mut next = 0;
        let mut plans = vec![plan(&mut lead, 0, &mut next)];
        assert!(lead.may_plan());
        plans.push(plan(&mut lead, 1, &mut next));
        assert_eq!(lead.claim(plans).unwrap(), 2);
        drop(lead);
        assert_eq!(files(&root, Kind::Com), [rid], "not put back");
        assert_eq!(files(&root, Kind::Batch).len(), 1, "kept");
        assert_eq!(landed(&store, 2), 0);
    }

    #[test]
    fn a_dropped_lead_puts_its_claims_back() {
        let (_dir, root) = new_store();
        let (rid, _alive) = seed(&root, Kind::Req, Ask::Frame(2), true);
        let store = open(&root);
        let mut lead = lead_on(&store, hold(&store), Ask::Frame(1), Seams::real());
        let mut next = 0;
        let mut plans = vec![plan(&mut lead, 0, &mut next)];
        assert!(lead.may_plan());
        plans.push(plan(&mut lead, 1, &mut next));
        assert_eq!(lead.claim(plans).unwrap(), 2);
        assert_eq!(files(&root, Kind::Com), std::slice::from_ref(&rid));
        drop(lead);
        assert_eq!(files(&root, Kind::Req), [rid]);
        assert_eq!(files(&root, Kind::Batch), [], "no manifest left");
    }

    #[test]
    fn an_unreadable_manifest_is_never_settled() {
        let (_dir, root) = new_store();
        let (rid, _alive) = seed(&root, Kind::Com, Ask::Frame(2), true);
        let leader = dead_manifest(&root, std::slice::from_ref(&rid));
        let path = Kind::Batch.path(&queue(&root), &leader);
        let mut torn = fs::read(&path).unwrap();
        torn.truncate(torn.len() - 1);
        fs::write(&path, torn).unwrap();
        let store = open(&root);
        drop(lead_on(&store, hold(&store), Ask::NoFrame, Seams::real()));
        assert_eq!(files(&root, Kind::Com), [rid], "never settled");
        assert_eq!(files(&root, Kind::Batch), [leader], "kept");
    }

    #[test]
    fn a_manifest_of_another_version_is_never_settled() {
        let (_dir, root) = new_store();
        let (rid, _alive) = seed(&root, Kind::Com, Ask::Frame(2), true);
        let leader = dead_manifest(&root, std::slice::from_ref(&rid));
        let path = Kind::Batch.path(&queue(&root), &leader);
        let mut other = fs::read(&path).unwrap();
        other.truncate(other.len() - CRC_LEN);
        other[MAGIC_LEN] = VERSION + 1;
        let crc = crc32fast::hash(&other);
        other.extend_from_slice(&crc.to_le_bytes());
        fs::write(&path, other).unwrap();
        let store = open(&root);
        drop(lead_on(&store, hold(&store), Ask::NoFrame, Seams::real()));
        assert_eq!(files(&root, Kind::Com), [rid], "never settled");
        assert_eq!(files(&root, Kind::Batch), [leader], "kept");
    }

    #[test]
    fn an_oversized_answer_is_declined_before_the_claim() {
        let (_dir, root) = new_store();
        let asks = [Ask::Big(2), Ask::Frame(3)];
        let (ends, led) = with_waiters(&root, &asks, |lock| {
            let store = open(&root);
            lead_all(lead_on(&store, lock, Ask::Frame(1), Seams::real()))
        });
        assert_eq!(led.held, 3, "both claimed, declined");
        assert_eq!(
            led.answer.unwrap().outcome,
            Outcome::Committed { frame_seq: 0 }
        );
        // Each ran again alone: the one planned on the oversized member's frame too.
        for end in &ends {
            let Ok(Ended::Led(alone)) = end else {
                panic!("{ends:?}");
            };
            assert_eq!(alone.requests, 1);
        }
        let store = open(&root);
        assert_eq!((landed(&store, 2), landed(&store, 3)), (1, 1));
    }

    #[test]
    fn a_post_commit_warning_is_cut_to_its_reserve() {
        let long = "é".repeat(WARNING_MAX);
        let cut = cut_warning(&long);
        assert!(
            cut.len() <= WARNING_MAX && cut.ends_with(ELLIPSIS),
            "{}",
            cut.len()
        );
        assert_eq!(cut_warning("short"), "short");

        // Through the commit's error, with the longest body a claim takes.
        let source = StoreError::BadCall(long);
        let committed = Err(StoreError::PostCommit {
            frame_seq: 4,
            frame_crc: 0,
            source: Box::new(source),
        });
        let outcome = outcome_of(FrameUse::Frame(1), &committed);
        let Outcome::PostCommit { frame_seq, warning } = &outcome else {
            panic!("{outcome:?}");
        };
        assert_eq!(*frame_seq, 5);
        assert!(warning.len() <= WARNING_MAX);
        let record = encode_answer(&Answer::new(outcome, vec![0; BODY_MAX]));
        assert!(record.len() <= ANSWER_MAX, "{}", record.len());
    }

    #[test]
    fn an_oversized_request_writes_no_queue_file() {
        let (_dir, root) = new_store();
        let holder = open(&root);
        let _lock = hold(&holder);
        let store = open(&root).with_lock_timeout(Duration::from_millis(50));
        let body = vec![0; REQUEST_MAX];
        let busy = store.take_turn(&body, Serve::Queue);
        assert!(matches!(busy, Err(StoreError::Busy)), "{busy:?}");
        assert!(!exists(&queue(&root)), "no queue file, no queue");
    }

    #[test]
    fn an_unreadable_answer_fails_without_a_rerun() {
        let (_dir, root) = new_store();
        let holder = open(&root);
        let lock = hold(&holder);
        let (tx, rx) = mpsc::channel();
        thread::scope(|s| {
            s.spawn(|| tx.send(turn(&open(&root), Ask::Frame(1))).unwrap());
            let rid = wait_files(&root, Kind::Req, 1).remove(0);
            let dir = queue(&root);
            fs::rename(Kind::Req.path(&dir, &rid), Kind::Com.path(&dir, &rid)).unwrap();
            fs::write(Kind::Ans.path(&dir, &rid), b"garbage").unwrap();
            let got = rx.recv_timeout(PATIENCE).unwrap();
            let Err(StoreError::Io(failure)) = &got else {
                panic!("{got:?}");
            };
            assert!(failure.to_string().contains(".ans"), "{failure}");
            drop(lock);
        });
        assert_eq!(landed(&open(&root), 1), 0, "never run again");
    }

    #[test]
    fn a_request_under_no_name_fails_closed() {
        let (_dir, root) = new_store();
        let holder = open(&root);
        let lock = hold(&holder);
        thread::scope(|s| {
            let waiter = s.spawn(|| turn(&open(&root), Ask::Frame(1)));
            let rid = wait_files(&root, Kind::Req, 1).remove(0);
            fs::remove_file(Kind::Req.path(&queue(&root), &rid)).unwrap();
            drop(lock);
            let got = waiter.join().unwrap();
            let Err(StoreError::Io(failure)) = &got else {
                panic!("{got:?}");
            };
            assert!(failure.to_string().contains(".req"), "{failure}");
        });
        assert_eq!(landed(&open(&root), 1), 0);
    }

    #[test]
    fn a_liveness_test_error_counts_as_alive() {
        let (_dir, root) = new_store();
        let (rid, _) = seed(&root, Kind::Req, Ask::Frame(2), false);
        let store = open(&root);
        let seams = Failing::seams(&[(Op::TryLock, "")]);
        let lead = lead_on(&store, hold(&store), Ask::NoFrame, seams);
        assert_eq!(lead.requests().len(), 2, "planned as alive");
        assert_eq!(files(&root, Kind::Req), [rid], "not removed");
        drop(lead);
    }

    #[test]
    fn own_only_serves_no_queue() {
        let (_dir, root) = new_store();
        let (rid, _alive) = seed(&root, Kind::Req, Ask::Frame(2), true);
        let store = open(&root);
        let Turn::Lead(alone) = store
            .take_turn(&Ask::Frame(1).body(), Serve::OwnOnly)
            .unwrap()
        else {
            panic!("the lock is free");
        };
        let led = lead_all(alone);
        assert_eq!((led.requests, led.held), (1, 1));
        assert_eq!(files(&root, Kind::Req), [rid], "left in the queue");
        assert_eq!(landed(&store, 2), 0);
    }

    #[test]
    fn a_queue_dir_that_is_no_directory_serves_no_queue() {
        let (_dir, root) = new_store();
        let elsewhere = root.join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, queue(&root)).unwrap();
        let holder = open(&root);
        let lock = hold(&holder);
        thread::scope(|s| {
            let waiter = s.spawn(|| {
                let store = open(&root).with_lock_timeout(Duration::from_millis(300));
                turn(&store, Ask::Frame(1))
            });
            // Nothing is made where the link points, while the writer waits.
            while !waiter.is_finished() {
                let made = fs::read_dir(&elsewhere).unwrap().count();
                assert_eq!(made, 0, "a queue file behind the link");
                thread::sleep(Duration::from_millis(1));
            }
            let busy = waiter.join().unwrap();
            assert!(matches!(busy, Err(StoreError::Busy)), "{busy:?}");
        });
        drop(lock);
        let Ended::Led(led) = turn(&open(&root), Ask::Frame(1)).unwrap() else {
            panic!("the lock is free");
        };
        assert_eq!(led.requests, 1);
    }

    #[test]
    fn a_declined_rerun_keeps_the_first_deadline() {
        let (_dir, root) = new_store();
        let timeout = Duration::from_millis(400);
        let holder = open(&root);
        let lock = hold(&holder);
        thread::scope(|s| {
            let started = Instant::now();
            let root = &root;
            let waiter = s.spawn(move || {
                let store = open(root).with_lock_timeout(timeout);
                let got = turn(&store, Ask::Frame(1));
                (got, started.elapsed())
            });
            let rid = wait_files(root, Kind::Req, 1).remove(0);
            let dir = queue(root);
            fs::rename(Kind::Req.path(&dir, &rid), Kind::Com.path(&dir, &rid)).unwrap();
            thread::sleep(Duration::from_millis(300));
            seed_answer(root, &rid, &Answer::new(Outcome::Declined, Vec::new()));
            let (got, spent) = waiter.join().unwrap();
            assert!(matches!(got, Err(StoreError::Busy)), "{got:?}");
            assert!(spent < timeout + Duration::from_millis(250), "{spent:?}");
            drop(lock);
        });
    }

    /// A declined waiter runs again on its first wait: the wait's helper, parked in `flock`, may
    /// take the lock the moment its holder lets go, and hands it to the rerun. Three rounds,
    /// each let go once the answer is read; a helper the round before left parked would fail
    /// the next round's zero-timeout hold.
    #[test]
    fn a_declined_rerun_waits_on_its_first_wait() {
        let (_dir, root) = new_store();
        for round in 0..3 {
            let holder = open(&root);
            let lock = hold(&holder);
            thread::scope(|s| {
                let waiter = s.spawn(|| {
                    let store = open(&root).with_lock_timeout(Duration::from_secs(2));
                    turn(&store, Ask::Frame(100 + round))
                });
                let rid = wait_files(&root, Kind::Req, 1).remove(0);
                let dir = queue(&root);
                fs::rename(Kind::Req.path(&dir, &rid), Kind::Com.path(&dir, &rid)).unwrap();
                seed_answer(&root, &rid, &Answer::new(Outcome::Declined, Vec::new()));
                // Let go once the waiter read its answer, its rerun under way.
                let deadline = Instant::now() + PATIENCE;
                while exists(&Kind::Ans.path(&dir, &rid)) {
                    assert!(
                        Instant::now() < deadline,
                        "round {round}: the answer is read"
                    );
                    thread::sleep(Duration::from_millis(1));
                }
                drop(lock);
                let ended = waiter.join().unwrap();
                assert!(
                    matches!(ended, Ok(Ended::Led(_))),
                    "round {round}: {ended:?}"
                );
            });
        }
    }

    /// How many lock-wait helper threads (`meta-lock-wait`) this process runs.
    fn helpers() -> usize {
        fs::read_dir("/proc/self/task")
            .unwrap()
            .filter(|task| {
                let comm = task.as_ref().unwrap().path().join("comm");
                fs::read_to_string(comm).is_ok_and(|name| name.trim() == "meta-lock-wait")
            })
            .count()
    }

    /// A declined rerun waits on its first wait's helper and starts no second one: a second
    /// would leave the first parked in `flock`, to take the lock for an instant after the call
    /// returned. Checked for 100 ms after the answer is read, while the holder keeps the lock.
    #[test]
    fn a_declined_rerun_starts_no_second_helper() {
        let (_dir, root) = new_store();
        let holder = open(&root);
        let lock = hold(&holder);
        thread::scope(|s| {
            let waiter = s.spawn(|| {
                let store = open(&root).with_lock_timeout(Duration::from_secs(5));
                turn(&store, Ask::Frame(1))
            });
            let rid = wait_files(&root, Kind::Req, 1).remove(0);
            let dir = queue(&root);
            fs::rename(Kind::Req.path(&dir, &rid), Kind::Com.path(&dir, &rid)).unwrap();
            seed_answer(&root, &rid, &Answer::new(Outcome::Declined, Vec::new()));

            // 1. The answer read, the rerun waits: one helper the whole time.
            let deadline = Instant::now() + PATIENCE;
            while exists(&Kind::Ans.path(&dir, &rid)) {
                assert!(Instant::now() < deadline, "the answer is read");
                thread::sleep(Duration::from_millis(1));
            }
            let watched = Instant::now() + Duration::from_millis(100);
            while Instant::now() < watched {
                assert!(helpers() <= 1, "a second helper");
                thread::sleep(Duration::from_millis(1));
            }

            // 2. Let go: the rerun leads.
            drop(lock);
            let ended = waiter.join().unwrap();
            assert!(matches!(ended, Ok(Ended::Led(_))), "{ended:?}");
        });
    }

    #[test]
    fn the_janitor_removes_dead_files() {
        let (_dir, root) = new_store();
        let dir = queue(&root);
        let (dead_req, _) = seed(&root, Kind::Req, Ask::Frame(2), false);
        let (dead_com, _) = seed(&root, Kind::Com, Ask::Frame(3), false);
        let (dead_tmp, _) = seed(&root, Kind::Temp, Ask::Frame(4), false);
        let (live, _alive) = seed(&root, Kind::Com, Ask::Frame(5), true);
        seed_answer(&root, &live, &Answer::new(Outcome::NoFrame, Vec::new()));
        let orphan = RequestId::new();
        seed_answer(&root, &orphan, &Answer::new(Outcome::NoFrame, Vec::new()));
        fs::write(Kind::TempAns.path(&dir, &orphan), b"").unwrap();
        fs::write(Kind::TempBatch.path(&dir, &orphan), b"").unwrap();
        fs::write(dir.join("notes.txt"), b"not ours").unwrap();
        let store = open(&root);
        drop(lead_on(&store, hold(&store), Ask::NoFrame, Seams::real()));
        let mut left: Vec<String> = (fs::read_dir(&dir).unwrap())
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        let mut want = vec![
            "notes.txt".to_owned(),
            format!("{live}.ans"),
            format!("{live}.com"),
        ];
        want.sort();
        assert_eq!(left, want, "{dead_req} {dead_com} {dead_tmp} removed");
    }

    #[test]
    fn an_uncontended_turn_writes_no_queue_file() {
        let (_dir, root) = new_store();
        let store = open(&root);
        let Ended::Led(led) = turn(&store, Ask::Frame(1)).unwrap() else {
            panic!("the lock is free");
        };
        assert_eq!((led.requests, led.held), (1, 1));
        assert!(!exists(&queue(&root)), "no queue made");
        assert_eq!(landed(&store, 1), 1);
    }

    /// `F`, its crash points hidden: a wrapper that does not forward them.
    struct Hide<F>(F);

    impl<F: FsOps> FsOps for Hide<F> {
        fn create_dir_all(&self, p: &Path) -> io::Result<()> {
            self.0.create_dir_all(p)
        }

        fn write_durable_temp(&self, dir: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
            self.0.write_durable_temp(dir, bytes)
        }

        fn link_noclobber(&self, from: &Path, to: &Path) -> io::Result<LinkOutcome> {
            self.0.link_noclobber(from, to)
        }

        fn fsync_dir(&self, p: &Path) -> io::Result<()> {
            self.0.fsync_dir(p)
        }

        fn read(&self, p: &Path) -> io::Result<Option<Vec<u8>>> {
            self.0.read(p)
        }

        fn exists(&self, p: &Path) -> io::Result<bool> {
            self.0.exists(p)
        }

        fn remove(&self, p: &Path) -> io::Result<()> {
            self.0.remove(p)
        }

        fn append_durable(&self, path: &Path, bytes: &[u8]) -> io::Result<u64> {
            self.0.append_durable(path, bytes)
        }

        fn file_len(&self, path: &Path) -> io::Result<u64> {
            self.0.file_len(path)
        }

        fn truncate(&self, path: &Path, len: u64) -> io::Result<()> {
            self.0.truncate(path, len)
        }

        fn atomic_replace(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
            self.0.atomic_replace(path, bytes)
        }

        fn sync_data(&self, path: &Path) -> io::Result<()> {
            self.0.sync_data(path)
        }

        fn append(&self, path: &Path, bytes: &[u8]) -> io::Result<u64> {
            self.0.append(path, bytes)
        }

        fn rename_durable(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.0.rename_durable(from, to)
        }
    }

    /// How many crash points a batch of the own request and two waiters' passes on its leader
    /// through `wrap`, the spy counting them.
    fn batch_points(wrap: impl FnOnce(Arc<Spy>) -> Arc<dyn FsOps>) -> u64 {
        let (_dir, root) = new_store();
        let (_a, _alive_a) = seed(&root, Kind::Req, Ask::Frame(2), true);
        let (_b, _alive_b) = seed(&root, Kind::Req, Ask::NoFrame, true);
        let spy = Spy::new(|_, _| Ok(()));
        let store = open_on(&root, wrap(Arc::clone(&spy)));
        let led = lead_all(lead_on(&store, hold(&store), Ask::Frame(1), Seams::real()));
        assert_eq!(led.held, 3);
        spy.points.load(Ordering::SeqCst)
    }

    /// A batch with two other members passes a crash point before its manifest rename, each
    /// claim, each answer and the manifest's removal: six, through every forwarding wrapper. Its
    /// self-check: a wrapper that does not forward them sees none.
    #[test]
    fn a_batch_passes_its_crash_points() {
        let direct = batch_points(|spy| spy);
        assert_eq!(direct, 6);
        let boxed = batch_points(|spy| Arc::new(Box::new(spy) as Box<dyn FsOps>));
        assert_eq!(boxed, 6, "Box forwards");
        #[cfg(feature = "test-hooks")]
        {
            use crate::fs_ops::{FailAt, FailAtFs, FailMode, InjectedError};
            let never = FailAt::new(u64::MAX, FailMode::Once, InjectedError::Eio);
            let failing = batch_points(|spy| Arc::new(FailAtFs::new(spy, never)));
            assert_eq!(failing, 6, "FailAtFs forwards");
            let checked = batch_points(|spy| Arc::new(crate::OrderCheckFs::new(spy)));
            assert_eq!(checked, 6, "OrderCheckFs forwards");
        }
        assert_eq!(batch_points(|spy| Arc::new(Hide(spy))), 0, "the self-check");

        // An uncontended turn passes none.
        let (_dir, root) = new_store();
        let spy = Spy::new(|_, _| Ok(()));
        let store = open_on(&root, Arc::clone(&spy) as Arc<dyn FsOps>);
        turn(&store, Ask::Frame(1)).unwrap();
        assert_eq!(spy.points.load(Ordering::SeqCst), 0);
    }

    /// A [`crate::CrashAtFs`] counts each crash point as a call.
    #[cfg(feature = "test-hooks")]
    #[test]
    fn crash_at_fs_counts_crash_points() {
        let crash = crate::CrashAtFs::new(RealFs, 0);
        crash.crash_point();
        assert_eq!(crash.calls(), 1);
    }

    #[cfg(feature = "test-hooks")]
    mod hooks {
        //! What needs the test hooks: the crash sweep of a lead, and the lead-wait hook, each in a
        //! child process of this test binary.

        use std::os::unix::process::ExitStatusExt;
        use std::process::{Command, Output};

        use super::*;
        use crate::fs_ops::{CRASH_AT_ENV, CrashAtFs, LEAD_WAIT_ENV};

        /// Set only in a child: the store root it runs on.
        const ROOT_ENV: &str = "SWARMOPS_TEST_QUEUE_ROOT";

        /// The signal `std::process::abort` raises.
        const SIGABRT: i32 = 6;

        /// This test binary, running its test `test` of this module alone, with `env`.
        fn child(test: &str, env: &[(&str, &str)]) -> Output {
            let module = module_path!().split_once("::").unwrap().1;
            let exe = std::env::current_exe().unwrap();
            let name = format!("{module}::{test}");
            let mut command = Command::new(exe);
            command
                .args(["--exact", &name, "--nocapture"])
                .envs(env.iter().copied());
            command.output().unwrap()
        }

        /// The root a child runs on, if this is one.
        fn child_root() -> Option<Utf8PathBuf> {
            let root = std::env::var(ROOT_ENV).ok()?;
            Some(Utf8PathBuf::from(root))
        }

        /// Three turns at once, in a process whose lead-wait hook asks for two other requests:
        /// whoever leads first waits for the other two, and leads all three.
        #[test]
        fn the_lead_wait_hook_gathers_requests() {
            if let Some(root) = child_root() {
                let ends: Vec<Ended> = thread::scope(|s| {
                    let root = &root;
                    let turns: Vec<_> = (1..=3)
                        .map(|n| s.spawn(move || turn(&open(root), Ask::Frame(n)).unwrap()))
                        .collect();
                    turns.into_iter().map(|t| t.join().unwrap()).collect()
                });
                let led: Vec<&Led> = (ends.iter())
                    .filter_map(|end| match end {
                        Ended::Led(led) => Some(led),
                        Ended::Answered(_) => None,
                    })
                    .collect();
                assert_eq!(led.len(), 1, "{ends:?}");
                assert_eq!((led[0].requests, led[0].held), (3, 3), "{ends:?}");
                return;
            }
            let (_dir, root) = new_store();
            let ran = child(
                "the_lead_wait_hook_gathers_requests",
                &[(ROOT_ENV, root.as_str()), (LEAD_WAIT_ENV, "2")],
            );
            let stderr = String::from_utf8_lossy(&ran.stderr);
            assert!(ran.status.success(), "{:?}\n{stderr}", ran.status);
            let store = open(&root);
            assert!((1..=3).all(|n| landed(&store, n) == 1));
        }

        /// A lead-wait hook that is not a number panics, on an uncontended turn too: the commit
        /// that the release build's proof makes with it succeeds there.
        #[test]
        fn a_malformed_lead_wait_panics() {
            if let Some(root) = child_root() {
                let _ = turn(&open(&root), Ask::Frame(1));
                return;
            }
            let (_dir, root) = new_store();
            let ran = child(
                "a_malformed_lead_wait_panics",
                &[(ROOT_ENV, root.as_str()), (LEAD_WAIT_ENV, "not-a-number")],
            );
            let stderr = String::from_utf8_lossy(&ran.stderr);
            assert!(!ran.status.success(), "it panicked: {stderr}");
            assert!(
                stderr.contains("SWARMOPS_TEST_LEAD_WAIT=\"not-a-number\""),
                "{stderr}"
            );
            assert_eq!(landed(&open(&root), 1), 0, "nothing committed");
        }

        /// What the sweep seeds: two waiters, one with a frame, one frameless.
        const SEEDED: [Ask; 2] = [Ask::Frame(2), Ask::NoFrame];

        /// A copy of `template` at `to`, with the [`SEEDED`] requests, alive while the files are.
        fn seeded(template: &Utf8PathBuf, to: &Utf8PathBuf) -> Vec<(RequestId, Option<File>)> {
            let copied = Command::new("cp")
                .args(["-a", template.as_str(), to.as_str()])
                .status();
            assert!(copied.unwrap().success(), "cp");
            SEEDED
                .iter()
                .map(|ask| seed(to, Kind::Req, *ask, true))
                .collect()
        }

        /// What the queue holds after a crash: (a manifest, a member claimed with no answer, a
        /// member answered), and how many manifests.
        fn state(root: &Utf8PathBuf, rids: &[RequestId]) -> (bool, bool, bool, usize) {
            let manifests = files(root, Kind::Batch).len();
            let answers = files(root, Kind::Ans);
            let claimed = files(root, Kind::Com);
            let open = claimed.iter().any(|rid| !answers.contains(rid));
            let answered = rids.iter().any(|rid| answers.contains(rid));
            (manifests > 0, open, answered, manifests)
        }

        /// Recovers the store at `root` as the next writers would, then checks that each seeded
        /// member was answered once: the one with a frame committed once, at the seq its answer
        /// names.
        fn check_once(root: &Utf8PathBuf, rids: &[RequestId], k: u64) {
            let store = open(root);
            turn(&store, Ask::NoFrame).unwrap_or_else(|e| panic!("k={k}: {e}"));
            let answer = |rid: &RequestId| {
                let path = Kind::Ans.path(&queue(root), rid);
                let record = fs::read(&path).unwrap_or_else(|e| panic!("k={k}: {rid}: {e}"));
                decode_answer(&record).unwrap()
            };
            let framed = answer(&rids[0]);
            let read = store.read().unwrap();
            let moves: Vec<u64> = (read.reflog("main", 100).unwrap().iter())
                .filter(|entry| entry.new == Some(id(2)))
                .map(|entry| entry.seq)
                .collect();
            assert_eq!(moves.len(), 1, "k={k}: {moves:?}");
            let committed = Outcome::Committed {
                frame_seq: moves[0],
            };
            assert_eq!(framed.outcome, committed, "k={k}");
            assert_eq!(answer(&rids[1]).outcome, Outcome::NoFrame, "k={k}");
            assert!(landed(&store, 1) <= 1, "k={k}");
        }

        /// A lead crashing at any call of its turn, with two waiters seeded (one frameless):
        /// after recovery each waiter is answered once, and its frame is committed once. The
        /// sweep reaches a partial claim (C2), partial answers (C7) and every answer with the
        /// manifest left (C8); no state holds two manifests.
        #[test]
        fn a_queue_turn_crashing_at_any_call_answers_each_once() {
            if let Some(root) = child_root() {
                let k = std::env::var(CRASH_AT_ENV).unwrap().parse().unwrap();
                let store = open_on(&root, Arc::new(CrashAtFs::new(RealFs, k)));
                turn(&store, Ask::Frame(1)).unwrap();
                return;
            }
            let (_template_dir, template) = new_store();
            let calls = {
                let dir = tempfile::tempdir().unwrap();
                let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
                let _alive = seeded(&template, &root);
                let counter = Arc::new(CrashAtFs::new(RealFs, 0));
                let store = open_on(&root, Arc::clone(&counter) as Arc<dyn FsOps>);
                let Ended::Led(led) = turn(&store, Ask::Frame(1)).unwrap() else {
                    panic!("the lock is free");
                };
                assert_eq!(led.held, 3, "{led:?}");
                counter.calls()
            };
            let mut reached = (false, false, false);
            for k in 1..=calls {
                let dir = tempfile::tempdir().unwrap();
                let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
                let alive = seeded(&template, &root);
                let rids: Vec<RequestId> = alive.iter().map(|(rid, _)| rid.clone()).collect();
                let k_text = k.to_string();
                let test = "a_queue_turn_crashing_at_any_call_answers_each_once";
                let ran = child(test, &[(ROOT_ENV, root.as_str()), (CRASH_AT_ENV, &k_text)]);
                let stderr = String::from_utf8_lossy(&ran.stderr);
                assert_eq!(ran.status.signal(), Some(SIGABRT), "k={k}: {stderr}");
                let (manifest, open_claim, answered, manifests) = state(&root, &rids);
                assert!(manifests <= 1, "k={k}: {manifests} manifests");
                let all = rids.iter().all(|rid| files(&root, Kind::Ans).contains(rid));
                reached.0 |= manifest && open_claim && !answered;
                reached.1 |= manifest && open_claim && answered;
                reached.2 |= manifest && all;
                check_once(&root, &rids, k);
                drop(alive);
            }
            assert_eq!(
                reached,
                (true, true, true),
                "C2, C7, C8 reached over {calls} calls"
            );
        }
    }
}
