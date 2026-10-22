//! The registry (§2.3 of the M5 design): one entry per workspace of a store,
//! `<store>/workspaces/<name>.json`, which makes names unique and lets a workspace be found by
//! name. Client-side data: no store operation reads or returns it, and a store's readers never
//! list the store's root, so older binaries do not see it.
//!
//! ```text
//! <store>/workspaces/<name>.json   {"format":1,"name","path","created_by","created_at","session"?}
//! <store>/workspaces/tmp/          synced temps, linked no-clobber onto an entry (W4)
//! ```
//!
//! The M5 plan's task W1 froze [`Registry`], [`Registry::open`], [`REGISTRY_DIR`], the entry's
//! type and its read (which `workspace::find` needs); task W4 writes create, list and delete.

use std::sync::Arc;

use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};
use swarmops_api::{ErrorCode, OpError};
use swarmops_store::{FsMethod, FsOps};

use crate::io_failed;

/// The registry's directory below the store's root: the one place its name is spelled.
pub const REGISTRY_DIR: &str = "workspaces";

/// The format of an entry this build reads and writes.
pub const ENTRY_FORMAT: u32 = 1;

/// The longest workspace name, in bytes.
const MAX_NAME_LEN: usize = 64;

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
        if !is_name(name) {
            return Err(OpError::new(
                ErrorCode::InvalidArg,
                format!(
                    "{name:?} is not a workspace name: 1 to {MAX_NAME_LEN} of a-z, 0-9, '.', '_' \
                     and '-', not starting with '.'"
                ),
            ));
        }

        // 2. The entry's bytes, if it exists.
        let file = format!("{REGISTRY_DIR}/{name}.json");
        let path = self.dir.join(format!("{name}.json"));
        let Some(bytes) =
            (self.fs.read(path.as_std_path())).map_err(|e| io_failed(FsMethod::Read, &file, &e))?
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
