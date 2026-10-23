//! The registry (§2.3 of the M5 design): one entry per workspace of a store,
//! `<store>/workspaces/<name>.json`, which makes names unique and lets a workspace be found by
//! name. Client-side data: no store operation reads or returns it, and a store's readers never
//! list the store's root, so older binaries do not see it.
//!
//! ```text
//! <store>/workspaces/<name>.json   {"format":1,"name","path","created_by","created_at","session"?}
//! <store>/workspaces/tmp/          synced temps, linked no-clobber onto an entry
//! ```
//!
//! An entry is made as the object store makes an object (storage spec §2, rule 1): a synced temp
//! in `tmp/`, a no-clobber link onto `<name>.json`, a directory sync; so two creates of one name
//! at once have one winner. It is removed by an unlink and a directory sync.

use std::io;
use std::sync::Arc;

use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use swarmops_api::{ErrorCode, OpError};
use swarmops_core::portable;
use swarmops_store::{FsMethod, FsOps, LinkOutcome};

use crate::io_error;

/// The registry's directory below the store's root: the one place its name is spelled.
pub const REGISTRY_DIR: &str = "workspaces";

/// The format of an entry this build reads and writes.
pub const ENTRY_FORMAT: u32 = 1;

/// The longest workspace name, in bytes.
const MAX_NAME_LEN: usize = 64;

/// The directory of the temps an entry is linked from, in [`REGISTRY_DIR`].
const TMP_DIR: &str = "tmp";

/// One workspace's entry: who made it, where, and when. No `base` or `head`: those change on
/// every commit and live in the workspace's `workspace.json`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The entry's format: [`ENTRY_FORMAT`].
    pub format: u32,
    /// The workspace's name.
    pub name: String,
    /// Its directory on the host.
    pub path: String,
    /// The identity that made it.
    pub created_by: String,
    /// When it was made, in Unix milliseconds.
    pub created_at: u64,
    /// The Claude Code hook's session that made it: 16 hex digits of the BLAKE3 hash of the
    /// hook's `session_id`; only on entries the hook made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

/// What [`Registry::create`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Linked {
    /// It linked the entry.
    New,
    /// The same workspace's entry was there already (name, path, creator and session alike),
    /// as it was made.
    Existing(Entry),
}

/// A store's registry, at the store's root the caller names.
pub struct Registry {
    /// `<store>/workspaces`.
    dir: Utf8PathBuf,
    /// The file primitives the entries are read and written through.
    fs: Arc<dyn FsOps>,
}

impl Registry {
    /// The registry of the store at `store_root`, read and written through `fs`. No I/O.
    pub fn open(store_root: &Utf8Path, fs: Arc<dyn FsOps>) -> Registry {
        Registry {
            dir: store_root.join(REGISTRY_DIR),
            fs,
        }
    }

    /// The entry of workspace `name`, if there is one. `E_INVALID_ARG` for a name outside the
    /// workspace names' grammar, or an entry this build cannot read.
    pub fn read(&self, name: &str) -> Result<Option<Entry>, OpError> {
        // 1. A name outside the grammar could name a file outside the registry.
        check_name(name)?;

        // 2. The entry's bytes, if it exists.
        let file = entry_file(name);
        let path = self.path(name);
        let Some(bytes) = (self.fs.read(path.as_std_path()))
            .map_err(|e| io_error::io_failed(FsMethod::Read, &file, &e))?
        else {
            return Ok(None);
        };

        // 3. Parsed, in a format this build reads.
        let entry: Entry = serde_json::from_slice(&bytes).map_err(|e| unreadable(&file, &e))?;
        if entry.format != ENTRY_FORMAT {
            return Err(OpError::new(
                ErrorCode::InvalidArg,
                format!(
                    "{file} has format {}, which this build does not read (it reads \
                     {ENTRY_FORMAT}): a newer swarmops wrote it",
                    entry.format
                ),
            ));
        }
        Ok(Some(entry))
    }

