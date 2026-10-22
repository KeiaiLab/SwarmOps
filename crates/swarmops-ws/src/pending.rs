//! The intent records (§2.4 of the M5 design): `journal/pending.json`, durable before the store
//! call it names, and `journal/materialize.json`, durable before the first file write.
//!
//! ```text
//! .swarmws/journal/pending.json      {op, key, rebase_key?, head, target?, attempt, state}
//! .swarmws/journal/materialize.json  {from?, to, prefixes}
//! ```
//!
//! Both are written whole and durably (`FsOps::atomic_replace`) and removed by an unlink and a
//! directory sync once what they name is done: a relaunch that finds one finishes it first
//! (WS1, WS3).

use camino::{Utf8Path, Utf8PathBuf};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use swarmops_api::{ErrorCode, OpError};
use swarmops_store::{FsMethod, FsOps};

use crate::io_failed;
use crate::state::SWARMWS_DIR;

/// The intents' directory, in [`SWARMWS_DIR`].
pub const JOURNAL_DIR: &str = "journal";

/// The record of a mutation's store call in flight, in [`JOURNAL_DIR`].
pub const PENDING_JSON: &str = "pending.json";

/// The record of a materialisation in flight, in [`JOURNAL_DIR`].
pub const MATERIALIZE_JSON: &str = "materialize.json";

/// The store call a pending record names.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum PendingOp {
    /// `ws commit`'s `commit`.
    Commit,
    /// `ws rebase`'s `rebase`.
    Rebase,
    /// `ws land`'s `land`, with its rebases under `--check`.
    Land,
}

/// A mutation's store call in flight: written before the call, with the key the call uses, so
/// that a relaunch makes the same call with the same key and the store replays it (WS1).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    /// The store call.
    pub op: PendingOp,
    /// Its idempotency key: a uuid v7.
    pub key: String,
    /// Under `ws land --check`: the key of the attempt's `rebase`, fresh for each attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rebase_key: Option<String>,
    /// The workspace's head the call starts from.
    pub head: String,
    /// The ref it lands on, for a land.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// The attempt, from 1.
    pub attempt: u32,
    /// Where the mutation stands, as the operation that writes it names it.
    pub state: String,
}

/// A materialisation in flight: written before its first file write, removed after the index
/// (WS3). `from` none writes the whole tree of `to` under `prefixes`; otherwise only what
/// differs from `from`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Intent {
    /// The changeset the files stood for, or none for a tree written whole.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// The changeset they will stand for.
    pub to: String,
    /// The repository paths written: the workspace's prefixes, or those a `ws materialize` adds;
    /// empty for the whole tree.
    pub prefixes: Vec<String>,
}

/// The pending record of the workspace at `dir`, if one is there.
pub fn read_pending(fs: &dyn FsOps, dir: &Utf8Path) -> Result<Option<Pending>, OpError> {
    read_json(fs, dir, PENDING_JSON)
}

/// Writes `pending` as the workspace's pending record, durably.
pub fn write_pending(fs: &dyn FsOps, dir: &Utf8Path, pending: &Pending) -> Result<(), OpError> {
    write_json(fs, dir, PENDING_JSON, pending)
}

/// Removes the workspace's pending record, durably; none there is removed already.
pub fn remove_pending(fs: &dyn FsOps, dir: &Utf8Path) -> Result<(), OpError> {
    remove_json(fs, dir, PENDING_JSON)
}

/// The materialisation intent of the workspace at `dir`, if one is there.
pub fn read_intent(fs: &dyn FsOps, dir: &Utf8Path) -> Result<Option<Intent>, OpError> {
    read_json(fs, dir, MATERIALIZE_JSON)
}

/// Writes `intent` as the workspace's materialisation intent, durably.
pub fn write_intent(fs: &dyn FsOps, dir: &Utf8Path, intent: &Intent) -> Result<(), OpError> {
    write_json(fs, dir, MATERIALIZE_JSON, intent)
}

/// Removes the workspace's materialisation intent, durably; none there is removed already.
pub fn remove_intent(fs: &dyn FsOps, dir: &Utf8Path) -> Result<(), OpError> {
    remove_json(fs, dir, MATERIALIZE_JSON)
}

/// The journal directory of the workspace at `dir`.
pub(crate) fn journal_in(dir: &Utf8Path) -> Utf8PathBuf {
    dir.join(SWARMWS_DIR).join(JOURNAL_DIR)
}

/// Record `name`'s path below the workspace's root, as errors name it.
fn shown(name: &str) -> String {
    format!("{SWARMWS_DIR}/{JOURNAL_DIR}/{name}")
}

