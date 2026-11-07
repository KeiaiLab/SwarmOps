//! The record of each request `serve` served (the M17 design §6): where its time went.
//!
//! ```text
//! parent serve: 1 start (unix µs), its connection id ─▶ SWARMOPS_SERVE_CONNECTION in git's env
//!               2 git runs ─┬─ pack-objects hook: git/serve-split/<id>
//!                           │    (its first byte's µs, the bytes it sent, clone or fetch)
//!                           └─ pre-receive hook: git/serve-split/<id>
//!                                (its start's µs, the quarantine's bytes, a refusal's code)
//!               3 git exits: the split read and removed, one ServeRecord line appended to
//!                 git/serve-records.jsonl; past MAX_SERVE_RECORDS_BYTES, the writer that takes
//!                 git/serve-records.lock renames it serve-records.1.jsonl
//! ```
//!
//! Diagnostics, not acknowledged state: nothing is synced and nothing reads them for
//! correctness; a reader skips a torn last line. A record that cannot be written is a
//! `warning:` line on stderr, never a failed request. A record holds the caller `serve`
//! resolved and numbers: no path, nothing of the environment.

use std::fs::{self, File, TryLockError};
use std::io::{self, Write as _};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use camino::Utf8Path;
use serde::{Deserialize, Serialize};
use swarmops_api::{ErrorCode, Identity, OpError, ServeRecord, ServeService};
use swarmops_git::serve::pack_key::PackDigest;

use super::Served;
use super::guard::Redactor;

/// The most bytes the records' file holds before it is renamed to the one older file.
pub const MAX_SERVE_RECORDS_BYTES: u64 = 64 << 20;

/// The variable the parent `serve` names its connection in, for its hooks' split.
pub(crate) const SWARMOPS_SERVE_CONNECTION: &str = "SWARMOPS_SERVE_CONNECTION";

/// The store's git directory, below its root.
const GIT: &str = "git";

/// The records' file, the one older file and the lock of its rotation, in the git directory.
const RECORDS: &str = "serve-records.jsonl";
const OLDER: &str = "serve-records.1.jsonl";
const ROTATE_LOCK: &str = "serve-records.lock";

/// The hooks' splits, one file per connection, in the git directory.
const SPLIT_DIR: &str = "serve-split";

/// The hex digits of a connection id.
const ID_HEX: usize = 16;

/// The longest line appended: one `write` of it is not interleaved with another's.
const MAX_LINE: usize = 4 << 10;

/// Microseconds in a millisecond.
const US_PER_MS: u64 = 1_000;

/// What the parent `serve` runs: a fetch's side or a push's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Side {
    /// `upload-pack`, or an HTTP request of a fetch.
    Fetch,
    /// `receive-pack`, or an HTTP request of a push.
    Push,
}

/// What a hook says of its part of a connection, in `git/serve-split/<id>`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub(crate) struct Split {
    /// The unix µs of the mark: a pack's first byte, or a push check's start.
    mark_us: u64,
    /// The pack's bytes: sent, or received into the quarantine.
    bytes: u64,
    /// The service the hook saw.
    service: ServeService,
    /// A refusal's code, when the hook refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    code: Option<ErrorCode>,
}

impl Split {
    /// The split of a fetch's pack: its first byte at `first_us`, `bytes` sent, a `service`
    /// clone or fetch.
    pub(crate) fn sent(first_us: u64, bytes: u64, service: ServeService) -> Split {
        Split {
            mark_us: first_us,
            bytes,
            service,
            code: None,
        }
    }

    /// The split of a push: its check started at `start_us` with `bytes` received, refused
    /// with `code` or not.
    pub(crate) fn pushed(start_us: u64, bytes: u64, code: Option<ErrorCode>) -> Split {
        Split {
            mark_us: start_us,
            bytes,
            service: ServeService::Push,
            code,
        }
    }
}

/// One connection of the parent `serve`, from its start.
#[derive(Debug, Clone)]
pub(crate) struct Connection {
    /// Its id: 16 hex digits of BLAKE3 of the pid and the start.
    id: String,
    /// Its start, in unix µs.
    start_us: u64,
}

impl Connection {
    /// The connection starting now.
    pub(crate) fn start() -> Connection {
        let start_us = now_us();
        let mut seed = Vec::with_capacity(12);
        seed.extend_from_slice(&std::process::id().to_le_bytes());
        seed.extend_from_slice(&start_us.to_le_bytes());
        // A read of a slice does not fail; the start alone would do as well.
        let id = PackDigest::of(&mut seed.as_slice()).map_or_else(
            |_| format!("{start_us:0ID_HEX$x}"),
            |(hex, _)| hex[..ID_HEX].to_owned(),
        );
        Connection { id, start_us }
    }