    /// Links `entry` into the registry, unless the same workspace's entry is there: name,
    /// path and creator alike, and the same session (both absent, or equal). `E_WS_EXISTS`
    /// (`details{name, path}` of the entry there) for any other entry of its name;
    /// `E_INVALID_ARG` for a name outside the grammar or one a platform cannot hold.
    pub fn create(&self, entry: &Entry) -> Result<Linked, OpError> {
        // 1. A name in the grammar that every platform can hold.
        check_name(&entry.name)?;

        // 2. An entry of that name already: the same workspace, or another. A create cut
        //    after its link may have left the names not yet durable: made so first.
        if let Some(there) = self.read(&entry.name)? {
            self.sync_names()?;
            return same(entry, there);
        }

        // 3. The registry's directories; their names are made durable in step 5.
        let tmp = self.dir.join(TMP_DIR);
        let tmp_file = format!("{REGISTRY_DIR}/{TMP_DIR}");
        (self.fs.create_dir_all(tmp.as_std_path()))
            .map_err(|e| io_error::io_failed(FsMethod::CreateDirAll, &tmp_file, &e))?;

        // 4. A synced temp, linked no-clobber onto the entry's name.
        let bytes = serde_json::to_vec(entry).expect("a registry entry serializes");
        let staged = (self.fs.write_durable_temp(tmp.as_std_path(), &bytes))
            .map_err(|e| io_error::io_failed(FsMethod::WriteDurableTemp, &tmp_file, &e))?;
        let file = entry_file(&entry.name);
        let outcome = (self
            .fs
            .link_noclobber(&staged, self.path(&entry.name).as_std_path()))
        .map_err(|e| io_error::io_failed(FsMethod::LinkNoclobber, &file, &e))?;

        // 5. The names made durable, whichever create linked it: the entry's in the registry,
        //    the registry's in the store root (a create cut earlier may have made it unsynced).
        self.sync_names()?;

        // 6. Another create that linked first decides, as an entry found in step 2 would.
        if outcome == LinkOutcome::Placed {
            return Ok(Linked::New);
        }
        let there = self.read(&entry.name)?.ok_or_else(|| {
            OpError::new(
                ErrorCode::Io,
                format!("{file} was linked by another create, then removed"),
            )
        })?;
        same(entry, there)
    }

    /// Every entry, by name. The directory is listed with `std::fs` (read-only: `FsOps` lists
    /// nothing); each entry is read through `FsOps`. Files that name no workspace (`tmp/`, a
    /// name outside the grammar) are passed over; none when the registry was never written.
    pub fn list(&self) -> Result<Vec<Entry>, OpError> {
        // 1. The directory's names; no directory, no entries.
        let listed = match std::fs::read_dir(&self.dir) {
            Ok(listed) => listed,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(unlisted(&e)),
        };
        let mut names = Vec::new();
        for item in listed {
            let item = item.map_err(|e| unlisted(&e))?;
            let Ok(file) = item.file_name().into_string() else {
                continue;
            };
            let Some(name) = file.strip_suffix(".json") else {
                continue;
            };
            if check_name(name).is_err() {
                continue;
            }
            names.push(name.to_owned());
        }
        names.sort();

        // 2. Each entry, read as `read` reads it; one removed meanwhile is passed over.
        let mut entries = Vec::with_capacity(names.len());
        for name in names {
            if let Some(entry) = self.read(&name)? {
                entries.push(entry);
            }
        }
        Ok(entries)
    }