/// Record `name` of the workspace at `dir`, parsed, if it is there.
fn read_json<T: DeserializeOwned>(
    fs: &dyn FsOps,
    dir: &Utf8Path,
    name: &str,
) -> Result<Option<T>, OpError> {
    // 1. The bytes, if any.
    let file = shown(name);
    let path = journal_in(dir).join(name);
    let Some(bytes) =
        (fs.read(path.as_std_path())).map_err(|e| io_failed(FsMethod::Read, &file, &e))?
    else {
        return Ok(None);
    };

    // 2. Parsed; a record this build cannot read is named.
    serde_json::from_slice(&bytes).map(Some).map_err(|e| {
        OpError::new(
            ErrorCode::InvalidArg,
            format!("{file} is not a record this build reads: {e}"),
        )
        .with_details(Map::from_iter([("file".to_owned(), Value::from(file))]))
    })
}

/// Writes `value` as record `name` of the workspace at `dir`, whole and durably.
fn write_json<T: Serialize>(
    fs: &dyn FsOps,
    dir: &Utf8Path,
    name: &str,
    value: &T,
) -> Result<(), OpError> {
    // 1. The journal directory, there from the workspace's creation; made if a crash left none.
    let journal = journal_in(dir);
    let file = shown(name);
    (fs.create_dir_all(journal.as_std_path()))
        .map_err(|e| io_failed(FsMethod::CreateDirAll, &file, &e))?;

    // 2. The record, replaced whole: temp, sync, rename, directory sync.
    let bytes = serde_json::to_vec(value).expect("an intent record serializes");
    (fs.atomic_replace(journal.join(name).as_std_path(), &bytes))
        .map_err(|e| io_failed(FsMethod::AtomicReplace, &file, &e))
}

/// Removes record `name` of the workspace at `dir`: an unlink, then a directory sync.
fn remove_json(fs: &dyn FsOps, dir: &Utf8Path, name: &str) -> Result<(), OpError> {
    // 1. The unlink; nothing there is removed already.
    let journal = journal_in(dir);
    let file = shown(name);
    match fs.remove(journal.join(name).as_std_path()) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(io_failed(FsMethod::Remove, &file, &e)),
    }

    // 2. The removal made durable.
    (fs.fsync_dir(journal.as_std_path())).map_err(|e| io_failed(FsMethod::FsyncDir, &file, &e))
}

#[cfg(test)]
mod tests {
    use swarmops_store::RealFs;

    use super::*;

    /// Each record is written whole, read back equal and removed; a record that does not parse
    /// is refused, and none there reads as none.
    #[test]
    fn the_records_round_trip_and_go() {
        // 1. A workspace directory with its .swarmws/.
        let tmp = tempfile::tempdir().unwrap();
        let dir = Utf8Path::from_path(tmp.path()).unwrap();
        std::fs::create_dir(dir.join(SWARMWS_DIR)).unwrap();
        let fs = RealFs;

        // 2. The pending record.
        let pending = Pending {
            op: PendingOp::Land,
            key: "0192f4c4-0000-7000-8000-000000000000".to_owned(),
            rebase_key: None,
            head: "c".repeat(64),
            target: Some("main".to_owned()),
            attempt: 1,
            state: "landing".to_owned(),
        };
        assert_eq!(read_pending(&fs, dir).unwrap(), None);
        write_pending(&fs, dir, &pending).unwrap();
        assert_eq!(read_pending(&fs, dir).unwrap(), Some(pending));
        remove_pending(&fs, dir).unwrap();
        remove_pending(&fs, dir).unwrap();
        assert_eq!(read_pending(&fs, dir).unwrap(), None);

        // 3. The materialisation intent, its `from` omitted when none.
        let intent = Intent {
            from: None,
            to: "d".repeat(64),
            prefixes: vec!["src".to_owned()],
        };
        write_intent(&fs, dir, &intent).unwrap();
        let text = std::fs::read_to_string(journal_in(dir).join(MATERIALIZE_JSON)).unwrap();
        assert!(!text.contains("from"), "{text}");
        assert_eq!(read_intent(&fs, dir).unwrap(), Some(intent));
        remove_intent(&fs, dir).unwrap();
        assert_eq!(read_intent(&fs, dir).unwrap(), None);

        // 4. A record that does not parse.
        std::fs::write(journal_in(dir).join(PENDING_JSON), b"{").unwrap();
        assert_eq!(read_pending(&fs, dir).unwrap_err().code, "E_INVALID_ARG");
    }
}