    /// Its id, for [`SWARMOPS_SERVE_CONNECTION`].
    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    /// Appends the record of this connection to the store at `store`, served to `who` on
    /// `side` and ended with `outcome`; a failure is a warning on stderr, masked by `redactor`.
    /// A path that holds no store's git directory gets none.
    pub(crate) fn finish(
        &self,
        store: &Utf8Path,
        who: &Identity,
        side: Side,
        outcome: &Result<Served, OpError>,
        redactor: &Redactor,
    ) {
        // 1. The store's git directory, which a refusal before the store never made.
        let git = store.as_std_path().join(GIT);
        if !git.is_dir() {
            return;
        }

        // 2. The split its hook left, then the record.
        let end_us = now_us();
        let split = take_split(&git, &self.id);
        let record = self.record(who, side, split, end_us, outcome);

        // 3. Appended: a failure warns and the request stands.
        if let Err(e) = append(&git, &record, MAX_SERVE_RECORDS_BYTES) {
            let mut stderr = io::stderr().lock();
            let warning = redactor.redact(&format!("serve records: {e}"));
            let _ = writeln!(stderr, "warning: {warning}");
        }
    }

    /// The record of this connection ended at `end_us` with `outcome`, its time split by
    /// `split` (the M17 design §6).
    fn record(
        &self,
        who: &Identity,
        side: Side,
        split: Option<Split>,
        end_us: u64,
        outcome: &Result<Served, OpError>,
    ) -> ServeRecord {
        // 1. The service and the split: a fetch's mark is its pack's first byte, a push's
        //    its check's start; without one, the whole connection prepared.
        let at = self.start_us / US_PER_MS;
        let fallback = match side {
            Side::Fetch => ServeService::Fetch,
            Side::Push => ServeService::Push,
        };
        let mut record = ServeRecord::new(at, who.agent(), fallback);
        let whole = ms(end_us.saturating_sub(self.start_us));
        record.prepare_ms = whole;
        if let Some(split) = split {
            let before = ms(split.mark_us.saturating_sub(self.start_us));
            let after = ms(end_us.saturating_sub(split.mark_us));
            let (prepare, send) = match split.service {
                ServeService::Push => (after, before),
                _ => (before, after),
            };
            record.service = split.service;
            record.prepare_ms = prepare;
            record.send_ms = send;
            record.bytes = split.bytes;
            record.code = split.code;
        }

        // 2. How it ended: git's exit, or the refusal's.
        match outcome {
            Ok(served) => record.exit = served.exit,
            Err(e) => {
                record.exit = e.exit;
                record.code = code_of(&e.code);
            }
        }
        record
    }
}

/// Writes `split` for this hook's connection, named by [`SWARMOPS_SERVE_CONNECTION`], in the
/// store at `store`. Advice: a hook run without a connection, or a split that cannot be
/// written, leaves the connection's record unsplit.
pub(crate) fn write_split(store: &Path, split: &Split) {
    let Some(id) = connection_id() else {
        return;
    };
    let dir = store.join(GIT).join(SPLIT_DIR);
    if let Err(e) = fs::create_dir(&dir)
        && e.kind() != io::ErrorKind::AlreadyExists
    {
        return;
    }
    let Ok(mut line) = serde_json::to_vec(split) else {
        return;
    };
    line.push(b'\n');
    let file = File::options()
        .write(true)
        .create_new(true)
        .open(dir.join(id));
    if let Ok(mut file) = file {
        let _ = file.write_all(&line);
    }
}

/// The error code whose `E_*` string is `code`, when one is.
pub(crate) fn code_of(code: &str) -> Option<ErrorCode> {
    serde_json::from_value(serde_json::Value::String(code.to_owned())).ok()
}

/// The unix µs now.
pub(crate) fn now_us() -> u64 {
    let since = SystemTime::now().duration_since(UNIX_EPOCH);
    since.map_or(0, |d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX))
}

/// The connection id the parent named, when it is one: 16 lowercase hex digits, never a path.
fn connection_id() -> Option<String> {
    let id = std::env::var(SWARMOPS_SERVE_CONNECTION).ok()?;
    let hex = id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    (id.len() == ID_HEX && hex).then_some(id)
}

/// The split of connection `id` in the git directory `git`, removed: none when its hook left
/// none or one that does not read.
fn take_split(git: &Path, id: &str) -> Option<Split> {
    let path = git.join(SPLIT_DIR).join(id);
    let text = fs::read(&path).ok()?;
    let _ = fs::remove_file(&path);
    serde_json::from_slice(&text).ok()
}