    /// Removes the entry of workspace `name`: an unlink, then a directory sync. An entry
    /// already gone is removed (a delete run again after a crash finishes). The caller removes
    /// the workspace's directory first (WS4).
    pub fn delete(&self, name: &str) -> Result<(), OpError> {
        // 1. The unlink; nothing to unlink is done already.
        check_name(name)?;
        let file = entry_file(name);
        match self.fs.remove(self.path(name).as_std_path()) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_error::io_failed(FsMethod::Remove, &file, &e)),
        }

        // 2. The removal made durable; a registry never written has nothing to sync.
        match self.fs.fsync_dir(self.dir.as_std_path()) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io_error::io_failed(FsMethod::FsyncDir, REGISTRY_DIR, &e)),
        }
    }

    /// The entry file of workspace `name`.
    fn path(&self, name: &str) -> Utf8PathBuf {
        self.dir.join(format!("{name}.json"))
    }

    /// Syncs directory `dir`, named `shown` in an error.
    fn sync(&self, dir: &Utf8Path, shown: &str) -> Result<(), OpError> {
        (self.fs.fsync_dir(dir.as_std_path()))
            .map_err(|e| io_error::io_failed(FsMethod::FsyncDir, shown, &e))
    }

    /// Syncs `workspaces/`, then the store root: an entry's name, then the registry's.
    fn sync_names(&self) -> Result<(), OpError> {
        self.sync(&self.dir, REGISTRY_DIR)?;
        let root = self.dir.parent().unwrap_or(&self.dir);
        self.sync(root, ".")
    }
}

/// `entry` against the entry `there` of its name: the same workspace when the path, the creator
/// and the session are alike; else `E_WS_EXISTS`, naming the one there.
fn same(entry: &Entry, there: Entry) -> Result<Linked, OpError> {
    let alike = entry.path == there.path
        && entry.created_by == there.created_by
        && entry.session == there.session;
    if alike {
        return Ok(Linked::Existing(there));
    }
    Err(OpError::new(
        ErrorCode::WsExists,
        format!(
            "a workspace named {:?} exists already, at {}, made by {}",
            there.name, there.path, there.created_by
        ),
    )
    .with_details(Map::from_iter([
        ("name".to_owned(), Value::from(there.name)),
        ("path".to_owned(), Value::from(there.path)),
    ]))
    .with_hint("swarmops ws list"))
}

/// The entry file of workspace `name`, below the store's root, as errors name it.
fn entry_file(name: &str) -> String {
    format!("{REGISTRY_DIR}/{name}.json")
}

/// `E_INVALID_ARG` unless `name` is a workspace name: in the grammar ([`is_name`]), and one
/// every platform can hold (no Windows device name, no trailing dot), on every platform.
fn check_name(name: &str) -> Result<(), OpError> {
    // 1. The grammar: it keeps a name inside the registry's directory.
    if !is_name(name) {
        return Err(OpError::new(
            ErrorCode::InvalidArg,
            format!(
                "{name:?} is not a workspace name: 1 to {MAX_NAME_LEN} of a-z, 0-9, '.', '_' and \
                 '-', not starting with '.'"
            ),
        ));
    }

    // 2. The portable names' rule, as refs follow it.
    portable::check_component(name).map_err(|why| {
        OpError::new(
            ErrorCode::InvalidArg,
            format!("{name:?} is not a workspace name every platform can hold: {why}"),
        )
    })
}

/// `E_IO` for a listing of the registry's directory that failed with `e`.
fn unlisted(e: &io::Error) -> OpError {
    OpError::new(ErrorCode::Io, format!("listing {REGISTRY_DIR}: {e}")).with_details(
        Map::from_iter([("file".to_owned(), Value::from(REGISTRY_DIR))]),
    )
}

/// Whether `name` is in the workspace names' grammar: 1 to 64 of `a-z`, `0-9`, `.`, `_` and
/// `-`, not starting with `.`.
fn is_name(name: &str) -> bool {
    let allowed = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b);
    (1..=MAX_NAME_LEN).contains(&name.len()) && !name.starts_with('.') && name.bytes().all(allowed)
}

/// `E_INVALID_ARG` for a registry entry that does not parse.
fn unreadable(file: &str, e: &serde_json::Error) -> OpError {
    OpError::new(
        ErrorCode::InvalidArg,
        format!("{file} is not a workspace entry: {e}"),
    )
}