/// `us` in whole milliseconds.
fn ms(us: u64) -> u64 {
    us / US_PER_MS
}

/// Appends `record` as one line to the records of the git directory `git`, in one `write`;
/// past `bound` bytes, renamed to the older file by the writer that takes the rotation's lock.
fn append(git: &Path, record: &ServeRecord, bound: u64) -> io::Result<()> {
    // 1. The line, short enough that one write of it is whole.
    let mut line = serde_json::to_vec(record).map_err(io::Error::other)?;
    line.push(b'\n');
    if line.len() > MAX_LINE {
        return Err(io::Error::other("a record past 4 KiB"));
    }

    // 2. Appended, unsynced: diagnostics.
    let path = git.join(RECORDS);
    let mut file = File::options().append(true).create(true).open(&path)?;
    file.write_all(&line)?;
    if file.metadata()?.len() <= bound {
        return Ok(());
    }

    // 3. Rotated by the writer that takes the lock, no wait; past the bound still, since
    //    another may have rotated first.
    rotate(git, &path, bound)
}

/// Renames the records at `path` to the older file when they are past `bound`, holding the
/// rotation's lock of `git`; another holder's lock leaves it to them.
fn rotate(git: &Path, path: &Path, bound: u64) -> io::Result<()> {
    let lock = File::options()
        .write(true)
        .create(true)
        .truncate(false)
        .open(git.join(ROTATE_LOCK))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Ok(()),
        Err(TryLockError::Error(e)) => return Err(e),
    }
    let len = fs::metadata(path).map_or(0, |meta| meta.len());
    if len > bound {
        fs::rename(path, git.join(OLDER))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use swarmops_api::{ErrorCode, Identity, OpError, ServeRecord, ServeService};

    use super::{Connection, OLDER, RECORDS, Side, Split, append};
    use crate::serve::Served;

    /// The caller of these records.
    fn ada() -> Identity {
        Identity::parse("person/ada").unwrap()
    }

    /// A connection started at 1 s.
    fn at_one_second() -> Connection {
        Connection {
            id: "0123456789abcdef".to_owned(),
            start_us: 1_000_000,
        }
    }

    /// A fetch's pack splits the connection at its first byte; a push's check at its start,
    /// the other way round.
    #[test]
    fn a_split_divides_the_connection() {
        let connection = at_one_second();
        let served = Ok(Served::new(0));
        let sent = Split::sent(1_200_000, 77, ServeService::Clone);
        let fetch = connection.record(&ada(), Side::Fetch, Some(sent), 1_500_000, &served);
        assert_eq!(
            (
                fetch.at,
                fetch.service,
                fetch.prepare_ms,
                fetch.send_ms,
                fetch.bytes
            ),
            (1_000, ServeService::Clone, 200, 300, 77)
        );
        let pushed = Split::pushed(1_200_000, 9, None);
        let push = connection.record(&ada(), Side::Push, Some(pushed), 1_500_000, &served);
        assert_eq!(
            (push.service, push.prepare_ms, push.send_ms, push.bytes),
            (ServeService::Push, 300, 200, 9)
        );
    }

    /// Without a split, the connection prepared; a refusal gives its exit and its code.
    #[test]
    fn an_unsplit_refusal_is_all_prepare() {
        let refused = Err(OpError::new(ErrorCode::Auth, "no"));
        let record = at_one_second().record(&ada(), Side::Fetch, None, 1_250_000, &refused);
        let mut expected = ServeRecord::new(1_000, "person/ada", ServeService::Fetch);
        expected.prepare_ms = 250;
        expected.exit = ErrorCode::Auth.exit();
        expected.code = Some(ErrorCode::Auth);
        assert_eq!(record, expected);
    }

    /// Past the bound the records move to the one older file, which the next rotation
    /// replaces; each line stays whole.
    #[test]
    fn records_rotate_past_the_bound() {
        let git = tempfile::tempdir().unwrap();
        let record = ServeRecord::new(1, "person/ada", ServeService::Fetch);
        let line_len = serde_json::to_vec(&record).unwrap().len() as u64 + 1;
        let bound = line_len * 2;
        for _ in 0..3 {
            append(git.path(), &record, bound).unwrap();
        }
        assert!(!git.path().join(RECORDS).exists());
        let older = fs::read_to_string(git.path().join(OLDER)).unwrap();
        assert_eq!(older.lines().count(), 3);
        append(git.path(), &record, bound).unwrap();
        let current = fs::read_to_string(git.path().join(RECORDS)).unwrap();
        assert_eq!(current.lines().count(), 1);
        for line in older.lines().chain(current.lines()) {
            let read: ServeRecord = serde_json::from_str(line).unwrap();
            assert_eq!(read, record);
        }
    }
}
